// SPDX-License-Identifier: GPL-3.0-or-later

//! The viewport model (`crate::viewport`, pure arithmetic, no window) and
//! its dispatch through `AppController::wheel`/`pointer`/`pointer_moved`
//! (also no window).

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use focusforge_gui::AppController;
use focusforge_gui::input_map::Modifiers;
use focusforge_gui::viewport::{ImageSize, PaneRect, fit, stops};

const NONE: Modifiers = Modifiers::NONE;

fn image(width: u32, height: u32) -> ImageSize {
    ImageSize { width, height }
}

fn pane(width: f32, height: f32) -> PaneRect {
    PaneRect {
        x: 0.0,
        y: 0.0,
        width,
        height,
    }
}

fn source_point(vp: &focusforge_gui::viewport::Viewport, anchor: (f32, f32)) -> (f64, f64) {
    let (ox, oy) = vp.origin();
    (
        ox + anchor.0 as f64 / vp.scale(),
        oy + anchor.1 as f64 / vp.scale(),
    )
}

fn mantis_512_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("mantis-512")
}

/// A controller with a fixture project loaded and a base-window pane
/// geometry pushed, as `src/main.rs` does after `--open` -- rebuilt on
/// `open_paths`: the fixture's own `frames/` layout (`frame_paths`'
/// redirect) makes it one `Unsaved` group, 512 x 512 image size, read from
/// the frame headers.
fn opened_controller() -> AppController {
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[mantis_512_dir()]);
    controller.set_pane_geometry(
        PaneRect {
            x: 16.0,
            y: 92.0,
            width: 566.0,
            height: 748.0,
        },
        PaneRect {
            x: 598.0,
            y: 92.0,
            width: 566.0,
            height: 748.0,
        },
    );
    controller
}

// ---------------------------------------------------------------------
// The model: `crate::viewport`, pure arithmetic.
// ---------------------------------------------------------------------

#[test]
fn zoom_anchors_at_the_cursor() {
    let img = image(1024, 1024);
    let p = pane(512.0, 512.0);
    let anchors = [
        (0.0, 0.0),
        (512.0, 512.0),
        (0.0, 512.0),
        (512.0, 0.0),
        (256.0, 256.0),
        (128.0, 384.0),
    ];

    for anchor in anchors {
        let mut vp = fit(img, p);
        let src_before = source_point(&vp, anchor);
        vp.zoom_steps(1, anchor, img, p);
        let src_after = source_point(&vp, anchor);
        let drift_x = ((src_after.0 - src_before.0) * vp.scale()).abs();
        let drift_y = ((src_after.1 - src_before.1) * vp.scale()).abs();
        assert!(
            drift_x < 1.0 && drift_y < 1.0,
            "anchor {anchor:?}: drift {drift_x}, {drift_y} screen px"
        );
    }

    // A zoom in followed by the same number of steps out, at the same
    // anchor, returns to the original viewport.
    let anchor = (128.0, 384.0);
    let mut vp = fit(img, p);
    let start = (vp.scale(), vp.origin());
    vp.zoom_steps(2, anchor, img, p);
    vp.zoom_steps(-2, anchor, img, p);
    assert!((vp.scale() - start.0).abs() < 1e-9, "scale must round-trip");
    assert!(
        (vp.origin().0 - start.1.0).abs() < 1e-6 && (vp.origin().1 - start.1.1).abs() < 1e-6,
        "origin must round-trip: got {:?}, expected {:?}",
        vp.origin(),
        start.1
    );
}

#[test]
fn zoom_is_bounded_at_both_ends() {
    let img = image(8192, 5464);
    let p = pane(566.0, 748.0);
    let ladder = stops(img, p);
    let anchor = (283.0, 374.0);

    let mut low = fit(img, p);
    low.zoom_steps(-40, anchor, img, p);
    assert!((low.scale() - ladder[0]).abs() < 1e-9);
    assert!(low.at_min(img, p));

    let mut high = fit(img, p);
    high.zoom_steps(40, anchor, img, p);
    assert!((high.scale() - *ladder.last().unwrap()).abs() < 1e-9);
    assert!(high.at_max(img, p));
}

#[test]
fn zoom_cluster_steps_the_ladder() {
    let mut controller = opened_controller();
    let start = controller.zoom_readout();

    controller.zoom_in();
    let after_in = controller.zoom_readout();
    assert_ne!(start, after_in, "zoom_in must change the readout");

    controller.zoom_out();
    assert_eq!(
        controller.zoom_readout(),
        start,
        "zoom_out after zoom_in must return to the start"
    );

    controller.fit();
    assert_eq!(controller.zoom_readout(), start);
}

