// SPDX-License-Identifier: GPL-3.0-or-later

//! The settings panel, driven through `AppController` with no window -- the
//! panel model built from the real registry, the way `tests/tab_scroll.rs`
//! already drives the tab strip.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use focusforge_core::params::{ParamKind, ParamValue, ParamValues, ParamViolation};
use focusforge_core::registry;
use focusforge_gui::settings_panel::{
    HeadingState, PANEL_CONTENT_WIDTH, PARAM_ERROR_HEIGHT, PARAM_LABEL_HEIGHT, PARAM_ROW_BOOL,
    PARAM_ROW_ENUM, PARAM_ROW_FIELD, PARAM_ROW_LINEAR, PARAM_ROW_LOG, RESET_ALL_HEIGHT,
    RESET_ALL_TOP_GAP, RowKind, SECTION_HEADING_BOTTOM_GAP, SECTION_HEADING_HEIGHT, SettingsRow,
    control_kind, slider_fraction,
};
use focusforge_gui::{AppController, OpenOutcome};

fn mantis_512() -> PathBuf {
    common::workspace_root().join("fixtures").join("mantis-512")
}

/// Opens the fixture as one project, asserting it landed as one tab.
fn open_mantis_512(controller: &mut AppController) {
    let outcome = common::open_and_settle(controller, &[mantis_512()]);
    assert!(
        matches!(outcome, OpenOutcome::Opened(_)),
        "fixtures/mantis-512 must open as one project"
    );
}

/// Every field row of `section` (no heading, no picker, no error rows), in
/// panel order -- which is declaration order by construction.
fn field_rows<'a>(rows: &'a [SettingsRow], section: &str) -> Vec<&'a SettingsRow> {
    let mut out = Vec::new();
    let mut in_section = false;
    for row in rows {
        if row.kind == RowKind::SectionHeading {
            in_section = row.section == section;
            continue;
        }
        if in_section && row.kind != RowKind::Error && row.kind != RowKind::Picker {
            out.push(row);
        }
    }
    out
}

fn heading<'a>(rows: &'a [SettingsRow], section: &str) -> &'a SettingsRow {
    rows.iter()
        .find(|r| r.kind == RowKind::SectionHeading && r.section == section)
        .unwrap_or_else(|| panic!("no {section} heading row"))
}

#[test]
fn every_param_kind_has_one_control() {
    let linear = ParamKind::Float {
        min: 0.0,
        max: 10.0,
        step: 0.1,
        logarithmic: false,
    };
    let log = ParamKind::Float {
        min: 1e-9,
        max: 1.0,
        step: 1e-5,
        logarithmic: true,
    };
    assert_eq!(control_kind(&linear), RowKind::LinearFloat);
    assert_eq!(control_kind(&log), RowKind::LogFloat);
    assert_ne!(
        control_kind(&linear),
        control_kind(&log),
        "the logarithmic flag must change the control, not just caption it"
    );
    assert_eq!(
        control_kind(&ParamKind::Int { min: 0, max: 10 }),
        RowKind::Int
    );
    assert_eq!(control_kind(&ParamKind::Bool), RowKind::Bool);
    assert_eq!(
        control_kind(&ParamKind::Enum { variants: vec![] }),
        RowKind::Enum
    );
}

#[test]
fn logarithmic_float_positions_by_decade() {
    let epsilon_px = ParamKind::Float {
        min: 1e-9,
        max: 1.0,
        step: 1e-5,
        logarithmic: true,
    };
    let fraction = slider_fraction(1e-4, &epsilon_px);
    assert!(
        (fraction - 0.5556).abs() < 0.001,
        "expected ~0.5556, got {fraction}"
    );

    let linear_reading = ParamKind::Float {
        min: 1e-9,
        max: 1.0,
        step: 1e-5,
        logarithmic: false,
    };
    assert!(slider_fraction(1e-4, &linear_reading) < 0.001);

    // A synthetic descriptor with `min <= 0` must fall back to the linear
    // reading rather than dividing by an infinite log.
    let non_positive_min = ParamKind::Float {
        min: 0.0,
        max: 10.0,
        step: 0.1,
        logarithmic: true,
    };
    assert_eq!(slider_fraction(5.0, &non_positive_min), 0.5);
}

#[test]
fn pyramidal_ecc_renders_in_declaration_order() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_align_algorithm("pyramidal-ecc");

    let ecc = registry::find_align("pyramidal-ecc").expect("pyramidal-ecc must be registered");
    let expected: Vec<String> = ecc.params().iter().map(|d| d.label.clone()).collect();
    let expected_sorted = {
        let mut sorted = expected.clone();
        sorted.sort();
        sorted
    };
    assert_ne!(
        expected, expected_sorted,
        "the schema's own declaration order must differ from its sorted order for this test to prove anything"
    );

    let panel = controller.settings_panel();
    let align_labels: Vec<String> = field_rows(&panel.rows, "align")
        .iter()
        .map(|r| r.label.clone())
        .collect();
    assert_eq!(align_labels, expected);
}

