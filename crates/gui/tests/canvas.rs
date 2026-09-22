// SPDX-License-Identifier: GPL-3.0-or-later

//! The canvas: the display path's no-readback and no-CPU-tonemap
//! invariants (source scans), the display format's refuse-by-name
//! contract, the texture-reuse rule, and both panes' chrome geometry.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use focusforge_gui::AppWindow;
use focusforge_gui::canvas::{
    CanvasFeed, DISPLAY_FORMAT, TextureSpec, draw_rect_fits, needs_realloc, validate_display_format,
};
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
    // `overlay.rs`'s own picture is a one-time 330x186 reduction of a
    // project's stored composite, off the interactive display path this
    // scan protects -- the one file allowed to tone-map on the CPU.
    const EXEMPT: &str = "overlay.rs";
    let forbidden = ["tonemap_cpu", "reduce16to8"];
    for path in common::find_rust_files(&common::src_dir()) {
        if path.file_name().and_then(|n| n.to_str()) == Some(EXEMPT) {
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

/// The reported crash's own numbers: an 8192x5464 viewport rect against a
/// 501x508 composite is refused, an exact-fit and an interior rect are not.
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
        controller.open_paths(std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    root
}

/// A controller with two sealed tabs open -- tab 0 from `fixtures/mantis-64`
/// (64x64), tab 1 from `fixtures/mantis-512` (512x512), the size mismatch
/// the reported crash needs. Tab 1 is active on return, the same order a
/// person opening a small project and then a large one would reach.
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
fn load_path(controller: &mut AppController, loaded: Option<usize>) -> PathBuf {
    match controller.canvas_feed(loaded) {
        CanvasFeed::Load { composite, .. } => composite,
        other => panic!("expected CanvasFeed::Load, got {other:?}"),
    }
}

/// Fuses `background_set`'s own (first-opened) tab while `foreground_set`'s
/// tab is active, asserting the active tab's already-loaded planes survive
/// the background run's completion unchanged -- the reported crash's own
/// sequence, minus the panic that no longer reproduces (asserted
/// separately by `a_read_past_the_composite_draws_nothing_instead_of_panicking`).
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
    assert!(ffp.is_file());

    open_fixture_tab(&mut controller, "saved", "mantis-512", 3);
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120))
        .expect("the second fuse must complete");
    assert!(matches!(
        controller.canvas_feed(None),
        CanvasFeed::Load { tab: 1, .. }
    ));

    // Reopening the sealed `.ffp` mid-session lands a third tab and must
    // load its own planes, not the tab it displaced.
    assert!(matches!(
        controller.open_paths(&[ffp]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.active_project(), Some(2));
    match controller.canvas_feed(Some(1)) {
        CanvasFeed::Load { tab, composite, .. } => {
            assert_eq!(tab, 2);
            assert!(composite.is_file());
        }
        other => {
            panic!("opening a saved project mid-session must load its own planes, got {other:?}")
        }
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
