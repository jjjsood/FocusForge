// SPDX-License-Identifier: GPL-3.0-or-later

//! The step calculator overlay: its button and open/close rules driven through
//! a headless window, and its entries and read-outs driven through
//! `AppController` against core's plan.

mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use focusforge_core::host;
use focusforge_core::params::{self, ParamValue, ParamValues};
use focusforge_core::step_plan::{PLAN_PARAMS, StepPlan, plan};
use focusforge_gui::calculator::{Calculator, CalculatorView};
use focusforge_gui::input_map::Modifiers;
use focusforge_gui::{AppController, AppWindow, ChromeKey};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};

type SharedController = Arc<Mutex<AppController>>;

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
        self.click_at(slint::LogicalPosition::new(
            pos.x + size.width / 2.0,
            pos.y + size.height / 2.0,
        ));
    }

    fn click_at(&self, position: slint::LogicalPosition) {
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

    fn escape(&self) {
        let text: slint::SharedString = Key::Escape.into();
        self.event(WindowEvent::KeyPressed { text: text.clone() });
        self.event(WindowEvent::KeyReleased { text });
        self.pump();
    }

    fn open(&self) -> bool {
        self.controller.lock().unwrap().calculator_open()
    }
}

fn rect(app: &AppWindow, id: &str) -> (f32, f32, f32, f32) {
    let handle = ElementQuery::from_root(app)
        .match_id(id)
        .find_first()
        .unwrap_or_else(|| panic!("no element matched id {id:?}"));
    let (pos, size) = (handle.absolute_position(), handle.size());
    (pos.x, pos.y, size.width, size.height)
}

/// The plan core computes for `entries` resolved against `PLAN_PARAMS`, as
/// `focusforge-cli step-plan --set` resolves them.
fn core_plan(entries: &[(&str, &str)]) -> StepPlan {
    let explicit: ParamValues = entries
        .iter()
        .map(|(id, text)| {
            let desc = PLAN_PARAMS.iter().find(|d| d.id == *id).unwrap();
            ((*id).to_string(), ParamValue::parse(desc, text).unwrap())
        })
        .collect();
    let resolved = params::resolve_in(&PLAN_PARAMS, &explicit, None, &host::snapshot())
        .expect("entries are inside the schema bounds");
    plan(&params::resolved_values(&resolved))
}

fn calculated(view: &CalculatorView) -> Vec<(&str, &str)> {
    view.calculated
        .iter()
        .map(|(c, v)| (c.as_str(), v.as_str()))
        .collect()
}

// Lens field slots: 0 time per frame, 1 magnification, 2 f-stop, 3 overlap,
// 4 depth range. Objective slot 2 is the numerical aperture; slot 1 is empty.

#[test]
fn calculator_button_toggles_the_overlay() {
    let rig = rig_with(AppController::new());
    let button = rig.button("Step calculator");
    assert!(!rig.open());
    assert!(!rig.app.get_calculator_open());

    rig.click(&button);
    assert!(rig.open());
    assert!(
        rig.app.get_calculator_open(),
        "the window shows the overlay"
    );

    // The overlay covers the canvas and leaves the dock uncovered.
    let overlay = rect(&rig.app, "AppWindow::calculator-overlay");
    let dock = rect(&rig.app, "AppWindow::dock");
    assert!(
        overlay.0 + overlay.2 <= dock.0 + 0.5,
        "{overlay:?} {dock:?}"
    );

    rig.click(&button);
    assert!(!rig.open(), "the button toggles it closed");

    rig.click(&button);
    rig.app.invoke_calculator_toggled(); // the close x
    rig.pump();
    assert!(!rig.open(), "the close x closes it");

    rig.click(&button);
    rig.escape();
    assert!(!rig.open(), "Esc closes it");
    assert!(!rig.app.get_calculator_open());

    // One full-canvas overlay at a time, with or without a project.
    let projects = AppController::new();
    let mut controllers = vec![
        projects,
        common::controller_with_frames("calculator_toggle", 2),
    ];
    for controller in &mut controllers {
        controller.toggle_calculator();
        controller.toggle_projects_overlay();
        assert!(!controller.calculator_open() && controller.projects_overlay_open());
        controller.toggle_calculator();
        assert!(controller.calculator_open() && !controller.projects_overlay_open());
        controller.open_center();
        assert!(!controller.calculator_open());
        controller.toggle_calculator();
        assert_eq!(
            controller.chrome_key("\u{1b}", Modifiers::default()),
            ChromeKey::Consumed
        );
        assert!(!controller.calculator_open());
    }
}

