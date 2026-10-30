// SPDX-License-Identifier: GPL-3.0-or-later

//! `laplacian`: the first real [`StackAlgorithm`], selecting per pyramid
//! level and per pixel by Laplacian magnitude across frames, and emitting
//! both the fused composite and the Z-Map that names the winning source
//! frame. Every rounding, ordering and tie-break choice below is binding: a
//! bit-parity GPU twin must reproduce them exactly.
//!
//! # Selection rule
//!
//! For each Laplacian level `k` and each pixel `p`, the winner is the frame
//! with the greatest windowed sharpness score (below), **frame 0 winning
//! any tie** because comparison uses **strictly greater** -- a later frame
//! only overwrites the running best when it is strictly sharper. The
//! composite's channels at `p` all come from the same winning frame: the
//! score sums over every channel before comparing, so a per-channel winner
//! (which would make the Z-Map's single index a lie) never happens. The
//! rule is not iterative -- there is no pass count to pin beyond the two
//! described below, and no state carries from one pixel to the next.
//!
//! A frame takes part in level `k` at pixel `p` only if its coverage mask
//! ([`crate::algorithm::TileCtx::for_each_frame`]), eroded by that level's reach
//! `2*(2^(k+1) - 1) + 2^k * r_k` (`sharpness::eligible`), is
//! true at level-0 position `(2^k x, 2^k y)`. An ineligible frame's score is
//! `-inf`: it never wins and never seeds the running best, so the first
//! eligible frame (not frame 0 as such) wins ties. With no coverage mask every
//! frame is eligible everywhere and the rule is unchanged.
//!
//! The score window is **per-level radius `r_k`**, wider than a fixed 3x3,
//! for the anti-speckle property it buys: a single-pixel
//! impulse in one frame's Laplacian makes that frame win a
//! `(2*r_k+1)`-square neighbourhood, not a single pixel
//! (`tests::score_reaches_exactly_the_window_radius`).
//!
//! ```text
//! SCORE_RADIUS   = 8                                   (pinned constant)
//! headroom(N)    = margin_for_levels(N) - 2 * (2^N - 1)
//! W(N)           = max(1, min(SCORE_RADIUS, headroom(N)))
//! r_k            = max(1, W(N) >> k)
//! ```
//!
//! `N` is the pyramid's `levels` and `k` the level index (`0` = finest, the
//! Z-Map level). `>> k` is what makes the rule **scale-invariant**: radius
//! `W >> k` at level `k` covers the same physical distance at every level
//! (a level-`k` pixel already represents `2^k` level-0 pixels), so one
//! number (`W`) describes the whole pyramid. The `max(1, ...)` outer clamp
//! guarantees every level keeps at least a 1 px window even where `W >> k`
//! would floor to 0 (the deepest levels of a shallow pyramid).
//!
//! The `headroom` clamp is what makes the tiling exactness derivation below
//! hold at **every** level count, not only the shipped `levels = 5`: at
//! `levels = 1` and `levels = 2` a flat radius 8 would not fit inside the
//! margin those depths are planned with (`2 + 8 > 4`, `6 + 8 > 12`), and the
//! clamp drops `W` to 2 and 6 there instead. Use
//! [`crate::tile::plan::margin_for_levels`], **never**
//! `crate::fuse::pyramid::MEASURED_MARGIN[levels]` directly: the planner
//! rounds the raw table up to the `2^levels` decimation grid, and that
//! rounded value is what the tile geometry actually guarantees. See
//! `score_radius` (`pub`, so the Depth Map and `crates/gpu`'s WGSL twin
//! can both derive the same radii) for the implementation and
//! `tests::selection_reach_fits_the_measured_margin` for the inequality
//! pinned at every level count `1..=MAX_LEVELS`.
//!
//! Accumulation is a **separable two-pass** sum, not the nested `dy`/`dx`/`c`
//! loop of a fixed 3x3 window -- `O(radius)` per pixel where the nested form
//! is `O(radius^2)` (68 adds per pixel at `radius = 8`, three channels,
//! versus the nested form's 867), and the shape a compute shader wants
//! (two dispatches, or one with a shared-memory row cache). This is a
//! **deliberate re-pin** of the single-nested-accumulator order the
//! original 3x3 rule used, measured to move 0 of 262,144 winners at
//! radius 1, 1 at radius 6 and 2 at radius 8 when the reordering is applied
//! to the same data -- evidence that the selection is decided by real score
//! margins, not by which order `f32` happens to add in.
//!
//! ```text
//! row[y][x]   = sum_{dx=-r..=r} sum_{c=0..spp}
//!               |L_k[(y, clamp(x+dx)) * spp + c]|      (dx asc, then c asc)
//! score[y][x] = sum_{dy=-r..=r} row[clamp(y+dy)][x]     (dy asc)
//! ```
//!
//! Both passes clamp-to-edge, the same rule [`crate::fuse::pyramid::reduce`]
//! and [`crate::fuse::pyramid::expand`] use, and both accumulate into one
//! `f32` in the pinned ascending order above because `f32` addition is not
//! associative (`crate::fuse::pyramid`'s own module docs, `:40-46`). This
//! two-pass order, together with `W(N)`/`r_k` and the `max(1, ...)` and
//! `headroom` clamps above, is everything a GPU twin needs to
//! reproduce this rule at <= 1 LSB: there is no other rounding, ordering or
//! tie-break choice hiding in the implementation.
//!
//! # The Z-Map
//!
//! The per-pixel Z-Map is the **level-0 winner** -- the finest level, one
//! index per pixel, never per sample. It is [`crate::zmap::NO_SOURCE`] only
//! where no frame is eligible at level 0 (the pixel's detail coefficients are
//! then zero); with no coverage mask every pixel has a winner. Every map
//! passes through [`crate::zmap::validate_tile`].
//!
//! # The residual
//!
//! The coarsest Gaussian (the pyramid's residual, carrying no detail to select on) is the
//! **arithmetic mean of the eligible frames' residuals, summed in `f64` in ascending frame order,
//! divided by the eligible count and rounded to `f32` once**. A frame is eligible at a residual
//! pixel when its coverage mask, eroded by `2*(2^N - 1)`, is true at level-0 `(2^N x, 2^N y)`; none
//! eligible gives `0`, no mask makes every frame eligible. The order is tile-limit independent. The
//! GPU twin's compensated `f32` pair stays within 1 LSB at 16-bit. At 5 levels the residual is 1/32
//! resolution, so uncorrected drift is a fraction of one residual pixel -- too small to ghost.
//!
//! # Memory
//!
//! Streaming, never `frame_count` pyramids at once: frame 0's pyramid
//! becomes the running best, and every later frame's pyramid is built,
//! compared level by level, and dropped. Peak live is two pyramids plus one
//! `f32` score plane and one `u16` winner plane per level -- independent of
//! `frame_count`.
//!
//! # Tiling
//!
//! A tiled run's composite and Z-Map equal an untiled run's byte for byte,
//! at any tile size the planner accepts. Every interior pixel reads
//! bit-identical samples in both runs, combined in the same order on the
//! same decimation grid, because the planned margin covers the whole
//! dependency cone of a composite pixel:
//!
//! - A level-`k` selection at `p` reads coefficients within `build`'s reach,
//!   `2*(2^N - 1)`, plus that level's score window `2^k * r_k`: 70 px at every
//!   level but level 4 at `N = 5`, where the `max(1, ...)` clamp makes it 78.
//! - `collapse` adds its own `2*(2^N - 1)` on top: the composite cone is
//!   `78 + 62 = 140` px at `N = 5`, 68 at 4, 36 at 3. Coverage depends on the
//!   global pixel position and adds nothing beyond it.
//!
//! [`Laplacian::tile_support`] reports that cone as the reach and the planner
//! rounds it up to the `2^N` decimation grid: a 160 px margin at `N = 5`, 80
//! at 4, 40 at 3. A selection flip at an interior edge is discrete, so no
//! smaller margin is exact. [`score_radius`] keeps reading the pyramid-only
//! [`crate::tile::plan::margin_for_levels`].
//!
//! | level `k` | cone at fixed radius 1, `62 + 2^k` | cone with `W = 8`, `62 + 2^k * r_k` |
//! |---|---|---|
//! | 0 | 63 | 70 |
//! | 1 | 64 | 70 |
//! | 2 | 66 | 70 |
//! | 3 | 70 | 70 |
//! | 4 | **78** | **78** |

