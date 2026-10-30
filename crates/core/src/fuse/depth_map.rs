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
//! # Coverage
//!
//! The argmax runs over eligible frames: those whose coverage mask
//! ([`crate::algorithm::TileCtx::for_each_frame`]), eroded by the full reach of its score,
//! `DETAIL_REACH + score-radius + smoothing-radius` (`4 + 4 + 8` at the
//! defaults; `sharpness::eligible`), is true at `p`. A pixel with no eligible frame is
//! [`crate::zmap::NO_SOURCE`] with zero samples.
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
//! # Reach and `tile_support`
//!
//! The measure's window reads `score-radius` px; the smoothing pass reads a
//! further `smoothing-radius` px of the *score* plane, which itself needed
//! `crate::fuse::sharpness::DETAIL_REACH` (`4`) to exist at all: total
//! reach `4 + score-radius + smoothing-radius`, `16` at the shipped
//! defaults. [`DepthMap::tile_support`] reports that reach on a one-level
//! (2 px) decimation grid, the only pyramid depth this algorithm builds.

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::fuse::error::{algorithm_error, missing_key};
use crate::fuse::laplacian::score_plane;
use crate::fuse::pyramid::Plane;
use crate::fuse::sharpness::{DETAIL_REACH, detail_plane, eligible as sharpness_eligible};
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::StackRegistration;
use crate::tile::plan::TileSupport;
use crate::tile::{Sample, SampleFormat, Tile};
use crate::zmap::NO_SOURCE;

const SCORE_RADIUS_DEFAULT: i64 = 4;
const SMOOTHING_RADIUS_DEFAULT: i64 = 8;

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "score-radius".to_string(),
            label: "Score Radius".to_string(),
            help: "How far, in pixels, each pixel looks around itself to judge \
                   how sharp it is."
                .to_string(),
            kind: ParamKind::Int { min: 0, max: 16 },
            default: ParamValue::Int(SCORE_RADIUS_DEFAULT),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "smoothing-radius".to_string(),
            label: "Smoothing Radius".to_string(),
            help: "How far, in pixels, the sharpness map is smoothed so that \
                   neighbouring pixels agree on which frame is in focus."
                .to_string(),
            kind: ParamKind::Int { min: 0, max: 16 },
            default: ParamValue::Int(SMOOTHING_RADIUS_DEFAULT),
            advanced: true,
            auto: None,
            host_default: None,
        },
    ]
});

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
        "Focus map"
    }

    fn advanced(&self) -> bool {
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

    /// The output (2 B per sample as `u16`, 4 B as `f32`), the detail
    /// plane's one-level pyramid with its cloned level (9 B per sample at the
    /// peak), and per pixel the `f32` best and `u16` winner (6 B) plus the
    /// per-frame `f32` detail, raw score and smoothed planes with the
    /// eligibility mask and its erosion scratch (14 B).
    fn working_bytes_per_pixel(
        &self,
        _cfg: &ParamValues,
        samples_per_pixel: u32,
        format: SampleFormat,
    ) -> u64 {
        let per_sample = if format == SampleFormat::F32 { 13 } else { 11 };
        per_sample * u64::from(samples_per_pixel) + 20
    }

    /// `4 + score-radius + smoothing-radius` on the one-level decimation
    /// grid of its detail plane.
    fn tile_support(&self, cfg: &ParamValues) -> TileSupport {
        TileSupport {
            reach_px: DETAIL_REACH + Self::score_radius(cfg) + Self::smoothing_radius(cfg),
            decimation_levels: 1,
        }
    }
}

impl DepthMap {
    /// Fuses frames read as `S` into a composite of `O` samples.
    fn fuse_as<S: Sample, O: Sample>(
        &self,
        ctx: &TileCtx<'_>,
        cfg: &ParamValues,
    ) -> Result<TileOutput<O>, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let score_radius =
            cfg.get("score-radius")
                .and_then(ParamValue::as_i64)
                .ok_or_else(|| missing_key("depth-map", "score-radius"))? as u32;
        let smoothing_radius =
            cfg.get("smoothing-radius")
                .and_then(ParamValue::as_i64)
                .ok_or_else(|| missing_key("depth-map", "smoothing-radius"))? as u32;

        let rect = ctx.rect();
        let (width, height) = (rect.width, rect.height);
        let frame_count = ctx.frame_count();
        let spp = ctx.samples_per_pixel()?;
        let spp_usize = spp as usize;
        let pixel_count = width as usize * height as usize;

        let mut best = vec![f32::NEG_INFINITY; pixel_count];
        let mut winner = vec![NO_SOURCE; pixel_count];
        let mut out = vec![O::default(); pixel_count * spp_usize];

