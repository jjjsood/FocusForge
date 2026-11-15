// SPDX-License-Identifier: GPL-3.0-or-later

//! A [`MachineTerm`] rescales the seconds of
//! [`predict_run`](super::prediction::predict_run) per stage; a
//! [`MachineHistory`] learns it from a machine's finished runs. Both are plain
//! data: nothing here reads a file or the environment. The first run on a new
//! machine uses a core-count estimate that can read high. The history file is
//! `machine-history.json` in the per-user data folder; deleting it resets it.

use serde::{Deserialize, Serialize};

use super::prediction::RawSource;
use crate::host::{self, FactsOrigin, HostFacts};
use crate::io::{DevelopStore, ReaderKind};
use crate::pipeline::RunPlanFeatures;
use crate::timing::StageTimings;

/// Runs kept per stage and pool. Four runs average out the noise of one
/// (finalize alone varies by a factor of three) and still follow a machine
/// whose state changed, such as a new driver.
const WINDOW: usize = 4;

/// A learned factor outside this range reads as a failed measurement, not a
/// machine, and is held at the nearer end.
const CLAMP: (f64, f64) = (0.02, 20.0);

/// The logical thread count of the machine the shipped seconds were
/// measured on, a 4-core CPU with 8 threads.
const PRIOR_THREADS: f64 = 8.0;

/// The core-count prior never claims more than 4x the shipped speed:
/// core count explains only part of the gap between machines.
const PRIOR_FLOOR: f64 = 0.25;

/// Version of the shipped seconds the history was learned against. A file
/// of another revision is not applied, because its factors answer a
/// different table.
pub const REVISION: u32 = 1;

/// Which tier produced a [`MachineTerm`]; [`MachineHistory::term_for`] takes
/// the first that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TermSource {
    /// History for this machine, backend and class of run: one factor per
    /// stage, each the pooled ratio of the last four measured to shipped
    /// seconds. Finalize is pooled over every run of the machine, since the
    /// disk and the output bind it, not the class.
    #[serde(rename = "history")]
    History,
    /// History for this machine and backend but a new class: one scalar on
    /// setup, develop, align and fuse, pooled over the computing stages of the
    /// last four runs of other classes. Finalize stays as shipped.
    #[serde(rename = "compute scalar")]
    ComputeScalar,
    /// No history: `max(8 / logical cores, 0.25)` on the same stages, from
    /// core count alone, so it can read high or low. A GPU plan gets it only
    /// with a discrete adapter already up; a plan whose GPU part is the
    /// alignment alone keeps the shipped seconds, since its adapter is not
    /// open before the run.
    #[serde(rename = "core prior")]
    CorePrior,
    /// No host facts, or a tier that scales by exactly 1: the shipped seconds.
    #[serde(rename = "shipped")]
    Shipped,
}

impl TermSource {
    /// The name shown to the user and written into reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::ComputeScalar => "compute scalar",
            Self::CorePrior => "core prior",
            Self::Shipped => "shipped",
        }
    }
}

impl std::fmt::Display for TermSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One multiplier per predicted stage. `warp_write` is never predicted, so
/// it has none.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StageMultipliers {
    pub setup: f64,
    pub develop: f64,
    pub align: f64,
    pub fuse: f64,
    pub finalize: f64,
}

impl StageMultipliers {
    /// Every stage as shipped.
    pub const IDENTITY: Self = Self::compute(1.0);

    /// `k` on the computing stages, finalize as shipped.
    const fn compute(k: f64) -> Self {
        Self {
            setup: k,
            develop: k,
            align: k,
            fuse: k,
            finalize: 1.0,
        }
    }
}

/// How the shipped seconds are rescaled for one machine, and where the
/// rescaling came from. The multipliers come from the machine's own finished
/// runs where it has any ([`TermSource::History`]); otherwise they are a
/// core-count estimate that can read high on the first run
/// ([`TermSource::CorePrior`]), or 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MachineTerm {
    pub multipliers: StageMultipliers,
    pub source: TermSource,
}

impl MachineTerm {
    /// The shipped seconds, unscaled: the term of a machine nothing is
    /// known about.
    pub const IDENTITY: Self = Self {
        multipliers: StageMultipliers::IDENTITY,
        source: TermSource::Shipped,
    };
}