#[test]
fn provenance_is_shown_per_section() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    let panel = controller.settings_panel();
    assert_eq!(
        heading(&panel.rows, "align").heading_state,
        HeadingState::Default
    );

    let ecc = registry::find_align("pyramidal-ecc").unwrap();
    let depth_id = ecc
        .params()
        .iter()
        .find(|d| d.id == "pyramid-depth")
        .unwrap()
        .id
        .clone();
    controller.set_param_value("align", &depth_id, ParamValue::Int(2));

    let panel = controller.settings_panel();
    assert_eq!(
        heading(&panel.rows, "align").heading_state,
        HeadingState::Explicit,
        "one changed value must switch the whole section to explicit"
    );
}

#[test]
fn reset_all_returns_every_section_to_default() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    let ecc = registry::find_align("pyramidal-ecc").unwrap();
    let ids: Vec<String> = ecc.params().iter().map(|d| d.id.clone()).collect();
    controller.set_param_value("align", &ids[1], ParamValue::Int(50));

    let panel = controller.settings_panel();
    assert_eq!(
        heading(&panel.rows, "align").heading_state,
        HeadingState::Explicit
    );

    controller.reset_all_sections();
    let panel = controller.settings_panel();
    assert_eq!(
        heading(&panel.rows, "align").heading_state,
        HeadingState::Default,
        "reset-all must return every section to default"
    );
}

#[test]
fn auto_before_a_stack_is_unevaluated() {
    let controller = AppController::new();
    // No project has ever been opened -- "+" is the only selection there is.
    let panel = controller.settings_panel();
    assert_eq!(
        heading(&panel.rows, "align").heading_state,
        HeadingState::Pending,
        "with no stack context an auto parameter must read as pending, not as a user's choice"
    );
}

#[test]
fn a_loaded_stack_resolves_its_auto_hooks() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_align_algorithm("pyramidal-ecc");

    let ecc = registry::find_align("pyramidal-ecc").unwrap();
    let depth_id = &ecc
        .params()
        .iter()
        .find(|d| d.id == "pyramid-depth")
        .unwrap()
        .id;

    let panel = controller.settings_panel();
    let depth_row = field_rows(&panel.rows, "align")
        .into_iter()
        .find(|r| r.id == *depth_id)
        .expect("pyramid-depth row must be present");
    // fixtures/mantis-512 is 512px, not the 8192px reference stack -- a
    // hardcoded "4" (the descriptor's own default) must not pass here.
    assert_ne!(
        depth_row.value_text, "4",
        "pyramid-depth must resolve against the loaded stack's own dimensions, not the descriptor default"
    );
}

#[test]
fn several_violations_are_shown_at_once() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_align_algorithm("pyramidal-ecc");

    let ecc = registry::find_align("pyramidal-ecc").unwrap();
    let ids: Vec<String> = ecc.params().iter().map(|d| d.id.clone()).collect();
    let max_iterations_id = ids
        .iter()
        .find(|id| id.as_str() == "max-iterations")
        .cloned()
        .expect("max-iterations must exist on pyramidal-ecc");
    let epsilon_id = ids
        .iter()
        .find(|id| id.as_str() == "epsilon-px")
        .cloned()
        .expect("epsilon-px must exist on pyramidal-ecc");

    controller.set_param_text("align", &max_iterations_id, "not-a-number");
    controller.set_param_text("align", &epsilon_id, "also-not-a-number");

    let panel = controller.settings_panel();
    let error_messages: Vec<&str> = panel
        .rows
        .iter()
        .filter(|r| r.kind == RowKind::Error && r.section == "align")
        .map(|r| r.value_text.as_str())
        .collect();
    assert_eq!(
        error_messages.len(),
        2,
        "both offending fields must carry their own message at once: {error_messages:?}"
    );
    for message in &error_messages {
        assert!(
            !message.starts_with("max-iterations") && !message.starts_with("epsilon-px"),
            "a rendered message must not repeat the parameter id: {message:?}"
        );
    }
}

#[test]
fn violation_text_drops_the_id_prefix() {
    use focusforge_gui::settings_panel::violation_text;

    let out_of_range = ParamViolation::OutOfRange {
        id: "levels".to_string(),
        value: 20.0,
        min: 0.0,
        max: 10.0,
    };
    assert!(!violation_text(&out_of_range).starts_with("levels"));

    let unparsable = ParamViolation::Unparsable {
        id: "max-iterations".to_string(),
        kind: "int",
        text: "abc".to_string(),
    };
    assert!(!violation_text(&unparsable).starts_with("max-iterations"));

    let unknown_key = ParamViolation::UnknownKey {
        key: "bogus".to_string(),
        declared: vec!["levels".to_string()],
    };
    assert_eq!(violation_text(&unknown_key), unknown_key.to_string());

    let auto = ParamViolation::Auto {
        id: "pyramid-depth".to_string(),
        source: Box::new(ParamViolation::OutOfRange {
            id: "pyramid-depth".to_string(),
            value: 99.0,
            min: 0.0,
            max: 6.0,
        }),
    };
    let text = violation_text(&auto);
    assert!(!text.contains("pyramid-depth"), "got: {text}");
}

/// Each picker's selected id, `(fuse, align)`.
fn picked_ids(controller: &AppController) -> (String, String) {
    let panel = controller.settings_panel();
    let picked = |section: &str| {
        panel
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Picker && r.section == section)
            .unwrap_or_else(|| panic!("no {section} picker row"))
            .selected
            .clone()
    };
    (picked("fuse"), picked("align"))
}

