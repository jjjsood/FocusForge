// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge_core::pipeline::run` on its own, over `fixtures/mantis-512`:
//! parameter resolution against the frames the driver actually opens, the
//! `Cancelling` interval during alignment, and the pause/resume segment
//! contract. `focusforge-cli` and `focusforge-gui` are this driver's two
//! callers; this file exercises the driver directly, through neither.

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus, StackAlgorithm,
    TileCtx, TileOutput, TileReader,
};
use focusforge_core::align::warp::WarpedTile;
use focusforge_core::align::{AlignError, FailurePolicy, Transform};
use focusforge_core::display::{PreviewCost, PreviewKind, PreviewKinds, StagePreview};
use focusforge_core::equalize::{EqualizeSettings, UNIT_GAIN};
use focusforge_core::fuse::laplacian::Laplacian;
use focusforge_core::gpu::{BackendInfo, TileBackend};
use focusforge_core::io::{FrameDesc, TiffHeader, TiffSpec, TiffWriter};
use focusforge_core::job::{
    AlignIdentity, ControlState, FrameFingerprint, JobControl, JobProgress, JobStage,
    StackFingerprint, StageCache, StageKey,
};
use focusforge_core::params::{ParamDesc, ParamValue, ParamValues, Provenance};
use focusforge_core::pipeline::{
    AlignRequest, FreeSpaceProbe, FuseOutcome, FuseOutput, FuseProduct, FuseReport, FuseRequest,
    PipelineError, RunStage, StageObserver, VolumeShortfall, VolumeSpace,
};
use focusforge_core::project::{self, Project};
use focusforge_core::registry;
use focusforge_core::tile::{Rect, SampleFormat, Tile, TileError};
use focusforge_core::zmap::ZMapReader;
use raw_dng::{DngSpec, write_dng};

fn mantis_frames(count: usize) -> Vec<PathBuf> {
    let dir = common::mantis_512_dir().join("frames");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("frames dir must exist")
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries.truncate(count);
    entries
}

/// `count` frames from `fixtures/mantis-64`, its three frames in order and
/// repeated when `count` exceeds three.
fn mantis_64_frames(count: usize) -> Vec<PathBuf> {
    let dir = common::fixtures_dir().join("mantis-64/frames");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("frames dir must exist")
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries.into_iter().cycle().take(count).collect()
}

fn scratch(label: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("pipeline_{label}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
    dir
}

/// Seals `held` as the root of a new project at `target`.
fn seal_chain_alone(target: &Path, held: &project::HeldCheckpoint) {
    let entry = project::InMemoryCheckpoint {
        kind: project::InMemoryKind::Fused(held.clone()),
        parent: None,
    };
    project::seal_in_memory(target, &[entry], &[], None, false)
        .expect("the held checkpoint must seal");
}

/// A request over `frames`, no alignment, the golden tile geometry unless
/// overridden by the caller.
fn base_request(label: &str, frames: Vec<PathBuf>, explicit: ParamValues) -> FuseRequest {
    let root = scratch(label);
    FuseRequest {
        frames,
        output: FuseOutput::Seal(root.join("project.ffp")),
        scratch_root: root.join("scratch"),
        staging_key: label.to_string(),
        algorithm: registry::find_stack("laplacian").expect("laplacian must be registered"),
        explicit,
        align: None,
        crop: false,
        equalize: None,
        tile_size: Some(448),
        max_host_memory: None,
        backend: None,
        align_backend: None,
        resume: false,
        cache: true,
        poll: Duration::from_millis(5),
        free_space: None,
    }
}

#[derive(Default)]
struct NoopObserver;
impl StageObserver for NoopObserver {}

/// Every path is on the volume `name` with `free` bytes free.
fn volume(name: &str, free: u64) -> std::io::Result<VolumeSpace> {
    Ok(VolumeSpace {
        volume: name.into(),
        free,
    })
}

/// A probe answering through `answer` that records every path it is asked about.
fn recording_probe(
    answer: impl Fn(&Path) -> std::io::Result<VolumeSpace> + Send + Sync + 'static,
) -> (FreeSpaceProbe, Arc<Mutex<Vec<PathBuf>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    let probe: FreeSpaceProbe = Arc::new(move |path: &Path| {
        log.lock().expect("probe log").push(path.to_path_buf());
        answer(path)
    });
    (probe, asked)
}

/// Everything under `out_dir` is the volume "output", everything else "scratch".
fn two_volumes(out_dir: &Path, scratch_free: u64, output_free: u64) -> FreeSpaceProbe {
    let out_dir = out_dir.to_path_buf();
    recording_probe(move |path| {
        if path.starts_with(&out_dir) {
            volume("output", output_free)
        } else {
            volume("scratch", scratch_free)
        }
    })
    .0
}

fn run_plain(request: &FuseRequest) -> Result<FuseOutcome, PipelineError> {
    focusforge_core::pipeline::run(request, &NoopObserver, JobControl::new(), None)
}

/// The shortfalls `request` is refused with under `probe`.
fn refusal(request: &mut FuseRequest, probe: FreeSpaceProbe) -> Vec<VolumeShortfall> {
    request.free_space = Some(probe);
    match run_plain(request) {
        Err(PipelineError::DiskSpace(shortfalls)) => shortfalls,
        Err(other) => panic!("expected a disk-space refusal, got {other:?}"),
        Ok(_) => panic!("a volume that is short must refuse the run"),
    }
}

/// The one shortfall `request` is refused with when its single volume has
/// nothing free.
fn needed_on_one_volume(request: &mut FuseRequest) -> u64 {
    let shortfalls = refusal(request, recording_probe(|_| volume("only", 0)).0);
    assert_eq!(shortfalls.len(), 1, "{shortfalls:?}");
    shortfalls[0].needed
}

/// A request whose result goes to a directory of its own, apart from the
/// scratch root; returns that directory and the run directory.
fn split_request(label: &str, file: &str) -> (FuseRequest, PathBuf, PathBuf) {
    let mut request = base_request(label, mantis_frames(2), ParamValues::new());
    let out_dir = scratch(&format!("{label}_out"));
    request.output = FuseOutput::Seal(out_dir.join(file));
    let root = request
        .scratch_root
        .parent()
        .expect("run directory")
        .to_path_buf();
    (request, root, out_dir)
}

fn hold_beside_scratch(request: &mut FuseRequest) {
    request.output = FuseOutput::Hold(request.scratch_root.with_file_name("held"));
}

#[test]
fn run_space_staged_bytes_cover_written_staging() {
    for (frames, tile_size) in [(2, 448), (3, 448), (2, 256), (3, 256)] {
        let label = format!("space_staged_{frames}_{tile_size}");
        let make = || {
            // Three levels keep the 256-pixel tile larger than the support margin.
            let mut explicit = ParamValues::new();
            explicit.insert("levels".to_string(), ParamValue::Int(3));
            let mut request = base_request(&label, mantis_frames(frames), explicit);
            request.tile_size = Some(tile_size);
            hold_beside_scratch(&mut request);
            request
        };
        let needed = needed_on_one_volume(&mut make());
        let FuseOutcome::Completed(report) = run_plain(&make()).expect("the hold run completes")
        else {
            panic!("expected a completed run");
        };
        let FuseProduct::Held(held) = report.product else {
            panic!("a hold request reports a held checkpoint");
        };
        let written: u64 = [&held.composite, &held.zmap]
            .iter()
            .map(|file| std::fs::metadata(file).expect("held plane").len())
            .sum();
        assert!(
            needed >= written && needed - written < 256 * 1024,
            "{frames} frames, tile {tile_size}: needed {needed}, staged {written}"
        );
    }
}

#[test]
fn tiff_run_refuses_when_run_space_is_short() {
    let label = "space_short";
    let needed = needed_on_one_volume(&mut base_request(
        label,
        mantis_frames(2),
        ParamValues::new(),
    ));
    for free in [needed - 1, 0] {
        let mut request = base_request(label, mantis_frames(2), ParamValues::new());
        let root = request
            .scratch_root
            .parent()
            .expect("run directory")
            .to_path_buf();
        request.free_space = Some(recording_probe(move |_| volume("only", free)).0);
        let Err(error) = run_plain(&request) else {
            panic!("a run that does not fit must be refused");
        };
        let PipelineError::DiskSpace(shortfalls) = &error else {
            panic!("expected a disk-space refusal, got {error:?}");
        };
        assert_eq!(
            shortfalls.as_slice(),
            [VolumeShortfall {
                path: root,
                needed,
                free
            }]
        );
        let text = error.to_string();
        for want in [
            format!("{needed} bytes"),
            format!("{free} bytes"),
            format!("{:.1} GB", needed as f64 / 1e9),
            format!("{:.1} GB", free as f64 / 1e9),
        ] {
            assert!(text.contains(&want), "{want:?} missing from {text}");
        }
        assert!(!request.scratch_root.join("projects").exists());
        assert!(!request.scratch_root.join("resume").exists());
    }
}

#[test]
fn run_space_is_summed_per_volume() {
    // Two volumes: one shortfall each, carrying only its own term.
    let (mut request, root, out_dir) = split_request("space_volumes", "project.ffp");
    let split = refusal(&mut request, two_volumes(&out_dir, 0, 0));
    assert_eq!(split.len(), 2, "{split:?}");
    let term = |dir: &Path| {
        split
            .iter()
            .find(|shortfall| shortfall.path == dir)
            .unwrap_or_else(|| panic!("{dir:?} missing from {split:?}"))
            .needed
    };
    let (scratch_term, output_term) = (term(&root), term(&out_dir));
    assert!(scratch_term > 0 && output_term > 0);

    // One volume: the single shortfall carries the sum, at the first path.
    let (mut request, root, _) = split_request("space_volumes", "project.ffp");
    let one = refusal(&mut request, recording_probe(|_| volume("only", 0)).0);
    assert_eq!(
        one,
        [VolumeShortfall {
            path: root,
            needed: scratch_term + output_term,
            free: 0
        }]
    );

    // Only the output volume short: only it is named, with its own bytes.
    let (mut request, _, out_dir) = split_request("space_volumes", "project.ffp");
    let only_output = refusal(&mut request, two_volumes(&out_dir, u64::MAX, 0));
    assert_eq!(
        only_output,
        [VolumeShortfall {
            path: out_dir,
            needed: output_term,
            free: 0
        }]
    );

    // A hold asks about the scratch root alone.
    let (mut request, root, _) = split_request("space_volumes", "project.ffp");
    hold_beside_scratch(&mut request);
    let (probe, asked) = recording_probe(|_| volume("only", 0));
    refusal(&mut request, probe);
    assert_eq!(asked.lock().expect("probe log").as_slice(), [root]);
}

#[test]
fn run_proceeds_when_run_space_suffices() {
    let held = |free: Option<u64>| {
        let mut request = base_request("space_proceeds", mantis_64_frames(2), ParamValues::new());
        hold_beside_scratch(&mut request);
        request.free_space = free.map(|free| recording_probe(move |_| volume("only", free)).0);
        let FuseOutcome::Completed(report) = run_plain(&request).expect("the run completes") else {
            panic!("expected a completed run");
        };
        let FuseProduct::Held(held) = report.product else {
            panic!("a hold request reports a held checkpoint");
        };
        std::fs::read(&held.composite).expect("composite is readable")
    };
    let mut request = base_request("space_proceeds", mantis_64_frames(2), ParamValues::new());
    hold_beside_scratch(&mut request);
    let needed = needed_on_one_volume(&mut request);
    assert_eq!(held(Some(needed)), held(None));

    // A sealed run whose two volumes each hold exactly their term.
    let (mut request, _, out_dir) = split_request("space_proceeds_seal", "project.ffp");
    request.frames = mantis_64_frames(2);
    let split = refusal(&mut request, two_volumes(&out_dir, 0, 0));
    let exact = |dir: &Path| split.iter().find(|s| s.path == dir).expect("volume").needed;
    let (mut request, root, out_dir) = split_request("space_proceeds_seal", "project.ffp");
    request.frames = mantis_64_frames(2);
    request.free_space = Some(two_volumes(&out_dir, exact(&root), exact(&out_dir)));
    assert!(matches!(
        run_plain(&request).expect("a run that exactly fits completes"),
        FuseOutcome::Completed(_)
    ));
}

#[test]
fn run_space_query_failure_refuses_the_run() {
    let (mut request, _, out_dir) = split_request("space_query", "a/b/project.ffp");
    let failing = out_dir.clone();
    let (probe, asked) = recording_probe(move |path| {
        if path.starts_with(&failing) {
            Err(std::io::Error::other("output volume refused"))
        } else {
            volume("scratch", u64::MAX)
        }
    });
    request.free_space = Some(probe);
    let Err(error) = run_plain(&request) else {
        panic!("an unanswered query must refuse the run");
    };
    let PipelineError::DiskSpaceQuery { path, .. } = &error else {
        panic!("expected a query failure, got {error:?}");
    };
    assert_eq!(path, &out_dir);
    let text = error.to_string();
    assert!(text.contains("output volume refused"), "{text}");
    assert!(text.contains(&out_dir.display().to_string()), "{text}");
    assert_eq!(
        asked
            .lock()
            .expect("probe log")
            .iter()
            .filter(|path| path.starts_with(&out_dir))
            .collect::<Vec<_>>(),
        [&out_dir]
    );
    assert!(!request.scratch_root.join("projects").exists());
    assert!(!request.scratch_root.join("resume").exists());
}

#[test]
fn run_resolves_params_against_the_frames_it_opens() {
    // An explicit value wins over what `auto` would derive.
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(3));
    let request = base_request("resolves_explicit", mantis_frames(4), explicit);
    let outcome = focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
        .expect("an explicit-levels run over four frames must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    assert_eq!(
        report.frame_count, 4,
        "the report must count the frames it actually opened"
    );
    let levels = report
        .stack_params
        .get("levels")
        .expect("levels must be resolved");
    assert_eq!(levels.value, ParamValue::Int(3));
    assert_eq!(levels.provenance, Provenance::Explicit);

    // With nothing explicit, `auto` derives its value from the stack this
    // call opens (512x512, the fixture's own crop) -- never from a context
    // built earlier or elsewhere.
    let request = base_request("resolves_auto", mantis_frames(4), ParamValues::new());
    let outcome = focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
        .expect("an auto-levels run over four frames must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    let levels = report
        .stack_params
        .get("levels")
        .expect("levels must be resolved");
    assert_eq!(
        levels.value,
        ParamValue::Int(5),
        "auto_levels over the fixture's 512x512 crop must derive 5"
    );
    assert_eq!(levels.provenance, Provenance::Auto);
}

#[derive(Default)]
struct StageLog {
    stages: Mutex<Vec<&'static str>>,
}

impl StageObserver for StageLog {
    fn stage(&self, stage: RunStage) {
        let label = match stage {
            RunStage::Develop { .. } => "develop",
            RunStage::Prepare { .. } => "prepare",
            RunStage::Align { .. } => "align",
            RunStage::Fuse => "fuse",
            RunStage::Seal => "seal",
            RunStage::Finished => "finished",
        };
        self.stages
            .lock()
            .expect("no panic while logging")
            .push(label);
    }
}

#[test]
fn cancel_before_the_first_fuse_unit_reports_align_stage() {
    let align = AlignRequest {
        algorithm: registry::find_align("dummy-align").expect("dummy-align must be registered"),
        explicit: ParamValues::new(),
        lax: false,
    };
    let mut request = base_request("cancel_align", mantis_frames(2), ParamValues::new());
    request.align = Some(align);
    request.tile_size = Some(480); // the ragged variant: 512 does not divide evenly.

    let control = JobControl::new();
    // Requested before the driver ever runs: the align stage is still
    // reported, and the cancel lands before its first registration.
    control.cancel();
    assert_eq!(control.state(), ControlState::CancelRequested);

    let observer = StageLog::default();
    let outcome = focusforge_core::pipeline::run(&request, &observer, control, None)
        .expect("a pre-cancelled run must not error");

    match outcome {
        FuseOutcome::Cancelled {
            units_done,
            unit_total,
        } => {
            assert_eq!(units_done, 0);
            assert_eq!(unit_total, 0);
        }
        FuseOutcome::Completed(_) => panic!("a cancel requested before align must not complete"),
    }
    let stages = observer.stages.lock().unwrap();
    assert_eq!(
        *stages,
        vec!["align"],
        "the align stage must be reported, and no stage past it"
    );
}

/// Converges every frame and counts its `align_frame` calls; the call
/// numbered `cancel_on` requests a cancel on `control` before it returns.
/// Its schema is `pyramidal-ecc`'s, so a request can choose the reference
/// policy.
struct CancellingAlign {
    control: Arc<JobControl>,
    cancel_on: usize,
    calls: AtomicUsize,
}

impl AlignAlgorithm for CancellingAlign {
    fn id(&self) -> &str {
        "cancelling-align"
    }
    fn display_name(&self) -> &str {
        "Cancelling Align"
    }
    fn params(&self) -> &[ParamDesc] {
        registry::find_align("pyramidal-ecc")
            .expect("pyramidal-ecc must be registered")
            .params()
    }
    fn align_frame(
        &self,
        _ctx: &AlignContext<'_>,
        _frame: usize,
        _cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.cancel_on {
            self.control.cancel();
        }
        Ok(AlignOutcome {
            transform: Transform::identity(),
            status: AlignStatus::Converged,
            iterations: 1,
            residual: 1.0,
        })
    }
}

/// Every file below `dir`, at any depth.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

#[test]
fn cancel_during_alignment_stops_at_a_frame_boundary() {
    const CANCEL_ON: usize = 2;
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    // More frames than the cancelling call plus one in-flight registration
    // per other worker, so a cancel noticed only once alignment returns
    // would register frames past that bound.
    let frame_count = CANCEL_ON + workers + 8;
    let frames = mantis_64_frames(frame_count);

    for (policy, bound) in [("anchor", CANCEL_ON + workers - 1), ("chain", CANCEL_ON)] {
        let control = JobControl::new();
        let algorithm: &'static CancellingAlign = Box::leak(Box::new(CancellingAlign {
            control: Arc::clone(&control),
            cancel_on: CANCEL_ON,
            calls: AtomicUsize::new(0),
        }));
        let mut explicit = ParamValues::new();
        explicit.insert(
            "reference-policy".to_string(),
            ParamValue::Enum(policy.to_string()),
        );
        let mut request = base_request(
            &format!("cancel_mid_align_{policy}"),
            frames.clone(),
            ParamValues::new(),
        );
        request.align = Some(AlignRequest {
            algorithm,
            explicit,
            lax: false,
        });
        let cache = StageCache::open(&request.scratch_root).expect("stage cache must open");

        let outcome =
            focusforge_core::pipeline::run(&request, &NoopObserver, control, Some(&cache))
                .unwrap_or_else(|err| {
                    panic!("{policy}: a cancel must not take the error path: {err}")
                });

        assert!(
            matches!(
                outcome,
                FuseOutcome::Cancelled {
                    units_done: 0,
                    unit_total: 0
                }
            ),
            "{policy}: a cancel during alignment must end the run as cancelled"
        );
        let calls = algorithm.calls.load(Ordering::SeqCst);
        assert!(
            calls <= bound,
            "{policy}: {calls} registrations ran, but at most {bound} may once the cancel lands"
        );
        assert_eq!(
            files_under(&request.scratch_root),
            Vec::<PathBuf>::new(),
            "{policy}: a cancelled alignment must write no stage-cache entry and no resume ledger"
        );
    }
}