/// The machine a history belongs to: its CPU brand string and logical core
/// count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineKey {
    cpu_brand: String,
    logical_cores: usize,
}

impl MachineKey {
    /// `None` for an empty brand or no cores.
    pub fn new(cpu_brand: &str, logical_cores: usize) -> Option<Self> {
        let cpu_brand = cpu_brand.trim();
        (!cpu_brand.is_empty() && logical_cores > 0).then(|| Self {
            cpu_brand: cpu_brand.to_string(),
            logical_cores,
        })
    }

    /// `None` without a brand, or when `facts` come from the fallback.
    pub fn from_facts(cpu_brand: Option<&str>, facts: &HostFacts) -> Option<Self> {
        if facts.origin() == FactsOrigin::Fallback {
            return None;
        }
        Self::new(cpu_brand?, facts.logical_cores())
    }

    /// The key of this machine, as the process reads it.
    pub fn current() -> Option<Self> {
        Self::from_facts(host::cpu_brand().as_deref(), &host::snapshot())
    }
}

/// Class of run a factor was learned on: everything the shipped seconds
/// are keyed by, besides the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClassKey {
    reader: String,
    raw_extension: Option<String>,
    raw_store: Option<DevelopStore>,
    algorithm: String,
    align_gpu: bool,
}

impl ClassKey {
    fn of(features: &RunPlanFeatures, raw: Option<RawSource<'_>>) -> Self {
        let raw = raw.filter(|_| features.reader == ReaderKind::Raw);
        Self {
            reader: match features.reader {
                ReaderKind::Mmap => "mmap",
                ReaderKind::Decoded => "decoded",
                ReaderKind::Raw => "raw",
                ReaderKind::Jpeg => "jpeg",
            }
            .to_string(),
            raw_extension: raw.map(|raw| raw.extension.to_ascii_lowercase()),
            raw_store: raw.map(|raw| raw.store),
            algorithm: features.algorithm.clone(),
            align_gpu: features.align_gpu,
        }
    }
}

/// One measured and shipped duration of the same work.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Pair {
    shipped: f64,
    actual: f64,
}

/// The computing stages of one run, summed, with its place in the machine's
/// order of runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct ComputeRun {
    seq: u64,
    pair: Pair,
}

/// What one class and backend taught: pairs per computing stage (setup,
/// develop, align, fuse) and per-run compute sums.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ClassRecord {
    gpu: bool,
    class: ClassKey,
    stages: [Vec<Pair>; 4],
    runs: Vec<ComputeRun>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct MachineRecord {
    key: MachineKey,
    /// The adapter the machine's GPU runs used, once one has been seen.
    adapter: Option<String>,
    seq: u64,
    classes: Vec<ClassRecord>,
    /// Finalize pairs of every run of the machine.
    finalize: Vec<Pair>,
}

/// What a finished run reports about itself, beside its seconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOutcome<'a> {
    /// Fuse tiles taken from a resume ledger.
    pub resumed_units: usize,
    pub paused: bool,
    pub backed_off: bool,
    /// The run fused on the host although it planned a GPU.
    pub gpu_fallback: bool,
    /// The alignment came from the stage cache.
    pub align_cached: bool,
    /// The GPU adapter the run used, when it used one.
    pub adapter: Option<&'a str>,
}

/// The per-stage factors learned from the finished runs of every machine that
/// shared the history. Each run that completed without a pause, resume, memory
/// back-off or host fallback adds one measured-over-shipped pair per stage;
/// the last four pairs decide, so the estimate follows a machine whose
/// speed changes. A history of another revision or one that does not parse is
/// the empty history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineHistory {
    revision: u32,
    machines: Vec<MachineRecord>,
}

impl Default for MachineHistory {
    fn default() -> Self {
        Self {
            revision: REVISION,
            machines: Vec::new(),
        }
    }
}

/// `sum(actual) / sum(shipped)` over the last four pairs, clamped; 1
/// without pairs.
fn factor(pairs: &[Pair]) -> f64 {
    let pairs = &pairs[pairs.len().saturating_sub(WINDOW)..];
    if pairs.is_empty() {
        return 1.0;
    }
    let shipped: f64 = pairs.iter().map(|p| p.shipped).sum();
    let actual: f64 = pairs.iter().map(|p| p.actual).sum();
    (actual / shipped).clamp(CLAMP.0, CLAMP.1)
}