use std::sync::{Arc, LazyLock};

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::fuse::error::{algorithm_error, missing_key};
use crate::fuse::sharpness::eligible;
use crate::fuse::{Plane, Pyramid, build, collapse, max_levels};
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues, StackContext};
use crate::registry::StackRegistration;
use crate::tile::plan::{
    DEFAULT_TILE_SIZE, MAX_LEVELS, TileSupport, margin_for_levels, max_levels_for_tile,
};
use crate::tile::scheduler::DEFAULT_LEVELS;
use crate::tile::{Sample, SampleFormat, Tile};
use crate::zmap::NO_SOURCE;

/// `auto_levels(ctx) = min(fuse::max_levels(ctx.width, ctx.height),
/// tile::plan::max_levels_for_tile(DEFAULT_TILE_SIZE)).max(1)` -- the hook
/// must never propose a depth the baseline tile geometry cannot plan. The
/// second term is a constant (`5`, since the measured minimum tile
/// `min_tile_size_for_levels(5) == 448 <= 512 < min_tile_size_for_levels(6)
/// == 832`), so resolution only moves this value below a small minimum
/// dimension.
fn auto_levels(ctx: &StackContext) -> ParamValue {
    let by_resolution = max_levels(ctx.width, ctx.height);
    let by_tile_geometry = max_levels_for_tile(DEFAULT_TILE_SIZE);
    ParamValue::Int(by_resolution.min(by_tile_geometry).max(1) as i64)
}

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![ParamDesc {
        id: "levels".to_string(),
        label: "Levels".to_string(),
        help: "How many layers of detail are compared and blended; the automatic \
               value suits the image size."
            .to_string(),
        kind: ParamKind::Int {
            min: 1,
            max: MAX_LEVELS as i64,
        },
        default: ParamValue::Int(DEFAULT_LEVELS as i64),
        advanced: true,
        auto: Some(Arc::new(auto_levels)),
        host_default: None,
    }]
});

/// Clamp-to-edge on one axis, the same rule
/// [`crate::fuse::pyramid::reduce`] and [`crate::fuse::pyramid::expand`]
/// use.
fn clamp_edge(v: i64, dim: u32) -> u32 {
    v.clamp(0, dim as i64 - 1) as u32
}

