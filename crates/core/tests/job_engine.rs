// SPDX-License-Identifier: GPL-3.0-or-later

//! Behavioural suite for `focusforge_core::job` over `fixtures/mantis-512`
//! at tile size 200 / `levels = 3` (16 tiles, below the 512 px crop the
//! fixture allows).
//!
//! The pause point in every test that needs one is driven by **units
//! completed** -- a rendezvous gate on completed-tile counts -- never by a
//! sleep, so no test here depends on machine speed.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use focusforge_core::algorithm::AlgorithmError;
use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
use focusforge_core::io::TiffStack;
use focusforge_core::job::{Job, JobOutcome};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{
    NoopPreprocess, TileConsumer, TileError, TileJob, TileScheduler, TileSchedulerOptions,
};

fn scratch_base(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("job_engine_scratch_{label}"))
}

fn options(label: &str, max_bytes_in_flight: usize) -> TileSchedulerOptions {
    let mut options =
        TileSchedulerOptions::explicit_budget(scratch_base(label), max_bytes_in_flight as u64);
    options.tile_size = 200;
    options.levels = 3;
    options
}

fn open_stack() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open")
}

fn build_scheduler<'a>(stack: &'a TiffStack, opts: &TileSchedulerOptions) -> TileScheduler<'a> {
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    TileScheduler::new(stack, stack_ctx, 3, opts).expect("scheduler must build")
}

/// A real `FusionConsumer` over the `laplacian` algorithm, matched to this
/// file's tile geometry (`levels = 3`, same as the scheduler's own
/// `options`).
fn laplacian_consumer(
    stack_ctx: StackContext,
    composite_path: &std::path::Path,
    zmap_path: Option<&std::path::Path>,
) -> FusionConsumer {
    let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(3));
    let resolved =
        params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);
    FusionConsumer::create(
        algo,
        values,
        composite_path,
        zmap_path,
        FusionGeometry {
            width: stack_ctx.width,
            height: stack_ctx.height,
            samples_per_pixel: 3,
            frame_count: stack_ctx.frame_count as u32,
            crop: None,
            description: None,
        },
    )
    .expect("consumer must create")
}

/// Does nothing, successfully -- for tests that only care about unit
/// counts and control timing, never about fused pixels.
struct Noop;

impl TileConsumer for Noop {
    fn consume(&self, _job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        Ok(())
    }
}

/// Blocks each `consume` call until [`Gate::release`] is called, and
/// records when a call has begun -- so a test can deterministically know
/// "at least N units are in flight" before it requests a pause or cancel,
/// with no sleep anywhere. Bounded by a timeout so a bug here surfaces as a
/// loud assertion failure rather than a hung test suite.
struct Gate {
    started: Mutex<usize>,
    started_cvar: Condvar,
    released: Mutex<bool>,
    released_cvar: Condvar,
    completed: AtomicUsize,
}

impl Gate {
    fn new() -> Self {
        Gate {
            started: Mutex::new(0),
            started_cvar: Condvar::new(),
            released: Mutex::new(false),
            released_cvar: Condvar::new(),
            completed: AtomicUsize::new(0),
        }
    }

    fn wait_for_started(&self, threshold: usize) {
        let started = self.started.lock().unwrap();
        let (_guard, timed_out) = self
            .started_cvar
            .wait_timeout_while(started, Duration::from_secs(10), |count| *count < threshold)
            .unwrap();
        assert!(
            !timed_out.timed_out(),
            "timed out waiting for {threshold} unit(s) to start"
        );
    }

    /// Opens the gate permanently: every call already waiting, and every
    /// future call, proceeds without blocking.
    fn release(&self) {
        let mut released = self.released.lock().unwrap();
        *released = true;
        self.released_cvar.notify_all();
    }

    fn completed(&self) -> usize {
        self.completed.load(Ordering::SeqCst)
    }

