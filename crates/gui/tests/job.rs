// SPDX-License-Identifier: GPL-3.0-or-later

//! Fusing from the window: a fuse run driven through
//! `AppController`, with no window and `backend = None`. Fast:
//! `fixtures/mantis-512` through the real engine, each run under its own
//! tempdir and its own scratch root, never the crate's shared `.scratch/`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::pipeline::{self, FuseOutcome, FuseRequest, StageObserver};
use focusforge_core::project::{self, Project};
use focusforge_core::registry;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use focusforge_gui::job;
use focusforge_gui::{AppController, OpenOutcome};

// ---------------------------------------------------------------------
// Fixture helpers -- copies only, never the committed fixture directory
// itself, since `start_fuse` seals a `.ffp` beside the frames it fuses.
// ---------------------------------------------------------------------

fn copy_frame(dest_dir: &Path, name: &str, source_index: usize) {
    std::fs::create_dir_all(dest_dir).unwrap();
    std::fs::copy(
        common::fixture_frame("mantis-512", source_index),
        dest_dir.join(name),
    )
    .expect("fixture frame must copy");
}

/// A controller with one unsaved project open over `count` copied fixture
/// frames (named `00.tif`, `01.tif`, ... in fixture order), its own scratch
/// root under `CARGO_TARGET_TMPDIR`.
fn controller_with_frames(name: &str, count: usize) -> AppController {
    let root = common::tree(&format!("{name}_frames"));
    for i in 0..count {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    let mut controller = AppController::new();
    assert!(matches!(
        controller.open_paths(&[root]),
        OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

/// A controller with two unsaved tabs, each over its own copied fixture
/// frames -- three for tab 0, two for tab 1, so a per-tab assertion cannot
/// pass on a shared frame count. Tab 0 is selected on return.
fn controller_with_two_tabs(name: &str) -> AppController {
    let mut controller = controller_with_frames(&format!("{name}_a"), 3);
    let root_b = common::tree(&format!("{name}_b_frames"));
    for i in 0..2 {
        copy_frame(&root_b, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        controller.open_paths(&[root_b]),
        OpenOutcome::Opened(_)
    ));
    controller.select_project(0);
    controller
}

/// A controller with three unsaved tabs, each over its own copied fixture
/// frames -- three, two and four frames, so a per-tab assertion cannot pass
/// on a shared frame count. Tab 0 is selected on return.
fn controller_with_three_tabs(name: &str) -> AppController {
    let mut controller = controller_with_two_tabs(name);
    let root_c = common::tree(&format!("{name}_c_frames"));
    for i in 0..4 {
        copy_frame(&root_c, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        controller.open_paths(&[root_c]),
        OpenOutcome::Opened(_)
    ));
    controller.select_project(0);
    controller
}

/// Starts a fuse on the active tab and holds it at a pause once the
/// observer has ticked at least one unit of progress -- pausing before any
/// tick would let a later assertion read a held run at `0.0` for reasons
/// this helper does not intend to prove.
fn start_and_hold_at_pause(controller: &mut AppController, timeout: Duration) {
    assert!(controller.start_fuse());
    let start = Instant::now();
    while controller.job_view().fraction <= 0.0 {
        assert!(
            start.elapsed() < timeout,
            "the run never ticked progress within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    controller.pause_or_resume();
    assert_eq!(controller.job_view().state, job::RunState::Paused);
}

// ---------------------------------------------------------------------
// A fuse run completes off the UI thread, seals, promotes, feeds
// the canvas.
// ---------------------------------------------------------------------

#[test]
fn fuse_runs_to_completion_off_the_ui_thread() {
    let mut controller = controller_with_frames("completion", 3);
    assert!(controller.start_fuse());
    // The call itself must not have blocked: the worker is still doing the
    // actual work the instant control returns to the caller.
    assert!(
        controller.job_running(),
        "start_fuse must return before the run finishes"
    );

    let planes = common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("a completed run must hand back its two materialized planes");
    assert!(planes.0.is_file() && planes.1.is_file());
    assert!(!controller.job_running());
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected a completed fuse, got {other:?}"),
    }
    assert!(
        !controller.project_tabs()[0].unsaved,
        "a sealed run promotes the tab from Unsaved to Opened"
    );
}

// ---------------------------------------------------------------------
// The same driver call, byte for byte against the golden at its own
// geometry.
// ---------------------------------------------------------------------

#[test]
fn gui_run_matches_the_cli_run() {
    // Structural half: the GUI's worker and the CLI's own spine call the
    // same extracted driver, not two copies of it.
    let gui_source = std::fs::read_to_string(common::src_dir().join("job.rs")).unwrap();
    assert!(
        gui_source.contains("pipeline::run("),
        "the GUI's worker must call the extracted pipeline driver"
    );
    let cli_source = std::fs::read_to_string(
        common::workspace_root()
            .join("crates")
            .join("cli")
            .join("src")
            .join("stack.rs"),
    )
    .unwrap();
    assert!(
        cli_source.contains("pipeline::run("),
        "the CLI's own spine must call the same extracted driver"
    );

    // Byte-for-byte half: the same eight frames the golden was built from,
    // pinned to the golden's own geometry (448/5, unaligned) -- neither the
    // panel's defaults nor the CLI's own default tile size, so this is a
    // genuine comparison rather than a re-assertion of a default.
    let root = common::tree("cli_match_frames");
    for i in 0..8 {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    let frames: Vec<PathBuf> = (0..8).map(|i| root.join(format!("{i:02}.tif"))).collect();

    let scratch = common::tree("cli_match_scratch");
    let mut explicit = ParamValues::new();
    explicit.insert(
        "levels".to_string(),
        focusforge_core::params::ParamValue::Int(5),
    );
    let request = FuseRequest {
        frames,
        project_path: root.join("pinned.ffp"),
        scratch_root: scratch,
        staging_key: "cli-match".to_string(),
        algorithm: registry::find_stack("laplacian").expect("laplacian must be registered"),
        explicit,
        align: None,
        crop: false,
        tile_size: Some(448),
        max_host_memory: None,
        backend: None,
        resume: false,
        cache: true,
        append: false,
        poll: Duration::from_millis(5),
    };
    struct Noop;
    impl StageObserver for Noop {}
    let outcome = pipeline::run(
        &request,
        &Noop,
        focusforge_core::job::JobControl::new(),
        None,
    )
    .expect("the pinned run must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };

    let golden_path = common::workspace_root()
        .join("fixtures")
        .join("mantis-512")
        .join("golden")
        .join("laplacian.tif");
    let golden = focusforge_core::io::TiffStack::open_files(&[golden_path]).unwrap();
    let (gw, gh) = golden.dimensions();
    assert_eq!((gw, gh), (report.width, report.height));
    let golden_samples = golden
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width: gw,
                height: gh,
            },
        )
        .unwrap()
        .samples;
    // The composite is a compressed blob inside the sealed `.ffp`, not a
    // standalone file -- materialize it through the same path `settle_job`
    // and `--open <saved>.ffp` already use.
    let (_project, composite_path, _zmap_path) = job::open_and_materialize(&report.sealed.path)
        .expect("the sealed project's composite must materialize");
    let sealed = focusforge_core::io::TiffStack::open_files(&[composite_path]).unwrap();
    let sealed_samples = sealed
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width: gw,
                height: gh,
            },
        )
        .unwrap()
        .samples;
    assert_eq!(
        golden_samples, sealed_samples,
        "the GUI's own driver call must reproduce the golden composite byte for byte"
    );
}

// ---------------------------------------------------------------------
// `eta` renders `estimating`/`ETA —`, never a rendered zero, while
// there is no number to show.
// ---------------------------------------------------------------------

#[test]
fn eta_none_never_renders_as_zero() {
    use focusforge_gui::job::{RunState, eta_text};
    assert_eq!(eta_text(&RunState::Idle, None), "");
    assert_eq!(eta_text(&RunState::Running, None), "estimating");
    assert_eq!(eta_text(&RunState::Paused, None), "ETA —");
    assert_eq!(eta_text(&RunState::Cancelling, None), "ETA —");
    assert_eq!(eta_text(&RunState::Failed("x".to_string()), None), "ETA —");
    for state in [
        RunState::Idle,
        RunState::Running,
        RunState::Paused,
        RunState::Cancelling,
        RunState::Failed("x".to_string()),
    ] {
        assert!(
            !eta_text(&state, None).contains("0:00"),
            "{state:?} must never render a rendered zero with no number to show"
        );
    }
}

// ---------------------------------------------------------------------
// Pause holds the run and releases its workers; resume continues rather
// than restarting, with the unit total unchanged. `control.pause()` is
// requested before the call starts, so the first segment observes it
// before any unit runs; resume runs synchronously inside `paused`, on
// the same worker thread.
// ---------------------------------------------------------------------

struct ResumeOnPause {
    calls: AtomicUsize,
    unit_total_at_pause: Mutex<Option<usize>>,
}

impl StageObserver for ResumeOnPause {
    fn paused(&self, control: &focusforge_core::job::JobControl) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.unit_total_at_pause.lock().unwrap() = Some(control.progress().unit_total);
        control.clear();
    }
}

fn no_align_request(label: &str, frame_count: usize) -> FuseRequest {
    let root = common::tree(label);
    for i in 0..frame_count {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    let frames = (0..frame_count)
        .map(|i| root.join(format!("{i:02}.tif")))
        .collect();
    FuseRequest {
        frames,
        project_path: root.join("project.ffp"),
        scratch_root: common::tree(&format!("{label}_scratch")),
        staging_key: label.to_string(),
        algorithm: registry::find_stack("laplacian").expect("laplacian must be registered"),
        explicit: ParamValues::new(),
        align: None,
        crop: false,
        tile_size: None,
        max_host_memory: None,
        backend: None,
        resume: false,
        cache: true,
        append: false,
        poll: Duration::from_millis(5),
    }
}

#[test]
fn pause_then_resume_continues() {
    let request = no_align_request("pause_then_resume", 2);
    let control = focusforge_core::job::JobControl::new();
    control.pause();
    let observer = ResumeOnPause {
        calls: AtomicUsize::new(0),
        unit_total_at_pause: Mutex::new(None),
    };
    let outcome = pipeline::run(&request, &observer, control, None)
        .expect("a run that pauses once and resumes must still complete");
    assert!(
        matches!(outcome, FuseOutcome::Completed(_)),
        "resume must let the same run continue to completion, not abandon it"
    );
    assert_eq!(
        observer.calls.load(Ordering::SeqCst),
        1,
        "paused must fire exactly once for the one segment that paused"
    );
}

#[test]
fn resume_does_not_reset_the_unit_total() {
    let request = no_align_request("resume_unit_total", 2);
    let control = focusforge_core::job::JobControl::new();
    control.pause();
    let observer = ResumeOnPause {
        calls: AtomicUsize::new(0),
        unit_total_at_pause: Mutex::new(None),
    };
    let outcome = pipeline::run(&request, &observer, control, None)
        .expect("a run that pauses once and resumes must still complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("expected a completed run");
    };
    let unit_total_at_pause = observer
        .unit_total_at_pause
        .lock()
        .unwrap()
        .expect("paused must have recorded the unit total");
    assert_eq!(
        unit_total_at_pause, report.tiles,
        "resuming must not re-seed a fresh, different unit total"
    );
}

// ---------------------------------------------------------------------
// Cancel during alignment shows the `Cancelling` interval before it
// can land.
// ---------------------------------------------------------------------

#[test]
fn cancel_during_align_shows_the_interval() {
    let mut controller = controller_with_frames("cancel_align", 2);
    assert!(controller.start_fuse());
    // Requested essentially the instant the worker starts -- real alignment
    // (the tab's default `pyramidal-ecc`) takes measurably longer than this
    // call, so the cancel lands while the bar still reads `Aligning`.
    controller.cancel_fuse();
    assert_eq!(controller.job_view().toolbar_run_state, "cancelling");

    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(!controller.job_running());
    assert_eq!(controller.job_view().state, job::RunState::Idle);
}

// ---------------------------------------------------------------------
// A clean cancel preserves the engine's resumable state.
// ---------------------------------------------------------------------

#[test]
fn clean_cancel_preserves_resumable_state() {
    let request = no_align_request("cancel_preserve", 2);
    let scratch_root = request.scratch_root.clone();
    let staging_key = request.staging_key.clone();
    let frames = request.frames.clone();
    let project_path = request.project_path.clone();

    let control = focusforge_core::job::JobControl::new();
    // Cancelled before the driver ever runs -- deterministic, and still a
    // genuine cancel: the staging directory and the resume ledger are both
    // created during setup, before the first fuse unit is ever considered.
    control.cancel();
    struct Noop;
    impl StageObserver for Noop {}
    let outcome =
        pipeline::run(&request, &Noop, control, None).expect("a pre-cancelled run must not error");
    assert!(matches!(outcome, FuseOutcome::Cancelled { .. }));

    let staging_dir = scratch_root.join("projects").join(&staging_key);
    assert!(
        staging_dir.join("composite.tif").is_file(),
        "the staged composite must survive a cancel"
    );
    let resume_dir = scratch_root.join("resume");
    assert!(
        std::fs::read_dir(&resume_dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false),
        "the resume ledger must survive a cancel"
    );

    // A second run, `resume: true`, over the same frames, scratch root and
    // staging key, must find that state rather than starting fresh. Built
    // from a distinct label so its own (unused) throwaway scratch/frame
    // dirs never collide with -- and wipe -- the state just asserted above.
    let mut resumed = no_align_request("cancel_preserve_second", 2);
    resumed.frames = frames;
    resumed.project_path = project_path;
    resumed.scratch_root = scratch_root;
    resumed.staging_key = staging_key;
    resumed.resume = true;
    let outcome = pipeline::run(
        &resumed,
        &Noop,
        focusforge_core::job::JobControl::new(),
        None,
    )
    .expect("a resumed run must find and continue the preserved state");
    assert!(matches!(outcome, FuseOutcome::Completed(_)));
}

// ---------------------------------------------------------------------
// A failed run is reported and retryable.
// ---------------------------------------------------------------------

/// Writes a tiny 3-channel 16-bit TIFF: a diagonal gradient (real gradient
/// information for ECC to lock onto) or a perfectly flat field (zero
/// gradient everywhere, which drives the solver's Hessian singular).
fn write_synthetic_frame(path: &Path, size: u32, flat: bool) {
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: size,
            height: size,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .unwrap();
    let mut samples = vec![0u16; (size * size * 3) as usize];
    if !flat {
        for y in 0..size {
            for x in 0..size {
                let v = (((x + y) * 65535 / (2 * size.max(1))) & 0xffff) as u16;
                let base = ((y * size + x) * 3) as usize;
                samples[base] = v;
                samples[base + 1] = v;
                samples[base + 2] = v;
            }
        }
    } else {
        samples.fill(32768);
    }
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: size,
                height: size,
            },
            &samples,
        )
        .unwrap();
    writer.finish().unwrap();
}

/// A controller with one unsaved project over two synthetic frames that
/// `pyramidal-ecc` cannot align: a textured reference and a perfectly flat
/// second frame, whose zero gradient drives the solver's Hessian singular
/// (`focusforge_core::align::ecc`'s own divergence tests use the same
/// construction).
fn controller_with_unalignable_frames(name: &str) -> AppController {
    let root = common::tree(&format!("{name}_frames"));
    write_synthetic_frame(&root.join("00.tif"), 64, false);
    write_synthetic_frame(&root.join("01.tif"), 64, true);
    let mut controller = AppController::new();
    assert!(matches!(
        controller.open_paths(&[root]),
        OpenOutcome::Opened(_)
    ));
    // Pinned rather than left at the tab's default seed: the flat frame's
    // zero gradient is what drives `pyramidal-ecc`'s solver singular, and
    // that failure mode is specific to this algorithm's own math.
    controller.set_align_algorithm("pyramidal-ecc");
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

#[test]
fn failure_is_reported_and_retryable() {
    let mut controller = controller_with_unalignable_frames("failure");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    assert!(!controller.job_running());
    let message = controller
        .job_failure_message()
        .expect("a failed run must carry its own message")
        .to_string();
    assert!(
        !message.is_empty(),
        "a failed run's message must not be empty"
    );
    match controller.job_view().state {
        job::RunState::Failed(_) => {}
        other => panic!("expected a failed run, got {other:?}"),
    }

    assert!(controller.retry_fuse(), "a failed run must be retryable");
    assert!(controller.job_running());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    assert!(!controller.job_running());
    match controller.job_view().state {
        job::RunState::Failed(_) => {}
        other => {
            panic!("retrying the same unalignable frames must fail the same way, got {other:?}")
        }
    }
}

#[test]
fn the_failure_message_names_the_frame_and_the_reason() {
    let mut controller = controller_with_unalignable_frames("message");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    assert!(!controller.job_running());
    let message = controller
        .job_failure_message()
        .expect("a failed run must carry its own message")
        .to_string();

    assert!(
        message.contains("01.tif"),
        "message must name the diverged frame's file name, got: {message}"
    );
    assert!(
        !message.contains("SingularHessian"),
        "message must spell the reason in words, not the status identifier, got: {message}"
    );
    assert!(
        message.split_whitespace().count() > 2,
        "message must carry a reason in words, not just a frame name: {message}"
    );
    assert!(
        !message.contains("total"),
        "only one frame diverges here, so the message must not state a frame count: {message}"
    );
}

// ---------------------------------------------------------------------
// The step vocabulary is derivable from what the engine reports.
// ---------------------------------------------------------------------

#[test]
fn steps_map_onto_engine_stages() {
    use focusforge_core::job::{JobProgress, JobStage};
    use focusforge_gui::job::{RunState, Step, current_step};

    let aligning = JobProgress {
        stage: JobStage::Align,
        units_done: 0,
        unit_total: 0,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
    };
    assert_eq!(
        current_step(&RunState::Running, Some(&aligning)),
        Some(Step::Align),
        "the engine's own Align stage must map onto the Align step"
    );

    let fusing = JobProgress {
        stage: JobStage::Fuse,
        units_done: 2,
        unit_total: 10,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
    };
    assert_eq!(
        current_step(&RunState::Running, Some(&fusing)),
        Some(Step::Fuse)
    );

    let sealing = JobProgress {
        stage: JobStage::Fuse,
        units_done: 10,
        unit_total: 10,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
    };
    assert_eq!(
        current_step(&RunState::Running, Some(&sealing)),
        Some(Step::Write),
        "a completed fuse stage with no engine stage of its own for the seal reads as Write"
    );

    // No step is fabricated for a state the engine never reported a
    // progress snapshot for.
    assert_eq!(current_step(&RunState::Running, None), Some(Step::Fuse));
    assert_eq!(current_step(&RunState::Idle, None), None);
    assert_eq!(current_step(&RunState::Paused, None), None);
}

// ---------------------------------------------------------------------
// A live run disables the panel as a whole and arms only the
// transport; with no run, `Fuse stack` is refused while a field is in
// violation.
// ---------------------------------------------------------------------

#[test]
fn running_disables_the_panel_and_arms_only_the_transport() {
    let mut controller = controller_with_frames("panel_disable", 3);
    assert!(!controller.job_running());
    assert!(controller.start_fuse());
    assert!(
        controller.job_running(),
        "the panel's own whole-panel disable reads this flag"
    );
    assert_eq!(controller.job_view().toolbar_run_state, "running");
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(!controller.job_running());
}

#[test]
fn fuse_stack_is_refused_while_a_field_is_in_violation() {
    let mut controller = controller_with_frames("refused_in_violation", 2);
    controller.set_align_algorithm("pyramidal-ecc");
    let ecc = registry::find_align("pyramidal-ecc").unwrap();
    let max_iterations_id = ecc
        .params()
        .iter()
        .find(|d| d.id == "max-iterations")
        .expect("max-iterations must exist on pyramidal-ecc")
        .id
        .clone();
    controller.set_param_text("align", &max_iterations_id, "not-a-number");

    assert!(
        !controller.start_fuse(),
        "a field in violation must refuse the run"
    );
    assert!(!controller.job_running());
}

// ---------------------------------------------------------------------
// Re-fusing an already-opened tab runs the pipeline again and appends a
// checkpoint, instead of the press doing nothing.
// ---------------------------------------------------------------------

fn opened_project_path(root_name: &str, controller: &AppController) -> PathBuf {
    let title = controller.project_tabs()[0].title.clone();
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(root_name)
        .join(format!("{title}.ffp"))
}

#[test]
fn re_fusing_an_opened_tab_appends_a_checkpoint() {
    let mut controller = controller_with_frames("reopen_appends", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(
        !controller.project_tabs()[0].unsaved,
        "the first run promotes Unsaved to Opened"
    );

    assert!(
        controller.start_fuse(),
        "pressing Fuse stack on an Opened tab must run the pipeline again"
    );
    assert!(controller.job_running());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(!controller.job_running());
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected a completed re-fuse, got {other:?}"),
    }
    assert_eq!(controller.checkpoint_count(), Some(2));
}

// ---------------------------------------------------------------------
// The checkpoint count is real: zero before the first run, incrementing
// per successful run, and read back from the container on reopen.
// ---------------------------------------------------------------------

#[test]
fn checkpoint_count_increments_per_run() {
    let mut controller = controller_with_frames("count_increments", 2);
    assert_eq!(controller.checkpoint_count(), Some(0));

    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(2));
}

#[test]
fn a_reopened_project_reports_the_count_on_disk() {
    let mut controller = controller_with_frames("reopen_count", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(2));

    let project_path = opened_project_path("reopen_count_frames", &controller);
    let mut reopened = AppController::new();
    assert!(matches!(
        reopened.open_paths(&[project_path]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(
        reopened.checkpoint_count(),
        Some(2),
        "the count must come from the container, not the session that wrote it"
    );
}

// ---------------------------------------------------------------------
// The fuse control is enabled at two or more frames, on an unfused and a
// fused tab alike, and disabled below that; unreachable with "+" selected
// or no tab.
// ---------------------------------------------------------------------

#[test]
fn fuse_is_enabled_from_two_frames_and_disabled_below() {
    let one = controller_with_frames("enabled_one", 1);
    assert!(!one.fuse_enabled(), "one frame is not a stack");

    let mut two = controller_with_frames("enabled_two", 2);
    assert!(two.fuse_enabled());

    assert!(two.start_fuse());
    common::wait_for_settle(&mut two, Duration::from_secs(120));
    assert!(
        two.fuse_enabled(),
        "a fused tab with its original frame count stays enabled"
    );
}

#[test]
fn the_plus_tab_offers_no_fuse_control() {
    let controller = AppController::new();
    assert!(controller.plus_selected());
    assert!(!controller.fuse_enabled(), "no tab means nothing to fuse");

    let mut with_frames = controller_with_frames("plus_tab", 2);
    assert!(with_frames.fuse_enabled());
    with_frames.select_plus();
    assert!(with_frames.plus_selected());
    assert!(
        !with_frames.fuse_enabled(),
        "the \"+\" tab offers no fuse control even with a project open in the background"
    );
}

// ---------------------------------------------------------------------
// A re-fuse refuses before a run starts when a recorded frame is no
// longer a file, naming it; an absolute but present source still re-fuses.
// ---------------------------------------------------------------------

#[test]
fn re_fuse_is_refused_when_a_frame_is_missing() {
    let mut controller = controller_with_frames("missing_frame", 2);
    assert!(controller.start_fuse());
    let planes = common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(planes.is_some());
    assert!(!controller.project_tabs()[0].unsaved);

    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("missing_frame_frames");
    std::fs::remove_file(root.join("00.tif")).unwrap();

    assert!(
        !controller.start_fuse(),
        "a re-fuse with a missing frame must be refused before a run starts"
    );
    assert!(!controller.job_running());
    let message = controller
        .last_refusal()
        .expect("a refusal message must be recorded");
    assert!(
        message.contains("00.tif"),
        "the refusal must name the missing frame: {message}"
    );
}

/// A sealed project whose sources are recorded outside its own directory --
/// `AsGiven`, present on disk -- built directly through `project::seal`
/// rather than through a run: only an externally-produced project (a
/// `-o <dir>` CLI run) ever holds one, since the GUI's own runs always seal
/// beside the frames they fuse.
fn sealed_project_with_as_given_sources(name: &str) -> PathBuf {
    let project_dir = common::tree(&format!("{name}_project"));
    let frames_dir = common::tree(&format!("{name}_frames"));
    for i in 0..2 {
        copy_frame(&frames_dir, &format!("{i:02}.tif"), i);
    }
    let sources: Vec<PathBuf> = (0..2)
        .map(|i| frames_dir.join(format!("{i:02}.tif")))
        .collect();

    let staging = common::tree(&format!("{name}_staging"));
    let composite = staging.join("composite.tif");
    std::fs::copy(common::fixture_frame("mantis-512", 0), &composite).unwrap();
    let zmap = staging.join("zmap.tif");
    let mut writer = ZMapWriter::create(&zmap, ZMapInfo::new(4, 4, 2).unwrap()).unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &[0u16; 16],
        )
        .unwrap();
    writer.finish().unwrap();

    let project_path = project_dir.join("demo.ffp");
    project::seal(project::SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: project::Metrics::default(),
    })
    .expect("seal must succeed");
    project_path
}

#[test]
fn an_absolute_but_present_source_still_re_fuses() {
    let project_path = sealed_project_with_as_given_sources("as_given");
    for source in &Project::open(&project_path).unwrap().manifest().sources {
        assert_eq!(source.resolution, project::SourceResolution::AsGiven);
    }

    let mut controller = AppController::new();
    assert!(matches!(
        controller.open_paths(&[project_path]),
        OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(common::tree("as_given_scratch"));

    assert!(
        controller.start_fuse(),
        "a recorded absolute source that is present must not refuse the re-fuse"
    );
    assert!(controller.last_refusal().is_none());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected a completed re-fuse, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// A re-fuse reports as a run in its own right: the idle line afterwards
// reflects the head checkpoint's own time, not a rendered clock reading.
// ---------------------------------------------------------------------

#[test]
fn a_second_run_reports_its_own_completion_time() {
    let mut controller = controller_with_frames("second_run_time", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    let job::RunState::Fused { at } = controller.job_view().state else {
        panic!("expected a completed fuse");
    };

    let project_path = opened_project_path("second_run_time_frames", &controller);
    let project = Project::open(&project_path).unwrap();
    let expected = job::format_utc_hhmm(project.checkpoint().created_at);
    assert_eq!(
        at, expected,
        "the idle line must reflect the head checkpoint's own time"
    );
}

// ---------------------------------------------------------------------
// Every project carries its own run: the bottom bar, the transport, the
// whole-panel disable and the overlay's cards all read the tab that owns
// them, never the last run's.
// ---------------------------------------------------------------------

#[test]
fn switching_to_an_idle_tab_clears_the_running_tabs_words() {
    let mut controller = controller_with_two_tabs("idle_clears");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));
    assert!(controller.job_view().fraction > 0.0);

    controller.select_project(1);
    let view = controller.job_view();
    assert_eq!(view.stage_word, "");
    assert_eq!(view.step_text, "");
    assert_eq!(view.eta_text, "");
    assert_eq!(view.fraction, 0.0);

    controller.select_project(0);
    controller.cancel_fuse();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
}

#[test]
fn returning_to_the_running_tab_restores_its_live_progress() {
    let mut controller = controller_with_two_tabs("returns_restores");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));
    let held_fraction = controller.job_view().fraction;
    assert!(held_fraction > 0.0);

    controller.select_project(1);
    assert_eq!(controller.job_view().fraction, 0.0);

    controller.select_project(0);
    let view = controller.job_view();
    assert_eq!(view.stage_word, "Paused");
    assert_eq!(
        view.fraction, held_fraction,
        "the same non-zero fraction the bar read before the switch"
    );
    assert_eq!(view.eta_text, "ETA —");

    controller.cancel_fuse();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
}

#[test]
fn a_fused_tab_keeps_its_own_completion_after_a_later_run() {
    let mut controller = controller_with_two_tabs("fused_keeps");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    let job::RunState::Fused { at } = controller.job_view().state else {
        panic!("expected tab 0 to complete");
    };

    controller.select_project(1);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected tab 1 to complete too, got {other:?}"),
    }

    controller.select_project(0);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Fused { at },
        "tab 0's own completion must survive tab 1's later run"
    );
}

