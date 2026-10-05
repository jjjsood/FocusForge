// SPDX-License-Identifier: GPL-3.0-or-later

//! The scheduler-scale test binary: the mantis stack replayed with
//! small synthetic offsets into 61-, 300- and 600-frame inputs, exercising
//! memory behaviour, pause/resume and progress reporting at frame counts no
//! real test data covers.
//!
//! # What this proves, and what it does not
//!
//! This is a **scheduler** test, not a microscopy test. Every replayed frame
//! is one of the same 61 real mantis frames, repeated with a small synthetic
//! [`focusforge_core::align::Transform`] injected through
//! [`focusforge_core::align::WarpingReader`] -- never a genuinely different
//! image and never a solved alignment. The resulting composite
//! is not a photograph anyone should keep: it must never be compared against
//! `Mantis_Stacked.tif` and must never be pinned under `golden/`. It says
//! nothing about fusion quality, alignment accuracy, or how any algorithm
//! behaves on hundreds of genuinely different frames -- **this is not a
//! microscopy test**; that requires real distinct planes.
//!
//! What it *does* prove: the per-frame bookkeeping
//! (strip tables, `job.json`, mmaps -- none of it inside the frame-tile
//! buffers a small run already exercises) scales to 300 and 600 frames
//! without breaking pause, resume or progress reporting, and that peak
//! live heap at 61, 300 and 600 frames stays under the **same
//! frame-count-independent ceiling** (`2 x DEFAULT_MAX_BYTES_IN_FLIGHT`).
//! A tile's bytes do not depend on the frame count, so the in-flight limit
//! is the same at every count; the fast test pins that. Each leg prints
//! `frame_tiles_read` and the decoded bytes the frame cache produced.
//!
//! # Its own global allocator
//!
//! Its own test binary, `#[global_allocator]` and all, for the same reason
//! `tile_scheduler_memory.rs` is: a counting allocator and a peak reading
//! are process-wide, so no other test binary's allocations may share this
//! one. `SERIAL` below stops this file's own tests from perturbing each
//! other the same way a foreign sibling test would.
//!
//! # Safety
//!
//! `GlobalAlloc` is an `unsafe trait`; there is no way to measure live heap
//! bytes without implementing it. Dev-only measurement scaffolding for this
//! one test binary, never shipped as part of `crates/core`'s library crate.
#![allow(unsafe_code)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use focusforge_core::algorithm::{AlgorithmError, StackAlgorithm};
use focusforge_core::align::{Transform, WarpingReader};
use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
use focusforge_core::io::{DecodedTiffStack, FrameReader, TiffStack};
use focusforge_core::job::fingerprint::{FrameFingerprint, StackFingerprint};
use focusforge_core::job::record::JobRecord;
use focusforge_core::job::{Job, JobOutcome, RecordingConsumer, Resumable, ResumeStore};
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::scheduler::DEFAULT_MAX_BYTES_IN_FLIGHT;
use focusforge_core::tile::{
    NoopPreprocess, TileConsumer, TileJob, TileScheduler, TileSchedulerOptions,
};
use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};

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
/// inflate another's peak reading.
static SERIAL: Mutex<()> = Mutex::new(());

/// How many of the plan's 442 tiles each ignored leg runs: bounded, since
/// the replayed composite is worthless and the full plan takes hours. Six
/// legs fit in ~30-60 minutes, and a gated first segment still records
/// some but not all units. Every measurement prints
/// `wall * 442 / SCALE_TILES`.
const SCALE_TILES: usize = 4;

