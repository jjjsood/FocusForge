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
//! `epsilon-px` is passed unscaled at every level -- it is a step measure
//! in that level's own pixels, so the same threshold means "this level's
//! own milli-pixel" everywhere, and a finer level always refines what a
//! coarser one left. A [`crate::align::EccStatus::Diverged`] status at any
//! level ends the loop there (the seed for finer levels would be
//! worthless), but the best-ρ transform that level found is still rescaled
//! all the way to level-0 coordinates, so a caller always gets a level-0
//! answer.
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
//! # Level-0 windows
//!
//! Level 0 is never resident. The reference template is prepared from the
//! region grown by one pixel; each frame's level-0 solve reads only the
//! window `Transform::source_footprint` names for the seed, grown by
//! [`LEVEL0_WINDOW_MARGIN_PX`] and held within [`LEVEL0_WINDOW_LIMIT_PX`] of
//! the region. Coordinates stay full-frame, so a solve whose samples all fall
//! inside the window is bit-identical to one over the whole plane. A result
//! whose footprint leaves the window counts one
//! [`TimingCounter::Level0WindowMisses`] and is solved again over a window
//! regrown to cover seed and result (counted again if still capped).
//!
//! # The off-centre check
//!
//! After a level-0 `Converged` or `HitCap` solve, `PyramidalEcc::validate`
//! evaluates the fit at up to eight off-centre patches; a frame where most
//! usable ones exceed [`VALIDATION_TOLERANCE_PX`] is reported
//! [`crate::align::DivergenceReason::ValidationFailed`], fit kept.

use std::sync::{Arc, LazyLock};

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus};
use crate::align::ecc::{EccTemplate, solve_prepared};
use crate::align::pyramid::StreamError;
use crate::align::{
    AlignError, AlignPyramid, DivergenceReason, EccIteration, EccMode, EccParams, EccResult,
    EccStatus, KERNEL_RADIUS, LumaAssembler, SourceFootprint, Transform,
};
use crate::fuse::pyramid::Plane;
use crate::params::{EnumVariant, ParamDesc, ParamKind, ParamValue, ParamValues, StackContext};
use crate::registry::AlignRegistration;
use crate::tile::plan::{DEFAULT_TILE_SIZE, MAX_LEVELS};
use crate::tile::{Rect, Tile};
use crate::timing::{TimingCounter, measurement_scope, record_level_iterations, record_timing};

/// Pixels the level-0 window extends past the seed's source footprint. Absorbs
/// the iterates' drift from the seed; a result that drifts further regrows the
/// window once.
const LEVEL0_WINDOW_MARGIN_PX: u32 = 64;

/// The farthest the level-0 window extends past the solve region on any side,
/// however far the seed or a result wanders. Bounds the window's memory, which
/// the accounting reserves.
const LEVEL0_WINDOW_LIMIT_PX: u32 = 256;

/// The largest per-patch residual, in level-0 pixels, a fit may leave at an
/// off-centre patch and still count as holding there.
const VALIDATION_TOLERANCE_PX: f64 = 1.0;

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

/// The fraction of each level axis [`PyramidalEcc::solve_region`] covers,
/// before the [`MAX_REGION_PIXELS`] budget shrink.
const REGION_FRACTION: f64 = 0.75;

/// The sample budget [`PyramidalEcc::solve_region`] never exceeds: at
/// 8192x5464 this turns a naive 6144x4098 (75%) rect into ~1253x836, a 43x
/// reduction in per-iteration taps.
const MAX_REGION_PIXELS: u32 = 1 << 20;

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
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "max-iterations".to_string(),
            label: "Max Iterations".to_string(),
            help: "The per-level iteration cap. A level that reaches it stops \
                   short of `epsilon-px` and reports `HitCap`, keeping the best \
                   transform it found rather than failing. Frames that converge \
                   earlier stop on `epsilon-px` and pay nothing. On a 61-frame \
                   44.8 MP stack a cap of 50 left 23 frames short (0.048-0.521 \
                   px residual); 400 converges all 61, the slowest at 332 \
                   iterations, for +31 s on a 182 s alignment pass."
                .to_string(),
            kind: ParamKind::Int { min: 1, max: 1000 },
            default: ParamValue::Int(400),
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "epsilon-px".to_string(),
            label: "Epsilon (px)".to_string(),
            help: "Maximum corner displacement in level pixels -- the \
                   per-iteration step measure the solver converges to, not \
                   a correlation threshold."
                .to_string(),
            kind: ParamKind::Float {
                min: 1e-9,
                max: 1.0,
                step: 1e-5,
                logarithmic: true,
            },
            default: ParamValue::Float(1e-4),
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "pyramid-depth".to_string(),
            label: "Pyramid Depth".to_string(),
            help: "How many coarser pyramid levels to register through \
                   before refining at full resolution. `auto` derives a \
                   depth from the frame's own resolution, bounded by a \
                   floor below which a level is too small to solve \
                   meaningfully."
                .to_string(),
            kind: ParamKind::Int {
                min: 0,
                max: MAX_LEVELS as i64,
            },
            default: ParamValue::Int(4),
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
            default: ParamValue::Enum("anchor".to_string()),
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

