// SPDX-License-Identifier: GPL-3.0-or-later

//! The viewport model (`crate::viewport`, pure arithmetic, no window) and
//! its dispatch through `AppController::wheel`/`pointer`/`pointer_moved`
//! (also no window).

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use focusforge_gui::AppController;
use focusforge_gui::input_map::{Button, Modifiers, PointerKind};
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

/// A tab that settles after the pane geometry was pushed gets no resize of
/// its own; the draw path's fit is what gives it a viewport.
#[test]
fn a_tab_opened_after_the_geometry_push_fits_on_draw() {
    let mut controller = AppController::new();
    controller.set_pane_geometry(pane(566.0, 748.0), pane(566.0, 748.0));
    common::open_and_settle(&mut controller, &[mantis_512_dir()]);
    assert_eq!(controller.zoom_readout(), "");

    controller.fit_if_unfitted();

    assert!(!controller.zoom_readout().is_empty());
    assert!(controller.frame_draw().is_some());
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
    let draw_left = controller.frame_draw().expect("a project is open");

    controller.fit();

    // Cursor over the right pane (x in [598, 1164]), at the same pane-local
    // offset as the left-pane cursor above (both panes are the same size,
    // offset only by `x`) -- must resolve through the same one viewport.
    controller.wheel(0.0, 60.0, 682.0, 100.0, NONE);
    let draw_right = controller.frame_draw().expect("a project is open");

    assert_eq!(
        draw_left, draw_right,
        "one viewport must respond identically regardless of which pane the cursor is over"
    );

    // Structural: `pixelated` is one shared property read by both `FFPane`s
    // in `ui/shell/canvas.slint`; `image-*` is the right pane's own
    // (composite/Z-Map) extent and `frame-image-*` the left pane's own,
    // each driven by that one viewport but naming its own pane's `ImageSize`.
    let canvas_slint =
        fs::read_to_string(common::ui_dir().join("shell").join("canvas.slint")).unwrap();
    let needle = "pixelated: root.pixelated";
    let count = canvas_slint.matches(needle).count();
    assert_eq!(
        count, 2,
        "pixelated must be forwarded identically to both panes, found {count} occurrence(s)"
    );
    for prop in ["image-x", "image-y", "image-w", "image-h"] {
        let right_needle = format!("{prop}: root.{prop}");
        let left_needle = format!("{prop}: root.frame-{prop}");
        assert_eq!(
            canvas_slint.matches(right_needle.as_str()).count(),
            1,
            "the right pane must bind its own {prop} exactly once"
        );
        assert_eq!(
            canvas_slint.matches(left_needle.as_str()).count(),
            1,
            "the left pane must bind its own frame-{prop} exactly once"
        );
    }
}

