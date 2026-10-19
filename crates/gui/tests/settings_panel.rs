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
    HeadingState, MORE_GAP, MORE_HEIGHT, PANEL_CONTENT_WIDTH, PARAM_ERROR_HEIGHT,
    PARAM_LABEL_HEIGHT, PARAM_ROW_BOOL, PARAM_ROW_BOOL_GAP, PARAM_ROW_ENUM, PARAM_ROW_FIELD,
    PARAM_ROW_LINEAR, PARAM_ROW_LOG, RESET_ALL_HEIGHT, RESET_ALL_TOP_GAP, RowKind,
    SECTION_HEADING_BOTTOM_GAP, SECTION_HEADING_HEIGHT, SettingsRow, control_kind, slider_fraction,
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

/// Every field row of `section` (no heading, picker, error or fold-out rows),
/// in panel order.
fn field_rows<'a>(rows: &'a [SettingsRow], section: &str) -> Vec<&'a SettingsRow> {
    let mut out = Vec::new();
    let mut in_section = false;
    for row in rows {
        if row.kind == RowKind::SectionHeading {
            in_section = row.section == section;
            continue;
        }
        if in_section
            && !matches!(
                row.kind,
                RowKind::Error
                    | RowKind::Picker
                    | RowKind::More
                    | RowKind::MoreEnd
                    | RowKind::ResetAll
            )
        {
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
    controller.toggle_section_more("align");
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
    let mut with_switch = vec!["Enabled".to_string()];
    with_switch.extend(expected);
    assert_eq!(align_labels, with_switch);
}

#[test]
fn align_lists_enabled_first_and_finish_lists_auto_crop_only() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let panel = controller.settings_panel();

    let align = field_rows(&panel.rows, "align");
    assert_eq!(
        (
            align[0].id.as_str(),
            align[0].kind,
            align[0].selected.as_str()
        ),
        ("align", RowKind::Bool, "true"),
        "the switch heads the section and defaults on"
    );
    assert_eq!(
        align[0].section, "align-switch",
        "the switch writes the project map"
    );
    let finish: Vec<&str> = field_rows(&panel.rows, "finish")
        .iter()
        .filter(|r| r.kind != RowKind::ResetAll)
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(finish, ["auto-crop"]);
}

#[test]
fn the_enabled_switch_writes_only_the_project_values() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_param_value("align-switch", "align", ParamValue::Bool(false));

    let panel = controller.settings_panel();
    assert_eq!(field_rows(&panel.rows, "align")[0].selected, "false");
    assert!(
        panel
            .rows
            .iter()
            .any(|r| r.kind == RowKind::ResetAll && r.selected == "true"),
        "the switch alone enables Reset All"
    );
    assert_eq!(
        heading(&panel.rows, "finish").heading_state,
        HeadingState::Default,
        "no other section reads the switch"
    );
}

#[test]
fn enabled_switches_drive_their_heading() {
    // (section whose switch is written, the switch's section, its id)
    for (heading_section, switch_section, id) in [
        ("align", "align-switch", "align"),
        ("prepare", "prepare", "equalize"),
    ] {
        let mut controller = AppController::new();
        open_mantis_512(&mut controller);
        assert_eq!(
            heading(&controller.settings_panel().rows, heading_section).heading_state,
            HeadingState::Default,
            "{id}: untouched"
        );
        assert!(
            !reset_all_enabled(&controller),
            "{id}: nothing to reset yet"
        );

        controller.set_param_value(switch_section, id, ParamValue::Bool(false));
        assert_eq!(
            heading(&controller.settings_panel().rows, heading_section).heading_state,
            HeadingState::Explicit,
            "{id}: writing the switch makes the heading explicit"
        );
        assert!(reset_all_enabled(&controller), "{id}: Reset All follows");

        controller.set_param_value(switch_section, id, ParamValue::Bool(true));
        assert_eq!(
            heading(&controller.settings_panel().rows, heading_section).heading_state,
            HeadingState::Explicit,
            "{id}: provenance is key presence, so writing the default back keeps it"
        );

        controller.reset_all_sections();
        assert_eq!(
            heading(&controller.settings_panel().rows, heading_section).heading_state,
            HeadingState::Default,
            "{id}: Reset All restores the badge"
        );
        assert!(!reset_all_enabled(&controller), "{id}: Reset All disarms");
    }
}