#[test]
fn wheel_steps_one_stop_per_detent() {
    let mut controller = opened_controller();
    let start = controller.zoom_readout();

    controller.wheel(0.0, 60.0, 100.0, 100.0, NONE);
    let after_detent = controller.zoom_readout();
    assert_ne!(start, after_detent, "one detent must step the ladder");

    controller.fit();
    controller.wheel(0.0, 20.0, 100.0, 100.0, NONE);
    controller.wheel(0.0, 20.0, 100.0, 100.0, NONE);
    controller.wheel(0.0, 20.0, 100.0, 100.0, NONE);
    assert_eq!(
        controller.zoom_readout(),
        after_detent,
        "three 20px deltas must accumulate into exactly one stop"
    );

    controller.fit();
    controller.wheel(0.0, 30.0, 100.0, 100.0, NONE);
    assert_eq!(
        controller.zoom_readout(),
        start,
        "a lone 30px delta must change nothing"
    );
}

#[test]
fn reaching_a_bound_is_visible() {
    let mut controller = opened_controller();
    assert!(!controller.at_min_zoom());

    for _ in 0..40 {
        controller.wheel(0.0, -60.0, 283.0, 374.0, NONE);
    }
    assert!(controller.at_min_zoom());
    let bound = controller.zoom_readout();

    controller.wheel(0.0, -60.0, 283.0, 374.0, NONE);
    assert_eq!(
        controller.zoom_readout(),
        bound,
        "wheeling further at the bound must not change the readout"
    );
    assert!(controller.at_min_zoom());

    for _ in 0..80 {
        controller.wheel(0.0, 60.0, 283.0, 374.0, NONE);
    }
    assert!(controller.at_max_zoom());
    let max_bound = controller.zoom_readout();
    controller.wheel(0.0, 60.0, 283.0, 374.0, NONE);
    assert_eq!(controller.zoom_readout(), max_bound);
    assert!(controller.at_max_zoom());
}

#[test]
fn fit_shows_the_whole_image() {
    for (w, h) in [(512u32, 512u32), (64u32, 64u32)] {
        let img = image(w, h);
        let p = pane(566.0, 748.0);
        let vp = fit(img, p);
        let draw = vp
            .draw(img, p)
            .expect("fit must always have something to draw");
        assert_eq!(
            draw.src,
            (0, 0, w, h),
            "fit must show the whole image at {w}x{h}"
        );

        let cx = draw.dst.0 + draw.dst.2 / 2.0;
        let cy = draw.dst.1 + draw.dst.3 / 2.0;
        assert!(
            (cx - p.width / 2.0).abs() < 1.0,
            "must be centred horizontally"
        );
        assert!(
            (cy - p.height / 2.0).abs() < 1.0,
            "must be centred vertically"
        );
    }

    // A fit that exceeds 500% is kept rather than capped -- the cap applies
    // to the wheel ladder only.
    let img = image(64, 64);
    let p = pane(566.0, 748.0);
    let vp = fit(img, p);
    assert!(
        vp.scale() > 5.0,
        "fit on a small image must exceed the wheel's 500% cap, got {}",
        vp.scale()
    );
}

#[test]
fn pan_is_clamped() {
    let img = image(1024, 1024);
    let p = pane(512.0, 512.0);

    // Below fit: pinned centred, does not move at all.
    let mut vp = fit(img, p);
    vp.zoom_steps(-1, (256.0, 256.0), img, p);
    let before = vp.origin();
    vp.pan_by(1000.0, 1000.0, img, p);
    assert_eq!(
        vp.origin(),
        before,
        "below fit the image must not move at all"
    );

    // Above fit: dragging hard toward a corner stops with the pane still
    // inside the image.
    let mut vp = fit(img, p);
    vp.zoom_steps(3, (256.0, 256.0), img, p);
    let scale = vp.scale();
    let visible_w = p.width as f64 / scale;
    let visible_h = p.height as f64 / scale;

    vp.pan_by(-100_000.0, -100_000.0, img, p);
    let (ox, oy) = vp.origin();
    assert!(
        ox >= -1e-6 && oy >= -1e-6,
        "origin must stay non-negative: {ox}, {oy}"
    );
    assert!(ox + visible_w <= img.width as f64 + 1e-6);
    assert!(oy + visible_h <= img.height as f64 + 1e-6);

    vp.pan_by(100_000.0, 100_000.0, img, p);
    let (ox2, oy2) = vp.origin();
    assert!(ox2 + visible_w <= img.width as f64 + 1e-6);
    assert!(oy2 + visible_h <= img.height as f64 + 1e-6);
    assert!(ox2 >= -1e-6 && oy2 >= -1e-6);
}

#[test]
fn panes_share_one_viewport() {
    let mut controller = opened_controller();

    // Cursor over the left pane (x in [16, 582]).
    controller.wheel(0.0, 60.0, 100.0, 100.0, NONE);
    let draw_left = controller.pane_draw().expect("a project is open");

    controller.fit();

    // Cursor over the right pane (x in [598, 1164]), at the same pane-local
    // offset as the left-pane cursor above (both panes are the same size,
    // offset only by `x`) -- must resolve through the same one viewport.
    controller.wheel(0.0, 60.0, 682.0, 100.0, NONE);
    let draw_right = controller.pane_draw().expect("a project is open");

    assert_eq!(
        draw_left, draw_right,
        "one viewport must respond identically regardless of which pane the cursor is over"
    );

    // Structural: both `FFPane`s in `ui/shell/canvas.slint` read one set of
    // placement properties (`root.image-*`/`root.pixelated`), not two.
    let canvas_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("canvas.slint")).unwrap();
    for prop in ["image-x", "image-y", "image-w", "image-h", "pixelated"] {
        let needle = format!("{prop}: root.{prop}");
        let count = canvas_slint.matches(needle.as_str()).count();
        assert_eq!(
            count, 2,
            "{prop} must be forwarded identically to both panes, found {count} occurrence(s)"
        );
    }
}