#[test]
fn calculator_readout_matches_the_cli_plan() {
    // Defaults for the lens optics, in the CLI's text format.
    let mut calculator = Calculator::new();
    assert_eq!(*calculator.plan(), core_plan(&[]));
    let view = calculator.view();
    assert_eq!(view.optics_selected, "Lens (f-stop)");
    assert_eq!(
        calculated(&view),
        [
            ("Numerical aperture", "0.031"),
            ("Depth of field", "563.2 \u{00B5}m"),
            ("Step", "422.4 \u{00B5}m"),
            ("Frames", "13"),
            ("Duration", "13 s"),
        ]
    );

    // Objective, depth range 0.1 mm.
    calculator.set_optics("Objective (NA)");
    calculator.edit(4, "0.1");
    assert_eq!(
        *calculator.plan(),
        core_plan(&[("optics", "objective"), ("range_mm", "0.1")])
    );
    let view = calculator.view();
    assert_eq!(view.optics_selected, "Objective (NA)");
    assert_eq!(
        calculated(&view),
        [
            ("Depth of field", "8.8 \u{00B5}m"),
            ("Step", "6.6 \u{00B5}m"),
            ("Frames", "17"),
            ("Duration", "17 s"),
        ]
    );
    assert_eq!(view.planes, 17);

    // Lens again, with edits through the controller as the window drives it.
    let mut controller = AppController::new();
    for (slot, text) in [(2, "2.8"), (1, "2"), (3, "0.5"), (4, "1"), (0, "2")] {
        controller.calculator_edit(slot, text);
    }
    let view = controller.calculator_view();
    assert_eq!(
        calculated(&view),
        [
            ("Numerical aperture", "0.060"),
            ("Depth of field", "155.23 \u{00B5}m"),
            ("Step", "77.62 \u{00B5}m"),
            ("Frames", "14"),
            ("Duration", "28 s"),
        ]
    );
    let mut calculator = Calculator::new();
    for (slot, text) in [(2, "2.8"), (1, "2"), (3, "0.5"), (4, "1"), (0, "2")] {
        calculator.edit(slot, text);
    }
    assert_eq!(
        *calculator.plan(),
        core_plan(&[
            ("aperture", "2.8"),
            ("magnification", "2"),
            ("overlap", "0.5"),
            ("range_mm", "1"),
            ("frame_seconds", "2"),
        ])
    );

    // A range that needs far more frames than can be drawn: the Frames
    // read-out keeps the true count, the drawing is capped.
    calculator.set_optics("Objective (NA)");
    calculator.edit(2, "0.95");
    calculator.edit(3, "0.9");
    calculator.edit(4, "1000");
    let view = calculator.view();
    assert_eq!(view.frames, calculator.plan().frames.to_string());
    assert!(calculator.plan().frames > 1_000_000);
    assert_eq!(view.planes, 30);
}