/// A controller tab that cannot align: a textured reference frame and
/// `frame_count - 1` perfectly flat ones, whose zero gradient drives
/// `pyramidal-ecc`'s solver singular. Opens as a new tab, which becomes
/// active.
fn add_unalignable_tab(controller: &mut AppController, name: &str, frame_count: usize) {
    let root = common::tree(&format!("{name}_frames"));
    write_synthetic_frame(&root.join("00.tif"), 64, false);
    for i in 1..frame_count {
        write_synthetic_frame(&root.join(format!("{i:02}.tif")), 64, true);
    }
    assert!(matches!(
        controller.open_paths(&[root]),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("pyramidal-ecc");
}

#[test]
fn two_failures_keep_their_own_messages_and_requests() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("two_failures_scratch"));

    add_unalignable_tab(&mut controller, "two_failures_a", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    let message_a = controller
        .job_failure_message()
        .expect("tab A must carry its own failure message")
        .to_string();

    add_unalignable_tab(&mut controller, "two_failures_b", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    let message_b = controller
        .job_failure_message()
        .expect("tab B must carry its own failure message")
        .to_string();

    controller.select_project(0);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Failed(message_a.clone())
    );
    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Failed(message_b)
    );

    assert!(
        controller.retry_fuse(),
        "retry on the active tab must start a run"
    );
    assert!(
        controller.job_running(),
        "the retried tab must read its own run as live"
    );
    assert!(
        !controller.project_cards()[0].fusing,
        "the other tab's card must stay idle while this one retries"
    );

    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    assert!(!controller.job_running());

    controller.select_project(0);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Failed(message_a),
        "tab A's own failure must survive tab B's retry"
    );
}

