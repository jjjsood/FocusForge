// SPDX-License-Identifier: GPL-3.0-or-later

//! `pyramidal-ecc` (the [`AlignAlgorithm`]) and [`align_stack`] (the
//! whole-stack driver), exercised against `fixtures/mantis-512` and
//! synthetic in-memory readers.
//!
//! Ground-truth pairs warp frame 3 (`03.tif`, the sharpest fixture frame) by
//! a known [`Transform`] and register it against its own unwarped self --
//! the only configuration where the "known" transform is actually known.

mod common;

use std::path::PathBuf;

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignStatus, TileReader,
};
use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{
    DivergenceReason, EccLevelRecord, EccStatus, FailurePolicy, PyramidalEcc, Transform,
    align_stack,
};
use focusforge_core::io::{FrameError, TiffError, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::{Rect, Tile};

const FRAME_3: usize = 3;

fn open_fixture() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512")
}

fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Opens `fixtures/mantis-512` and reads frame 3's full tile.
fn frame_3_tile() -> (Tile, u32, u32, u32) {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(FRAME_3).samples_per_pixel;
    let rect = full_rect(width, height);
    let tile = stack
        .read_tile(FRAME_3, rect)
        .expect("reading frame 3's tile");
    (tile, width, height, spp)
}

/// The region every ground-truth test here solves over -- identical to the
/// region `crates/core/tests/ecc_solver.rs` measures and to what
/// `PyramidalEcc::solve_region` itself computes for a 512px level, so these
/// tests measure against the same rect the driver actually uses.
fn default_region() -> Rect {
    Rect {
        x: 64,
        y: 64,
        width: 384,
        height: 384,
    }
}

/// The maximum, over `region`'s four corners, of the Euclidean distance
/// between where `a` and `b` map that corner -- the same "corner
/// displacement" measure the solver's own termination criterion uses.
fn corner_error(a: &Transform, b: &Transform, region: Rect) -> f64 {
    let corners = [
        (region.x as f64, region.y as f64),
        ((region.x + region.width - 1) as f64, region.y as f64),
        (region.x as f64, (region.y + region.height - 1) as f64),
        (
            (region.x + region.width - 1) as f64,
            (region.y + region.height - 1) as f64,
        ),
    ];
    corners
        .iter()
        .map(|&(x, y)| {
            let (ax, ay) = a.map_point(x, y).expect("ground truth stays in domain");
            let (bx, by) = b
                .map_point(x, y)
                .expect("recovered transform stays in domain");
            ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
        })
        .fold(0.0f64, f64::max)
}

/// A rotation+scale [`Transform`] about `(cx, cy)`.
fn similarity_about(scale: f64, radians: f64, cx: f64, cy: f64) -> Transform {
    let to_origin = Transform::translation(-cx, -cy);
    let rot_scale = Transform::similarity(scale, radians, 0.0, 0.0);
    let back = Transform::translation(cx, cy);
    back.compose(&rot_scale).compose(&to_origin)
}

/// Deterministic xorshift32 PRNG.
struct XorShift32(u32);

impl XorShift32 {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

/// A synthetic in-memory [`TileReader`] over an arbitrary number of
/// same-sized frames, honouring the same exact-length and in-bounds
/// contracts as [`focusforge_core::io::TiffStack::read_tile_into`].
struct FramesReader {
    width: u32,
    height: u32,
    spp: u32,
    frames: Vec<Vec<u16>>,
}

impl TileReader for FramesReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let Some(src) = self.frames.get(frame) else {
            return Err(AlgorithmError::FrameOutOfRange {
                frame,
                frame_count: self.frames.len(),
            });
        };
        let expected = rect.width as usize * rect.height as usize * self.spp as usize;
        if out.len() != expected {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::BufferSize {
                    expected,
                    got: out.len(),
                },
            )));
        }
        let in_bounds = rect
            .x
            .checked_add(rect.width)
            .is_some_and(|xw| xw <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.height);
        if !in_bounds {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::OutOfBounds {
                    path: PathBuf::from("<memory>"),
                    requested: rect,
                    frame: (self.width, self.height),
                },
            )));
        }
        let stride = self.width as usize * self.spp as usize;
        let row_len = rect.width as usize * self.spp as usize;
        let mut off = 0;
        for r in rect.y..(rect.y + rect.height) {
            let row_start = r as usize * stride + rect.x as usize * self.spp as usize;
            out[off..off + row_len].copy_from_slice(&src[row_start..row_start + row_len]);
            off += row_len;
        }
        Ok(())
    }
}

