// SPDX-License-Identifier: GPL-3.0-or-later

//! `pyramidal-ecc`: the [`AlignAlgorithm`] driving [`crate::align::solve_level`]
//! coarse-to-fine over an [`crate::align::AlignPyramid`] pair, in two modes
//! -- translation-only and rotation+scale -- selected by the `mode`
//! parameter.
//!
//! # The coarse-to-fine loop
//!
//! For pyramid level `k` from `pyramid-depth` down to `0`,
//! [`crate::align::solve_level`] runs over a `PyramidalEcc::solve_region`-sized
//! rect, seeded from the previous
//! (coarser) level's result rescaled by `S = Transform::similarity(2.0,
//! 0.0, 0.0, 0.0)`: `seed_{k-1} = S.compose(&result_k).compose(&S_inv)`.
//! `epsilon-px` is a step measure in the level's own pixels and applies as
//! given at level 0, which produces the result; levels above 0 stop at
//! [`COARSE_EPSILON_FACTOR`] times it, since level 0 refines what they leave.
//! A frame's status is the worst over its levels (`Converged` < `HitCap` <
//! `Stalled` < `Diverged`). A [`crate::align::EccStatus::Diverged`] status at any
//! level ends the loop there (the seed for finer levels would be
//! worthless), but the best-ρ transform that level found is still rescaled
//! all the way to level-0 coordinates, so a caller always gets a level-0
//! answer. The exception is a `CorrelationDecreased` divergence at a middle
//! level (`0 < k < pyramid-depth`, a coarser level having finished): its
//! best-ρ transform seeds level `k - 1` and the level counts as `Stalled`,
//! since a defocused frame loses correlation there on noise, not on a wrong
//! seed. The coarsest level and every other reason still end the loop.
//!
//! # The solve region
//!
//! [`crate::align::solve_level`]'s inner loop is bicubic, 16 taps per
//! sample per iteration -- a full-frame level-0 solve on an 8192x5464
//! frame is ~716M taps per iteration, which buys nothing a six-parameter
//! fit needs. `PyramidalEcc::solve_region` instead solves a centred rect covering
//! [`REGION_FRACTION`] of each axis, shrunk (aspect preserved) to
//! [`MAX_REGION_PIXELS`] when that rect would exceed it. This is safe for
//! the 4-DOF mode because rotation and scale get their lever arm from the
//! *coarse* levels, where the region is most of the (small) level; the
//! budgeted level-0 pass only refines an estimate already close.
//!
//! Level 0's budget is [`LEVEL0_REGION_PIXELS`], the footprint level 1 solves
//! over at full resolution: a smaller rect follows its own centre away from
//! the level-1 fit (a jump of 40 px and more at 2^20 px on a 33-frame
//! 5230x2868 stack, a 0.03 px median move at this budget).
//!
//! # Level-0 windows
//!
//! Level 0 is never resident. The reference template is prepared from the
//! region grown by one pixel; each frame's level-0 solve reads only the
//! window `Transform::source_footprint` names for the seed, grown by
//! [`LEVEL0_WINDOW_MARGIN_PX`] and held within [`LEVEL0_WINDOW_LIMIT_PX`] of
//! the region. Coordinates stay full-frame, so a solve whose samples all fall
//! inside the window is bit-identical to one over the whole plane. Level 0
//! is low-passed with [`LEVEL0_SMOOTHING_PASSES`] binomial passes, template
//! and frame alike, over a window grown by the smoothing reach and cropped
//! back, so windowed and whole-plane smoothing agree; a solve region within
//! that reach of an edge reads level 0 unsmoothed. Coarse levels, the
//! off-centre patches and the feature seed are never smoothed. A result
//! whose footprint leaves the window counts one
//! [`TimingCounter::Level0WindowMisses`] and is solved again over a window
//! regrown to cover seed and result (counted again if still capped).
//!
//! # The off-centre check
//!
//! After a frame that did not diverge, `PyramidalEcc::validate`
//! evaluates the fit at up to eight off-centre patches; a frame where most
//! usable ones exceed [`VALIDATION_TOLERANCE_PX`] is reported
//! [`crate::align::DivergenceReason::ValidationFailed`], fit kept. A frame
//! whose final correlation lies below [`MIN_FINAL_CORRELATION`] is reported
//! [`crate::align::DivergenceReason::WeakMatch`] instead, fit kept, without
//! the patch evaluation.

use std::sync::{Arc, LazyLock, OnceLock};

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus};
use crate::align::ecc::{EccTemplate, solve_prepared, solve_prepared_on};
use crate::align::feature_seed::{MAX_FEATURES, SEED_PROXY_LONG_SIDE};
use crate::align::pyramid::StreamError;
use crate::align::{
    AlignError, AlignPyramid, DivergenceReason, EccIteration, EccMode, EccParams, EccResult,
    EccStatus, Features, KERNEL_RADIUS, LumaAssembler, SeedFit, SourceFootprint, Transform,
    fit_between,
};
use crate::fuse::pyramid::{KERNEL, Plane};
use crate::params::{EnumVariant, ParamDesc, ParamKind, ParamValue, ParamValues, StackContext};
use crate::registry::AlignRegistration;
use crate::tile::plan::{DEFAULT_TILE_SIZE, MAX_LEVELS};
use crate::tile::{Rect, Sample, SampleFormat};
use crate::timing::{TimingCounter, measurement_scope, record_level_iterations, record_timing};

/// Pixels the level-0 window extends past the seed's source footprint. Absorbs
/// the iterates' drift from the seed; a result that drifts further regrows the
/// window once.
const LEVEL0_WINDOW_MARGIN_PX: u32 = 64;

/// The farthest the level-0 window extends past the solve region on any side,
/// however far the seed or a result wanders. Bounds the window's memory, which
/// the accounting reserves.
const LEVEL0_WINDOW_LIMIT_PX: u32 = 256;

/// Binomial passes ([`KERNEL`], the filter pyramid level 1 is reduced with)
/// applied to the reference template and every level-0 frame read. Depth-varying
/// defocus blurs the frames of a pair differently and biases the finest-detail
/// optimum; on a 64-frame 512x512 synthetic stack with it, 21 frames end over
/// their error budget unsmoothed, 5 after one pass, 2 after two, none after
/// three (the worst at 0.96 of its budget).
const LEVEL0_SMOOTHING_PASSES: u32 = 3;

/// Pixels one smoothing pass reads past the sample it produces.
const SMOOTHING_PASS_RADIUS_PX: u32 = (KERNEL.len() / 2) as u32;

/// Pixels a smoothed level-0 read extends past the window it returns.
const LEVEL0_SMOOTHING_RADIUS_PX: u32 = LEVEL0_SMOOTHING_PASSES * SMOOTHING_PASS_RADIUS_PX;

/// The largest per-patch residual, in level-0 pixels, a fit may leave at an
/// off-centre patch and still count as holding there.
const VALIDATION_TOLERANCE_PX: f64 = 1.0;

/// The lowest final correlation a frame may end on and still count as
/// registered. A solver-robustness floor for gross wrong optima, not a
/// quality measure: a wrong fit on a 64-frame synthetic stack ended at 0.061
/// while the lowest correct synthetic frame reached 0.355 and real frames
/// 0.7 and above.
const MIN_FINAL_CORRELATION: f64 = 0.2;

/// Usable patches a frame needs before the off-centre check may flag it.
const MIN_USABLE_PATCHES: usize = 3;

/// Bounds on the off-centre patch side, in level-0 pixels.
const PATCH_SIDE_RANGE_PX: (u32, u32) = (32, 384);

/// Patch centres as fractions of each axis; the grid is 3x3 without its
/// centre.
const PATCH_CENTRES: [f64; 3] = [0.2, 0.5, 0.8];

/// Bytes per patch-template pixel: values (4), a two-column Jacobian (16) and
/// the full-overlap term (8).
const PATCH_TEMPLATE_BYTES_PER_PX: u64 = 4 + 2 * 8 + 8;

/// The patch footprint, per axis, the check accepts: twice the patch side
/// plus the kernel reach. A fit that stretches a patch further is not evaluated
/// there.
fn patch_window_extent(side: u32) -> u32 {
    2 * side + 2 * KERNEL_RADIUS
}

/// Solver-robustness threshold, not a user setting -- the same class as
/// `crate::align`'s own `PIVOT_EPS` and `CORRELATION_SLACK`. `0.75` keeps
/// an 80px translation on the 512px fixture (valid fraction ~= 0.958) a
/// partial-overlap solve rather than a
/// [`crate::align::DivergenceReason::TooFewValidSamples`] refusal.
const MIN_VALID_FRACTION: f64 = 0.75;

/// Levels above 0 stop at this multiple of `epsilon-px`; level 0, which
/// produces the result, stops at `epsilon-px` itself. Solver-robustness
/// threshold, not a user setting. On the 61-frame 44.8 MP reference stack 100x
/// cut iterations 8,114 to 5,762 and solve time 322 to 245 thread-seconds, and
/// moved no transform by more than 0.0001 px at the solve region's corners
/// (0.0004 px at the frame's); 10x cut the iterations to 6,891.
const COARSE_EPSILON_FACTOR: f64 = 100.0;

/// The fraction of each level axis [`PyramidalEcc::solve_region`] covers,
/// before the sample budget shrink.
const REGION_FRACTION: f64 = 0.75;

/// The sample budget of a coarse level's solve region: at 8192x5464, level 1
/// (4096x2732) has a 75 % rect of 3072x2049 that shrinks to ~1253x836.
const MAX_REGION_PIXELS: u32 = 1 << 20;

/// The sample budget of the level-0 solve region: four times
/// [`MAX_REGION_PIXELS`], the footprint a capped level 1 covers at full
/// resolution. Its four-column rotation-scale Jacobian, 16 B per pixel, stays
/// at 64 MiB.
const LEVEL0_REGION_PIXELS: u32 = 4 * MAX_REGION_PIXELS;

/// The fewest solve-region pixels at which a level is offered to an align
/// backend. Smaller levels cost less to solve than to hand over.
const ECC_BACKEND_MIN_REGION_PX: u64 = 1_000_000;
const _: () = assert!(ECC_BACKEND_MIN_REGION_PX <= MAX_REGION_PIXELS as u64);

/// The floor on a pyramid level's shorter dimension the `auto`
/// pyramid-depth closure refuses to go below. Below ~64px the solve region
/// is a few dozen pixels of a heavily blurred plane, where the solver
/// measures noise rather than motion.
const MIN_COARSEST_EXTENT_PX: u32 = 64;

/// Wraps any of this module's own error sources (align, pyramid) into an
/// [`AlgorithmError::Failed`] tagged with this algorithm's id.
fn wrap_error(source: impl std::fmt::Display) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "pyramidal-ecc".to_string(),
        detail: source.to_string(),
    }
}

fn missing_key(key: &str) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "pyramidal-ecc".to_string(),
        detail: format!(
            "resolved values are missing `{key}`; call crate::params::resolve before align_frame"
        ),
    }
}

/// `min(by_extent, by_floor)`: `by_extent` is [`crate::align::max_levels`]
/// (the frame-extent cap every pyramid in this crate shares), `by_floor` is
/// the largest `k` with `ceil(min(w, h) / 2^k) >= MIN_COARSEST_EXTENT_PX`.
/// Deliberately no tile-geometry term: each level is a whole plane in RAM,
/// so this closure is not subject to a per-tile cap.
/// Measured: `512x512 -> 3`, `8192x5464 -> 6`.
fn auto_pyramid_depth(ctx: &StackContext) -> ParamValue {
    let by_extent = crate::align::max_levels(ctx.width, ctx.height);
    let min_dim = ctx.width.min(ctx.height);

    let mut by_floor = 0u32;
    while by_floor < MAX_LEVELS {
        let next = by_floor + 1;
        let extent_at_next = min_dim.div_ceil(1u32 << next);
        if extent_at_next < MIN_COARSEST_EXTENT_PX {
            break;
        }
        by_floor = next;
    }

    ParamValue::Int(by_extent.min(by_floor) as i64)
}

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "mode".to_string(),
            label: "Mode".to_string(),
            help: "Translation-only (2 DOF) or rotation+scale (4 DOF, for \
                   focus breathing) -- selects which degrees of freedom the \
                   solver moves inside the one projective Transform; the \
                   unmoved degrees of freedom stay at their identity values."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "translation".to_string(),
                        label: "Translation only".to_string(),
                    },
                    EnumVariant {
                        id: "rotation-scale".to_string(),
                        label: "Rotation + Scale".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("rotation-scale".to_string()),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "max-iterations".to_string(),
            label: "Max Iterations".to_string(),
            help: "The per-level iteration cap. A level that reaches it stops \
                   short of `epsilon-px` and reports `HitCap`, keeping its last \
                   transform rather than failing; when its last ten steps were \
                   flat or oscillating it reports `Stalled` and keeps the best \
                   correlation transform instead. \
                   Frames that converge earlier stop on `epsilon-px` and pay \
                   nothing. On a 61-frame 44.8 MP stack no level needed more \
                   than 202 iterations."
                .to_string(),
            kind: ParamKind::Int { min: 1, max: 1000 },
            default: ParamValue::Int(400),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "epsilon-px".to_string(),
            label: "Epsilon (px)".to_string(),
            help: "Maximum corner displacement in level pixels -- the \
                   per-iteration step measure the solver converges to, not \
                   a correlation threshold. Applies at full resolution; \
                   coarser levels stop at 100 times it."
                .to_string(),
            kind: ParamKind::Float {
                min: 1e-9,
                max: 1.0,
                step: 1e-5,
                logarithmic: true,
            },
            default: ParamValue::Float(1e-4),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "pyramid-depth".to_string(),
            label: "Pyramid Depth".to_string(),
            help: "How many coarser pyramid levels to register through \
                   before refining at full resolution. `auto` derives the \
                   depth from the loaded frames: the deepest one whose \
                   coarsest level keeps the shorter side at 64 px or more, \
                   so frames shorter than 127 px register at full \
                   resolution only (0) and a 512 px frame gets 3."
                .to_string(),
            kind: ParamKind::Int {
                min: 0,
                max: MAX_LEVELS as i64,
            },
            default: ParamValue::Int(4),
            advanced: true,
            auto: Some(Arc::new(auto_pyramid_depth)),
            host_default: None,
        },
        ParamDesc {
            id: "reference-policy".to_string(),
            label: "Reference Policy".to_string(),
            help: "Whether every frame registers against one anchor frame, \
                   or chains hop-by-hop through its neighbours toward the \
                   reference."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "anchor".to_string(),
                        label: "Anchor".to_string(),
                    },
                    EnumVariant {
                        id: "chain".to_string(),
                        label: "Chain".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("chain".to_string()),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "reference-frame".to_string(),
            label: "Reference Frame".to_string(),
            help: "Which frame the stack registers against: the first, the \
                   middle, or the last."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "first".to_string(),
                        label: "First".to_string(),
                    },
                    EnumVariant {
                        id: "middle".to_string(),
                        label: "Middle".to_string(),
                    },
                    EnumVariant {
                        id: "last".to_string(),
                        label: "Last".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("middle".to_string()),
            advanced: true,
            auto: None,
            host_default: None,
        },
    ]
});