    /// Marks one call as started, then blocks until [`Self::release`] is
    /// called -- the body [`Gated::consume`] and [`GatedFailure::consume`]
    /// both need, once to record arrival and once to hold the tile in
    /// flight until the test is ready to let it finish.
    fn block(&self) {
        {
            let mut started = self.started.lock().unwrap();
            *started += 1;
            self.started_cvar.notify_all();
        }
        let released = self.released.lock().unwrap();
        let (_guard, timed_out) = self
            .released_cvar
            .wait_timeout_while(released, Duration::from_secs(10), |r| !*r)
            .unwrap();
        assert!(
            !timed_out.timed_out(),
            "timed out waiting for the gate to open"
        );
    }
}

/// Wraps a real [`TileConsumer`] with a [`Gate`] around each `consume`
/// call.
struct Gated<'a> {
    inner: &'a dyn TileConsumer,
    gate: &'a Gate,
}

impl TileConsumer for Gated<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        self.gate.block();
        let result = self.inner.consume(job);
        if result.is_ok() {
            self.gate.completed.fetch_add(1, Ordering::SeqCst);
        }
        result
    }

    fn abort(&self) {
        self.inner.abort();
    }
}

/// Fails deterministically for one plan index (never by arrival order,
/// same discipline as `crates/core/tests/tile_scheduler.rs`'s
/// `FailingOneTile`), and counts how many times `abort` is called.
struct AbortRecorder<'a> {
    inner: &'a dyn TileConsumer,
    abort_calls: AtomicUsize,
    fail_index: Option<usize>,
}

impl TileConsumer for AbortRecorder<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        if Some(job.tile().index()) == self.fail_index {
            return Err(AlgorithmError::Failed {
                algorithm: "abort-recorder".to_string(),
                detail: "intentional test failure".to_string(),
            });
        }
        self.inner.consume(job)
    }

    fn abort(&self) {
        self.abort_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.abort();
    }
}

/// Gated exactly like [`Gate`]/[`Gated`], but only for the one tile named
/// `target_index` -- every other tile runs unblocked. This is what makes
/// "a genuine failure is already in flight when `pause()` is called"
/// deterministic: `target_index`'s tile is guaranteed to have passed the
/// control check and entered `consume` before the test releases it, however
/// rayon happens to schedule the rest.
struct GatedFailure<'a> {
    target_index: usize,
    gate: &'a Gate,
}

impl TileConsumer for GatedFailure<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        if job.tile().index() != self.target_index {
            return Ok(());
        }
        self.gate.block();
        Err(AlgorithmError::Failed {
            algorithm: "gated-failure".to_string(),
            detail: "intentional test failure".to_string(),
        })
    }
}

#[test]
fn progress_reaches_the_unit_total() {
    let stack = open_stack();
    let opts = options("progress", 1);
    let scheduler = build_scheduler(&stack, &opts);
    let plan_len = scheduler.plan().len();

    let mut job = Job::new(&scheduler);
    let control = job.control();

    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let stop_polling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = {
        let control = Arc::clone(&control);
        let snapshots = Arc::clone(&snapshots);
        let stop_polling = Arc::clone(&stop_polling);
        std::thread::spawn(move || {
            while !stop_polling.load(Ordering::SeqCst) {
                snapshots.lock().unwrap().push(control.progress());
                std::thread::sleep(Duration::from_micros(200));
            }
            snapshots.lock().unwrap().push(control.progress());
        })
    };

    let composite_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("progress_reaches_the_unit_total.tif");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let consumer = laplacian_consumer(stack_ctx, &composite_path, None);

    let outcome = job
        .run(&NoopPreprocess, &consumer)
        .expect("uninterrupted run must succeed");
    stop_polling.store(true, Ordering::SeqCst);
    poller.join().unwrap();

    assert_eq!(outcome, JobOutcome::Completed);
    consumer.finish().expect("finish must succeed");

    let history = snapshots.lock().unwrap();
    assert!(!history.is_empty());
    let mut prev = 0usize;
    for snap in history.iter() {
        assert_eq!(
            snap.unit_total, plan_len,
            "unit_total must equal the plan's tile count in every snapshot"
        );
        assert!(snap.units_done >= prev, "units done must be non-decreasing");
        prev = snap.units_done;
    }
    assert_eq!(
        history.last().unwrap().units_done,
        plan_len,
        "the run must end exactly at the unit total"
    );
}

