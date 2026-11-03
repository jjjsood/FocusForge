// SPDX-License-Identifier: GPL-3.0-or-later

//! The canvas: the display path's no-readback and no-CPU-tonemap
//! invariants (source scans), the display format's refuse-by-name
//! contract, the texture-reuse rule, and both panes' chrome geometry.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use focusforge_core::io::TiffHeader;
use focusforge_gui::AppWindow;
use focusforge_gui::canvas::{
    CanvasFeed, DISPLAY_FORMAT, FrameSource, RepaintGate, TextureSpec, draw_rect_fits,
    needs_realloc, truncate_pane_label, validate_display_format,
};
use focusforge_gui::job;
use focusforge_gui::viewport::PaneRect;
use focusforge_gui::{AppController, OpenOutcome};
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
    // These files reduce a stored composite or original source on a worker,
    // outside the interactive display path this scan protects.
    const EXEMPT: &[&str] = &["overlay.rs", "source_previews.rs"];
    let forbidden = ["tonemap_cpu", "reduce16to8"];
    for path in common::find_rust_files(&common::src_dir()) {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| EXEMPT.contains(&name))
        {
            continue;
        }
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

#[test]
fn repaint_gate_collapses_requests_until_taken() {
    let gate = RepaintGate::default();
    assert!(gate.request(), "the first request schedules a repaint");
    for _ in 0..100 {
        assert!(!gate.request(), "later requests ride the scheduled one");
    }
    assert!(gate.take(), "the scheduled repaint finds a pending request");
    assert!(!gate.take(), "taking twice finds nothing pending");
    assert!(gate.request(), "the gate re-arms after a take");
}

#[test]
fn undo_and_redo_refresh_window_without_another_pointer_event() {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    i_slint_backend_testing::init_no_event_loop();
    let app = AppWindow::new().unwrap();
    let controller = Arc::new(Mutex::new(AppController::new()));
    let canvas = Rc::new(RefCell::new(focusforge_gui::bridge::Canvas::default()));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        canvas,
        Rc::new(RefCell::new(None)),
    );
    let expected = controller.lock().unwrap().zoom_readout();

    for action in [
        Box::new(|| app.invoke_undo_requested()) as Box<dyn Fn()>,
        Box::new(|| app.invoke_redo_requested()),
        Box::new(|| app.invoke_canvas_key_pressed("z".into(), true, false, false)),
        Box::new(|| app.invoke_canvas_key_pressed("z".into(), true, true, false)),
    ] {
        app.set_zoom_readout("stale".into());
        action();
        assert_eq!(app.get_zoom_readout(), expected);
    }
}

/// A rect far larger than a small composite: an 8192x5464 viewport rect
/// against a 501x508 composite is refused, an exact-fit and an interior
/// rect are not.
#[test]
fn a_read_past_the_composite_draws_nothing_instead_of_panicking() {
    assert!(
        !draw_rect_fits((501, 508), (0, 0, 8192, 5464)),
        "a rect far larger than the frame must be refused"
    );
    assert!(
        draw_rect_fits((501, 508), (0, 0, 501, 508)),
        "a rect exactly matching the frame must be accepted"
    );
    assert!(
        draw_rect_fits((501, 508), (10, 10, 100, 100)),
        "a rect strictly inside the frame must be accepted"
    );
    assert!(
        !draw_rect_fits((501, 508), (450, 0, 100, 508)),
        "a rect whose x + width overflows the frame's width must be refused"
    );
    assert!(
        !draw_rect_fits((501, 508), (0, 450, 501, 100)),
        "a rect whose y + height overflows the frame's height must be refused"
    );
    assert!(
        !draw_rect_fits((501, 508), (0, 0, 0, 508)),
        "a zero-width rect must be refused"
    );
}

#[test]
fn renderer_refuses_invalid_visible_draw_before_guarding() {
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use focusforge_gui::viewport::PaneDraw;
    use std::num::NonZeroU32;

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite, zmap) = golden_plane_files();
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer.set_planes(&composite, Some(&zmap)).unwrap();
    let draw = PaneDraw {
        src: (0, 0, 16, 16),
        dst: (0.0, 0.0, 16.0, 16.0),
        crop: (0.0, 0.0, 16.0, 16.0),
        pixelated: true,
        step: NonZeroU32::MIN,
    };

    for src in [(0, 0, 0, 16), (0, 0, 16, 0), (600, 0, 10, 10)] {
        assert!(
            renderer
                .request_planes(&PaneDraw { src, ..draw }, None, None)
                .is_none(),
            "invalid visible draw {src:?} must be refused before guard expansion"
        );
    }
}

#[test]
fn cached_redraw_refuses_invalid_visible_draw_without_losing_coverage() {
    use focusforge_gui::canvas::Pane;
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use focusforge_gui::viewport::PaneDraw;
    use std::num::NonZeroU32;

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite, zmap) = golden_plane_files();
    let frame = common::fixture_frame("mantis-64", 0);
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer.set_planes(&composite, Some(&zmap)).unwrap();
    renderer
        .set_frame(&frame, None, job::host_budget())
        .expect("fixture frame opens");
    let plane = PaneDraw {
        src: (128, 128, 16, 16),
        dst: (0.0, 0.0, 16.0, 16.0),
        crop: (0.0, 0.0, 16.0, 16.0),
        pixelated: true,
        step: NonZeroU32::MIN,
    };
    let frame = PaneDraw {
        src: (8, 8, 16, 16),
        ..plane
    };
    renderer
        .redraw_planes(&plane, None, None)
        .expect("valid plane coverage publishes");
    renderer
        .redraw_frame(&frame)
        .expect("valid frame coverage publishes");
    let plane_placement = renderer
        .placement(Pane::Composite, &plane)
        .expect("plane coverage is present")
        .1;
    let frame_placement = renderer
        .placement(Pane::Frame, &frame)
        .expect("frame coverage is present")
        .1;

    for src in [(128, 128, 0, 16), (512, 128, 1, 1)] {
        assert!(
            renderer
                .redraw_planes(&PaneDraw { src, ..plane }, None, None)
                .is_none(),
            "invalid plane draw {src:?} must never use cached coverage"
        );
    }
    for src in [(8, 8, 0, 16), (64, 8, 1, 1)] {
        assert!(
            renderer.redraw_frame(&PaneDraw { src, ..frame }).is_none(),
            "invalid frame draw {src:?} must never use cached coverage"
        );
    }
    assert_eq!(
        renderer.placement(Pane::Composite, &plane).unwrap().1,
        plane_placement,
        "invalid plane redraw leaves valid coverage intact"
    );
    assert_eq!(
        renderer.placement(Pane::Frame, &frame).unwrap().1,
        frame_placement,
        "invalid frame redraw leaves valid coverage intact"
    );
}

#[test]
fn pending_pan_keeps_retained_coverage_inside_current_crop_until_latest_publish() {
    use focusforge_gui::canvas::Pane;
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use focusforge_gui::viewport::PaneDraw;
    use std::num::NonZeroU32;

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite, zmap) = golden_plane_files();
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer.set_planes(&composite, Some(&zmap)).unwrap();
    let first = PaneDraw {
        src: (128, 128, 128, 128),
        dst: (0.0, 0.0, 128.0, 128.0),
        crop: (0.0, 0.0, 128.0, 128.0),
        pixelated: true,
        step: NonZeroU32::MIN,
    };
    renderer
        .redraw_planes(&first, None, None)
        .expect("the initial coverage publishes");

    // This leaves 20 source pixels inside the retained texture and asks the
    // preparer for the rest. Keep that job unpublished to model its worker
    // read being in flight while Slint renders the last texture.
    let current = PaneDraw {
        src: (300, 128, 128, 128),
        ..first
    };
    let pending = renderer
        .request_planes(&current, None, None)
        .expect("a pan beyond retained coverage requires a latest job");
    let (_, retained) = renderer
        .placement(Pane::Composite, &current)
        .expect("the previous texture remains available while the job is pending");
    let crop = current.crop;
    assert!(
        retained.0 < crop.0 && retained.0 + retained.2 > crop.0,
        "old coverage overlaps only the left edge of the current crop"
    );
    assert!(
        retained.0 + retained.2 < crop.0 + crop.2,
        "without the current-crop clip, old pixels would leave a stale edge and black surplus"
    );

    let (texture_width, texture_height) = pending.draw.extent();
    let mut samples = vec![
        0;
        texture_width as usize
            * texture_height as usize
            * pending.reader.samples_per_pixel() as usize
    ];
    let (x, y, width, height) = pending.draw.src;
    pending
        .reader
        .read_step_into(
            focusforge_core::tile::Rect {
                x,
                y,
                width,
                height,
            },
            pending.draw.step,
            &mut samples,
        )
        .expect("the latest job reads");
    renderer
        .publish(&pending, &samples)
        .expect("the latest job publishes");
    let (_, latest) = renderer
        .placement(Pane::Composite, &current)
        .expect("the latest coverage replaces retained coverage");
    assert_eq!(latest, current.place(pending.draw.src));
    assert!(
        latest.0 <= crop.0
            && latest.1 <= crop.1
            && latest.0 + latest.2 >= crop.0 + crop.2
            && latest.1 + latest.3 >= crop.1 + crop.3,
        "the current crop clips the guarded latest texture to the exact visible draw"
    );
}

// ---------------------------------------------------------------------
// The renderer cell follows the active tab: routing tests over two real
// projects of different sizes (`fixtures/mantis-64`, `fixtures/mantis-512`),
// driven through `AppController` with no window and no GPU, asserting on
// `canvas_feed`'s answer.
// ---------------------------------------------------------------------

/// Opens `fixtures/<set>`'s first `count` frames, copied under a fresh
/// directory (never the committed fixture itself, since a fuse seals a
/// `.ffp` beside its frames), as a new tab -- the newly opened tab becomes
/// active. Returns the folder the tab was opened from.
fn open_fixture_tab(
    controller: &mut AppController,
    name: &str,
    set: &str,
    count: usize,
) -> PathBuf {
    let root = common::tree(&format!("{name}_{set}_frames"));
    for i in 0..count {
        std::fs::copy(
            common::fixture_frame(set, i),
            root.join(format!("{i:02}.tif")),
        )
        .expect("fixture frame must copy");
    }
    assert!(matches!(
        common::open_and_settle(controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    // Canvas feeding is under test, not alignment quality; an algorithm that
    // always converges keeps every run clean.
    controller.set_align_algorithm("dummy-align");
    root
}

/// A controller with two sealed tabs open -- tab 0 from `fixtures/mantis-64`
/// (64x64), tab 1 from `fixtures/mantis-512` (512x512), the size mismatch
/// a background run must survive. Tab 1 is active on return, the same
/// order a person opening a small project and then a large one would reach.
fn two_sealed_tabs(name: &str) -> AppController {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    open_fixture_tab(&mut controller, name, "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the small fixture must fuse");
    open_fixture_tab(&mut controller, name, "mantis-512", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("the large fixture must fuse");
    controller
}

/// Calls `canvas_feed(loaded)`, panicking with the actual answer unless it
/// is `Load`, and returns the composite path it carries -- what most of
/// these tests use to confirm which tab's own planes just got loaded.
/// A saved row answers `Clear` or `Keep` until its render settles, so the
/// feed is repeated after each settle for up to a minute.
fn load_path(controller: &mut AppController, loaded: Option<usize>) -> PathBuf {
    let start = Instant::now();
    loop {
        match controller.canvas_feed(loaded) {
            CanvasFeed::Load { composite, .. } => return composite,
            other if start.elapsed() > Duration::from_secs(60) => {
                panic!("expected CanvasFeed::Load, got {other:?}")
            }
            _ => {
                controller.settle_checkpoint_render();
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Fuses `background_set`'s own (first-opened) tab while `foreground_set`'s
/// tab is active, asserting the active tab's already-loaded planes survive
/// the background run's completion unchanged (asserted separately by
/// `a_read_past_the_composite_draws_nothing_instead_of_panicking`).
fn background_run_leaves_canvas_alone(name: &str, background_set: &str, foreground_set: &str) {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    open_fixture_tab(&mut controller, name, background_set, 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the background tab's first fuse must complete");
    open_fixture_tab(&mut controller, name, foreground_set, 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("the foreground tab's first fuse must complete");
    assert_eq!(controller.active_project(), Some(1));

    // A real repaint would already have loaded the active (foreground)
    // tab's own planes by this point.
    assert!(matches!(
        controller.canvas_feed(None),
        CanvasFeed::Load { tab: 1, .. }
    ));

    // Re-fuse the background tab while the foreground tab stays active.
    controller.select_project(0);
    assert!(controller.start_fuse());
    controller.select_project(1);
    assert_eq!(
        controller.active_project(),
        Some(1),
        "switching tabs mid-run must not disturb which tab the run belongs to"
    );
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the background tab's run must still settle while another tab is active");

    assert!(
        matches!(controller.canvas_feed(Some(1)), CanvasFeed::Keep),
        "a run completing on a background tab must leave the active tab's canvas alone"
    );
}

#[test]
fn a_background_run_completing_does_not_reload_the_active_tabs_canvas() {
    background_run_leaves_canvas_alone("bg_small_under_large", "mantis-64", "mantis-512");
    background_run_leaves_canvas_alone("bg_large_under_small", "mantis-512", "mantis-64");
}

#[test]
fn a_foreground_run_completing_still_feeds_the_canvas() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("fg_scratch"));
    open_fixture_tab(&mut controller, "fg", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the first fuse must complete");

    // The canvas already holds the (only, active) tab's planes.
    assert!(matches!(
        controller.canvas_feed(None),
        CanvasFeed::Load { tab: 0, .. }
    ));
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Keep));

    // A second run on the same, still-active tab: the owner already
    // matches, but the planes on disk changed underneath it -- the case
    // `Keep` alone cannot distinguish from the first run.
    assert!(
        controller.start_fuse(),
        "pressing Fuse stack again on an Opened tab must run the pipeline again"
    );
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the second fuse must complete");
    assert!(matches!(
        controller.canvas_feed(Some(0)),
        CanvasFeed::Load { tab: 0, .. }
    ));
}

#[test]
fn a_tab_switch_feeds_the_active_tabs_planes() {
    let mut controller = two_sealed_tabs("switch");
    let large = load_path(&mut controller, None);

    controller.select_project(0);
    let small = load_path(&mut controller, Some(1));
    assert_ne!(
        small, large,
        "switching tabs must load the newly active tab's own planes"
    );

    controller.select_project(1);
    let large_again = load_path(&mut controller, Some(0));
    assert_eq!(
        large_again, large,
        "switching back must load the same tab's own planes again"
    );
}

#[test]
fn switching_to_an_unfused_tab_shows_no_stale_planes() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("unfused_scratch"));
    open_fixture_tab(&mut controller, "unfused", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the fuse must complete");
    // The canvas now holds tab 0's planes.
    let _ = load_path(&mut controller, None);

    // A second tab, opened but never fused -- nothing sealed to show yet.
    open_fixture_tab(&mut controller, "unfused", "mantis-512", 3);
    assert_eq!(controller.active_project(), Some(1));
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Clear));
}

#[test]
fn closing_the_active_tab_feeds_the_neighbour() {
    // The active tab itself closes; the tab that slides into its own raw
    // index is a different project, so the coincidence of the index
    // matching must not read as "nothing changed".
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("close_active_scratch"));
    open_fixture_tab(&mut controller, "close_active", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60)).expect("tab 0 must fuse");
    open_fixture_tab(&mut controller, "close_active", "mantis-512", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120)).expect("tab 1 must fuse");
    open_fixture_tab(&mut controller, "close_active", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60)).expect("tab 2 must fuse");

    controller.select_project(1);
    let middle = load_path(&mut controller, None);
    controller.close_project(1);
    assert_eq!(
        controller.active_project(),
        Some(1),
        "closing the middle of three tabs re-selects the tab that slides into its place"
    );
    match controller.canvas_feed(Some(1)) {
        CanvasFeed::Load { tab, composite, .. } => {
            assert_eq!(tab, 1);
            assert_ne!(
                composite, middle,
                "the tab that slides into the closed one's raw index must load its own \
                 planes, not the just-closed tab's stale ones"
            );
        }
        other => {
            panic!("expected a reload after the close renumbered the active tab, got {other:?}")
        }
    }

    // Tab 2 is active and loaded; closing tab 0 (not the active tab)
    // renumbers tab 2 down to index 1 -- the same raw index a stale,
    // unrelated load could have left behind.
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("close_bg_scratch"));
    open_fixture_tab(&mut controller, "close_bg", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60)).expect("tab 0 must fuse");
    open_fixture_tab(&mut controller, "close_bg", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60)).expect("tab 1 must fuse");
    open_fixture_tab(&mut controller, "close_bg", "mantis-512", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120)).expect("tab 2 must fuse");

    let tab2 = load_path(&mut controller, None);
    controller.close_project(0);
    assert_eq!(
        controller.active_project(),
        Some(1),
        "closing tab 0 while tab 2 is active renumbers it down to index 1"
    );
    // Simulates a canvas that still holds tab 1's own (stale) planes from
    // an earlier switch never reconciled again before this close --
    // exactly the raw index the renumbered active tab now also carries.
    match controller.canvas_feed(Some(1)) {
        CanvasFeed::Load { tab, composite, .. } => {
            assert_eq!(tab, 1);
            assert_eq!(
                composite, tab2,
                "the renumbered tab must load the project that was actually active before \
                 the close, not whatever the coincidentally matching raw index last held"
            );
        }
        other => panic!("expected a reload, got {other:?}"),
    }
}

