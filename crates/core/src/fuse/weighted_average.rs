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
//! arithmetic mean over frames in ascending order, and the Z-Map index is
//! `0`. Never [`crate::zmap::NO_SOURCE`], never a division by zero, never a
//! panic -- `weight_f(p) >= 0.0` for every `f` (a sum of absolute values
//! raised to a positive power), so `den(p) == 0.0` can only mean every term
//! was `0.0`, and it is checked for exactly, not approximated.
//!
//! # The Z-Map
//!
//! **The composite is a blend; the Z-Map is not the blend's provenance in
//! full.** It records the pixel's **dominant contributor**: `argmax_f
//! weight_f(p)`, **strictly greater** so the **lowest index wins an exact
//! tie** -- identical to `laplacian`'s tie-break. Retouch recomposites from
//! the Z-Map plus source textures; that reconstruction recovers exactly one
//! frame's pixels, never the blend the user actually saw, so this Z-Map is
//! *not* sufficient to reconstruct this algorithm's own composite -- the
//! one place in the project where that gap is real rather than
//! theoretical.
//!
//! # Reach and `tile_levels`
//!
//! A windowed weight of radius `r` reads `r` px of its own neighbourhood
//! plus `crate::fuse::sharpness::DETAIL_REACH` (`2`) for the one-level
//! build-plus-expand underneath it: `2 + r`. [`WeightedAverage::tile_levels`]
//! overrides the trait's default (which would answer `0`, since this
//! schema declares no key named `"levels"`) with
//! `crate::fuse::sharpness::levels_for_margin(2 + radius)` -- `2` at the
//! default `radius = 8`.
//!
//! # Tiling
//!
//! Neither a pyramid nor `collapse` survives past the per-pixel blend --
//! every output sample is a weighted combination of samples the read rect
//! already holds, all within `2 + radius` px of `p`. Where that reach sits
//! inside the scheduler's margin (always, given the override above), the
//! composite **and** the Z-Map are bit-exact between a tiled and an
//! untiled run -- unlike `laplacian`'s composite, which additionally passes
//! through `collapse` and is therefore not (see
//! `crate::fuse::laplacian`'s "Tiling and the composite seam" section).

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::fuse::PyramidError;
use crate::fuse::laplacian::score_plane;
use crate::fuse::pyramid::Plane;
use crate::fuse::sharpness::{DETAIL_REACH, detail_plane, levels_for_margin};
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::StackRegistration;
use crate::tile::Tile;

const RADIUS_DEFAULT: i64 = 8;

/// `2.0`. Measured over `fixtures/mantis-512` (single-tile, 704 px):
/// composite Laplacian variance is 83,079.4 at `1.0`, 169,581.2 at `2.0`
/// and 218,923.3 at `4.0` -- `2.0` already captures most of the exponent's
/// benefit (roughly 2x `1.0`'s variance), while `4.0` buys only a further
/// ~29% over `2.0` for walking twice as far toward selection, which is
/// what "weighted average" is meant not to be.
const WEIGHT_EXPONENT_DEFAULT: f64 = 2.0;

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "radius".to_string(),
            label: "Radius".to_string(),
            help: "Sharpness-window radius, in level-0 pixels: how far the \
                   per-pixel weight looks for local detail before blending. \
                   A detail-scale property, not resolution-derived -- no \
                   `auto` hook, like `laplacian`'s own `SCORE_RADIUS` \
                   constant."
                .to_string(),
            kind: ParamKind::Int { min: 0, max: 16 },
            default: ParamValue::Int(RADIUS_DEFAULT),
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "weight-exponent".to_string(),
            label: "Weight Exponent".to_string(),
            help: "How aggressively the blend favours locally sharper \
                   frames: `1.0` is the plainest reading of \"weighted \
                   average\", higher values walk the blend toward \
                   selection. A taste knob, not resolution-derived -- no \
                   `auto` hook."
                .to_string(),
            kind: ParamKind::Float {
                min: 1.0,
                max: 8.0,
                step: 0.1,
                logarithmic: false,
            },
            default: ParamValue::Float(WEIGHT_EXPONENT_DEFAULT),
            auto: None,
            host_default: None,
        },
    ]
});

fn wrap_pyramid_error(source: PyramidError) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "weighted-average".to_string(),
        detail: source.to_string(),
    }
}

fn missing_key(key: &str) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "weighted-average".to_string(),
        detail: format!(
            "resolved values are missing `{key}`; call crate::params::resolve before fuse"
        ),
    }
}

/// The Weighted Average fusion algorithm. See the module docs for the
/// weight, the blend, the degenerate rule and the Z-Map semantics.
pub struct WeightedAverage;

impl WeightedAverage {
    /// `radius`, read out of `cfg` with the descriptor's own default as the
    /// fallback -- `tile_levels` has no `Result`, so it cannot report a
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
        "Weighted Average"
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let radius = cfg
            .get("radius")
            .and_then(ParamValue::as_i64)
            .ok_or_else(|| missing_key("radius"))? as u32;
        let weight_exponent = cfg
            .get("weight-exponent")
            .and_then(ParamValue::as_f64)
            .ok_or_else(|| missing_key("weight-exponent"))? as f32;