/// Progress must be **live**, not a per-segment summary: a caller polling
/// from another thread sees completed units rise during a single
/// uninterrupted segment, never `0/16` until the end.
#[test]
fn progress_advances_while_one_segment_is_still_running() {
    use std::collections::BTreeSet;

    let stack = open_stack();
    let opts = options("progress-live", 1);
    let scheduler = build_scheduler(&stack, &opts);
    let plan_len = scheduler.plan().len();
    assert!(plan_len > 4, "this test needs a multi-unit plan");

    let mut job = Job::new(&scheduler);
    let control = job.control();

    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let stop_polling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = {
        let control = Arc::clone(&control);
        let snapshots = Arc::clone(&snapshots);
        let stop_polling = Arc::clone(&stop_polling);
        std::thread::spawn(move || {
            while !stop_polling.load(Ordering::SeqCst) {
                snapshots.lock().unwrap().push(control.progress());
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };

    let composite_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("progress_advances_while_one_segment_is_still_running.tif");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let consumer = laplacian_consumer(stack_ctx, &composite_path, None);

    let outcome = job
        .run(&NoopPreprocess, &consumer)
        .expect("uninterrupted run must succeed");
    // Stop polling only *after* the single segment has returned, so every
    // snapshot below was genuinely taken with the run in flight.
    stop_polling.store(true, Ordering::SeqCst);
    poller.join().unwrap();
    assert_eq!(outcome, JobOutcome::Completed);
    consumer.finish().expect("finish must succeed");

    let history = snapshots.lock().unwrap();
    let intermediate: Vec<_> = history
        .iter()
        .filter(|snap| snap.units_done > 0 && snap.units_done < plan_len)
        .collect();
    let distinct: BTreeSet<usize> = intermediate.iter().map(|snap| snap.units_done).collect();
    assert!(
        distinct.len() >= 2,
        "progress must advance per completed unit, not once per segment: saw {distinct:?} \
         strictly between 0 and {plan_len} across {} snapshots",
        history.len()
    );
    assert!(
        intermediate
            .iter()
            .all(|snap| snap.pixels_done > 0 && snap.eta.is_some()),
        "every snapshot with a completed unit must carry a completed pixel count and an ETA"
    );
    assert!(
        intermediate
            .iter()
            .any(|snap| snap.running > Duration::ZERO && snap.eta > Some(Duration::ZERO)),
        "the running clock must advance mid-segment too -- otherwise the ETA it feeds is \
         rendered as `0s` for the whole run"
    );
}

#[test]
fn engine_output_is_byte_identical() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);

    // Reference side: `TileScheduler::run` directly, never the engine with
    // controls unused.
    let ref_opts = options("byte-identical-reference", 1 << 30);
    let ref_scheduler = build_scheduler(&stack, &ref_opts);
    let ref_composite =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("byte_identical_reference.tif");
    let ref_zmap =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("byte_identical_reference-zmap.tif");
    let ref_consumer = laplacian_consumer(stack_ctx, &ref_composite, Some(&ref_zmap));
    ref_scheduler
        .run(&NoopPreprocess, &ref_consumer)
        .expect("reference run must succeed");
    ref_consumer
        .finish()
        .expect("reference finish must succeed");

    // Engine side.
    let job_opts = options("byte-identical-engine", 1 << 30);
    let job_scheduler = build_scheduler(&stack, &job_opts);
    let mut job = Job::new(&job_scheduler);
    let job_composite =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("byte_identical_engine.tif");
    let job_zmap =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("byte_identical_engine-zmap.tif");
    let job_consumer = laplacian_consumer(stack_ctx, &job_composite, Some(&job_zmap));
    let outcome = job
        .run(&NoopPreprocess, &job_consumer)
        .expect("engine run must succeed");
    assert_eq!(outcome, JobOutcome::Completed);
    job_consumer.finish().expect("engine finish must succeed");

    assert_eq!(
        std::fs::read(&ref_composite).unwrap(),
        std::fs::read(&job_composite).unwrap(),
        "composite output must be byte-identical with and without the engine"
    );
    assert_eq!(
        std::fs::read(&ref_zmap).unwrap(),
        std::fs::read(&job_zmap).unwrap(),
        "Z-Map output must be byte-identical with and without the engine"
    );
}

/// Delegates to `inner` and records the most `consume` calls running at once.
struct PeakTracking<'a> {
    inner: &'a dyn TileConsumer,
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl TileConsumer for PeakTracking<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(2));
        let result = self.inner.consume(job);
        self.live.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

