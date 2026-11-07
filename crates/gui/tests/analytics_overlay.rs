// SPDX-License-Identifier: GPL-3.0-or-later

//! The Analytics overlay: its rows against a direct call of core's sharp-area
//! count, the count's off-thread life cycle driven through `AppController`,
//! and its button and open/close rules driven through a headless window.

mod common;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use focusforge_core::host;
use focusforge_core::io::open_frame_set;
use focusforge_core::params::ParamValues;
use focusforge_core::sharp_areas::{count_sharp_areas, count_sharp_areas_reporting};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, NoControl};
use focusforge_core::zmap::source_color;
use focusforge_gui::analytics::{AnalyticsRow, AnalyticsView, CountPriority, RowValue};
use focusforge_gui::input_map::Modifiers;
use focusforge_gui::{AppController, AppWindow, ChromeKey, ShellMode};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model};

type SharedController = Arc<Mutex<AppController>>;

/// Long enough that the worker is still asleep when the test acts, short
/// enough that a test which joins it does not wait long.
const HOLD: Duration = Duration::from_millis(1500);

struct Rig {
    app: AppWindow,
    controller: SharedController,
    _installed: focusforge_gui::bridge::Installed,
}

fn rig_with(controller: AppController) -> Rig {
    common::init_window_backend();
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    let controller = Arc::new(Mutex::new(controller));
    let installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        Rc::new(RefCell::new(None)),
    );
    Rig {
        app,
        controller,
        _installed: installed,
    }
}

impl Rig {
    fn event(&self, event: WindowEvent) {
        self.app.window().dispatch_event(event);
        slint::platform::update_timers_and_animations();
    }

    /// Pushes the controller's overlay state to the window, as the bridge
    /// does on the event loop this headless window does not run.
    fn pump(&self) {
        focusforge_gui::bridge::push_projects(&self.app, &self.controller);
        focusforge_gui::bridge::push_overlays(&self.app, &self.controller);
        slint::platform::update_timers_and_animations();
    }

    fn click(&self, handle: &ElementHandle) {
        let (pos, size) = (handle.absolute_position(), handle.size());
        let position =
            slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
        self.event(WindowEvent::PointerMoved { position });
        self.event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        self.event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        self.pump();
    }

    fn button(&self, label: &str) -> ElementHandle {
        let mut found: Vec<_> = ElementQuery::from_root(&self.app)
            .match_accessible_role(AccessibleRole::Button)
            .find_all()
            .into_iter()
            .filter(|b| b.accessible_label().as_deref() == Some(label))
            .collect();
        assert_eq!(found.len(), 1, "exactly one button named {label:?}");
        found.remove(0)
    }

    fn press(&self, key: slint::SharedString) {
        self.event(WindowEvent::KeyPressed { text: key.clone() });
        self.event(WindowEvent::KeyReleased { text: key });
        self.pump();
    }

    fn escape(&self) {
        self.press(Key::Escape.into());
    }

    /// `key` pressed and released while Control is held.
    fn chord(&self, key: &str) {
        let control: slint::SharedString = Key::Control.into();
        self.event(WindowEvent::KeyPressed {
            text: control.clone(),
        });
        self.press(key.into());
        self.event(WindowEvent::KeyReleased { text: control });
        self.pump();
    }

    fn open(&self) -> bool {
        self.controller.lock().unwrap().analytics_open()
    }
}

/// `count_sharp_areas` over `paths` with the command line's defaults.
fn core_counts(paths: &[PathBuf]) -> Vec<u32> {
    let set = open_frame_set(paths, HostBudget::host_derived(&host::snapshot()))
        .expect("the fixture frames open");
    let reader = set.reader.as_ref();
    count_sharp_areas(
        reader,
        reader.len(),
        reader.dimensions(),
        reader.frame(0).samples_per_pixel,
        &ParamValues::new(),
        DEFAULT_TILE_SIZE,
        &NoControl,
    )
    .expect("the fixture frames count")
    .counts
}

fn fixture_frames(count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|i| common::fixture_frame("mantis-512", i))
        .collect()
}

/// Opens the overlay, waits out the worker and applies its result.
fn count_now(controller: &mut AppController) {
    controller
        .toggle_analytics()
        .expect("opening over a stack starts a count")
        .join()
        .expect("the count worker did not panic");
    controller.settle_analytics();
}

