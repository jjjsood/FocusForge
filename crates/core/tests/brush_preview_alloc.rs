// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary: a counting `#[global_allocator]` is process-wide, so it
//! must not share a binary with other tests. The count is per thread, so the
//! test runner's own threads cannot inflate it. Counting allocations requires the
//! `unsafe` `GlobalAlloc` trait below.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::num::NonZeroU32;

use focusforge_core::project::{BrushMode, Point};
use focusforge_core::retouch::preview::{BrushPreviewSpec, brush_preview_bounds};

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

fn count() {
    ALLOCATIONS.with(|n| n.set(n.get() + 1));
}

// SAFETY: delegates every operation straight to `System`; the only addition is
// a thread-local counter bump that itself allocates nothing.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: `layout` is what the caller of `GlobalAlloc::alloc` must
        // already guarantee; forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as `alloc`, forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: same contract as `alloc`, forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn brush_preview_bounds_allocates_nothing() {
    let step = NonZeroU32::new(2).unwrap();
    let mut answered = 0;
    for mode in [
        BrushMode::Normal,
        BrushMode::Lighten,
        BrushMode::Darken,
        BrushMode::Detail,
    ] {
        // In range, clipped at the edge, off the image and an invalid radius:
        // the answers differ, the allocation count does not.
        for (centre, radius) in [
            ((101, 97), 9.5),
            ((3, 297), 12.0),
            ((900, 900), 5.0),
            ((50, 50), -1.0),
        ] {
            let spec = BrushPreviewSpec {
                centre: Point {
                    x: centre.0,
                    y: centre.1,
                },
                radius,
                softness: 0.3,
                mode,
                opacity: 1.0,
            };
            let before = ALLOCATIONS.with(Cell::get);
            let bounds = brush_preview_bounds((300, 300), &spec, step);
            let made = ALLOCATIONS.with(Cell::get) - before;
            answered += usize::from(bounds.is_some());
            assert_eq!(made, 0, "{mode:?} at {centre:?} radius {radius}");
        }
    }
    assert_eq!(answered, 8, "two in-range brushes per mode answer");
}