#[test]
fn capped_admission_is_byte_identical() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);

    let ref_opts = options("capped-reference", 1 << 30);
    let ref_scheduler = build_scheduler(&stack, &ref_opts);
    let ref_composite = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("capped_reference.tif");
    let ref_zmap = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("capped_reference-zmap.tif");
    let ref_consumer = laplacian_consumer(stack_ctx, &ref_composite, Some(&ref_zmap));
    ref_scheduler
        .run(&NoopPreprocess, &ref_consumer)
        .expect("reference run must succeed");
    ref_consumer
        .finish()
        .expect("reference finish must succeed");

    let job_opts = options("capped-engine", 1 << 30);
    let job_scheduler = build_scheduler(&stack, &job_opts);
    if job_scheduler.in_flight_limit() < 2 {
        eprintln!("host reports too little parallelism to admit several tiles; skipping");
        return;
    }
    let mut job = Job::new(&job_scheduler);
    job.control().set_admission_cap(Some(1));
    let job_composite = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("capped_engine.tif");
    let job_zmap = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("capped_engine-zmap.tif");
    let job_consumer = laplacian_consumer(stack_ctx, &job_composite, Some(&job_zmap));
    let tracked = PeakTracking {
        inner: &job_consumer,
        live: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let outcome = job
        .run(&NoopPreprocess, &tracked)
        .expect("capped engine run must succeed");
    assert_eq!(outcome, JobOutcome::Completed);
    let peak = tracked.peak.load(Ordering::SeqCst);
    job_consumer.finish().expect("engine finish must succeed");

    assert_eq!(peak, 1, "a cap of 1 admits one tile at a time");
    assert_eq!(
        std::fs::read(&ref_composite).unwrap(),
        std::fs::read(&job_composite).unwrap(),
        "composite output must be byte-identical capped and uncapped"
    );
    assert_eq!(
        std::fs::read(&ref_zmap).unwrap(),
        std::fs::read(&job_zmap).unwrap(),
        "Z-Map output must be byte-identical capped and uncapped"
    );
}

#[test]
fn pause_lands_within_the_unit_bound() {
    let stack = open_stack();

    // Exactly one worker: the pause bound is exact in this case.
    let opts = options("pause-bound-one", 1);
    let scheduler = build_scheduler(&stack, &opts);
    assert_eq!(scheduler.in_flight_limit(), 1);

    let gate = Gate::new();
    let noop = Noop;
    let consumer = Gated {
        inner: &noop,
        gate: &gate,
    };
    let mut job = Job::new(&scheduler);
    let control = job.control();

    std::thread::scope(|scope| {
        let handle = scope.spawn(|| job.run(&NoopPreprocess, &consumer));
        gate.wait_for_started(1);
        control.pause();
        gate.release();
        let outcome = handle
            .join()
            .unwrap()
            .expect("a pause must not be an error");
        assert_eq!(outcome, JobOutcome::Paused);
    });
    assert!(
        gate.completed() <= 1,
        "at most 1 further unit may complete once pause() has returned, at in_flight_limit=1; \
         got {}",
        gate.completed()
    );

    // More than one worker: at most `in_flight_limit` further units
    // complete.
    let bytes_per_tile = 200u64 * 200 * (3 * 2 + 1);
    let opts2 = options("pause-bound-many", (bytes_per_tile * 4) as usize);
    let scheduler2 = build_scheduler(&stack, &opts2);
    let limit = scheduler2.in_flight_limit();
    if limit < 2 {
        eprintln!("host reports too little parallelism for the >1-worker case; skipping");
        return;
    }

    let gate2 = Gate::new();
    let consumer2 = Gated {
        inner: &noop,
        gate: &gate2,
    };
    let mut job2 = Job::new(&scheduler2);
    let control2 = job2.control();

    std::thread::scope(|scope| {
        let handle = scope.spawn(|| job2.run(&NoopPreprocess, &consumer2));
        gate2.wait_for_started(limit);
        control2.pause();
        gate2.release();
        let outcome = handle
            .join()
            .unwrap()
            .expect("a pause must not be an error");
        assert!(matches!(
            outcome,
            JobOutcome::Paused | JobOutcome::Completed
        ));
    });
    assert!(
        gate2.completed() <= limit,
        "at most in_flight_limit={limit} further units may complete once pause() has returned; \
         got {}",
        gate2.completed()
    );
}