fn rows_of(controller: &AppController) -> Vec<AnalyticsRow> {
    match controller.analytics_view() {
        AnalyticsView::Rows(rows) => rows.clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The rows of a counting view, which holds the stack's rows before the
/// count is done.
fn counting_rows_of(controller: &AppController) -> Vec<AnalyticsRow> {
    match controller.analytics_view() {
        AnalyticsView::Counting(rows) => rows.clone(),
        other => panic!("expected the counting view, got {other:?}"),
    }
}

/// How many `FFSpinner`s the Analytics overlay draws.
fn spinners(rig: &Rig) -> usize {
    ElementHandle::find_by_element_id(&rig.app, "AppWindow::analytics-overlay")
        .next()
        .expect("the window has the Analytics overlay")
        .query_descendants()
        .match_type_name("FFSpinner")
        .find_all()
        .len()
}

/// How many elements of the window carry exactly `text` as their label.
fn labelled(rig: &Rig, text: &str) -> usize {
    ElementHandle::find_by_accessible_label(&rig.app, text).count()
}

/// The value state the window holds for each row.
fn row_states(rig: &Rig) -> Vec<String> {
    rig.app
        .get_analytics_rows()
        .iter()
        .map(|row| row.value_state.to_string())
        .collect()
}

/// Asserts `controller` shows `count` rows no count has reported for.
fn assert_pending(controller: &AppController, count: usize) {
    let rows = counting_rows_of(controller);
    assert_eq!(rows.len(), count);
    assert!(
        rows.iter()
            .all(|r| r.value == RowValue::Pending && r.count == 0 && r.fraction == 0.0),
        "{rows:?}"
    );
}

/// A tab over `count` frames named `00.tif`.. that are all copies of
/// `mantis-64` frame 0.
fn controller_with_identical_frames(name: &str, count: usize) -> AppController {
    let root = common::tree(&format!("{name}_frames"));
    for i in 0..count {
        std::fs::copy(
            common::fixture_frame("mantis-64", 0),
            root.join(format!("{i:02}.tif")),
        )
        .expect("fixture frame must copy");
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        focusforge_gui::OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

#[test]
fn rows_match_the_core_counts() {
    for count in [8, 3] {
        let mut controller = common::controller_with_frames_from(
            "mantis-512",
            &format!("analytics_rows_{count}"),
            count,
        );
        count_now(&mut controller);
        let rows = rows_of(&controller);
        let expected = core_counts(&fixture_frames(count));
        let largest = *expected.iter().max().unwrap();
        if count == 8 {
            assert!(largest > 0, "the fixture gives the bars something to show");
        }

        assert_eq!(rows.len(), count);
        for (i, row) in rows.iter().enumerate() {
            let at = format!("row {i} of {count}");
            assert_eq!(row.value, RowValue::Final, "{at}: value");
            assert_eq!(row.count, expected[i], "{at}: count");
            assert_eq!(row.name, format!("{i:02}.tif"), "{at}: name");
            assert_eq!(row.chip, source_color(i as u16, count as u32), "{at}: chip");
            assert_eq!(
                row.fraction,
                expected[i] as f32 / largest as f32,
                "{at}: bar"
            );
        }

        // The window gets the same rows.
        let rig = rig_with(controller);
        rig.pump();
        assert_eq!(rig.app.get_analytics_state(), "rows");
        let model = rig.app.get_analytics_rows();
        assert_eq!(model.row_count(), count);
        for (i, row) in rows.iter().enumerate() {
            let shown = model.row_data(i).unwrap();
            assert_eq!(shown.name, row.name.as_str());
            assert_eq!(shown.count, row.count as i32);
            assert_eq!(shown.fraction, row.fraction);
            assert_eq!(
                shown.chip,
                slint::Color::from_rgb_u8(row.chip[0], row.chip[1], row.chip[2])
            );
        }
    }
}

#[test]
fn identical_frames_leave_every_bar_empty() {
    let mut controller = controller_with_identical_frames("analytics_identical", 3);
    count_now(&mut controller);
    let rows = rows_of(&controller);
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().all(|r| r.count == 0 && r.fraction == 0.0),
        "{rows:?}"
    );
}

#[test]
fn no_position_no_mm_column() {
    let mut controller = common::controller_with_frames("analytics_mm", 3);
    count_now(&mut controller);
    assert!(rows_of(&controller).iter().all(|r| r.mm_text.is_empty()));

    let rig = rig_with(controller);
    rig.pump();
    let model = rig.app.get_analytics_rows();
    assert_eq!(model.row_count(), 3);
    assert!(model.iter().all(|row| row.mm_text.is_empty()));
}

#[test]
fn count_runs_off_the_ui_thread() {
    let mut controller = common::controller_with_frames("analytics_off_thread", 3);
    controller.set_analytics_hold(HOLD);
    let worker = controller
        .toggle_analytics()
        .expect("opening over a stack starts a count");

    // The call returned while the worker still sleeps: the rows wait.
    assert!(!worker.is_finished());
    assert!(controller.analytics_open());
    assert_pending(&controller, 3);
    controller.settle_analytics();
    assert_pending(&controller, 3);
    let rig = rig_with(controller);
    rig.pump();
    assert_eq!(rig.app.get_analytics_state(), "counting");
    assert_eq!(rig.app.get_analytics_rows().row_count(), 3);
    assert_eq!(row_states(&rig), ["pending"; 3]);
    assert_eq!(spinners(&rig), 3, "one spinner per pending row");

    worker.join().expect("the count worker did not panic");
    rig.controller.lock().unwrap().settle_analytics();
    rig.pump();
    assert_eq!(rig.app.get_analytics_state(), "rows");
    assert_eq!(rig.app.get_analytics_rows().row_count(), 3);
    assert_eq!(row_states(&rig), ["final"; 3]);
    assert_eq!(
        spinners(&rig),
        0,
        "no spinner is left when the count is done"
    );
}

#[test]
fn rows_show_while_the_count_loads() {
    let mut controller = common::controller_with_frames("analytics_loading_rows", 3);
    controller.set_analytics_hold(HOLD);
    let worker = controller.toggle_analytics().expect("a count started");

    let rows = counting_rows_of(&controller);
    assert_eq!(rows.len(), 3);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.name, format!("{i:02}.tif"), "row {i}");
        assert_eq!(row.chip, source_color(i as u16, 3), "row {i}");
        assert_eq!(row.value, RowValue::Pending, "row {i}");
    }
    assert_pending(&controller, 3);

    controller.close_analytics();
    worker.join().unwrap();
}

#[test]
fn partial_counts_grow_with_the_tiles() {
    const TILE_SIZE: u32 = 96;
    const FRAMES: usize = 4;
    let mut controller =
        common::controller_with_frames_from("mantis-512", "analytics_partial", FRAMES);
    controller.set_analytics_tile_size(TILE_SIZE);

    // The core's own reports, one per tile, are what the window must step through.
    let paths = fixture_frames(FRAMES);
    let set = open_frame_set(&paths, HostBudget::host_derived(&host::snapshot())).unwrap();
    let reader = set.reader.as_ref();
    let mut reports: Vec<Vec<u32>> = Vec::new();
    let done = count_sharp_areas_reporting(
        reader,
        reader.len(),
        reader.dimensions(),
        reader.frame(0).samples_per_pixel,
        &ParamValues::new(),
        TILE_SIZE,
        &NoControl,
        &mut |counts| reports.push(counts.to_vec()),
    )
    .unwrap();
    assert!(reports.len() > 1, "the tile size is below the frame");

    // The worker parks one snapshot per event and waits for the test to
    // settle it before reading on.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let rig = rig_with(controller);
    rig.controller
        .lock()
        .unwrap()
        .set_sink(Arc::new(move |event| {
            if matches!(event, focusforge_gui::ShellEvent::AnalyticsReady) {
                ready_tx.send(()).ok();
                go_rx.lock().unwrap().recv().ok();
            }
        }));
    let worker = rig
        .controller
        .lock()
        .unwrap()
        .toggle_analytics()
        .expect("a count started");

    for (i, expected) in reports.iter().enumerate() {
        ready_rx.recv().expect("the worker reports");
        let rows = {
            let mut controller = rig.controller.lock().unwrap();
            controller.settle_analytics();
            counting_rows_of(&controller)
        };
        rig.pump();
        assert_eq!(rig.app.get_analytics_state(), "counting", "report {i}");
        assert_eq!(row_states(&rig), ["partial"; FRAMES], "report {i}");
        assert_eq!(spinners(&rig), FRAMES, "report {i}: the spinners stay");
        let largest = expected.iter().copied().max().unwrap();
        assert_eq!(rows.len(), FRAMES);
        for (row, &count) in rows.iter().zip(expected) {
            assert_eq!(row.value, RowValue::Partial, "report {i}");
            assert_eq!(row.count, count, "report {i}");
            let fraction = if largest == 0 {
                0.0
            } else {
                count as f32 / largest as f32
            };
            assert_eq!(row.fraction, fraction, "report {i}");
        }
        go_tx.send(()).unwrap();
    }

    ready_rx.recv().expect("the worker finishes");
    let rows = {
        let mut controller = rig.controller.lock().unwrap();
        controller.settle_analytics();
        rows_of(&controller)
    };
    rig.pump();
    assert_eq!(row_states(&rig), ["final"; FRAMES]);
    assert_eq!(spinners(&rig), 0);
    assert!(rows.iter().all(|r| r.value == RowValue::Final));
    assert_eq!(
        rows.iter().map(|r| r.count).collect::<Vec<_>>(),
        done.counts
    );
    go_tx.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn unreadable_frame_shows_the_error() {
    let root = common::tree("analytics_unreadable_frames");
    for i in 0..3 {
        common::copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&root)),
        focusforge_gui::OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(common::tree("analytics_unreadable_scratch"));
    std::fs::remove_file(root.join("01.tif")).unwrap();

    count_now(&mut controller);
    let AnalyticsView::Failed(message) = controller.analytics_view().clone() else {
        panic!("expected a failure, got {:?}", controller.analytics_view());
    };
    assert!(message.contains("01.tif"), "{message}");

    let rig = rig_with(controller);
    rig.pump();
    assert_eq!(rig.app.get_analytics_state(), "failed");
    assert_eq!(rig.app.get_analytics_error(), message.as_str());
    assert_eq!(rig.app.get_analytics_rows().row_count(), 0);
    assert_eq!(labelled(&rig, "Counting failed"), 1);
    assert_eq!(
        labelled(&rig, &message),
        1,
        "the core message is the caption"
    );
    assert_eq!(spinners(&rig), 0);

    rig.app.invoke_analytics_toggled(); // the close x
    rig.pump();
    assert!(!rig.open(), "close x closes the failure state");
    assert!(!rig.app.get_analytics_open());
}

#[test]
fn one_overlay_at_a_time() {
    let mut controller = common::controller_with_frames("analytics_exclusive", 2);
    controller.set_analytics_hold(Duration::from_secs(30));

    controller.toggle_analytics();
    controller.toggle_calculator();
    assert!(!controller.analytics_open() && controller.calculator_open());
    controller.toggle_analytics();
    assert!(controller.analytics_open() && !controller.calculator_open());

    controller.toggle_projects_overlay();
    assert!(!controller.analytics_open() && controller.projects_overlay_open());
    controller.toggle_analytics();
    assert!(controller.analytics_open() && !controller.projects_overlay_open());

    controller.open_center();
    assert!(!controller.analytics_open() && controller.notification_center_open());
    controller.toggle_analytics();
    assert!(controller.analytics_open() && !controller.notification_center_open());

    // The window: the button opens and closes it, the calculator replaces it.
    let rig = rig_with(controller);
    rig.pump();
    let button = rig.button("Analytics");
    rig.click(&button);
    assert!(
        !rig.open() && !rig.app.get_analytics_open(),
        "the button closes it"
    );
    rig.click(&button);
    assert!(rig.open() && rig.app.get_analytics_open());

    rig.click(&rig.button("Step calculator"));
    assert!(!rig.open() && rig.controller.lock().unwrap().calculator_open());
    assert!(
        !rig.app.get_analytics_open(),
        "the button is no longer selected"
    );

    rig.click(&button);
    rig.app.invoke_analytics_toggled(); // the close x
    rig.pump();
    assert!(
        !rig.open() && !rig.app.get_analytics_open(),
        "close x closes it"
    );

    rig.click(&button);
    rig.escape();
    assert!(!rig.open(), "Esc closes it");
    let mut controller = rig.controller.lock().unwrap();
    controller.toggle_analytics();
    assert_eq!(
        controller.chrome_key("\u{1b}", Modifiers::default()),
        ChromeKey::Consumed
    );
    assert!(!controller.analytics_open());
}

#[test]
fn opening_analytics_replaces_what_the_window_draws() {
    let mut controller = common::controller_with_frames("analytics_window_exclusive", 2);
    controller.set_analytics_hold(Duration::from_secs(30));
    let rig = rig_with(controller);
    let with = |f: &dyn Fn(&mut AppController)| {
        f(&mut rig.controller.lock().unwrap());
        rig.pump();
    };

    // Each other overlay, open, is gone from the window once Analytics opens
    // and Analytics is gone once that overlay opens again.
    with(&|c| c.open_center());
    assert!(rig.app.get_notification_center_open());
    with(&|c| {
        c.toggle_analytics();
    });
    assert!(rig.app.get_analytics_open() && !rig.app.get_notification_center_open());
    with(&|c| c.open_center());
    assert!(rig.app.get_notification_center_open() && !rig.app.get_analytics_open());
    with(&|c| c.close_center());

    with(&|c| c.toggle_projects_overlay());
    assert!(rig.app.get_projects_overlay_open());
    with(&|c| {
        c.toggle_analytics();
    });
    assert!(rig.app.get_analytics_open() && !rig.app.get_projects_overlay_open());
    with(&|c| c.toggle_projects_overlay());
    assert!(rig.app.get_projects_overlay_open() && !rig.app.get_analytics_open());
    with(&|c| c.toggle_projects_overlay());

    with(&|c| c.toggle_calculator());
    assert!(rig.app.get_calculator_open());
    with(&|c| {
        c.toggle_analytics();
    });
    assert!(rig.app.get_analytics_open() && !rig.app.get_calculator_open());
    with(&|c| c.toggle_calculator());
    assert!(rig.app.get_calculator_open() && !rig.app.get_analytics_open());
}

#[test]
fn closing_lowers_the_count_and_keeps_it() {
    let mut controller = common::controller_with_frames("analytics_close_keeps", 3);
    controller.set_analytics_hold(HOLD);
    let worker = controller.toggle_analytics().expect("a count started");
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Normal)
    );
    controller.close_analytics();
    assert!(!controller.analytics_open());
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Low)
    );

    // The count finishes and lands while the overlay is closed.
    worker.join().unwrap();
    controller.settle_analytics();
    assert_eq!(rows_of(&controller).len(), 3);

    // Opening again shows the kept rows and starts nothing.
    assert!(controller.toggle_analytics().is_none());
    assert!(controller.take_analytics_worker().is_none());
    assert_eq!(rows_of(&controller).len(), 3);
}