        ctx.for_each_frame_as::<S>(|f, samples, covered| {
            let detail =
                detail_plane(samples, width, height, spp).map_err(algorithm_error("depth-map"))?;
            let raw_score = score_plane(&detail, score_radius);
            let smoothed = box_smooth(&raw_score, width, height, smoothing_radius);
            let eligible = covered.map(|covered| {
                sharpness_eligible(
                    covered,
                    width,
                    height,
                    DETAIL_REACH + score_radius + smoothing_radius,
                )
            });

            for p in 0..pixel_count {
                if eligible.as_ref().is_some_and(|e| !e[p]) {
                    continue;
                }
                // Strictly greater: the lowest-indexed frame reaching a
                // score first wins an exact tie, identical to `laplacian`'s
                // tie-break.
                if smoothed[p] > best[p] {
                    best[p] = smoothed[p];
                    winner[p] = f as u16;
                    let base = p * spp_usize;
                    for (out, sample) in out[base..base + spp_usize]
                        .iter_mut()
                        .zip(&samples[base..base + spp_usize])
                    {
                        *out = O::narrow(sample.widen());
                    }
                }
            }
            Ok(())
        })?;

        crate::zmap::validate_tile(&winner, frame_count as u32)
            .map_err(algorithm_error("depth-map"))?;

        Ok(TileOutput {
            tile: Tile {
                rect,
                samples_per_pixel: spp,
                samples: out,
            },
            zmap: winner,
        })
    }
}

