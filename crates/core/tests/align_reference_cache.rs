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
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, Rect};

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

/// The off-centre patches of a 512 px frame: a 3x3 grid without its middle.
const PATCH_COUNT: usize = 8;

/// Reads one registration makes of a frame's own pixels up to its solve: the
/// level-1 bands (each output band of `DEFAULT_TILE_SIZE / 2` rows reads its
/// rows plus halo), without the level-0 window or patches.
fn streamed_band_reads(stack: &StackContext) -> usize {
    ceil_div(stack.height.div_ceil(2), DEFAULT_TILE_SIZE / 2)
}

/// The reference frame is read as bands, one level-0 template window and one
/// window per patch template -- once per run, whatever the registration count.
fn reference_reads(stack: &StackContext) -> usize {
    streamed_band_reads(stack) + 1 + PATCH_COUNT
}

/// Reads one escalation adds for a frame no larger than the detector's 512 px
/// proxy: the whole frame at level 0. The reference's is made by the first
/// escalation of a run and shared by every later one.
const ESCALATION_READS: usize = 1;

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

/// One outcome as exact bits and its status, so a recorded run and a later
/// one compare without a tolerance.
fn outcome_fingerprint(outcome: &AlignOutcome) -> String {
    let m: Vec<String> = outcome
        .transform
        .m
        .iter()
        .flatten()
        .map(|v| format!("{:016x}", v.to_bits()))
        .collect();
    format!(
        "m=[{}] status={:?} iterations={} residual={:016x}",
        m.join(","),
        outcome.status,
        outcome.iterations,
        outcome.residual.to_bits()
    )
}

/// The middle frame, the shipped `reference-frame` default on eight frames.
const MIDDLE_FRAME: usize = 4;

fn recorded_fingerprints(mode: &str) -> Vec<String> {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let mut cfg = resolved_defaults(&stack_ctx);
    cfg.insert("mode".to_string(), ParamValue::Enum(mode.to_string()));
    uncached_outcomes(stack_ctx, &stack, MIDDLE_FRAME, &cfg)
        .iter()
        .map(outcome_fingerprint)
        .collect()
}

/// Per-frame outcomes at the shipped defaults, pinned to recorded values; the
/// fixture's diverging frames are part of the record.
#[test]
fn per_frame_outcomes_match_the_recorded_run() {
    assert_eq!(recorded_fingerprints("translation"), RECORDED_TRANSLATION);
    assert_eq!(
        recorded_fingerprints("rotation-scale"),
        RECORDED_ROTATION_SCALE
    );
}

