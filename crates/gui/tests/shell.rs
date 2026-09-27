// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless geometry: `i_slint_backend_testing::init_no_event_loop()`
//! simulates a windowing system without one and renders no pixels, exactly
//! as `tests/theme.rs` already relies on. Every rect is read back and
//! measured, never assumed.

mod common;

use std::fs;

use focusforge_gui::AppWindow;
use i_slint_backend_testing::ElementQuery;
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
    assert_rect(&app, "AppWindow::toolbar", (0.0, 36.0, 1180.0, 40.0));
    assert_rect(&app, "AppWindow::canvas-ground", (0.0, 76.0, 1180.0, 780.0));
    assert_rect(
        &app,
        "FFCanvas::pane-left",
        (16.0, 76.0 + 16.0, 566.0, 748.0),
    );
    assert_rect(
        &app,
        "FFCanvas::pane-right",
        (1180.0 - 16.0 - 566.0, 76.0 + 16.0, 566.0, 748.0),
    );
    assert_rect(&app, "AppWindow::dock", (1180.0, 36.0, 260.0, 820.0));
    assert_rect(&app, "AppWindow::bottom-bar", (0.0, 856.0, 1440.0, 44.0));
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
        (16.0, 76.0 + 16.0, 446.0, 608.0),
    );
    assert_rect(
        &app,
        "FFCanvas::pane-right",
        (940.0 - 16.0 - 446.0, 76.0 + 16.0, 446.0, 608.0),
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
    let controller = focusforge_gui::AppController::new();
    assert!(controller.checkpoints().is_empty());
    assert!(!controller.export_enabled());

    // The canvas legitimately touches storage (a project's TIFFs),
    // `input_config.rs` is a config file, and `settings_panel.rs` is the one
    // module allowed to name the schema/registry crate; `brush.rs` builds
    // the stored stroke's path in core's own point type. Excluded by name:
    // this test's own subject is the checkpoint panel, not the whole crate.
    let canvas_files = [
        "brush.rs",
        "canvas.rs",
        "gpu_canvas.rs",
        "entry.rs",
        "input_config.rs",
        "settings_panel.rs",
        "job.rs",
        "images.rs",
        "overlay.rs",
        "recent.rs",
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
        for needle in ["focusforge_core", "fs::read", "File::open"] {
            assert!(
                !contents.contains(needle),
                "{}: contains {needle:?} -- the checkpoint panel must touch no storage",
                path.display()
            );
        }
    }
}

/// The zoom cluster sits at the bottom bar's right end, 216 x 32,
/// mirroring the gear's `Theme.space-3` inset at the left. Zoom-out,
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

/// The toast paints at the pane gutter under the tab strip and toolbar, at
/// its fixed size, regardless of window width; with no notice pending, no
/// element matches its id at all -- the conditional `if` never instantiates
/// it, rather than leaving it merely invisible.
#[test]
fn crash_notice_placement_and_absence() {
    let app = app_at(1440, 900);
    assert!(
        ElementQuery::from_root(&app)
            .match_id("AppWindow::crash-notice")
            .find_first()
            .is_none(),
        "no element must match the toast's id while no notice is pending"
    );

    app.set_crash_notice_visible(true);
    app.set_crash_notice_title("1 crash found".into());
    assert_rect(&app, "AppWindow::crash-notice", (16.0, 92.0, 300.0, 84.0));

    let narrow = app_at(1200, 760);
    narrow.set_crash_notice_visible(true);
    narrow.set_crash_notice_title("1 crash found".into());
    let (x, y, _, _) = rect(&narrow, "AppWindow::crash-notice");
    assert!(
        (x - 16.0).abs() < EPS && (y - 92.0).abs() < EPS,
        "the toast's top-left must stay at the pane gutter regardless of window width: got ({x}, {y})"
    );
}

/// The gear carries its own persistently-boxed background -- the "icon
/// button" catalogue entry's default state already carries a visible box --
/// rather than a bare `TouchArea`. It does **not** share the zoom
/// cluster's own `FFZoomIconButton`, whose grouped, hover-only convention
/// is a different at-rest state. The gear is a single standalone control,
/// so it keeps its own individual box, sized to the same 32 x 32 baseline
/// the zoom buttons use for one shared bar-wide rhythm.
#[test]
fn gear_has_its_own_persistent_box() {
    let app = app_at(1440, 900);

    let (_, _, w, h) = rect(&app, "FFBottomBar::gear");
    assert_eq!(
        (w, h),
        (32.0, 32.0),
        "the gear's hit box must match the zoom cluster's own 32 x 32 baseline \
         (Theme.icon-md + 2 * Theme.space-2)"
    );

    let bottom_bar_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("bottom_bar.slint")).unwrap();
    let code = common::strip_line_comments(&bottom_bar_slint);
    assert!(
        code.contains("gear := FFGearButton"),
        "bottom_bar.slint: the gear must use its own FFGearButton component, not the zoom \
         cluster's FFZoomIconButton -- the two must not share chrome"
    );
    assert!(
        !code.contains("gear := FFZoomIconButton"),
        "bottom_bar.slint: the gear must not reuse the zoom cluster's own icon-button component"
    );

    // `FFGearButton`'s default (non-hover) background must not be
    // `transparent`: a button with chrome only on hover reads as no button
    // at all until the mouse finds it.
    let default_background = code
        .split("background: area.has-hover ? Theme.surface-hover : ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .expect("FFGearButton must bind its background to a hover/default pair");
    assert_ne!(
        default_background.trim(),
        "transparent",
        "FFGearButton's default state must carry a visible background"
    );
}