/// Converges every frame and counts its `align_frame` calls, sharing that
/// count with the test's own observer; the call numbered `pause_on`
/// requests a pause on `control` before it returns. Its schema is
/// `pyramidal-ecc`'s, so a request can choose the reference policy.
struct PausingAlign {
    control: Arc<JobControl>,
    pause_on: usize,
    calls: Arc<AtomicUsize>,
    /// Slept in every `align_frame`, so a watcher tick can land mid-stage.
    delay: Duration,
    /// An observer's tick count. When set, each `align_frame` returns only
    /// once a tick has landed during it, or the job stops running, or 10 s
    /// pass: a short stage on a loaded machine can otherwise end before the
    /// watcher thread is first scheduled.
    tick_count: Option<Arc<AtomicUsize>>,
}

impl AlignAlgorithm for PausingAlign {
    fn id(&self) -> &str {
        "pausing-align"
    }
    fn display_name(&self) -> &str {
        "Pausing Align"
    }
    fn params(&self) -> &[ParamDesc] {
        registry::find_align("pyramidal-ecc")
            .expect("pyramidal-ecc must be registered")
            .params()
    }
    fn align_frame(
        &self,
        _ctx: &AlignContext<'_>,
        _frame: usize,
        _cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        let entry = self.tick_count.as_ref().map(|t| t.load(Ordering::SeqCst));
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.pause_on {
            self.control.pause();
        }
        std::thread::sleep(self.delay);
        if let Some((ticks, entry)) = self.tick_count.as_ref().zip(entry) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while ticks.load(Ordering::SeqCst) == entry
                && self.control.state() == ControlState::Running
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(AlignOutcome {
            transform: Transform::identity(),
            status: AlignStatus::Converged,
            iterations: 1,
            residual: 1.0,
        })
    }
}

/// Records how many times [`StageObserver::paused`] was entered, asserts it
/// lands mid-`JobStage::Align` with `calls` inside `bound`, samples `calls`
/// before and after a 500 ms sleep to prove nothing registered during the
/// hold, then clears the control so the run continues.
struct PauseHoldObserver {
    entries: AtomicUsize,
    calls: Arc<AtomicUsize>,
    bound: std::ops::RangeInclusive<usize>,
}

impl StageObserver for PauseHoldObserver {
    fn paused(&self, control: &JobControl) {
        self.entries.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            control.progress().stage,
            JobStage::Align,
            "paused must land while the align stage is still running"
        );
        let before = self.calls.load(Ordering::SeqCst);
        assert!(
            self.bound.contains(&before),
            "paused entered after {before} registrations, it must enter after {:?}",
            self.bound
        );
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            self.calls.load(Ordering::SeqCst),
            before,
            "no further align_frame call may start during the hold"
        );
        control.clear();
    }
}

/// The same [`StageObserver::paused`] contract, but cancelling instead of
/// resuming -- a cancel requested against a run that is already held.
/// Records `calls` as the hold begins.
struct CancelOnPause {
    calls: Arc<AtomicUsize>,
    at_hold: OnceLock<usize>,
}

impl StageObserver for CancelOnPause {
    fn paused(&self, control: &JobControl) {
        self.at_hold
            .set(self.calls.load(Ordering::SeqCst))
            .expect("paused must be entered once");
        control.cancel();
    }
}

#[test]
fn pause_during_alignment_stops_registering_frames() {
    const PAUSE_ON: usize = 2;
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    // Same shape as the cancel test's own sizing: enough frames that a
    // pause noticed only once alignment returns would register frames past
    // the bound below.
    let frame_count = PAUSE_ON + workers + 8;
    let frames = mantis_64_frames(frame_count);

    // The pausing call itself, plus at most one in-flight registration per
    // other worker; the chain arm registers on one thread.
    for (policy, bound) in [("anchor", PAUSE_ON + workers - 1), ("chain", PAUSE_ON)] {
        let control = JobControl::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let algorithm: &'static PausingAlign = Box::leak(Box::new(PausingAlign {
            control: Arc::clone(&control),
            pause_on: PAUSE_ON,
            calls: Arc::clone(&calls),
            delay: Duration::ZERO,
            tick_count: None,
        }));
        let mut explicit = ParamValues::new();
        explicit.insert(
            "reference-policy".to_string(),
            ParamValue::Enum(policy.to_string()),
        );
        let mut request = base_request(
            &format!("pause_mid_align_{policy}"),
            frames.clone(),
            ParamValues::new(),
        );
        request.align = Some(AlignRequest {
            algorithm,
            explicit,
            lax: false,
        });

        let observer = PauseHoldObserver {
            entries: AtomicUsize::new(0),
            calls: Arc::clone(&calls),
            bound: PAUSE_ON..=bound,
        };
        let outcome = focusforge_core::pipeline::run(&request, &observer, control, None)
            .unwrap_or_else(|err| {
                panic!("{policy}: a paused-then-resumed run must not error: {err}")
            });

        assert!(
            matches!(outcome, FuseOutcome::Completed(_)),
            "{policy}: the run must complete once the hold clears"
        );
        assert_eq!(
            observer.entries.load(Ordering::SeqCst),
            1,
            "{policy}: paused must be entered exactly once"
        );
    }
}

#[test]
fn cancel_during_alignment_stops_at_a_frame_boundary_from_a_held_pause() {
    const PAUSE_ON: usize = 2;
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    let frame_count = PAUSE_ON + workers + 8;
    let frames = mantis_64_frames(frame_count);

    for (policy, bound) in [("anchor", PAUSE_ON + workers - 1), ("chain", PAUSE_ON)] {
        let control = JobControl::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let algorithm: &'static PausingAlign = Box::leak(Box::new(PausingAlign {
            control: Arc::clone(&control),
            pause_on: PAUSE_ON,
            calls: Arc::clone(&calls),
            delay: Duration::ZERO,
            tick_count: None,
        }));
        let mut explicit = ParamValues::new();
        explicit.insert(
            "reference-policy".to_string(),
            ParamValue::Enum(policy.to_string()),
        );
        let mut request = base_request(
            &format!("cancel_held_align_{policy}"),
            frames.clone(),
            ParamValues::new(),
        );
        request.align = Some(AlignRequest {
            algorithm,
            explicit,
            lax: false,
        });
        let cache = StageCache::open(&request.scratch_root).expect("stage cache must open");
        let observer = CancelOnPause {
            calls: Arc::clone(&calls),
            at_hold: OnceLock::new(),
        };

        let outcome = focusforge_core::pipeline::run(&request, &observer, control, Some(&cache))
            .unwrap_or_else(|err| {
                panic!("{policy}: a cancel from a held pause must not take the error path: {err}")
            });

        assert!(
            matches!(
                outcome,
                FuseOutcome::Cancelled {
                    units_done: 0,
                    unit_total: 0
                }
            ),
            "{policy}: a cancel requested while held must end the run as cancelled"
        );
        let calls_at_hold = *observer
            .at_hold
            .get()
            .unwrap_or_else(|| panic!("{policy}: the run must have been held"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            calls_at_hold,
            "{policy}: no align_frame call may start once the hold begins"
        );
        assert!(
            (PAUSE_ON..=bound).contains(&calls_at_hold),
            "{policy}: the hold began after {calls_at_hold} registrations, it must begin after \
             {PAUSE_ON}..={bound}"
        );
        assert_eq!(
            files_under(&request.scratch_root),
            Vec::<PathBuf>::new(),
            "{policy}: a cancelled alignment must write no stage-cache entry and no resume ledger"
        );
    }
}

#[derive(Default)]
struct ResumeOnPause {
    calls: AtomicUsize,
}

impl StageObserver for ResumeOnPause {
    fn paused(&self, control: &JobControl) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        control.clear();
    }
}

#[test]
fn paused_observer_is_called_once_per_segment() {
    // A single frame read through the other file mantis-512 ships --
    // `reference.tif`, the crop's own reference composite -- rather than the
    // golden run's eight-frame set.
    let reference = common::mantis_512_dir().join("reference.tif");
    let mut request = base_request("paused_once", vec![reference], ParamValues::new());
    request.tile_size = Some(704); // one tile covers the whole 512x512 crop.

    let control = JobControl::new();
    // Requested before the first (and only) fuse unit runs, so the very
    // first segment reports `Paused` with nothing completed.
    control.pause();

    let observer = ResumeOnPause::default();
    let outcome = focusforge_core::pipeline::run(&request, &observer, control, None)
        .expect("a run that pauses once and clears must still complete");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));
    assert_eq!(
        observer.calls.load(Ordering::SeqCst),
        1,
        "paused must be called exactly once for the one segment that paused"
    );
}

// ---------------------------------------------------------------------
// The align stage's own frame counter.
// ---------------------------------------------------------------------

#[derive(Default)]
struct AlignTickLog {
    ticks: Mutex<Vec<(usize, usize)>>,
}

impl StageObserver for AlignTickLog {
    fn tick(&self, progress: &JobProgress) {
        self.ticks
            .lock()
            .expect("no panic while logging")
            .push((progress.align_done, progress.align_total));
    }
}

#[test]
fn align_progress_counts_every_frame() {
    for count in [2, 5] {
        let align = AlignRequest {
            algorithm: registry::find_align("dummy-align").expect("dummy-align must be registered"),
            explicit: ParamValues::new(),
            lax: false,
        };
        let mut request = base_request(
            &format!("align_progress_{count}"),
            mantis_64_frames(count),
            ParamValues::new(),
        );
        request.align = Some(align);

        let control = JobControl::new();
        let observer = AlignTickLog::default();
        let outcome =
            focusforge_core::pipeline::run(&request, &observer, Arc::clone(&control), None)
                .unwrap_or_else(|err| panic!("a {count}-frame run must complete: {err}"));
        assert!(matches!(outcome, FuseOutcome::Completed(_)));

        // A fast aligner may finish before the watcher's first tick, so the
        // final tally is read from the control itself rather than from the
        // last tick -- `align_done`/`align_total` are never reset once the
        // align stage ends.
        let final_progress = control.progress();
        assert_eq!(final_progress.align_total, count);
        assert_eq!(final_progress.align_done, count);

        let ticks = observer.ticks.lock().expect("no panic while logging");
        let mut previous_done = 0;
        for &(done, total) in ticks.iter() {
            if total == 0 {
                continue; // a tick from before or after the align stage.
            }
            assert_eq!(total, count, "the total must not change mid-stage");
            assert!(done <= total, "done must never exceed total");
            assert!(done >= previous_done, "done must never decrease");
            previous_done = done;
        }
    }

    // No align stage at all: both counters stay at 0.
    let request = base_request(
        "align_progress_none",
        mantis_64_frames(2),
        ParamValues::new(),
    );
    let control = JobControl::new();
    let outcome =
        focusforge_core::pipeline::run(&request, &NoopObserver, Arc::clone(&control), None)
            .expect("a run with no align stage must complete");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));
    let progress = control.progress();
    assert_eq!(progress.align_done, 0);
    assert_eq!(progress.align_total, 0);
}

