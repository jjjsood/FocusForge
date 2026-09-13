// SPDX-License-Identifier: GPL-3.0-or-later

//! The alignment pyramid: a per-frame single-channel luminance [`Plane`],
//! streamed in from tiles and assembled whole, then reduced to a Gaussian
//! pyramid via [`gaussian_levels`].
//!
//! Level sizing (`ceil(dim / 2^k)`) and clamp-to-edge are inherited from
//! [`crate::fuse::pyramid`] and are not restated here -- see that module's
//! docs for the kernel, pass order and border rule. The single channel is
//! **luminance**, the arithmetic mean of a pixel's samples
//! ([`crate::fuse::metrics::luma`]), never a perceptual (Rec.709) weighting.
//!
//! # Residency
//!
//! A pairwise
//! registration holds exactly two frames' pyramids live, level 0 included --
//! 179 MB per frame at level 0 (8192 x 5464 `f32`) plus 59.7 MB for levels
//! 1..N, **477 MB steady-state for the pair** (corrected: an earlier figure
//! of 358 MB for the pair contradicted its own components, 179 +
//! 59.7 per frame is 238.7, and the pair is 477.4). The **transient** peak is
//! higher still, ~622 MB, with the assembler's own buffer and
//! [`Plane::from_f32`]'s copy of it also live for one frame at a time --
//! [`crate::align::driver::bytes_per_registration`] is the one place that
//! full figure is computed, term by term. Levels are streamed in from tiles
//! ([`LumaAssembler`], one `f32` write per pixel, no whole-frame buffer in
//! the *reader*) and then held in RAM as whole [`Plane`]s for the pairwise
//! solve. `level(0)` is retained -- the level-0-omitted variant is not
//! built.

use crate::align::error::AlignError;
use crate::fuse::PyramidError;
pub use crate::fuse::pyramid::max_levels;
use crate::fuse::pyramid::{Plane, gaussian_levels};
use crate::tile::Tile;

/// Assembles a single-channel luminance [`Plane`] out of tiles pushed in
/// any order: one `f32` per pixel, the arithmetic mean of that pixel's
/// samples ([`crate::fuse::metrics::luma`], computed in `f64` then cast
/// once).
pub struct LumaAssembler {
    width: u32,
    height: u32,
    data: Vec<f32>,
}

impl LumaAssembler {
    pub fn new(width: u32, height: u32) -> Self {
        LumaAssembler {
            width,
            height,
            data: vec![0.0f32; width as usize * height as usize],
        }
    }

    /// Writes `tile`'s luminance into this assembler at `tile.rect`'s
    /// absolute position. [`AlignError::TileOutsideFrame`] when `tile.rect`
    /// is not fully inside `(width, height)` -- never clamped.
    pub fn push_tile(&mut self, tile: &Tile) -> Result<(), AlignError> {
        let rect = tile.rect;
        let in_bounds = rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|xw| xw <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.height);
        if !in_bounds {
            return Err(AlignError::TileOutsideFrame {
                tile: rect,
                frame: (self.width, self.height),
            });
        }

        let spp = tile.samples_per_pixel as usize;
        for y in 0..rect.height {
            for x in 0..rect.width {
                let pixel_start = (y as usize * rect.width as usize + x as usize) * spp;
                let pixel = &tile.samples[pixel_start..pixel_start + spp];
                let v = crate::fuse::metrics::luma(pixel, tile.samples_per_pixel) as f32;
                let out_x = rect.x + x;
                let out_y = rect.y + y;
                self.data[out_y as usize * self.width as usize + out_x as usize] = v;
            }
        }
        Ok(())
    }

    /// Finalizes assembly into a single-channel [`Plane`]
    /// ([`Plane::from_f32`], `samples_per_pixel = 1`).
    pub fn finish(self) -> Result<Plane, PyramidError> {
        Plane::from_f32(&self.data, self.width, self.height, 1)
    }
}

/// A per-frame Gaussian luminance pyramid for alignment. `level(0)` is the
/// full-resolution plane and is retained -- the level-0-omitted variant is
/// not built.
pub struct AlignPyramid {
    levels: Vec<Plane>,
}

impl AlignPyramid {
    /// Wraps [`gaussian_levels`] over an already-assembled luminance base
    /// (typically [`LumaAssembler::finish`]'s output).
    pub fn from_luma(base: Plane, levels: u32) -> Result<Self, PyramidError> {
        let levels = gaussian_levels(base, levels)?;
        Ok(AlignPyramid { levels })
    }