/// The pinned upper bound on the level-0 sharpness window's radius,
/// before the per-level-count headroom clamp in [`score_radius`] is
/// applied. See the module docs' "Selection rule" section for the full
/// `W(N)`/`r_k` derivation.
pub const SCORE_RADIUS: u32 = 8;

/// The windowed sharpness score's radius at pyramid `level` of a `levels`-
/// deep pyramid: `W(N) = max(1, min(SCORE_RADIUS, headroom(N)))`,
/// `headroom(N) = margin_for_levels(N) - 2*(2^N - 1)`, and
/// `r_k = max(1, W(N) >> k)`. See the module docs' "Selection rule" section
/// for why the headroom clamp exists and why the right shift is what makes
/// the rule scale-invariant. `pub` because the Depth Map reuses the
/// scoring rule rather than re-deriving it, and `crates/gpu`'s WGSL twin
/// computes its per-level radius host-side from this same function rather
/// than re-deriving it in a shader.
///
/// Uses [`margin_for_levels`], **never** `pyramid::MEASURED_MARGIN[levels]`
/// directly: the planner rounds the raw table up to the `2^levels`
/// decimation grid, and that rounded value is what the tile geometry
/// actually guarantees.
pub fn score_radius(levels: u32, level: u32) -> u32 {
    let build_reach = 2 * ((1u32 << levels) - 1);
    let headroom = margin_for_levels(levels).saturating_sub(build_reach);
    let w = SCORE_RADIUS.min(headroom).max(1);
    (w >> level).max(1)
}

/// The windowed sharpness score of every pixel of one Laplacian level: a
/// `(2*radius+1)`-square, clamp-to-edge sum of `|L|` over every channel, in
/// the pinned **separable two-pass** accumulation order this module's docs
/// pin -- row sums (`dx` ascending, then `c` ascending) into a scratch
/// plane, then column sums (`dy` ascending) of that scratch plane into the
/// result. `O(radius)` per pixel, not `O(radius^2)`: at `radius = 8` this is
/// 68 adds per pixel (three-channel) rather than the nested form's 867. One
/// `f32` per pixel, never per sample.
///
/// `pub` so [`crate::fuse::weighted_average`] and [`crate::fuse::depth_map`] call this exact
/// function (at their own radii) instead of reimplementing a second windowed sum -- keeping all
/// three algorithms comparable on the same fixture and measure -- and so `crates/gpu`'s parity
/// tests can call the identical CPU twin their WGSL kernel is compared against.
pub fn score_plane(level: &Plane, radius: u32) -> Vec<f32> {
    let width = level.width();
    let height = level.height();
    let spp = level.samples_per_pixel();
    let data = level.data();
    let r = radius as i64;
    let pixel_count = (width as usize) * (height as usize);

    // Pass 1: row sums, `dx` ascending then `c` ascending.
    let mut row_sums = vec![0.0f32; pixel_count];
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0f32;
            for dx in -r..=r {
                let sx = clamp_edge(x as i64 + dx, width);
                for c in 0..spp {
                    let idx = ((y * width + sx) * spp + c) as usize;
                    acc += data[idx].abs();
                }
            }
            row_sums[(y * width + x) as usize] = acc;
        }
    }

    // Pass 2: column sums of the row sums, `dy` ascending.
    let mut scores = vec![0.0f32; pixel_count];
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0f32;
            for dy in -r..=r {
                let sy = clamp_edge(y as i64 + dy, height);
                acc += row_sums[(sy * width + x) as usize];
            }
            scores[(y * width + x) as usize] = acc;
        }
    }
    scores
}

/// The level-0 distance within which level `k`'s selection of a `levels`-deep
/// pyramid reads samples: `build`'s reach for `k + 1` levels plus the level's
/// score window, `2*(2^(k+1) - 1) + 2^k * r_k`.
fn level_reach(levels: u32, k: u32) -> u32 {
    2 * ((1u32 << (k + 1)) - 1) + (score_radius(levels, k) << k)
}

/// The level-0 distance within which the residual of a `levels`-deep pyramid
/// reads samples: `build`'s reach, `2*(2^N - 1)`.
fn residual_reach(levels: u32) -> u32 {
    2 * ((1u32 << levels) - 1)
}

/// The level-0 distance a composite pixel of a `levels`-deep pyramid depends
/// on: the widest selection cone (any level's `level_reach`, or the
/// residual's) plus `collapse`'s own reach.
fn composite_reach(levels: u32) -> u32 {
    let levels = levels.min(MAX_LEVELS);
    let selection = (0..levels)
        .map(|k| level_reach(levels, k))
        .max()
        .unwrap_or(0)
        .max(residual_reach(levels));
    selection + residual_reach(levels)
}

/// Where one frame takes part, per pyramid level and for the residual: its
/// coverage mask eroded by that level's reach, read at each level pixel's
/// level-0 position `(2^k x, 2^k y)`. Level dimensions follow the
/// `ceil(n / 2)` chain [`crate::fuse::pyramid::reduce`] uses.
pub struct EligibleMasks {
    /// One mask per Laplacian level, row-major at that level's dimensions.
    pub levels: Vec<Vec<bool>>,
    /// The mask at the residual's dimensions.
    pub residual: Vec<bool>,
}

