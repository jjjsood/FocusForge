// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary, holding only the memory tests: a counting
//! `#[global_allocator]` and a peak-in-flight figure are both process-wide,
//! so any sibling test sharing this binary would inflate them exactly as
//! this file's own tests would inflate each other without `SERIAL`
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
use std::sync::{Condvar, Mutex, PoisonError};
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

/// Every test in this binary takes this first: without it, cargo's
/// parallel-by-default test threads would let one test's allocations
/// inflate another's peak reading exactly as a foreign sibling test
/// would.
static SERIAL: Mutex<()> = Mutex::new(());

/// A panicking test poisons [`SERIAL`]; the next one must still run.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

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
        job.ctx().for_each_frame(|_, _, _| Ok(()))
    }
}

#[test]
fn peak_live_heap_stays_under_ceiling() {
    let _guard = serial();

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

    // A real fusion consumer, not the pass-through `NoAllocConsumer`, so
    // this ceiling measures the algorithm's own per-worker pyramid
    // allocations (two live pyramids plus one score/winner plane per level)
    // on top of the scheduler's frame-tile buffer.
    let stack_ctx = StackContext::new(frame_count, width, height, 3);
    let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
    let mut explicit = ParamValues::new();
    explicit.insert(
        "levels".to_string(),
        focusforge_core::params::ParamValue::Int(2),
    );
    let resolved =
        params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);

    // Two accounted tiles of the method's own bytes: the budget admits two.
    let tile_bytes = 128 * 128 * (3 * 2 + 1 + algo.working_bytes_per_pixel(&values, 3));
    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_ceiling");
    let mut options = TileSchedulerOptions::explicit_budget(scratch_base, 2 * tile_bytes);
    options.tile_size = 128;
    options.levels = 2;
    options.working_bytes_per_pixel = algo.working_bytes_per_pixel(&values, 3);
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(
        scheduler.in_flight_limit(),
        2,
        "a budget of two accounted tiles must admit exactly two"
    );

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
    let _guard = serial();

    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, 3);
    let probe_options = {
        let mut options = TileSchedulerOptions::explicit_budget(
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_inflight_probe"),
            1,
        );
        options.tile_size = 128;
        options.levels = 2;
        options
    };
    let bytes_per_tile = TileScheduler::new(&stack, stack_ctx, 3, &probe_options)
        .expect("scheduler must build")
        .bytes_per_tile();
    assert_eq!(bytes_per_tile, 128 * 128 * (3 * 2 + 1));

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
            bytes_per_tile * target as u64
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
    let _guard = serial();
    let available_parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if available_parallelism <= 1 {
        eprintln!("host reports only one logical core; skipping the decoupling case");
        return;
    }

    let frame_count = 600usize;
    // usable_available = 300_000 B; budget = 40% = 120_000 B. One 128x128 RGB
    // tile is 114_688 B whatever the frame count, so the budget covers it
    // once and `in_flight_limit()` is 1, while `pool_threads()` stays
    // decoupled from it.
    let facts = HostFacts::new(600_000, 300_000, None, None, available_parallelism);
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
        "the byte budget must admit exactly one tile at this scale"
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
        "the admission gate must still cap live tiles at the limit: peak={}",
        report.peak_in_flight
    );
}