/// rayon 1.12.0 does not specify which of several parallel closure results
/// `try_for_each_init` returns, so a tile that genuinely fails while
/// another observes a pause request could, in principle, be swallowed and
/// reported as a clean pause. The mitigation (the first real error
/// recorded in a side channel, consulted unconditionally) is what this test
/// exercises end to end: a designated tile is guaranteed in flight (already
/// past the control check, blocked in `consume`) when `pause()` is called,
/// so the run necessarily has both a genuine failure and a stop request
/// alive in the same batch.
#[test]
fn error_wins_over_a_racing_pause_request() {
    let stack = open_stack();
    let bytes_per_tile = 200u64 * 200 * (3 * 2 + 1);
    let opts = options("error-wins-over-pause", (bytes_per_tile * 4) as usize);
    let scheduler = build_scheduler(&stack, &opts);
    if scheduler.in_flight_limit() < 2 {
        eprintln!("host reports too little parallelism for this race; skipping");
        return;
    }
    let target_index = scheduler.plan().len() / 2;

    let gate = Gate::new();
    let consumer = GatedFailure {
        target_index,
        gate: &gate,
    };
    let mut job = Job::new(&scheduler);
    let control = job.control();

    let result = std::thread::scope(|scope| {
        let handle = scope.spawn(|| job.run(&NoopPreprocess, &consumer));
        gate.wait_for_started(1);
        control.pause();
        gate.release();
        handle.join().unwrap()
    });

    match result {
        Err(TileError::Consumer { .. }) => {}
        other => panic!(
            "a genuine failure racing a pause request must never be reported as a clean pause; \
             got {other:?}"
        ),
    }
}

#[test]
fn pause_then_resume_matches_uninterrupted_output() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);

    // Reference: uninterrupted `TileScheduler::run`.
    let ref_opts = options("resume-reference", 1 << 30);
    let ref_scheduler = build_scheduler(&stack, &ref_opts);
    let ref_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("resume_reference.tif");
    let ref_consumer = laplacian_consumer(stack_ctx, &ref_path, None);
    ref_scheduler
        .run(&NoopPreprocess, &ref_consumer)
        .expect("reference run must succeed");
    ref_consumer
        .finish()
        .expect("reference finish must succeed");

    // Paused-then-resumed job, single worker so the pause point is exact.
    let job_opts = options("resume-job", 1);
    let job_scheduler = build_scheduler(&stack, &job_opts);
    let plan_len = job_scheduler.plan().len();
    let job_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("resume_job.tif");
    let job_consumer = laplacian_consumer(stack_ctx, &job_path, None);

    let gate = Gate::new();
    let gated = Gated {
        inner: &job_consumer,
        gate: &gate,
    };
    let mut job = Job::new(&job_scheduler);
    let control = job.control();

    let first_outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
        gate.wait_for_started(1);
        control.pause();
        gate.release();
        handle.join().unwrap().expect("first segment must succeed")
    });
    assert_eq!(first_outcome, JobOutcome::Paused);
    assert!(gate.completed() >= 1 && gate.completed() < plan_len);

    // Resume: `control.clear()` then `job.run(...)` again, same consumer.
    control.clear();
    let second_outcome = job
        .run(&NoopPreprocess, &gated)
        .expect("resumed segment must succeed");
    assert_eq!(second_outcome, JobOutcome::Completed);
    assert_eq!(
        gate.completed(),
        plan_len,
        "the resumed segment must run only the units the first segment did not complete"
    );

    job_consumer.finish().expect("job finish must succeed");

    assert_eq!(
        std::fs::read(&ref_path).unwrap(),
        std::fs::read(&job_path).unwrap(),
        "a paused-then-resumed run must match an uninterrupted one byte-for-byte"
    );
}

