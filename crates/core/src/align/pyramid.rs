// SPDX-License-Identifier: GPL-3.0-or-later

//! The alignment pyramid: a per-frame single-channel luminance Gaussian
//! pyramid of levels `1..=depth`, built from full-width row bands so the
//! full-resolution plane is never resident.
//!
//! Level sizing (`ceil(dim / 2^k)`) and clamp-to-edge are inherited from
//! [`crate::fuse::pyramid`] and are not restated here -- see that module's
//! docs for the kernel, pass order and border rule. The single channel is
//! **luminance**, the arithmetic mean of a pixel's samples
//! ([`crate::fuse::metrics::luma`]), never a perceptual (Rec.709) weighting.
//!
//! # Residency
//!
//! A frame's pyramid holds levels 1..N only: 59.7 MB at 8192 x 5464 `f32`
//! (a third of the 179 MB level-0 plane). Level 1 is assembled band by band
//! and level 0 is only ever read as a bounded window around the solve
//! region.
//!
//! An anchored run holds one *shared* term for the whole run
//! ([`crate::align::driver::bytes_shared_reference`]): the reference pyramid
//! plus the reference templates the solver retains per level. Each concurrent
//! registration adds its own frame pyramid, a streaming band or the solve
//! scratch ([`crate::align::driver::bytes_per_registration`]). At the
//! reference scale that is about 254 MB shared and 115 MB per registration.

use crate::align::error::AlignError;
use crate::fuse::PyramidError;
pub use crate::fuse::pyramid::max_levels;
use crate::fuse::pyramid::{Plane, gaussian_levels, reduce};
use crate::tile::{Rect, Tile};
use crate::timing::{TimingCounter, record_timing};

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
    /// ([`Plane::from_f32_vec`], `samples_per_pixel = 1`): the plane owns the
    /// assembler's buffer, no second copy is made.
    pub fn finish(self) -> Result<Plane, PyramidError> {
        Plane::from_f32_vec(self.data, self.width, self.height, 1)
    }
}

/// Why [`AlignPyramid::stream`] failed: the caller's reader, or the pyramid
/// geometry itself.
#[derive(Debug)]
pub enum StreamError<E> {
    Read(E),
    Pyramid(PyramidError),
}

/// A per-frame Gaussian luminance pyramid for alignment: levels `1..=depth`
/// only. Level 0 is never resident.
pub struct AlignPyramid {
    /// `levels[k - 1]` is level `k`.
    levels: Vec<Plane>,
    width: u32,
    height: u32,
}

impl AlignPyramid {
    /// Builds levels `1..=depth` of a `width x height`, `spp`-sample frame
    /// from `read`, which fills a full-width rect of `u16` samples. Level 1
    /// is reduced from row bands of `band_rows` output rows, each read with
    /// a 2-row top and 3-row bottom halo; levels 2.. follow with
    /// [`gaussian_levels`]. Each level is bit-identical to a whole-frame
    /// `reduce` chain's. `depth == 0` reads nothing.
    pub fn stream<E>(
        (width, height, spp): (u32, u32, u32),
        depth: u32,
        band_rows: u32,
        mut read: impl FnMut(Rect, &mut [u16]) -> Result<(), E>,
    ) -> Result<Self, StreamError<E>> {
        if width == 0 || height == 0 {
            return Err(StreamError::Pyramid(PyramidError::EmptyExtent {
                width,
                height,
            }));
        }
        let max = max_levels(width, height);
        if depth > max {
            return Err(StreamError::Pyramid(PyramidError::LevelsTooDeep {
                requested: depth,
                max,
                width,
                height,
            }));
        }
        if depth == 0 {
            return Ok(AlignPyramid {
                levels: Vec::new(),
                width,
                height,
            });
        }

        let band_rows = band_rows.max(1);
        let (w1, h1) = (width.div_ceil(2), height.div_ceil(2));
        let mut level1 = vec![0.0f32; w1 as usize * h1 as usize];

        let band_len = |rows: u32| width as usize * rows as usize * spp as usize;
        let tallest = (2 * band_rows + 5).min(height);
        let mut tile = Tile {
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height: tallest,
            },
            samples_per_pixel: spp,
            samples: vec![0u16; band_len(tallest)],
        };
        record_timing(
            TimingCounter::AllocationBytes,
            (tile.samples.capacity() * std::mem::size_of::<u16>()) as u64,
        );
        record_timing(TimingCounter::AllocationOperations, 1);

        let mut r0 = 0u32;
        while r0 < h1 {
            let r1 = (r0 + band_rows).min(h1);
            let in0 = (2 * r0).saturating_sub(2);
            let in1 = (2 * r1 + 3).min(height);
            let rows = in1 - in0;
            tile.rect = Rect {
                x: 0,
                y: 0,
                width,
                height: rows,
            };
            tile.samples.resize(band_len(rows), 0);
            read(
                Rect {
                    x: 0,
                    y: in0,
                    width,
                    height: rows,
                },
                &mut tile.samples,
            )
            .map_err(StreamError::Read)?;

            let mut assembler = LumaAssembler::new(width, rows);
            assembler
                .push_tile(&tile)
                .expect("the band tile lies inside the band assembler");
            let band = assembler.finish().map_err(StreamError::Pyramid)?;
            let reduced = reduce(&band);

            // Output row 1 of a band that starts below the frame top is the
            // band's first own row: its input halo is two rows.
            let skip = usize::from(r0 > 0) * w1 as usize;
            let own = (r1 - r0) as usize * w1 as usize;
            level1[r0 as usize * w1 as usize..][..own]
                .copy_from_slice(&reduced.data()[skip..skip + own]);
            r0 = r1;
        }