#[test]
fn a_tab_switch_updates_the_bar_without_a_job_event() {
    use slint::ComponentHandle;

    i_slint_backend_testing::init_no_event_loop();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let mut controller = controller_with_two_tabs("bar_without_event");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));
    let controller = std::sync::Arc::new(std::sync::Mutex::new(controller));

    focusforge_gui::bridge::push_projects(&app, &controller);
    assert_eq!(app.get_stage_word(), "Paused");

    // Only the tab switch, no `JobChanged`/poll event of its own.
    controller.lock().unwrap().select_project(1);
    focusforge_gui::bridge::push_projects(&app, &controller);

    assert_eq!(app.get_stage_word(), "");
    assert_eq!(app.get_step_text(), "");
    assert_eq!(app.get_eta_text(), "");

    let mut guard = controller.lock().unwrap();
    guard.select_project(0);
    guard.cancel_fuse();
    common::wait_for_settle(&mut guard, Duration::from_secs(120));
}

#[test]
fn an_idle_tab_stays_editable_while_another_tab_fuses() {
    let mut controller = controller_with_two_tabs("idle_editable");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(
        !controller.job_running(),
        "the idle tab must not read another tab's run as its own"
    );
    controller.toggle_frame_excluded(0);
    assert!(
        controller.images_panel().rows[0].excluded,
        "the idle tab's own images panel must stay editable while another tab fuses"
    );

    controller.select_project(0);
    controller.cancel_fuse();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
}

