// SPDX-License-Identifier: GPL-3.0-or-later

//! The tab strip's overflow layout and scroll arithmetic
//! (`tabs_overflow`, `tab_viewport_width`, `clamp_tab_scroll`,
//! `tab_scroll_into_view`) and the arrow click reaching its callback.
//! On overflow the strip reads
//! [left arrow][tabs viewport][right arrow][+] with "+" pinned, and the
//! scroll has hard stops at the first tab's left edge and the last tab's
//! right edge.

use std::cell::Cell;
use std::rc::Rc;

use focusforge_gui::{
    STRIP_BOX_GAP, STRIP_BOX_WIDTH, TAB_PITCH, TAB_WIDTH, clamp_tab_scroll, max_tab_scroll,
    tab_content_width, tab_scroll_into_view, tab_viewport_width, tabs_overflow,
};
use slint::ComponentHandle;
use slint::platform::{PointerEventButton, WindowEvent};

/// `tabs-region` in a strip `window` wide: less the 47 px projects slot,
/// the 105 px window glyphs and the 32 px cluster gap.
fn region(window: f32) -> f32 {
    window - 47.0 - 105.0 - 32.0
}

/// Advances mock time past `Theme.motion-fast` (150 ms), so a rendered
/// position read after a `tab-scroll-x` change is the settled one, not a
/// value mid-way through the scroll animation.
fn settle_scroll_animation() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(200));
}

/// Where tab `index` sits inside the viewport at `offset`: (left, right).
fn tab_edges_in_viewport(index: usize, offset: f32) -> (f32, f32) {
    let left = index as f32 * TAB_PITCH - offset;
    (left, left + TAB_WIDTH)
}

#[test]
fn content_ends_at_the_last_tabs_right_edge() {
    assert_eq!(tab_content_width(0), 0.0);
    assert_eq!(tab_content_width(1), TAB_WIDTH);
    assert_eq!(tab_content_width(10), 9.0 * TAB_PITCH + TAB_WIDTH);
}

#[test]
fn overflow_is_decided_against_the_no_arrow_layout() {
    // Tabs plus "+" exactly filling the region still fit.
    let exact = 5.0 * TAB_PITCH + STRIP_BOX_WIDTH;
    assert!(!tabs_overflow(5, exact));
    assert_eq!(tab_viewport_width(5, exact), exact);
    assert!(tabs_overflow(5, exact - 1.0));
    // One pixel past the threshold, the overflow layout still has room to
    // scroll -- no state in which arrows show over nothing to scroll.
    let viewport = tab_viewport_width(5, exact - 1.0);
    assert_eq!(
        viewport,
        exact - 1.0 - 3.0 * (STRIP_BOX_WIDTH + STRIP_BOX_GAP)
    );
    assert!(max_tab_scroll(tab_content_width(5), viewport) > 0.0);
}

#[test]
fn no_overflow_pins_the_offset_to_zero() {
    let r = region(1440.0);
    assert!(!tabs_overflow(3, r));
    let viewport = tab_viewport_width(3, r);
    assert_eq!(max_tab_scroll(tab_content_width(3), viewport), 0.0);
    assert_eq!(clamp_tab_scroll(500.0, tab_content_width(3), viewport), 0.0);
}

#[test]
fn min_stop_puts_the_first_tab_flush_left() {
    let viewport = tab_viewport_width(10, region(1440.0));
    let s = clamp_tab_scroll(-228.0, tab_content_width(10), viewport);
    assert_eq!(s, 0.0);
    assert_eq!(tab_edges_in_viewport(0, s).0, 0.0);
}

#[test]
fn max_stop_puts_the_last_tab_flush_right() {
    for window in [900.0, 1200.0, 1440.0, 1600.0, 1917.0] {
        let viewport = tab_viewport_width(10, region(window));
        let s = clamp_tab_scroll(f32::MAX, tab_content_width(10), viewport);
        assert_eq!(s, tab_content_width(10) - viewport, "window {window}");
        assert_eq!(tab_edges_in_viewport(9, s).1, viewport, "window {window}");
    }
}

