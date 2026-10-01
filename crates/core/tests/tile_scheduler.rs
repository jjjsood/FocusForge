// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture-backed driver tests for [`focusforge_core::tile::TileScheduler`]
//! at tile size 200, `levels = 3` (margin 24, interior 152, 4x4 = 16 tiles
//! over `fixtures/mantis-512`'s 512x512 extent, last interior 56 px) --
//! below the fixture's own 512 px crop, so seams are exercised.
//!
//! Deliberately fuses nothing: every consumer here is trivial (a
//! pass-through writer, a recording preprocess hook, a scratch probe, a
//! single deterministically-failing tile) so a scheduler bug and a fusion
//! bug can never be diagnosed as one symptom.

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::StackContext;
use focusforge_core::tile::{
    NoopPreprocess, TileConsumer, TileError, TileJob, TilePlan, TileScheduler, TileSchedulerOptions,
};

/// A fresh scratch base under this binary's own `CARGO_TARGET_TMPDIR`
/// subdirectory, named after `label` so parallel tests in this file never
/// collide.
fn scratch_base(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("tile_scheduler_scratch_{label}"))
}

fn options(label: &str, max_bytes_in_flight: u64) -> TileSchedulerOptions {
    let mut options =
        TileSchedulerOptions::explicit_budget(scratch_base(label), max_bytes_in_flight);
    options.tile_size = 200;
    options.levels = 3;
    options
}

/// A consumer that crops frame 0's read tile down to the tile's interior
/// and writes it through a `TiffWriter`, reassembling frame 0 with no
/// fusion at all -- the scheduler's own reassembly, proved independently of
/// any algorithm.
struct PassthroughWriter {
    writer: Mutex<Option<TiffWriter<File>>>,
}

impl PassthroughWriter {
    fn create(path: &Path, width: u32, height: u32) -> Self {
        let writer = TiffWriter::create(
            path,
            TiffSpec {
                width,
                height,
                samples_per_pixel: 3,
                description: None,
            },
        )
        .expect("create must succeed");
        PassthroughWriter {
            writer: Mutex::new(Some(writer)),
        }
    }

    fn finish(self) {
        let writer = self.writer.lock().unwrap().take().expect("writer present");
        writer.finish().expect("frame must be fully covered");
    }
}

impl TileConsumer for PassthroughWriter {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let interior = job.interior();
        let mut cropped = vec![0u16; (interior.width * interior.height * 3) as usize];
        job.ctx().for_each_frame(|f, tile, _| {
            if f == 0 {
                tile.crop_into(interior, &mut cropped).map_err(|source| {
                    AlgorithmError::Failed {
                        algorithm: "passthrough-writer".to_string(),
                        detail: source.to_string(),
                    }
                })?;
            }
            Ok(())
        })?;
        self.writer
            .lock()
            .unwrap()
            .as_mut()
            .expect("writer present")
            .write_tile(interior, &cropped)?;
        Ok(())
    }
}

fn run_passthrough(out_path: &Path, options: &TileSchedulerOptions) {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, 3, options).expect("scheduler must build");
    let consumer = PassthroughWriter::create(out_path, width, height);
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("run must succeed");
    consumer.finish();
}

#[test]
fn passthrough_roundtrip_is_bit_identical() {
    let out_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("passthrough_roundtrip.tif");
    run_passthrough(&out_path, &options("roundtrip", 1 << 30));

    let source = TiffStack::open(common::mantis_512_dir()).expect("source must reopen");
    let (width, height) = source.dimensions();
    let direct = source
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .expect("direct read must succeed");

    let written = TiffStack::open_files(&[out_path]).expect("written file must open");
    let via_scheduler = written
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .expect("scheduler output must read");

    assert_eq!(
        via_scheduler.samples, direct.samples,
        "scheduler-assembled frame 0 must be bit-identical to a direct tile-by-tile read"
    );
}

#[test]
fn output_is_thread_count_independent() {
    let bytes_per_tile = 200u64 * 200 * (3 * 2 + 1);

    let out_1 = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("thread_count_1.tif");
    run_passthrough(&out_1, &options("thread1", 1));

    let out_8 = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("thread_count_8.tif");
    run_passthrough(&out_8, &options("thread8", bytes_per_tile * 8));

    let bytes_1 = std::fs::read(&out_1).unwrap();
    let bytes_8 = std::fs::read(&out_8).unwrap();
    assert_eq!(
        bytes_1, bytes_8,
        "the assembled output must not depend on the in-flight tile limit"
    );
}

/// Records every `(frame, rect)` pair the hook is called with, and leaves
/// pixels untouched -- so the hook's identity behaviour is asserted, not
/// assumed.
struct RecordingPreprocess {
    calls: Mutex<Vec<(usize, Rect)>>,
}

