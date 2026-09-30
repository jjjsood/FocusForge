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

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::fuse::FusionConsumer;
use focusforge_core::host::HostFacts;
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{
    NoopPreprocess, Rect, TileConsumer, TileError, TileJob, TileScheduler, TileSchedulerOptions,
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

/// A [`TileReader`] that fills every read with zeros without touching disk
/// or allocating per-frame state of its own -- cheap enough to drive a
/// fabricated 600-frame stack that never existed as real files.
struct ZeroReader;

impl TileReader for ZeroReader {
    fn read_tile_into(
        &self,
        _frame: usize,
        _rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        out.fill(0);
        Ok(())
    }
}

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
    // 5 MiB: scheduler buffers plus per-worker pyramids; still far under
    // the 12 MiB a load-every-frame implementation needs.
    const CEILING: usize = 5 * 1024 * 1024;
    assert!(
        (CEILING as u64) < full_load_bytes,
        "ceiling ({CEILING} B) must be below a full load ({full_load_bytes} B) for this \
         fixture, or this test cannot discriminate a tiled implementation from a full-load one"
    );

    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_ceiling");
    let mut options = TileSchedulerOptions::explicit_budget(scratch_base, 2 * 1024 * 1024);
    options.tile_size = 128;
    options.levels = 2;
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
            crop: None,
            description: None,
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
            bytes_per_tile_set * target as u64
        };
        let mut options = TileSchedulerOptions::explicit_budget(scratch_base, max_bytes_in_flight);
        options.tile_size = 128;
        options.levels = 2;
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

