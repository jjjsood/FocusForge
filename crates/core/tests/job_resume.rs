// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end resume suite over `fixtures/mantis-512`, tile size 200 /
//! `levels = 3` (16 tiles) -- the same geometry `job_engine.rs` pins, so no
//! test here depends on machine speed.
//!
//! "Killed mid-run" is proven by an in-process crash simulation -- write N
//! ledger entries (or write a tile's rows without its ledger entry), drop
//! the consumer without calling `finish()`, and resume in a fresh `Job`
//! over a reopened consumer, never a real subprocess kill.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use focusforge_core::algorithm::AlgorithmError;
use focusforge_core::algorithm::AlignStatus;
use focusforge_core::align::{
    FailurePolicy, FrameAlignment, ReferencePolicy, StackAlignment, Transform,
};
use focusforge_core::equalize::{Equalization, UNIT_GAIN};
use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
use focusforge_core::io::{FrameReader, TiffStack};
use focusforge_core::job::fingerprint::{FrameFingerprint, StackFingerprint};
use focusforge_core::job::record::JobRecord;
use focusforge_core::job::{AlignIdentity, Job, JobOutcome, Resumable, ResumeStore};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{
    NoopPreprocess, TileConsumer, TileJob, TileScheduler, TileSchedulerOptions,
};

fn scratch_base(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("job_resume_scratch_{label}"))
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

fn resolved_laplacian_values(stack_ctx: StackContext) -> ParamValues {
    let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(3));
    let resolved =
        params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
    params::resolved_values(&resolved)
}

