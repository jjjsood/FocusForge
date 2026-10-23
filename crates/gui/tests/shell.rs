// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless geometry: `i_slint_backend_testing::init_no_event_loop()`
//! simulates a windowing system without one and renders no pixels, exactly
//! as `tests/theme.rs` already relies on. Every rect is read back and
//! measured, never assumed.

mod common;

use std::fs;

use focusforge_gui::{AppController, AppWindow};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::ComponentHandle;

/// A headless `AppWindow`, sized explicitly -- `TestingWindow::size()`
/// otherwise reports 800 x 600 until set (`i-slint-backend-testing`
/// 1.17.1's `TestingWindow`).
fn app_at(width: u32, height: u32) -> AppWindow {
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
    app.window()
        .set_size(slint::PhysicalSize::new(width, height));
    app
}

/// `(x, y, width, height)` of the element `id` matches, in logical pixels.
/// `id` is `"<Component>::<local-id>"`, mapping `_` to `-` (`i-slint-
/// backend-testing` 1.17.1's `ElementQuery::match_id`).
fn rect(app: &AppWindow, id: &str) -> (f32, f32, f32, f32) {
    let handle = ElementQuery::from_root(app)
        .match_id(id)
        .find_first()
        .unwrap_or_else(|| panic!("no element matched id {id:?}"));
    let pos = handle.absolute_position();
    let size = handle.size();
    (pos.x, pos.y, size.width, size.height)
}

const EPS: f32 = 0.5;

fn assert_rect(app: &AppWindow, id: &str, expected: (f32, f32, f32, f32)) {
    let got = rect(app, id);
    assert!(
        (got.0 - expected.0).abs() < EPS
            && (got.1 - expected.1).abs() < EPS
            && (got.2 - expected.2).abs() < EPS
            && (got.3 - expected.3).abs() < EPS,
        "{id}: got {got:?}, expected {expected:?}"
    );
}

#[test]
fn shell_bands_match_the_spec() {
    let app = app_at(1440, 900);
    // The toolbar row only renders with a project open; these bands assume
    // one is.
    app.set_show_empty_state(false);

    assert_rect(&app, "AppWindow::tab-strip", (0.0, 0.0, 1440.0, 36.0));
    assert_rect(&app, "AppWindow::icon-bar", (0.0, 36.0, 48.0, 864.0));
    assert_rect(&app, "AppWindow::toolbar", (48.0, 36.0, 1132.0, 40.0));
    assert_rect(
        &app,
        "AppWindow::canvas-ground",
        (48.0, 76.0, 1132.0, 780.0),
    );
    assert_rect(
        &app,
        "FFCanvas::pane-left",
        (48.0 + 16.0, 76.0 + 16.0, 542.0, 748.0),
    );
    assert_rect(
        &app,
        "FFCanvas::pane-right",
        (1180.0 - 16.0 - 542.0, 76.0 + 16.0, 542.0, 748.0),
    );
    assert_rect(&app, "AppWindow::dock", (1180.0, 36.0, 260.0, 820.0));
    assert_rect(&app, "AppWindow::bottom-bar", (48.0, 856.0, 1392.0, 44.0));
}

#[test]
fn shell_bands_hold_at_the_minimum() {
    let app = app_at(1200, 760);
    app.set_show_empty_state(false);

    let dock = rect(&app, "AppWindow::dock");
    assert!(
        (dock.2 - 260.0).abs() < EPS,
        "dock must stay 260 wide, got {dock:?}"
    );

    assert_rect(
        &app,
        "FFCanvas::pane-left",
        (48.0 + 16.0, 76.0 + 16.0, 422.0, 608.0),
    );
    assert_rect(
        &app,
        "FFCanvas::pane-right",
        (940.0 - 16.0 - 422.0, 76.0 + 16.0, 422.0, 608.0),
    );

    for id in [
        "AppWindow::tab-strip",
        "AppWindow::toolbar",
        "AppWindow::canvas-ground",
        "AppWindow::dock",
        "AppWindow::bottom-bar",
        "FFCanvas::pane-left",
        "FFCanvas::pane-right",
    ] {
        let (_, _, w, h) = rect(&app, id);
        assert!(w > 0.0 && h > 0.0, "{id}: non-positive size {w}x{h}");
    }
}

#[test]
fn window_minimum_size_is_enforced() {
    // The WM-level refusal itself needs a window manager, which no headless
    // backend has -- that half needs a real window. This test pins the
    // binding a headless backend *can* see: the root's own
    // `min-width`/`min-height` read `ShellGeometry`'s 1200/760.
    let app_slint = fs::read_to_string(common::ui_dir().join("app.slint")).unwrap();
    assert!(app_slint.contains("min-width: ShellGeometry.min-width"));
    assert!(app_slint.contains("min-height: ShellGeometry.min-height"));

    let geometry_slint = fs::read_to_string(common::ui_dir().join("geometry.slint")).unwrap();
    assert!(geometry_slint.contains("min-width: 1200px"));
    assert!(geometry_slint.contains("min-height: 760px"));
}

#[test]
fn checkpoint_panel_shows_its_empty_state() {
    let app = app_at(1440, 900);
    // The dock tab row exists only while a project is loaded
    // (`dock_without_a_project_says_so`).
    app.set_project_loaded(true);

    let panel = ElementQuery::from_root(&app)
        .match_id("FFDock::checkpoint-panel")
        .find_first();
    let panel = panel.expect("no element matched id \"FFDock::checkpoint-panel\"");
    let tabs = ElementQuery::from_root(&app)
        .match_id("FFDock::dock-tabs")
        .find_first();
    let tabs = tabs.expect("no element matched id \"FFDock::dock-tabs\"");

    assert!(
        panel.absolute_position().y < tabs.absolute_position().y,
        "checkpoint-panel must sit above dock-tabs"
    );

    let export = ElementQuery::from_root(&app)
        .match_id("FFCheckpointPanel::checkpoint-export")
        .find_first()
        .expect("no element matched id \"FFCheckpointPanel::checkpoint-export\"");
    assert_eq!(export.accessible_enabled(), Some(false));
}

#[test]
fn checkpoint_panel_touches_no_storage() {
    // Rows come only from an open project's history; with no tab there are
    // none. A tab with a project shows its rows (`tests/checkpoint_panel.rs`).
    let controller = focusforge_gui::AppController::new();
    assert!(controller.checkpoints_panel().entries.is_empty());
    assert!(!controller.export_enabled());

    // The canvas legitimately touches storage (a project's TIFFs),
    // `input_config.rs` is a config file, `settings_panel.rs` is the one
    // module allowed to name the schema/registry crate, and `diagnostics.rs`
    // is the one module allowed to name the timing/measurement API --
    // every other file reaches it through `crate::diagnostics`, never
    // `focusforge_core` directly. `checkpoints.rs` reads the history the
    // panel lists, beside `job.rs`, which reads its planes, and `brush.rs`,
    // which builds the stored stroke's path in core's own point type.
    // `performance_settings.rs` parses its cap strings with core's size
    // syntax. Excluded by name: this test's own subject is the checkpoint
    // panel, not the whole crate. Source preview reductions also read frames.
    let canvas_files = [
        "checkpoints.rs",
        "brush.rs",
        "canvas.rs",
        "gpu_canvas.rs",
        "entry.rs",
        "input_config.rs",
        "settings_panel.rs",
        "diagnostics.rs",
        "job.rs",
        "images.rs",
        "source_previews.rs",
        "overlay.rs",
        "performance_settings.rs",
        "recent.rs",
        "settings.rs",
    ];
    for path in common::find_rust_files(&common::src_dir()) {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| canvas_files.contains(&n))
        {
            continue;
        }
        let contents = fs::read_to_string(&path).unwrap();
        let production = common::production_rust_source(&contents);
        for needle in ["focusforge_core", "fs::read", "File::open"] {
            assert!(
                !production.contains(needle),
                "{}: contains {needle:?} -- the checkpoint panel must touch no storage",
                path.display()
            );
        }
    }
}

/// The zoom cluster sits at the bottom bar's right end, 216 x 32, inset
/// `Theme.space-3` from the window edge. Zoom-out,
/// readout and zoom-in share one rounded surface (`zoom-group`, 120 px, no
/// internal gaps), with `Fit` kept visually separate by a full
/// `Theme.space-4` (16 px) gap.
#[test]
fn zoom_cluster_sits_at_the_bottom_bar_right_end() {
    let app = app_at(1440, 900);

    assert_rect(
        &app,
        "FFBottomBar::zoom-cluster",
        (
            1440.0 - 12.0 - 216.0,
            856.0 + (44.0 - 32.0) / 2.0,
            216.0,
            32.0,
        ),
    );

    // Neither pane carries a zoom control of its own -- the cluster is the
    // only place in the shell a zoom gesture's *button* form reaches.
    // `FFPane`/`FFCanvas` (`ui/shell/canvas.slint`) declare no `FFZoomCluster`
    // and no standalone zoom `TouchArea`; `FFZoomCluster` itself lives only
    // in `ui/shell/bottom_bar.slint`.
    let canvas_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("canvas.slint")).unwrap();
    let code = common::strip_line_comments(&canvas_slint);
    assert!(
        !code.contains("FFZoomCluster") && !code.contains("zoom-in") && !code.contains("zoom-out"),
        "ui/shell/canvas.slint must declare no zoom control of its own"
    );
}

/// Before a project is open there is no viewport to zoom or fit, so the
/// whole cluster -- not just the readout text -- is greyed and inert:
/// greying the readout alone would leave the buttons looking live.
#[test]
fn zoom_cluster_is_disabled_without_a_project() {
    let app = app_at(1440, 900);

    for id in [
        "FFZoomCluster::zoom-out",
        "FFZoomCluster::zoom-in",
        "FFZoomCluster::zoom-fit",
    ] {
        let handle = ElementQuery::from_root(&app)
            .match_id(id)
            .find_first()
            .unwrap_or_else(|| panic!("no element matched id {id:?}"));
        assert_eq!(
            handle.accessible_enabled(),
            Some(false),
            "{id}: must be disabled while no project is open"
        );
    }
}

/// With no project open `zoom-readout` is bound from an empty Rust string
/// (`AppController::zoom_readout`); the cluster shows an em dash rather
/// than a blank gap so the readout still reads as a real, if inert, part
/// of the control.
#[test]
fn zoom_readout_shows_a_dash_without_a_project() {
    let app = app_at(1440, 900);

    let readout = ElementQuery::from_root(&app)
        .match_id("FFZoomCluster::zoom-readout")
        .find_first()
        .expect("no element matched id \"FFZoomCluster::zoom-readout\"");
    assert_eq!(readout.accessible_label().as_deref(), Some("—"));
}

/// The maximise glyph must look different maximised than not.
/// `ElementQuery` cannot read an `Image` element's `source` back, so this
/// asserts the same `window-maximized`
/// binding through the accessible label the glyph button carries alongside
/// it -- proving the binding actually depends on the property, not just
/// that both a "Maximize" and a "Restore" string exist somewhere in the
/// source.
#[test]
fn maximize_glyph_reflects_the_maximized_state() {
    let app = app_at(1440, 900);

    let label = || {
        ElementQuery::from_root(&app)
            .match_id("FFTabStrip::window-maximize")
            .find_first()
            .expect("no element matched id \"FFTabStrip::window-maximize\"")
            .accessible_label()
    };

    assert_eq!(
        label().as_deref(),
        Some("Maximize"),
        "not maximized must read \"Maximize\""
    );

    app.set_window_maximized(true);
    assert_eq!(
        label().as_deref(),
        Some("Restore"),
        "maximized must read \"Restore\" -- the label did not follow the property"
    );

    app.set_window_maximized(false);
    assert_eq!(
        label().as_deref(),
        Some("Maximize"),
        "un-maximizing again must flip the label back"
    );
}

/// `FFManifestList`'s own rect, found by type since the panel gives its one
/// instance no local id.
fn manifest_list_rect(app: &AppWindow) -> (f32, f32, f32, f32) {
    let handle = ElementQuery::from_root(app)
        .match_descendants()
        .match_type_name("FFManifestList")
        .find_first()
        .expect("no FFManifestList element found");
    let pos = handle.absolute_position();
    let size = handle.size();
    (pos.x, pos.y, size.width, size.height)
}

fn manifest_row(name: &str) -> focusforge_gui::FFManifestRow {
    focusforge_gui::FFManifestRow {
        name: name.into(),
        value: "1 KB".into(),
        not_redacted: false,
    }
}

/// The footer sits at one position throughout every state the diagnostics
/// panel can show, and the manifest list's own size never grows with its
/// row count -- both load-bearing for "every row present in every state"
/// layout.
#[test]
fn diagnostics_panel_footer_and_list_are_stable_across_states() {
    let app = app_at(1440, 900);
    app.set_settings_open(true);
    app.set_settings_section("diagnostics".into());

    let one_row = std::rc::Rc::new(slint::VecModel::from(vec![manifest_row("adapter.json")]));
    app.set_diagnostics_rows(one_row.into());

    let footer_id = "FFDiagnosticsPanel::footer";
    let idle = rect(&app, footer_id);
    let (_, _, idle_width, idle_height) = manifest_list_rect(&app);

    app.set_diagnostics_include_minidumps(true);
    assert_rect(&app, footer_id, idle);

    app.set_diagnostics_status("existing-file".into());
    app.set_diagnostics_message(
        "This file already exists. Creating the bundle will overwrite it.".into(),
    );
    app.set_diagnostics_primary_label("Overwrite and create".into());
    assert_rect(&app, footer_id, idle);

    app.set_diagnostics_status("writing".into());
    app.set_diagnostics_busy(true);
    app.set_diagnostics_primary_label("Writing…".into());
    assert_rect(&app, footer_id, idle);

    app.set_diagnostics_status("written".into());
    app.set_diagnostics_busy(false);
    app.set_diagnostics_message("Bundle created successfully.".into());
    app.set_diagnostics_primary_label("Create bundle".into());
    assert_rect(&app, footer_id, idle);

    app.set_diagnostics_status("failed".into());
    app.set_diagnostics_message("Could not write the bundle. Nothing was written.".into());
    app.set_diagnostics_primary_label("Create bundle".into());
    assert_rect(&app, footer_id, idle);

    let many_rows = std::rc::Rc::new(slint::VecModel::from(
        (0..12)
            .map(|i| manifest_row(&format!("entry-{i}.json")))
            .collect::<Vec<_>>(),
    ));
    app.set_diagnostics_rows(many_rows.into());
    let (_, _, width, height) = manifest_list_rect(&app);
    assert!(
        (width - idle_width).abs() < EPS && (height - idle_height).abs() < EPS,
        "the manifest list's own size must not grow with the row count: got {width}x{height}, expected {idle_width}x{idle_height}"
    );
}

/// While no scan has completed, the footer stays at its idle position and
/// only the primary action disables -- the toggle and destination field are
/// bound on `busy` alone and stay live throughout.
#[test]
fn diagnostics_panel_loading() {
    let app = app_at(1440, 900);
    app.set_settings_open(true);
    app.set_settings_section("diagnostics".into());

    let footer_id = "FFDiagnosticsPanel::footer";
    let idle = rect(&app, footer_id);

    let primary_id = "FFDiagnosticsPanel::bundle-primary";
    let primary = || {
        ElementQuery::from_root(&app)
            .match_id(primary_id)
            .find_first()
            .unwrap_or_else(|| panic!("no element matched id {primary_id:?}"))
    };
    assert_eq!(primary().accessible_enabled(), Some(true));

    app.set_diagnostics_loading(true);
    assert_rect(&app, footer_id, idle);
    assert_eq!(primary().accessible_enabled(), Some(false));

    app.set_diagnostics_loading(false);
    assert_eq!(primary().accessible_enabled(), Some(true));
}

/// `FFDiagnosticsPanel`'s own rect, found by type since the settings frame
/// gives its one instance no local id.
fn diagnostics_panel_rect(app: &AppWindow) -> (f32, f32, f32, f32) {
    let handle = ElementQuery::from_root(app)
        .match_descendants()
        .match_type_name("FFDiagnosticsPanel")
        .find_first()
        .expect("no FFDiagnosticsPanel element found");
    let pos = handle.absolute_position();
    let size = handle.size();
    (pos.x, pos.y, size.width, size.height)
}

fn is_inside(inner: (f32, f32, f32, f32), outer: (f32, f32, f32, f32)) -> bool {
    inner.0 >= outer.0 - EPS
        && inner.1 >= outer.1 - EPS
        && inner.0 + inner.2 <= outer.0 + outer.2 + EPS
        && inner.1 + inner.3 <= outer.1 + outer.3 + EPS
}