/// Builds `count` entries from `sources`, repeating `sources[i %
/// sources.len()]` for `i in 0..count`. Refuses (never silently truncates)
/// a `count` smaller than `sources.len()`: a caller asking to replay fewer
/// frames than the real source count would end up covering less than it
/// asked to. Writes nothing to disk -- the returned `Vec<PathBuf>` is
/// exactly what [`TiffStack::open_files`] wants.
fn replay_paths(sources: &[PathBuf], count: usize) -> Result<Vec<PathBuf>, String> {
    if sources.is_empty() {
        return Err("replay_paths: sources must be non-empty".to_string());
    }
    if count < sources.len() {
        return Err(format!(
            "replay_paths: refusing to build {count} entries from {} sources -- a request \
             smaller than the source count would silently cover less than asked for",
            sources.len()
        ));
    }
    Ok((0..count)
        .map(|i| sources[i % sources.len()].clone())
        .collect())
}

/// Frame `index`'s synthetic offset when replaying `sources` frames: a
/// fractional (off-grid) translation of `(0.75, -0.5)` per replica
/// `index / sources`. Replica 0 equals `Transform::identity()`
/// (`-0.0 == 0.0`), so the first `sources` frames take [`WarpingReader`]'s
/// bit-identical short-circuit and every later replica runs the bicubic warp.
fn replay_transform(index: usize, sources: usize) -> Transform {
    let replica = (index / sources) as f64;
    Transform::translation(replica * 0.75, replica * -0.5)
}

/// The 61 real reference-stack source frames, `IMG_`-filtered so
/// `Mantis_Stacked.tif` (the reference *result*) is never mistaken for a
/// 62nd frame. `None` when the reference stack directory is unreachable, so
/// every ignored test below skips rather than fails when none is
/// configured.
fn reference_source_paths() -> Option<Vec<PathBuf>> {
    let stack_dir = common::reference_stack_dir()?;
    let paths = focusforge_core::io::frame_paths(&stack_dir)
        .expect("frame_paths must enumerate the reference stack directory")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths.len(),
        61,
        "IMG_-filtered list must be the 61 source frames"
    );
    Some(paths)
}

/// The path list and per-frame transforms for one `frame_count`-frame
/// replay -- `None` when the reference stack is unreachable.
fn build_replay(frame_count: usize) -> Option<(Vec<PathBuf>, Vec<Transform>)> {
    let sources = reference_source_paths()?;
    let paths = replay_paths(&sources, frame_count).expect("replay_paths must succeed");
    let transforms = (0..frame_count)
        .map(|i| replay_transform(i, sources.len()))
        .collect();
    Some((paths, transforms))
}

fn laplacian_algo_and_values(
    stack_ctx: StackContext,
) -> (&'static dyn StackAlgorithm, ParamValues) {
    let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
    let resolved = params::resolve(algo.params(), &ParamValues::new(), &stack_ctx)
        .expect("resolve must succeed");
    (algo, params::resolved_values(&resolved))
}

fn geometry_of(width: u32, height: u32, frame_count: usize) -> FusionGeometry {
    FusionGeometry {
        width,
        height,
        samples_per_pixel: 3,
        frame_count: frame_count as u32,
        crop: None,
        description: None,
    }
}