/// A fingerprint without its status token: the transform, iteration count and
/// residual bits alone.
fn without_status(fingerprint: &str) -> String {
    fingerprint
        .split_whitespace()
        .filter(|token| !token.starts_with("status="))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The transform, iteration count and residual of every frame stay bit for bit
/// what the recorded run produced, whatever status the off-centre check adds.
#[test]
fn transforms_iterations_and_residuals_match_the_recorded_run() {
    for (mode, recorded) in [
        ("translation", RECORDED_TRANSLATION),
        ("rotation-scale", RECORDED_ROTATION_SCALE),
    ] {
        let now: Vec<String> = recorded_fingerprints(mode)
            .iter()
            .map(|f| without_status(f))
            .collect();
        let then: Vec<String> = recorded.iter().map(|f| without_status(f)).collect();
        assert_eq!(now, then, "{mode}");
    }
}

const RECORDED_TRANSLATION: [&str; 8] = [
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Diverged(TooFewValidSamples) iterations=1 residual=bfa2f68153100307",
    "m=[3ff0000000000000,0000000000000000,405e77aab7af070f,0000000000000000,3ff0000000000000,c04dd107ed3cd89f,0000000000000000,0000000000000000,3ff0000000000000] status=Diverged(ValidationFailed) iterations=36 residual=3feee3ee052a97da",
    "m=[3ff0000000000000,0000000000000000,40407748ffc853dd,0000000000000000,3ff0000000000000,c037838888e6f1af,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=13 residual=3feefc117e5fa3ef",
    "m=[3ff0000000000000,0000000000000000,c033283401277f8d,0000000000000000,3ff0000000000000,4022da8c8b0ea70c,0000000000000000,0000000000000000,3ff0000000000000] status=Diverged(ValidationFailed) iterations=14 residual=3fee48ad734c3d60",
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=0 residual=3ff0000000000000",
    "m=[3ff0000000000000,0000000000000000,c01c87fdecca54f1,0000000000000000,3ff0000000000000,c0307f78348e026a,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=12 residual=3fefec6272a42e6a",
    "m=[3ff0000000000000,0000000000000000,c02b22c80ff54495,0000000000000000,3ff0000000000000,403d871879042d99,0000000000000000,0000000000000000,3ff0000000000000] status=Diverged(ValidationFailed) iterations=36 residual=3fef214f9548db3e",
    "m=[3ff0000000000000,0000000000000000,4035e64d6fd19b15,0000000000000000,3ff0000000000000,4038cd26c9f30cef,0000000000000000,0000000000000000,3ff0000000000000] status=Diverged(CorrelationDecreased) iterations=6 residual=3fed8e1afae6eb4e",
];
const RECORDED_ROTATION_SCALE: [&str; 8] = [
    "m=[3fd2f08090a14c09,bf816516ffa13062,40715b33b68a89f5,3f816516ffa13062,3fd2f08090a14c09,405a91b7c1dd18fe,0000000000000000,0000000000000000,3fefffffffffffff] status=Diverged(CorrelationDecreased) iterations=12 residual=3fea7dac85df34d0",
    "m=[3ff148659462033a,bf932b345d397cce,4059d8dd1b568789,3f932b345d397cce,3ff148659462033a,c054b5031df0dff9,0000000000000000,0000000000000000,3feffffffffffffe] status=Diverged(TooFewValidSamples) iterations=8 residual=3fef215af392f665",
    "m=[3feff119c6605426,3f3237f914222c75,4040b76ec1642175,bf3237f914222c75,3feff119c6605426,c03700927f88dedb,0000000000000000,0000000000000000,3feffffffffffffa] status=Converged iterations=19 residual=3feefbf9269e9038",
    "m=[3fee69d082673551,bf7abf4eb6b782a7,bfd09a170c37ac8e,3f7abf4eb6b782a7,3fee69d082673551,4033e889c367528e,0000000000000000,0000000000000000,3feffffffffffffa] status=Diverged(ValidationFailed) iterations=23 residual=3fee49d07cd0b8c2",
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=0 residual=3ff0000000000000",
    "m=[3ff04c2c46b99f37,3f6a1f9b6c4c78d3,c02d588c4d41d211,bf6a1f9b6c4c78d3,3ff04c2c46b99f37,c03478e1cfabe417,0000000000000000,0000000000000000,3feffffffffffffe] status=Converged iterations=13 residual=3fefeee9b1c24922",
    "m=[3ff167ad3c569cc0,3f8a49e0975285e0,c04a13eec016fca5,bf8a49e0975285e0,3ff167ad3c569cc0,4021762180619ff7,0000000000000000,0000000000000000,3fefffffffffffff] status=Converged iterations=33 residual=3fef4fe9f3da86d1",
    "m=[3ff333c4cb45824b,3f8560fc4af52cce,c055b556461d1f7d,bf8560fc4af52cce,3ff333c4cb45824b,c040b99f036e83e0,0000000000000000,0000000000000000,3feffffffffffffc] status=Converged iterations=109 residual=3fee3d6c99295d76",
];

/// Fingerprints of `frames` aligned against `reference` of `stack` under
/// `cfg`, with `prepared` attached to the context.
fn outcomes_with(
    stack: &TiffStack,
    reference: usize,
    cfg: &ParamValues,
    prepared: Option<&(dyn std::any::Any + Send + Sync)>,
    frames: &[usize],
) -> Vec<String> {
    let ctx = AlignContext::new(stack_context(stack), reference, stack).with_prepared(prepared);
    frames
        .iter()
        .map(|&frame| {
            outcome_fingerprint(
                &PyramidalEcc
                    .align_frame(&ctx, frame, cfg)
                    .expect("align_frame must not error"),
            )
        })
        .collect()
}

/// Prepares a run over `prepared_stack` at `prepared_reference` and
/// `prepared_cfg`, then aligns `frames` of `run_stack` against
/// `run_reference` under `run_cfg` with that value attached. The outcomes
/// must equal the ones with nothing prepared.
fn assert_mismatched_prepared_is_ignored(
    (prepared_stack, prepared_reference, prepared_cfg): (&TiffStack, usize, &ParamValues),
    (run_stack, run_reference, run_cfg): (&TiffStack, usize, &ParamValues),
    frames: &[usize],
) {
    let prepared = PyramidalEcc
        .prepare_run(
            &AlignContext::new(
                stack_context(prepared_stack),
                prepared_reference,
                prepared_stack,
            ),
            prepared_cfg,
        )
        .expect("prepare_run completes")
        .expect("pyramidal-ecc prepares its reference");
    let with = outcomes_with(run_stack, run_reference, run_cfg, Some(&*prepared), frames);
    let without = outcomes_with(run_stack, run_reference, run_cfg, None, frames);
    assert_eq!(with, without);
}

#[test]
fn prepared_for_another_reference_frame_is_ignored() {
    let stack = open_fixture();
    let cfg = lowered_max_iterations(&stack_context(&stack));
    assert_mismatched_prepared_is_ignored((&stack, 4, &cfg), (&stack, 2, &cfg), &[1, 6]);
}

#[test]
fn prepared_for_another_mode_is_ignored() {
    let stack = open_fixture();
    let cfg = lowered_max_iterations(&stack_context(&stack));
    let mut translation = cfg.clone();
    translation.insert(
        "mode".to_string(),
        ParamValue::Enum("translation".to_string()),
    );
    assert_mismatched_prepared_is_ignored((&stack, 4, &translation), (&stack, 4, &cfg), &[1, 6]);
}

#[test]
fn prepared_for_another_pyramid_depth_is_ignored() {
    let stack = open_fixture();
    let cfg = lowered_max_iterations(&stack_context(&stack));
    let mut shallow = cfg.clone();
    shallow.insert("pyramid-depth".to_string(), ParamValue::Int(2));
    assert_ne!(cfg.get("pyramid-depth"), shallow.get("pyramid-depth"));
    assert_mismatched_prepared_is_ignored((&stack, 4, &shallow), (&stack, 4, &cfg), &[1, 6]);
}

#[test]
fn prepared_for_another_stack_size_is_ignored() {
    let large = open_fixture();
    let small = TiffStack::open(common::fixtures_dir().join("mantis-64"))
        .expect("opening fixtures/mantis-64");
    assert_ne!(large.dimensions(), small.dimensions());
    let mut cfg = lowered_max_iterations(&stack_context(&small));
    cfg.insert("pyramid-depth".to_string(), ParamValue::Int(1));
    assert_mismatched_prepared_is_ignored((&large, 4, &cfg), (&small, 1, &cfg), &[0, 2]);
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
            budget: HostBudget::explicit(1),
            ..Default::default()
        },
    )
    .expect("serial anchored run completes");

    let bands = streamed_band_reads(&stack_ctx);
    let reference = run.alignment.reference;

    // Several fixture frames diverge and escalate, yet the reference gains one
    // read, not one per escalation.
    assert_eq!(
        reader.reads_for(reference),
        reference_reads(&stack_ctx) + ESCALATION_READS,
        "the cached reference frame must be read once (bands, level-0 window, patch \
         windows) plus its one shared escalation read, not once per registration"
    );
    for i in 0..stack_ctx.frame_count {
        if i == reference {
            continue;
        }
        // Bands always; a frame that reaches level 0 adds its window, one
        // that converges there adds at most one window per patch, and one that
        // diverges adds its own whole-frame escalation read.
        let reads = reader.reads_for(i);
        let most = bands + 1 + PATCH_COUNT + ESCALATION_READS;
        assert!(
            (bands..=most).contains(&reads),
            "non-reference frame {i} made {reads} reads, expected {bands} to {most}"
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
            budget: HostBudget::explicit(1),
            ..Default::default()
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
    let expected_band_reads = reference_reads(&stack_ctx);

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
            budget: HostBudget::explicit(1),
            ..Default::default()
        },
    )
    .expect("first run completes");
    assert_eq!(first.alignment.reference, 0);
    assert_eq!(
        reader_first.reads_for(0),
        expected_band_reads + ESCALATION_READS
    );

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
            budget: HostBudget::explicit(1),
            ..Default::default()
        },
    )
    .expect("second run completes");
    assert_eq!(second.alignment.reference, stack_ctx.frame_count - 1);

    // The second run must build its own reference pyramid from scratch --
    // its own reference frame is read exactly once, the same count the
    // first run's own reference frame was read, never zero (which would
    // mean it silently reused the first run's already-finished pyramid).
    let second_reads = reader_second.reads_for(second.alignment.reference);
    assert!(
        (expected_band_reads..=expected_band_reads + ESCALATION_READS).contains(&second_reads),
        "the second run's reference frame must be read once on its own, plus at most the \
         one shared escalation read: {second_reads} reads"
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
            budget: HostBudget::explicit(1),
            ..Default::default()
        },
    )
    .expect("serial run completes");

    assert!(
        flag.load(Ordering::SeqCst),
        "the prepared value must be dropped by the time align_stack_with returns"
    );
}