// ---------------------------------------------------------------------
// A second `Fuse stack` press while a run is live queues the tab instead
// of refusing the press, the queue runs in press order, a queued tab's
// position renumbers when the queue changes, and a queued tab holds no
// worker of its own.
// ---------------------------------------------------------------------

#[test]
fn a_second_fuse_queues_and_then_runs() {
    let mut controller = controller_with_two_tabs("second_fuse_queues");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(
        controller.start_fuse(),
        "a press on the second tab while the first runs must enqueue it"
    );
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 },
        "the queued tab reads its own position"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(
        !controller.project_tabs()[0].unsaved,
        "the first tab seals a checkpoint"
    );

    // The queue promotes the second tab once the first settles.
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.select_project(1);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the queued tab to complete, got {other:?}"),
    }
    assert!(!controller.project_tabs()[1].unsaved);
}

#[test]
fn the_queue_runs_in_press_order() {
    let mut controller = controller_with_three_tabs("queue_runs_in_press_order");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    // Press on tab 2 before tab 1 -- the strip's own order is 0, 1, 2, so
    // the queue must run 2 before 1 to prove it is press order, not tab
    // order.
    controller.select_project(2);
    assert!(controller.start_fuse());
    controller.select_project(1);
    assert!(controller.start_fuse());
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 3 },
        "the later press sits behind the earlier one"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    // The queue's head (tab 2, pressed first) runs next.
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.select_project(2);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the first-pressed tab to run next, got {other:?}"),
    }

    // Tab 1 (pressed second) runs last.
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.select_project(1);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the second-pressed tab to run last, got {other:?}"),
    }
}