#[test]
fn analytics_opens_without_a_stack() {
    let rig = rig_with(AppController::new());
    rig.pump();
    let button = rig.button("Analytics");
    rig.click(&button);
    assert!(
        rig.open() && rig.app.get_analytics_open(),
        "the button opens it"
    );
    assert_eq!(
        *rig.controller.lock().unwrap().analytics_view(),
        AnalyticsView::Waiting
    );
    assert_eq!(rig.app.get_analytics_state(), "waiting");
    assert_eq!(rig.app.get_analytics_rows().row_count(), 0);
    assert_eq!(labelled(&rig, "No stack loaded"), 1);
    assert_eq!(labelled(&rig, "Load a stack to count its sharp areas."), 1);
    assert_eq!(spinners(&rig), 0, "the waiting panel has no spinner");
    assert!(
        rig.controller
            .lock()
            .unwrap()
            .take_analytics_worker()
            .is_none()
    );

    rig.click(&button);
    assert!(!rig.open(), "the button closes it again");
    rig.click(&button);
    rig.escape();
    assert!(!rig.open(), "Esc closes the waiting overlay");
}

#[test]
fn closing_the_stack_returns_to_waiting() {
    let mut controller = common::controller_with_frames("analytics_needs_stack", 2);
    controller.set_analytics_hold(HOLD);
    let held = controller.toggle_analytics().expect("a count started");
    assert_pending(&controller, 2);

    controller.close_project(0);
    assert!(controller.analytics_open(), "the overlay stays open");
    assert_eq!(*controller.analytics_view(), AnalyticsView::Waiting);
    held.join().unwrap();
    controller.settle_analytics();
    assert_eq!(
        *controller.analytics_view(),
        AnalyticsView::Waiting,
        "the closed stack's count shows nothing"
    );

    // Opening a stack while it waits starts the count for that stack.
    let root = common::tree("analytics_needs_stack_again");
    common::copy_frame(&root, "a.tif", 0);
    common::copy_frame(&root, "b.tif", 1);
    common::copy_frame(&root, "c.tif", 2);
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        focusforge_gui::OpenOutcome::Opened(_)
    ));
    assert_pending(&controller, 3);
    let worker = controller
        .take_analytics_worker()
        .expect("opening the stack started a count");
    controller.close_analytics();
    worker.join().unwrap();
}