#[test]
fn the_align_rows_are_disabled_exactly_while_the_switch_is_off() {
    /// `(is the Enabled switch, disabled)` for every row between the ALIGN
    /// heading and the next heading, the picker included; every other row
    /// (headings too) is returned apart.
    fn split(controller: &AppController) -> (Vec<(bool, bool)>, Vec<bool>) {
        let (mut in_align, mut align, mut others) = (false, Vec::new(), Vec::new());
        for row in controller.settings_panel().rows {
            if row.kind == RowKind::SectionHeading {
                in_align = row.section == "align";
                others.push(row.disabled);
            } else if in_align {
                align.push((
                    row.section == "align-switch" && row.id == "align",
                    row.disabled,
                ));
            } else {
                others.push(row.disabled);
            }
        }
        (align, others)
    }
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    // The open fold-out's `More...` header and nested rows grey as well.
    controller.toggle_section_more("align");

    let (align, others) = split(&controller);
    assert!(align.iter().all(|(_, off)| !off) && others.iter().all(|off| !off));

    controller.set_param_value("align-switch", "align", ParamValue::Bool(false));
    let (align, others) = split(&controller);
    assert!(align.len() > 2, "the section has rows to disable");
    assert_eq!(align.iter().filter(|(switch, _)| *switch).count(), 1);
    assert!(
        align.iter().all(|(switch, off)| *off != *switch),
        "every ALIGN row but the switch is disabled"
    );
    assert!(
        others.iter().all(|off| !off),
        "PREPARE, FUSE, FINISH and headings stay active"
    );
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
    controller.toggle_section_more("align");
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

/// The Pyramid Depth row reads what `stack --dry-run` prints for the same
/// frames: 0 for a 64 px frame, 3 for a 512 px frame, neither an explicit
/// choice.
#[test]
fn pyramid_depth_matches_dry_run() {
    let mantis_64 = common::workspace_root().join("fixtures").join("mantis-64");
    for (frames, expected) in [(mantis_64, "0"), (mantis_512(), "3")] {
        let mut controller = AppController::new();
        assert!(matches!(
            common::open_and_settle(&mut controller, std::slice::from_ref(&frames)),
            OpenOutcome::Opened(_)
        ));
        controller.toggle_section_more("align");
        controller.set_align_algorithm("pyramidal-ecc");
        let panel = controller.settings_panel();
        let depth_row = field_rows(&panel.rows, "align")
            .into_iter()
            .find(|r| r.id == "pyramid-depth")
            .expect("pyramid-depth row must be present");
        assert_eq!(depth_row.value_text, expected, "{}", frames.display());
        assert_ne!(
            heading(&panel.rows, "align").heading_state,
            HeadingState::Explicit
        );
    }
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
        .find(|r| r.section == "finish" && r.id == "auto-crop")
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
fn a_reopened_project_without_an_align_record_opens_with_align_explicit() {
    let mut controller = AppController::new();
    let project = seal_without_align("reopened_unaligned_heading", "laplacian");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(
        heading(&controller.settings_panel().rows, "align").heading_state,
        HeadingState::Explicit
    );
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
            .find(|r| r.section == "prepare" && r.id == id)
            .unwrap_or_else(|| panic!("the prepare section has a {id} row"))
    };

    let mut controller = AppController::new();
    let project = seal_with_registration(
        "reopened_equalized",
        "laplacian",
        &ParamValues::new(),
        registration(Some(Equalization {
            max_gain: 1.5,
            target_stops: 0.3,
            gains: vec![65536, 66000],
        })),
    );
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project]),
        OpenOutcome::Opened(_)
    ));
    controller.toggle_section_more("prepare");
    assert_eq!(row(&controller, "equalize").selected, "true");
    assert_eq!(row(&controller, "equalize-max-gain").value_text, "1.5");
    assert_eq!(row(&controller, "equalize-target").value_text, "+0.3");

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
    plain.toggle_section_more("prepare");
    assert_eq!(row(&plain, "equalize").selected, "false");
    assert_eq!(row(&plain, "equalize-max-gain").value_text, "1.25");
    assert_eq!(row(&plain, "equalize-target").value_text, "0.0");
}

fn equalize_selected(controller: &AppController) -> String {
    controller
        .settings_panel()
        .rows
        .into_iter()
        .find(|r| r.section == "prepare" && r.id == "equalize")
        .expect("the prepare section has an equalize row")
        .selected
}

#[test]
fn the_equalize_row_is_on_for_every_method() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("prepare");

    for method in ["depth-map", "laplacian", "weighted-average", "depth-map"] {
        controller.set_fuse_algorithm(method);
        assert_eq!(equalize_selected(&controller), "true", "{method}");
    }
}

#[test]
fn an_explicit_equalize_choice_and_each_methods_values_survive_switching() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("fuse");
    controller.toggle_section_more("prepare");

    controller.set_fuse_algorithm("depth-map");
    controller.set_param_value("fuse", "score-radius", ParamValue::Int(7));
    controller.set_param_value("prepare", "equalize", ParamValue::Bool(false));
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
            target_stops: 0.0,
            gains: vec![65536, 66000],
        })),
    );
    assert!(matches!(
        common::open_and_settle(&mut equalized, &[project]),
        OpenOutcome::Opened(_)
    ));
    equalized.toggle_section_more("prepare");
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
    plain.toggle_section_more("prepare");
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
fn pickers_hide_test_only() {
    assert!(
        registry::find_stack("dummy-stack").unwrap().test_only(),
        "the test build registers the dummy stack method"
    );
    assert!(registry::find_align("dummy-align").unwrap().test_only());

    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let rows = controller.settings_panel().rows;
    for section in ["fuse", "align"] {
        let ids = picker_ids(&rows, section);
        assert!(!ids.is_empty(), "{section} picker lists something");
        for id in ids {
            let algorithm = registry::find(&id).unwrap();
            assert!(!algorithm.test_only(), "{section} picker lists {id}");
        }
    }

    // A tab that holds a test-only algorithm keeps its closed-row label but
    // does not offer it.
    controller.set_align_algorithm("dummy-align");
    let rows = controller.settings_panel().rows;
    let picker = rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "align")
        .expect("align picker row must exist");
    assert_eq!(picker.value_text, "Dummy Align (test-only)");
    assert!(!picker.choice_ids.contains(&"dummy-align".to_string()));
}

#[test]
fn pickers_list_the_registry() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let panel = controller.settings_panel();

    assert_eq!(
        picker_ids(&panel.rows, "fuse"),
        listed_fuse_ids(),
        "the collapsed fuse picker lists every non-test method, the default first"
    );

    let align_picker = panel
        .rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "align")
        .expect("align picker row must exist");
    let mut expected_align: Vec<String> = registry::align_algorithms()
        .iter()
        .filter(|a| !a.test_only())
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
    controller.toggle_section_more("align");
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
    controller.toggle_section_more("align");
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
    controller.toggle_section_more("align");
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
    let crop_row = field_rows(&panel.rows, "finish")
        .into_iter()
        .find(|r| r.id == "auto-crop")
        .expect("auto-crop row must be present");
    assert_eq!(crop_row.kind, RowKind::Bool);
    assert_eq!(crop_row.selected, "true", "auto-crop must default on");

    controller.set_param_value("finish", "auto-crop", ParamValue::Bool(false));
    let panel = controller.settings_panel();
    let crop_row = field_rows(&panel.rows, "finish")
        .into_iter()
        .find(|r| r.id == "auto-crop")
        .unwrap();
    assert_eq!(crop_row.selected, "false");

    controller.set_param_value("finish", "auto-crop", ParamValue::Bool(true));
    let panel = controller.settings_panel();
    let crop_row = field_rows(&panel.rows, "finish")
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
        (PARAM_ROW_BOOL_GAP, "param-row-bool-gap: 12px"),
        (MORE_HEIGHT, "more-height: 32px"),
        (MORE_GAP, "more-gap: 12px"),
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

    let boundary = "const SHIPPED: &str = \"laplacian\";\n#[cfg(test)]\nmod tests {\nconst FIXTURE: &str = \"dummy-align\";\n}";
    let production = common::production_rust_source(boundary);
    assert!(production.contains("laplacian"));
    assert!(
        !production.contains("dummy-align"),
        "only cfg(test) fixture identifiers may leave the production scan"
    );
    assert!(
        common::production_rust_source("const SHIPPED: &str = \"laplacian\";")
            .contains("laplacian"),
        "files without a cfg(test) tail stay fully scanned"
    );

    let mut offenders = Vec::new();
    for path in common::find_rust_files(&common::src_dir()) {
        let contents = fs::read_to_string(&path).unwrap();
        let code = common::strip_line_comments(common::production_rust_source(&contents));
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
    controller.toggle_section_more("fuse");

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
    controller.toggle_section_more("fuse");
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
    controller.toggle_section_more("fuse");
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

    controller.toggle_section_more("fuse");
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

// -- The per-section `More...` fold-out. --

/// The ids of `section`'s picker options, in shown order.
fn picker_ids(rows: &[SettingsRow], section: &str) -> Vec<String> {
    rows.iter()
        .find(|r| r.kind == RowKind::Picker && r.section == section)
        .unwrap_or_else(|| panic!("no {section} picker"))
        .choice_ids
        .clone()
}

/// Every registered stack method's id, test-only ones included, the default
/// first and the advanced ones last.
fn every_fuse_id() -> Vec<String> {
    let mut methods = registry::stack_algorithms();
    methods.sort_by_key(|a| (!a.is_default(), a.advanced()));
    methods.iter().map(|a| a.id().to_string()).collect()
}

/// What the Fuse picker lists, `More...` open or not: [`every_fuse_id`]
/// without the test-only methods.
fn listed_fuse_ids() -> Vec<String> {
    every_fuse_id()
        .into_iter()
        .filter(|id| !registry::find_stack(id).unwrap().test_only())
        .collect()
}

fn more_rows<'a>(rows: &'a [SettingsRow], section: &str) -> Vec<&'a SettingsRow> {
    rows.iter()
        .filter(|r| r.kind == RowKind::More && r.section == section)
        .collect()
}