#[test]
fn calculator_fields_follow_the_plan_params() {
    let desc = |id: &str| PLAN_PARAMS.iter().find(|d| d.id == id).unwrap();
    let mut calculator = Calculator::new();

    let lens = [
        "frame_seconds",
        "magnification",
        "aperture",
        "overlap",
        "range_mm",
    ];
    let view = calculator.view();
    assert!(!view.objective);
    assert_eq!(view.fields.len(), 5);
    for (field, id) in view.fields.iter().zip(lens) {
        let field = field.as_ref().expect("every Lens slot is shown");
        let d = desc(id);
        assert_eq!(field.label, d.label, "{id}");
        let default = params::resolved_values(
            &params::resolve_in(&PLAN_PARAMS, &ParamValues::new(), None, &host::snapshot())
                .unwrap(),
        );
        assert_eq!(
            ParamValue::parse(d, &field.text).unwrap(),
            default[id],
            "{id} shows its default"
        );
        assert!(field.error.is_empty());
    }
    assert_eq!(
        view.entered
            .iter()
            .map(|(c, _)| c.as_str())
            .collect::<Vec<_>>(),
        [
            "Optics",
            "Magnification",
            "f-stop",
            "Overlap",
            "Depth range",
            "Time per frame"
        ]
    );
    assert_eq!(view.entered[1].1, "1\u{00D7}");

    calculator.set_optics("Objective (NA)");
    let view = calculator.view();
    assert!(view.objective);
    let objective = [
        Some("frame_seconds"),
        None,
        Some("na"),
        Some("overlap"),
        Some("range_mm"),
    ];
    for (field, id) in view.fields.iter().zip(objective) {
        assert_eq!(
            field.as_ref().map(|f| f.label.as_str()),
            id.map(|i| desc(i).label.as_str())
        );
    }
    assert_eq!(
        view.entered
            .iter()
            .map(|(c, _)| c.as_str())
            .collect::<Vec<_>>(),
        [
            "Optics",
            "Numerical aperture",
            "Overlap",
            "Depth range",
            "Time per frame"
        ]
    );
    assert!(
        calculated(&view)
            .iter()
            .all(|(c, _)| *c != "Numerical aperture"),
        "CALCULATED lists the numerical aperture only with Lens"
    );

    // No file size field and no Settle time in either optics.
    for view in [calculator.view(), Calculator::new().view()] {
        for label in view
            .fields
            .iter()
            .flatten()
            .map(|f| f.label.as_str())
            .chain(view.entered.iter().map(|(c, _)| c.as_str()))
        {
            assert!(
                !label.contains("Settle") && !label.contains("MB"),
                "{label}"
            );
        }
    }

    // The window receives the same slots, the unused one hidden.
    let rig = rig_with(AppController::new());
    rig.controller
        .lock()
        .unwrap()
        .calculator_set_optics("Objective (NA)");
    rig.pump();
    let fields: Vec<_> = {
        use slint::Model;
        rig.app.get_calculator_fields().iter().collect()
    };
    assert_eq!(fields.len(), 5);
    assert_eq!(
        fields.iter().map(|f| f.shown).collect::<Vec<_>>(),
        [true, false, true, true, true]
    );
}

#[test]
fn calculator_violation_keeps_the_last_plan() {
    let mut calculator = Calculator::new();
    let defaults = calculator.view();
    let line = |c: &Calculator, slot: usize| c.view().fields[slot].clone().unwrap().error;

    calculator.edit(3, "0.95");
    assert_eq!(
        line(&calculator, 3),
        "value 0.95 is outside the allowed range [0, 0.9]"
    );
    calculator.edit(1, "0.001");
    assert!(line(&calculator, 1).contains("outside the allowed range"));
    calculator.edit(2, "eight");
    assert_eq!(line(&calculator, 2), "Not a number");

    // Three lines at once; every read-out keeps the last valid plan.
    let view = calculator.view();
    assert_eq!(view.calculated, defaults.calculated);
    assert_eq!(view.entered, defaults.entered);
    assert_eq!(view.step, defaults.step);
    assert_eq!(*calculator.plan(), core_plan(&[]));
    assert_eq!(
        view.fields[3].as_ref().unwrap().text,
        "0.95",
        "the entry stays as typed"
    );

    // Correcting an entry clears its line and re-plans.
    calculator.edit(3, "0.5");
    assert_eq!(line(&calculator, 3), "");
    assert!(!line(&calculator, 1).is_empty());
    assert_eq!(*calculator.plan(), core_plan(&[("overlap", "0.5")]));
    assert_ne!(calculator.view().step, defaults.step);

    // A single comma is a decimal point; two commas do not parse.
    calculator.edit(3, "0,25");
    assert_eq!(calculator.view().fields[3].as_ref().unwrap().text, "0.25");
    assert_eq!(line(&calculator, 3), "");
    calculator.edit(3, "0,2,5");
    assert_eq!(line(&calculator, 3), "Not a number");

    // The window shows the line on its field.
    let rig = rig_with(AppController::new());
    rig.controller.lock().unwrap().calculator_edit(3, "0.95");
    rig.pump();
    use slint::Model;
    let field = rig.app.get_calculator_fields().row_data(3).unwrap();
    assert_eq!(
        field.error,
        "value 0.95 is outside the allowed range [0, 0.9]"
    );
}