fn push_window<T>(window: &mut Vec<T>, item: T) {
    window.push(item);
    if window.len() > WINDOW {
        window.remove(0);
    }
}

/// A pair worth learning from: both durations finite and positive.
fn pair(shipped: Option<f64>, actual: Option<f64>) -> Option<Pair> {
    let (shipped, actual) = (shipped?, actual?);
    (shipped.is_finite() && shipped > 0.0 && actual.is_finite() && actual > 0.0)
        .then_some(Pair { shipped, actual })
}

fn compute_stages(t: &StageTimings) -> [Option<f64>; 4] {
    [t.setup, t.develop, t.align, t.fuse]
}

impl MachineHistory {
    /// The history in `bytes`; empty for bytes that do not parse and for a
    /// history of another [`REVISION`].
    pub fn parse(bytes: &[u8]) -> Self {
        serde_json::from_slice::<Self>(bytes)
            .ok()
            .filter(|history| history.revision == REVISION)
            .unwrap_or_default()
    }

    /// The term for a run of `features` on the machine `key`: the first
    /// [`TermSource`] tier that applies, in the order of its variants.
    /// `discrete_adapter` is whether the run's fuse backend is a discrete GPU.
    pub fn term_for(
        &self,
        key: Option<&MachineKey>,
        features: &RunPlanFeatures,
        raw: Option<RawSource<'_>>,
        discrete_adapter: bool,
    ) -> MachineTerm {
        let Some(key) = key else {
            return MachineTerm::IDENTITY;
        };
        let record = if self.revision == REVISION {
            self.machines.iter().find(|m| m.key == *key)
        } else {
            None
        };
        if let Some(record) = record {
            let class = ClassKey::of(features, raw);
            let own = record
                .classes
                .iter()
                .find(|c| c.gpu == features.gpu && c.class == class);
            if let Some(own) = own {
                let [setup, develop, align, fuse] = own.stages.each_ref().map(|p| factor(p));
                return MachineTerm {
                    multipliers: StageMultipliers {
                        setup,
                        develop,
                        align,
                        fuse,
                        finalize: factor(&record.finalize),
                    },
                    source: TermSource::History,
                };
            }
            let mut others: Vec<ComputeRun> = record
                .classes
                .iter()
                .filter(|c| c.gpu == features.gpu)
                .flat_map(|c| c.runs.iter().copied())
                .collect();
            others.sort_by_key(|run| run.seq);
            let others: Vec<Pair> = others.iter().map(|run| run.pair).collect();
            if !others.is_empty() {
                return MachineTerm {
                    multipliers: StageMultipliers::compute(factor(&others)),
                    source: TermSource::ComputeScalar,
                };
            }
        }
        let prior_applies = if features.gpu {
            discrete_adapter
        } else {
            !features.align_gpu
        };
        let k = (PRIOR_THREADS / key.logical_cores as f64).max(PRIOR_FLOOR);
        if prior_applies && k != 1.0 {
            MachineTerm {
                multipliers: StageMultipliers::compute(k),
                source: TermSource::CorePrior,
            }
        } else {
            MachineTerm::IDENTITY
        }
    }

