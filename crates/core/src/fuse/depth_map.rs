// SPDX-License-Identifier: GPL-3.0-or-later

//! `depth-map`: a [`StackAlgorithm`] that selects, per pixel, from a
//! **smoothed** sharpness surface -- a per-frame measure, coherent at a
//! larger scale than the raw detail, rather than `laplacian`'s per-pyramid-
//! level selection. Reuses `crate::fuse::laplacian::score_plane` twice:
//! once as the sharpness measure over the level-0 detail plane, and a
//! second time as a box-smoothing pass over the resulting score plane --
//! one windowed-sum implementation in the crate, not two. (`pub(crate)`, so
//! this module's public rustdoc names it in code font rather than as an
//! intra-doc link.)
//!
//! # The measure, the smoothing, and the selection
//!
//! For frame `f` and pixel `p`:
//!
//! ```text
//! detail_f = build(frame_f.samples, w, h, spp, 1).levels()[0]
//! raw_f    = score_plane(detail_f, score-radius)               (crate::fuse::laplacian::score_plane)
//! s_f      = score_plane(raw_f as a one-channel Plane, smoothing-radius)   (box_smooth, this module)
//! winner(p) = argmax_f s_f(p), strictly greater -- lowest index wins an exact tie
//! composite(p) = frames[winner(p)](p)                          (a byte copy, no arithmetic)
//! ```
//!
//! `box_smooth` is `score_plane` applied to a one-channel [`Plane`]
//! wrapping the raw score values -- `Plane` has no `f32` constructor, so it
//! is built the same round-trip way `crate::fuse::sharpness::detail_plane`
//! and `laplacian`'s own tests are: [`Plane::from_u16`] for the right
//! dimensions, then [`Plane::data_mut`] overwritten with the real `f32`
//! values.
//!
//! # The composite *is* the Z-Map
//!
//! Unlike `weighted-average`'s blend, this algorithm's composite is a raw
//! copy of the winning frame's own samples -- no `f32`, no rounding, no
//! `collapse`. `composite[p*spp+c] == frames[zmap[p]][p*spp+c]` is therefore
//! an assertable identity (`tests::the_winning_frame_is_copied_verbatim`,
//! and `fuse_algorithms::depth_map_composite_is_its_zmap` over the fixture),
//! and it is exactly what makes this composite (like `weighted-average`'s
//! blend, for a different reason) bit-exact under tiling: every output
//! sample already sits inside the read rect the margin guarantees, with
//! nothing added on top.
//!
//! # Reach and `tile_levels`
//!
//! The measure's window reads `score-radius` px; the smoothing pass reads a
//! further `smoothing-radius` px of the *score* plane, which itself needed
//! `crate::fuse::sharpness::DETAIL_REACH` (`2`) to exist at all: total
//! reach `2 + score-radius + smoothing-radius`, `14` at the shipped
//! defaults. [`DepthMap::tile_levels`] overrides the trait's default with
//! `crate::fuse::sharpness::levels_for_margin` of that reach -- `3` at the
//! defaults, from a pyramid this algorithm only ever builds to **one**
//! level. The `levels=3` `focusforge-cli stack --algo depth-map` reports is
//! therefore a reach-derived override, not a pyramid depth.

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::fuse::PyramidError;
use crate::fuse::laplacian::score_plane;
use crate::fuse::pyramid::Plane;
use crate::fuse::sharpness::{DETAIL_REACH, detail_plane, levels_for_margin};
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::StackRegistration;
use crate::tile::Tile;

const SCORE_RADIUS_DEFAULT: i64 = 4;
const SMOOTHING_RADIUS_DEFAULT: i64 = 8;

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "score-radius".to_string(),
            label: "Score Radius".to_string(),
            help: "Sharpness-measure window radius, in level-0 pixels -- the \
                   same kind of window `laplacian` and `weighted-average` \
                   use. A detail-scale property, not resolution-derived."
                .to_string(),
            kind: ParamKind::Int { min: 0, max: 16 },
            default: ParamValue::Int(SCORE_RADIUS_DEFAULT),
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "smoothing-radius".to_string(),
            label: "Smoothing Radius".to_string(),
            help: "A second windowed pass, over the sharpness measure \
                   itself rather than over pixel detail, that makes the \
                   depth surface -- and therefore the Z-Map -- coherent at \
                   a larger scale. A detail-scale property, not \
                   resolution-derived."
                .to_string(),
            kind: ParamKind::Int { min: 0, max: 16 },
            default: ParamValue::Int(SMOOTHING_RADIUS_DEFAULT),
            auto: None,
            host_default: None,
        },
    ]
});