#[test]
fn cancel_writes_nothing_at_the_named_output_path() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let opts = options("cancel-output-path", 1);
    let scheduler = build_scheduler(&stack, &opts);

    let named_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cancel_named_output.tif");
    let partial_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cancel_named_output.tif.partial");
    let _ = std::fs::remove_file(&named_path);
    let _ = std::fs::remove_file(&partial_path);
    let consumer = laplacian_consumer(stack_ctx, &partial_path, None);

    let mut job = Job::new(&scheduler);
    // Requested before the first unit starts -- a cancel that lands
    // immediately still must not create the named output.
    job.control().cancel();
    let outcome = job
        .run(&NoopPreprocess, &consumer)
        .expect("cancel must not be an error");

    assert_eq!(outcome, JobOutcome::Cancelled);
    assert!(
        !named_path.exists(),
        "no file must appear at the previously-unwritten named output path after a cancel"
    );
    assert!(
        partial_path.exists(),
        "the .partial file must survive a clean cancel, for a later resume"
    );
}

#[test]
fn cancel_is_not_an_error() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let opts = options("cancel-not-error", 1);
    let scheduler = build_scheduler(&stack, &opts);
    let plan_len = scheduler.plan().len();
    let path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cancel_is_not_an_error.tif.partial");
    let consumer = laplacian_consumer(stack_ctx, &path, None);

    let gate = Gate::new();
    let gated = Gated {
        inner: &consumer,
        gate: &gate,
    };
    let mut job = Job::new(&scheduler);
    let control = job.control();

    let outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
        gate.wait_for_started(1);
        control.cancel();
        gate.release();
        handle.join().unwrap()
    });

    match outcome {
        Ok(JobOutcome::Cancelled) => {}
        other => panic!("expected Ok(JobOutcome::Cancelled), got {other:?}"),
    }
    assert!(
        gate.completed() < plan_len,
        "a cancel must stop the run before every unit completes for this test to mean anything"
    );
    assert_eq!(
        control.progress().units_done,
        gate.completed(),
        "the cancelled job's unit count must be preserved, matching exactly what completed"
    );
}

#[test]
fn cancel_before_the_first_unit_runs_nothing() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let opts = options("cancel-before-first", 1 << 30);
    let scheduler = build_scheduler(&stack, &opts);
    let path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cancel_before_the_first_unit.tif.partial");
    let consumer = laplacian_consumer(stack_ctx, &path, None);

    let mut job = Job::new(&scheduler);
    // The alignment-stage cancel path, which has no seam of its own: a
    // cancel requested before any fuse unit starts must run zero of them.
    job.control().cancel();
    let outcome = job
        .run(&NoopPreprocess, &consumer)
        .expect("cancel before any unit must not be an error");

    assert_eq!(outcome, JobOutcome::Cancelled);
    assert_eq!(
        job.control().progress().units_done,
        0,
        "a cancel requested before the first unit must complete zero units"
    );
}