/// `composite_ready` follows the *active* tab's own checkpoint count: false
/// with no tab active, false right after opening (no run yet), true once
/// that tab's own fuse settles, false while a second, never-fused tab is
/// active even though the first tab is fused, true again on returning to
/// the fused tab.
#[test]
fn composite_ready_follows_the_active_tabs_own_fuse() {
    let mut controller = AppController::new();
    assert!(
        !controller.composite_ready(),
        "no active tab has no composite to show"
    );

    controller.set_scratch_root_for_test(common::tree("composite_ready_scratch"));
    open_fixture_tab(&mut controller, "composite_ready", "mantis-64", 2);
    assert!(
        !controller.composite_ready(),
        "a freshly opened tab has no composite until its first fuse"
    );

    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the first fuse must complete");
    assert!(
        controller.composite_ready(),
        "the active tab's own settled fuse must flip it true"
    );
    assert!(matches!(
        controller.canvas_feed(None),
        CanvasFeed::Load { tab: 0, .. }
    ));

    open_fixture_tab(&mut controller, "composite_ready", "mantis-512", 3);
    assert!(
        !controller.composite_ready(),
        "a second, never-fused tab being active must read false, even though tab 0 is fused"
    );

    controller.select_project(0);
    assert!(
        controller.composite_ready(),
        "returning to the fused tab must read true again"
    );

    controller.select_plus();
    assert!(
        !controller.composite_ready(),
        "\"+\" selected must read false, even though tab 0 is fused"
    );

    controller.select_project(0);
    assert!(
        controller.composite_ready(),
        "leaving \"+\" for the fused tab must read true again"
    );
}

#[test]
fn opening_a_saved_project_feeds_its_own_planes() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("saved_scratch"));
    let root = open_fixture_tab(&mut controller, "saved", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the fuse must complete");
    let title = controller.project_tabs()[0].title.clone();
    let ffp = root.join(format!("{title}.ffp"));
    assert!(controller.save_active_project(|proposed| Some(proposed.to_path_buf())));
    assert!(ffp.is_file());

    open_fixture_tab(&mut controller, "saved", "mantis-512", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("the second fuse must complete");
    assert!(matches!(
        controller.canvas_feed(None),
        CanvasFeed::Load { tab: 1, .. }
    ));

    // Reopening the saved `.ffp` mid-session lands a third tab and must
    // load its own planes, not the tab it displaced.
    assert!(matches!(
        common::open_and_settle(&mut controller, &[ffp]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.active_project(), Some(2));
    let composite = load_path(&mut controller, Some(1));
    assert!(composite.is_file());
    assert!(matches!(
        controller.canvas_feed(Some(1)),
        CanvasFeed::Load { tab: 2, .. }
    ));
}

/// A fused and saved `.ffp` of `set`'s first `count` frames, written by a
/// throwaway controller whose scratch root nobody else reads.
fn sealed_project(name: &str, set: &str, count: usize) -> PathBuf {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_{set}_writer")));
    open_fixture_tab(&mut controller, name, set, count);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("the fixture must fuse");
    let mut target = None;
    assert!(common::save_and_settle(&mut controller, |proposed| {
        target = Some(proposed.to_path_buf());
        target.clone()
    }));
    target.expect("a never-saved tab asks where to save")
}

fn head_of(ffp: &Path) -> String {
    focusforge_core::project::Project::open(ffp)
        .unwrap()
        .checkpoint()
        .id
        .clone()
}

/// A controller over its own scratch root, with `ffps` opened in order.
fn controller_over(scratch: &Path, ffps: &[&Path]) -> AppController {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(scratch.to_path_buf());
    for ffp in ffps {
        assert!(matches!(
            common::open_and_settle(&mut controller, &[ffp.to_path_buf()]),
            OpenOutcome::Opened(_)
        ));
    }
    controller
}

#[test]
fn an_opened_project_feeds_its_planes_once_the_render_settles() {
    let ffp = sealed_project("render_settles", "mantis-512", 3);
    let scratch = common::tree("render_settles_scratch");
    let mut controller = controller_over(&scratch, &[&ffp]);

    assert!(matches!(controller.canvas_feed(None), CanvasFeed::Clear));
    assert!(controller.take_render_started());
    assert!(controller.materialize_active_tab_planes().is_none());
    common::settle_render(&mut controller);
    let dir = job::checkpoint_plane_dir(&scratch, &ffp, &head_of(&ffp));
    match controller.canvas_feed(None) {
        CanvasFeed::Load {
            composite, zmap, ..
        } => {
            let zmap = zmap.expect("a readable Z-Map feeds the canvas");
            assert!(composite.starts_with(&dir) && zmap.starts_with(&dir));
            assert!(composite.is_file() && zmap.is_file());
        }
        other => panic!("the settled render must load, got {other:?}"),
    }

    let mut second = controller_over(&scratch, &[&ffp]);
    assert!(matches!(
        second.canvas_feed(None),
        CanvasFeed::Load { tab: 0, .. }
    ));
    assert!(
        !second.take_render_started(),
        "a fresh cache needs no render"
    );
}

#[test]
fn a_saved_rows_planes_never_feed_a_tab_switched_to_meanwhile() {
    let small = sealed_project("switched_meanwhile", "mantis-64", 3);
    let large = sealed_project("switched_meanwhile", "mantis-512", 3);
    let scratch = common::tree("switched_meanwhile_scratch");
    let mut controller = controller_over(&scratch, &[&small, &large]);

    controller.select_project(0);
    assert!(matches!(controller.canvas_feed(None), CanvasFeed::Clear));
    assert!(controller.take_render_started());
    controller.select_project(1);
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Clear));
    assert!(!controller.take_render_started(), "the one slot is busy");

    common::settle_render(&mut controller);
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Clear));
    assert!(controller.take_render_started());
    common::settle_render(&mut controller);
    match controller.canvas_feed(Some(0)) {
        CanvasFeed::Load { tab, composite, .. } => {
            assert_eq!(tab, 1);
            let dir = job::checkpoint_plane_dir(&scratch, &large, &head_of(&large));
            assert!(composite.starts_with(dir));
        }
        other => panic!("the active tab's own render must load, got {other:?}"),
    }
}

#[test]
fn a_refused_saved_row_is_not_rendered_again() {
    let ffp = sealed_project("refused_row", "mantis-64", 3);
    let scratch = common::tree("refused_row_scratch");
    let blocked = job::checkpoint_plane_dir(&scratch, &ffp, &head_of(&ffp));
    fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    fs::write(&blocked, b"not a directory").unwrap();
    let mut controller = controller_over(&scratch, &[&ffp]);

    assert!(matches!(controller.canvas_feed(None), CanvasFeed::Clear));
    assert!(controller.take_render_started());
    common::settle_render(&mut controller);
    assert!(matches!(controller.canvas_feed(None), CanvasFeed::Clear));
    assert!(!controller.composite_message_lines().is_empty());
    assert!(matches!(controller.canvas_feed(None), CanvasFeed::Clear));
    assert!(!controller.take_render_started(), "a refusal is final");

    let row = controller
        .checkpoints_panel()
        .entries
        .iter()
        .position(|entry| entry.id.is_some())
        .expect("the project lists its checkpoint");
    controller.select_checkpoint_row(row);
    assert!(matches!(controller.canvas_feed(None), CanvasFeed::Clear));
    assert!(controller.take_render_started(), "re-selecting tries again");
    common::settle_render(&mut controller);
}

/// Both panes draw at the sealed composite's own extent once a tab holds
/// one, not the raw source frames': fuses a `mantis-512` tab (512x512
/// frames), whose alignment crop makes the composite smaller, and asserts
/// `frame_draw` and `plane_draw` return the identical source rect, bounded
/// by the composite's own dimensions.
#[test]
fn right_pane_uses_composite_extent() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("right_pane_extent_scratch"));
    open_fixture_tab(&mut controller, "right_pane_extent", "mantis-512", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("the fuse must complete");

    let composite_path = load_path(&mut controller, None);
    let composite_header =
        TiffHeader::open(&composite_path).expect("the sealed composite must have a real header");
    assert!(
        composite_header.width < 512 || composite_header.height < 512,
        "the alignment crop must make the composite smaller than the 512x512 frames, got {}x{}",
        composite_header.width,
        composite_header.height
    );

    controller.set_pane_geometry(
        PaneRect {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 600.0,
            scale_factor: 1.0,
        },
        PaneRect {
            x: 600.0,
            y: 0.0,
            width: 600.0,
            height: 600.0,
            scale_factor: 1.0,
        },
    );
    controller.fit();

    let frame = controller
        .frame_draw()
        .expect("a fused tab still has a frame pane to draw");
    let plane = controller
        .plane_draw()
        .expect("a fused tab has a sealed plane to draw");

    assert!(
        plane.src.2 <= composite_header.width && plane.src.3 <= composite_header.height,
        "the right pane's source rect {:?} must stay within the sealed composite's own extent \
         {}x{}",
        plane.src,
        composite_header.width,
        composite_header.height
    );
    assert_eq!(
        plane.src, frame.src,
        "both panes must draw the same source rect once the tab holds a composite"
    );
}

/// A frame whose compression neither the mmap reader nor the decoded reader
/// supports ([`open_frame_set`] opens LZW, Deflate and Adobe-Deflate; JPEG
/// is refused by both) shows the decoded reader's own refusal text, and
/// dropping into it still drops whatever was open before -- the
/// drop-before-open rule holds even on a failed open.
#[test]
fn unsupported_compression_frame_shows_reader_refusal() {
    let dir = common::tree("unsupported_compression_frame_refusal");
    let good = common::fixture_frame("mantis-64", 0);
    let jpeg_tagged = dir.join("jpeg-tagged.tif");
    write_compressed_copy(&good, &jpeg_tagged, 7);

    let mut frame = FrameSource::default();
    frame
        .set(&good, None, job::host_budget())
        .expect("the uncompressed fixture frame must open");
    assert!(frame.path().is_some());

    let err = frame
        .set(&jpeg_tagged, None, job::host_budget())
        .expect_err("an unsupported compression must be refused");
    assert!(
        err.to_string()
            .contains("Compression=7 is not supported by the decoded TIFF reader"),
        "the decoded reader's own refusal text must reach the surface, got: {err}"
    );
    assert!(
        frame.path().is_none(),
        "a failed open must still drop whatever was open before it"
    );
}

/// Copies `source` (a little-endian, classic-TIFF fixture frame) to `dest`,
/// patching its `Compression` tag (259) to `compression` in place.
fn write_compressed_copy(source: &Path, dest: &Path, compression: u16) {
    let mut bytes = fs::read(source).expect("read fixture frame");
    assert_eq!(
        &bytes[0..2],
        b"II",
        "fixture frame must be little-endian for this byte-level patch"
    );
    let ifd_offset = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let n_entries =
        u16::from_le_bytes(bytes[ifd_offset..ifd_offset + 2].try_into().unwrap()) as usize;
    let mut patched = false;
    for i in 0..n_entries {
        let entry_off = ifd_offset + 2 + i * 12;
        let tag = u16::from_le_bytes(bytes[entry_off..entry_off + 2].try_into().unwrap());
        if tag == 259 {
            bytes[entry_off + 8..entry_off + 10].copy_from_slice(&compression.to_le_bytes());
            patched = true;
            break;
        }
    }
    assert!(
        patched,
        "Compression tag (259) not found in {}",
        source.display()
    );
    fs::write(dest, bytes).expect("write compressed copy");
}

/// The drop-before-open rule with no GPU and no window: stepping over every
/// frame of a fixture set leaves exactly one handle open (the last one), and
/// a failed `set` leaves none open at all.
#[test]
fn frame_source_holds_one_frame() {
    let mut frame = FrameSource::default();
    for i in 0..3 {
        frame
            .set(
                &common::fixture_frame("mantis-64", i),
                None,
                job::host_budget(),
            )
            .expect("a fixture frame must open");
    }
    assert_eq!(
        frame.path().unwrap().file_name().unwrap().to_str().unwrap(),
        "02.tif",
        "only the last-opened frame's handle must remain"
    );
    assert!(frame.dimensions().is_some());

    let missing = PathBuf::from("/does/not/exist.tif");
    assert!(frame.set(&missing, None, job::host_budget()).is_err());
    assert!(
        frame.path().is_none(),
        "a failed set must leave no handle open at all"
    );
    assert!(frame.dimensions().is_none());
}