impl EligibleMasks {
    /// `covered` is the row-major `width * height` coverage of one frame.
    pub fn new(covered: &[bool], width: u32, height: u32, levels: u32) -> Self {
        let sampled = |reach: u32, k: u32| {
            let eroded = eligible(covered, width, height, reach);
            let (level_width, level_height) = (width.div_ceil(1 << k), height.div_ceil(1 << k));
            let mut out = Vec::with_capacity(level_width as usize * level_height as usize);
            for y in 0..level_height {
                let row = clamp_edge(i64::from(y) << k, height) as usize * width as usize;
                out.extend(
                    (0..level_width)
                        .map(|x| eroded[row + clamp_edge(i64::from(x) << k, width) as usize]),
                );
            }
            out
        };
        Self {
            levels: (0..levels)
                .map(|k| sampled(level_reach(levels, k), k))
                .collect(),
            residual: sampled(residual_reach(levels), levels),
        }
    }
}

/// The per-tile state carried from frame to frame: the running best pyramid,
/// its per-level scores and winners, and the residual sum and count.
struct Running {
    best: Pyramid,
    best_scores: Vec<Vec<f32>>,
    winner: Vec<Vec<u16>>,
    residual_sum: Vec<f64>,
    residual_count: Vec<u32>,
}

/// The Laplacian Pyramid fusion algorithm. See the module docs for the
/// selection rule, the residual rule and the Z-Map semantics.
pub struct Laplacian;

impl StackAlgorithm for Laplacian {
    fn id(&self) -> &str {
        "laplacian"
    }

    fn display_name(&self) -> &str {
        "Details"
    }

    fn is_default(&self) -> bool {
        true
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        match ctx.sample_format() {
            SampleFormat::U16 => self.fuse_as::<u16, u16>(ctx, cfg),
            SampleFormat::F32 => self.fuse_as::<f32, u16>(ctx, cfg),
        }
    }

    fn fuse_f32(
        &self,
        ctx: &TileCtx<'_>,
        cfg: &ParamValues,
    ) -> Result<TileOutput<f32>, AlgorithmError> {
        match ctx.sample_format() {
            SampleFormat::U16 => self.fuse_as::<u16, f32>(ctx, cfg),
            SampleFormat::F32 => self.fuse_as::<f32, f32>(ctx, cfg),
        }
    }

    /// Two `f32` pyramids (the running best and the current frame's, `4/3`
    /// for the levels), the collapsed plane and `u16` output, then per level
    /// a score, a winner, the current score and an eligibility byte, plus the
    /// Z-Map; an `f32` output adds 2 B per sample.
    fn working_bytes_per_pixel(
        &self,
        _cfg: &ParamValues,
        samples_per_pixel: u32,
        format: SampleFormat,
    ) -> u64 {
        let per_sample = if format == SampleFormat::F32 { 19 } else { 17 };
        per_sample * u64::from(samples_per_pixel) + 17
    }

    /// The composite's dependency cone (`composite_reach`) on the `levels`
    /// decimation grid.
    fn tile_support(&self, cfg: &ParamValues) -> TileSupport {
        let levels = cfg
            .get("levels")
            .and_then(ParamValue::as_i64)
            .map(|levels| levels.max(0) as u32)
            .unwrap_or(0);
        TileSupport {
            reach_px: composite_reach(levels),
            decimation_levels: levels,
        }
    }
}

