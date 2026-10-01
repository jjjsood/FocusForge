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

use std::sync::{Arc, LazyLock};

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus};
use crate::align::ecc::{EccTemplate, solve_prepared};
use crate::align::{
    AlignPyramid, EccIteration, EccMode, EccParams, EccResult, EccStatus, LumaAssembler, Transform,
};
use crate::params::{EnumVariant, ParamDesc, ParamKind, ParamValue, ParamValues, StackContext};
use crate::registry::AlignRegistration;
use crate::tile::plan::{DEFAULT_TILE_SIZE, MAX_LEVELS};
use crate::tile::{Rect, Tile};
use crate::timing::{TimingCounter, measurement_scope, record_timing};

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
/// Deliberately no tile-geometry term: whole planes are held in RAM, so
/// this closure is not subject to a per-tile cap.
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
                   short of `epsilon-px` and reports `HitCap`, keeping the best \
                   transform it found rather than failing. Frames that converge \
                   earlier stop on `epsilon-px` and pay nothing. On a 61-frame \
                   44.8 MP stack a cap of 50 left 23 frames short (0.048-0.521 \
                   px residual); 400 converges all 61, the slowest at 332 \
                   iterations, for +31 s on a 182 s alignment pass."
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
                   a correlation threshold."
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
            default: ParamValue::Enum("anchor".to_string()),
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

/// Bytes of reference templates a run retains, over every level the
/// pyramid can have (`ceil(dim / 2^k)`, `k` up to [`MAX_LEVELS`]): per
/// solve-region pixel a value (4 B), a four-column Jacobian (32 B) and the
/// full-overlap term (8 B), the rotation-scale mode being the larger one.
pub(crate) fn retained_template_bytes(width: u32, height: u32) -> u64 {
    (0..=MAX_LEVELS)
        .map(|k| {
            let region =
                PyramidalEcc::solve_region(width.div_ceil(1 << k), height.div_ceil(1 << k));
            region.width as u64 * region.height as u64 * (4 + 8 * 4 + 8)
        })
        .sum()
}

/// Bytes one solve allocates besides the templates: three 8-byte-per-pixel
/// vectors over the level-0 solve region, the largest of any level.
pub(crate) fn solve_scratch_bytes(width: u32, height: u32) -> u64 {
    let region = PyramidalEcc::solve_region(width, height);
    region.width as u64 * region.height as u64 * 24
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
    /// [`DEFAULT_TILE_SIZE`] rows and assembles it into an
    /// [`AlignPyramid`] of `depth` levels -- never a whole-frame RGB
    /// buffer.
    fn build_pyramid(
        ctx: &AlignContext<'_>,
        frame: usize,
        depth: u32,
    ) -> Result<AlignPyramid, AlgorithmError> {
        let _scope = measurement_scope("pyramid_prepare");
        let stack = ctx.stack();
        let (width, height, spp) = (stack.width, stack.height, stack.samples_per_pixel);
        let mut assembler = LumaAssembler::new(width, height);

        // One buffer sized for the first (tallest) band serves every band;
        // the last, shorter one only shrinks it.
        let band_len = |rows: u32| width as usize * rows as usize * spp as usize;
        let mut tile = Tile {
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height: DEFAULT_TILE_SIZE.min(height),
            },
            samples_per_pixel: spp,
            samples: vec![0u16; band_len(DEFAULT_TILE_SIZE.min(height))],
        };
        let band_bytes = tile.samples.capacity() * std::mem::size_of::<u16>();
        if band_bytes != 0 {
            record_timing(TimingCounter::AllocationBytes, band_bytes as u64);
            record_timing(TimingCounter::AllocationOperations, 1);
        }

        let mut y = 0u32;
        while y < height {
            let band_height = DEFAULT_TILE_SIZE.min(height - y);
            tile.rect = Rect {
                x: 0,
                y,
                width,
                height: band_height,
            };
            tile.samples.truncate(band_len(band_height));
            ctx.read_tile_into(frame, tile.rect, &mut tile.samples)?;
            assembler.push_tile(&tile).map_err(wrap_error)?;
            y += band_height;
        }

        let base = assembler.finish().map_err(wrap_error)?;
        AlignPyramid::from_luma(base, depth).map_err(wrap_error)
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
                    && p.pyramid.level_count() == depth as usize + 1
                    && p.pyramid.dimensions() == (stack.width, stack.height)
            });
        let built;
        let reference_pyramid: &AlignPyramid = match prepared {
            Some(shared) => &shared.pyramid,
            None => {
                built = Self::build_pyramid(ctx, ctx.reference(), depth)?;
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

        let _scope = measurement_scope("ecc_solve");
        for k in (0..=depth).rev() {
            let level_ref = reference_pyramid.level(k as usize);
            let level_frame = frame_pyramid.level(k as usize);
            let region = Self::solve_region(level_ref.width(), level_ref.height());

            let own_template;
            let template = match prepared
                .and_then(|p| p.templates[k as usize].as_ref())
                .filter(|t| t.region() == region)
            {
                Some(shared) => shared,
                None => {
                    own_template =
                        EccTemplate::prepare(level_ref, region, mode).map_err(wrap_error)?;
                    &own_template
                }
            };

            let result = match levels.as_deref_mut() {
                Some(recs) => {
                    let mut t = Vec::new();
                    let r = solve_prepared(template, level_frame, &seed, &params, Some(&mut t));
                    recs.push(EccLevelRecord {
                        level: k,
                        result: r,
                        trace: t,
                    });
                    r
                }
                None => solve_prepared(template, level_frame, &seed, &params, None),
            };
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

        Ok(AlignOutcome {
            transform: final_transform,
            status: final_status,
            iterations: total_iterations,
            residual: final_residual,
        })
    }
}

/// What [`AlignAlgorithm::prepare_run`] shares across a run's registrations:
/// the reference frame's pyramid and, per level, the solver template over that
/// level's solve region.
struct PreparedReference {
    reference: usize,
    mode: EccMode,
    pyramid: AlignPyramid,
    templates: Vec<Option<EccTemplate>>,
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

    /// Builds the reference frame's pyramid and, per level, its solver
    /// template once, for the driver to hand back to every
    /// [`AlignAlgorithm::align_frame`] call of this run through
    /// [`AlignContext::prepared`]. A level whose solve region is invalid
    /// gets no template, so `run` reports that error itself, per frame,
    /// exactly as it does without a prepared value.
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
        let pyramid = Self::build_pyramid(ctx, ctx.reference(), depth)?;
        let templates = (0..pyramid.level_count())
            .map(|k| {
                let level = pyramid.level(k);
                let region = Self::solve_region(level.width(), level.height());
                EccTemplate::prepare(level, region, mode).ok()
            })
            .collect();
        Ok(Some(Arc::new(PreparedReference {
            reference: ctx.reference(),
            mode,
            pyramid,
            templates,
        })))
    }
}

inventory::submit! { AlignRegistration::new(&PyramidalEcc) }

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(retained_template_bytes(8192, 5464), 161_361_640);
        assert_eq!(retained_template_bytes(512, 512), 8_650_400);
        assert_eq!(solve_scratch_bytes(512, 512), 24 * 384 * 384);
        assert_eq!(solve_scratch_bytes(8192, 5464), 24 * 1253 * 836);
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