#[test]
fn cancelling_a_queued_tab_spares_the_running_one_and_renumbers() {
    let mut controller = controller_with_three_tabs("cancel_queued_spares_and_renumbers");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(controller.start_fuse());
    controller.select_project(2);
    assert!(controller.start_fuse());

    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 }
    );
    controller.cancel_fuse();
    assert_eq!(
        controller.job_view().state,
        job::RunState::Idle,
        "cancelling a queued tab returns it to Idle"
    );

    controller.select_project(0);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Paused,
        "the running tab is untouched by a queued cancel"
    );

    controller.select_project(2);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 },
        "the tab behind the cancelled one renumbers into its place"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Idle,
        "the cancelled tab never ran"
    );
    assert!(
        controller.project_tabs()[1].unsaved,
        "the cancelled tab never sealed a checkpoint"
    );
    controller.select_project(2);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the promoted tab to complete, got {other:?}"),
    }
}

#[test]
fn cancelling_the_running_tab_promotes_the_queue() {
    let mut controller = controller_with_two_tabs("cancel_running_promotes_the_queue");
    assert!(controller.start_fuse());

    controller.select_project(1);
    assert!(controller.start_fuse());
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 }
    );

    controller.select_project(0);
    // Cancelled essentially the instant the worker starts -- real alignment
    // takes measurably longer than this call, so the cancel reliably lands
    // before the tiny fixture could complete on its own.
    controller.cancel_fuse();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    assert_eq!(
        controller.job_view().state,
        job::RunState::Idle,
        "the cancelled tab is not re-queued"
    );

    controller.select_project(1);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the promoted tab to complete after the cancel, got {other:?}"),
    }
}