/// Logs the stage, align tally and ETA of every tick, and the tally and ETA
/// at the moment a hold begins; holds for 500 ms, then resumes.
#[derive(Default)]
struct AlignEtaLog {
    ticks: Mutex<Vec<(JobStage, usize, Option<Duration>)>>,
    tick_count: Arc<AtomicUsize>,
    hold_entry: Mutex<Option<(usize, Option<Duration>)>>,
}

impl StageObserver for AlignEtaLog {
    fn tick(&self, progress: &JobProgress) {
        self.ticks.lock().expect("no panic while logging").push((
            progress.stage,
            progress.align_done,
            progress.eta,
        ));
        self.tick_count.fetch_add(1, Ordering::SeqCst);
    }

    fn paused(&self, control: &JobControl) {
        let progress = control.progress();
        *self.hold_entry.lock().expect("no panic while logging") =
            Some((progress.align_done, progress.eta));
        std::thread::sleep(Duration::from_millis(500));
        control.clear();
    }
}

/// A chain-policy request over `count` mantis frames through a
/// [`PausingAlign`] that sleeps `delay` per frame, holds each frame until
/// `observer` has ticked during it, and pauses on call `pause_on`.
fn timed_align_request(
    label: &str,
    count: usize,
    control: &Arc<JobControl>,
    pause_on: usize,
    delay: Duration,
    observer: &AlignEtaLog,
) -> FuseRequest {
    let algorithm: &'static PausingAlign = Box::leak(Box::new(PausingAlign {
        control: Arc::clone(control),
        pause_on,
        calls: Arc::new(AtomicUsize::new(0)),
        delay,
        tick_count: Some(Arc::clone(&observer.tick_count)),
    }));
    let mut explicit = ParamValues::new();
    explicit.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("chain".to_string()),
    );
    let mut request = base_request(label, mantis_64_frames(count), ParamValues::new());
    request.align = Some(AlignRequest {
        algorithm,
        explicit,
        lax: false,
    });
    request.poll = Duration::from_millis(1);
    request
}

#[test]
fn align_progress_carries_an_eta() {
    let control = JobControl::new();
    let observer = AlignEtaLog::default();
    let request = timed_align_request(
        "align_eta",
        5,
        &control,
        usize::MAX,
        Duration::from_millis(20),
        &observer,
    );
    let cache = StageCache::open(&request.scratch_root).expect("stage cache must open");

    let outcome = focusforge_core::pipeline::run(&request, &observer, control, Some(&cache))
        .expect("a computed alignment must complete");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));
    let ticks = observer.ticks.lock().expect("no panic while logging");
    let align: Vec<_> = ticks.iter().filter(|t| t.0 == JobStage::Align).collect();
    assert!(
        align.iter().filter(|t| t.1 == 0).all(|t| t.2.is_none()),
        "no ETA before a frame completes: {align:?}"
    );
    assert!(
        align.iter().any(|t| 0 < t.1 && t.1 < 5 && t.2.is_some()),
        "a mid-stage tick must carry an ETA: {align:?}"
    );
    drop(ticks);

    // The same request against the same cache is served from it.
    let observer = AlignEtaLog::default();
    let outcome =
        focusforge_core::pipeline::run(&request, &observer, JobControl::new(), Some(&cache))
            .expect("a cached alignment must complete");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));
    let ticks = observer.ticks.lock().expect("no panic while logging");
    assert!(
        !ticks
            .iter()
            .any(|t| t.0 == JobStage::Align && t.2.is_some()),
        "a cached alignment has no per-frame work to estimate: {ticks:?}"
    );
}

#[test]
fn align_eta_excludes_a_pause() {
    let control = JobControl::new();
    let observer = AlignEtaLog::default();
    let request = timed_align_request(
        "align_eta_pause",
        8,
        &control,
        2,
        Duration::from_millis(20),
        &observer,
    );
    let outcome = focusforge_core::pipeline::run(&request, &observer, control, None)
        .expect("a paused-then-resumed alignment must complete");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));

    let (done_at_hold, eta_at_hold) = observer
        .hold_entry
        .lock()
        .expect("no panic while logging")
        .expect("the hold must be entered");
    let eta_at_hold = eta_at_hold.expect("frames were done when the hold began");
    let ticks = observer.ticks.lock().expect("no panic while logging");
    let after: Vec<_> = ticks
        .iter()
        .filter(|t| t.0 == JobStage::Align && t.1 > done_at_hold)
        .filter_map(|t| t.2)
        .collect();
    assert!(
        !after.is_empty(),
        "ticks after the resume must carry an ETA"
    );
    for eta in after {
        assert!(
            eta <= eta_at_hold + Duration::from_millis(250),
            "{eta:?} after the hold vs {eta_at_hold:?} before it"
        );
    }
}

/// Logs the Prepare-stage tally of every tick, and the control's tally at
/// the moment the align stage is announced.
struct PrepareTickLog {
    control: Arc<JobControl>,
    prepare_ticks: Mutex<Vec<(usize, usize)>>,
    all_ticks: Mutex<Vec<(usize, usize)>>,
    at_align: Mutex<Option<(usize, usize)>>,
}

impl PrepareTickLog {
    fn new(control: &Arc<JobControl>) -> Self {
        PrepareTickLog {
            control: Arc::clone(control),
            prepare_ticks: Mutex::default(),
            all_ticks: Mutex::default(),
            at_align: Mutex::default(),
        }
    }
}

impl StageObserver for PrepareTickLog {
    fn stage(&self, stage: RunStage) {
        if matches!(stage, RunStage::Align { .. }) {
            let p = self.control.progress();
            *self.at_align.lock().expect("no panic while logging") =
                Some((p.prepare_done, p.prepare_total));
        }
    }

    fn tick(&self, progress: &JobProgress) {
        let tally = (progress.prepare_done, progress.prepare_total);
        self.all_ticks
            .lock()
            .expect("no panic while logging")
            .push(tally);
        if progress.stage == JobStage::Prepare {
            self.prepare_ticks
                .lock()
                .expect("no panic while logging")
                .push(tally);
        }
    }
}

fn prepare_request(label: &str, count: usize, equalize: Option<EqualizeSettings>) -> FuseRequest {
    let mut request = base_request(label, mantis_64_frames(count), ParamValues::new());
    request.align = Some(dummy_align_request());
    request.equalize = equalize;
    request.poll = Duration::from_millis(1);
    request
}

#[test]
fn prepare_progress_counts_every_frame() {
    for count in [2, 5] {
        let request = prepare_request(
            &format!("prepare_progress_{count}"),
            count,
            eq_settings(1.25, 0.0),
        );
        let control = JobControl::new();
        let observer = PrepareTickLog::new(&control);
        let outcome =
            focusforge_core::pipeline::run(&request, &observer, Arc::clone(&control), None)
                .unwrap_or_else(|err| panic!("a {count}-frame run must complete: {err}"));
        assert!(matches!(outcome, FuseOutcome::Completed(_)));

        assert_eq!(
            *observer.at_align.lock().expect("no panic while logging"),
            Some((count, count)),
            "Prepare is complete when Align is announced"
        );
        let ticks = observer
            .prepare_ticks
            .lock()
            .expect("no panic while logging");
        let mut previous = 0;
        for &(done, total) in ticks.iter() {
            assert_eq!(total, count, "the total is the frame count on every tick");
            assert!(done <= total, "done must never exceed total");
            assert!(done >= previous, "done must never decrease");
            previous = done;
        }
    }
}

#[test]
fn prepare_progress_is_zero_without_equalization() {
    let request = prepare_request("prepare_progress_off", 3, None);
    let control = JobControl::new();
    let observer = PrepareTickLog::new(&control);
    let outcome = focusforge_core::pipeline::run(&request, &observer, Arc::clone(&control), None)
        .expect("a run without equalization must complete");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));

    let all = observer.all_ticks.lock().expect("no panic while logging");
    assert!(all.iter().all(|&tally| tally == (0, 0)), "{all:?}");
    assert!(
        observer
            .prepare_ticks
            .lock()
            .expect("no panic while logging")
            .is_empty()
    );
    let progress = control.progress();
    assert_eq!((progress.prepare_done, progress.prepare_total), (0, 0));
    assert_eq!((progress.align_done, progress.align_total), (3, 3));
}

/// Frame 06 of the fixture solves with an 11% zoom against frame 03 that the
/// off-centre check rejects. The strict default refuses the run and names that
/// frame with the check's own wording; frames 03..=06 leave it the only failure.
#[test]
fn the_strict_default_refuses_a_frame_the_off_centre_check_rejects() {
    let mut explicit = ParamValues::new();
    explicit.insert(
        "reference-frame".to_string(),
        ParamValue::Enum("first".to_string()),
    );
    explicit.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );
    let mut request = base_request(
        "strict_off_centre",
        mantis_frames(8)[3..=6].to_vec(),
        ParamValues::new(),
    );
    request.align = Some(AlignRequest {
        algorithm: registry::find_align("pyramidal-ecc").expect("pyramidal-ecc registered"),
        explicit,
        lax: false,
    });
    request.cache = false;

    let result = focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None);
    let Err(PipelineError::Align(detail)) = result else {
        panic!("the strict default must refuse the rejected frame");
    };
    assert!(detail.contains("06.tif"), "{detail}");
    assert!(
        detail.contains("the alignment does not hold away from the centre of the frame"),
        "{detail}"
    );
    assert!(
        !detail.contains("frames total"),
        "only one frame fails: {detail}"
    );
}

#[test]
fn a_held_run_seals_to_the_same_project() {
    let frames = mantis_frames(3);
    let align = AlignRequest {
        algorithm: registry::find_align("dummy-align").expect("dummy-align must be registered"),
        explicit: ParamValues::new(),
        lax: false,
    };

    let hold_dir = scratch("held_run_hold");
    let mut held_request = base_request("held_run", frames.clone(), ParamValues::new());
    held_request.align = Some(align);
    held_request.crop = true;
    held_request.output = FuseOutput::Hold(hold_dir.clone());
    let staging = held_request
        .scratch_root
        .join("projects")
        .join(&held_request.staging_key);
    let outcome =
        focusforge_core::pipeline::run(&held_request, &NoopObserver, JobControl::new(), None)
            .expect("a held run must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    assert!(report.tiles > 1, "the run must span more than one tile");
    let FuseProduct::Held(held) = report.product else {
        panic!("a hold request must report a held checkpoint");
    };
    assert_eq!(held.composite, hold_dir.join("composite.tif"));
    assert_eq!(held.zmap, hold_dir.join("zmap.tif"));
    assert!(held.composite.is_file() && held.zmap.is_file());
    assert!(
        !staging.exists(),
        "the run's staging directory is cleaned up"
    );

    let target = scratch("held_run_saved").join("held.ffp");
    seal_chain_alone(&target, &held);

    let mut direct_request = base_request("direct_run", frames, ParamValues::new());
    direct_request.align = Some(AlignRequest {
        algorithm: registry::find_align("dummy-align").expect("dummy-align must be registered"),
        explicit: ParamValues::new(),
        lax: false,
    });
    direct_request.crop = true;
    let outcome =
        focusforge_core::pipeline::run(&direct_request, &NoopObserver, JobControl::new(), None)
            .expect("a direct run must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    let FuseProduct::Sealed(sealed) = report.product else {
        panic!("a seal request must report a sealed project");
    };

    let from_held = Project::open(&target).unwrap().manifest().clone();
    let direct = Project::open(&sealed.path).unwrap().manifest().clone();
    assert_eq!(
        from_held.registration.transforms.len(),
        3,
        "one transform per frame"
    );
    assert_eq!(
        from_held.registration.crop, direct.registration.crop,
        "the same crop either way"
    );
    assert!(
        from_held.registration.crop.is_some(),
        "the aligned frames must crop"
    );
    assert_eq!(from_held.algorithm, direct.algorithm);
    assert_eq!(from_held.params, direct.params);
    assert_eq!(from_held.sources, direct.sources);
    assert_eq!(from_held.zmap.blob, direct.zmap.blob);
}

// ---------------------------------------------------------------------
// `Registration`: every frame's transform and the applied crop survive
// seal, hold and reseal, bit-for-bit.
// ---------------------------------------------------------------------

fn dummy_align_request() -> AlignRequest {
    AlignRequest {
        algorithm: registry::find_align("dummy-align").expect("dummy-align must be registered"),
        explicit: ParamValues::new(),
        lax: false,
    }
}

/// Per-entry `f64::to_bits()` equality -- the bit-exact comparison the
/// manifest's own encoding is meant to preserve.
fn assert_transforms_bit_eq(actual: &[Transform], expected: &[Transform], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: transform count");
    for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
        for row in 0..3 {
            for col in 0..3 {
                assert_eq!(
                    a.m[row][col].to_bits(),
                    e.m[row][col].to_bits(),
                    "{what}: frame {i} transform[{row}][{col}] bit pattern differs"
                );
            }
        }
    }
}