/// The four-square Projects-overlay button sits at the strip's far left and
/// the tabs start after it -- `ShellGeometry.projects-button-slot` wired to
/// `tabs-region`'s `x`, not merely declared and left unused: button x
/// 8..38, first tab x 46.
#[test]
fn tab_strip_reserves_the_projects_button_slot() {
    let app = app_at(1440, 900);

    let (x, _, _, _) = rect(&app, "FFTabStrip::tabs-region");
    assert_eq!(
        x, 46.0,
        "tabs-region must start at ShellGeometry.projects-button-slot (46px), not flush at x=0"
    );
    // The projects button is a 30x30 strip box at x 8, on the tabs' top edge.
    assert_rect(&app, "FFTabStrip::projects-button", (8.0, 6.0, 30.0, 30.0));
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

/// The empty state: the wordmark above one raised 520-wide card, centred
/// on the canvas column, holding the title, the copy,
/// the single `Open folder…` button (168 x 34) and the drop caption, in that
/// order and all inside the card's 32 px inset.
#[test]
fn empty_state_is_a_card_under_the_wordmark() {
    let app = app_at(1440, 900);

    let card = rect(&app, "FFEmptyState::empty-card");
    assert_eq!((card.0, card.2), (330.0, 520.0), "card x/width: {card:?}");
    let wordmark = rect(&app, "FFEmptyState::wordmark");
    assert_eq!(
        (wordmark.0, wordmark.2),
        (446.0, 288.0),
        "wordmark: {wordmark:?}"
    );
    assert!(
        wordmark.1 + wordmark.3 < card.1,
        "the wordmark sits above the card"
    );

    assert_eq!(label(&app, "FFEmptyState::empty-title"), "No project open");
    assert_eq!(
        label(&app, "FFEmptyState::empty-body"),
        "Open a folder of frames. Each detected stack becomes its own project, in its own tab."
    );
    assert_eq!(label(&app, "FFEmptyState::open-folder"), "Open folder…");
    assert_eq!(
        label(&app, "FFEmptyState::drop-caption"),
        "Dropping a folder works on X11, macOS and Windows — not on native Wayland. \
         Open folder… works everywhere."
    );

    let button = rect(&app, "FFEmptyState::open-folder");
    assert_eq!(
        (button.2, button.3),
        (168.0, 34.0),
        "button size: {button:?}"
    );

    let mut previous_bottom = card.1;
    for id in [
        "FFEmptyState::empty-title",
        "FFEmptyState::empty-body",
        "FFEmptyState::open-folder",
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

/// The dock without a project: the checkpoint panel's empty line, a
/// disabled `Export image`, and `No project open.` in
/// place of the dock tab row, which only a loaded project shows.
#[test]
fn dock_without_a_project_says_so() {
    let app = app_at(1440, 900);

    assert_eq!(
        label(&app, "FFCheckpointPanel::checkpoint-note"),
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

/// The bottom bar's status line without a project: a rule one grid step
/// right of the gear, then `No project open` (rule x 52, text x 68).
#[test]
fn bottom_bar_status_reads_no_project_open() {
    let app = app_at(1440, 900);

    assert_eq!(label(&app, "FFBottomBar::status-text"), "No project open");
    assert_rect(&app, "FFBottomBar::status-rule", (52.0, 868.0, 1.0, 20.0));
    let (x, _, _, _) = rect(&app, "FFBottomBar::status-text");
    assert_eq!(x, 68.0);

    app.set_project_loaded(true);
    assert_ne!(
        label(&app, "FFBottomBar::status-text"),
        "No project open",
        "a loaded project does not read \"No project open\""
    );
}

/// The toolbar's transport buttons: 116 x 26, 10 px apart, the first at
/// `Theme.space-3` (12px) and the second at 138px -- the running state's
/// `Pause`/`Cancel` pair.
#[test]
fn transport() {
    let app = app_at(1440, 900);
    app.set_show_empty_state(false);
    app.set_toolbar_run_state("running".into());

    assert_rect(&app, "FFToolbar::pause-button", (12.0, 43.0, 116.0, 26.0));
    assert_rect(&app, "FFToolbar::cancel-button", (138.0, 43.0, 116.0, 26.0));
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
        (250.0, 876.0, 798.0, 4.0),
    );
    let (eta_x, eta_y, _, _) = rect(&app, "FFBottomBar::eta-text");
    assert_eq!((eta_x, eta_y), (1064.0, 856.0));

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
        (250.0, 736.0, 558.0, 4.0),
    );
    let (eta_x, eta_y, _, _) = rect(&app_min, "FFBottomBar::eta-text");
    assert_eq!((eta_x, eta_y), (824.0, 716.0));
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
