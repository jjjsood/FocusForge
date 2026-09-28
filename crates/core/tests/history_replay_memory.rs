// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary, holding only the one memory test: a counting
//! `#[global_allocator]` is process-wide, so it must never share a binary
//! with a sibling test that would inflate its reading.
//!
//! # Safety
//!
//! `GlobalAlloc` is an `unsafe trait`; there is no way to measure live heap
//! bytes without implementing it. Dev-only measurement scaffolding for this
//! one test binary, never shipped as part of `crates/core`'s library crate.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::project::{Op, Point};
use focusforge_core::retouch::replay;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapError, ZMapInfo};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// SAFETY: delegates every operation straight to `System`, the platform
// allocator that is already sound; the only addition is byte-counting
// bookkeeping around each call, which touches no allocation itself.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is exactly what the caller of `GlobalAlloc::alloc`
        // must already guarantee is valid; forwarded unchanged to `System`.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let cur = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(cur, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as `alloc` above, forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as `alloc` above, forwarded unchanged.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            if new_size >= layout.size() {
                let delta = new_size - layout.size();
                let cur = LIVE.fetch_add(delta, Ordering::SeqCst) + delta;
                PEAK.fetch_max(cur, Ordering::SeqCst);
            } else {
                let delta = layout.size() - new_size;
                LIVE.fetch_sub(delta, Ordering::SeqCst);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static BASELINE: AtomicUsize = AtomicUsize::new(0);

fn reset_peak() {
    let live = LIVE.load(Ordering::SeqCst);
    BASELINE.store(live, Ordering::SeqCst);
    PEAK.store(live, Ordering::SeqCst);
}

/// How far live heap bytes grew above the baseline captured at
/// [`reset_peak`] -- the pre-existing `indices` buffer is baseline, not
/// growth.
fn peak_growth_since_reset() -> usize {
    PEAK.load(Ordering::SeqCst)
        .saturating_sub(BASELINE.load(Ordering::SeqCst))
}

/// A 39-brush-stroke list scattered over the whole map, plus one
/// `ResetRegion` bounded to `reset_rect` -- what makes `tile` (the union of
/// every op's footprint) the *whole* map, so `indices` is a full-size buffer
/// allocated before measurement, exactly like a real resident session tile
/// would be at this scale.
fn ops_40(reset_rect: Rect, width: u32, height: u32) -> Vec<Op> {
    let mut ops: Vec<Op> = (0..39)
        .map(|i| Op::BrushStroke {
            source_id: (i % 4) as u16,
            path: vec![Point {
                x: (i * 37) % width as i32,
                y: (i * 53) % height as i32,
            }],
            radius: 2.0,
            softness: 0.2,
        })
        .collect();
    ops.push(Op::ResetRegion { rect: reset_rect });
    ops
}

fn run_case(width: u32, height: u32) {
    let info = ZMapInfo::new(width, height, 4).unwrap();
    let full = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let reset_rect = Rect {
        x: 0,
        y: 0,
        width: width.min(64),
        height: height.min(64),
    };
    let ops = ops_40(reset_rect, width, height);

    // The full map's own indices, allocated *before* measurement -- proving
    // replay's own peak does not scale with this buffer's size. `full` is
    // handed to `replay` as the resident tile regardless of the ops' own
    // (smaller) union, exactly as a session holding the whole map resident
    // would.
    let mut indices = vec![0u16; info.tile_len(full)];

    let ceiling = info.tile_len(reset_rect) * 2 + 64 * 1024;
    reset_peak();
    let mut fused = |_rect: Rect, buf: &mut [u16]| -> Result<(), ZMapError> {
        buf.fill(0);
        Ok(())
    };
    replay::replay(&ops, info, full, &mut indices, &mut fused).unwrap();
    let peak = peak_growth_since_reset();

    assert!(
        peak < ceiling,
        "peak live heap {peak} B exceeded the {ceiling} B ceiling at {width}x{height} \
         (the map's own indices buffer alone is {} B, allocated before measuring)",
        info.tile_len(full) * 2
    );
}

#[test]
fn replay_is_in_place() {
    run_case(1024, 768);
    run_case(8192, 5468);
}