fn stack_fingerprint(stack: &TiffStack) -> StackFingerprint {
    StackFingerprint {
        frames: (0..stack.len())
            .map(|i| FrameFingerprint::from(FrameReader::frame(stack, i)))
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
fn job_record(
    stack: &TiffStack,
    values: &ParamValues,
    composite_path: &Path,
    zmap_path: Option<&Path>,
    frame_count: usize,
    width: u32,
    height: u32,
) -> JobRecord {
    JobRecord {
        fingerprint: stack_fingerprint(stack),
        width,
        height,
        samples_per_pixel: 3,
        frame_count,
        tile_size: 512,
        levels: 5,
        stack_algorithm: "laplacian".to_string(),
        backend: "cpu".to_string(),
        stack_params: values.clone(),
        align: None,
        crop: None,
        equalization: None,
        composite_output: composite_path.to_path_buf(),
        zmap_output: zmap_path.map(|p| p.to_path_buf()),
    }
}

/// Blocks each `consume` call until [`Gate::release`] is called, and
/// records when a call has begun -- same rendezvous shape as
/// `crates/core/tests/job_resume.rs`'s `Gate`, duplicated here since
/// integration test binaries share no code except `common`.
struct Gate {
    started: Mutex<usize>,
    started_cvar: Condvar,
    released: Mutex<bool>,
    released_cvar: Condvar,
}

impl Gate {
    fn new() -> Self {
        Gate {
            started: Mutex::new(0),
            started_cvar: Condvar::new(),
            released: Mutex::new(false),
            released_cvar: Condvar::new(),
        }
    }

    fn wait_for_started(&self, threshold: usize) {
        let started = self.started.lock().unwrap();
        let (_guard, timed_out) = self
            .started_cvar
            .wait_timeout_while(started, Duration::from_secs(600), |count| {
                *count < threshold
            })
            .unwrap();
        assert!(
            !timed_out.timed_out(),
            "timed out waiting for units to start"
        );
    }

    fn release(&self) {
        let mut released = self.released.lock().unwrap();
        *released = true;
        self.released_cvar.notify_all();
    }

    fn block(&self) {
        {
            let mut started = self.started.lock().unwrap();
            *started += 1;
            self.started_cvar.notify_all();
        }
        let released = self.released.lock().unwrap();
        let (_guard, timed_out) = self
            .released_cvar
            .wait_timeout_while(released, Duration::from_secs(600), |r| !*r)
            .unwrap();
        assert!(
            !timed_out.timed_out(),
            "timed out waiting for the gate to open"
        );
    }
}

struct Gated<'a> {
    inner: &'a dyn TileConsumer,
    gate: &'a Gate,
}

impl TileConsumer for Gated<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        self.gate.block();
        self.inner.consume(job)
    }
    fn abort(&self) {
        self.inner.abort();
    }
}

/// Counts every `consume` call it forwards -- same shape as
/// `crates/core/tests/job_resume.rs`'s `CountingWrap`.
struct CountingWrap<'a> {
    inner: &'a dyn TileConsumer,
    calls: AtomicUsize,
}

impl TileConsumer for CountingWrap<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.consume(job)
    }
    fn abort(&self) {
        self.inner.abort();
    }
}

fn read_vmhwm() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(|l| l.to_string())
        })
        .unwrap_or_else(|| "VmHWM unavailable (not Linux)".to_string())
}

// ---------------------------------------------------------------------
// Fast tests: fixture-scale, no reference stack. These run inside
// `cargo test --workspace` and are what keep the expensive runs below from
// failing on a builder bug hours into an approved run.
// ---------------------------------------------------------------------

#[test]
fn replay_builder_produces_exactly_the_requested_count_in_repeat_order() {
    let sources: Vec<PathBuf> = (0..8)
        .map(|i| PathBuf::from(format!("frame-{i}.tif")))
        .collect();

    let forty = replay_paths(&sources, 40).expect("40 from 8 must succeed");
    assert_eq!(forty.len(), 40);
    for (i, p) in forty.iter().enumerate() {
        assert_eq!(p, &sources[i % 8], "entry {i} must be source {}", i % 8);
    }

    // The partial last repeat: 45 = 5*8 + 5 -- the same shape 300-from-61
    // (4*61+56) and 600-from-61 (9*61+51) both hit.
    let forty_five = replay_paths(&sources, 45).expect("45 from 8 must succeed");
    assert_eq!(forty_five.len(), 45);
    assert_eq!(forty_five[40], sources[0]);
    assert_eq!(forty_five[44], sources[4]);

    // 300-from-61 and 600-from-61 themselves, at the real source count of 61.
    let sixty_one: Vec<PathBuf> = (0..61)
        .map(|i| PathBuf::from(format!("ref-{i}.tif")))
        .collect();
    assert_eq!(replay_paths(&sixty_one, 300).unwrap().len(), 300);
    assert_eq!(replay_paths(&sixty_one, 600).unwrap().len(), 600);
}