/// A sealed project recorded with no `align` and fused by `algorithm`.
fn seal_without_align(name: &str, algorithm: &str) -> PathBuf {
    seal_with_params(name, algorithm, &ParamValues::new())
}

/// [`seal_without_align`] recording `params` as the run's explicit settings.
fn seal_with_params(name: &str, algorithm: &str, params: &ParamValues) -> PathBuf {
    seal_with_registration(
        name,
        algorithm,
        params,
        focusforge_core::align::Registration::default(),
    )
}

/// [`seal_with_params`] recording `registration`, the crop a run applied.
fn seal_with_registration(
    name: &str,
    algorithm: &str,
    params: &ParamValues,
    registration: focusforge_core::align::Registration,
) -> PathBuf {
    use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
    use focusforge_core::project::{Metrics, SealRequest, seal};
    use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

    let dir = common::tree(name);
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
    let full = Rect {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    };
    let mut writer = TiffWriter::create(
        &composite,
        TiffSpec {
            width: 4,
            height: 4,
            samples_per_pixel: 3,
            description: Some("staged".to_string()),
        },
    )
    .unwrap();
    writer.write_tile(full, &[0u16; 4 * 4 * 3]).unwrap();
    writer.finish().unwrap();
    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(4, 4, 1).unwrap()).unwrap();
    map.write_tile(full, &[0u16; 4 * 4]).unwrap();
    map.finish().unwrap();
    let project_path = dir.join("test.ffp");
    seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &[],
        algorithm,
        params,
        align: None,
        registration,
        composite_description: "staged".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .unwrap();
    project_path
}

/// The active tab's crop-section `auto-crop` toggle, `"true"` or `"false"`.
fn auto_crop_selected(controller: &AppController) -> String {
    controller
        .settings_panel()
        .rows
        .into_iter()
        .find(|r| r.section == "crop" && r.id == "auto-crop")
        .expect("the crop section has an auto-crop row")
        .selected
}

#[test]
fn reopened_uncropped_project_shows_auto_crop_off() {
    let mut controller = AppController::new();
    let project = seal_without_align("reopened_uncropped", "laplacian");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(auto_crop_selected(&controller), "false");
}

#[test]
fn reopened_cropped_project_shows_auto_crop_on() {
    use focusforge_core::align::Registration;
    use focusforge_core::io::Rect;

    let mut controller = AppController::new();
    let registration = Registration {
        transforms: Vec::new(),
        crop: Some(Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        }),
        equalization: None,
    };
    let project = seal_with_registration(
        "reopened_cropped",
        "laplacian",
        &ParamValues::new(),
        registration,
    );
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(auto_crop_selected(&controller), "true");
}

