// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge_core::pipeline::run` on its own, over `fixtures/mantis-512`:
//! parameter resolution against the frames the driver actually opens, the
//! `Cancelling` interval during alignment, and the pause/resume segment
//! contract. `focusforge-cli` and `focusforge-gui` are this driver's two
//! callers; this file exercises the driver directly, through neither.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use focusforge_core::align::Transform;
use focusforge_core::job::{ControlState, JobControl, JobProgress};
use focusforge_core::params::{ParamValue, ParamValues, Provenance};
use focusforge_core::pipeline::{
    AlignRequest, FuseOutcome, FuseOutput, FuseProduct, FuseRequest, RunStage, StageObserver,
};
use focusforge_core::project::{self, Project};
use focusforge_core::registry;

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
    // Requested before the driver ever runs: alignment still has to be
    // reported (the engine's own "reported, never controlled" contract), and
    // the cancel can only land once it returns.
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
