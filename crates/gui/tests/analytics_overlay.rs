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
use focusforge_core::sharp_areas::count_sharp_areas;
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, NoControl};
use focusforge_core::zmap::source_color;
use focusforge_gui::analytics::{AnalyticsRow, AnalyticsView};
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

    // The call returned while the worker still sleeps: the overlay waits.
    assert!(!worker.is_finished());
    assert!(controller.analytics_open());
    assert_eq!(*controller.analytics_view(), AnalyticsView::Computing);
    controller.settle_analytics();
    assert_eq!(
        *controller.analytics_view(),
        AnalyticsView::Computing,
        "settling with nothing parked shows nothing new"
    );
    let rig = rig_with(controller);
    rig.pump();
    assert_eq!(rig.app.get_analytics_state(), "computing");
    assert_eq!(rig.app.get_analytics_rows().row_count(), 0);

    worker.join().expect("the count worker did not panic");
    rig.controller.lock().unwrap().settle_analytics();
    rig.pump();
    assert_eq!(rig.app.get_analytics_state(), "rows");
    assert_eq!(rig.app.get_analytics_rows().row_count(), 3);
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
fn close_cancels_the_count() {
    // A count held asleep is stopped by the close; the stack then counted by
    // the next open keeps its result, which the stopped worker, finishing
    // later, must not replace.
    let mut controller = common::controller_with_frames("analytics_cancel", 3);
    controller.set_analytics_hold(HOLD);
    let stopped = controller.toggle_analytics().expect("a count started");
    controller.close_analytics();
    assert!(!controller.analytics_open());

    controller.set_analytics_hold(Duration::ZERO);
    let current = controller.toggle_analytics().expect("a count started");
    current.join().unwrap();
    stopped.join().unwrap();
    controller.settle_analytics();
    assert_eq!(rows_of(&controller).len(), 3);
}

#[test]
fn close_cancels_the_count_stale_result_dropped() {
    let mut controller = common::controller_with_frames("analytics_stale", 3);

    // A finished count parked before the close is not shown after it.
    controller.set_analytics_hold(Duration::ZERO);
    let finished = controller.toggle_analytics().expect("a count started");
    finished.join().unwrap();
    controller.close_analytics();
    controller.settle_analytics();
    assert_eq!(*controller.analytics_view(), AnalyticsView::Computing);

    // Nor after the overlay opened again over a new, still running count.
    let finished = controller.toggle_analytics().expect("a count started");
    finished.join().unwrap();
    controller.close_analytics();
    controller.set_analytics_hold(HOLD);
    let running = controller.toggle_analytics().expect("a count started");
    controller.settle_analytics();
    assert_eq!(
        *controller.analytics_view(),
        AnalyticsView::Computing,
        "the earlier count's result belongs to a closed overlay"
    );
    controller.close_analytics();
    running.join().unwrap();
}

#[test]
fn analytics_button_needs_a_stack() {
    let rig = rig_with(AppController::new());
    rig.pump();
    assert!(!rig.app.get_analytics_enabled());
    assert!(!rig.controller.lock().unwrap().analytics_enabled());
    assert!(rig.controller.lock().unwrap().toggle_analytics().is_none());
    rig.click(&rig.button("Analytics"));
    assert!(!rig.open(), "the disabled button opens nothing");

    // Closing the stack closes the overlay.
    let mut controller = common::controller_with_frames("analytics_needs_stack", 2);
    controller.set_analytics_hold(Duration::from_secs(30));
    assert!(controller.analytics_enabled());
    controller.toggle_analytics();
    assert!(controller.analytics_open());
    controller.close_project(0);
    assert!(!controller.analytics_enabled());
    assert!(!controller.analytics_open(), "no stack, no overlay");
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
    assert_eq!(*controller.analytics_view(), AnalyticsView::Computing);
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

    assert!(controller.analytics_enabled(), "enabled in the Editor");
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