#[test]
fn replay_builder_refuses_a_count_below_the_source_count() {
    let sources: Vec<PathBuf> = (0..8)
        .map(|i| PathBuf::from(format!("frame-{i}.tif")))
        .collect();
    let err = replay_paths(&sources, 7).unwrap_err();
    assert!(err.contains('7'), "{err}");
    assert!(err.contains("refus"), "{err}");
}

#[test]
fn replay_transform_identity_at_replica_zero_translation_after() {
    assert_eq!(replay_transform(0, 61), Transform::identity());
    assert_eq!(replay_transform(60, 61), Transform::identity());
    assert_eq!(replay_transform(61, 61), Transform::translation(0.75, -0.5));
    assert_eq!(replay_transform(122, 61), Transform::translation(1.5, -1.0));
}

/// The admission limit the budget predicts for `scheduler`: the budget over
/// one tile's bytes, clamped to `[1, pool_threads]`.
fn predicted_in_flight(scheduler: &TileScheduler<'_>) -> usize {
    (DEFAULT_MAX_BYTES_IN_FLIGHT as u64 / scheduler.bytes_per_tile())
        .clamp(1, scheduler.pool_threads() as u64) as usize
}

#[test]
fn in_flight_limit_is_the_same_at_61_300_and_600_frames() {
    let _guard = SERIAL.lock().unwrap();
    // No frames are ever read: `TileScheduler::new` never touches its
    // reader, so a fixture stack stands in even though its own geometry
    // (512x512, 8 frames) disagrees with the reference geometry passed via
    // `StackContext` -- pure arithmetic over the per-tile bytes.
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");

    let mut seen = Vec::new();
    for frames in [61usize, 300, 600] {
        let scratch_base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("scheduler_scale_in_flight_{frames}"));
        let options = {
            let mut o = TileSchedulerOptions::explicit_budget(
                scratch_base,
                DEFAULT_MAX_BYTES_IN_FLIGHT as u64,
            );
            o.tile_size = 512;
            o.levels = 5;
            o
        };
        let stack_ctx = StackContext::new(frames, 8192, 5464, 3);
        let scheduler =
            TileScheduler::new(&stack, stack_ctx, 3, &options).expect("scheduler must build");
        assert_eq!(
            scheduler.in_flight_limit(),
            predicted_in_flight(&scheduler),
            "budget arithmetic must predict the in-flight limit at {frames} frames"
        );
        seen.push((scheduler.in_flight_limit(), scheduler.bytes_per_tile()));
    }
    assert!(
        seen.windows(2).all(|pair| pair[0] == pair[1]),
        "per-tile bytes and the in-flight limit must not depend on the frame count: {seen:?}"
    );
}

// ---------------------------------------------------------------------
// Ignored: reads the 16 GB reference stack, runs for minutes, `--release`
// only. Run once, at the end, with `--nocapture`, only after the runs have
// been approved -- never inside a red-green loop.
// ---------------------------------------------------------------------

/// What one bounded scale leg measured.
struct ScaleLeg {
    in_flight_limit: usize,
    peak_heap: usize,
    wall: Duration,
    vmhwm: String,
    /// Frames the fusion visited, summed over the run's tiles.
    frame_tiles_read: usize,
    /// Decoded sample bytes the frame cache produced during the run.
    decoded_bytes: u64,
}

/// Decoded-segment cache of the scale legs, small against the heap ceiling.
const SCALE_CACHE_BYTES: u64 = 256 << 20;