fn field_ids(rows: &[SettingsRow], section: &str) -> Vec<String> {
    field_rows(rows, section)
        .iter()
        .map(|r| r.id.clone())
        .collect()
}

fn row_index(rows: &[SettingsRow], section: &str, id: &str) -> usize {
    rows.iter()
        .position(|r| r.section == section && r.id == id && r.kind != RowKind::Error)
        .unwrap_or_else(|| panic!("no {section}:{id} row"))
}

fn schema_ids(method: &str) -> Vec<String> {
    registry::find(method)
        .expect("registered")
        .params()
        .iter()
        .map(|d| d.id.clone())
        .collect()
}

#[test]
fn more_toggle_is_a_shared_primitive() {
    let contents = fs::read_to_string(
        common::ui_dir()
            .join("components")
            .join("ff_more_toggle.slint"),
    )
    .unwrap();
    let code = common::strip_line_comments(&contents);
    assert_eq!(
        code.matches("export component").count(),
        1,
        "ff_more_toggle.slint must export exactly one component"
    );
    assert!(code.contains("export component FFMoreToggle"));
    for shell_import in ["shell/", "\"../shell"] {
        assert!(
            !code.contains(shell_import),
            "a primitive must import no shell file"
        );
    }
    assert!(!code.contains('#'), "a primitive holds no colour literal");
}

#[test]
fn sections_start_collapsed_with_one_adjustment_at_most() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_align_algorithm("pyramidal-ecc");

    for (method, visible) in [
        ("laplacian", &[][..]),
        ("weighted-average", &["weight-exponent"][..]),
        ("depth-map", &[][..]),
    ] {
        controller.set_fuse_algorithm(method);
        let rows = controller.settings_panel().rows;
        assert_eq!(field_ids(&rows, "fuse"), visible, "{method}");
        assert_eq!(field_ids(&rows, "align"), ["align"], "{method}");
        assert_eq!(
            field_ids(&rows, "prepare"),
            Vec::<String>::new(),
            "{method}"
        );
        assert_eq!(field_ids(&rows, "finish"), ["auto-crop"], "{method}");
        assert!(
            more_rows(&rows, "finish").is_empty(),
            "{method}: Finish has no More row"
        );
        for section in ["prepare", "fuse", "align"] {
            let more = more_rows(&rows, section);
            assert_eq!(more.len(), 1, "{method}: one More row in {section}");
            assert_eq!(
                more[0].selected, "false",
                "{method}: {section} starts closed"
            );
            let last_other = rows
                .iter()
                .rposition(|r| r.section == section && r.kind != RowKind::More)
                .expect("the section has rows");
            assert!(
                row_index(&rows, section, "more") > last_other,
                "{method}: {section}'s More row comes after its visible rows"
            );
        }
    }

    controller.set_fuse_algorithm("laplacian");
    let fuse_picker = picker_ids(&controller.settings_panel().rows, "fuse");
    assert_eq!(fuse_picker, listed_fuse_ids());
    assert_eq!(fuse_picker[0], "laplacian", "the default comes first");
    assert!(fuse_picker.contains(&"weighted-average".to_string()));
    assert!(fuse_picker.contains(&"depth-map".to_string()));
}

#[test]
fn more_reveals_that_sections_advanced_rows() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_align_algorithm("pyramidal-ecc");

    controller.toggle_section_more("fuse");
    let rows = controller.settings_panel().rows;
    let mut shown = field_ids(&rows, "fuse");
    shown.sort();
    let mut declared = schema_ids("laplacian");
    declared.sort();
    assert_eq!(shown, declared, "Fuse on Details shows every descriptor");
    let more = rows
        .iter()
        .position(|r| r.kind == RowKind::More && r.section == "fuse")
        .unwrap();
    assert_eq!(rows[more].selected, "true");
    assert!(
        row_index(&rows, "fuse", "levels") > more,
        "levels is advanced"
    );
    assert_eq!(
        field_ids(&rows, "align"),
        ["align"],
        "other sections stay collapsed, the Align switch alone showing"
    );

    controller.toggle_section_more("align");
    let rows = controller.settings_panel().rows;
    let mut with_switch = vec!["align".to_string()];
    with_switch.extend(schema_ids("pyramidal-ecc"));
    assert_eq!(
        field_ids(&rows, "align"),
        with_switch,
        "Align opens to every parameter, in declaration order, after its switch"
    );

    controller.set_fuse_algorithm("depth-map");
    controller.toggle_section_more("prepare");
    let rows = controller.settings_panel().rows;
    assert_eq!(
        field_ids(&rows, "prepare"),
        ["equalize", "equalize-max-gain", "equalize-target"]
    );
    assert_eq!(field_ids(&rows, "finish"), ["auto-crop"]);
    assert_eq!(
        rows[row_index(&rows, "prepare", "equalize")].selected,
        "true",
        "Focus map equalizes by default"
    );
    controller.set_fuse_algorithm("laplacian");
    let rows = controller.settings_panel().rows;
    assert_eq!(
        rows[row_index(&rows, "prepare", "equalize")].selected,
        "true",
        "Details equalizes by default"
    );
}