/// A reader that must never be asked for a pixel -- proves a rejection
/// happened before any registration ran.
struct PanicReader;

impl TileReader for PanicReader {
    fn read_tile_into(
        &self,
        _frame: usize,
        _rect: Rect,
        _out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        panic!("no pixel should be read once the schema has rejected the parameters")
    }
}

/// A two-frame reader: frame 0 is `reference`, frame 1 is `moving`.
fn two_frame_reader(
    width: u32,
    height: u32,
    spp: u32,
    reference: Vec<u16>,
    moving: Vec<u16>,
) -> FramesReader {
    FramesReader {
        width,
        height,
        spp,
        frames: vec![reference, moving],
    }
}

fn cfg_with(mode: &str, pyramid_depth: i64, max_iterations: i64) -> ParamValues {
    let mut cfg = ParamValues::new();
    cfg.insert("mode".to_string(), ParamValue::Enum(mode.to_string()));
    cfg.insert(
        "max-iterations".to_string(),
        ParamValue::Int(max_iterations),
    );
    cfg.insert("epsilon-px".to_string(), ParamValue::Float(1e-4));
    cfg.insert("pyramid-depth".to_string(), ParamValue::Int(pyramid_depth));
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );
    cfg.insert(
        "reference-frame".to_string(),
        ParamValue::Enum("first".to_string()),
    );
    cfg
}

/// Fully resolved defaults for `PyramidalEcc` against `ctx` -- what a real
/// caller (`align_stack`'s own caller) is expected to hand `align_frame`,
/// per the same "`fuse` receives a fully resolved `ParamValues`" contract
/// `StackAlgorithm::fuse` documents.
fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), ctx)
        .expect("defaults resolve cleanly");
    params::resolved_values(&resolved)
}

/// Measured 2026-10-05: corner error `3.10e-5` px through the four-level
/// driven path (`pyramid-depth = 3`), larger than the single-level
/// `9.24e-7` px measurement on the same data and quantity
/// (`crates/core/tests/ecc_solver.rs`) because the driven path enters
/// level 0 already near the optimum and stops further out than a solve
/// iterated in from identity. About 3x under the pinned `1e-4` px.
const TRANSLATION_TOLERANCE_PX: f64 = 1e-4;

/// Measured 2026-10-05 the same way,
/// `recovers_known_rotation_scale_through_the_driver`: corner error
/// `5.51e-4` px, about 1.8x under the single-level test's pinned `1e-3` px.
/// The figure includes the smoothing pass before the final level-0 solve.
const ROTATION_SCALE_TOLERANCE_PX: f64 = 1e-3;

/// Measured with `cargo test -p focusforge-core --test align_algorithm
/// an_80px_shift_with_a_clean_border_converges_through_the_pyramid --
/// --nocapture`, driven path (`pyramid-depth = 3`, `max-iterations = 100`)
/// recovering an 80px translation over an edge-repeated border: corner error
/// `8.2e-5` px. Pinned at ~12x the measured figure.
const LARGE_TRANSLATION_TOLERANCE_PX: f64 = 1e-3;

