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
use focusforge_core::pipeline::{self, FuseOutcome, FuseOutput, FuseRequest, StageObserver};
use focusforge_core::project::{self, Project};
use focusforge_core::registry;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use focusforge_gui::canvas::CanvasFeed;
use focusforge_gui::input_map::Modifiers;
use focusforge_gui::job;
use focusforge_gui::viewport::PaneRect;
use focusforge_gui::{AppController, ChromeKey, OpenOutcome, ShellEvent};

// ---------------------------------------------------------------------
// Fixture helpers -- copies only, never the committed fixture directory
// itself, since a save writes a `.ffp` beside the frames it fused.
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
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    // These tabs exercise queueing, saving and closing, not alignment
    // quality; an algorithm that always converges keeps every run clean.
    controller.set_align_algorithm("dummy-align");
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
        common::open_and_settle(&mut controller, &[root_b]),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("dummy-align");
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
        common::open_and_settle(&mut controller, &[root_c]),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("dummy-align");
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
// A fuse run completes off the UI thread, holds its checkpoint, feeds
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
        controller.pending_divergence().is_none(),
        "a run with no diverged frame asks nothing"
    );
    assert!(
        controller.project_tabs()[0].unsaved,
        "a fused tab stays unsaved until it is saved"
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
        output: FuseOutput::Seal(root.join("pinned.ffp")),
        scratch_root: scratch.clone(),
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
    // standalone file -- materialize it through the same path `--open
    // <saved>.ffp` uses.
    let pipeline::FuseProduct::Sealed(sealed) = &report.product else {
        panic!("a seal request must report a sealed project");
    };
    let mut project = Project::open(&sealed.path).unwrap();
    let (composite_path, _zmap_path) = job::materialize_planes(&mut project, &scratch)
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
        output: FuseOutput::Seal(root.join("project.ffp")),
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
// Pause holds a still-aligning run: no progress moves while it is held,
// and a second press lets the same run finish.
// ---------------------------------------------------------------------

#[test]
fn pause_during_alignment_holds_align_progress() {
    let mut controller = controller_with_frames("pause_align", 8);
    // `dummy-align` (`controller_with_frames`'s own default) finishes in
    // microseconds; a real solver is what gives the hold something to
    // catch mid-stage. Translation-only chain registration converges on
    // every one of this fixture's eight frames, so the run fuses cleanly
    // rather than parking on a diverged frame.
    controller.set_align_algorithm("pyramidal-ecc");
    controller.set_param_value("align", "mode", ParamValue::Enum("translation".to_string()));
    controller.set_param_value(
        "align",
        "reference-policy",
        ParamValue::Enum("chain".to_string()),
    );
    assert!(controller.start_fuse());

    let start = Instant::now();
    while !(controller.job_view().state == job::RunState::Running
        && controller.job_view().stage_word == "Aligning")
    {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the run never reached the align stage within 60s"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    controller.pause_or_resume();
    assert_eq!(controller.job_view().state, job::RunState::Paused);

    let held = controller.job_view();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        controller.job_view(),
        held,
        "the align stage's own progress must not move while the hold is in effect"
    );

    controller.pause_or_resume();
    // A paused view shows `Paused` in place of the stage word; the first
    // view after resume reads the progress the hold froze, which must still
    // be the align stage's.
    assert_eq!(
        controller.job_view().stage_word,
        "Aligning",
        "the hold must have landed in the align stage"
    );
    common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("a resumed run must hand back its fused planes");
    assert!(!controller.job_running());
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("a resumed run must settle as fused, got {other:?}"),
    }
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
    let FuseOutput::Seal(project_path) = &request.output else {
        panic!("the request seals");
    };
    let project_path = project_path.clone();

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
    resumed.output = FuseOutput::Seal(project_path);
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

/// Opens a new tab over `frame_count` textured synthetic frames, then
/// overwrites the last one with bytes the TIFF reader refuses, so a fuse of
/// it fails inside the pipeline. The tab becomes active.
fn add_unreadable_tab(controller: &mut AppController, name: &str, frame_count: usize) {
    let root = common::tree(&format!("{name}_frames"));
    for i in 0..frame_count {
        write_synthetic_frame(&root.join(format!("{i:02}.tif")), 64, false);
    }
    assert!(matches!(
        common::open_and_settle(controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("dummy-align");
    std::fs::write(
        root.join(format!("{:02}.tif", frame_count - 1)),
        b"not a tiff",
    )
    .unwrap();
}

/// A controller with one tab over two frames, the second unreadable.
fn controller_with_an_unreadable_frame(name: &str) -> AppController {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    add_unreadable_tab(&mut controller, name, 2);
    controller
}

/// A controller with one unsaved project over two synthetic frames that
/// `pyramidal-ecc` cannot align: a textured reference and a perfectly flat
/// second frame, whose zero gradient drives the solver's Hessian singular
/// (`focusforge_core::align::ecc`'s own divergence tests use the same
/// construction). Its fuse completes lax and parks, naming `01.tif`. The
/// scratch root is `<name>_scratch` under `CARGO_TARGET_TMPDIR`.
fn controller_with_unalignable_frames(name: &str) -> AppController {
    let root = common::tree(&format!("{name}_frames"));
    write_synthetic_frame(&root.join("00.tif"), 64, false);
    write_synthetic_frame(&root.join("01.tif"), 64, true);
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    // The flat frame's zero gradient drives this algorithm's own solver
    // singular.
    controller.set_align_algorithm("pyramidal-ecc");
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

/// Every file under `scratch` that sits in a `held` directory.
fn held_files(scratch: &Path) -> Vec<PathBuf> {
    listing(scratch)
        .into_iter()
        .filter(|p| p.components().any(|c| c.as_os_str() == "held"))
        .filter(|p| scratch.join(p).is_file())
        .collect()
}

/// [`controller_with_unalignable_frames`], fused until its run parks.
/// Returns the controller and its scratch root.
fn parked_controller(name: &str) -> (AppController, PathBuf) {
    let mut controller = controller_with_unalignable_frames(name);
    assert!(controller.start_fuse());
    assert!(
        common::wait_for_settle(&mut controller, Duration::from_secs(60)).is_none(),
        "a parked run feeds the canvas nothing"
    );
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}_scratch"));
    (controller, scratch)
}

#[test]
fn a_diverging_run_parks_naming_the_frame() {
    let (controller, scratch) = parked_controller("parks");
    assert_eq!(controller.job_view().state, job::RunState::Idle);
    assert_eq!(controller.checkpoint_count(), Some(0));
    assert!(controller.job_failure_message().is_none());
    assert_eq!(
        controller.pending_divergence(),
        Some(&["01.tif".to_string()][..])
    );
    assert!(
        !held_files(&scratch).is_empty(),
        "the parked run's files stay on disk until a decision"
    );
}

#[test]
fn keep_commits_a_parked_run_like_a_clean_one() {
    let (mut controller, _) = parked_controller("keep");
    let planes = controller
        .keep_fused()
        .expect("a parked run is there to keep");
    assert_eq!(controller.checkpoint_count(), Some(1));
    assert!(matches!(
        controller.job_view().state,
        job::RunState::Fused { .. }
    ));
    assert!(controller.pending_divergence().is_none());
    let held = controller.held_checkpoints().last().unwrap();
    assert_eq!(planes, (held.composite.clone(), held.zmap.clone()));
    assert_eq!(controller.materialize_active_tab_planes(), Some(planes));
}

#[test]
fn discard_deletes_a_parked_run() {
    let (mut controller, scratch) = parked_controller("discard");
    controller.discard_fused();
    assert_eq!(controller.checkpoint_count(), Some(0));
    assert_eq!(controller.job_view().state, job::RunState::Idle);
    assert!(controller.pending_divergence().is_none());
    assert_eq!(held_files(&scratch), Vec::<PathBuf>::new());
}

#[test]
fn closing_a_tab_deletes_its_parked_run() {
    let (mut controller, scratch) = parked_controller("close_parked");
    controller.request_close(0);
    assert!(controller.pending_close().is_none());
    assert_eq!(controller.project_tabs().len(), 0);
    assert_eq!(held_files(&scratch), Vec::<PathBuf>::new());
}

#[test]
fn a_parked_run_makes_quit_ask() {
    let (mut controller, _) = parked_controller("quit_parked");
    controller.window_close();
    let copy = controller
        .pending_quit_copy()
        .expect("an undecided run is unsaved work");
    assert!(copy.body.starts_with("1 project"), "{}", copy.body);
}

#[test]
fn failure_is_reported_and_retryable() {
    let mut controller = controller_with_an_unreadable_frame("failure");
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
            panic!("retrying the same unreadable frame must fail the same way, got {other:?}")
        }
    }
}

/// The failure toast's short summary follows the error's typed variant, not
/// which `JobStage` the control happened to read: setup (a corrupted
/// frame), alignment (the solver singular) and fusion (a scratch root that
/// cannot stage) each append a notification naming the operation that
/// actually failed, with the tab's own full message unchanged inside it.
#[test]
fn failure_summary_names_the_failed_operation() {
    // A GUI fuse no longer fails on a per-frame alignment divergence (lax
    // mode parks it instead, see `controller_with_unalignable_frames`'s own
    // doc comment), so the `Align` classification is checked directly against
    // the error variant it actually comes from -- the solver's `Err` in
    // `pipeline::run` -- rather than through an end-to-end GUI run.
    let alignment_operation;
    let alignment_summary;
    {
        let operation = job::FailedOperation::from(&pipeline::PipelineError::Align(
            "synthetic alignment failure".to_string(),
        ));
        alignment_operation = operation;
        alignment_summary = operation.summary();
        assert_eq!(alignment_operation, job::FailedOperation::Alignment);
    }

    let setup_operation;
    let setup_summary;
    {
        let root = common::tree("summary_setup_frames");
        copy_frame(&root, "00.tif", 0);
        copy_frame(&root, "01.tif", 1);
        let mut controller = AppController::new();
        assert!(matches!(
            common::open_and_settle(&mut controller, std::slice::from_ref(&root)),
            OpenOutcome::Opened(_)
        ));
        controller.set_scratch_root_for_test(common::tree("summary_setup_scratch"));
        // Still a file, so the missing-frame refusal never fires -- the
        // failure has to come from actually opening it.
        std::fs::write(root.join("01.tif"), b"not a tiff any more").unwrap();
        assert!(controller.start_fuse());
        common::wait_for_settle(&mut controller, Duration::from_secs(60));
        let message = controller
            .job_failure_message()
            .expect("a corrupted frame must fail the run")
            .to_string();
        let notification = controller
            .notifications()
            .last()
            .expect("the failed run must append a notification");
        assert_eq!(notification.message, message);
        setup_operation = notification.operation;
        setup_summary = notification.operation.summary();
        assert_eq!(setup_operation, job::FailedOperation::Setup);
    }

    let fusion_operation;
    let fusion_summary;
    {
        let mut controller = controller_with_frames("summary_fusion", 2);
        let scratch_parent = common::tree("summary_fusion_scratch");
        let scratch_root = scratch_parent.join("as-file");
        std::fs::write(&scratch_root, b"not a directory").unwrap();
        controller.set_scratch_root_for_test(scratch_root);
        assert!(controller.start_fuse());
        common::wait_for_settle(&mut controller, Duration::from_secs(60));
        let message = controller
            .job_failure_message()
            .expect("a scratch root that is a file must fail staging")
            .to_string();
        let notification = controller
            .notifications()
            .last()
            .expect("the failed run must append a notification");
        assert_eq!(notification.message, message);
        fusion_operation = notification.operation;
        fusion_summary = notification.operation.summary();
        assert_eq!(fusion_operation, job::FailedOperation::Fusion);
    }

    let mut summaries = vec![alignment_summary, setup_summary, fusion_summary];
    summaries.sort_unstable();
    summaries.dedup();
    assert_eq!(
        summaries.len(),
        3,
        "the three summaries must be pairwise distinct"
    );
}

#[test]
fn the_failure_message_names_the_frame() {
    let mut controller = controller_with_an_unreadable_frame("message");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    assert!(!controller.job_running());
    let message = controller
        .job_failure_message()
        .expect("a failed run must carry its own message")
        .to_string();
    assert!(
        message.contains("01.tif"),
        "message must name the unreadable frame's file name, got: {message}"
    );
}

// ---------------------------------------------------------------------
// The phase vocabulary is derivable from what the engine reports.
// ---------------------------------------------------------------------

#[test]
fn phases_map_onto_engine_stages() {
    use focusforge_core::job::{JobProgress, JobStage};
    use focusforge_gui::job::{Phase, RunState, current_phase};

    let aligning = JobProgress {
        stage: JobStage::Align,
        units_done: 0,
        unit_total: 0,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
        align_done: 0,
        align_total: 0,
    };
    assert_eq!(
        current_phase(&RunState::Running, Some(&aligning)),
        Some(Phase::Aligning),
        "the engine's own Align stage must map onto the Aligning phase"
    );

    let fusing = JobProgress {
        stage: JobStage::Fuse,
        units_done: 2,
        unit_total: 10,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
        align_done: 0,
        align_total: 0,
    };
    assert_eq!(
        current_phase(&RunState::Running, Some(&fusing)),
        Some(Phase::Fusing)
    );

    let sealing = JobProgress {
        stage: JobStage::Fuse,
        units_done: 10,
        unit_total: 10,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
        align_done: 0,
        align_total: 0,
    };
    assert_eq!(
        current_phase(&RunState::Running, Some(&sealing)),
        Some(Phase::Writing),
        "a completed fuse stage with no engine stage of its own for the seal reads as Writing"
    );

    // No phase is fabricated for a state the engine never reported a
    // progress snapshot for.
    assert_eq!(current_phase(&RunState::Running, None), Some(Phase::Fusing));
    assert_eq!(current_phase(&RunState::Idle, None), None);
    assert_eq!(current_phase(&RunState::Paused, None), None);
}

// ---------------------------------------------------------------------
// The align stage's step text and bar fraction carry the live frame count.
// ---------------------------------------------------------------------

fn align_progress(align_done: usize, align_total: usize) -> focusforge_core::job::JobProgress {
    use focusforge_core::job::JobStage;
    focusforge_core::job::JobProgress {
        stage: JobStage::Align,
        units_done: 0,
        unit_total: 0,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
        align_done,
        align_total,
    }
}

fn fuse_progress(units_done: usize, unit_total: usize) -> focusforge_core::job::JobProgress {
    use focusforge_core::job::JobStage;
    focusforge_core::job::JobProgress {
        stage: JobStage::Fuse,
        units_done,
        unit_total,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
        align_done: 0,
        align_total: 0,
    }
}

#[test]
fn align_progress_text_counts_frames() {
    use focusforge_gui::job::{RunState, progress_text};

    let three_of_eight = align_progress(3, 8);
    assert_eq!(
        progress_text(&RunState::Running, Some(&three_of_eight)),
        "3 of 8"
    );

    let forty_two_of_sixty_one = align_progress(42, 61);
    assert_eq!(
        progress_text(&RunState::Running, Some(&forty_two_of_sixty_one)),
        "42 of 61"
    );
}

#[test]
fn fuse_progress_text_is_the_name_only() {
    use focusforge_gui::job::{RunState, progress_text};

    for (done, total) in [(0, 0), (4, 10), (10, 10)] {
        assert_eq!(
            progress_text(&RunState::Running, Some(&fuse_progress(done, total))),
            "",
            "a fuse snapshot at {done}/{total} must render no count"
        );
    }
}

#[test]
fn write_progress_text_is_the_name_only() {
    use focusforge_gui::job::{RunState, progress_text};

    let writing = fuse_progress(10, 10);
    assert_eq!(progress_text(&RunState::Running, Some(&writing)), "");
}

#[test]
fn align_bar_fraction_follows_the_frame_count() {
    use focusforge_gui::job::bar_fraction;

    assert_eq!(bar_fraction(Some(&align_progress(0, 8))), 0.0);
    assert_eq!(bar_fraction(Some(&align_progress(1, 8))), 1.0 / 8.0);
    assert_eq!(bar_fraction(Some(&align_progress(7, 8))), 7.0 / 8.0);

    // The first fuse-stage snapshot follows the fuse units again.
    let fusing = fuse_progress(2, 10);
    assert_eq!(bar_fraction(Some(&fusing)), 0.2);
}

#[test]
fn progress_text_without_a_total_is_the_name_only() {
    use focusforge_gui::job::{RunState, bar_fraction, progress_text};

    // A cached alignment: `stage == Align`, but `align_total == 0` (no
    // computed stage set a total) -- never `0 of <n>`, and never a rendered
    // `0 of 0`.
    let cached = align_progress(0, 0);
    assert_eq!(progress_text(&RunState::Running, Some(&cached)), "");
    assert_eq!(bar_fraction(Some(&cached)), 0.0);

    // A fuse snapshot with no total yet -- the plan's totals not credited --
    // renders no count, never `0 of 0` or `0 of N` with no basis.
    let no_total = fuse_progress(0, 0);
    assert_eq!(progress_text(&RunState::Running, Some(&no_total)), "");

    // A fuse snapshot that still carries a finished align stage's `8/8`
    // shows no count -- only Aligning counts frames.
    let mut finished_align_then_fusing = fuse_progress(0, 10);
    finished_align_then_fusing.align_done = 8;
    finished_align_then_fusing.align_total = 8;
    assert_eq!(
        progress_text(&RunState::Running, Some(&finished_align_then_fusing)),
        ""
    );
}

#[test]
fn progress_text_never_names_a_step() {
    use focusforge_gui::job::{RunState, progress_text};

    let states = [
        RunState::Idle,
        RunState::Running,
        RunState::Paused,
        RunState::Cancelling,
        RunState::Failed("stopped".to_string()),
        RunState::Fused {
            at: "12:00".to_string(),
        },
        RunState::Queued { position: 2 },
    ];
    let snapshots = [
        None,
        Some(align_progress(0, 0)),
        Some(align_progress(3, 8)),
        Some(fuse_progress(0, 0)),
        Some(fuse_progress(0, 10)),
        Some(fuse_progress(4, 10)),
        Some(fuse_progress(10, 10)),
    ];
    for state in &states {
        for snapshot in &snapshots {
            let text = progress_text(state, snapshot.as_ref());
            assert!(
                !text.contains("step"),
                "{state:?} over {snapshot:?} must never render the word 'step': {text:?}"
            );
        }
    }
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
// Re-fusing an already-saved tab runs the pipeline again and holds a new
// checkpoint, which the next save appends to the same file.
// ---------------------------------------------------------------------

/// Saves the active, never-saved tab to the path the save dialog proposes,
/// and returns that path.
fn save_to_proposed(controller: &mut AppController) -> PathBuf {
    let mut target = None;
    assert!(controller.save_active_project(|proposed| {
        target = Some(proposed.to_path_buf());
        target.clone()
    }));
    target.expect("a never-saved tab asks where to save")
}

#[test]
fn re_fusing_an_opened_tab_appends_a_checkpoint() {
    let mut controller = controller_with_frames("reopen_appends", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    let project_path = save_to_proposed(&mut controller);
    assert!(
        !controller.project_tabs()[0].unsaved,
        "a save opens the written project"
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
    assert!(
        controller.project_tabs()[0].unsaved,
        "a re-fuse of a saved tab is unsaved again"
    );

    assert!(
        controller.save_active_project(|_| panic!("a saved tab never asks where")),
        "the second save writes the same file"
    );
    let chain = Project::open(&project_path)
        .unwrap()
        .checkpoint_chain()
        .unwrap();
    assert_eq!(chain.len(), 2);
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

    let project_path = save_to_proposed(&mut controller);
    let mut reopened = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut reopened, &[project_path]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(
        reopened.checkpoint_count(),
        Some(2),
        "the count must come from the container, not the session that wrote it"
    );
}

// ---------------------------------------------------------------------
// The checkpoints panel's own line follows the active tab's real count:
// the empty wording with no tab or none fused, the shared count phrase
// once it has, updated by a tab switch and by every further fuse.
// ---------------------------------------------------------------------

#[test]
fn checkpoint_panel_line_without_a_tab() {
    let controller = AppController::new();
    assert_eq!(
        controller.checkpoint_panel_line(),
        "None yet — fuse a stack"
    );
}

#[test]
fn checkpoint_panel_line_counts_each_run() {
    let mut controller = controller_with_frames("panel_line_counts", 2);
    assert_eq!(
        controller.checkpoint_panel_line(),
        "None yet — fuse a stack"
    );

    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_panel_line(), "1 checkpoint");
    assert!(
        controller.project_cards()[0]
            .meta_text
            .ends_with(&controller.checkpoint_panel_line()),
        "the panel and the card must quote the same count phrase"
    );

    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_panel_line(), "2 checkpoints");
    assert!(
        controller.project_cards()[0]
            .meta_text
            .ends_with(&controller.checkpoint_panel_line()),
        "the panel and the card must quote the same count phrase"
    );
}

#[test]
fn checkpoint_panel_line_follows_the_active_tab() {
    let mut controller = controller_with_two_tabs("panel_line_follows");
    controller.select_project(1);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_panel_line(), "1 checkpoint");

    controller.select_project(0);
    assert_eq!(
        controller.checkpoint_panel_line(),
        "None yet — fuse a stack"
    );

    controller.select_project(1);
    assert_eq!(controller.checkpoint_panel_line(), "1 checkpoint");
}

#[test]
fn checkpoint_panel_is_pushed_with_the_projects() {
    use slint::ComponentHandle;

    i_slint_backend_testing::init_no_event_loop();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let mut controller = controller_with_two_tabs("panel_line_pushed");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    let controller = std::sync::Arc::new(std::sync::Mutex::new(controller));

    focusforge_gui::bridge::push_projects(&app, &controller);
    assert_eq!(app.get_checkpoint_line(), "1 checkpoint");

    controller.lock().unwrap().select_project(1);
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert_eq!(app.get_checkpoint_line(), "None yet — fuse a stack");
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
    assert_eq!(controller.checkpoint_count(), Some(1));

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
/// rather than through a run and a save, which proposes the frames' own
/// directory.
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
        common::open_and_settle(&mut controller, &[project_path]),
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

    let project_path = save_to_proposed(&mut controller);
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

#[test]
fn two_failures_keep_their_own_messages_and_requests() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("two_failures_scratch"));

    add_unreadable_tab(&mut controller, "two_failures_a", 2);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    let message_a = controller
        .job_failure_message()
        .expect("tab A must carry its own failure message")
        .to_string();

    add_unreadable_tab(&mut controller, "two_failures_b", 3);
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

/// Notification records outlive the single-slot toast they came from: a
/// second failure replaces the first, still-unread toast, a retry appends
/// its own record, and switching tabs afterwards drops nothing. Every
/// record keeps its own tab's title, path and full message.
#[test]
fn failure_notifications_outlive_the_toast() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("outlive_scratch"));

    add_unreadable_tab(&mut controller, "outlive_a", 2);
    let title_a = controller.project_tabs()[0].title.clone();
    let path_a = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("outlive_a_frames");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    let message_a = controller
        .job_failure_message()
        .expect("tab A must fail")
        .to_string();

    add_unreadable_tab(&mut controller, "outlive_b", 3);
    let title_b = controller.project_tabs()[1].title.clone();
    let path_b = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("outlive_b_frames");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    let message_b = controller
        .job_failure_message()
        .expect("tab B must fail")
        .to_string();

    // The second failure replaces the first, still-unread toast slot.
    let taken = controller
        .take_settled_failure()
        .expect("the most recent failure must still be pending");
    assert_eq!(taken.message, message_b);
    assert!(
        controller.take_settled_failure().is_none(),
        "a taken failure does not reappear"
    );

    assert!(
        controller.retry_fuse(),
        "retry on the active tab must start a run"
    );
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    let retry_message = controller
        .job_failure_message()
        .expect("the retried tab must fail the same way")
        .to_string();

    controller.select_project(0);

    let notifications = controller.notifications();
    assert_eq!(
        notifications.len(),
        3,
        "every failure must be recorded, in order"
    );

    assert_eq!(notifications[0].message, message_a);
    assert_eq!(notifications[0].operation, job::FailedOperation::Setup);
    assert_eq!(notifications[0].project_title, Some(title_a));
    assert_eq!(notifications[0].project_path, Some(path_a));
    assert_eq!(notifications[0].target, None);

    for (notification, message) in [
        (&notifications[1], &message_b),
        (&notifications[2], &retry_message),
    ] {
        assert_eq!(notification.message, *message);
        assert_eq!(notification.operation, job::FailedOperation::Setup);
        assert_eq!(notification.project_title, Some(title_b.clone()));
        assert_eq!(notification.project_path, Some(path_b.clone()));
        assert_eq!(notification.target, None);
    }
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
    assert_eq!(
        controller.checkpoint_count(),
        Some(1),
        "the first tab holds a checkpoint"
    );

    // The queue promotes the second tab once the first settles.
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.select_project(1);
    match controller.job_view().state {
        job::RunState::Fused { .. } => {}
        other => panic!("expected the queued tab to complete, got {other:?}"),
    }
    assert_eq!(controller.checkpoint_count(), Some(1));
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
    assert_eq!(
        controller.checkpoint_count(),
        Some(0),
        "the cancelled tab never gained a checkpoint"
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
        common::open_and_settle(&mut controller, &[root_c]),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("dummy-align");

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

    controller.select_project(1);
    let project_path = save_to_proposed(&mut controller);
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
    assert_eq!(controller.checkpoint_count(), Some(1));

    let root_b = common::tree("invalid_frame_not_queued_b_frames");
    for i in 0..2 {
        copy_frame(&root_b, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root_b]),
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
    add_unreadable_tab(&mut controller, "retry_queues_a", 2);
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
        common::open_and_settle(&mut controller, &[survivor_root]),
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
    add_unreadable_tab(&mut controller, "closing_mid_run_a", 2);
    // The survivor: a different frame count than tab A's, so a leaked value
    // could not pass by coincidence.
    let survivor_root = common::tree("closing_mid_run_b_frames");
    for i in 0..3 {
        copy_frame(&survivor_root, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[survivor_root]),
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

// ---------------------------------------------------------------------
// A fuse writes no project: every checkpoint is held until the user saves,
// and a save is the one path that creates or changes a `.ffp`.
// ---------------------------------------------------------------------

/// Every path under `dir`, relative to it, sorted -- a directory tree's
/// whole state for a before/after comparison.
fn listing(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The frames directory `controller_with_frames(name, ..)` copied into.
fn frames_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}_frames"))
}

fn fuse_and_settle(controller: &mut AppController) {
    assert!(controller.start_fuse());
    common::wait_for_settle(controller, Duration::from_secs(120)).expect("the fuse must complete");
}

#[test]
fn a_first_fuse_writes_no_project_file() {
    let root = common::tree("first_fuse_writes_nothing_frames");
    for i in 0..3 {
        std::fs::copy(
            common::fixture_frame("mantis-64", i),
            root.join(format!("{i:02}.tif")),
        )
        .unwrap();
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("dummy-align");
    controller.set_scratch_root_for_test(common::tree("first_fuse_writes_nothing_scratch"));
    let before = listing(&root);

    fuse_and_settle(&mut controller);

    assert_eq!(
        listing(&root),
        before,
        "a fuse leaves the frames directory exactly as it was"
    );
    assert!(controller.project_tabs()[0].unsaved);
    assert_eq!(controller.checkpoint_count(), Some(1));
    match controller.canvas_feed(None) {
        CanvasFeed::Load {
            composite, zmap, ..
        } => {
            for plane in [&composite, &zmap] {
                assert!(plane.is_file());
                assert!(
                    !plane.starts_with(&root),
                    "{} is beside the frames",
                    plane.display()
                );
            }
        }
        other => panic!("a fused tab must feed its held planes, got {other:?}"),
    }
    let cards = controller.project_cards();
    let card = cards
        .iter()
        .find(|c| c.kind == "open")
        .expect("the tab has a card");
    assert!(card.picture.is_some());
}

#[test]
fn re_fuse_holds_each_checkpoint_in_memory() {
    let mut controller = controller_with_frames("re_fuse_holds", 2);
    let root = frames_dir("re_fuse_holds");
    let before = listing(&root);
    for count in 1..=3 {
        fuse_and_settle(&mut controller);
        assert_eq!(controller.checkpoint_count(), Some(count));
        assert_eq!(controller.held_checkpoints().len(), count);
        assert_eq!(listing(&root), before, "after fuse {count}");
    }
}

#[test]
fn save_is_enabled_only_with_unsaved_checkpoints() {
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    let mut controller = controller_with_frames("save_enabled", 2);
    let root = frames_dir("save_enabled");
    let before = listing(&root);

    assert!(!controller.save_enabled(), "nothing fused yet");
    controller.toggle_projects_overlay();
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::Consumed);
    assert_eq!(listing(&root), before, "a consumed Ctrl+S writes nothing");
    controller.toggle_projects_overlay();

    fuse_and_settle(&mut controller);
    assert!(controller.save_enabled());
    controller.toggle_projects_overlay();
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::SaveProject);

    save_to_proposed(&mut controller);
    assert!(!controller.save_enabled(), "a save leaves nothing to save");
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::Consumed);
}