impl Laplacian {
    /// Fuses frames read as `S` into a composite of `O` samples.
    fn fuse_as<S: Sample, O: Sample>(
        &self,
        ctx: &TileCtx<'_>,
        cfg: &ParamValues,
    ) -> Result<TileOutput<O>, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let levels = cfg
            .get("levels")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("laplacian", "levels"))? as u32;

        let rect = ctx.rect();
        let width = rect.width;
        let height = rect.height;

        let max = max_levels(width, height);
        if levels > max {
            return Err(AlgorithmError::Failed {
                algorithm: "laplacian".to_string(),
                detail: format!(
                    "read rect {width}x{height} supports at most {max} levels, requested {levels}"
                ),
            });
        }

        let frame_count = ctx.frame_count();
        let spp = ctx.samples_per_pixel()?;
        let spp_usize = spp as usize;

        // Frame 0's pyramid seeds the running best -- streaming, never
        // `frame_count` pyramids at once.
        let mut running: Option<Running> = None;
        ctx.for_each_frame_as::<S>(|f, samples, covered| {
            let current =
                build(samples, width, height, spp, levels).map_err(algorithm_error("laplacian"))?;
            let masks = covered.map(|covered| EligibleMasks::new(covered, width, height, levels));

            let Some(run) = running.as_mut() else {
                let mut best = current;
                let mut best_scores: Vec<Vec<f32>> = best
                    .levels()
                    .iter()
                    .enumerate()
                    .map(|(k, plane)| score_plane(plane, score_radius(levels, k as u32)))
                    .collect();
                let mut winner: Vec<Vec<u16>> = best_scores
                    .iter()
                    .map(|scores| vec![0u16; scores.len()])
                    .collect();

                // Ascending frame order, `f64`, per the residual rule above.
                // Frame 0's own contribution is the seed.
                let mut residual_sum: Vec<f64> = best
                    .residual()
                    .data()
                    .iter()
                    .map(|&value| f64::from(value))
                    .collect();
                let mut residual_count = vec![1u32; residual_sum.len()];

                // An ineligible frame 0 must not seed the running best: its
                // pixels start as "no source" with zero coefficients and a
                // `-inf` score.
                if let Some(masks) = &masks {
                    for k in 0..levels as usize {
                        for (p, &ok) in masks.levels[k].iter().enumerate() {
                            if !ok {
                                best_scores[k][p] = f32::NEG_INFINITY;
                                winner[k][p] = NO_SOURCE;
                                best.level_mut(k).data_mut()[p * spp_usize..(p + 1) * spp_usize]
                                    .fill(0.0);
                            }
                        }
                    }
                    for (i, count) in residual_count.iter_mut().enumerate() {
                        if !masks.residual[i / spp_usize] {
                            residual_sum[i] = 0.0;
                            *count = 0;
                        }
                    }
                }
                running = Some(Running {
                    best,
                    best_scores,
                    winner,
                    residual_sum,
                    residual_count,
                });
                return Ok(());
            };

            for (i, (sum, &value)) in run
                .residual_sum
                .iter_mut()
                .zip(current.residual().data().iter())
                .enumerate()
            {
                if masks.as_ref().is_none_or(|m| m.residual[i / spp_usize]) {
                    *sum += f64::from(value);
                    run.residual_count[i] += 1;
                }
            }

            for k in 0..levels as usize {
                let current_level = &current.levels()[k];
                let current_score = score_plane(current_level, score_radius(levels, k as u32));
                for (p, &score) in current_score.iter().enumerate() {
                    if masks.as_ref().is_some_and(|m| !m.levels[k][p]) {
                        continue;
                    }
                    // Strictly greater: frame 0 (or the lowest-indexed
                    // frame reaching this score first) wins an exact tie.
                    if score > run.best_scores[k][p] {
                        let src = &current_level.data()[p * spp_usize..(p + 1) * spp_usize];
                        let dst_level = run.best.level_mut(k);
                        dst_level.data_mut()[p * spp_usize..(p + 1) * spp_usize]
                            .copy_from_slice(src);
                        run.best_scores[k][p] = score;
                        run.winner[k][p] = f as u16;
                    }
                }
            }
            Ok(())
        })?;
        let Running {
            mut best,
            winner,
            residual_sum,
            residual_count,
            ..
        } = running.expect("samples_per_pixel succeeded, so frame 0 seeded the running best");

        for (out, (sum, &count)) in best
            .residual_mut()
            .data_mut()
            .iter_mut()
            .zip(residual_sum.iter().zip(&residual_count))
        {
            *out = if count == 0 {
                0.0
            } else {
                (*sum / f64::from(count)) as f32
            };
        }

        let collapsed = collapse(&best);
        let samples: Vec<O> = collapsed.data().iter().map(|&v| O::narrow(v)).collect();

        // Level-0 winner is the Z-Map; `levels >= 1` is enforced by the
        // schema, so `winner` always has at least one entry.
        let zmap = winner
            .into_iter()
            .next()
            .expect("levels >= 1 (enforced by the schema) guarantees a level-0 winner plane");
        crate::zmap::validate_tile(&zmap, frame_count as u32)
            .map_err(algorithm_error("laplacian"))?;

        Ok(TileOutput {
            tile: Tile {
                rect,
                samples_per_pixel: spp,
                samples,
            },
            zmap,
        })
    }
}

