// SPDX-License-Identifier: GPL-3.0-or-later

//! The anchored reference frame's pyramid is built once per run, not once per
//! registration. Shares `mod common;`. `max-iterations` is lowered to 40 from
//! the shipped 400: cheap on an 8-frame fixture, and some frames diverge, which
//! the byte-identity tests want.
//!
//! `CountingReader` wraps `&dyn TileReader` with one `AtomicUsize` per
//! frame, incremented in `read_tile_into` -- a test double over the
//! *reader*, never a stub algorithm, so the real `PyramidalEcc` code path is
//! what is being counted.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus, TileReader,
};
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, PyramidalEcc, Transform, align_stack_with,
};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamDesc, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::Rect;
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;

fn open_fixture() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512")
}

fn stack_context(stack: &TiffStack) -> StackContext {
    let (width, height) = stack.dimensions();
    StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    )
}

/// Fully resolved defaults for `PyramidalEcc`, `max-iterations` then lowered
/// to 40 by every caller here (see the module docs).
fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), ctx)
        .expect("defaults resolve cleanly");
    params::resolved_values(&resolved)
}

fn lowered_max_iterations(ctx: &StackContext) -> ParamValues {
    let mut cfg = resolved_defaults(ctx);
    cfg.insert("max-iterations".to_string(), ParamValue::Int(40));
    cfg
}

fn ceil_div(a: u32, b: u32) -> usize {
    a.div_ceil(b) as usize
}

/// Wraps another [`TileReader`], counting `read_tile_into` calls per frame
/// index. One `AtomicUsize` per frame, so concurrent registrations (the
/// rayon anchor sub-arm) count correctly with no lock.
struct CountingReader<'a> {
    inner: &'a dyn TileReader,
    reads: Vec<AtomicUsize>,
}

impl<'a> CountingReader<'a> {
    fn new(inner: &'a dyn TileReader, frame_count: usize) -> Self {
        CountingReader {
            inner,
            reads: (0..frame_count).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    fn reads_for(&self, frame: usize) -> usize {
        self.reads[frame].load(Ordering::SeqCst)
    }
}

impl TileReader for CountingReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.reads[frame].fetch_add(1, Ordering::SeqCst);
        self.inner.read_tile_into(frame, rect, out)
    }
}

/// A stub [`AlignAlgorithm`] that counts its own `prepare_run` calls and
/// otherwise produces a trivial, always-converged outcome -- identity for
/// the reference frame, a fixed translation otherwise. Used only by
/// `chain_mode_prepares_nothing`, which asserts on the call count rather
/// than on any transform.
struct CountingPrepareAlign {
    prepare_calls: AtomicUsize,
}

impl CountingPrepareAlign {
    fn new() -> Self {
        CountingPrepareAlign {
            prepare_calls: AtomicUsize::new(0),
        }
    }
}

impl AlignAlgorithm for CountingPrepareAlign {
    fn id(&self) -> &str {
        "counting-prepare-align"
    }

    fn display_name(&self) -> &str {
        "Counting Prepare Align"
    }