#[test]
fn ctrl_s_saves_with_the_overlay_closed() {
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    let mut controller = controller_with_frames("ctrl_s_closed_overlay", 2);
    let root = frames_dir("ctrl_s_closed_overlay");
    let before = listing(&root);

    assert!(!controller.projects_overlay_open());
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::Consumed);
    assert_eq!(listing(&root), before, "a consumed Ctrl+S writes nothing");

    fuse_and_settle(&mut controller);
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::SaveProject);

    save_to_proposed(&mut controller);
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::Consumed);
    assert!(!controller.projects_overlay_open());
}

#[test]
fn file_chords_under_the_settings_modal() {
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    let mut controller = controller_with_two_tabs("file_chords_settings_modal");
    fuse_and_settle(&mut controller);
    assert!(controller.save_enabled());

    controller.open_settings();
    assert_eq!(controller.chrome_key("n", ctrl), ChromeKey::Consumed);
    assert_eq!(controller.chrome_key("o", ctrl), ChromeKey::Consumed);
    assert_eq!(
        controller.chrome_key("s", ctrl),
        ChromeKey::Consumed,
        "held checkpoints do not enable Ctrl+S under Settings"
    );
    assert!(
        controller.save_enabled(),
        "the consumed Ctrl+S wrote nothing"
    );

    controller.select_project(1);
    let tabs_before = controller.project_tabs().len();
    assert_eq!(
        controller.chrome_key("w", ctrl),
        ChromeKey::Consumed,
        "Ctrl+W closes nothing on an idle tab under Settings"
    );
    assert_eq!(
        controller.project_tabs().len(),
        tabs_before,
        "the tab is still open"
    );

    controller.close_settings();
    controller.select_project(0);
    assert_eq!(controller.chrome_key("n", ctrl), ChromeKey::NewProject);
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::SaveProject);

    controller.select_project(1);
    assert_eq!(
        controller.chrome_key("w", ctrl),
        ChromeKey::Consumed,
        "Ctrl+W closes the idle tab once Settings is closed"
    );
    assert_eq!(controller.project_tabs().len(), tabs_before - 1);
}