/// Reads the resident set size (bytes) from `/proc/self/statm`'s second
/// field (pages), Linux-only -- this is the reference-stack step-through's
/// own bound, exercised nowhere else.
#[cfg(target_os = "linux")]
fn resident_bytes() -> usize {
    let statm = fs::read_to_string("/proc/self/statm").expect("/proc/self/statm must read");
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .expect("statm must have a resident field")
        .parse()
        .expect("resident field must be a number");
    pages * 4096
}

/// Steps `FrameSource` over all 61 frames of the reference stack, reading
/// one bounded rect from each into a reused buffer, asserting resident
/// memory stays flat rather than growing with the frame count -- the
/// evidence that exactly one mmap is ever open at a time.
#[test]
#[ignore = "reads the reference stack, minutes"]
fn frame_steps_keep_one_handle() {
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("resident-memory measurement needs /proc/self/statm; skipping");
    }
    #[cfg(target_os = "linux")]
    {
        let Some(stack_dir) = common::reference_stack_dir() else {
            return;
        };
        let paths: Vec<PathBuf> = focusforge_core::io::frame_paths(&stack_dir)
            .expect("frame_paths must enumerate the directory")
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("IMG_"))
            })
            .collect();
        assert_eq!(
            paths.len(),
            61,
            "IMG_-filtered list must exclude Mantis_Stacked.tif"
        );

        let mut frame = FrameSource::default();
        let rect = focusforge_core::tile::Rect {
            x: 0,
            y: 0,
            width: 512,
            height: 512,
        };
        let mut buf = vec![0u16; 512 * 512 * 3];

        let mut baseline = 0usize;
        let mut peak = 0usize;
        for (i, path) in paths.iter().enumerate() {
            frame
                .set(path, None, job::host_budget())
                .expect("a reference-stack frame must open");
            frame
                .read_into(rect, std::num::NonZeroU32::MIN, &mut buf)
                .expect("the visible rect must read");
            let rss = resident_bytes();
            peak = peak.max(rss);
            if i == 0 {
                baseline = rss;
            }
        }
        let growth = peak.saturating_sub(baseline);
        eprintln!(
            "resident after frame 0: {baseline} bytes, peak across 61 frames: {peak} bytes, \
             growth {growth} bytes"
        );
        assert!(
            growth < 300 * 1024 * 1024,
            "resident memory must stay flat while stepping through every frame: grew \
             {growth} bytes ({} MB)",
            growth / (1024 * 1024)
        );
        assert_eq!(
            frame.path(),
            Some(paths.last().unwrap().as_path()),
            "only the last frame's handle must remain open"
        );
    }
}

/// The fit-zoom redraw cost on one full-size reference frame in a 566 x 377
/// pane: the decimated read's sample count, and the wall-clock of the step-1
/// and stepped reads at fit and of the 100 % read, plus the renderer's own
/// `redraw_frame` when a GPU is present.
#[test]
#[ignore = "reads the reference stack, minutes"]
fn fit_zoom_redraw_reads_a_pane_sized_image() {
    use std::num::NonZeroU32;
    use std::time::Instant;

    use focusforge_gui::viewport::{ImageSize, fit};

    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };
    let first = focusforge_core::io::frame_paths(&stack_dir)
        .expect("frame_paths must enumerate the directory")
        .into_iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .expect("the reference stack holds source frames");
    let mut frame = FrameSource::default();
    frame
        .set(&first, None, job::host_budget())
        .expect("a reference frame must open");
    let (w, h) = frame.dimensions().unwrap();
    let spp = frame.samples_per_pixel().unwrap() as usize;
    let img = ImageSize {
        width: w,
        height: h,
    };
    let pane = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 566.0,
        height: 377.0,
        scale_factor: 1.0,
    };

    let mut vp = fit(img, pane);
    let at_fit = vp.draw(img, pane).expect("fit draws");
    let rect_of = |d: &focusforge_gui::viewport::PaneDraw| focusforge_core::tile::Rect {
        x: d.src.0,
        y: d.src.1,
        width: d.src.2,
        height: d.src.3,
    };
    let (ew, eh) = at_fit.extent();
    let stepped_samples = ew as usize * eh as usize * spp;
    let full_samples = at_fit.src.2 as usize * at_fit.src.3 as usize * spp;
    assert!(
        stepped_samples < 1_000_000,
        "fit reads {stepped_samples} samples"
    );

    let time_read = |rect, step: NonZeroU32, len: usize| {
        let mut buf = vec![0u16; len];
        let start = Instant::now();
        frame.read_into(rect, step, &mut buf).expect("read");
        start.elapsed()
    };
    // Warm the page cache once so both fit timings read from memory.
    time_read(rect_of(&at_fit), NonZeroU32::MIN, full_samples);
    let step1 = time_read(rect_of(&at_fit), NonZeroU32::MIN, full_samples);
    let stepped = time_read(rect_of(&at_fit), at_fit.step, stepped_samples);

    while vp.scale() < 1.0 - 1e-9 {
        vp.zoom_steps(1, (283.0, 188.5), img, pane);
    }
    let at_100 = vp.draw(img, pane).unwrap();
    let len_100 = at_100.src.2 as usize * at_100.src.3 as usize * spp;
    let one_to_one = time_read(rect_of(&at_100), at_100.step, len_100);
    eprintln!(
        "{w}x{h} in 566x377: fit step {} reads {stepped_samples} samples ({ew}x{eh}) in \
         {stepped:?}; step 1 reads {full_samples} in {step1:?}; 100 % reads {len_100} in \
         {one_to_one:?}",
        at_fit.step
    );

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let mut renderer = focusforge_gui::gpu_canvas::CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer
        .set_frame(&first, None, job::host_budget())
        .expect("the renderer opens the frame");
    for (label, draw) in [("fit", at_fit), ("100 %", at_100)] {
        let start = Instant::now();
        let drawn = renderer.redraw_frame(&draw);
        eprintln!(
            "redraw_frame at {label}: {:?} (CPU side, submit included), drawn: {}",
            start.elapsed(),
            drawn.is_some()
        );
    }
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
/// yet run `bridge::repaint`.
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

#[test]
fn current_crop_bounds_each_main_pane() {
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
    app.set_frame_crop_x(21.0);
    app.set_frame_crop_y(34.0);
    app.set_frame_crop_w(300.0);
    app.set_frame_crop_h(200.0);
    app.set_crop_x(15.0);
    app.set_crop_y(27.0);
    app.set_crop_w(280.0);
    app.set_crop_h(180.0);

    let pane = |id: &str| {
        ElementQuery::from_root(&app)
            .match_id(format!("FFCanvas::{id}"))
            .find_first()
            .unwrap_or_else(|| panic!("missing {id}"))
    };
    let crops = ElementQuery::from_root(&app)
        .match_id("FFPane::image-crop")
        .find_all();
    let assert_crop = |pane_id: &str, local: (f32, f32), size: (f32, f32)| {
        let pane = pane(pane_id);
        let pane_pos = pane.absolute_position();
        let crop = crops
            .iter()
            .find(|crop| {
                let pos = crop.absolute_position();
                (pos.x - (pane_pos.x + local.0)).abs() < GEOMETRY_EPS
                    && (pos.y - (pane_pos.y + local.1)).abs() < GEOMETRY_EPS
            })
            .unwrap_or_else(|| panic!("{pane_id}: missing current crop"));
        let got = crop.size();
        assert!(
            (got.width - size.0).abs() < GEOMETRY_EPS && (got.height - size.1).abs() < GEOMETRY_EPS,
            "{pane_id}: crop {got:?}, expected {size:?}"
        );
    };
    assert_crop("pane-left", (21.0, 34.0), (300.0, 200.0));
    assert_crop("pane-right", (15.0, 27.0), (280.0, 180.0));
}

/// The not-yet-fused placeholder is on screen exactly while a project is
/// loaded and the active tab has no composite yet, 220 px wide and centred
/// on `pane-right`'s own rect without covering its label, at both the base
/// and the minimum window size -- and a mid-run set of bottom-bar
/// properties changes neither its visibility nor its rect, since it reads
/// no run state at all.
#[test]
fn pane_placeholder_shows_only_until_the_first_fuse() {
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

    for (label, width, height) in [("base", 1440, 900), ("minimum", 1200, 760)] {
        app.window()
            .set_size(slint::PhysicalSize::new(width, height));
        app.set_project_loaded(true);
        app.set_composite_ready(false);

        let pane = ElementQuery::from_root(&app)
            .match_id("FFCanvas::pane-right")
            .find_first()
            .unwrap_or_else(|| panic!("{label}: no element matched id \"FFCanvas::pane-right\""));
        let pane_pos = pane.absolute_position();
        let pane_size = pane.size();

        let placeholder = ElementQuery::from_root(&app)
            .match_id("FFCanvas::pane-placeholder")
            .find_first()
            .unwrap_or_else(|| {
                panic!("{label}: placeholder must show while the active tab has no composite")
            });
        let p_pos = placeholder.absolute_position();
        let p_size = placeholder.size();
        assert!(
            (p_size.width - 220.0).abs() < GEOMETRY_EPS,
            "{label}: placeholder width {}, expected 220",
            p_size.width
        );
        assert!(
            (p_pos.x - (pane_pos.x + (pane_size.width - p_size.width) / 2.0)).abs() < GEOMETRY_EPS
                && (p_pos.y - (pane_pos.y + (pane_size.height - p_size.height) / 2.0)).abs()
                    < GEOMETRY_EPS,
            "{label}: placeholder must be centred on pane-right's rect, got {p_pos:?} over pane \
             {pane_pos:?} {pane_size:?}"
        );

        // The 120x22 label at pane-right's own +10/+10 must still be found,
        // and the placeholder must not overlap it.
        let label_el = ElementQuery::from_root(&app)
            .match_id("FFPane::pane-label")
            .find_all()
            .into_iter()
            .find(|el| {
                let p = el.absolute_position();
                (p.x - (pane_pos.x + 10.0)).abs() < GEOMETRY_EPS
                    && (p.y - (pane_pos.y + 10.0)).abs() < GEOMETRY_EPS
            })
            .unwrap_or_else(|| panic!("{label}: no pane-label found at pane-right's +10/+10"));
        let l_pos = label_el.absolute_position();
        let l_size = label_el.size();
        let disjoint = p_pos.x >= l_pos.x + l_size.width
            || l_pos.x >= p_pos.x + p_size.width
            || p_pos.y >= l_pos.y + l_size.height
            || l_pos.y >= p_pos.y + p_size.height;
        assert!(
            disjoint,
            "{label}: placeholder {p_pos:?} {p_size:?} must not overlap the pane label \
             {l_pos:?} {l_size:?}"
        );

        // A mid-run bottom-bar state must not move or hide the placeholder
        // -- it reads no run property.
        app.set_stage_word("Aligning".into());
        app.set_step_text("42 of 61".into());
        app.set_bar_fraction(0.62);
        app.set_bar_tone("running".into());
        let placeholder_after = ElementQuery::from_root(&app)
            .match_id("FFCanvas::pane-placeholder")
            .find_first()
            .unwrap_or_else(|| panic!("{label}: placeholder must still show mid-run"));
        assert_eq!(
            placeholder_after.absolute_position(),
            p_pos,
            "{label}: a mid-run bottom-bar state must not move the placeholder"
        );
        assert_eq!(
            placeholder_after.size(),
            p_size,
            "{label}: a mid-run bottom-bar state must not resize the placeholder"
        );
        app.set_stage_word("".into());
        app.set_step_text("".into());
        app.set_bar_fraction(0.0);
        app.set_bar_tone("".into());

        app.set_composite_ready(true);
        assert!(
            ElementQuery::from_root(&app)
                .match_id("FFCanvas::pane-placeholder")
                .find_all()
                .is_empty(),
            "{label}: placeholder must be gone once the active tab has a composite"
        );

        app.set_project_loaded(false);
        assert!(
            ElementQuery::from_root(&app)
                .match_id("FFCanvas::pane-placeholder")
                .find_all()
                .is_empty(),
            "{label}: placeholder must stay hidden with no project loaded"
        );
    }
}