#[test]
fn abort_fires_on_error_only() {
    let stack = open_stack();
    let noop = Noop;

    // A genuine error: abort fires exactly once.
    let opts_fail = options("abort-error", 1 << 30);
    let scheduler_fail = build_scheduler(&stack, &opts_fail);
    let target_index = scheduler_fail.plan().len() / 2;
    let recorder = AbortRecorder {
        inner: &noop,
        abort_calls: AtomicUsize::new(0),
        fail_index: Some(target_index),
    };
    let mut job = Job::new(&scheduler_fail);
    let err = job
        .run(&NoopPreprocess, &recorder)
        .expect_err("a failing tile must return Err");
    assert!(matches!(err, TileError::Consumer { .. }));
    assert_eq!(
        recorder.abort_calls.load(Ordering::SeqCst),
        1,
        "abort must fire exactly once after a genuine error"
    );

    // Pause: abort never fires.
    let opts_pause = options("abort-pause", 1);
    let scheduler_pause = build_scheduler(&stack, &opts_pause);
    let gate_pause = Gate::new();
    let recorder_pause = AbortRecorder {
        inner: &noop,
        abort_calls: AtomicUsize::new(0),
        fail_index: None,
    };
    let gated_pause = Gated {
        inner: &recorder_pause,
        gate: &gate_pause,
    };
    let mut job_pause = Job::new(&scheduler_pause);
    let control_pause = job_pause.control();
    let outcome_pause = std::thread::scope(|scope| {
        let handle = scope.spawn(|| job_pause.run(&NoopPreprocess, &gated_pause));
        gate_pause.wait_for_started(1);
        control_pause.pause();
        gate_pause.release();
        handle.join().unwrap().expect("pause must not be an error")
    });
    assert_eq!(outcome_pause, JobOutcome::Paused);
    assert_eq!(
        recorder_pause.abort_calls.load(Ordering::SeqCst),
        0,
        "abort must not fire on pause"
    );

    // Cancel: abort never fires.
    let opts_cancel = options("abort-cancel", 1);
    let scheduler_cancel = build_scheduler(&stack, &opts_cancel);
    let gate_cancel = Gate::new();
    let recorder_cancel = AbortRecorder {
        inner: &noop,
        abort_calls: AtomicUsize::new(0),
        fail_index: None,
    };
    let gated_cancel = Gated {
        inner: &recorder_cancel,
        gate: &gate_cancel,
    };
    let mut job_cancel = Job::new(&scheduler_cancel);
    let control_cancel = job_cancel.control();
    let outcome_cancel = std::thread::scope(|scope| {
        let handle = scope.spawn(|| job_cancel.run(&NoopPreprocess, &gated_cancel));
        gate_cancel.wait_for_started(1);
        control_cancel.cancel();
        gate_cancel.release();
        handle.join().unwrap().expect("cancel must not be an error")
    });
    assert_eq!(outcome_cancel, JobOutcome::Cancelled);
    assert_eq!(
        recorder_cancel.abort_calls.load(Ordering::SeqCst),
        0,
        "abort must not fire on cancel"
    );

    // A clean completion: abort never fires.
    let opts_ok = options("abort-completed", 1 << 30);
    let scheduler_ok = build_scheduler(&stack, &opts_ok);
    let recorder_ok = AbortRecorder {
        inner: &noop,
        abort_calls: AtomicUsize::new(0),
        fail_index: None,
    };
    let mut job_ok = Job::new(&scheduler_ok);
    let outcome_ok = job_ok
        .run(&NoopPreprocess, &recorder_ok)
        .expect("a completed run must succeed");
    assert_eq!(outcome_ok, JobOutcome::Completed);
    assert_eq!(
        recorder_ok.abort_calls.load(Ordering::SeqCst),
        0,
        "abort must not fire after a clean completion"
    );
}

#[test]
fn eta_ignores_time_spent_paused() {
    let stack = open_stack();
    let opts = options("eta-ignores-pause", 1);
    let scheduler = build_scheduler(&stack, &opts);

    let gate = Gate::new();
    let noop = Noop;
    let gated = Gated {
        inner: &noop,
        gate: &gate,
    };
    let mut job = Job::new(&scheduler);
    let control = job.control();

    let first_outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
        gate.wait_for_started(1);
        control.pause();
        gate.release();
        handle.join().unwrap().expect("first segment must succeed")
    });
    assert_eq!(first_outcome, JobOutcome::Paused);

    let running_at_pause = control.progress().running;

    // Simulate a long real-world pause: nothing below touches `job` or
    // `control` in any way that would grow the running clock.
    std::thread::sleep(Duration::from_millis(50));

    assert_eq!(
        control.progress().running,
        running_at_pause,
        "running time -- and therefore the ETA it feeds -- must not grow while the job sits \
         paused"
    );

    control.clear();
    let second_outcome = job
        .run(&NoopPreprocess, &gated)
        .expect("resumed segment must succeed");
    assert_eq!(second_outcome, JobOutcome::Completed);

    assert!(
        control.progress().running >= running_at_pause,
        "running time only grows while a segment actually executes"
    );
}