#[test]
fn reopened_equalized_project_seeds_equalize_and_its_bound() {
    use focusforge_core::align::Registration;
    use focusforge_core::equalize::Equalization;

    let registration = |equalization| Registration {
        transforms: Vec::new(),
        crop: None,
        equalization,
    };
    let row = |controller: &AppController, id: &str| {
        controller
            .settings_panel()
            .rows
            .into_iter()
            .find(|r| r.section == "crop" && r.id == id)
            .unwrap_or_else(|| panic!("the crop section has a {id} row"))
    };

    let mut controller = AppController::new();
    let project = seal_with_registration(
        "reopened_equalized",
        "laplacian",
        &ParamValues::new(),
        registration(Some(Equalization {
            max_gain: 1.5,
            gains: vec![65536, 66000],
        })),
    );
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(row(&controller, "equalize").selected, "true");
    assert_eq!(row(&controller, "equalize-max-gain").value_text, "1.5");

    let mut plain = AppController::new();
    let project = seal_with_registration(
        "reopened_unequalized",
        "laplacian",
        &ParamValues::new(),
        registration(None),
    );
    assert!(matches!(
        common::open_and_settle(&mut plain, &[project]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(row(&plain, "equalize").selected, "false");
    assert_eq!(row(&plain, "equalize-max-gain").value_text, "1.25");
}

fn equalize_selected(controller: &AppController) -> String {
    controller
        .settings_panel()
        .rows
        .into_iter()
        .find(|r| r.section == "crop" && r.id == "equalize")
        .expect("the crop section has an equalize row")
        .selected
}

#[test]
fn the_equalize_row_shows_the_selected_methods_default() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    for (method, shown) in [
        ("depth-map", "true"),
        ("laplacian", "false"),
        ("weighted-average", "false"),
        ("depth-map", "true"),
    ] {
        controller.set_fuse_algorithm(method);
        assert_eq!(equalize_selected(&controller), shown, "{method}");
    }
}

#[test]
fn an_explicit_equalize_choice_and_each_methods_values_survive_switching() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    controller.set_fuse_algorithm("depth-map");
    controller.set_param_value("fuse", "score-radius", ParamValue::Int(7));
    controller.set_param_value("crop", "equalize", ParamValue::Bool(false));
    controller.set_fuse_algorithm("laplacian");
    controller.set_param_value("fuse", "levels", ParamValue::Int(3));
    assert_eq!(equalize_selected(&controller), "false");

    controller.set_fuse_algorithm("depth-map");
    assert_eq!(equalize_selected(&controller), "false", "the choice stays");
    assert_eq!(fuse_value(&controller, "score-radius"), "7");
    controller.set_fuse_algorithm("laplacian");
    assert_eq!(fuse_value(&controller, "levels"), "3");

    controller.reset_all_sections();
    controller.set_fuse_algorithm("depth-map");
    assert_eq!(equalize_selected(&controller), "true", "reset restores it");
}

#[test]
fn a_reopened_focus_map_project_shows_how_its_run_was_equalized() {
    use focusforge_core::align::Registration;
    use focusforge_core::equalize::Equalization;

    let registration = |equalization| Registration {
        transforms: Vec::new(),
        crop: None,
        equalization,
    };
    let mut equalized = AppController::new();
    let project = seal_with_registration(
        "reopened_focus_map_equalized",
        "depth-map",
        &ParamValues::new(),
        registration(Some(Equalization {
            max_gain: 1.5,
            gains: vec![65536, 66000],
        })),
    );
    assert!(matches!(
        common::open_and_settle(&mut equalized, &[project]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(equalize_selected(&equalized), "true");

    let mut plain = AppController::new();
    let project = seal_with_registration(
        "reopened_focus_map_unequalized",
        "depth-map",
        &ParamValues::new(),
        registration(None),
    );
    assert!(matches!(
        common::open_and_settle(&mut plain, &[project]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(
        equalize_selected(&plain),
        "false",
        "a run that was not equalized does not reopen as equalized"
    );
}

#[test]
fn seeded_ids_are_never_test_only() {
    let is_test_only = |id: &str| {
        registry::find(id)
            .unwrap_or_else(|e| panic!("seeded id {id:?} must resolve: {e}"))
            .test_only()
    };

    let mut unsaved = AppController::new();
    open_mantis_512(&mut unsaved);
    let (fuse, align) = picked_ids(&unsaved);
    assert!(!is_test_only(&fuse), "new tab seeded {fuse}");
    assert!(!is_test_only(&align), "new tab seeded {align}");

    let mut opened = AppController::new();
    let project = seal_without_align("seeded_ids_no_align", "laplacian");
    assert!(matches!(
        common::open_and_settle(&mut opened, &[project]),
        OpenOutcome::Opened(_)
    ));
    let (fuse, align) = picked_ids(&opened);
    assert_eq!(
        fuse, "laplacian",
        "a manifest's own fuse id is kept verbatim"
    );
    assert!(
        !is_test_only(&align),
        "a manifest without align seeded {align}"
    );
}

#[test]
fn pickers_list_the_registry() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let panel = controller.settings_panel();

    let fuse_picker = panel
        .rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "fuse")
        .expect("fuse picker row must exist");
    let mut expected: Vec<String> = registry::stack_algorithms()
        .iter()
        .map(|a| a.id().to_string())
        .collect();
    expected.sort();
    let mut got = fuse_picker.choice_ids.clone();
    got.sort();
    assert_eq!(got, expected);

    let align_picker = panel
        .rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "align")
        .expect("align picker row must exist");
    let mut expected_align: Vec<String> = registry::align_algorithms()
        .iter()
        .map(|a| a.id().to_string())
        .collect();
    expected_align.sort();
    let mut got_align = align_picker.choice_ids.clone();
    got_align.sort();
    assert_eq!(got_align, expected_align);
    assert!(
        !got_align.is_empty(),
        "even a single-entry family must open"
    );
}

/// Renders the real panel headless, then rewrites one row's own label and
/// choices to 200 characters and reads its rendered width back -- proving
/// `overflow: elide` (`ff_field.slint`) actually holds the row at
/// `PANEL_CONTENT_WIDTH`, not just that the constant itself does not move.
#[test]
fn layout_survives_a_long_label() {
    use i_slint_backend_testing::ElementQuery;
    use slint::{ComponentHandle, Model as _};

    thread_local! {
        static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INIT.with(|done| {
        if !done.get() {
            i_slint_backend_testing::init_no_event_loop();
            done.set(true);
        }
    });

    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_project_loaded(true);
    app.set_dock_tab("settings".into());

    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    // A default-seeded id-sorted pick would land on `dummy-align` under a
    // feature-unified workspace build, whose synthetic schema carries no
    // enum field at all -- pin the real one explicitly, the way every
    // sibling test in this file already does.
    controller.set_align_algorithm("pyramidal-ecc");
    focusforge_gui::bridge::push_settings_panel(&app, &controller);

    let rows = app.get_settings_rows();
    let enum_index = (0..rows.row_count())
        .find(|&i| rows.row_data(i).unwrap().kind == "enum")
        .expect("the align schema carries an enum field");
    let long = "x".repeat(200);
    let mut row = rows.row_data(enum_index).unwrap();
    row.label = long.clone().into();
    row.value_text = long.clone().into();
    row.choices = std::rc::Rc::new(slint::VecModel::from(vec![long.into()])).into();
    rows.set_row_data(enum_index, row);

    let width = ElementQuery::from_root(&app)
        .match_id("FFSettingsPanel::row-area")
        .find_all()
        .get(enum_index)
        .unwrap_or_else(|| panic!("no row-area rendered at index {enum_index}"))
        .size()
        .width;
    assert_eq!(
        width, PANEL_CONTENT_WIDTH,
        "a 200-character label must not widen its own row"
    );
}

#[test]
fn panel_scrolls_and_the_heading_sticks() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_settings_viewport_height(200.0);

    let panel = controller.settings_panel();
    assert!(
        panel.content_height > panel.viewport_height,
        "pyramidal-ecc's six parameters plus fuse and crop must not fit a 200px viewport"
    );

    controller.scroll_settings_panel(1000.0);
    assert!(
        controller.settings_scroll_y() > 0.0,
        "the panel must have scrolled"
    );
    let panel = controller.settings_panel();
    assert!(
        panel.sticky.is_some(),
        "a heading must be stuck after scrolling past it"
    );
}

fn init_window_backend() {
    thread_local! {
        static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INIT.with(|done| {
        if !done.get() {
            i_slint_backend_testing::init_no_event_loop();
            done.set(true);
        }
    });
}

fn mounted(app: &focusforge_gui::AppWindow, id: &str) -> i_slint_backend_testing::ElementHandle {
    i_slint_backend_testing::ElementQuery::from_root(app)
        .match_id(id)
        .find_first()
        .unwrap_or_else(|| panic!("{id} is mounted"))
}

fn element_center(handle: &i_slint_backend_testing::ElementHandle) -> slint::LogicalPosition {
    let pos = handle.absolute_position();
    let size = handle.size();
    slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0)
}

/// Indices whose shown row differs between `old` and `new`, over the rows
/// both hold; a row's `choices` compare by model identity.
fn changed_rows<T: PartialEq>(old: &[T], new: &[T]) -> Vec<usize> {
    old.iter()
        .zip(new)
        .enumerate()
        .filter(|(_, (o, n))| o != n)
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn settings_scrolling_notifies_no_row_and_edits_notify_only_changed_rows() {
    use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
    use focusforge_gui::ShellEvent;
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{ComponentHandle, Model};
    use std::sync::{Arc, Mutex};

    // Timing accounting is process-global, so this runs alone in a child.
    const CHILD: &str = "FOCUSFORGE_GUI_SETTINGS_ACCOUNTING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "--exact",
            "settings_scrolling_notifies_no_row_and_edits_notify_only_changed_rows",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .status()
        .expect("the isolated accounting test starts");
        assert!(
            status.success(),
            "the isolated accounting test passes: {status}"
        );
        return;
    }

    enable_trace_accounting();
    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_align_algorithm("pyramidal-ecc");
    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.lock().unwrap().set_sink(Arc::new({
        let events = events.clone();
        move |event| events.lock().unwrap().push(event)
    }));
    app.set_project_loaded(true);
    app.set_dock_tab("settings".into());

    // What the event loop does with each captured event.
    let perform = || {
        let drained: Vec<ShellEvent> = events.lock().unwrap().drain(..).collect();
        for event in &drained {
            let controller = controller.lock().unwrap();
            match event {
                ShellEvent::SettingsScrolled => {
                    focusforge_gui::bridge::push_settings_scroll(&app, &controller.settings_panel())
                }
                ShellEvent::SettingsChanged => {
                    focusforge_gui::bridge::push_settings_panel(&app, &controller)
                }
                _ => {}
            }
        }
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
        drained
    };
    let dispatch = |event: WindowEvent| {
        app.window().dispatch_event(event);
        perform()
    };
    let row_updates = || {
        timing_snapshot()
            .expect("trace accounting is enabled")
            .row_updates
    };
    let shown = || app.get_settings_rows().iter().collect::<Vec<_>>();
    let assert_scroll_mirrored = || {
        let panel = controller.lock().unwrap().settings_panel();
        assert_eq!(app.get_settings_scroll_y(), panel.scroll_y);
        let sticky = app.get_settings_sticky();
        assert_eq!(sticky.visible, panel.sticky.is_some());
        if let Some(i) = panel.sticky {
            assert_eq!(sticky.label.as_str(), panel.rows[i].label);
            assert_eq!(sticky.push, panel.sticky_push);
        }
        panel.sticky_push != 0.0
    };

    focusforge_gui::bridge::push_settings_panel(&app, &controller.lock().unwrap());
    perform();
    controller
        .lock()
        .unwrap()
        .set_settings_viewport_height(200.0);
    perform();
    let before = row_updates();
    let model = app.get_settings_rows();

    // Wheel down in small steps, so the sticky heading's push is crossed.
    let hit_center = element_center(&mounted(&app, "FFSettingsPanel::hit-area"));
    let mut seen = Vec::new();
    let mut pushed = false;
    loop {
        let offset = controller.lock().unwrap().settings_scroll_y();
        seen.extend(dispatch(WindowEvent::PointerScrolled {
            position: hit_center,
            delta_x: 0.0,
            delta_y: -4.0,
        }));
        pushed |= assert_scroll_mirrored();
        if controller.lock().unwrap().settings_scroll_y() == offset {
            break;
        }
    }
    assert!(
        controller.lock().unwrap().settings_scroll_y() > 0.0,
        "the wheel moved the panel"
    );
    assert!(pushed, "some step pushed the sticky heading up");

    // A press on the bare track above the thumb jumps back to the top.
    let bar = mounted(&app, "FFSettingsPanel::scrollbar");
    let top = slint::LogicalPosition::new(element_center(&bar).x, bar.absolute_position().y + 1.0);
    for event in [
        WindowEvent::PointerMoved { position: top },
        WindowEvent::PointerPressed {
            position: top,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position: top,
            button: PointerEventButton::Left,
        },
    ] {
        seen.extend(dispatch(event));
    }
    assert_eq!(controller.lock().unwrap().settings_scroll_y(), 0.0);
    assert_scroll_mirrored();

    controller
        .lock()
        .unwrap()
        .set_settings_viewport_height(260.0);
    seen.extend(perform());
    assert_scroll_mirrored();

    assert_eq!(row_updates(), before, "scrolling notifies no row");
    assert!(!seen.contains(&ShellEvent::SettingsChanged), "{seen:?}");
    assert!(seen.contains(&ShellEvent::SettingsScrolled), "{seen:?}");

    // Each edit notifies exactly the rows whose shown data differ.
    let edit = |change: &dyn Fn(&mut AppController)| {
        let old = shown();
        change(&mut controller.lock().unwrap());
        let before = row_updates();
        perform();
        let new = shown();
        let changed = changed_rows(&old, &new);
        let touched = changed.len() + old.len().abs_diff(new.len());
        assert_eq!(row_updates() - before, touched as u64);
        assert!(app.get_settings_rows() == model, "the row model is kept");
        (old, new, changed)
    };
    let slider = shown()
        .iter()
        .position(|r| ["linear-float", "log-float", "int"].contains(&r.kind.as_str()))
        .expect("pyramidal-ecc has a slider");
    let (section, id, start) = {
        let row = &shown()[slider];
        (row.section.to_string(), row.id.to_string(), row.fraction)
    };
    let far = if start < 0.5 { 0.9 } else { 0.1 };
    let (_, _, first) = edit(&|c| c.set_param_fraction(&section, &id, far));
    assert!(first.contains(&slider), "{first:?}");
    let (_, _, second) = edit(&|c| c.set_param_fraction(&section, &id, 0.5));
    assert_eq!(second, vec![slider]);

    let menu = shown()
        .iter()
        .position(|r| r.kind == "enum")
        .expect("pyramidal-ecc has an enum field");
    let (section, id) = {
        let row = &shown()[menu];
        (row.section.to_string(), row.id.to_string())
    };
    let (old, new, toggled) = edit(&|c| c.toggle_param_menu_for(&section, &id));
    assert_eq!(toggled, vec![menu]);
    assert!(new[menu].menu_open);
    assert!(new[menu].choices == old[menu].choices, "choices kept");

    let mut resized = false;
    for algorithm in focusforge_core::registry::align_algorithms() {
        if algorithm.id() == "pyramidal-ecc" {
            continue;
        }
        let (old, new, _) = edit(&|c| c.set_align_algorithm(algorithm.id()));
        resized |= old.len() != new.len();
    }
    assert!(resized, "some align algorithm changes the row count");
}

/// A slider drag keeps its grab across a panel push between two moves.
#[test]
fn a_slider_drag_survives_a_panel_push() {
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_align_algorithm("pyramidal-ecc");
    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    app.set_dock_tab("settings".into());
    let push = || {
        focusforge_gui::bridge::push_settings_panel(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    };
    push();
    let fractions = || {
        controller
            .lock()
            .unwrap()
            .settings_panel()
            .rows
            .iter()
            .map(|r| r.fraction)
            .collect::<Vec<_>>()
    };

    let area = mounted(&app, "FFSlider::drag-area");
    let (pos, size) = (area.absolute_position(), area.size());
    let at =
        |f: f32| slint::LogicalPosition::new(pos.x + size.width * f, pos.y + size.height / 2.0);
    let start = fractions();
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: at(0.2) });
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: at(0.2),
        button: PointerEventButton::Left,
    });
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: at(0.4) });
    let first = fractions();
    assert_ne!(first, start, "the first move changes the value");
    push();
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: at(0.7) });
    let second = fractions();
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: at(0.7),
        button: PointerEventButton::Left,
    });
    assert_ne!(second, first, "the move after the push still drags");
}