fn geometry(stack_ctx: StackContext) -> FusionGeometry {
    FusionGeometry {
        width: stack_ctx.width,
        height: stack_ctx.height,
        samples_per_pixel: 3,
        frame_count: stack_ctx.frame_count as u32,
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

fn job_record(
    stack: &TiffStack,
    values: &ParamValues,
    composite_path: &std::path::Path,
    zmap_path: Option<&std::path::Path>,
) -> JobRecord {
    let (width, height) = stack.dimensions();
    JobRecord {
        fingerprint: stack_fingerprint(stack),
        width,
        height,
        samples_per_pixel: 3,
        frame_count: stack.len(),
        tile_size: 200,
        levels: 3,
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

#[test]
fn resume_store_rejects_a_ledger_missing_raw_identity() {
    let stack = open_stack();
    let scratch = scratch_base("legacy-frame-fingerprint");
    let composite = scratch.join("composite.tif");
    std::fs::create_dir_all(&scratch).expect("scratch must exist");
    std::fs::write(&composite, []).expect("output marker must exist");
    let values = ParamValues::new();
    let record = job_record(&stack, &values, &composite, None);
    let mut legacy = serde_json::to_value(&record).expect("record must serialize");
    legacy["fingerprint"] = serde_json::json!({
        "frames": (0..stack.len()).map(|index| {
            let header = stack.header(index);
            serde_json::json!({
                "path": header.path.clone(),
                "file_size": header.file_size,
                "image_description": header.image_description.clone(),
                "width": header.width,
                "height": header.height,
                "samples_per_pixel": header.samples_per_pixel,
                "strip_offsets": header.strip_offsets.clone(),
                "strip_byte_counts": header.strip_byte_counts.clone(),
            })
        }).collect::<Vec<_>>(),
    });
    let ledger_dir = scratch.join("resume").join("legacy");
    std::fs::create_dir_all(&ledger_dir).expect("legacy ledger directory must exist");
    std::fs::write(
        ledger_dir.join("job.json"),
        serde_json::to_vec(&legacy).expect("legacy record must serialize"),
    )
    .expect("legacy job must write");
    let store = ResumeStore::open(&scratch).expect("resume store must open");
    assert!(matches!(
        store.find(&record).expect("find must succeed"),
        Resumable::None
    ));
}

/// Blocks each `consume` call until [`Gate::release`] is called, and
/// records when a call has begun -- same rendezvous shape as
/// `crates/core/tests/job_engine.rs`'s `Gate`, duplicated here since
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
            .wait_timeout_while(started, Duration::from_secs(10), |count| *count < threshold)
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
            .wait_timeout_while(released, Duration::from_secs(10), |r| !*r)
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

/// Records every unit except `except` into the ledger -- simulating a crash
/// in the window between a tile's rows reaching the OS and its ledger line
/// being appended: `except`'s rows are fully written (its `inner.consume`
/// returns `Ok`), but no ledger line is appended for it.
struct RecordExceptOne<'a> {
    inner: &'a dyn TileConsumer,
    ledger: &'a focusforge_core::job::Ledger,
    except: usize,
}

impl TileConsumer for RecordExceptOne<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        self.inner.consume(job)?;
        if job.tile().index() != self.except {
            self.ledger
                .record_unit(job.tile().index())
                .map_err(|source| AlgorithmError::Failed {
                    algorithm: "resume-ledger".to_string(),
                    detail: source.to_string(),
                })?;
        }
        Ok(())
    }
    fn abort(&self) {
        self.inner.abort();
    }
}

/// Counts every `consume` call it forwards.
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

#[test]
fn kill_and_resume_matches_uninterrupted_output() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    // Reference: an uninterrupted run.
    let ref_opts = options("kill-resume-reference", 1 << 30);
    let ref_scheduler = build_scheduler(&stack, &ref_opts);
    let ref_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kill_resume_reference.tif");
    let algo = registry::find_stack("laplacian").unwrap();
    let ref_consumer =
        FusionConsumer::create(algo, values.clone(), &ref_path, None, geometry(stack_ctx)).unwrap();
    ref_scheduler
        .run(&NoopPreprocess, &ref_consumer)
        .expect("reference run must succeed");
    ref_consumer.finish().unwrap();

    // First "process": run a segment, record some units, then crash --
    // drop everything without calling `finish()` or `discard()`.
    let scratch = scratch_base("kill-resume-job");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    let job_opts = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1_u64);
        o.tile_size = 200;
        o.levels = 3;
        o
    };
    {
        let scheduler = build_scheduler(&stack, &job_opts);
        let record = job_record(&stack, &values, &composite_path, None);
        let store = ResumeStore::open(&scratch).unwrap();
        let ledger = store.begin(&record).unwrap();
        let consumer = FusionConsumer::create(
            algo,
            values.clone(),
            &composite_path,
            None,
            geometry(stack_ctx),
        )
        .unwrap();
        let recording = focusforge_core::job::RecordingConsumer {
            inner: &consumer,
            ledger: &ledger,
        };
        let gate = Gate::new();
        let gated = Gated {
            inner: &recording,
            gate: &gate,
        };
        let mut job = Job::new(&scheduler);
        let control = job.control();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
            gate.wait_for_started(1);
            control.pause();
            gate.release();
            let outcome = handle.join().unwrap().expect("segment must not error");
            assert_eq!(outcome, JobOutcome::Paused);
        });
        // Crash: `consumer`, `ledger`, `job` all dropped here, unfinished.
    }

    // Second "process": resume.
    let stack2 = open_stack();
    let scheduler2 = build_scheduler(&stack2, &job_opts);
    let record2 = job_record(&stack2, &values, &composite_path, None);
    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record2).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    };
    let completed = ledger2.completed().to_vec();
    assert!(
        !completed.is_empty() && completed.len() < scheduler2.plan().len(),
        "the killed segment must have recorded some but not all units, got {}/{}",
        completed.len(),
        scheduler2.plan().len()
    );

    let (resumed_consumer, surviving) = FusionConsumer::resume(
        scheduler2.plan(),
        &completed,
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry(stack_ctx),
    )
    .unwrap();
    let recording2 = focusforge_core::job::RecordingConsumer {
        inner: &resumed_consumer,
        ledger: &ledger2,
    };
    let mut job2 = Job::resuming(&scheduler2, &surviving);
    let outcome2 = job2
        .run(&NoopPreprocess, &recording2)
        .expect("resumed segment must succeed");
    assert_eq!(outcome2, JobOutcome::Completed);
    resumed_consumer.finish().unwrap();
    ledger2.discard().unwrap();

    assert_eq!(
        std::fs::read(&ref_path).unwrap(),
        std::fs::read(&composite_path).unwrap(),
        "a killed-then-resumed run must match an uninterrupted one byte-for-byte, header included"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn resume_skips_completed_units() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);
    let algo = registry::find_stack("laplacian").unwrap();

    let scratch = scratch_base("skip-completed");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    let job_opts = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1_u64);
        o.tile_size = 200;
        o.levels = 3;
        o
    };
    let plan_len;
    {
        let scheduler = build_scheduler(&stack, &job_opts);
        plan_len = scheduler.plan().len();
        let record = job_record(&stack, &values, &composite_path, None);
        let store = ResumeStore::open(&scratch).unwrap();
        let ledger = store.begin(&record).unwrap();
        let consumer = FusionConsumer::create(
            algo,
            values.clone(),
            &composite_path,
            None,
            geometry(stack_ctx),
        )
        .unwrap();
        let recording = focusforge_core::job::RecordingConsumer {
            inner: &consumer,
            ledger: &ledger,
        };
        let gate = Gate::new();
        let gated = Gated {
            inner: &recording,
            gate: &gate,
        };
        let mut job = Job::new(&scheduler);
        let control = job.control();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
            gate.wait_for_started(1);
            control.pause();
            gate.release();
            let _ = handle.join().unwrap().expect("segment must not error");
        });
    }

    let stack2 = open_stack();
    let scheduler2 = build_scheduler(&stack2, &job_opts);
    let record2 = job_record(&stack2, &values, &composite_path, None);
    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record2).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    };
    let completed = ledger2.completed().to_vec();
    let expected_calls = plan_len - completed.len();

    let (resumed_consumer, surviving) = FusionConsumer::resume(
        scheduler2.plan(),
        &completed,
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry(stack_ctx),
    )
    .unwrap();
    let counting = CountingWrap {
        inner: &resumed_consumer,
        calls: AtomicUsize::new(0),
    };
    let mut job2 = Job::resuming(&scheduler2, &surviving);
    let outcome2 = job2
        .run(&NoopPreprocess, &counting)
        .expect("resume must succeed");
    assert_eq!(outcome2, JobOutcome::Completed);
    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        expected_calls,
        "the resumed run's consume calls must equal plan.len() minus the recorded units"
    );
    resumed_consumer.finish().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn resume_refuses_a_changed_stack() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("changed-stack");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    // A partial output file must exist for the candidate to be considered.
    std::fs::write(&composite_path, b"partial").unwrap();

    let store = ResumeStore::open(&scratch).unwrap();
    let record = job_record(&stack, &values, &composite_path, None);
    store.begin(&record).unwrap();

    // "Replace" one frame's file: mutate its recorded fingerprint by hand,
    // simulating a frame that changed on disk between the two runs.
    let mut changed_record = record.clone();
    changed_record.fingerprint.frames[0].file_size += 1;
    let renamed_path = changed_record.fingerprint.frames[0].path.clone();

    match store.find(&changed_record).unwrap() {
        Resumable::Mismatch(mismatch) => {
            assert_eq!(mismatch.field, "file_size");
            let file_name = renamed_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            assert!(
                mismatch.to_string().contains(&file_name),
                "message must name the frame's file: {mismatch}"
            );
        }
        other => panic!("expected Resumable::Mismatch, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn resume_refuses_changed_params() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("changed-params");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&composite_path, b"partial").unwrap();

    let store = ResumeStore::open(&scratch).unwrap();
    let record = job_record(&stack, &values, &composite_path, None);
    store.begin(&record).unwrap();

    let mut changed = record.clone();
    changed
        .stack_params
        .insert("levels".to_string(), ParamValue::Int(4));

    match store.find(&changed).unwrap() {
        Resumable::Mismatch(mismatch) => assert_eq!(mismatch.field, "stack_params"),
        other => panic!("expected Resumable::Mismatch, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn resume_refuses_changed_equalization() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("changed-equalization");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&composite_path, b"partial").unwrap();

    let store = ResumeStore::open(&scratch).unwrap();
    let mut record = job_record(&stack, &values, &composite_path, None);
    record.equalization = Some(Equalization {
        max_gain: 1.25,
        target_stops: 0.0,
        gains: vec![UNIT_GAIN; stack.len()],
    });
    record.align = Some(AlignIdentity::new(
        "pyramidal-ecc",
        ParamValues::new(),
        FailurePolicy::Strict,
        vec![UNIT_GAIN; stack.len()],
    ));
    store.begin(&record).unwrap();

    let same = record.clone();
    assert!(
        matches!(store.find(&same).unwrap(), Resumable::Ledger(_)),
        "the recorded equalization resumes"
    );

    let mut off = record.clone();
    off.equalization = None;
    let mut other_gain = record.clone();
    other_gain.equalization.as_mut().unwrap().gains[0] = UNIT_GAIN + 1;
    let mut other_bound = record.clone();
    other_bound.equalization.as_mut().unwrap().max_gain = 1.5;
    let mut other_target = record.clone();
    other_target.equalization.as_mut().unwrap().target_stops = 0.1;
    for changed in [off, other_gain, other_bound, other_target] {
        match store.find(&changed).unwrap() {
            Resumable::Mismatch(mismatch) => assert_eq!(mismatch.field, "equalization"),
            other => panic!("expected Resumable::Mismatch, got {other:?}"),
        }
    }

    // The alignment read the prepared sources: other prepared gains, or none,
    // are another alignment.
    let mut other_prepared = record.clone();
    other_prepared.align.as_mut().unwrap().gains[0] = UNIT_GAIN + 1;
    let mut unprepared = record.clone();
    unprepared.align.as_mut().unwrap().gains.clear();
    for changed in [other_prepared, unprepared] {
        match store.find(&changed).unwrap() {
            Resumable::Mismatch(mismatch) => assert_eq!(mismatch.field, "align"),
            other => panic!("expected Resumable::Mismatch, got {other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn resume_refuses_changed_tile_geometry() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("changed-geometry");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&composite_path, b"partial").unwrap();

    let store = ResumeStore::open(&scratch).unwrap();
    let record = job_record(&stack, &values, &composite_path, None);
    store.begin(&record).unwrap();

    let mut changed = record.clone();
    changed.tile_size = 256;

    match store.find(&changed).unwrap() {
        Resumable::Mismatch(mismatch) => assert_eq!(mismatch.field, "tile_size"),
        other => panic!("expected Resumable::Mismatch, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn crash_between_tile_write_and_ledger_recomputes_one_unit() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);
    let algo = registry::find_stack("laplacian").unwrap();

    let ref_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("crash_window_reference.tif");
    let ref_opts = options("crash-window-reference", 1 << 30);
    let ref_scheduler = build_scheduler(&stack, &ref_opts);
    let ref_consumer =
        FusionConsumer::create(algo, values.clone(), &ref_path, None, geometry(stack_ctx)).unwrap();
    ref_scheduler.run(&NoopPreprocess, &ref_consumer).unwrap();
    ref_consumer.finish().unwrap();

    let scratch = scratch_base("crash-window");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    let job_opts = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1_u64 << 30);
        o.tile_size = 200;
        o.levels = 3;
        o
    };
    let target_index;
    {
        let scheduler = build_scheduler(&stack, &job_opts);
        target_index = scheduler.plan().len() / 2;
        let record = job_record(&stack, &values, &composite_path, None);
        let store = ResumeStore::open(&scratch).unwrap();
        let ledger = store.begin(&record).unwrap();
        let consumer = FusionConsumer::create(
            algo,
            values.clone(),
            &composite_path,
            None,
            geometry(stack_ctx),
        )
        .unwrap();
        let except = RecordExceptOne {
            inner: &consumer,
            ledger: &ledger,
            except: target_index,
        };
        let mut job = Job::new(&scheduler);
        let outcome = job.run(&NoopPreprocess, &except).unwrap();
        assert_eq!(outcome, JobOutcome::Completed);
        // Crash: dropped unfinished.
    }

    let stack2 = open_stack();
    let scheduler2 = build_scheduler(&stack2, &job_opts);
    let record2 = job_record(&stack2, &values, &composite_path, None);
    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record2).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    };
    let completed = ledger2.completed().to_vec();
    assert!(
        !completed.contains(&target_index),
        "the crash-window unit must never have reached the ledger"
    );
    assert_eq!(
        completed.len(),
        scheduler2.plan().len() - 1,
        "exactly one unit must be missing from the ledger"
    );

    let (resumed_consumer, surviving) = FusionConsumer::resume(
        scheduler2.plan(),
        &completed,
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry(stack_ctx),
    )
    .unwrap();
    let recording2 = focusforge_core::job::RecordingConsumer {
        inner: &resumed_consumer,
        ledger: &ledger2,
    };
    let mut job2 = Job::resuming(&scheduler2, &surviving);
    let outcome2 = job2.run(&NoopPreprocess, &recording2).unwrap();
    assert_eq!(outcome2, JobOutcome::Completed);
    resumed_consumer.finish().unwrap();

    assert_eq!(
        std::fs::read(&ref_path).unwrap(),
        std::fs::read(&composite_path).unwrap(),
        "recomputing the crash-window unit must still match the uninterrupted reference exactly"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn truncated_output_is_not_complete() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);
    let algo = registry::find_stack("laplacian").unwrap();

    let scratch = scratch_base("truncated");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    let job_opts = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1_u64);
        o.tile_size = 200;
        o.levels = 3;
        o
    };
    {
        let scheduler = build_scheduler(&stack, &job_opts);
        let record = job_record(&stack, &values, &composite_path, None);
        let store = ResumeStore::open(&scratch).unwrap();
        let ledger = store.begin(&record).unwrap();
        let consumer = FusionConsumer::create(
            algo,
            values.clone(),
            &composite_path,
            None,
            geometry(stack_ctx),
        )
        .unwrap();
        let recording = focusforge_core::job::RecordingConsumer {
            inner: &consumer,
            ledger: &ledger,
        };
        let gate = Gate::new();
        let gated = Gated {
            inner: &recording,
            gate: &gate,
        };
        let mut job = Job::new(&scheduler);
        let control = job.control();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
            gate.wait_for_started(1);
            control.pause();
            gate.release();
            let _ = handle.join().unwrap().expect("segment must not error");
        });
    }

    // Truncate the partial output file by hand -- the on-disk bytes no
    // longer hold every row the ledger claims complete.
    let len_before = std::fs::metadata(&composite_path).unwrap().len();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&composite_path)
        .unwrap();
    file.set_len(len_before / 2).unwrap();
    drop(file);

    let stack2 = open_stack();
    let scheduler2 = build_scheduler(&stack2, &job_opts);
    let record2 = job_record(&stack2, &values, &composite_path, None);
    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record2).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    };
    let completed = ledger2.completed().to_vec();
    assert!(
        !completed.is_empty(),
        "the first segment must have recorded some units"
    );

    let (_resumed_consumer, surviving) = FusionConsumer::resume(
        scheduler2.plan(),
        &completed,
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry(stack_ctx),
    )
    .unwrap();
    assert!(
        surviving.len() < completed.len(),
        "at least one recorded unit's rows must no longer be held by the truncated file: \
         completed={completed:?}, surviving={surviving:?}"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_record_from_another_solver_revision_is_not_resumable() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("solver-revision");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&composite_path, b"partial").unwrap();

    let mut record = job_record(&stack, &values, &composite_path, None);
    record.align = Some(AlignIdentity::new(
        "pyramidal-ecc",
        ParamValues::new(),
        FailurePolicy::Strict,
        Vec::new(),
    ));
    let store = ResumeStore::open(&scratch).unwrap();
    let _ledger = store.begin(&record).unwrap();
    assert!(
        matches!(store.find(&record).unwrap(), Resumable::Ledger(_)),
        "the unchanged record resumes"
    );

    // A different revision is a mismatch on the align identity.
    let mut other = record.clone();
    other.align.as_mut().unwrap().solver_version += 1;
    match store.find(&other).unwrap() {
        Resumable::Mismatch(mismatch) => assert_eq!(mismatch.field, "align"),
        other => panic!("expected Resumable::Mismatch, got {other:?}"),
    }

    // A record written without the field does not parse and is skipped.
    let job_json = walk(&scratch.join("resume"))
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "job.json"))
        .expect("begin writes a job.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&job_json).unwrap()).unwrap();
    json["align"]
        .as_object_mut()
        .expect("the record carries an align identity")
        .remove("solver_version")
        .expect("the identity carries solver_version");
    std::fs::write(&job_json, serde_json::to_string(&json).unwrap()).unwrap();
    match store.find(&record).unwrap() {
        Resumable::None => {}
        other => {
            panic!("expected Resumable::None for a record without solver_version, got {other:?}")
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn ledger_stays_under_the_scratch_base() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);
    let algo = registry::find_stack("laplacian").unwrap();

    let scratch = scratch_base("stays-under-base");
    let output_dir = scratch.join("output-dir");
    std::fs::create_dir_all(&output_dir).unwrap();
    let composite_path = output_dir.join("out.tif.partial");

    let job_opts = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1_u64);
        o.tile_size = 200;
        o.levels = 3;
        o
    };
    let scheduler = build_scheduler(&stack, &job_opts);
    let record = job_record(&stack, &values, &composite_path, None);
    let store = ResumeStore::open(&scratch).unwrap();
    let ledger = store.begin(&record).unwrap();
    let consumer = FusionConsumer::create(
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry(stack_ctx),
    )
    .unwrap();
    let recording = focusforge_core::job::RecordingConsumer {
        inner: &consumer,
        ledger: &ledger,
    };
    let gate = Gate::new();
    let gated = Gated {
        inner: &recording,
        gate: &gate,
    };
    let mut job = Job::new(&scheduler);
    let control = job.control();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
        gate.wait_for_started(1);
        control.pause();
        gate.release();
        let _ = handle.join().unwrap().expect("segment must not error");
    });

    // The output directory holds only the `.partial` file.
    let output_entries: Vec<_> = std::fs::read_dir(&output_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        output_entries,
        vec![std::ffi::OsString::from("out.tif.partial")],
        "the output directory must hold only the .partial file"
    );

    // Every ledger artefact is under `<scratch_base>/resume/`.
    let resume_root = scratch.join("resume");
    assert!(ledger.dir().starts_with(&resume_root));
    for entry in walk(&resume_root) {
        assert!(entry.starts_with(&resume_root));
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn discard_removes_the_store_entry() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("discard");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&composite_path, b"partial").unwrap();

    let store = ResumeStore::open(&scratch).unwrap();
    let record = job_record(&stack, &values, &composite_path, None);
    let ledger = store.begin(&record).unwrap();
    ledger.discard().unwrap();

    match store.find(&record).unwrap() {
        Resumable::None => {}
        other => panic!("expected Resumable::None after discard, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A ledger reopened through [`ResumeStore::find`] holds its own
/// `units.log` append handle, the same as a freshly [`ResumeStore::begin`]'d
/// one -- `discard` must release it before removing the directory on a
/// filesystem that turns an unlinked-but-open file into a hidden entry
/// (FUSE mounts without `hard_remove`, this host's NTFS cargo target
/// included), or the following `remove_dir_all` sees a non-empty directory.
#[test]
fn discard_removes_a_ledger_find_opened() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("discard-find-opened");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&composite_path, b"partial").unwrap();

    let store = ResumeStore::open(&scratch).unwrap();
    let record = job_record(&stack, &values, &composite_path, None);
    let ledger = store.begin(&record).unwrap();
    ledger.record_unit(0).unwrap();
    drop(ledger);

    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    };
    let dir = ledger2.dir().to_path_buf();
    assert!(dir.is_dir());
    ledger2.discard().unwrap();
    assert!(
        !dir.exists(),
        "discard must remove the whole ledger directory, hidden entries included"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn resume_reuses_the_recorded_alignment() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("reuses-alignment");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&composite_path, b"partial").unwrap();

    let store = ResumeStore::open(&scratch).unwrap();
    let record = job_record(&stack, &values, &composite_path, None);
    let ledger = store.begin(&record).unwrap();

    let alignment = StackAlignment {
        reference: 0,
        policy: ReferencePolicy::Anchor,
        frames: vec![
            FrameAlignment {
                frame: 0,
                transform: Transform::identity(),
                status: AlignStatus::Converged,
                iterations: 0,
                residual: 1.0,
                fallback: false,
            },
            FrameAlignment {
                frame: 1,
                transform: Transform::identity(),
                status: AlignStatus::HitCap,
                iterations: 50,
                residual: 0.5,
                fallback: false,
            },
            FrameAlignment {
                frame: 2,
                transform: Transform::identity(),
                status: AlignStatus::Diverged(
                    focusforge_core::align::DivergenceReason::LemmaOneRegion,
                ),
                iterations: 12,
                residual: 0.1,
                fallback: true,
            },
        ],
    };
    ledger.record_alignment(&alignment).unwrap();
    drop(ledger);

    match store.find(&record).unwrap() {
        Resumable::Ledger(ledger) => {
            assert_eq!(
                ledger.alignment(),
                Some(&alignment),
                "the recorded StackAlignment must round-trip and compare equal frame by frame"
            );
        }
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn resume_into_an_empty_scratch_directory() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);

    let scratch = scratch_base("empty-scratch");
    let composite_path = scratch.join("out.tif");
    let store = ResumeStore::open(&scratch).unwrap();
    let record = job_record(&stack, &values, &composite_path, None);

    match store.find(&record).unwrap() {
        Resumable::None => {}
        other => panic!("expected Resumable::None over an empty store, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A cancelled job's ledger and `.partial` both survive, and a later
/// process resumes from them.
#[test]
fn resume_after_a_clean_cancel_continues() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);
    let algo = registry::find_stack("laplacian").unwrap();

    let scratch = scratch_base("after-cancel");
    let composite_path = scratch.join("out.tif");
    std::fs::create_dir_all(&scratch).unwrap();
    let job_opts = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1_u64);
        o.tile_size = 200;
        o.levels = 3;
        o
    };
    {
        let scheduler = build_scheduler(&stack, &job_opts);
        let record = job_record(&stack, &values, &composite_path, None);
        let store = ResumeStore::open(&scratch).unwrap();
        let ledger = store.begin(&record).unwrap();
        let consumer = FusionConsumer::create(
            algo,
            values.clone(),
            &composite_path,
            None,
            geometry(stack_ctx),
        )
        .unwrap();
        let recording = focusforge_core::job::RecordingConsumer {
            inner: &consumer,
            ledger: &ledger,
        };
        let gate = Gate::new();
        let gated = Gated {
            inner: &recording,
            gate: &gate,
        };
        let mut job = Job::new(&scheduler);
        let control = job.control();
        let outcome = std::thread::scope(|scope| {
            let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
            gate.wait_for_started(1);
            control.cancel();
            gate.release();
            handle.join().unwrap().expect("cancel must not be an error")
        });
        assert_eq!(outcome, JobOutcome::Cancelled);
        assert!(
            composite_path.exists(),
            "a clean cancel must leave the .partial file"
        );
    }

    let stack2 = open_stack();
    let scheduler2 = build_scheduler(&stack2, &job_opts);
    let record2 = job_record(&stack2, &values, &composite_path, None);
    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record2).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger after a clean cancel, got {other:?}"),
    };
    let completed = ledger2.completed().to_vec();
    let (resumed_consumer, surviving) = FusionConsumer::resume(
        scheduler2.plan(),
        &completed,
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry(stack_ctx),
    )
    .unwrap();
    let recording2 = focusforge_core::job::RecordingConsumer {
        inner: &resumed_consumer,
        ledger: &ledger2,
    };
    let mut job2 = Job::resuming(&scheduler2, &surviving);
    let outcome2 = job2.run(&NoopPreprocess, &recording2).unwrap();
    assert_eq!(outcome2, JobOutcome::Completed);
    resumed_consumer.finish().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Reads the real 16 GB reference stack -- the full 8192x5464 canvas, not a
