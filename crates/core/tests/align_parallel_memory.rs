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
//! this one file.
#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, TileReader};
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, PyramidalEcc, align_stack_with, bytes_per_registration,
    bytes_shared_reference,
};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, Rect};

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

/// Serializes memory-measuring tests in this binary against each other.
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
    // Only the anchor arm runs more than one worker.
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );

    let shared_reference = bytes_shared_reference(stack_ctx);
    let per_registration = bytes_per_registration(stack_ctx, DEFAULT_TILE_SIZE);
    // 7 registrations on this fixture (8 frames): a 1x budget must floor to
    // exactly one worker, and a 3x budget must clear that floor without
    // running into the `registrations` cap the fixture's small frame count
    // could otherwise trip. The budget's shared reference
    // term is a per-run constant, not a per-worker one -- both budgets add
    // it in once, then n worker-shares of the frame-side figure.
    let one_x = AlignDriverOptions {
        budget: HostBudget::explicit(shared_reference + per_registration),
        ..Default::default()
    };
    let three_x = AlignDriverOptions {
        budget: HostBudget::explicit(shared_reference + per_registration * 3),
        ..Default::default()
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

    // Slack: the fixture's own shared-reference-plus-one-registration figure
    // is derived at 512x512 (~5 MB total), and the serial arm still
    // allocates its own read buffers, pyramids and driver bookkeeping around
    // that one live registration -- 4x is generous headroom over the
    // arithmetic figure while still discriminating a budget that is
    // actually enforced from one that is merely divided on paper.
    let ceiling = (shared_reference + per_registration).saturating_mul(4);
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

fn open_fixture(name: &str) -> (TiffStack, StackContext) {
    let stack = TiffStack::open(common::fixtures_dir().join(name))
        .unwrap_or_else(|err| panic!("{name} must open: {err}"));
    let (width, height) = stack.dimensions();
    let ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    (stack, ctx)
}

fn config(ctx: &StackContext, mode: &str, policy: &str) -> ParamValues {
    let mut cfg = resolved_defaults(ctx);
    cfg.insert("mode".to_string(), ParamValue::Enum(mode.to_string()));
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum(policy.to_string()),
    );
    cfg.insert("max-iterations".to_string(), ParamValue::Int(40));
    cfg
}

/// What the shared reference data really holds once `prepare_run` returns:
/// at most the accounted figure, and close to it in rotation-scale mode,
/// where the templates are largest.
#[test]
fn retained_heap_after_prepare_run_matches_the_shared_figure() {
    let _guard = SERIAL.lock().unwrap();
    for name in ["mantis-512", "mantis-64"] {
        let (stack, ctx) = open_fixture(name);
        let shared = bytes_shared_reference(ctx);
        for mode in ["translation", "rotation-scale"] {
            let cfg = config(&ctx, mode, "anchor");
            let before = LIVE.load(Ordering::SeqCst);
            let prepared = PyramidalEcc
                .prepare_run(&AlignContext::new(ctx, 0, &stack), &cfg)
                .expect("prepare_run succeeds");
            let retained = (LIVE.load(Ordering::SeqCst) - before) as u64;
            assert!(
                retained <= shared,
                "{name} {mode}: retained {retained} B exceeds the shared figure {shared} B"
            );
            if mode == "rotation-scale" && name == "mantis-512" {
                assert!(
                    retained as f64 >= shared as f64 * 0.9,
                    "{name}: retained {retained} B is more than 10% under the shared figure \
                     {shared} B"
                );
            }
            drop(prepared);
        }
    }
}

/// One worker never holds more than the shared data plus one registration.
#[test]
fn serial_peak_stays_within_shared_plus_one_registration() {
    let _guard = SERIAL.lock().unwrap();
    for name in ["mantis-512", "mantis-64"] {
        let (stack, ctx) = open_fixture(name);
        let accounted =
            bytes_shared_reference(ctx) + bytes_per_registration(ctx, DEFAULT_TILE_SIZE);
        for mode in ["translation", "rotation-scale"] {
            for policy in ["anchor", "chain"] {
                let cfg = config(&ctx, mode, policy);
                let options = AlignDriverOptions {
                    budget: HostBudget::explicit(1),
                    ..Default::default()
                };
                let before = LIVE.load(Ordering::SeqCst);
                reset_peak();
                let run = align_stack_with(
                    &PyramidalEcc,
                    ctx,
                    &stack,
                    &cfg,
                    FailurePolicy::IdentityFallback,
                    &options,
                )
                .expect("the run completes");
                let peak = (peak_since_reset() - before) as u64;
                assert_eq!(run.worker_limit, 1);
                assert!(
                    peak <= accounted,
                    "{name} {mode} {policy}: peak live heap {peak} B exceeds shared + one \
                     registration {accounted} B"
                );
            }
        }
    }
}

/// In-memory single-sample frames, so a frame large enough that the level-0
/// window sits strictly inside it needs no fixture.
struct SyntheticFrames {
    width: u32,
    frames: Vec<Vec<u16>>,
}

impl TileReader for SyntheticFrames {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        for row in 0..rect.height as usize {
            let src = (rect.y as usize + row) * self.width as usize + rect.x as usize;
            out[row * rect.width as usize..][..rect.width as usize]
                .copy_from_slice(&self.frames[frame][src..src + rect.width as usize]);
        }
        Ok(())
    }
}

/// The same serial bound on a 1031x777 frame, where the level-0 window
/// (solve region plus margin) lies strictly inside the frame.
#[test]
fn serial_peak_stays_within_the_figure_when_the_level0_window_is_inside_the_frame() {
    let _guard = SERIAL.lock().unwrap();
    let (width, height) = (1031u32, 777u32);
    let texture = |x: u32, y: u32, shift: u32| -> u16 {
        let (x, y) = (x + shift, y);
        ((x * 7 + y * 13) % 251 * 200 + (x / 9 + y / 5) % 17 * 1000) as u16
    };
    let frames: Vec<Vec<u16>> = (0..3u32)
        .map(|i| {
            (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .map(|(x, y)| texture(x, y, i * 2))
                .collect()
        })
        .collect();
    let reader = SyntheticFrames { width, frames };
    let ctx = StackContext::new(3, width, height, 1);
    let accounted = bytes_shared_reference(ctx) + bytes_per_registration(ctx, DEFAULT_TILE_SIZE);
    for mode in ["translation", "rotation-scale"] {
        let mut cfg = config(&ctx, mode, "anchor");
        cfg.insert("max-iterations".to_string(), ParamValue::Int(6));
        let options = AlignDriverOptions {
            budget: HostBudget::explicit(1),
            ..Default::default()
        };
        let before = LIVE.load(Ordering::SeqCst);
        reset_peak();
        let run = align_stack_with(
            &PyramidalEcc,
            ctx,
            &reader,
            &cfg,
            FailurePolicy::IdentityFallback,
            &options,
        )
        .expect("the run completes");
        let peak = (peak_since_reset() - before) as u64;
        assert_eq!(run.worker_limit, 1);
        assert!(
            peak <= accounted,
            "{mode}: peak live heap {peak} B exceeds shared + one registration {accounted} B"
        );
    }
}