#[test]
fn closing_a_queued_tab_leaves_the_queue_and_the_rest_still_run() {
    let mut controller = controller_with_three_tabs("closing_queued_leaves_the_rest");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(controller.start_fuse());
    controller.select_project(2);
    assert!(controller.start_fuse());
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 3 }
    );

    // Tab 1 is next in line; closing it must not disturb the running tab
    // or the tab still behind it.
    controller.close_project(1);
    assert_eq!(controller.project_tabs().len(), 2);

    controller.select_project(0);
    assert_eq!(controller.job_view().state, job::RunState::Paused);

    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 },
        "the tab behind the closed one renumbers into its place"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the remaining queued tab to complete, got {other:?}"),
    }
}

#[test]
fn closing_a_tab_before_the_running_one_does_not_misidentify_the_run() {
    let mut controller = controller_with_three_tabs("close_before_running");
    controller.select_project(1);
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(2);
    assert!(controller.start_fuse());
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 }
    );

    // Closing an earlier tab shifts every later index down by one -- the
    // running tab was index 1, the queued tab index 2.
    controller.close_project(0);
    assert_eq!(controller.project_tabs().len(), 2);

    controller.select_project(0);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Paused,
        "the run must still be attached to its own tab after an earlier tab closes"
    );
    assert!(controller.any_job_running());

    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 },
        "the queued tab must still resolve after an earlier tab closes"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the promoted tab to complete, got {other:?}"),
    }
}

