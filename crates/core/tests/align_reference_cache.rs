// SPDX-License-Identifier: GPL-3.0-or-later

//! The anchored reference frame's pyramid is prepared once per run and never
//! shared across runs, and a mismatched prepared value is ignored. Also pins the
//! per-frame outcomes of an uncached run to recorded values. Runs that are not
//! pinned lower `max-iterations` to 40 from the shipped 400: cheap on an
//! 8-frame fixture, and some frames diverge.

mod common;

use common::align::{
    CountingReader, Drift, ESCALATION_READS, anchor_defaults, assert_fingerprints_match,
    drifting_focus_stack, frame_fingerprint, lowered_max_iterations, open_fixture,
    open_stack_context, outcome_fingerprint, reference_reads, resolved_defaults, samples_digest,
    shifted_frame_3, stack_context, streamed_band_reads, two_frame_reader, uncached_outcomes,
};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus, TileReader,
};
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, PyramidalEcc, Transform, align_stack_with,
    bytes_per_chain_hop, bytes_per_frame_build,
};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{ParamDesc, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, Rect};

/// A stub [`AlignAlgorithm`] that counts its own `prepare_run` calls and
/// otherwise produces a trivial, always-converged outcome -- identity for
/// the reference frame, a fixed translation otherwise. Used only by
/// `chain_mode_builds_each_frame_once`, which asserts on the call count rather
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

/// The reference frame the recorded pins were taken against.
const RECORDED_REFERENCE: usize = 4;

fn recorded_fingerprints(mode: &str) -> Vec<String> {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let mut cfg = anchor_defaults(&stack_ctx);
    cfg.insert("mode".to_string(), ParamValue::Enum(mode.to_string()));
    uncached_outcomes(stack_ctx, &stack, RECORDED_REFERENCE, &cfg)
        .iter()
        .map(outcome_fingerprint)
        .collect()
}

/// Per-frame outcomes at the shipped defaults, pinned to recorded values; the
/// fixture's diverging frames are part of the record. The transform, iteration
/// count and residual of every frame are checked first, so a changed status
/// cannot hide a changed solve.
#[test]
fn per_frame_outcomes_match_the_recorded_run() {
    for (mode, recorded) in [
        ("translation", RECORDED_TRANSLATION),
        ("rotation-scale", RECORDED_ROTATION_SCALE),
    ] {
        assert_fingerprints_match(mode, &recorded_fingerprints(mode), &recorded);
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
    let (stack, ctx) = open_stack_context("mantis-64");
    let cfg = lowered_max_iterations(&ctx);
    assert_mismatched_prepared_is_ignored((&stack, 0, &cfg), (&stack, 2, &cfg), &[1]);
}

#[test]
fn prepared_for_another_mode_is_ignored() {
    let (stack, ctx) = open_stack_context("mantis-64");
    let cfg = lowered_max_iterations(&ctx);
    let mut translation = cfg.clone();
    translation.insert(
        "mode".to_string(),
        ParamValue::Enum("translation".to_string()),
    );
    assert_mismatched_prepared_is_ignored((&stack, 1, &translation), (&stack, 1, &cfg), &[0, 2]);
}

/// The automatic depth at 64 px is 0: the prepared reference lacks the
/// coarse level the run's explicit depth 1 needs.
#[test]
fn prepared_for_another_pyramid_depth_is_ignored() {
    let (stack, ctx) = open_stack_context("mantis-64");
    let cfg = lowered_max_iterations(&ctx);
    let mut deeper = cfg.clone();
    deeper.insert("pyramid-depth".to_string(), ParamValue::Int(1));
    assert_ne!(cfg.get("pyramid-depth"), deeper.get("pyramid-depth"));
    assert_mismatched_prepared_is_ignored((&stack, 1, &cfg), (&stack, 1, &deeper), &[0, 2]);
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

/// Counts, per frame, the reads of full-width bands shorter than the frame:
/// the reads that stream a frame into its pyramid.
struct BandReader<'a> {
    inner: &'a dyn TileReader,
    frame: (u32, u32),
    bands: Vec<AtomicUsize>,
}

impl TileReader for BandReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        if rect.width == self.frame.0 && rect.height < self.frame.1 {
            self.bands[frame].fetch_add(1, Ordering::SeqCst);
        }
        self.inner.read_tile_into(frame, rect, out)
    }
}