    pub fn level(&self, k: usize) -> &Plane {
        &self.levels[k]
    }

    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// The base image's `(width, height)`, i.e. `level(0)`'s.
    pub fn dimensions(&self) -> (u32, u32) {
        let base = &self.levels[0];
        (base.width(), base.height())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::metrics::luma;
    use crate::tile::Rect;

    fn constant_tile(rect: Rect, spp: u32, value: u16) -> Tile {
        Tile {
            rect,
            samples_per_pixel: spp,
            samples: vec![value; rect.width as usize * rect.height as usize * spp as usize],
        }
    }

    #[test]
    fn level_dimensions_and_odd_sizes() {
        // `dim_k = ceil(dim / 2^k)`, the rule `crate::fuse::pyramid`
        // already states, not a second one: 13 -> 7 -> 4 -> 2. A base of
        // 13x13 only supports one legal reduce
        // (`crate::fuse::pyramid::max_levels(13, 13) == 1`, the same "do
        // not shrink below ~4px" floor `fuse::pyramid::tests::
        // level_count_limit_is_enforced` pins), so the odd-size step this
        // module can build (13 -> 7) is checked against a real
        // `AlignPyramid`, and the formula's next two steps (7 -> 4 -> 2,
        // below any pyramid's legal floor) are checked directly against
        // the same `div_ceil` rule.
        let base = Plane::from_f32(&vec![5.0f32; 169], 13, 13, 1).unwrap();
        assert_eq!(max_levels(13, 13), 1);
        let pyramid = AlignPyramid::from_luma(base, 1).unwrap();
        assert_eq!(pyramid.level_count(), 2);
        assert_eq!(pyramid.level(0).width(), 13);
        assert_eq!(pyramid.level(0).height(), 13);
        assert_eq!(pyramid.level(1).width(), 7);
        assert_eq!(pyramid.level(1).height(), 7);
        assert_eq!(pyramid.dimensions(), (13, 13));

        assert_eq!(13u32.div_ceil(2), 7);
        assert_eq!(7u32.div_ceil(2), 4);
        assert_eq!(4u32.div_ceil(2), 2);
    }

    #[test]
    fn constant_image_survives_downsampling() {
        // Both `reduce` passes' taps sum to exactly 1 in `f32`, so this is
        // an equality assertion, not a tolerance.
        let base = Plane::from_f32(&vec![1234.5f32; 32 * 32], 32, 32, 1).unwrap();
        assert_eq!(max_levels(32, 32), 3);
        let pyramid = AlignPyramid::from_luma(base, 3).unwrap();
        for k in 0..pyramid.level_count() {
            let level = pyramid.level(k);
            for &v in level.data() {
                assert_eq!(v, 1234.5f32, "level {k} did not survive as a constant");
            }
        }
    }

    #[test]
    fn luma_matches_fuse_metrics() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let spp = 3u32;
        let mut samples = Vec::with_capacity(4 * 4 * 3);
        // Deterministic, non-constant RGB content.
        for i in 0..(4 * 4) {
            samples.push((i * 100) as u16);
            samples.push((i * 100 + 7) as u16);
            samples.push((i * 100 + 23) as u16);
        }
        let tile = Tile {
            rect,
            samples_per_pixel: spp,
            samples: samples.clone(),
        };

        let mut assembler = LumaAssembler::new(4, 4);
        assembler.push_tile(&tile).unwrap();
        let plane = assembler.finish().unwrap();

        for i in 0..(4 * 4) {
            let expected = luma(&samples[i * 3..i * 3 + 3], spp) as f32;
            assert_eq!(plane.data()[i], expected, "pixel {i} luma mismatch");
        }
    }

    #[test]
    fn tiles_assemble_into_the_same_plane() {
        let spp = 3u32;
        let (width, height) = (8u32, 8u32);
        let mut whole_samples = Vec::with_capacity(8 * 8 * 3);
        for i in 0..(8 * 8) {
            whole_samples.push(((i * 37) % 65536) as u16);
            whole_samples.push(((i * 53 + 11) % 65536) as u16);
            whole_samples.push(((i * 71 + 5) % 65536) as u16);
        }
        let whole_rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let whole_tile = Tile {
            rect: whole_rect,
            samples_per_pixel: spp,
            samples: whole_samples.clone(),
        };
        let mut whole_assembler = LumaAssembler::new(width, height);
        whole_assembler.push_tile(&whole_tile).unwrap();
        let whole_plane = whole_assembler.finish().unwrap();

        // Four 4x4 quadrants, each cropped out of the same source data.
        let mut quad_assembler = LumaAssembler::new(width, height);
        for &(qx, qy) in &[(0u32, 0u32), (4, 0), (0, 4), (4, 4)] {
            let qrect = Rect {
                x: qx,
                y: qy,
                width: 4,
                height: 4,
            };
            let mut qsamples = Vec::with_capacity(4 * 4 * 3);
            for dy in 0..4u32 {
                for dx in 0..4u32 {
                    let src_x = qx + dx;
                    let src_y = qy + dy;
                    let idx = (src_y as usize * width as usize + src_x as usize) * 3;
                    qsamples.push(whole_samples[idx]);
                    qsamples.push(whole_samples[idx + 1]);
                    qsamples.push(whole_samples[idx + 2]);
                }
            }
            let qtile = Tile {
                rect: qrect,
                samples_per_pixel: spp,
                samples: qsamples,
            };
            quad_assembler.push_tile(&qtile).unwrap();
        }
        let quad_plane = quad_assembler.finish().unwrap();

        assert_eq!(whole_plane.data(), quad_plane.data());
    }

    #[test]
    fn tile_outside_frame_is_rejected() {
        let mut assembler = LumaAssembler::new(4, 4);
        let rect = Rect {
            x: 2,
            y: 2,
            width: 4,
            height: 4,
        };
        let tile = constant_tile(rect, 1, 100);
        match assembler.push_tile(&tile) {
            Err(AlignError::TileOutsideFrame { tile: t, frame }) => {
                assert_eq!(t, rect);
                assert_eq!(frame, (4, 4));
            }
            other => panic!("expected TileOutsideFrame, got {other:?}"),
        }
    }
}