/// Runs the first `SCALE_TILES` tiles of the real engine, unchanged, over a
/// `frame_count`-frame replay of the 61 real mantis frames warped through
/// synthetic per-frame offsets. Never calls `consumer.finish()` -- coverage
/// is deliberately incomplete (only `SCALE_TILES` of the plan's 442 tiles
/// are ever written), and `finish()` would refuse exactly as it does after a
/// real cancel. `None` when the reference stack is unreachable.
fn run_scale_leg(frame_count: usize) -> Option<ScaleLeg> {
    let (paths, transforms) = build_replay(frame_count)?;
    let stack =
        DecodedTiffStack::open_files(&paths, SCALE_CACHE_BYTES).expect("replay stack must open");
    let (width, height) = stack.dimensions();
    assert_eq!((width, height), (8192, 5464), "reference frame geometry");
    let reader = WarpingReader::new(&stack, (width, height), 3, transforms);

    let scratch_base =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("scheduler_scale_{frame_count}"));
    let options = {
        let mut o =
            TileSchedulerOptions::explicit_budget(scratch_base, DEFAULT_MAX_BYTES_IN_FLIGHT as u64);
        o.tile_size = 512;
        o.levels = 5;
        o
    };
    let stack_ctx = StackContext::new(frame_count, width, height, 3);
    let scheduler =
        TileScheduler::new(&reader, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(
        scheduler.in_flight_limit(),
        predicted_in_flight(&scheduler),
        "budget arithmetic must predict the in-flight limit at {frame_count} frames"
    );

    let (algo, values) = laplacian_algo_and_values(stack_ctx);
    let composite_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("scheduler_scale_{frame_count}.partial.tif"));
    let consumer = FusionConsumer::create(
        algo,
        values,
        &composite_path,
        None,
        geometry_of(width, height, frame_count),
    )
    .expect("consumer must create");

    enable_trace_accounting();
    let decoded_before = timing_snapshot()
        .expect("accounting is enabled")
        .decoded_bytes;
    reset_peak();
    let start = Instant::now();
    let report = scheduler
        .run_tiles(
            &scheduler.plan().tiles()[..SCALE_TILES],
            &NoopPreprocess,
            &consumer,
        )
        .expect("bounded run must succeed");
    let wall = start.elapsed();
    let peak_heap = peak_since_reset();
    let decoded_bytes = timing_snapshot()
        .expect("accounting is enabled")
        .decoded_bytes
        - decoded_before;
    assert_eq!(
        report.tiles, SCALE_TILES,
        "the bounded run submits exactly SCALE_TILES units"
    );

    let vmhwm = read_vmhwm();
    drop(consumer);
    let _ = std::fs::remove_file(&composite_path);

    Some(ScaleLeg {
        in_flight_limit: scheduler.in_flight_limit(),
        peak_heap,
        wall,
        vmhwm,
        frame_tiles_read: report.frame_tiles_read,
        decoded_bytes,
    })
}