impl RecordingPreprocess {
    fn new() -> Self {
        RecordingPreprocess {
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl focusforge_core::tile::FramePreprocess for RecordingPreprocess {
    fn preprocess(
        &self,
        frame: usize,
        tile: &mut focusforge_core::tile::Tile,
    ) -> Result<(), AlgorithmError> {
        self.calls.lock().unwrap().push((frame, tile.rect));
        Ok(())
    }
}

#[test]
fn preprocess_hook_sees_every_frame_tile() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let opts = options("preprocess", 1 << 30);
    let scheduler = TileScheduler::new(&stack, stack_ctx, 3, &opts).expect("scheduler must build");
    let plan_len = scheduler.plan().len();

    let recording = RecordingPreprocess::new();
    let recording_out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("preprocess_recording.tif");
    let recording_consumer = PassthroughWriter::create(&recording_out, width, height);
    scheduler
        .run(&recording, &recording_consumer)
        .expect("run must succeed");
    recording_consumer.finish();

    let calls = recording.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        stack.len() * plan_len,
        "the hook must see exactly one call per frame-tile"
    );
    drop(calls);

    let noop_out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("preprocess_noop.tif");
    let noop_consumer = PassthroughWriter::create(&noop_out, width, height);
    scheduler
        .run(&NoopPreprocess, &noop_consumer)
        .expect("run must succeed");
    noop_consumer.finish();

    let recording_bytes = std::fs::read(&recording_out).unwrap();
    let noop_bytes = std::fs::read(&noop_out).unwrap();
    assert_eq!(
        recording_bytes, noop_bytes,
        "a recording-only hook must produce the same composite as NoopPreprocess"
    );
}

/// Writes a file into the scratch directory it is handed, and records
/// whether that directory existed at the time of the call.
struct ScratchProbe {
    recorded_dir: Mutex<Option<PathBuf>>,
    existed_during_run: Mutex<bool>,
    fail: bool,
    already_failed: AtomicUsize,
}

impl ScratchProbe {
    fn new(fail: bool) -> Self {
        ScratchProbe {
            recorded_dir: Mutex::new(None),
            existed_during_run: Mutex::new(false),
            fail,
            already_failed: AtomicUsize::new(0),
        }
    }
}

impl TileConsumer for ScratchProbe {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let dir = job.scratch();
        *self.existed_during_run.lock().unwrap() = dir.is_dir();
        {
            let mut recorded = self.recorded_dir.lock().unwrap();
            if recorded.is_none() {
                *recorded = Some(dir.to_path_buf());
            }
        }
        std::fs::write(dir.join(format!("tile-{}.bin", job.tile().index())), b"x")
            .expect("scratch dir must be writable during the run");

        if self.fail && self.already_failed.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(AlgorithmError::Failed {
                algorithm: "scratch-probe".to_string(),
                detail: "intentional test failure".to_string(),
            });
        }
        Ok(())
    }
}

#[test]
fn scratch_is_cleaned_up_on_error() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);

    // Success path.
    let opts_ok = options("scratch-ok", 1 << 30);
    let scheduler_ok =
        TileScheduler::new(&stack, stack_ctx, 3, &opts_ok).expect("scheduler must build");
    let probe_ok = ScratchProbe::new(false);
    scheduler_ok
        .run(&NoopPreprocess, &probe_ok)
        .expect("run must succeed");
    let dir_ok = probe_ok
        .recorded_dir
        .lock()
        .unwrap()
        .clone()
        .expect("a scratch dir must have been recorded");
    assert!(*probe_ok.existed_during_run.lock().unwrap());
    assert!(
        !dir_ok.exists(),
        "scratch directory must be removed after a successful run"
    );

    // Error path.
    let opts_err = options("scratch-err", 1 << 30);
    let scheduler_err =
        TileScheduler::new(&stack, stack_ctx, 3, &opts_err).expect("scheduler must build");
    let probe_err = ScratchProbe::new(true);
    let result = scheduler_err.run(&NoopPreprocess, &probe_err);
    assert!(result.is_err(), "the run must report the consumer's error");
    let dir_err = probe_err
        .recorded_dir
        .lock()
        .unwrap()
        .clone()
        .expect("a scratch dir must have been recorded");
    assert!(*probe_err.existed_during_run.lock().unwrap());
    assert!(
        !dir_err.exists(),
        "scratch directory must be removed even when the consumer errors"
    );
}

/// Fails for exactly one tile, chosen by its stable plan index rather than
/// by arrival order -- rayon does not specify which error wins when several
/// tiles fail in parallel, so only one tile is ever made to fail at all.
struct FailingOneTile {
    target_index: usize,
}

impl TileConsumer for FailingOneTile {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        if job.tile().index() == self.target_index {
            return Err(AlgorithmError::Failed {
                algorithm: "failing-one-tile".to_string(),
                detail: "intentional test failure".to_string(),
            });
        }
        Ok(())
    }
}

