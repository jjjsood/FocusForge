// SPDX-License-Identifier: GPL-3.0-or-later

//! `weighted-average`: a [`StackAlgorithm`] that blends every frame at every
//! pixel by a windowed sharpness weight, rather than selecting a single
//! winner. Reuses `crate::fuse::laplacian::score_plane` (the same
//! windowed, clamp-to-edge, separable-two-pass sharpness measure
//! `laplacian` uses) over `crate::fuse::sharpness::detail_plane`'s
//! level-0 Laplacian, so the three built-in algorithms are comparable on
//! the same fixture with the same measure. (Both are `pub(crate)`, so this
//! module's public rustdoc names them in code font rather than as an
//! intra-doc link.)
//!
//! # The weight and the blend
//!
//! For frame `f` and pixel `p`:
//!
//! ```text
//! detail_f = build(frame_f.samples, w, h, spp, 1).levels()[0]
//! score_f  = score_plane(detail_f, radius)          (crate::fuse::laplacian::score_plane)
//! w_f(p)   = score_f(p) ^ weight-exponent            (f64 powf, see below)
//! out(p)   = sum_f w_f(p) * frame_f(p)  /  sum_f w_f(p)     (den(p) > 0)
//!          = mean_f frame_f(p)                              (den(p) == 0, see below)
//! ```
//!
//! # Precision, and why it is `f64` here
//!
//! The score is a windowed sum of Laplacian magnitudes, so on high-contrast
//! input it reaches the tens of thousands; raised to the declared maximum
//! `weight-exponent` of 8.0, an `f32` weight overflows to infinity once the
//! score passes roughly 63,000 (`f32::MAX` is ~3.4e38). Infinity then
//! reaches the normalisation as `inf / inf`, which is `NaN`, and `NaN`
//! rounds to 0 -- a silently black pixel at the sharpest place in the
//! image, exactly where the algorithm should be most confident.
//!
//! A twin reproduces this by widening and narrowing in the same two places:
//!
//! - **Widen at the weight.** `score` is read as `f64` and `powf` is taken
//!   in `f64`. Both accumulators -- the weighted numerator and the plain
//!   sample sum kept for the degenerate case -- and the denominator are
//!   `f64`, as is the running best used for the tie-break.
//! - **Narrow once, at the finished ratio.** `num / den` (or the plain mean)
//!   is cast to `f32` per sample and written into a [`Plane`]. Nothing else
//!   narrows.
//!
//! Both accumulations run in **ascending frame order**, pinned like every
//! other accumulation in this crate: floating-point addition is not
//! associative, and the order is part of the specification rather than an
//! implementation detail.
//!
//! ## The optimisation that must not be taken
//!
//! The obvious way to keep the weights small is to normalise the scores by
//! the tile's maximum before exponentiating. **Do not.** It would make every
//! weight a function of the tile's contents, so a pixel's blend would depend
//! on which tile it was computed in -- destroying the property this
//! algorithm has and `laplacian` does not: its composite is bit-identical
//! between a tiled and an untiled run, asserted with no tolerance in
//! `tests/fuse_algorithms.rs`. Widening the accumulator is per-pixel and
//! tile-independent; normalising by a tile statistic is not. The final `f32` blend is written into a [`Plane`] and
//! converted with [`Plane::to_u16_into`] (`floor(clamp(v, 0, 65535) + 0.5)`)
//! -- one rounding rule in the project, never an open-coded cast.
//!
//! # The degenerate rule
//!
//! Where every frame's weight at a pixel is exactly `0.0` (a perfectly flat
//! neighbourhood in every frame --
//! reachable, and exercised by
//! `tests::flat_stack_falls_back_to_the_plain_mean`), the blend is the plain
//! arithmetic mean over the eligible frames in ascending order, and the
//! Z-Map index is the lowest eligible frame (`0` with no coverage masks).
//! Never a division by zero -- `weight_f(p) >= 0.0` for every `f`, so
//! `den(p) == 0.0` can only mean every term was `0.0`, checked exactly.
//!
//! A frame is eligible at `p` when its coverage mask
//! ([`crate::algorithm::TileCtx::for_each_frame`]), eroded by the full reach of its
//! weight, `DETAIL_REACH + radius` (`sharpness::eligible`), is true there. An
//! ineligible frame adds nothing; a pixel with no eligible frame is
//! [`crate::zmap::NO_SOURCE`] with zero samples.
//!
//! # The Z-Map
//!
//! **The composite is a blend; the Z-Map is not the blend's provenance in
//! full.** It records the pixel's **dominant contributor**: `argmax_f
//! weight_f(p)` over the eligible frames, **strictly greater** so the
//! **lowest index wins an exact tie** -- identical to `laplacian`'s tie-break.
//! Reading each pixel from the frame the Z-Map names recovers exactly one
//! frame's pixels, never the blend the user actually saw, so this Z-Map is
//! *not* sufficient to reconstruct this algorithm's own composite -- the
//! one place in the project where that gap is real rather than
//! theoretical.
//!
//! # Reach and `tile_support`
//!
//! A windowed weight of radius `r` reads `r` px of its own neighbourhood
//! plus `crate::fuse::sharpness::DETAIL_REACH` (`4`) for the one-level
//! reduce-and-expand underneath it: `4 + r`. [`WeightedAverage::tile_support`]
//! reports that reach on a one-level (2 px) decimation grid, overriding the
//! trait's default (no reach, since this schema declares no key named
//! `"levels"`): the planned margin is `4 + radius` rounded up to the grid.
//!
//! # Tiling
//!
//! Neither a pyramid nor `collapse` survives past the per-pixel blend --
//! every output sample is a weighted combination of samples the read rect
//! already holds, all within `4 + radius` px of `p`. That reach sits inside
//! the planned margin, so the composite **and** the Z-Map are bit-exact
//! between a tiled and an untiled run; `laplacian` reaches the same
//! guarantee with a margin that also covers `collapse` (see
//! `crate::fuse::laplacian`'s "Tiling" section).

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::fuse::error::{algorithm_error, missing_key};
use crate::fuse::laplacian::score_plane;
use crate::fuse::pyramid::Plane;
use crate::fuse::sharpness::{DETAIL_REACH, detail_plane, eligible as sharpness_eligible};
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::StackRegistration;
use crate::tile::Tile;
use crate::tile::plan::TileSupport;
use crate::zmap::NO_SOURCE;

