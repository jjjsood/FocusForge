// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary, holding only the two memory tests: a counting
//! `#[global_allocator]` and a peak-in-flight figure are both process-wide,
//! so any sibling test sharing this binary would inflate them exactly as
//! this file's own two tests would inflate each other without `SERIAL`
//! below -- cargo's harness runs a binary's tests on parallel threads by
//! default.
//!
//! # Safety
//!
//! `GlobalAlloc` is an `unsafe trait`; there is no way to measure live heap
//! bytes without implementing it. This is dev-only measurement scaffolding
//! for one dedicated test binary, never shipped as part of `crates/core`'s
//! library crate -- the workspace's `unsafe_code = "deny"` lint
//! (`Cargo.toml`) is about the shipped crate, and `crates/core/src/tile/`
//! itself adds none; the allow below is local to this one file.
#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use focusforge_core::algorithm::AlgorithmError;
use focusforge_core::fuse::FusionConsumer;
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{
    NoopPreprocess, TileConsumer, TileJob, TileScheduler, TileSchedulerOptions,
};

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

/// Both tests in this binary take this first: without it, cargo's
/// parallel-by-default test threads would let one test's allocations
/// inflate the other's peak reading exactly as a foreign sibling test
/// would.
static SERIAL: Mutex<()> = Mutex::new(());

/// Touches every frame's tile without accumulating anything, so the
/// measured peak reflects the scheduler's own frame-tile buffers, not this
/// consumer's.
struct NoAllocConsumer;

impl TileConsumer for NoAllocConsumer {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        for i in 0..job.ctx().frame_count() {
            let _ = job.ctx().frame(i)?;
        }
        Ok(())
    }
}

#[test]
fn peak_live_heap_stays_under_ceiling() {
    let _guard = SERIAL.lock().unwrap();

    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let frame_count = stack.len();

    // Computed from the fixture's own header, not hardcoded: 8 frames x
    // 512 x 512 x 3 samples x 2 B. This is what makes "a load-every-frame
    // implementation fails by construction" a fact rather than a claim.
    let full_load_bytes = frame_count as u64 * width as u64 * height as u64 * 3 * 2;
    // Raised from 4 MiB to 5 MiB when this test grew a real allocating
    // consumer. The ceiling moved because what is being measured grew --
    // a `FusionConsumer` builds per-worker Laplacian pyramids on top of
    // the scheduler's frame-tile buffers, where the old `NoAllocConsumer`
    // added nothing -- NOT because the test was failing and the number
    // was in the way. That is the whole of it: a first diagnosis blamed
    // the budget formula instead and was wrong, because `tile_size` is
    // already the outer size. The consumer's pyramids are the real
    // reason, and they are why this ceiling stays raised while the
    // formula went back.
    //
    // The discrimination the test exists for is intact and asserted just
    // below: 5 MiB is still far under the 12 MiB a load-every-frame
    // implementation would need, so a full-load regression still fails
    // this test loudly.
    const CEILING: usize = 5 * 1024 * 1024;
    assert!(
        (CEILING as u64) < full_load_bytes,
        "ceiling ({CEILING} B) must be below a full load ({full_load_bytes} B) for this \
         fixture, or this test cannot discriminate a tiled implementation from a full-load one"
    );

    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_ceiling");
    let options = TileSchedulerOptions {
        tile_size: 128,
        levels: 2,
        max_bytes_in_flight: 2 * 1024 * 1024,
        scratch_base,
    };
    let stack_ctx = StackContext::new(frame_count, width, height, 3);
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(
        scheduler.in_flight_limit(),
        2,
        "a 2 MiB budget at this tile size must floor to 2 in-flight tile sets"
    );

    // A real fusion consumer, not the pass-through `NoAllocConsumer`, so
    // this ceiling measures the algorithm's own per-worker pyramid
    // allocations (two live pyramids plus one score/winner plane per level)
    // on top of the scheduler's frame-tile buffers.
    let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
    let mut explicit = ParamValues::new();
    explicit.insert(
        "levels".to_string(),
        focusforge_core::params::ParamValue::Int(2),
    );
    let resolved =
        params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);
    let composite_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_ceiling.tif");
    let zmap_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_ceiling-zmap.tif");
    let consumer = FusionConsumer::create(
        algo,
        values,
        &composite_path,
        Some(&zmap_path),
        focusforge_core::fuse::FusionGeometry {
            width,
            height,
            samples_per_pixel: 3,
            frame_count: frame_count as u32,
        },
    )
    .expect("consumer must create");

    reset_peak();
    let report = scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("run must succeed");
    let peak = peak_since_reset();
    consumer.finish().expect("finish must succeed");

    assert!(
        peak < CEILING,
        "peak live heap {peak} B exceeded the {CEILING} B ceiling (a full load would need \
         {full_load_bytes} B; report={report:?})"
    );
}