#[test]
fn scrollbar_is_a_shared_primitive() {
    let contents = fs::read_to_string(
        common::ui_dir()
            .join("components")
            .join("ff_scrollbar.slint"),
    )
    .unwrap();
    let code = common::strip_line_comments(&contents);
    assert_eq!(
        code.matches("export component").count(),
        1,
        "ff_scrollbar.slint must export exactly one component"
    );
    assert!(code.contains("export component FFScrollbar"));
    for shell_import in ["shell/", "\"../shell"] {
        assert!(
            !code.contains(shell_import),
            "a primitive must import no shell file"
        );
    }
}

#[test]
fn auto_crop_defaults_on_and_is_a_reversible_flag() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    let panel = controller.settings_panel();
    let crop_row = field_rows(&panel.rows, "crop")
        .into_iter()
        .find(|r| r.id == "auto-crop")
        .expect("auto-crop row must be present");
    assert_eq!(crop_row.kind, RowKind::Bool);
    assert_eq!(crop_row.selected, "true", "auto-crop must default on");

    controller.set_param_value("crop", "auto-crop", ParamValue::Bool(false));
    let panel = controller.settings_panel();
    let crop_row = field_rows(&panel.rows, "crop")
        .into_iter()
        .find(|r| r.id == "auto-crop")
        .unwrap();
    assert_eq!(crop_row.selected, "false");

    controller.set_param_value("crop", "auto-crop", ParamValue::Bool(true));
    let panel = controller.settings_panel();
    let crop_row = field_rows(&panel.rows, "crop")
        .into_iter()
        .find(|r| r.id == "auto-crop")
        .unwrap();
    assert_eq!(
        crop_row.selected, "true",
        "toggling off and back on must return to the starting state"
    );
}