#[test]
fn the_target_row_follows_max_gain_under_more() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("prepare");
    let rows = controller.settings_panel().rows;
    let target = &rows[row_index(&rows, "prepare", "equalize-target")];
    assert_eq!(
        row_index(&rows, "prepare", "equalize-target"),
        row_index(&rows, "prepare", "equalize-max-gain") + 1,
        "the target row follows Max brightness gain"
    );
    assert_eq!(target.label, "Target brightness (stops)");
    assert_eq!(
        target.value_text, "0.0",
        "the default is the neutral target"
    );
    assert!(
        row_index(&rows, "prepare", "equalize-target")
            > rows
                .iter()
                .position(|r| r.kind == RowKind::More && r.section == "prepare")
                .unwrap(),
        "the target is advanced, so it sits under More"
    );

    controller.toggle_section_more("prepare");
    let rows = controller.settings_panel().rows;
    assert!(
        rows.iter()
            .all(|r| !(r.section == "prepare" && r.id == "equalize-target")),
        "More closed hides the target row"
    );
}

#[test]
fn more_row_covers_its_drawer() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    let rows = controller.settings_panel().rows;
    let closed = rows
        .iter()
        .position(|r| r.kind == RowKind::More && r.section == "fuse")
        .unwrap();
    assert_eq!(rows[closed].extent, MORE_HEIGHT);
    assert_eq!(
        rows[closed + 1].top - rows[closed].top,
        MORE_HEIGHT + MORE_GAP
    );
    assert!(rows.iter().all(|r| !r.nested));

    controller.toggle_section_more("fuse");
    let rows = controller.settings_panel().rows;
    let more = rows
        .iter()
        .position(|r| r.kind == RowKind::More && r.section == "fuse")
        .unwrap();
    let end = rows
        .iter()
        .position(|r| r.kind == RowKind::MoreEnd && r.section == "fuse")
        .expect("an open fold-out ends in a closing row");
    assert!(end > more + 1, "the drawer holds rows");
    assert!(rows[more + 1..end].iter().all(|r| r.nested));
    assert!(rows[..=more].iter().chain(&rows[end..]).all(|r| !r.nested));
    assert_eq!(
        rows[more].extent,
        rows[end].top - rows[more].top,
        "the band reaches the closing row"
    );
    assert_eq!(rows[end + 1].top - rows[end].top, MORE_GAP);
}

#[test]
fn more_state_is_per_tab_and_per_section() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let second = seal_without_align("more_second_tab", "laplacian");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[second]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.project_tabs().len(), 2);

    let open_sections = |controller: &AppController| -> Vec<String> {
        let rows = controller.settings_panel().rows;
        ["prepare", "fuse", "align"]
            .into_iter()
            .filter(|s| more_rows(&rows, s)[0].selected == "true")
            .map(str::to_string)
            .collect()
    };

    controller.select_project(0);
    controller.toggle_section_more("fuse");
    assert_eq!(open_sections(&controller), ["fuse"]);
    controller.select_project(1);
    assert!(
        open_sections(&controller).is_empty(),
        "the other tab is collapsed"
    );
    controller.select_project(0);
    assert_eq!(
        open_sections(&controller),
        ["fuse"],
        "the first tab kept its state"
    );
    controller.toggle_section_more("fuse");
    assert!(
        open_sections(&controller).is_empty(),
        "a second toggle closes it"
    );
}

#[test]
fn more_state_survives_method_switch_and_reset_all() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("fuse");
    controller.toggle_section_more("prepare");

    controller.set_fuse_algorithm("weighted-average");
    controller.set_param_value("fuse", "weight-exponent", ParamValue::Float(3.0));
    controller.reset_all_sections();
    let rows = controller.settings_panel().rows;
    for section in ["fuse", "prepare"] {
        assert_eq!(more_rows(&rows, section)[0].selected, "true", "{section}");
    }
    assert_eq!(more_rows(&rows, "align")[0].selected, "false");
    assert!(!field_ids(&rows, "fuse").is_empty());
}

#[test]
fn fuse_picker_lists_every_method_in_both_more_states() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let collapsed = picker_ids(&controller.settings_panel().rows, "fuse");
    assert_eq!(
        collapsed,
        listed_fuse_ids(),
        "More... closed lists every method"
    );
    assert_eq!(collapsed.last().map(String::as_str), Some("depth-map"));

    controller.toggle_section_more("fuse");
    let rows = controller.settings_panel().rows;
    let open = picker_ids(&rows, "fuse");
    assert_eq!(open, collapsed, "More... open lists the same methods");
    let picker = rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "fuse")
        .unwrap();
    assert_eq!(
        picker.choices.last().map(String::as_str),
        Some("Focus map"),
        "the picker shows the plain display name"
    );
}