#[test]
fn excluded_frames_are_listed() {
    let mut controller = common::controller_with_frames_from("mantis-512", "analytics_excluded", 4);
    controller.toggle_frame_excluded(1);
    count_now(&mut controller);
    let rows = rows_of(&controller);
    let expected = core_counts(&fixture_frames(4));
    assert_eq!(rows.len(), 4);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.chip, source_color(i as u16, 4), "row {i}");
        assert_eq!(row.count, expected[i], "row {i}");
    }
}

#[test]
fn tab_switch_follows_the_active_stack() {
    let mut controller = common::controller_with_frames("analytics_tabs", 3);
    let second = common::tree("analytics_tabs_second_frames");
    for i in 0..2 {
        common::copy_frame(&second, &format!("s{i}.tif"), i + 1);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&second)),
        focusforge_gui::OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.active_project(), Some(1));

    // Open over the second tab while a held count of the first is replaced.
    controller.select_project(0);
    controller.set_analytics_hold(Duration::from_millis(400));
    let first = controller.toggle_analytics().expect("a count started");
    controller.select_project(1);
    assert_pending(&controller, 2);
    let second_worker = controller
        .take_analytics_worker()
        .expect("the switch started a count");
    first.join().unwrap();
    second_worker.join().unwrap();
    controller.settle_analytics();
    let names: Vec<_> = rows_of(&controller).into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["s0.tif", "s1.tif"]);

    // Back to the first tab, and the overlay stays open for it.
    controller.set_analytics_hold(Duration::ZERO);
    controller.select_project(0);
    controller
        .take_analytics_worker()
        .expect("the switch started a count")
        .join()
        .unwrap();
    controller.settle_analytics();
    assert!(controller.analytics_open());
    assert_eq!(rows_of(&controller).len(), 3);
}

