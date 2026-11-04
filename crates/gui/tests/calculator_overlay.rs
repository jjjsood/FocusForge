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
    thread_local! {
        static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INIT.with(|done| {
        if !done.get() {
            i_slint_backend_testing::init_no_event_loop();
            done.set(true);
        }
    });
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

    /// Pushes the controller's calculator state to the window, as the
    /// bridge does on the event loop this headless window does not run.
    fn pump(&self) {
        focusforge_gui::bridge::push_calculator(&self.app, &self.controller.lock().unwrap());
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