#[test]
fn paused_job_frees_its_workers() {
    let stack = open_stack();
    let opts1 = options("frees-workers-1", 1);
    let scheduler1 = build_scheduler(&stack, &opts1);

    let gate = Gate::new();
    let noop = Noop;
    let gated = Gated {
        inner: &noop,
        gate: &gate,
    };
    let mut job1 = Job::new(&scheduler1);
    let control1 = job1.control();

    std::thread::scope(|scope| {
        let handle = scope.spawn(|| job1.run(&NoopPreprocess, &gated));
        gate.wait_for_started(1);
        control1.pause();
        gate.release();
        let outcome = handle
            .join()
            .unwrap()
            .expect("first job must pause cleanly");
        assert_eq!(outcome, JobOutcome::Paused);

        // `job1.run` has already returned -- its scheduler-owned pool is
        // already dropped and holds no worker. A second, unrelated job must
        // be able to run to completion using the same worker budget while
        // job1 sits paused (never resumed in this test).
        let opts2 = options("frees-workers-2", 1);
        let scheduler2 = build_scheduler(&stack, &opts2);
        let mut job2 = Job::new(&scheduler2);
        let outcome2 = job2
            .run(&NoopPreprocess, &Noop)
            .expect("second job must run to completion");
        assert_eq!(outcome2, JobOutcome::Completed);
    });
}

#[test]
fn control_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<focusforge_core::job::JobControl>();
    assert_send_sync::<Arc<focusforge_core::job::JobControl>>();

    let stack = open_stack();
    let opts = options("control-send-sync", 1);
    let scheduler = build_scheduler(&stack, &opts);
    let gate = Gate::new();
    let noop = Noop;
    let gated = Gated {
        inner: &noop,
        gate: &gate,
    };
    let mut job = Job::new(&scheduler);
    let control = job.control();

    let outcome = std::thread::scope(|scope| {
        let runner = scope.spawn(|| job.run(&NoopPreprocess, &gated));
        let control_for_pause = Arc::clone(&control);
        let gate_ref = &gate;
        let pauser = scope.spawn(move || {
            gate_ref.wait_for_started(1);
            control_for_pause.pause();
            gate_ref.release();
        });
        pauser.join().unwrap();
        runner.join().unwrap().expect("run must succeed")
    });
    assert_eq!(outcome, JobOutcome::Paused);
}

#[test]
fn control_from_another_thread() {
    let stack = open_stack();
    let opts = options("control-from-another-thread", 1);
    let scheduler = build_scheduler(&stack, &opts);
    let plan_len = scheduler.plan().len();

    let gate1 = Gate::new();
    let noop = Noop;
    let gated1 = Gated {
        inner: &noop,
        gate: &gate1,
    };
    let mut job = Job::new(&scheduler);
    let control = job.control();

    let outcome1 = std::thread::scope(|scope| {
        let runner = scope.spawn(|| job.run(&NoopPreprocess, &gated1));
        let control_thread = Arc::clone(&control);
        let gate1_ref = &gate1;
        let controller = scope.spawn(move || {
            gate1_ref.wait_for_started(1);
            control_thread.pause();
            gate1_ref.release();
        });
        controller.join().unwrap();
        runner.join().unwrap().expect("first segment must succeed")
    });
    assert_eq!(outcome1, JobOutcome::Paused);
    assert!(control.progress().units_done < plan_len);

    // Resume, then cancel from another thread mid-second-segment.
    control.clear();
    let gate2 = Gate::new();
    let gated2 = Gated {
        inner: &noop,
        gate: &gate2,
    };
    let outcome2 = std::thread::scope(|scope| {
        let runner = scope.spawn(|| job.run(&NoopPreprocess, &gated2));
        let control_thread = Arc::clone(&control);
        let gate2_ref = &gate2;
        let controller = scope.spawn(move || {
            gate2_ref.wait_for_started(1);
            control_thread.cancel();
            gate2_ref.release();
        });
        controller.join().unwrap();
        runner
            .join()
            .unwrap()
            .expect("resumed segment must succeed")
    });
    assert_eq!(outcome2, JobOutcome::Cancelled);
}