#[test]
fn chain_mode_builds_each_frame_once() {
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
    .expect("a chain run completes when prepare_run returns nothing");
    assert_eq!(
        chain_algo.prepare_calls.load(Ordering::SeqCst),
        1,
        "chain mode must call prepare_run exactly once per run"
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

    // Tall enough that a frame streams in several bands, which no other read
    // of it resembles.
    let size = (200u32, 1100u32);
    let drift = Drift {
        shift: (1.5, -1.0),
        scale: 0.002,
    };
    let (frames, _) = drifting_focus_stack(size, 7, &drift, 15.0);
    let tall = StackContext::new(7, size.0, size.1, 1);
    let mut cfg = lowered_max_iterations(&tall);
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("chain".to_string()),
    );
    let bands_per_frame = streamed_band_reads(&tall);
    assert!(bands_per_frame > 1);
    for workers in [1u64, 2, 4, 8] {
        let reader = BandReader {
            inner: &frames,
            frame: size,
            bands: (0..tall.frame_count).map(|_| AtomicUsize::new(0)).collect(),
        };
        let budget =
            bytes_per_frame_build(tall) + workers * bytes_per_chain_hop(tall, DEFAULT_TILE_SIZE);
        let run = align_stack_with(
            &PyramidalEcc,
            tall,
            &reader,
            &cfg,
            FailurePolicy::Strict,
            &AlignDriverOptions {
                budget: HostBudget::explicit(budget),
                ..Default::default()
            },
        )
        .expect("chain run completes");
        assert_eq!(run.alignment.reference, 3);
        let counts: Vec<usize> = reader
            .bands
            .iter()
            .map(|n| n.load(Ordering::SeqCst))
            .collect();
        assert_eq!(
            counts,
            vec![bands_per_frame; tall.frame_count],
            "{workers} workers: band reads per frame"
        );
    }
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

/// Chain alignment of `reader` at the shipped defaults, with `tweak` applied to
/// the resolved values: the reference the run picked and every frame's
/// fingerprint.
fn chain_run(
    reader: &dyn TileReader,
    stack_ctx: StackContext,
    tweak: impl FnOnce(&mut ParamValues),
) -> (usize, Vec<String>) {
    let mut cfg = resolved_defaults(&stack_ctx);
    tweak(&mut cfg);
    let run = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        reader,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("the chain run completes");
    let fingerprints = run.alignment.frames.iter().map(frame_fingerprint).collect();
    (run.alignment.reference, fingerprints)
}

/// Chain output at the shipped defaults over three inputs, pinned to recorded
/// values: a real 8-frame stack, a drifting 31-frame synthetic stack and a
/// two-frame pair whose second frame is the first shifted 80 px with a
/// zero-filled border.
#[test]
fn chain_outcomes_match_the_recorded_run() {
    let stack = open_fixture();
    let (reference, now) = chain_run(&stack, stack_context(&stack), |_| {});
    assert_eq!(reference, 3, "mantis-512: reference frame");
    assert_fingerprints_match("mantis-512", &now, &RECORDED_CHAIN_MANTIS_512);

    let size = (640u32, 480u32);
    let drift = Drift {
        shift: (1.5, -1.0),
        scale: 0.002,
    };
    let (drifting, _) = drifting_focus_stack(size, 31, &drift, 15.0);
    let digest = samples_digest(&drifting);
    assert_eq!(
        digest, RECORDED_DRIFTING_DIGEST,
        "drifting stack: generated samples differ from the recorded input"
    );
    let drifting_ctx = StackContext::new(31, size.0, size.1, 1);
    let (reference, now) = chain_run(&drifting, drifting_ctx, |_| {});
    assert_eq!(reference, 15, "drifting stack: reference frame");
    assert_fingerprints_match("drifting stack", &now, &RECORDED_CHAIN_DRIFTING);

    let (warped, tile, width, height, spp) = shifted_frame_3(80.0);
    let pair = two_frame_reader(width, height, spp, warped, tile.samples);
    let pair_ctx = StackContext::new(2, width, height, spp);
    let (reference, now) = chain_run(&pair, pair_ctx, |_| {});
    assert_eq!(reference, 0, "80 px shift: reference frame");
    assert_fingerprints_match("80 px shift", &now, &RECORDED_CHAIN_SHIFT_80);

    // The largest epsilon-px; 1e-2 leaves this pair's fingerprints unchanged.
    let (_, coarser) = chain_run(&pair, pair_ctx, |cfg| {
        cfg.insert("epsilon-px".to_string(), ParamValue::Float(1.0));
    });
    assert_ne!(
        coarser,
        RECORDED_CHAIN_SHIFT_80.map(String::from),
        "80 px shift: a non-default epsilon-px must change at least one fingerprint"
    );
}

const RECORDED_DRIFTING_DIGEST: &str = "d3d827ccd5ed33c2";
const RECORDED_CHAIN_MANTIS_512: [&str; 8] = [
    "m=[3ff0e7ce9c870c04,bf84894797350789,4064ffe352765c74,3f84894797350789,3ff0e7ce9c870c04,c05b0ffa89d1cee4,0000000000000000,0000000000000000,3fefffffffffffa4] status=Diverged(ValidationFailed) iterations=22 residual=3fefd0b0d6bc5e1b",
    "m=[3ff100cae086ba3a,bf71766303f76920,405dab3590f18b7c,3f71766303f76920,3ff100cae086ba3a,c0551c71e142756e,0000000000000000,0000000000000000,3fefffffffffffa8] status=Diverged(ValidationFailed) iterations=526 residual=3fed014a3472d810",
    "m=[3ff0643f52c22f3c,3f5eb35375069855,40468068dca26377,bf5eb35375069855,3ff0643f52c22f3c,c042fc8e46571d62,0000000000000000,0000000000000000,3feffffffffffff8] status=Converged iterations=29 residual=3fef29a66ad1dfff",
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=0 residual=3ff0000000000000",
    "m=[3ff09cc1c32b17b6,3f7e40513b13b247,4008969062d6db28,bf7e40513b13b247,3ff09cc1c32b17b6,c031379d200df1ba,0000000000000000,0000000000000000,3feffffffffffffe] status=Converged iterations=94 residual=3fee730e38de683e",
    "m=[3ff0ebbfa1ea4a06,3f86300e36a7b4da,c02731c0c8bb4a76,bf86300e36a7b4da,3ff0ebbfa1ea4a06,c043027c89782a07,0000000000000000,0000000000000000,3feffffffffffffc] status=Converged iterations=13 residual=3fefeee9b1c24922",
    "m=[3ff1f96c26317a19,3f91a88e423a0a08,c0478e808e7ca100,bf91a88e423a0a08,3ff1f96c26317a19,c0240a9e93f508d6,0000000000000000,0000000000000000,3feffffffffffffb] status=Converged iterations=26 residual=3fef918d5c30627c",
    "m=[3ff5c4b36087d3cd,3f9ee1e76711e673,c05ce918e59e374e,bf9ee1e76711e673,3ff5c4b36087d3cd,c0540c5c3de3d6ff,0000000000000000,0000000000000000,3feffffffffffff3] status=Converged iterations=32 residual=3fefaa7f67f62232",
];
const RECORDED_CHAIN_DRIFTING: [&str; 31] = [
    "m=[3feed8445acd224d,3f6b74710c08dd37,c029b871407e0bc9,bf6b74710c08dd37,3feed8445acd224d,40380f8e362503bf,0000000000000000,0000000000000000,3feffffffffffff9] status=Converged iterations=7 residual=3feffc468e75a126",
    "m=[3feeedb182f2d2a1,3f697b95f4a854b5,c0280ec66a3c255d,bf697b95f4a854b5,3feeedb182f2d2a1,40367490562b9bfd,0000000000000000,0000000000000000,3feffffffffffff9] status=Converged iterations=7 residual=3feffbdb7d479668",
    "m=[3fef0269df46120a,3f67038588b15039,c0264b13e15ec3d1,bf67038588b15039,3fef0269df46120a,4034d6bc70f9610d,0000000000000000,0000000000000000,3feffffffffffff9] status=Converged iterations=7 residual=3feffb9ebfde05bf",
    "m=[3fef16d3652e4f85,3f648273d1cb238f,c0248008e1984e31,bf648273d1cb238f,3fef16d3652e4f85,403338fbb0a4028f,0000000000000000,0000000000000000,3feffffffffffffa] status=Converged iterations=8 residual=3feffbbc06070522",
    "m=[3fef2b2795e4e430,3f62019b41ff318a,c022ba541c6029d4,bf62019b41ff318a,3fef2b2795e4e430,403199e27edfced5,0000000000000000,0000000000000000,3feffffffffffffa] status=Converged iterations=8 residual=3feffc060eb4d7cb",
    "m=[3fef3fb0bb282437,3f5ffc95ad23f5f8,c02106362d215c3f,bf5ffc95ad23f5f8,3fef3fb0bb282437,402ff5f8384c4005,0000000000000000,0000000000000000,3feffffffffffffa] status=Converged iterations=8 residual=3feffc19fb4ede11",
    "m=[3fef54965438f1e7,3f5cd642689cd841,c01ec8ec19686705,bf5cd642689cd841,3fef54965438f1e7,402cb6bfba25adef,0000000000000000,0000000000000000,3feffffffffffffa] status=Converged iterations=8 residual=3feffbc1369584f3",
    "m=[3fef690873186c3f,3f5afcbc62c8a382,c01b97f8cdb011da,bf5afcbc62c8a382,3fef690873186c3f,4029862d88a1fa67,0000000000000000,0000000000000000,3feffffffffffffa] status=Converged iterations=8 residual=3feffb421b2f06a8",
    "m=[3fef7d1072e3223b,3f59cf8bc48496cf,c0186447e6d648b6,bf59cf8bc48496cf,3fef7d1072e3223b,4026613b9eeb7601,0000000000000000,0000000000000000,3feffffffffffffb] status=Converged iterations=8 residual=3feffabca7847933",
    "m=[3fef90c1fb8114bd,3f5891d58d45e998,c015239132a9b26d,bf5891d58d45e998,3fef90c1fb8114bd,40233cee1fd3b731,0000000000000000,0000000000000000,3feffffffffffffb] status=Converged iterations=8 residual=3feffa812437d214",
    "m=[3fefa405d6b30dd2,3f571e2bc9f0287a,c011cf3b04ad6530,bf571e2bc9f0287a,3fefa405d6b30dd2,402017f1380e9511,0000000000000000,0000000000000000,3feffffffffffffd] status=Converged iterations=8 residual=3feffa51d777c05d",
    "m=[3fefb71b5a45f9a8,3f546cf8236131ef,c00cc47edbf246ed,bf546cf8236131ef,3fefb71b5a45f9a8,4019d11f4d4fedc6,0000000000000000,0000000000000000,3feffffffffffffe] status=Converged iterations=8 residual=3feffa40878e1722",
    "m=[3fefc967a3dff1c6,3f5095cb11da0468,c005a40abc2a108a,bf5095cb11da0468,3fefc967a3dff1c6,40136e90dcdec40b,0000000000000000,0000000000000000,3feffffffffffffe] status=Converged iterations=8 residual=3feffa22b5c88401",
    "m=[3fefdb9201442097,3f4743170d8c461e,bffccebfc52a35ce,bf4743170d8c461e,3fefdb9201442097,4009f5ee212ec7c2,0000000000000000,0000000000000000,3feffffffffffffe] status=Converged iterations=8 residual=3feffa0d6943bfb7",
    "m=[3fefedd8f915af5e,3f381e90dfc68b25,bfece5cbaed514d4,bf381e90dfc68b25,3fefedd8f915af5e,3ff9e66ea8534c51,0000000000000000,0000000000000000,3fefffffffffffff] status=Converged iterations=8 residual=3feff9e20683420e",
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=0 residual=3ff0000000000000",
    "m=[3ff008ec6c9a75fd,bf37f6932ea35a5c,3fed4eb344d26fff,3f37f6932ea35a5c,3ff008ec6c9a75fd,bff9a5d2ed81d5b5,0000000000000000,0000000000000000,3feffffffffffffe] status=Converged iterations=8 residual=3feff9bd9ac9987e",
    "m=[3ff011dea4dc3b41,bf46459056af66c2,3ffd21ffeecba5cc,3f46459056af66c2,3ff011dea4dc3b41,c0097581a1aba0f0,0000000000000000,0000000000000000,3feffffffffffffd] status=Converged iterations=8 residual=3feff9dc13e2b32f",
    "m=[3ff01a70f4478803,bf4d952e117b9d1c,400600191248dd3d,3f4d952e117b9d1c,3ff01a70f4478803,c012d8937c2bec3d,0000000000000000,0000000000000000,3feffffffffffffb] status=Converged iterations=8 residual=3feffa1223dcf4d4",
    "m=[3ff022d8a4f993b2,bf4ff66978c09ad6,400d5823c32a9cb6,3f4ff66978c09ad6,3ff022d8a4f993b2,c018c11d4ebe69dc,0000000000000000,0000000000000000,3feffffffffffffa] status=Converged iterations=8 residual=3feffa644740890c",
    "m=[3ff02af4be7f4b13,bf4e1c286c9c5d6d,401262cd2c3343a9,3f4e1c286c9c5d6d,3ff02af4be7f4b13,c01e816a868fd9c3,0000000000000000,0000000000000000,3feffffffffffff8] status=Converged iterations=8 residual=3feffabe6484f5b5",
    "m=[3ff032a7b9f9bc0b,bf493680eef36ce3,40162eddbe407130,3f493680eef36ce3,3ff032a7b9f9bc0b,c02210597ede122a,0000000000000000,0000000000000000,3feffffffffffff8] status=Converged iterations=8 residual=3feffb0273021c06",
    "m=[3ff03a026d82b2aa,bf42ea2c51a14e86,401a18189ad20340,3f42ea2c51a14e86,3ff03a026d82b2aa,c024da239c183ee1,0000000000000000,0000000000000000,3feffffffffffff6] status=Converged iterations=8 residual=3feffb55ccf59cdb",
    "m=[3ff040e147a077ce,bf3b23deaaa8461d,401e377eb23a5781,3f3b23deaaa8461d,3ff040e147a077ce,c027a46f6a63d5cc,0000000000000000,0000000000000000,3feffffffffffff5] status=Converged iterations=8 residual=3feffbb0d77cc6ac",
    "m=[3ff0471e2720722e,bf34b27d7d5ce899,402153269434d967,3f34b27d7d5ce899,3ff0471e2720722e,c02a6ea64d10842c,0000000000000000,0000000000000000,3feffffffffffff3] status=Converged iterations=8 residual=3feffc1176587bc3",
    "m=[3ff04ca647b41604,bf3294bdd1db4586,4023b57512304836,3f3294bdd1db4586,3ff04ca647b41604,c02d3028a9c61f53,0000000000000000,0000000000000000,3feffffffffffff2] status=Converged iterations=8 residual=3feffc341387de3a",
    "m=[3ff0518b8ba3d936,bf364d2ba4f633e5,402645224886d249,3f364d2ba4f633e5,3ff0518b8ba3d936,c02feec4801487cc,0000000000000000,0000000000000000,3feffffffffffff0] status=Converged iterations=8 residual=3feffc3432a24adb",
    "m=[3ff055db8b4e89db,bf3c78429def007e,4028f8a693d6be94,3f3c78429def007e,3ff055db8b4e89db,c0314f5e8011c94e,0000000000000000,0000000000000000,3fefffffffffffef] status=Converged iterations=8 residual=3feffc284f937f38",
    "m=[3ff059695a19d8ef,bf426a521fffd99a,402bd1e6eae26974,3f426a521fffd99a,3ff059695a19d8ef,c032a02c954c9a6c,0000000000000000,0000000000000000,3fefffffffffffee] status=Converged iterations=8 residual=3feffc4a285a6015",
    "m=[3ff05c7ba21bea4c,bf4737f43e177244,402ec3589cf31002,3f4737f43e177244,3ff05c7ba21bea4c,c033eaf22a99d978,0000000000000000,0000000000000000,3fefffffffffffed] status=Converged iterations=8 residual=3feffc95ff338a75",
    "m=[3ff05f135b8bdf8f,bf4cf559eddd5443,4030e3b8bf44607a,3f4cf559eddd5443,3ff05f135b8bdf8f,c03533c871e03a32,0000000000000000,0000000000000000,3fefffffffffffeb] status=Converged iterations=8 residual=3feffcdcbefde699",
];
const RECORDED_CHAIN_SHIFT_80: [&str; 2] = [
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=0 residual=3ff0000000000000",
    "m=[3ff32a7414a31a07,3fadeacf4fdab516,c052abb8b5e394a6,bfadeacf4fdab516,3ff32a7414a31a07,c04bfd262b02a16e,0000000000000000,0000000000000000,3ff0000000000000] status=Diverged(CorrelationDecreased) iterations=3 residual=3fe71863cd8ce3ae",
];