/// The side of the off-centre patches at a `width x height` frame.
fn patch_side(width: u32, height: u32) -> u32 {
    (width.min(height) / 4).clamp(PATCH_SIDE_RANGE_PX.0, PATCH_SIDE_RANGE_PX.1)
}

/// The off-centre patches at a `width x height` frame: the eight cells of a
/// 3x3 grid of centres at 20, 50 and 80 % of each axis, minus the middle one.
/// Empty when any patch would not fit inside the frame's gradient-valid
/// interior, so a small frame is not checked at all.
fn patch_grid(width: u32, height: u32) -> Vec<Rect> {
    let side = i64::from(patch_side(width, height));
    let mut grid = Vec::with_capacity(8);
    for (row, fy) in PATCH_CENTRES.iter().enumerate() {
        for (col, fx) in PATCH_CENTRES.iter().enumerate() {
            if row == 1 && col == 1 {
                continue;
            }
            let x = (f64::from(width) * fx).round() as i64 - side / 2;
            let y = (f64::from(height) * fy).round() as i64 - side / 2;
            let fits =
                x >= 1 && y >= 1 && x + side < i64::from(width) && y + side < i64::from(height);
            if !fits {
                return Vec::new();
            }
            grid.push(Rect {
                x: x as u32,
                y: y as u32,
                width: side as u32,
                height: side as u32,
            });
        }
    }
    grid
}

/// Bytes one set of features holds at most: a keypoint (two `f64`) and a
/// 64-byte descriptor per feature.
const FEATURE_SET_BYTES: u64 = MAX_FEATURES as u64 * (64 + 8);

/// Bytes of scale space the detector allocates per pixel of the level it
/// searches: nine `f32` images per evolution step over four octaves of four
/// sublevels.
const DETECTOR_BYTES_PER_PX: u64 = 192;

/// Bytes of reference templates a run retains, over every level the
/// pyramid can have (`ceil(dim / 2^k)`, `k` up to [`MAX_LEVELS`]): per
/// solve-region pixel a value (4 B), a four-column Jacobian (32 B) and the
/// full-overlap term (8 B), the rotation-scale mode being the larger one --
/// plus the off-centre patch templates and the reference's feature set.
pub(crate) fn retained_template_bytes(width: u32, height: u32) -> u64 {
    let levels: u64 = (0..=MAX_LEVELS)
        .map(|k| {
            let region =
                PyramidalEcc::solve_region(width.div_ceil(1 << k), height.div_ceil(1 << k), k);
            region.width as u64 * region.height as u64 * (4 + 8 * 4 + 8)
        })
        .sum();
    let patches: u64 = patch_grid(width, height)
        .iter()
        .map(|p| p.width as u64 * p.height as u64 * PATCH_TEMPLATE_BYTES_PER_PX)
        .sum();
    levels + patches + FEATURE_SET_BYTES
}

/// Bytes one escalation allocates besides the pyramids: the detector's scale
/// space over the seed level, that level's own read at `4 + 2 * spp` bytes per
/// pixel when it is level 0, and the frame's feature set. Zero for a frame no
/// level of which is small enough to search.
pub(crate) fn seed_scratch_bytes(width: u32, height: u32, spp: u32) -> u64 {
    let Some(level) = seed_level(width, height, MAX_LEVELS) else {
        return 0;
    };
    let px = u64::from(width.div_ceil(1 << level)) * u64::from(height.div_ceil(1 << level));
    let read = if level == 0 {
        px * (4 + 2 * u64::from(spp))
    } else {
        0
    };
    px * DETECTOR_BYTES_PER_PX + read + FEATURE_SET_BYTES
}

/// Bytes one solve allocates besides the templates: three 8-byte-per-pixel
/// vectors over the level-0 solve region, plus the largest level-0 frame
/// read (the region grown by [`LEVEL0_WINDOW_LIMIT_PX`] and the smoothing
/// reach per side, clamped to the frame: the larger of `4 + 2 * spp` bytes per
/// pixel for the `f32` luma and the `u16` samples it was read from and 8 for
/// the luma plus the scratch plane smoothing works in) and the largest
/// off-centre patch window at `4 + 2 * spp` bytes per pixel.
pub(crate) fn solve_scratch_bytes(width: u32, height: u32, spp: u32) -> u64 {
    let region = PyramidalEcc::solve_region(width, height, 0);
    let per_px = 4 + 2 * spp as u64;
    let reach = LEVEL0_WINDOW_LIMIT_PX + LEVEL0_SMOOTHING_RADIUS_PX;
    let window_w = (region.width + 2 * reach).min(width) as u64;
    let window_h = (region.height + 2 * reach).min(height) as u64;
    let patch_extent = patch_window_extent(patch_side(width, height));
    let patch_w = patch_extent.min(width) as u64;
    let patch_h = patch_extent.min(height) as u64;
    region.width as u64 * region.height as u64 * 24
        + window_w * window_h * per_px.max(8)
        + patch_w * patch_h * per_px
}

/// The Pyramidal ECC alignment algorithm. See the module docs for the
/// coarse-to-fine loop and the solve region.
pub struct PyramidalEcc;