#[test]
fn analytics_works_in_editor_mode() {
    let root = common::tree("analytics_editor_frames");
    for i in 0..3 {
        std::fs::copy(
            common::fixture_frame("mantis-64", i),
            root.join(format!("{i:02}.tif")),
        )
        .unwrap();
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        focusforge_gui::OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(common::tree("analytics_editor_scratch"));
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    common::enter_editor(&mut controller);
    assert_eq!(controller.mode(), ShellMode::Editor);

    count_now(&mut controller);
    assert_eq!(rows_of(&controller).len(), 3);
}

#[test]
fn long_stack_lists_every_row() {
    let mut controller = controller_with_identical_frames("analytics_long", 61);
    count_now(&mut controller);
    assert_eq!(rows_of(&controller).len(), 61);

    let rig = rig_with(controller);
    rig.pump();
    let model = rig.app.get_analytics_rows();
    assert_eq!(model.row_count(), 61);
    assert_eq!(model.row_data(60).unwrap().name, "60.tif");
}

#[test]
fn escape_closes_after_a_tab_click() {
    for bell_first in [false, true] {
        for tab in 0..2 {
            let at = format!("tab {tab}, notifications first: {bell_first}");
            let mut controller = common::controller_with_two_stacks("analytics_escape_tab");
            controller.set_analytics_hold(Duration::from_secs(30));
            let rig = rig_with(controller);
            rig.pump();
            if bell_first {
                rig.click(&rig.button("Notifications"));
                assert!(rig.app.get_notification_center_open(), "{at}: center opens");
            }
            rig.click(&rig.button("Analytics"));
            assert!(rig.open() && rig.app.get_analytics_open(), "{at}: opens");
            assert!(
                !rig.app.get_notification_center_open(),
                "{at}: center closed"
            );

            rig.click(&common::tab(&rig.app, tab));
            assert!(rig.open(), "{at}: a tab click keeps it open");
            rig.escape();
            assert!(!rig.open(), "{at}: Escape closes it");
            assert!(!rig.app.get_analytics_open(), "{at}: button plain");
        }
    }
}

/// Where the count of the active stack stands when the pointer gesture starts.
#[derive(Clone, Copy, Debug)]
enum CountState {
    /// Asleep in its start-up hold.
    Pending,
    /// One partial snapshot settled, the worker parked on the next.
    Counting,
    /// Finished and settled.
    Done,
}

/// A file name no dock row can show whole.
const LONG_NAME: &str = "a_frame_file_name_far_longer_than_the_dock_row_can_show_in_full_0001.tif";

/// Releases the worker of a `Counting` rig one snapshot at a time.
struct SnapshotGate {
    ready: std::sync::mpsc::Receiver<()>,
    go: std::sync::mpsc::Sender<()>,
    /// Whether the count after the test's last click ends in a joinable time;
    /// a held start-up count does not.
    join: bool,
}

impl SnapshotGate {
    /// Lets the parked worker read on and settles the snapshot it parks next.
    fn settle_next(&self, rig: &Rig) {
        self.go.send(()).expect("the worker waits");
        self.ready.recv().expect("the worker reports");
        rig.controller.lock().unwrap().settle_analytics();
        rig.pump();
    }

    /// Lets every parked worker run out and joins the one the last tab
    /// click started.
    fn finish(self, rig: &Rig) {
        drop(self.go);
        let worker = rig.controller.lock().unwrap().take_analytics_worker();
        if let (true, Some(worker)) = (self.join, worker) {
            worker.join().expect("the count worker did not panic");
        }
    }
}

/// A window over a stack of three frames and a second, active stack of two
/// frames named `second`, with Analytics opened by its button and the active
/// stack's count in `state`.
fn rig_counting(tag: &str, second: [&str; 2], state: CountState) -> (Rig, SnapshotGate) {
    // Several snapshots need a frame larger than one tile.
    const TILE_SIZE: u32 = 96;
    let set = match state {
        CountState::Counting => "mantis-512",
        _ => "mantis-64",
    };
    let mut controller = common::controller_with_frames_from(set, tag, 3);
    let tree = common::tree(&format!("{tag}_second_frames"));
    for (i, name) in second.iter().enumerate() {
        common::copy_frame_from(set, &tree, name, i + 1);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[tree]),
        focusforge_gui::OpenOutcome::Opened(_)
    ));
    controller.set_analytics_hold(match state {
        CountState::Pending => Duration::from_secs(30),
        _ => Duration::ZERO,
    });
    if matches!(state, CountState::Counting) {
        controller.set_analytics_tile_size(TILE_SIZE);
    }
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    if matches!(state, CountState::Done) {
        count_now(&mut controller);
    }
    let rig = rig_with(controller);
    if matches!(state, CountState::Counting) {
        let go_rx = Mutex::new(go_rx);
        rig.controller
            .lock()
            .unwrap()
            .set_sink(Arc::new(move |event| {
                if matches!(event, focusforge_gui::ShellEvent::AnalyticsReady) {
                    ready_tx.send(()).ok();
                    go_rx.lock().unwrap().recv().ok();
                }
            }));
    }
    let gate = SnapshotGate {
        ready,
        go,
        join: !matches!(state, CountState::Pending),
    };
    rig.pump();
    if !matches!(state, CountState::Done) {
        rig.click(&rig.button("Analytics"));
    }
    assert!(
        rig.open() && rig.app.get_analytics_open(),
        "{state:?}: opens"
    );
    match state {
        CountState::Pending => assert_pending(&rig.controller.lock().unwrap(), 2),
        CountState::Counting => {
            gate.ready.recv().expect("the worker reports");
            rig.controller.lock().unwrap().settle_analytics();
            rig.pump();
            assert_eq!(rig.app.get_analytics_state(), "counting");
            assert_eq!(row_states(&rig), ["partial"; 2], "{state:?}");
        }
        CountState::Done => assert_eq!(row_states(&rig), ["final"; 2], "{state:?}"),
    }
    (rig, gate)
}