/// Per method: pick it, change the one field only `More...` shows, switch away
/// and back, and read the equalization switch each method starts with.
#[test]
fn settings_journey_per_method() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    let more_open = |c: &AppController, section: &str| {
        more_rows(&c.settings_panel().rows, section)[0].selected == "true"
    };
    let has_field = |c: &AppController, id: &str| {
        field_ids(&c.settings_panel().rows, "fuse").contains(&id.to_string())
    };
    let methods = [
        ("laplacian", "levels", ParamValue::Int(4), "4"),
        ("weighted-average", "radius", ParamValue::Int(6), "6"),
        ("depth-map", "score-radius", ParamValue::Int(5), "5"),
    ];

    for (id, field, value, text) in &methods {
        if more_open(&controller, "fuse") {
            controller.toggle_section_more("fuse");
        }
        let display = registry::find(id).expect("registered").display_name();
        assert!(
            picker_ids(&controller.settings_panel().rows, "fuse").contains(&id.to_string()),
            "{id}: listed with Fuse's More... closed"
        );
        controller.set_fuse_algorithm(id);
        assert!(!has_field(&controller, field), "{id}: {field} hides");

        controller.toggle_section_more("fuse");
        assert!(has_field(&controller, field), "{id}: {field} shows");
        assert_ne!(
            fuse_value(&controller, field),
            *text,
            "{id}: the planned value is off the default"
        );
        controller.set_param_value("fuse", field, value.clone());
        assert_eq!(fuse_value(&controller, field), *text, "{id}");

        let other = if *id == "laplacian" {
            "weighted-average"
        } else {
            "laplacian"
        };
        controller.set_fuse_algorithm(other);
        controller.set_fuse_algorithm(id);
        assert_eq!(fuse_value(&controller, field), *text, "{id} kept");
        assert!(more_open(&controller, "fuse"), "{id}: fold-out kept");
        let rows = controller.settings_panel().rows;
        let picker = rows
            .iter()
            .find(|r| r.kind == RowKind::Picker && r.section == "fuse")
            .unwrap();
        assert_eq!(picker.selected, *id);
        assert_eq!(picker.value_text, display);

        controller.toggle_section_more("prepare");
        let equalize = controller
            .settings_panel()
            .rows
            .into_iter()
            .find(|r| r.section == "prepare" && r.id == "equalize")
            .expect("the prepare fold-out lists equalize");
        assert_eq!(equalize.selected, "true", "{id}: equalize");
        controller.toggle_section_more("prepare");
    }
}

#[test]
fn a_sealed_focus_map_is_named_with_more_closed() {
    let mut controller = AppController::new();
    let project = seal_without_align("more_sealed_focus_map", "depth-map");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project]),
        OpenOutcome::Opened(_)
    ));
    let rows = controller.settings_panel().rows;
    assert_eq!(more_rows(&rows, "fuse")[0].selected, "false");
    let picker = rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "fuse")
        .unwrap();
    assert!(picker.choice_ids.contains(&"depth-map".to_string()));
    assert_eq!(picker.value_text, "Focus map");
    assert_eq!(picker.selected, "depth-map");
}

#[test]
fn hidden_explicit_values_keep_the_heading_explicit() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_param_value("fuse", "levels", ParamValue::Int(3));

    let rows = controller.settings_panel().rows;
    assert!(field_ids(&rows, "fuse").is_empty(), "levels is hidden");
    assert_eq!(heading(&rows, "fuse").heading_state, HeadingState::Explicit);
    assert!(reset_all_enabled(&controller));
}

#[test]
fn hidden_violation_opens_its_section() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.set_fuse_algorithm("depth-map");
    controller.set_param_text("fuse", "score-radius", "not-a-number");

    let rows = controller.settings_panel().rows;
    assert!(
        rows.iter()
            .any(|r| r.kind == RowKind::Error && r.section == "fuse" && r.id == "score-radius"),
        "the error row shows although the section was never opened"
    );
    assert_eq!(more_rows(&rows, "fuse")[0].selected, "true");
    assert_eq!(more_rows(&rows, "align")[0].selected, "false");
}

#[test]
fn prepare_and_finish_sections_hold_the_project_settings() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);

    for method in every_fuse_id() {
        controller.set_fuse_algorithm(&method);
        let rows = controller.settings_panel().rows;
        let headings: Vec<(&str, &str)> = rows
            .iter()
            .filter(|r| r.kind == RowKind::SectionHeading)
            .map(|r| (r.section.as_str(), r.label.as_str()))
            .collect();
        assert_eq!(
            headings,
            [
                ("prepare", "PREPARE"),
                ("fuse", "FUSE"),
                ("align", "ALIGN"),
                ("finish", "FINISH"),
            ],
            "{method}"
        );
        assert!(
            rows.iter()
                .all(|r| r.section != "crop" && r.label != "CROP"),
            "{method}: no crop section or CROP label remains"
        );
        assert_eq!(
            field_ids(&rows, "prepare"),
            Vec::<String>::new(),
            "{method}"
        );
        assert_eq!(more_rows(&rows, "prepare")[0].selected, "false", "{method}");
        assert_eq!(field_ids(&rows, "finish"), ["auto-crop"], "{method}");

        controller.toggle_section_more("prepare");
        let rows = controller.settings_panel().rows;
        assert_eq!(
            field_ids(&rows, "prepare"),
            ["equalize", "equalize-max-gain", "equalize-target"],
            "{method}"
        );
        assert_eq!(
            rows[row_index(&rows, "prepare", "equalize")].selected,
            "true",
            "{method}: equalize starts on"
        );
        assert_eq!(field_ids(&rows, "finish"), ["auto-crop"], "{method}");
        controller.toggle_section_more("prepare");
    }
}

#[test]
fn section_membership_comes_from_core() {
    use focusforge_core::project::{FINISH_PARAMS, PREPARE_PARAMS};

    let ids = |group: &[focusforge_core::params::ParamDesc]| -> Vec<String> {
        group.iter().map(|d| d.id.clone()).collect()
    };
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("prepare");

    for method in every_fuse_id() {
        controller.set_fuse_algorithm(&method);
        let rows = controller.settings_panel().rows;
        assert_eq!(
            field_ids(&rows, "prepare"),
            ids(&PREPARE_PARAMS),
            "{method}"
        );
        assert_eq!(field_ids(&rows, "finish"), ids(&FINISH_PARAMS), "{method}");
    }

    controller.set_param_value("finish", "auto-crop", ParamValue::Bool(false));
    let rows = controller.settings_panel().rows;
    assert_eq!(
        heading(&rows, "finish").heading_state,
        HeadingState::Explicit
    );
    assert_eq!(
        heading(&rows, "prepare").heading_state,
        HeadingState::Default,
        "a Finish value leaves Prepare's heading alone"
    );
}

#[test]
fn hidden_rows_with_no_tab_render_collapsed() {
    let rows = AppController::new().settings_panel().rows;
    for section in ["prepare", "fuse", "align"] {
        assert_eq!(more_rows(&rows, section).len(), 1, "{section}");
        assert_eq!(more_rows(&rows, section)[0].selected, "false", "{section}");
    }
    assert!(
        more_rows(&rows, "finish").is_empty(),
        "Finish has no More row"
    );
    assert_eq!(field_ids(&rows, "prepare"), Vec::<String>::new());
    assert_eq!(field_ids(&rows, "align"), ["align"]);
    assert_eq!(field_ids(&rows, "finish"), ["auto-crop"]);
    for section in ["prepare", "finish"] {
        assert_eq!(
            heading(&rows, section).heading_state,
            HeadingState::Pending,
            "{section}"
        );
    }
}