#[test]
fn a_run_keeps_its_tab_when_an_earlier_tab_closes() {
    let mut controller = controller_with_two_tabs("run_keeps_its_tab");
    let root_c = common::tree("run_keeps_its_tab_c_frames");
    for i in 0..4 {
        copy_frame(&root_c, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        controller.open_paths(&[root_c]),
        OpenOutcome::Opened(_)
    ));

    // Tab 2 (four frames) runs; tabs 0 and 1 stay idle.
    controller.select_project(2);
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    // Closing an earlier tab shifts every later index down by one -- the
    // running tab was index 2, becomes index 1.
    controller.close_project(0);
    assert_eq!(controller.project_tabs().len(), 2);

    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Paused,
        "the run must still be attached to its own tab after an earlier tab closes"
    );

    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    assert_eq!(
        controller.checkpoint_count(),
        Some(1),
        "the completed run's checkpoint lands on its own tab"
    );
    controller.select_project(0);
    assert_eq!(
        controller.checkpoint_count(),
        Some(0),
        "the run's outcome must not land on the surviving idle tab"
    );
}

#[test]
fn a_queued_run_reads_the_settings_current_when_it_starts() {
    let mut controller = controller_with_two_tabs("queued_reads_current_settings");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(controller.start_fuse());
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 }
    );

    // A queued tab stays editable -- this value differs from the panel's
    // own default, so a request built at enqueue time would fail the
    // assertion below.
    controller.set_param_value("fuse", "score-radius", ParamValue::Int(7));

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    let title = controller.project_tabs()[1].title.clone();
    let project_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("queued_reads_current_settings_b_frames")
        .join(format!("{title}.ffp"));
    let manifest = Project::open(&project_path).unwrap().manifest().clone();
    assert_eq!(
        manifest.params.get("score-radius"),
        Some(&ParamValue::Int(7)),
        "the queued run must use the settings current when it started, not when it was queued"
    );
}

#[test]
fn an_invalid_tab_is_refused_rather_than_queued() {
    // Case 1: a field in violation.
    let mut controller = controller_with_two_tabs("invalid_field_not_queued");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    controller.set_align_algorithm("pyramidal-ecc");
    let ecc = registry::find_align("pyramidal-ecc").unwrap();
    let max_iterations_id = ecc
        .params()
        .iter()
        .find(|d| d.id == "max-iterations")
        .expect("max-iterations must exist on pyramidal-ecc")
        .id
        .clone();
    controller.set_param_text("align", &max_iterations_id, "not-a-number");

    assert!(
        !controller.start_fuse(),
        "a field in violation must be refused, not queued, even while another run is live"
    );
    assert_eq!(
        controller.job_view().state,
        job::RunState::Idle,
        "the invalid tab does not enter the queue"
    );

    controller.select_project(0);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Paused,
        "the running tab is untouched by a refused enqueue"
    );
    controller.cancel_fuse();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    // Case 2: a recorded frame no longer on disk.
    let mut controller = controller_with_frames("invalid_frame_not_queued", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(!controller.project_tabs()[0].unsaved);

    let root_b = common::tree("invalid_frame_not_queued_b_frames");
    for i in 0..2 {
        copy_frame(&root_b, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        controller.open_paths(&[root_b]),
        OpenOutcome::Opened(_)
    ));
    controller.select_project(1);
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(0);
    let frames_root =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("invalid_frame_not_queued_frames");
    std::fs::remove_file(frames_root.join("00.tif")).unwrap();

    let before = controller.job_view().state;
    assert!(
        !controller.start_fuse(),
        "a re-fuse with a missing frame must be refused before it can be queued"
    );
    let message = controller
        .last_refusal()
        .expect("a refusal message must be recorded")
        .to_string();
    assert!(message.contains("00.tif"));
    assert_eq!(
        controller.job_view().state,
        before,
        "the invalid tab's run state is untouched by a refused enqueue -- not Queued, not reset"
    );

    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Paused,
        "the running tab is untouched by a refused enqueue"
    );
    controller.cancel_fuse();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
}

