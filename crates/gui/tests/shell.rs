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
    // The crate legitimately has storage-touching code
    // for the canvas -- reading a loaded project's composite/Z-Map TIFFs is

    // The canvas legitimately touches storage: reading a loaded project's
    // composite/Z-Map TIFFs is exactly what the canvas is for, and
    // `input_config.rs` is a config file, not the checkpoint panel reaching
    // for storage. Excluded by name: this test's own subject is the
    // checkpoint panel, not the whole crate.
    let canvas_files = [
        "canvas.rs",
        "gpu_canvas.rs",
        "project_stub.rs",
        "input_config.rs",
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