#[test]
fn registration_persists_through_seal_and_hold() {
    let mut sealed_request =
        base_request("registration_seal", mantis_frames(3), ParamValues::new());
    sealed_request.align = Some(dummy_align_request());
    sealed_request.crop = true;
    let outcome =
        focusforge_core::pipeline::run(&sealed_request, &NoopObserver, JobControl::new(), None)
            .expect("an aligned, cropped run must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    let expected_transforms: Vec<Transform> = report
        .align
        .as_ref()
        .expect("an aligned run reports its alignment")
        .alignment
        .frames
        .iter()
        .map(|f| f.transform)
        .collect();
    let expected_crop = report.crop.expect("a cropped run must report a crop");
    let FuseProduct::Sealed(sealed) = report.product else {
        panic!("expected a sealed project");
    };
    let manifest = Project::open(&sealed.path)
        .expect("reopen sealed project")
        .manifest()
        .clone();
    assert_transforms_bit_eq(
        &manifest.registration.transforms,
        &expected_transforms,
        "sealed manifest",
    );
    assert_eq!(manifest.registration.crop, Some(expected_crop));
    assert_eq!(
        (manifest.composite.width, manifest.composite.height),
        (expected_crop.width, expected_crop.height),
        "the composite must be declared at the crop's own size"
    );

    // A held checkpoint, sealed later, carries the same shape of values.
    let hold_dir = scratch("registration_hold");
    let mut held_request = base_request(
        "registration_hold_run",
        mantis_frames(3),
        ParamValues::new(),
    );
    held_request.align = Some(dummy_align_request());
    held_request.crop = true;
    held_request.output = FuseOutput::Hold(hold_dir.clone());
    let outcome =
        focusforge_core::pipeline::run(&held_request, &NoopObserver, JobControl::new(), None)
            .expect("a held run must complete");
    let FuseOutcome::Completed(held_report) = outcome else {
        panic!("expected a completed run");
    };
    let held_expected_transforms: Vec<Transform> = held_report
        .align
        .as_ref()
        .expect("an aligned run reports its alignment")
        .alignment
        .frames
        .iter()
        .map(|f| f.transform)
        .collect();
    let held_expected_crop = held_report.crop.expect("a cropped run must report a crop");
    let FuseProduct::Held(held) = held_report.product else {
        panic!("expected a held checkpoint");
    };
    assert_transforms_bit_eq(
        &held.registration.transforms,
        &held_expected_transforms,
        "held checkpoint",
    );
    assert_eq!(held.registration.crop, Some(held_expected_crop));

    let target = scratch("registration_hold_saved").join("held.ffp");
    seal_chain_alone(&target, &held);
    let sealed_from_held = Project::open(&target).expect("reopen").manifest().clone();
    assert_transforms_bit_eq(
        &sealed_from_held.registration.transforms,
        &held_expected_transforms,
        "manifest sealed from a held checkpoint",
    );
    assert_eq!(sealed_from_held.registration.crop, Some(held_expected_crop));
}

#[test]
fn no_align_registration_is_identity_transforms_and_the_kernel_inset_crop() {
    let mut request = base_request(
        "registration_no_align",
        mantis_frames(3),
        ParamValues::new(),
    );
    request.crop = true;
    let outcome = focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
        .expect("a no-align cropped run must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    let crop = report
        .crop
        .expect("the kernel-inset crop must still apply at identity, not None");
    let FuseProduct::Sealed(sealed) = report.product else {
        panic!("expected a sealed project");
    };
    let manifest = Project::open(&sealed.path)
        .expect("reopen sealed project")
        .manifest()
        .clone();
    assert_eq!(manifest.registration.crop, Some(crop));
    assert_eq!(manifest.registration.transforms.len(), 3);
    assert!(
        manifest
            .registration
            .transforms
            .iter()
            .all(|t| *t == Transform::identity()),
        "a --no-align run must record identity for every frame"
    );
}

/// Registers every frame it is asked about a full frame width to the right,
/// so no pixel of the shifted frame overlaps the reference.
struct ShiftPastTheFrame;

impl AlignAlgorithm for ShiftPastTheFrame {
    fn id(&self) -> &str {
        "shift-past-the-frame"
    }
    fn display_name(&self) -> &str {
        "Shift Past The Frame"
    }
    fn params(&self) -> &[ParamDesc] {
        registry::find_align("pyramidal-ecc")
            .expect("pyramidal-ecc must be registered")
            .params()
    }
    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        _frame: usize,
        _cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        let width = f64::from(ctx.stack().width);
        Ok(AlignOutcome {
            transform: Transform::similarity(1.0, 0.0, width + 8.0, 0.0),
            status: AlignStatus::Converged,
            iterations: 1,
            residual: 1.0,
        })
    }
}

#[test]
fn an_empty_overlap_with_crop_names_the_remedy() {
    static SHIFT: ShiftPastTheFrame = ShiftPastTheFrame;
    let align = || AlignRequest {
        algorithm: &SHIFT,
        explicit: ParamValues::new(),
        lax: false,
    };

    let mut request = base_request("empty_overlap_crop", mantis_frames(2), ParamValues::new());
    request.align = Some(align());
    request.crop = true;
    let Err(PipelineError::Align(message)) =
        focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
    else {
        panic!("an empty overlap with auto-crop on must fail with PipelineError::Align");
    };
    assert!(
        message.contains("turn off auto-crop"),
        "the error must name the remedy: {message}"
    );

    let mut request = base_request(
        "empty_overlap_no_crop",
        mantis_frames(2),
        ParamValues::new(),
    );
    request.align = Some(align());
    request.crop = false;
    let outcome = focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
        .expect("the same run without auto-crop completes");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    assert_eq!(
        (report.output_width, report.output_height),
        (report.width, report.height),
        "without auto-crop the output keeps the full frame extent"
    );
}

/// The pipeline divides its capped host budget into alignment workers: a
/// larger cap runs more workers, the alignment and the composite bytes do
/// not depend on the cap, and the reported accounted bytes follow the worker count.
#[test]
fn the_host_cap_sets_the_alignment_workers_but_not_the_alignment() {
    use focusforge_core::align::{bytes_per_registration, bytes_shared_reference};
    use focusforge_core::params::StackContext;
    use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;

    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let stack = StackContext::new(4, 512, 512, 3);
    let shared = bytes_shared_reference(stack);
    let per_registration = bytes_per_registration(stack, DEFAULT_TILE_SIZE);

    let run_with_cap = |label: &str, cap: u64| {
        let mut explicit = ParamValues::new();
        explicit.insert("max-iterations".to_string(), ParamValue::Int(20));
        explicit.insert(
            "reference-policy".to_string(),
            ParamValue::Enum("anchor".to_string()),
        );
        let mut request = base_request(label, mantis_frames(4), ParamValues::new());
        request.align = Some(AlignRequest {
            algorithm: registry::find_align("pyramidal-ecc").expect("pyramidal-ecc registered"),
            explicit,
            lax: true,
        });
        request.cache = false;
        request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
        request.max_host_memory = Some(cap);
        match focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
            .expect("the capped run completes")
        {
            FuseOutcome::Completed(report) => report,
            _ => panic!("the run must complete"),
        }
    };

    let one = run_with_cap("cap_one_worker", 1);
    let two = run_with_cap("cap_two_workers", shared + 2 * per_registration);
    assert_eq!(one.align_workers, Some(1));
    assert_eq!(two.align_workers, Some(available.min(2)));
    for report in [&one, &two] {
        let workers = report.align_workers.unwrap() as u64;
        assert_eq!(
            report.align_accounted_bytes,
            Some(shared + workers * per_registration)
        );
    }
    let frames = |report: &focusforge_core::pipeline::FuseReport| {
        format!(
            "{:?}",
            report.align.as_ref().expect("aligned").alignment.frames
        )
    };
    assert_eq!(frames(&one), frames(&two));
    let composite = |report: &focusforge_core::pipeline::FuseReport| match &report.product {
        FuseProduct::Held(held) => std::fs::read(&held.composite).expect("composite is readable"),
        FuseProduct::Sealed(_) => panic!("the run holds its checkpoint"),
    };
    assert_eq!(composite(&one), composite(&two));
}

// ---------------------------------------------------------------------
// A backend's own tile-size ceiling lowers the planned tile size.
// ---------------------------------------------------------------------

/// A host-fusion [`TileBackend`] that accepts read rects up to `cap` pixels
/// on a side. With a `shortfall` it reports that message for every tile it is
/// asked about and fails any fuse call, so a run that reaches it is a bug;
/// `asked` records the rects it was asked about.
struct CappedBackend {
    cap: u32,
    shortfall: Option<String>,
    asked: Mutex<Vec<(u32, u32)>>,
    /// Reports the device as lost, so every tile finishes on the host.
    lost: bool,
}

impl TileBackend for CappedBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "capped-stub".into(),
            backend: "capped-stub".into(),
            driver: "capped-stub".into(),
            driver_info: "capped-stub".into(),
        }
    }

    fn max_tile_size(&self, _samples_per_pixel: u32) -> u32 {
        self.cap
    }

    fn device_lost(&self) -> bool {
        self.lost
    }

    fn tile_budget_shortfall(
        &self,
        width: u32,
        height: u32,
        _samples_per_pixel: u32,
        _levels: u32,
        _transforms: &[Transform],
        _sample_format: SampleFormat,
    ) -> Option<String> {
        self.asked.lock().unwrap().push((width, height));
        self.shortfall.clone()
    }

    fn self_check(
        &self,
        _reader: &dyn TileReader,
        _frame: usize,
        _rect: Rect,
        _samples_per_pixel: u32,
        _gain: f32,
    ) -> Result<Tile, AlgorithmError> {
        unimplemented!("the pipeline never calls the self-check")
    }

    fn fuse_tile(
        &self,
        _reader: &dyn TileReader,
        _frame_count: usize,
        _rect: Rect,
        _samples_per_pixel: u32,
        _levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        unimplemented!("the pipeline fuses through warp_fuse_tile")
    }

    fn warp_tile(
        &self,
        _reader: &dyn TileReader,
        _frame_index: usize,
        _frame: (u32, u32),
        _samples_per_pixel: u32,
        _dst: Rect,
        _transform: &Transform,
    ) -> Result<WarpedTile, AlignError> {
        unimplemented!("the pipeline fuses through warp_fuse_tile")
    }

    fn warp_fuse_tile(
        &self,
        reader: &dyn TileReader,
        _frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        assert!(
            self.shortfall.is_none(),
            "a backend that reports a shortfall must never fuse"
        );
        assert!(
            rect.width <= self.cap && rect.height <= self.cap,
            "a read rect of {}x{} exceeds the backend's cap of {}",
            rect.width,
            rect.height,
            self.cap
        );
        let mut tiles = Vec::with_capacity(transforms.len());
        for (i, t) in transforms.iter().enumerate() {
            assert_eq!(*t, Transform::identity(), "no alignment runs here");
            let mut samples = vec![0u16; (rect.width * rect.height * samples_per_pixel) as usize];
            reader.read_tile_into(i, rect, &mut samples)?;
            tiles.push(Tile {
                rect,
                samples_per_pixel,
                samples,
            });
        }
        let ctx = TileCtx::new(rect, &tiles);
        let mut cfg = ParamValues::new();
        cfg.insert("levels".to_string(), ParamValue::Int(i64::from(levels)));
        Laplacian.fuse(&ctx, &cfg)
    }
}

fn leaked_backend(cap: u32, shortfall: Option<&str>) -> &'static CappedBackend {
    Box::leak(Box::new(CappedBackend {
        cap,
        shortfall: shortfall.map(str::to_string),
        asked: Mutex::new(Vec::new()),
        lost: false,
    }))
}

fn capped_backend(cap: u32) -> &'static dyn TileBackend {
    leaked_backend(cap, None)
}

/// Collects every notice a run raises.
#[derive(Default)]
struct NoticeObserver {
    notices: Mutex<Vec<String>>,
}

impl StageObserver for NoticeObserver {
    fn notice(&self, message: &str) {
        self.notices.lock().unwrap().push(message.to_string());
    }
}

/// Collects the frame index each frame notice names.
#[derive(Default)]
struct FrameNoticeObserver {
    notices: Mutex<Vec<(usize, String)>>,
}

impl StageObserver for FrameNoticeObserver {
    fn frame_notice(&self, frame: usize, message: &str) {
        self.notices
            .lock()
            .unwrap()
            .push((frame, message.to_string()));
    }
}

/// A held run's report with the bytes of the composite and Z-Map it wrote.
struct HeldRun {
    report: Box<FuseReport>,
    composite: Vec<u8>,
    zmap: Vec<u8>,
}

/// Runs a held fuse over three frames.
fn held_run(
    label: &str,
    tile_size: u32,
    levels: i64,
    backend: Option<&'static dyn TileBackend>,
) -> Result<HeldRun, PipelineError> {
    held_run_observed(label, tile_size, levels, backend, &NoopObserver)
}

fn held_run_observed(
    label: &str,
    tile_size: u32,
    levels: i64,
    backend: Option<&'static dyn TileBackend>,
    observer: &dyn StageObserver,
) -> Result<HeldRun, PipelineError> {
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(levels));
    let mut request = base_request(label, mantis_frames(3), explicit);
    request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
    request.tile_size = Some(tile_size);
    request.backend = backend;
    let FuseOutcome::Completed(report) =
        focusforge_core::pipeline::run(&request, observer, JobControl::new(), None)?
    else {
        panic!("expected a completed run");
    };
    Ok(held_run_of(report))
}

fn held_run_of(report: Box<FuseReport>) -> HeldRun {
    let FuseProduct::Held(held) = &report.product else {
        panic!("a hold request must report a held checkpoint");
    };
    let composite = std::fs::read(&held.composite).expect("composite must be readable");
    let zmap = std::fs::read(&held.zmap).expect("z-map must be readable");
    HeldRun {
        report,
        composite,
        zmap,
    }
}

/// Cancels the run on the first progress snapshot that shows a fuse unit done.
struct CancelAfterFirstUnit(Arc<JobControl>);
impl StageObserver for CancelAfterFirstUnit {
    fn tick(&self, progress: &JobProgress) {
        if progress.stage == JobStage::Fuse && progress.units_done > 0 {
            self.0.cancel();
        }
    }
}

#[test]
fn cancel_mid_fusion_then_resume_matches_an_uninterrupted_run() {
    for method in ["laplacian", "weighted-average", "depth-map"] {
        let request = |label: &str| {
            let label = format!("{label}_{method}");
            // Five levels need a 448 px tile; three fit the 256 px tile that plans several.
            let mut explicit = ParamValues::new();
            if method == "laplacian" {
                explicit.insert("levels".to_string(), ParamValue::Int(3));
            }
            let mut request = base_request(&label, mantis_frames(3), explicit);
            request.algorithm = registry::find_stack(method).expect("method must be registered");
            request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
            request.tile_size = Some(256);
            // One tile in flight, so units finish one at a time and the cancel lands before the last.
            request.max_host_memory = Some(1);
            request.poll = Duration::from_millis(1);
            request
        };
        let whole = request("uninterrupted");
        let FuseOutcome::Completed(report) =
            focusforge_core::pipeline::run(&whole, &NoopObserver, JobControl::new(), None)
                .unwrap_or_else(|err| panic!("{method}: the uninterrupted run failed: {err}"))
        else {
            panic!("{method}: the uninterrupted run must complete");
        };
        let whole = held_run_of(report);
        assert!(
            whole.report.tiles > 2,
            "{method}: {} tiles",
            whole.report.tiles
        );

        let mut interrupted = request("interrupted");
        let control = JobControl::new();
        let outcome = focusforge_core::pipeline::run(
            &interrupted,
            &CancelAfterFirstUnit(Arc::clone(&control)),
            control,
            None,
        )
        .unwrap_or_else(|err| panic!("{method}: a cancel must not take the error path: {err}"));
        let FuseOutcome::Cancelled {
            units_done,
            unit_total,
        } = outcome
        else {
            panic!("{method}: the cancelled run must end as cancelled");
        };
        assert!(
            0 < units_done && units_done < unit_total,
            "{method}: {units_done} of {unit_total} units done at the cancel"
        );

        interrupted.resume = true;
        let FuseOutcome::Completed(report) =
            focusforge_core::pipeline::run(&interrupted, &NoopObserver, JobControl::new(), None)
                .unwrap_or_else(|err| panic!("{method}: the resumed run failed: {err}"))
        else {
            panic!("{method}: the resumed run must complete");
        };
        let resumed = held_run_of(report);
        assert!(
            resumed.composite == whole.composite,
            "{method}: the resumed composite must equal the uninterrupted one"
        );
        assert!(
            resumed.zmap == whole.zmap,
            "{method}: the resumed Z-Map must equal the uninterrupted one"
        );
    }
}