#[test]
fn recovers_known_translation_through_the_driver() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);
    let w_true = Transform::translation(6.5, -4.25);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("translation stays well inside the frame");

    let reader = two_frame_reader(width, height, spp, warped.tile.samples, tile.samples);
    let stack_ctx = StackContext::new(2, width, height, spp);
    let ctx = AlignContext::new(stack_ctx, 0, &reader);
    let cfg = cfg_with("translation", 3, 50);

    let outcome = PyramidalEcc
        .align_frame(&ctx, 1, &cfg)
        .expect("valid solver inputs");
    assert_eq!(
        outcome.status,
        AlignStatus::Converged,
        "outcome: {outcome:?}"
    );
    let error = corner_error(&outcome.transform, &w_true, default_region());
    eprintln!("recovers_known_translation_through_the_driver: corner error = {error} px");
    assert!(
        error < TRANSLATION_TOLERANCE_PX,
        "corner error {error} px exceeds {TRANSLATION_TOLERANCE_PX} px"
    );
}

#[test]
fn recovers_known_rotation_scale_through_the_driver() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);
    let region = default_region();
    let (cx, cy) = (
        (region.x + region.width / 2) as f64,
        (region.y + region.height / 2) as f64,
    );
    let w_true = similarity_about(1.002, 2.0f64.to_radians(), cx, cy);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("rotation+scale stays well inside the frame");

    let reader = two_frame_reader(width, height, spp, warped.tile.samples, tile.samples);
    let stack_ctx = StackContext::new(2, width, height, spp);
    let ctx = AlignContext::new(stack_ctx, 0, &reader);
    let cfg = cfg_with("rotation-scale", 3, 50);

    let outcome = PyramidalEcc
        .align_frame(&ctx, 1, &cfg)
        .expect("valid solver inputs");
    assert_eq!(
        outcome.status,
        AlignStatus::Converged,
        "outcome: {outcome:?}"
    );
    let error = corner_error(&outcome.transform, &w_true, region);
    eprintln!("recovers_known_rotation_scale_through_the_driver: corner error = {error} px");
    assert!(
        error < ROTATION_SCALE_TOLERANCE_PX,
        "corner error {error} px exceeds {ROTATION_SCALE_TOLERANCE_PX} px"
    );
}

#[test]
fn translation_mode_returns_pure_translation() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);
    let w_true = Transform::translation(20.0, -15.0);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("translation stays well inside the frame");

    let reader = two_frame_reader(width, height, spp, warped.tile.samples, tile.samples);
    let stack_ctx = StackContext::new(2, width, height, spp);
    let ctx = AlignContext::new(stack_ctx, 0, &reader);
    let cfg = cfg_with("translation", 3, 50);

    let outcome = PyramidalEcc
        .align_frame(&ctx, 1, &cfg)
        .expect("valid solver inputs");
    let m = outcome.transform.m;
    assert_eq!(
        m[0][0], 1.0,
        "translation mode must never move scale/rotation: {m:?}"
    );
    assert_eq!(m[1][1], 1.0, "{m:?}");
    assert_eq!(m[0][1], 0.0, "{m:?}");
    assert_eq!(m[1][0], 0.0, "{m:?}");
    assert_eq!(m[2], [0.0, 0.0, 1.0], "{m:?}");
}

#[test]
fn rotation_scale_mode_returns_a_similarity() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);
    let region = default_region();
    let (cx, cy) = (
        (region.x + region.width / 2) as f64,
        (region.y + region.height / 2) as f64,
    );
    let w_true = similarity_about(1.002, 2.0f64.to_radians(), cx, cy);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("rotation+scale stays well inside the frame");

    let reader = two_frame_reader(width, height, spp, warped.tile.samples, tile.samples);
    let stack_ctx = StackContext::new(2, width, height, spp);
    let ctx = AlignContext::new(stack_ctx, 0, &reader);
    let cfg = cfg_with("rotation-scale", 3, 50);

    let outcome = PyramidalEcc
        .align_frame(&ctx, 1, &cfg)
        .expect("valid solver inputs");
    let m = outcome.transform.m;
    // 1e-12: the same tolerance `transform.rs`'s own similarity-shape check
    // uses -- composing several per-level similarity matrices accumulates
    // float error in the bottom-right entry (measured: `0.9999999999999997`),
    // so this is a rounding tolerance, not a structural one.
    assert!((m[2][0] - 0.0).abs() < 1e-12, "{m:?}");
    assert!((m[2][1] - 0.0).abs() < 1e-12, "{m:?}");
    assert!((m[2][2] - 1.0).abs() < 1e-12, "{m:?}");
    assert!((m[0][0] - m[1][1]).abs() < 1e-12, "{m:?}");
    assert!((m[0][1] + m[1][0]).abs() < 1e-12, "{m:?}");
}