        let level1 = Plane::from_f32_vec(level1, w1, h1, 1).map_err(StreamError::Pyramid)?;
        let levels = gaussian_levels(level1, depth - 1).map_err(StreamError::Pyramid)?;
        Ok(AlignPyramid {
            levels,
            width,
            height,
        })
    }

    /// Level `k`, `1 <= k <= depth`.
    pub fn level(&self, k: usize) -> &Plane {
        &self.levels[k - 1]
    }

    /// The deepest level this pyramid holds; `0` when it holds none.
    pub fn depth(&self) -> u32 {
        self.levels.len() as u32
    }

    /// The base image's `(width, height)` -- level 0's, which is not held.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
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

    /// Seeded `u16` noise, `width * height * spp` samples.
    fn noise(width: u32, height: u32, spp: u32, seed: u64) -> Vec<u16> {
        let mut state = seed;
        (0..width as usize * height as usize * spp as usize)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 48) as u16
            })
            .collect()
    }

    fn read_from(
        samples: &[u16],
        (width, spp): (u32, u32),
    ) -> impl FnMut(Rect, &mut [u16]) -> Result<(), ()> + '_ {
        move |rect, out| {
            assert_eq!((rect.x, rect.width), (0, width), "bands are full width");
            let row = width as usize * spp as usize;
            let start = rect.y as usize * row;
            out.copy_from_slice(&samples[start..start + rect.height as usize * row]);
            Ok(())
        }
    }

    #[test]
    fn streamed_levels_equal_the_whole_frame_reduce_chain() {
        for (width, height) in [(13u32, 7u32), (64, 1030), (1031, 777)] {
            for spp in [1u32, 3] {
                let samples = noise(width, height, spp, u64::from(width) * 31 + u64::from(spp));
                let mut assembler = LumaAssembler::new(width, height);
                let whole = Tile {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                    samples_per_pixel: spp,
                    samples: samples.clone(),
                };
                assembler.push_tile(&whole).unwrap();
                let depth = max_levels(width, height).min(4);
                let expected = gaussian_levels(assembler.finish().unwrap(), depth).unwrap();

                for band_rows in [1u32, 2, 3, 256] {
                    let pyramid = AlignPyramid::stream(
                        (width, height, spp),
                        depth,
                        band_rows,
                        read_from(&samples, (width, spp)),
                    )
                    .unwrap_or_else(|_| panic!("{width}x{height} spp {spp} band {band_rows}"));
                    assert_eq!(pyramid.depth(), depth);
                    assert_eq!(pyramid.dimensions(), (width, height));
                    for (k, level) in expected.iter().enumerate().skip(1) {
                        assert_eq!(
                            pyramid.level(k).data(),
                            level.data(),
                            "{width}x{height} spp {spp} band {band_rows} level {k}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn depth_zero_reads_nothing_and_too_deep_is_refused() {
        let never = |_: Rect, _: &mut [u16]| -> Result<(), ()> { panic!("depth 0 must not read") };
        let pyramid = AlignPyramid::stream((13, 13, 1), 0, 4, never).unwrap();
        assert_eq!((pyramid.depth(), pyramid.dimensions()), (0, (13, 13)));

        let too_deep = AlignPyramid::stream((13, 13, 1), 2, 4, never);
        assert!(matches!(
            too_deep,
            Err(StreamError::Pyramid(PyramidError::LevelsTooDeep { .. }))
        ));
    }

    #[test]
    fn a_failing_read_surfaces_as_a_read_error() {
        let failing = |_: Rect, _: &mut [u16]| Err("disk");
        let result = AlignPyramid::stream((13, 13, 1), 1, 4, failing);
        assert!(matches!(result, Err(StreamError::Read("disk"))));
    }

    #[test]
    fn constant_image_survives_downsampling() {
        // Both `reduce` passes' taps sum to exactly 1 in `f32`, so this is
        // an equality assertion, not a tolerance.
        let samples = vec![1234u16; 32 * 32];
        assert_eq!(max_levels(32, 32), 3);
        let pyramid =
            AlignPyramid::stream((32, 32, 1), 3, 4, read_from(&samples, (32, 1))).unwrap();
        for k in 1..=3 {
            for &v in pyramid.level(k).data() {
                assert_eq!(v, 1234.0f32, "level {k} did not survive as a constant");
            }
        }
    }

    #[test]
    fn finish_moves_the_assembler_buffer_into_the_plane() {
        for (width, height) in [(13u32, 13u32), (7, 5)] {
            let assembler = LumaAssembler::new(width, height);
            let buffer = assembler.data.as_ptr();
            let plane = assembler.finish().unwrap();
            assert_eq!(plane.data().as_ptr(), buffer, "{width}x{height} was copied");
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