/// A failure message and a destination far longer than their slots stay
/// inside the slot and the panel, and leave the footer and the status slot
/// where the idle panel has them.
#[test]
fn diagnostics_long_failure_message_keeps_the_footer() {
    let message = "disk full ".repeat(20);
    let destination = "/data/".repeat(50);
    assert_eq!(message.chars().count(), 200);
    assert_eq!(destination.chars().count(), 300);

    for (width, height) in [(1440u32, 900u32), (1100, 700)] {
        let app = app_at(width, height);
        app.set_settings_open(true);
        app.set_settings_section("diagnostics".into());
        let footer = rect(&app, "FFDiagnosticsPanel::footer");
        let slot = rect(&app, "FFDiagnosticsPanel::status-slot");

        app.set_diagnostics_destination(destination.as_str().into());
        app.set_diagnostics_status("failed".into());
        app.set_diagnostics_message(message.as_str().into());

        assert_rect(&app, "FFDiagnosticsPanel::footer", footer);
        assert_rect(&app, "FFDiagnosticsPanel::status-slot", slot);
        let panel = diagnostics_panel_rect(&app);
        let shown = rect(&app, "FFDiagnosticsPanel::message");
        assert!(
            is_inside(shown, slot) && is_inside(shown, panel),
            "message at {width}x{height}: {shown:?} must lie inside the slot {slot:?} and the panel {panel:?}"
        );
        let field = rect(&app, "FFDiagnosticsPanel::dest-field");
        assert!(
            is_inside(field, panel),
            "dest-field at {width}x{height}: {field:?} must lie inside the panel {panel:?}"
        );
    }
}

fn ui_source(relative: &str) -> String {
    common::strip_line_comments(&fs::read_to_string(common::ui_dir().join(relative)).unwrap())
}

/// A row's text can take the accent colour, and the component sheet draws
/// that tone, a wrapped paragraph and the 6 px gap between paragraphs.
#[test]
fn row_text_accent_and_paragraph_gap_are_on_the_sheet() {
    let row_text = ui_source("components/ff_row_text.slint");
    assert!(row_text.contains("in property <bool> accent: false;"));
    assert!(row_text.contains("Theme.accent-default"));

    let geometry = ui_source("geometry.slint");
    assert!(geometry.contains("settings-paragraph-gap: 6px;"));

    let sheet = ui_source("theme_sheet.slint");
    for needle in [
        "accent: true;",
        "wrap: word-wrap;",
        "spacing: ShellGeometry.settings-paragraph-gap;",
    ] {
        assert!(sheet.contains(needle), "the sheet does not draw {needle:?}");
    }
}

/// The Diagnostics tab is layout over shared components: its heading, rows
/// and status lines are the shared section, row and status text, it styles
/// nothing itself, its footer sizes nothing, and its message elides to one
/// line (geometry cannot show elision, so the source must).
#[test]
fn diagnostics_panel_composes_only_shared_components() {
    let panel = ui_source("shell/settings_modal/diagnostics_panel.slint");
    for needle in [
        " Text {",
        "color:",
        "font-",
        "background",
        "border-",
        "height: 20px",
        "height: 22px",
        "Theme.status-",
    ] {
        assert!(!panel.contains(needle), "the panel sets {needle:?}");
    }
    for shared in ["FFSettingsSection {", "FFSettingsRow {", "FFStatusText {"] {
        assert!(panel.contains(shared), "the panel does not draw {shared}");
    }

    let footer = panel
        .split("footer :=")
        .nth(1)
        .expect("the panel has a footer");
    assert!(footer.trim_start().starts_with("HorizontalLayout"));
    assert!(!footer.contains("px"), "the footer sizes nothing itself");

    let message = panel
        .split("message := FFStatusText")
        .nth(1)
        .expect("the panel has a message line")
        .split('}')
        .next()
        .unwrap();
    assert!(
        message.contains("overflow: elide"),
        "the message must elide"
    );
}

/// The action row's status line is the shared status text, so the status
/// colours are written once, in the primitive.
#[test]
fn settings_action_row_draws_its_status_line_through_the_status_text() {
    let row = ui_source("components/ff_settings_action_row.slint");
    assert!(row.contains("FFStatusText {"));
    for needle in [" Text {", "Theme.status-"] {
        assert!(!row.contains(needle), "the action row writes {needle:?}");
    }
}

/// The action row's status line starts where the helper text does and runs to
/// the row's right edge: the content width less the label column.
#[test]
fn settings_action_row_status_line_starts_in_the_helper_column() {
    let app = app_at(1440, 900);
    app.set_settings_open(true);
    app.set_settings_section("storage".into());
    let helper = rect(&app, "FFSettingsActionRow::helper-text");
    let status = rect(&app, "FFSettingsActionRow::status");
    assert!(
        (status.0 - helper.0).abs() < EPS,
        "status x {} but helper x {}",
        status.0,
        helper.0
    );
    assert!(
        (status.2 - 306.0).abs() < EPS,
        "status width {}, expected 446 - 140",
        status.2
    );
}

/// The path row elides a long path to the value column, leaves the
/// `not created yet` suffix whole, and draws an unresolved location as a
/// muted word in place of the path (geometry cannot show elision, so the
/// source must).
#[test]
fn settings_path_row_elides_its_path_and_marks_an_unresolved_location() {
    let row = ui_source("components/ff_settings_path_row.slint");
    let block = |opening: &str| {
        row.split(opening)
            .nth(1)
            .unwrap_or_else(|| panic!("the path row has no {opening:?}"))
            .split('}')
            .next()
            .unwrap()
            .to_string()
    };

    let path = block("if root.available: FFRowText {");
    assert!(path.contains("text: root.path"));
    assert!(path.contains("overflow: elide"), "the path must elide");
    let suffix = block("if root.available && root.missing: FFRowText {");
    assert!(suffix.contains("not created yet"));
    assert!(!suffix.contains("overflow"), "the suffix stays whole");
    let unavailable = block("if !root.available: FFRowText {");
    assert!(unavailable.contains("muted: true"));
    assert!(unavailable.contains("\"Unavailable\""));
    assert!(row.contains("in property <bool> available: true;"));
}

/// Tab component each section renders, by slug. Every other tab component is
/// absent while a section is selected.
const SETTINGS_TABS: [(&str, &str); 6] = [
    ("performance", "FFPerformanceTab"),
    ("appearance", "FFAppearanceTab"),
    ("storage", "FFStorageTab"),
    ("input-map", "FFInputMapTab"),
    ("diagnostics", "FFDiagnosticsPanel"),
    ("about", "FFAboutTab"),
];

/// The rail's entries top to bottom. A component's root element is reported
/// once per element merged into its item, so entries are told apart by
/// position.
fn nav_rail_entries(app: &AppWindow) -> Vec<i_slint_backend_testing::ElementHandle> {
    let mut entries: Vec<i_slint_backend_testing::ElementHandle> = Vec::new();
    for handle in ElementQuery::from_root(app)
        .match_descendants()
        .match_type_name("FFNavRailEntry")
        .find_all()
    {
        if entries
            .iter()
            .all(|seen| seen.absolute_position() != handle.absolute_position())
        {
            entries.push(handle);
        }
    }
    entries
}

fn settings_slint_sources() -> Vec<(String, String)> {
    let dir = common::ui_dir().join("shell");
    let mut files = vec![dir.join("settings_modal.slint")];
    files.extend(common::find_slint_files(&dir.join("settings_modal")));
    files
        .into_iter()
        .map(|path| {
            let code = common::strip_line_comments(&fs::read_to_string(&path).unwrap());
            // Matched by `/`-separated suffixes, whatever the platform's separator is.
            (path.display().to_string().replace('\\', "/"), code)
        })
        .collect()
}

/// Opening Settings without choosing a section (the icon bar's Settings button) shows Performance:
/// the window's initial section is the controller's default, and the rail
/// marks that entry selected.
#[test]
fn settings_open_with_performance_selected() {
    let app = app_at(1440, 900);
    assert_eq!(
        app.get_settings_section().as_str(),
        focusforge_gui::SettingsSection::default().slug()
    );
    app.set_settings_open(true);
    let checked: Vec<Option<bool>> = nav_rail_entries(&app)
        .iter()
        .map(|entry| entry.accessible_checked())
        .collect();
    assert_eq!(
        checked,
        [
            Some(true),
            Some(false),
            Some(false),
            Some(false),
            Some(false),
            Some(false)
        ]
    );
}

/// The rail carries one entry per live section, in `SettingsSection::ALL`
/// order, whichever entry is selected, and activating an entry raises its
/// slug.
#[test]
fn settings_nav_rail_lists_only_live_sections() {
    let app = app_at(1440, 900);
    app.set_settings_open(true);
    let selected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_settings_section_selected({
        let selected = selected.clone();
        move |slug| selected.borrow_mut().push(slug.to_string())
    });

    let slugs: Vec<&str> = focusforge_gui::SettingsSection::ALL
        .iter()
        .map(|section| section.slug())
        .collect();
    for start in &slugs {
        app.set_settings_section((*start).into());
        let entries = nav_rail_entries(&app);
        let labels: Vec<String> = entries
            .iter()
            .map(|entry| entry.accessible_label().unwrap().to_string())
            .collect();
        assert_eq!(
            labels,
            [
                "Performance",
                "Appearance",
                "Storage",
                "Input map",
                "Diagnostics",
                "About"
            ],
            "from {start}"
        );
        assert_eq!(labels.len(), slugs.len());

        selected.borrow_mut().clear();
        for entry in &entries {
            entry.invoke_accessible_default_action();
        }
        assert_eq!(*selected.borrow(), slugs, "from {start}");
    }

    for (path, code) in settings_slint_sources() {
        assert!(!code.contains("\"General\""), "{path} names a General tab");
    }
    let entry =
        fs::read_to_string(common::ui_dir().join("components/ff_nav_rail_entry.slint")).unwrap();
    assert!(
        !common::strip_line_comments(&entry).contains("enabled"),
        "a rail entry has no disabled state"
    );
}

/// One mouse click activates an entry and leaves the keyboard focus ring
/// off.
#[test]
fn a_mouse_click_on_a_rail_entry_selects_it_without_a_focus_ring() {
    let app = app_at(1440, 900);
    app.set_settings_open(true);
    let selected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_settings_section_selected({
        let selected = selected.clone();
        move |slug| selected.borrow_mut().push(slug.to_string())
    });

    nav_rail_entries(&app)[1].mock_single_click(slint::platform::PointerEventButton::Left);

    assert_eq!(*selected.borrow(), ["appearance"]);
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFNavRailEntry::ring")
            .find_first()
            .is_none(),
        "a mouse click must not draw the keyboard focus ring"
    );
}

/// Tab reaches a rail entry and draws its ring; Return and Space then
/// activate the focused entry, and any other key does not.
#[test]
fn return_and_space_activate_the_focused_rail_entry() {
    use slint::platform::{Key, WindowEvent};

    let app = app_at(1440, 900);
    app.set_settings_open(true);
    let selected = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_settings_section_selected({
        let selected = selected.clone();
        move |slug| selected.borrow_mut().push(slug.to_string())
    });
    let press = |key: slint::SharedString| {
        app.window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
        app.window()
            .dispatch_event(WindowEvent::KeyReleased { text: key });
    };
    let ring_shown = || {
        ElementQuery::from_root(&app)
            .match_id("FFNavRailEntry::ring")
            .find_first()
            .is_some()
    };
    let tab_to_ring = || {
        for _ in 0..40 {
            if ring_shown() {
                return;
            }
            press(Key::Tab.into());
        }
        panic!("Tab never reached a rail entry");
    };

    tab_to_ring();
    press("a".into());
    assert!(selected.borrow().is_empty(), "a letter must not activate");
    press(Key::Return.into());
    assert_eq!(*selected.borrow(), ["performance"]);

    press(Key::Tab.into());
    tab_to_ring();
    press(" ".into());
    assert_eq!(*selected.borrow(), ["performance", "appearance"]);
}

/// Exactly the selected section's tab component is instantiated, and each
/// tab is declared in its own file.
#[test]
fn settings_tabs_render_from_their_own_files() {
    let slugs: Vec<&str> = focusforge_gui::SettingsSection::ALL
        .iter()
        .map(|section| section.slug())
        .collect();
    assert_eq!(slugs, SETTINGS_TABS.map(|(slug, _)| slug));

    let app = app_at(1440, 900);
    app.set_settings_open(true);
    for (slug, shown) in SETTINGS_TABS {
        app.set_settings_section(slug.into());
        for (_, tab) in SETTINGS_TABS {
            let found = ElementQuery::from_root(&app)
                .match_descendants()
                .match_type_name(tab)
                .find_first()
                .is_some();
            assert_eq!(found, tab == shown, "{tab} under {slug}");
        }
    }

    let tabs = common::ui_dir().join("shell").join("settings_modal");
    for (file, component) in [
        ("performance_tab.slint", "FFPerformanceTab"),
        ("appearance_tab.slint", "FFAppearanceTab"),
        ("storage_tab.slint", "FFStorageTab"),
        ("input_map_tab.slint", "FFInputMapTab"),
        ("diagnostics_panel.slint", "FFDiagnosticsPanel"),
        ("about_tab.slint", "FFAboutTab"),
    ] {
        let code = common::strip_line_comments(&fs::read_to_string(tabs.join(file)).unwrap());
        assert!(
            code.contains(&format!("export component {component} ")),
            "{file} must declare {component}"
        );
    }
    let frame = common::strip_line_comments(
        &fs::read_to_string(tabs.parent().unwrap().join("settings_modal.slint")).unwrap(),
    );
    assert_eq!(
        frame.matches("component ").count(),
        1,
        "settings_modal.slint declares only FFSettingsModal"
    );
    assert!(frame.contains("export component FFSettingsModal "));
}

/// The frame is `FFModalPanel`'s panel: 680 x 520 and centred. The rail's
/// ground starts at the panel top, the header rule crosses the full width
/// and the entries start 72 px down on a 32 px pitch.
#[test]
fn settings_frame_sits_on_the_modal_panel() {
    let frame = fs::read_to_string(common::ui_dir().join("shell/settings_modal.slint")).unwrap();
    assert!(
        !frame.contains("scroll-event("),
        "the scrim is FFModalPanel's"
    );

    for (width, height) in [(1440u32, 900u32), (1100, 700)] {
        let app = app_at(width, height);
        app.set_settings_open(true);
        let px = (width as f32 - 680.0) / 2.0;
        let py = (height as f32 - 520.0) / 2.0;

        assert_rect(&app, "FFModalPanel::panel", (px, py, 680.0, 520.0));
        assert_rect(&app, "FFSettingsModal::rail", (px, py, 190.0, 520.0));
        assert_rect(
            &app,
            "FFSettingsModal::header-rule",
            (px, py + 59.0, 680.0, 1.0),
        );
        for (i, entry) in nav_rail_entries(&app).iter().enumerate() {
            let pos = entry.absolute_position();
            let size = entry.size();
            let got = (pos.x, pos.y, size.width, size.height);
            let expected = (px, py + 72.0 + 32.0 * i as f32, 190.0, 32.0);
            assert!(
                (got.0 - expected.0).abs() < EPS
                    && (got.1 - expected.1).abs() < EPS
                    && (got.2 - expected.2).abs() < EPS
                    && (got.3 - expected.3).abs() < EPS,
                "entry {i} at {width}x{height}: got {got:?}, expected {expected:?}"
            );
        }
    }
}