#[test]
fn picking_from_a_list_closes_it() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("align");
    let open_menus = |controller: &AppController| -> Vec<String> {
        controller
            .settings_panel()
            .rows
            .iter()
            .filter(|r| r.menu_open)
            .map(|r| format!("{}:{}", r.section, r.id))
            .collect()
    };

    controller.toggle_param_menu_for("fuse", "algorithm");
    assert_eq!(open_menus(&controller), ["fuse:algorithm"]);
    controller.set_fuse_algorithm("weighted-average");
    assert!(
        open_menus(&controller).is_empty(),
        "a new method closes the list"
    );

    controller.toggle_param_menu_for("fuse", "algorithm");
    controller.set_fuse_algorithm("weighted-average");
    assert!(
        open_menus(&controller).is_empty(),
        "picking the current method closes the list too"
    );

    let rows = controller.settings_panel().rows;
    let mode = &rows[row_index(&rows, "align", "mode")];
    assert_eq!(mode.kind, RowKind::Enum);
    let other = mode
        .choice_ids
        .iter()
        .find(|id| **id != mode.selected)
        .expect("the enum has a second choice")
        .clone();
    controller.toggle_param_menu_for("align", "mode");
    assert_eq!(open_menus(&controller), ["align:mode"]);
    controller.set_param_text("align", "mode", &other);
    assert!(
        open_menus(&controller).is_empty(),
        "an enum pick closes its list"
    );
}

/// The window mounted the way the application runs it: controller events are
/// captured through a sink and replayed as the event loop does, so every click
/// reaches the panel through `ShellEvent::SettingsChanged`.
struct MountedPanel {
    app: focusforge_gui::AppWindow,
    controller: std::sync::Arc<std::sync::Mutex<AppController>>,
    events: std::sync::Arc<std::sync::Mutex<Vec<focusforge_gui::ShellEvent>>>,
    _installed: focusforge_gui::bridge::Installed,
}

impl MountedPanel {
    fn open() -> Self {
        use slint::ComponentHandle;
        use std::sync::{Arc, Mutex};

        init_window_backend();
        let app = focusforge_gui::AppWindow::new().unwrap();
        // Tall enough that every strip stays inside the panel's viewport with
        // the Prepare and Fuse sections open: a strip below the viewport's edge
        // is not mounted.
        app.window().set_size(slint::PhysicalSize::new(1440, 1200));
        let mut controller = AppController::new();
        open_mantis_512(&mut controller);
        let controller = Arc::new(Mutex::new(controller));
        let installed = focusforge_gui::bridge::install(
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
        focusforge_gui::bridge::push_settings_panel(&app, &controller.lock().unwrap());
        let panel = MountedPanel {
            app,
            controller,
            events,
            _installed: installed,
        };
        panel.perform();
        panel
    }

    /// What the event loop does with each captured event.
    fn perform(&self) {
        use focusforge_gui::ShellEvent;

        let drained: Vec<ShellEvent> = self.events.lock().unwrap().drain(..).collect();
        for event in &drained {
            let controller = self.controller.lock().unwrap();
            match event {
                ShellEvent::SettingsScrolled => focusforge_gui::bridge::push_settings_scroll(
                    &self.app,
                    &controller.settings_panel(),
                ),
                ShellEvent::SettingsChanged => {
                    focusforge_gui::bridge::push_settings_panel(&self.app, &controller)
                }
                _ => {}
            }
        }
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    }

    fn click(&self, position: slint::LogicalPosition) {
        use slint::ComponentHandle;
        use slint::platform::{PointerEventButton, WindowEvent};

        for event in [
            WindowEvent::PointerMoved { position },
            WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            },
            WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            },
        ] {
            self.app.window().dispatch_event(event);
            self.perform();
        }
    }

    fn elements(&self, id: &str) -> Vec<i_slint_backend_testing::ElementHandle> {
        i_slint_backend_testing::ElementQuery::from_root(&self.app)
            .match_id(id)
            .find_all()
    }

    fn rows(&self) -> Vec<SettingsRow> {
        self.controller.lock().unwrap().settings_panel().rows
    }

    /// The mounted `More...` strip of `section`; the mounted strips follow the
    /// panel's `More` rows in order.
    fn strip(&self, section: &str) -> i_slint_backend_testing::ElementHandle {
        let index = self
            .rows()
            .iter()
            .filter(|r| r.kind == RowKind::More)
            .position(|r| r.section == section)
            .unwrap_or_else(|| panic!("{section} has a More row"));
        self.elements("FFMoreToggle::root").remove(index)
    }

    /// Clicks the middle of the strip's header band, where the label sits,
    /// however tall the strip is while open.
    fn click_strip(&self, section: &str) {
        let strip = self.strip(section);
        let top_left = strip.absolute_position();
        self.click(slint::LogicalPosition::new(
            top_left.x + strip.size().width / 2.0,
            top_left.y + MORE_HEIGHT / 2.0,
        ));
    }

    fn is_open(&self, section: &str) -> bool {
        more_rows(&self.rows(), section)[0].selected == "true"
    }
}