#[test]
fn an_80px_shift_with_a_clean_border_converges_through_the_pyramid() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);
    let w_true = Transform::translation(80.0, 0.0);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("80px translation stays well inside the frame");

    // The warp zero-fills columns 432..512. A zero step edge inside the solve
    // region bleeds through the level-0 smoothing, whereas real frames are
    // edge-repeated at the frame boundary, so column 431 is repeated instead.
    let (w, s) = (width as usize, spp as usize);
    let mut filled = warped.tile.samples;
    for row in filled.chunks_exact_mut(w * s) {
        let (content, border) = row.split_at_mut(432 * s);
        let edge = &content[431 * s..];
        for px in border.chunks_exact_mut(s) {
            px.copy_from_slice(edge);
        }
    }

    let reader = two_frame_reader(width, height, spp, filled, tile.samples.clone());
    let stack_ctx = StackContext::new(2, width, height, spp);
    let ctx = AlignContext::new(stack_ctx, 0, &reader);

    let driven = PyramidalEcc
        .align_frame(&ctx, 1, &cfg_with("translation", 3, 100))
        .expect("valid solver inputs");
    assert_eq!(driven.status, AlignStatus::Converged, "outcome: {driven:?}");
    let error = corner_error(&driven.transform, &w_true, default_region());
    eprintln!(
        "an_80px_shift_with_a_clean_border_converges_through_the_pyramid: corner error = {error} px"
    );
    assert!(
        error < LARGE_TRANSLATION_TOLERANCE_PX,
        "corner error {error} px exceeds {LARGE_TRANSLATION_TOLERANCE_PX} px"
    );
}

#[test]
fn reference_frame_is_identity() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let cfg = resolved_defaults(&stack_ctx);

    let result = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
    )
    .expect("real fixture frames register");

    assert_eq!(result.reference, 3, "middle of 8 frames is (8-1)/2 = 3");
    assert_eq!(result.frames.len(), 8);
    let reference_frame = &result.frames[3];
    assert_eq!(reference_frame.transform, Transform::identity());
    assert_eq!(
        reference_frame.iterations, 0,
        "the reference short-circuit reads no pixel"
    );

    for (i, frame) in result.frames.iter().enumerate() {
        eprintln!(
            "reference_frame_is_identity: frame {i} status={:?} iterations={} residual={}",
            frame.status, frame.iterations, frame.residual
        );
        if i == 3 {
            continue;
        }
        if i == 0 {
            // Measured 2026-09-13, default settings (rotation-scale,
            // auto pyramid-depth = 3, max-iterations = 50): frame 0
            // genuinely diverges against the middle-anchor frame 3
            // (`Diverged(LemmaOneRegion)` on the coarsest level's very
            // first Newton step), not a solver defect -- consistent with a
            // measured high-passed NCC of <= 0.07 for frames two or more
            // steps from the anchor. The solver reports this honestly
            // (`Diverged`, best-rho transform = identity, since the very first
            // step failed) rather than fabricating a plausible-looking match.
            assert!(
                matches!(frame.status, AlignStatus::Diverged(_)),
                "frame 0's own divergence must still be reported, not silently \
                 substituted: {frame:?}"
            );
            continue;
        }
        assert_ne!(
            frame.transform,
            Transform::identity(),
            "frame {i} must not report identity against a different reference: {frame:?}"
        );
    }
}