#[test]
fn wheel_actions_are_blocked_under_the_settings_modal() {
    let mut controller = controller_with_frames("wheel_blocked_settings_modal", 3);
    controller.set_pane_geometry(
        PaneRect {
            x: 0.0,
            y: 0.0,
            width: 566.0,
            height: 748.0,
        },
        PaneRect {
            x: 566.0,
            y: 0.0,
            width: 566.0,
            height: 748.0,
        },
    );
    controller.fit_if_unfitted();
    let fitted = controller.zoom_readout();
    assert!(
        !fitted.is_empty(),
        "a fitted viewport reads a non-empty zoom"
    );

    let events: std::sync::Arc<Mutex<Vec<ShellEvent>>> =
        std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(std::sync::Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    controller.open_settings();
    controller.wheel(0.0, 60.0, 100.0, 100.0, Modifiers::NONE);
    assert!(
        events.lock().unwrap().is_empty(),
        "a wheel-triggered action must raise no event under Settings"
    );
    assert_eq!(
        controller.zoom_readout(),
        fitted,
        "zoom must not change under Settings"
    );

    controller.close_settings();
    controller.wheel(0.0, 60.0, 100.0, 100.0, Modifiers::NONE);
    assert!(
        !events.lock().unwrap().is_empty(),
        "the same wheel event fires its action once Settings is closed"
    );
    assert_ne!(
        controller.zoom_readout(),
        fitted,
        "zoom changes again once Settings is closed"
    );
}

#[test]
fn save_writes_every_held_checkpoint() {
    let mut controller = controller_with_frames("save_every_held", 3);
    let root = frames_dir("save_every_held");
    fuse_and_settle(&mut controller);
    controller.toggle_frame_excluded(2);
    fuse_and_settle(&mut controller);

    let held = controller.held_checkpoints().to_vec();
    assert_eq!(held.len(), 2);
    assert_ne!(held[0].sources, held[1].sources);

    let title = controller.project_tabs()[0].title.clone();
    let target = common::tree("save_every_held_elsewhere").join("saved.ffp");
    let mut asked = Vec::new();
    assert!(controller.save_active_project(|proposed| {
        asked.push(proposed.to_path_buf());
        Some(target.clone())
    }));
    assert_eq!(asked, [root.join(format!("{title}.ffp"))]);
    assert!(target.is_file());
    assert!(
        !listing(&root)
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "ffp")),
        "the file goes where the picker said, not beside the frames"
    );

    let chain = Project::open(&target).unwrap().checkpoint_chain().unwrap();
    assert_eq!(chain.len(), 2);
    for (checkpoint, held) in chain.iter().rev().zip(&held) {
        assert_eq!(checkpoint.created_at, held.created_at);
        assert_eq!(checkpoint.algorithm, held.algorithm);
        assert_eq!(checkpoint.params, held.params);
    }
    assert_ne!(chain[0].stack_fingerprint, chain[1].stack_fingerprint);

    assert!(!controller.project_tabs()[0].unsaved);
    assert_eq!(controller.checkpoint_count(), Some(2));
    for checkpoint in &held {
        let dir = checkpoint.composite.parent().unwrap();
        assert!(!dir.exists(), "{} survived the save", dir.display());
        assert!(
            !dir.parent().unwrap().exists(),
            "the tab's held directory is emptied away"
        );
    }
}

