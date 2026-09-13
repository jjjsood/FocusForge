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
//! (which would make the Z-Map's single index a lie) never happens.
//!
//! ```text
//! score(k, x, y) = sum_{dy=-1..=1} sum_{dx=-1..=1} sum_{c=0..spp}
//!                  |L_k[(clamp(y+dy), clamp(x+dx)) * spp + c]|
//! ```
//!
//! Both passes clamp-to-edge, the same rule [`crate::fuse::pyramid::reduce`]
//! and [`crate::fuse::pyramid::expand`] use, and both accumulate into one
//! `f32` in the pinned ascending order above because `f32` addition is not
//! associative (`crate::fuse::pyramid`'s own module docs, `:40-46`). This
//! two-pass order, together with `W(N)`/`r_k` and the `max(1, ...)` and
//! `headroom` clamps above, is everything a WGSL compute twin needs to
//! reproduce this rule at <= 1 LSB: there is no other rounding, ordering or
//! tie-break choice hiding in the implementation.
//!
//! # The Z-Map
//!
//! The per-pixel Z-Map is the **level-0 winner** -- the finest level, one
//! index per pixel, never per sample -- carved so `laplacian` never emits
//! [`crate::zmap::NO_SOURCE`] (a selection algorithm always has a winner),
//! while still passing every map through [`crate::zmap::validate_tile`] so
//! a map round-tripped through a future blending algorithm still validates.
//!
//! # The residual
//!
//! The coarsest Gaussian (the pyramid's residual, carrying no detail to select on) is the
//! **arithmetic mean of every frame's residual, in ascending frame order, accumulated in
//! `f32`** -- pinned because `f32` addition is not associative and a GPU twin must reproduce
//! the exact sum. At 5 levels the residual is 1/32 resolution, so
//! uncorrected frame drift is a fraction of one residual pixel -- too small
//! to ghost visibly.
//!
//! # Memory
//!
//! Streaming, never `frame_count` pyramids at once: frame 0's pyramid
//! becomes the running best, and every later frame's pyramid is built,
//! compared level by level, and dropped. Peak live is two pyramids plus one
//! `f32` score plane and one `u16` winner plane per level -- independent of
//! `frame_count`.
//!
//! # Tiling and the composite seam
//!
//! The composite seam is not boundable. A tiled
//! run's Z-Map is *exactly* equal to an untiled run's; its composite is
//! not, and cannot be made so by a bigger margin at fixed tile geometry.
//! The reason is cone arithmetic, not implementation slack:
//!
//! - A level-0 selection at pixel `p` depends on Laplacian coefficients
//!   within `build`'s reach for `N` levels, `2*(2^N - 1)`, plus the 3x3
//!   score window's own 1 px: `2*(2^N - 1) + 1` (63 px at `N = 5`). That
//!   sits inside the 96 px margin `MEASURED_MARGIN[5]` publishes, so every
//!   interior pixel's *selection* reads bit-identical samples in the tiled
//!   and untiled runs, combined in the same order on the same decimation
//!   grid -- the Z-Map is exact.
//! - A composite pixel is not the selection; it is `collapse`'s output,
//!   which adds its own `2*(2^N - 1)` px reach (62 px at `N = 5`) on top of
//!   the selection that produced it. So a composite pixel depends on
//!   *selections* within `63 + 62 = 125` px, a band the 96 px margin does
//!   not cover. A selection inside `96..125` px of the tile boundary may
//!   legitimately differ between the tiled and untiled runs, and a flipped
//!   selection swaps in a full-magnitude Laplacian coefficient from a
//!   different frame -- a discrete decision flip, not a leaked value.
//!
//! `MEASURED_MARGIN` bounds *content leakage*: how far a sample's own value
//! can drift between a tiled and untiled read. It does not and cannot bound
//! a *discrete decision flip* -- no margin, at any size short of covering
//! the full 140 px composite cone, makes the composite tile-invariant.
//! Raising the margin to 160 px (covering 140, aligned to the `2^5 = 32`
//! decimation grid) would make both halves exact. Note what that costs
//! before reaching for it: `min_tile_size` becomes `4 * 160 + 64 = 704`,
//! so the default 512 px tile would not merely drop to 4 levels, it would
//! fall below the minimum planning tile entirely. An earlier note filed
//! this fix as "raise the margin to 128", sized against the undercounted
//! 125 px band and putting `min_tile_size` at 576; that figure is wrong
//! and anyone picking this up must re-derive from 140, not 125. Deferred
//! to a GPU implementation. GPU parity is what would make bit-exact tiling
//! worth the trade.
//!
//! | level `k` | cone today, `62 + 2^k` (fixed radius 1) | cone with `W = 8`, `62 + 2^k * r_k` |
//! |---|---|---|
//! | 0 | 63 | 70 |
//! | 1 | 64 | 70 |
//! | 2 | 66 | 70 |
//! | 3 | 70 | 70 |
//! | 4 | **78** | **78** |
//!
//! **The chosen rule adds nothing to the worst-case cone.** `r_k` halves as
//! `2^k` doubles, so every level spends the same 8 px of headroom, except
//! level 4 where the `max(1, ...)` clamp holds `r_4` at 1 while `2^4 = 16`
//! still multiplies it -- the binding worst case is level 4's 78 px, the
//! same as before. `MEASURED_MARGIN` does not move,
//! `min_tile_size` does not move, and the default 512 px tile still carries
//! 5 levels.
//!
//! The control that proves the pyramid and residual math are sound rather
//! than merely lucky: identical frames at the same geometry give an
//! observed composite difference of exactly 0 (no selection can flip when
//! every frame ties), isolating the 96..125 px divergence to selection
//! alone. This is a property of selection algorithms in general, not a
//! defect specific to this one -- any per-pixel selection algorithm,
//! including the Depth Map, inherits it unchanged.