#[test]
fn calculator_has_no_apply_action() {
    let rig = rig_with(AppController::new());
    rig.controller.lock().unwrap().toggle_calculator();
    rig.controller
        .lock()
        .unwrap()
        .calculator_set_optics("Objective (NA)");
    slint::platform::update_timers_and_animations();
    for objective in [false, true] {
        if !objective {
            rig.controller
                .lock()
                .unwrap()
                .calculator_set_optics("Lens (f-stop)");
        }
        let labels: Vec<String> = ElementQuery::from_root(&rig.app)
            .match_descendants()
            .find_all()
            .iter()
            .filter_map(|e| e.accessible_label())
            .map(|l| l.to_string())
            .collect();
        assert!(
            labels.iter().any(|l| l.contains("Step calculator")),
            "{labels:?}"
        );
        for label in &labels {
            assert!(
                !label.contains("Apply") && !label.contains("Fills Step"),
                "unexpected {label:?}"
            );
        }
    }
}

/// A window over two stacks, the second tab active.
fn two_stack_rig(name: &str) -> Rig {
    let mut controller = common::controller_with_two_stacks(name);
    controller.set_analytics_hold(std::time::Duration::from_secs(30));
    let rig = rig_with(controller);
    rig.pump();
    rig
}

/// The calculator's first entry box.
fn first_entry_box(rig: &Rig) -> ElementHandle {
    let (ox, oy, ow, oh) = rect(&rig.app, "AppWindow::calculator-overlay");
    let mut boxes: Vec<_> = ElementQuery::from_root(&rig.app)
        .match_id("FFField::input")
        .find_all()
        .into_iter()
        .filter(|b| {
            let p = b.absolute_position();
            p.x >= ox && p.y >= oy && p.x < ox + ow && p.y < oy + oh && b.size().width > 0.0
        })
        .collect();
    boxes.sort_by(|a, b| {
        let (a, b) = (a.absolute_position(), b.absolute_position());
        (a.y, a.x).partial_cmp(&(b.y, b.x)).unwrap()
    });
    boxes.remove(0)
}

#[test]
fn escape_closes_after_a_tab_click() {
    for bell_first in [false, true] {
        for tab in 0..2 {
            let at = format!("tab {tab}, notifications first: {bell_first}");
            let rig = two_stack_rig("calculator_escape_tab");
            if bell_first {
                rig.click(&rig.button("Notifications"));
                assert!(rig.app.get_notification_center_open(), "{at}: center opens");
            }
            rig.click(&rig.button("Step calculator"));
            assert!(rig.open() && rig.app.get_calculator_open(), "{at}: opens");
            assert!(
                !rig.app.get_notification_center_open(),
                "{at}: center closed"
            );

            rig.click(&common::tab(&rig.app, tab));
            assert!(rig.open(), "{at}: a tab click keeps it open");
            rig.escape();
            assert!(!rig.open(), "{at}: Escape closes it");
            assert!(!rig.app.get_calculator_open(), "{at}: button plain");
        }
    }

    // Focus inside an entry box: the overlay's own scope hears Escape.
    let rig = two_stack_rig("calculator_escape_entry");
    rig.click(&rig.button("Step calculator"));
    rig.click(&first_entry_box(&rig));
    rig.escape();
    assert!(!rig.open(), "Escape in an entry box closes the calculator");

    // The calculator closed with focus left in the hidden box.
    let rig = two_stack_rig("calculator_escape_entry_analytics");
    rig.click(&rig.button("Step calculator"));
    rig.click(&first_entry_box(&rig));
    rig.click(&rig.button("Analytics"));
    assert!(rig.controller.lock().unwrap().analytics_open());
    assert!(!rig.open());
    rig.escape();
    assert!(
        !rig.controller.lock().unwrap().analytics_open(),
        "Escape closes Analytics opened from an entry box"
    );

    // Nothing open: nothing changes.
    let rig = two_stack_rig("calculator_escape_nothing");
    rig.click(&common::tab(&rig.app, 0));
    rig.escape();
    assert!(!rig.open() && !rig.controller.lock().unwrap().analytics_open());
    assert!(!rig.app.get_calculator_open() && !rig.app.get_analytics_open());

    // A dialog up after a tab click is dismissed first; the overlay stays.
    let rig = two_stack_rig("calculator_escape_dialog");
    rig.click(&rig.button("Step calculator"));
    rig.click(&common::tab(&rig.app, 0));
    // Set after the last pump: a pump resets the flag to the controller's.
    rig.app.set_divergence_open(true);
    rig.escape();
    assert!(rig.open(), "the dialog takes the key, the calculator stays");
    rig.app.set_divergence_open(false);
    rig.pump();
    rig.escape();
    assert!(
        !rig.open(),
        "Escape reaches the calculator once the dialog is gone"
    );
}