/// One viewport drives two extents at once: the frame pane's own size and a
/// composite cropped smaller on both axes, by an odd number of pixels each.
/// A zoom taken over the frame pane keeps that source point registered
/// under the cursor there, and both panes' draws report the same scale.
#[test]
fn two_pane_sizes_share_one_viewport() {
    let frame_size = image(1024, 1024);
    let plane_size = image(1024 - 7, 1024 - 5);
    let p = pane(512.0, 512.0);
    let anchor = (233.0, 177.0);

    let mut vp = fit(frame_size, p);
    let src_before = source_point(&vp, anchor);
    vp.zoom_steps(2, anchor, frame_size, p);
    let src_after = source_point(&vp, anchor);
    let drift_x = ((src_after.0 - src_before.0) * vp.scale()).abs();
    let drift_y = ((src_after.1 - src_before.1) * vp.scale()).abs();
    assert!(
        drift_x < 1.0 && drift_y < 1.0,
        "the anchor point must stay under the cursor in the pane it was taken in: \
         drift {drift_x}, {drift_y} screen px"
    );

    let frame_draw = vp
        .draw(frame_size, p)
        .expect("the frame pane must have something to draw");
    let plane_draw = vp
        .draw(plane_size, p)
        .expect("the plane pane must have something to draw");

    let scale_of = |d: &focusforge_gui::viewport::PaneDraw| d.dst.2 as f64 / d.src.2 as f64;
    assert!(
        (scale_of(&frame_draw) - scale_of(&plane_draw)).abs() < 1e-6,
        "both panes must draw at the same one scale: {} vs {}",
        scale_of(&frame_draw),
        scale_of(&plane_draw)
    );
    assert_eq!(
        frame_draw.pixelated, plane_draw.pixelated,
        "both panes must cross the pixelated threshold together"
    );
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

/// Every ladder stop, lowest first.
fn every_stop(img: ImageSize, p: PaneRect) -> Vec<focusforge_gui::viewport::Viewport> {
    let centre = (p.width / 2.0, p.height / 2.0);
    let mut vp = fit(img, p);
    vp.zoom_steps(-100, centre, img, p);
    let mut out = vec![vp];
    for _ in 1..stops(img, p).len() {
        vp.zoom_steps(1, centre, img, p);
        out.push(vp);
    }
    out
}

#[test]
fn step_follows_the_scale_on_every_stop() {
    for (img, p) in [
        (image(8192, 5464), pane(566.0, 748.0)),
        (image(1024, 1024), pane(512.0, 512.0)),
    ] {
        for vp in every_stop(img, p) {
            let draw = vp.draw(img, p).expect("every stop draws");
            let scale = vp.scale();
            let step = draw.step.get();
            let (ew, eh) = draw.extent();
            if scale >= 1.0 {
                assert_eq!(step, 1, "at {} the step must be 1", vp.readout());
                continue;
            }
            assert_eq!(
                step,
                ((1.0 / scale) + 1e-6).floor().max(1.0) as u32,
                "at {}",
                vp.readout()
            );
            // At least one sample per pane pixel, under twice the pane per axis.
            assert!(ew as f32 >= draw.dst.2 - 1e-3, "at {}", vp.readout());
            assert!(eh as f32 >= draw.dst.3 - 1e-3, "at {}", vp.readout());
            assert!((ew as f32) < 2.0 * draw.dst.2 + 1.0, "at {}", vp.readout());
            assert!((eh as f32) < 2.0 * draw.dst.3 + 1.0, "at {}", vp.readout());
        }
    }
}

#[test]
fn step_lands_on_whole_numbers_at_the_half_and_quarter_stops() {
    let img = image(1024, 1024);
    let p = pane(512.0, 512.0);
    let vp = fit(img, p);
    assert_eq!(vp.readout(), "50 %");
    assert_eq!(vp.draw(img, p).unwrap().step.get(), 2);
    let mut vp = vp;
    vp.zoom_steps(-1, (256.0, 256.0), img, p);
    assert_eq!(vp.readout(), "25 %");
    assert_eq!(vp.draw(img, p).unwrap().step.get(), 4);
}

#[test]
fn step_texels_overshoot_the_rect_by_under_one_step() {
    for (img, p) in [
        (image(8192, 5464), pane(566.0, 748.0)),
        (image(1017, 1019), pane(512.0, 512.0)),
    ] {
        for vp in every_stop(img, p) {
            let draw = vp.draw(img, p).unwrap();
            let step = draw.step.get();
            let (ew, eh) = draw.extent();
            let (_, _, sw, sh) = draw.src;
            assert!(
                ew * step >= sw && ew * step - sw < step,
                "at {}",
                vp.readout()
            );
            assert!(
                eh * step >= sh && eh * step - sh < step,
                "at {}",
                vp.readout()
            );
            // Under one logical pixel on screen.
            assert!(((ew * step - sw) as f64 * vp.scale()) < 1.0);
            assert!(((eh * step - sh) as f64 * vp.scale()) < 1.0);
        }
    }
}

#[test]
fn step_leaves_the_rect_as_it_is_at_and_above_one_to_one() {
    let img = image(8192, 5464);
    let p = pane(566.0, 748.0);
    for vp in every_stop(img, p) {
        let pct = vp.readout();
        if pct != "100 %" && pct != "200 %" {
            continue;
        }
        let draw = vp.draw(img, p).unwrap();
        assert_eq!(draw.step.get(), 1);
        assert_eq!(draw.extent(), (draw.src.2, draw.src.3));
        let (ox, oy) = vp.origin();
        let s = vp.scale();
        let x1 = (ox + p.width as f64 / s).min(8192.0).ceil();
        let y1 = (oy + p.height as f64 / s).min(5464.0).ceil();
        let (sx0, sy0) = (ox.max(0.0).floor(), oy.max(0.0).floor());
        assert_eq!(
            draw.src,
            (sx0 as u32, sy0 as u32, (x1 - sx0) as u32, (y1 - sy0) as u32),
            "at {pct}"
        );
        assert_eq!(
            draw.dst,
            (
                ((sx0 - ox) * s) as f32,
                ((sy0 - oy) * s) as f32,
                ((x1 - sx0) * s) as f32,
                ((y1 - sy0) * s) as f32
            ),
            "at {pct}"
        );
    }
}

/// The storage binding (128 MiB) and the old 1-D dispatch ceiling
/// (65,535 workgroups of 64) that an undecimated fit draw of 8192 x 5464
/// exceeds: 268,566,528 bytes, 44,761,088 texels.
#[test]
fn step_keeps_the_fit_upload_under_the_device_limits() {
    let img = image(8192, 5464);
    for p in [pane(566.0, 377.0), pane(1920.0, 1080.0)] {
        let draw = fit(img, p).draw(img, p).unwrap();
        let (ew, eh) = draw.extent();
        let texels = ew as u64 * eh as u64;
        assert!(
            texels * 3 * 2 < 128 * 1024 * 1024,
            "{p:?}: {ew}x{eh} packs to {} bytes",
            texels * 6
        );
        assert!(texels < 4_194_240, "{p:?}: {texels} texels");
    }
}

#[test]
fn drag_moves_end_at_the_summed_pan() {
    let mut stepped = opened_controller();
    let mut whole = opened_controller();
    for c in [&mut stepped, &mut whole] {
        c.zoom_in();
        c.zoom_in();
        c.pointer(PointerKind::Down, Button::Middle, 300.0, 400.0, NONE);
    }
    let before = whole.frame_draw().unwrap();
    let (mut x, mut y) = (300.0f32, 400.0f32);
    for i in 0..60 {
        x += if i % 3 == 0 { 2.0 } else { -0.5 };
        y += if i % 2 == 0 { 1.0 } else { -0.5 };
        stepped.pointer_moved(x, y);
    }
    whole.pointer_moved(x, y);
    let a = stepped.frame_draw().unwrap();
    let b = whole.frame_draw().unwrap();
    assert_eq!(a.src, b.src);
    for (p, q) in [(a.dst.0, b.dst.0), (a.dst.1, b.dst.1)] {
        assert!((p - q).abs() < 1e-3, "{:?} vs {:?}", a.dst, b.dst);
    }
    assert_ne!(before.dst, b.dst, "the drag must have moved the view");
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