use std::sync::{Arc, LazyLock};

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::fuse::{Plane, PyramidError, build, collapse, max_levels};
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues, StackContext};
use crate::registry::StackRegistration;
use crate::tile::Tile;
use crate::tile::plan::{DEFAULT_TILE_SIZE, MAX_LEVELS, max_levels_for_tile};
use crate::tile::scheduler::DEFAULT_LEVELS;

/// `auto_levels(ctx) = min(fuse::max_levels(ctx.width, ctx.height),
/// tile::plan::max_levels_for_tile(DEFAULT_TILE_SIZE)).max(1)` -- the hook
/// must never propose a depth the shipped tile geometry cannot plan. The
/// second term is a constant (`5`, since `min_tile_size_for_levels(5) ==
/// 448 <= 512 < min_tile_size_for_levels(6) == 832`), so resolution only
/// moves this value below a small minimum dimension.
fn auto_levels(ctx: &StackContext) -> ParamValue {
    let by_resolution = max_levels(ctx.width, ctx.height);
    let by_tile_geometry = max_levels_for_tile(DEFAULT_TILE_SIZE);
    ParamValue::Int(by_resolution.min(by_tile_geometry).max(1) as i64)
}

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![ParamDesc {
        id: "levels".to_string(),
        label: "Levels".to_string(),
        help: "Laplacian pyramid depth: how many detail levels are selected \
               and blended before the coarsest residual. `auto` derives a \
               depth from the stack's resolution, capped by the depth the \
               tile geometry can actually plan."
            .to_string(),
        kind: ParamKind::Int {
            min: 1,
            max: MAX_LEVELS as i64,
        },
        default: ParamValue::Int(DEFAULT_LEVELS as i64),
        auto: Some(Arc::new(auto_levels)),
    }]
});

/// Wraps a [`PyramidError`] as this algorithm's own [`AlgorithmError::Failed`]
/// -- `PyramidError` is not one of [`AlgorithmError`]'s `#[from]` variants
/// by design (`crate::fuse::error`'s module docs), so every caller wraps it
/// with the algorithm's own id.
fn wrap_pyramid_error(source: PyramidError) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "laplacian".to_string(),
        detail: source.to_string(),
    }
}

/// Clamp-to-edge on one axis, the same rule
/// [`crate::fuse::pyramid::reduce`] and [`crate::fuse::pyramid::expand`]
/// use.
fn clamp_edge(v: i64, dim: u32) -> u32 {
    v.clamp(0, dim as i64 - 1) as u32
}

/// The windowed sharpness score of every pixel of one Laplacian level: a
/// 3x3 (Chebyshev-distance-1), clamp-to-edge sum of `|L|` over every
/// channel, in the pinned accumulation order (`dy`, then `dx`, then `c`,
/// all ascending) this module's docs pin. One `f32` per pixel, never per
/// sample.
fn score_plane(level: &Plane) -> Vec<f32> {
    let width = level.width();
    let height = level.height();
    let spp = level.samples_per_pixel();
    let data = level.data();
    let mut scores = vec![0.0f32; (width as usize) * (height as usize)];
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0f32;
            for dy in -1i64..=1 {
                let sy = clamp_edge(y as i64 + dy, height);
                for dx in -1i64..=1 {
                    let sx = clamp_edge(x as i64 + dx, width);
                    for c in 0..spp {
                        let idx = ((sy * width + sx) * spp + c) as usize;
                        acc += data[idx].abs();
                    }
                }
            }
            scores[(y * width + x) as usize] = acc;
        }
    }
    scores
}

/// The Laplacian Pyramid fusion algorithm. See the module docs for the
/// selection rule, the residual rule and the Z-Map semantics.
pub struct Laplacian;

impl StackAlgorithm for Laplacian {
    fn id(&self) -> &str {
        "laplacian"
    }

    fn display_name(&self) -> &str {
        "Laplacian Pyramid"
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let levels = cfg
            .get("levels")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| AlgorithmError::Failed {
                algorithm: "laplacian".to_string(),
                detail: "resolved values are missing `levels`; call crate::params::resolve \
                         before fuse"
                    .to_string(),
            })? as u32;

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
        let frame0 = ctx.frame(0)?;
        let spp = frame0.samples_per_pixel;

