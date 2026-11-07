// SPDX-License-Identifier: GPL-3.0-or-later

//! The one thing `weighted_average` and `depth_map` genuinely share: the
//! level-0 detail plane both take their per-pixel sharpness measure from
//! (via [`crate::fuse::laplacian::score_plane`], `pub` so `crates/gpu`'s
//! parity tests can call it too), and its pinned reach constant. Nothing
//! algorithm-specific lives here -- no weight formula, no tie-break, no
//! degenerate-input rule.

use crate::fuse::PyramidError;
use crate::fuse::pyramid::Plane;
use crate::tile::Sample;

/// How far a pixel of the detail plane reads in the source: the one-level
/// reduce (`2`) plus the expand it is subtracted from (`2`). Coverage erosion
/// and tile planning both start from this reach.
pub(crate) const DETAIL_REACH: u32 = 4;

/// Which pixels of a `width x height` tile keep a frame eligible: those with
/// no uncovered pixel within `reach` (Chebyshev) of them, i.e. the coverage
/// mask eroded by a `(2 * reach + 1)`-square box. Pixels past the tile's
/// edge are not looked at; the scheduler's margin already holds real samples
/// there.
pub(crate) fn eligible(covered: &[bool], width: u32, height: u32, reach: u32) -> Vec<bool> {
    let (w, h, r) = (width as usize, height as usize, reach as usize);
    debug_assert_eq!(covered.len(), w * h);
    // Marks each slot of `line` whose `r`-neighbourhood holds a `true`, by prefix counts.
    fn smear(line: &mut [bool], r: usize, prefix: &mut Vec<u32>) {
        prefix.clear();
        let mut count = 0;
        prefix.push(count);
        for &uncovered in line.iter() {
            count += u32::from(uncovered);
            prefix.push(count);
        }
        for (i, slot) in line.iter_mut().enumerate() {
            let (lo, hi) = (i.saturating_sub(r), (i + r + 1).min(prefix.len() - 1));
            *slot = prefix[hi] > prefix[lo];
        }
    }
    let mut prefix = Vec::new();
    let mut bad: Vec<bool> = covered.iter().map(|&c| !c).collect();
    for row in bad.chunks_mut(w) {
        smear(row, r, &mut prefix);
    }
    let mut column = vec![false; h];
    for x in 0..w {
        for y in 0..h {
            column[y] = bad[y * w + x];
        }
        smear(&mut column, r, &mut prefix);
        for y in 0..h {
            bad[y * w + x] = column[y];
        }
    }
    bad.iter().map(|&b| !b).collect()
}

/// The level-0 Laplacian detail plane of one frame's tile: `build(samples,
/// width, height, spp, 1).levels()[0]`. `L_0 = base - expand(reduce(base))`
/// is the whole detail plane, with reach [`DETAIL_REACH`], so one level
/// suffices. The level is cloned out of the pyramid (`Plane` has no cheaper
/// move out of a [`crate::fuse::pyramid::Pyramid`]) and the pyramid,
/// residual included, is dropped immediately after.
pub(crate) fn detail_plane<S: Sample>(
    samples: &[S],
    width: u32,
    height: u32,
    spp: u32,
) -> Result<Plane, PyramidError> {
    let pyramid = crate::fuse::build(samples, width, height, spp, 1)?;
    Ok(pyramid.levels()[0].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eligible_erodes_by_exactly_the_reach() {
        let (w, h) = (40u32, 23u32);
        for reach in [0u32, 3, 14] {
            let mut covered = vec![true; (w * h) as usize];
            covered[(7 * w + 10) as usize] = false;
            covered[((h - 1) * w) as usize] = false;
            let got = eligible(&covered, w, h, reach);
            for y in 0..h {
                for x in 0..w {
                    let near =
                        |cx: u32, cy: u32| x.abs_diff(cx) <= reach && y.abs_diff(cy) <= reach;
                    let expected = !(near(10, 7) || near(0, h - 1));
                    assert_eq!(
                        got[(y * w + x) as usize],
                        expected,
                        "reach {reach} at ({x}, {y})"
                    );
                }
            }
        }
    }

    fn flat(width: u32, height: u32, spp: u32, value: u16) -> Vec<u16> {
        vec![value; width as usize * height as usize * spp as usize]
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