/// The `{ ... }` body of the first element declared as `id := ` in `code`.
fn element_block<'a>(code: &'a str, id: &str) -> &'a str {
    let start = code
        .find(&format!("{id} := "))
        .unwrap_or_else(|| panic!("no element `{id}` in the source"));
    let open = start + code[start..].find('{').expect("element without a body");
    let mut depth = 0;
    for (i, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &code[open..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after `{id}`");
}

/// The header height, the header's horizontal inset and the tabs' content
/// inset are each written once, as geometry tokens, and read from there.
#[test]
fn settings_modal_reads_the_header_and_inset_tokens() {
    let sources = settings_slint_sources();
    let frame = &sources
        .iter()
        .find(|(path, _)| path.ends_with("shell/settings_modal.slint"))
        .expect("the modal frame source")
        .1;
    for literal in ["60px", "59px"] {
        assert!(
            !frame.contains(literal),
            "settings_modal.slint still writes {literal}"
        );
    }
    for token in ["settings-header-height", "settings-header-inset-x"] {
        assert!(
            frame.contains(&format!("ShellGeometry.{token}")),
            "settings_modal.slint must read ShellGeometry.{token}"
        );
    }

    for (path, code) in &sources {
        assert!(
            !code.contains("Theme.space-4 + Theme.space-1"),
            "{path} still spells a 20 px inset from theme spacing"
        );
        assert!(
            !code.contains("padding-right: Theme.space-6"),
            "{path} still spells the 24 px right inset from theme spacing"
        );
    }
    for (path, code) in sources
        .iter()
        .filter(|(p, _)| p.contains("settings_modal/"))
    {
        let mut sides = vec!["left", "right", "top"];
        if !path.ends_with("diagnostics_panel.slint") {
            sides.push("bottom");
        }
        for side in sides {
            assert!(
                code.contains(&format!("ShellGeometry.settings-content-inset-{side}")),
                "{path} must read the {side} content inset token"
            );
        }
        for line in code
            .lines()
            .filter(|l| l.trim_start().starts_with("padding-"))
        {
            assert!(
                !line.contains(" + ") && !line.contains("px"),
                "{path}: padding sized by an expression or a literal: {line}"
            );
        }
    }

    let list = ui_source("components/ff_manifest_list.slint");
    let value = element_block(&list, "value-text");
    assert!(
        !value.contains("width:") && !value.contains("overflow"),
        "the value text must size to its text: {value}"
    );
    assert!(element_block(&list, "name-text").contains("overflow: elide"));
}

/// Every section's content host sits right of the rail and under the
/// header, and its first section starts at the content inset.
#[test]
fn settings_tabs_keep_their_content_inset() {
    for (width, height) in [(1440u32, 900u32), (1100, 700)] {
        let px = (width as f32 - 680.0) / 2.0;
        let py = (height as f32 - 520.0) / 2.0;
        for (slug, _) in SETTINGS_TABS {
            let app = app_at(width, height);
            app.set_settings_open(true);
            app.set_settings_section(slug.into());

            assert_rect(
                &app,
                "FFSettingsModal::content",
                (px + 190.0, py + 60.0, 490.0, 460.0),
            );
            let first = by_position(
                ElementQuery::from_root(&app)
                    .match_descendants()
                    .match_type_name("FFSettingsSection"),
            )
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("{slug}: no FFSettingsSection"));
            let (pos, size) = (first.absolute_position(), first.size());
            let got = (pos.x, pos.y, size.width);
            let expected = (px + 210.0, py + 76.0, 446.0);
            assert!(
                (got.0 - expected.0).abs() < EPS
                    && (got.1 - expected.1).abs() < EPS
                    && (got.2 - expected.2).abs() < EPS,
                "{slug} at {width}x{height}: first section {got:?}, expected {expected:?}"
            );
        }
    }
}

fn manifest_row_with(name: &str, value: &str, not_redacted: bool) -> focusforge_gui::FFManifestRow {
    focusforge_gui::FFManifestRow {
        name: name.into(),
        value: value.into(),
        not_redacted,
    }
}

/// A manifest row's value keeps its whole text and sits at the clip area's
/// right edge; the name takes the rest and never reaches under the value or
/// the scrollbar, however long it is.
#[test]
fn manifest_list_value_is_never_clipped() {
    let long_name = format!(
        "crashes/crash-2026-09-18T09-03-12-{}.dmp",
        "focusforge-gui-pid-48213".repeat(2)
    );
    assert!(long_name.chars().count() >= 70);
    let rows = vec![
        manifest_row_with("adapter.json", "1 KB", false),
        manifest_row_with("crashes/crash-a.dmp", "2.4 MB · not redacted", true),
        manifest_row_with("crashes/crash-b.dmp", "1023.9 MB · not redacted", true),
        manifest_row_with(&long_name, "2.4 MB · not redacted", true),
    ];
    let app = app_at(1440, 900);
    app.set_settings_open(true);
    app.set_settings_section("diagnostics".into());
    app.set_diagnostics_rows(
        std::rc::Rc::new(slint::VecModel::from(vec![manifest_row("one.json")])).into(),
    );
    let one_row_list = manifest_list_rect(&app);

    app.set_diagnostics_manifest_content_height(px_for_rows(rows.len()));
    app.set_diagnostics_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    assert_eq!(manifest_list_rect(&app), one_row_list);

    let find = |id: &str| by_position(ElementQuery::from_root(&app).match_id(id));
    let names = find("FFManifestList::name-text");
    let values = find("FFManifestList::value-text");
    assert_eq!((names.len(), values.len()), (4, 4));
    let clip = rect(&app, "FFManifestList::clip-area");
    let scrollbar = rect(&app, "FFManifestList::scrollbar");

    let width = |i: usize| values[i].size().width;
    assert!(
        width(0) < 110.0 && 110.0 < width(1) && width(1) < width(2),
        "value widths must follow their text: {} < 110 < {} < {}",
        width(0),
        width(1),
        width(2)
    );
    assert!(
        (width(1) - width(3)).abs() < EPS,
        "the same text, the same width"
    );

    for (i, (name, value)) in names.iter().zip(&values).enumerate() {
        let (n, v) = (name.absolute_position(), value.absolute_position());
        let value_right = v.x + value.size().width;
        assert!(
            (n.x - clip.0).abs() < EPS,
            "row {i}: name starts at {}, clip at {}",
            n.x,
            clip.0
        );
        assert!(
            n.x + name.size().width <= v.x + EPS,
            "row {i}: name ends at {} past the value at {}",
            n.x + name.size().width,
            v.x
        );
        assert!(
            (value_right - (clip.0 + clip.2)).abs() < EPS,
            "row {i}: value ends at {value_right}, clip area at {}",
            clip.0 + clip.2
        );
        assert!(
            scrollbar.0 >= value_right,
            "row {i}: value ends at {value_right}, under the scrollbar at {}",
            scrollbar.0
        );
    }
}

/// Content height of `rows` manifest rows: 18 px each, 8 px apart.
fn px_for_rows(rows: usize) -> f32 {
    18.0 * rows as f32 + 8.0 * rows.saturating_sub(1) as f32
}

/// A window on the Input map section, its rows filled from `controller` the
/// way the running app fills them.
fn input_map_window(width: u32, height: u32, controller: &AppController) -> AppWindow {
    let app = app_at(width, height);
    let rows: Vec<focusforge_gui::InputRow> = controller
        .input_rows()
        .into_iter()
        .map(|r| focusforge_gui::InputRow {
            input: r.input.into(),
            stacking: r.stacking.into(),
            editor: r.editor.into(),
        })
        .collect();
    app.set_input_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    app.set_settings_open(true);
    app.set_settings_section("input-map".into());
    app
}

fn input_map_tab(app: &AppWindow) -> ElementHandle {
    ElementQuery::from_root(app)
        .match_descendants()
        .match_type_name("FFInputMapTab")
        .find_first()
        .expect("no FFInputMapTab element found")
}

/// The elements `query` matches, one per position (a component's root is
/// reported once per element merged into its item), top to bottom.
fn by_position(query: ElementQuery) -> Vec<ElementHandle> {
    let mut found: Vec<ElementHandle> = Vec::new();
    for handle in query.find_all() {
        if found
            .iter()
            .all(|seen| seen.absolute_position() != handle.absolute_position())
        {
            found.push(handle);
        }
    }
    found.sort_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y));
    found
}

/// The header row, then one row per binding, as `[input, stacking, editor]`
/// cell texts.
fn input_map_cells(tab: &ElementHandle) -> Vec<[String; 3]> {
    let column = |id: &str| -> Vec<String> {
        by_position(tab.query_descendants().match_id(id))
            .iter()
            .map(|cell| cell.accessible_label().unwrap().to_string())
            .collect()
    };
    let input = column("FFInputTableRow::input-cell");
    let stacking = column("FFInputTableRow::stacking-cell");
    let editor = column("FFInputTableRow::editor-cell");
    assert_eq!(input.len(), stacking.len());
    assert_eq!(input.len(), editor.len());
    input
        .into_iter()
        .zip(stacking)
        .zip(editor)
        .map(|((input, stacking), editor)| [input, stacking, editor])
        .collect()
}

/// With the section open, the tab shows its heading, the column header and
/// then exactly the controller's rows in order, cell for cell. Run on the
/// shipped table and on one with a binding changed, so a table written into
/// the tab fails.
#[test]
fn input_map_tab_shows_the_controller_rows() {
    let shipped = AppController::new();
    let mut changed = AppController::new();
    let mut table = focusforge_gui::input_map::InputTable::default();
    table.bindings[0].stacking = Some(focusforge_gui::input_map::Action::PanHorizontal);
    changed.install_input_table(table);
    assert_ne!(
        shipped.input_rows(),
        changed.input_rows(),
        "the changed binding must show in the rows"
    );

    for controller in [&shipped, &changed] {
        let app = input_map_window(1440, 900, controller);
        let tab = input_map_tab(&app);

        let headings: Vec<String> = tab
            .query_descendants()
            .match_type_name("FFSectionHeading")
            .find_first()
            .expect("the tab has no section heading")
            .query_descendants()
            .match_accessible_role(AccessibleRole::Text)
            .find_all()
            .iter()
            .map(|text| text.accessible_label().unwrap().to_string())
            .collect();
        assert_eq!(headings, ["BINDINGS"]);

        let mut expected = vec![["INPUT".to_string(), "STACKING".into(), "EDITOR".into()]];
        expected.extend(
            controller
                .input_rows()
                .into_iter()
                .map(|row| [row.input, row.stacking, row.editor]),
        );
        assert_eq!(input_map_cells(&tab), expected);
    }
}

/// A 1 px rule sits directly under the header and under every row, the last
/// one included.
#[test]
fn input_map_table_draws_a_rule_under_every_row() {
    let controller = AppController::new();
    let app = input_map_window(1440, 900, &controller);
    let tab = input_map_tab(&app);
    let cells = by_position(
        tab.query_descendants()
            .match_id("FFInputTableRow::input-cell"),
    );
    let rules = by_position(tab.query_descendants().match_type_name("FFDivider"));
    assert_eq!(rules.len(), cells.len(), "one rule per table row");

    for (i, (cell, rule)) in cells.iter().zip(&rules).enumerate() {
        let below = cell.absolute_position().y + cell.size().height;
        let next = cells
            .get(i + 1)
            .map_or(f32::INFINITY, |next| next.absolute_position().y);
        let y = rule.absolute_position().y;
        assert!(
            y >= below - EPS && y < next,
            "row {i}: rule at {y} is not between the row's end {below} and the next row {next}"
        );
        assert_eq!(rule.size().height, 1.0, "row {i}: the rule is 1 px tall");
    }
}

/// Every table row stays inside the settings content area, horizontally and
/// above its bottom edge.
#[test]
fn input_map_rows_stay_inside_the_content_area() {
    let controller = AppController::new();
    for (width, height) in [(1440u32, 900u32), (1100, 700)] {
        let app = input_map_window(width, height, &controller);
        let (cx, cy, cw, ch) = rect(&app, "FFSettingsModal::content");
        let rows = by_position(
            input_map_tab(&app)
                .query_descendants()
                .match_id("FFInputTableRow::input-cell"),
        );
        assert_eq!(rows.len(), controller.input_rows().len() + 1);
        for (i, cell) in rows.iter().enumerate() {
            let pos = cell.absolute_position();
            let size = cell.size();
            assert!(
                pos.x >= cx - EPS
                    && pos.x + size.width <= cx + cw + EPS
                    && pos.y + size.height <= cy + ch + EPS,
                "row {i} at {width}x{height}: cell {pos:?} {size:?} leaves the content {:?}",
                (cx, cy, cw, ch)
            );
        }
    }
}

/// Under the Projects overview a failure shows no toast and stays unread,
/// and the closed toast never blocks a click on the card underneath it.
#[test]
fn projects_suppresses_a_failure_toast_and_keeps_it_unread() {
    let mut controller = common::controller_with_frames("shell_projects_failure", 3);
    controller.set_align_algorithm("dummy-align");
    controller.toggle_projects_overlay();
    assert!(controller.start_fuse());
    assert!(
        controller
            .add_frames(&[common::fixture_frame("mantis-64", 4)])
            .is_err(),
        "a running job refuses an add"
    );
    common::wait_for_settle(&mut controller, std::time::Duration::from_secs(60));
    let controller: SharedController = std::sync::Arc::new(std::sync::Mutex::new(controller));
    let app = app_at(1440, 900);
    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        slot.clone(),
    );
    focusforge_gui::bridge::push_toast(&app, &controller, &slot);
    assert!(!app.get_toast_open(), "Projects shows no toast");
    assert_eq!(controller.lock().unwrap().unread_notifications(), 1);

    app.set_projects_overlay_open(true);
    let card = focusforge_gui::ProjectCardView {
        kind: "open".into(),
        opened: true,
        current: false,
        unsaved: false,
        fusing: false,
        fusing_fraction: 0.0,
        project_name: "demo".into(),
        meta_text: "".into(),
        headline: "".into(),
        stage: "".into(),
        reason: "".into(),
        action_label: "".into(),
        has_picture: false,
        picture: slint::Image::default(),
        x: 0.0,
        y: 0.0,
    };
    app.set_project_cards(std::rc::Rc::new(slint::VecModel::from(vec![card])).into());
    app.set_gallery_content_height(300.0);
    let recorded = std::rc::Rc::new(std::cell::RefCell::new(None));
    app.on_projects_card_clicked({
        let recorded = recorded.clone();
        move |i| *recorded.borrow_mut() = Some(i)
    });
    let position = slint::LogicalPosition::new(400.0, 170.0);
    app.window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    app.window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    assert_eq!(
        *recorded.borrow(),
        Some(0),
        "a point inside the closed toast's rect that is also inside the first \
         card must still reach the card"
    );
}

/// The four-square Projects-overlay button sits at the strip's far left and
/// the tabs start after it -- `ShellGeometry.projects-button-slot` wired to
/// `tabs-region`'s `x`, not merely declared and left unused: button x
/// 9..39, first tab x 47.
#[test]
fn tab_strip_reserves_the_projects_button_slot() {
    let app = app_at(1440, 900);

    let (x, _, _, _) = rect(&app, "FFTabStrip::tabs-region");
    assert_eq!(
        x, 47.0,
        "tabs-region must start at ShellGeometry.projects-button-slot (47px), not flush at x=0"
    );
    // The projects button is a 30x30 strip box at x 9, on the tabs' top edge.
    assert_rect(&app, "FFTabStrip::projects-button", (9.0, 6.0, 30.0, 30.0));
}

/// The tab strip and the icon bar under it are one column: the bar starts
/// on the strip boxes' bottom edge, the first tab's left frame sits on the
/// bar's 1 px right rule so the line runs unbroken from the tab into the
/// bar, and the projects button is centred on the bell and Settings glyphs.
#[test]
fn tab_strip_lines_up_with_the_icon_bar() {
    for (width, height) in [(1440, 900), (1200, 760)] {
        let app = app_at(width, height);
        let (bar_x, bar_y, bar_w, _) = rect(&app, "AppWindow::icon-bar");
        let (button_x, button_y, button_w, button_h) = rect(&app, "FFTabStrip::projects-button");
        let (tabs_x, ..) = rect(&app, "FFTabStrip::tabs-region");
        let (bell_x, _, bell_w, _) = rect(&app, "FFIconBar::bell");
        let (gear_x, _, gear_w, _) = rect(&app, "FFIconBar::settings");

        assert_eq!(
            bar_y,
            button_y + button_h,
            "the icon bar must start on the strip boxes' bottom edge at {width}x{height}"
        );
        assert_eq!(
            tabs_x,
            bar_x + bar_w - 1.0,
            "the first tab's left frame must sit on the icon bar's right rule at {width}x{height}"
        );
        let button_centre = button_x + button_w / 2.0;
        assert_eq!(
            button_centre,
            bell_x + bell_w / 2.0,
            "bell at {width}x{height}"
        );
        assert_eq!(
            button_centre,
            gear_x + gear_w / 2.0,
            "Settings at {width}x{height}"
        );
    }
}

/// The tab strip's scroll arrows must actually be wired to the
/// controller's flags, not merely lit over a strip with no overflow. Both
/// arrows show exactly while the strip overflows, and each greys out
/// (disabled) at its end of the list.
#[test]
fn scroll_arrows_are_bound_to_the_overflow_flags() {
    let tab_strip_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("tab_strip.slint")).unwrap();
    let code = common::strip_line_comments(&tab_strip_slint);

    for (arrow, flag) in [
        ("left-arrow", "can-scroll-left"),
        ("right-arrow", "can-scroll-right"),
    ] {
        let start = code
            .find(&format!("{arrow} := FFTabStripIconButton {{"))
            .unwrap_or_else(|| panic!("tab_strip.slint must declare {arrow}"));
        let body = &code[start..start + 400.min(code.len() - start)];
        assert!(
            body.contains("visible: root.tabs-overflow;")
                && body.contains(&format!("enabled: root.{flag};")),
            "{arrow} must bind visible to root.tabs-overflow and enabled to root.{flag}: {body}"
        );
    }
}