#[test]
fn arrow_steps_are_one_pitch_but_stop_at_the_ends() {
    let viewport = tab_viewport_width(10, region(1440.0));
    let content = tab_content_width(10);
    let max = max_tab_scroll(content, viewport);
    let mut s = 0.0;
    let mut steps = Vec::new();
    for _ in 0..12 {
        s = clamp_tab_scroll(s + TAB_PITCH, content, viewport);
        steps.push(s);
    }
    assert_eq!(steps[0], TAB_PITCH);
    assert_eq!(
        *steps.last().unwrap(),
        max,
        "stepping right stops exactly at max"
    );
    assert!(
        max % TAB_PITCH != 0.0,
        "max is not rounded to a pitch multiple here"
    );
    for _ in 0..12 {
        s = clamp_tab_scroll(s - TAB_PITCH, content, viewport);
    }
    assert_eq!(s, 0.0, "stepping left stops exactly at zero");
}

#[test]
fn growing_the_window_after_full_right_scroll_leaves_no_gap() {
    let content = tab_content_width(10);
    let at_right = max_tab_scroll(content, tab_viewport_width(10, region(900.0)));
    let wide = tab_viewport_width(10, region(1600.0));
    let s = clamp_tab_scroll(at_right, content, wide);
    assert_eq!(tab_edges_in_viewport(9, s).1, wide);
}

#[test]
fn shrinking_the_window_after_full_right_scroll_keeps_the_offset() {
    let content = tab_content_width(10);
    let at_right = max_tab_scroll(content, tab_viewport_width(10, region(1600.0)));
    let narrow = tab_viewport_width(10, region(900.0));
    assert_eq!(clamp_tab_scroll(at_right, content, narrow), at_right);
    assert!(
        at_right < max_tab_scroll(content, narrow),
        "more is now hidden to the right"
    );
}

#[test]
fn growing_until_everything_fits_resets_to_zero() {
    let content = tab_content_width(4);
    let at_right = max_tab_scroll(content, tab_viewport_width(4, 700.0));
    assert!(at_right > 0.0);
    assert!(!tabs_overflow(4, 2000.0));
    assert_eq!(
        clamp_tab_scroll(at_right, content, tab_viewport_width(4, 2000.0)),
        0.0
    );
}

#[test]
fn closing_the_last_tab_at_full_right_scroll_refills_the_viewport() {
    let r = region(1440.0);
    let at_right = max_tab_scroll(tab_content_width(10), tab_viewport_width(10, r));
    let viewport = tab_viewport_width(9, r);
    let s = clamp_tab_scroll(at_right, tab_content_width(9), viewport);
    assert_eq!(tab_edges_in_viewport(8, s).1, viewport);
}

#[test]
fn plus_is_pinned_outside_the_viewport_on_overflow() {
    // Region = [arrow gap][viewport][gap arrow][gap +]: the viewport plus
    // three boxes and three gaps is exactly the region, so "+" (the last
    // box) ends at the region's right edge whatever the scroll offset.
    let r = region(1440.0);
    let viewport = tab_viewport_width(10, r);
    assert_eq!(viewport + 3.0 * (STRIP_BOX_WIDTH + STRIP_BOX_GAP), r);
}

#[test]
fn scroll_into_view_is_minimal_and_exact() {
    let count = 10;
    let viewport = tab_viewport_width(count, region(1440.0));
    let max = max_tab_scroll(tab_content_width(count), viewport);

    assert_eq!(tab_scroll_into_view(0.0, count - 1, count, viewport), max);

    let s = tab_scroll_into_view(0.0, 6, count, viewport);
    assert_eq!(tab_edges_in_viewport(6, s).1, viewport);

    let s = tab_scroll_into_view(max, 2, count, viewport);
    assert_eq!(tab_edges_in_viewport(2, s).0, 0.0);

    assert_eq!(tab_scroll_into_view(max, 0, count, viewport), 0.0);
    assert_eq!(tab_scroll_into_view(228.0, 3, count, viewport), 228.0);
}

fn init_backend() {
    thread_local! {
        static INIT: Cell<bool> = const { Cell::new(false) };
    }
    INIT.with(|done| {
        if !done.get() {
            i_slint_backend_testing::init_no_event_loop();
            done.set(true);
        }
    });
}