/// In Editor mode, hovering either pane shows both `FFBrushCursor`s at the
/// expected centre -- the one under the pointer at the raw hover point, the
/// other at the projected pane-local point -- neither shows over the gutter or
/// outside Editor mode, and each cursor's clip container matches its own
/// pane's rect exactly.
#[test]
fn both_brush_cursors_follow_either_pane() {
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
    app.set_project_loaded(true);
    app.set_mode("editor".into());
    app.set_cursor_extent(20.0);
    app.set_cursor_visible(true);

    let rect = |id: &str| {
        let handle = ElementQuery::from_root(&app)
            .match_id(format!("FFCanvas::{id}"))
            .find_first()
            .unwrap_or_else(|| panic!("no element matched id \"FFCanvas::{id}\""));
        (handle.absolute_position(), handle.size())
    };
    let (left_pos, left_size) = rect("pane-left");
    let (right_pos, _right_size) = rect("pane-right");
    app.set_cursor_frame_x(40.0);
    app.set_cursor_frame_y(left_size.height / 2.0);
    app.set_cursor_composite_x(40.0);
    app.set_cursor_composite_y(left_size.height / 2.0);

    // Each clip container matches its own pane's rect exactly.
    for (clip_id, pane_id) in [
        ("brush-cursor-left-clip", "pane-left"),
        ("brush-cursor-right-clip", "pane-right"),
    ] {
        let (clip_pos, clip_size) = rect(clip_id);
        let (pane_pos, pane_size) = rect(pane_id);
        assert!(
            (clip_pos.x - pane_pos.x).abs() < GEOMETRY_EPS
                && (clip_pos.y - pane_pos.y).abs() < GEOMETRY_EPS,
            "{clip_id}: position {clip_pos:?}, expected {pane_pos:?}"
        );
        assert!(
            (clip_size.width - pane_size.width).abs() < GEOMETRY_EPS
                && (clip_size.height - pane_size.height).abs() < GEOMETRY_EPS,
            "{clip_id}: size {clip_size:?}, expected {pane_size:?}"
        );
    }

    let find_cursor = |id: &str| {
        ElementQuery::from_root(&app)
            .match_id(format!("FFCanvas::{id}"))
            .find_first()
    };
    let center = |handle: &i_slint_backend_testing::ElementHandle| {
        let p = handle.absolute_position();
        let s = handle.size();
        (p.x + s.width / 2.0, p.y + s.height / 2.0)
    };
    let move_to = |point: slint::LogicalPosition| {
        app.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: point });
    };

    // Hovering the left pane: the home cursor sits at the raw pointer, the
    // mirrored one on the right pane at the same local point.
    let left_point =
        slint::LogicalPosition::new(left_pos.x + 40.0, left_pos.y + left_size.height / 2.0);
    move_to(left_point);
    assert!(app.get_canvas_pane_hovered());
    let home = find_cursor("brush-cursor-left").expect("shows while hovering its own pane");
    let mirrored = find_cursor("brush-cursor-right").expect("mirrors onto the other pane");
    let (hx, hy) = center(&home);
    assert!(
        (hx - left_point.x).abs() < GEOMETRY_EPS && (hy - left_point.y).abs() < GEOMETRY_EPS,
        "left cursor at the raw pointer, got ({hx}, {hy})"
    );
    let (mx, my) = center(&mirrored);
    assert!(
        (mx - (right_pos.x + 40.0)).abs() < GEOMETRY_EPS
            && (my - left_point.y).abs() < GEOMETRY_EPS,
        "right cursor at the projected point, got ({mx}, {my})"
    );

    // Hovering the right pane: the roles (and the sign of the shift) flip.
    let right_point =
        slint::LogicalPosition::new(right_pos.x + 40.0, left_pos.y + left_size.height / 2.0);
    move_to(right_point);
    assert!(app.get_canvas_pane_hovered());
    let home = find_cursor("brush-cursor-right").expect("shows while hovering its own pane");
    let mirrored = find_cursor("brush-cursor-left").expect("mirrors onto the other pane");
    let (hx, hy) = center(&home);
    assert!(
        (hx - right_point.x).abs() < GEOMETRY_EPS && (hy - right_point.y).abs() < GEOMETRY_EPS,
        "right cursor at the raw pointer, got ({hx}, {hy})"
    );
    let (mx, my) = center(&mirrored);
    assert!(
        (mx - (left_pos.x + 40.0)).abs() < GEOMETRY_EPS
            && (my - right_point.y).abs() < GEOMETRY_EPS,
        "left cursor at the projected point, got ({mx}, {my})"
    );

    // The bridge hides both cursors when the pointer leaves either pane.
    let gutter_point = slint::LogicalPosition::new(
        (left_pos.x + left_size.width + right_pos.x) / 2.0,
        left_pos.y + left_size.height / 2.0,
    );
    move_to(gutter_point);
    assert!(!app.get_canvas_pane_hovered());
    app.set_cursor_visible(false);
    assert!(
        find_cursor("brush-cursor-left").is_none() && find_cursor("brush-cursor-right").is_none(),
        "both cursors hidden over the gutter"
    );

    // Outside Editor mode, neither shows even while hovering a pane.
    app.set_mode("stacking".into());
    move_to(left_point);
    assert!(
        find_cursor("brush-cursor-left").is_none() && find_cursor("brush-cursor-right").is_none(),
        "both cursors hidden outside Editor mode"
    );
}

#[test]
fn pane_label_short_enough_passes_through_unchanged() {
    assert_eq!(truncate_pane_label("Composite"), "Composite");
}

#[test]
fn pane_label_too_long_keeps_its_tail() {
    let name = "20260829-IMG_9800.tif";
    let truncated = truncate_pane_label(name);
    assert!(
        truncated.chars().count() < name.chars().count(),
        "{truncated:?} must be shorter than the untruncated {name:?}"
    );
    assert!(
        truncated.ends_with("9800.tif"),
        "{truncated:?} must keep the distinguishing tail of {name:?}"
    );
    assert!(
        !truncated.contains("20260829-IMG_9"),
        "{truncated:?} must not still carry the shared, non-distinguishing prefix"
    );
    assert_eq!(
        truncated.matches('\u{2026}').count(),
        1,
        "{truncated:?} must mark the truncation with exactly one ellipsis"
    );
    assert!(
        truncated.starts_with('\u{2026}'),
        "{truncated:?} must lead with the ellipsis, since the head is what was dropped"
    );
}

// ---------------------------------------------------------------------
// Display resource reuse and content-coverage skipping through
// `CanvasRenderer`, with a real GPU device.
// ---------------------------------------------------------------------

/// The one composite/Z-Map golden pair under `fixtures/mantis-512/golden`.
fn golden_plane_files() -> (PathBuf, PathBuf) {
    let dir = common::workspace_root().join("fixtures/mantis-512/golden");
    let mut composite = None;
    let mut zmap = None;
    for entry in fs::read_dir(&dir).expect("golden directory opens") {
        let path = entry.expect("golden directory entry reads").path();
        if path.extension().and_then(|e| e.to_str()) != Some("tif") {
            continue;
        }
        if focusforge_core::zmap::ZMapReader::open(&path).is_ok() {
            assert!(zmap.replace(path).is_none(), "one Z-Map golden is required");
        } else {
            assert!(
                composite.replace(path).is_none(),
                "one composite golden is required"
            );
        }
    }
    (
        composite.expect("composite golden exists"),
        zmap.expect("Z-Map golden exists"),
    )
}

/// Copies of the golden planes under `name`'s own tree: planes at other
/// paths, with the golden bytes.
fn copied_planes(name: &str) -> (PathBuf, PathBuf) {
    let (composite, zmap) = golden_plane_files();
    let dir = common::tree(name);
    let copy = |from: &Path| {
        let to = dir.join(from.file_name().expect("a plane file has a name"));
        fs::copy(from, &to).expect("the plane copies");
        to
    };
    (copy(&composite), copy(&zmap))
}

/// A warmup draw, then redraws covering every reuse/skip rule: an identical
/// draw, a `dst`-only shift, a pan, a zoom, a plane switch to Z-Map and
/// back, and a `set_planes`/`set_frame` on the unchanged draw -- asserting
/// each step's own counter deltas, exactly one plane's bytes wherever a
/// step reads. The golden draw's `src` has an odd width, decimated by a
/// `step` of 2.
#[test]
fn display_resources_are_reused_and_unchanged_content_is_not_reread() {
    use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use focusforge_gui::viewport::{ImageSize, PaneDraw};
    use std::num::NonZeroU32;

    // Timing accounting is process-global, so this runs alone in a child.
    const CHILD: &str = "FOCUSFORGE_GUI_CANVAS_DISPLAY_REUSE_ACCOUNTING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "--exact",
            "display_resources_are_reused_and_unchanged_content_is_not_reread",
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
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };

    let (composite_path, zmap_path) = golden_plane_files();
    let frame_path = common::fixture_frame("mantis-64", 0);

    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .expect("golden planes open");
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .expect("fixture frame opens");

    let golden = PaneDraw {
        src: (3, 5, 37, 23),
        dst: (0.0, 0.0, 37.0, 23.0),
        crop: (0.0, 0.0, 37.0, 23.0),
        pixelated: false,
        step: NonZeroU32::new(2).unwrap(),
    };
    assert!(
        golden.src.2 % 2 == 1,
        "the golden draw must carry an odd width"
    );

    let samples_per_pixel = |path: &Path| {
        let stack = focusforge_core::io::TiffStack::open_files(&[path.to_path_buf()])
            .expect("fixture TIFF opens");
        u64::from(stack.header(0).samples_per_pixel)
    };
    let image_size = |path: &Path| {
        let stack = focusforge_core::io::TiffStack::open_files(&[path.to_path_buf()])
            .expect("fixture TIFF opens");
        let header = stack.header(0);
        ImageSize {
            width: header.width,
            height: header.height,
        }
    };
    let composite_spp = samples_per_pixel(&composite_path);
    let frame_spp = samples_per_pixel(&frame_path);
    let plane_image = image_size(&composite_path);
    let frame_image = image_size(&frame_path);

    let counters = || {
        let s = timing_snapshot().expect("trace accounting is enabled");
        (
            s.pipeline_creations,
            s.allocation_operations,
            s.sample_bytes,
            s.converted_bytes,
            s.upload_bytes,
        )
    };
    type Counters = (u64, u64, u64, u64, u64);
    let assert_steady = |before: Counters, after: Counters, label: &str| {
        assert_eq!(
            after.0 - before.0,
            0,
            "{label}: pipeline_creations must not grow"
        );
        assert_eq!(
            after.1 - before.1,
            0,
            "{label}: allocation_operations must not grow"
        );
        assert_eq!(after.2 - before.2, 0, "{label}: sample_bytes must not grow");
    };
    let assert_nothing_read = |before: Counters, after: Counters, label: &str| {
        assert_eq!(
            after.3 - before.3,
            0,
            "{label}: converted_bytes must not grow"
        );
        assert_eq!(after.4 - before.4, 0, "{label}: upload_bytes must not grow");
    };
    // Exactly one plane's `draw.extent()` read as `u16` samples and uploaded
    // packed two per `u32` word, the odd tail padded to a whole word.
    let assert_one_plane =
        |before: Counters, after: Counters, draw: &PaneDraw, spp: u64, label: &str| {
            let (width, height) = draw.extent();
            let samples = u64::from(width) * u64::from(height) * spp;
            assert_eq!(
                after.0 - before.0,
                0,
                "{label}: pipeline_creations must not grow"
            );
            assert_eq!(
                after.3 - before.3,
                samples * 2,
                "{label}: converted_bytes must be exactly one plane's samples"
            );
            assert_eq!(
                after.4 - before.4,
                samples.div_ceil(2) * 4,
                "{label}: upload_bytes must be exactly one plane's packed words"
            );
        };

    // Warmup: builds the retained pipeline/buffers/scratch and both panes'
    // own coverage.
    assert!(renderer.redraw_planes(&golden, None, None).is_some());
    assert!(renderer.redraw_frame(&golden).is_some());

    // 1. An identical draw: no read, no upload, no allocation.
    let before = counters();
    assert!(renderer.redraw_planes(&golden, None, None).is_some());
    assert!(renderer.redraw_frame(&golden).is_some());
    let after = counters();
    assert_steady(before, after, "identical draw");
    assert_nothing_read(before, after, "identical draw");

    // 2. A dst-only shift: same src/step, different placement -- still no
    // read/upload, whatever dst/pixelated now are.
    let shifted = PaneDraw {
        dst: (10.0, 4.0, 37.0, 23.0),
        pixelated: true,
        ..golden
    };
    let before = counters();
    assert!(renderer.redraw_planes(&shifted, None, None).is_some());
    assert!(renderer.redraw_frame(&shifted).is_some());
    let after = counters();
    assert_steady(before, after, "dst-only shift");
    assert_nothing_read(before, after, "dst-only shift");

    // 3. A small pan remains inside the retained guard, so it changes
    // placement without reading or uploading either pane.
    let panned = PaneDraw {
        src: (4, 6, 37, 23),
        dst: golden.dst,
        ..golden
    };
    let before = counters();
    assert!(renderer.redraw_planes(&panned, None, None).is_some());
    let after = counters();
    assert_steady(before, after, "guarded plane pan");
    assert_nothing_read(before, after, "guarded plane pan");

    let before = counters();
    assert!(renderer.redraw_frame(&panned).is_some());
    let after = counters();
    assert_steady(before, after, "guarded frame pan");
    assert_nothing_read(before, after, "guarded frame pan");

    // 4. A zoom to a different step: reads and uploads again.
    let zoomed = PaneDraw {
        step: NonZeroU32::new(1).unwrap(),
        ..panned
    };
    let before = counters();
    assert!(renderer.redraw_planes(&zoomed, None, None).is_some());
    let after = counters();
    assert_one_plane(
        before,
        after,
        &zoomed
            .guarded(plane_image, u32::MAX)
            .expect("an unbounded test texture fits"),
        composite_spp,
        "zoom",
    );

    // 5. `set_planes` with other planes on the unchanged draw invalidates the
    // composite pane's coverage; the next draw re-reads exactly it.
    let (other_composite, other_zmap) = copied_planes("display_resources_other_planes");
    renderer
        .set_planes(&other_composite, Some(&other_zmap))
        .expect("other planes open");
    let before = counters();
    renderer
        .redraw_planes(&zoomed, None, None)
        .expect("composite plane draws again");
    let after = counters();
    assert_one_plane(
        before,
        after,
        &zoomed
            .guarded(plane_image, u32::MAX)
            .expect("an unbounded test texture fits"),
        composite_spp,
        "post set_planes",
    );

    // 6. `set_frame` on the frame's unchanged draw invalidates its coverage;
    // the next frame draw re-reads exactly the frame.
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .expect("fixture frame re-opens");
    let before = counters();
    assert!(renderer.redraw_frame(&panned).is_some());
    let after = counters();
    assert_one_plane(
        before,
        after,
        &panned
            .guarded(frame_image, u32::MAX)
            .expect("an unbounded test texture fits"),
        frame_spp,
        "post set_frame",
    );
}

#[test]
fn pane_label_at_the_budget_passes_through_unchanged() {
    let at_budget = "829-IMG_9800.tif";
    assert_eq!(
        at_budget.chars().count(),
        16,
        "fixture must sit at the budget"
    );
    assert_eq!(truncate_pane_label(at_budget), at_budget);

    let one_over = "0829-IMG_9800.tif";
    assert_eq!(truncate_pane_label(one_over), "\u{2026}29-IMG_9800.tif");
}

// ---------------------------------------------------------------------
// The off-thread preparer: one active read, one newest pending request,
// one completed slot; and the freshness floor that refuses stale results.
// ---------------------------------------------------------------------

mod preparing {
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use focusforge_core::tile::Rect;
    use focusforge_gui::canvas::{Pane, PaneJob, PaneReadError, PaneReader, Prepared, Preparer};
    use focusforge_gui::viewport::PaneDraw;

    pub const WAIT: Duration = Duration::from_secs(10);

    /// One read the fake served: when it began and, once released, ended.
    pub struct Served {
        pub rect: Rect,
        pub started: Instant,
        pub ended: Option<Instant>,
    }

    /// A reader that logs every read and blocks each one until the test
    /// sends a token.
    pub struct FakeReader {
        spp: u32,
        pub log: Mutex<Vec<Served>>,
        gate: Mutex<Receiver<()>>,
    }

    impl FakeReader {
        pub fn new(spp: u32) -> (Arc<Self>, Sender<()>) {
            let (release, gate) = channel();
            let reader = Arc::new(Self {
                spp,
                log: Mutex::new(Vec::new()),
                gate: Mutex::new(gate),
            });
            (reader, release)
        }

        pub fn reads(&self) -> usize {
            self.log.lock().unwrap().len()
        }