#[test]
fn more_strip_closes_after_opening_in_every_section() {
    let panel = MountedPanel::open();
    let assert_toggles = |when: &str| {
        for section in ["prepare", "fuse", "align"] {
            for round in 0..2 {
                assert!(!panel.is_open(section), "{when}: {section} starts closed");
                panel.click_strip(section);
                assert!(
                    panel.is_open(section),
                    "{when}: {section} opens, round {round}"
                );
                assert!(
                    !field_ids(&panel.rows(), section).is_empty(),
                    "{when}: {section} lists its rows when open"
                );
                panel.click_strip(section);
                assert!(
                    !panel.is_open(section),
                    "{when}: {section} closes again, round {round}"
                );
            }
        }
    };
    assert_toggles("fresh panel");

    // Open Fuse's picker and pick a method from its list, as a user does.
    panel.click(element_center(&panel.elements("FFMenu::area")[0]));
    let rows = panel.rows();
    let wanted = rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "fuse")
        .expect("Fuse has a picker")
        .choice_ids
        .iter()
        .position(|id| id == "weighted-average")
        .expect("the picker lists Weighted average");
    let options = panel.elements("FFMenuList::option-area");
    panel.click(element_center(&options[wanted]));
    let rows = panel.rows();
    let picked = rows
        .iter()
        .find(|r| r.kind == RowKind::Picker && r.section == "fuse")
        .expect("Fuse has a picker");
    assert_eq!(
        picked.selected, "weighted-average",
        "the list option was picked"
    );
    assert_toggles("after a list pick");

    // Wheel the panel a few steps, then toggle again.
    panel
        .controller
        .lock()
        .unwrap()
        .set_settings_viewport_height(300.0);
    panel.perform();
    let hit = element_center(&panel.elements("FFSettingsPanel::hit-area")[0]);
    for _ in 0..3 {
        use slint::ComponentHandle;
        panel
            .app
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: hit,
                delta_x: 0.0,
                delta_y: -4.0,
            });
        panel.perform();
    }
    assert!(
        panel.controller.lock().unwrap().settings_scroll_y() > 0.0,
        "the wheel moved the panel"
    );
    for round in 0..2 {
        panel.click_strip("fuse");
        assert!(
            panel.is_open("fuse"),
            "after a scroll: Fuse opens, round {round}"
        );
        panel.click_strip("fuse");
        assert!(
            !panel.is_open("fuse"),
            "after a scroll: Fuse closes, round {round}"
        );
    }
}

#[test]
fn more_strip_is_disabled_while_running() {
    let panel = MountedPanel::open();
    panel.app.set_settings_enabled(false);
    for section in ["prepare", "fuse", "align"] {
        panel.click_strip(section);
        assert!(
            !panel.is_open(section),
            "{section}'s strip ignores clicks while the panel is disabled"
        );
    }
    panel.app.set_settings_enabled(true);
    for section in ["prepare", "fuse", "align"] {
        panel.click_strip(section);
        assert!(panel.is_open(section), "{section} opens once enabled again");
    }
}

/// The panel's controls are found by role: Reset All carries its enabled
/// state, each switch its row label and on state, each menu its row label and
/// shown value, each fold-out header its label.
#[test]
fn reset_all_and_switches_report_their_state() {
    use focusforge_gui::AppWindow;
    use i_slint_backend_testing::{AccessibleRole, ElementQuery};
    use slint::{ComponentHandle, Model as _};

    init_window_backend();
    let app = AppWindow::new().unwrap();
    // Tall enough that no row is clipped out of the panel's viewport, where
    // an element query does not look.
    app.window().set_size(slint::PhysicalSize::new(1440, 2400));
    app.set_project_loaded(true);
    app.set_dock_tab("settings".into());

    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("align");
    controller.toggle_section_more("fuse");
    controller.set_align_algorithm("pyramidal-ecc");
    focusforge_gui::bridge::push_settings_panel(&app, &controller);

    let find = |id: &str| {
        ElementQuery::from_root(&app)
            .match_id("FFDock::settings-panel")
            .match_descendants()
            .match_id(id)
            .find_all()
    };
    let reset_enabled = || {
        let reset = find("FFResetAll::area");
        assert_eq!(reset.len(), 1, "one Reset All in the panel");
        assert_eq!(reset[0].accessible_role(), Some(AccessibleRole::Button));
        assert_eq!(
            reset[0].accessible_label().as_deref(),
            Some("Reset All to Default")
        );
        reset[0].accessible_enabled()
    };
    assert_eq!(reset_enabled(), Some(false), "disabled while at defaults");

    controller.set_param_text("finish", "auto-crop", "false");
    focusforge_gui::bridge::push_settings_panel(&app, &controller);
    assert_eq!(reset_enabled(), Some(true), "enabled once a value is set");

    let rows = app.get_settings_rows();
    let of_kind = |kinds: &[&str]| {
        (0..rows.row_count())
            .map(|i| rows.row_data(i).unwrap())
            .filter(|r| kinds.contains(&r.kind.as_str()))
            .collect::<Vec<_>>()
    };

    let switches = find("FFToggle::area");
    let bools = of_kind(&["bool"]);
    assert!(!bools.is_empty(), "the registry has bool rows");
    assert_eq!(switches.len(), bools.len());
    for (switch, row) in switches.iter().zip(&bools) {
        assert_eq!(switch.accessible_role(), Some(AccessibleRole::Button));
        assert_eq!(
            switch.accessible_label().as_deref(),
            Some(row.label.as_str())
        );
        assert_eq!(switch.accessible_checked(), Some(row.selected == "true"));
    }
    let crop = bools.iter().position(|r| r.id == "auto-crop").unwrap();
    assert_eq!(switches[crop].accessible_checked(), Some(false));
    controller.set_param_text("finish", "auto-crop", "true");
    focusforge_gui::bridge::push_settings_panel(&app, &controller);
    assert_eq!(
        find("FFToggle::area")[crop].accessible_checked(),
        Some(true),
        "checked follows the value"
    );

    assert!(
        find("FFMenu::area")
            .iter()
            .all(|area| area.accessible_role() == Some(AccessibleRole::None)),
        "a menu is one accessible node, its root"
    );
    let menus = find("FFMenu::root");
    let fields = of_kind(&["picker", "enum"]);
    assert!(!fields.is_empty(), "the registry has picker and enum rows");
    assert_eq!(menus.len(), fields.len());
    for (menu, row) in menus.iter().zip(&fields) {
        assert_eq!(menu.accessible_role(), Some(AccessibleRole::Combobox));
        assert_eq!(menu.accessible_enabled(), Some(true));
        assert_eq!(menu.accessible_label().as_deref(), Some(row.label.as_str()));
        assert_eq!(
            menu.accessible_value().as_deref(),
            Some(row.value_text.as_str())
        );
    }

    let mores = find("FFMoreToggle::header-area");
    assert!(!of_kind(&["more"]).is_empty(), "the registry has fold-outs");
    assert_eq!(mores.len(), of_kind(&["more"]).len());
    for more in &mores {
        assert_eq!(more.accessible_role(), Some(AccessibleRole::Button));
        assert_eq!(more.accessible_label().as_deref(), Some("More..."));
    }

    controller.reset_all_sections();
    focusforge_gui::bridge::push_settings_panel(&app, &controller);
    assert_eq!(reset_enabled(), Some(false), "disabled again after a reset");
}