        let rect = ctx.rect();
        let (width, height) = (rect.width, rect.height);
        let frame_count = ctx.frame_count();
        let frame0 = ctx.frame(0)?;
        let spp = frame0.samples_per_pixel;
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
        let mut winner = vec![0u16; pixel_count];

        for f in 0..frame_count {
            let tile = ctx.frame(f)?;
            let detail =
                detail_plane(&tile.samples, width, height, spp).map_err(wrap_pyramid_error)?;
            let scores = score_plane(&detail, radius);

            for p in 0..pixel_count {
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
        }

        let frame_count_f64 = frame_count as f64;
        let mut blended = vec![0.0f32; pixel_count * spp_usize];
        for p in 0..pixel_count {
            let base = p * spp_usize;
            if den[p] > 0.0 {
                for c in 0..spp_usize {
                    // The one narrowing: every weight and every sum above is
                    // `f64`, and only the finished ratio becomes `f32`.
                    blended[base + c] = (num[base + c] / den[p]) as f32;
                }
            } else {
                // Degenerate rule: every frame's weight was exactly zero at
                // this pixel. Fall back to the plain arithmetic mean, and
                // record the Z-Map index as 0 -- never `NO_SOURCE`, never a
                // division by zero.
                for c in 0..spp_usize {
                    blended[base + c] = (sample_sum[base + c] / frame_count_f64) as f32;
                }
                winner[p] = 0;
            }
        }

        // One rounding rule in the project: round-trip through `Plane` so
        // the pinned `floor(clamp(v, 0, 65535) + 0.5)` expression is the
        // only place a `f32` sample becomes a `u16` one.
        let mut plane = Plane::from_u16(&vec![0u16; pixel_count * spp_usize], width, height, spp)
            .map_err(wrap_pyramid_error)?;
        plane.data_mut().copy_from_slice(&blended);
        let mut samples = vec![0u16; pixel_count * spp_usize];
        plane
            .to_u16_into(&mut samples)
            .map_err(wrap_pyramid_error)?;

        crate::zmap::validate_tile(&winner, frame_count as u32).map_err(|source| {
            AlgorithmError::Failed {
                algorithm: "weighted-average".to_string(),
                detail: source.to_string(),
            }
        })?;

        Ok(TileOutput {
            tile: Tile {
                rect,
                samples_per_pixel: spp,
                samples,
            },
            zmap: winner,
        })
    }

    /// This algorithm builds no pyramid at all, but its windowed weight
    /// still has a nonzero reach (`crate::fuse::sharpness::DETAIL_REACH +
    /// radius`), so it cannot inherit the trait's default (which would
    /// answer `0`, since this schema declares no `"levels"` key).
    fn tile_levels(&self, cfg: &ParamValues) -> u32 {
        let radius = Self::radius(cfg);
        levels_for_margin(DETAIL_REACH + radius)
    }
}

inventory::submit! { StackRegistration::new(&WeightedAverage) }

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
    /// A radius-8 window over a full 0..65535 checkerboard gives a score
    /// around 1e6. At `weight-exponent = 8.0` -- the declared maximum --
    /// `1e6^8` is about `1e48`, which is infinity in `f32` (max ~3.4e38);
    /// at that weight the declared range must not overflow to a black
    /// pixel.
    #[test]
    fn the_top_of_the_declared_range_does_not_overflow() {
        let (w, h, spp) = (32u32, 32u32, 1u32);
        let sharp = checkerboard_amplitude(w, h, spp, 0, 65535);
        let mild = checkerboard_amplitude(w, h, spp, 30000, 35000);
        let tiles = vec![tile_of(w, h, spp, sharp.clone()), tile_of(w, h, spp, mild)];
        let ctx = TileCtx::new(rect(w, h), &tiles);

        let out = WeightedAverage
            .fuse(&ctx, &cfg(8, 8.0))
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
        for radius in 0..=16i64 {
            let cfg = cfg(radius, 2.0);
            let levels = algo.tile_levels(&cfg);
            let reach = DETAIL_REACH + radius as u32;
            assert!(
                margin_for_levels(levels) >= reach,
                "radius={radius}: margin_for_levels({levels}) = {} < reach {reach}",
                margin_for_levels(levels)
            );
            assert!(
                min_tile_size_for_levels(levels) <= DEFAULT_TILE_SIZE,
                "radius={radius}: min_tile_size_for_levels({levels}) = {} > {DEFAULT_TILE_SIZE}",
                min_tile_size_for_levels(levels)
            );
        }
    }

    #[test]
    fn tile_levels_falls_back_to_the_default_radius_when_absent() {
        let algo = WeightedAverage;
        assert_eq!(
            algo.tile_levels(&ParamValues::new()),
            levels_for_margin(DETAIL_REACH + RADIUS_DEFAULT as u32)
        );
    }
}