    /// Learns from one finished run: `shipped` is the shipped prediction of
    /// the run's own plan and `actual` what each stage took. A resumed,
    /// paused, backed-off or host-fallback run is skipped whole, a cached
    /// alignment only its align stage. A different adapter name on the
    /// machine, or a different [`REVISION`], discards what was learned.
    pub fn observe(
        &mut self,
        key: &MachineKey,
        features: &RunPlanFeatures,
        raw: Option<RawSource<'_>>,
        shipped: &StageTimings,
        actual: &StageTimings,
        outcome: &RunOutcome<'_>,
    ) {
        if self.revision != REVISION {
            *self = Self::default();
        }
        if outcome.resumed_units > 0 || outcome.paused || outcome.backed_off || outcome.gpu_fallback
        {
            return;
        }
        let mut shipped_stages = compute_stages(shipped);
        if outcome.align_cached {
            shipped_stages[2] = None;
        }
        let pairs = shipped_stages
            .into_iter()
            .zip(compute_stages(actual))
            .map(|(s, a)| pair(s, a))
            .collect::<Vec<_>>();
        let finalize = pair(shipped.finalize, actual.finalize);
        if pairs.iter().all(Option::is_none) && finalize.is_none() {
            return;
        }

        let index = match self.machines.iter().position(|m| m.key == *key) {
            Some(index) => index,
            None => {
                self.machines.push(MachineRecord {
                    key: key.clone(),
                    adapter: None,
                    seq: 0,
                    classes: Vec::new(),
                    finalize: Vec::new(),
                });
                self.machines.len() - 1
            }
        };
        let record = &mut self.machines[index];
        if let Some(name) = outcome.adapter {
            match record.adapter.as_deref() {
                Some(known) if known != name => {
                    record.classes.clear();
                    record.finalize.clear();
                    record.seq = 0;
                    record.adapter = Some(name.to_string());
                }
                Some(_) => {}
                None => record.adapter = Some(name.to_string()),
            }
        }
        record.seq += 1;
        let seq = record.seq;

        let class = ClassKey::of(features, raw);
        let slot = match record
            .classes
            .iter()
            .position(|c| c.gpu == features.gpu && c.class == class)
        {
            Some(slot) => slot,
            None => {
                record.classes.push(ClassRecord {
                    gpu: features.gpu,
                    class,
                    stages: Default::default(),
                    runs: Vec::new(),
                });
                record.classes.len() - 1
            }
        };
        let class = &mut record.classes[slot];
        let (mut shipped_sum, mut actual_sum) = (0.0, 0.0);
        for (window, pair) in class.stages.iter_mut().zip(&pairs) {
            if let Some(pair) = *pair {
                shipped_sum += pair.shipped;
                actual_sum += pair.actual;
                push_window(window, pair);
            }
        }
        if shipped_sum > 0.0 {
            push_window(
                &mut class.runs,
                ComputeRun {
                    seq,
                    pair: Pair {
                        shipped: shipped_sum,
                        actual: actual_sum,
                    },
                },
            );
        }
        if let Some(finalize) = finalize {
            push_window(&mut record.finalize, finalize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::prediction::predict_run;

    fn plan(gpu: bool, align_gpu: bool, algorithm: &str) -> RunPlanFeatures {
        RunPlanFeatures {
            frame_count: 30,
            frame_pixels: 15_000_000,
            source_bits_per_sample: 16,
            reader: ReaderKind::Mmap,
            gpu,
            align_gpu,
            workers: 8,
            algorithm: algorithm.to_string(),
            align_cache_hit: false,
            resumed_units: 0,
        }
    }

    fn cpu_plan() -> RunPlanFeatures {
        plan(false, false, "laplacian")
    }

    fn key(cores: usize) -> MachineKey {
        MachineKey::new("Test CPU", cores).expect("a brand and cores")
    }

    fn secs(
        setup: f64,
        develop: Option<f64>,
        align: f64,
        fuse: f64,
        finalize: f64,
    ) -> StageTimings {
        StageTimings {
            setup: Some(setup),
            develop,
            align: Some(align),
            fuse: Some(fuse),
            warp_write: None,
            finalize: Some(finalize),
        }
    }

    /// Setup 1, align 10, fuse 20, finalize 5 shipped; `scale` times that
    /// on the computing stages, `fin` times the finalize seconds measured.
    fn run_at(scale: f64, fin: f64) -> (StageTimings, StageTimings) {
        (
            secs(1.0, None, 10.0, 20.0, 5.0),
            secs(scale, None, 10.0 * scale, 20.0 * scale, 5.0 * fin),
        )
    }

    fn learn(
        history: &mut MachineHistory,
        key: &MachineKey,
        features: &RunPlanFeatures,
        (shipped, actual): &(StageTimings, StageTimings),
        outcome: &RunOutcome<'_>,
    ) {
        history.observe(key, features, None, shipped, actual, outcome);
    }

    fn term(history: &MachineHistory, key: &MachineKey, features: &RunPlanFeatures) -> MachineTerm {
        history.term_for(Some(key), features, None, false)
    }

    fn mults(t: &MachineTerm) -> [f64; 5] {
        let m = t.multipliers;
        [m.setup, m.develop, m.align, m.fuse, m.finalize]
    }

    #[test]
    fn without_host_facts_the_shipped_seconds_stand() {
        let history = MachineHistory::default();
        assert_eq!(
            history.term_for(None, &cpu_plan(), None, true),
            MachineTerm::IDENTITY
        );
        assert_eq!(MachineKey::new("", 8), None);
        assert_eq!(MachineKey::new("CPU", 0), None);
        let probed = HostFacts::new(1 << 30, 1 << 29, None, None, 8);
        assert_eq!(MachineKey::from_facts(None, &probed), None, "no brand");
        assert_eq!(
            MachineKey::from_facts(Some("CPU"), &HostFacts::fallback()),
            None,
            "fallback facts"
        );
        assert_eq!(
            MachineKey::from_facts(Some(" CPU "), &probed),
            MachineKey::new("CPU", 8)
        );
    }

    #[test]
    fn the_core_prior_scales_the_computing_stages_by_eight_over_cores() {
        let history = MachineHistory::default();
        let cases = [(32, 0.25), (64, 0.25), (12, 8.0 / 12.0), (4, 2.0)];
        for (cores, k) in cases {
            let t = term(&history, &key(cores), &cpu_plan());
            assert_eq!(t.source, TermSource::CorePrior, "{cores} cores");
            assert_eq!(mults(&t), [k, k, k, k, 1.0], "{cores} cores");
        }
    }

    #[test]
    fn eight_logical_cores_leave_the_shipped_seconds_bit_for_bit() {
        let t = term(&MachineHistory::default(), &key(8), &cpu_plan());
        assert_eq!(t, MachineTerm::IDENTITY);
        let shipped = predict_run(&cpu_plan(), true, None, &MachineTerm::IDENTITY).unwrap();
        let scaled = predict_run(&cpu_plan(), true, None, &t).unwrap();
        assert_eq!(
            scaled.total_seconds.to_bits(),
            shipped.total_seconds.to_bits()
        );
    }

    #[test]
    fn a_gpu_plan_takes_the_prior_only_with_a_discrete_adapter() {
        let history = MachineHistory::default();
        let gpu = plan(true, true, "laplacian");
        let k = key(32);
        let discrete = history.term_for(Some(&k), &gpu, None, true);
        assert_eq!(discrete.source, TermSource::CorePrior);
        assert_eq!(mults(&discrete), [0.25, 0.25, 0.25, 0.25, 1.0]);
        let integrated = history.term_for(Some(&k), &gpu, None, false);
        assert_eq!(integrated, MachineTerm::IDENTITY);
    }

    #[test]
    fn a_plan_whose_gpu_part_is_the_alignment_alone_keeps_the_shipped_seconds() {
        let align_only = plan(false, true, "depth-map");
        for discrete in [false, true] {
            let t = MachineHistory::default().term_for(Some(&key(32)), &align_only, None, discrete);
            assert_eq!(t, MachineTerm::IDENTITY, "discrete: {discrete}");
        }
    }

    #[test]
    fn a_class_seen_before_gets_its_own_stage_factors() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let features = cpu_plan();
        learn(
            &mut history,
            &k,
            &features,
            &run_at(0.5, 3.0),
            &RunOutcome::default(),
        );
        let t = term(&history, &k, &features);
        assert_eq!(t.source, TermSource::History);
        assert_eq!(mults(&t), [0.5, 1.0, 0.5, 0.5, 3.0]);
        let other_machine = term(&history, &key(32), &features);
        assert_eq!(other_machine.source, TermSource::CorePrior);
    }

    #[test]
    fn finalize_is_pooled_over_every_class_and_backend_of_the_machine() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let a = cpu_plan();
        let b = plan(true, false, "depth-map");
        learn(
            &mut history,
            &k,
            &a,
            &run_at(1.0, 2.0),
            &RunOutcome::default(),
        );
        learn(
            &mut history,
            &k,
            &b,
            &run_at(1.0, 4.0),
            &RunOutcome::default(),
        );
        // Pairs (5, 10) and (5, 20): sum(actual) / sum(shipped) = 3, where
        // the class's own pair alone would give 2.
        assert_eq!(mults(&term(&history, &k, &a))[4], 3.0);
        assert_eq!(mults(&term(&history, &k, &b))[4], 3.0);
    }

    #[test]
    fn a_new_class_takes_one_compute_scalar_and_leaves_finalize() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let seen = plan(false, false, "depth-map");
        learn(
            &mut history,
            &k,
            &seen,
            &run_at(2.0, 3.0),
            &RunOutcome::default(),
        );
        let t = term(&history, &k, &cpu_plan());
        assert_eq!(t.source, TermSource::ComputeScalar);
        assert_eq!(mults(&t), [2.0, 2.0, 2.0, 2.0, 1.0]);
        let gpu = history.term_for(Some(&k), &plan(true, false, "laplacian"), None, true);
        assert_eq!(
            gpu.source,
            TermSource::CorePrior,
            "another backend has no history"
        );
    }

