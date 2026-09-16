// SPDX-License-Identifier: GPL-3.0-or-later

//! The canvas: the display path's no-readback and no-CPU-tonemap
//! invariants (source scans), the display format's refuse-by-name
//! contract, the texture-reuse rule, and both panes' chrome geometry.

mod common;

use std::fs;

use focusforge_gui::AppWindow;
use focusforge_gui::canvas::{DISPLAY_FORMAT, TextureSpec, needs_realloc, validate_display_format};
use i_slint_backend_testing::ElementQuery;
use slint::ComponentHandle;

#[test]
fn display_path_has_no_readback() {
    let forbidden = [
        "map_async",
        "copy_texture_to_buffer",
        "download_buffer",
        "download_f32",
        "MAP_READ",
    ];
    for path in common::find_rust_files(&common::src_dir()) {
        let contents = fs::read_to_string(&path).unwrap();
        let stripped = common::strip_line_comments(&contents);
        for needle in forbidden {
            assert!(
                !stripped.contains(needle),
                "{}: contains {needle:?} -- the display path must perform no CPU round-trip",
                path.display()
            );
        }
    }
}

#[test]
fn no_cpu_tonemap_in_the_gui_crate() {
    let forbidden = ["tonemap_cpu", "reduce16to8"];
    for path in common::find_rust_files(&common::src_dir()) {
        let contents = fs::read_to_string(&path).unwrap();
        let stripped = common::strip_line_comments(&contents);
        for needle in forbidden {
            assert!(
                !stripped.contains(needle),
                "{}: contains {needle:?} -- the composite's samples must never be \
                 converted to 8 bit in crates/gui",
                path.display()
            );
        }
    }
}

/// The "one viewport over two panes" invariant, enforced by name in
/// `canvas::check_pane_dimensions`, a pure function.
#[test]
fn check_pane_dimensions_refuses_a_composite_zmap_mismatch_by_name() {
    use focusforge_gui::canvas::{CanvasError, check_pane_dimensions};

    match check_pane_dimensions((8, 8), (4, 4)) {
        Err(CanvasError::PaneDimensionsDiffer { composite, zmap }) => {
            assert_eq!(composite, (8, 8));
            assert_eq!(zmap, (4, 4));
        }
        other => panic!("expected PaneDimensionsDiffer, got {other:?}"),
    }
    assert!(check_pane_dimensions((8, 8), (8, 8)).is_ok());
}

#[test]
fn display_format_is_rgba8unorm() {
    assert_eq!(DISPLAY_FORMAT, "Rgba8Unorm");
    assert!(validate_display_format(DISPLAY_FORMAT).is_ok());
    assert!(
        validate_display_format("Rgba8UnormSrgb").is_err(),
        "any other format name must be refused by name here"
    );
    assert!(validate_display_format("Bgra8Unorm").is_err());
}

#[test]
fn texture_is_reused_across_frames() {
    let a = TextureSpec {
        width: 512,
        height: 512,
    };
    let same = TextureSpec {
        width: 512,
        height: 512,
    };
    let wider = TextureSpec {
        width: 600,
        height: 512,
    };
    let taller = TextureSpec {
        width: 512,
        height: 600,
    };

    assert!(
        !needs_realloc(Some(a), same),
        "an identical size must not trigger a re-allocation"
    );
    assert!(
        needs_realloc(Some(a), wider),
        "a wider wanted size must trigger a re-allocation"
    );
    assert!(
        needs_realloc(Some(a), taller),
        "a taller wanted size must trigger a re-allocation"
    );
    assert!(
        needs_realloc(None, a),
        "no current texture must trigger a (first) allocation"
    );
}

const GEOMETRY_EPS: f32 = 0.5;

#[test]
fn pane_chrome_geometry() {
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
    // The label and undock control are hidden until a project is open --
    // see `pane_chrome_hidden_without_a_project` for the default (`false`) case.
    app.set_project_loaded(true);

    for pane in ["pane-left", "pane-right"] {
        let pane_handle = ElementQuery::from_root(&app)
            .match_id(format!("FFCanvas::{pane}"))
            .find_first()
            .unwrap_or_else(|| panic!("no element matched id \"FFCanvas::{pane}\""));
        let pane_pos = pane_handle.absolute_position();

        // 120 x 22 label at +10/+10 from the pane's top-left.
        let label = ElementQuery::from_root(&app)
            .match_id("FFPane::pane-label")
            .find_all()
            .into_iter()
            .find(|el| {
                let p = el.absolute_position();
                (p.x - (pane_pos.x + 10.0)).abs() < GEOMETRY_EPS
                    && (p.y - (pane_pos.y + 10.0)).abs() < GEOMETRY_EPS
            })
            .unwrap_or_else(|| panic!("{pane}: no pane-label found at +10/+10"));
        let label_size = label.size();
        assert!(
            (label_size.width - 120.0).abs() < GEOMETRY_EPS
                && (label_size.height - 22.0).abs() < GEOMETRY_EPS,
            "{pane}: label size {label_size:?}, expected 120x22"
        );

        // 26 x 22 undock control at +10/+10 from the pane's top-right.
        let undock = ElementQuery::from_root(&app)
            .match_id("FFPane::pane-undock")
            .find_all()
            .into_iter()
            .find(|el| {
                let p = el.absolute_position();
                (p.x - (pane_pos.x + pane_handle.size().width - 10.0 - 26.0)).abs() < GEOMETRY_EPS
                    && (p.y - (pane_pos.y + 10.0)).abs() < GEOMETRY_EPS
            })
            .unwrap_or_else(|| panic!("{pane}: no pane-undock found at +10/+10 from the right"));
        let undock_size = undock.size();
        assert!(
            (undock_size.width - 26.0).abs() < GEOMETRY_EPS
                && (undock_size.height - 22.0).abs() < GEOMETRY_EPS,
            "{pane}: undock size {undock_size:?}, expected 26x22"
        );
    }
}

/// Before a project is open there is nothing for a pane label to name or
/// for an undock control to undock, so both are hidden rather than drawn
/// over an empty pane.
/// `project-loaded` defaults to `false`, matching every window that has not
/// yet run `bridge::repaint` -- see `src/bridge.rs::repaint`.
#[test]
fn pane_chrome_hidden_without_a_project() {
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

    for id in ["FFPane::pane-label", "FFPane::pane-undock"] {
        assert!(
            ElementQuery::from_root(&app)
                .match_id(id)
                .find_all()
                .is_empty(),
            "{id}: must be hidden while no project is open"
        );
    }
}