/// Every box on the tab strip -- each tab, the projects button, the scroll
/// arrows and "+" -- is one `FFStripBox`, which draws the 1px
/// `Theme.border-default` frame on its left, right and top edges. Sharing
/// the component is what keeps their top edge, height and frame from
/// drifting apart.
#[test]
fn every_strip_box_shares_one_framed_component() {
    let tab_strip_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("tab_strip.slint")).unwrap();
    let code = common::strip_line_comments(&tab_strip_slint);

    let start = code
        .find("component FFStripBox inherits Rectangle {")
        .expect("tab_strip.slint must declare FFStripBox");
    let end = start
        + code[start..]
            .find("component FFTab inherits FFStripBox {")
            .expect("FFStripBox must be followed by FFTab inheriting it");
    let body = &code[start..end];

    for needle in [
        "y: ShellGeometry.strip-box-top;",
        "height: ShellGeometry.strip-box-height;",
        "x: parent.width - 1px;",
        "background: Theme.border-default;",
    ] {
        assert!(
            body.contains(needle),
            "FFStripBox must carry {needle}: {body}"
        );
    }
    assert!(
        code.contains("component FFTabStripIconButton inherits FFStripBox {"),
        "the icon boxes must share FFStripBox with the tabs"
    );
}

/// The images panel header, the zoom cluster and the tab strip's window
/// controls all draw their glyph buttons from one shared primitive, at the
/// size and padding each call site sets -- not a copy declared locally.
#[test]
fn icon_button_is_a_shared_primitive() {
    let primitive = fs::read_to_string(
        common::ui_dir()
            .join("components")
            .join("ff_icon_button.slint"),
    )
    .unwrap();
    let primitive_code = common::strip_line_comments(&primitive);
    assert_eq!(
        primitive_code.matches("export component").count(),
        1,
        "ff_icon_button.slint must export exactly one component"
    );
    assert!(primitive_code.contains("export component FFIconButton"));
    for shell_import in ["shell/", "\"../shell"] {
        assert!(
            !primitive_code.contains(shell_import),
            "a primitive must import no shell file"
        );
    }

    for (region_path, needle) in [
        (
            common::ui_dir()
                .join("shell")
                .join("dock")
                .join("images_panel.slint"),
            "component FFImagesHeaderButton",
        ),
        (
            common::ui_dir()
                .join("shell")
                .join("bottom_bar")
                .join("zoom_cluster.slint"),
            "component FFZoomIconButton",
        ),
        (
            common::ui_dir().join("shell").join("tab_strip.slint"),
            "component WindowGlyphButton",
        ),
    ] {
        let region_code = common::strip_line_comments(&fs::read_to_string(&region_path).unwrap());
        assert!(
            !region_code.contains(needle),
            "{}: must declare no glyph-button component of its own",
            region_path.display()
        );
    }

    for (width, height) in [(1440, 900), (1200, 760)] {
        let app = app_at(width, height);
        app.set_project_loaded(true);
        for (id, side) in [
            ("FFZoomCluster::zoom-out", 32.0),
            ("FFImagesPanel::add-button", 22.0),
            ("FFTabStrip::window-close", 30.0),
        ] {
            let (_, _, w, h) = rect(&app, id);
            assert_eq!(
                (w, h),
                (side, side),
                "{id}: must render {side}x{side} at window width {width}"
            );
        }
    }
}

/// `FFTab`'s title/unsaved/close row must be anchored to the tab's full
/// bounds -- as a child of a plain `Rectangle` rather than a layout-managed
/// parent, an un-anchored `HorizontalLayout` sizes to its own content's
/// natural height, which is why its `vertical-alignment: center` texts
/// would otherwise read top-aligned instead of centered in the tab's
/// full 30px height.
#[test]
fn tab_content_row_fills_the_tabs_full_bounds() {
    let tab_strip_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("tab_strip.slint")).unwrap();
    let code = common::strip_line_comments(&tab_strip_slint);

    let layout_start = code
        .find("HorizontalLayout {")
        .expect("FFTab must declare its content HorizontalLayout");
    let body = &code[layout_start..layout_start + 300.min(code.len() - layout_start)];

    for needle in ["x: 0px;", "y: 0px;", "width: 100%;", "height: 100%;"] {
        assert!(
            body.contains(needle),
            "FFTab's content HorizontalLayout must anchor to the tab's full bounds \
             ({needle} missing): {body}"
        );
    }
}

/// The tabs, "+" and the visible scroll arrows share one top edge and one
/// height -- measured on the live element tree, not on source text.
#[test]
fn strip_boxes_share_top_edge_and_height() {
    let app = app_at(1440, 900);

    let (_, plus_y, plus_w, plus_h) = rect(&app, "FFTabStrip::plus-button");
    let (_, button_y, _, button_h) = rect(&app, "FFTabStrip::projects-button");
    assert_eq!((plus_y, plus_h), (button_y, button_h));
    assert_eq!((plus_y, plus_w, plus_h), (6.0, 30.0, 30.0));
}

/// The window glyphs share the strip boxes' vertical band, so their glyphs
/// centre on the tab titles' and "+"'s line, and sit at x
/// 1342 / 1378 / 1414 (16 px glyph, 7 px box margin, 36 px pitch) --
/// right-anchored, so the inset survives a resize.
#[test]
fn window_glyphs_share_the_strip_box_band() {
    for (width, height) in [(1440, 900), (1200, 760)] {
        let app = app_at(width, height);
        let (_, plus_y, _, plus_h) = rect(&app, "FFTabStrip::plus-button");
        let right = width as f32;
        for (i, id) in [
            "FFTabStrip::window-minimize",
            "FFTabStrip::window-maximize",
            "FFTabStrip::window-close",
        ]
        .into_iter()
        .enumerate()
        {
            let (x, y, w, h) = rect(&app, id);
            assert_eq!(
                (y, h),
                (plus_y, plus_h),
                "{id} must share the strip boxes' band"
            );
            assert_eq!(w, 30.0, "{id}");
            assert_eq!(
                x,
                right - 105.0 + i as f32 * 36.0,
                "{id} at window width {width}"
            );
        }
    }
}

/// The accessible label of the first element matching `id` -- a `Text`'s is
/// its `text`.
fn label(app: &AppWindow, id: &str) -> String {
    ElementQuery::from_root(app)
        .match_id(id)
        .find_first()
        .unwrap_or_else(|| panic!("no element matched id {id:?}"))
        .accessible_label()
        .unwrap_or_else(|| panic!("{id}: no accessible label"))
        .to_string()
}

/// The drop caption is one short line wherever dropping works, and says so
/// where it does not.
#[test]
fn drop_caption_follows_drop_unavailable() {
    let app = app_at(1440, 900);
    let caption = "FFEmptyState::drop-caption";
    assert_eq!(label(&app, caption), "Drop a folder or stack");
    app.set_drop_unavailable(true);
    assert_eq!(
        label(&app, caption),
        "Dropping is unavailable on native Wayland"
    );
    app.set_drop_unavailable(false);
    assert_eq!(label(&app, caption), "Drop a folder or stack");
}

/// The empty state: the wordmark above one raised 520-wide card, centred
/// on the canvas column, holding the title, the copy,
/// the single `Open…` button (168 x 34) and the drop caption, in that
/// order and all inside the card's 32 px inset.
#[test]
fn empty_state_is_a_card_under_the_wordmark() {
    let app = app_at(1440, 900);

    let card = rect(&app, "FFEmptyState::empty-card");
    assert_eq!((card.0, card.2), (354.0, 520.0), "card x/width: {card:?}");
    let wordmark = rect(&app, "FFEmptyState::wordmark");
    assert_eq!(
        (wordmark.0, wordmark.2),
        (470.0, 288.0),
        "wordmark: {wordmark:?}"
    );
    assert!(
        wordmark.1 + wordmark.3 < card.1,
        "the wordmark sits above the card"
    );

    assert_eq!(label(&app, "FFEmptyState::empty-title"), "No project open");
    assert_eq!(
        label(&app, "FFEmptyState::empty-body"),
        "Choose the frames of one stack, or drop a folder: each stack it holds becomes its own project, in its own tab."
    );
    assert_eq!(label(&app, "FFEmptyState::open"), "Open…");
    assert_eq!(
        label(&app, "FFEmptyState::drop-caption"),
        "Drop a folder or stack"
    );

    let button = rect(&app, "FFEmptyState::open");
    assert_eq!(
        (button.2, button.3),
        (168.0, 34.0),
        "button size: {button:?}"
    );

    let mut previous_bottom = card.1;
    for id in [
        "FFEmptyState::empty-title",
        "FFEmptyState::empty-body",
        "FFEmptyState::open",
        "FFEmptyState::drop-caption",
    ] {
        let (x, y, w, h) = rect(&app, id);
        assert_eq!(x, card.0 + 32.0, "{id}: 32 px left inset");
        assert!(
            x + w <= card.0 + card.2 - 32.0 + EPS,
            "{id}: inside the right inset"
        );
        assert!(y >= previous_bottom, "{id}: below its predecessor");
        previous_bottom = y + h;
    }
    assert!(
        previous_bottom <= card.1 + card.3 - 32.0 + EPS,
        "the caption ends inside the card's bottom inset"
    );
}

/// The empty state offers one `Open…` button, no folder button, and pressing
/// it raises `open-requested` once, at both window sizes.
#[test]
fn empty_state_open_button_raises_the_open_request() {
    for (width, height) in [(1440, 900), (1200, 760)] {
        let app = app_at(width, height);
        assert_eq!(label(&app, "FFEmptyState::open"), "Open…");
        assert!(
            ElementQuery::from_root(&app)
                .match_id("FFEmptyState::open-folder")
                .find_first()
                .is_none(),
            "{width}x{height}: no folder button"
        );
        let card = rect(&app, "FFEmptyState::empty-card");
        let button = rect(&app, "FFEmptyState::open");
        let caption = rect(&app, "FFEmptyState::drop-caption");
        assert_eq!(button.0, card.0 + 32.0, "{width}x{height}: 32 px inset");
        assert!(caption.1 >= button.1 + button.3, "caption below the button");

        let requested = std::rc::Rc::new(std::cell::Cell::new(0));
        app.on_open_requested({
            let requested = requested.clone();
            move || requested.set(requested.get() + 1)
        });
        ElementQuery::from_root(&app)
            .match_id("FFEmptyState::open")
            .find_first()
            .expect("the open button")
            .mock_single_click(slint::platform::PointerEventButton::Left);
        assert_eq!(requested.get(), 1, "{width}x{height}: one request");
    }
}

/// The dock without a project: the checkpoint panel's empty line, a
/// disabled `Export image`, and `No project open.` in
/// place of the dock tab row, which only a loaded project shows.
#[test]
fn dock_without_a_project_says_so() {
    let app = app_at(1440, 900);

    assert_eq!(
        label(&app, "FFCheckpointPanel::empty-note"),
        "None yet — fuse a stack"
    );
    assert_eq!(
        label(&app, "FFCheckpointPanel::checkpoint-export"),
        "Export image"
    );
    assert_rect(
        &app,
        "FFCheckpointPanel::checkpoint-export",
        (1309.0, 93.0, 119.0, 26.0),
    );
    assert_eq!(label(&app, "FFDock::no-project-note"), "No project open.");
    let rule = rect(&app, "FFDock::checkpoint-rule");
    let note = rect(&app, "FFDock::no-project-note");
    assert!(note.1 > rule.1, "the note sits under the checkpoint rule");

    let found = |id: &str| {
        ElementQuery::from_root(&app)
            .match_id(id)
            .find_first()
            .is_some()
    };
    assert!(
        !found("FFDock::dock-tabs"),
        "no dock tab row without a project"
    );

    app.set_project_loaded(true);
    assert!(
        found("FFDock::dock-tabs"),
        "a loaded project shows the dock tab row"
    );
    assert!(
        !found("FFDock::no-project-note"),
        "a loaded project hides the note"
    );
}

/// The editor-mode dock is tabless: no tab row and no Settings panel even
/// while the Settings tab is the chosen one, and the frame legend (no add
/// button) in their place; stacking mode brings the tabs and the Settings
/// panel back.
#[test]
fn editor_dock_has_no_tab_row() {
    let app = app_at(1440, 900);
    let found = |id: &str| {
        ElementQuery::from_root(&app)
            .match_id(id)
            .find_first()
            .is_some()
    };
    app.set_project_loaded(true);
    app.set_dock_tab("settings".into());

    app.set_mode("editor".into());
    assert!(!found("FFDock::dock-tabs"), "editor mode shows no tab row");
    assert!(
        !found("FFDock::settings-panel"),
        "editor mode hides the Settings panel"
    );
    assert!(
        found("FFDock::images-panel"),
        "editor mode shows the frame legend"
    );
    assert!(
        !found("FFImagesPanel::add-button"),
        "the legend is read-only"
    );

    app.set_mode("stacking".into());
    assert!(
        found("FFDock::dock-tabs"),
        "stacking mode shows the tab row"
    );
    assert!(
        found("FFDock::settings-panel"),
        "stacking mode shows the Settings panel for the chosen tab"
    );
    assert!(
        !found("FFDock::images-panel"),
        "the Settings tab hides the frame list"
    );
}

/// The bottom bar's status line without a project: `No project open`, 20 px
/// in from the bar's left edge.
#[test]
fn bottom_bar_status_reads_no_project_open() {
    let app = app_at(1440, 900);

    assert_eq!(label(&app, "FFBottomBar::status-text"), "No project open");
    let (x, _, _, _) = rect(&app, "FFBottomBar::status-text");
    assert_eq!(x, 68.0);

    app.set_project_loaded(true);
    assert_ne!(
        label(&app, "FFBottomBar::status-text"),
        "No project open",
        "a loaded project does not read \"No project open\""
    );
}

/// The toolbar's transport buttons: 116 x 26, 10 px apart, the first
/// `Theme.space-3` (12px) inside the toolbar (x 60) and the second at 186px
/// -- the running state's
/// `Pause`/`Cancel` pair.
#[test]
fn transport() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);
    app.set_toolbar_run_state("running".into());

    assert_rect(&app, "FFToolbar::pause-button", (60.0, 43.0, 116.0, 26.0));
    assert_rect(&app, "FFToolbar::cancel-button", (186.0, 43.0, 116.0, 26.0));
}

/// The bottom bar's progress track, in reading order (stage -> step -> bar
/// -> ETA), at the base width and at the 1200px minimum: the bar's width is
/// dynamic, always ending `Theme.space-4` before the ETA.
#[test]
fn bottom_bar_progress() {
    let app = app_at(1440, 900);
    app.set_project_loaded(true);
    app.set_stage_word("Aligning".into());
    app.set_step_text("42 of 61".into());
    app.set_bar_fraction(0.5);
    app.set_eta_text("ETA 1:12".into());

    let (step_x, step_y, _, _) = rect(&app, "FFBottomBar::step-text");
    assert_eq!((step_x, step_y), (166.0, 856.0));
    assert_rect(
        &app,
        "FFBottomBar::progress-bar",
        (250.0, 876.0, 562.0, 4.0),
    );
    let (eta_x, eta_y, _, _) = rect(&app, "FFBottomBar::eta-text");
    assert_eq!((eta_x, eta_y), (828.0, 856.0));

    let app_min = app_at(1200, 760);
    app_min.set_project_loaded(true);
    app_min.set_stage_word("Aligning".into());
    app_min.set_step_text("42 of 61".into());
    app_min.set_bar_fraction(0.5);
    app_min.set_eta_text("ETA 1:12".into());

    let (step_x, step_y, _, _) = rect(&app_min, "FFBottomBar::step-text");
    assert_eq!((step_x, step_y), (166.0, 716.0));
    assert_rect(
        &app_min,
        "FFBottomBar::progress-bar",
        (250.0, 736.0, 322.0, 4.0),
    );
    let (eta_x, eta_y, _, _) = rect(&app_min, "FFBottomBar::eta-text");
    assert_eq!((eta_x, eta_y), (588.0, 716.0));
}

/// The ETA readout never reaches the overlay caption to its right, at the
/// minimum width and wider, even for an hours-long estimate.
#[test]
fn bottom_bar_eta_clears_the_overlay_caption() {
    for width in [1200, 1280, 1440] {
        let app = app_at(width, 900);
        app.set_project_loaded(true);
        app.set_bar_fraction(0.5);
        app.set_eta_text("ETA 12:34:56".into());

        let (eta_x, _, eta_w, _) = rect(&app, "FFBottomBar::eta-text");
        let (caption_x, _, _, _) = rect(&app, "FFBottomBar::overlay-caption");
        assert!(
            eta_x + eta_w <= caption_x,
            "{width}px wide: ETA spans {eta_x}..{}, overlay caption starts at {caption_x}",
            eta_x + eta_w
        );
        let (bar_x, _, bar_w, _) = rect(&app, "FFBottomBar::progress-bar");
        assert!(
            bar_x + bar_w <= eta_x,
            "{width}px wide: track ends at {}, ETA starts at {eta_x}",
            bar_x + bar_w
        );
    }
}