#[test]
fn a_backend_cap_lowers_the_planned_tile_size() {
    let capped =
        held_run("cap_256_backend", 448, 3, Some(capped_backend(256))).expect("a capped run");
    assert_eq!(capped.report.tile_size, 256);

    let plain = held_run("cap_256_plain", 256, 3, None).expect("a backend-free run at 256");
    assert_eq!(plain.report.tile_size, 256);
    assert!(
        capped.report.tiles > 1 && capped.report.tiles == plain.report.tiles,
        "ragged edge tiles: {} capped vs {} plain",
        capped.report.tiles,
        plain.report.tiles
    );
    assert!(
        capped.composite == plain.composite,
        "the capped run must write the composite a backend-free run at 256 writes"
    );
    assert!(
        capped.zmap == plain.zmap,
        "the capped run must write the Z-Map a backend-free run at 256 writes"
    );
}

#[test]
fn no_backend_keeps_the_requested_tile_size() {
    let run = held_run("cap_none", 448, 3, None).expect("a backend-free run at 448");
    assert_eq!(run.report.tile_size, 448);
}

#[test]
fn deep_levels_past_the_device_cap_fuse_on_the_host() {
    const CAP: u32 = 64;
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(4));
    let algorithm = registry::find_stack("laplacian").expect("laplacian must be registered");
    let minimum = algorithm.tile_support(&explicit).min_tile_size();
    assert!(
        CAP < minimum,
        "the cap {CAP} must sit below the minimum {minimum}"
    );
    let expected = format!(
        "the GPU accepts tiles up to {CAP} px and 4 levels need {minimum} px; \
         continuing on the CPU path"
    );

    let run = |label: &str,
               tile_size: Option<u32>,
               backend: Option<&'static dyn TileBackend>,
               observer: &dyn StageObserver| {
        let mut request = base_request(label, mantis_64_frames(3), explicit.clone());
        request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
        request.tile_size = tile_size;
        request.backend = backend;
        let FuseOutcome::Completed(report) =
            focusforge_core::pipeline::run(&request, observer, JobControl::new(), None)
                .unwrap_or_else(|err| {
                    panic!("{label}: a cap below the minimum must not fail: {err}")
                })
        else {
            panic!("{label}: expected a completed run");
        };
        held_run_of(report)
    };

    for (name, tile_size) in [("default", None), ("explicit_512", Some(512))] {
        let plain = run(
            &format!("deep_plain_{name}"),
            tile_size,
            None,
            &NoopObserver,
        );
        assert!(plain.report.gpu_fallback.is_none(), "{name}");

        let backend = leaked_backend(CAP, None);
        let observer = NoticeObserver::default();
        let capped = run(
            &format!("deep_capped_{name}"),
            tile_size,
            Some(backend),
            &observer,
        );
        assert_eq!(
            capped.report.gpu_fallback.as_deref(),
            Some(expected.as_str()),
            "{name}"
        );
        assert_eq!(
            *observer.notices.lock().unwrap(),
            std::slice::from_ref(&expected),
            "{name}"
        );
        assert_eq!(capped.report.tile_size, plain.report.tile_size, "{name}");
        assert!(
            capped.report.tile_size > CAP,
            "{name}: the uncapped size is kept"
        );
        assert!(
            backend.asked.lock().unwrap().is_empty(),
            "{name}: the notice replaces the shortfall query"
        );
        assert!(
            capped.composite == plain.composite,
            "{name}: composite on the host"
        );
        assert!(capped.zmap == plain.zmap, "{name}: Z-Map on the host");
    }
}

#[test]
fn an_explicit_tile_size_below_the_levels_minimum_is_refused_whatever_the_cap() {
    for backend in [None, Some(capped_backend(64))] {
        let Err(err) = held_run("explicit_64", 64, 5, backend) else {
            panic!("a 64 px tile leaves no interior at five levels");
        };
        assert!(
            matches!(
                err,
                PipelineError::Tile(TileError::NoInterior { tile_size: 64, .. })
            ),
            "unexpected error: {err}"
        );
    }
}

#[test]
fn gpu_fallback_runs_on_the_host_and_reports_why() {
    let message = "one 256x256 tile needs more than the device holds; continuing on the CPU path";
    let plain = held_run("fallback_plain", 256, 3, None).expect("a backend-free run at 256");
    assert!(plain.report.gpu_fallback.is_none());

    let short = leaked_backend(u32::MAX, Some(message));
    let observer = NoticeObserver::default();
    let run = held_run_observed("fallback_256", 256, 3, Some(short), &observer)
        .expect("a run the backend cannot hold falls back to the host");
    assert_eq!(run.report.gpu_fallback.as_deref(), Some(message));
    assert_eq!(*observer.notices.lock().unwrap(), [message]);
    assert!(run.report.tiles > 1, "ragged tiles: {}", run.report.tiles);
    assert!(run.composite == plain.composite, "composite on the host");
    assert!(run.zmap == plain.zmap, "Z-Map on the host");
    assert_eq!(*short.asked.lock().unwrap(), [(256, 256)]);

    let wide = leaked_backend(u32::MAX, Some(message));
    held_run("fallback_704", 704, 3, Some(wide)).expect("a 704 tile on a 512 px frame");
    assert_eq!(*wide.asked.lock().unwrap(), [(512, 512)]);

    let fits = leaked_backend(u32::MAX, None);
    let run = held_run("fallback_fits", 256, 3, Some(fits)).expect("a backend that fits");
    assert!(run.report.gpu_fallback.is_none());
    assert_eq!(*fits.asked.lock().unwrap(), [(256, 256)]);
    assert!(run.composite == plain.composite);
}

// ---------------------------------------------------------------------
// Opt-in brightness equalization.
// ---------------------------------------------------------------------

const EQ_SIZE: u32 = 300;

/// A deterministic texture in 15000..45000, tinted per channel when `spp` is 3.
fn eq_texture(spp: u32) -> Vec<u16> {
    let mut state = 0x9e37_79b9_u32;
    let mut samples = Vec::with_capacity((EQ_SIZE * EQ_SIZE * spp) as usize);
    for _ in 0..EQ_SIZE * EQ_SIZE {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let base = 15_000 + (state >> 16) % 30_000;
        for c in 0..spp {
            samples.push((base * (10 + c) / 12).clamp(1, 65_535) as u16);
        }
    }
    samples
}

/// One TIFF per factor: the texture scaled by it, rounded, saturating.
fn brightness_stack(label: &str, spp: u32, factors: &[f64]) -> Vec<PathBuf> {
    let dir = scratch(&format!("{label}_frames"));
    let texture = eq_texture(spp);
    let full = Rect {
        x: 0,
        y: 0,
        width: EQ_SIZE,
        height: EQ_SIZE,
    };
    factors
        .iter()
        .enumerate()
        .map(|(i, factor)| {
            let path = dir.join(format!("{i:02}.tif"));
            let samples: Vec<u16> = texture
                .iter()
                .map(|&s| (f64::from(s) * factor).round().min(65_535.0) as u16)
                .collect();
            let mut writer = TiffWriter::create(
                &path,
                TiffSpec {
                    width: EQ_SIZE,
                    height: EQ_SIZE,
                    samples_per_pixel: spp,
                    description: None,
                },
            )
            .expect("writer must create the frame");
            writer.write_tile(full, &samples).expect("frame samples");
            writer.finish().expect("frame must finish");
            path
        })
        .collect()
}

/// Holds one run over `frames` with `algorithm` at tile size 256.
fn eq_run(
    label: &str,
    frames: &[PathBuf],
    algorithm: &str,
    equalize: Option<EqualizeSettings>,
    backend: Option<&'static dyn TileBackend>,
    observer: &dyn StageObserver,
) -> HeldRun {
    let mut explicit = ParamValues::new();
    if algorithm == "laplacian" {
        explicit.insert("levels".to_string(), ParamValue::Int(3));
    }
    let mut request = base_request(label, frames.to_vec(), explicit);
    request.algorithm = registry::find_stack(algorithm).expect("a registered algorithm");
    request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
    request.tile_size = Some(256);
    request.equalize = equalize;
    request.backend = backend;
    let FuseOutcome::Completed(report) =
        focusforge_core::pipeline::run(&request, observer, JobControl::new(), None)
            .expect("the run must complete")
    else {
        panic!("expected a completed run");
    };
    let FuseProduct::Held(held) = &report.product else {
        panic!("a hold request must report a held checkpoint");
    };
    let composite = std::fs::read(&held.composite).expect("composite must be readable");
    let zmap = std::fs::read(&held.zmap).expect("z-map must be readable");
    HeldRun {
        report,
        composite,
        zmap,
    }
}

fn eq_settings(max_gain: f64, target_stops: f64) -> Option<EqualizeSettings> {
    Some(EqualizeSettings {
        max_gain,
        target_stops,
    })
}

#[test]
fn equalization_is_off_by_default_and_records_nothing() {
    let frames = brightness_stack("eq_off", 3, &[0.9, 1.0, 1.1]);
    let run = eq_run(
        "eq_off_run",
        &frames,
        "laplacian",
        None,
        None,
        &NoopObserver,
    );
    assert!(run.report.equalization.is_none());
    let FuseProduct::Held(held) = &run.report.product else {
        panic!("held run");
    };
    assert!(held.registration.equalization.is_none());
}

#[test]
fn equalizing_repeated_frames_changes_nothing() {
    for spp in [3, 1] {
        let frames = brightness_stack(&format!("eq_repeat_{spp}"), spp, &[1.0, 1.0, 1.0]);
        let off = eq_run(
            &format!("eq_repeat_off_{spp}"),
            &frames,
            "laplacian",
            None,
            None,
            &NoopObserver,
        );
        let on = eq_run(
            &format!("eq_repeat_on_{spp}"),
            &frames,
            "laplacian",
            eq_settings(1.25, 0.0),
            None,
            &NoopObserver,
        );
        let estimate = on.report.equalization.as_ref().expect("estimate reported");
        assert!(
            estimate
                .frames
                .iter()
                .all(|f| f.applied == 1.0 && !f.clamped),
            "spp {spp}"
        );
        let FuseProduct::Held(held) = &on.report.product else {
            panic!("held run");
        };
        assert_eq!(
            held.registration.equalization.as_ref().unwrap().gains,
            vec![UNIT_GAIN; 3],
            "spp {spp}"
        );
        assert!(on.composite == off.composite, "composite, spp {spp}");
        assert!(on.zmap == off.zmap, "z-map, spp {spp}");
    }
}

#[test]
fn equalization_converges_every_frame_on_the_stack_mean() {
    for spp in [1, 3] {
        let frames = brightness_stack(&format!("eq_levels_{spp}"), spp, &[0.9, 1.0, 1.1]);
        for stops in [0.0, 0.3] {
            let on = eq_run(
                &format!("eq_levels_run_{spp}_{stops}"),
                &frames,
                "laplacian",
                eq_settings(2.0, stops),
                None,
                &NoopObserver,
            );
            let estimate = on.report.equalization.as_ref().unwrap();
            assert_eq!((estimate.max_gain, estimate.target_stops), (2.0, stops));
            let mean_of_means = estimate.frames.iter().map(|f| f.mean).sum::<f64>() / 3.0;
            assert!((estimate.neutral - mean_of_means).abs() < 1e-6);
            assert!((estimate.target - estimate.neutral * f64::powf(2.0, stops)).abs() < 1e-9);
            for (i, frame) in estimate.frames.iter().enumerate() {
                assert!(
                    !frame.clamped && frame.clipped_samples == 0,
                    "spp {spp} stops {stops} frame {i}: {frame:?}"
                );
                assert!(
                    (frame.output_mean - estimate.target).abs() <= 1.0,
                    "spp {spp} stops {stops} frame {i}: {frame:?} target {}",
                    estimate.target
                );
            }
        }
    }
    let frames = brightness_stack("eq_levels_off_frames", 3, &[0.9, 1.0, 1.1]);
    let on = eq_run(
        "eq_levels_on",
        &frames,
        "laplacian",
        eq_settings(1.25, 0.0),
        None,
        &NoopObserver,
    );
    let off = eq_run(
        "eq_levels_off",
        &frames,
        "laplacian",
        None,
        None,
        &NoopObserver,
    );
    assert!(on.composite != off.composite, "the gains reach the fuse");
}

/// Records the whole-frame mean its reader delivers, for every frame it
/// aligns and for the reference.
struct MeanRecorder {
    means: Mutex<std::collections::BTreeMap<usize, f64>>,
    calls: AtomicUsize,
}

impl MeanRecorder {
    fn leaked() -> &'static MeanRecorder {
        Box::leak(Box::new(MeanRecorder {
            means: Mutex::default(),
            calls: AtomicUsize::new(0),
        }))
    }
}

impl AlignAlgorithm for MeanRecorder {
    fn id(&self) -> &str {
        "mean-recorder"
    }
    fn display_name(&self) -> &str {
        "Mean Recorder"
    }
    fn params(&self) -> &[ParamDesc] {
        registry::find_align("dummy-align")
            .expect("dummy-align must be registered")
            .params()
    }
    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        _cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stack = ctx.stack();
        let rect = Rect {
            x: 0,
            y: 0,
            width: stack.width,
            height: stack.height,
        };
        for index in [frame, ctx.reference()] {
            let mut samples =
                vec![0u16; (stack.width * stack.height * stack.samples_per_pixel) as usize];
            ctx.read_tile_into(index, rect, &mut samples)?;
            let mean = samples.iter().map(|&v| f64::from(v)).sum::<f64>() / samples.len() as f64;
            self.means.lock().unwrap().insert(index, mean);
        }
        Ok(AlignOutcome {
            transform: Transform::identity(),
            status: AlignStatus::Converged,
            iterations: 1,
            residual: 1.0,
        })
    }
}

fn recorder_request(
    label: &str,
    frames: &[PathBuf],
    recorder: &'static MeanRecorder,
) -> FuseRequest {
    let mut request = base_request(label, frames.to_vec(), ParamValues::new());
    request.align = Some(AlignRequest {
        algorithm: recorder,
        explicit: ParamValues::new(),
        lax: false,
    });
    request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
    request
}