        pub fn finished(&self) -> usize {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|served| served.ended.is_some())
                .count()
        }
    }

    impl PaneReader for FakeReader {
        fn samples_per_pixel(&self) -> u32 {
            self.spp
        }

        fn read_step_into(
            &self,
            rect: Rect,
            _step: NonZeroU32,
            out: &mut [u16],
        ) -> Result<(), PaneReadError> {
            self.log.lock().unwrap().push(Served {
                rect,
                started: Instant::now(),
                ended: None,
            });
            self.gate
                .lock()
                .unwrap()
                .recv()
                .expect("the test keeps the release sender alive");
            out.fill(rect.width as u16);
            self.log.lock().unwrap().last_mut().unwrap().ended = Some(Instant::now());
            Ok(())
        }
    }

    /// A one-job request whose source width is `width` and whose stamp is
    /// `stamp`, so a completion says which submit it came from.
    pub fn job(reader: &Arc<FakeReader>, width: u32, stamp: u64) -> PaneJob {
        job_in(Pane::Frame, reader, width, stamp)
    }

    /// [`job`] for `pane`.
    pub fn job_in(pane: Pane, reader: &Arc<FakeReader>, width: u32, stamp: u64) -> PaneJob {
        PaneJob {
            pane,
            draw: PaneDraw {
                src: (0, 0, width, 8),
                dst: (0.0, 0.0, width as f32, 8.0),
                crop: (0.0, 0.0, width as f32, 8.0),
                pixelated: false,
                step: NonZeroU32::MIN,
            },
            stamp,
            reader: reader.clone(),
            indices: None,
            state: Default::default(),
            backdrop: false,
        }
    }

    pub fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Polls `take` until a completion carrying `stamp` shows up; older
    /// completions taken on the way go back to the pool.
    pub fn take_stamp(preparer: &Preparer, stamp: u64) -> Vec<Prepared> {
        let mut found = None;
        wait_for("a completion", || {
            let Some(done) = preparer.take() else {
                return false;
            };
            if done.iter().any(|p| p.job.stamp == stamp) {
                found = Some(done);
                true
            } else {
                preparer.recycle(done);
                false
            }
        });
        found.unwrap()
    }

    pub fn counter() -> (Arc<AtomicUsize>, impl Fn() + Send + Sync + 'static) {
        let count = Arc::new(AtomicUsize::new(0));
        let wake = {
            let count = count.clone();
            move || {
                count.fetch_add(1, Ordering::SeqCst);
            }
        };
        (count, wake)
    }
}

// ---------------------------------------------------------------------
// The edited view: resident rendered tiles over the sealed planes.
// ---------------------------------------------------------------------

use std::num::NonZeroU32;

use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_gui::canvas::{ResidentTiles, overlay_edited};

const MAP: u32 = 64;
const FRAMES: usize = 3;

fn mantis64_stack() -> TiffStack {
    let paths: Vec<PathBuf> = (0..FRAMES)
        .map(|i| common::fixture_frame("mantis-64", i))
        .collect();
    TiffStack::open_files(&paths).expect("the mantis-64 frames open")
}

fn whole_map() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: MAP,
        height: MAP,
    }
}

/// Every frame's whole plane, for per-pixel reference lookups.
fn frame_planes(stack: &TiffStack) -> Vec<Vec<u16>> {
    (0..FRAMES)
        .map(|frame| {
            let mut plane = vec![0u16; (MAP * MAP * 3) as usize];
            stack
                .read_tile_into(frame, whole_map(), &mut plane)
                .unwrap();
            plane
        })
        .collect()
}

/// `region`'s rows of the whole-map `plane`, three samples a pixel.
fn region_samples(plane: &[u16], region: Rect) -> Vec<u16> {
    let mut out = Vec::new();
    for y in region.y..region.y + region.height {
        let start = ((y * MAP + region.x) * 3) as usize;
        out.extend_from_slice(&plane[start..start + region.width as usize * 3]);
    }
    out
}

fn contains(r: Rect, x: u32, y: u32) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

#[test]
fn resident_tiles_allocate_only_the_tiles_a_region_meets() {
    let stack = mantis64_stack();
    let planes = frame_planes(&stack);
    // A 13 px grid: the region below spans tiles (0..=1, 1..=2).
    let mut tiles = ResidentTiles::new(13, MAP, MAP, 3);
    let region = Rect {
        x: 10,
        y: 20,
        width: 10,
        height: 10,
    };
    let generation = tiles.generation();
    tiles.install(region, &region_samples(&planes[1], region));
    assert_eq!(tiles.generation(), generation + 1, "one bump per call");
    assert_eq!(tiles.take_rendered_regions(), vec![region]);
    assert!(tiles.take_rendered_regions().is_empty(), "the log drains");

    // The overlay shows each texel's resident pixel wherever one was
    // installed: the region's, and the sealed sentinel everywhere else,
    // including the rest of the four tiles the region met.
    const SENTINEL: u16 = 7777;
    let mut samples = vec![SENTINEL; (MAP * MAP * 3) as usize];
    overlay_edited(&mut samples, whole_map(), NonZeroU32::MIN, 3, &tiles);
    for y in 0..MAP {
        for x in 0..MAP {
            let i = ((y * MAP + x) * 3) as usize;
            let expected: &[u16] = if contains(region, x, y) {
                &planes[1][i..i + 3]
            } else {
                &[SENTINEL; 3]
            };
            assert_eq!(&samples[i..i + 3], expected, "({x}, {y})");
        }
    }
}

#[test]
fn a_pending_region_never_shows_unfilled_tile_samples() {
    let stack = mantis64_stack();
    let planes = frame_planes(&stack);
    // A 13 px grid: `first` spans tiles (0..=1, 1..=2); `pending` reaches
    // untouched texels of tiles (1, 1) and (1, 2) and the fresh tiles (2, 1)
    // and (2, 2).
    let mut tiles = ResidentTiles::new(13, MAP, MAP, 3);
    let first = Rect {
        x: 10,
        y: 20,
        width: 10,
        height: 10,
    };
    let pending = Rect {
        x: 18,
        y: 24,
        width: 14,
        height: 10,
    };
    tiles.install(first, &region_samples(&planes[1], first));
    let generation = tiles.generation();
    tiles.touch();
    assert_eq!(
        tiles.generation(),
        generation + 1,
        "touch is the renderer's cue"
    );

    const SENTINEL: u16 = 7777;
    let shown = |step: u32, tiles: &ResidentTiles| {
        let step = NonZeroU32::new(step).unwrap();
        let out_w = MAP.div_ceil(step.get());
        let texels = (out_w * out_w) as usize;
        let mut samples = vec![SENTINEL; texels * 3];
        overlay_edited(&mut samples, whole_map(), step, 3, tiles);
        (step.get(), out_w, samples)
    };
    let check = |step: u32, tiles: &ResidentTiles, expected: &dyn Fn(u32, u32) -> Vec<u16>| {
        let (step, out_w, samples) = shown(step, tiles);
        for ty in 0..out_w {
            for tx in 0..out_w {
                let (x, y) = (tx * step, ty * step);
                let t = ((ty * out_w + tx) * 3) as usize;
                assert_eq!(
                    &samples[t..t + 3],
                    &expected(x, y)[..],
                    "step {step} ({x}, {y})"
                );
            }
        }
    };
    let plane_pixel = |frame: usize, x: u32, y: u32| {
        let i = ((y * MAP + x) * 3) as usize;
        planes[frame][i..i + 3].to_vec()
    };

    let before = |x: u32, y: u32| {
        if contains(first, x, y) {
            plane_pixel(1, x, y)
        } else {
            vec![SENTINEL; 3]
        }
    };
    for step in [1, 2] {
        check(step, &tiles, &before);
    }

    tiles.install(pending, &region_samples(&planes[2], pending));
    let after = |x: u32, y: u32| {
        if contains(pending, x, y) {
            plane_pixel(2, x, y)
        } else {
            before(x, y)
        }
    };
    for step in [1, 2] {
        check(step, &tiles, &after);
    }
}

#[test]
fn preparer_keeps_one_active_and_the_newest_pending_request() {
    use preparing::*;
    let (reader, release) = FakeReader::new(1);
    let preparer = focusforge_gui::canvas::Preparer::new(|| {});

    preparer.submit(vec![job(&reader, 1, 1)]);
    wait_for("the first read to start", || reader.reads() == 1);

    let started = std::time::Instant::now();
    for width in 2..=100 {
        preparer.submit(vec![job(&reader, width, u64::from(width))]);
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "submitting while a read is blocked returns at once"
    );

    release.send(()).unwrap();
    release.send(()).unwrap();
    let done = take_stamp(&preparer, 100);

    assert_eq!(reader.reads(), 2, "one active read plus the newest pending");
    assert_eq!(done.len(), 1);
    assert_eq!(
        done[0].job.draw.src.2, 100,
        "the second read is the last submit"
    );
    assert_eq!(reader.log.lock().unwrap()[1].rect.width, 100);
}

#[test]
fn preparer_wakes_once_per_unconsumed_completion() {
    use preparing::*;
    let (reader, release) = FakeReader::new(1);
    for _ in 0..3 {
        release.send(()).unwrap();
    }
    let (wakes, wake) = counter();
    let preparer = focusforge_gui::canvas::Preparer::new(wake);

    preparer.submit(vec![job(&reader, 1, 1)]);
    wait_for("the first wake", || {
        wakes.load(std::sync::atomic::Ordering::SeqCst) == 1
    });
    preparer.submit(vec![job(&reader, 2, 2)]);
    wait_for("the second read", || reader.finished() == 2);

    let done = take_stamp(&preparer, 2);
    assert_eq!(done.len(), 1, "take yields only the newest completion");
    assert_eq!(
        wakes.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a completion landing on a full slot wakes nobody"
    );
    assert!(preparer.take().is_none(), "the slot is empty after take");

    preparer.submit(vec![job(&reader, 3, 3)]);
    wait_for("the wake for an emptied slot", || {
        wakes.load(std::sync::atomic::Ordering::SeqCst) == 2
    });
    take_stamp(&preparer, 3);
}

/// Timing accounting is process-global, so this runs alone in a child.
#[test]
fn preparer_reuses_its_buffers_across_a_burst() {
    use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
    use preparing::*;

    const CHILD: &str = "FOCUSFORGE_GUI_PREPARER_BUFFER_ACCOUNTING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "--exact",
            "preparer_reuses_its_buffers_across_a_burst",
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
    let sample_bytes = || {
        timing_snapshot()
            .expect("trace accounting is enabled")
            .sample_bytes
    };
    let preparer = focusforge_gui::canvas::Preparer::new(|| {});
    let mut stamp = 0;
    for spp in [3, 1] {
        let (reader, release) = FakeReader::new(spp);
        for _ in 0..200 {
            release.send(()).unwrap();
        }
        let request = |stamp: &mut u64, width: u32| {
            *stamp += 1;
            preparer.submit(vec![
                job(&reader, width, *stamp),
                job(&reader, width, *stamp),
            ]);
        };
        let settle = |stamp: u64| preparer.recycle(take_stamp(&preparer, stamp));

        // Warm-up reaches the full pipeline depth: a completion parked in
        // the slot while a second one replaces it.
        for round in 1..=2 {
            request(&mut stamp, 64);
            wait_for("the warm-up reads", || reader.finished() == 2 * round);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        settle(stamp);
        let warm = sample_bytes();

        for _ in 0..20 {
            request(&mut stamp, 64);
            settle(stamp);
        }
        for _ in 0..20 {
            request(&mut stamp, 64);
        }
        settle(stamp);
        assert_eq!(
            sample_bytes() - warm,
            0,
            "spp {spp}: a burst of same-extent requests allocates no sample bytes"
        );
    }
}

#[test]
fn preparer_starts_the_newest_request_without_delay() {
    use preparing::*;
    use std::time::{Duration, Instant};

    let (reader, release) = FakeReader::new(1);
    let preparer = focusforge_gui::canvas::Preparer::new(|| {});

    let submitted = Instant::now();
    preparer.submit(vec![job(&reader, 1, 1)]);
    wait_for("the idle read to start", || reader.reads() == 1);
    let idle_start = reader.log.lock().unwrap()[0].started;
    assert!(
        idle_start - submitted < Duration::from_millis(50),
        "an idle worker starts at once: {:?}",
        idle_start - submitted
    );

    preparer.submit(vec![job(&reader, 2, 2)]);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(reader.reads(), 1, "the busy worker starts nothing else");
    release.send(()).unwrap();
    release.send(()).unwrap();
    wait_for("the newest read to finish", || reader.finished() == 2);

    let log = reader.log.lock().unwrap();
    let first_end = log[0].ended.expect("the first read finished");
    let gap = log[1].started.saturating_duration_since(first_end);
    assert!(
        log[1].started >= first_end && gap < Duration::from_millis(50),
        "the newest read follows the active one's end: {gap:?}"
    );
}

#[test]
fn preparer_dropped_during_a_slow_read_wakes_nobody() {
    use preparing::*;
    let (reader, release) = FakeReader::new(1);
    let (wakes, wake) = counter();
    let preparer = focusforge_gui::canvas::Preparer::new(wake);

    preparer.submit(vec![job(&reader, 1, 1)]);
    wait_for("the read to start", || reader.reads() == 1);
    drop(preparer);
    release.send(()).unwrap();
    wait_for("the read to finish", || reader.finished() == 1);
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn a_plane_read_is_delivered_while_a_frame_read_is_blocked() {
    use focusforge_gui::canvas::Pane;
    use preparing::*;
    let (frame, release_frame) = FakeReader::new(1);
    let (plane, release_plane) = FakeReader::new(1);
    release_plane.send(()).unwrap();
    let preparer = focusforge_gui::canvas::Preparer::new(|| {});

    preparer.submit(vec![
        job_in(Pane::Frame, &frame, 8, 1),
        job_in(Pane::Composite, &plane, 8, 2),
    ]);
    let done = take_stamp(&preparer, 2);
    assert_eq!(
        frame.finished(),
        0,
        "the composite arrives while the frame read is still blocked"
    );
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].job.pane, Pane::Composite);

    release_frame.send(()).unwrap();
    let late = take_stamp(&preparer, 1);
    assert_eq!(late.len(), 1, "the frame lands on its own once released");
    assert_eq!(late[0].job.pane, Pane::Frame);
}

#[test]
fn stale_stamps_are_refused_in_any_arrival_order() {
    use focusforge_gui::canvas::StampFloor;

    let accepted = |floor: &mut StampFloor, stamps: &[u64]| -> Vec<u64> {
        stamps
            .iter()
            .copied()
            .filter(|&stamp| floor.accept(stamp))
            .collect()
    };
    assert_eq!(accepted(&mut StampFloor::default(), &[1, 2, 3]), [1, 2, 3]);
    assert_eq!(accepted(&mut StampFloor::default(), &[3, 2, 1]), [3]);
    assert_eq!(
        accepted(&mut StampFloor::default(), &[2, 1, 4, 3, 5, 5]),
        [2, 4, 5]
    );

    let mut floor = StampFloor::default();
    assert_eq!(accepted(&mut floor, &[1, 2]), [1, 2]);
    floor.reset(10);
    assert_eq!(
        accepted(&mut floor, &[9, 10, 11, 8, 12]),
        [11, 12],
        "work issued before a reset is refused, whatever its order"
    );
}