/// The loading indicator's show threshold: still hidden 100ms short of the
/// 500ms it needs, visible once past it.
#[test]
fn loading_indicator_appears_only_past_its_threshold() {
    let app = app_at(1440, 900);
    app.set_load_active(true);
    // Registers the freshly-`running` threshold timer against the current
    // mocked tick -- without this zero-length call, the later jump below
    // would start the timer's own countdown from whatever tick it first
    // gets instantiated at instead of from here.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);

    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(400));
    assert!(
        !app.get_load_indicator_visible(),
        "must stay hidden 100ms short of the 500ms threshold"
    );

    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(150));
    assert!(
        app.get_load_indicator_visible(),
        "must be visible once the threshold has passed"
    );
}

/// A load that ends before the threshold never shows the indicator at all --
/// clearing `load-active` before the gate opens must not open it later.
#[test]
fn loading_indicator_never_shows_for_a_load_that_ends_before_the_threshold() {
    let app = app_at(1440, 900);
    app.set_load_active(true);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);

    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(400));
    assert!(!app.get_load_indicator_visible());

    app.set_load_active(false);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(150));
    assert!(
        !app.get_load_indicator_visible(),
        "a load that finished before the threshold must never flash the indicator"
    );
}

/// The bottom bar's overlay caption, toggle and slider exist in stacking and
/// in editor mode, with or without a project, and the slider stays present
/// (greyed by its own `disabled` binding) whether the overlay is on or off.
#[test]
fn overlay_controls_exist_in_both_modes() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);

    let present = |id: &str| {
        ElementQuery::from_root(&app)
            .match_id(id)
            .find_first()
            .is_some()
    };
    let ids = [
        "FFBottomBar::overlay-caption",
        "FFBottomBar::overlay-toggle",
        "FFBottomBar::overlay-slider",
    ];

    for mode in ["stacking", "editor"] {
        app.set_mode(mode.into());
        for loaded in [false, true] {
            app.set_project_loaded(loaded);
            for on in [false, true] {
                app.set_overlay_on(on);
                for id in ids {
                    assert!(
                        present(id),
                        "{id}: {mode} mode, project {loaded}, overlay {on}"
                    );
                }
            }
        }
    }
}

/// With `overlay-available` false the toggle takes no click in stacking mode;
/// with it true the same click fires `overlay-toggled` once.
#[test]
fn overlay_toggle_is_inert_while_disabled() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);
    app.set_mode("stacking".into());
    let fired = std::rc::Rc::new(std::cell::Cell::new(0u32));
    app.on_overlay_toggled({
        let fired = fired.clone();
        move |_| fired.set(fired.get() + 1)
    });

    let (x, y, w, h) = rect(&app, "FFBottomBar::overlay-toggle");
    let position = slint::LogicalPosition::new(x + w / 2.0, y + h / 2.0);
    let click = || {
        app.window()
            .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        app.window()
            .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
    };

    app.set_overlay_available(false);
    click();
    assert_eq!(fired.get(), 0, "a disabled toggle takes no click");

    app.set_overlay_available(true);
    click();
    assert_eq!(fired.get(), 1, "an enabled toggle fires once");
}

#[test]
fn detached_pane_uses_a_distinct_accessible_dock_control() {
    let pane = include_str!("../ui/components/ff_pane.slint");
    let control = include_str!("../ui/components/ff_undock_control.slint");
    assert!(pane.contains("visible: root.project-loaded && root.undock-enabled"));
    assert!(control.contains("Dock pane"));
    assert!(control.contains("dock-pane.svg"));
}

#[test]
fn detached_main_pane_fills_the_canvas_on_either_side() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);
    for (detached, remaining) in [
        ("frame", "FFCanvas::pane-right"),
        ("composite", "FFCanvas::pane-left"),
    ] {
        app.set_detached_pane(detached.into());
        assert_rect(&app, remaining, (64.0, 92.0, 1100.0, 748.0));
    }
    app.set_detached_pane("".into());
    assert_rect(&app, "FFCanvas::pane-left", (64.0, 92.0, 542.0, 748.0));
    assert_rect(&app, "FFCanvas::pane-right", (622.0, 92.0, 542.0, 748.0));
}

// --- Performance tab -------------------------------------------------------

type SharedController = std::sync::Arc<std::sync::Mutex<AppController>>;

fn performance_window(controller: &SharedController) -> AppWindow {
    let app = app_at(1440, 900);
    focusforge_gui::bridge::push_performance_view(&app, controller);
    app.set_settings_open(true);
    app.set_settings_section("performance".into());
    app
}

fn performance_tab(app: &AppWindow) -> ElementHandle {
    ElementQuery::from_root(app)
        .match_descendants()
        .match_type_name("FFPerformanceTab")
        .find_first()
        .expect("no FFPerformanceTab element found")
}

/// Whether the tab's switch draws as on: the knob rests at the right end.
fn performance_switch_is_on(tab: &ElementHandle) -> bool {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    let toggle = by_position(tab.query_descendants().match_type_name("FFToggle"));
    let knob = by_position(tab.query_descendants().match_id("FFToggle::knob"));
    assert_eq!(knob.len(), 1, "the tab has exactly one switch");
    knob[0].absolute_position().x - toggle[0].absolute_position().x > 10.0
}

fn performance_cap_inputs(tab: &ElementHandle) -> Vec<ElementHandle> {
    by_position(tab.query_descendants().match_id("FFTextField::input"))
}

fn performance_row_texts(tab: &ElementHandle) -> Vec<String> {
    by_position(tab.query_descendants().match_type_name("FFRowText"))
        .iter()
        .map(|text| text.accessible_label().unwrap_or_default().to_string())
        .collect()
}

fn performance_placeholders(tab: &ElementHandle) -> Vec<String> {
    by_position(
        tab.query_descendants()
            .match_type_name("FFTextField")
            .match_descendants()
            .match_accessible_role(AccessibleRole::Text),
    )
    .iter()
    .map(|text| text.accessible_label().unwrap_or_default().to_string())
    .collect()
}

/// The tab draws what the controller's view holds: the switch, both cap
/// fields (or their placeholders) and the three read-only figures, whichever
/// way the stored settings are.
#[test]
fn performance_tab_shows_the_controller_view() {
    let mut on = AppController::new();
    on.set_gpu_fusion(true);
    let mut capped = AppController::new();
    capped.set_gpu_fusion(false);
    capped.set_host_memory_cap("2GiB");
    capped.set_vram_cap("512MiB");

    for (controller, gpu, caps) in [(on, true, ["", ""]), (capped, false, ["2GiB", "512MiB"])] {
        let view = controller.performance_view();
        assert_eq!(view.gpu, gpu);
        let shared: SharedController = std::sync::Arc::new(std::sync::Mutex::new(controller));
        let app = performance_window(&shared);
        let tab = performance_tab(&app);

        assert_eq!(performance_switch_is_on(&tab), gpu);
        let texts: Vec<String> = performance_cap_inputs(&tab)
            .iter()
            .map(|input| input.accessible_value().unwrap_or_default().to_string())
            .collect();
        assert_eq!(texts, caps);
        let placeholders = performance_placeholders(&tab);
        if gpu {
            assert_eq!(placeholders, ["No cap", "No cap"]);
        } else {
            assert!(placeholders.is_empty(), "{placeholders:?}");
        }
        assert_eq!(
            performance_row_texts(&tab),
            [
                view.backend.as_str(),
                view.host_budget.as_str(),
                view.vram_budget.as_str()
            ]
        );
        assert_eq!(view.vram_budget, "Not probed yet");
    }
}

/// The tab is layout over shared components: its sections, rows, switch,
/// fields and note are the shared ones, and it styles nothing itself.
#[test]
fn performance_tab_composes_only_shared_components() {
    let tab = ui_source("shell/settings_modal/performance_tab.slint");
    for needle in [
        " Text {",
        "color:",
        "font-",
        "background",
        "border-",
        "Theme.status-",
        "Theme.text-",
        "Theme.surface-",
    ] {
        assert!(!tab.contains(needle), "the tab sets {needle:?}");
    }
    for shared in [
        "FFSettingsSection {",
        "FFSettingsRow {",
        "FFToggle {",
        "FFTextField {",
        "FFRowText {",
        "FFStatusText {",
    ] {
        assert!(tab.contains(shared), "the tab does not draw {shared}");
    }
}

// --- Storage tab -----------------------------------------------------------

fn storage_window(controller: &SharedController) -> AppWindow {
    let app = app_at(1440, 900);
    focusforge_gui::bridge::push_storage_view(&app, controller);
    app.set_settings_open(true);
    app.set_settings_section("storage".into());
    app
}

fn storage_tab(app: &AppWindow) -> ElementHandle {
    ElementQuery::from_root(app)
        .match_descendants()
        .match_type_name("FFStorageTab")
        .find_first()
        .expect("no FFStorageTab element found")
}

/// The tab's row texts, top to bottom: each location's path, its suffix or
/// the unavailable word, then the action row's helper.
fn storage_row_texts(tab: &ElementHandle) -> Vec<String> {
    by_position(tab.query_descendants().match_type_name("FFRowText"))
        .iter()
        .map(|text| text.accessible_label().unwrap_or_default().to_string())
        .collect()
}

fn storage_button(tab: &ElementHandle) -> ElementHandle {
    let mut buttons = by_position(tab.query_descendants().match_type_name("FFButton"));
    assert_eq!(buttons.len(), 1, "the tab has exactly one button");
    buttons.remove(0)
}

fn storage_status_line(tab: &ElementHandle) -> String {
    let lines = by_position(tab.query_descendants().match_type_name("FFStatusText"));
    assert_eq!(lines.len(), 1, "the tab has exactly one status line");
    lines[0].accessible_label().unwrap_or_default().to_string()
}

/// The tab draws what the controller's view holds: the four locations with a
/// suffix on a directory not created yet, the button's enabled state and the
/// status line, and the button raises the clear request only while enabled.
#[test]
fn storage_tab_shows_the_controller_view() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("shown-config");
    let data = root.path().join("shown-data");
    for dir in [&config, &data.join("logs"), &data.join("scratch")] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(config.join("settings.toml"), "").unwrap();
    let mut controller = AppController::new();
    controller.install_recent(Vec::new(), config);
    controller.set_diagnostics_dirs(Some(data.clone()), None);
    controller.set_scratch_root_for_test(data.join("scratch"));

    let view = controller.storage_view();
    assert!(view.crashes.missing && !view.logs.missing && !view.scratch.missing);
    let shared: SharedController = std::sync::Arc::new(std::sync::Mutex::new(controller));
    let app = storage_window(&shared);
    let tab = storage_tab(&app);

    assert_eq!(
        storage_row_texts(&tab),
        [
            view.settings_file.path.as_str(),
            view.logs.path.as_str(),
            view.crashes.path.as_str(),
            "· not created yet",
            view.scratch.path.as_str(),
            "Deletes cached fuse stages.",
        ]
    );
    assert!(view.crashes.path.contains("shown-data") && view.crashes.path.ends_with("crashes"));
    let button = storage_button(&tab);
    assert_eq!(button.accessible_label().as_deref(), Some("Clear cache"));
    assert_eq!(button.accessible_enabled(), Some(true));
    assert_eq!(storage_status_line(&tab), "");

    let requested = std::rc::Rc::new(std::cell::Cell::new(0));
    app.on_storage_clear_requested({
        let requested = requested.clone();
        move || requested.set(requested.get() + 1)
    });
    button.mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(requested.get(), 1, "an enabled button asks for the clear");

    let mut state = app.get_storage();
    state.clear_enabled = false;
    state.status = "A run is active.".into();
    state.status_tone = "warning".into();
    app.set_storage(state);
    assert_eq!(button.accessible_enabled(), Some(false));
    assert_eq!(storage_status_line(&tab), "A run is active.");
    button.mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(requested.get(), 1, "a disabled button asks for nothing");
}

/// With no config or data directory the rows read `Unavailable` and the
/// scratch row keeps its path; a clear's result reaches the status line.
#[test]
fn storage_tab_shows_unresolved_locations_and_the_last_result() {
    let scratch = tempfile::tempdir().unwrap();
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(scratch.path().to_path_buf());
    controller.clear_stage_cache();
    let view = controller.storage_view();
    let shared: SharedController = std::sync::Arc::new(std::sync::Mutex::new(controller));
    let app = storage_window(&shared);
    let tab = storage_tab(&app);

    assert_eq!(
        storage_row_texts(&tab),
        [
            "Unavailable",
            "Unavailable",
            "Unavailable",
            view.scratch.path.as_str(),
            "Deletes cached fuse stages.",
        ]
    );
    assert_eq!(storage_status_line(&tab), "Cleared 0 stage entries.");
    assert_eq!(app.get_storage().status_tone.as_str(), "success");
}

/// The tab is layout over shared components: its sections, path rows and
/// action row are the shared ones, and it styles nothing itself.
#[test]
fn storage_tab_composes_only_shared_components() {
    let tab = ui_source("shell/settings_modal/storage_tab.slint");
    for needle in [
        " Text {",
        "color:",
        "font-",
        "background",
        "border-",
        "Theme.status-",
        "Theme.text-",
        "Theme.surface-",
    ] {
        assert!(!tab.contains(needle), "the tab sets {needle:?}");
    }
    for shared in [
        "FFSettingsSection {",
        "FFSettingsPathRow {",
        "FFSettingsActionRow {",
    ] {
        assert!(tab.contains(shared), "the tab does not draw {shared}");
    }
}

fn type_into(app: &AppWindow, text: &str) {
    use slint::platform::WindowEvent;
    for ch in text.chars() {
        let key: slint::SharedString = ch.to_string().into();
        app.window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
        app.window()
            .dispatch_event(WindowEvent::KeyReleased { text: key });
    }
}

/// Clicking the switch asks for the opposite setting, and typing in a cap
/// field reports every edit with the cursor's new position.
#[test]
fn performance_controls_report_to_their_callbacks() {
    let controller = AppController::new();
    let shared: SharedController = std::sync::Arc::new(std::sync::Mutex::new(controller));
    let app = performance_window(&shared);
    let tab = performance_tab(&app);

    let toggled = std::rc::Rc::new(std::cell::RefCell::new(Vec::<bool>::new()));
    app.on_performance_gpu_toggled({
        let toggled = toggled.clone();
        move |on| toggled.borrow_mut().push(on)
    });
    let host_edits = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_performance_host_cap_edited({
        let edits = host_edits.clone();
        move |text| edits.borrow_mut().push(text.to_string())
    });
    let vram_edits = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    app.on_performance_vram_cap_edited({
        let edits = vram_edits.clone();
        move |text| edits.borrow_mut().push(text.to_string())
    });
    let cursors = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(char, f32, f32)>::new()));
    app.on_performance_host_cap_cursor_moved({
        let cursors = cursors.clone();
        move |x, w| cursors.borrow_mut().push(('h', x, w))
    });
    app.on_performance_vram_cap_cursor_moved({
        let cursors = cursors.clone();
        move |x, w| cursors.borrow_mut().push(('v', x, w))
    });

    let before = performance_switch_is_on(&tab);
    tab.query_descendants()
        .match_id("FFToggle::area")
        .find_first()
        .expect("the switch has a touch area")
        .mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(*toggled.borrow(), [!before]);

    let inputs = performance_cap_inputs(&tab);
    inputs[0].mock_single_click(slint::platform::PointerEventButton::Left);
    type_into(&app, "2G");
    inputs[1].mock_single_click(slint::platform::PointerEventButton::Left);
    type_into(&app, "5");
    assert_eq!(*host_edits.borrow(), ["2", "2G"]);
    assert_eq!(*vram_edits.borrow(), ["5"]);

    let cursors = cursors.borrow();
    let host: Vec<_> = cursors.iter().filter(|c| c.0 == 'h').collect();
    let vram: Vec<_> = cursors.iter().filter(|c| c.0 == 'v').collect();
    assert!(
        host.last().is_some_and(|c| c.1 > 0.0),
        "typing moves the host cursor right: {host:?}"
    );
    assert!(
        vram.last().is_some_and(|c| c.1 > 0.0),
        "typing moves the VRAM cursor right: {vram:?}"
    );
    let visible = 136.0 - 2.0 * 6.0;
    assert!(
        cursors.iter().all(|c| (c.2 - visible).abs() < EPS),
        "the visible width is the field less its two insets: {cursors:?}"
    );
}