    #[test]
    fn the_compute_scalar_pools_the_last_four_runs_of_other_classes() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let b = plan(false, false, "depth-map");
        let c = plan(false, false, "weighted-average");
        let ok = RunOutcome::default();
        for (features, scale) in [(&b, 1.0), (&c, 1.0), (&b, 1.0), (&c, 1.0), (&b, 5.0)] {
            learn(&mut history, &k, features, &run_at(scale, 1.0), &ok);
        }
        // The first run is out of the window: (1 + 1 + 1 + 5) / 4.
        assert_eq!(mults(&term(&history, &k, &cpu_plan()))[0], 2.0);
    }

    #[test]
    fn a_fifth_run_of_a_class_evicts_the_first() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let ok = RunOutcome::default();
        for scale in [9.0, 1.0, 1.0, 1.0] {
            learn(&mut history, &k, &cpu_plan(), &run_at(scale, 1.0), &ok);
        }
        assert_eq!(
            mults(&term(&history, &k, &cpu_plan()))[3],
            3.0,
            "(9+1+1+1)/4"
        );
        learn(&mut history, &k, &cpu_plan(), &run_at(1.0, 1.0), &ok);
        assert_eq!(mults(&term(&history, &k, &cpu_plan()))[3], 1.0);
    }

    #[test]
    fn stage_factors_weigh_each_run_by_its_length() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let ok = RunOutcome::default();
        let long = (
            secs(1.0, None, 1.0, 90.0, 1.0),
            secs(1.0, None, 1.0, 90.0, 1.0),
        );
        let short = (
            secs(1.0, None, 1.0, 10.0, 1.0),
            secs(1.0, None, 1.0, 20.0, 1.0),
        );
        learn(&mut history, &k, &cpu_plan(), &short, &ok);
        learn(&mut history, &k, &cpu_plan(), &long, &ok);
        // 110 / 100, not the mean of the ratios 2 and 1.
        assert_eq!(mults(&term(&history, &k, &cpu_plan()))[3], 1.1);
    }

    #[test]
    fn a_run_that_says_little_about_the_machine_teaches_nothing() {
        let k = key(16);
        let reasons = [
            RunOutcome {
                resumed_units: 3,
                ..RunOutcome::default()
            },
            RunOutcome {
                paused: true,
                ..RunOutcome::default()
            },
            RunOutcome {
                backed_off: true,
                ..RunOutcome::default()
            },
            RunOutcome {
                gpu_fallback: true,
                ..RunOutcome::default()
            },
        ];
        for outcome in reasons {
            let mut history = MachineHistory::default();
            learn(&mut history, &k, &cpu_plan(), &run_at(0.5, 3.0), &outcome);
            assert_eq!(history, MachineHistory::default(), "{outcome:?}");
        }
    }

    #[test]
    fn a_cached_alignment_skips_only_the_align_stage() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let cached = RunOutcome {
            align_cached: true,
            ..RunOutcome::default()
        };
        learn(&mut history, &k, &cpu_plan(), &run_at(0.5, 2.0), &cached);
        let t = term(&history, &k, &cpu_plan());
        assert_eq!(mults(&t), [0.5, 1.0, 1.0, 0.5, 2.0]);
    }

    #[test]
    fn factors_clamp_at_both_ends() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let ok = RunOutcome::default();
        learn(&mut history, &k, &cpu_plan(), &run_at(1000.0, 1000.0), &ok);
        assert_eq!(
            mults(&term(&history, &k, &cpu_plan())),
            [20.0, 1.0, 20.0, 20.0, 20.0]
        );
        let mut history = MachineHistory::default();
        learn(&mut history, &k, &cpu_plan(), &run_at(0.0001, 0.0001), &ok);
        assert_eq!(
            mults(&term(&history, &k, &cpu_plan())),
            [0.02, 1.0, 0.02, 0.02, 0.02]
        );
        let scalar = term(&history, &k, &plan(false, false, "depth-map"));
        assert_eq!(scalar.source, TermSource::ComputeScalar);
        assert_eq!(mults(&scalar)[0], 0.02, "the scalar clamps too");
    }

    #[test]
    fn a_pair_with_a_nan_zero_or_infinite_duration_is_dropped() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let ok = RunOutcome::default();
        let shipped = secs(1.0, None, 10.0, 20.0, 5.0);
        let actual = secs(0.5, None, f64::NAN, f64::INFINITY, 0.0);
        history.observe(&k, &cpu_plan(), None, &shipped, &actual, &ok);
        assert_eq!(
            mults(&term(&history, &k, &cpu_plan())),
            [0.5, 1.0, 1.0, 1.0, 1.0]
        );
        let junk = secs(f64::NAN, None, 0.0, f64::INFINITY, f64::NAN);
        let mut empty = MachineHistory::default();
        empty.observe(&k, &cpu_plan(), None, &junk, &junk, &ok);
        assert_eq!(empty, MachineHistory::default());
    }

    #[test]
    fn another_adapter_name_resets_the_machine() {
        let mut history = MachineHistory::default();
        let k = key(16);
        let gpu = plan(true, false, "laplacian");
        let on = |name| RunOutcome {
            adapter: Some(name),
            ..RunOutcome::default()
        };
        learn(&mut history, &k, &gpu, &run_at(2.0, 1.0), &on("Adapter A"));
        learn(&mut history, &k, &gpu, &run_at(2.0, 1.0), &on("Adapter A"));
        learn(
            &mut history,
            &k,
            &gpu,
            &run_at(4.0, 1.0),
            &RunOutcome::default(),
        );
        assert_eq!(
            mults(&term(&history, &k, &gpu))[3],
            8.0 / 3.0,
            "kept: no adapter named"
        );
        learn(&mut history, &k, &gpu, &run_at(5.0, 1.0), &on("Adapter B"));
        assert_eq!(
            mults(&term(&history, &k, &gpu))[3],
            5.0,
            "only the new adapter's run"
        );
    }

    #[test]
    fn a_history_of_another_revision_starts_empty() {
        let k = key(16);
        let mut history = MachineHistory::default();
        learn(
            &mut history,
            &k,
            &cpu_plan(),
            &run_at(0.5, 1.0),
            &RunOutcome::default(),
        );
        let mut text = serde_json::to_value(&history).unwrap();
        text["revision"] = (REVISION + 1).into();
        assert_eq!(
            MachineHistory::parse(&serde_json::to_vec(&text).unwrap()),
            MachineHistory::default()
        );
        let mut stale = serde_json::from_value::<MachineHistory>(text).unwrap();
        assert_eq!(term(&stale, &k, &cpu_plan()).source, TermSource::CorePrior);
        learn(
            &mut stale,
            &k,
            &cpu_plan(),
            &run_at(2.0, 1.0),
            &RunOutcome::default(),
        );
        assert_eq!(stale.revision, REVISION);
        assert_eq!(
            mults(&term(&stale, &k, &cpu_plan()))[3],
            2.0,
            "only the new run"
        );
    }

    #[test]
    fn a_history_survives_serialization_and_garbage_is_empty() {
        let k = key(16);
        let mut history = MachineHistory::default();
        learn(
            &mut history,
            &k,
            &cpu_plan(),
            &run_at(0.5, 2.0),
            &RunOutcome::default(),
        );
        let bytes = serde_json::to_vec(&history).unwrap();
        assert_eq!(MachineHistory::parse(&bytes), history);
        for junk in [&b""[..], b"{not json", b"[1, 2]", b"{\"revision\": 1}"] {
            assert_eq!(MachineHistory::parse(junk), MachineHistory::default());
        }
    }

    #[test]
    fn the_term_names_its_source() {
        let names: Vec<_> = [
            TermSource::History,
            TermSource::ComputeScalar,
            TermSource::CorePrior,
            TermSource::Shipped,
        ]
        .into_iter()
        .map(|s| (s.to_string(), serde_json::to_value(s).unwrap()))
        .collect();
        for (shown, written) in names {
            assert_eq!(written, serde_json::Value::String(shown));
        }
    }
}