#[test]
fn prepared_samples_match_a_direct_read() {
    use focusforge_core::io::TiffStack;
    use focusforge_core::tile::Rect;
    use focusforge_gui::canvas::{Pane, PaneJob, PaneReader, Preparer};
    use focusforge_gui::viewport::PaneDraw;
    use preparing::take_stamp;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    let (composite_path, _) = golden_plane_files();
    let frame_path = common::fixture_frame("mantis-64", 0);
    let frame = Arc::new(TiffStack::open_files(&[frame_path]).expect("frame opens"));
    let composite = Arc::new(TiffStack::open_files(&[composite_path]).expect("composite opens"));

    let preparer = Preparer::new(|| {});
    let mut stamp = 0;
    let mut check = |pane: Pane,
                     reader: Arc<dyn PaneReader>,
                     size: (u32, u32),
                     direct: &dyn Fn(Rect, NonZeroU32, &mut [u16])| {
        let odd = |extent: u32| (extent - 4) | 1;
        let rects = [(1, 3, odd(size.0), odd(size.1)), (0, 0, size.0, size.1)];
        for (x, y, width, height) in rects {
            for step in 1..=3 {
                let step = NonZeroU32::new(step).unwrap();
                let draw = PaneDraw {
                    src: (x, y, width, height),
                    dst: (0.0, 0.0, width as f32, height as f32),
                    crop: (0.0, 0.0, width as f32, height as f32),
                    pixelated: false,
                    step,
                };
                let (tex_w, tex_h) = draw.extent();
                let mut expected =
                    vec![
                        0u16;
                        tex_w as usize * tex_h as usize * reader.samples_per_pixel() as usize
                    ];
                let rect = Rect {
                    x,
                    y,
                    width,
                    height,
                };
                direct(rect, step, &mut expected);
                stamp += 1;
                preparer.submit(vec![PaneJob {
                    pane,
                    draw,
                    stamp,
                    reader: reader.clone(),
                    indices: None,
                    state: Default::default(),
                    backdrop: false,
                }]);
                let done = take_stamp(&preparer, stamp);
                assert_eq!(
                    done[0].samples, expected,
                    "{pane:?} {:?} step {step}: worker samples equal a direct read",
                    draw.src
                );
                preparer.recycle(done);
            }
        }
    };

    check(
        Pane::Frame,
        frame.clone(),
        frame.dimensions(),
        &|rect, step, out| frame.read_tile_step_into(0, rect, step, out).unwrap(),
    );
    let composite_size = composite.dimensions();
    check(
        Pane::Composite,
        composite.clone(),
        composite_size,
        &|rect, step, out| composite.read_tile_step_into(0, rect, step, out).unwrap(),
    );
}

/// A job requested before a source switch, close, device
/// change or a newer publish renders and uploads nothing. Upload accounting
/// is process-global, so this runs alone in a child.
#[test]
fn stale_results_render_nothing() {
    use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
    use focusforge_gui::canvas::PaneJob;
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use focusforge_gui::viewport::PaneDraw;
    use std::num::NonZeroU32;

    const CHILD: &str = "FOCUSFORGE_GUI_STALE_RESULTS_ACCOUNTING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "--exact",
            "stale_results_render_nothing",
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
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite_path, zmap_path) = golden_plane_files();
    let frame_path = common::fixture_frame("mantis-64", 0);
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .unwrap();
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();

    let draw = |x: u32| PaneDraw {
        src: (x, 5, 37, 23),
        dst: (0.0, 0.0, 37.0, 23.0),
        crop: (0.0, 0.0, 37.0, 23.0),
        pixelated: false,
        step: NonZeroU32::new(2).unwrap(),
    };
    let uploaded = || {
        timing_snapshot()
            .expect("trace accounting is enabled")
            .upload_bytes
    };
    let samples = |job: &PaneJob| {
        let (width, height) = job.draw.extent();
        let mut samples =
            vec![0u16; width as usize * height as usize * job.reader.samples_per_pixel() as usize];
        let (x, y, w, h) = job.draw.src;
        let rect = focusforge_core::tile::Rect {
            x,
            y,
            width: w,
            height: h,
        };
        job.reader
            .read_step_into(rect, job.draw.step, &mut samples)
            .expect("the fixture reads");
        samples
    };
    let refused = |renderer: &mut CanvasRenderer, job: &PaneJob, label: &str| {
        let before = uploaded();
        assert!(
            renderer.publish(job, &samples(job)).is_none(),
            "{label}: the stale job is refused"
        );
        assert_eq!(uploaded(), before, "{label}: nothing is uploaded");
    };
    let accepted = |renderer: &mut CanvasRenderer, job: &PaneJob, label: &str| {
        let before = uploaded();
        assert!(
            renderer.publish(job, &samples(job)).is_some(),
            "{label}: the fresh job is accepted"
        );
        assert!(uploaded() > before, "{label}: the accepted job uploads");
    };

    // Older than a published view.
    let older = renderer
        .request_planes(&draw(4), None, None)
        .expect("plane job");
    let newer = renderer
        .request_planes(&draw(200), None, None)
        .expect("plane job outside retained coverage");
    accepted(&mut renderer, &newer, "newer frame view");
    refused(&mut renderer, &older, "older frame view");

    // Requesting a view the pane already shows retires work in flight.
    let in_flight = renderer
        .request_planes(&draw(350), None, None)
        .expect("plane job outside retained coverage");
    assert!(
        renderer.request_planes(&draw(200), None, None).is_none(),
        "already shown"
    );
    refused(
        &mut renderer,
        &in_flight,
        "view retired by a covered request",
    );

    // A source switch.
    let before_frame_switch = renderer.request_frame(&draw(7)).expect("frame job");
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();
    refused(&mut renderer, &before_frame_switch, "set_frame");
    let before_frame_close = renderer.request_frame(&draw(7)).expect("frame job");
    renderer.clear_frame();
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();
    refused(&mut renderer, &before_frame_close, "clear_frame");

    let before_planes = renderer
        .request_planes(&draw(7), None, None)
        .expect("plane job");
    let (other_composite, other_zmap) = copied_planes("stale_plane_jobs_other_planes");
    renderer
        .set_planes(&other_composite, Some(&other_zmap))
        .unwrap();
    refused(&mut renderer, &before_planes, "set_planes");
    let before_clear = renderer
        .request_planes(&draw(7), None, None)
        .expect("plane job");
    renderer.clear_planes();
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .unwrap();
    refused(&mut renderer, &before_clear, "clear_planes");

    // A device change.
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .unwrap();
    let before_device = renderer
        .request_planes(&draw(7), None, None)
        .expect("plane job after reset");
    renderer.detach_device();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    refused(&mut renderer, &before_device, "detach and attach");
}

// ---------------------------------------------------------------------
// The backdrop: the whole image at a coarse step beneath a pane's exact view.
// ---------------------------------------------------------------------

/// The samples a worker would read for `job`.
fn job_samples(job: &focusforge_gui::canvas::PaneJob) -> Vec<u16> {
    let (width, height) = job.draw.extent();
    let mut samples =
        vec![0u16; width as usize * height as usize * job.reader.samples_per_pixel() as usize];
    let (x, y, w, h) = job.draw.src;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    job.reader
        .read_step_into(rect, job.draw.step, &mut samples)
        .expect("the fixture reads");
    samples
}

fn backdrop_of(width: u32, height: u32, pane: f32) -> focusforge_gui::viewport::PaneDraw {
    focusforge_gui::viewport::backdrop_draw(
        focusforge_gui::viewport::ImageSize { width, height },
        PaneRect {
            x: 0.0,
            y: 0.0,
            width: pane,
            height: pane,
            scale_factor: 1.0,
        },
    )
    .expect("a non-empty image and pane draw a backdrop")
}

/// The backdrop queue keeps one job per pane: a later submit for another
/// pane leaves a pending job alone, and one for the same pane replaces it.
/// Per pane and per content change the renderer issues one job: not again
/// while it is in flight or published, and not for any burst of pan or zoom
/// reads.
#[test]
fn backdrop_is_read_once_per_content_change() {
    use focusforge_gui::canvas::{OverlayDraw, Pane, Preparer};
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use preparing::*;

    // Worker queue, no device needed.
    let (reader, release) = FakeReader::new(1);
    let preparer = Preparer::new(|| {});
    preparer.submit(vec![job(&reader, 1, 1)]);
    wait_for("the blocking read to start", || reader.reads() == 1);
    let backdrop = |pane: Pane, stamp: u64| {
        let mut queued = job(&reader, 8 + stamp as u32, stamp);
        queued.pane = pane;
        queued.backdrop = true;
        queued
    };
    preparer.submit_backdrop(vec![backdrop(Pane::Frame, 10)]);
    preparer.submit_backdrop(vec![backdrop(Pane::Frame, 11)]);
    preparer.submit_backdrop(vec![backdrop(Pane::Composite, 12)]);
    for _ in 0..3 {
        release.send(()).unwrap();
    }
    let mut stamps = Vec::new();
    wait_for("both panes' backdrops", || {
        if let Some(done) = preparer.take() {
            stamps.extend(done.iter().map(|p| (p.job.stamp, p.job.backdrop)));
            preparer.recycle(done);
        }
        stamps.iter().filter(|(_, backdrop)| *backdrop).count() == 2
    });
    stamps.sort();
    assert_eq!(
        stamps.into_iter().filter(|(_, b)| *b).collect::<Vec<_>>(),
        [(11, true), (12, true)],
        "the newer frame job replaced the older; the composite job kept the frame's"
    );
    assert_eq!(reader.reads(), 3, "the blocker plus one read per pane");

    // Renderer job issue.
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite_path, zmap_path) = golden_plane_files();
    let frame_path = common::fixture_frame("mantis-64", 0);
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .unwrap();
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();
    let frame_back = backdrop_of(64, 64, 32.0);
    let plane_back = backdrop_of(512, 512, 128.0);
    let view = |x: u32| focusforge_gui::viewport::PaneDraw {
        src: (x, 5, 37, 23),
        dst: (0.0, 0.0, 37.0, 23.0),
        crop: (0.0, 0.0, 37.0, 23.0),
        pixelated: false,
        step: NonZeroU32::new(2).unwrap(),
    };

    let first = renderer
        .request_frame_backdrop(&frame_back)
        .expect("the first frame backdrop job");
    assert!(
        renderer.request_frame_backdrop(&frame_back).is_none(),
        "no second job while one is in flight"
    );
    assert!(renderer.publish(&first, &job_samples(&first)).is_some());
    for x in 0..20 {
        assert!(renderer.request_frame(&view(x)).is_some(), "exact view {x}");
        assert!(
            renderer.request_frame_backdrop(&frame_back).is_none(),
            "a pan or zoom repaint issues no frame backdrop job"
        );
    }

    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();
    let switched = renderer
        .request_frame_backdrop(&frame_back)
        .expect("a frame switch issues one job");
    assert!(renderer.request_frame_backdrop(&frame_back).is_none());
    assert!(
        renderer
            .publish(&switched, &job_samples(&switched))
            .is_some()
    );
    assert!(renderer.request_frame_backdrop(&frame_back).is_none());

    let plane = |renderer: &mut CanvasRenderer,
                 generation: Option<u64>,
                 overlay: Option<&OverlayDraw>,
                 label: &str| {
        let issued = renderer
            .request_plane_backdrop(&plane_back, generation, overlay)
            .unwrap_or_else(|| panic!("{label}: one new composite backdrop job"));
        assert!(
            renderer
                .request_plane_backdrop(&plane_back, generation, overlay)
                .is_none(),
            "{label}: not again while in flight"
        );
        assert!(
            renderer.publish(&issued, &job_samples(&issued)).is_some(),
            "{label}: the job publishes"
        );
        assert!(
            renderer
                .request_plane_backdrop(&plane_back, generation, overlay)
                .is_none(),
            "{label}: not again once published"
        );
    };
    plane(&mut renderer, None, None, "first");
    for x in 0..20 {
        assert!(
            renderer.request_planes(&view(x), None, None).is_some(),
            "exact plane view {x}"
        );
        assert!(
            renderer
                .request_plane_backdrop(&plane_back, None, None)
                .is_none(),
            "a pan or zoom repaint issues no composite backdrop job"
        );
    }
    plane(&mut renderer, Some(1), None, "an edit generation");
    plane(&mut renderer, Some(2), None, "the next edit generation");
    let overlay = OverlayDraw::for_opacity(8, 0.5);
    plane(&mut renderer, Some(2), Some(&overlay), "an overlay toggle");
}

/// The composite job of `renderer` for `generation` and `overlay`, exact or
/// backdrop, issued over freshly set planes so no earlier publish covers it.
fn composite_job(
    renderer: &mut focusforge_gui::gpu_canvas::CanvasRenderer,
    planes: (&Path, Option<&Path>),
    generation: Option<u64>,
    overlay: Option<&focusforge_gui::canvas::OverlayDraw>,
    backdrop: bool,
) -> focusforge_gui::canvas::PaneJob {
    let exact = focusforge_gui::viewport::PaneDraw {
        src: (0, 0, 64, 64),
        dst: (0.0, 0.0, 64.0, 64.0),
        crop: (0.0, 0.0, 64.0, 64.0),
        pixelated: false,
        step: NonZeroU32::MIN,
    };
    renderer
        .set_planes(planes.0, planes.1)
        .expect("the composite opens whatever the Z-Map is");
    let job = if backdrop {
        renderer.request_plane_backdrop(&backdrop_of(512, 512, 128.0), generation, overlay)
    } else {
        renderer.request_planes(&exact, generation, overlay)
    };
    job.unwrap_or_else(|| {
        panic!("a composite job issues (backdrop {backdrop}, generation {generation:?})")
    })
}

/// The composite pane draws without the Z-Map: absent, or a file that fails
/// to open, its jobs issue and publish exact and backdrop, with and without
/// an edit generation, and with the overlay requested, and none carries an
/// index reader.
#[test]
fn display_ignores_the_zmap() {
    use focusforge_gui::canvas::EditedView;
    use focusforge_gui::gpu_canvas::CanvasRenderer;

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite, _) = golden_plane_files();
    let garbage = common::tree("display_ignores_the_zmap").join("zmap.tif");
    fs::write(&garbage, b"not a Z-Map").unwrap();
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    let tiles = ResidentTiles::new(16, 512, 512, 3);
    let view = EditedView { tiles: &tiles };
    let overlay = focusforge_gui::canvas::OverlayDraw::for_opacity(8, 0.5);

    for zmap in [None, Some(garbage.as_path())] {
        for generation in [None, Some(tiles.generation())] {
            for overlay in [None, Some(&overlay)] {
                let label = format!("zmap {zmap:?}, generation {generation:?}");
                let edited = generation.map(|_| &view);
                for backdrop in [false, true] {
                    let planes = (composite.as_path(), zmap);
                    let job = composite_job(&mut renderer, planes, generation, overlay, backdrop);
                    assert!(job.indices.is_none(), "{label}: no index reader");
                    let published = renderer.publish_edited(&job, &job_samples(&job), &[], edited);
                    assert!(
                        published.is_some(),
                        "{label}: the job publishes (backdrop {backdrop})"
                    );
                }
            }
        }
    }
}