#[test]
fn a_queued_tab_that_can_no_longer_run_is_refused_and_the_next_one_starts() {
    let mut controller = controller_with_three_tabs("queued_tab_dropped_at_promotion");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(controller.start_fuse());
    controller.select_project(2);
    assert!(controller.start_fuse());

    // Tab 1's frames go away while it waits, so the promotion that reaches
    // it cannot build a run for it.
    let frames_b =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("queued_tab_dropped_at_promotion_b_frames");
    std::fs::remove_file(frames_b.join("00.tif")).unwrap();

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Idle,
        "a queued tab dropped at promotion returns to Idle rather than waiting forever"
    );
    let message = controller
        .last_refusal()
        .expect("the dropped tab must say why it left the queue")
        .to_string();
    assert!(message.contains("00.tif"), "got {message}");

    controller.select_project(2);
    assert!(
        matches!(
            controller.job_view().state,
            job::RunState::Running | job::RunState::Fused { .. }
        ),
        "the tab behind the dropped one is promoted in its place"
    );
    assert!(
        controller.work_pending(),
        "the promoted tab still owes the clock a poll"
    );

    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(
        !controller.work_pending(),
        "the clock stops once the queue has drained"
    );
    controller.select_project(1);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Idle,
        "the dropped tab is still idle after the queue drains, so it can be fused again"
    );
}

#[test]
fn the_clock_keeps_running_while_work_is_pending() {
    let mut controller = controller_with_two_tabs("clock_keeps_running");
    assert!(!controller.work_pending());

    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));
    assert!(controller.work_pending());

    controller.select_project(1);
    assert!(controller.start_fuse());
    assert!(
        controller.work_pending(),
        "a queued tab has no worker but still owes the clock a poll"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(
        controller.work_pending(),
        "the promoted tab is now the live worker"
    );

    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(
        !controller.work_pending(),
        "the clock may stop once the queue is empty and no run is live"
    );
}

#[test]
fn the_strip_marks_the_fusing_tab_and_the_queued_ones() {
    let mut controller = controller_with_three_tabs("strip_marks_fusing_and_queued");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(controller.start_fuse());

    // The third tab is selected but takes no part in the run -- its mark
    // must stay clear, and the marks on the other two must be readable
    // without switching to them.
    controller.select_project(2);

    let tabs = controller.project_tabs();
    assert_eq!(
        tabs[0].run_mark_icon, "play",
        "the running tab reads its own mark from the strip alone"
    );
    assert_eq!(
        tabs[1].run_mark_icon, "ellipsis",
        "the queued tab reads its own mark from the strip alone"
    );
    assert_eq!(
        tabs[2].run_mark_icon, "",
        "the selected but idle tab carries no run mark"
    );
    assert_eq!(
        controller.job_view().toolbar_run_state,
        "idle",
        "the bottom bar reports only the active tab, not the run happening elsewhere"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    let tabs = controller.project_tabs();
    assert_eq!(tabs[0].run_mark_icon, "", "the completed tab's mark clears");
    assert_eq!(
        tabs[1].run_mark_icon, "",
        "the completed, promoted tab's mark clears once it settles"
    );
}

#[test]
fn retry_queues_while_another_run_is_live() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("retry_queues_scratch"));
    add_unalignable_tab(&mut controller, "retry_queues_a", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    match controller.job_view().state {
        job::RunState::Failed(_) => {}
        other => panic!("expected the first tab to fail, got {other:?}"),
    }

    let survivor_root = common::tree("retry_queues_b_frames");
    for i in 0..3 {
        copy_frame(&survivor_root, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        controller.open_paths(&[survivor_root]),
        OpenOutcome::Opened(_)
    ));

    controller.select_project(1);
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(0);
    assert!(
        controller.retry_fuse(),
        "retry on a failed tab while another run is live must enqueue rather than refuse"
    );
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 }
    );

    controller.select_project(1);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    common::wait_for_settle(&mut controller, Duration::from_secs(60));

    controller.select_project(0);
    match controller.job_view().state {
        job::RunState::Failed(_) => {}
        other => {
            panic!("expected the retried tab to fail the same way once promoted, got {other:?}")
        }
    }
}

#[test]
fn a_second_start_queues_while_a_run_is_live() {
    let mut controller = controller_with_two_tabs("second_start_queues");
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(
        controller.start_fuse(),
        "starting a second fuse while one is live must enqueue it, not refuse it"
    );
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 },
        "the second tab reads its own queued position"
    );

    controller.select_project(0);
    assert_eq!(
        controller.job_view().state,
        job::RunState::Paused,
        "the first run must be unaffected by the enqueued second start"
    );

    controller.cancel_fuse();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
}

#[test]
fn closing_the_owning_tab_mid_run_promotes_nothing() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("closing_mid_run_scratch"));
    add_unalignable_tab(&mut controller, "closing_mid_run_a", 2);
    // The survivor: a different frame count than tab A's, so a leaked value
    // could not pass by coincidence.
    let survivor_root = common::tree("closing_mid_run_b_frames");
    for i in 0..3 {
        copy_frame(&survivor_root, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        controller.open_paths(&[survivor_root]),
        OpenOutcome::Opened(_)
    ));

    controller.select_project(0);
    assert!(controller.start_fuse());
    assert!(controller.any_job_running());

    controller.close_project(0);
    assert_eq!(controller.project_tabs().len(), 1);
    assert!(
        controller.any_job_running(),
        "the orphaned run keeps running until it settles"
    );

    assert!(
        controller.start_fuse(),
        "a press on the survivor while the orphaned run is still live enqueues it, rather than refusing"
    );
    assert_eq!(
        controller.job_view().state,
        job::RunState::Queued { position: 2 },
        "the survivor waits behind the orphaned run rather than starting immediately"
    );

    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(!controller.any_job_running());

    assert_eq!(
        controller.project_cards().len(),
        1,
        "the orphaned run's failure card must not land on the survivor"
    );
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the promoted queued run to complete, got {other:?}"),
    }
    assert_eq!(controller.checkpoint_count(), Some(1));
}