/// Bytes of reference templates a run retains, over every level the
/// pyramid can have (`ceil(dim / 2^k)`, `k` up to [`MAX_LEVELS`]): per
/// solve-region pixel a value (4 B), a four-column Jacobian (32 B) and the
/// full-overlap term (8 B), the rotation-scale mode being the larger one --
/// plus the off-centre patch templates.
pub(crate) fn retained_template_bytes(width: u32, height: u32) -> u64 {
    let levels: u64 = (0..=MAX_LEVELS)
        .map(|k| {
            let region =
                PyramidalEcc::solve_region(width.div_ceil(1 << k), height.div_ceil(1 << k));
            region.width as u64 * region.height as u64 * (4 + 8 * 4 + 8)
        })
        .sum();
    let patches: u64 = patch_grid(width, height)
        .iter()
        .map(|p| p.width as u64 * p.height as u64 * PATCH_TEMPLATE_BYTES_PER_PX)
        .sum();
    levels + patches
}

/// Bytes one solve allocates besides the templates: three 8-byte-per-pixel
/// vectors over the level-0 solve region, plus the largest level-0 frame
/// window (the region grown by [`LEVEL0_WINDOW_LIMIT_PX`] per side, clamped
/// to the frame: `f32` luma plus the `u16` samples it was read from) and the
/// largest off-centre patch window, each at `4 + 2 * spp` bytes per pixel.
pub(crate) fn solve_scratch_bytes(width: u32, height: u32, spp: u32) -> u64 {
    let region = PyramidalEcc::solve_region(width, height);
    let per_px = 4 + 2 * spp as u64;
    let window_w = (region.width + 2 * LEVEL0_WINDOW_LIMIT_PX).min(width) as u64;
    let window_h = (region.height + 2 * LEVEL0_WINDOW_LIMIT_PX).min(height) as u64;
    let patch_extent = patch_window_extent(patch_side(width, height));
    let patch_w = patch_extent.min(width) as u64;
    let patch_h = patch_extent.min(height) as u64;
    region.width as u64 * region.height as u64 * 24
        + window_w * window_h * per_px
        + patch_w * patch_h * per_px
}

/// The Pyramidal ECC alignment algorithm. See the module docs for the
/// coarse-to-fine loop and the solve region.
pub struct PyramidalEcc;