/// Re-setting the planes already open keeps a composite job issued before it
/// valid; planes rewritten under the same paths refuse it.
#[test]
fn set_planes_on_the_open_planes_keeps_an_in_flight_job() {
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use std::io::Write;

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite, zmap) = copied_planes("set_planes_on_the_open_planes");
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    let job = composite_job(&mut renderer, (&composite, Some(&zmap)), None, None, false);

    renderer.set_planes(&composite, Some(&zmap)).unwrap();
    assert!(
        renderer
            .publish_edited(&job, &job_samples(&job), &[], None)
            .is_some(),
        "the same planes leave the in-flight composite valid"
    );

    let wider = focusforge_gui::viewport::PaneDraw {
        src: (0, 0, 200, 200),
        dst: (0.0, 0.0, 200.0, 200.0),
        crop: (0.0, 0.0, 200.0, 200.0),
        pixelated: false,
        step: NonZeroU32::MIN,
    };
    let job = renderer
        .request_planes(&wider, None, None)
        .expect("a composite job issues");
    fs::OpenOptions::new()
        .append(true)
        .open(&composite)
        .unwrap()
        .write_all(&[0])
        .unwrap();
    renderer.set_planes(&composite, Some(&zmap)).unwrap();
    assert!(
        renderer
            .publish_edited(&job, &job_samples(&job), &[], None)
            .is_none(),
        "a composite rewritten in place refuses the job issued before it"
    );
}

/// The index reader rides a composite job iff the overlay is on: an edit
/// generation, the exact view and the backdrop alone never attach it.
#[test]
fn zmap_is_read_lazily() {
    use focusforge_gui::gpu_canvas::CanvasRenderer;

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite, zmap) = golden_plane_files();
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    let overlay = focusforge_gui::canvas::OverlayDraw::for_opacity(8, 0.5);

    for generation in [None, Some(1)] {
        for overlay in [None, Some(&overlay)] {
            for backdrop in [false, true] {
                let planes = (composite.as_path(), Some(zmap.as_path()));
                let job = composite_job(&mut renderer, planes, generation, overlay, backdrop);
                assert_eq!(
                    job.indices.is_some(),
                    overlay.is_some(),
                    "generation {generation:?}, overlay {}, backdrop {backdrop}",
                    overlay.is_some()
                );
            }
        }
    }
}

/// A backdrop job issued before a source, frame, device or newer
/// publish renders and uploads nothing, whichever of it and the exact
/// results arrives first. Upload accounting is process-global, so this runs
/// alone in a child.
#[test]
fn backdrop_from_a_previous_content_is_refused() {
    use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
    use focusforge_gui::canvas::PaneJob;
    use focusforge_gui::gpu_canvas::CanvasRenderer;

    const CHILD: &str = "FOCUSFORGE_GUI_STALE_BACKDROP_ACCOUNTING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "--exact",
            "backdrop_from_a_previous_content_is_refused",
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
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (composite_path, zmap_path) = golden_plane_files();
    let frame_path = common::fixture_frame("mantis-64", 0);
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .unwrap();
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();
    let frame_back = backdrop_of(64, 64, 32.0);
    let plane_back = backdrop_of(512, 512, 128.0);
    let exact = focusforge_gui::viewport::PaneDraw {
        src: (4, 5, 37, 23),
        dst: (0.0, 0.0, 37.0, 23.0),
        crop: (0.0, 0.0, 37.0, 23.0),
        pixelated: false,
        step: NonZeroU32::new(2).unwrap(),
    };
    let uploaded = || {
        timing_snapshot()
            .expect("trace accounting is enabled")
            .upload_bytes
    };
    let refused = |renderer: &mut CanvasRenderer, job: &PaneJob, label: &str| {
        let before = uploaded();
        assert!(
            renderer.publish(job, &job_samples(job)).is_none(),
            "{label}: the stale backdrop is refused"
        );
        assert_eq!(uploaded(), before, "{label}: nothing is uploaded");
    };
    let accepted = |renderer: &mut CanvasRenderer, job: &PaneJob, label: &str| {
        let before = uploaded();
        assert!(
            renderer.publish(job, &job_samples(job)).is_some(),
            "{label}: the fresh backdrop is accepted"
        );
        assert!(uploaded() > before, "{label}: the accepted job uploads");
    };

    // Every reset, each with an exact result landing before or after.
    for exact_first in [false, true] {
        let order = if exact_first {
            "exact first"
        } else {
            "exact last"
        };
        let old = renderer.request_frame_backdrop(&frame_back).expect("job");
        renderer
            .set_frame(&frame_path, None, job::host_budget())
            .unwrap();
        let fresh_exact = renderer.request_frame(&exact).expect("exact job");
        if exact_first {
            accepted(&mut renderer, &fresh_exact, "the exact result");
        }
        refused(&mut renderer, &old, &format!("set_frame, {order}"));
        if !exact_first {
            accepted(&mut renderer, &fresh_exact, "the exact result");
        }
    }
    let old = renderer.request_frame_backdrop(&frame_back).expect("job");
    renderer.clear_frame();
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();
    refused(&mut renderer, &old, "clear_frame");

    let old = renderer
        .request_plane_backdrop(&plane_back, None, None)
        .expect("job");
    let (other_composite, other_zmap) = copied_planes("stale_backdrop_jobs_other_planes");
    renderer
        .set_planes(&other_composite, Some(&other_zmap))
        .unwrap();
    refused(&mut renderer, &old, "set_planes");
    let old = renderer
        .request_plane_backdrop(&plane_back, None, None)
        .expect("job");
    renderer.clear_planes();
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .unwrap();
    refused(&mut renderer, &old, "clear_planes");
    let old = renderer
        .request_plane_backdrop(&plane_back, Some(1), None)
        .expect("job");
    renderer.detach_device();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    refused(&mut renderer, &old, "detach and attach");

    // An older backdrop job behind a newer published one.
    let older = renderer.request_frame_backdrop(&frame_back).expect("job");
    let newer = renderer
        .request_frame_backdrop(&backdrop_of(64, 64, 16.0))
        .expect("job");
    accepted(&mut renderer, &newer, "the newer backdrop");
    refused(&mut renderer, &older, "behind a newer backdrop");
}

/// Without a device no backdrop is requested or shown; losing the device
/// drops the published one and a late job; attaching one asks again.
#[test]
fn backdrop_follows_device_detach_and_attach() {
    use focusforge_gui::canvas::Pane;
    use focusforge_gui::gpu_canvas::CanvasRenderer;

    let (composite_path, zmap_path) = golden_plane_files();
    let frame_path = common::fixture_frame("mantis-64", 0);
    let frame_back = backdrop_of(64, 64, 32.0);
    let plane_back = backdrop_of(512, 512, 128.0);

    let mut renderer = CanvasRenderer::new();
    renderer
        .set_planes(&composite_path, Some(&zmap_path))
        .unwrap();
    renderer
        .set_frame(&frame_path, None, job::host_budget())
        .unwrap();
    assert!(renderer.request_frame_backdrop(&frame_back).is_none());
    assert!(
        renderer
            .request_plane_backdrop(&plane_back, None, None)
            .is_none()
    );
    assert!(
        renderer
            .backdrop_placement(Pane::Frame, &frame_back)
            .is_none()
    );

    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    let first = renderer.request_frame_backdrop(&frame_back).expect("job");
    assert!(renderer.publish(&first, &job_samples(&first)).is_some());
    assert!(
        renderer
            .backdrop_placement(Pane::Frame, &frame_back)
            .is_some()
    );
    let plane = renderer
        .request_plane_backdrop(&plane_back, None, None)
        .expect("job");
    assert!(renderer.publish(&plane, &job_samples(&plane)).is_some());
    assert!(
        renderer
            .backdrop_placement(Pane::Composite, &plane_back)
            .is_some()
    );

    let late = renderer
        .request_frame_backdrop(&backdrop_of(64, 64, 16.0))
        .expect("job");
    renderer.detach_device();
    assert!(
        renderer
            .backdrop_placement(Pane::Frame, &frame_back)
            .is_none(),
        "the frame backdrop is gone with the device"
    );
    assert!(
        renderer
            .backdrop_placement(Pane::Composite, &plane_back)
            .is_none(),
        "the composite backdrop is gone with the device"
    );
    assert!(renderer.request_frame_backdrop(&frame_back).is_none());
    assert!(renderer.publish(&late, &job_samples(&late)).is_none());

    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    assert!(
        renderer
            .backdrop_placement(Pane::Frame, &frame_back)
            .is_none(),
        "an attached device starts with no backdrop"
    );
    let again = renderer
        .request_frame_backdrop(&frame_back)
        .expect("the backdrop is requested again");
    assert!(renderer.publish(&again, &job_samples(&again)).is_some());
    assert!(
        renderer
            .request_plane_backdrop(&plane_back, None, None)
            .is_some(),
        "the composite backdrop is requested again"
    );
}

/// The backdrop reads the same samples a direct stepped read of its source
/// does, at the step `backdrop_draw` chose.
#[test]
fn backdrop_samples_match_a_direct_read() {
    use focusforge_gui::canvas::{Pane, PaneJob, PaneReader, Preparer};
    use preparing::take_stamp;
    use std::sync::Arc;

    let (composite_path, _) = golden_plane_files();
    let frame = Arc::new(
        TiffStack::open_files(&[common::fixture_frame("mantis-64", 0)]).expect("frame opens"),
    );
    let composite = Arc::new(TiffStack::open_files(&[composite_path]).expect("composite opens"));

    let (width, height) = (1u32, 40u32);
    let column: Vec<u16> = (0..height).map(|i| (i * 311 % 1009) as u16).collect();
    let column_path = common::tree("backdrop_one_pixel_wide").join("column.tif");
    let mut writer = TiffWriter::create(
        &column_path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: 1,
            description: None,
        },
    )
    .unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            &column,
        )
        .unwrap();
    writer.finish().unwrap();
    let thin = Arc::new(TiffStack::open_files(&[column_path]).expect("column opens"));

    let preparer = Preparer::new(|| {});
    let mut stamp = 0;
    let mut check = |label: &str,
                     pane: Pane,
                     reader: Arc<dyn PaneReader>,
                     size: (u32, u32),
                     direct: &dyn Fn(Rect, NonZeroU32, &mut [u16])| {
        // An image smaller than the pane, then panes it needs coarser reads for.
        for pane_side in [4.0 * size.0.max(size.1) as f32, 16.0, 7.0, 3.0] {
            let draw = focusforge_gui::viewport::backdrop_draw(
                focusforge_gui::viewport::ImageSize {
                    width: size.0,
                    height: size.1,
                },
                PaneRect {
                    x: 0.0,
                    y: 0.0,
                    width: pane_side,
                    height: pane_side,
                    scale_factor: 1.0,
                },
            )
            .expect("a backdrop draw");
            let (tex_w, tex_h) = draw.extent();
            let mut expected =
                vec![0u16; tex_w as usize * tex_h as usize * reader.samples_per_pixel() as usize];
            let (x, y, w, h) = draw.src;
            direct(
                Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
                draw.step,
                &mut expected,
            );
            stamp += 1;
            preparer.submit_backdrop(vec![PaneJob {
                pane,
                draw,
                stamp,
                reader: reader.clone(),
                indices: None,
                state: Default::default(),
                backdrop: true,
            }]);
            let done = take_stamp(&preparer, stamp);
            let got = done
                .iter()
                .find(|p| p.job.stamp == stamp)
                .expect("the backdrop completes");
            assert_eq!(
                got.samples, expected,
                "{label} in a pane of {pane_side}: step {} extent {tex_w}x{tex_h}",
                draw.step
            );
            preparer.recycle(done);
        }
    };

    check(
        "mantis-64 frame",
        Pane::Frame,
        frame.clone(),
        frame.dimensions(),
        &|rect, step, out| frame.read_tile_step_into(0, rect, step, out).unwrap(),
    );
    let size = composite.dimensions();
    check(
        "mantis-512 composite",
        Pane::Composite,
        composite.clone(),
        size,
        &|rect, step, out| composite.read_tile_step_into(0, rect, step, out).unwrap(),
    );
    check(
        "one-pixel-wide image",
        Pane::Frame,
        thin.clone(),
        (width, height),
        &|rect, step, out| thin.read_tile_step_into(0, rect, step, out).unwrap(),
    );
}

/// With a request of each kind pending, the worker reads the exact one
/// first, and a newer exact request submitted over a pending backdrop still
/// runs before it; neither result replaces the other.
#[test]
fn preparer_serves_exact_before_backdrop() {
    use focusforge_gui::canvas::Preparer;
    use preparing::*;

    let widths_read = |reader: &FakeReader| -> Vec<u32> {
        reader
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|served| served.rect.width)
            .collect()
    };
    for newer_exact in [false, true] {
        let (reader, release) = FakeReader::new(1);
        let preparer = Preparer::new(|| {});
        preparer.submit(vec![job(&reader, 1, 1)]);
        wait_for("the blocking read to start", || reader.reads() == 1);

        let mut fill = job(&reader, 22, 2);
        fill.backdrop = true;
        preparer.submit_backdrop(vec![fill]);
        preparer.submit(vec![job(&reader, 33, 3)]);
        let last_exact = if newer_exact {
            preparer.submit(vec![job(&reader, 44, 4)]);
            4
        } else {
            3
        };
        for _ in 0..3 {
            release.send(()).unwrap();
        }
        wait_for("the last read", || reader.finished() == 3);
        let order = widths_read(&reader);
        assert_eq!(
            order,
            [1, 11 * last_exact as u32, 22],
            "the exact read runs before the pending backdrop (newer exact: {newer_exact})"
        );
        let mut done = Vec::new();
        wait_for("both results", || {
            if let Some(batch) = preparer.take() {
                done.extend(batch.iter().map(|p| (p.job.stamp, p.job.backdrop)));
                preparer.recycle(batch);
            }
            done.iter().any(|(_, backdrop)| *backdrop) && done.iter().any(|(_, b)| !*b)
        });
        done.retain(|(stamp, _)| *stamp != 1);
        assert_eq!(
            done,
            [(last_exact, false), (2, true)],
            "exact result first, the backdrop result beside it"
        );
    }
}