#[test]
fn refuse_after_save_leaves_the_file_untouched() {
    let mut controller = controller_with_frames("refuse_after_save", 2);
    fuse_and_settle(&mut controller);
    let path = save_to_proposed(&mut controller);
    let untouched = |path: &Path| {
        (
            std::fs::read(path).unwrap(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        )
    };

    let before = untouched(&path);
    fuse_and_settle(&mut controller);
    assert_eq!(untouched(&path), before, "a re-fuse of a saved tab");
    assert!(controller.project_tabs()[0].unsaved);
    assert!(controller.save_active_project(|_| panic!("a saved tab never asks where")));
    let chain_len = |path: &Path| {
        Project::open(path)
            .unwrap()
            .checkpoint_chain()
            .unwrap()
            .len()
    };
    assert_eq!(chain_len(&path), 2);

    let mut reopened = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut reopened, std::slice::from_ref(&path)),
        OpenOutcome::Opened(_)
    ));
    reopened.set_scratch_root_for_test(common::tree("refuse_after_save_reopened_scratch"));
    assert!(!reopened.project_tabs()[0].unsaved);

    let before = untouched(&path);
    fuse_and_settle(&mut reopened);
    fuse_and_settle(&mut reopened);
    assert_eq!(untouched(&path), before, "a re-fuse of a reopened project");
    assert!(reopened.project_tabs()[0].unsaved);
    assert!(reopened.save_active_project(|_| panic!("an opened project never asks where")));
    assert_eq!(chain_len(&path), 4, "two fuses since the last save");
}

#[cfg(unix)]
fn set_read_only(dir: &Path, read_only: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if read_only { 0o555 } else { 0o755 };
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_failed_save_keeps_the_project_in_memory() {
    let mut controller = controller_with_frames("failed_save", 2);
    fuse_and_settle(&mut controller);
    let held_before: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();
    let still_in_memory = |controller: &mut AppController| {
        assert!(controller.project_tabs()[0].unsaved);
        let held: Vec<PathBuf> = controller
            .held_checkpoints()
            .iter()
            .map(|h| h.composite.clone())
            .collect();
        assert_eq!(held, held_before);
        assert!(held.iter().all(|p| p.is_file()));
        assert!(matches!(
            controller.canvas_feed(None),
            CanvasFeed::Load { .. }
        ));
    };

    assert!(!controller.save_active_project(|_| None));
    assert!(
        controller.take_settled_failure().is_none(),
        "a cancelled dialog is not a failure"
    );
    assert!(
        controller.notifications().is_empty(),
        "a cancelled dialog appends no record"
    );
    still_in_memory(&mut controller);

    let empty = common::tree("failed_save_read_only_empty");
    let target = empty.join("saved.ffp");
    set_read_only(&empty, true);
    let saved = controller.save_active_project(|_| Some(target.clone()));
    let left = listing(&empty);
    set_read_only(&empty, false);
    assert!(!saved);
    assert!(left.is_empty(), "nothing was left behind: {left:?}");
    let notification = controller
        .take_settled_failure()
        .expect("a failed save says why");
    assert_eq!(notification.operation, job::FailedOperation::Save);
    assert_eq!(notification.target, Some(target.clone()));
    assert!(
        notification.message.contains(&target.display().to_string()),
        "{}",
        notification.message
    );
    assert!(
        notification.message.contains("Permission denied"),
        "{}",
        notification.message
    );
    assert_eq!(controller.notifications().len(), 1);
    still_in_memory(&mut controller);

    let holding = common::tree("failed_save_read_only_existing");
    let existing = holding.join("existing.ffp");
    std::fs::write(&existing, b"an earlier project").unwrap();
    set_read_only(&holding, true);
    let saved = controller.save_active_project(|_| Some(existing.clone()));
    let left = listing(&holding);
    set_read_only(&holding, false);
    assert!(!saved);
    assert_eq!(left, [PathBuf::from("existing.ffp")]);
    assert_eq!(std::fs::read(&existing).unwrap(), b"an earlier project");
    let notification = controller
        .take_settled_failure()
        .expect("a failed save says why");
    assert_eq!(notification.operation, job::FailedOperation::Save);
    assert_eq!(notification.target, Some(existing.clone()));
    assert!(
        notification
            .message
            .contains(&existing.display().to_string()),
        "{}",
        notification.message
    );
    assert_eq!(controller.notifications().len(), 2);
    still_in_memory(&mut controller);
}

// ---------------------------------------------------------------------
// The unsaved-close guard: every close path asks the same question when a
// tab holds checkpoints no `.ffp` contains, and only then.
// ---------------------------------------------------------------------

#[test]
fn a_close_asks_only_with_held_checkpoints() {
    let mut controller = controller_with_three_tabs("asks_only_with_held");
    controller.select_project(1);
    fuse_and_settle(&mut controller);
    controller.select_project(2);
    fuse_and_settle(&mut controller);
    save_to_proposed(&mut controller);

    let mut now = Instant::now();
    let tabs_before = controller.project_tabs().len();
    controller.request_close_at(0, now);
    assert!(
        controller.pending_close().is_none(),
        "a never-fused tab closes at once"
    );
    assert_eq!(controller.project_tabs().len(), tabs_before - 1);

    now += Duration::from_secs(1);
    let tabs_before = controller.project_tabs().len();
    controller.request_close_at(1, now);
    assert!(
        controller.pending_close().is_none(),
        "a fused-then-saved tab closes at once"
    );
    assert_eq!(controller.project_tabs().len(), tabs_before - 1);

    now += Duration::from_secs(1);
    let tabs_before = controller.project_tabs().len();
    controller.request_close_at(0, now);
    assert!(
        controller.pending_close().is_some(),
        "a fused, unsaved tab asks first"
    );
    assert_eq!(
        controller.project_tabs().len(),
        tabs_before,
        "asking leaves the tab open"
    );
}

#[test]
fn every_close_path_asks_the_same_question() {
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    let mut controller = controller_with_two_tabs("every_close_path_asks");
    fuse_and_settle(&mut controller);
    controller.select_project(1);
    fuse_and_settle(&mut controller);
    fuse_and_settle(&mut controller);

    let title1 = controller.project_tabs()[1].title.clone();
    assert_eq!(controller.chrome_key("w", ctrl), ChromeKey::Consumed);
    let copy = controller
        .pending_close()
        .expect("Ctrl+W on the active, fused tab asks");
    assert_eq!(copy.context, format!("Closing {title1}"));
    assert!(
        copy.body.starts_with("2 checkpoints have been fused"),
        "{}",
        copy.body
    );
    controller.cancel_close();

    let title0 = controller.project_tabs()[0].title.clone();
    controller.request_close(0);
    let copy = controller
        .pending_close()
        .expect("request_close on the inactive, fused tab asks");
    assert_eq!(copy.context, format!("Closing {title0}"));
    assert!(
        copy.body.starts_with("1 checkpoint has been fused"),
        "{}",
        copy.body
    );
}

#[test]
fn cancelling_a_close_keeps_everything() {
    let mut controller = controller_with_frames("cancelling_a_close", 2);
    fuse_and_settle(&mut controller);
    let held_before: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();

    controller.request_close(0);
    assert!(controller.pending_close().is_some());
    controller.cancel_close();

    assert!(controller.pending_close().is_none());
    assert_eq!(controller.project_tabs().len(), 1);
    assert!(controller.save_enabled());
    let held_after: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();
    assert_eq!(held_after, held_before);
    for path in &held_after {
        assert!(
            path.is_file(),
            "{} must survive a cancelled close",
            path.display()
        );
    }
}

#[test]
fn closing_without_saving_drops_the_held_checkpoints() {
    let mut controller = controller_with_frames("closing_without_saving", 2);
    let root = frames_dir("closing_without_saving");
    let before = listing(&root);
    fuse_and_settle(&mut controller);
    let held = controller.held_checkpoints().to_vec();
    assert!(!held.is_empty());

    controller.request_close(0);
    assert!(controller.pending_close().is_some());
    controller.close_without_saving();

    assert!(controller.pending_close().is_none());
    assert_eq!(controller.project_tabs().len(), 0);
    assert_eq!(
        listing(&root),
        before,
        "closing without saving leaves the frames directory untouched"
    );
    assert!(
        !listing(&root)
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "ffp")),
        "no .ffp was written"
    );
    for checkpoint in &held {
        let dir = checkpoint.composite.parent().unwrap();
        assert!(
            !dir.exists(),
            "{} survived closing without saving",
            dir.display()
        );
    }
}