    fn params(&self) -> &[ParamDesc] {
        &[]
    }

    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        _cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        let transform = if frame == ctx.reference() {
            Transform::identity()
        } else {
            Transform::translation(1.0, 0.0)
        };
        Ok(AlignOutcome {
            transform,
            status: AlignStatus::Converged,
            iterations: 1,
            residual: 1.0,
        })
    }

    fn prepare_run(
        &self,
        _ctx: &AlignContext<'_>,
        _cfg: &ParamValues,
    ) -> Result<Option<Arc<dyn std::any::Any + Send + Sync>>, AlgorithmError> {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

/// Sets a shared flag on `Drop`, so a test can observe exactly when the
/// value `prepare_run` returned actually goes away.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A stub [`AlignAlgorithm`] whose `prepare_run` returns a [`DropFlag`], and
/// whose `align_frame` asserts the flag is still unset for every
/// non-reference frame -- i.e. the prepared value outlives every
/// registration of the run that created it.
struct DropCheckingAlign {
    flag: Arc<AtomicBool>,
}

impl AlignAlgorithm for DropCheckingAlign {
    fn id(&self) -> &str {
        "drop-checking-align"
    }

    fn display_name(&self) -> &str {
        "Drop Checking Align"
    }

    fn params(&self) -> &[ParamDesc] {
        &[]
    }

    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        _cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        if frame != ctx.reference() {
            assert!(
                !self.flag.load(Ordering::SeqCst),
                "prepared state was dropped before align_stack_with returned"
            );
        }
        let transform = if frame == ctx.reference() {
            Transform::identity()
        } else {
            Transform::translation(1.0, 0.0)
        };
        Ok(AlignOutcome {
            transform,
            status: AlignStatus::Converged,
            iterations: 1,
            residual: 1.0,
        })
    }

    fn prepare_run(
        &self,
        _ctx: &AlignContext<'_>,
        _cfg: &ParamValues,
    ) -> Result<Option<Arc<dyn std::any::Any + Send + Sync>>, AlgorithmError> {
        Ok(Some(Arc::new(DropFlag(self.flag.clone()))))
    }
}

/// A no-prepared-state comparison loop: `PyramidalEcc::align_frame` called
/// directly over a plain `AlignContext::new` with no `.with_prepared` --
/// the byte-identity baseline this test asserts against.
fn uncached_outcomes(
    stack_ctx: StackContext,
    reader: &dyn TileReader,
    reference: usize,
    cfg: &ParamValues,
) -> Vec<AlignOutcome> {
    let ctx = AlignContext::new(stack_ctx, reference, reader);
    (0..stack_ctx.frame_count)
        .map(|i| {
            PyramidalEcc
                .align_frame(&ctx, i, cfg)
                .expect("uncached align_frame must not error")
        })
        .collect()
}

fn assert_frames_match_outcomes(
    frames: &[focusforge_core::align::FrameAlignment],
    outcomes: &[AlignOutcome],
) {
    assert_eq!(frames.len(), outcomes.len());
    for (frame, outcome) in frames.iter().zip(outcomes) {
        assert_eq!(
            frame.transform, outcome.transform,
            "frame {}: transform must be byte-identical to the uncached loop",
            frame.frame
        );
        assert_eq!(
            frame.status, outcome.status,
            "frame {}: status must match the uncached loop",
            frame.frame
        );
        assert_eq!(
            frame.iterations, outcome.iterations,
            "frame {}: iterations must match the uncached loop",
            frame.frame
        );
        assert_eq!(
            frame.residual, outcome.residual,
            "frame {}: residual must match the uncached loop",
            frame.frame
        );
    }
}

#[test]
fn reference_frame_is_read_once_per_run() {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let cfg = lowered_max_iterations(&stack_ctx);
    let reader = CountingReader::new(&stack, stack_ctx.frame_count);

    let run = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &reader,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions {
            max_bytes_in_flight: 1,
        },
    )
    .expect("serial anchored run completes");

    let expected_band_reads = ceil_div(stack_ctx.height, DEFAULT_TILE_SIZE);
    let reference = run.alignment.reference;

    assert_eq!(
        reader.reads_for(reference),
        expected_band_reads,
        "the cached reference frame must be read exactly once (in bands), not once per \
         registration"
    );
    for i in 0..stack_ctx.frame_count {
        if i == reference {
            continue;
        }
        assert_eq!(
            reader.reads_for(i),
            expected_band_reads,
            "non-reference frame {i}'s own read count must be unchanged"
        );
    }
}

#[test]
fn cached_transforms_match_uncached() {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let cfg = lowered_max_iterations(&stack_ctx);

    let cached = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions {
            max_bytes_in_flight: 1,
        },
    )
    .expect("serial anchored run completes");
    assert_eq!(cached.worker_limit, 1, "this is the serial sub-arm");

    let uncached = uncached_outcomes(stack_ctx, &stack, cached.alignment.reference, &cfg);
    assert_frames_match_outcomes(&cached.alignment.frames, &uncached);
}