/// Exact reads are idle only once nothing is queued, none is running and the
/// result has been taken; a backdrop read does not count.
#[test]
fn preparer_is_exact_idle_only_once_the_exact_read_is_taken() {
    use focusforge_gui::canvas::Preparer;
    use preparing::*;

    let (reader, release) = FakeReader::new(1);
    let preparer = Preparer::new(|| {});
    assert!(preparer.exact_idle(), "a fresh preparer has no exact work");

    preparer.submit(vec![job(&reader, 1, 1)]);
    wait_for("the exact read to start", || reader.reads() == 1);
    assert!(!preparer.exact_idle(), "an exact read is running");

    release.send(()).unwrap();
    wait_for("the exact read to finish", || reader.finished() == 1);
    assert!(!preparer.exact_idle(), "the result is not taken yet");

    take_stamp(&preparer, 1);
    assert!(preparer.exact_idle(), "taken: nothing exact is left");

    let (reader, release) = FakeReader::new(1);
    let preparer = Preparer::new(|| {});
    let mut fill = job(&reader, 22, 2);
    fill.backdrop = true;
    preparer.submit_backdrop(vec![fill]);
    wait_for("the backdrop read to start", || reader.reads() == 1);
    assert!(preparer.exact_idle(), "a backdrop read never holds the cap");
    release.send(()).unwrap();
}

#[test]
fn overlay_keeps_sealed_pixels_where_index_unchanged() {
    let stack = mantis64_stack();
    let planes = frame_planes(&stack);
    let region = Rect {
        x: 9,
        y: 14,
        width: 23,
        height: 19,
    };
    // Inside the region one diagonal band comes from another frame; every
    // other pixel there keeps the first frame's.
    let mut installed = region_samples(&planes[0], region);
    for y in 0..region.height {
        for x in 0..region.width {
            if (region.x + x + region.y + y).is_multiple_of(4) {
                let i = (((region.y + y) * MAP + region.x + x) * 3) as usize;
                let at = ((y * region.width + x) * 3) as usize;
                installed[at..at + 3].copy_from_slice(&planes[1][i..i + 3]);
            }
        }
    }
    let mut tiles = ResidentTiles::new(16, MAP, MAP, 3);
    tiles.install(region, &installed);

    // The sealed composite stands in as a sentinel no frame sample equals,
    // so an installed pixel in its place cannot hide.
    let sealed_sample = |i: usize| 60000 + (i % 1000) as u16;
    for step in [1u32, 2] {
        let step = NonZeroU32::new(step).unwrap();
        let out_w = MAP.div_ceil(step.get());
        let texels = (out_w * out_w) as usize;
        let mut samples: Vec<u16> = (0..texels * 3).map(sealed_sample).collect();
        overlay_edited(&mut samples, whole_map(), step, 3, &tiles);
        for ty in 0..out_w {
            for tx in 0..out_w {
                let (x, y) = (tx * step.get(), ty * step.get());
                let t = (ty * out_w + tx) as usize;
                let expected: Vec<u16> = if contains(region, x, y) {
                    let at = (((y - region.y) * region.width + (x - region.x)) * 3) as usize;
                    installed[at..at + 3].to_vec()
                } else {
                    (t * 3..t * 3 + 3).map(sealed_sample).collect()
                };
                assert_eq!(
                    &samples[t * 3..t * 3 + 3],
                    expected.as_slice(),
                    "step {step} ({x}, {y})"
                );
            }
        }
    }
}

/// `FrameSource` opens an LZW-compressed frame through `open_frame_set`,
/// reading the same samples as its uncompressed original, at step 1 and
/// step 2 -- the editor is not narrowed to the mmap fast path.
#[test]
fn compressed_frame_reads_the_same_samples_as_its_uncompressed_original() {
    let uncompressed = common::fixture_frame("mantis-64", 0);
    let compressed = common::fixture_frame("mantis-64-lzw-p2-strip-ii", 0);

    for step in [
        std::num::NonZeroU32::MIN,
        std::num::NonZeroU32::new(2).unwrap(),
    ] {
        let mut plain = FrameSource::default();
        plain
            .set(&uncompressed, None, job::host_budget())
            .expect("the uncompressed fixture frame must open");
        let mut lzw = FrameSource::default();
        lzw.set(&compressed, None, job::host_budget())
            .expect("the LZW fixture frame must open through open_frame_set");

        let (w, h) = plain
            .dimensions()
            .expect("the uncompressed frame has a size");
        assert_eq!(
            lzw.dimensions(),
            Some((w, h)),
            "step {step}: both readers report the same frame size"
        );
        let spp = plain.samples_per_pixel().expect("samples per pixel");
        let rect = focusforge_core::tile::Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let out_w = w.div_ceil(step.get());
        let out_h = h.div_ceil(step.get());
        let mut plain_out = vec![0u16; out_w as usize * out_h as usize * spp as usize];
        let mut lzw_out = plain_out.clone();
        plain
            .read_into(rect, step, &mut plain_out)
            .expect("the uncompressed frame reads");
        lzw.read_into(rect, step, &mut lzw_out)
            .expect("the LZW frame reads");
        assert_eq!(
            plain_out, lzw_out,
            "step {step}: the LZW frame must read the same samples as its uncompressed original"
        );
    }
}

/// Real pointer events across the Composite pane paint a stroke that settles
/// in under a second, and the click on Undo that follows is handled: the
/// stroke's pointer grab does not outlive its release.
#[test]
fn a_stroke_does_not_swallow_the_next_click() {
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    const BUDGET: Duration = Duration::from_secs(1);

    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree("stroke_next_click_scratch"));
    open_fixture_tab(&mut controller, "stroke_next_click", "mantis-64", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60))
        .expect("the fixture must fuse");
    common::enter_editor(&mut controller);

    i_slint_backend_testing::init_no_event_loop();
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_project_loaded(true);
    app.set_mode("editor".into());
    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        Rc::new(RefCell::new(None)),
    );
    slint::platform::update_timers_and_animations();
    app.invoke_canvas_resized();

    let rect = |id: &str| {
        let handle = ElementQuery::from_root(&app)
            .match_id(id)
            .find_first()
            .unwrap_or_else(|| panic!("no element matched id {id:?}"));
        (handle.absolute_position(), handle.size())
    };
    let event = |event: WindowEvent| {
        app.window().dispatch_event(event);
        slint::platform::update_timers_and_animations();
    };
    let (pane, size) = rect("FFCanvas::pane-right");
    let at = |fx: f32, fy: f32| {
        slint::LogicalPosition::new(pane.x + size.width * fx, pane.y + size.height * fy)
    };

    let started = Instant::now();
    let first = at(0.45, 0.5);
    event(WindowEvent::PointerMoved { position: first });
    event(WindowEvent::PointerPressed {
        position: first,
        button: PointerEventButton::Left,
    });
    for step in 1..=20 {
        event(WindowEvent::PointerMoved {
            position: at(0.45 + 0.01 * step as f32, 0.5),
        });
    }
    let last = at(0.65, 0.5);
    event(WindowEvent::PointerReleased {
        position: last,
        button: PointerEventButton::Left,
    });
    common::settle_region_renders(&mut controller.lock().unwrap());
    let stroke = started.elapsed();
    assert_eq!(
        controller
            .lock()
            .unwrap()
            .edit_session()
            .unwrap()
            .ops()
            .count(),
        1,
        "the drag across the Composite appends one stroke"
    );

    focusforge_gui::bridge::push_editor_toolbar(&app, &controller.lock().unwrap());
    assert!(app.get_can_undo(), "the stroke enables Undo");
    let (undo, undo_size) = rect("FFEditorToolbar::undo");
    let button = slint::LogicalPosition::new(
        undo.x + undo_size.width / 2.0,
        undo.y + undo_size.height / 2.0,
    );
    let started = Instant::now();
    event(WindowEvent::PointerMoved { position: button });
    event(WindowEvent::PointerPressed {
        position: button,
        button: PointerEventButton::Left,
    });
    event(WindowEvent::PointerReleased {
        position: button,
        button: PointerEventButton::Left,
    });
    common::settle_region_renders(&mut controller.lock().unwrap());
    let click = started.elapsed();
    {
        let controller = controller.lock().unwrap();
        let session = controller.edit_session().unwrap();
        assert!(!session.can_undo(), "the click on Undo landed");
        assert!(session.can_redo(), "the undone stroke can be redone");
    }

    println!("stroke across the Composite, press to settled render: {stroke:?}");
    println!("click on Undo, press to settled render: {click:?}");
    assert!(
        stroke < BUDGET,
        "the stroke took {stroke:?}, over {BUDGET:?}"
    );
    assert!(
        click < BUDGET,
        "the Undo click took {click:?}, over {BUDGET:?}"
    );
}

/// A window's scale factor changes with its logical size untouched, as when
/// a window moves to another monitor: the next geometry read carries the new
/// factor into the draw.
#[test]
fn scale_factor_change_without_resize_reaches_the_next_draw() {
    use slint::platform::WindowEvent;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    let mut controller = AppController::new();
    open_fixture_tab(&mut controller, "scale_factor_change", "mantis-512", 3);

    i_slint_backend_testing::init_no_event_loop();
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1200, 760));
    app.set_project_loaded(true);
    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        Rc::new(RefCell::new(None)),
    );
    slint::platform::update_timers_and_animations();
    app.invoke_canvas_resized();
    {
        let mut controller = controller.lock().unwrap();
        controller.fit();
        controller.zoom_out();
    }
    let before = controller
        .lock()
        .unwrap()
        .frame_draw()
        .expect("a loaded project draws");
    assert!(
        before.step.get() >= 2,
        "the zoomed-out view decimates at factor 1 (step {})",
        before.step
    );
    let width = app.get_canvas_pane_left_width();

    app.window()
        .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
    slint::platform::update_timers_and_animations();
    app.invoke_canvas_resized();

    assert_eq!(app.get_canvas_pane_left_width(), width);
    let after = controller.lock().unwrap().frame_draw().unwrap();
    assert_eq!(after.step.get(), (before.step.get() / 2).max(1));
    assert_eq!(after.src, before.src);
}

const RAW_W: u32 = 37;
const RAW_H: u32 = 23;

/// Float values around both ends of the 16-bit range and its rounding points.
const RAW_VALUES: [f32; 13] = [
    -9000.5, -0.25, 0.0, 0.49, 0.5, 1.5, 123.4, 40_000.5, 65_534.5, 65_535.0, 65_535.5, 80_000.0,
    1.0e9,
];

fn raw_samples(seed: usize) -> Vec<f32> {
    (0..(RAW_W * RAW_H * 3) as usize)
        .map(|i| RAW_VALUES[(i * seed + seed) % RAW_VALUES.len()])
        .collect()
}

fn raw_whole() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: RAW_W,
        height: RAW_H,
    }
}

fn write_float_composite(path: &Path, samples: &[f32]) {
    let mut writer = TiffWriter::create_f32(
        path,
        TiffSpec {
            width: RAW_W,
            height: RAW_H,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .unwrap();
    writer.write_tile_f32(raw_whole(), samples).unwrap();
    writer.finish().unwrap();
}

/// `stored` (the whole plane, three samples a pixel) clipped to 16 bits and
/// decimated at `step`.
fn clipped_at(stored: &[f32], step: u32) -> Vec<u16> {
    use focusforge_core::tile::Sample;
    let mut out = Vec::new();
    for y in (0..RAW_H).step_by(step as usize) {
        for x in (0..RAW_W).step_by(step as usize) {
            let at = ((y * RAW_W + x) * 3) as usize;
            out.extend(stored[at..at + 3].iter().map(|&v| u16::narrow(v)));
        }
    }
    out
}

#[test]
fn raw_project_display_clips_once() {
    use focusforge_core::tile::Sample;
    use focusforge_gui::canvas::PaneReader;
    use focusforge_gui::gpu_canvas::CanvasRenderer;
    use focusforge_gui::viewport::PaneDraw;

    let dir = common::tree("raw_project_display_clips_once");
    let path = dir.join("composite.tif");
    let stored = raw_samples(7);
    write_float_composite(&path, &stored);

    // Unedited texels: the composite pane's read narrows the stored floats.
    let stack = TiffStack::open_staged(&path).expect("a float staged composite opens");
    for step in [1u32, 2] {
        let expected = clipped_at(&stored, step);
        let mut shown = vec![0u16; expected.len()];
        stack
            .read_step_into(raw_whole(), NonZeroU32::new(step).unwrap(), &mut shown)
            .unwrap();
        assert_eq!(shown, expected, "step {step}");
    }

    // Edited texels: f32 resident tiles narrow the same way over the composite.
    let region = Rect {
        x: 5,
        y: 3,
        width: 20,
        height: 11,
    };
    let edited = raw_samples(11);
    let installed: Vec<f32> = (region.y..region.y + region.height)
        .flat_map(|y| {
            let start = ((y * RAW_W + region.x) * 3) as usize;
            edited[start..start + region.width as usize * 3].to_vec()
        })
        .collect();
    let mut tiles = ResidentTiles::new(16, RAW_W, RAW_H, 3);
    tiles.install(region, &installed);
    for step in [1u32, 2] {
        let mut shown = clipped_at(&stored, step);
        let nz = NonZeroU32::new(step).unwrap();
        overlay_edited(&mut shown, raw_whole(), nz, 3, &tiles);
        let out_w = RAW_W.div_ceil(step);
        for (t, pixel) in shown.chunks_exact(3).enumerate() {
            let (x, y) = ((t as u32 % out_w) * step, (t as u32 / out_w) * step);
            let plane = if contains(region, x, y) {
                &edited
            } else {
                &stored
            };
            let at = ((y * RAW_W + x) * 3) as usize;
            let expected: Vec<u16> = plane[at..at + 3].iter().map(|&v| u16::narrow(v)).collect();
            assert_eq!(pixel, expected.as_slice(), "step {step} ({x}, {y})");
        }
    }

    // The renderer opens the float file and hands the same narrowed samples
    // to the upload.
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let mut renderer = CanvasRenderer::new();
    renderer.attach_device(shared.device().clone(), shared.queue().clone());
    renderer.set_planes(&path, None).unwrap();
    let draw = PaneDraw {
        src: (0, 0, RAW_W, RAW_H),
        dst: (0.0, 0.0, RAW_W as f32, RAW_H as f32),
        crop: (0.0, 0.0, RAW_W as f32, RAW_H as f32),
        pixelated: true,
        step: NonZeroU32::MIN,
    };
    let job = renderer
        .request_planes(&draw, None, None)
        .expect("the float composite needs a read");
    let mut shown = vec![0u16; (RAW_W * RAW_H * 3) as usize];
    job.reader
        .read_step_into(raw_whole(), job.draw.step, &mut shown)
        .expect("the renderer's reader reads the float composite");
    assert_eq!(shown, clipped_at(&stored, 1));
}