#[test]
fn gestures_come_from_the_input_map() {
    let canvas_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("canvas.slint")).unwrap();
    assert_eq!(canvas_slint.matches("scroll-event(").count(), 1);
    assert_eq!(canvas_slint.matches("pointer-event(").count(), 1);
    assert_eq!(canvas_slint.matches("moved =>").count(), 1);

    // `ff_slider.slint`'s drag, `ff_scrollbar.slint`'s drag and
    // `settings_panel.slint`'s scroll are each a one-line forward to
    // `AppController`, the same as the canvas's own gesture table -- this
    // single-dispatch-point rule governs the canvas's rebindable zoom/pan/
    // drag gestures specifically, not every interactive widget in the crate.
    let raw_gesture_callback_exceptions = [
        "canvas.slint",
        "ff_slider.slint",
        "ff_scrollbar.slint",
        "settings_panel.slint",
        "images_panel.slint",
        "ff_image_row.slint",
        "gallery.slint",
        "ff_manifest_list.slint",
    ];
    for path in common::find_slint_files(&common::ui_dir()) {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| raw_gesture_callback_exceptions.contains(&name))
        {
            continue;
        }
        let contents = fs::read_to_string(&path).unwrap();
        for needle in ["scroll-event(", "pointer-event(", "moved =>"] {
            assert!(
                !contents.contains(needle),
                "{}: declares {needle}",
                path.display()
            );
        }
    }

    for path in common::find_rust_files(&common::src_dir()) {
        if path.file_name().and_then(|n| n.to_str()) == Some("controller.rs") {
            continue;
        }
        let contents = fs::read_to_string(&path).unwrap();
        let code = common::strip_line_comments(&contents);
        assert!(
            !code.contains(".action("),
            "{}: maps a gesture to an Action outside the dispatch point",
            path.display()
        );
    }
}

#[test]
fn readout_tracks_the_viewport() {
    let img = image(1024, 1024);
    let p = pane(512.0, 512.0);

    let vp = fit(img, p);
    assert_eq!(vp.readout(), "50 %");

    let mut vp = vp;
    vp.zoom_steps(-1, (256.0, 256.0), img, p);
    assert_eq!(vp.readout(), "25 %");

    vp.zoom_steps(2, (256.0, 256.0), img, p);
    assert_eq!(vp.readout(), "67 %");

    let big = image(8192, 5464);
    let bp = pane(566.0, 748.0);
    let mut max = fit(big, bp);
    for _ in 0..40 {
        max.zoom_steps(1, (283.0, 374.0), big, bp);
    }
    assert_eq!(max.readout(), "500 %");
}

#[test]
fn resize_is_sticky_at_fit() {
    let img = image(8192, 5464);
    let base = pane(566.0, 748.0);
    let min_pane = pane(446.0, 608.0);

    // At fit: re-fits to the new pane.
    let mut vp = fit(img, base);
    vp.resize(img, min_pane);
    let refit = fit(img, min_pane);
    assert!((vp.scale() - refit.scale()).abs() < 1e-9);
    assert_eq!(vp.origin(), refit.origin());

    // Not at fit: scale kept, offset re-clamped.
    let mut vp2 = fit(img, base);
    vp2.zoom_steps(2, (283.0, 374.0), img, base);
    let scale_before = vp2.scale();
    vp2.resize(img, min_pane);
    assert!(
        (vp2.scale() - scale_before).abs() < 1e-9,
        "a deliberate zoom must survive a resize"
    );

    // A scale that falls below the new pane's minimum is raised to it.
    let mut vp3 = fit(img, base);
    vp3.zoom_steps(-1, (283.0, 374.0), img, base);
    let bigger = pane(1132.0, 1496.0);
    vp3.resize(img, bigger);
    let new_min = stops(img, bigger)[0];
    assert!(
        (vp3.scale() - new_min).abs() < 1e-9,
        "a scale below the new minimum must be raised to it"
    );
}

#[test]
fn rendering_switches_at_one_to_one() {
    let img = image(1024, 1024);
    let p = pane(512.0, 512.0);

    let vp = fit(img, p); // 50%, below 100
    assert!(
        !vp.draw(img, p).unwrap().pixelated,
        "below 100% must be smooth"
    );

    let mut vp = vp;
    vp.zoom_steps(2, (256.0, 256.0), img, p); // 50 -> 67 -> 100
    assert_eq!(vp.readout(), "100 %");
    assert!(
        vp.draw(img, p).unwrap().pixelated,
        "at or above 100% must be pixelated"
    );
}