#[test]
fn save_and_close_saves_that_tab_then_closes_it() {
    let outer = "save_and_close_saves_that_tab";
    let mut controller = controller_with_two_tabs(outer);
    let root_b = frames_dir(&format!("{outer}_b"));

    fuse_and_settle(&mut controller);
    let title0 = controller.project_tabs()[0].title.clone();
    let held0_before: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();

    controller.select_project(1);
    fuse_and_settle(&mut controller);
    controller.select_project(0);

    controller.request_close(1);
    assert!(controller.pending_close().is_some());
    let target = common::tree(&format!("{outer}_elsewhere")).join("tab1.ffp");
    let mut asked = Vec::new();
    assert!(controller.save_and_close(|proposed| {
        asked.push(proposed.to_path_buf());
        Some(target.clone())
    }));
    assert!(
        asked[0].starts_with(&root_b),
        "a never-saved tab proposes a path inside its own frames directory: {asked:?}"
    );
    assert!(target.is_file());
    assert!(controller.pending_close().is_none());
    assert_eq!(controller.project_tabs().len(), 1);
    assert_eq!(
        controller.project_tabs()[0].title,
        title0,
        "the other tab is untouched"
    );
    let held0_after: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();
    assert_eq!(held0_after, held0_before);

    // Past `AppController::CLOSE_DEBOUNCE`, so the next close is not the
    // one just above's double-click guard swallowing it.
    std::thread::sleep(Duration::from_millis(350));

    // A cancelled picker leaves a never-saved, fused tab open with its
    // checkpoints held.
    let root_c = common::tree(&format!("{outer}_c_frames"));
    for i in 0..2 {
        copy_frame(&root_c, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root_c]),
        OpenOutcome::Opened(_)
    ));
    let new_index = controller.project_tabs().len() - 1;
    fuse_and_settle(&mut controller);
    let tabs_before = controller.project_tabs().len();

    controller.request_close(new_index);
    assert!(controller.pending_close().is_some());
    assert!(!controller.save_and_close(|_| None));
    assert!(
        controller.pending_close().is_none(),
        "the question clears either way"
    );
    assert_eq!(
        controller.project_tabs().len(),
        tabs_before,
        "a cancelled picker leaves the tab open"
    );
    assert!(!controller.held_checkpoints().is_empty());

    // Reopening and re-fusing the tab saved above appends without asking.
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&target)),
        OpenOutcome::Opened(_)
    ));
    let reopened_index = controller.project_tabs().len() - 1;
    fuse_and_settle(&mut controller);
    let tabs_before = controller.project_tabs().len();

    controller.request_close(reopened_index);
    assert!(controller.pending_close().is_some());
    assert!(controller.save_and_close(|_| panic!("an opened tab never asks where")));
    assert_eq!(controller.project_tabs().len(), tabs_before - 1);

    let chain = Project::open(&target).unwrap().checkpoint_chain().unwrap();
    assert_eq!(
        chain.len(),
        2,
        "the second fuse appended onto the same file"
    );
}

#[test]
fn chords_are_consumed_while_a_close_is_pending() {
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    let mut controller = controller_with_frames("chords_consumed_pending", 2);
    let root = frames_dir("chords_consumed_pending");
    fuse_and_settle(&mut controller);
    controller.request_close(0);
    assert!(controller.pending_close().is_some());

    let before = listing(&root);
    let tabs_before = controller.project_tabs().len();
    for key in ["n", "o", "s", "w"] {
        assert_eq!(
            controller.chrome_key(key, ctrl),
            ChromeKey::Consumed,
            "{key} while a close is pending"
        );
    }
    assert_eq!(controller.project_tabs().len(), tabs_before);
    assert_eq!(
        listing(&root),
        before,
        "nothing is written while the question is open"
    );
    assert!(
        controller.pending_close().is_some(),
        "the question is still pending"
    );
}

#[test]
fn close_guard_is_pushed_to_the_window() {
    use slint::ComponentHandle;

    i_slint_backend_testing::init_no_event_loop();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let mut controller = controller_with_two_tabs("close_guard_pushed");
    fuse_and_settle(&mut controller);
    let now = Instant::now();
    controller.request_close_at(0, now);
    assert!(controller.pending_close().is_some());

    let controller = std::sync::Arc::new(std::sync::Mutex::new(controller));
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(app.get_close_guard_open());
    assert!(!app.get_close_guard_body().is_empty());

    controller.lock().unwrap().cancel_close();
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(!app.get_close_guard_open());

    let tabs_before = controller.lock().unwrap().project_tabs().len();
    controller
        .lock()
        .unwrap()
        .request_close_at(1, now + Duration::from_millis(100));
    assert_eq!(
        controller.lock().unwrap().project_tabs().len(),
        tabs_before - 1,
        "tab 1 has nothing held and closes at once"
    );

    let tabs_before = controller.lock().unwrap().project_tabs().len();
    controller
        .lock()
        .unwrap()
        .request_close_at(0, now + Duration::from_millis(200));
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(
        !app.get_close_guard_open(),
        "inside the debounce, the request is ignored"
    );
    assert_eq!(
        controller.lock().unwrap().project_tabs().len(),
        tabs_before,
        "a debounced request neither closes nor asks"
    );
}

#[test]
fn quit_asks_only_with_held_checkpoints() {
    let mut controller = controller_with_two_tabs("quit_asks_only");

    let events: std::sync::Arc<Mutex<Vec<ShellEvent>>> =
        std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(std::sync::Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    controller.window_close();
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ShellEvent::WindowCloseRequested)),
        "an all-idle window quits at once"
    );
    assert!(controller.pending_quit_copy().is_none());

    events.lock().unwrap().clear();
    fuse_and_settle(&mut controller);
    let tabs_before = controller.project_tabs().len();
    controller.window_close();
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ShellEvent::WindowCloseRequested)),
        "a window with a fused tab does not quit at once"
    );
    assert!(controller.pending_quit_copy().is_some());
    assert_eq!(
        controller.project_tabs().len(),
        tabs_before,
        "no tab is touched while the quit question is open"
    );
}

#[test]
fn cancelling_or_forcing_a_quit() {
    let mut controller = controller_with_two_tabs("cancelling_or_forcing_a_quit");
    fuse_and_settle(&mut controller);
    let held0: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();
    assert!(!held0.is_empty());

    controller.select_project(1);
    fuse_and_settle(&mut controller);
    let held1: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();
    assert!(!held1.is_empty());

    controller.window_close();
    assert!(controller.pending_quit_copy().is_some());
    controller.cancel_quit();

    assert!(controller.pending_quit_copy().is_none());
    assert_eq!(controller.project_tabs().len(), 2);
    controller.select_project(0);
    assert_eq!(
        controller
            .held_checkpoints()
            .iter()
            .map(|h| h.composite.clone())
            .collect::<Vec<_>>(),
        held0,
        "tab 0's held checkpoints survive Cancel"
    );
    controller.select_project(1);
    assert_eq!(
        controller
            .held_checkpoints()
            .iter()
            .map(|h| h.composite.clone())
            .collect::<Vec<_>>(),
        held1,
        "tab 1's held checkpoints survive Cancel"
    );

    let events: std::sync::Arc<Mutex<Vec<ShellEvent>>> =
        std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(std::sync::Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    controller.window_close();
    assert!(controller.pending_quit_copy().is_some());
    controller.quit_without_saving();

    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ShellEvent::WindowCloseRequested)),
        "Quit without saving emits WindowCloseRequested"
    );
    assert!(controller.pending_quit_copy().is_none());
    for path in held0.iter().chain(held1.iter()) {
        assert!(
            path.is_file(),
            "{} must survive quitting without saving -- nothing is written or removed",
            path.display()
        );
    }
}

#[test]
fn save_all_and_quit_saves_every_unsaved_tab() {
    let outer = "save_all_and_quit";
    let mut controller = controller_with_two_tabs(outer);
    let root_b = frames_dir(&format!("{outer}_b"));

    // Tab 0: saved once, then re-fused so it holds one more unsaved
    // checkpoint an opened project appends onto.
    fuse_and_settle(&mut controller);
    let target0 = save_to_proposed(&mut controller);
    fuse_and_settle(&mut controller);
    assert!(!controller.held_checkpoints().is_empty());

    // Tab 1: never saved.
    controller.select_project(1);
    fuse_and_settle(&mut controller);
    controller.select_project(0);

    let target1 = common::tree(&format!("{outer}_elsewhere")).join("tab1.ffp");
    let asked = std::cell::RefCell::new(Vec::new());
    controller.window_close();
    assert!(controller.pending_quit_copy().is_some());
    assert!(controller.save_all_and_quit(|proposed| {
        asked.borrow_mut().push(proposed.to_path_buf());
        Some(target1.clone())
    }));
    let asked = asked.into_inner();

    assert_eq!(
        asked.len(),
        1,
        "the picker is asked only for the never-saved tab: {asked:?}"
    );
    assert!(
        asked[0].starts_with(&root_b),
        "a never-saved tab proposes a path inside its own frames directory: {asked:?}"
    );
    assert!(target1.is_file());
    let chain0 = Project::open(&target0).unwrap().checkpoint_chain().unwrap();
    assert_eq!(
        chain0.len(),
        2,
        "the opened tab appended with no picker call"
    );
    assert!(controller.pending_quit_copy().is_none());
    controller.select_project(0);
    assert!(controller.held_checkpoints().is_empty());
    controller.select_project(1);
    assert!(controller.held_checkpoints().is_empty());

    // A second run: tab 0 (opened, re-fused) saves first, then tab 1
    // (never-saved) cancels the picker -- the quit is cancelled but tab 0's
    // save earlier in this same run is not undone.
    let outer2 = "save_all_and_quit_cancelled";
    let mut controller = controller_with_two_tabs(outer2);
    fuse_and_settle(&mut controller);
    let target0b = save_to_proposed(&mut controller);
    fuse_and_settle(&mut controller);
    assert!(!controller.held_checkpoints().is_empty());

    controller.select_project(1);
    fuse_and_settle(&mut controller);
    let held1_before: Vec<PathBuf> = controller
        .held_checkpoints()
        .iter()
        .map(|h| h.composite.clone())
        .collect();
    assert!(!held1_before.is_empty());
    controller.select_project(0);

    controller.window_close();
    assert!(controller.pending_quit_copy().is_some());
    assert!(!controller.save_all_and_quit(|_| None));

    assert!(
        controller.pending_quit_copy().is_none(),
        "the quit is cancelled either way"
    );
    let chain0b = Project::open(&target0b)
        .unwrap()
        .checkpoint_chain()
        .unwrap();
    assert_eq!(
        chain0b.len(),
        2,
        "tab 0's save, already done this run, is not undone by the cancellation"
    );
    controller.select_project(0);
    assert!(
        controller.held_checkpoints().is_empty(),
        "tab 0 is saved and no longer holds anything"
    );
    controller.select_project(1);
    assert_eq!(
        controller
            .held_checkpoints()
            .iter()
            .map(|h| h.composite.clone())
            .collect::<Vec<_>>(),
        held1_before,
        "tab 1's checkpoint stays held after the picker cancels"
    );
}