/// Runs one scale leg and checks it against the frame-count-independent
/// heap ceiling, which sits below a full load of `frame_count` frames.
fn check_scale_leg(name: &str, frame_count: usize) {
    let _guard = SERIAL.lock().unwrap();
    // Derived, not pinned: `2 * DEFAULT_MAX_BYTES_IN_FLIGHT` is a function
    // of the budget, not of the frame count, so the same ceiling applies at
    // 61, 300 and 600 frames.
    let ceiling = 2 * DEFAULT_MAX_BYTES_IN_FLIGHT;
    let full_load_bytes = frame_count as u64 * 8192 * 5464 * 3 * 2;
    assert!(
        (ceiling as u64) < full_load_bytes,
        "ceiling ({ceiling} B) must be below a full load ({full_load_bytes} B), or this test \
         cannot discriminate a tiled implementation from a full-load one"
    );

    let Some(leg) = run_scale_leg(frame_count) else {
        return;
    };
    let extrapolated = leg.wall.as_secs_f64() * 442.0 / SCALE_TILES as f64;
    println!(
        "{name}: in_flight_limit={} peak_live_heap={} B (ceiling {ceiling} B) wall={:?} \
         ({SCALE_TILES} of 442 tiles) extrapolated_442_tiles={extrapolated:.1}s \
         frame_tiles_read={} decoded_bytes={} {}",
        leg.in_flight_limit,
        leg.peak_heap,
        leg.wall,
        leg.frame_tiles_read,
        leg.decoded_bytes,
        leg.vmhwm
    );
    assert!(
        leg.peak_heap < ceiling,
        "peak live heap {} B exceeded the {ceiling} B ceiling",
        leg.peak_heap
    );
    assert!(
        leg.frame_tiles_read <= SCALE_TILES * frame_count,
        "frame_tiles_read {} exceeds one visit per frame per tile",
        leg.frame_tiles_read
    );
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn scale_61_frames() {
    check_scale_leg("scale_61_frames", 61);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn scale_300_frames() {
    check_scale_leg("scale_300_frames", 300);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn scale_600_frames() {
    check_scale_leg("scale_600_frames", 600);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn scale_600_pause_latency() {
    let _guard = SERIAL.lock().unwrap();
    let frame_count = 600usize;
    let Some((paths, transforms)) = build_replay(frame_count) else {
        return;
    };
    let stack = TiffStack::open_files(&paths).expect("replay stack must open");
    let (width, height) = stack.dimensions();
    let reader = WarpingReader::new(&stack, (width, height), 3, transforms);

    let scratch_base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("scheduler_scale_600_pause");
    let options = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch_base, 1_u64);
        o.tile_size = 512;
        o.levels = 5;
        o
    };
    let stack_ctx = StackContext::new(frame_count, width, height, 3);
    let scheduler =
        TileScheduler::new(&reader, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(scheduler.in_flight_limit(), 1);

    let (algo, values) = laplacian_algo_and_values(stack_ctx);
    let composite_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("scheduler_scale_600_pause.partial.tif");
    let consumer = FusionConsumer::create(
        algo,
        values,
        &composite_path,
        None,
        geometry_of(width, height, frame_count),
    )
    .expect("consumer must create");

    let plan_len = scheduler.plan().len();
    let pre_credited: Vec<usize> = (SCALE_TILES..plan_len).collect();
    let mut job = Job::resuming(&scheduler, &pre_credited);
    let control = job.control();
    let start_units = control.progress().units_done;

    // No gate: poll until a unit of *this* segment completes, then `pause()`
    // and time until `Job::run` returns. At most `in_flight_limit` further
    // units complete after `pause()`'s store returns (`JobControl`'s docs), so
    // "further" is counted from right before that call, not from the segment
    // start: the unit that woke this poll is not itself a further one.
    let (outcome, pause_latency, units_at_pause_call) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| job.run(&NoopPreprocess, &consumer));
        loop {
            if control.progress().units_done > start_units {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let units_at_pause_call = control.progress().units_done;
        let pause_start = Instant::now();
        control.pause();
        let outcome = handle.join().unwrap().expect("segment must not error");
        (outcome, pause_start.elapsed(), units_at_pause_call)
    });

    assert_eq!(outcome, JobOutcome::Paused, "outcome={outcome:?}");
    let units_completed_after_pause = control.progress().units_done - units_at_pause_call;
    assert!(
        units_completed_after_pause <= scheduler.in_flight_limit(),
        "at most in_flight_limit ({}) further units may complete after pause() returns, got {}",
        scheduler.in_flight_limit(),
        units_completed_after_pause
    );

    println!(
        "scale_600_pause_latency: pause_latency={pause_latency:?} \
         units_completed_after_pause={units_completed_after_pause} (bound {})",
        scheduler.in_flight_limit()
    );

    drop(consumer);
    let _ = std::fs::remove_file(&composite_path);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn scale_600_resume_continues() {
    let _guard = SERIAL.lock().unwrap();
    let frame_count = 600usize;
    let Some((paths, transforms)) = build_replay(frame_count) else {
        return;
    };

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("scheduler_scale_600_resume");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();

    let stack = TiffStack::open_files(&paths).expect("replay stack must open");
    let (width, height) = stack.dimensions();
    let reader = WarpingReader::new(&stack, (width, height), 3, transforms.clone());
    let options = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1_u64);
        o.tile_size = 512;
        o.levels = 5;
        o
    };
    let stack_ctx = StackContext::new(frame_count, width, height, 3);
    let scheduler =
        TileScheduler::new(&reader, stack_ctx, 3, &options).expect("scheduler must build");
    assert_eq!(scheduler.in_flight_limit(), 1);

    let (algo, values) = laplacian_algo_and_values(stack_ctx);
    let composite_path = scratch.join("out.partial.tif");
    let plan_len = scheduler.plan().len();
    let pre_credited: Vec<usize> = (SCALE_TILES..plan_len).collect();

    {
        // First "process": a gated consumer stops after exactly one of the
        // SCALE_TILES bounded units via the Gate rendezvous, never a sleep.
        let record = job_record(
            &stack,
            &values,
            &composite_path,
            None,
            frame_count,
            width,
            height,
        );
        let store = ResumeStore::open(&scratch).unwrap();
        let ledger = store.begin(&record).unwrap();
        let consumer = FusionConsumer::create(
            algo,
            values.clone(),
            &composite_path,
            None,
            geometry_of(width, height, frame_count),
        )
        .unwrap();
        let recording = RecordingConsumer {
            inner: &consumer,
            ledger: &ledger,
        };
        let gate = Gate::new();
        let gated = Gated {
            inner: &recording,
            gate: &gate,
        };
        let mut job = Job::resuming(&scheduler, &pre_credited);
        let control = job.control();
        // A one-byte budget pins `in_flight_limit()` to 1: exactly one worker, so
        // `wait_for_started` must ask for exactly 1 -- a threshold above the
        // scheduler's own `in_flight_limit` can never be reached, since the
        // one worker blocks *inside* `Gated::consume` and cannot pick up a
        // second tile until this segment's `gate.release()` lets it finish
        // the first (same reasoning as `crates/core/tests/job_resume.rs`'s
        // own `resume_on_the_reference_stack`, `gate.wait_for_started(1)` at
        // its own `max_bytes_in_flight: 1`).
        assert_eq!(scheduler.in_flight_limit(), 1);
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
            gate.wait_for_started(1);
            control.pause();
            gate.release();
            let outcome = handle.join().unwrap().expect("segment must not error");
            assert_eq!(outcome, JobOutcome::Paused);
        });
        // Deliberately *not* read here: `Ledger::completed()` is a snapshot
        // taken once, when this `ledger` was opened (`Ledger::open`'s own
        // docs) -- `RecordingConsumer::record_unit` appends to the on-disk
        // log without updating that in-memory field, so reading it from
        // *this* handle would always report the empty pre-crash state. The
        // real count is read back after the crash, from a freshly reopened
        // `ledger2` below -- the same two-ledger shape
        // `crates/core/tests/job_resume.rs`'s own resume tests use.
        // Crash: `consumer`, `ledger`, `job` all dropped here, unfinished.
    }

    // Second "process": resume, over a freshly reopened stack.
    let stack2 = TiffStack::open_files(&paths).expect("replay stack must reopen");
    let reader2 = WarpingReader::new(&stack2, (width, height), 3, transforms);
    let scheduler2 =
        TileScheduler::new(&reader2, stack_ctx, 3, &options).expect("resume scheduler must build");
    let record2 = job_record(
        &stack2,
        &values,
        &composite_path,
        None,
        frame_count,
        width,
        height,
    );
    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record2).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    };
    let job_json_len = std::fs::metadata(ledger2.dir().join("job.json"))
        .unwrap()
        .len();
    let ledger_completed = ledger2.completed().to_vec();
    let completed_len = ledger_completed.len();
    assert!(
        completed_len > 0 && completed_len < SCALE_TILES,
        "the killed segment must record some but not all of the {SCALE_TILES} bounded units, \
         got {completed_len}"
    );

    let (resumed_consumer, surviving) = FusionConsumer::resume(
        scheduler2.plan(),
        &ledger_completed,
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry_of(width, height, frame_count),
    )
    .unwrap();
    let recording2 = RecordingConsumer {
        inner: &resumed_consumer,
        ledger: &ledger2,
    };

    // The union: tiles the pre-credited bound already skips, plus the
    // ledger's genuinely-surviving units -- *not* the same set once the
    // union bounds the run.
    let mut resuming_skip: HashSet<usize> = pre_credited.iter().copied().collect();
    resuming_skip.extend(surviving.iter().copied());
    let resuming_skip: Vec<usize> = resuming_skip.into_iter().collect();

    let counting = CountingWrap {
        inner: &recording2,
        calls: AtomicUsize::new(0),
    };
    let mut job2 = Job::resuming(&scheduler2, &resuming_skip);
    let outcome2 = job2
        .run(&NoopPreprocess, &counting)
        .expect("resumed run must succeed");
    assert_eq!(outcome2, JobOutcome::Completed);

    let recomputed = counting.calls.load(Ordering::SeqCst);
    let expected_remaining = SCALE_TILES - completed_len;
    assert_eq!(
        recomputed, expected_remaining,
        "resumed run must execute exactly SCALE_TILES - completed units"
    );

    // Never `resumed_consumer.finish()`: even after the resume, only
    // SCALE_TILES of the plan's 442 tiles were ever written -- coverage
    // stays deliberately incomplete.
    drop(resumed_consumer);
    ledger2.discard().unwrap();

    println!(
        "scale_600_resume_continues: SCALE_TILES={SCALE_TILES} \
         completed_before_resume={completed_len} recomputed={recomputed} \
         job_json_bytes={job_json_len}"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn scale_progress_reaches_the_total() {
    let _guard = SERIAL.lock().unwrap();
    let mut first_limit = None;
    for &frame_count in &[300usize, 600usize] {
        let Some((paths, transforms)) = build_replay(frame_count) else {
            return;
        };
        let stack = TiffStack::open_files(&paths).expect("replay stack must open");
        let (width, height) = stack.dimensions();
        let reader = WarpingReader::new(&stack, (width, height), 3, transforms);

        let scratch_base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("scheduler_scale_progress_{frame_count}"));
        let options = {
            let mut o = TileSchedulerOptions::explicit_budget(
                scratch_base,
                DEFAULT_MAX_BYTES_IN_FLIGHT as u64,
            );
            o.tile_size = 512;
            o.levels = 5;
            o
        };
        let stack_ctx = StackContext::new(frame_count, width, height, 3);
        let scheduler =
            TileScheduler::new(&reader, stack_ctx, 3, &options).expect("scheduler must build");
        let limit = scheduler.in_flight_limit();
        assert_eq!(*first_limit.get_or_insert(limit), limit);

        let (algo, values) = laplacian_algo_and_values(stack_ctx);
        let composite_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "scheduler_scale_progress_{frame_count}.partial.tif"
        ));
        let consumer = FusionConsumer::create(
            algo,
            values,
            &composite_path,
            None,
            geometry_of(width, height, frame_count),
        )
        .unwrap();

        let plan_len = scheduler.plan().len();
        let pre_credited: Vec<usize> = (SCALE_TILES..plan_len).collect();
        let mut job = Job::resuming(&scheduler, &pre_credited);
        let control = job.control();

        let samples = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| job.run(&NoopPreprocess, &consumer));
            loop {
                let p = control.progress();
                samples.lock().unwrap().push(p.units_done);
                if p.units_done >= p.unit_total {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let outcome = handle.join().unwrap().expect("run must succeed");
            assert_eq!(outcome, JobOutcome::Completed);
        });

        let final_progress = control.progress();
        assert_eq!(final_progress.units_done, final_progress.unit_total);
        assert_eq!(final_progress.eta, Some(Duration::ZERO));

        let samples = samples.into_inner().unwrap();
        assert!(
            samples.windows(2).all(|w| w[0] <= w[1]),
            "units_done must be non-decreasing at {frame_count} frames: {samples:?}"
        );

        println!(
            "scale_progress_reaches_the_total: {frame_count} frames, {} samples, \
             final={}/{}",
            samples.len(),
            final_progress.units_done,
            final_progress.unit_total
        );

        drop(consumer);
        let _ = std::fs::remove_file(&composite_path);
    }
}