/// `16`. A defocused frame's spread edge energy reaches past a narrow window,
/// so a wide window keeps the sharp edge's score dominant. With exponent 4.0,
/// 16 px against 8 px takes the dark-side brightening next to strong edges on
/// two aligned 1024 x 1024 crops (counts against a per-pixel sharpest-frame
/// pick) from 147.6 / -81.1 to -62.8 / -193.0; at exponent 2.0 it alone loses
/// hair detail (retention 0.8108 against 0.8644).
const RADIUS_DEFAULT: i64 = 16;

/// `4.0`. With the 16 px window it takes the dark-side brightening next to
/// strong edges on the two crops from 729.9 / 250.6 (radius 8, exponent 2.0)
/// to -62.8 / -193.0 counts and raises hair retention from 0.8644 to 0.9163.
/// It stays a blend: hair retention and glint core (0.9611) stay below
/// Details (0.9927, 0.9985) by more than 0.07 and 0.03, where exponent 6.0 at
/// radius 8 comes within 0.015 on hair retention.
const WEIGHT_EXPONENT_DEFAULT: f64 = 4.0;

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "radius".to_string(),
            label: "Radius".to_string(),
            help: "How far, in pixels, each pixel looks around itself to judge \
                   how sharp it is."
                .to_string(),
            kind: ParamKind::Int { min: 0, max: 16 },
            default: ParamValue::Int(RADIUS_DEFAULT),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "weight-exponent".to_string(),
            label: "Sharp-source preference".to_string(),
            help: "How strongly the sharpest frame is preferred where frames \
                   overlap: low values blend smoothly, high values lean \
                   toward picking the sharpest source."
                .to_string(),
            kind: ParamKind::Float {
                min: 1.0,
                max: 8.0,
                step: 0.1,
                logarithmic: false,
            },
            default: ParamValue::Float(WEIGHT_EXPONENT_DEFAULT),
            advanced: false,
            auto: None,
            host_default: None,
        },
    ]
});