/// `job_running()` is currently hardcoded `false`, with no way to drive a
/// real job; what matters here is the wiring from one bool to the *whole*
/// panel -- opacity and the scroll/click hit-area together, not a per-field
/// disable -- so the property is driven directly instead.
#[test]
fn panel_disable_is_whole_panel() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;

    let controller = AppController::new();
    assert!(
        !controller.job_running(),
        "job_running must read false with no job running -- the panel reads it as a whole-panel disable"
    );

    thread_local! {
        static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INIT.with(|done| {
        if !done.get() {
            i_slint_backend_testing::init_no_event_loop();
            done.set(true);
        }
    });
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_project_loaded(true);
    app.set_dock_tab("settings".into());

    let opacity = || {
        ElementQuery::from_root(&app)
            .match_id("FFDock::settings-panel")
            .find_first()
            .expect("a loaded project on the settings tab always shows the panel")
            .computed_opacity()
    };

    app.set_settings_enabled(true);
    assert_eq!(opacity(), 1.0, "enabled draws at full opacity");
    app.set_settings_enabled(false);
    assert_eq!(
        opacity(),
        0.5,
        "disabling reads as one whole-panel opacity drop, not a per-field one"
    );
}

#[test]
fn panel_metrics_mirror_the_slint_geometry() {
    let geometry = fs::read_to_string(common::ui_dir().join("geometry.slint")).unwrap();
    for (rust_value, slint_needle) in [
        (PARAM_LABEL_HEIGHT, "param-label-height: 17px"),
        (PARAM_ROW_LINEAR, "param-row-linear: 43px"),
        (PARAM_ROW_LOG, "param-row-log: 60px"),
        (PARAM_ROW_FIELD, "param-row-field: 45px"),
        (PARAM_ROW_BOOL, "param-row-bool: 20px"),
        (PARAM_ROW_ENUM, "param-row-enum: 43px"),
        (PARAM_ERROR_HEIGHT, "param-error-height: 16px"),
        (SECTION_HEADING_HEIGHT, "section-heading-height: 41px"),
        (
            SECTION_HEADING_BOTTOM_GAP,
            "section-heading-bottom-gap: 8px",
        ),
        (PANEL_CONTENT_WIDTH, "panel-content-width: 228px"),
        (RESET_ALL_TOP_GAP, "reset-all-top-gap: 32px"),
        (RESET_ALL_HEIGHT, "reset-all-height: 32px"),
    ] {
        assert!(
            geometry.contains(slint_needle),
            "geometry.slint must declare {slint_needle:?}"
        );
        assert_eq!(
            rust_value,
            slint_needle
                .split(": ")
                .nth(1)
                .unwrap()
                .trim_end_matches("px")
                .parse::<f32>()
                .unwrap()
        );
    }
}