#[test]
fn consumer_error_names_its_tile() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let opts = options("failing-tile", 1 << 30);
    let scheduler = TileScheduler::new(&stack, stack_ctx, 3, &opts).expect("scheduler must build");

    let plan = TilePlan::new(width, height, 200, 3).expect("plan must build");
    let target_index = plan.len() / 2;
    let expected_rect = plan.tiles()[target_index].interior();

    let err = scheduler
        .run(&NoopPreprocess, &FailingOneTile { target_index })
        .expect_err("exactly one failing tile must abort the run");
    let rendered = err.to_string();

    match &err {
        TileError::Consumer { rect, .. } => assert_eq!(*rect, expected_rect),
        other => panic!("expected TileError::Consumer naming the failing tile, got {other:?}"),
    }
    assert!(
        rendered.contains(&expected_rect.x.to_string())
            && rendered.contains(&expected_rect.y.to_string()),
        "error message must name the failing tile's rect: {rendered}"
    );
}

/// Serves ones, except that `frame` of the tile at the frame's origin fails.
struct FailsAtOneFrame {
    frame: usize,
}

impl TileReader for FailsAtOneFrame {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        if frame == self.frame && rect.x == 0 && rect.y == 0 {
            return Err(AlgorithmError::Failed {
                algorithm: "fails-at-one-frame".to_string(),
                detail: format!("frame {frame} is unreadable"),
            });
        }
        out.fill(1);
        Ok(())
    }
}

#[test]
fn a_failing_frame_read_fails_the_run_with_the_readers_error() {
    let stack_ctx = StackContext::new(8, 512, 512, 3);
    let opts = options("failing-read", 1 << 30);
    let reader = FailsAtOneFrame { frame: 5 };
    let scheduler = TileScheduler::new(&reader, stack_ctx, 3, &opts).expect("scheduler must build");

    struct VisitsEveryFrame;
    impl TileConsumer for VisitsEveryFrame {
        fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
            job.ctx().for_each_frame(|_, _, _| Ok(()))
        }
    }

    let err = scheduler
        .run(&NoopPreprocess, &VisitsEveryFrame)
        .expect_err("one unreadable frame must fail the run");
    match &err {
        TileError::Consumer {
            source: AlgorithmError::Failed { algorithm, detail },
            ..
        } => {
            assert_eq!(algorithm, "fails-at-one-frame");
            assert_eq!(detail, "frame 5 is unreadable");
        }
        other => {
            panic!("expected the reader's own error inside TileError::Consumer, got {other:?}")
        }
    }
}

/// `#[ignore]`d: reads the 16 GB reference stack. Run with `--nocapture` to
/// see the wall clock and `VmHWM`.
#[test]
#[ignore = "reads the 16 GB reference stack"]
fn schedules_over_the_reference_stack() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };

    let paths = focusforge_core::io::frame_paths(&stack_dir)
        .expect("frame_paths must enumerate the directory")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("IMG_") || n.contains("IMG_"))
        })
        .collect::<Vec<_>>();

    let stack = TiffStack::open_files(&paths).expect("reference stack must open");
    assert_eq!(
        stack.len(),
        61,
        "IMG_-filtered list must exclude Mantis_Stacked.tif"
    );

    let (width, height) = stack.dimensions();
    let plan = TilePlan::new(width, height, 512, 3).expect("plan must build");
    // Derived from the plan's own arithmetic, never a pinned literal: the
    // tile count is a function of the measured overlap margin, which moves
    // whenever `crate::fuse::pyramid::MEASURED_MARGIN` is re-measured. A
    // literal here would silently stop describing the plan it checks.
    let levels = 3u32;
    let alignment = 1u32 << levels;
    let margin = focusforge_core::tile::plan::margin_for_levels(levels);
    let interior = ((512 - 2 * margin) / alignment) * alignment;
    let expected = (width.div_ceil(interior) * height.div_ceil(interior)) as usize;
    assert_eq!(
        plan.len(),
        expected,
        "512px tiles at levels={levels} (margin {margin}, interior {interior}) must yield \
         {} x {} = {expected} tiles",
        width.div_ceil(interior),
        height.div_ceil(interior)
    );

    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let opts = options("reference-stack", 1 << 30);
    let scheduler = TileScheduler::new(&stack, stack_ctx, 3, &opts).expect("scheduler must build");

    struct ChecksumConsumer;
    impl TileConsumer for ChecksumConsumer {
        fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
            job.ctx().for_each_frame(|_, _, _| Ok(()))
        }
    }

    let start = std::time::Instant::now();
    let report = scheduler
        .run_tiles(&plan.tiles()[..8], &NoopPreprocess, &ChecksumConsumer)
        .expect("run over 8 tiles of the reference stack must succeed");
    let elapsed = start.elapsed();

    let peak_rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(|l| l.to_string())
        })
        .unwrap_or_else(|| "VmHWM unavailable (not Linux)".to_string());

    println!(
        "schedules_over_the_reference_stack: {} tiles, {} frame-tiles read, wall clock {:?}, {peak_rss}",
        report.tiles, report.frame_tiles_read, elapsed
    );
}