/// The Weighted Average fusion algorithm. See the module docs for the
/// weight, the blend, the degenerate rule and the Z-Map semantics.
pub struct WeightedAverage;

impl WeightedAverage {
    /// `radius`, read out of `cfg` with the descriptor's own default as the
    /// fallback -- `tile_support` has no `Result`, so it cannot report a
    /// missing key the way `fuse` does.
    fn radius(cfg: &ParamValues) -> u32 {
        cfg.get("radius")
            .and_then(ParamValue::as_i64)
            .unwrap_or(RADIUS_DEFAULT)
            .max(0) as u32
    }
}

impl StackAlgorithm for WeightedAverage {
    fn id(&self) -> &str {
        "weighted-average"
    }

    fn display_name(&self) -> &str {
        "Soft transitions"
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let radius = cfg
            .get("radius")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("weighted-average", "radius"))? as u32;
        let weight_exponent = cfg
            .get("weight-exponent")
            .and_then(ParamValue::as_f64)
            .ok_or_else(|| missing_key("weighted-average", "weight-exponent"))?
            as f32;

        let rect = ctx.rect();
        let (width, height) = (rect.width, rect.height);
        let frame_count = ctx.frame_count();
        let spp = ctx.samples_per_pixel()?;
        let spp_usize = spp as usize;
        let pixel_count = width as usize * height as usize;

        // Streaming: only the per-pixel accumulators below survive across
        // frames. Each frame's detail plane and score plane are built,
        // folded into the accumulators and dropped -- peak live memory is
        // independent of `frame_count`.
        let mut num = vec![0.0f64; pixel_count * spp_usize];
        let mut sample_sum = vec![0.0f64; pixel_count * spp_usize];
        let mut den = vec![0.0f64; pixel_count];
        let mut best = vec![f64::NEG_INFINITY; pixel_count];
        let mut winner = vec![NO_SOURCE; pixel_count];
        let mut eligible_frames = vec![0u32; pixel_count];

        ctx.for_each_frame(|f, tile, covered| {
            let detail = detail_plane(&tile.samples, width, height, spp)
                .map_err(algorithm_error("weighted-average"))?;
            let scores = score_plane(&detail, radius);
            let eligible = covered
                .map(|covered| sharpness_eligible(covered, width, height, DETAIL_REACH + radius));

            for p in 0..pixel_count {
                if eligible.as_ref().is_some_and(|e| !e[p]) {
                    continue;
                }
                eligible_frames[p] += 1;
                let w = f64::from(scores[p]).powf(f64::from(weight_exponent));
                let base = p * spp_usize;
                for c in 0..spp_usize {
                    let sample = f64::from(tile.samples[base + c]);
                    num[base + c] += w * sample;
                    sample_sum[base + c] += sample;
                }
                den[p] += w;
                // Strictly greater: the lowest-indexed frame reaching a
                // weight first wins an exact tie, identical to `laplacian`'s
                // tie-break.
                if w > best[p] {
                    best[p] = w;
                    winner[p] = f as u16;
                }
            }
            Ok(())
        })?;