#[test]
fn quit_and_close_questions_cannot_overlap() {
    let mut controller = controller_with_two_tabs("quit_and_close_cannot_overlap");
    fuse_and_settle(&mut controller);
    controller.select_project(1);
    fuse_and_settle(&mut controller);
    controller.select_project(0);

    // `window_close` is a no-op while a close question is pending.
    controller.request_close(0);
    assert!(controller.pending_close().is_some());
    let tabs_before = controller.project_tabs().len();
    controller.window_close();
    assert!(
        controller.pending_quit_copy().is_none(),
        "window_close is a no-op while a close question is pending"
    );
    assert_eq!(controller.project_tabs().len(), tabs_before);
    controller.cancel_close();

    // `request_close_at` is a no-op while a quit question is pending.
    controller.window_close();
    assert!(controller.pending_quit_copy().is_some());
    let now = Instant::now();
    let tabs_before = controller.project_tabs().len();
    controller.request_close_at(1, now);
    assert!(
        controller.pending_close().is_none(),
        "request_close_at is a no-op while a quit question is pending"
    );
    assert_eq!(controller.project_tabs().len(), tabs_before);
}

// ---------------------------------------------------------------------
// `job::resolve_backend`'s own wording, and `AppController::fusion_backend`
// reporting the same line from every entry point that reaches `job::spawn`.
// No GPU needed: every tab here keeps the registry's default `depth-map`
// algorithm, which `backend_for` refuses before ever probing an adapter.
// ---------------------------------------------------------------------

#[test]
fn fusion_backend_off_reports_gpu_disabled() {
    let (backend, line) = job::resolve_backend("depth-map", false);
    assert!(backend.is_none());
    assert_eq!(line, "fusion backend: cpu — GPU processing is off");
}

#[test]
fn fusion_backend_on_depth_map_reports_no_gpu_implementation() {
    let (backend, line) = job::resolve_backend("depth-map", true);
    assert!(backend.is_none());
    assert_eq!(
        line,
        "fusion backend: cpu — depth-map has no GPU implementation"
    );
}

#[test]
fn fusion_backend_on_dummy_stack_reports_no_gpu_implementation() {
    let (backend, line) = job::resolve_backend("dummy-stack", true);
    assert!(backend.is_none());
    assert_eq!(
        line,
        "fusion backend: cpu — dummy-stack has no GPU implementation"
    );
}

/// Polls `AppController::fusion_backend()` until the worker thread it was
/// just handed has resolved and stored its own line.
fn wait_for_backend_line(controller: &AppController, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Some(line) = controller.fusion_backend() {
            return line;
        }
        assert!(
            start.elapsed() < timeout,
            "no fusion backend line was reported within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

const DEPTH_MAP_CPU_LINE: &str = "fusion backend: cpu — depth-map has no GPU implementation";

#[test]
fn fusion_backend_reports_on_a_paused_direct_start() {
    let mut controller = controller_with_frames("fusion_backend_direct", 2);
    controller.set_gpu_fusion(true);
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));
    assert_eq!(
        wait_for_backend_line(&controller, Duration::from_secs(30)),
        DEPTH_MAP_CPU_LINE
    );
}

#[test]
fn fusion_backend_reports_on_queue_promotion() {
    let mut controller = controller_with_two_tabs("fusion_backend_queue");
    controller.set_gpu_fusion(true);
    start_and_hold_at_pause(&mut controller, Duration::from_secs(120));

    controller.select_project(1);
    assert!(
        controller.start_fuse(),
        "a press on the second tab while the first runs must enqueue it"
    );

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    // The queue promotion inside `settle_job` has just spawned tab 1's own
    // worker; its `resolve_backend` line lands moments later.
    assert_eq!(
        wait_for_backend_line(&controller, Duration::from_secs(30)),
        DEPTH_MAP_CPU_LINE
    );
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
}

#[test]
fn fusion_backend_reports_on_retry() {
    let mut controller = controller_with_an_unreadable_frame("fusion_backend_retry");
    controller.set_gpu_fusion(true);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));

    assert!(controller.retry_fuse(), "a failed run must be retryable");
    assert_eq!(
        wait_for_backend_line(&controller, Duration::from_secs(30)),
        DEPTH_MAP_CPU_LINE
    );
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
}

#[test]
fn fusion_backend_reports_on_run_gallery_action() {
    let mut controller = controller_with_an_unreadable_frame("fusion_backend_gallery_action");
    controller.set_gpu_fusion(true);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));

    assert!(
        controller.run_gallery_action(0),
        "a `Run again` press on a failed card's own kept request must start a run"
    );
    assert_eq!(
        wait_for_backend_line(&controller, Duration::from_secs(30)),
        DEPTH_MAP_CPU_LINE
    );
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
}

// A Laplacian run with GPU fusion on matches the same frames' CPU run: every
// Z-Map index the two runs disagree on is a proven near-tie, a pixel where
// perturbing the frames' warped samples by ±1 LSB flips the level-0 winner,
// and the composite stays within ±1 LSB except where such a flip, at level 0
// or at a coarser pyramid level, is proven.

#[cfg(feature = "gpu-tests")]
mod gpu_fusion {
    use super::*;
    use focusforge_core::algorithm::TileReader;
    use focusforge_core::align::{
        AlignDriverOptions, FailurePolicy, WarpingReader, align_stack_with, auto_crop,
    };
    use focusforge_core::fuse::laplacian::{score_plane, score_radius};
    use focusforge_core::fuse::pyramid;
    use focusforge_core::io::{FrameReader, TiffStack};
    use std::collections::HashMap;

    const LSB_TOLERANCE: i32 = 1;

    #[derive(Clone)]
    struct Run {
        composite: Vec<u16>,
        zmap: Vec<u16>,
        held: job::HeldCheckpoint,
    }

    /// Fuses `frame_count` mantis-512 frames on `laplacian` through a fresh
    /// controller (`dummy-align`, deterministic), GPU or host, with frame
    /// `excluded` switched off first. Returns the run's composite samples,
    /// Z-Map indices and held checkpoint.
    fn run_laplacian(name: &str, frame_count: usize, excluded: Option<usize>, gpu: bool) -> Run {
        let mut controller = controller_with_frames(name, frame_count);
        if let Some(index) = excluded {
            controller.toggle_frame_excluded(index);
            assert!(controller.images_panel().rows[index].excluded);
        }
        controller.set_fuse_algorithm("laplacian");
        controller.set_gpu_fusion(gpu);
        assert!(controller.start_fuse());
        if gpu {
            let line = wait_for_backend_line(&controller, Duration::from_secs(120));
            assert!(
                line.starts_with("fusion backend: gpu — "),
                "a GPU fusion run must actually resolve the GPU path: {line}"
            );
        }
        let planes = common::wait_for_settle(&mut controller, Duration::from_secs(300));
        let (composite_path, zmap_path) = planes.unwrap_or_else(|| {
            panic!(
                "a completed Laplacian run must hand back its planes, got {:?}",
                controller.job_view().state
            )
        });
        let held = controller
            .held_checkpoints()
            .last()
            .cloned()
            .expect("a completed run is held until saved");
        assert_eq!(
            held.sources.len(),
            frame_count - usize::from(excluded.is_some()),
            "the run must fuse exactly the included frames"
        );

        let stack = TiffStack::open_files(&[composite_path]).unwrap();
        let (width, height) = stack.dimensions();
        assert_eq!((width, height), (held.width, held.height));
        let composite = stack
            .read_tile(0, full_rect(width, height))
            .unwrap()
            .samples;

        let reader = focusforge_core::zmap::ZMapReader::open(&zmap_path).unwrap();
        let tile = full_rect(width, height);
        let mut zmap = vec![0u16; reader.info().tile_len(tile)];
        reader.read_tile_into(tile, &mut zmap).unwrap();

        Run {
            composite,
            zmap,
            held,
        }
    }

    fn full_rect(width: u32, height: u32) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// A run's frames re-aligned, warped and scored exactly as its fusion
    /// scored them: every frame's warped samples and windowed score at every
    /// pyramid level over the whole canvas, and the crop that maps a Z-Map
    /// pixel onto it.
    struct NearTieProof {
        samples: Vec<Vec<u16>>,
        /// `scores[level][frame]`, one value per pixel of that level.
        scores: Vec<Vec<Vec<f32>>>,
        width: u32,
        height: u32,
        spp: u32,
        levels: u32,
        crop: Rect,
    }

    impl NearTieProof {
        fn for_run(held: &job::HeldCheckpoint) -> NearTieProof {
            let stack = TiffStack::open_files(&held.sources).unwrap();
            let ctx = stack.stack_context();
            let (width, height, spp) = (ctx.width, ctx.height, ctx.samples_per_pixel);
            let align = held.align.as_ref().expect("the run records its alignment");
            let aligner = registry::find_align(&align.algorithm).unwrap();
            let alignment = align_stack_with(
                aligner,
                ctx,
                &stack,
                &align.params,
                FailurePolicy::IdentityFallback,
                &AlignDriverOptions::default(),
            )
            .unwrap()
            .alignment;
            assert_eq!(alignment.reference, align.reference);
            let crop = auto_crop(&alignment, (width, height))
                .unwrap()
                .expect("the run was cropped, so its alignment leaves an interior");
            assert_eq!((crop.width, crop.height), (held.width, held.height));
            let levels =
                held.params
                    .get("levels")
                    .and_then(ParamValue::as_i64)
                    .expect("a Laplacian run records its resolved depth") as u32;
            let transforms = alignment.frames.iter().map(|f| f.transform).collect();
            let reader = WarpingReader::new(&stack, (width, height), spp, transforms);

            let mut proof = NearTieProof {
                samples: Vec::new(),
                scores: vec![Vec::new(); levels as usize],
                width,
                height,
                spp,
                levels,
                crop,
            };
            for frame in 0..held.sources.len() {
                let mut samples = vec![0u16; (width * height * spp) as usize];
                reader
                    .read_tile_into(frame, full_rect(width, height), &mut samples)
                    .unwrap();
                let pyramid = pyramid::build(&samples, width, height, spp, levels).unwrap();
                for level in 0..levels {
                    let scores =
                        score_plane(&pyramid.levels()[level as usize], proof.radius(level));
                    proof.scores[level as usize].push(scores);
                }
                proof.samples.push(samples);
            }
            proof
        }