#[test]
fn alignment_reads_the_equalized_frames() {
    let frames = brightness_stack("eq_align_reads", 3, &[0.9, 1.0, 1.1]);
    let raw = MeanRecorder::leaked();
    let request = recorder_request("eq_align_reads_off", &frames, raw);
    focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
        .expect("an unprepared run completes");
    let raw_means = raw.means.lock().unwrap().clone();
    assert_eq!(raw_means.len(), 3);
    let spread = raw_means.values().cloned().fold(f64::MIN, f64::max)
        - raw_means.values().cloned().fold(f64::MAX, f64::min);
    assert!(spread > 1_000.0, "the sources differ: {raw_means:?}");

    let prepared = MeanRecorder::leaked();
    let mut request = recorder_request("eq_align_reads_on", &frames, prepared);
    request.equalize = eq_settings(2.0, 0.0);
    let FuseOutcome::Completed(report) =
        focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
            .expect("a prepared run completes")
    else {
        panic!("expected a completed run");
    };
    let target = report.equalization.as_ref().expect("prepared").target;
    let means = prepared.means.lock().unwrap().clone();
    assert_eq!(means.len(), 3);
    for (frame, mean) in means {
        assert!(
            (mean - target).abs() <= 1.0,
            "frame {frame}: alignment read mean {mean}, target {target}"
        );
    }
}

/// Logs each stage with the job stage the control reports at that moment,
/// and can cancel as preparation is announced.
struct StageProbe {
    control: Arc<JobControl>,
    cancel_on_prepare: bool,
    seen: Mutex<Vec<(&'static str, JobStage)>>,
}

impl StageObserver for StageProbe {
    fn stage(&self, stage: RunStage) {
        let label = match stage {
            RunStage::Develop { .. } => "develop",
            RunStage::Prepare { .. } => "prepare",
            RunStage::Align { .. } => "align",
            RunStage::Fuse => "fuse",
            RunStage::Seal => "seal",
            RunStage::Finished => "finished",
        };
        self.seen
            .lock()
            .unwrap()
            .push((label, self.control.progress().stage));
        if self.cancel_on_prepare && label == "prepare" {
            self.control.cancel();
        }
    }
}

#[test]
fn prepare_stage_is_reported_before_align() {
    let frames = brightness_stack("eq_prepare_stage", 1, &[0.9, 1.0, 1.1]);
    for (equalize, first) in [(eq_settings(1.25, 0.0), "prepare"), (None, "align")] {
        let control = JobControl::new();
        let probe = StageProbe {
            control: Arc::clone(&control),
            cancel_on_prepare: false,
            seen: Mutex::default(),
        };
        let mut request = base_request("eq_prepare_stage_run", frames.clone(), ParamValues::new());
        request.align = Some(dummy_align_request());
        request.equalize = equalize;
        request.output = FuseOutput::Hold(scratch("eq_prepare_stage_hold"));
        focusforge_core::pipeline::run(&request, &probe, control, None).expect("the run completes");
        let seen = probe.seen.lock().unwrap();
        assert!(
            seen.iter().all(|(label, _)| *label != "develop"),
            "a TIFF run develops nothing up front: {seen:?}"
        );
        assert_eq!(seen[0].0, first);
        if first == "prepare" {
            assert_eq!(seen[0], ("prepare", JobStage::Prepare));
            assert_eq!(seen[1], ("align", JobStage::Align));
        } else {
            assert!(seen.iter().all(|(label, _)| *label != "prepare"));
        }
    }
}

#[test]
fn equalization_cancel_during_prepare_publishes_nothing() {
    let frames = brightness_stack("eq_prepare_cancel", 1, &[0.9, 1.0, 1.1]);
    let control = JobControl::new();
    let probe = StageProbe {
        control: Arc::clone(&control),
        cancel_on_prepare: true,
        seen: Mutex::default(),
    };
    let recorder = MeanRecorder::leaked();
    let mut request = recorder_request("eq_prepare_cancel_run", &frames, recorder);
    request.equalize = eq_settings(1.25, 0.0);
    let outcome = focusforge_core::pipeline::run(&request, &probe, control, None)
        .expect("a cancel is not an error");
    assert!(matches!(
        outcome,
        FuseOutcome::Cancelled {
            units_done: 0,
            unit_total: 0
        }
    ));
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 0, "nothing aligned");
    assert_eq!(
        files_under(&request.scratch_root),
        Vec::<PathBuf>::new(),
        "nothing staged, cached or recorded"
    );
    let seen = probe.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "no stage past preparation: {seen:?}");
}

#[test]
fn equalized_run_measures_its_input_sharpness_on_the_gained_frames() {
    let frames = brightness_stack("eq_metrics", 3, &[0.85, 1.0, 1.15]);
    let run = |label: &str, equalize| {
        let held = eq_run(label, &frames, "laplacian", equalize, None, &NoopObserver);
        held.report
            .metrics
            .max_input_lapvar
            .expect("the sharpness window fits the frames")
    };
    let off = run("eq_metrics_off", None);
    let on = run("eq_metrics_on", eq_settings(1.25, 0.0));
    // Variance scales with the square of brightness: the brightest frame is
    // 1.15x the reference raw and the reference once gained.
    let want = off / (1.15 * 1.15);
    assert!(
        (on - want).abs() < 0.05 * want,
        "equalized max input lapvar {on} vs {want}"
    );
}

#[test]
fn equalization_clamps_a_frame_beyond_the_bound_with_a_notice() {
    let frames = brightness_stack("eq_clamp", 3, &[0.6, 1.0, 1.0]);
    let observer = NoticeObserver::default();
    let run = eq_run(
        "eq_clamp_run",
        &frames,
        "laplacian",
        eq_settings(1.25, 0.0),
        None,
        &observer,
    );
    let estimate = run.report.equalization.as_ref().unwrap();
    assert!(estimate.frames[0].clamped && !estimate.frames[1].clamped);
    assert_eq!(estimate.frames[0].applied, 1.25);
    let notices = observer.notices.lock().unwrap();
    let clamp: Vec<_> = notices.iter().filter(|n| n.contains("frame 0")).collect();
    assert_eq!(
        clamp.len(),
        1,
        "one notice for the one clamped frame: {notices:?}"
    );
    for part in ["1.444", "1.25", "1.45"] {
        assert!(clamp[0].contains(part), "{part} in {:?}", clamp[0]);
    }
}

#[test]
fn equalization_frame_notice_carries_the_frame_index() {
    let frames = brightness_stack("eq_frame_notice", 3, &[1.0, 1.0, 0.6]);
    let observer = FrameNoticeObserver::default();
    let run = eq_run(
        "eq_frame_notice_run",
        &frames,
        "laplacian",
        eq_settings(1.25, 0.0),
        None,
        &observer,
    );
    let estimate = run.report.equalization.as_ref().unwrap();
    assert!(estimate.frames[2].clamped && !estimate.frames[0].clamped);
    let notices = observer.notices.lock().unwrap();
    assert_eq!(notices.len(), 1, "one clamped frame: {notices:?}");
    assert_eq!(notices[0].0, 2, "the clamped frame sits at index 2");
    assert!(notices[0].1.contains("frame 2"), "{:?}", notices[0].1);
}

#[test]
fn equalization_keeps_the_depth_map_on_source_frames() {
    let frames = brightness_stack("eq_depth", 3, &[0.9, 1.0, 1.1]);
    let run = eq_run(
        "eq_depth_run",
        &frames,
        "depth-map",
        eq_settings(1.25, 0.0),
        None,
        &NoopObserver,
    );
    let FuseProduct::Held(held) = &run.report.product else {
        panic!("held run");
    };
    let zmap = ZMapReader::open(&held.zmap).expect("the Z-Map opens");
    assert_eq!(zmap.frame_count(), 3);
    zmap.validate_all(64)
        .expect("every Z-Map value is a source frame index");
}

#[test]
fn equalization_is_recorded_on_seal_and_hold() {
    let frames = brightness_stack("eq_record", 3, &[0.9, 1.0, 1.1]);
    let mut request = base_request("eq_record_seal", frames, ParamValues::new());
    request.equalize = eq_settings(1.25, 0.1);
    let FuseOutcome::Completed(report) =
        focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
            .expect("a sealed equalized run must complete")
    else {
        panic!("expected a completed run");
    };
    let estimate = report.equalization.as_ref().expect("estimate reported");
    let want: Vec<u32> = estimate.frames.iter().map(|f| f.gain).collect();
    assert_ne!(want, vec![UNIT_GAIN; 3]);
    let FuseProduct::Sealed(sealed) = &report.product else {
        panic!("expected a sealed project");
    };
    let recorded = Project::open(&sealed.path)
        .expect("reopen")
        .manifest()
        .registration
        .equalization
        .clone()
        .expect("the manifest records the equalization");
    assert_eq!(recorded.max_gain, 1.25);
    assert_eq!(recorded.target_stops, 0.1);
    assert_eq!(recorded.gains, want);

    request.output = FuseOutput::Hold(scratch("eq_record_hold"));
    let FuseOutcome::Completed(held_report) =
        focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
            .expect("a held equalized run must complete")
    else {
        panic!("expected a completed run");
    };
    let FuseProduct::Held(held) = held_report.product else {
        panic!("expected a held checkpoint");
    };
    assert_eq!(held.registration.equalization.as_ref(), Some(&recorded));
    let target = scratch("eq_record_saved").join("held.ffp");
    seal_chain_alone(&target, &held);
    let reopened = Project::open(&target).expect("reopen");
    assert_eq!(
        reopened.manifest().registration.equalization.as_ref(),
        Some(&recorded)
    );
}

#[test]
fn equalization_reaches_every_fusion_path() {
    let frames = brightness_stack("eq_backend", 3, &[0.9, 1.0, 1.1]);
    let host = eq_run(
        "eq_backend_host",
        &frames,
        "laplacian",
        eq_settings(1.25, 0.0),
        None,
        &NoopObserver,
    );
    let ungained = eq_run(
        "eq_backend_off",
        &frames,
        "laplacian",
        None,
        None,
        &NoopObserver,
    );
    assert!(host.composite != ungained.composite);

    let lost: &'static CappedBackend = Box::leak(Box::new(CappedBackend {
        cap: 256,
        shortfall: None,
        asked: Mutex::new(Vec::new()),
        lost: true,
    }));
    let short = leaked_backend(u32::MAX, Some("one tile does not fit"));
    let paths: [(&str, &'static dyn TileBackend); 3] = [
        ("device", capped_backend(256)),
        ("device lost", lost),
        ("host fallback", short),
    ];
    for (name, backend) in paths {
        let run = eq_run(
            &format!("eq_backend_{}", name.replace(' ', "_")),
            &frames,
            "laplacian",
            eq_settings(1.25, 0.0),
            Some(backend),
            &NoopObserver,
        );
        assert!(run.composite == host.composite, "{name}: composite");
        assert!(run.zmap == host.zmap, "{name}: Z-Map");
    }
}

#[test]
fn equalization_change_misses_the_cached_alignment() {
    let frames = brightness_stack("eq_cache", 3, &[0.9, 1.0, 1.1]);
    let root = scratch("eq_cache_root");
    let cache = StageCache::open(&root.join("scratch")).expect("the stage cache opens");
    let run_once = |label: &str, equalize: Option<EqualizeSettings>| {
        let mut request = base_request(label, frames.clone(), ParamValues::new());
        request.scratch_root = root.join("scratch");
        request.align = Some(dummy_align_request());
        request.equalize = equalize;
        request.output = FuseOutput::Hold(root.join(label));
        let FuseOutcome::Completed(report) = focusforge_core::pipeline::run(
            &request,
            &NoopObserver,
            JobControl::new(),
            Some(&cache),
        )
        .expect("the run must complete") else {
            panic!("expected a completed run");
        };
        report
    };
    assert!(!run_once("eq_cache_off", None).align_cached);
    assert!(
        run_once("eq_cache_off_again", None).align_cached,
        "unchanged: a hit"
    );
    let on = run_once("eq_cache_on", eq_settings(1.25, 0.0));
    assert!(!on.align_cached, "preparing the sources is another input");
    assert!(on.equalization.is_some());
    assert!(run_once("eq_cache_on_again", eq_settings(1.25, 0.0)).align_cached);
    assert!(
        !run_once("eq_cache_target", eq_settings(1.25, 0.1)).align_cached,
        "another target changes the gains"
    );
    assert!(
        run_once("eq_cache_bound", eq_settings(1.5, 0.0)).align_cached,
        "a bound that leaves every gain identical still hits"
    );
}

/// One held run over `frames` with alignment and the stage cache, scratch
/// shared under `root`.
fn cached_run(
    label: &str,
    frames: &[PathBuf],
    root: &Path,
    equalize: Option<EqualizeSettings>,
    observer: &dyn StageObserver,
    cache: &StageCache,
) -> Result<FuseOutcome, PipelineError> {
    let mut request = base_request(label, frames.to_vec(), ParamValues::new());
    request.scratch_root = root.join("scratch");
    request.align = Some(dummy_align_request());
    request.equalize = equalize;
    request.output = FuseOutput::Hold(root.join(label));
    focusforge_core::pipeline::run(&request, observer, JobControl::new(), Some(cache))
}

fn completed(outcome: Result<FuseOutcome, PipelineError>) -> FuseReport {
    let FuseOutcome::Completed(report) = outcome.expect("the run must complete") else {
        panic!("expected a completed run");
    };
    *report
}

/// The align-stage key digest of the stack at `frames`, built from the TIFF
/// headers alone.
fn header_key_digest(frames: &[PathBuf]) -> String {
    let fingerprint = StackFingerprint {
        frames: frames
            .iter()
            .map(|p| FrameFingerprint::from(&FrameDesc::from(&TiffHeader::open(p).unwrap())))
            .collect(),
    };
    let identity = AlignIdentity::new(
        "dummy-align",
        ParamValues::new(),
        FailurePolicy::Strict,
        Vec::new(),
    );
    StageKey::align(&fingerprint, &identity).digest()
}

#[test]
fn same_headers_different_pixels_miss_the_stage_cache() {
    let first = brightness_stack("ci_miss_a", 3, &[0.9, 1.0, 1.1]);
    let second = brightness_stack("ci_miss_b", 3, &[1.1, 0.8, 1.0]);
    assert_eq!(
        header_key_digest(&first),
        header_key_digest(&second),
        "the two stacks must be indistinguishable by their headers"
    );
    assert_ne!(
        std::fs::read(&first[0]).unwrap(),
        std::fs::read(&second[0]).unwrap(),
        "the two stacks must differ in pixels"
    );

    let root = scratch("ci_miss_root");
    let cache = StageCache::open(&root.join("scratch")).expect("the stage cache opens");
    let run = |label: &str, frames: &[PathBuf]| {
        completed(cached_run(
            label,
            frames,
            &root,
            None,
            &NoopObserver,
            &cache,
        ))
    };
    assert!(!run("first", &first).align_cached);
    assert!(
        !run("second", &second).align_cached,
        "other pixels under the same headers must not reuse the alignment"
    );
}