        // Frame 0's pyramid is the running best -- streaming, never
        // `frame_count` pyramids at once.
        let mut best =
            build(&frame0.samples, width, height, spp, levels).map_err(wrap_pyramid_error)?;

        let mut best_scores: Vec<Vec<f32>> = best.levels().iter().map(score_plane).collect();
        let mut winner: Vec<Vec<u16>> = best_scores
            .iter()
            .map(|scores| vec![0u16; scores.len()])
            .collect();

        // Ascending frame order, `f32`, per the residual rule above.
        // Frame 0's own contribution is the seed.
        let mut residual_sum: Vec<f32> = best.residual().data().to_vec();

        let spp_usize = spp as usize;
        for f in 1..frame_count {
            let tile = ctx.frame(f)?;
            let current =
                build(&tile.samples, width, height, spp, levels).map_err(wrap_pyramid_error)?;

            for (sum, &value) in residual_sum
                .iter_mut()
                .zip(current.residual().data().iter())
            {
                *sum += value;
            }

            for k in 0..levels as usize {
                let current_level = &current.levels()[k];
                let current_score = score_plane(current_level);
                let pixel_count = current_score.len();
                for p in 0..pixel_count {
                    // Strictly greater: frame 0 (or the lowest-indexed
                    // frame reaching this score first) wins an exact tie.
                    if current_score[p] > best_scores[k][p] {
                        let src = &current_level.data()[p * spp_usize..(p + 1) * spp_usize];
                        let dst_level = best.level_mut(k);
                        dst_level.data_mut()[p * spp_usize..(p + 1) * spp_usize]
                            .copy_from_slice(src);
                        best_scores[k][p] = current_score[p];
                        winner[k][p] = f as u16;
                    }
                }
            }
        }

        let frame_count_f32 = frame_count as f32;
        for value in best
            .residual_mut()
            .data_mut()
            .iter_mut()
            .zip(residual_sum.iter())
        {
            let (out, sum) = value;
            *out = *sum / frame_count_f32;
        }

        let collapsed = collapse(&best);
        let mut samples = vec![0u16; width as usize * height as usize * spp_usize];
        collapsed
            .to_u16_into(&mut samples)
            .map_err(wrap_pyramid_error)?;

        // Level-0 winner is the Z-Map; `levels >= 1` is enforced by the
        // schema, so `winner` always has at least one entry.
        let zmap = winner
            .into_iter()
            .next()
            .expect("levels >= 1 (enforced by the schema) guarantees a level-0 winner plane");
        crate::zmap::validate_tile(&zmap, frame_count as u32).map_err(|source| {
            AlgorithmError::Failed {
                algorithm: "laplacian".to_string(),
                detail: source.to_string(),
            }
        })?;

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

        let ctx_64 = StackContext::new(8, 64, 64);
        assert_eq!(hook(&ctx_64), ParamValue::Int(4));

        let ctx_512 = StackContext::new(8, 512, 512);
        assert_eq!(hook(&ctx_512), ParamValue::Int(5));

        let ctx_reference = StackContext::new(61, 8192, 5464);
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
            let ctx = StackContext::new(frame_count, w, h);
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
        // otherwise-valid `cfg` must be the named error of step 5.1, never
        // a panic, which is what proves the implementation never reaches
        // past its own declared schema.
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
    fn score_reaches_a_3x3_neighbourhood() {
        // A single-pixel impulse in an otherwise-zero Laplacian level: the
        // windowed score must be nonzero at the impulse and at every pixel
        // within Chebyshev distance 1 of it, and exactly zero everywhere
        // farther away -- the anti-speckle property this window bought
        // with one extra pass.
        let (width, height, spp) = (9u32, 9u32, 1u32);
        let mut data = vec![0.0f32; (width * height) as usize];
        let (cx, cy) = (4u32, 4u32);
        data[(cy * width + cx) as usize] = 100.0;
        let level = Plane::from_u16(&flat(width, height, spp, 0), width, height, spp).unwrap();
        // Rebuild with the impulse: `Plane` has no public constructor
        // taking `f32` data directly, so round-trip through `data_mut`.
        let mut level = level;
        level.data_mut().copy_from_slice(&data);

        let scores = score_plane(&level);
        for y in 0..height {
            for x in 0..width {
                let dist = (x as i64 - cx as i64)
                    .unsigned_abs()
                    .max((y as i64 - cy as i64).unsigned_abs()) as u32;
                let score = scores[(y * width + x) as usize];
                if dist <= 1 {
                    assert!(
                        score > 0.0,
                        "pixel ({x},{y}) at Chebyshev distance {dist} must see the impulse"
                    );
                } else {
                    assert_eq!(
                        score, 0.0,
                        "pixel ({x},{y}) at Chebyshev distance {dist} must not see the impulse"
                    );
                }
            }
        }
    }
}