impl PyramidalEcc {
    /// The region [`solve_level`](crate::align::solve_level) solves over at
    /// pyramid `level`, `width x height` pixels: a centred rect covering
    /// [`REGION_FRACTION`] of each axis, shrunk (aspect ratio preserved,
    /// about its own centre) so its pixel count never exceeds
    /// [`LEVEL0_REGION_PIXELS`] at level 0 and [`MAX_REGION_PIXELS`] above it.
    /// On a 512px level:
    /// `Rect { x: 64, y: 64, width: 384, height: 384 }`.
    fn solve_region(width: u32, height: u32, level: u32) -> Rect {
        let budget = if level == 0 {
            LEVEL0_REGION_PIXELS
        } else {
            MAX_REGION_PIXELS
        };
        let raw_w = ((width as f64) * REGION_FRACTION).round().max(1.0) as u32;
        let raw_h = ((height as f64) * REGION_FRACTION).round().max(1.0) as u32;

        let count = raw_w as u64 * raw_h as u64;
        let (shrunk_w, shrunk_h) = if count > budget as u64 {
            let factor = (budget as f64 / count as f64).sqrt();
            (
                ((raw_w as f64 * factor).floor().max(1.0)) as u32,
                ((raw_h as f64 * factor).floor().max(1.0)) as u32,
            )
        } else {
            (raw_w, raw_h)
        };

        let shrunk_w = shrunk_w.min(width);
        let shrunk_h = shrunk_h.min(height);

        let x = ((width - shrunk_w) / 2).max(1);
        let y = ((height - shrunk_h) / 2).max(1);

        let valid_x_hi = width.saturating_sub(1);
        let valid_y_hi = height.saturating_sub(1);
        let w = shrunk_w.min(valid_x_hi.saturating_sub(x));
        let h = shrunk_h.min(valid_y_hi.saturating_sub(y));

        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// Streams `frame` in from `ctx` in full-width row bands of
    /// [`DEFAULT_TILE_SIZE`] input rows and reduces them into an
    /// [`AlignPyramid`] of levels `1..=depth` -- never a whole-frame RGB or
    /// luma buffer.
    fn build_pyramid(
        ctx: &AlignContext<'_>,
        frame: usize,
        depth: u32,
    ) -> Result<AlignPyramid, AlgorithmError> {
        let _scope = measurement_scope("pyramid_prepare");
        let stack = ctx.stack();
        let extent = (stack.width, stack.height, stack.samples_per_pixel);
        let band_rows = DEFAULT_TILE_SIZE / 2;
        let streamed = match ctx.sample_format() {
            SampleFormat::U16 => AlignPyramid::stream(extent, depth, band_rows, |rect, out| {
                ctx.read_tile_into(frame, rect, out)
            }),
            SampleFormat::F32 => AlignPyramid::stream(extent, depth, band_rows, |rect, out| {
                ctx.read_tile_f32_into(frame, rect, out)
            }),
        };
        streamed.map_err(|e| match e {
            StreamError::Read(e) => e,
            StreamError::Pyramid(e) => wrap_error(e),
        })
    }

    /// [`AlignAlgorithm::align_frame`], plus one [`EccLevelRecord`] per level
    /// actually solved. The reference frame short-circuits (see `run`'s own
    /// early return) and records nothing. Diagnostic surface: additive, and
    /// never on the path [`AlignAlgorithm::align_frame`] takes.
    pub fn align_frame_traced(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        cfg: &ParamValues,
        levels: &mut Vec<EccLevelRecord>,
    ) -> Result<AlignOutcome, AlgorithmError> {
        self.run(ctx, frame, cfg, Some(levels))
    }

    /// The `mode` parameter as the solver's degrees of freedom.
    fn mode(cfg: &ParamValues) -> Result<EccMode, AlgorithmError> {
        match cfg.get("mode").and_then(ParamValue::as_enum) {
            Some("translation") => Ok(EccMode::Translation),
            Some("rotation-scale") => Ok(EccMode::Similarity),
            Some(other) => Err(AlgorithmError::Failed {
                algorithm: "pyramidal-ecc".to_string(),
                detail: format!("unknown `mode` value `{other}`"),
            }),
            None => Err(missing_key("mode")),
        }
    }

    /// The coarse-to-fine loop shared by [`AlignAlgorithm::align_frame`] and
    /// [`PyramidalEcc::align_frame_traced`]. `levels` is `None` on the
    /// shipped path, so it allocates no trace; `Some` records the per-iteration
    /// trace and one [`EccLevelRecord`] per level solved.
    fn run(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        cfg: &ParamValues,
        levels: Option<&mut Vec<EccLevelRecord>>,
    ) -> Result<AlignOutcome, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;

        // The reference frame needs no pixel read at all.
        if frame == ctx.reference() {
            return Ok(AlignOutcome {
                transform: Transform::identity(),
                status: AlignStatus::Converged,
                iterations: 0,
                residual: 1.0,
            });
        }

        let mode = Self::mode(cfg)?;
        let max_iterations = cfg
            .get("max-iterations")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("max-iterations"))? as u32;
        let epsilon_px = cfg
            .get("epsilon-px")
            .and_then(ParamValue::as_f64)
            .ok_or_else(|| missing_key("epsilon-px"))?;
        let depth = cfg
            .get("pyramid-depth")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("pyramid-depth"))? as u32;

        let params = EccParams {
            max_iterations,
            epsilon_px,
            min_valid_fraction: MIN_VALID_FRACTION,
        };

        let stack = ctx.stack();
        // Accept a driver-prepared reference only when it is the right
        // thing: the right concrete type, prepared for this reference frame
        // and mode, built at the same `pyramid-depth`, at this stack's own
        // dimensions. Any miss falls back to a rebuild, so a mismatch costs
        // speed and never correctness -- see the module docs on
        // `AlignContext::prepared`.
        let prepared = ctx
            .prepared()
            .and_then(|p| p.downcast_ref::<PreparedReference>())
            .filter(|p| {
                p.reference == ctx.reference()
                    && p.mode == mode
                    && p.pyramid.depth() == depth
                    && p.pyramid.dimensions() == (stack.width, stack.height)
            });
        let built;
        let reference: &PreparedReference = match prepared {
            Some(shared) => shared,
            None => {
                built = PreparedReference::build(ctx, ctx.reference(), mode, depth)?;
                &built
            }
        };
        let (attempt, escalation) =
            Self::register(ctx, frame, reference, depth, &params, levels.is_some())?;
        if escalation.attempted {
            record_timing(TimingCounter::FeatureSeedAttempts, 1);
        }
        if escalation.accepted {
            record_timing(TimingCounter::FeatureSeedsAccepted, 1);
        }
        if let Some(out) = levels {
            out.extend(attempt.records);
        }
        Ok(AlignOutcome {
            transform: attempt.transform,
            status: attempt.status,
            iterations: attempt.iterations,
            residual: attempt.residual,
        })
    }

    /// Registers `frame` against the reference from an identity start. A frame
    /// that diverges gets exactly one more run, from a feature-based seed, and
    /// that run's outcome replaces the first only when it does not diverge.
    fn register(
        ctx: &AlignContext<'_>,
        frame: usize,
        reference: &PreparedReference,
        depth: u32,
        params: &EccParams,
        traced: bool,
    ) -> Result<(Attempt, Escalation), AlgorithmError> {
        let frame_pyramid = Self::build_pyramid(ctx, frame, depth)?;
        let first = Self::solve_from(
            ctx,
            frame,
            reference,
            &frame_pyramid,
            depth,
            params,
            Transform::identity(),
            traced,
        )?;
        let mut escalation = Escalation::default();
        if !matches!(first.status, AlignStatus::Diverged(_)) {
            return Ok((first, escalation));
        }
        let Some(seed) = Self::feature_seed(
            ctx,
            frame,
            reference,
            &frame_pyramid,
            depth,
            &mut escalation,
        )?
        else {
            return Ok((first, escalation));
        };
        escalation.rerun = true;
        let seeded = Self::solve_from(
            ctx,
            frame,
            reference,
            &frame_pyramid,
            depth,
            params,
            seed,
            traced,
        )?;
        if matches!(seeded.status, AlignStatus::Diverged(_)) {
            return Ok((first, escalation));
        }
        escalation.accepted = true;
        Ok((seeded, escalation))
    }

    /// A start for the coarsest level, in its pixels, fitted from matched
    /// features between the reference and `frame` on the seed level; `None`
    /// when no level within `depth` is small enough to search, the detector
    /// refuses a level, or the fit fails the inlier and coverage gates.
    fn feature_seed(
        ctx: &AlignContext<'_>,
        frame: usize,
        reference: &PreparedReference,
        frame_pyramid: &AlignPyramid,
        depth: u32,
        escalation: &mut Escalation,
    ) -> Result<Option<Transform>, AlgorithmError> {
        let stack = ctx.stack();
        let Some(level) = seed_level(stack.width, stack.height, depth) else {
            return Ok(None);
        };
        escalation.attempted = true;
        let _scope = measurement_scope("feature_seed");
        let Some(reference_features) = reference
            .features
            .get_or_init(|| detect_level(ctx, reference.reference, &reference.pyramid, level))
            .as_ref()
            .map_err(wrap_error)?
        else {
            return Ok(None);
        };
        let Some(frame_features) = detect_level(ctx, frame, frame_pyramid, level)? else {
            return Ok(None);
        };
        let dims = (
            stack.width.div_ceil(1 << level),
            stack.height.div_ceil(1 << level),
        );
        let fit = fit_between(reference_features, &frame_features, dims);
        tracing::debug!(
            level,
            keypoints_reference = reference_features.len(),
            keypoints_frame = frame_features.len(),
            matches = fit.map_or(0, |f| f.matches),
            inliers = fit.map_or(0, |f| f.inliers),
            occupied_cells = fit.map_or(0, |f| f.occupied_cells),
            "feature seed"
        );
        let Some(fit) = fit.filter(SeedFit::accepted) else {
            return Ok(None);
        };
        let lifted = lift_seed(&fit.transform, level, depth);
        Ok(Some(match reference.mode {
            EccMode::Similarity => lifted,
            // A translation template never updates rotation or scale, so the
            // seed keeps only its displacement at the coarsest region's centre.
            EccMode::Translation => {
                let region = reference.templates[depth as usize]
                    .as_ref()
                    .map_err(wrap_error)?
                    .region();
                let cx = f64::from(region.x) + (f64::from(region.width) - 1.0) / 2.0;
                let cy = f64::from(region.y) + (f64::from(region.height) - 1.0) / 2.0;
                let m = lifted.m;
                Transform::translation(
                    m[0][0] * cx + m[0][1] * cy + m[0][2] - cx,
                    m[1][0] * cx + m[1][1] * cy + m[1][2] - cy,
                )
            }
        }))
    }

    /// One coarse-to-fine pass from `start`, a transform in the coarsest
    /// level's pixels, then the off-centre check on a fit that converged. With
    /// `traced`, the attempt carries one [`EccLevelRecord`] per level solved.
    #[allow(clippy::too_many_arguments)]
    fn solve_from(
        ctx: &AlignContext<'_>,
        frame: usize,
        reference: &PreparedReference,
        frame_pyramid: &AlignPyramid,
        depth: u32,
        params: &EccParams,
        start: Transform,
        traced: bool,
    ) -> Result<Attempt, AlgorithmError> {
        let up = Transform::similarity(2.0, 0.0, 0.0, 0.0);
        let down = Transform::similarity(0.5, 0.0, 0.0, 0.0);

        let mut seed = start;
        let mut records = Vec::new();
        let mut total_iterations = 0u32;
        let mut final_transform = Transform::identity();
        let mut final_status = AlignStatus::Converged;
        let mut final_residual = 1.0f64;
        let mut window_misses = 0u32;
        let coarse_params = EccParams {
            epsilon_px: params.epsilon_px * COARSE_EPSILON_FACTOR,
            ..*params
        };

        let _scope = measurement_scope("ecc_solve");
        for k in (0..=depth).rev() {
            let template = reference.templates[k as usize]
                .as_ref()
                .map_err(wrap_error)?;

            let mut trace = traced.then(Vec::new);
            let result = if k == 0 {
                let solved = solve_level0(
                    ctx,
                    frame,
                    template,
                    &seed,
                    params,
                    LEVEL0_WINDOW_MARGIN_PX,
                    trace.as_mut(),
                )?;
                window_misses += solved.misses;
                solved.result
            } else {
                solve_routed(
                    ctx,
                    template,
                    frame_pyramid.level(k as usize),
                    (0, 0),
                    &seed,
                    &coarse_params,
                    trace.as_mut(),
                )?
            };
            if traced {
                records.push(EccLevelRecord {
                    level: k,
                    result,
                    trace: trace.unwrap_or_default(),
                });
            }
            record_level_iterations(k, result.iterations, &level_status_label(result.status));
            total_iterations += result.iterations;
            final_residual = result.correlation;

            // Rescale this level's transform all the way to level-0
            // coordinates -- `k` applications of the same up/down
            // composition the seed hand-off uses, so a diverging level's
            // best-ρ transform is a usable level-0 answer too.
            let mut at_level0 = result.transform;
            for _ in 0..k {
                at_level0 = up.compose(&at_level0).compose(&down);
            }
            final_transform = at_level0;
            let handed_on = k > 0
                && k < depth
                && result.status == EccStatus::Diverged(DivergenceReason::CorrelationDecreased);
            let level_status = if handed_on {
                AlignStatus::Stalled
            } else {
                result.status.into()
            };
            final_status = worse_status(final_status, level_status);

            match result.status {
                EccStatus::Diverged(_) if !handed_on => break,
                _ => {
                    if k > 0 {
                        seed = up.compose(&result.transform).compose(&down);
                    }
                }
            }
        }
        if window_misses > 0 {
            record_timing(TimingCounter::Level0WindowMisses, u64::from(window_misses));
        }

        if !matches!(final_status, AlignStatus::Diverged(_)) {
            if final_residual < MIN_FINAL_CORRELATION {
                final_status = AlignStatus::Diverged(DivergenceReason::WeakMatch);
            } else {
                let _scope = measurement_scope("ecc_validate");
                let check = reference.validate(ctx, frame, &final_transform, params)?;
                if check.flagged() {
                    final_status = AlignStatus::Diverged(DivergenceReason::ValidationFailed);
                }
            }
        }

        Ok(Attempt {
            transform: final_transform,
            status: final_status,
            iterations: total_iterations,
            residual: final_residual,
            records,
        })
    }
}

/// One coarse-to-fine pass's outcome and, when traced, its per-level records.
#[derive(Debug)]
struct Attempt {
    transform: Transform,
    status: AlignStatus,
    iterations: u32,
    residual: f64,
    records: Vec<EccLevelRecord>,
}

/// What escalating a diverged frame did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Escalation {
    /// The detector ran on the frame.
    attempted: bool,
    /// The fit passed the gates and a seeded pass ran.
    rerun: bool,
    /// The seeded pass did not diverge and its outcome is the one reported.
    accepted: bool,
}

/// A level's outcome as a kebab-case trace label: `converged`, `hit-cap`,
/// `stalled` or `diverged-<reason>`.
fn level_status_label(status: EccStatus) -> String {
    match status {
        EccStatus::Converged => "converged".to_string(),
        EccStatus::HitCap => "hit-cap".to_string(),
        EccStatus::Stalled => "stalled".to_string(),
        EccStatus::Diverged(reason) => {
            let reason = serde_json::to_value(reason).expect("a divergence reason serialises");
            format!(
                "diverged-{}",
                reason.as_str().expect("a unit variant is a string")
            )
        }
    }
}

/// The worse of two level statuses: `Converged` < `HitCap` < `Stalled` <
/// `Diverged`; the first wins a tie.
fn worse_status(a: AlignStatus, b: AlignStatus) -> AlignStatus {
    let rank = |s: AlignStatus| match s {
        AlignStatus::Converged => 0,
        AlignStatus::HitCap => 1,
        AlignStatus::Stalled => 2,
        AlignStatus::Diverged(_) => 3,
    };
    if rank(b) > rank(a) { b } else { a }
}

/// The finest pyramid level within `depth` whose long side is at most
/// [`SEED_PROXY_LONG_SIDE`]; `None` when no level qualifies, so a frame is
/// never searched at a size the accounting does not price.
fn seed_level(width: u32, height: u32, depth: u32) -> Option<u32> {
    (0..=depth)
        .find(|&k| width.div_ceil(1 << k).max(height.div_ceil(1 << k)) <= SEED_PROXY_LONG_SIDE)
}

/// A transform in level-`from` pixels as the solver's coarsest level (`depth`)
/// sees it: the inverse of the seed hand-off between levels.
fn lift_seed(seed: &Transform, from: u32, depth: u32) -> Transform {
    let up = Transform::similarity(2.0, 0.0, 0.0, 0.0);
    let down = Transform::similarity(0.5, 0.0, 0.0, 0.0);
    (from..depth).fold(*seed, |t, _| down.compose(&t).compose(&up))
}

/// Features of `frame` at pyramid `level` (level 0 is read whole, only when
/// [`seed_level`] chose it); `None` when the detector refuses the plane.
fn detect_level(
    ctx: &AlignContext<'_>,
    frame: usize,
    pyramid: &AlignPyramid,
    level: u32,
) -> Result<Option<Features>, AlgorithmError> {
    let whole;
    let plane = if level == 0 {
        let stack = ctx.stack();
        whole = read_luma_window(
            ctx,
            frame,
            Rect {
                x: 0,
                y: 0,
                width: stack.width,
                height: stack.height,
            },
        )?;
        &whole
    } else {
        pyramid.level(level as usize)
    };
    match Features::detect(plane) {
        Ok(features) => Ok(Some(features)),
        Err(refused) => {
            tracing::debug!(%refused, "no feature seed");
            Ok(None)
        }
    }
}

/// `rect` grown by `by` on every side and clamped to a `frame`-sized plane.
fn grown_clamped(rect: Rect, by: u32, (width, height): (u32, u32)) -> Rect {
    let x0 = rect.x.saturating_sub(by);
    let y0 = rect.y.saturating_sub(by);
    let x1 = (rect.x + rect.width + by).min(width);
    let y1 = (rect.y + rect.height + by).min(height);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

fn bounding_union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    Rect {
        x: x0,
        y: y0,
        width: (a.x + a.width).max(b.x + b.width) - x0,
        height: (a.y + a.height).max(b.y + b.height) - y0,
    }
}

/// The frame-clamped source rect `t` samples when it warps `dst`, or `None`
/// when every sample lies outside the frame or the horizon crosses `dst`.
fn footprint(t: &Transform, dst: Rect, frame: (u32, u32)) -> Option<Rect> {
    match t.source_footprint(dst, frame) {
        Ok(SourceFootprint::Covers { read, .. }) => Some(read),
        Ok(SourceFootprint::Outside) | Err(_) => None,
    }
}

/// The level-0 window for a footprint: `fp` grown by `margin`, within
/// [`LEVEL0_WINDOW_LIMIT_PX`] of `region`. A single pixel when there is no
/// footprint (or none within the limit), so every sample reads as out of
/// domain exactly as it does over the whole plane.
fn level0_window(fp: Option<Rect>, region: Rect, margin: u32, frame: (u32, u32)) -> Rect {
    fp.and_then(|fp| {
        grown_clamped(fp, margin, frame).intersect(grown_clamped(
            region,
            LEVEL0_WINDOW_LIMIT_PX,
            frame,
        ))
    })
    .unwrap_or(Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    })
}

/// Whether level 0 is smoothed on a `frame`-sized stack solved over `region`:
/// only when the template window (the region grown by one pixel) and its
/// smoothing reach all lie inside the frame. Template and frame reads share
/// the answer, so both are smoothed or neither is.
fn smooths_level0(region: Rect, (width, height): (u32, u32)) -> bool {
    let by = LEVEL0_SMOOTHING_RADIUS_PX + 1;
    region.x >= by
        && region.y >= by
        && region.x + region.width + by <= width
        && region.y + region.height + by <= height
}

/// [`LEVEL0_SMOOTHING_PASSES`] separable binomial passes over `plane` in place,
/// the edge sample repeated past the border.
fn smooth_level0(plane: &mut Plane) {
    let (w, h) = (plane.width() as usize, plane.height() as usize);
    let tap = |i: usize, d: usize, n: usize| {
        (i + d)
            .saturating_sub(SMOOTHING_PASS_RADIUS_PX as usize)
            .min(n - 1)
    };
    let cur = plane.data_mut();
    let mut next = vec![0f32; cur.len()];
    for _ in 0..LEVEL0_SMOOTHING_PASSES {
        for y in 0..h {
            for x in 0..w {
                next[y * w + x] = KERNEL
                    .iter()
                    .enumerate()
                    .map(|(d, k)| k * cur[y * w + tap(x, d, w)])
                    .sum();
            }
        }
        for y in 0..h {
            for x in 0..w {
                cur[y * w + x] = KERNEL
                    .iter()
                    .enumerate()
                    .map(|(d, k)| k * next[tap(y, d, h) * w + x])
                    .sum();
            }
        }
    }
}

