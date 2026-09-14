// SPDX-License-Identifier: GPL-3.0-or-later

//! The one thing `weighted_average` and `depth_map` genuinely share: the
//! level-0 detail plane both take their per-pixel sharpness measure from
//! (via [`crate::fuse::laplacian::score_plane`], `pub` so `crates/gpu`'s
//! parity tests can call it too), its pinned reach constant, and the
//! inversion of
//! [`crate::tile::plan::margin_for_levels`] both algorithms' `tile_levels`
//! overrides need. Nothing algorithm-specific lives here -- no weight
//! formula, no tie-break, no degenerate-input rule.

use crate::fuse::PyramidError;
use crate::fuse::pyramid::Plane;
use crate::tile::plan::{MAX_LEVELS, margin_for_levels};

/// The build-plus-expand reach of a **one**-level pyramid: `2 * (2^1 - 1)`,
/// the same expression [`crate::fuse::laplacian::score_radius`] evaluates at
/// `levels = 1`. Both algorithms build no deeper than one level --
/// `L_0 = base - expand(reduce(base))` does not depend on how many further
/// levels a caller would go on to build -- so this is always the detail
/// plane's own reach, never a function of anything else.
pub(crate) const DETAIL_REACH: u32 = 2; // 2 * (2^1 - 1)

/// The level-0 Laplacian detail plane of one frame's tile: `build(samples,
/// width, height, spp, 1).levels()[0]`. A deeper pyramid is pointless here --
/// see [`DETAIL_REACH`] -- so this always builds to exactly one level and
/// takes that level by value (`Plane` has no cheaper move out of a
/// [`crate::fuse::pyramid::Pyramid`], and the pyramid itself, residual
/// included, is dropped immediately after).
pub(crate) fn detail_plane(
    samples: &[u16],
    width: u32,
    height: u32,
    spp: u32,
) -> Result<Plane, PyramidError> {
    let pyramid = crate::fuse::build(samples, width, height, spp, 1)?;
    Ok(pyramid.levels()[0].clone())
}

/// The inversion of [`margin_for_levels`] both `tile_levels` overrides need:
/// the smallest pyramid-level count `n <= MAX_LEVELS` whose margin covers a
/// reach of `px` pixels, saturating at [`MAX_LEVELS`] rather than panicking
/// or overflowing when no level count in range covers `px`.
///
/// `margin_for_levels` is monotone non-decreasing over
/// [`crate::fuse::pyramid::MEASURED_MARGIN`] and the analytic tail past it,
/// so a linear scan from `0` is both correct and cheap (`MAX_LEVELS + 1`
/// iterations at worst, `16`).
///
/// `tile_levels` returns a level count, but what both overriding
/// algorithms actually know is a pixel margin. This function is the
/// lossy conversion from the second to the first.
pub(crate) fn levels_for_margin(px: u32) -> u32 {
    for n in 0..=MAX_LEVELS {
        if margin_for_levels(n) >= px {
            return n;
        }
    }
    MAX_LEVELS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(width: u32, height: u32, spp: u32, value: u16) -> Vec<u16> {
        vec![value; width as usize * height as usize * spp as usize]
    }

    #[test]
    fn levels_for_margin_matches_the_two_algorithms_own_derivations() {
        // weighted-average at its default radius (8): reach = 2 + 8 = 10,
        // which inverts to 2 levels.
        assert_eq!(levels_for_margin(2 + 8), 2);
        // depth-map at its defaults (score-radius 4, smoothing-radius 8):
        // reach = 2 + 4 + 8 = 14, which inverts to 3 levels.
        assert_eq!(levels_for_margin(2 + 4 + 8), 3);
        // weighted-average's worst legal setting (radius = 16): reach = 18,
        // inverts to 3 levels, and `min_tile_size_for_levels(3) == 160`.
        assert_eq!(levels_for_margin(2 + 16), 3);
        // depth-map's worst legal setting (score-radius = smoothing-radius
        // = 16): reach = 34, inverts to 4 levels, and
        // `min_tile_size_for_levels(4) == 256`.
        assert_eq!(levels_for_margin(2 + 16 + 16), 4);
    }

    #[test]
    fn levels_for_margin_is_the_smallest_level_count_that_covers_the_reach() {
        assert_eq!(levels_for_margin(0), 0);
        // margin_for_levels(0) == 0 < 1, margin_for_levels(1) == 4 >= 1.
        assert_eq!(levels_for_margin(1), 1);
        for px in 0..200u32 {
            let n = levels_for_margin(px);
            assert!(
                margin_for_levels(n) >= px,
                "levels_for_margin({px}) = {n}, but margin_for_levels({n}) = {} < {px}",
                margin_for_levels(n)
            );
            if n > 0 {
                assert!(
                    margin_for_levels(n - 1) < px,
                    "levels_for_margin({px}) = {n} is not the smallest: \
                     margin_for_levels({}) = {} already covers {px}",
                    n - 1,
                    margin_for_levels(n - 1)
                );
            }
        }
    }

    #[test]
    fn levels_for_margin_is_monotone() {
        let mut previous = 0u32;
        for px in 0..600u32 {
            let n = levels_for_margin(px);
            assert!(
                n >= previous,
                "levels_for_margin must be non-decreasing: px={px} gave {n}, previous was {previous}"
            );
            previous = n;
        }
    }

    #[test]
    fn levels_for_margin_saturates_at_max_levels_rather_than_overflowing() {
        assert_eq!(levels_for_margin(u32::MAX), MAX_LEVELS);
    }

    #[test]
    fn detail_plane_of_flat_input_is_zero() {
        let (width, height, spp) = (8u32, 8u32, 3u32);
        let samples = flat(width, height, spp, 12345);
        let plane = detail_plane(&samples, width, height, spp).unwrap();
        assert_eq!(plane.width(), width);
        assert_eq!(plane.height(), height);
        assert_eq!(plane.samples_per_pixel(), spp);
        assert!(
            plane.data().iter().all(|&v| v == 0.0),
            "a flat frame's level-0 Laplacian must be exactly zero everywhere"
        );
    }

    #[test]
    fn detail_plane_matches_a_direct_one_level_build() {
        // `max_levels(width, height)` must be at least 1 for a direct
        // one-level `build` to succeed at all -- `floor(log2(min(w, h) / 4))`
        // needs `min(w, h) >= 8`.
        let (width, height, spp) = (16u32, 12u32, 1u32);
        let samples: Vec<u16> = (0..(width * height * spp))
            .map(|i| (i * 37) as u16)
            .collect();
        let plane = detail_plane(&samples, width, height, spp).unwrap();
        let direct = crate::fuse::build(&samples, width, height, spp, 1).unwrap();
        assert_eq!(plane.data(), direct.levels()[0].data());
    }

    #[test]
    fn detail_plane_propagates_a_buffer_size_error() {
        let err = detail_plane(&[0u16; 3], 4, 4, 1).unwrap_err();
        assert!(matches!(err, PyramidError::BufferSize { .. }));
    }
}
