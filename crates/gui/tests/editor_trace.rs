// SPDX-License-Identifier: GPL-3.0-or-later

//! The editor spans a `trace` build records: each brush segment from the UI
//! thread through the worker to the published, rendered frame, plus release,
//! pan, zoom and source switch. Each test re-runs itself in a child process,
//! because the trace subscriber, the timing accounting and the Slint platform
//! are each set once per process; the parent then reads the child's trace.

#![cfg(feature = "trace")]

mod common;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use focusforge_core::tile::Rect;
use focusforge_diagnostics::Options;
use focusforge_gpu::blend::Blender;
use focusforge_gpu::device::SharedDevice;
use focusforge_gui::AppWindow;
use focusforge_gui::bridge;
use focusforge_gui::viewport::PaneRect;
use serde_json::Value;
use slint::ComponentHandle as _;
use slint::platform::PointerEventButton;

const CHILD: &str = "FOCUSFORGE_EDITOR_TRACE_CHILD";
const IMAGE_SIDE: u32 = 64;
const SAMPLES_PER_PIXEL: usize = 3;
const STEP_INTERVAL: Duration = Duration::from_millis(40);
const TAIL_TICKS: u32 = 10;
/// One wheel notch: two of the controller's 60 px detents.
const NOTCH: f32 = 120.0;

/// One scripted editor session.
#[derive(Clone, Copy)]
struct Session<'a> {
    /// The device the canvas publishes on; `None` leaves every publish out.
    device: Option<&'a SharedDevice>,
    /// Whether strokes blend on the device rather than the CPU.
    device_blend: bool,
    /// Motion steps of the one drag stroke.
    segments: usize,
    /// Whether the session ends with a pan, five zoom notches and three
    /// source steps after the stroke.
    actions: bool,
}

type Step = Box<dyn Fn(&AppWindow)>;

fn pointer(button: PointerEventButton, kind: i32, x: f32, y: f32) -> Step {
    Box::new(move |app| app.invoke_canvas_pointer(button, kind, x, y, false, false, false))
}

fn dragged(x: f32, y: f32) -> Step {
    Box::new(move |app| app.invoke_canvas_dragged(x, y))
}

fn wheel(delta: f32, x: f32, y: f32, ctrl: bool) -> Step {
    Box::new(move |app| app.invoke_canvas_scrolled(0.0, delta, x, y, ctrl, false, false))
}