fn click(app: &focusforge_gui::AppWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let window = app.window();
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn rect(app: &focusforge_gui::AppWindow, id: &str) -> (f32, f32, f32, f32) {
    let el = i_slint_backend_testing::ElementQuery::from_root(app)
        .match_id(id)
        .find_first()
        .unwrap_or_else(|| panic!("no element matched id {id:?}"));
    let p = el.absolute_position();
    let s = el.size();
    (p.x, p.y, s.width, s.height)
}

/// The rendered overflow layout matches the controller's arithmetic, and a
/// click on each arrow reaches `tab-scroll-left`/`-right`, not the drag
/// region under it.
#[test]
fn overflow_layout_matches_the_arithmetic_and_arrows_click() {
    init_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let r = app.get_tab_region_width();
    assert_eq!(r, region(1440.0));

    let lefts = Rc::new(Cell::new(0));
    let rights = Rc::new(Cell::new(0));
    app.on_tab_scroll_left({
        let lefts = lefts.clone();
        move || lefts.set(lefts.get() + 1)
    });
    app.on_tab_scroll_right({
        let rights = rights.clone();
        move || rights.set(rights.get() + 1)
    });
    app.set_tabs_overflow(true);
    app.set_can_scroll_left(true);
    app.set_can_scroll_right(true);
    app.set_tab_scroll_x(228.0);

    let (region_x, ..) = rect(&app, "FFTabStrip::tabs-region");
    let (vx, _, vw, _) = rect(&app, "FFTabStrip::tabs-viewport");
    assert_eq!(vw, tab_viewport_width(10, r));
    let (lx, _, lw, _) = rect(&app, "FFTabStrip::left-arrow");
    let (rx, _, rw, _) = rect(&app, "FFTabStrip::right-arrow");
    let (px, _, pw, _) = rect(&app, "FFTabStrip::plus-button");
    assert_eq!(lx, region_x);
    assert_eq!(vx, lx + lw + STRIP_BOX_GAP);
    assert_eq!(rx, vx + vw + STRIP_BOX_GAP);
    assert_eq!(px, rx + rw + STRIP_BOX_GAP);
    assert_eq!(px + pw, region_x + r);

    // Ten tabs at both hard stops: the same 4 px gap separates the left
    // arrow from the first tab and the last tab from the right arrow.
    let tabs: Vec<focusforge_gui::ProjectTabView> = (0..10)
        .map(|i| focusforge_gui::ProjectTabView {
            title: format!("tab {i}").into(),
            unsaved: false,
            selected: false,
            run_mark_icon: "".into(),
        })
        .collect();
    app.set_project_tabs(Rc::new(slint::VecModel::from(tabs)).into());
    let tab_x = |index: usize| -> f32 {
        i_slint_backend_testing::ElementQuery::from_root(&app)
            .match_id("FFTabStrip::tab-instance")
            .find_all()
            .into_iter()
            .find(|el| el.accessible_label().as_deref() == Some(&format!("tab-{index}")))
            .expect("tab instance")
            .absolute_position()
            .x
    };
    app.set_tab_scroll_x(0.0);
    settle_scroll_animation();
    assert_eq!(tab_x(0), lx + lw + STRIP_BOX_GAP);
    let max = max_tab_scroll(tab_content_width(10), vw);
    app.set_tab_scroll_x(max);
    settle_scroll_animation();
    assert_eq!(tab_x(9) + TAB_WIDTH + STRIP_BOX_GAP, rx);
    app.set_tab_scroll_x(228.0);

    let (x, y, w, h) = rect(&app, "FFTabStrip::right-arrow");
    click(&app, x + w / 2.0, y + h / 2.0);
    assert_eq!(
        rights.get(),
        1,
        "right arrow click must invoke tab-scroll-right"
    );

    let (x, y, w, h) = rect(&app, "FFTabStrip::left-arrow");
    click(&app, x + w / 2.0, y + h / 2.0);
    assert_eq!(
        lefts.get(),
        1,
        "left arrow click must invoke tab-scroll-left"
    );

    // At a stop the arrow stays in place but is disabled: clicks do nothing.
    app.set_can_scroll_left(false);
    click(&app, x + w / 2.0, y + h / 2.0);
    assert_eq!(lefts.get(), 1, "a disabled left arrow must ignore clicks");
    assert_eq!(
        rect(&app, "FFTabStrip::left-arrow"),
        (x, y, w, h),
        "layout never shifts"
    );
    app.set_can_scroll_right(false);
    let (x, y, w, h) = rect(&app, "FFTabStrip::right-arrow");
    click(&app, x + w / 2.0, y + h / 2.0);
    assert_eq!(rights.get(), 1, "a disabled right arrow must ignore clicks");
}