inventory::submit! { StackRegistration::new(&Laplacian) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::plan::TilePlan;

    fn cfg_with_levels(levels: i64) -> ParamValues {
        let mut cfg = ParamValues::new();
        cfg.insert("levels".to_string(), ParamValue::Int(levels));
        cfg
    }

    fn rect(width: u32, height: u32) -> crate::tile::Rect {
        crate::tile::Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn tile_of(width: u32, height: u32, spp: u32, samples: Vec<u16>) -> Tile {
        Tile {
            rect: rect(width, height),
            samples_per_pixel: spp,
            samples,
        }
    }

    fn flat(width: u32, height: u32, spp: u32, value: u16) -> Vec<u16> {
        vec![value; width as usize * height as usize * spp as usize]
    }

    fn checkerboard(width: u32, height: u32, spp: u32) -> Vec<u16> {
        let mut out = vec![0u16; width as usize * height as usize * spp as usize];
        for y in 0..height {
            for x in 0..width {
                let v = if (x + y) % 2 == 0 { 40000 } else { 20000 };
                for c in 0..spp {
                    out[((y * width + x) * spp + c) as usize] = v;
                }
            }
        }
        out
    }

    #[test]
    fn auto_levels_are_capped_by_tile_geometry() {
        let algo = Laplacian;
        let hook = algo.params()[0]
            .auto
            .clone()
            .expect("levels has an auto hook");

        let ctx_64 = StackContext::new(8, 64, 64, 3);
        assert_eq!(hook(&ctx_64), ParamValue::Int(4));

        let ctx_512 = StackContext::new(8, 512, 512, 3);
        assert_eq!(hook(&ctx_512), ParamValue::Int(5));

        let ctx_reference = StackContext::new(61, 8192, 5464, 3);
        assert_eq!(hook(&ctx_reference), ParamValue::Int(5));
    }

    #[test]
    fn auto_levels_always_plan() {
        let algo = Laplacian;
        let hook = algo.params()[0]
            .auto
            .clone()
            .expect("levels has an auto hook");

        for &(frame_count, w, h) in &[(8usize, 64u32, 64u32), (8, 512, 512), (61, 8192, 5464)] {
            let ctx = StackContext::new(frame_count, w, h, 3);
            let ParamValue::Int(levels) = hook(&ctx) else {
                panic!("auto must produce an Int")
            };
            TilePlan::new(w, h, DEFAULT_TILE_SIZE, levels as u32).unwrap_or_else(|e| {
                panic!("auto-proposed levels={levels} must plan for {w}x{h}: {e}")
            });
        }
    }

    #[test]
    fn every_parameter_the_implementation_reads_is_declared() {
        // `fuse` reads exactly one key, "levels" -- omitting it from an
        // otherwise-valid `cfg` must be a named error, never a panic, which
        // is what proves the implementation never reaches past its own
        // declared schema.
        let algo = Laplacian;
        let (rect_, tiles) = {
            let (w, h, spp) = (8u32, 8u32, 3u32);
            (rect(w, h), vec![tile_of(w, h, spp, flat(w, h, spp, 100))])
        };
        let ctx = TileCtx::new(rect_, &tiles);
        let err = algo.fuse(&ctx, &ParamValues::new()).unwrap_err();
        match err {
            AlgorithmError::Failed { algorithm, detail } => {
                assert_eq!(algorithm, "laplacian");
                assert!(detail.contains("levels"));
            }
            other => panic!("expected AlgorithmError::Failed naming `levels`, got {other:?}"),
        }
    }

    #[test]
    fn a_read_rect_too_small_for_the_requested_depth_is_a_named_error() {
        let algo = Laplacian;
        let (w, h, spp) = (8u32, 8u32, 3u32);
        // max_levels(8, 8) == 1, so levels=3 must be refused by name.
        let tiles = vec![tile_of(w, h, spp, flat(w, h, spp, 100))];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let err = algo.fuse(&ctx, &cfg_with_levels(3)).unwrap_err();
        match err {
            AlgorithmError::Failed { algorithm, detail } => {
                assert_eq!(algorithm, "laplacian");
                assert!(detail.contains('1'), "must name the actual max: {detail}");
                assert!(
                    detail.contains('3'),
                    "must name the requested depth: {detail}"
                );
            }
            other => panic!("expected AlgorithmError::Failed, got {other:?}"),
        }
    }

    #[test]
    fn identical_frames_tie_break_to_the_lowest_index() {
        let algo = Laplacian;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let content = checkerboard(w, h, spp);
        let tiles = vec![
            tile_of(w, h, spp, content.clone()),
            tile_of(w, h, spp, content.clone()),
            tile_of(w, h, spp, content),
        ];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg_with_levels(1)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 0),
            "identical frames must tie-break to frame 0 everywhere"
        );
    }

    #[test]
    fn a_single_frame_stack_round_trips_and_wins_everywhere() {
        let algo = Laplacian;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let content = checkerboard(w, h, spp);
        let tiles = vec![tile_of(w, h, spp, content.clone())];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg_with_levels(2)).unwrap();
        assert!(out.zmap.iter().all(|&index| index == 0));
        assert_eq!(
            out.tile.samples, content,
            "a one-frame stack's composite must round-trip the input bit-identically"
        );
    }

    #[test]
    fn the_sharpest_frame_wins_everywhere() {
        let algo = Laplacian;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let flat_a = flat(w, h, spp, 30000);
        let sharp = checkerboard(w, h, spp);
        let flat_c = flat(w, h, spp, 30000);
        let tiles = vec![
            tile_of(w, h, spp, flat_a),
            tile_of(w, h, spp, sharp),
            tile_of(w, h, spp, flat_c),
        ];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg_with_levels(1)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 1),
            "the only frame with detail must win every pixel"
        );
    }

    #[test]
    fn score_reaches_exactly_the_window_radius() {
        // A single-pixel impulse in an otherwise-zero Laplacian level: the
        // windowed score at radius `r` must be nonzero at every pixel within
        // Chebyshev distance `r` of the impulse, and exactly zero everywhere
        // farther away -- the anti-speckle property the windowed score
        // buys, checked at both radius 1 and radius 8 on a plane large
        // enough for radius 8's full exclusion zone.
        let (width, height, spp) = (33u32, 33u32, 1u32);
        let (cx, cy) = (16u32, 16u32);
        let mut data = vec![0.0f32; (width * height) as usize];
        data[(cy * width + cx) as usize] = 100.0;
        let level = Plane::from_u16(&flat(width, height, spp, 0), width, height, spp).unwrap();
        // Rebuild with the impulse: `Plane` has no public constructor
        // taking `f32` data directly, so round-trip through `data_mut`.
        let mut level = level;
        level.data_mut().copy_from_slice(&data);

        for &r in &[1u32, 8u32] {
            let scores = score_plane(&level, r);
            for y in 0..height {
                for x in 0..width {
                    let dist = (x as i64 - cx as i64)
                        .unsigned_abs()
                        .max((y as i64 - cy as i64).unsigned_abs())
                        as u32;
                    let score = scores[(y * width + x) as usize];
                    if dist <= r {
                        assert!(
                            score > 0.0,
                            "radius {r}: pixel ({x},{y}) at Chebyshev distance {dist} must see \
                             the impulse"
                        );
                    } else {
                        assert_eq!(
                            score, 0.0,
                            "radius {r}: pixel ({x},{y}) at Chebyshev distance {dist} must not \
                             see the impulse"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn selection_reach_fits_the_measured_margin() {
        // The load-bearing inequality the "Tiling"
        // rustdoc states: every level's selection cone must fit inside the
        // margin the tile planner actually guarantees, at every level
        // count -- not just the shipped `levels = 5`. A later change to
        // `SCORE_RADIUS` or to the per-level derivation that breaks this
        // must fail here, loudly, rather than silently break the tiled
        // Z-Map equality.
        for levels in 1..=MAX_LEVELS {
            let margin = crate::tile::plan::margin_for_levels(levels);
            let build_reach = 2 * ((1u32 << levels) - 1);
            for k in 0..levels {
                let radius = score_radius(levels, k);
                let cone = build_reach + (radius << k);
                assert!(
                    cone <= margin,
                    "levels={levels} level={k}: cone {cone} (build reach {build_reach} + \
                     radius {radius} << {k}) exceeds margin_for_levels({levels}) = {margin}"
                );
            }
        }
    }

    fn fuse_masked(
        tiles: &[Tile],
        coverage: &[Option<Vec<bool>>],
        levels: i64,
        w: u32,
        h: u32,
    ) -> TileOutput {
        let ctx = TileCtx::new(rect(w, h), tiles).with_coverage(coverage);
        Laplacian.fuse(&ctx, &cfg_with_levels(levels)).unwrap()
    }

    fn columns_uncovered_below(w: u32, h: u32, border: u32) -> Option<Vec<bool>> {
        Some(
            (0..h)
                .flat_map(|_| (0..w).map(move |x| x >= border))
                .collect(),
        )
    }

    #[test]
    fn working_bytes_per_pixel_are_pinned() {
        let cfg = cfg_with_levels(5);
        assert_eq!(
            Laplacian.working_bytes_per_pixel(&cfg, 3, SampleFormat::U16),
            68
        );
        assert_eq!(
            Laplacian.working_bytes_per_pixel(&cfg, 1, SampleFormat::U16),
            34
        );
    }

    #[test]
    fn f32_output_adds_two_bytes_per_sample_to_the_working_bytes() {
        let cfg = cfg_with_levels(5);
        for spp in [1, 3] {
            let u16_bytes = Laplacian.working_bytes_per_pixel(&cfg, spp, SampleFormat::U16);
            let f32_bytes = Laplacian.working_bytes_per_pixel(&cfg, spp, SampleFormat::F32);
            assert_eq!(f32_bytes - u16_bytes, 2 * u64::from(spp));
        }
    }

    #[test]
    fn tile_support_covers_the_composite_cone() {
        for (levels, reach, margin) in [(3i64, 36, 40), (4, 68, 80), (5, 140, 160)] {
            let support = Laplacian.tile_support(&cfg_with_levels(levels));
            assert_eq!(support.reach_px, reach, "levels {levels}");
            assert_eq!(support.decimation_levels, levels as u32);
            assert_eq!(support.margin(), margin, "levels {levels}");
        }
        for levels in 1..=MAX_LEVELS {
            let support = Laplacian.tile_support(&cfg_with_levels(i64::from(levels)));
            assert!(support.margin() >= support.reach_px);
            assert!(support.margin() >= margin_for_levels(levels));
        }
        assert_eq!(
            Laplacian
                .tile_support(&cfg_with_levels(5))
                .default_tile_size(),
            704
        );
    }

    #[test]
    fn level_reaches_follow_the_cone_derivation() {
        let reaches: Vec<u32> = (0..5).map(|k| level_reach(5, k)).collect();
        assert_eq!(reaches, vec![10, 14, 22, 38, 78]);
        assert_eq!(residual_reach(5), 62);
        let reaches: Vec<u32> = (0..3).map(|k| level_reach(3, k)).collect();
        assert_eq!(reaches, vec![10, 14, 22]);
    }

    #[test]
    fn a_frame_with_an_uncovered_border_wins_only_beyond_the_reach() {
        let (w, h, spp, border) = (160u32, 128u32, 1u32, 20u32);
        for levels in [3i64, 5] {
            let tiles = [
                tile_of(w, h, spp, flat(w, h, spp, 30000)),
                tile_of(w, h, spp, checkerboard(w, h, spp)),
            ];
            let masks = [None, columns_uncovered_below(w, h, border)];
            let out = fuse_masked(&tiles, &masks, levels, w, h);
            let reach = level_reach(levels as u32, 0);
            for y in 0..h {
                for x in 0..w {
                    let expected = u16::from(x >= border + reach);
                    assert_eq!(
                        out.zmap[(y * w + x) as usize],
                        expected,
                        "levels {levels} at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_residual_averages_only_the_eligible_frames() {
        let (w, h, spp) = (256u32, 128u32, 1u32);
        let tiles = [
            tile_of(w, h, spp, flat(w, h, spp, 10000)),
            tile_of(w, h, spp, flat(w, h, spp, 50000)),
        ];
        let masks = [None, columns_uncovered_below(w, h, 16)];
        let out = fuse_masked(&tiles, &masks, 5, w, h);
        let at = |x: u32| out.tile.samples[(64 * w + x) as usize];
        for x in 0..16 {
            assert!(at(x).abs_diff(10000) <= 1, "uncovered edge at x {x}");
        }
        for x in 160..w {
            assert!(at(x).abs_diff(30000) <= 1, "both frames at x {x}");
        }
    }

    const CONSTANT_TRIPLES: [[u16; 3]; 3] = [[40001; 3], [39835, 49858, 60138], [73, 3073, 5001]];

    fn constant_tiles(triple: [u16; 3], count: usize, w: u32, h: u32) -> Vec<Tile> {
        let samples: Vec<u16> = triple
            .iter()
            .copied()
            .cycle()
            .take((w * h * 3) as usize)
            .collect();
        (0..count)
            .map(|_| tile_of(w, h, 3, samples.clone()))
            .collect()
    }

    fn assert_every_sample_is(out: &TileOutput, triple: [u16; 3], context: &str) {
        for (i, &sample) in out.tile.samples.iter().enumerate() {
            assert_eq!(sample, triple[i % 3], "{context}: sample {i}");
        }
    }

    #[test]
    fn constant_stack_exports_the_constant() {
        let (w, h) = (128u32, 128u32);
        for triple in CONSTANT_TRIPLES {
            let tiles = constant_tiles(triple, 999, w, h);
            for n in [1usize, 61, 999] {
                let ctx = TileCtx::new(rect(w, h), &tiles[..n]);
                let out = Laplacian.fuse(&ctx, &cfg_with_levels(5)).unwrap();
                assert_every_sample_is(&out, triple, &format!("{triple:?} at {n} frames"));
            }
        }
    }

    #[test]
    fn constant_stack_masked_exports_the_constant() {
        let (w, h) = (128u32, 128u32);
        for triple in CONSTANT_TRIPLES {
            let tiles = constant_tiles(triple, 999, w, h);
            let coverage: Vec<Option<Vec<bool>>> = (0..tiles.len())
                .map(|f| {
                    if f % 2 == 1 {
                        columns_uncovered_below(w, h, 16)
                    } else {
                        None
                    }
                })
                .collect();
            for n in [1usize, 61, 999] {
                let out = fuse_masked(&tiles[..n], &coverage[..n], 5, w, h);
                assert_every_sample_is(&out, triple, &format!("{triple:?} masked at {n} frames"));
            }
        }
    }

    #[test]
    fn an_ineligible_first_frame_does_not_seed_the_selection() {
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let content = checkerboard(w, h, spp);
        let tiles = [
            tile_of(w, h, spp, flat(w, h, spp, 12345)),
            tile_of(w, h, spp, content.clone()),
        ];
        let none = Some(vec![false; (w * h) as usize]);
        let out = fuse_masked(&tiles, &[none, None], 2, w, h);
        assert!(out.zmap.iter().all(|&z| z == 1));
        assert_eq!(out.tile.samples, content);
    }

    #[test]
    fn a_pixel_with_no_eligible_frame_is_no_source_with_zero_samples() {
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let tiles = [
            tile_of(w, h, spp, checkerboard(w, h, spp)),
            tile_of(w, h, spp, flat(w, h, spp, 999)),
        ];
        let none = Some(vec![false; (w * h) as usize]);
        let out = fuse_masked(&tiles, &[none.clone(), none], 2, w, h);
        assert!(out.zmap.iter().all(|&z| z == crate::zmap::NO_SOURCE));
        assert!(out.tile.samples.iter().all(|&s| s == 0));
    }

    #[test]
    fn full_coverage_masks_reproduce_the_unmasked_output() {
        let (w, h, spp) = (32u32, 32u32, 3u32);
        let tiles = [
            tile_of(w, h, spp, checkerboard(w, h, spp)),
            tile_of(w, h, spp, flat(w, h, spp, 31000)),
            tile_of(w, h, spp, checkerboard(w, h, spp)),
        ];
        let plain = Laplacian
            .fuse(&TileCtx::new(rect(w, h), &tiles), &cfg_with_levels(3))
            .unwrap();
        let full = Some(vec![true; (w * h) as usize]);
        let masked = fuse_masked(&tiles, &[full.clone(), full.clone(), full], 3, w, h);
        assert_eq!(masked.tile.samples, plain.tile.samples);
        assert_eq!(masked.zmap, plain.zmap);
    }

    #[test]
    fn eligible_masks_follow_the_pyramid_dimensions() {
        for (w, h, levels) in [(75u32, 47u32, 3u32), (355, 227, 5)] {
            let covered = vec![true; (w * h) as usize];
            let masks = EligibleMasks::new(&covered, w, h, levels);
            let pyramid = build(&vec![0u16; (w * h) as usize], w, h, 1, levels).unwrap();
            assert_eq!(masks.levels.len(), levels as usize);
            for (mask, plane) in masks.levels.iter().zip(pyramid.levels()) {
                assert_eq!(mask.len(), (plane.width() * plane.height()) as usize);
            }
            let residual = pyramid.residual();
            assert_eq!(
                masks.residual.len(),
                (residual.width() * residual.height()) as usize
            );
        }
    }

    #[test]
    fn score_radius_halves_with_level() {
        // levels = 5: headroom = 96 - 62 = 34, so W = min(8, 34) = 8
        // (unclamped), and r_k = max(1, W >> k) halves each level.
        assert_eq!(
            (0..5u32).map(|k| score_radius(5, k)).collect::<Vec<_>>(),
            vec![8, 4, 2, 1, 1]
        );
        // levels = 1: headroom = margin_for_levels(1) - 2*(2^1 - 1) = 4 - 2
        // = 2, so the clamp binds: W = min(8, 2) = 2.
        assert_eq!(score_radius(1, 0), 2);
        // levels = 2: headroom = margin_for_levels(2) - 2*(2^2 - 1) = 12 - 6
        // = 6, so the clamp binds: W = min(8, 6) = 6, and r_1 = max(1, 3).
        assert_eq!(score_radius(2, 0), 6);
        assert_eq!(score_radius(2, 1), 3);
    }
}