/// Reads `rect` of `frame` as a level-0 luma plane, smoothed when `smooth`
/// (see [`smooths_level0`]). The smoothing reads the window grown by
/// [`LEVEL0_SMOOTHING_RADIUS_PX`] and crops back, so every returned sample
/// equals the one a whole-plane smoothing gives.
fn read_level0_luma(
    ctx: &AlignContext<'_>,
    frame: usize,
    rect: Rect,
    smooth: bool,
) -> Result<Plane, AlgorithmError> {
    if !smooth {
        return read_luma_window(ctx, frame, rect);
    }
    let stack = ctx.stack();
    let grown = grown_clamped(
        rect,
        LEVEL0_SMOOTHING_RADIUS_PX,
        (stack.width, stack.height),
    );
    let mut smoothed = read_luma_window(ctx, frame, grown)?;
    smooth_level0(&mut smoothed);
    let (dx, dy) = ((rect.x - grown.x) as usize, (rect.y - grown.y) as usize);
    let mut out = Vec::with_capacity(rect.width as usize * rect.height as usize);
    for row in 0..rect.height as usize {
        let start = (dy + row) * grown.width as usize + dx;
        out.extend_from_slice(&smoothed.data()[start..start + rect.width as usize]);
    }
    Plane::from_f32_vec(out, rect.width, rect.height, 1).map_err(wrap_error)
}

/// Reads `rect` of `frame` and converts it to a luma plane.
fn read_luma_window(
    ctx: &AlignContext<'_>,
    frame: usize,
    rect: Rect,
) -> Result<Plane, AlgorithmError> {
    match ctx.sample_format() {
        SampleFormat::U16 => {
            luma_window::<u16>(ctx, rect, |out| ctx.read_tile_into(frame, rect, out))
        }
        SampleFormat::F32 => {
            luma_window::<f32>(ctx, rect, |out| ctx.read_tile_f32_into(frame, rect, out))
        }
    }
}

fn luma_window<S: Sample>(
    ctx: &AlignContext<'_>,
    rect: Rect,
    read: impl FnOnce(&mut [S]) -> Result<(), AlgorithmError>,
) -> Result<Plane, AlgorithmError> {
    let spp = ctx.stack().samples_per_pixel;
    let mut samples = vec![S::default(); rect.width as usize * rect.height as usize * spp as usize];
    record_timing(
        TimingCounter::AllocationBytes,
        (samples.capacity() * std::mem::size_of::<S>()) as u64,
    );
    record_timing(TimingCounter::AllocationOperations, 1);
    read(&mut samples)?;
    let mut assembler = LumaAssembler::new(rect.width, rect.height);
    let local = Rect {
        x: 0,
        y: 0,
        width: rect.width,
        height: rect.height,
    };
    assembler
        .push_samples(local, spp, &samples)
        .map_err(wrap_error)?;
    assembler.finish().map_err(wrap_error)
}

/// Solves one pyramid level. A template region of at least
/// [`ECC_BACKEND_MIN_REGION_PX`] pixels is offered to the context's align
/// backend; a level with no backend, a declined one or a smaller region runs on
/// the CPU. The off-centre patch checks call [`solve_prepared`] directly and
/// are never offered.
fn solve_routed(
    ctx: &AlignContext<'_>,
    template: &EccTemplate,
    moving: &Plane,
    moving_origin: (u32, u32),
    seed: &Transform,
    params: &EccParams,
    mut trace: Option<&mut Vec<EccIteration>>,
) -> Result<EccResult, AlgorithmError> {
    let region = template.region();
    let offered = u64::from(region.width) * u64::from(region.height) >= ECC_BACKEND_MIN_REGION_PX;
    if let Some(backend) = ctx.align_backend().filter(|_| offered)
        && let Some(result) = solve_prepared_on(
            template,
            moving,
            moving_origin,
            seed,
            params,
            trace.as_deref_mut(),
            backend,
        )?
    {
        return Ok(result);
    }
    Ok(solve_prepared(
        template,
        moving,
        moving_origin,
        seed,
        params,
        trace,
    ))
}

/// One level-0 solve and the window misses it took.
struct Level0Solve {
    result: EccResult,
    misses: u32,
}

/// Solves level 0 over the window of `frame` the seed's footprint names (see
/// the module docs' "Level-0 windows"). `margin` is the window's slack past
/// the footprint.
fn solve_level0(
    ctx: &AlignContext<'_>,
    frame: usize,
    template: &EccTemplate,
    seed: &Transform,
    params: &EccParams,
    margin: u32,
    trace: Option<&mut Vec<EccIteration>>,
) -> Result<Level0Solve, AlgorithmError> {
    let stack = ctx.stack();
    let dims = (stack.width, stack.height);
    let region = template.region();
    let escaped = |t: &Transform, window: Rect| {
        footprint(t, region, dims).is_some_and(|fp| !contains(window, fp))
    };

    let smooth = smooths_level0(region, dims);
    let seed_fp = footprint(seed, region, dims);
    let window = level0_window(seed_fp, region, margin, dims);
    let plane = read_level0_luma(ctx, frame, window, smooth)?;
    let mut first_trace = trace.is_some().then(Vec::new);
    let result = solve_routed(
        ctx,
        template,
        &plane,
        (window.x, window.y),
        seed,
        params,
        first_trace.as_mut(),
    )?;
    drop(plane);

    let mut misses = 0;
    let result_fp = footprint(&result.transform, region, dims);
    if let Some(fp) = result_fp.filter(|fp| !contains(window, *fp)) {
        misses += 1;
        let covered = seed_fp.map_or(fp, |s| bounding_union(s, fp));
        let regrown = level0_window(Some(covered), region, margin, dims);
        if regrown != window {
            let plane = read_level0_luma(ctx, frame, regrown, smooth)?;
            let mut second_trace = trace.is_some().then(Vec::new);
            let result = solve_routed(
                ctx,
                template,
                &plane,
                (regrown.x, regrown.y),
                seed,
                params,
                second_trace.as_mut(),
            )?;
            if escaped(&result.transform, regrown) {
                misses += 1;
            }
            if let (Some(out), Some(t)) = (trace, second_trace) {
                out.extend(t);
            }
            return Ok(Level0Solve { result, misses });
        }
    }
    if let (Some(out), Some(t)) = (trace, first_trace) {
        out.extend(t);
    }
    Ok(Level0Solve { result, misses })
}

/// The patch residuals of one frame: how many patches were usable and how many
/// of those exceeded [`VALIDATION_TOLERANCE_PX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Validation {
    usable: usize,
    failed: usize,
}

impl Validation {
    /// Enough usable patches, and more than half of them failed.
    fn flagged(&self) -> bool {
        self.usable >= MIN_USABLE_PATCHES && self.failed * 2 > self.usable
    }
}

/// A reference patch's translation template, in full-frame coordinates.
struct PatchTemplate {
    rect: Rect,
    template: EccTemplate,
}

/// What [`AlignAlgorithm::prepare_run`] shares across a run's registrations:
/// the reference frame's coarse pyramid, the solver template per level (level
/// 0's from a window around its solve region) and the off-centre patch
/// templates.
struct PreparedReference {
    reference: usize,
    mode: EccMode,
    pyramid: AlignPyramid,
    /// Indexed by level; a level whose region is invalid keeps its error, so
    /// `run` reports it per frame.
    templates: Vec<Result<EccTemplate, AlignError>>,
    patches: Vec<PatchTemplate>,
    /// The reference's features on the seed level, detected by the first
    /// escalation of a run and shared by the rest; `None` when the detector
    /// refuses that level.
    features: OnceLock<Result<Option<Features>, AlgorithmError>>,
}

impl PreparedReference {
    fn build(
        ctx: &AlignContext<'_>,
        reference: usize,
        mode: EccMode,
        depth: u32,
    ) -> Result<Self, AlgorithmError> {
        let stack = ctx.stack();
        let dims = (stack.width, stack.height);
        let pyramid = PyramidalEcc::build_pyramid(ctx, reference, depth)?;

        let mut templates = Vec::with_capacity(depth as usize + 1);
        let region = PyramidalEcc::solve_region(dims.0, dims.1, 0);
        let window = grown_clamped(region, 1, dims);
        let plane = read_level0_luma(ctx, reference, window, smooths_level0(region, dims))?;
        templates.push(EccTemplate::prepare(
            &plane,
            (window.x, window.y),
            region,
            mode,
        ));
        for k in 1..=depth as usize {
            let level = pyramid.level(k);
            let region = PyramidalEcc::solve_region(level.width(), level.height(), k as u32);
            templates.push(EccTemplate::prepare(level, (0, 0), region, mode));
        }

        let mut patches = Vec::new();
        for rect in patch_grid(dims.0, dims.1) {
            let window = grown_clamped(rect, 1, dims);
            let plane = read_luma_window(ctx, reference, window)?;
            let template =
                EccTemplate::prepare(&plane, (window.x, window.y), rect, EccMode::Translation)
                    .map_err(wrap_error)?;
            patches.push(PatchTemplate { rect, template });
        }

        Ok(PreparedReference {
            reference,
            mode,
            pyramid,
            templates,
            patches,
            features: OnceLock::new(),
        })
    }

    /// Evaluates `fit` at every off-centre patch with a single translation
    /// iteration: no re-fit, the step measure is the residual in level-0
    /// pixels. A patch whose texture is too flat or that falls mostly outside
    /// the frame is unusable, not failed.
    fn validate(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        fit: &Transform,
        params: &EccParams,
    ) -> Result<Validation, AlgorithmError> {
        let stack = ctx.stack();
        let dims = (stack.width, stack.height);
        let one_step = EccParams {
            max_iterations: 1,
            ..*params
        };
        let mut check = Validation {
            usable: 0,
            failed: 0,
        };
        let mut worst = 0.0f64;
        for patch in &self.patches {
            let extent = patch_window_extent(patch.rect.width);
            let Some(window) = footprint(fit, patch.rect, dims)
                .filter(|w| w.width <= extent && w.height <= extent)
            else {
                continue;
            };
            let plane = read_luma_window(ctx, frame, window)?;
            let r = solve_prepared(
                &patch.template,
                &plane,
                (window.x, window.y),
                fit,
                &one_step,
                None,
            );
            match r.status {
                EccStatus::Diverged(
                    DivergenceReason::SingularHessian | DivergenceReason::TooFewValidSamples,
                ) => {}
                EccStatus::Diverged(_) => {
                    check.usable += 1;
                    check.failed += 1;
                }
                EccStatus::Converged | EccStatus::HitCap | EccStatus::Stalled => {
                    check.usable += 1;
                    let step = r.final_step.unwrap_or(f64::INFINITY);
                    worst = worst.max(step);
                    if step > VALIDATION_TOLERANCE_PX {
                        check.failed += 1;
                    }
                }
            }
        }
        tracing::debug!(
            usable = check.usable,
            failed = check.failed,
            worst_residual_px = worst,
            "off-centre check"
        );
        Ok(check)
    }
}

/// One pyramid level of one [`PyramidalEcc::align_frame_traced`] call.
#[derive(Debug, Clone)]
pub struct EccLevelRecord {
    /// The pyramid level, coarse-to-fine as the loop visits it: `depth` down
    /// to `0`.
    pub level: u32,
    /// That level's own result, in that level's own pixels (never rescaled
    /// to level 0).
    pub result: EccResult,
    pub trace: Vec<EccIteration>,
}

impl AlignAlgorithm for PyramidalEcc {
    fn id(&self) -> &str {
        "pyramidal-ecc"
    }

    fn display_name(&self) -> &str {
        "Pyramidal ECC"
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        self.run(ctx, frame, cfg, None)
    }

    /// Builds the reference frame's coarse pyramid and, per level, its solver
    /// template, plus the off-centre patch templates, once, for the driver to
    /// hand back to every [`AlignAlgorithm::align_frame`] call of this run
    /// through [`AlignContext::prepared`]. A level whose solve region is
    /// invalid keeps its error, so `run` reports it per frame, exactly as it
    /// does without a prepared value.
    fn prepare_run(
        &self,
        ctx: &AlignContext<'_>,
        cfg: &ParamValues,
    ) -> Result<Option<Arc<dyn std::any::Any + Send + Sync>>, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let mode = Self::mode(cfg)?;
        let depth = cfg
            .get("pyramid-depth")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("pyramid-depth"))? as u32;
        let prepared = PreparedReference::build(ctx, ctx.reference(), mode, depth)?;
        Ok(Some(Arc::new(prepared)))
    }
}