fn wrap_pyramid_error(source: PyramidError) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "depth-map".to_string(),
        detail: source.to_string(),
    }
}

fn missing_key(key: &str) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "depth-map".to_string(),
        detail: format!(
            "resolved values are missing `{key}`; call crate::params::resolve before fuse"
        ),
    }
}

/// A second windowed-sum pass over a one-channel score plane -- exactly
/// [`score_plane`], applied to a synthetic one-`samples_per_pixel` [`Plane`]
/// built from the raw score values by the same round-trip
/// [`crate::fuse::sharpness::detail_plane`]'s sibling module and
/// `laplacian`'s own tests use, since `Plane` has no `f32` constructor.
fn box_smooth(scores: &[f32], width: u32, height: u32, radius: u32) -> Vec<f32> {
    let mut plane = Plane::from_u16(&vec![0u16; scores.len()], width, height, 1)
        .expect("scores.len() == width * height by construction");
    plane.data_mut().copy_from_slice(scores);
    score_plane(&plane, radius)
}

/// The Depth Map fusion algorithm. See the module docs for the measure, the
/// smoothing pass, the selection rule and the Z-Map identity.
pub struct DepthMap;

impl DepthMap {
    fn score_radius(cfg: &ParamValues) -> u32 {
        cfg.get("score-radius")
            .and_then(ParamValue::as_i64)
            .unwrap_or(SCORE_RADIUS_DEFAULT)
            .max(0) as u32
    }

    fn smoothing_radius(cfg: &ParamValues) -> u32 {
        cfg.get("smoothing-radius")
            .and_then(ParamValue::as_i64)
            .unwrap_or(SMOOTHING_RADIUS_DEFAULT)
            .max(0) as u32
    }
}

impl StackAlgorithm for DepthMap {
    fn id(&self) -> &str {
        "depth-map"
    }

    fn display_name(&self) -> &str {
        "Depth Map"
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let score_radius = cfg
            .get("score-radius")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("score-radius"))? as u32;
        let smoothing_radius = cfg
            .get("smoothing-radius")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("smoothing-radius"))? as u32;

        let rect = ctx.rect();
        let (width, height) = (rect.width, rect.height);
        let frame_count = ctx.frame_count();
        let frame0 = ctx.frame(0)?;
        let spp = frame0.samples_per_pixel;
        let spp_usize = spp as usize;
        let pixel_count = width as usize * height as usize;

        let mut best = vec![f32::NEG_INFINITY; pixel_count];
        let mut winner = vec![0u16; pixel_count];
        let mut out = vec![0u16; pixel_count * spp_usize];

        for f in 0..frame_count {
            let tile = ctx.frame(f)?;
            let detail =
                detail_plane(&tile.samples, width, height, spp).map_err(wrap_pyramid_error)?;
            let raw_score = score_plane(&detail, score_radius);
            let smoothed = box_smooth(&raw_score, width, height, smoothing_radius);

            for p in 0..pixel_count {
                // Strictly greater: the lowest-indexed frame reaching a
                // score first wins an exact tie, identical to `laplacian`'s
                // tie-break.
                if smoothed[p] > best[p] {
                    best[p] = smoothed[p];
                    winner[p] = f as u16;
                    let base = p * spp_usize;
                    out[base..base + spp_usize]
                        .copy_from_slice(&tile.samples[base..base + spp_usize]);
                }
            }
        }

        crate::zmap::validate_tile(&winner, frame_count as u32).map_err(|source| {
            AlgorithmError::Failed {
                algorithm: "depth-map".to_string(),
                detail: source.to_string(),
            }
        })?;

        Ok(TileOutput {
            tile: Tile {
                rect,
                samples_per_pixel: spp,
                samples: out,
            },
            zmap: winner,
        })
    }

    /// This algorithm's reach is expressible as a level count, but not as
    /// its own level count -- it builds a one-level pyramid per frame yet
    /// must report enough levels for `margin_for_levels` to cover
    /// `2 + score-radius + smoothing-radius`.
    fn tile_levels(&self, cfg: &ParamValues) -> u32 {
        let score_radius = Self::score_radius(cfg);
        let smoothing_radius = Self::smoothing_radius(cfg);
        levels_for_margin(DETAIL_REACH + score_radius + smoothing_radius)
    }
}