#[test]
fn unregisterable_frame_follows_failure_policy() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);

    let shift = Transform::translation(8.0, 3.0);
    let warped = warp_from_tile(&tile, (width, height), rect, &shift)
        .expect("small translation stays well inside the frame");

    let mut rng = XorShift32(0xC0FFEE);
    let noise: Vec<u16> = (0..tile.samples.len())
        .map(|_| (rng.next() % 65536) as u16)
        .collect();

    let stack_ctx = StackContext::new(3, width, height, spp);
    let mut cfg = resolved_defaults(&stack_ctx);
    cfg.insert(
        "reference-frame".to_string(),
        ParamValue::Enum("first".to_string()),
    );

    let make_reader = || FramesReader {
        width,
        height,
        spp,
        frames: vec![
            tile.samples.clone(),
            warped.tile.samples.clone(),
            noise.clone(),
        ],
    };

    let strict = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &make_reader(),
        &cfg,
        FailurePolicy::Strict,
    )
    .expect("the run completes even with a failing frame");
    assert_eq!(strict.frames.len(), 3, "every frame carries an outcome");
    assert_eq!(
        strict.failure_count(),
        1,
        "only the noise frame must fail: {:?}",
        strict.frames
    );
    let strict_failed = strict.failed().next().expect("one failure");
    assert_eq!(strict_failed.frame, 2);
    assert!(!strict_failed.fallback);
    // Strict reports whatever the solver's own best-rho transform was --
    // here that happens to be identity, because the noise pair diverges on
    // the coarsest level's very first step, before any update is applied.
    // `fallback == false` is what proves this is the solver's honest
    // answer and not a substitution.
    eprintln!(
        "unregisterable_frame_follows_failure_policy: strict best-rho transform = {:?}",
        strict_failed.transform
    );

    let fallback = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &make_reader(),
        &cfg,
        FailurePolicy::IdentityFallback,
    )
    .expect("the run completes even with a failing frame");
    assert_eq!(fallback.frames.len(), 3);
    assert_eq!(fallback.failure_count(), 1);
    let fallback_failed = fallback.failed().next().expect("one failure");
    assert_eq!(fallback_failed.frame, 2);
    assert!(fallback_failed.fallback);
    assert_eq!(fallback_failed.transform, Transform::identity());
}

#[test]
fn anchor_and_chain_policies_both_run() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let mut cfg = resolved_defaults(&stack_ctx);

    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );
    let anchor = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
    )
    .expect("anchor run completes");
    assert_eq!(anchor.frames.len(), 8);

    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("chain".to_string()),
    );
    let chain = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
    )
    .expect("chain run completes");
    assert_eq!(chain.frames.len(), 8);

    // Measured 2026-09-13 (`-- --nocapture`), not asserted at fixture
    // scale -- ground truth is weak on this fixture either way.
    let last = 7;
    let diff = corner_error(
        &anchor.frames[last].transform,
        &chain.frames[last].transform,
        default_region(),
    );
    eprintln!(
        "anchor_and_chain_policies_both_run: frame {last} anchor-vs-chain corner difference = {diff} px"
    );
}

#[test]
fn outcome_distinguishes_converged_from_capped() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);
    let w_true = Transform::translation(6.5, -4.25);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("translation stays well inside the frame");

    let reader = two_frame_reader(
        width,
        height,
        spp,
        warped.tile.samples.clone(),
        tile.samples.clone(),
    );
    let stack_ctx = StackContext::new(2, width, height, spp);
    let ctx = AlignContext::new(stack_ctx, 0, &reader);

    let converged = PyramidalEcc
        .align_frame(&ctx, 1, &cfg_with("translation", 3, 50))
        .expect("valid solver inputs");
    assert_eq!(converged.status, AlignStatus::Converged, "{converged:?}");

    let capped = PyramidalEcc
        .align_frame(&ctx, 1, &cfg_with("translation", 3, 1))
        .expect("valid solver inputs");
    assert_ne!(
        capped.status,
        AlignStatus::Converged,
        "one iteration per level must not be enough to converge: {capped:?}"
    );

    for outcome in [&converged, &capped] {
        assert!(
            outcome.residual.is_finite() && (-1.0..=1.0).contains(&outcome.residual),
            "residual {} is out of [-1, 1]",
            outcome.residual
        );
    }
    assert_ne!(converged.status, capped.status);
}