        let mut blended = vec![0.0f32; pixel_count * spp_usize];
        for p in 0..pixel_count {
            let base = p * spp_usize;
            if eligible_frames[p] == 0 {
                // No frame has real samples in reach: zero samples, `NO_SOURCE`.
                continue;
            }
            if den[p] > 0.0 {
                for c in 0..spp_usize {
                    // The one narrowing: every weight and every sum above is
                    // `f64`, and only the finished ratio becomes `f32`.
                    blended[base + c] = (num[base + c] / den[p]) as f32;
                }
            } else {
                // Degenerate rule: every eligible frame's weight was exactly
                // zero at this pixel. Fall back to the plain arithmetic mean
                // over the eligible frames; `winner` already holds the lowest
                // eligible index, since a first weight of `0.0` beats the
                // initial `-inf`. Never a division by zero.
                let eligible_count = f64::from(eligible_frames[p]);
                for c in 0..spp_usize {
                    blended[base + c] = (sample_sum[base + c] / eligible_count) as f32;
                }
            }
        }

        // One rounding rule in the project: round-trip through `Plane` so
        // the pinned `floor(clamp(v, 0, 65535) + 0.5)` expression is the
        // only place a `f32` sample becomes a `u16` one.
        let mut plane = Plane::from_u16(&vec![0u16; pixel_count * spp_usize], width, height, spp)
            .map_err(algorithm_error("weighted-average"))?;
        plane.data_mut().copy_from_slice(&blended);
        let mut samples = vec![0u16; pixel_count * spp_usize];
        plane
            .to_u16_into(&mut samples)
            .map_err(algorithm_error("weighted-average"))?;

        crate::zmap::validate_tile(&winner, frame_count as u32)
            .map_err(algorithm_error("weighted-average"))?;

        Ok(TileOutput {
            tile: Tile {
                rect,
                samples_per_pixel: spp,
                samples,
            },
            zmap: winner,
        })
    }

    /// Two `f64` sums per sample, and per pixel the `f64` weight and best,
    /// the winner and the eligible count (22 B); then, live with the sums,
    /// the blended `f32` plane, its `Plane` copy, the zero-filled `u16`
    /// seed and the `u16` output (12 B per sample).
    fn working_bytes_per_pixel(&self, _cfg: &ParamValues, samples_per_pixel: u32) -> u64 {
        28 * u64::from(samples_per_pixel) + 22
    }

    /// The windowed weight's reach (`crate::fuse::sharpness::DETAIL_REACH +
    /// radius`) on the one-level decimation grid of its detail plane.
    fn tile_support(&self, cfg: &ParamValues) -> TileSupport {
        TileSupport {
            reach_px: DETAIL_REACH + Self::radius(cfg),
            decimation_levels: 1,
        }
    }
}