        fn radius(&self, level: u32) -> u32 {
            score_radius(self.levels, level)
        }

        /// The canvas pixel under Z-Map pixel `p`.
        fn canvas_pixel(&self, p: usize) -> usize {
            let x = self.crop.x as usize + p % self.crop.width as usize;
            let y = self.crop.y as usize + p / self.crop.width as usize;
            y * self.width as usize + x
        }

        /// The pixel of pyramid level `level` that canvas pixel `q` lies in.
        fn level_pixel(&self, q: usize, level: u32) -> usize {
            let x = (q % self.width as usize) >> level;
            let y = (q / self.width as usize) >> level;
            y * self.width.div_ceil(1 << level) as usize + x
        }

        /// The canvas pixel at the origin of level `level`'s pixel `m`.
        fn canvas_origin(&self, m: usize, level: u32) -> usize {
            let level_width = self.width.div_ceil(1 << level) as usize;
            ((m / level_width) << level) * self.width as usize + ((m % level_width) << level)
        }

        /// Every pixel of pyramid level `level` whose collapse footprint
        /// reaches canvas pixel `q`: `expand` spreads level pixel `m` over the
        /// canvas pixels within `2(2^level - 1)` of `2^level · m` per axis.
        fn reaching_pixels(&self, q: usize, level: u32) -> Vec<usize> {
            let step = 1i64 << level;
            let reach = 2 * (step - 1);
            let level_width = i64::from(self.width.div_ceil(1 << level));
            let level_height = i64::from(self.height.div_ceil(1 << level));
            let span = |c: i64, len: i64| {
                let lo = -(reach - c).div_euclid(step);
                let hi = (c + reach).div_euclid(step);
                lo.max(0)..=hi.min(len - 1)
            };
            let xs = span((q % self.width as usize) as i64, level_width);
            span((q / self.width as usize) as i64, level_height)
                .flat_map(|my| xs.clone().map(move |mx| (my * level_width + mx) as usize))
                .collect()
        }

        /// Level `level`'s best two frames at level pixel `m`, lowest frame
        /// first on a tie, and the winner's margin over the runner-up.
        fn top_two(&self, level: u32, m: usize) -> (usize, usize, f32) {
            let scores = &self.scores[level as usize];
            let mut frames: Vec<usize> = (0..scores.len()).collect();
            frames.sort_by(|&a, &b| scores[b][m].total_cmp(&scores[a][m]));
            let (winner, runner_up) = (frames[0], frames[1]);
            (winner, runner_up, scores[winner][m] - scores[runner_up][m])
        }

        /// Laplacian levels `0..=level` of `base` with every
        /// `(pixel, delta)` in `deltas` applied to all channels, clamped to
        /// `u16`. Level `k` depends only on the Gaussians up to `k + 1`.
        fn laplacian(&self, base: &[u16], deltas: &[(usize, i32)], level: u32) -> pyramid::Pyramid {
            let mut samples = base.to_vec();
            for &(pixel, delta) in deltas {
                for c in 0..self.spp as usize {
                    let i = pixel * self.spp as usize + c;
                    samples[i] = (i32::from(samples[i]) + delta).clamp(0, 65535) as u16;
                }
            }
            pyramid::build(&samples, self.width, self.height, self.spp, level + 1).unwrap()
        }

        /// Frame `base`'s level-`level` score at level pixel `m` with
        /// `deltas` applied, through the real `score_plane`.
        fn perturbed_score(
            &self,
            base: &[u16],
            deltas: &[(usize, i32)],
            level: u32,
            m: usize,
        ) -> f32 {
            let pyramid = self.laplacian(base, deltas, level);
            score_plane(&pyramid.levels()[level as usize], self.radius(level))[m]
        }

        /// The winner at level pixel `m` of level `level`, lowest frame
        /// winning a tie as the Laplacian selection does, with frame
        /// `replaced`'s score there read as `replaced_score`.
        fn winner_with(&self, level: u32, m: usize, replaced: usize, replaced_score: f32) -> usize {
            let scores = &self.scores[level as usize];
            let score = |f: usize| {
                if f == replaced {
                    replaced_score
                } else {
                    scores[f][m]
                }
            };
            (1..scores.len()).fold(0, |best, f| if score(f) > score(best) { f } else { best })
        }

        /// Z-Map pixels where `other` disagrees with `cpu` and no ±1 LSB
        /// perturbation of the frames' warped samples reaches `other`'s
        /// pick from the CPU winner at level 0, plus the total disagreement
        /// count.
        fn unproven_disagreements(&self, other: &[u16], cpu: &[u16]) -> (usize, Vec<usize>) {
            let differing: Vec<usize> = (0..cpu.len()).filter(|&p| other[p] != cpu[p]).collect();
            let unproven = differing
                .iter()
                .copied()
                .filter(|&p| {
                    let q = self.canvas_pixel(p);
                    !self.reachable_near_tie(0, q, cpu[p] as usize, other[p] as usize)
                })
                .collect();
            (differing.len(), unproven)
        }

        /// A level `1..levels` with a pixel whose collapse footprint reaches
        /// canvas pixel `q` and where a ±1 LSB perturbation moves the
        /// selection off its winner toward its runner-up, narrowest margins
        /// tried first. No run records coarser winners, so each level's top
        /// two scores stand in for them. `proven` caches each
        /// `(level, pixel)` verdict across calls.
        fn coarser_near_tie(
            &self,
            q: usize,
            proven: &mut HashMap<(u32, usize), bool>,
        ) -> Option<u32> {
            let mut candidates: Vec<(u32, usize)> = (1..self.levels)
                .flat_map(|level| {
                    self.reaching_pixels(q, level)
                        .into_iter()
                        .map(move |m| (level, m))
                })
                .collect();
            candidates.sort_by(|&(ka, a), &(kb, b)| {
                self.top_two(ka, a).2.total_cmp(&self.top_two(kb, b).2)
            });
            candidates.into_iter().find_map(|(level, m)| {
                let found = *proven.entry((level, m)).or_insert_with(|| {
                    let (winner, runner_up, margin) = self.top_two(level, m);
                    let origin = self.canvas_origin(m, level);
                    let found = self.reachable_near_tie(level, origin, winner, runner_up);
                    let level_width = self.width.div_ceil(1 << level) as usize;
                    println!(
                        "level {level} pixel ({}, {}): winner {winner} runner-up {runner_up} \
                         margin {margin} reachable near-tie {found}",
                        m % level_width,
                        m / level_width
                    );
                    found
                });
                found.then_some(level)
            })
        }

        /// Whether a ±1 LSB perturbation of the warped samples moves the
        /// level-`level` winner over canvas pixel `q` off `winner`: first
        /// each frame's own sample at `q` alone, then every sample in the
        /// level-0 score window around `q` at once, `winner` pushed to its
        /// lowest score and `challenger` to its highest.
        fn reachable_near_tie(
            &self,
            level: u32,
            q: usize,
            winner: usize,
            challenger: usize,
        ) -> bool {
            let m = self.level_pixel(q, level);
            for (frame, base) in self.samples.iter().enumerate() {
                for delta in [1, -1] {
                    let perturbed = self.perturbed_score(base, &[(q, delta)], level, m);
                    if self.winner_with(level, m, frame, perturbed) != winner {
                        return true;
                    }
                }
            }
            self.window_extreme(level, winner, q, false)
                < self.window_extreme(level, challenger, q, true)
        }

        /// Frame `frame`'s level-`level` score over canvas pixel `q` with
        /// every sample of the level-0 score window around `q` moved 1 LSB
        /// in whichever direction, tried alone, pushes the score up
        /// (`raise`) or down.
        fn window_extreme(&self, level: u32, frame: usize, q: usize, raise: bool) -> f32 {
            let base = &self.samples[frame];
            let m = self.level_pixel(q, level);
            let baseline = self.scores[level as usize][frame][m];
            let r = i64::from(self.radius(0));
            let (qx, qy) = (
                (q % self.width as usize) as i64,
                (q / self.width as usize) as i64,
            );
            let mut taps: Vec<usize> = (-r..=r)
                .flat_map(|dy| (-r..=r).map(move |dx| (dx, dy)))
                .map(|(dx, dy)| {
                    let x = (qx + dx).clamp(0, i64::from(self.width) - 1) as usize;
                    let y = (qy + dy).clamp(0, i64::from(self.height) - 1) as usize;
                    y * self.width as usize + x
                })
                .collect();
            taps.sort_unstable();
            taps.dedup();
            let probe = |tap: usize| {
                let plus = self.score_at(&self.laplacian(base, &[(tap, 1)], level), level, m);
                let minus = self.score_at(&self.laplacian(base, &[(tap, -1)], level), level, m);
                let plus_wins = if raise { plus >= minus } else { plus <= minus };
                let (delta, score) = if plus_wins { (1, plus) } else { (-1, minus) };
                let helps = if raise {
                    score > baseline
                } else {
                    score < baseline
                };
                helps.then_some((tap, delta))
            };
            // Each probe rebuilds a whole-canvas pyramid; spread them over
            // the cores.
            let workers = std::thread::available_parallelism().map_or(1, usize::from);
            let deltas: Vec<(usize, i32)> = std::thread::scope(|s| {
                let probe = &probe;
                let handles: Vec<_> = taps
                    .chunks(taps.len().div_ceil(workers))
                    .map(|chunk| {
                        s.spawn(move || chunk.iter().filter_map(|&t| probe(t)).collect::<Vec<_>>())
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().expect("a tap probe does not panic"))
                    .collect()
            });
            self.perturbed_score(base, &deltas, level, m)
        }

        /// `score_plane`'s window sum at level pixel `m` alone, for ranking
        /// single-tap probes; only the summation order differs from
        /// `score_plane`.
        fn score_at(&self, pyramid: &pyramid::Pyramid, level: u32, m: usize) -> f32 {
            let plane = &pyramid.levels()[level as usize];
            let (width, height) = (plane.width() as usize, plane.height() as usize);
            let data = plane.data();
            let spp = self.spp as usize;
            let r = i64::from(self.radius(level));
            let (mx, my) = ((m % width) as i64, (m / width) as i64);
            let mut acc = 0.0f32;
            for dy in -r..=r {
                let y = (my + dy).clamp(0, height as i64 - 1) as usize;
                for dx in -r..=r {
                    let x = (mx + dx).clamp(0, width as i64 - 1) as usize;
                    let i = (y * width + x) * spp;
                    for v in &data[i..i + spp] {
                        acc += v.abs();
                    }
                }
            }
            acc
        }

        /// Every composite sample of `other` more than `LSB_TOLERANCE` off
        /// `cpu`'s, as `((x, y), delta)`, at a pixel no proven near-tie
        /// explains. A pixel whose Z-Map indices differ counts as proven:
        /// callers first require [`Self::unproven_disagreements`] to be
        /// empty. Where the indices agree, some level `1..levels` must prove
        /// a near-tie at a pixel whose collapse footprint reaches it.
        fn unexplained_composite(&self, other: &Run, cpu: &Run) -> Vec<((usize, usize), i32)> {
            let spp = cpu.composite.len() / cpu.zmap.len();
            let width = cpu.held.width as usize;
            let mut explained = vec![0usize; self.levels as usize];
            let mut proven = HashMap::new();
            let mut over_bound = Vec::new();
            for p in 0..cpu.zmap.len() {
                let deltas: Vec<i32> = (p * spp..(p + 1) * spp)
                    .map(|i| (i32::from(other.composite[i]) - i32::from(cpu.composite[i])).abs())
                    .collect();
                if deltas.iter().all(|&d| d <= LSB_TOLERANCE) {
                    continue;
                }
                let level = if other.zmap[p] != cpu.zmap[p] {
                    Some(0)
                } else {
                    self.coarser_near_tie(self.canvas_pixel(p), &mut proven)
                };
                match level {
                    Some(level) => explained[level as usize] += 1,
                    None => over_bound.extend(
                        deltas
                            .into_iter()
                            .filter(|&d| d > LSB_TOLERANCE)
                            .map(|d| ((p % width, p / width), d)),
                    ),
                }
            }
            println!("pixels over {LSB_TOLERANCE} LSB explained per level: {explained:?}");
            over_bound
        }
    }