impl PyramidalEcc {
    /// The region [`solve_level`](crate::align::solve_level) solves over at
    /// one `width x height` pyramid level: a centred rect covering
    /// [`REGION_FRACTION`] of each axis, shrunk (aspect ratio preserved,
    /// about its own centre) so its pixel count never exceeds
    /// [`MAX_REGION_PIXELS`]. On a 512px level:
    /// `Rect { x: 64, y: 64, width: 384, height: 384 }`.
    fn solve_region(width: u32, height: u32) -> Rect {
        let raw_w = ((width as f64) * REGION_FRACTION).round().max(1.0) as u32;
        let raw_h = ((height as f64) * REGION_FRACTION).round().max(1.0) as u32;

        let count = raw_w as u64 * raw_h as u64;
        let (shrunk_w, shrunk_h) = if count > MAX_REGION_PIXELS as u64 {
            let factor = (MAX_REGION_PIXELS as f64 / count as f64).sqrt();
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
        AlignPyramid::stream(
            (stack.width, stack.height, stack.samples_per_pixel),
            depth,
            DEFAULT_TILE_SIZE / 2,
            |rect, out| ctx.read_tile_into(frame, rect, out),
        )
        .map_err(|e| match e {
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
        mut levels: Option<&mut Vec<EccLevelRecord>>,
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
        let frame_pyramid = Self::build_pyramid(ctx, frame, depth)?;

        let up = Transform::similarity(2.0, 0.0, 0.0, 0.0);
        let down = Transform::similarity(0.5, 0.0, 0.0, 0.0);

        let mut seed = Transform::identity();
        let mut total_iterations = 0u32;
        let mut final_transform = Transform::identity();
        let mut final_status = AlignStatus::Converged;
        let mut final_residual = 1.0f64;
        let mut window_misses = 0u32;

        let _scope = measurement_scope("ecc_solve");
        for k in (0..=depth).rev() {
            let template = reference.templates[k as usize]
                .as_ref()
                .map_err(wrap_error)?;

            let mut trace = levels.is_some().then(Vec::new);
            let result = if k == 0 {
                let solved = solve_level0(
                    ctx,
                    frame,
                    template,
                    &seed,
                    &params,
                    LEVEL0_WINDOW_MARGIN_PX,
                    trace.as_mut(),
                )?;
                window_misses += solved.misses;
                solved.result
            } else {
                solve_prepared(
                    template,
                    frame_pyramid.level(k as usize),
                    (0, 0),
                    &seed,
                    &params,
                    trace.as_mut(),
                )
            };
            if let Some(recs) = levels.as_deref_mut() {
                recs.push(EccLevelRecord {
                    level: k,
                    result,
                    trace: trace.unwrap_or_default(),
                });
            }
            record_level_iterations(k, result.iterations);
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
            final_status = result.status.into();

            match result.status {
                EccStatus::Diverged(_) => break,
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

        if matches!(final_status, AlignStatus::Converged | AlignStatus::HitCap) {
            let _scope = measurement_scope("ecc_validate");
            let check = reference.validate(ctx, frame, &final_transform, &params)?;
            if check.flagged() {
                final_status = AlignStatus::Diverged(DivergenceReason::ValidationFailed);
            }
        }

        Ok(AlignOutcome {
            transform: final_transform,
            status: final_status,
            iterations: total_iterations,
            residual: final_residual,
        })
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

/// Reads `rect` of `frame` and converts it to a luma plane.
fn read_luma_window(
    ctx: &AlignContext<'_>,
    frame: usize,
    rect: Rect,
) -> Result<Plane, AlgorithmError> {
    let spp = ctx.stack().samples_per_pixel;
    let mut tile = Tile {
        rect: Rect {
            x: 0,
            y: 0,
            width: rect.width,
            height: rect.height,
        },
        samples_per_pixel: spp,
        samples: vec![0u16; rect.width as usize * rect.height as usize * spp as usize],
    };
    record_timing(
        TimingCounter::AllocationBytes,
        (tile.samples.capacity() * std::mem::size_of::<u16>()) as u64,
    );
    record_timing(TimingCounter::AllocationOperations, 1);
    ctx.read_tile_into(frame, rect, &mut tile.samples)?;
    let mut assembler = LumaAssembler::new(rect.width, rect.height);
    assembler.push_tile(&tile).map_err(wrap_error)?;
    assembler.finish().map_err(wrap_error)
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

    let seed_fp = footprint(seed, region, dims);
    let window = level0_window(seed_fp, region, margin, dims);
    let plane = read_luma_window(ctx, frame, window)?;
    let mut first_trace = trace.is_some().then(Vec::new);
    let result = solve_prepared(
        template,
        &plane,
        (window.x, window.y),
        seed,
        params,
        first_trace.as_mut(),
    );
    drop(plane);

    let mut misses = 0;
    let result_fp = footprint(&result.transform, region, dims);
    if let Some(fp) = result_fp.filter(|fp| !contains(window, *fp)) {
        misses += 1;
        let covered = seed_fp.map_or(fp, |s| bounding_union(s, fp));
        let regrown = level0_window(Some(covered), region, margin, dims);
        if regrown != window {
            let plane = read_luma_window(ctx, frame, regrown)?;
            let mut second_trace = trace.is_some().then(Vec::new);
            let result = solve_prepared(
                template,
                &plane,
                (regrown.x, regrown.y),
                seed,
                params,
                second_trace.as_mut(),
            );
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
        let region = PyramidalEcc::solve_region(dims.0, dims.1);
        let window = grown_clamped(region, 1, dims);
        let plane = read_luma_window(ctx, reference, window)?;
        templates.push(EccTemplate::prepare(
            &plane,
            (window.x, window.y),
            region,
            mode,
        ));
        for k in 1..=depth as usize {
            let level = pyramid.level(k);
            let region = PyramidalEcc::solve_region(level.width(), level.height());
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
                EccStatus::Converged | EccStatus::HitCap => {
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
    use crate::align::pyramid::max_levels;

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
            ParamValue::Enum("anchor".to_string())
        );
        let reference_frame = &algo.params()[5];
        assert_eq!(
            reference_frame.default,
            ParamValue::Enum("middle".to_string())
        );
    }

    #[test]
    fn auto_depth_scales_with_resolution() {
        let ctx_512 = StackContext::new(8, 512, 512, 3);
        assert_eq!(auto_pyramid_depth(&ctx_512), ParamValue::Int(3));

        let ctx_ref = StackContext::new(61, 8192, 5464, 3);
        assert_eq!(auto_pyramid_depth(&ctx_ref), ParamValue::Int(6));
    }

    #[test]
    fn solve_region_is_the_centred_three_quarters_at_512px() {
        let region = PyramidalEcc::solve_region(512, 512);
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
        // Level regions at 8192x5464: 1253x836 on levels 0-2 (the sample
        // budget binds), then the 0.75 share of each halved level; 44 bytes
        // per pixel (values, Jacobian, full-overlap terms).
        // The eight 384 px (128 px at 512) patch templates, 28 bytes per
        // pixel, come on top.
        assert_eq!(
            retained_template_bytes(8192, 5464),
            161_361_640 + 8 * 384 * 384 * 28
        );
        assert_eq!(
            retained_template_bytes(512, 512),
            8_650_400 + 8 * 128 * 128 * 28
        );
    }

    #[test]
    fn solve_scratch_adds_the_capped_windows_to_the_solve_vectors() {
        // Per-pixel bytes of a window: f32 luma plus the u16 samples read
        // into it, `4 + 2 * spp`.
        assert_eq!(
            solve_scratch_bytes(512, 512, 3),
            24 * 384 * 384 + 512 * 512 * 10 + 260 * 260 * 10
        );
        assert_eq!(
            solve_scratch_bytes(8192, 5464, 3),
            24 * 1253 * 836 + 1765 * 1348 * 10 + 772 * 772 * 10
        );
        assert_eq!(
            solve_scratch_bytes(8192, 5464, 1),
            24 * 1253 * 836 + 1765 * 1348 * 6 + 772 * 772 * 6
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

    /// A smooth textured scene: sinusoids of 17 to 131 px wavelength. With
    /// `detail` off only the long ones remain, a defocused view of it.
    fn scene(x: f64, y: f64, detail: bool) -> f64 {
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

    #[test]
    fn a_windowed_level0_solve_equals_the_whole_plane_solve() {
        // Frame 1 is the reference moved by (3, 2), frame 2 by (80, 3).
        let mem = reader(W, H, &[(0.0, 0.0), (3.0, 2.0), (80.0, 3.0)], false);
        let ctx = AlignContext::new(StackContext::new(3, W, H, 1), 0, &mem);
        let reference = whole_plane(&ctx, 0);
        let region = PyramidalEcc::solve_region(W, H);
        let params = test_params(6);

        for mode in [EccMode::Translation, EccMode::Similarity] {
            let template = EccTemplate::prepare(&reference, (0, 0), region, mode).unwrap();
            for (frame, seed) in [
                (1usize, Transform::translation(2.0, 1.0)),
                (2, Transform::translation(78.0, 2.0)),
            ] {
                let moving = whole_plane(&ctx, frame);
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
        let reference = whole_plane(&ctx, 0);
        let region = PyramidalEcc::solve_region(W, H);
        let template =
            EccTemplate::prepare(&reference, (0, 0), region, EccMode::Translation).unwrap();
        let seed = Transform::translation(5000.0, 0.0);
        let params = test_params(6);

        let whole = solve_prepared(
            &template,
            &whole_plane(&ctx, 1),
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
        let reference = whole_plane(&ctx, 0);
        let region = PyramidalEcc::solve_region(W, H);
        let template =
            EccTemplate::prepare(&reference, (0, 0), region, EccMode::Translation).unwrap();
        let params = test_params(8);
        let whole = solve_prepared(
            &template,
            &whole_plane(&ctx, 1),
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
            AlignStatus::Converged | AlignStatus::HitCap
        ));
    }

    #[test]
    fn solve_region_respects_the_sample_budget() {
        let region = PyramidalEcc::solve_region(8192, 5464);
        let count = region.width as u64 * region.height as u64;
        assert!(
            count <= MAX_REGION_PIXELS as u64,
            "region {region:?} holds {count} px, over the {MAX_REGION_PIXELS} budget"
        );
        // Shrinking the naive 6144x4098 (75%) rect by
        // sqrt(2^20 / (6144*4098)) and flooring yields exactly this.
        assert_eq!(region.width, 1253);
        assert_eq!(region.height, 836);
    }
}