#[test]
fn identical_content_at_another_path_hits_the_stage_cache() {
    let first = brightness_stack("ci_hit_a", 3, &[0.9, 1.0, 1.1]);
    let other_dir = scratch("ci_hit_copy");
    let copies: Vec<PathBuf> = first
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let copy = other_dir.join(format!("renamed-{}.tif", 9 - i));
            std::fs::copy(p, &copy).unwrap();
            copy
        })
        .collect();

    let root = scratch("ci_hit_root");
    let cache = StageCache::open(&root.join("scratch")).expect("the stage cache opens");
    let run = |label: &str, frames: &[PathBuf]| {
        completed(cached_run(
            label,
            frames,
            &root,
            None,
            &NoopObserver,
            &cache,
        ))
    };
    let computed = run("first", &first);
    assert!(!computed.align_cached);
    let reused = run("second", &copies);
    assert!(
        reused.align_cached,
        "the same bytes under other names must hit"
    );
    let transforms = |report: &FuseReport| -> Vec<Transform> {
        report
            .align
            .as_ref()
            .expect("an aligned run reports its alignment")
            .alignment
            .frames
            .iter()
            .map(|f| f.transform)
            .collect()
    };
    assert_transforms_bit_eq(&transforms(&reused), &transforms(&computed), "cached");
}

/// Removes every permission from `path` at the first frame notice, the one
/// observer call after the equalization pass has mapped every frame and
/// before the stage-cache lookup; the open mappings keep working.
#[cfg(unix)]
struct LockFrame {
    path: PathBuf,
    still_readable: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
impl StageObserver for LockFrame {
    fn frame_notice(&self, _frame: usize, _message: &str) {
        set_mode(&self.path, 0o000);
        self.still_readable
            .store(std::fs::File::open(&self.path).is_ok(), Ordering::SeqCst);
    }
}

/// Puts a frame's mode back when the test ends, panic or not.
#[cfg(unix)]
struct RestoreMode(PathBuf);

#[cfg(unix)]
impl Drop for RestoreMode {
    fn drop(&mut self) {
        set_mode(&self.0, 0o644);
    }
}

#[cfg(unix)]
#[test]
fn an_unhashable_frame_is_a_stage_cache_miss() {
    // The dim last frame is clamped by the equalization bound, which raises
    // the frame notice that locks the file.
    let frames = brightness_stack("ci_unhash", 3, &[1.0, 1.0, 0.6]);
    let root = scratch("ci_unhash_root");
    let cache = StageCache::open(&root.join("scratch")).expect("the stage cache opens");
    let entries = root.join("scratch").join("stage-cache").join("align");
    let entry_count = || std::fs::read_dir(&entries).map_or(0, Iterator::count);
    let _restore = RestoreMode(frames[1].clone());
    let equalize = eq_settings(1.25, 0.0);
    // `None` when the frame stays readable at mode 000 (a privileged user).
    let locked_run = |label: &str| -> Option<FuseReport> {
        let observer = LockFrame {
            path: frames[1].clone(),
            still_readable: std::sync::atomic::AtomicBool::new(true),
        };
        let outcome = cached_run(label, &frames, &root, equalize, &observer, &cache);
        set_mode(&frames[1], 0o644);
        (!observer.still_readable.load(Ordering::SeqCst)).then(|| completed(outcome))
    };

    let Some(report) = locked_run("first") else {
        eprintln!("skipped: the frame stays readable at mode 000 for this user");
        return;
    };
    assert!(!report.align_cached);
    assert_eq!(entry_count(), 0, "an unhashable frame must leave no entry");

    let populated = completed(cached_run(
        "populate",
        &frames,
        &root,
        equalize,
        &NoopObserver,
        &cache,
    ));
    assert!(!populated.align_cached);
    assert_eq!(entry_count(), 1, "the readable run populates the cache");
    let report = locked_run("second").expect("the frame is still unreadable");
    assert!(
        !report.align_cached,
        "a frame that cannot be hashed must not hit"
    );
}

// ---------------------------------------------------------------------
// The opt-in preview feed.
// ---------------------------------------------------------------------

/// One preview as the observer saw it, with the thread it arrived on.
#[derive(Clone)]
struct Shown {
    kind: PreviewKind,
    width: u32,
    height: u32,
    extent: Rect,
    covered: usize,
    rgba: Vec<u8>,
    tiles: Arc<[Rect]>,
    thread: ThreadId,
    thread_name: String,
}

enum Event {
    Stage(&'static str),
    Tick(ThreadId),
    Shown(Shown),
    Unavailable(PreviewKinds),
}

/// Subscribes to `kinds` and logs stages, ticks and previews in arrival order.
struct PreviewLog {
    kinds: PreviewKinds,
    events: Mutex<Vec<Event>>,
    /// How long each `preview` call takes, so the job outlives a delivery.
    delay: Duration,
}

impl PreviewLog {
    fn new(aligned_frame: bool, composite: bool) -> Self {
        PreviewLog {
            kinds: PreviewKinds {
                aligned_frame,
                composite,
                heat: false,
            },
            events: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        }
    }

    fn with_heat(mut self) -> Self {
        self.kinds.heat = true;
        self
    }

    fn heat_only() -> Self {
        PreviewLog::new(false, false).with_heat()
    }

    fn slow(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn shown(&self, wanted: impl Fn(PreviewKind) -> bool) -> Vec<Shown> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::Shown(s) if wanted(s.kind) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn aligned(&self) -> Vec<Shown> {
        self.shown(|kind| matches!(kind, PreviewKind::AlignedFrame { .. }))
    }

    fn composites(&self) -> Vec<Shown> {
        self.shown(|kind| kind == PreviewKind::Composite)
    }

    fn heats(&self) -> Vec<Shown> {
        self.shown(|kind| kind == PreviewKind::Heat)
    }

    /// The kinds of every `preview_unavailable` call, with the position of
    /// each in the event order.
    fn unavailable(&self) -> Vec<(usize, PreviewKinds)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .filter_map(|(at, event)| match event {
                Event::Unavailable(kinds) => Some((at, *kinds)),
                _ => None,
            })
            .collect()
    }

    /// Position of the first composite preview.
    fn first_composite_at(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .position(|event| matches!(event, Event::Shown(s) if s.kind == PreviewKind::Composite))
            .expect("the run delivered a composite preview")
    }

    /// Position of the first stage marker named `name`.
    fn stage_at(&self, name: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .position(|event| matches!(event, Event::Stage(n) if *n == name))
            .unwrap_or_else(|| panic!("the run never reported the {name} stage"))
    }
}

impl StageObserver for PreviewLog {
    fn stage(&self, stage: RunStage) {
        let name = match stage {
            RunStage::Develop { .. } => "develop",
            RunStage::Prepare { .. } => "prepare",
            RunStage::Align { .. } => "align",
            RunStage::Fuse => "fuse",
            RunStage::Seal => "seal",
            RunStage::Finished => "finished",
        };
        self.events.lock().unwrap().push(Event::Stage(name));
    }

    fn tick(&self, _progress: &JobProgress) {
        let id = std::thread::current().id();
        self.events.lock().unwrap().push(Event::Tick(id));
    }

    fn preview_kinds(&self) -> PreviewKinds {
        self.kinds
    }

    fn preview_unavailable(&self, kinds: PreviewKinds) {
        self.events.lock().unwrap().push(Event::Unavailable(kinds));
    }