/// A cap entry longer than the field scrolls to keep the cursor in view,
/// and moving the cursor to the start brings the start back.
#[test]
fn performance_cap_entry_scrolls_to_follow_the_cursor() {
    use slint::platform::{Key, WindowEvent};

    let controller: SharedController =
        std::sync::Arc::new(std::sync::Mutex::new(AppController::new()));
    let app = app_at(1440, 900);
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_settings_open(true);
    app.set_settings_section("performance".into());
    focusforge_gui::bridge::push_performance_view(&app, &controller);
    let tab = performance_tab(&app);

    for field in 0..2 {
        performance_cap_inputs(&tab)[field]
            .mock_single_click(slint::platform::PointerEventButton::Left);
        let rest = performance_cap_inputs(&tab)[field].absolute_position().x;

        type_into(&app, "12345");
        assert_eq!(
            performance_cap_inputs(&tab)[field].absolute_position().x,
            rest,
            "field {field}: a short entry does not scroll"
        );

        type_into(&app, &"1234567890".repeat(4));
        let scrolled = performance_cap_inputs(&tab)[field].absolute_position().x;
        assert!(
            scrolled < rest - 20.0,
            "field {field}: a long entry scrolls left, rest {rest}, now {scrolled}"
        );

        // Plain Home moves no cursor on Apple platforms; the Control/Command
        // chord with Left reaches the start of this one-word entry on all.
        for key in [Key::Control, Key::LeftArrow] {
            app.window()
                .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        }
        for key in [Key::LeftArrow, Key::Control] {
            app.window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        }
        assert_eq!(
            performance_cap_inputs(&tab)[field].absolute_position().x,
            rest,
            "field {field}: the start-of-entry chord shows the start of the entry again"
        );
    }
}

// --- About tab -------------------------------------------------------------

const ABOUT_REDISTRIBUTE: &str = "FocusForge is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3 of the License.";
const ABOUT_WARRANTY: &str = "This program comes with ABSOLUTELY NO WARRANTY. See the LICENSE file for the full license text.";

fn about_window() -> AppWindow {
    let app = app_at(1440, 900);
    focusforge_gui::bridge::push_about_view(&app);
    app.set_settings_open(true);
    app.set_settings_section("about".into());
    app
}

/// The text the About tab draws, one string per line of the tab: a row is its
/// label and value joined by ` | `, a heading or paragraph stands alone.
fn about_rows(app: &AppWindow) -> Vec<String> {
    let tab = ElementQuery::from_root(app)
        .match_descendants()
        .match_type_name("FFAboutTab")
        .find_first()
        .expect("no FFAboutTab element found");
    let mut texts: Vec<(f32, f32, String)> = Vec::new();
    for handle in tab
        .query_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .find_all()
    {
        let pos = handle.absolute_position();
        if texts.iter().all(|(x, y, _)| (*x, *y) != (pos.x, pos.y)) {
            let label = handle.accessible_label().unwrap_or_default().to_string();
            texts.push((pos.x, pos.y, label));
        }
    }
    texts.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut lines: Vec<(f32, Vec<(f32, String)>)> = Vec::new();
    for (x, y, label) in texts {
        match lines.last_mut() {
            Some((top, line)) if y - *top < 8.0 => line.push((x, label)),
            _ => lines.push((y, vec![(x, label)])),
        }
    }
    lines
        .into_iter()
        .map(|(_, mut line)| {
            line.sort_by(|a, b| a.0.total_cmp(&b.0));
            line.into_iter()
                .map(|(_, label)| label)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect()
}

/// The version and the source come from the build, the licence and the notice
/// are fixed text; a state set from outside is drawn as given, so neither
/// build value is a literal in the tab.
#[test]
fn about_tab_shows_the_build_metadata() {
    let app = about_window();
    let version = env!("CARGO_PKG_VERSION");

    let rows = about_rows(&app);
    let (source_row, rest) = rows.split_last().expect("the tab draws rows");
    let source = source_row
        .strip_prefix("Source | ")
        .expect("the last row is Source");
    assert!(!source.is_empty());
    assert_eq!(
        format!("https://{source}"),
        env!("CARGO_PKG_REPOSITORY"),
        "the source is the repository without its scheme"
    );
    let notice = |version: &str| -> Vec<String> {
        vec![
            "FOCUSFORGE".to_string(),
            format!("Version | {version}"),
            "License | GPL-3.0".to_string(),
            "NOTICE".to_string(),
            "Copyright © 2026 Johannes Sood".to_string(),
            ABOUT_REDISTRIBUTE.to_string(),
            ABOUT_WARRANTY.to_string(),
        ]
    };
    assert_eq!(rest, notice(version));

    app.set_about(focusforge_gui::AboutState {
        version: "9.9.9".into(),
        source: "example.org/x".into(),
    });
    let rows = about_rows(&app);
    let (source_row, rest) = rows.split_last().unwrap();
    assert_eq!(source_row, "Source | example.org/x");
    assert_eq!(rest, notice("9.9.9"));
}

/// The tab is layout over shared components: its sections, rows and texts are
/// the shared ones, and it styles nothing itself.
#[test]
fn about_tab_composes_only_shared_components() {
    let tab = ui_source("shell/settings_modal/about_tab.slint");
    for needle in [
        " Text {",
        "color:",
        "font-",
        "background",
        "border-",
        "Theme.status-",
        "Theme.text-",
        "Theme.surface-",
    ] {
        assert!(!tab.contains(needle), "the tab sets {needle:?}");
    }
    for shared in ["FFSettingsSection {", "FFSettingsRow {", "FFRowText {"] {
        assert!(tab.contains(shared), "the tab does not draw {shared}");
    }
}

fn string_model(items: &[&str]) -> slint::ModelRc<slint::SharedString> {
    let items: Vec<slint::SharedString> = items.iter().map(|s| (*s).into()).collect();
    slint::ModelRc::new(slint::VecModel::from(items))
}

fn all(app: &AppWindow, id: &str) -> Vec<ElementHandle> {
    ElementQuery::from_root(app).match_id(id).find_all()
}

fn one(app: &AppWindow, id: &str) -> ElementHandle {
    let mut found = all(app, id);
    assert_eq!(found.len(), 1, "exactly one {id}");
    found.remove(0)
}

/// The label and checked state of every segment under each `FFSegmented`.
fn segment_rows(app: &AppWindow) -> Vec<Vec<(String, Option<bool>)>> {
    all(app, "FFSegmented::root")
        .iter()
        .map(|row| {
            row.query_descendants()
                .match_id("FFSegmented::segment")
                .find_all()
                .iter()
                .inspect(|s| {
                    assert_eq!(s.accessible_role(), Some(AccessibleRole::Button));
                    assert_eq!(s.accessible_enabled(), Some(true));
                })
                .map(|s| {
                    (
                        s.accessible_label().unwrap().to_string(),
                        s.accessible_checked(),
                    )
                })
                .collect()
        })
        .collect()
}

/// Each segment of an `FFSegmented` is a button carrying its label, and only
/// the selected one reports checked.
#[test]
fn segmented_segments_are_labelled_buttons() {
    let app = app_at(1440, 900);
    app.set_export_open(true);
    app.set_export_format_segments(string_model(&["TIFF", "PNG", "JPEG"]));
    app.set_export_format_selected("JPEG".into());
    let row = |id: &str, choices: &[&str], selected: &str| focusforge_gui::ExportRowView {
        id: id.into(),
        choices: string_model(choices),
        value_text: selected.into(),
        ..Default::default()
    };
    app.set_export_rows(
        std::rc::Rc::new(slint::VecModel::from(vec![
            row("quality", &[], "90"),
            row("chroma", &["4:4:4", "4:2:0"], "4:2:0"),
        ]))
        .into(),
    );
    app.set_export_slider_group(true);

    let mark = |label: &str, selected: &str| (label.to_string(), Some(label == selected));
    assert_eq!(
        segment_rows(&app),
        [
            ["TIFF", "PNG", "JPEG"].map(|l| mark(l, "JPEG")).to_vec(),
            ["4:4:4", "4:2:0"].map(|l| mark(l, "4:2:0")).to_vec(),
        ]
    );
    let window_wide: Vec<_> = all(&app, "FFSegmented::segment")
        .iter()
        .map(|s| s.accessible_label().unwrap().to_string())
        .collect();
    assert_eq!(window_wide, ["TIFF", "PNG", "JPEG", "4:4:4", "4:2:0"]);

    app.set_export_format_selected("TIFF".into());
    assert_eq!(
        segment_rows(&app)[0],
        ["TIFF", "PNG", "JPEG"].map(|l| mark(l, "TIFF")).to_vec()
    );
}

/// Icon-only buttons in the bottom bar and the settings window carry a name.
#[test]
fn zoom_and_settings_close_are_labelled_buttons() {
    let app = app_at(1440, 900);
    app.set_project_loaded(true);
    app.set_settings_open(true);
    for (id, label) in [
        ("FFZoomCluster::zoom-out", "Zoom out"),
        ("FFZoomCluster::zoom-in", "Zoom in"),
        ("FFSettingsModal::close", "Close settings"),
    ] {
        let button = one(&app, id);
        assert_eq!(
            button.accessible_role(),
            Some(AccessibleRole::Button),
            "{id}"
        );
        assert_eq!(button.accessible_label().as_deref(), Some(label), "{id}");
    }
}

/// The toolbar's pipette, undo and redo are addressed by their own ids; undo
/// and redo follow the history, the pipette follows picking.
#[test]
fn editor_undo_redo_are_labelled_buttons() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);
    app.set_mode("editor".into());
    let state = |id: &str| {
        let button = one(&app, &format!("FFEditorToolbar::{id}"));
        assert_eq!(button.accessible_role(), Some(AccessibleRole::Button));
        (
            button.accessible_label().unwrap().to_string(),
            button.accessible_enabled(),
            button.accessible_checked(),
        )
    };

    app.set_can_undo(false);
    app.set_can_redo(true);
    app.set_brush_picking(false);
    assert_eq!(state("undo"), ("Undo".into(), Some(false), Some(false)));
    assert_eq!(state("redo"), ("Redo".into(), Some(true), Some(false)));
    assert_eq!(
        state("pipette"),
        ("Pipette".into(), Some(true), Some(false))
    );

    app.set_can_undo(true);
    app.set_can_redo(false);
    app.set_brush_picking(true);
    assert_eq!(state("undo").1, Some(true));
    assert_eq!(state("redo").1, Some(false));
    assert_eq!(state("pipette").2, Some(true));
}

fn move_pointer(app: &AppWindow, position: slint::LogicalPosition) {
    app.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
}

/// Moves the pointer to the centre of match `index` of `id`.
fn hover(app: &AppWindow, id: &str, index: usize) {
    let handle = all(app, id).remove(index);
    let (pos, size) = (handle.absolute_position(), handle.size());
    move_pointer(
        app,
        slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0),
    );
}

fn assert_button(handle: &ElementHandle, label: &str) {
    assert_eq!(handle.accessible_role(), Some(AccessibleRole::Button));
    assert_eq!(handle.accessible_label().as_deref(), Some(label));
    assert_eq!(handle.accessible_enabled(), Some(true), "{label}");
}

/// The trash and the close glyph exist only under the pointer, and then
/// only on a deletable row or an opened card.
#[test]
fn hover_only_controls_appear_with_id_only_under_the_pointer() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);
    app.set_project_loaded(true);
    let rows = |deletable: bool, count: usize| {
        let row = |i: usize| focusforge_gui::CheckpointEntryView {
            kind: "row".into(),
            algorithm: "Pyramid".into(),
            time_text: "10:00".into(),
            top: 40.0 * i as f32,
            deletable,
            ..Default::default()
        };
        std::rc::Rc::new(slint::VecModel::from(
            (0..count).map(row).collect::<Vec<_>>(),
        ))
    };
    let away = slint::LogicalPosition::new(4.0, 4.0);

    app.set_checkpoint_rows(rows(true, 2).into());
    assert!(all(&app, "FFCheckpointRow::trash").is_empty());
    hover(&app, "FFCheckpointRow::area", 1);
    let trash = all(&app, "FFCheckpointRow::trash");
    assert_eq!(trash.len(), 1);
    assert_button(&trash[0], "Delete checkpoint");
    let row = all(&app, "FFCheckpointRow::area").remove(1);
    assert!(
        trash[0].absolute_position().y >= row.absolute_position().y,
        "the trash belongs to the hovered row, not the first"
    );
    move_pointer(&app, away);
    assert!(all(&app, "FFCheckpointRow::trash").is_empty());

    app.set_checkpoint_rows(rows(false, 1).into());
    hover(&app, "FFCheckpointRow::area", 0);
    assert!(
        all(&app, "FFCheckpointRow::trash").is_empty(),
        "a lone row offers no delete"
    );
    move_pointer(&app, away);

    app.set_projects_overlay_open(true);
    let card = |opened: bool| focusforge_gui::ProjectCardView {
        kind: "open".into(),
        opened,
        project_name: "demo".into(),
        ..Default::default()
    };
    app.set_gallery_content_height(300.0);
    app.set_project_cards(std::rc::Rc::new(slint::VecModel::from(vec![card(true)])).into());
    assert!(all(&app, "FFProjectCard::close").is_empty());
    hover(&app, "FFProjectCard::card-area", 0);
    let close = all(&app, "FFProjectCard::close");
    assert_eq!(close.len(), 1);
    assert_button(&close[0], "Close");
    move_pointer(&app, away);
    assert!(all(&app, "FFProjectCard::close").is_empty());

    app.set_project_cards(std::rc::Rc::new(slint::VecModel::from(vec![card(false)])).into());
    hover(&app, "FFProjectCard::card-area", 0);
    assert!(
        all(&app, "FFProjectCard::close").is_empty(),
        "a card that is not opened has no close glyph"
    );
}

/// The one switch window-wide carrying `name`.
fn switch(app: &AppWindow, name: &str) -> ElementHandle {
    let mut found: Vec<_> = all(app, "FFToggle::area")
        .into_iter()
        .filter(|s| s.accessible_label().as_deref() == Some(name))
        .collect();
    assert_eq!(found.len(), 1, "exactly one switch named {name:?}");
    let found = found.remove(0);
    assert_eq!(found.accessible_role(), Some(AccessibleRole::Button));
    found
}

/// The switches outside the dock panel carry their row name and report
/// their on state.
#[test]
fn switches_carry_their_row_name() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);

    for on in [true, false] {
        app.set_overlay_on(on);
        assert_eq!(switch(&app, "Overlay").accessible_checked(), Some(on));
    }

    app.set_export_open(true);
    for on in [true, false] {
        app.set_export_metadata_on(on);
        assert_eq!(switch(&app, "Metadata").accessible_checked(), Some(on));
    }

    app.set_settings_open(true);
    let mut performance = app.get_performance();
    performance.gpu = true;
    app.set_performance(performance);
    assert_eq!(
        switch(&app, "GPU processing").accessible_checked(),
        Some(true)
    );
}

#[test]
fn appearance_menu_keyboard_selects_and_escape_keeps_the_session_choice() {
    use slint::platform::{Key, WindowEvent};
    use std::cell::RefCell;
    use std::rc::Rc;

    let app = app_at(1440, 900);
    app.set_settings_open(true);
    app.set_settings_section("appearance".into());
    app.set_appearance_presentation("image-list".into());
    let choices = Rc::new(RefCell::new(Vec::<String>::new()));
    app.on_appearance_menu_toggled({
        let weak = app.as_weak();
        move || {
            let app = weak.upgrade().unwrap();
            app.set_appearance_menu_open(!app.get_appearance_menu_open());
        }
    });
    app.on_appearance_presentation_selected({
        let weak = app.as_weak();
        let choices = choices.clone();
        move |choice| {
            choices.borrow_mut().push(choice.to_string());
            let app = weak.upgrade().unwrap();
            app.set_appearance_presentation(choice);
            app.set_appearance_menu_open(false);
        }
    });
    let press = |key: slint::SharedString| {
        app.window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
        app.window()
            .dispatch_event(WindowEvent::KeyReleased { text: key });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    };
    for _ in 0..40 {
        press(Key::Tab.into());
        press(Key::Return.into());
        if app.get_appearance_menu_open() {
            break;
        }
    }
    assert!(
        app.get_appearance_menu_open(),
        "Tab and Enter must open Presentation"
    );
    press(Key::DownArrow.into());
    assert_eq!(
        app.get_appearance_presentation(),
        "image-list",
        "arrow navigation does not commit a choice"
    );
    press(Key::Return.into());
    assert!(!app.get_appearance_menu_open());
    assert_eq!(app.get_appearance_presentation(), "large");
    assert_eq!(*choices.borrow(), ["large"]);

    press(Key::Return.into());
    press(Key::UpArrow.into());
    press(Key::Escape.into());
    assert!(!app.get_appearance_menu_open());
    assert_eq!(
        app.get_appearance_presentation(),
        "large",
        "Escape discards the pending choice"
    );
    assert_eq!(*choices.borrow(), ["large"]);

    press(Key::Return.into());
    press(Key::UpArrow.into());
    press(Key::UpArrow.into());
    press(Key::Return.into());
    assert_eq!(app.get_appearance_presentation(), "list");
    press(Key::Return.into());
    press(Key::DownArrow.into());
    press(Key::Return.into());
    assert_eq!(*choices.borrow(), ["large", "list", "image-list"]);

    app.set_appearance_menu_disabled(true);
    press(Key::Return.into());
    press(Key::DownArrow.into());
    press(Key::Return.into());
    assert!(
        !app.get_appearance_menu_open(),
        "disabled Presentation cannot open from the keyboard"
    );
    assert_eq!(*choices.borrow(), ["large", "list", "image-list"]);
}