#[test]
fn rejects_out_of_range_parameters() {
    let stack_ctx = StackContext::new(2, 512, 512, 3);
    let reader = PanicReader;
    let ctx = AlignContext::new(stack_ctx, 0, &reader);

    let mut bad_iterations = cfg_with("translation", 3, 50);
    bad_iterations.insert("max-iterations".to_string(), ParamValue::Int(0));
    match PyramidalEcc.align_frame(&ctx, 1, &bad_iterations) {
        Err(AlgorithmError::Params(_)) => {}
        other => panic!("expected AlgorithmError::Params for max-iterations=0, got {other:?}"),
    }

    let mut bad_epsilon = cfg_with("translation", 3, 50);
    bad_epsilon.insert("epsilon-px".to_string(), ParamValue::Float(-1.0));
    match PyramidalEcc.align_frame(&ctx, 1, &bad_epsilon) {
        Err(AlgorithmError::Params(_)) => {}
        other => panic!("expected AlgorithmError::Params for epsilon-px=-1.0, got {other:?}"),
    }
}

/// Opens `fixtures/mantis-512` with its `StackContext` and fully resolved
/// [`PyramidalEcc`] defaults -- the setup both whole-fixture tests below
/// share.
fn fixture_with_defaults() -> (TiffStack, StackContext, ParamValues) {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let cfg = resolved_defaults(&stack_ctx);
    (stack, stack_ctx, cfg)
}

/// Every non-reference fixture frame, traced level by level at fully
/// resolved defaults. Prints one row per [`EccLevelRecord`], plus a diverged
/// level's last few [`EccIteration`] rows.
#[test]
fn each_divergence_reports_its_pyramid_level() {
    let (stack, stack_ctx, cfg) = fixture_with_defaults();
    let reference = 3usize;
    assert_eq!(reference, (stack.len() - 1) / 2, "middle of 8 frames is 3");
    let ctx = AlignContext::new(stack_ctx, reference, &stack);

    for frame in 0..stack.len() {
        if frame == reference {
            continue;
        }
        let mut levels: Vec<EccLevelRecord> = Vec::new();
        let outcome = PyramidalEcc
            .align_frame_traced(&ctx, frame, &cfg, &mut levels)
            .expect("fixture frames register");
        eprintln!(
            "each_divergence_reports_its_pyramid_level: frame={frame} status={:?} \
             iterations={} residual={}",
            outcome.status, outcome.iterations, outcome.residual
        );
        for rec in &levels {
            eprintln!(
                "  level={} status={:?} iterations={} correlation={:.6} \
                 valid_fraction={:.6} final_step={:?}",
                rec.level,
                rec.result.status,
                rec.result.iterations,
                rec.result.correlation,
                rec.result.valid_fraction,
                rec.result.final_step
            );
            if matches!(rec.result.status, EccStatus::Diverged(_)) {
                let tail = rec.trace.len().min(5);
                for it in &rec.trace[rec.trace.len() - tail..] {
                    eprintln!(
                        "    iteration correlation={:.6} valid_fraction={:.6} step={:.6}",
                        it.correlation, it.valid_fraction, it.step
                    );
                }
            }
        }

        let last = levels
            .last()
            .expect("at least the coarsest level is always solved");
        if matches!(
            outcome.status,
            AlignStatus::Diverged(DivergenceReason::ValidationFailed | DivergenceReason::WeakMatch)
        ) {
            assert!(
                matches!(
                    AlignStatus::from(last.result.status),
                    AlignStatus::Converged | AlignStatus::HitCap | AlignStatus::Stalled
                ),
                "frame {frame}: only a solve that finished can fail the off-centre check: {levels:?}"
            );
        } else {
            let severity = |s: AlignStatus| match s {
                AlignStatus::Converged => 0,
                AlignStatus::HitCap => 1,
                AlignStatus::Stalled => 2,
                _ => 3,
            };
            // A middle level's `correlation-decreased` divergence hands its
            // fit on and counts as `Stalled`.
            let depth = levels[0].level;
            let worst = levels
                .iter()
                .map(|rec| match rec.result.status {
                    EccStatus::Diverged(DivergenceReason::CorrelationDecreased)
                        if rec.level > 0 && rec.level < depth =>
                    {
                        AlignStatus::Stalled
                    }
                    status => AlignStatus::from(status),
                })
                .max_by_key(|&s| severity(s))
                .expect("at least the coarsest level is always solved");
            assert_eq!(
                worst, outcome.status,
                "frame {frame}: the reported status must be the worst level status"
            );
        }
        if matches!(last.result.status, EccStatus::Diverged(_)) {
            assert!(
                levels.iter().all(|rec| rec.level >= last.level),
                "frame {frame}: a level finer than the one that diverged must not follow it: \
                 {levels:?}"
            );
        }
    }
}