inventory::submit! { StackRegistration::new(&DepthMap) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::Rect;
    use crate::tile::plan::{DEFAULT_TILE_SIZE, margin_for_levels, min_tile_size_for_levels};

    fn rect(width: u32, height: u32) -> Rect {
        Rect {
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

    fn cfg(score_radius: i64, smoothing_radius: i64) -> ParamValues {
        let mut cfg = ParamValues::new();
        cfg.insert("score-radius".to_string(), ParamValue::Int(score_radius));
        cfg.insert(
            "smoothing-radius".to_string(),
            ParamValue::Int(smoothing_radius),
        );
        cfg
    }

    #[test]
    fn every_parameter_the_implementation_reads_is_declared() {
        let algo = DepthMap;
        let (w, h, spp) = (8u32, 8u32, 3u32);
        let tiles = vec![tile_of(w, h, spp, flat(w, h, spp, 100))];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let err = algo.fuse(&ctx, &ParamValues::new()).unwrap_err();
        match err {
            AlgorithmError::Failed { algorithm, detail } => {
                assert_eq!(algorithm, "depth-map");
                assert!(detail.contains("score-radius"));
            }
            other => {
                panic!("expected AlgorithmError::Failed naming `score-radius`, got {other:?}")
            }
        }
    }

    #[test]
    fn a_single_frame_stack_is_copied_bit_identically_and_wins_everywhere() {
        let algo = DepthMap;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let content = checkerboard(w, h, spp);
        let tiles = vec![tile_of(w, h, spp, content.clone())];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg(4, 8)).unwrap();
        assert!(out.zmap.iter().all(|&index| index == 0));
        assert_eq!(
            out.tile.samples, content,
            "a one-frame stack's composite must be a bit-identical copy"
        );
    }

    #[test]
    fn identical_frames_tie_break_to_the_lowest_index() {
        let algo = DepthMap;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let content = checkerboard(w, h, spp);
        let tiles = vec![
            tile_of(w, h, spp, content.clone()),
            tile_of(w, h, spp, content.clone()),
            tile_of(w, h, spp, content),
        ];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg(4, 8)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 0),
            "identical frames must tie-break to frame 0 everywhere"
        );
    }

    #[test]
    fn the_winning_frame_is_copied_verbatim() {
        let algo = DepthMap;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let flat_a = flat(w, h, spp, 30000);
        let sharp = checkerboard(w, h, spp);
        let flat_c = flat(w, h, spp, 30000);
        let tiles = vec![
            tile_of(w, h, spp, flat_a),
            tile_of(w, h, spp, sharp.clone()),
            tile_of(w, h, spp, flat_c),
        ];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg(4, 8)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 1),
            "the only frame with detail must win every pixel"
        );
        assert_eq!(
            out.tile.samples, sharp,
            "the composite must be a byte copy of the winning frame's own samples, no rounding"
        );
    }

    #[test]
    fn box_smooth_never_decreases_the_score() {
        // Every weight in `score_plane`'s window is 1 (a plain sum), and
        // every raw score is non-negative, so smoothing over any radius >= 0
        // can only add non-negative terms on top of the pixel's own value.
        let (w, h) = (12u32, 12u32);
        let raw: Vec<f32> = (0..(w * h)).map(|i| (i % 7) as f32).collect();
        for &radius in &[0u32, 3, 8] {
            let smoothed = box_smooth(&raw, w, h, radius);
            for (i, (&s, &r)) in smoothed.iter().zip(raw.iter()).enumerate() {
                assert!(
                    s >= r,
                    "box_smooth must never decrease a pixel's own score: radius={radius} \
                     index={i} raw={r} smoothed={s}"
                );
            }
        }
    }

    #[test]
    fn declared_ranges_fit_the_shipped_geometry() {
        let algo = DepthMap;
        for score_radius in 0..=16i64 {
            for smoothing_radius in 0..=16i64 {
                let cfg = cfg(score_radius, smoothing_radius);
                let levels = algo.tile_levels(&cfg);
                let reach = DETAIL_REACH + score_radius as u32 + smoothing_radius as u32;
                assert!(
                    margin_for_levels(levels) >= reach,
                    "score_radius={score_radius} smoothing_radius={smoothing_radius}: \
                     margin_for_levels({levels}) = {} < reach {reach}",
                    margin_for_levels(levels)
                );
                assert!(
                    min_tile_size_for_levels(levels) <= DEFAULT_TILE_SIZE,
                    "score_radius={score_radius} smoothing_radius={smoothing_radius}: \
                     min_tile_size_for_levels({levels}) = {} > {DEFAULT_TILE_SIZE}",
                    min_tile_size_for_levels(levels)
                );
            }
        }
    }

    #[test]
    fn tile_levels_falls_back_to_the_default_radii_when_absent() {
        let algo = DepthMap;
        assert_eq!(
            algo.tile_levels(&ParamValues::new()),
            levels_for_margin(
                DETAIL_REACH + SCORE_RADIUS_DEFAULT as u32 + SMOOTHING_RADIUS_DEFAULT as u32
            )
        );
    }
}