#[test]
fn appearance_save_failure_keeps_the_session_choice_and_surfaces_its_error() {
    use focusforge_gui::appearance_settings::SourcePresentation;
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir
        .path()
        .join(format!("appearance-config-{}", "x".repeat(128)));
    fs::create_dir_all(&config_dir).unwrap();
    let settings = config_dir.join("settings.toml");
    let original = "[appearance";
    fs::write(&settings, original).unwrap();
    let controller = Arc::new(Mutex::new(AppController::new()));
    controller
        .lock()
        .unwrap()
        .install_recent(Vec::new(), config_dir);

    let app = app_at(1200, 760);
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_settings_open(true);
    app.set_settings_section("appearance".into());
    app.invoke_appearance_presentation_selected("large".into());

    assert_eq!(
        controller.lock().unwrap().source_presentation(),
        SourcePresentation::Large,
        "a failed settings write does not discard the session presentation"
    );
    assert_eq!(app.get_appearance_presentation(), "large");
    assert!(
        app.get_appearance_save_error().contains("not valid TOML"),
        "the Appearance tab exposes the persistence failure"
    );
    assert!(
        app.get_appearance_save_error().contains('\n'),
        "the native TOML diagnostic retains its detail lines"
    );
    assert_eq!(fs::read_to_string(&settings).unwrap(), original);

    let error = ElementQuery::from_root(&app)
        .match_descendants()
        .match_type_name("FFErrorRow")
        .find_first()
        .expect("the Appearance save failure is visible");
    let source_note = ElementQuery::from_root(&app)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .match_predicate(|element| {
            element
                .accessible_label()
                .is_some_and(|label| label == "Editor source rows stay read-only.")
        })
        .find_first()
        .expect("the source-rows note precedes the save failure");
    let session_note = ElementQuery::from_root(&app)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .match_predicate(|element| {
            element
                .accessible_label()
                .is_some_and(|label| label == "The selected view is active for this session.")
        })
        .find_first()
        .expect("the session note follows the save failure");
    let error_pos = error.absolute_position();
    let error_size = error.size();
    assert!(
        error_size.height > 16.0,
        "the long native path error reserves its wrapped text height"
    );
    assert!(
        source_note.absolute_position().y + source_note.size().height <= error_pos.y,
        "the failure begins after the preceding source-rows note"
    );
    assert!(
        error_pos.y + error_size.height <= session_note.absolute_position().y,
        "the wrapped failure ends before the session note"
    );
}

// --- Notification surface --------------------------------------------------

mod notification {
    use super::*;
    use focusforge_core::params::ParamValue;
    use focusforge_gui::notifications::Kind;
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    type Slot = Rc<RefCell<Option<slint::Timer>>>;

    /// A window over `controller`, wired the way the application runs it,
    /// with the toast already pushed.
    struct Rig {
        app: AppWindow,
        controller: SharedController,
        slot: Slot,
        _installed: focusforge_gui::bridge::Installed,
    }

    fn rig(controller: AppController, width: u32, height: u32) -> Rig {
        let controller: SharedController = Arc::new(Mutex::new(controller));
        let app = app_at(width, height);
        app.window()
            .dispatch_event(WindowEvent::WindowActiveChanged(true));
        let slot: Slot = Rc::new(RefCell::new(None));
        let installed = focusforge_gui::bridge::install(
            &app,
            controller.clone(),
            common::empty_canvas(),
            slot.clone(),
        );
        focusforge_gui::bridge::push_toast(&app, &controller, &slot);
        Rig {
            app,
            controller,
            slot,
            _installed: installed,
        }
    }

    impl Rig {
        fn event(&self, event: WindowEvent) {
            self.app.window().dispatch_event(event);
            slint::platform::update_timers_and_animations();
        }

        fn move_to(&self, x: f32, y: f32) {
            self.event(WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(x, y),
            });
        }

        fn click(&self, x: f32, y: f32) {
            let position = slint::LogicalPosition::new(x, y);
            self.move_to(x, y);
            self.event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            self.event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        }

        /// Lets the center finish sliding in or out.
        fn settle_slide(&self) {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
            slint::platform::update_timers_and_animations();
        }

        fn key(&self, key: Key) {
            let text: slint::SharedString = key.into();
            self.event(WindowEvent::KeyPressed { text: text.clone() });
            self.event(WindowEvent::KeyReleased { text });
        }

        /// The bell's centre in a window `height` tall.
        fn bell(height: f32) -> (f32, f32) {
            (24.0, height - 64.0)
        }

        fn selection(&self) -> (Option<u64>, bool) {
            self.controller.lock().unwrap().notification_selection()
        }

        fn center_open(&self) -> bool {
            self.controller.lock().unwrap().notification_center_open()
        }