/// Runs `session` in a fresh window and returns the edited image's samples
/// once every region has been installed.
fn run_session(session: Session) -> Vec<f32> {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        "editor_trace_{}_{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    );

    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let pane = |x| PaneRect {
        x,
        y: 0.0,
        width: 512.0,
        height: 512.0,
        scale_factor: 1.0,
    };
    let (left, right) = (pane(0.0), pane(520.0));

    let mut opened = common::scratch_controller(&name);
    common::open_fixture_tab(&mut opened, &name, "mantis-64", 3);
    common::fuse(&mut opened);
    opened.set_pane_geometry(left, right);
    if let (Some(shared), true) = (session.device, session.device_blend) {
        opened.attach_blender(Blender::new(shared.device(), shared.queue()));
    }
    common::enter_editor(&mut opened);
    opened.select_frame(1);
    opened.take_session_started();
    opened.take_render_started();
    opened.take_origin_started();
    opened.take_thumbnails_started();
    let controller = Arc::new(Mutex::new(opened));
    let canvas = common::empty_canvas();
    if let Some(shared) = session.device {
        canvas
            .borrow_mut()
            .attach_device(shared.device().clone(), shared.queue().clone());
    }
    struct Detach(Rc<RefCell<bridge::Canvas>>);
    impl Drop for Detach {
        fn drop(&mut self) {
            self.0.borrow_mut().detach_device();
        }
    }
    let _detach = Detach(canvas.clone());
    let _installed = bridge::install(
        &app,
        controller.clone(),
        canvas.clone(),
        Rc::new(RefCell::new(None)),
    );

    // Set by a step after which the script waits for the exact pane reads.
    let hold = Rc::new(Cell::new(false));
    let (x0, y0) = (right.x + 200.0, 200.0);
    let mut script: Vec<Step> = vec![
        pointer(PointerEventButton::Left, 2, x0, y0),
        pointer(PointerEventButton::Left, 0, x0, y0),
    ];
    let (mut x, mut y) = (x0, y0);
    for step in 0..session.segments {
        x += 12.0;
        y += if step % 2 == 0 { 5.0 } else { -3.0 };
        script.push(dragged(x, y));
    }
    script.push(pointer(PointerEventButton::Left, 1, x, y));
    if session.actions {
        let centre = (right.x + 256.0, 256.0);
        script.push(pointer(PointerEventButton::Middle, 0, centre.0, centre.1));
        for step in 1..=3 {
            script.push(dragged(centre.0 + 6.0 * step as f32, centre.1));
        }
        script.push(pointer(
            PointerEventButton::Middle,
            1,
            centre.0 + 18.0,
            centre.1,
        ));
        for notch in 0..5 {
            let delta = if notch < 3 { NOTCH } else { -NOTCH };
            let step = wheel(delta, centre.0, centre.1, false);
            // The last zoom notch holds the script until the pane reads the
            // zoom queued are published, so no zoom publish lands among the
            // source steps.
            script.push(if notch == 4 {
                let hold = hold.clone();
                Box::new(move |app| {
                    step(app);
                    hold.set(true);
                })
            } else {
                step
            });
        }
        // Each Ctrl+wheel notch must land on another frame than the one
        // before it, and holds the script until its pane read is published.
        let selected = Rc::new(RefCell::new(
            controller.lock().unwrap().selected_frame_path(),
        ));
        for notch in 0..3 {
            let delta = if notch % 2 == 0 { -NOTCH } else { NOTCH };
            let step = wheel(delta, centre.0, centre.1, true);
            let (controller, selected, hold) = (controller.clone(), selected.clone(), hold.clone());
            script.push(Box::new(move |app| {
                step(app);
                let now = controller.lock().unwrap().selected_frame_path();
                let before = selected.replace(now.clone());
                assert_ne!(
                    before, now,
                    "Ctrl+wheel notch {notch} selects another frame"
                );
                hold.set(true);
            }));
        }
    }

    let script = RefCell::new(VecDeque::from(script));
    let tail = Cell::new(0u32);
    let weak = app.as_weak();
    let driven_canvas = canvas.clone();
    let driver = slint::Timer::default();
    driver.start(slint::TimerMode::Repeated, STEP_INTERVAL, move || {
        let app = weak.upgrade().expect("the window outlives its script");
        if hold.get() {
            if !driven_canvas.borrow().exact_idle() {
                return;
            }
            hold.set(false);
        }
        let next = script.borrow_mut().pop_front();
        match next {
            Some(step) => step(&app),
            None => {
                tail.set(tail.get() + 1);
                if tail.get() >= TAIL_TICKS {
                    slint::quit_event_loop().expect("the loop is running");
                }
            }
        }
    });
    let frames = slint::Timer::default();
    frames.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        bridge::record_frame_rendered,
    );
    let watchdog = slint::Timer::default();
    watchdog.start(
        slint::TimerMode::SingleShot,
        Duration::from_secs(120),
        || panic!("the scripted session did not finish"),
    );
    slint::run_event_loop_until_quit().expect("the event loop runs");
    drop((driver, frames, watchdog));

    let mut controller = controller.lock().unwrap();
    common::settle_region_renders(&mut controller);
    let mut samples = vec![-1.0f32; (IMAGE_SIDE * IMAGE_SIDE) as usize * SAMPLES_PER_PIXEL];
    controller
        .edited_view()
        .expect("the editor holds resident tiles")
        .tiles
        .overlay_into(
            Rect {
                x: 0,
                y: 0,
                width: IMAGE_SIDE,
                height: IMAGE_SIDE,
            },
            &mut samples,
        );
    assert!(
        samples.iter().any(|sample| *sample >= 0.0),
        "the stroke painted resident samples"
    );
    samples
}