/// A tile size beyond the frame never reads more than the frame, so a tile
/// is accounted at the frame's extent, not at `tile_size^2`.
#[test]
fn a_tile_larger_than_the_frame_is_accounted_at_the_frame_extent() {
    let _guard = serial();
    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tile_scheduler_memory_small_frame");
    let mut options = TileSchedulerOptions::explicit_budget(scratch_base, 1 << 30);
    options.tile_size = 2000;
    options.levels = 0;

    let reader = ZeroReader;
    let frame_count = 8usize;
    let stack_ctx = StackContext::new(frame_count, 100, 60, 3);
    let scheduler =
        TileScheduler::new(&reader, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(scheduler.bytes_per_tile(), 100 * 60 * (3 * 2 + 1));
}

/// A host-derived, uncapped budget below one tile is refused by name --
/// never floored, never a hang.
#[test]
fn too_small_host_budget_is_refused_by_name() {
    let _guard = serial();
    // usable_available = 100 B; budget = 40% = 40 B, far under one tile.
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

    let expected_bytes_per_tile = 128 * 128 * (3 * 2 + 1);
    match &err {
        TileError::HostMemoryTooSmall {
            budget_bytes,
            bytes_per_tile,
            tile_size,
        } => {
            assert_eq!(*budget_bytes, 40);
            assert_eq!(*bytes_per_tile, expected_bytes_per_tile);
            assert_eq!(*tile_size, 128);
        }
        other => panic!("expected HostMemoryTooSmall, got {other:?}"),
    }

    let rendered = err.to_string();
    assert!(rendered.contains(&40.to_string()), "{rendered}");
    assert!(
        rendered.contains(&expected_bytes_per_tile.to_string()),
        "{rendered}"
    );
    assert!(
        !rendered.contains("frames"),
        "a per-tile budget gives no frame-count advice: {rendered}"
    );
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

/// The host refusal compares the budget with the tile plus the reader's
/// per-read scratch, not the frame tile alone.
#[test]
fn host_refusal_counts_reader_scratch() {
    let _guard = serial();
    // usable_available = 2_000_000 B; budget = 40% = 800_000 B, above the
    // 114_688 B of tile and below the 1_114_688 B with scratch.
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
            bytes_per_tile,
            ..
        }) => {
            assert_eq!(budget_bytes, 800_000);
            assert_eq!(bytes_per_tile, 1_114_688);
        }
        Ok(_) => panic!("scratch must count towards the host refusal"),
        Err(other) => panic!("expected HostMemoryTooSmall, got {other:?}"),
    }
}

/// A scaling warp reads a wider footprint than the tile it delivers, so the
/// same explicit budget admits fewer tiles than an identity warp does.
#[test]
fn a_scale_warp_lowers_the_in_flight_limit() {
    use focusforge_core::align::{Transform, WarpingReader};

    let _guard = serial();
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if available < 2 {
        eprintln!("host reports one logical core; the in-flight limit cannot exceed 1");
        return;
    }
    let frame_count = 8usize;
    let bytes_per_tile = 128u64 * 128 * (3 * 2 + 1);
    let limit_for = |transform: Transform, name: &str| {
        let reader = WarpingReader::new(&ZeroReader, (512, 512), 3, vec![transform; frame_count]);
        let scratch_base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let mut options = TileSchedulerOptions::explicit_budget(scratch_base, 2 * bytes_per_tile);
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

/// A reader serving position- and frame-dependent content, so every method
/// has real detail to rank without any file behind it.
struct PatternReader;

impl TileReader for PatternReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let mut at = 0;
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let base = (x * 7 + y * 13 + frame as u32 * 31) ^ (x * y);
                for c in 0..3u32 {
                    out[at] = (base.wrapping_mul(2654435761) >> 16) as u16 ^ (c * 1009) as u16;
                    at += 1;
                }
            }
        }
        Ok(())
    }
}