/// The window's `FFImageRow`s that take space, as the dock draws them.
fn dock_rows(rig: &Rig) -> usize {
    ElementQuery::from_root(&rig.app)
        .match_id("FFImageRow::area")
        .find_all()
        .into_iter()
        .filter(|r| r.size().width > 0.0)
        .count()
}

fn analytics_overlay(rig: &Rig) -> ElementHandle {
    ElementHandle::find_by_element_id(&rig.app, "AppWindow::analytics-overlay")
        .next()
        .expect("the window has the Analytics overlay")
}

/// The UI driver's hover: a drag with the `Other` button from the centre of
/// the Analytics overlay onto the centre of dock image row `row`.
fn hover_dock_row(rig: &Rig, row: usize) {
    let target = common::image_row(&rig.app, row);
    let (pos, size) = (target.absolute_position(), target.size());
    analytics_overlay(rig).mock_drag(
        slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0),
        PointerEventButton::Other,
    );
    rig.pump();
}

#[test]
fn a_click_after_a_dock_hover_switches_the_tab() {
    // (count state, second stack's names, dock row hovered, a snapshot
    // settles between hover and click, tab clicked)
    let cases = [
        (CountState::Pending, ["s0.tif", "s1.tif"], 0, false, 0),
        (CountState::Pending, ["s0.tif", LONG_NAME], 1, false, 0),
        (CountState::Pending, ["s0.tif", "s1.tif"], 0, false, 1),
        (CountState::Counting, ["s0.tif", "s1.tif"], 0, true, 0),
        (CountState::Done, ["s0.tif", "s1.tif"], 0, false, 0),
    ];
    for (n, (state, second, row, snapshot, tab)) in cases.into_iter().enumerate() {
        let at = format!("{state:?}, {second:?}, row {row}, snapshot {snapshot}, tab {tab}");
        let (rig, gate) = rig_counting(&format!("analytics_hover_tab_{n}"), second, state);
        hover_dock_row(&rig, row);
        if snapshot {
            gate.settle_next(&rig);
        }
        rig.click(&common::tab(&rig.app, tab));

        let rows = if tab == 0 { 3 } else { 2 };
        assert_eq!(
            rig.controller.lock().unwrap().active_project(),
            Some(tab),
            "{at}: the tab is selected"
        );
        assert_eq!(dock_rows(&rig), rows, "{at}: the dock lists its rows");
        assert_eq!(
            rig.app.get_analytics_rows().row_count(),
            rows,
            "{at}: the overlay follows"
        );
        assert!(
            rig.open() && rig.app.get_analytics_open(),
            "{at}: stays open"
        );
        gate.finish(&rig);
    }
}