/// Re-runs `test` in a child process, with the trace file set when `trace`
/// names one and `env` added, and returns whether it succeeded. The child
/// initialises diagnostics itself, with native-crash capture off so it does
/// not re-spawn this binary as its own monitor.
fn run_child(test: &str, trace: Option<&Path>, env: &[(&str, &str)]) -> bool {
    let mut command = Command::new(std::env::current_exe().expect("the test binary has a path"));
    command
        .args(["--exact", test, "--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .env("FF_CRASH_MONITOR", "off")
        .envs(env.iter().copied())
        .env_remove("FOCUSFORGE_TRACE_FILE");
    if let Some(path) = trace {
        command.env("FOCUSFORGE_TRACE_FILE", path);
    }
    command.status().expect("the child test starts").success()
}

fn trace_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("the target temp directory is writable");
    let path = dir.join(format!("{name}.json"));
    let _ = std::fs::remove_file(&path);
    path
}

fn diagnostics_options() -> Options {
    Options {
        data_dir: None,
        verbosity: -1,
    }
}

/// One `gui_*` or `gpu_*` span of the trace.
#[derive(Debug, Clone)]
struct Span {
    kind: String,
    tid: u64,
    start: f64,
    end: f64,
    stroke: u64,
    segment: u64,
    area: u64,
    generation: u64,
}

fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim_matches('"').parse().ok())
        })
        .unwrap_or(0)
}

/// The closed span records of the trace file, paired per thread.
fn read_spans(path: &Path) -> Vec<Span> {
    let text = std::fs::read_to_string(path).expect("the child wrote its trace");
    let events: Vec<Value> = serde_json::from_str(&text).expect("the trace is one closed array");
    let mut open: std::collections::HashMap<u64, Vec<Option<Span>>> = Default::default();
    let mut spans = Vec::new();
    for event in &events {
        let tid = event["tid"].as_u64().unwrap_or(0);
        match event["ph"].as_str() {
            Some("B") => {
                let args = &event["args"];
                let span = (event["name"] == "timing").then(|| Span {
                    kind: args["kind"]
                        .as_str()
                        .unwrap_or("")
                        .trim_matches('"')
                        .to_owned(),
                    tid,
                    start: event["ts"].as_f64().unwrap_or(0.0),
                    end: 0.0,
                    stroke: number(&args["stroke"]),
                    segment: number(&args["segment"]),
                    area: number(&args["area"]),
                    generation: number(&args["generation"]),
                });
                open.entry(tid).or_default().push(span);
            }
            Some("E") => {
                if let Some(Some(mut span)) = open.entry(tid).or_default().pop() {
                    span.end = event["ts"].as_f64().unwrap_or(span.start);
                    spans.push(span);
                }
            }
            _ => {}
        }
    }
    spans.sort_by(|a, b| a.start.total_cmp(&b.start));
    spans
}

fn of<'a>(spans: &'a [Span], kind: &'a str) -> impl Iterator<Item = &'a Span> + 'a {
    spans.iter().filter(move |span| span.kind == kind)
}

fn inside(outer: &Span, inner: &Span) -> bool {
    outer.tid == inner.tid && inner.start >= outer.start && inner.end <= outer.end
}

fn contains(spans: &[Span], outer: &Span, kind: &str) -> bool {
    of(spans, kind).any(|inner| inside(outer, inner))
}

fn edit_of<'a>(spans: &'a [Span], kind: &'a str, stroke: u64, segment: u64) -> Vec<&'a Span> {
    of(spans, kind)
        .filter(|span| span.stroke == stroke && span.segment == segment)
        .collect()
}

#[test]
fn live_stroke_records_every_stage_per_segment() {
    const CASES: [(bool, usize); 4] = [(true, 3), (true, 7), (false, 3), (false, 7)];
    let test = "live_stroke_records_every_stage_per_segment";
    if std::env::var_os(CHILD).is_none() {
        if focusforge_gpu::device::shared_device_or_skip().is_none() {
            return;
        }
        let path = trace_path(test);
        assert!(run_child(test, Some(&path), &[]), "the traced child passes");
        check_segment_chains(&path, &CASES);
        return;
    }

    let shared =
        focusforge_gpu::device::shared_device_or_skip().expect("the parent saw a graphics adapter");
    i_slint_backend_testing::init_integration_test_with_system_time();
    let session = |(device_blend, segments)| Session {
        device: Some(shared),
        device_blend,
        segments,
        actions: false,
    };
    let untraced: Vec<_> = CASES
        .iter()
        .map(|case| run_session(session(*case)))
        .collect();
    let diagnostics = focusforge_diagnostics::init(&diagnostics_options());
    assert!(focusforge_core::timing::timing_enabled());
    for (case, expected) in CASES.iter().zip(&untraced) {
        assert!(
            run_session(session(*case)) == *expected,
            "a traced stroke leaves the samples of the untraced one: {case:?}"
        );
    }
    drop(diagnostics);
}