/// A 600-frame stack over a host with room for it keeps `pool_threads()`
/// above one even though the byte budget floors `in_flight_limit()` to one:
/// pool size and in-flight limit are decoupled.
#[test]
fn pool_decouples_from_the_in_flight_limit_at_scale() {
    let available_parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if available_parallelism <= 1 {
        eprintln!("host reports only one logical core; skipping the decoupling case");
        return;
    }

    let frame_count = 600usize;
    // usable_available = 150_000_000 B; budget = 40% = 60_000_000 B.
    // bytes_per_tile_set(600, 128, 3) = 58_982_400 B, so the budget covers
    // exactly one tile set and floors `in_flight_limit()` to 1, while
    // `pool_threads()` stays decoupled from it.
    let facts = HostFacts::new(300_000_000, 150_000_000, None, None, available_parallelism);
    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_decouple");
    let mut options = TileSchedulerOptions::with_host_facts(scratch_base, &facts);
    options.tile_size = 128;
    options.levels = 0;

    let reader = ZeroReader;
    let stack_ctx = StackContext::new(frame_count, 128, 128, 3);
    let scheduler =
        TileScheduler::new(&reader, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(
        scheduler.in_flight_limit(),
        1,
        "the byte budget must floor to exactly one tile set at this scale"
    );
    assert!(
        scheduler.pool_threads() > 1,
        "the pool must not collapse with the budget: pool_threads={}",
        scheduler.pool_threads()
    );

    let report = scheduler
        .run(&NoopPreprocess, &NoAllocConsumer)
        .expect("run must succeed");
    assert!(
        report.peak_in_flight <= scheduler.in_flight_limit(),
        "the admission gate must still cap live tile sets at the limit: peak={}",
        report.peak_in_flight
    );
}

/// A host-derived, uncapped budget below one tile set is refused by name --
/// never floored, never a hang.
#[test]
fn too_small_host_budget_is_refused_by_name() {
    // usable_available = 100 B; budget = 40% = 40 B, far under one tile set.
    let facts = HostFacts::new(1_000, 100, None, None, 4);
    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_too_small");
    let mut options = TileSchedulerOptions::with_host_facts(scratch_base, &facts);
    options.tile_size = 128;
    options.levels = 0;

    let reader = ZeroReader;
    let frame_count = 8usize;
    let stack_ctx = StackContext::new(frame_count, 128, 128, 3);
    let err = match TileScheduler::new(&reader, stack_ctx, 3, &options) {
        Err(err) => err,
        Ok(_) => panic!("a too-small host-derived budget must be refused"),
    };

    let expected_bytes_per_tile_set = frame_count as u64 * 128 * 128 * 3 * 2;
    match &err {
        TileError::HostMemoryTooSmall {
            budget_bytes,
            bytes_per_tile_set,
            frame_count: reported_frame_count,
            tile_size,
        } => {
            assert_eq!(*budget_bytes, 40);
            assert_eq!(*bytes_per_tile_set, expected_bytes_per_tile_set);
            assert_eq!(*reported_frame_count, frame_count);
            assert_eq!(*tile_size, 128);
        }
        other => panic!("expected HostMemoryTooSmall, got {other:?}"),
    }

    let rendered = err.to_string();
    assert!(rendered.contains(&40.to_string()), "{rendered}");
    assert!(
        rendered.contains(&expected_bytes_per_tile_set.to_string()),
        "{rendered}"
    );
    assert!(rendered.contains("stack fewer frames"), "{rendered}");
}

/// A zero-filling reader that reports a fixed scratch cost per read.
struct ScratchReader(u64);

impl TileReader for ScratchReader {
    fn read_tile_into(
        &self,
        _frame: usize,
        _rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        out.fill(0);
        Ok(())
    }

    fn read_scratch_bytes(&self, _width: u32, _height: u32) -> u64 {
        self.0
    }
}

/// The host refusal compares the budget with the tile set plus the reader's
/// per-read scratch, not the frame tiles alone.
#[test]
fn host_refusal_counts_reader_scratch() {
    // usable_available = 2_000_000 B; budget = 40% = 800_000 B, above the
    // 786_432 B of frame tiles and below the 1_786_432 B with scratch.
    let facts = HostFacts::new(4_000_000, 2_000_000, None, None, 4);
    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_scratch_refusal");
    let mut options = TileSchedulerOptions::with_host_facts(scratch_base, &facts);
    options.tile_size = 128;
    options.levels = 0;
    let stack_ctx = StackContext::new(8, 128, 128, 3);

    assert!(TileScheduler::new(&ZeroReader, stack_ctx, 3, &options).is_ok());
    let reader = ScratchReader(1_000_000);
    match TileScheduler::new(&reader, stack_ctx, 3, &options) {
        Err(TileError::HostMemoryTooSmall {
            budget_bytes,
            bytes_per_tile_set,
            ..
        }) => {
            assert_eq!(budget_bytes, 800_000);
            assert_eq!(bytes_per_tile_set, 1_786_432);
        }
        Ok(_) => panic!("scratch must count towards the host refusal"),
        Err(other) => panic!("expected HostMemoryTooSmall, got {other:?}"),
    }
}

/// A scaling warp reads a wider footprint than the tile it delivers, so the
/// same explicit budget admits fewer tile sets than an identity warp does.
#[test]
fn a_scale_warp_lowers_the_in_flight_limit() {
    use focusforge_core::align::{Transform, WarpingReader};

    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if available < 2 {
        eprintln!("host reports one logical core; the in-flight limit cannot exceed 1");
        return;
    }
    let frame_count = 8usize;
    let bytes_per_tile_set = frame_count as u64 * 128 * 128 * 3 * 2;
    let limit_for = |transform: Transform, name: &str| {
        let reader = WarpingReader::new(&ZeroReader, (512, 512), 3, vec![transform; frame_count]);
        let scratch_base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let mut options =
            TileSchedulerOptions::explicit_budget(scratch_base, 2 * bytes_per_tile_set);
        options.tile_size = 128;
        options.levels = 0;
        let stack_ctx = StackContext::new(frame_count, 512, 512, 3);
        TileScheduler::new(&reader, stack_ctx, 3, &options)
            .expect("scheduler must build")
            .in_flight_limit()
    };
    let identity = limit_for(Transform::identity(), "tile_scheduler_memory_warp_identity");
    let scaled = limit_for(
        Transform::similarity(1.5, 0.0, 0.0, 0.0),
        "tile_scheduler_memory_warp_scale",
    );
    assert_eq!(identity, 2);
    assert!(scaled < identity, "scaled {scaled} vs identity {identity}");
}