#[test]
fn escape_closes_after_a_plus_click() {
    for bell_first in [false, true] {
        let at = format!("notifications first: {bell_first}");
        let rig = two_stack_rig("calculator_escape_plus");
        if bell_first {
            rig.click(&rig.button("Notifications"));
            assert!(rig.app.get_notification_center_open(), "{at}: center opens");
            rig.click(&rig.button("Step calculator")); // opens, closes the center
            rig.click(&rig.button("Step calculator")); // closes
            assert!(!rig.open(), "{at}: calculator closed again");
        }
        rig.click(&common::plus_button(&rig.app));
        rig.click(&rig.button("Step calculator"));
        assert!(rig.open() && rig.app.get_calculator_open(), "{at}: opens");
        rig.escape();
        assert!(!rig.open(), "{at}: Escape closes it");
        assert!(!rig.app.get_calculator_open(), "{at}: button plain");
    }
}

#[test]
fn tab_and_plus_clicks_start_no_window_move() {
    use focusforge_gui::WindowTarget;

    for width in [1440, 1200] {
        let rig = two_stack_rig("calculator_no_window_move");
        rig.app
            .window()
            .set_size(slint::PhysicalSize::new(width, 900));
        rig.pump();
        let calls: Arc<Mutex<Vec<WindowTarget>>> = Arc::new(Mutex::new(Vec::new()));
        let hook = calls.clone();
        rig.controller
            .lock()
            .unwrap()
            .set_native_drag(Arc::new(move |target| hook.lock().unwrap().push(target)));

        rig.click(&common::tab(&rig.app, 0));
        rig.click(&common::tab(&rig.app, 1));
        rig.click(&common::plus_button(&rig.app));
        assert!(
            calls.lock().unwrap().is_empty(),
            "width {width}: a tab or plus click moves no window"
        );

        // The bare strip between the plus box and the window controls.
        let (x, y, w, h) = rect(&rig.app, "FFTabStrip::plus-button");
        let (glyphs_x, ..) = rect(&rig.app, "FFTabStrip::window-glyphs");
        rig.click_at(slint::LogicalPosition::new(
            (x + w + glyphs_x) / 2.0,
            y + h / 2.0,
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![WindowTarget::Main],
            "width {width}: a left press on bare strip starts the move"
        );
    }
}

#[test]
fn escape_closes_after_an_image_row_click() {
    for row in 0..2 {
        let rig = two_stack_rig("calculator_escape_row");
        rig.click(&common::image_row(&rig.app, row));
        rig.click(&rig.button("Step calculator"));
        assert!(
            rig.open() && rig.app.get_calculator_open(),
            "row {row}: opens"
        );

        rig.escape();
        assert!(!rig.open(), "row {row}: Escape closes it");
        assert!(!rig.app.get_calculator_open(), "row {row}: button plain");
    }
}

type Rect = (f32, f32, f32, f32);

/// A window of `size` with the calculator open, `objective` optics chosen and
/// each `(slot, text)` entered.
fn calculator_rig(size: (u32, u32), objective: bool, entries: &[(usize, &str)]) -> Rig {
    let rig = rig_with(AppController::new());
    rig.app
        .window()
        .set_size(slint::PhysicalSize::new(size.0, size.1));
    {
        let mut controller = rig.controller.lock().unwrap();
        controller.toggle_calculator();
        if objective {
            controller.calculator_set_optics("Objective (NA)");
        }
        for (slot, text) in entries {
            controller.calculator_edit(*slot, text);
        }
    }
    rig.pump();
    rig
}

fn drawing_scale(rig: &Rig) -> f32 {
    rig.app.get_calculator_drawing_scale()
}

fn shown(rig: &Rig, slot: usize) -> bool {
    use slint::Model;
    rig.app
        .get_calculator_fields()
        .row_data(slot)
        .unwrap()
        .shown
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn inside(inner: Rect, outer: Rect) -> bool {
    inner.0 >= outer.0
        && inner.1 >= outer.1
        && inner.0 + inner.2 <= outer.0 + outer.2
        && inner.1 + inner.3 <= outer.1 + outer.3
}

/// `rect` with its size multiplied by `scale`; the origin is kept.
fn drawn(rect: Rect, scale: f32) -> Rect {
    (rect.0, rect.1, rect.2 * scale, rect.3 * scale)
}

#[test]
fn calculator_drawing_is_centred_at_two_window_sizes() {
    // (window, scale): the drawing is height-bound at 1440 x 900 (683 / 543)
    // and clamped at the maximum at 1920 x 1200.
    for (window, expected) in [((1440, 900), 1.2578), ((1920, 1200), 1.3)] {
        for objective in [false, true] {
            // The Overlap error grows the card by 20 px and the centred box with it.
            for (entries, grow) in [(vec![], 0.0), (vec![(3, "0.95")], 20.0)] {
                let at = format!("{window:?} objective {objective} grow {grow}");
                let rig = calculator_rig(window, objective, &entries);
                let s = drawing_scale(&rig);
                assert!((s - expected).abs() < 0.001, "{at}: scale {s}");

                // The overlay is the window minus the dock and icon bar
                // (308 px) and the strip, toolbar and bottom bar (120 px).
                let overlay = rect(&rig.app, "AppWindow::calculator-overlay");
                assert_eq!(overlay.2, window.0 as f32 - 308.0, "{at}");
                assert_eq!(overlay.3, window.1 as f32 - 120.0, "{at}");

                let illustration = rect(&rig.app, "FFCalculatorOverlay::illustration");
                let inset = rect(&rig.app, "FFCalculatorOverlay::inset");
                assert_eq!(inset.3, 159.0 + grow, "{at}: card height");

                let left = illustration.0;
                let right = inset.0 + 415.0 * s;
                assert!(
                    ((left + right) / 2.0 - (overlay.0 + overlay.2 / 2.0)).abs() <= 1.0,
                    "{at}: horizontal centre {left}..{right} in {overlay:?}"
                );

                // Body: below the 41 px header and rule, above the 56 px footer;
                // free: above the 84 px summary band.
                let free_height = overlay.3 - 41.0 - 56.0 - 84.0;
                let top = overlay.1 + 41.0 + (free_height - s * (435.0 + grow)) / 2.0;
                assert!(
                    (inset.1 - top).abs() <= 1.0,
                    "{at}: inset top {} against {top}",
                    inset.1
                );
            }
        }
    }
}

#[test]
fn calculator_fields_keep_their_size_while_the_drawing_scales() {
    let mut widths = Vec::new();
    for window in [(1200, 760), (1440, 900), (1920, 1200)] {
        let rig = calculator_rig(window, false, &[]);
        let s = drawing_scale(&rig);
        let illustration = rect(&rig.app, "FFCalculatorOverlay::illustration");
        widths.push(illustration.2 * s);

        for slot in 0..5 {
            let at = format!("{window:?} slot {slot}");
            let slot_rect = rect(&rig.app, &format!("FFCalculatorOverlay::slot-{slot}"));
            assert_eq!((slot_rect.2, slot_rect.3), (56.0, 28.0), "{at}: box");
        }
        let (ox, oy, ow, oh) = rect(&rig.app, "AppWindow::calculator-overlay");
        let inputs: Vec<_> = ElementQuery::from_root(&rig.app)
            .match_id("FFField::input")
            .find_all()
            .into_iter()
            .filter(|b| {
                let p = b.absolute_position();
                p.x >= ox && p.y >= oy && p.x < ox + ow && p.y < oy + oh
            })
            .collect();
        assert_eq!(inputs.len(), 5, "{window:?}: five entry boxes");
        for input in inputs {
            assert_eq!(input.size().height, 28.0, "{window:?}: entry box height");
        }
    }
    assert!(
        (widths[0] - 770.0).abs() < 0.01,
        "{widths:?}: 770 at the minimum"
    );
    assert!(widths[1] > widths[0], "{widths:?}");
    assert!(widths[2] > widths[1], "{widths:?}");
}

#[test]
fn calculator_fields_stay_on_their_leader_lines() {
    // (leader, field end is the bottom end, slot): a leader's 1 px stroke sits
    // 2 px right of its element's left edge.
    let rows = [
        (0, false, 0),
        (2, false, 1),
        (3, true, 2),
        (5, true, 3),
        (4, true, 4),
    ];
    for window in [(1200, 760), (1440, 900), (1600, 760), (1920, 1200)] {
        for objective in [false, true] {
            for entries in [vec![], vec![(3, "0.95")]] {
                let at = format!("{window:?} objective {objective} entries {entries:?}");
                let rig = calculator_rig(window, objective, &entries);
                let s = drawing_scale(&rig);
                for (leader, bottom, slot) in rows {
                    let found = ElementQuery::from_root(&rig.app)
                        .match_id(format!("FFCalculatorOverlay::leader-{leader}"))
                        .find_first();
                    if objective && slot == 1 {
                        assert!(found.is_none(), "{at}: no magnification leader");
                        assert!(!shown(&rig, slot), "{at}: no magnification field");
                        continue;
                    }
                    let line = found.unwrap_or_else(|| panic!("{at}: leader-{leader}"));
                    let (pos, size) = (line.absolute_position(), line.size());
                    let end = (
                        pos.x + 2.0 * s,
                        pos.y + if bottom { size.height * s } else { 0.0 },
                    );
                    let slot_rect = rect(&rig.app, &format!("FFCalculatorOverlay::slot-{slot}"));
                    let grown = (
                        slot_rect.0 - 1.0,
                        slot_rect.1 - 1.0,
                        slot_rect.2 + 2.0,
                        slot_rect.3 + 2.0,
                    );
                    assert!(
                        inside((end.0, end.1, 0.0, 0.0), grown),
                        "{at}: leader-{leader} end {end:?} outside slot {slot} box {slot_rect:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn calculator_scale_is_clamped_and_nothing_overlaps() {
    // Overlap and Depth range violation lines at once.
    let entries = [(3, "0.95"), (4, "abc")];
    for (window, expected) in [((1200, 760), 1.0), ((3840, 2160), 1.3)] {
        for objective in [false, true] {
            let at = format!("{window:?} objective {objective}");
            let rig = calculator_rig(window, objective, &entries);
            let s = drawing_scale(&rig);
            assert_eq!(s, expected, "{at}: scale");

            let inset = drawn(rect(&rig.app, "FFCalculatorOverlay::inset"), s);
            let optics = rect(&rig.app, "FFCalculatorOverlay::optics");
            let summary = rect(&rig.app, "FFCalculatorOverlay::summary");
            for slot in (0..5).filter(|slot| shown(&rig, *slot)) {
                let handle = ElementQuery::from_root(&rig.app)
                    .match_id(format!("FFCalculatorOverlay::slot-{slot}"))
                    .find_first()
                    .unwrap();
                let part = |id: &str| {
                    handle
                        .query_descendants()
                        .match_id(id)
                        .find_first()
                        .map(|h| {
                            let (p, z) = (h.absolute_position(), h.size());
                            (p.x, p.y, z.width, z.height)
                        })
                };
                let pieces = [
                    (
                        "box",
                        Some(rect(&rig.app, &format!("FFCalculatorOverlay::slot-{slot}"))),
                    ),
                    ("caption", part("FieldSlot::caption")),
                    ("violation", part("FieldSlot::violation")),
                ];
                for (name, piece) in pieces {
                    let Some(piece) = piece else { continue };
                    let at = format!("{at}: slot {slot} {name} {piece:?}");
                    assert!(!intersects(piece, optics), "{at}: optics {optics:?}");
                    assert!(!intersects(piece, summary), "{at}: summary {summary:?}");
                    if slot == 3 {
                        // The Overlap field is drawn inside the grown card.
                        if name != "caption" {
                            assert!(inside(piece, inset), "{at}: inside the card {inset:?}");
                        }
                    } else {
                        assert!(!intersects(piece, inset), "{at}: card {inset:?}");
                    }
                }
            }
        }
    }
}