    fn preview(&self, preview: &StagePreview) {
        let current = std::thread::current();
        let covered = preview.rgba.chunks_exact(4).filter(|p| p[3] == 255).count();
        self.events.lock().unwrap().push(Event::Shown(Shown {
            kind: preview.kind,
            width: preview.width,
            height: preview.height,
            extent: preview.extent,
            covered,
            rgba: preview.rgba.clone(),
            tiles: Arc::clone(&preview.tiles),
            thread: current.id(),
            thread_name: current.name().unwrap_or_default().to_string(),
        }));
        std::thread::sleep(self.delay);
    }
}

/// A held `pyramidal-ecc` run over `count` mantis frames.
fn ecc_request(label: &str, count: usize, policy: &str) -> FuseRequest {
    let mut explicit = ParamValues::new();
    explicit.insert(
        "reference-policy".to_string(),
        ParamValue::Enum(policy.to_string()),
    );
    let mut request = base_request(label, mantis_frames(count), ParamValues::new());
    request.align = Some(AlignRequest {
        algorithm: registry::find_align("pyramidal-ecc").expect("pyramidal-ecc is registered"),
        explicit,
        lax: true,
    });
    request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
    request
}

fn run_observed(request: &FuseRequest, observer: &dyn StageObserver) -> FuseReport {
    completed(focusforge_core::pipeline::run(
        request,
        observer,
        JobControl::new(),
        None,
    ))
}

fn whole_frame(size: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: size,
        height: size,
    }
}

#[test]
fn preview_feed_delivers_aligned_frames() {
    for policy in ["anchor", "chain"] {
        let log = PreviewLog::new(true, false);
        let request = ecc_request(&format!("pf_aligned_{policy}"), 4, policy);
        let report = run_observed(&request, &log);
        let reference = report.align.as_ref().unwrap().alignment.reference;

        let aligned = log.aligned();
        assert!(!aligned.is_empty(), "{policy}: no aligned-frame preview");
        let (start, end) = (log.stage_at("align"), log.stage_at("fuse"));
        let events = log.events.lock().unwrap();
        for (at, event) in events.iter().enumerate() {
            if matches!(event, Event::Shown(_)) {
                assert!(start < at && at < end, "{policy}: preview outside align");
            }
        }
        for preview in &aligned {
            let PreviewKind::AlignedFrame { frame } = preview.kind else {
                unreachable!("filtered to aligned frames")
            };
            assert_ne!(frame, reference, "{policy}: the reference was previewed");
            assert!(preview.width.max(preview.height) <= 512);
            assert_eq!(preview.width, preview.height, "{policy}: aspect");
            assert_eq!(preview.extent, whole_frame(512), "{policy}: extent");
        }
    }
}

#[test]
fn preview_feed_composite_grows_tile_by_tile() {
    for (tile_size, crop) in [(480u32, false), (448, true)] {
        // A slow delivery lets the job finish while the poll thread is inside
        // it, so only the closing flush can show the last tiles.
        let log = PreviewLog::new(false, true).slow(Duration::from_millis(400));
        let mut explicit = ParamValues::new();
        explicit.insert("levels".to_string(), ParamValue::Int(3));
        let mut request = base_request(&format!("pf_grow_{tile_size}"), mantis_frames(3), explicit);
        request.output = FuseOutput::Hold(scratch(&format!("pf_grow_{tile_size}_hold")));
        request.tile_size = Some(tile_size);
        request.crop = crop;
        let report = run_observed(&request, &log);

        let extent = report.crop.unwrap_or(whole_frame(512));
        assert_eq!(report.crop.is_some(), crop);
        let composites = log.composites();
        assert!(!composites.is_empty(), "tile {tile_size}: no preview");
        let mut previous = 0;
        for preview in &composites {
            assert!(preview.covered >= previous, "coverage must never shrink");
            previous = preview.covered;
            assert_eq!(preview.extent, extent);
            assert_eq!(
                (preview.width, preview.height),
                (extent.width, extent.height)
            );
        }
        let last = composites.last().unwrap();
        assert_eq!(last.covered, (extent.width * extent.height) as usize);
        let seal = log.stage_at("seal");
        let events = log.events.lock().unwrap();
        let last_shown = events
            .iter()
            .rposition(|e| matches!(e, Event::Shown(_)))
            .unwrap();
        assert!(last_shown < seal, "the full image arrives before the seal");
    }
}

#[test]
fn preview_feed_is_throttled_to_the_poll() {
    let log = PreviewLog::new(true, true);
    let mut request = ecc_request("pf_throttle", 4, "anchor");
    request.tile_size = Some(256);
    request
        .explicit
        .insert("levels".to_string(), ParamValue::Int(3));
    run_observed(&request, &log);

    let (align_at, fuse_at) = (log.stage_at("align"), log.stage_at("fuse"));
    let events = log.events.lock().unwrap();
    let tick_threads: Vec<ThreadId> = events
        .iter()
        .filter_map(|e| match e {
            Event::Tick(id) => Some(*id),
            _ => None,
        })
        .collect();
    assert!(!tick_threads.is_empty());
    let shown: Vec<&Shown> = events
        .iter()
        .filter_map(|e| match e {
            Event::Shown(s) => Some(s),
            _ => None,
        })
        .collect();
    for s in &shown {
        assert!(tick_threads.contains(&s.thread), "not a poll thread");
        assert!(!s.thread_name.starts_with("focusforge-align"));
    }
    // Per stage: the polls deliver at most one image per kind each, plus the
    // one closing flush.
    let ticks = |from: usize, to: usize| {
        events[from..to]
            .iter()
            .filter(|e| matches!(e, Event::Tick(_)))
            .count()
    };
    let aligned = shown
        .iter()
        .filter(|s| matches!(s.kind, PreviewKind::AlignedFrame { .. }))
        .count();
    let composites = shown.len() - aligned;
    assert!(aligned <= ticks(align_at, fuse_at) + 1, "{aligned} aligned");
    assert!(
        composites <= ticks(fuse_at, events.len()) + 1,
        "{composites} composite"
    );
}

/// Overrides `preview` but never subscribes.
#[derive(Default)]
struct DeafSubscriber {
    calls: AtomicUsize,
}

impl StageObserver for DeafSubscriber {
    fn preview(&self, _preview: &StagePreview) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn preview_feed_absent_without_subscriber() {
    let observer = DeafSubscriber::default();
    let request = ecc_request("pf_absent", 3, "anchor");
    run_observed(&request, &observer);
    assert_eq!(observer.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn preview_cost_is_reported_per_requested_kind() {
    let plain = run_observed(&ecc_request("pf_cost_plain", 3, "anchor"), &NoopObserver);
    assert_eq!(plain.preview_cost, PreviewCost::default());

    let log = PreviewLog::new(true, true);
    let report = run_observed(&ecc_request("pf_cost_sub", 3, "anchor"), &log);
    let cost = report.preview_cost;
    assert!(cost.composite > Duration::ZERO, "composite work is charged");
    assert!(
        cost.aligned_frame > Duration::ZERO,
        "aligned work is charged"
    );
    assert_eq!(cost.heat, Duration::ZERO, "heat was not requested");

    let heat = PreviewLog::heat_only();
    let report = run_observed(&ecc_request("pf_cost_heat", 3, "anchor"), &heat);
    assert!(report.preview_cost.heat > Duration::ZERO);
    assert_eq!(report.preview_cost.composite, Duration::ZERO);
}

fn mantis8_frames(count: usize) -> Vec<PathBuf> {
    let dir = common::fixtures_dir().join("mantis8-64").join("frames");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("frames dir must exist")
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries.truncate(count);
    entries
}

#[test]
fn preview_feed_leaves_output_bit_identical() {
    let requests = |label: &str| {
        let aligned = ecc_request(&format!("{label}_16"), 3, "anchor");
        let mut eight = base_request(&format!("{label}_8"), mantis8_frames(3), ParamValues::new());
        eight.output = FuseOutput::Hold(scratch(&format!("{label}_8_hold")));
        [aligned, eight]
    };
    let plain = requests("pf_bits_plain");
    let subscribed = requests("pf_bits_sub");
    for (plain, subscribed) in plain.iter().zip(&subscribed) {
        let log = PreviewLog::new(true, true);
        let with = held_run_of(Box::new(run_observed(subscribed, &log)));
        let without = held_run_of(Box::new(run_observed(plain, &NoopObserver)));
        assert!(!log.composites().is_empty(), "the subscribed run previews");
        assert!(with.composite == without.composite, "composite differs");
        assert!(with.zmap == without.zmap, "Z-Map differs");
    }
}

#[test]
fn preview_feed_cached_and_resumed_runs() {
    // A cached alignment registers nothing, so nothing is previewed.
    let root = scratch("pf_cache_root");
    let cache = StageCache::open(&root.join("scratch")).expect("the stage cache opens");
    let cached = |label: &str, log: &PreviewLog| {
        let mut request = ecc_request(label, 3, "anchor");
        request.scratch_root = root.join("scratch");
        completed(focusforge_core::pipeline::run(
            &request,
            log,
            JobControl::new(),
            Some(&cache),
        ))
    };
    let first = PreviewLog::new(true, true);
    assert!(!cached("pf_cache_a", &first).align_cached);
    assert!(!first.aligned().is_empty());
    let second = PreviewLog::new(true, true);
    assert!(cached("pf_cache_b", &second).align_cached);
    assert!(
        second.aligned().is_empty(),
        "a cached alignment previews nothing"
    );
    assert!(!second.composites().is_empty());

    // A resumed fuse previews only the tiles it fuses itself, and an
    // alignment read back from the resume ledger previews no frames.
    for aligned in [false, true] {
        let label = format!("pf_resume_{aligned}");
        let mut request = if aligned {
            ecc_request(&label, 3, "anchor")
        } else {
            base_request(&label, mantis_frames(3), ParamValues::new())
        };
        request
            .explicit
            .insert("levels".to_string(), ParamValue::Int(3));
        request.tile_size = Some(256);
        request.max_host_memory = Some(1);
        request.poll = Duration::from_millis(1);
        request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
        let control = JobControl::new();
        let outcome = focusforge_core::pipeline::run(
            &request,
            &CancelAfterFirstUnit(Arc::clone(&control)),
            control,
            None,
        )
        .expect("a cancel is not an error");
        assert!(matches!(outcome, FuseOutcome::Cancelled { .. }));
        request.resume = true;
        let log = PreviewLog::new(aligned, true);
        let report = run_observed(&request, &log);
        assert_eq!(
            report.align_source,
            if aligned { "resumed" } else { "skipped" },
            "aligned={aligned}"
        );
        assert!(
            log.aligned().is_empty(),
            "aligned={aligned}: a resumed alignment previews no frames"
        );
        let composites = log.composites();
        let last = composites.last().expect("the resumed run previews");
        assert!(last.covered > 0, "aligned={aligned}");
        assert!(
            last.covered < 512 * 512,
            "aligned={aligned}: tiles fused before the cancel must not appear"
        );
    }
}

#[test]
fn preview_feed_gpu_path() {
    for lost in [false, true] {
        let backend = || -> &'static dyn TileBackend {
            Box::leak(Box::new(CappedBackend {
                cap: u32::MAX,
                shortfall: None,
                asked: Mutex::new(Vec::new()),
                lost,
            }))
        };
        let plain = held_run_observed("pf_gpu_plain", 256, 3, Some(backend()), &NoopObserver)
            .expect("an unsubscribed backend run");
        let log = PreviewLog::new(false, true);
        let run = held_run_observed("pf_gpu_sub", 256, 3, Some(backend()), &log)
            .expect("a subscribed backend run");
        assert!(run.composite == plain.composite, "lost={lost}: composite");
        assert!(run.zmap == plain.zmap, "lost={lost}: Z-Map");
        let last = log.composites().pop().expect("the backend path previews");
        assert_eq!(last.covered, 512 * 512, "lost={lost}");
    }
}

#[test]
fn preview_feed_subscribes_per_kind() {
    let only_composite = PreviewLog::new(false, true);
    run_observed(&ecc_request("pf_kind_c", 3, "anchor"), &only_composite);
    assert!(only_composite.aligned().is_empty());
    assert!(!only_composite.composites().is_empty());

    let only_aligned = PreviewLog::new(true, false);
    run_observed(&ecc_request("pf_kind_a", 3, "anchor"), &only_aligned);
    assert!(only_aligned.composites().is_empty());
    assert!(!only_aligned.aligned().is_empty());
}

const STACK_ALGORITHMS: [&str; 3] = ["laplacian", "depth-map", "weighted-average"];

/// A held run over three mantis frames with `algorithm`, observed by `observer`.
fn heat_run(
    label: &str,
    algorithm: &str,
    tile_size: u32,
    crop: bool,
    observer: &dyn StageObserver,
) -> FuseReport {
    let mut explicit = ParamValues::new();
    if algorithm == "laplacian" {
        explicit.insert("levels".to_string(), ParamValue::Int(3));
    }
    let mut request = base_request(label, mantis_frames(3), explicit);
    request.algorithm = registry::find_stack(algorithm).expect("a registered algorithm");
    request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
    request.tile_size = Some(tile_size);
    request.crop = crop;
    run_observed(&request, observer)
}

#[test]
fn sharpness_heatmap_covers_every_tile() {
    for algorithm in STACK_ALGORITHMS {
        for (tile_size, crop) in [(256u32, false), (448, true)] {
            let label = format!("hm_cover_{algorithm}_{tile_size}");
            let log = PreviewLog::new(false, true).with_heat();
            let report = heat_run(&label, algorithm, tile_size, crop, &log);
            let context = format!("{algorithm} tile {tile_size} crop {crop}");

            assert!(report.tiles >= 2, "{context}: {} tiles", report.tiles);
            let (heats, composites) = (log.heats(), log.composites());
            assert!(!heats.is_empty(), "{context}: no heat preview");
            let mut previous = 0;
            for heat in &heats {
                assert!(heat.covered >= previous, "{context}: coverage shrank");
                previous = heat.covered;
                assert_eq!(heat.extent, composites[0].extent, "{context}: extent");
                assert_eq!(
                    (heat.width, heat.height),
                    (composites[0].width, composites[0].height),
                    "{context}: size"
                );
            }
            let last = heats.last().unwrap();
            assert_eq!(
                last.covered,
                (last.width * last.height) as usize,
                "{context}"
            );
            let events = log.events.lock().unwrap();
            let last_shown = events
                .iter()
                .rposition(|e| matches!(e, Event::Shown(s) if s.kind == PreviewKind::Heat))
                .unwrap();
            drop(events);
            assert!(
                last_shown < log.stage_at("seal"),
                "{context}: arrives before the seal"
            );
        }
    }
}

#[test]
fn composite_preview_carries_the_tile_grid() {
    for (tile_size, crop) in [(256u32, false), (256, true), (448, true)] {
        let label = format!("grid_{tile_size}_{crop}");
        let log = PreviewLog::new(false, true).with_heat();
        heat_run(&label, "laplacian", tile_size, crop, &log);
        let shown: Vec<Shown> = log.heats().into_iter().chain(log.composites()).collect();
        assert!(
            !shown.is_empty(),
            "tile {tile_size} crop {crop}: no preview"
        );
        for preview in &shown {
            let (w, h) = (preview.extent.width, preview.extent.height);
            assert!(
                preview.tiles.len() > 1,
                "tile {tile_size} crop {crop}: {} tiles",
                preview.tiles.len()
            );
            let mut painted = vec![0u8; w as usize * h as usize];
            for tile in preview.tiles.iter() {
                assert!(tile.x + tile.width <= w && tile.y + tile.height <= h);
                for y in tile.y..tile.y + tile.height {
                    for x in tile.x..tile.x + tile.width {
                        painted[(y * w + x) as usize] += 1;
                    }
                }
            }
            assert!(
                painted.iter().all(|&n| n == 1),
                "tile {tile_size} crop {crop}: the tiles do not partition the output extent"
            );
            assert_eq!(preview.tiles, shown[0].tiles, "one grid per run");
        }
    }
}

#[test]
fn sharpness_heatmap_scale_is_run_wide() {
    for algorithm in STACK_ALGORITHMS {
        let last_heat = |tile_size: u32| {
            let log = PreviewLog::heat_only();
            let label = format!("hm_scale_{algorithm}_{tile_size}");
            heat_run(&label, algorithm, tile_size, false, &log);
            log.heats().pop().expect("a heat preview").rgba
        };
        let whole = last_heat(512);
        assert!(
            whole.chunks_exact(4).any(|p| p[0] > 0),
            "{algorithm}: all cold"
        );
        for tile_size in [256, 480] {
            assert!(
                last_heat(tile_size) == whole,
                "{algorithm}: tile {tile_size} differs from one 512 tile"
            );
        }
    }
}

#[test]
fn sharpness_heatmap_absent_for_algorithm_without_plane() {
    let all = PreviewLog::new(true, true).with_heat();
    let mut request = base_request("hm_absent", mantis_frames(3), ParamValues::new());
    request.algorithm = registry::find_stack("dummy-stack").expect("dummy-stack is registered");
    request.output = FuseOutput::Hold(scratch("hm_absent_hold"));
    run_observed(&request, &all);

    let calls = all.unavailable();
    assert_eq!(calls.len(), 1, "one call");
    let heat_only = PreviewKinds {
        heat: true,
        ..PreviewKinds::default()
    };
    assert_eq!(calls[0].1, heat_only);
    assert!(all.stage_at("fuse") < calls[0].0);
    assert!(calls[0].0 < all.first_composite_at());
    assert!(all.heats().is_empty(), "no heat without a plane");
    assert!(!all.composites().is_empty(), "the composite still grows");

    let no_heat = PreviewLog::new(true, true);
    request.output = FuseOutput::Hold(scratch("hm_absent_off_hold"));
    run_observed(&request, &no_heat);
    assert!(no_heat.unavailable().is_empty(), "heat was not asked for");

    let supported = PreviewLog::heat_only();
    heat_run("hm_absent_supported", "laplacian", 448, false, &supported);
    assert!(
        supported.unavailable().is_empty(),
        "laplacian supplies heat"
    );
}

#[test]
fn sharpness_heatmap_output_identical_with_and_without_subscriber() {
    use focusforge_core::display::PreviewFeed;

    let heat_only = PreviewKinds {
        heat: true,
        ..PreviewKinds::default()
    };
    assert!(PreviewFeed::new(heat_only, (64, 64)).is_some());
    assert!(PreviewFeed::new(PreviewKinds::default(), (64, 64)).is_none());

    for algorithm in STACK_ALGORITHMS {
        let run = |label: &str, observer: &dyn StageObserver| {
            held_run_of(Box::new(heat_run(
                &format!("hm_bits_{algorithm}_{label}"),
                algorithm,
                256,
                false,
                observer,
            )))
        };
        let plain = run("plain", &NoopObserver);
        let without_heat = run("no_heat", &PreviewLog::new(true, true));
        let log = PreviewLog::new(true, true).with_heat();
        let with_heat = run("heat", &log);
        assert!(!log.heats().is_empty(), "{algorithm}: heat was delivered");
        for (label, other) in [("without heat", &without_heat), ("with heat", &with_heat)] {
            assert!(
                other.composite == plain.composite,
                "{algorithm} {label}: composite"
            );
            assert!(other.zmap == plain.zmap, "{algorithm} {label}: Z-Map");
        }

        let only_heat = PreviewLog::heat_only();
        run("heat_only", &only_heat);
        assert!(!only_heat.heats().is_empty());
        assert!(only_heat.composites().is_empty(), "{algorithm}: per-kind");
    }
}

/// `count` 12-bit DNG frames of `width` x `height` under `dir`, each sharp in
/// its own vertical band and bright enough that the as-shot white balance
/// lifts the red channel past white.
fn raw_frames(dir: &Path, count: usize, width: u32, height: u32) -> Vec<PathBuf> {
    (0..count)
        .map(|frame| {
            let samples = (0..width * height)
                .map(|i| {
                    let (x, y) = (i % width, i / width);
                    let band = (x as usize * count / width as usize).min(count - 1);
                    let amplitude = if band == frame { 380 } else { 50 };
                    let checker = if (x / 2 + y / 2) % 2 == 0 {
                        0
                    } else {
                        amplitude
                    };
                    (2_700 + frame as u32 * 40 + checker) as u16
                })
                .collect();
            let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], samples);
            spec.width = width;
            spec.height = height;
            spec.as_shot_neutral = Some([0.5, 1.0, 0.625]);
            let written = write_dng(&spec, false);
            let path = dir.join(format!("{frame:02}.dng"));
            std::fs::copy(written.path(), &path).expect("the DNG frame copies");
            path
        })
        .collect()
}

#[test]
fn raw_run_previews_its_f32_composite_and_heat() {
    let frames = raw_frames(&scratch("raw_preview_frames"), 3, 320, 256);
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(2));
    let mut request = base_request("raw_preview", frames, explicit);
    request.output = FuseOutput::Hold(scratch("raw_preview_hold"));
    request.tile_size = Some(128);
    let log = PreviewLog::new(false, true).with_heat();
    let report = run_observed(&request, &log);

    let FuseProduct::Held(held) = &report.product else {
        panic!("a held run");
    };
    assert_eq!(
        focusforge_core::io::TiffStack::open_staged(&held.composite)
            .expect("the held composite opens")
            .sample_format(),
        SampleFormat::F32,
        "a RAW stack fuses into an f32 composite"
    );
    assert!(report.tiles >= 2, "{} tiles", report.tiles);
    assert!(log.unavailable().is_empty(), "every kind is delivered");

    for (kind, shown) in [("composite", log.composites()), ("heat", log.heats())] {
        let last = shown.last().unwrap_or_else(|| panic!("no {kind} preview"));
        assert_eq!(
            last.covered,
            (last.width * last.height) as usize,
            "the last {kind} preview covers the whole extent"
        );
        assert!(
            last.rgba.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]),
            "the {kind} preview is not black"
        );
        assert!(last.tiles.len() >= 2, "the {kind} preview carries the grid");
    }
}

#[test]
fn sharpness_heatmap_backend_path() {
    let cpu = PreviewLog::heat_only();
    held_run_observed("hm_backend_cpu", 256, 3, None, &cpu).expect("a plain CPU run");
    let expected = cpu.heats().pop().expect("the CPU path previews heat");
    assert_eq!(expected.covered, 512 * 512);

    for lost in [false, true] {
        let backend: &'static dyn TileBackend = Box::leak(Box::new(CappedBackend {
            cap: u32::MAX,
            shortfall: None,
            asked: Mutex::new(Vec::new()),
            lost,
        }));
        let log = PreviewLog::heat_only();
        held_run_observed(&format!("hm_backend_{lost}"), 256, 3, Some(backend), &log)
            .expect("a subscribed backend run");
        let last = log.heats().pop().expect("the backend path previews heat");
        assert_eq!(last.covered, 512 * 512, "lost={lost}");
        assert!(
            last.rgba == expected.rgba,
            "lost={lost}: differs from the CPU run"
        );
    }
}
