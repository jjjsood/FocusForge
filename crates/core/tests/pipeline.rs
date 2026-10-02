// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge_core::pipeline::run` on its own, over `fixtures/mantis-512`:
//! parameter resolution against the frames the driver actually opens, the
//! `Cancelling` interval during alignment, and the pause/resume segment
//! contract. `focusforge-cli` and `focusforge-gui` are this driver's two
//! callers; this file exercises the driver directly, through neither.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus, StackAlgorithm,
    TileCtx, TileOutput, TileReader,
};
use focusforge_core::align::warp::WarpedTile;
use focusforge_core::align::{AlignError, Transform};
use focusforge_core::fuse::laplacian::Laplacian;
use focusforge_core::gpu::{BackendInfo, TileBackend};
use focusforge_core::job::{ControlState, JobControl, JobProgress, JobStage, StageCache};
use focusforge_core::params::{ParamDesc, ParamValue, ParamValues, Provenance};
use focusforge_core::pipeline::{
    AlignRequest, FuseOutcome, FuseOutput, FuseProduct, FuseReport, FuseRequest, PipelineError,
    RunStage, StageObserver,
};
use focusforge_core::project::{self, Project};
use focusforge_core::registry;
use focusforge_core::tile::{Rect, Tile, TileError};

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

fn scratch(label: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("pipeline_{label}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
    dir
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
        tile_size: Some(448),
        max_host_memory: None,
        backend: None,
        align_backend: None,
        resume: false,
        cache: true,
        poll: Duration::from_millis(5),
    }
}

#[derive(Default)]
struct NoopObserver;
impl StageObserver for NoopObserver {}

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
    let frames: Vec<PathBuf> = mantis_frames(8)
        .into_iter()
        .cycle()
        .take(frame_count)
        .collect();

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
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.pause_on {
            self.control.pause();
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
    let frames: Vec<PathBuf> = mantis_frames(8)
        .into_iter()
        .cycle()
        .take(frame_count)
        .collect();

    // The pausing call itself, plus at most one in-flight registration per
    // other worker; the chain arm registers on one thread.
    for (policy, bound) in [("anchor", PAUSE_ON + workers - 1), ("chain", PAUSE_ON)] {
        let control = JobControl::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let algorithm: &'static PausingAlign = Box::leak(Box::new(PausingAlign {
            control: Arc::clone(&control),
            pause_on: PAUSE_ON,
            calls: Arc::clone(&calls),
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
    let frames: Vec<PathBuf> = mantis_frames(8)
        .into_iter()
        .cycle()
        .take(frame_count)
        .collect();

    for (policy, bound) in [("anchor", PAUSE_ON + workers - 1), ("chain", PAUSE_ON)] {
        let control = JobControl::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let algorithm: &'static PausingAlign = Box::leak(Box::new(PausingAlign {
            control: Arc::clone(&control),
            pause_on: PAUSE_ON,
            calls: Arc::clone(&calls),
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
            mantis_frames(count),
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
    let request = base_request("align_progress_none", mantis_frames(2), ParamValues::new());
    let control = JobControl::new();
    let outcome =
        focusforge_core::pipeline::run(&request, &NoopObserver, Arc::clone(&control), None)
            .expect("a run with no align stage must complete");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));
    let progress = control.progress();
    assert_eq!(progress.align_done, 0);
    assert_eq!(progress.align_total, 0);
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

    let hold_dir = scratch("held_run_hold");
    let mut held_request = base_request("held_run", frames.clone(), ParamValues::new());
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
    project::seal_held(&target, std::slice::from_ref(&held), false)
        .expect("the held checkpoint must seal");

    let direct_request = base_request("direct_run", frames, ParamValues::new());
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
    project::seal_held(&target, std::slice::from_ref(&held), false)
        .expect("the held checkpoint must seal");
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
/// on a side.
struct CappedBackend {
    cap: u32,
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

fn capped_backend(cap: u32) -> &'static dyn TileBackend {
    Box::leak(Box::new(CappedBackend { cap }))
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
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(levels));
    let mut request = base_request(label, mantis_frames(3), explicit);
    request.output = FuseOutput::Hold(scratch(&format!("{label}_hold")));
    request.tile_size = Some(tile_size);
    request.backend = backend;
    let FuseOutcome::Completed(report) =
        focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)?
    else {
        panic!("expected a completed run");
    };
    let FuseProduct::Held(held) = &report.product else {
        panic!("a hold request must report a held checkpoint");
    };
    let composite = std::fs::read(&held.composite).expect("composite must be readable");
    let zmap = std::fs::read(&held.zmap).expect("z-map must be readable");
    Ok(HeldRun {
        report,
        composite,
        zmap,
    })
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
fn a_cap_below_the_levels_minimum_fails_with_no_interior() {
    let Err(err) = held_run("cap_64", 448, 5, Some(capped_backend(64))) else {
        panic!("a 64-pixel cap leaves no interior at five levels");
    };
    assert!(
        matches!(
            err,
            PipelineError::Tile(TileError::NoInterior { tile_size: 64, .. })
        ),
        "unexpected error: {err}"
    );
}