#[test]
fn no_algorithm_id_in_the_gui() {
    let ids: Vec<String> = registry::algorithms()
        .into_iter()
        .map(|a| a.id().to_string())
        .collect();
    assert!(!ids.is_empty());

    let mut offenders = Vec::new();
    for path in common::find_rust_files(&common::src_dir()) {
        let contents = fs::read_to_string(&path).unwrap();
        let code = common::strip_line_comments(&contents);
        for id in &ids {
            if code.contains(id.as_str()) {
                offenders.push(format!("{}: {id}", path.display()));
            }
        }
    }
    for path in common::find_slint_files(&common::ui_dir()) {
        let contents = fs::read_to_string(&path).unwrap();
        let code = common::strip_line_comments(&contents);
        for id in &ids {
            if code.contains(id.as_str()) {
                offenders.push(format!("{}: {id}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "no algorithm id may appear anywhere in crates/gui: {offenders:?}"
    );
}

/// The fuse row `id`'s shown value.
fn fuse_value(controller: &AppController, id: &str) -> String {
    let panel = controller.settings_panel();
    field_rows(&panel.rows, "fuse")
        .into_iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no fuse row {id}"))
        .value_text
        .clone()
}

/// A fuse parameter's declared default, as the row shows it.
fn fuse_default_text(algorithm: &str, id: &str) -> String {
    let desc = registry::find(algorithm)
        .expect("registered")
        .params()
        .iter()
        .find(|d| d.id == id)
        .expect("declared")
        .default
        .clone();
    match desc {
        ParamValue::Int(v) => v.to_string(),
        ParamValue::Float(v) => format!("{v}"),
        other => panic!("unexpected default {other:?}"),
    }
}

fn reset_all_enabled(controller: &AppController) -> bool {
    controller
        .settings_panel()
        .rows
        .iter()
        .find(|r| r.kind == RowKind::ResetAll)
        .expect("the reset-all row")
        .selected
        == "true"
}

#[test]
fn new_tab_and_empty_panel_default_to_details() {
    let default = registry::default_stack().expect("a default is marked");
    assert_eq!(default.id(), "laplacian");

    let fuse_picker = |controller: &AppController| {
        controller
            .settings_panel()
            .rows
            .into_iter()
            .find(|r| r.kind == RowKind::Picker && r.section == "fuse")
            .expect("fuse picker row")
    };

    let empty = AppController::new();
    let picker = fuse_picker(&empty);
    assert_eq!(picker.selected, default.id());
    assert_eq!(picker.value_text, default.display_name());

    let mut tab = AppController::new();
    open_mantis_512(&mut tab);
    let picker = fuse_picker(&tab);
    assert_eq!(picker.selected, default.id());
    assert_eq!(picker.value_text, default.display_name());
}

#[test]
fn switching_methods_remembers_each_methods_values() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    controller.set_fuse_algorithm("laplacian");
    controller.set_param_value("fuse", "levels", ParamValue::Int(3));
    controller.set_fuse_algorithm("weighted-average");
    controller.set_param_value("fuse", "weight-exponent", ParamValue::Float(3.0));
    controller.set_fuse_algorithm("depth-map");
    controller.set_param_value("fuse", "score-radius", ParamValue::Int(7));

    for _ in 0..2 {
        controller.set_fuse_algorithm("laplacian");
        assert_eq!(fuse_value(&controller, "levels"), "3");
        controller.set_fuse_algorithm("weighted-average");
        assert_eq!(fuse_value(&controller, "weight-exponent"), "3");
        assert_eq!(
            fuse_value(&controller, "radius"),
            fuse_default_text("weighted-average", "radius"),
            "a value never set stays at its default"
        );
        controller.set_fuse_algorithm("depth-map");
        assert_eq!(fuse_value(&controller, "score-radius"), "7");
        assert_eq!(
            fuse_value(&controller, "smoothing-radius"),
            fuse_default_text("depth-map", "smoothing-radius")
        );
    }
}

#[test]
fn a_violation_is_not_remembered_with_a_method() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let has_error = |c: &AppController| {
        c.settings_panel()
            .rows
            .iter()
            .any(|r| r.kind == RowKind::Error && r.section == "fuse")
    };

    controller.set_fuse_algorithm("depth-map");
    controller.set_param_value("fuse", "smoothing-radius", ParamValue::Int(2));
    controller.set_param_text("fuse", "score-radius", "99");
    assert!(has_error(&controller), "the typed violation shows");

    controller.set_fuse_algorithm("weighted-average");
    assert!(!has_error(&controller), "the violation stays behind");
    controller.set_fuse_algorithm("depth-map");
    assert!(!has_error(&controller), "and is not restored");
    assert_eq!(
        fuse_value(&controller, "score-radius"),
        fuse_default_text("depth-map", "score-radius"),
        "the rejected text was never a value"
    );
    assert_eq!(
        fuse_value(&controller, "smoothing-radius"),
        "2",
        "the valid value beside it is kept"
    );
}

#[test]
fn reselecting_the_current_method_keeps_its_values() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_fuse_algorithm("weighted-average");
    controller.set_param_value("fuse", "radius", ParamValue::Int(3));

    controller.set_fuse_algorithm("weighted-average");

    assert_eq!(fuse_value(&controller, "radius"), "3");
    assert_eq!(
        heading(&controller.settings_panel().rows, "fuse").heading_state,
        HeadingState::Explicit
    );
}

#[test]
fn reset_all_forgets_remembered_methods() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_fuse_algorithm("weighted-average");
    controller.set_param_value("fuse", "weight-exponent", ParamValue::Float(3.0));
    controller.set_fuse_algorithm("laplacian");

    assert_eq!(
        heading(&controller.settings_panel().rows, "fuse").heading_state,
        HeadingState::Default,
        "the shown method itself has no value"
    );
    assert!(
        reset_all_enabled(&controller),
        "a remembered value keeps reset-all enabled"
    );

    controller.reset_all_sections();
    assert!(!reset_all_enabled(&controller));
    controller.set_fuse_algorithm("weighted-average");
    assert_eq!(
        fuse_value(&controller, "weight-exponent"),
        fuse_default_text("weighted-average", "weight-exponent")
    );
}

#[test]
fn reopened_project_remembers_only_its_sealed_method() {
    let mut params = ParamValues::new();
    params.insert("weight-exponent".to_string(), ParamValue::Float(5.0));
    let project = seal_with_params("reopen_remembers_sealed", "weighted-average", &params);
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project]),
        OpenOutcome::Opened(_)
    ));

    assert_eq!(picked_ids(&controller).0, "weighted-average");
    assert_eq!(fuse_value(&controller, "weight-exponent"), "5");

    controller.set_fuse_algorithm("laplacian");
    assert_eq!(
        heading(&controller.settings_panel().rows, "fuse").heading_state,
        HeadingState::Default,
        "another method starts from defaults"
    );
    controller.set_fuse_algorithm("depth-map");
    assert_eq!(
        fuse_value(&controller, "score-radius"),
        fuse_default_text("depth-map", "score-radius")
    );
    controller.set_fuse_algorithm("weighted-average");
    assert_eq!(
        fuse_value(&controller, "weight-exponent"),
        "5",
        "the sealed method's values come back"
    );
}