    /// Fails unless the reconstructed scores pick exactly the CPU run's
    /// Z-Map, so the near-tie proof reads the scores that run selected on.
    fn assert_scores_reproduce(cpu_zmap: &[u16], proof: &NearTieProof) {
        let mismatched = (0..cpu_zmap.len())
            .filter(|&p| {
                let q = proof.canvas_pixel(p);
                proof.winner_with(0, q, 0, proof.scores[0][0][q]) != cpu_zmap[p] as usize
            })
            .count();
        assert_eq!(
            mismatched, 0,
            "the reconstructed level-0 scores must select the CPU run's own Z-Map"
        );
    }

    #[test]
    fn zmap_parity_rejects_a_disagreement_no_near_tie_explains() {
        let cpu = run_laplacian("zmap_parity_rejects", 2, None, false);
        let proof = NearTieProof::for_run(&cpu.held);
        assert_scores_reproduce(&cpu.zmap, &proof);

        // The pixel whose winner leads the other frame by the widest margin,
        // relabelled to that other frame with the composite left untouched.
        let (p, loser) = (0..cpu.zmap.len())
            .map(|p| {
                let q = proof.canvas_pixel(p);
                let winner = cpu.zmap[p] as usize;
                let loser = 1 - winner;
                (
                    p,
                    loser,
                    proof.scores[0][winner][q] - proof.scores[0][loser][q],
                )
            })
            .max_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(p, loser, _)| (p, loser))
            .unwrap();
        let mut relabelled = cpu.zmap.clone();
        relabelled[p] = loser as u16;

        let (differing, unproven) = proof.unproven_disagreements(&relabelled, &cpu.zmap);
        assert_eq!(differing, 1);
        assert_eq!(unproven, [p], "a wide-margin relabel is no near-tie");
    }

    #[test]
    fn composite_parity_rejects_a_delta_no_coarser_near_tie_explains() {
        let cpu = run_laplacian("composite_parity_rejects", 2, None, false);
        let proof = NearTieProof::for_run(&cpu.held);
        assert_scores_reproduce(&cpu.zmap, &proof);

        // The pixel whose narrowest winner-over-loser margin across the
        // coarser pixels reaching it is the widest, its composite moved by
        // 10 LSB with the Z-Map left untouched.
        let margin = |p: usize| {
            let q = proof.canvas_pixel(p);
            (1..proof.levels)
                .flat_map(|level| {
                    let scores = &proof.scores[level as usize];
                    proof
                        .reaching_pixels(q, level)
                        .into_iter()
                        .map(move |m| (scores[0][m] - scores[1][m]).abs())
                })
                .fold(f32::INFINITY, f32::min)
        };
        let p = (0..cpu.zmap.len())
            .map(|p| (p, margin(p)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| p)
            .unwrap();
        let spp = cpu.composite.len() / cpu.zmap.len();
        let mut shifted = cpu.clone();
        for sample in &mut shifted.composite[p * spp..(p + 1) * spp] {
            *sample = if *sample > 32768 {
                *sample - 10
            } else {
                *sample + 10
            };
        }

        let width = cpu.held.width as usize;
        let expected: Vec<_> = (0..spp).map(|_| ((p % width, p / width), 10)).collect();
        assert_eq!(
            proof.unexplained_composite(&shifted, &cpu),
            expected,
            "a composite delta where every level has a wide margin is no near-tie"
        );
    }

    #[test]
    fn composite_parity_exempts_a_delta_a_neighbouring_coarse_near_tie_reaches() {
        let cpu = run_laplacian("composite_parity_reach", 2, None, false);
        let proof = NearTieProof::for_run(&cpu.held);
        assert_scores_reproduce(&cpu.zmap, &proof);

        // The narrowest-margin level-1 pixel whose whole footprint lies in
        // the Z-Map, and the footprint pixel outside its own block whose
        // own coarser pixels have the widest narrowest margin.
        let (width, crop) = (proof.width as usize, proof.crop);
        let zmap_pixel = |q: usize| {
            let (x, y) = (q % width, q / width);
            let inside = (crop.x as usize..(crop.x + crop.width) as usize).contains(&x)
                && (crop.y as usize..(crop.y + crop.height) as usize).contains(&y);
            inside.then(|| (y - crop.y as usize) * crop.width as usize + x - crop.x as usize)
        };
        let footprint = |origin: usize| -> Vec<usize> {
            let (x, y) = ((origin % width) as i64, (origin / width) as i64);
            (-2..=2)
                .flat_map(|dy| (-2..=2).map(move |dx| (dx, dy)))
                .map(|(dx, dy)| {
                    let (x, y) = (x + dx, y + dy);
                    (x >= 0 && y >= 0 && (x as usize) < width)
                        .then(|| y as usize * width + x as usize)
                })
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default()
        };
        let tie = (0..proof.scores[1][0].len())
            .filter(|&m| {
                let q = footprint(proof.canvas_origin(m, 1));
                q.len() == 25 && q.iter().all(|&q| zmap_pixel(q).is_some())
            })
            .min_by(|&a, &b| proof.top_two(1, a).2.total_cmp(&proof.top_two(1, b).2))
            .unwrap();
        let origin = proof.canvas_origin(tie, 1);
        let (winner, runner_up, _) = proof.top_two(1, tie);
        assert!(
            proof.reachable_near_tie(1, origin, winner, runner_up),
            "the narrowest level-1 margin must be a reachable near-tie"
        );
        let own_margin = |q: usize| {
            (1..proof.levels)
                .map(|level| proof.top_two(level, proof.level_pixel(q, level)).2)
                .fold(f32::INFINITY, f32::min)
        };
        let q = footprint(origin)
            .into_iter()
            .filter(|&q| proof.level_pixel(q, 1) != tie)
            .max_by(|&a, &b| own_margin(a).total_cmp(&own_margin(b)))
            .unwrap();
        let p = zmap_pixel(q).unwrap();

        let spp = cpu.composite.len() / cpu.zmap.len();
        let mut shifted = cpu.clone();
        for sample in &mut shifted.composite[p * spp..(p + 1) * spp] {
            *sample = if *sample > 32768 {
                *sample - 10
            } else {
                *sample + 10
            };
        }
        let unexplained = proof.unexplained_composite(&shifted, &cpu);
        assert!(
            unexplained.is_empty(),
            "a composite delta inside a coarser near-tie's collapse footprint is explained, \
             got {unexplained:?}"
        );
    }

    fn assert_gpu_matches_cpu(name: &str, frame_count: usize, excluded: Option<usize>) {
        let gpu = run_laplacian(&format!("{name}_gpu"), frame_count, excluded, true);
        let cpu = run_laplacian(&format!("{name}_cpu"), frame_count, excluded, false);
        assert_eq!(gpu.held.sources.len(), cpu.held.sources.len());
        assert_eq!(
            (gpu.held.width, gpu.held.height),
            (cpu.held.width, cpu.held.height),
            "both runs must fuse the same canvas"
        );

        let proof = NearTieProof::for_run(&cpu.held);
        assert_scores_reproduce(&cpu.zmap, &proof);
        let (differing, unproven) = proof.unproven_disagreements(&gpu.zmap, &cpu.zmap);
        assert!(
            unproven.is_empty(),
            "{} of {differing} differing Z-Map indices are no ±1 LSB-reachable near-tie: \
             pixels {unproven:?}",
            unproven.len()
        );

        // A flipped near-tie composites a different, equally valid frame whose
        // value can differ by hundreds of LSB, so pixels a proof covers are
        // exempt from the bound; every other pixel must stay within it.
        let over_bound = proof.unexplained_composite(&gpu, &cpu);
        println!(
            "gpu fusion parity ({name}): {differing} differing Z-Map index(es), all near-ties"
        );
        assert!(
            over_bound.is_empty(),
            "GPU/CPU composite must stay within {LSB_TOLERANCE} LSB wherever no near-tie is \
             proven; {} sample(s) over, ((x, y), delta): {:?}",
            over_bound.len(),
            &over_bound[..over_bound.len().min(40)]
        );
    }

    #[test]
    fn gpu_fusion_matches_cpu_on_the_full_mantis_512_stack() {
        assert_gpu_matches_cpu("gpu_fusion_full", 8, None);
    }

    #[test]
    fn gpu_fusion_matches_cpu_with_one_frame_excluded() {
        assert_gpu_matches_cpu("gpu_fusion_excluded", 8, Some(3));
    }
}