inventory::submit! { StackRegistration::new(&WeightedAverage) }

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

    fn masked_tile(w: u32, h: u32, spp: u32, f: impl Fn(u32, u32, u32) -> u16) -> Tile {
        let mut samples = Vec::new();
        for y in 0..h {
            for x in 0..w {
                for c in 0..spp {
                    samples.push(f(x, y, c));
                }
            }
        }
        Tile {
            rect: rect(w, h),
            samples_per_pixel: spp,
            samples,
        }
    }

    fn fuse_masked(tiles: &[Tile], coverage: &[Option<Vec<bool>>], w: u32, h: u32) -> TileOutput {
        let ctx = TileCtx::new(rect(w, h), tiles).with_coverage(coverage);
        let mut cfg = ParamValues::new();
        cfg.insert("radius".to_string(), ParamValue::Int(2));
        cfg.insert("weight-exponent".to_string(), ParamValue::Float(2.0));
        WeightedAverage.fuse(&ctx, &cfg).unwrap()
    }

    fn texture(seed: u32) -> impl Fn(u32, u32, u32) -> u16 {
        move |x, y, c| ((x * 7 + y * 13 + c * 5 + seed).wrapping_mul(2654435761u32) >> 16) as u16
    }

    #[test]
    fn a_masked_frame_contributes_nothing() {
        let (w, h) = (24u32, 20u32);
        let all = [
            masked_tile(w, h, 3, texture(1)),
            masked_tile(w, h, 3, texture(2)),
            masked_tile(w, h, 3, texture(3)),
        ];
        let masks = [None, Some(vec![false; (w * h) as usize]), None];
        let masked = fuse_masked(&all, &masks, w, h);
        let without = fuse_masked(&[all[0].clone(), all[2].clone()], &[], w, h);
        assert_eq!(masked.tile.samples, without.tile.samples);
        assert!(
            masked.zmap.iter().all(|&z| z == 0 || z == 2),
            "never the masked frame"
        );
    }

    #[test]
    fn flat_pixels_resolve_to_the_lowest_eligible_frame_and_the_eligible_mean() {
        let (w, h) = (16u32, 16u32);
        for spp in [1u32, 3] {
            let tiles: Vec<Tile> = [100u16, 200, 400]
                .iter()
                .map(|&v| masked_tile(w, h, spp, move |_, _, _| v))
                .collect();
            let masks = [Some(vec![false; (w * h) as usize]), None, None];
            let out = fuse_masked(&tiles, &masks, w, h);
            assert!(out.zmap.iter().all(|&z| z == 1), "spp {spp}");
            assert!(out.tile.samples.iter().all(|&s| s == 300), "spp {spp}");
        }
    }

    #[test]
    fn a_masked_frame_is_eligible_only_beyond_its_detail_reach_and_radius() {
        let (w, h) = (48u32, 16u32);
        let flat = masked_tile(w, h, 1, |_, _, _| 1_000);
        let checker = masked_tile(w, h, 1, |x, y, _| if (x + y) % 2 == 0 { 0 } else { 60_000 });
        let uncovered_columns = 8u32;
        let mask: Vec<bool> = (0..h)
            .flat_map(|_| (0..w).map(|x| x >= uncovered_columns))
            .collect();
        let out = fuse_masked(&[flat, checker], &[None, Some(mask)], w, h);
        // The detail plane reads 4 px (reduce plus expand), the weight window 2 (radius).
        let first_eligible = uncovered_columns + 4 + 2;
        for y in 4..12 {
            for x in 0..w {
                let expected = if x < first_eligible { 0 } else { 1 };
                assert_eq!(out.zmap[(y * w + x) as usize], expected, "({x}, {y})");
            }
        }
    }

    #[test]
    fn a_pixel_with_no_eligible_frame_is_no_source_with_zero_samples() {
        let (w, h) = (16u32, 16u32);
        let tiles = [
            masked_tile(w, h, 3, texture(1)),
            masked_tile(w, h, 3, texture(2)),
        ];
        let none = Some(vec![false; (w * h) as usize]);
        let out = fuse_masked(&tiles, &[none.clone(), none], w, h);
        assert!(out.zmap.iter().all(|&z| z == crate::zmap::NO_SOURCE));
        assert!(out.tile.samples.iter().all(|&s| s == 0));
    }

    #[test]
    fn every_channel_is_blended_with_the_same_weight() {
        let (w, h) = (32u32, 16u32);
        // A: red edge, constant green. B: constant green, blue edge. Green
        // alone fixes each pixel's blend fraction; red and blue must follow it.
        let a = masked_tile(w, h, 3, |x, _, c| match c {
            0 => {
                if x < 16 {
                    10_000
                } else {
                    40_000
                }
            }
            1 => 2_000,
            _ => 1_000,
        });
        let b = masked_tile(w, h, 3, |x, _, c| match c {
            0 => 20_000,
            1 => 3_000,
            _ => {
                if x < 16 {
                    5_000
                } else {
                    30_000
                }
            }
        });
        let out = fuse_masked(&[a.clone(), b.clone()], &[], w, h);
        let mut checked = 0;
        for p in 0..(w * h) as usize {
            let t = (f64::from(out.tile.samples[p * 3 + 1]) - 3_000.0) / (2_000.0 - 3_000.0);
            if !(0.05..0.95).contains(&t) {
                continue;
            }
            checked += 1;
            for c in [0usize, 2] {
                let (sa, sb) = (
                    f64::from(a.samples[p * 3 + c]),
                    f64::from(b.samples[p * 3 + c]),
                );
                let expected = t * sa + (1.0 - t) * sb;
                let got = f64::from(out.tile.samples[p * 3 + c]);
                assert!(
                    (got - expected).abs() < 40.0,
                    "pixel {p} channel {c}: {got} vs {expected}"
                );
            }
        }
        assert!(checked > 0, "no pixel blended both frames");
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
        checkerboard_amplitude(width, height, spp, 20000, 40000)
    }

    fn checkerboard_amplitude(width: u32, height: u32, spp: u32, lo: u16, hi: u16) -> Vec<u16> {
        let mut out = vec![0u16; width as usize * height as usize * spp as usize];
        for y in 0..height {
            for x in 0..width {
                let v = if (x + y) % 2 == 0 { hi } else { lo };
                for c in 0..spp {
                    out[((y * width + x) * spp + c) as usize] = v;
                }
            }
        }
        out
    }

    fn cfg(radius: i64, weight_exponent: f64) -> ParamValues {
        let mut cfg = ParamValues::new();
        cfg.insert("radius".to_string(), ParamValue::Int(radius));
        cfg.insert(
            "weight-exponent".to_string(),
            ParamValue::Float(weight_exponent),
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
    fn shipped_radius_range_defaults_and_endpoints_remain_validated() {
        let radius = PARAMS
            .iter()
            .find(|parameter| parameter.id == "radius")
            .expect("radius must be declared");
        assert!(matches!(&radius.kind, ParamKind::Int { min: 0, max: 16 }));
        assert_eq!(radius.default, ParamValue::Int(RADIUS_DEFAULT));

        let algo = WeightedAverage;
        let (width, height, spp) = (64, 64, 3);
        let tiles = vec![tile_of(
            width,
            height,
            spp,
            checkerboard(width, height, spp),
        )];
        let ctx = TileCtx::new(rect(width, height), &tiles);
        for radius in [0, 16] {
            algo.fuse(&ctx, &cfg(radius, WEIGHT_EXPONENT_DEFAULT))
                .unwrap_or_else(|error| panic!("radius={radius} must fuse: {error}"));
        }
        let error = algo
            .fuse(&ctx, &cfg(17, WEIGHT_EXPONENT_DEFAULT))
            .expect_err("radius=17 must be refused");
        assert!(
            error
                .to_string()
                .contains("radius: value 17 is outside the allowed range [0, 16]"),
            "error: {error}"
        );
    }

    #[test]
    fn shipped_defaults_are_a_16_px_window_and_exponent_4() {
        let default_of = |id: &str| {
            PARAMS
                .iter()
                .find(|parameter| parameter.id == id)
                .unwrap_or_else(|| panic!("{id} must be declared"))
                .default
                .clone()
        };
        assert_eq!(default_of("radius"), ParamValue::Int(16));
        assert_eq!(default_of("weight-exponent"), ParamValue::Float(4.0));
    }

    #[test]
    fn every_parameter_the_implementation_reads_is_declared() {
        let algo = WeightedAverage;
        let (w, h, spp) = (8u32, 8u32, 3u32);
        let tiles = vec![tile_of(w, h, spp, flat(w, h, spp, 100))];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let err = algo.fuse(&ctx, &ParamValues::new()).unwrap_err();
        match err {
            AlgorithmError::Failed { algorithm, detail } => {
                assert_eq!(algorithm, "weighted-average");
                assert!(detail.contains("radius"));
            }
            other => panic!("expected AlgorithmError::Failed naming `radius`, got {other:?}"),
        }
    }

    #[test]
    fn a_single_frame_stack_round_trips_within_one_lsb_and_wins_everywhere() {
        let algo = WeightedAverage;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let content = checkerboard(w, h, spp);
        let tiles = vec![tile_of(w, h, spp, content.clone())];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg(8, 2.0)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 0),
            "a one-frame stack must name frame 0 everywhere"
        );
        for (&got, &want) in out.tile.samples.iter().zip(content.iter()) {
            let diff = (got as i32 - want as i32).abs();
            assert!(
                diff <= 1,
                "a one-frame stack's composite must round-trip within 1 LSB: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn identical_frames_tie_break_to_the_lowest_index() {
        let algo = WeightedAverage;
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let content = checkerboard(w, h, spp);
        let tiles = vec![
            tile_of(w, h, spp, content.clone()),
            tile_of(w, h, spp, content.clone()),
            tile_of(w, h, spp, content),
        ];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg(8, 2.0)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 0),
            "identical frames must tie-break to frame 0 everywhere"
        );
    }

    #[test]
    fn the_only_frame_with_detail_dominates_the_blend_everywhere() {
        let algo = WeightedAverage;
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
        let out = algo.fuse(&ctx, &cfg(8, 2.0)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 1),
            "the only frame with detail must dominate the blend everywhere"
        );
        for (&got, &want) in out.tile.samples.iter().zip(sharp.iter()) {
            let diff = (got as i32 - want as i32).abs();
            assert!(
                diff <= 1,
                "with every other frame's weight at exactly 0, the blend must equal the \
                 dominant frame within 1 LSB: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn flat_stack_falls_back_to_the_plain_mean() {
        let algo = WeightedAverage;
        let (w, h, spp) = (8u32, 8u32, 1u32);
        let tiles = vec![
            tile_of(w, h, spp, flat(w, h, spp, 10000)),
            tile_of(w, h, spp, flat(w, h, spp, 20000)),
            tile_of(w, h, spp, flat(w, h, spp, 30000)),
        ];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let out = algo.fuse(&ctx, &cfg(8, 2.0)).unwrap();
        assert!(
            out.zmap.iter().all(|&index| index == 0),
            "the degenerate (den == 0) rule must record index 0 everywhere"
        );
        for &sample in &out.tile.samples {
            assert_eq!(
                sample, 20000,
                "three flat frames' plain mean must be exactly 20000"
            );
        }
    }

    #[test]
    fn higher_weight_exponent_shifts_the_blend_toward_the_sharper_frame() {
        // A modest radius and amplitude: this test is about the exponent's
        // *sign* of effect, not its extremes. The extreme is covered by
        // `the_top_of_the_declared_range_does_not_overflow`.
        let (w, h, spp) = (16u32, 16u32, 1u32);
        let sharp = checkerboard_amplitude(w, h, spp, 10000, 50000);
        let mild = checkerboard_amplitude(w, h, spp, 30000, 35000);
        let tiles = vec![
            tile_of(w, h, spp, sharp.clone()),
            tile_of(w, h, spp, mild.clone()),
        ];
        let ctx = TileCtx::new(rect(w, h), &tiles);
        let algo = WeightedAverage;

        let low = algo.fuse(&ctx, &cfg(4, 1.0)).unwrap();
        let high = algo.fuse(&ctx, &cfg(4, 4.0)).unwrap();

        // An interior pixel, away from the clamp-to-edge border, so the
        // window sees genuine neighbours on every side.
        let p = (8 * w + 8) as usize;
        let sharp_val = sharp[p] as i64;
        let low_val = low.tile.samples[p] as i64;
        let high_val = high.tile.samples[p] as i64;
        let dist_low = (sharp_val - low_val).abs();
        let dist_high = (sharp_val - high_val).abs();
        assert!(
            dist_high < dist_low,
            "a higher weight-exponent must move the blend strictly closer to the sharper \
             frame at an interior pixel: dist_low={dist_low}, dist_high={dist_high}"
        );
    }

    /// The top of the declared parameter range must produce a real pixel,
    /// not a silently black one.
    ///
    /// A radius-16 window over a full 0..65535 checkerboard gives a score
    /// of several million. At `weight-exponent = 8.0` -- the declared maximum --
    /// `1e6^8` is about `1e48`, which is infinity in `f32` (max ~3.4e38).
    /// Before the weight was widened to `f64` this produced `inf / inf` in
    /// the normalisation, i.e. `NaN`, which rounds to 0: a black pixel at
    /// the sharpest point in the image, with no error raised anywhere.
    ///
    /// The declared range is part of the contract, so the whole of it has
    /// to work. This is the criterion that was missing when the range was
    /// declared.
    #[test]
    fn the_top_of_the_declared_range_does_not_overflow() {
        let (w, h, spp) = (32u32, 32u32, 1u32);
        let sharp = checkerboard_amplitude(w, h, spp, 0, 65535);
        let mild = checkerboard_amplitude(w, h, spp, 30000, 35000);
        let tiles = vec![tile_of(w, h, spp, sharp.clone()), tile_of(w, h, spp, mild)];
        let ctx = TileCtx::new(rect(w, h), &tiles);

        let out = WeightedAverage
            .fuse(&ctx, &cfg(16, 8.0))
            .expect("the declared maximum exponent must fuse, not fail");

        // Interior pixel, away from the clamp-to-edge border.
        let p = (16 * w + 16) as usize;
        let sharp_val = sharp[p];
        assert_eq!(
            out.tile.samples[p], sharp_val,
            "at the declared maximum exponent the sharpest frame should dominate              completely; a 0 here is the old f32 overflow returning as NaN"
        );

        // And nothing anywhere collapsed to the NaN-rounds-to-zero pixel.
        let sharp_is_bright = sharp.iter().filter(|&&v| v > 0).count();
        let out_is_bright = out.tile.samples.iter().filter(|&&v| v > 0).count();
        assert_eq!(
            out_is_bright, sharp_is_bright,
            "every pixel bright in the dominating frame must stay bright in the blend"
        );
    }

    #[test]
    fn declared_ranges_fit_the_shipped_geometry() {
        let algo = WeightedAverage;
        let max_radius = declared_int_max("radius");
        for radius in 0..=max_radius {
            let support = algo.tile_support(&cfg(radius, 2.0));
            let reach = DETAIL_REACH + radius as u32;
            assert_eq!(support.reach_px, reach);
            assert_eq!(
                support.margin(),
                reach.next_multiple_of(2).max(margin_for_levels(1)),
                "radius={radius}: the margin is the reach on the 2 px grid"
            );
            assert!(
                support.min_tile_size() <= DEFAULT_TILE_SIZE,
                "radius={radius}: min_tile_size {} > {DEFAULT_TILE_SIZE}",
                support.min_tile_size()
            );
        }
        let support = algo.tile_support(&cfg(max_radius, WEIGHT_EXPONENT_DEFAULT));
        let minimum = support.min_tile_size();
        assert!(
            TilePlan::new(minimum, minimum, minimum, support).is_ok(),
            "radius={max_radius}: the reported minimum tile {minimum} must plan"
        );
        assert!(
            matches!(
                TilePlan::new(minimum, minimum, minimum - 1, support),
                Err(TileError::NoInterior { min_tile_size, .. }) if min_tile_size == minimum
            ),
            "radius={max_radius}: one pixel below the reported minimum {minimum} must be refused"
        );
    }

    #[test]
    fn working_bytes_per_pixel_are_pinned() {
        let cfg = ParamValues::new();
        assert_eq!(WeightedAverage.working_bytes_per_pixel(&cfg, 3), 106);
        assert_eq!(WeightedAverage.working_bytes_per_pixel(&cfg, 1), 50);
    }

    #[test]
    fn tile_support_falls_back_to_the_default_radius_when_absent() {
        let algo = WeightedAverage;
        assert_eq!(
            algo.tile_support(&ParamValues::new()),
            TileSupport {
                reach_px: DETAIL_REACH + RADIUS_DEFAULT as u32,
                decimation_levels: 1,
            }
        );
    }
}