        fn ids(&self) -> Vec<u64> {
            self.controller
                .lock()
                .unwrap()
                .notifications()
                .iter()
                .map(|n| n.id)
                .collect()
        }
    }

    /// A project whose run is still going when `refusals` adds are refused,
    /// settled afterwards: one failure record per refusal. The first toast is
    /// held while the run settles, so a slow settle cannot expire it.
    fn controller_with_refusals(name: &str, refusals: usize) -> AppController {
        let mut controller = common::controller_with_frames(name, 4);
        controller.set_align_algorithm("dummy-align");
        assert!(controller.start_fuse());
        for _ in 0..refusals {
            assert!(
                controller
                    .add_frames(&[common::fixture_frame("mantis-64", 4)])
                    .is_err(),
                "a running job refuses an add"
            );
            controller.hold_toast(true);
        }
        common::wait_for_settle(&mut controller, Duration::from_secs(60));
        controller.hold_toast(false);
        controller
    }

    /// Two unreported crash reports under a fresh data directory.
    fn crash_data_dir(name: &str) -> std::path::PathBuf {
        let data = common::tree(name);
        let crashes = data.join("crashes");
        fs::create_dir_all(&crashes).unwrap();
        for stamp in ["20260918T010203Z", "20260918T010204Z"] {
            fs::write(
                crashes.join(format!("{stamp}-focusforge-gui-panic.json")),
                "{\"format\":1,\"payload\":\"boom\"}",
            )
            .unwrap();
        }
        data
    }

    fn count_badge(app: &AppWindow) -> Option<String> {
        ElementQuery::from_root(app)
            .match_id("FFIconButton::count")
            .find_first()
            .map(|_| label(app, "FFIconButton::count"))
    }

    #[test]
    fn icon_bar_places_bell_above_settings() {
        for (width, height) in [(1440u32, 900u32), (1200, 760)] {
            let app = app_at(width, height);
            app.set_show_empty_state(false);
            app.set_projects_overlay_open(true);
            let (w, h) = (width as f32, height as f32);

            assert_rect(&app, "AppWindow::icon-bar", (0.0, 36.0, 48.0, h - 36.0));
            assert_rect(&app, "FFIconBar::bell", (8.0, h - 80.0, 32.0, 32.0));
            assert_rect(&app, "FFIconBar::settings", (8.0, h - 40.0, 32.0, 32.0));
            for id in [
                "AppWindow::toolbar",
                "AppWindow::canvas-ground",
                "AppWindow::bottom-bar",
                "AppWindow::projects-overlay",
            ] {
                assert_eq!(rect(&app, id).0, 48.0, "{id} at {width}x{height}");
            }
            assert_eq!(rect(&app, "AppWindow::bottom-bar").2, w - 48.0);
        }
    }

    #[test]
    fn unread_badge_counts_and_hides_at_zero() {
        let rig = rig(controller_with_refusals("shell_badge", 3), 1440, 900);
        assert_eq!(rig.app.get_notifications_unread(), 3);
        assert_eq!(count_badge(&rig.app).as_deref(), Some("3"));

        let (x, y) = Rig::bell(900.0);
        rig.click(x, y);
        assert!(rig.app.get_notification_center_open());
        assert_eq!(rig.app.get_notifications_unread(), 0);
        assert_eq!(count_badge(&rig.app), None, "no badge at zero");
    }

    #[test]
    fn the_bell_toggles_the_center() {
        let rig = rig(controller_with_refusals("shell_bell_toggle", 1), 1440, 900);
        let (x, y) = Rig::bell(900.0);
        rig.click(x, y);
        assert!(rig.center_open());
        rig.click(x, y);
        assert!(!rig.center_open(), "the bell closes an open center");
        assert!(!rig.app.get_notification_center_open());
        assert_eq!(
            rig.app.get_notifications_unread(),
            0,
            "closing reads nothing new"
        );
    }

    #[test]
    fn toast_activation_selects_its_record_after_later_events() {
        let rig = rig(controller_with_refusals("shell_toast_select", 3), 1440, 900);
        let ids = rig.ids();
        assert!(rig.app.get_toast_open());
        assert_eq!(
            rig.app.get_toast_id() as u64,
            ids[0],
            "the first record is up"
        );
        assert_eq!(rig.app.get_toast_kind(), "failure");

        // The `Open notification` link.
        rig.click(64.0 + 80.0, 92.0 + 44.0);
        assert!(!rig.app.get_toast_open());
        assert!(rig.center_open());
        assert_eq!(rig.selection(), (Some(ids[0]), true));
        let rows = rig.app.get_notification_rows();
        assert_eq!(slint::Model::row_count(&rows), 3);
        for (row, id) in (0..3).zip(ids.iter().rev()) {
            let view = slint::Model::row_data(&rows, row).unwrap();
            assert_eq!(view.id as u64, *id, "newest first");
            assert_eq!(view.selected, *id == ids[0]);
            assert_eq!(view.expanded, *id == ids[0]);
        }
    }

    #[test]
    fn a_closed_toast_lets_the_pointer_reach_the_canvas() {
        let rig = rig(controller_with_refusals("shell_closed_toast", 1), 1440, 900);
        rig.app.set_show_empty_state(false);
        rig.app.set_project_loaded(true);
        let reached = Rc::new(RefCell::new(0usize));
        rig.app.on_canvas_pointer({
            let reached = reached.clone();
            move |_, kind, _, _, _, _, _| {
                if kind == 0 {
                    *reached.borrow_mut() += 1;
                }
            }
        });

        assert!(rig.app.get_toast_open());

        rig.controller
            .lock()
            .unwrap()
            .expire_toast(Instant::now() + Duration::from_secs(7));
        focusforge_gui::bridge::push_toast(&rig.app, &rig.controller, &rig.slot);
        assert!(!rig.app.get_toast_open());
        rig.click(150.0, 130.0);
        assert!(*reached.borrow() > 0, "the closed toast takes no input");
    }

    #[test]
    fn center_keys_select_expand_and_escape_returns_focus_to_the_bell() {
        let rig = rig(controller_with_refusals("shell_center_keys", 2), 1440, 900);
        let ids = rig.ids();
        let (x, y) = Rig::bell(900.0);
        rig.click(x, y);
        rig.settle_slide();
        assert!(rig.center_open());

        rig.key(Key::DownArrow);
        assert_eq!(
            rig.selection(),
            (Some(ids[1]), false),
            "the first row is the newest"
        );
        rig.key(Key::DownArrow);
        assert_eq!(rig.selection(), (Some(ids[0]), false));
        rig.key(Key::Return);
        assert_eq!(rig.selection(), (Some(ids[0]), true));

        rig.key(Key::Escape);
        assert!(!rig.center_open(), "Escape closes the center");
        assert!(
            !rig.controller.lock().unwrap().projects_overlay_open(),
            "Escape never opens Projects"
        );

        rig.key(Key::Return);
        assert!(
            rig.center_open(),
            "focus is on the bell, so Return opens the center again"
        );
    }

    #[test]
    fn a_long_message_scrolls_inside_the_center() {
        let root = common::tree("shell_long_message");
        common::copy_frame(&root, "00.tif", 0);
        for i in 0..40 {
            let dir = root.join(format!("skipped-{i:02}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("readme.txt"), b"not a frame").unwrap();
        }
        let mut controller = AppController::new();
        common::open_and_settle(&mut controller, &[root]);
        let rig = rig(controller, 1200, 760);
        assert_eq!(rig.app.get_toast_title(), "Some images were skipped");
        assert_eq!(rig.app.get_toast_kind(), "info");

        rig.app.invoke_toast_activated(rig.app.get_toast_id());
        let rows = rig.app.get_notification_rows();
        let view = slint::Model::row_data(&rows, 0).unwrap();
        assert!(view.expanded);
        assert_eq!(
            view.message.lines().count(),
            41,
            "the count line and every path"
        );

        let (_, _, _, card) = rect(&rig.app, "FFNotificationRow::card");
        let (_, _, _, viewport) = rect(&rig.app, "FFNotificationCenter::scroller");
        assert!(
            card > viewport,
            "the expanded card ({card}) outgrows the list viewport ({viewport})"
        );
    }

    #[test]
    fn toast_hover_pauses_its_timer() {
        let rig = rig(controller_with_refusals("shell_toast_hover", 1), 1440, 900);
        let original = rig.controller.lock().unwrap().toast_deadline().unwrap();
        let running = || rig.slot.borrow().as_ref().is_some_and(|t| t.running());
        assert!(running(), "one timer is armed for the deadline");

        rig.move_to(150.0, 130.0);
        assert_eq!(rig.controller.lock().unwrap().toast_deadline(), None);
        assert!(!running(), "the same timer is stopped, not replaced");

        rig.move_to(900.0, 500.0);
        let resumed = rig.controller.lock().unwrap().toast_deadline().unwrap();
        assert!(resumed > original, "the time spent paused is added back");
        assert!(running());
    }

    #[test]
    fn a_brightness_row_names_the_frame_file() {
        let mut controller = common::controller_with_frames("shell_brightness_row", 4);
        controller.set_align_algorithm("dummy-align");
        controller.set_param_value("prepare", "equalize", ParamValue::Bool(true));
        controller.set_param_value("prepare", "equalize-target", ParamValue::Float(0.5));
        controller.set_param_value("prepare", "equalize-max-gain", ParamValue::Float(1.25));
        controller.toggle_frame_excluded(0);
        assert!(controller.start_fuse());
        common::wait_for_settle(&mut controller, Duration::from_secs(60));
        let project = controller.notifications()[0]
            .project_title
            .clone()
            .expect("a brightness record names its project");
        assert!(
            controller
                .notifications()
                .iter()
                .all(|n| n.kind == Kind::Warning)
        );

        let rig = rig(controller, 1440, 900);
        assert_eq!(rig.app.get_toast_title(), "Brightness target limited");
        assert_eq!(rig.app.get_toast_kind(), "warning");
        let (x, y) = Rig::bell(900.0);
        rig.click(x, y);
        let rows = rig.app.get_notification_rows();
        let contexts: Vec<String> = (0..slint::Model::row_count(&rows))
            .map(|i| {
                slint::Model::row_data(&rows, i)
                    .unwrap()
                    .context
                    .to_string()
            })
            .collect();
        assert_eq!(
            contexts.len(),
            3,
            "the excluded frame is not part of the run"
        );
        for (context, file) in contexts.iter().rev().zip(["01.tif", "02.tif", "03.tif"]) {
            assert!(
                context.starts_with(&format!("{project} · {file}")),
                "{context:?} names {file}"
            );
        }
    }

    #[test]
    fn a_crash_toast_carries_the_crash_kind() {
        let mut controller = AppController::new();
        controller.set_diagnostics_dirs(Some(crash_data_dir("shell_crash_toast")), None);
        controller.scan_crash_notice();
        let rig = rig(controller, 1440, 900);
        assert!(rig.app.get_toast_open());
        assert_eq!(rig.app.get_toast_kind(), "crash");
        assert_eq!(rig.app.get_toast_title(), "Previous session stopped");
    }

    /// The crash row's Diagnostics button opens the settings modal on its
    /// Diagnostics section, not on the section the modal last showed.
    #[test]
    fn crash_diagnostics_opens_settings_on_the_diagnostics_section() {
        let mut controller = AppController::new();
        controller.set_diagnostics_dirs(Some(crash_data_dir("shell_crash_diagnostics")), None);
        controller.scan_crash_notice();
        let rig = rig(controller, 1440, 900);
        assert_eq!(rig.app.get_settings_section().as_str(), "performance");

        rig.app
            .invoke_notification_diagnostics_requested(rig.ids()[0] as i32);

        assert!(rig.app.get_settings_open());
        assert_eq!(rig.app.get_settings_section().as_str(), "diagnostics");
    }

    /// The icon bar and the center compose primitives and set no colour: the
    /// unread bell glyph and the muted close glyph are `FFIconButton`'s own.
    #[test]
    fn icon_bar_and_center_set_no_colour_themselves() {
        for file in ["shell/icon_bar.slint", "shell/notification_center.slint"] {
            let source = ui_source(file);
            for needle in [
                "color:",
                "background",
                "border-",
                "font-",
                "Theme.text-",
                "Theme.accent-",
                "Theme.surface-",
            ] {
                assert!(!source.contains(needle), "{file} sets {needle:?}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn crash_row_shows_the_failed_acknowledgement_error() {
        use std::os::unix::fs::PermissionsExt;

        let data = crash_data_dir("shell_crash_row");
        let crashes = data.join("crashes");
        let writable = fs::metadata(&crashes).unwrap().permissions();
        let mut controller = AppController::new();
        controller.set_diagnostics_dirs(Some(data), None);
        controller.scan_crash_notice();
        let rig = rig(controller, 1440, 900);
        let id = rig.ids()[0];
        let row = || slint::Model::row_data(&rig.app.get_notification_rows(), 0).unwrap();
        assert_eq!(row().crash_state, "unacknowledged");

        fs::set_permissions(&crashes, fs::Permissions::from_mode(0o555)).unwrap();
        rig.app.invoke_notification_acknowledge_requested(id as i32);
        let writable_now = fs::write(crashes.join("probe"), "x").is_ok();
        fs::set_permissions(&crashes, writable).unwrap();
        if writable_now {
            return; // privileged user: a read-only directory still accepts writes
        }
        let stored = rig.controller.lock().unwrap().notifications()[0]
            .crash
            .as_ref()
            .and_then(|c| c.error.clone())
            .expect("the refused write is stored");
        assert_eq!(row().crash_state, "failed");
        assert_eq!(row().crash_error, stored.as_str());

        rig.app.invoke_notification_acknowledge_requested(id as i32);
        assert_eq!(row().crash_state, "acknowledged");
        assert_eq!(row().crash_error, "");
    }
}

// --- Escape in the Settings modal -------------------------------------------

mod settings_escape {
    use super::*;
    use focusforge_gui::SettingsSection;
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    struct Rig {
        app: AppWindow,
        controller: SharedController,
        _installed: focusforge_gui::bridge::Installed,
    }

    fn rig() -> Rig {
        let controller: SharedController = Arc::new(Mutex::new(AppController::new()));
        let app = app_at(1440, 900);
        app.window()
            .dispatch_event(WindowEvent::WindowActiveChanged(true));
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

        fn open_settings(&self, section: SettingsSection) {
            self.app.invoke_settings_open_requested();
            self.app
                .invoke_settings_section_selected(section.slug().into());
            slint::platform::update_timers_and_animations();
            assert!(self.app.get_settings_open());
        }

        fn escape(&self) {
            let text: slint::SharedString = Key::Escape.into();
            self.event(WindowEvent::KeyPressed { text: text.clone() });
            self.event(WindowEvent::KeyReleased { text });
        }

        fn click_element(&self, id: &str) {
            let (x, y, w, h) = rect(&self.app, id);
            let position = slint::LogicalPosition::new(x + w / 2.0, y + h / 2.0);
            self.event(WindowEvent::PointerMoved { position });
            self.event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            self.event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        }

        fn overlay_open(&self) -> bool {
            self.controller.lock().unwrap().projects_overlay_open()
        }
    }

    #[test]
    fn escape_closes_settings_from_each_section() {
        let rig = rig();
        for section in SettingsSection::ALL {
            rig.open_settings(section);
            rig.escape();
            assert!(
                !rig.app.get_settings_open(),
                "Escape closes Settings from {}",
                section.slug()
            );
        }
    }

    #[test]
    fn escape_closes_settings_after_a_text_field_held_focus() {
        let rig = rig();
        rig.open_settings(SettingsSection::Diagnostics);
        rig.click_element("FFDiagnosticsPanel::dest-field");
        rig.app
            .invoke_settings_section_selected(SettingsSection::About.slug().into());
        slint::platform::update_timers_and_animations();
        rig.escape();
        assert!(
            !rig.app.get_settings_open(),
            "Escape closes Settings after the Save to field held focus and About was selected"
        );
    }

    #[test]
    fn escape_closes_settings_and_leaves_the_overlay_to_the_next_escape() {
        let rig = rig();
        rig.controller.lock().unwrap().toggle_projects_overlay();
        rig.open_settings(SettingsSection::Performance);
        rig.escape();
        assert!(
            !rig.app.get_settings_open(),
            "the first Escape closes Settings"
        );
        assert!(
            rig.overlay_open(),
            "the projects overlay underneath stays open"
        );
        rig.escape();
        assert!(!rig.overlay_open(), "the second Escape closes the overlay");
    }
}

mod dialog_close {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    type Counts = Vec<(&'static str, Rc<Cell<u32>>)>;

    /// Counts every argument-free callback the modal dialogs raise.
    macro_rules! count_callbacks {
        ($app:expr, $($on:ident => $name:literal),* $(,)?) => {{
            let mut counts: Counts = Vec::new();
            $({
                let count = Rc::new(Cell::new(0u32));
                let hit = count.clone();
                $app.$on(move || hit.set(hit.get() + 1));
                counts.push(($name, count));
            })*
            counts
        }};
    }

    fn count_dialog_callbacks(app: &AppWindow) -> Counts {
        count_callbacks!(app,
            on_question_open_as_one => "question-open-as-one",
            on_question_open_as_many => "question-open-as-many",
            on_question_cancelled => "question-cancelled",
            on_refusal_pick_another_folder => "refusal-pick-another-folder",
            on_mismatch_open_anyway => "mismatch-open-anyway",
            on_mismatch_locate_frames => "mismatch-locate-frames",
            on_mismatch_cancelled => "mismatch-cancelled",
            on_sources_missing_locate_frames => "sources-missing-locate-frames",
            on_sources_missing_closed => "sources-missing-closed",
            on_export_confirmed => "export-confirmed",
            on_export_cancelled => "export-cancelled",
            on_frame_warning_cancelled => "frame-warning-cancelled",
            on_frame_warning_secondary_pressed => "frame-warning-secondary-pressed",
            on_frame_warning_confirmed => "frame-warning-confirmed",
            on_divergence_discarded => "divergence-discarded",
            on_divergence_kept => "divergence-kept",
            on_close_guard_cancelled => "close-guard-cancelled",
            on_close_guard_secondary_pressed => "close-guard-secondary-pressed",
            on_close_guard_confirmed => "close-guard-confirmed",
            on_checkpoint_delete_cancelled => "checkpoint-delete-cancelled",
            on_checkpoint_delete_confirmed => "checkpoint-delete-confirmed",
        )
    }

    fn fired(counts: &Counts) -> Vec<(&'static str, u32)> {
        counts
            .iter()
            .map(|(name, count)| (*name, count.get()))
            .filter(|(_, count)| *count > 0)
            .collect()
    }

    /// A dialog that carries the close control: how to open it, the
    /// `AppWindow` callback its own cancel raises, and its panel size.
    struct Dialog {
        type_name: &'static str,
        label: &'static str,
        cancel: &'static str,
        panel: (f32, f32),
        open: fn(&AppWindow),
    }

    const INCLUDED: [Dialog; 8] = [
        Dialog {
            type_name: "FFFrameSetWarning",
            label: "frame-set warning",
            cancel: "frame-warning-cancelled",
            panel: (480.0, 300.0),
            open: |app| app.set_frame_warning_open(true),
        },
        Dialog {
            type_name: "FFCloseGuard",
            label: "close guard",
            cancel: "close-guard-cancelled",
            panel: (480.0, 300.0),
            open: |app| app.set_close_guard_open(true),
        },
        Dialog {
            type_name: "FFCheckpointDelete",
            label: "checkpoint delete",
            cancel: "checkpoint-delete-cancelled",
            panel: (560.0, 316.0),
            open: |app| app.set_checkpoint_delete_open(true),
        },
        Dialog {
            type_name: "FFFingerprintMismatch",
            label: "fingerprint mismatch",
            cancel: "mismatch-cancelled",
            panel: (600.0, 362.0),
            open: |app| app.set_mismatch_open(true),
        },
        Dialog {
            type_name: "FFSourcesMissing",
            label: "sources missing",
            cancel: "sources-missing-closed",
            panel: (540.0, 330.0),
            open: |app| app.set_sources_missing_open(true),
        },
        Dialog {
            type_name: "FFExportDialog",
            label: "export, ready",
            cancel: "export-cancelled",
            panel: (560.0, 365.0),
            open: |app| {
                app.set_export_stage("ready".into());
                app.set_export_open(true);
            },
        },
        Dialog {
            type_name: "FFExportDialog",
            label: "export, refused",
            cancel: "export-cancelled",
            panel: (560.0, 365.0),
            open: |app| {
                app.set_export_stage("refused".into());
                app.set_export_open(true);
            },
        },
        Dialog {
            type_name: "FFGroupingQuestion",
            label: "grouping question",
            cancel: "question-cancelled",
            panel: (480.0, 200.0),
            open: |app| app.set_question_open(true),
        },
    ];

    fn dialog_root(app: &AppWindow, type_name: &str) -> ElementHandle {
        ElementQuery::from_root(app)
            .match_descendants()
            .match_type_name(type_name)
            .find_first()
            .unwrap_or_else(|| panic!("no {type_name} in the window"))
    }

    fn close_buttons(root: &ElementHandle) -> Vec<ElementHandle> {
        root.query_descendants()
            .match_type_name("FFIconButton")
            .match_predicate(|e| e.accessible_label().as_deref() == Some("Close dialog"))
            .find_all()
    }

    fn close_button(app: &AppWindow, dialog: &Dialog) -> ElementHandle {
        let mut found = close_buttons(&dialog_root(app, dialog.type_name));
        assert_eq!(
            found.len(),
            1,
            "{}: exactly one Close dialog control",
            dialog.label
        );
        found.remove(0)
    }

    fn panel_of(root: &ElementHandle) -> ElementHandle {
        root.query_descendants()
            .match_id("FFModalPanel::panel")
            .find_first()
            .expect("the dialog's own panel")
    }

    fn click(handle: &ElementHandle) {
        handle.mock_single_click(slint::platform::PointerEventButton::Left);
    }

    #[test]
    fn dialog_close_button_acts_as_cancel() {
        for dialog in &INCLUDED {
            let app = app_at(1440, 900);
            let counts = count_dialog_callbacks(&app);
            (dialog.open)(&app);
            let button = close_button(&app, dialog);
            assert_eq!(button.accessible_role(), Some(AccessibleRole::Button));
            if matches!(dialog.label, "sources missing" | "export, refused") {
                let footer = dialog_root(&app, dialog.type_name)
                    .query_descendants()
                    .match_type_name("FFButton")
                    .match_predicate(|e| e.accessible_label().as_deref() == Some("Close"))
                    .find_all();
                assert_eq!(footer.len(), 1, "{}: one footer Close", dialog.label);
                assert_ne!(
                    footer[0].absolute_position(),
                    button.absolute_position(),
                    "{}: the footer Close is a different element",
                    dialog.label
                );
            }
            click(&button);
            assert_eq!(
                fired(&counts),
                [(dialog.cancel, 1)],
                "{}: the Close dialog control raises its own cancel once and nothing else",
                dialog.label
            );
        }
    }

    #[test]
    fn dialog_close_button_acts_as_cancel_leaves_excluded_dialogs_bare() {
        let app = app_at(1440, 900);
        app.set_refusal_open(true);
        app.set_divergence_open(true);
        for type_name in ["FFRefusalDialog", "FFDivergenceWarning"] {
            let root = dialog_root(&app, type_name);
            assert!(
                close_buttons(&root).is_empty(),
                "{type_name} shows no Close dialog control"
            );
        }

        app.set_settings_open(true);
        let settings = dialog_root(&app, "FFSettingsModal");
        let own: Vec<_> = settings
            .query_descendants()
            .match_type_name("FFIconButton")
            .match_predicate(|e| {
                e.accessible_label()
                    .is_some_and(|label| label.starts_with("Close"))
            })
            .find_all();
        assert_eq!(own.len(), 1, "the settings modal keeps exactly one");
        assert_eq!(own[0].accessible_label().as_deref(), Some("Close settings"));
    }

    #[test]
    fn export_close_button_cancels_a_running_write() {
        let app = app_at(1440, 900);
        let mut fused = common::controller_with_frames("export_close_writing", 2);
        fused.set_align_algorithm("dummy-align");
        assert!(fused.start_fuse());
        while common::wait_for_settle(&mut fused, std::time::Duration::from_secs(60)).is_some() {}
        assert_eq!(fused.checkpoint_count(), Some(1), "the fixture fuses");
        let controller: SharedController = std::sync::Arc::new(std::sync::Mutex::new(fused));
        controller.lock().unwrap().checkpoint_export_requested();
        assert!(
            controller.lock().unwrap().export_view().is_some(),
            "the export modal is open"
        );
        let _installed = focusforge_gui::bridge::install(
            &app,
            controller.clone(),
            common::empty_canvas(),
            Rc::new(std::cell::RefCell::new(None)),
        );
        let export = &INCLUDED[5];

        (export.open)(&app);
        assert_eq!(close_button(&app, export).computed_opacity(), 1.0);

        app.set_export_stage("writing".into());
        let button = close_button(&app, export);
        assert_eq!(button.computed_opacity(), 0.5, "dimmed like the footer");
        click(&button);
        assert!(
            controller.lock().unwrap().export_view().is_none(),
            "the click reaches AppController::cancel_export"
        );
    }

    #[test]
    fn dialog_close_button_sits_in_the_panel_corner() {
        const INSET: f32 = 16.0;
        const GLYPH: f32 = 16.0;
        for (width, height) in [(1440, 900), (1200, 760)] {
            for dialog in &INCLUDED {
                let app = app_at(width, height);
                (dialog.open)(&app);
                let root = dialog_root(&app, dialog.type_name);
                let panel = panel_of(&root);
                let (px, py) = (panel.absolute_position().x, panel.absolute_position().y);
                let (pw, ph) = (panel.size().width, panel.size().height);
                let at = format!("{} at {width}x{height}", dialog.label);

                assert_eq!((pw, ph), dialog.panel, "{at}: the panel keeps its size");

                let button = close_button(&app, dialog);
                let (bx, by) = (button.absolute_position().x, button.absolute_position().y);
                let (bw, bh) = (button.size().width, button.size().height);
                let pad = (bw - GLYPH) / 2.0;
                assert!(
                    bx >= px && by >= py && bx + bw <= px + pw && by + bh <= py + ph,
                    "{at}: the control lies inside the panel"
                );
                assert!(
                    (bx + bw - pad - (px + pw - INSET)).abs() < EPS,
                    "{at}: the glyph's right edge is on the content inset"
                );
                assert!(
                    (by + bh / 2.0 - (py + INSET + 10.0)).abs() < EPS,
                    "{at}: the control is centred on the first row at the top inset"
                );

                if let Some(footer) = root
                    .query_descendants()
                    .match_type_name("FFDialogFooter")
                    .find_first()
                {
                    let footer_bottom = footer.absolute_position().y + footer.size().height;
                    assert!(
                        (py + ph - footer_bottom - INSET).abs() < EPS,
                        "{at}: footer stays `space-4` above the panel bottom"
                    );
                }
            }
        }
    }

    #[test]
    fn the_longest_dialog_title_stops_before_the_close_control() {
        for (width, height) in [(1440, 900), (1200, 760)] {
            let app = app_at(width, height);
            let mismatch = &INCLUDED[3];
            (mismatch.open)(&app);
            let title = dialog_root(&app, mismatch.type_name)
                .query_descendants()
                .match_predicate(|e| {
                    e.accessible_label().as_deref() == Some("The source frames are not the same")
                })
                .find_first()
                .expect("the title text");
            let button = close_button(&app, mismatch);
            assert!(
                title.absolute_position().x + title.size().width
                    <= button.absolute_position().x + EPS,
                "{width}x{height}: the title ends left of the control"
            );
        }
    }
}