#[test]
fn a_click_after_a_dock_hover_closes_the_overlay() {
    for (n, state) in [CountState::Pending, CountState::Counting, CountState::Done]
        .into_iter()
        .enumerate()
    {
        let (rig, gate) = rig_counting(
            &format!("analytics_hover_close_{n}"),
            ["s0.tif", "s1.tif"],
            state,
        );
        hover_dock_row(&rig, 0);
        let mut close: Vec<_> = analytics_overlay(&rig)
            .query_descendants()
            .match_accessible_role(AccessibleRole::Button)
            .find_all()
            .into_iter()
            .filter(|b| b.accessible_label().as_deref() == Some("Close"))
            .collect();
        assert_eq!(close.len(), 1, "{state:?}: one Close in the overlay");
        rig.click(&close.remove(0));
        assert!(!rig.open(), "{state:?}: Close closes it");
        assert!(!rig.app.get_analytics_open(), "{state:?}: button plain");
        gate.finish(&rig);
    }
}

#[test]
fn escape_closes_after_an_image_row_click() {
    for row in 0..2 {
        let mut controller = common::controller_with_two_stacks("analytics_escape_row");
        controller.set_analytics_hold(Duration::from_secs(30));
        let rig = rig_with(controller);
        rig.pump();
        rig.click(&common::image_row(&rig.app, row));
        rig.click(&rig.button("Analytics"));
        assert!(
            rig.open() && rig.app.get_analytics_open(),
            "row {row}: opens"
        );

        rig.escape();
        assert!(!rig.open(), "row {row}: Escape closes it");
        assert!(!rig.app.get_analytics_open(), "row {row}: button plain");
    }
}

#[test]
fn ctrl_chords_reach_after_an_image_row_click() {
    let rig = rig_with(common::controller_with_two_stacks("analytics_chords_row"));
    rig.pump();
    let tabs = || rig.controller.lock().unwrap().project_tabs().len();
    assert_eq!(tabs(), 2);
    rig.click(&common::image_row(&rig.app, 0));
    rig.chord("w");
    assert_eq!(tabs(), 1, "Ctrl+W closes the active project");

    // N, O and an unsaved S open a native picker past the bridge, so
    // recorders stand in for it.
    let pressed = Rc::new(RefCell::new(Vec::new()));
    let released = Rc::new(RefCell::new(Vec::new()));
    rig.app.on_canvas_key_pressed({
        let pressed = pressed.clone();
        move |text, ctrl, _, _| pressed.borrow_mut().push((text.to_string(), ctrl))
    });
    rig.app.on_canvas_key_released({
        let released = released.clone();
        move |text, _, _, _| released.borrow_mut().push(text.to_string())
    });
    rig.click(&common::image_row(&rig.app, 0));
    for key in ["n", "o", "s"] {
        rig.chord(key);
    }
    let pressed = pressed.borrow();
    for key in ["n", "o", "s"] {
        assert!(
            pressed.contains(&(key.to_string(), true)),
            "Ctrl+{key} reaches canvas-key-pressed, saw {pressed:?}"
        );
    }
    assert!(
        released.borrow().iter().any(|k| k == "s"),
        "a release reaches canvas-key-released, saw {:?}",
        released.borrow()
    );
}

#[test]
fn image_row_click_keeps_arrows_and_escape_order() {
    let mut controller = common::controller_with_two_stacks("analytics_row_boundary");
    controller.set_analytics_hold(Duration::from_secs(30));
    let rig = rig_with(controller);
    rig.pump();
    let label = || rig.controller.lock().unwrap().frame_pane_label();

    rig.click(&common::image_row(&rig.app, 0));
    rig.press(Key::DownArrow.into());
    assert_eq!(label(), "s1.tif", "Down moves to the next row");
    rig.press(Key::UpArrow.into());
    assert_eq!(label(), "s0.tif", "Up moves back");

    rig.escape();
    assert!(!rig.open() && !rig.app.get_analytics_open());
    assert!(!rig.app.get_calculator_open());
    assert_eq!(rig.controller.lock().unwrap().project_tabs().len(), 2);

    rig.click(&rig.button("Analytics"));
    rig.click(&common::image_row(&rig.app, 0));
    // Set after the last pump: a pump resets the flag to the controller's.
    rig.app.set_divergence_open(true);
    rig.escape();
    assert!(rig.open(), "the dialog takes the key, Analytics stays");
    rig.app.set_divergence_open(false);
    rig.pump();
    rig.escape();
    assert!(!rig.open(), "a second Escape closes Analytics");
}