/// The complete eight-frame fixture set at fully resolved defaults under the
/// lenient policy, so every frame is checked. Frames 00, 01 and 07 diverge on
/// this 512x512 crop for reasons written down beside
/// [`each_divergence_reports_its_pyramid_level`] -- content the crop does not
/// carry enough of, not a solver defect. Frame 06 solves with an 11% zoom
/// the off-centre check rejects; every other frame converges.
#[test]
fn the_whole_checked_in_stack_aligns() {
    let (stack, stack_ctx, cfg) = fixture_with_defaults();

    let result = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::IdentityFallback,
    )
    .expect("the run completes even with diverging frames");

    assert_eq!(result.reference, 3, "middle of 8 frames is (8-1)/2 = 3");
    assert_eq!(result.frames.len(), 8);

    let expected = [
        AlignStatus::Diverged(DivergenceReason::LemmaOneRegion),
        AlignStatus::Diverged(DivergenceReason::TooFewValidSamples),
        AlignStatus::Converged,
        AlignStatus::Converged,
        AlignStatus::Converged,
        AlignStatus::Converged,
        AlignStatus::Diverged(DivergenceReason::ValidationFailed),
        AlignStatus::Diverged(DivergenceReason::CorrelationDecreased),
    ];

    for (i, frame) in result.frames.iter().enumerate() {
        assert_eq!(frame.frame, i, "frames must be reported in stack order");
        assert_eq!(frame.status, expected[i], "frame {i} status: {frame:?}");
        assert_eq!(
            frame.fallback,
            matches!(expected[i], AlignStatus::Diverged(_)),
            "exactly the diverged frames fall back to identity: frame {i}: {frame:?}"
        );
    }

    let reference_frame = &result.frames[3];
    assert_eq!(reference_frame.transform, Transform::identity());
    assert_eq!(reference_frame.residual, 1.0);

    assert_eq!(
        result.failure_count(),
        4,
        "exactly the four documented divergences: {:?}",
        result.frames
    );
}

#[test]
fn pyramid_depth_beyond_the_resolution_is_a_named_error() {
    let (tile, width, height, spp) = frame_3_tile();
    let reader = two_frame_reader(
        width,
        height,
        spp,
        tile.samples.clone(),
        tile.samples.clone(),
    );
    let stack_ctx = StackContext::new(2, width, height, spp);
    let ctx = AlignContext::new(stack_ctx, 0, &reader);

    // max_levels(512, 512) == 7, so 9 is beyond the resolution.
    let cfg = cfg_with("translation", 9, 50);
    match PyramidalEcc.align_frame(&ctx, 1, &cfg) {
        Err(AlgorithmError::Failed { algorithm, detail }) => {
            assert_eq!(algorithm, "pyramidal-ecc");
            assert!(
                detail.contains("exceeds the maximum"),
                "detail must carry PyramidError::LevelsTooDeep's own message: {detail}"
            );
        }
        other => panic!("expected AlgorithmError::Failed, got {other:?}"),
    }
}