#[test]
fn cached_parallel_transforms_match_uncached() {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let cfg = lowered_max_iterations(&stack_ctx);

    let cached = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("parallel anchored run completes");
    assert!(
        cached.worker_limit > 1,
        "the default budget must exercise the parallel arm on this 8-frame fixture: {}",
        cached.worker_limit
    );

    let uncached = uncached_outcomes(stack_ctx, &stack, cached.alignment.reference, &cfg);
    assert_frames_match_outcomes(&cached.alignment.frames, &uncached);
}

#[test]
fn chain_mode_prepares_nothing() {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let mut cfg = lowered_max_iterations(&stack_ctx);

    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("chain".to_string()),
    );
    let chain_algo = CountingPrepareAlign::new();
    align_stack_with(
        &chain_algo,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("chain run completes");
    assert_eq!(
        chain_algo.prepare_calls.load(Ordering::SeqCst),
        0,
        "chain mode must never call prepare_run"
    );

    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );
    let anchor_algo = CountingPrepareAlign::new();
    align_stack_with(
        &anchor_algo,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("anchor run completes");
    assert_eq!(
        anchor_algo.prepare_calls.load(Ordering::SeqCst),
        1,
        "anchor mode must call prepare_run exactly once per run"
    );
}

#[test]
fn two_runs_do_not_share_a_cache() {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let expected_band_reads = ceil_div(stack_ctx.height, DEFAULT_TILE_SIZE);

    let mut cfg_first = lowered_max_iterations(&stack_ctx);
    cfg_first.insert(
        "reference-frame".to_string(),
        ParamValue::Enum("first".to_string()),
    );
    let reader_first = CountingReader::new(&stack, stack_ctx.frame_count);
    let first = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &reader_first,
        &cfg_first,
        FailurePolicy::Strict,
        &AlignDriverOptions {
            max_bytes_in_flight: 1,
        },
    )
    .expect("first run completes");
    assert_eq!(first.alignment.reference, 0);
    assert_eq!(reader_first.reads_for(0), expected_band_reads);

    let mut cfg_second = lowered_max_iterations(&stack_ctx);
    cfg_second.insert(
        "reference-frame".to_string(),
        ParamValue::Enum("last".to_string()),
    );
    let reader_second = CountingReader::new(&stack, stack_ctx.frame_count);
    let second = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &reader_second,
        &cfg_second,
        FailurePolicy::Strict,
        &AlignDriverOptions {
            max_bytes_in_flight: 1,
        },
    )
    .expect("second run completes");
    assert_eq!(second.alignment.reference, stack_ctx.frame_count - 1);

    // The second run must build its own reference pyramid from scratch --
    // its own reference frame is read exactly once, the same count the
    // first run's own reference frame was read, never zero (which would
    // mean it silently reused the first run's already-finished pyramid).
    assert_eq!(
        reader_second.reads_for(second.alignment.reference),
        expected_band_reads,
        "the second run's reference frame must be read exactly once, on its own"
    );
    assert_eq!(
        reader_first.reads_for(first.alignment.reference),
        reader_second.reads_for(second.alignment.reference),
        "per-frame read counts must be identical across the two independent runs"
    );

    let uncached_second =
        uncached_outcomes(stack_ctx, &stack, second.alignment.reference, &cfg_second);
    assert_frames_match_outcomes(&second.alignment.frames, &uncached_second);
}

#[test]
fn prepared_state_is_dropped_when_the_run_ends() {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let cfg = lowered_max_iterations(&stack_ctx);

    let flag = Arc::new(AtomicBool::new(false));
    let algo = DropCheckingAlign { flag: flag.clone() };

    align_stack_with(
        &algo,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions {
            max_bytes_in_flight: 1,
        },
    )
    .expect("serial run completes");

    assert!(
        flag.load(Ordering::SeqCst),
        "the prepared value must be dropped by the time align_stack_with returns"
    );
}