inventory::submit! { StackRegistration::new(&DepthMap) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::plan::{DEFAULT_TILE_SIZE, TilePlan, margin_for_levels};
    use crate::tile::{Rect, TileError};

    fn rect(width: u32, height: u32) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn masked_tile(w: u32, h: u32, f: impl Fn(u32, u32) -> u16) -> Tile {
        let samples = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| f(x, y));
        Tile {
            rect: rect(w, h),
            samples_per_pixel: 1,
            samples: samples.collect(),
        }
    }

    fn fuse_masked(tiles: &[Tile], coverage: &[Option<Vec<bool>>], w: u32, h: u32) -> TileOutput {
        let ctx = TileCtx::new(rect(w, h), tiles).with_coverage(coverage);
        let mut cfg = ParamValues::new();
        cfg.insert("score-radius".to_string(), ParamValue::Int(2));
        cfg.insert("smoothing-radius".to_string(), ParamValue::Int(3));
        DepthMap.fuse(&ctx, &cfg).unwrap()
    }

    #[test]
    fn a_masked_frame_wins_only_beyond_its_eroded_band() {
        let (w, h) = (48u32, 16u32);
        let flat = masked_tile(w, h, |_, _| 1_000);
        let checker = masked_tile(w, h, |x, y| if (x + y) % 2 == 0 { 0 } else { 60_000 });
        let uncovered_columns = 8u32;
        let mask: Vec<bool> = (0..h)
            .flat_map(|_| (0..w).map(|x| x >= uncovered_columns))
            .collect();
        let out = fuse_masked(&[flat, checker.clone()], &[None, Some(mask)], w, h);
        let reach = 4 + 2 + 3;
        let first_eligible = uncovered_columns + reach;
        for y in 4..12 {
            for x in 0..w {
                let p = (y * w + x) as usize;
                let expected = if x < first_eligible { 0 } else { 1 };
                assert_eq!(out.zmap[p], expected, "({x}, {y})");
                let source = if expected == 1 {
                    &checker
                } else {
                    &masked_tile(w, h, |_, _| 1_000)
                };
                assert_eq!(
                    out.tile.samples[p], source.samples[p],
                    "composite copies the winner"
                );
            }
        }
    }

    #[test]
    fn a_saturated_plateau_resolves_to_the_lowest_eligible_frame() {
        let (w, h) = (16u32, 16u32);
        let tiles = [
            masked_tile(w, h, |_, _| 65_535),
            masked_tile(w, h, |_, _| 65_535),
            masked_tile(w, h, |_, _| 65_535),
        ];
        let masks = [Some(vec![false; (w * h) as usize]), None, None];
        let out = fuse_masked(&tiles, &masks, w, h);
        assert!(out.zmap.iter().all(|&z| z == 1));
        assert!(out.tile.samples.iter().all(|&s| s == 65_535));
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

    fn declared_int_max(id: &str) -> i64 {
        PARAMS
            .iter()
            .find(|parameter| parameter.id == id)
            .and_then(|parameter| match &parameter.kind {
                ParamKind::Int { max, .. } => Some(*max),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{id} must be declared as an integer parameter"))
    }

    #[test]
    fn shipped_radius_ranges_defaults_and_endpoints_remain_validated() {
        for (id, default) in [
            ("score-radius", SCORE_RADIUS_DEFAULT),
            ("smoothing-radius", SMOOTHING_RADIUS_DEFAULT),
        ] {
            let parameter = PARAMS
                .iter()
                .find(|parameter| parameter.id == id)
                .unwrap_or_else(|| panic!("{id} must be declared"));
            assert!(matches!(
                &parameter.kind,
                ParamKind::Int { min: 0, max: 16 }
            ));
            assert_eq!(parameter.default, ParamValue::Int(default));
        }

        let algo = DepthMap;
        let (width, height, spp) = (64, 64, 3);
        let tiles = vec![tile_of(
            width,
            height,
            spp,
            checkerboard(width, height, spp),
        )];
        let ctx = TileCtx::new(rect(width, height), &tiles);
        for (score_radius, smoothing_radius) in [(0, 8), (4, 0), (16, 8), (4, 16), (16, 16)] {
            algo.fuse(&ctx, &cfg(score_radius, smoothing_radius))
                .unwrap_or_else(|error| {
                    panic!(
                        "score-radius={score_radius} smoothing-radius={smoothing_radius} must fuse: {error}"
                    )
                });
        }
        for (score_radius, smoothing_radius, parameter) in
            [(17, 8, "score-radius"), (4, 17, "smoothing-radius")]
        {
            let error = algo
                .fuse(&ctx, &cfg(score_radius, smoothing_radius))
                .expect_err("one above the bound must be refused");
            assert!(
                error.to_string().contains(&format!(
                    "{parameter}: value 17 is outside the allowed range [0, 16]"
                )),
                "error: {error}"
            );
        }
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
        let max_score_radius = declared_int_max("score-radius");
        let max_smoothing_radius = declared_int_max("smoothing-radius");
        for score_radius in 0..=max_score_radius {
            for smoothing_radius in 0..=max_smoothing_radius {
                let support = algo.tile_support(&cfg(score_radius, smoothing_radius));
                let reach = DETAIL_REACH + score_radius as u32 + smoothing_radius as u32;
                assert_eq!(support.reach_px, reach);
                assert_eq!(
                    support.margin(),
                    reach.next_multiple_of(2).max(margin_for_levels(1)),
                    "score_radius={score_radius} smoothing_radius={smoothing_radius}: \
                     the margin is the reach on the 2 px grid"
                );
                assert!(
                    support.min_tile_size() <= DEFAULT_TILE_SIZE,
                    "score_radius={score_radius} smoothing_radius={smoothing_radius}: \
                     min_tile_size {} > {DEFAULT_TILE_SIZE}",
                    support.min_tile_size()
                );
            }
        }
        let support = algo.tile_support(&cfg(max_score_radius, max_smoothing_radius));
        let minimum = support.min_tile_size();
        assert!(
            TilePlan::new(minimum, minimum, minimum, support).is_ok(),
            "score-radius={max_score_radius} smoothing-radius={max_smoothing_radius}: \
             the reported minimum tile {minimum} must plan"
        );
        assert!(
            matches!(
                TilePlan::new(minimum, minimum, minimum - 1, support),
                Err(TileError::NoInterior { min_tile_size, .. }) if min_tile_size == minimum
            ),
            "score-radius={max_score_radius} smoothing-radius={max_smoothing_radius}: \
             one pixel below the reported minimum {minimum} must be refused"
        );
    }

    #[test]
    fn working_bytes_per_pixel_are_pinned() {
        let cfg = ParamValues::new();
        assert_eq!(
            DepthMap.working_bytes_per_pixel(&cfg, 3, SampleFormat::U16),
            53
        );
        assert_eq!(
            DepthMap.working_bytes_per_pixel(&cfg, 1, SampleFormat::U16),
            31
        );
    }

    #[test]
    fn f32_output_adds_two_bytes_per_sample_to_the_working_bytes() {
        let cfg = ParamValues::new();
        for spp in [1, 3] {
            let u16_bytes = DepthMap.working_bytes_per_pixel(&cfg, spp, SampleFormat::U16);
            let f32_bytes = DepthMap.working_bytes_per_pixel(&cfg, spp, SampleFormat::F32);
            assert_eq!(f32_bytes - u16_bytes, 2 * u64::from(spp));
        }
    }

    #[test]
    fn tile_support_falls_back_to_the_default_radii_when_absent() {
        let algo = DepthMap;
        assert_eq!(
            algo.tile_support(&ParamValues::new()),
            TileSupport {
                reach_px: DETAIL_REACH
                    + SCORE_RADIUS_DEFAULT as u32
                    + SMOOTHING_RADIUS_DEFAULT as u32,
                decimation_levels: 1,
            }
        );
    }
}