/// The live heap growth, in bytes, of running a `frame_count`-frame stack
/// through the scheduler at one tile in flight: into `method`'s fusion when
/// `fuse` is set, else into a consumer that only visits every frame.
fn tile_peak(method: &str, frame_count: usize, width: u32, height: u32, fuse: bool) -> usize {
    let algo = registry::find_stack(method).expect("method must be registered");
    let mut explicit = ParamValues::new();
    if method == "laplacian" {
        explicit.insert(
            "levels".to_string(),
            focusforge_core::params::ParamValue::Int(2),
        );
    }
    let stack_ctx = StackContext::new(frame_count, width, height, 3);
    let resolved =
        params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);

    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let tag = format!("tile_scheduler_memory_{method}_{frame_count}");
    let mut options = TileSchedulerOptions::explicit_budget(base.join(&tag), 1);
    options.apply_support(algo.tile_support(&values));
    options.tile_size = 128;
    let scheduler =
        TileScheduler::new(&PatternReader, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(scheduler.in_flight_limit(), 1);

    let fusion = fuse.then(|| {
        FusionConsumer::create(
            algo,
            values,
            &base.join(format!("{tag}.tif")),
            Some(&base.join(format!("{tag}-zmap.tif"))),
            focusforge_core::fuse::FusionGeometry {
                width,
                height,
                samples_per_pixel: 3,
                frame_count: frame_count as u32,
                crop: None,
                description: None,
            },
        )
        .expect("consumer must create")
    });

    let live_before = LIVE.load(Ordering::SeqCst);
    reset_peak();
    let consumer: &dyn TileConsumer = match &fusion {
        Some(fusion) => fusion,
        None => &NoAllocConsumer,
    };
    scheduler
        .run(&NoopPreprocess, consumer)
        .expect("run must succeed");
    let peak = peak_since_reset();
    if let Some(fusion) = fusion {
        fusion.finish().expect("finish must succeed");
    }
    peak.saturating_sub(live_before)
}

/// [`tile_peak`] after one discarded warm-up run (first-use allocations of the
/// worker pool and codecs), as the minimum over several repetitions: the
/// live-heap peak of one run varies by about 120 kB from allocator and
/// scheduling noise, the minimum drops most of it.
fn settled_peak(method: &str, frame_count: usize, width: u32, height: u32, fuse: bool) -> usize {
    tile_peak(method, frame_count, width, height, fuse);
    (0..3)
        .map(|_| tile_peak(method, frame_count, width, height, fuse))
        .min()
        .expect("at least one repetition")
}

/// Each method's declared working bytes cover what one tile of its fusion
/// holds beyond a pass-through of the same frames, and are at most twice it.
#[test]
fn declared_working_bytes_cover_the_measured_peak() {
    let _guard = serial();
    let (width, height) = (128u32, 128u32);
    for method in ["laplacian", "weighted-average", "depth-map"] {
        let algo = registry::find_stack(method).expect("method must be registered");
        let mut explicit = ParamValues::new();
        if method == "laplacian" {
            explicit.insert(
                "levels".to_string(),
                focusforge_core::params::ParamValue::Int(2),
            );
        }
        let stack_ctx = StackContext::new(8, width, height, 3);
        let resolved =
            params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
        let values = params::resolved_values(&resolved);
        let declared = algo.working_bytes_per_pixel(&values, 3) * u64::from(width * height);

        let fused = settled_peak(method, 8, width, height, true);
        let baseline = settled_peak(method, 8, width, height, false);
        let measured = fused.saturating_sub(baseline) as u64;
        eprintln!("{method}: declared {declared} B, measured {measured} B");
        assert!(
            measured <= declared && declared <= 2 * measured,
            "{method}: declared {declared} B must lie in [measured, 2 * measured] with \
             measured = {measured} B"
        );
    }
}

/// Frame count must not move the per-tile peak beyond measurement noise. The
/// bound is two frame tiles, about 1.6 times the 120 kB spread of a single
/// run; [`settled_peak`] shrinks the residual difference to under 16 kB.
/// Keeping every frame's tile resident would differ by 56 frame tiles between
/// 8 and 64 frames.
#[test]
fn peak_heap_is_independent_of_frame_count() {
    let _guard = serial();
    let frame_tile = 128 * 128 * 3 * 2;
    for method in ["laplacian", "weighted-average", "depth-map"] {
        let small = settled_peak(method, 8, 300, 200, true);
        let large = settled_peak(method, 64, 300, 200, true);
        eprintln!("{method}: peak at 8 frames {small} B, at 64 frames {large} B");
        assert!(
            large < small + 2 * frame_tile,
            "{method}: peak {large} B at 64 frames exceeds the 8-frame peak {small} B by \
             {} B, one frame tile is {frame_tile} B",
            large.saturating_sub(small)
        );
    }
}
