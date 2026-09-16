// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary, holding the one memory test that needs isolation --
//! a counting `#[global_allocator]` is process-wide, so a sibling test in the
//! same binary would inflate the reading.
//!
//! # Safety
//!
//! `GlobalAlloc` is an `unsafe trait`; there is no way to measure live heap
//! bytes without implementing it. This is dev-only measurement scaffolding
//! for one dedicated test binary, never shipped as part of `crates/core`'s
//! library crate -- the workspace's `unsafe_code = "deny"` lint
//! (`Cargo.toml`) is about the shipped crate; the allow below is local to
//! this one file, copied verbatim from `tile_scheduler_memory.rs`.
#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::AlignAlgorithm;
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, PyramidalEcc, align_stack_with, bytes_per_registration,
};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;

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

fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
}

fn peak_since_reset() -> usize {
    PEAK.load(Ordering::SeqCst)
}

/// This binary holds only one test, but the guard is kept anyway -- copied
/// verbatim from `tile_scheduler_memory.rs`'s own rationale, and cheap
/// insurance against a future sibling test landing in this file without
/// re-deriving the reason it would need one.
static SERIAL: Mutex<()> = Mutex::new(());

fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), ctx)
        .expect("defaults resolve cleanly");
    params::resolved_values(&resolved)
}

/// The worker budget genuinely bounds live heap, not merely divides a
/// number on paper: a 1x-`bytes_per_registration` budget runs the anchor arm
/// serially (`workers_for_budget` floors to 1) and its peak stays under one
/// registration's own figure plus slack, while a 3x budget engages the
/// parallel arm and reaches a strictly higher peak.
#[test]
fn peak_live_heap_tracks_the_worker_budget() {
    let _guard = SERIAL.lock().unwrap();

    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let mut cfg = resolved_defaults(&stack_ctx);
    // The shipped 400 costs seconds this memory-shape property does not need.
    cfg.insert("max-iterations".to_string(), ParamValue::Int(40));

    let per_registration = bytes_per_registration(stack_ctx, DEFAULT_TILE_SIZE);
    // 7 registrations on this fixture (8 frames): a 1x budget must floor to
    // exactly one worker, and a 3x budget must clear that floor without
    // running into the `registrations` cap the fixture's small frame count
    // could otherwise trip.
    let one_x = AlignDriverOptions {
        max_bytes_in_flight: per_registration as usize,
    };
    let three_x = AlignDriverOptions {
        max_bytes_in_flight: (per_registration * 3) as usize,
    };

    reset_peak();
    let serial = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &one_x,
    )
    .expect("1x-budget run completes");
    let serial_peak = peak_since_reset();
    assert_eq!(
        serial.worker_limit, 1,
        "a 1x-bytes_per_registration budget must floor to one worker"
    );

    reset_peak();
    let parallel = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &three_x,
    )
    .expect("3x-budget run completes");
    let parallel_peak = peak_since_reset();
    assert!(
        parallel.worker_limit >= 3,
        "a 3x-bytes_per_registration budget must clear the one-worker floor: {}",
        parallel.worker_limit
    );

    // Slack: the fixture's own per-registration figure is derived at
    // 512x512 (~5 MB), and the serial arm still allocates its own read
    // buffers, pyramids and driver bookkeeping around that one live
    // registration -- 4x is generous headroom over the arithmetic figure
    // while still discriminating a budget that is actually enforced from
    // one that is merely divided on paper.
    let ceiling = per_registration.saturating_mul(4);
    assert!(
        (serial_peak as u64) < ceiling,
        "1x-budget peak {serial_peak} B exceeded {ceiling} B (4x one registration's {per_registration} B)"
    );
    assert!(
        parallel_peak > serial_peak,
        "3x-budget peak {parallel_peak} B must exceed the 1x-budget peak {serial_peak} B -- \
         otherwise the budget is being divided rather than actually bounding concurrency"
    );

    assert_eq!(
        serial.alignment.frames, parallel.alignment.frames,
        "the byte-identity property must hold at fixture scale here too"
    );
}