fn check_segment_chains(path: &Path, cases: &[(bool, usize)]) {
    let spans = read_spans(path);
    let strokes: BTreeSet<u64> = of(&spans, "gui_stroke_segment")
        .map(|span| span.stroke)
        .collect();
    assert_eq!(strokes.len(), cases.len(), "one stroke per traced case");
    for (stroke, (device_blend, segments)) in strokes.into_iter().zip(cases) {
        for segment in 1..=*segments as u64 {
            let at = format!("stroke {stroke} segment {segment}");
            let [queued] = edit_of(&spans, "gui_stroke_segment", stroke, segment)[..] else {
                panic!("{at}: one gui_stroke_segment");
            };
            let [job] = edit_of(&spans, "gui_region_job", stroke, segment)[..] else {
                panic!("{at}: one gui_region_job");
            };
            assert!(queued.area > 0 && job.area == queued.area, "{at}: the area");
            let blends: Vec<&Span> = of(&spans, "gui_region_blend")
                .filter(|blend| inside(job, blend))
                .collect();
            assert!(!blends.is_empty(), "{at}: the job holds a blend");
            let waits = blends
                .iter()
                .filter(|blend| contains(&spans, blend, "gpu_readback_wait"))
                .count();
            if *device_blend {
                assert_eq!(
                    waits,
                    blends.len(),
                    "{at}: each device blend waits on a readback"
                );
            } else {
                assert_eq!(waits, 0, "{at}: the CPU blend reads nothing back");
            }
            let park = of(&spans, "gui_region_park")
                .find(|park| park.tid == job.tid && park.start >= job.end)
                .unwrap_or_else(|| panic!("{at}: the worker parks the batch"));
            let [install] = edit_of(&spans, "gui_region_install", stroke, segment)[..] else {
                panic!("{at}: one gui_region_install");
            };
            assert_eq!(install.area, job.area, "{at}: the install area");
            let settle = of(&spans, "gui_region_render_settle")
                .find(|settle| settle.start >= park.start && inside(settle, install))
                .unwrap_or_else(|| panic!("{at}: a settle holds the install"));
            let shown = of(&spans, "gui_composite_shown")
                .find(|shown| shown.start >= settle.start && shown.generation >= install.generation)
                .unwrap_or_else(|| panic!("{at}: the composite shows the install"));
            let publish = of(&spans, "gui_canvas_publish")
                .filter(|publish| publish.tid == shown.tid && publish.end <= shown.start)
                .last()
                .unwrap_or_else(|| panic!("{at}: a publish precedes the marker"));
            for stage in [
                "gui_publish_copy",
                "gui_publish_overlay_lookup",
                "gui_publish_upload",
            ] {
                assert!(
                    contains(&spans, publish, stage),
                    "{at}: the publish holds {stage}"
                );
            }
            assert!(
                of(&spans, "gui_frame_rendered").any(|frame| frame.start >= shown.start),
                "{at}: a frame is rendered after the composite shows it"
            );
        }
    }
}

#[test]
fn editor_actions_record_settle_pan_zoom_and_source_switch() {
    let test = "editor_actions_record_settle_pan_zoom_and_source_switch";
    if std::env::var_os(CHILD).is_none() {
        if focusforge_gpu::device::shared_device_or_skip().is_none() {
            return;
        }
        let path = trace_path(test);
        assert!(run_child(test, Some(&path), &[]), "the traced child passes");
        check_actions(&path);
        return;
    }

    let shared =
        focusforge_gpu::device::shared_device_or_skip().expect("the parent saw a graphics adapter");
    i_slint_backend_testing::init_integration_test_with_system_time();
    let diagnostics = focusforge_diagnostics::init(&diagnostics_options());
    run_session(Session {
        device: Some(shared),
        device_blend: true,
        segments: 3,
        actions: true,
    });
    drop(diagnostics);
}