/// A controller with the background count on and every count held asleep for
/// `hold`, over `count` freshly loaded `mantis-64` frames.
fn background_controller(name: &str, count: usize, hold: Duration) -> AppController {
    let mut controller = AppController::new();
    controller.set_analytics_background(true);
    controller.set_analytics_hold(hold);
    common::open_mantis64_tab(&mut controller, name, count);
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

/// Joins the count worker the last load or edit started, then applies it.
fn finish_count(controller: &mut AppController) {
    controller
        .take_analytics_worker()
        .expect("a count is running")
        .join()
        .expect("the count worker did not panic");
    controller.settle_analytics();
}

fn names_of(controller: &AppController) -> Vec<String> {
    rows_of(controller).into_iter().map(|r| r.name).collect()
}

#[test]
fn background_count_starts_on_load() {
    let mut off = common::controller_with_frames("analytics_bg_off", 3);
    assert!(off.take_analytics_worker().is_none());
    assert_eq!(off.analytics_count_priority(), None);
    assert_eq!(*off.analytics_view(), AnalyticsView::Waiting);

    let mut on = background_controller("analytics_bg_on", 3, HOLD);
    assert!(!on.analytics_open());
    assert_eq!(on.analytics_count_priority(), Some(CountPriority::Low));
    assert_pending(&on, 3);
    let worker = on.take_analytics_worker().expect("loading started a count");
    on.close_project(0);
    worker.join().unwrap();
}

#[test]
fn opening_analytics_boosts_the_running_count() {
    let mut controller = background_controller("analytics_bg_boost", 3, HOLD);
    let worker = controller.take_analytics_worker().expect("a count runs");

    assert!(controller.toggle_analytics().is_none(), "no restart");
    assert!(
        controller.take_analytics_worker().is_none(),
        "no new worker"
    );
    assert!(controller.analytics_open());
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Normal)
    );
    assert_pending(&controller, 3);

    controller.close_analytics();
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Low)
    );

    worker.join().unwrap();
    controller.settle_analytics();
    let rows = rows_of(&controller);
    assert!(rows.iter().all(|r| r.value == RowValue::Final), "{rows:?}");
}

#[test]
fn cached_counts_show_at_once() {
    let mut controller = background_controller("analytics_bg_cache", 3, Duration::ZERO);
    finish_count(&mut controller);
    let counted = rows_of(&controller);

    assert!(controller.toggle_analytics().is_none(), "no new worker");
    assert!(controller.take_analytics_worker().is_none());
    let rig = rig_with(controller);
    rig.pump();
    assert_eq!(rig.app.get_analytics_state(), "rows");
    assert_eq!(row_states(&rig), ["final"; 3]);
    assert_eq!(spinners(&rig), 0);

    // A second stack counts on its own; the first shows its kept rows again.
    {
        let mut controller = rig.controller.lock().unwrap();
        common::open_frames_from(&mut controller, "mantis-64", "analytics_bg_cache_b", 2);
        assert_eq!(controller.active_project(), Some(1));
        controller.select_project(0);
        assert_eq!(rows_of(&controller), counted);
        if let Some(stale) = controller.take_analytics_worker() {
            stale.join().unwrap();
        }
        controller.settle_analytics();
        assert_eq!(
            rows_of(&controller),
            counted,
            "the cancelled count lands nowhere"
        );
    }
    rig.pump();
    assert_eq!(row_states(&rig), ["final"; 3]);
    assert_eq!(spinners(&rig), 0);
}

#[test]
fn frame_list_change_drops_the_cache() {
    let mut controller = background_controller("analytics_bg_edit", 3, Duration::ZERO);
    finish_count(&mut controller);
    assert!(controller.toggle_analytics().is_none());
    assert_eq!(rows_of(&controller).len(), 3);

    controller.remove_frame(0);
    assert!(
        matches!(controller.analytics_view(), AnalyticsView::Counting(_)),
        "the old rows are not shown"
    );
    finish_count(&mut controller);
    assert_eq!(names_of(&controller), ["01.tif", "02.tif"]);

    controller.move_frame(0, 1);
    assert!(matches!(
        controller.analytics_view(),
        AnalyticsView::Counting(_)
    ));
    finish_count(&mut controller);
    assert_eq!(names_of(&controller), ["02.tif", "01.tif"]);
}

#[test]
fn opening_another_stack_restarts_the_count() {
    let mut controller = background_controller("analytics_bg_other_a", 3, HOLD);
    let first = controller.take_analytics_worker().expect("a count for A");

    common::open_frames_from(&mut controller, "mantis-64", "analytics_bg_other_b", 2);
    let second = controller
        .take_analytics_worker()
        .expect("opening B started a count");
    assert_pending(&controller, 2);

    // A's cancelled worker parks nothing into B's view.
    first.join().unwrap();
    controller.settle_analytics();
    assert_pending(&controller, 2);

    // Nothing of A was cached: going back to it counts again.
    controller.select_project(0);
    assert_pending(&controller, 3);
    let again = controller
        .take_analytics_worker()
        .expect("A has no cached counts");
    second.join().unwrap();
    again.join().unwrap();
    controller.settle_analytics();
    assert_eq!(names_of(&controller), ["00.tif", "01.tif", "02.tif"]);
}

#[test]
fn stacking_holds_the_background_count() {
    let mut controller = background_controller("analytics_bg_job", 3, HOLD);
    let worker = controller.take_analytics_worker().expect("a count runs");
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Low)
    );

    controller.set_slow_run(1, Duration::from_secs(3));
    assert!(controller.start_fuse());
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Held)
    );
    assert!(
        controller.take_analytics_worker().is_none(),
        "not restarted"
    );

    assert!(controller.toggle_analytics().is_none());
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Low)
    );
    controller.close_analytics();
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Held)
    );

    common::wait_for_settle(&mut controller, Duration::from_secs(120)).expect("the run settles");
    assert_eq!(
        controller.analytics_count_priority(),
        Some(CountPriority::Low)
    );
    worker.join().unwrap();
    controller.settle_analytics();
    let rows = rows_of(&controller);
    assert!(rows.iter().all(|r| r.value == RowValue::Final), "{rows:?}");
}