/// Blocks each caller until `threshold` calls are simultaneously inside
/// `consume`, forcing real concurrency instead of hoping enough tiles
/// happen to overlap. Bounded by `wait_timeout_while` so a run that (for
/// whatever reason) never reaches the threshold does not hang the test
/// suite -- it simply proceeds, and the report-level assertion then fails
/// with a clear message instead of a timeout.
struct RendezvousConsumer {
    threshold: usize,
    count: Mutex<usize>,
    cvar: Condvar,
}

impl RendezvousConsumer {
    fn new(threshold: usize) -> Self {
        RendezvousConsumer {
            threshold,
            count: Mutex::new(0),
            cvar: Condvar::new(),
        }
    }
}

impl TileConsumer for RendezvousConsumer {
    fn consume(&self, _job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let mut count = self.count.lock().unwrap();
        *count += 1;
        if *count >= self.threshold {
            self.cvar.notify_all();
        } else {
            let (_count, _timed_out) = self
                .cvar
                .wait_timeout_while(count, Duration::from_secs(2), |c| *c < self.threshold)
                .unwrap();
        }
        Ok(())
    }
}

#[test]
fn in_flight_tiles_respect_the_budget() {
    let _guard = SERIAL.lock().unwrap();

    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let frame_count = stack.len();
    let bytes_per_tile_set = frame_count as u64 * 128 * 128 * 3 * 2;

    for &target in &[1usize, 2, 4] {
        if target > available {
            eprintln!(
                "host reports only {available} logical cores; skipping the {target}-in-flight case"
            );
            continue;
        }

        let scratch_base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("tile_scheduler_memory_inflight_{target}"));
        let max_bytes_in_flight = if target == 1 {
            1
        } else {
            (bytes_per_tile_set * target as u64) as usize
        };
        let options = TileSchedulerOptions {
            tile_size: 128,
            levels: 2,
            max_bytes_in_flight,
            scratch_base,
        };
        let stack_ctx = StackContext::new(frame_count, width, height, 3);
        let scheduler =
            TileScheduler::new(&stack, stack_ctx, 3, &options).expect("scheduler must build");
        assert_eq!(scheduler.in_flight_limit(), target);

        let report = if target >= 2 {
            scheduler
                .run(&NoopPreprocess, &RendezvousConsumer::new(2))
                .expect("run must succeed")
        } else {
            scheduler
                .run(&NoopPreprocess, &NoAllocConsumer)
                .expect("run must succeed")
        };

        assert!(
            report.peak_in_flight <= report.in_flight_limit,
            "peak_in_flight {} exceeded the limit {} at target {target}",
            report.peak_in_flight,
            report.in_flight_limit
        );
        if target >= 2 {
            assert!(
                report.peak_in_flight >= 2,
                "at in_flight_limit {target}, peak_in_flight must reach at least 2 -- \
                 otherwise the bound would be trivially satisfied by running serially"
            );
        }
    }
}