fn check_actions(path: &Path) {
    let spans = read_spans(path);
    let [release] = of(&spans, "gui_stroke_release").collect::<Vec<_>>()[..] else {
        panic!("one gui_stroke_release");
    };
    assert_eq!(release.segment, 3, "the release counts the segments taken");
    let installs: Vec<&Span> = of(&spans, "gui_region_install")
        .filter(|install| install.stroke == release.stroke && install.segment == 0)
        .collect();
    assert!(!installs.is_empty(), "the release re-renders its tiles");
    assert!(
        installs
            .iter()
            .all(|install| install.start >= release.start)
    );
    let last = installs
        .iter()
        .map(|install| install.generation)
        .max()
        .unwrap();
    let shown = of(&spans, "gui_composite_shown")
        .find(|shown| shown.generation >= last)
        .expect("the composite reaches the final generation");
    assert!(of(&spans, "gui_frame_rendered").any(|frame| frame.start >= shown.start));

    let pans = of(&spans, "gui_action_pan").count();
    let zooms = of(&spans, "gui_action_zoom").count();
    assert!(pans >= 1, "a middle-button drag records its pan");
    assert_eq!(zooms, 5, "one zoom record per wheel notch");
    for kind in [
        "gui_action_pan",
        "gui_action_zoom",
        "gui_action_source_step",
    ] {
        for action in of(&spans, kind) {
            assert!(
                of(&spans, "gui_frame_rendered").any(|frame| frame.start >= action.start),
                "{kind}: a frame is rendered after it"
            );
        }
    }
    let steps: Vec<&Span> = of(&spans, "gui_action_source_step").collect();
    assert_eq!(steps.len(), 3, "one record per Ctrl+wheel notch");
    // The script holds each step until the pane reads before it are
    // published, so a step's window holds its own publish alone: a zoom's
    // late publish would make the first window hold two.
    for (at, step) in steps.iter().enumerate() {
        let until = steps.get(at + 1).map_or(f64::INFINITY, |next| next.start);
        let publishes: Vec<&Span> = of(&spans, "gui_frame_pane_publish")
            .filter(|publish| publish.start >= step.start && publish.start < until)
            .collect();
        let [publish] = publishes[..] else {
            panic!(
                "source step {at}: one frame pane publish, found {}",
                publishes.len()
            );
        };
        assert!(
            contains(&spans, publish, "gui_publish_upload"),
            "source step {at} uploads the new frame"
        );
    }
}

/// Counts the measurement spans (`timing`, `timing_level`, ...) opened in the
/// process, on any thread, and keeps none. The fuse that sets a session up
/// opens spans of its own, which are not measurements.
struct SpanCounter;

static SPANS_OPENED: AtomicUsize = AtomicUsize::new(0);

impl tracing::Subscriber for SpanCounter {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        if attributes.metadata().name().starts_with("timing") {
            SPANS_OPENED.fetch_add(1, Ordering::Relaxed);
        }
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn live_stroke_records_nothing_without_a_trace_file() {
    const COUNT_SPANS: &str = "FOCUSFORGE_EDITOR_TRACE_COUNT_SPANS";
    let test = "live_stroke_records_nothing_without_a_trace_file";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            run_child(test, None, &[]),
            "the child with diagnostics and no trace file passes"
        );
        assert!(
            run_child(test, None, &[(COUNT_SPANS, "1")]),
            "the child counting spans passes"
        );
        return;
    }

    i_slint_backend_testing::init_integration_test_with_system_time();
    let stroke = || {
        run_session(Session {
            device: None,
            device_blend: false,
            segments: 5,
            actions: false,
        })
    };
    if std::env::var_os(COUNT_SPANS).is_some() {
        tracing::subscriber::set_global_default(SpanCounter)
            .expect("no subscriber is installed in this process yet");
        stroke();
        assert_eq!(
            SPANS_OPENED.load(Ordering::Relaxed),
            0,
            "a stroke and its release open no span without a trace file"
        );
        return;
    }
    let _diagnostics = focusforge_diagnostics::init(&diagnostics_options());
    assert!(!focusforge_core::timing::timing_enabled());
    assert!(focusforge_core::timing::timing_snapshot().is_none());
    stroke();
    assert!(
        focusforge_core::timing::timing_snapshot().is_none(),
        "a stroke and its release leave accounting off"
    );
}