/// Texture only in a frame ring, a flat grey where the solve region sits: the
/// level-0 solve has nothing to register on and reports it, in both modes. The
/// off-centre check never recovers the pair, because it never re-solves.
#[test]
fn an_empty_centre_reports_a_singular_hessian_at_level_0() {
    let (width, height) = (256u32, 256u32);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut frame = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let ring = x < 24 || x >= width - 24 || y < 24 || y >= height - 24;
            frame.push(if ring { (state >> 48) as u16 } else { 30_000 });
        }
    }
    let reader = two_frame_reader(width, height, 1, frame.clone(), frame);
    let ctx = AlignContext::new(StackContext::new(2, width, height, 1), 0, &reader);

    for mode in ["translation", "rotation-scale"] {
        let outcome = PyramidalEcc
            .align_frame(&ctx, 1, &cfg_with(mode, 0, 20))
            .expect("a flat centre is an outcome, not an error");
        assert_eq!(
            outcome.status,
            AlignStatus::Diverged(DivergenceReason::SingularHessian),
            "{mode}"
        );
    }
}

/// Crossed sinusoid gratings (periods 14 and 18 px) have no unique match once
/// a shift passes half a period, so each frame must end `Diverged` or inside
/// its budget of the known motion; a wrong `Converged` fit is the failure.
#[test]
fn a_periodic_scene_never_reports_a_wrong_converged_fit() {
    let (width, height) = (640u32, 480u32);
    let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
    let scene = |x: f64, y: f64| {
        let grating = |p: f64, period: f64| (std::f64::consts::TAU * p / period).sin();
        0.5 + 0.25 * grating(x, 14.0) + 0.25 * grating(y, 18.0)
    };
    // Frame k holds the scene moved by `truth(k)`: it maps frame 0's
    // coordinates onto frame k's.
    let truth = |k: usize| {
        let k = k as f64;
        Transform::translation(3.5 * k, -2.6 * k).compose(&similarity_about(
            1.0,
            (0.3 * k).to_radians(),
            cx,
            cy,
        ))
    };

    let mut rng = XorShift32(0xBEEF_0042);
    let frames: Vec<Vec<u16>> = (0..5)
        .map(|k| {
            let inverse = truth(k).inverse().expect("a similarity is invertible");
            let mut samples = Vec::with_capacity((width * height) as usize);
            for y in 0..height {
                for x in 0..width {
                    let (sx, sy) = inverse
                        .map_point(x as f64, y as f64)
                        .expect("a similarity maps every pixel");
                    let noise = (rng.next() % 601) as f64 - 300.0;
                    samples.push((scene(sx, sy) * 65_535.0 + noise).clamp(0.0, 65_535.0) as u16);
                }
            }
            samples
        })
        .collect();
    let reader = FramesReader {
        width,
        height,
        spp: 1,
        frames,
    };

    let result = align_stack(
        &PyramidalEcc,
        StackContext::new(5, width, height, 1),
        &reader,
        &cfg_with("rotation-scale", 3, 400),
        FailurePolicy::Strict,
    )
    .expect("a periodic stack is an outcome, not an error");

    assert_eq!(result.reference, 0);
    for frame in result.frames.iter().skip(1) {
        let error = corner_error(
            &frame.transform,
            &truth(frame.frame),
            full_rect(width, height),
        );
        eprintln!(
            "a_periodic_scene_never_reports_a_wrong_converged_fit: frame {} status={:?} error={error} px",
            frame.frame, frame.status
        );
        let budget = 0.04 + 0.03 * frame.frame as f64;
        assert!(
            matches!(frame.status, AlignStatus::Diverged(_)) || error <= budget,
            "frame {} reports {:?} at {error} px against a {budget} px budget",
            frame.frame,
            frame.status
        );
    }
}