inventory::submit! { AlignRegistration::new(&PyramidalEcc) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::TileReader;
    use crate::align::ecc::CpuMomentSession;
    use crate::align::feature_seed::test_scene::blob_scene;
    use crate::align::pyramid::max_levels;
    use crate::align::{AlignDriverOptions, FailurePolicy, align_stack_with};
    use crate::gpu::{AlignBackend, ECC_MOMENTS, EccMomentSession, EccSessionRequest};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn schema_declares_every_parameter() {
        let algo = PyramidalEcc;
        let ids: Vec<&str> = algo.params().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "mode",
                "max-iterations",
                "epsilon-px",
                "pyramid-depth",
                "reference-policy",
                "reference-frame",
            ],
            "no seventh parameter -- in particular, no `min-correlation`: \
             convergence is a step-size measure"
        );

        let mode = &algo.params()[0];
        assert_eq!(mode.default, ParamValue::Enum("rotation-scale".to_string()));
        match &mode.kind {
            ParamKind::Enum { variants } => {
                assert_eq!(variants.len(), 2);
                assert!(variants.iter().any(|v| v.id == "translation"));
                assert!(variants.iter().any(|v| v.id == "rotation-scale"));
            }
            other => panic!("expected Enum, got {other:?}"),
        }

        let max_iterations = &algo.params()[1];
        assert_eq!(max_iterations.default, ParamValue::Int(400));
        assert_eq!(max_iterations.kind, ParamKind::Int { min: 1, max: 1000 });

        let epsilon = &algo.params()[2];
        assert_eq!(epsilon.default, ParamValue::Float(1e-4));
        assert_eq!(
            epsilon.kind,
            ParamKind::Float {
                min: 1e-9,
                max: 1.0,
                step: 1e-5,
                logarithmic: true,
            }
        );

        let depth = &algo.params()[3];
        assert_eq!(depth.default, ParamValue::Int(4));
        assert_eq!(
            depth.kind,
            ParamKind::Int {
                min: 0,
                max: MAX_LEVELS as i64,
            }
        );
        assert!(
            depth.auto.is_some(),
            "pyramid-depth must carry an auto hook"
        );

        let reference_policy = &algo.params()[4];
        assert_eq!(
            reference_policy.default,
            ParamValue::Enum("chain".to_string())
        );
        let reference_frame = &algo.params()[5];
        assert_eq!(
            reference_frame.default,
            ParamValue::Enum("middle".to_string())
        );
    }

    #[test]
    fn auto_depth_scales_with_resolution() {
        for (size, depth) in [(64, 0), (126, 0), (127, 1), (512, 3)] {
            let ctx = StackContext::new(8, size, size, 3);
            assert_eq!(auto_pyramid_depth(&ctx), ParamValue::Int(depth), "{size}");
        }

        let ctx_ref = StackContext::new(61, 8192, 5464, 3);
        assert_eq!(auto_pyramid_depth(&ctx_ref), ParamValue::Int(6));
    }

    #[test]
    fn solve_region_is_the_centred_three_quarters_at_512px() {
        let region = PyramidalEcc::solve_region(512, 512, 0);
        assert_eq!(
            region,
            Rect {
                x: 64,
                y: 64,
                width: 384,
                height: 384,
            }
        );
    }

    #[test]
    fn retained_templates_sum_the_solve_regions_of_every_level() {
        // Level regions at 8192x5464: 2507x1672 on level 0 and 1253x836 on
        // levels 1-2 (their sample budgets bind), then the 0.75 share of each
        // halved level from level 3 (768x512) on; 44 bytes per pixel (values,
        // Jacobian, full-overlap terms).
        // The eight 384 px (128 px at 512) patch templates, 28 bytes per
        // pixel, and the reference's 2000 features (72 bytes each) come on top.
        assert_eq!(
            retained_template_bytes(8192, 5464),
            299_706_264 + 8 * 384 * 384 * 28 + 2000 * 72
        );
        assert_eq!(
            retained_template_bytes(512, 512),
            8_650_400 + 8 * 128 * 128 * 28 + 2000 * 72
        );
    }

    #[test]
    fn solve_scratch_adds_the_capped_windows_to_the_solve_vectors() {
        // Per-pixel bytes of a level-0 read: f32 luma plus the u16 samples
        // read into it, `4 + 2 * spp`, or 8 for the luma and the scratch
        // plane smoothing works in, whichever is larger; the window is the
        // region grown by 256 + 6 px a side. An off-centre patch window costs
        // `4 + 2 * spp`.
        assert_eq!(
            solve_scratch_bytes(512, 512, 3),
            24 * 384 * 384 + 512 * 512 * 10 + 260 * 260 * 10
        );
        assert_eq!(
            solve_scratch_bytes(8192, 5464, 3),
            24 * 2507 * 1672 + 3031 * 2196 * 10 + 772 * 772 * 10
        );
        assert_eq!(
            solve_scratch_bytes(8192, 5464, 1),
            24 * 2507 * 1672 + 3031 * 2196 * 8 + 772 * 772 * 6
        );
    }

    #[test]
    fn seed_scratch_prices_the_detector_on_the_seed_level() {
        // 8192x5464 is searched on level 4 (512x342): 192 bytes per pixel of
        // scale space plus the frame's 2000 features (72 bytes each).
        assert_eq!(
            seed_scratch_bytes(8192, 5464, 3),
            512 * 342 * 192 + 2000 * 72
        );
        // A 512 px frame is searched whole at level 0, which is also read
        // in: `4 + 2 * spp` bytes per pixel.
        assert_eq!(
            seed_scratch_bytes(512, 512, 3),
            512 * 512 * (192 + 10) + 2000 * 72
        );
        assert_eq!(
            seed_scratch_bytes(1 << 26, 1 << 26, 1),
            0,
            "no level to search"
        );
    }

    #[test]
    fn the_patch_grid_is_eight_patches_inside_the_interior_or_nothing() {
        let grid = patch_grid(1031, 777);
        assert_eq!(grid.len(), 8);
        for patch in &grid {
            assert_eq!((patch.width, patch.height), (194, 194));
            assert!(patch.x >= 1 && patch.x + patch.width <= 1030, "{patch:?}");
            assert!(patch.y >= 1 && patch.y + patch.height <= 776, "{patch:?}");
        }
        let centre = (515i64, 388i64);
        assert!(grid.iter().all(|p| {
            let c = (
                i64::from(p.x) + i64::from(p.width / 2),
                i64::from(p.y) + i64::from(p.height / 2),
            );
            (c.0 - centre.0).abs() > 100 || (c.1 - centre.1).abs() > 100
        }));
        assert!(
            patch_grid(64, 64).is_empty(),
            "a 64 px frame is not checked"
        );
        assert_eq!(patch_side(8192, 5464), 384);
    }

    /// Frames in memory, one `u16` sample per pixel.
    struct MemReader {
        width: u32,
        frames: Vec<Vec<u16>>,
    }

    impl TileReader for MemReader {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            for row in 0..rect.height as usize {
                let src = (rect.y as usize + row) * self.width as usize + rect.x as usize;
                out[row * rect.width as usize..][..rect.width as usize]
                    .copy_from_slice(&self.frames[frame][src..src + rect.width as usize]);
            }
            Ok(())
        }
    }

    /// [`MemReader`]'s frames served as `f32` only: its `u16` read fails.
    struct WideReader(MemReader);

    impl TileReader for WideReader {
        fn sample_format(&self) -> SampleFormat {
            SampleFormat::F32
        }

        fn read_tile_into(&self, _: usize, _: Rect, _: &mut [u16]) -> Result<(), AlgorithmError> {
            Err(AlgorithmError::Failed {
                algorithm: "wide".to_string(),
                detail: "serves f32 only".to_string(),
            })
        }

        fn read_tile_f32_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [f32],
        ) -> Result<(), AlgorithmError> {
            let mut narrow = vec![0u16; out.len()];
            self.0.read_tile_into(frame, rect, &mut narrow)?;
            for (wide, narrow) in out.iter_mut().zip(narrow) {
                *wide = f32::from(narrow);
            }
            Ok(())
        }
    }

    #[test]
    fn ecc_reads_an_f32_serving_context_through_its_f32_read() {
        let shifts = [(0.0, 0.0), (3.0, -2.0)];
        let wide = WideReader(reader(W, H, &shifts, false));
        let narrow = reader(W, H, &shifts, false);
        let stack = StackContext::new(2, W, H, 1);
        let mut cfg = crate::params::resolved_values(
            &crate::params::resolve(PyramidalEcc.params(), &ParamValues::new(), &stack).unwrap(),
        );
        cfg.insert("pyramid-depth".to_string(), ParamValue::Int(1));
        cfg.insert("max-iterations".to_string(), ParamValue::Int(20));
        cfg.insert(
            "mode".to_string(),
            ParamValue::Enum("translation".to_string()),
        );
        let found = PyramidalEcc
            .align_frame(&AlignContext::new(stack, 0, &wide), 1, &cfg)
            .unwrap();
        let expected = PyramidalEcc
            .align_frame(&AlignContext::new(stack, 0, &narrow), 1, &cfg)
            .unwrap();
        assert_eq!(found, expected);
        let (_, _, tx, ty) = found.transform.similarity_parts();
        assert!(
            (tx - 3.0).abs() < 0.05 && (ty + 2.0).abs() < 0.05,
            "({tx}, {ty})"
        );
    }

    /// The scene's sinusoids: spatial frequency per pixel (x, y), phase,
    /// amplitude and whether it is fine detail.
    const RIPPLES: [(f64, f64, f64, f64, bool); 8] = [
        (1.0 / 97.0, 1.0 / 131.0, 0.3, 1.0, false),
        (1.0 / 61.0, -1.0 / 83.0, 1.7, 0.8, false),
        (-1.0 / 47.0, 1.0 / 71.0, 4.1, 0.6, false),
        (1.0 / 113.0, 1.0 / 37.0, 2.4, 0.7, false),
        (1.0 / 23.0, 1.0 / 29.0, 0.9, 0.5, true),
        (-1.0 / 19.0, 1.0 / 17.0, 5.0, 0.4, true),
        (1.0 / 31.0, 1.0 / 43.0, 3.3, 0.5, true),
        (-1.0 / 41.0, -1.0 / 27.0, 1.1, 0.4, true),
    ];

    /// A smooth textured scene: sinusoids of 17 to 131 px wavelength. With
    /// `detail` off only the long ones remain, a defocused view of it.
    fn scene(x: f64, y: f64, detail: bool) -> f64 {
        RIPPLES
            .iter()
            .filter(|w| detail || !w.4)
            .map(|&(fx, fy, phase, amp, _)| {
                amp * (std::f64::consts::TAU * (fx * x + fy * y) + phase).sin()
            })
            .sum()
    }

    /// The scene seen at pixel `(x, y)` after translating it by `shift`;
    /// `blur_right` drops the fine detail right of the middle column.
    fn render(width: u32, height: u32, shift: (f64, f64), blur_right: bool) -> Vec<u16> {
        let mut out = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                let detail = !(blur_right && x > width / 2);
                let v = scene(f64::from(x) - shift.0, f64::from(y) - shift.1, detail);
                out.push((32768.0 + 6000.0 * v).clamp(0.0, 65535.0) as u16);
            }
        }
        out
    }

    fn reader(width: u32, height: u32, shifts: &[(f64, f64)], blur_last: bool) -> MemReader {
        let n = shifts.len();
        MemReader {
            width,
            frames: shifts
                .iter()
                .enumerate()
                .map(|(i, &shift)| render(width, height, shift, blur_last && i == n - 1))
                .collect(),
        }
    }

    fn test_params(max_iterations: u32) -> EccParams {
        EccParams {
            max_iterations,
            epsilon_px: 1e-4,
            min_valid_fraction: MIN_VALID_FRACTION,
        }
    }

    fn whole_plane(ctx: &AlignContext<'_>, frame: usize) -> Plane {
        let stack = ctx.stack();
        let all = Rect {
            x: 0,
            y: 0,
            width: stack.width,
            height: stack.height,
        };
        read_luma_window(ctx, frame, all).unwrap()
    }

    const W: u32 = 1031;
    const H: u32 = 777;

    /// `plane` smoothed whole, the reference the windowed reads must match.
    fn smoothed(plane: &Plane) -> Plane {
        let mut copy =
            Plane::from_f32_vec(plane.data().to_vec(), plane.width(), plane.height(), 1).unwrap();
        smooth_level0(&mut copy);
        copy
    }

    #[test]
    fn a_smoothed_level0_read_equals_the_whole_plane_smoothing() {
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, 2.0)], false);
        let ctx = AlignContext::new(StackContext::new(2, W, H, 1), 0, &mem);
        let whole = smoothed(&whole_plane(&ctx, 1));
        let inside = Rect {
            x: 200,
            y: 120,
            width: 300,
            height: 200,
        };
        let at_the_corner = Rect {
            x: W - 90,
            y: H - 60,
            width: 90,
            height: 60,
        };
        for rect in [inside, at_the_corner] {
            let read = read_level0_luma(&ctx, 1, rect, true).unwrap();
            for row in 0..rect.height {
                let from = ((rect.y + row) * W + rect.x) as usize;
                assert_eq!(
                    &read.data()[(row * rect.width) as usize..][..rect.width as usize],
                    &whole.data()[from..from + rect.width as usize],
                    "{rect:?} row {row}"
                );
            }
        }
    }

    #[test]
    fn a_solve_region_within_the_smoothing_reach_of_an_edge_reads_level_zero_unsmoothed() {
        // 40 px: the region starts 5 px in, short of the 7 px the smoothing
        // and the template's extra pixel need.
        let region = PyramidalEcc::solve_region(40, 40, 0);
        assert_eq!(region.x, 5);
        assert!(!smooths_level0(region, (40, 40)));
        assert!(smooths_level0(PyramidalEcc::solve_region(W, H, 0), (W, H)));

        let mem = reader(40, 40, &[(0.0, 0.0), (1.0, 0.0)], false);
        let ctx = AlignContext::new(StackContext::new(2, 40, 40, 1), 0, &mem);
        let rect = grown_clamped(region, 1, (40, 40));
        let read = read_level0_luma(&ctx, 1, rect, smooths_level0(region, (40, 40))).unwrap();
        assert_eq!(read, read_luma_window(&ctx, 1, rect).unwrap());
    }

    #[test]
    fn a_windowed_level0_solve_equals_the_whole_plane_solve() {
        // Frame 1 is the reference moved by (3, 2), frame 2 by (80, 3).
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, 2.0), (80.0, 3.0)], false);
        let ctx = AlignContext::new(StackContext::new(3, W, H, 1), 0, &mem);
        let reference = smoothed(&whole_plane(&ctx, 0));
        let region = PyramidalEcc::solve_region(W, H, 0);
        let params = test_params(6);

        for mode in [EccMode::Translation, EccMode::Similarity] {
            let template = EccTemplate::prepare(&reference, (0, 0), region, mode).unwrap();
            for (frame, seed) in [
                (1usize, Transform::translation(2.0, 1.0)),
                (2, Transform::translation(78.0, 2.0)),
            ] {
                let moving = smoothed(&whole_plane(&ctx, frame));
                let whole = solve_prepared(&template, &moving, (0, 0), &seed, &params, None);

                let window = level0_window(
                    footprint(&seed, region, (W, H)),
                    region,
                    LEVEL0_WINDOW_MARGIN_PX,
                    (W, H),
                );
                if frame == 1 {
                    assert!(window.x > 0 && window.y > 0, "{window:?} touches the frame");
                    assert!(window.x + window.width < W && window.y + window.height < H);
                } else {
                    assert_eq!(window.x + window.width, W, "{window:?} clamps right");
                }

                let solved = solve_level0(
                    &ctx,
                    frame,
                    &template,
                    &seed,
                    &params,
                    LEVEL0_WINDOW_MARGIN_PX,
                    None,
                )
                .unwrap();
                assert_eq!(solved.result, whole, "{mode:?} frame {frame}");
                assert_eq!(solved.misses, 0);
            }
        }
    }

    #[test]
    fn a_seed_outside_the_frame_reads_as_too_few_valid_samples() {
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, 2.0)], false);
        let ctx = AlignContext::new(StackContext::new(2, W, H, 1), 0, &mem);
        let reference = smoothed(&whole_plane(&ctx, 0));
        let region = PyramidalEcc::solve_region(W, H, 0);
        let template =
            EccTemplate::prepare(&reference, (0, 0), region, EccMode::Translation).unwrap();
        let seed = Transform::translation(5000.0, 0.0);
        let params = test_params(6);

        let whole = solve_prepared(
            &template,
            &smoothed(&whole_plane(&ctx, 1)),
            (0, 0),
            &seed,
            &params,
            None,
        );
        let solved = solve_level0(&ctx, 1, &template, &seed, &params, 64, None).unwrap();
        assert_eq!(
            whole.status,
            EccStatus::Diverged(DivergenceReason::TooFewValidSamples)
        );
        assert_eq!(solved.result, whole);
        assert_eq!(solved.misses, 0);
    }

    #[test]
    fn an_iterate_leaving_the_window_is_counted_and_regrown() {
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, 2.0)], false);
        let ctx = AlignContext::new(StackContext::new(2, W, H, 1), 0, &mem);
        let reference = smoothed(&whole_plane(&ctx, 0));
        let region = PyramidalEcc::solve_region(W, H, 0);
        let template =
            EccTemplate::prepare(&reference, (0, 0), region, EccMode::Translation).unwrap();
        let params = test_params(8);
        let whole = solve_prepared(
            &template,
            &smoothed(&whole_plane(&ctx, 1)),
            (0, 0),
            &Transform::identity(),
            &params,
            None,
        );

        // A one-pixel margin cannot hold a 3 px drift: counted once, then
        // the regrown window reproduces the whole-plane result.
        let solved =
            solve_level0(&ctx, 1, &template, &Transform::identity(), &params, 1, None).unwrap();
        assert_eq!(solved.misses, 1);
        assert_eq!(solved.result, whole);
    }

    /// The off-centre check at `fit`, against frame 1 of `mem`.
    fn check(mem: &MemReader, fit: &Transform) -> Validation {
        let ctx = AlignContext::new(StackContext::new(mem.frames.len(), W, H, 1), 0, mem);
        let reference = PreparedReference::build(&ctx, 0, EccMode::Translation, 0).unwrap();
        assert_eq!(reference.patches.len(), 8);
        reference.validate(&ctx, 1, fit, &test_params(400)).unwrap()
    }

    fn scale_about_centre(scale: f64) -> Transform {
        let (cx, cy) = (f64::from(W - 1) / 2.0, f64::from(H - 1) / 2.0);
        Transform::translation(cx, cy)
            .compose(&Transform::similarity(scale, 0.0, 0.0, 0.0))
            .compose(&Transform::translation(-cx, -cy))
    }

    #[test]
    fn the_true_transform_is_not_flagged_even_with_one_half_defocused() {
        let truth = Transform::translation(3.0, -2.0);
        for blur in [false, true] {
            let mem = reader(W, H, &[(0.0, 0.0), (3.0, -2.0)], blur);
            let v = check(&mem, &truth);
            assert!(v.usable >= MIN_USABLE_PATCHES, "blur {blur}: {v:?}");
            assert!(!v.flagged(), "blur {blur}: {v:?}");
        }
        let sharp = check(&reader(W, H, &[(0.0, 0.0), (3.0, -2.0)], false), &truth);
        assert_eq!(
            sharp,
            Validation {
                usable: 8,
                failed: 0
            }
        );
    }

    #[test]
    fn a_two_pixel_offset_and_a_scale_error_are_flagged() {
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, -2.0)], false);
        let offset = check(&mem, &Transform::translation(5.0, -2.0));
        assert!(offset.flagged(), "+2 px: {offset:?}");

        let scaled = check(
            &mem,
            &Transform::translation(3.0, -2.0).compose(&scale_about_centre(1.01)),
        );
        assert!(scaled.flagged(), "x1.01: {scaled:?}");
    }

    #[test]
    fn patches_beyond_the_frame_after_a_large_shift_are_unusable_not_failed() {
        let mem = reader(W, H, &[(0.0, 0.0), (200.0, 0.0)], false);
        let v = check(&mem, &Transform::translation(200.0, 0.0));
        assert_eq!(
            v,
            Validation {
                usable: 5,
                failed: 0
            },
            "{v:?}"
        );
        assert!(!v.flagged());
    }

    #[test]
    fn the_flag_needs_enough_usable_patches_and_a_failing_majority() {
        assert!(
            !Validation {
                usable: 2,
                failed: 2
            }
            .flagged()
        );
        assert!(
            !Validation {
                usable: 8,
                failed: 4
            }
            .flagged()
        );
        assert!(
            Validation {
                usable: 8,
                failed: 5
            }
            .flagged()
        );
        assert!(
            Validation {
                usable: 3,
                failed: 2
            }
            .flagged()
        );
    }

    #[test]
    fn a_driver_prepared_reference_gives_the_same_outcome_as_none() {
        // Depth 1 on a frame deep enough to have a level 1; the reference's
        // level-0 template comes from a window, never the whole plane.
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, -2.0)], false);
        let stack = StackContext::new(2, W, H, 1);
        let ctx = AlignContext::new(stack, 0, &mem);
        let ctx_prepared_later = AlignContext::new(stack, 0, &mem);
        let mut cfg = crate::params::resolved_values(
            &crate::params::resolve(PyramidalEcc.params(), &ParamValues::new(), &stack).unwrap(),
        );
        cfg.insert("pyramid-depth".to_string(), ParamValue::Int(1));
        cfg.insert("max-iterations".to_string(), ParamValue::Int(20));
        cfg.insert(
            "mode".to_string(),
            ParamValue::Enum("translation".to_string()),
        );
        assert!(max_levels(W, H) >= 1);

        let prepared = PyramidalEcc.prepare_run(&ctx, &cfg).unwrap().unwrap();
        let with = PyramidalEcc
            .align_frame(&ctx_prepared_later.with_prepared(Some(&*prepared)), 1, &cfg)
            .unwrap();
        let without = PyramidalEcc.align_frame(&ctx, 1, &cfg).unwrap();
        assert_eq!(with, without);
        assert!(matches!(
            with.status,
            AlignStatus::Converged | AlignStatus::HitCap | AlignStatus::Stalled
        ));
    }

    #[test]
    fn solve_region_respects_the_sample_budget() {
        let pixels = |r: Rect| u64::from(r.width) * u64::from(r.height);
        let level0 = PyramidalEcc::solve_region(8192, 5464, 0);
        assert!(
            pixels(level0) <= LEVEL0_REGION_PIXELS as u64,
            "region {level0:?} is over the {LEVEL0_REGION_PIXELS} budget"
        );
        // Shrinking the naive 6144x4098 (75%) rect by
        // sqrt(4 * 2^20 / (6144*4098)) and flooring yields exactly this.
        assert_eq!((level0.width, level0.height), (2507, 1672));

        let level1 = PyramidalEcc::solve_region(4096, 2732, 1);
        assert!(
            pixels(level1) <= MAX_REGION_PIXELS as u64,
            "region {level1:?} is over the {MAX_REGION_PIXELS} budget"
        );
        assert_eq!((level1.width, level1.height), (1253, 836));

        // An uncapped level-0 rect is the footprint level 1 solves over.
        let level0 = PyramidalEcc::solve_region(2400, 1320, 0);
        let level1 = PyramidalEcc::solve_region(1200, 660, 1);
        assert_eq!((level0.width, level0.height), (1800, 990));
        assert_eq!((level1.width * 2, level1.height * 2), (1800, 990));
    }

    /// Frames of the blob scene, each moved by its shift: feature-rich, unlike
    /// the smooth sinusoid scene, whose near-periodic detail matches wrongly.
    fn blob_reader(shifts: &[(f64, f64)]) -> MemReader {
        MemReader {
            width: W,
            frames: shifts
                .iter()
                .map(|&(sx, sy)| {
                    blob_scene(W, H, &Transform::translation(sx, sy), 1.0)
                        .iter()
                        .map(|&v| v.clamp(0.0, 65535.0) as u16)
                        .collect()
                })
                .collect(),
        }
    }

    /// Frame `frame` of `mem` registered at `depth` in `mode` against frame
    /// 0: the reference, the outcome and what the escalation did.
    fn registered(
        mem: &MemReader,
        mode: EccMode,
        depth: u32,
        frame: usize,
    ) -> (PreparedReference, Attempt, Escalation) {
        let ctx = AlignContext::new(StackContext::new(mem.frames.len(), W, H, 1), 0, mem);
        let reference = PreparedReference::build(&ctx, 0, mode, depth).unwrap();
        let (attempt, escalation) =
            PyramidalEcc::register(&ctx, frame, &reference, depth, &test_params(400), false)
                .unwrap();
        (reference, attempt, escalation)
    }

    /// One attempt on frame `frame` from `start`, a transform in the coarsest
    /// level's pixels, with no escalation.
    fn attempt_from(
        mem: &MemReader,
        mode: EccMode,
        depth: u32,
        frame: usize,
        start: Transform,
    ) -> Attempt {
        let ctx = AlignContext::new(StackContext::new(mem.frames.len(), W, H, 1), 0, mem);
        let reference = PreparedReference::build(&ctx, 0, mode, depth).unwrap();
        let pyramid = PyramidalEcc::build_pyramid(&ctx, frame, depth).unwrap();
        PyramidalEcc::solve_from(
            &ctx,
            frame,
            &reference,
            &pyramid,
            depth,
            &test_params(400),
            start,
            true,
        )
        .unwrap()
    }

    /// The identity-start attempt alone: what a frame gets with no escalation.
    fn unseeded(mem: &MemReader, mode: EccMode, depth: u32, frame: usize) -> Attempt {
        attempt_from(mem, mode, depth, frame, Transform::identity())
    }

    #[test]
    fn a_fit_that_settles_in_a_wrong_basin_below_the_floor_is_flagged_with_its_fit_kept() {
        // Frame 1 equals the reference; the start is 15 degrees and 8.5 % off
        // about the coarsest level's centre, inside a basin the solver settles
        // in with a correlation near 0.1.
        let mem = blob_reader(&[(0.0, 0.0), (0.0, 0.0)]);
        let (cx, cy) = (f64::from(W / 4 - 1) / 2.0, f64::from(H / 4 - 1) / 2.0);
        let start = Transform::translation(cx, cy)
            .compose(&Transform::similarity(
                1.085,
                (-15.0f64).to_radians(),
                0.0,
                0.0,
            ))
            .compose(&Transform::translation(-cx, -cy));

        let attempt = attempt_from(&mem, EccMode::Similarity, 2, 1, start);

        assert!(attempt.residual < MIN_FINAL_CORRELATION, "{attempt:?}");
        assert_eq!(
            attempt.status,
            AlignStatus::Diverged(DivergenceReason::WeakMatch),
            "{attempt:?}"
        );
        let (scale, radians, _, _) = attempt.transform.similarity_parts();
        assert!(
            (radians.to_degrees() + 15.0).abs() < 3.0 && scale > 1.05,
            "the wrong fit is kept, not the identity: scale {scale}, {} deg",
            radians.to_degrees()
        );
    }

    /// Uniform-sum noise of unit variance, a pure function of the pixel and a
    /// per-frame salt.
    fn noise(x: u32, y: u32, salt: u64) -> f64 {
        let mut h = (u64::from(x) << 32 | u64::from(y)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ salt.wrapping_mul(0xD1B5_4A32_D192_ED03);
        let mut sum = 0.0;
        for _ in 0..4 {
            h ^= h >> 29;
            h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            h ^= h >> 32;
            sum += (h >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
        }
        sum * 3.0f64.sqrt()
    }

    /// Two frames of the defocused scene (long ripples only) at `amplitude`
    /// times the usual contrast, the second moved by `shift`, each with its
    /// own sensor noise of standard deviation `noise_sd` grey levels.
    fn defocused_noisy_reader(shift: (f64, f64), amplitude: f64, noise_sd: f64) -> MemReader {
        let frame = |shift: (f64, f64), salt: u64| -> Vec<u16> {
            let mut out = Vec::with_capacity(W as usize * H as usize);
            for y in 0..H {
                for x in 0..W {
                    let v = scene(f64::from(x) - shift.0, f64::from(y) - shift.1, false);
                    let grey = 32768.0 + 6000.0 * amplitude * v + noise_sd * noise(x, y, salt);
                    out.push(grey.clamp(0.0, 65535.0) as u16);
                }
            }
            out
        };
        MemReader {
            width: W,
            frames: vec![frame((0.0, 0.0), 1), frame(shift, 2)],
        }
    }

    /// Pixel-scale ripples the defocused scene adds to [`RIPPLES`]: the detail
    /// a mismatched blur biases the finest level with.
    const FINE_RIPPLES: [(f64, f64, f64, f64, bool); 3] = [
        (1.0 / 7.0, 1.0 / 9.0, 0.6, 1.5, true),
        (-1.0 / 6.0, 1.0 / 11.0, 2.2, 1.5, true),
        (1.0 / 5.5, 1.0 / 5.0, 4.0, 1.2, true),
    ];
    /// The scene as a lens focused at depth `focus` renders it at `(x, y)`:
    /// the depth tilts across the frame and every ripple is attenuated by the
    /// Gaussian of sigma `DEFOCUS_PX_PER_DEPTH * |depth - focus|`.
    fn defocused_scene(x: f64, y: f64, focus: f64) -> f64 {
        const DEFOCUS_PX_PER_DEPTH: f64 = 2.0;
        let depth = 4.0 * x / f64::from(W) + 1.0 * y / f64::from(H);
        let sigma = DEFOCUS_PX_PER_DEPTH * (depth - focus).abs();
        RIPPLES
            .iter()
            .chain(FINE_RIPPLES.iter())
            .map(|&(fx, fy, phase, amp, _)| {
                let tau = std::f64::consts::TAU;
                let attenuation = (-0.5 * tau * tau * sigma * sigma * (fx * fx + fy * fy)).exp();
                amp * attenuation * (tau * (fx * x + fy * y) + phase).sin()
            })
            .sum()
    }

    /// The reference (focused at depth 1) and a frame focused at depth 3
    /// that sees the scene through `truth`.
    fn depth_defocused_reader(truth: &Transform) -> MemReader {
        let back = truth.inverse().expect("the truth is invertible");
        let frame = |focus: f64, to_scene: Option<&Transform>| -> Vec<u16> {
            let mut out = Vec::with_capacity(W as usize * H as usize);
            for y in 0..H {
                for x in 0..W {
                    let (qx, qy) = match to_scene {
                        Some(t) => t
                            .map_point(f64::from(x), f64::from(y))
                            .expect("an affine map"),
                        None => (f64::from(x), f64::from(y)),
                    };
                    let v = defocused_scene(qx, qy, focus);
                    out.push((32768.0 + 6000.0 * v).clamp(0.0, 65535.0) as u16);
                }
            }
            out
        };
        MemReader {
            width: W,
            frames: vec![frame(1.0, None), frame(3.0, Some(&back))],
        }
    }

    /// The largest distance between where `fit` and `truth` send the corners
    /// of the level-0 solve region.
    fn corner_error_px(fit: &Transform, truth: &Transform) -> f64 {
        let region = PyramidalEcc::solve_region(W, H, 0);
        let (x0, y0) = (f64::from(region.x), f64::from(region.y));
        let (x1, y1) = (x0 + f64::from(region.width), y0 + f64::from(region.height));
        [(x0, y0), (x1, y0), (x0, y1), (x1, y1)]
            .iter()
            .map(|&(x, y)| {
                let a = fit.map_point(x, y).expect("an affine map");
                let b = truth.map_point(x, y).expect("an affine map");
                (a.0 - b.0).hypot(a.1 - b.1)
            })
            .fold(0.0, f64::max)
    }

    #[test]
    fn level_zero_stays_within_budget_under_depth_varying_defocus() {
        let (cx, cy) = (f64::from(W - 1) / 2.0, f64::from(H - 1) / 2.0);
        let truth = Transform::translation(cx + 3.5, cy - 2.5)
            .compose(&Transform::similarity(1.002, 0.004, 0.0, 0.0))
            .compose(&Transform::translation(-cx, -cy));
        let mem = depth_defocused_reader(&truth);

        let attempt = unseeded(&mem, EccMode::Similarity, 3, 1);

        let error = corner_error_px(&attempt.transform, &truth);
        assert!(error < 0.03, "{error} px; {attempt:?}");
    }

    #[test]
    fn a_correlation_decrease_at_a_middle_level_hands_its_fit_to_the_next_level() {
        // Levels 3 and 2 converge; level 1 then diverges after a few
        // iterations on the noise, with its best-correlation fit kept.
        let mem = defocused_noisy_reader((9.0, 6.0), 0.01, 250.0);
        let attempt = unseeded(&mem, EccMode::Similarity, 3, 1);

        let solved: Vec<_> = attempt
            .records
            .iter()
            .map(|r| (r.level, r.result.status))
            .collect();
        assert_eq!(
            solved[..3],
            [
                (3, EccStatus::Converged),
                (2, EccStatus::Converged),
                (
                    1,
                    EccStatus::Diverged(DivergenceReason::CorrelationDecreased)
                ),
            ],
            "{solved:?}"
        );
        assert_eq!(
            solved.last().map(|&(level, _)| level),
            Some(0),
            "level 0 is solved from the level-1 fit: {solved:?}"
        );
        assert_ne!(attempt.status, AlignStatus::Converged, "{attempt:?}");
    }

    #[test]
    fn a_correlation_decrease_at_the_coarsest_level_stops_the_pass() {
        let mem = defocused_noisy_reader((9.0, 6.0), 0.004, 500.0);
        let attempt = unseeded(&mem, EccMode::Similarity, 3, 1);

        assert_eq!(
            attempt.status,
            AlignStatus::Diverged(DivergenceReason::CorrelationDecreased),
            "{attempt:?}"
        );
        let levels: Vec<u32> = attempt.records.iter().map(|r| r.level).collect();
        assert_eq!(
            levels,
            [3],
            "no finer level follows a diverged coarsest level"
        );
    }

    #[test]
    fn a_frame_reports_the_worst_status_over_its_levels() {
        let diverged = AlignStatus::Diverged(DivergenceReason::LemmaOneRegion);
        let ladder = [
            AlignStatus::Converged,
            AlignStatus::HitCap,
            AlignStatus::Stalled,
            diverged,
        ];
        for (i, &low) in ladder.iter().enumerate() {
            for &high in &ladder[i..] {
                assert_eq!(worse_status(low, high), high, "{low:?} then {high:?}");
                assert_eq!(worse_status(high, low), high, "{high:?} then {low:?}");
            }
        }
    }

    #[test]
    fn coarse_levels_stop_at_a_hundred_times_epsilon_and_level_zero_at_epsilon() {
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, -2.0)], false);
        let ctx = AlignContext::new(StackContext::new(2, W, H, 1), 0, &mem);
        let reference = PreparedReference::build(&ctx, 0, EccMode::Translation, 2).unwrap();
        let pyramid = PyramidalEcc::build_pyramid(&ctx, 1, 2).unwrap();
        let params = EccParams {
            epsilon_px: 1e-3,
            ..test_params(400)
        };
        let attempt = PyramidalEcc::solve_from(
            &ctx,
            1,
            &reference,
            &pyramid,
            2,
            &params,
            Transform::identity(),
            true,
        )
        .unwrap();
        let coarse_steps: Vec<f64> = attempt
            .records
            .iter()
            .filter(|r| r.level > 0)
            .map(|r| r.result.final_step.expect("a solved level has a step"))
            .collect();
        assert!(coarse_steps.iter().all(|&s| s < 0.1), "{coarse_steps:?}");
        assert!(coarse_steps.iter().any(|&s| s >= 1e-3), "{coarse_steps:?}");
        let level0 = attempt.records.last().expect("level 0 is solved");
        assert_eq!(level0.level, 0);
        assert!(level0.result.final_step.expect("a step") < 1e-3);
    }

    #[test]
    fn a_converging_frame_never_runs_the_detector() {
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, -2.0)], false);
        for mode in [EccMode::Translation, EccMode::Similarity] {
            let (reference, attempt, escalation) = registered(&mem, mode, 2, 1);
            assert!(matches!(
                attempt.status,
                AlignStatus::Converged | AlignStatus::HitCap | AlignStatus::Stalled
            ));
            assert_eq!(escalation, Escalation::default(), "{mode:?}");
            assert!(reference.features.get().is_none(), "{mode:?}");
        }
    }

    #[test]
    fn a_shift_past_the_identity_starts_reach_converges_seeded_in_both_modes() {
        let mem = blob_reader(&[(0.0, 0.0), (150.0, -90.0)]);
        for mode in [EccMode::Translation, EccMode::Similarity] {
            let first = unseeded(&mem, mode, 2, 1);
            assert!(
                matches!(first.status, AlignStatus::Diverged(_)),
                "{mode:?}: {first:?}"
            );

            let (reference, attempt, escalation) = registered(&mem, mode, 2, 1);
            assert_eq!(
                escalation,
                Escalation {
                    attempted: true,
                    rerun: true,
                    accepted: true
                },
                "{mode:?}"
            );
            assert!(reference.features.get().is_some());
            assert!(
                matches!(
                    attempt.status,
                    AlignStatus::Converged | AlignStatus::HitCap | AlignStatus::Stalled
                ),
                "{mode:?}: {attempt:?}"
            );
            let (scale, radians, tx, ty) = attempt.transform.similarity_parts();
            assert!(
                (tx - 150.0).abs() < 0.05 && (ty + 90.0).abs() < 0.05,
                "{mode:?}: ({tx}, {ty})"
            );
            if mode == EccMode::Translation {
                assert_eq!((scale, radians), (1.0, 0.0), "a pure translation");
            }
        }
    }

    #[test]
    fn a_frame_whose_features_cluster_converges_seeded() {
        // Texture only inside one rectangle of the reference (and the same
        // content, moved by the shift, in the frame): the matches fall in a
        // few grid cells but are nearly all right.
        let shift = (150.0, -90.0);
        let keep = |x0: u32, y0: u32| {
            move |i: usize| {
                let (x, y) = (i as u32 % W, i as u32 / W);
                (x0..x0 + 400).contains(&x) && (y0..y0 + 300).contains(&y)
            }
        };
        let mut mem = blob_reader(&[(0.0, 0.0), shift]);
        for (frame, (x0, y0)) in mem.frames.iter_mut().zip([(300, 200), (450, 110)]) {
            let inside = keep(x0, y0);
            for (i, v) in frame.iter_mut().enumerate() {
                if !inside(i) {
                    *v = 32768;
                }
            }
        }
        let depth = 3;

        let ctx = AlignContext::new(StackContext::new(2, W, H, 1), 0, &mem);
        let reference = PreparedReference::build(&ctx, 0, EccMode::Similarity, depth).unwrap();
        let frame_pyramid = PyramidalEcc::build_pyramid(&ctx, 1, depth).unwrap();
        let level = seed_level(W, H, depth).expect("a seed level");
        let frame_features = detect_level(&ctx, 1, &frame_pyramid, level)
            .unwrap()
            .expect("features");
        let reference_features = detect_level(&ctx, 0, &reference.pyramid, level)
            .unwrap()
            .expect("features");
        let dims = (W.div_ceil(1 << level), H.div_ceil(1 << level));
        let fit = fit_between(&reference_features, &frame_features, dims).expect("a fit");
        assert!(
            (2..6).contains(&fit.occupied_cells),
            "the matches must cluster: {fit:?}"
        );

        let first = unseeded(&mem, EccMode::Similarity, depth, 1);
        assert!(
            matches!(first.status, AlignStatus::Diverged(_)),
            "{first:?}"
        );
        let (_, attempt, escalation) = registered(&mem, EccMode::Similarity, depth, 1);
        assert_eq!(
            escalation,
            Escalation {
                attempted: true,
                rerun: true,
                accepted: true
            },
            "{fit:?}"
        );
        assert!(
            !matches!(attempt.status, AlignStatus::Diverged(_)),
            "{attempt:?}"
        );
        let (_, _, tx, ty) = attempt.transform.similarity_parts();
        assert!(
            (tx - shift.0).abs() < 0.05 && (ty - shift.1).abs() < 0.05,
            "({tx}, {ty})"
        );
    }

    #[test]
    fn a_quasi_periodic_scene_keeps_its_identity_start_outcome() {
        // The smooth sinusoid scene matches a half-turn of itself on a
        // fifth of the matches: not a seed.
        let mem = reader(W, H, &[(0.0, 0.0), (150.0, -90.0)], false);
        let (_, attempt, escalation) = registered(&mem, EccMode::Similarity, 2, 1);
        let first = unseeded(&mem, EccMode::Similarity, 2, 1);
        assert!(
            matches!(first.status, AlignStatus::Diverged(_)),
            "{first:?}"
        );
        assert_eq!(
            escalation,
            Escalation {
                attempted: true,
                rerun: false,
                accepted: false
            }
        );
        assert_eq!(attempt.transform, first.transform);
    }

    #[test]
    fn a_frame_with_no_features_keeps_its_identity_start_outcome() {
        let mut mem = reader(W, H, &[(0.0, 0.0), (0.0, 0.0)], false);
        mem.frames[1] = vec![30000; (W * H) as usize];
        let (_, attempt, escalation) = registered(&mem, EccMode::Similarity, 2, 1);
        let first = unseeded(&mem, EccMode::Similarity, 2, 1);
        assert!(
            matches!(first.status, AlignStatus::Diverged(_)),
            "{first:?}"
        );
        assert_eq!(
            escalation,
            Escalation {
                attempted: true,
                rerun: false,
                accepted: false
            }
        );
        assert_eq!(
            (
                attempt.transform,
                attempt.status,
                attempt.iterations,
                attempt.residual
            ),
            (
                first.transform,
                first.status,
                first.iterations,
                first.residual
            )
        );
    }

    /// Frames of the reference stack solved from their own feature seed with
    /// the default parameters must end within a pixel of the transform their
    /// identity-start solve ends at.
    #[test]
    #[ignore = "reads the 16 GB reference stack"]
    fn reference_stack_frames_solved_from_their_feature_seed_match_the_shipped_fit() {
        let Some(dir) = std::env::var_os("FOCUSFORGE_REFERENCE_STACK")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
        else {
            println!("FOCUSFORGE_REFERENCE_STACK does not name a directory; skipped");
            return;
        };
        let paths: Vec<_> = crate::io::frame_paths(&dir)
            .expect("the stack directory enumerates")
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("IMG_"))
            })
            .collect();
        let stack = crate::io::TiffStack::open_files(&paths).expect("the stack opens");
        let (width, height) = stack.dimensions();
        let stack_ctx = StackContext::new(
            stack.len(),
            width,
            height,
            stack.header(0).samples_per_pixel,
        );
        let algo = crate::registry::find_align("pyramidal-ecc").expect("registered");
        let resolved = crate::params::resolve(algo.params(), &ParamValues::new(), &stack_ctx)
            .expect("the defaults resolve");
        let cfg = crate::params::resolved_values(&resolved);
        let reference_frame = (stack.len() - 1) / 2;
        let ctx = AlignContext::new(stack_ctx, reference_frame, &stack);

        let mode = PyramidalEcc::mode(&cfg).unwrap();
        let depth = cfg
            .get("pyramid-depth")
            .and_then(ParamValue::as_i64)
            .unwrap() as u32;
        let params = EccParams {
            max_iterations: cfg
                .get("max-iterations")
                .and_then(ParamValue::as_i64)
                .unwrap() as u32,
            epsilon_px: cfg.get("epsilon-px").and_then(ParamValue::as_f64).unwrap(),
            min_valid_fraction: MIN_VALID_FRACTION,
        };
        let reference = PreparedReference::build(&ctx, reference_frame, mode, depth).unwrap();
        let level = seed_level(width, height, depth).expect("a seed level");
        let dims = (width.div_ceil(1 << level), height.div_ceil(1 << level));
        let (w, h) = (f64::from(width), f64::from(height));
        let spots = [[0.0, 0.0], [w, 0.0], [0.0, h], [w, h], [w / 2.0, h / 2.0]];
        let moved = |a: &Transform, b: &Transform| {
            spots
                .iter()
                .map(|p| {
                    let at = |t: &Transform| {
                        [
                            t.m[0][0] * p[0] + t.m[0][1] * p[1] + t.m[0][2],
                            t.m[1][0] * p[0] + t.m[1][1] * p[1] + t.m[1][2],
                        ]
                    };
                    let (x, y) = (at(a), at(b));
                    (x[0] - y[0]).hypot(x[1] - y[1])
                })
                .fold(0.0, f64::max)
        };

        for frame in [0usize, 15, 45, 60] {
            let shipped = PyramidalEcc.align_frame(&ctx, frame, &cfg).unwrap();
            let pyramid = PyramidalEcc::build_pyramid(&ctx, frame, depth).unwrap();
            let mut escalation = Escalation::default();
            let seed = PyramidalEcc::feature_seed(
                &ctx,
                frame,
                &reference,
                &pyramid,
                depth,
                &mut escalation,
            )
            .unwrap();
            let fit = {
                let r = reference.features.get().unwrap().as_ref().unwrap().as_ref();
                let f = detect_level(&ctx, frame, &pyramid, level).unwrap();
                r.zip(f.as_ref()).and_then(|(r, f)| fit_between(r, f, dims))
            };
            let Some(seed) = seed else {
                panic!("frame {frame}: no accepted seed, fit={fit:?}");
            };
            let started = std::time::Instant::now();
            let attempt = PyramidalEcc::solve_from(
                &ctx, frame, &reference, &pyramid, depth, &params, seed, false,
            )
            .unwrap();
            let off = moved(&attempt.transform, &shipped.transform);
            println!(
                "seeded frame={frame} level={level} fit={fit:?} shipped={:?} shipped_corr={:.4} \
                 seeded={:?} seeded_corr={:.4} floor={MIN_FINAL_CORRELATION} iterations={} \
                 moved_from_shipped_px={off:.3} secs={:.1}",
                shipped.status,
                shipped.residual,
                attempt.status,
                attempt.residual,
                attempt.iterations,
                started.elapsed().as_secs_f64(),
            );
            assert!(
                !matches!(attempt.status, AlignStatus::Diverged(_)) && off < 1.0,
                "frame {frame}: {:?}, {off} px from the shipped fit",
                attempt.status
            );
        }
    }

    #[test]
    fn the_seed_lift_round_trips_at_depths_one_to_four() {
        let up = Transform::similarity(2.0, 0.0, 0.0, 0.0);
        let down = Transform::similarity(0.5, 0.0, 0.0, 0.0);
        let seed = Transform::translation(310.0, -120.0)
            .compose(&Transform::similarity(1.03, 0.04, 0.0, 0.0));
        for depth in 1..=4u32 {
            for from in 0..=depth {
                let lifted = lift_seed(&seed, from, depth);
                let mut back = lifted;
                for _ in from..depth {
                    back = up.compose(&back).compose(&down);
                }
                for (a, b) in back.m.iter().flatten().zip(seed.m.iter().flatten()) {
                    assert!((a - b).abs() < 1e-9, "depth {depth} from {from}");
                }
            }
        }
        let lifted = lift_seed(&Transform::translation(80.0, -40.0), 1, 4);
        assert_eq!(lifted, Transform::translation(10.0, -5.0));
    }

    #[test]
    fn the_seed_level_is_the_finest_one_within_the_proxy_size() {
        assert_eq!(seed_level(512, 512, 3), Some(0));
        assert_eq!(seed_level(1031, 777, 3), Some(2));
        assert_eq!(seed_level(1031, 777, 1), None, "level 1 is 516 px");
        assert_eq!(seed_level(8192, 5464, 6), Some(4));
        assert_eq!(seed_level(8192, 5464, 3), None);
        assert_eq!(seed_level(2049, 1367, 5), Some(3));
    }

    #[test]
    fn a_driver_prepared_reference_gives_the_same_seeded_outcome_as_none() {
        let mem = blob_reader(&[(0.0, 0.0), (150.0, -90.0)]);
        let stack = StackContext::new(2, W, H, 1);
        let ctx = AlignContext::new(stack, 0, &mem);
        let mut cfg = crate::params::resolved_values(
            &crate::params::resolve(PyramidalEcc.params(), &ParamValues::new(), &stack).unwrap(),
        );
        cfg.insert("pyramid-depth".to_string(), ParamValue::Int(2));
        cfg.insert(
            "mode".to_string(),
            ParamValue::Enum("translation".to_string()),
        );

        let prepared = PyramidalEcc.prepare_run(&ctx, &cfg).unwrap().unwrap();
        let shared = prepared.downcast_ref::<PreparedReference>().unwrap();
        assert!(shared.features.get().is_none(), "preparing detects nothing");
        let with = PyramidalEcc
            .align_frame(
                &AlignContext::new(stack, 0, &mem).with_prepared(Some(&*prepared)),
                1,
                &cfg,
            )
            .unwrap();
        assert!(shared.features.get().is_some());
        let without = PyramidalEcc.align_frame(&ctx, 1, &cfg).unwrap();
        assert_eq!(with, without);
        assert!(matches!(
            with.status,
            AlignStatus::Converged | AlignStatus::HitCap | AlignStatus::Stalled
        ));
    }

    /// Records every region offered to it; declines each, or opens a session
    /// that sums the moments on the CPU.
    struct RecordingBackend {
        offered: std::sync::Mutex<Vec<Rect>>,
        accept: bool,
    }

    impl RecordingBackend {
        fn new(accept: bool) -> Self {
            Self {
                offered: std::sync::Mutex::new(Vec::new()),
                accept,
            }
        }

        fn offered(&self) -> Vec<Rect> {
            self.offered.lock().unwrap().clone()
        }
    }

    impl AlignBackend for RecordingBackend {
        fn open_ecc(
            &self,
            request: &EccSessionRequest<'_>,
        ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
            self.offered.lock().unwrap().push(request.region);
            Ok(self
                .accept
                .then(|| Box::new(CpuMomentSession::new(request)) as Box<dyn EccMomentSession>))
        }
    }

    #[test]
    fn ecc_level_routing_skips_small_regions() {
        const BIG_W: u32 = 1700;
        const BIG_H: u32 = 1100;
        let region0 = PyramidalEcc::solve_region(BIG_W, BIG_H, 0);
        let region1 = PyramidalEcc::solve_region(BIG_W / 2, BIG_H / 2, 1);
        let pixels = |r: Rect| u64::from(r.width) * u64::from(r.height);
        assert_eq!((region0.width, region0.height), (1275, 825));
        assert!(pixels(region0) >= ECC_BACKEND_MIN_REGION_PX);
        assert!(pixels(region1) < ECC_BACKEND_MIN_REGION_PX);

        let shift = (0.4, -0.3);
        let mem = reader(BIG_W, BIG_H, &[(0.0, 0.0), shift], false);
        let stack = StackContext::new(2, BIG_W, BIG_H, 1);
        let config = |policy: &str| {
            let mut cfg = crate::params::resolved_values(
                &crate::params::resolve(PyramidalEcc.params(), &ParamValues::new(), &stack)
                    .unwrap(),
            );
            cfg.insert("pyramid-depth".to_string(), ParamValue::Int(1));
            cfg.insert("max-iterations".to_string(), ParamValue::Int(8));
            cfg.insert(
                "mode".to_string(),
                ParamValue::Enum("translation".to_string()),
            );
            cfg.insert(
                "reference-policy".to_string(),
                ParamValue::Enum(policy.to_string()),
            );
            cfg
        };
        let run = |policy: &str, backend: Option<&dyn AlignBackend>| {
            align_stack_with(
                &PyramidalEcc,
                stack,
                &mem,
                &config(policy),
                FailurePolicy::Strict,
                &AlignDriverOptions {
                    align_backend: backend,
                    ..AlignDriverOptions::default()
                },
            )
            .unwrap()
            .alignment
        };

        let plain = run("anchor", None);
        let declining = RecordingBackend::new(false);
        assert_eq!(run("anchor", Some(&declining)), plain);
        assert_eq!(declining.offered(), vec![region0]);

        let accepting = RecordingBackend::new(true);
        let chained = run("chain", Some(&accepting));
        assert_eq!(accepting.offered(), vec![region0]);
        let m = chained.frames[1].transform.m;
        assert!((m[0][2] - shift.0).abs() < 0.05, "x {}", m[0][2]);
        assert!((m[1][2] - shift.1).abs() < 0.05, "y {}", m[1][2]);
    }

    /// Opens sessions whose first `moments` call reports a lost device.
    struct LosingBackend;

    struct LostSession;

    impl EccMomentSession for LostSession {
        fn moments(&mut self, _warp: &Transform) -> Result<[f64; ECC_MOMENTS], AlgorithmError> {
            Err(AlgorithmError::BackendLost {
                detail: "the device was lost".to_string(),
            })
        }
    }

    impl AlignBackend for LosingBackend {
        fn open_ecc(
            &self,
            _request: &EccSessionRequest<'_>,
        ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
            Ok(Some(Box::new(LostSession)))
        }
    }

    #[test]
    fn align_stage_reruns_on_cpu_after_a_backend_loss() {
        const BIG_W: u32 = 1700;
        const BIG_H: u32 = 1100;
        let shifts = [(0.0, 0.0), (0.6, -0.2), (-0.35, 0.45)];
        let mem = reader(BIG_W, BIG_H, &shifts, false);
        let stack = StackContext::new(3, BIG_W, BIG_H, 1);
        let mut cfg = crate::params::resolved_values(
            &crate::params::resolve(PyramidalEcc.params(), &ParamValues::new(), &stack).unwrap(),
        );
        cfg.insert("pyramid-depth".to_string(), ParamValue::Int(1));
        cfg.insert("max-iterations".to_string(), ParamValue::Int(8));
        cfg.insert(
            "mode".to_string(),
            ParamValue::Enum("translation".to_string()),
        );
        let run = |backend: Option<&dyn AlignBackend>, done: &Arc<AtomicUsize>| {
            align_stack_with(
                &PyramidalEcc,
                stack,
                &mem,
                &cfg,
                FailurePolicy::Strict,
                &AlignDriverOptions {
                    align_backend: backend,
                    frames_done: Some(Arc::clone(done)),
                    ..AlignDriverOptions::default()
                },
            )
            .unwrap()
        };

        let plain_done = Arc::new(AtomicUsize::new(0));
        let plain = run(None, &plain_done);
        assert!(!plain.backend_used);
        assert_eq!(plain.rerun_reason, None);

        let done = Arc::new(AtomicUsize::new(0));
        let rerun = run(Some(&LosingBackend), &done);
        assert_eq!(rerun.alignment, plain.alignment);
        assert!(!rerun.backend_used);
        let reason = rerun.rerun_reason.expect("the loss is reported");
        assert!(reason.contains("the device was lost"), "{reason}");
        assert_eq!(done.load(Ordering::SeqCst), stack.frame_count);

        let accepting = RecordingBackend::new(true);
        let accepted = run(Some(&accepting), &Arc::new(AtomicUsize::new(0)));
        assert!(accepted.backend_used);
        assert_eq!(accepted.rerun_reason, None);
    }
}