fn prepare_row(controller: &AppController, id: &str) -> SettingsRow {
    controller
        .settings_panel()
        .rows
        .into_iter()
        .find(|r| r.section == "prepare" && r.id == id && r.kind != RowKind::Error)
        .unwrap_or_else(|| panic!("the prepare section has a {id} row"))
}

#[test]
fn equalize_off_disables_target_and_max_gain_and_keeps_their_values() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("prepare");
    controller.set_param_value("prepare", "equalize-target", ParamValue::Float(0.3));
    controller.set_param_value("prepare", "equalize-max-gain", ParamValue::Float(1.5));
    let disabled_prepare_rows = |controller: &AppController| -> Vec<String> {
        controller
            .settings_panel()
            .rows
            .iter()
            .filter(|r| r.section == "prepare" && r.disabled)
            .map(|r| r.id.clone())
            .collect()
    };

    assert!(disabled_prepare_rows(&controller).is_empty());

    controller.set_param_value("prepare", "equalize", ParamValue::Bool(false));
    assert_eq!(
        disabled_prepare_rows(&controller),
        ["equalize-max-gain", "equalize-target"],
        "exactly the two dependent rows grey; the switch stays enabled"
    );
    assert!(!prepare_row(&controller, "equalize").disabled);
    assert_eq!(
        prepare_row(&controller, "equalize-target").value_text,
        "+0.3"
    );
    assert_eq!(
        prepare_row(&controller, "equalize-max-gain").value_text,
        "1.5"
    );

    controller.set_param_value("prepare", "equalize", ParamValue::Bool(true));
    assert!(disabled_prepare_rows(&controller).is_empty());
    assert_eq!(
        prepare_row(&controller, "equalize-target").value_text,
        "+0.3"
    );
    assert_eq!(
        prepare_row(&controller, "equalize-max-gain").value_text,
        "1.5"
    );
}

#[test]
fn target_slider_snaps_to_its_step() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("prepare");

    for (fraction, shown) in [(0.0, "\u{2212}0.5"), (0.37, "\u{2212}0.1"), (1.0, "+0.5")] {
        controller.set_param_fraction("prepare", "equalize-target", fraction);
        assert_eq!(
            prepare_row(&controller, "equalize-target").value_text,
            shown,
            "target at fraction {fraction}"
        );
    }

    // The other step (0.01) and the unsigned format: 1.01 + 0.37 * 0.99 snaps
    // to 1.38, written without residue.
    controller.set_param_fraction("prepare", "equalize-max-gain", 0.37);
    assert_eq!(
        prepare_row(&controller, "equalize-max-gain").value_text,
        "1.38"
    );
}

#[test]
fn signed_readout_formats_negative_zero_and_positive() {
    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("prepare");

    for (value, shown) in [
        (-0.5, "\u{2212}0.5"),
        (0.0, "0.0"),
        (0.2, "+0.2"),
        (-0.0, "0.0"),
    ] {
        controller.set_param_value("prepare", "equalize-target", ParamValue::Float(value));
        assert_eq!(
            prepare_row(&controller, "equalize-target").value_text,
            shown,
            "target {value:?}"
        );
    }

    // A range that stays non-negative keeps the plain number.
    controller.toggle_section_more("fuse");
    controller.set_fuse_algorithm("weighted-average");
    controller.set_param_value("fuse", "weight-exponent", ParamValue::Float(3.0));
    assert_eq!(fuse_value(&controller, "weight-exponent"), "3");
}

#[test]
fn keyboard_step_moves_the_target_by_one_step_and_clamps() {
    use slint::ComponentHandle;
    use slint::platform::{Key, WindowEvent};

    let mut controller = AppController::new();
    open_mantis_512(&mut controller);
    controller.toggle_section_more("prepare");
    let shown = |controller: &AppController| prepare_row(controller, "equalize-target").value_text;

    controller.step_param("prepare", "equalize-target", 1);
    assert_eq!(shown(&controller), "+0.1");

    controller.set_param_value("prepare", "equalize-target", ParamValue::Float(-0.5));
    controller.step_param("prepare", "equalize-target", -1);
    assert_eq!(
        shown(&controller),
        "\u{2212}0.5",
        "Left at the minimum stays"
    );

    controller.set_param_value("prepare", "equalize-target", ParamValue::Float(0.0));
    for _ in 0..7 {
        controller.step_param("prepare", "equalize-target", 1);
    }
    assert_eq!(shown(&controller), "+0.5", "Right at the maximum stays");

    // The same press through the mounted window: a pointer press at the
    // track's left end sets -0.5 and focuses the slider, Right then steps it.
    let panel = MountedPanel::open();
    panel.click_strip("prepare");
    let rows = panel.rows();
    let target_at = row_index(&rows, "prepare", "equalize-target");
    let slider_position = rows[..target_at]
        .iter()
        .filter(|r| {
            matches!(
                r.kind,
                RowKind::LinearFloat | RowKind::LogFloat | RowKind::Int
            )
        })
        .count();
    let track = panel
        .elements("FFSlider::drag-area")
        .remove(slider_position);
    let (origin, size) = (track.absolute_position(), track.size());
    panel.click(slint::LogicalPosition::new(
        origin.x + 1.0,
        origin.y + size.height / 2.0,
    ));
    assert_eq!(
        prepare_row(&panel.controller.lock().unwrap(), "equalize-target").value_text,
        "\u{2212}0.5"
    );

    for key in [Key::RightArrow, Key::RightArrow] {
        panel
            .app
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        panel
            .app
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        panel.perform();
    }
    assert_eq!(
        prepare_row(&panel.controller.lock().unwrap(), "equalize-target").value_text,
        "\u{2212}0.3",
        "two Right presses move the stored target two steps"
    );
}

#[test]
fn the_panel_is_disabled_without_a_project() {
    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    let mut controller = AppController::new();
    focusforge_gui::bridge::push_settings_panel(&app, &controller);
    assert!(!app.get_settings_enabled(), "no project open");

    open_mantis_512(&mut controller);
    focusforge_gui::bridge::push_settings_panel(&app, &controller);
    assert!(app.get_settings_enabled(), "a project is open");
}

#[test]
fn settings_opens_from_the_icon_bar() {
    let panel = MountedPanel::open();
    assert!(!panel.app.get_settings_open());

    // The bar's lower control, 8 px above the window's bottom edge.
    panel.click(slint::LogicalPosition::new(24.0, 1200.0 - 24.0));
    assert!(panel.app.get_settings_open());
}