/// crop, so the composite `.partial` lands at the same ~268 MB one 16-bit
/// source frame occupies. Mirrors
/// `kill_and_resume_matches_uninterrupted_output` at full scale: an
/// uninterrupted reference run, a segment killed after exactly one tile via the
/// `Gate` rendezvous (never a sleep), and a resumed run compared against the
/// reference byte for byte. Run with `--nocapture` to see the wall clock,
/// recomputed-unit count and `job.json` size.
#[test]
#[ignore = "reads the 16 GB reference stack"]
fn resume_on_the_reference_stack() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        eprintln!("reference stack not found; skipping");
        return;
    };

    let paths = focusforge_core::io::frame_paths(&stack_dir)
        .expect("frame_paths must enumerate the reference stack directory")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .collect::<Vec<_>>();

    let stack = TiffStack::open_files(&paths).expect("reference stack must open");
    assert_eq!(
        stack.len(),
        61,
        "IMG_-filtered list must exclude Mantis_Stacked.tif"
    );

    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let values = resolved_laplacian_values(stack_ctx);
    let algo = registry::find_stack("laplacian").unwrap();

    let tile_size = 512;
    let levels = 3;
    let scratch = scratch_base("reference-stack");
    std::fs::create_dir_all(&scratch).unwrap();

    // Reference: an uninterrupted run, full parallelism, its own scratch
    // base (a per-call `ScratchDir`, not the resume store).
    let ref_opts = {
        let mut o = TileSchedulerOptions::explicit_budget(
            scratch_base("reference-stack-reference"),
            1 << 30,
        );
        o.tile_size = tile_size;
        o.levels = levels;
        o
    };
    let ref_scheduler = TileScheduler::new(&stack, stack_ctx, 3, &ref_opts)
        .expect("reference scheduler must build");
    let ref_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("reference_stack_reference.tif");
    let ref_consumer =
        FusionConsumer::create(algo, values.clone(), &ref_path, None, geometry(stack_ctx)).unwrap();
    let ref_start = std::time::Instant::now();
    ref_scheduler
        .run(&NoopPreprocess, &ref_consumer)
        .expect("reference run over the full reference stack must succeed");
    ref_consumer.finish().unwrap();
    let ref_elapsed = ref_start.elapsed();

    // First "process": kill after exactly one tile via the Gate rendezvous
    // (`max_bytes_in_flight: 1` is what makes that deterministic, same as
    // every other kill point in this file).
    let composite_path = scratch.join("out.tif");
    let job_opts_kill = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1);
        o.tile_size = tile_size;
        o.levels = levels;
        o
    };
    let kill_start = std::time::Instant::now();
    {
        let scheduler = TileScheduler::new(&stack, stack_ctx, 3, &job_opts_kill)
            .expect("job scheduler must build");
        let record = JobRecord {
            fingerprint: stack_fingerprint(&stack),
            width,
            height,
            samples_per_pixel: 3,
            frame_count: stack.len(),
            tile_size,
            levels,
            stack_algorithm: "laplacian".to_string(),
            backend: "cpu".to_string(),
            stack_params: values.clone(),
            align: None,
            crop: None,
            equalization: None,
            composite_output: composite_path.clone(),
            zmap_output: None,
        };
        let store = ResumeStore::open(&scratch).unwrap();
        let ledger = store.begin(&record).unwrap();
        let consumer = FusionConsumer::create(
            algo,
            values.clone(),
            &composite_path,
            None,
            geometry(stack_ctx),
        )
        .unwrap();
        let recording = focusforge_core::job::RecordingConsumer {
            inner: &consumer,
            ledger: &ledger,
        };
        let gate = Gate::new();
        let gated = Gated {
            inner: &recording,
            gate: &gate,
        };
        let mut job = Job::new(&scheduler);
        let control = job.control();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| job.run(&NoopPreprocess, &gated));
            gate.wait_for_started(1);
            control.pause();
            gate.release();
            let outcome = handle.join().unwrap().expect("segment must not error");
            assert_eq!(outcome, JobOutcome::Paused);
        });
        // Crash: `consumer`, `ledger`, `job` all dropped here, unfinished.
    }
    let kill_elapsed = kill_start.elapsed();

    // Second "process": resume, full parallelism for the recomputed units.
    let job_opts_resume = {
        let mut o = TileSchedulerOptions::explicit_budget(scratch.clone(), 1 << 30);
        o.tile_size = tile_size;
        o.levels = levels;
        o
    };
    let stack2 = TiffStack::open_files(&paths).expect("reference stack must reopen");
    let scheduler2 = TileScheduler::new(&stack2, stack_ctx, 3, &job_opts_resume)
        .expect("resume scheduler must build");
    let record2 = JobRecord {
        fingerprint: stack_fingerprint(&stack2),
        width,
        height,
        samples_per_pixel: 3,
        frame_count: stack2.len(),
        tile_size,
        levels,
        stack_algorithm: "laplacian".to_string(),
        backend: "cpu".to_string(),
        stack_params: values.clone(),
        align: None,
        crop: None,
        equalization: None,
        composite_output: composite_path.clone(),
        zmap_output: None,
    };
    let store2 = ResumeStore::open(&scratch).unwrap();
    let ledger2 = match store2.find(&record2).unwrap() {
        Resumable::Ledger(ledger) => ledger,
        other => panic!("expected Resumable::Ledger, got {other:?}"),
    };
    let job_json_len = std::fs::metadata(ledger2.dir().join("job.json"))
        .expect("job.json must exist under the resume store")
        .len();
    let completed = ledger2.completed().to_vec();
    let plan_len = scheduler2.plan().len();
    assert!(
        !completed.is_empty() && completed.len() < plan_len,
        "the killed segment must have recorded some but not all units, got {}/{}",
        completed.len(),
        plan_len
    );
    let recomputed = plan_len - completed.len();

    let (resumed_consumer, surviving) = FusionConsumer::resume(
        scheduler2.plan(),
        &completed,
        algo,
        values.clone(),
        &composite_path,
        None,
        geometry(stack_ctx),
    )
    .unwrap();
    let recording2 = focusforge_core::job::RecordingConsumer {
        inner: &resumed_consumer,
        ledger: &ledger2,
    };
    let mut job2 = Job::resuming(&scheduler2, &surviving);
    let resume_start = std::time::Instant::now();
    let outcome2 = job2
        .run(&NoopPreprocess, &recording2)
        .expect("resumed run over the full reference stack must succeed");
    let resume_elapsed = resume_start.elapsed();
    assert_eq!(outcome2, JobOutcome::Completed);
    resumed_consumer.finish().unwrap();
    ledger2.discard().unwrap();

    assert_eq!(
        std::fs::read(&ref_path).unwrap(),
        std::fs::read(&composite_path).unwrap(),
        "a killed-then-resumed run over the reference stack must match an uninterrupted one \
         byte for byte, header included"
    );

    eprintln!(
        "resume_on_the_reference_stack: reference {ref_elapsed:.2?} ({plan_len} tiles), \
         killed segment {kill_elapsed:.2?} (1 tile), resumed {resume_elapsed:.2?} \
         ({recomputed} of {plan_len} tiles recomputed), job.json {job_json_len} bytes"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    let _ = std::fs::remove_dir_all(scratch_base("reference-stack-reference"));
}
