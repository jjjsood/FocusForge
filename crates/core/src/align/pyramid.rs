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
//! (a third of the 179 MB level-0 plane). Level 1 is built row by row from
//! bands of input rows, each read once, and level 0 is only ever read as a
//! bounded window around the solve region.
//!
//! An anchored run holds one *shared* term for the whole run
//! ([`crate::align::driver::bytes_shared_reference`]): the reference pyramid
//! plus the reference templates the solver retains per level. Each concurrent
//! registration adds its own frame pyramid, a streaming band or the solve
//! scratch ([`crate::align::driver::bytes_per_registration`]). At the
//! reference scale that is about 254 MB shared and 233 MB per registration.

use crate::align::error::AlignError;
use crate::fuse::PyramidError;
pub use crate::fuse::pyramid::max_levels;
use crate::fuse::pyramid::{KERNEL, Plane, gaussian_levels};
use crate::tile::{Rect, Sample, Tile};
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
        self.push_samples(tile.rect, tile.samples_per_pixel, &tile.samples)
    }

    /// [`Self::push_tile`] for the interleaved `samples` of `rect`, in any
    /// [`Sample`] type.
    pub fn push_samples<S: Sample>(
        &mut self,
        rect: Rect,
        samples_per_pixel: u32,
        samples: &[S],
    ) -> Result<(), AlignError> {
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

        let spp = samples_per_pixel as usize;
        for y in 0..rect.height {
            for x in 0..rect.width {
                let pixel_start = (y as usize * rect.width as usize + x as usize) * spp;
                let pixel = &samples[pixel_start..pixel_start + spp];
                let v = crate::fuse::metrics::luma(pixel, samples_per_pixel) as f32;
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

/// Horizontal 5-tap reduce of one luma row into `ceil(w / 2)` samples:
/// clamp-to-edge taps added in ascending order, exactly as
/// [`crate::fuse::pyramid::reduce`]'s first pass.
fn reduce_row(src: &[f32], dst: &mut [f32]) {
    let last = src.len() - 1;
    for (x, d) in dst.iter_mut().enumerate() {
        let at = |k: usize| src[(2 * x + k).saturating_sub(2).min(last)];
        let mut acc = 0.0f32;
        acc += KERNEL[0] * at(0);
        acc += KERNEL[1] * at(1);
        acc += KERNEL[2] * at(2);
        acc += KERNEL[3] * at(3);
        acc += KERNEL[4] * at(4);
        *d = acc;
    }
}

impl AlignPyramid {
    /// Output rows of level 1 per streamed band.
    pub const BAND_ROWS: u32 = 64;

    /// Builds levels `1..=depth` of a `width x height`, `spp`-sample frame
    /// from `read`, which fills a full-width rect of `S` samples. Each input
    /// row is read once, in order, in rects of at most `2 * band_rows + 5`
    /// rows. Level 1 comes from a ring of horizontally reduced rows that the
    /// vertical reduce drains `band_rows` output rows at a time; levels 2..
    /// follow with [`gaussian_levels`]. Each level is bit-identical to a
    /// whole-frame `reduce` chain's. `depth == 0` reads nothing.
    pub fn stream<S: Sample, E>(
        (width, height, spp): (u32, u32, u32),
        depth: u32,
        band_rows: u32,
        mut read: impl FnMut(Rect, &mut [S]) -> Result<(), E>,
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

        let (w1, h1) = (width.div_ceil(2), height.div_ceil(2));
        // A band taller than the level changes neither the result nor the reads.
        let band_rows = band_rows.clamp(1, h1);
        let mut level1 = vec![0.0f32; w1 as usize * h1 as usize];

        let (wu, w1u, spp_u) = (width as usize, w1 as usize, spp as usize);
        // The first band reads `2 * band_rows + 3` rows, later ones
        // `2 * band_rows`; the ring holds the reduced rows `[lo, hi)` a band's
        // taps span.
        let mut raw = vec![S::default(); (2 * band_rows + 3).min(height) as usize * wu * spp_u];
        let mut ring = vec![0.0f32; (2 * band_rows + 5).min(height) as usize * w1u];
        let mut luma_row = vec![0.0f32; wu];
        record_timing(
            TimingCounter::AllocationBytes,
            (raw.capacity() * std::mem::size_of::<S>()
                + (ring.capacity() + luma_row.capacity()) * std::mem::size_of::<f32>())
                as u64,
        );
        record_timing(TimingCounter::AllocationOperations, 1);

        let (mut lo, mut hi) = (0u32, 0u32);
        let mut r0 = 0u32;
        while r0 < h1 {
            let r1 = (r0 + band_rows).min(h1);
            let need_lo = (2 * r0).saturating_sub(2);
            let need_hi = (2 * r1 + 3).min(height);

            // Reduced rows of the previous band that this band still needs.
            let kept = hi.saturating_sub(need_lo);
            if kept > 0 {
                ring.copy_within((need_lo - lo) as usize * w1u..(hi - lo) as usize * w1u, 0);
            }
            let start = need_lo.max(hi);
            let new_rows = need_hi - start;
            if new_rows > 0 {
                let rows = &mut raw[..new_rows as usize * wu * spp_u];
                read(
                    Rect {
                        x: 0,
                        y: start,
                        width,
                        height: new_rows,
                    },
                    rows,
                )
                .map_err(StreamError::Read)?;
                for (i, raw_row) in rows.chunks_exact(wu * spp_u).enumerate() {
                    for (l, pixel) in luma_row.iter_mut().zip(raw_row.chunks_exact(spp_u)) {
                        *l = crate::fuse::metrics::luma(pixel, spp) as f32;
                    }
                    reduce_row(&luma_row, &mut ring[(kept as usize + i) * w1u..][..w1u]);
                }
            }
            (lo, hi) = (need_lo, need_hi);

            for r in r0..r1 {
                let tap = |k: u32| {
                    let y = (2 * r + k).saturating_sub(2).min(height - 1);
                    &ring[(y - lo) as usize * w1u..][..w1u]
                };
                let (t0, t1, t2, t3, t4) = (tap(0), tap(1), tap(2), tap(3), tap(4));
                for (x, out) in level1[r as usize * w1u..][..w1u].iter_mut().enumerate() {
                    let mut acc = 0.0f32;
                    acc += KERNEL[0] * t0[x];
                    acc += KERNEL[1] * t1[x];
                    acc += KERNEL[2] * t2[x];
                    acc += KERNEL[3] * t3[x];
                    acc += KERNEL[4] * t4[x];
                    *out = acc;
                }
            }
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
        let sizes = [
            (8u32, 8u32),
            (9, 8),
            (8, 9),
            (64, 9),
            (31, 70),
            (64, 1030),
            (1031, 777),
        ];
        for (width, height) in sizes {
            for spp in [1u32, 3, 4] {
                let samples = noise(width, height, spp, u64::from(width) * 31 + u64::from(spp));
                let wide: Vec<f32> = samples.iter().map(|s| f32::from(*s)).collect();
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
                assert!(depth >= 1, "{width}x{height} streams no level");
                let expected = gaussian_levels(assembler.finish().unwrap(), depth).unwrap();
                let row = width as usize * spp as usize;

                for band_rows in [1u32, 2, 3, 5, 32, 64, 256] {
                    let narrow = AlignPyramid::stream(
                        (width, height, spp),
                        depth,
                        band_rows,
                        read_from(&samples, (width, spp)),
                    )
                    .unwrap_or_else(|_| panic!("{width}x{height} spp {spp} band {band_rows}"));
                    let widened = AlignPyramid::stream::<_, ()>(
                        (width, height, spp),
                        depth,
                        band_rows,
                        |rect, out: &mut [f32]| {
                            out.copy_from_slice(&wide[rect.y as usize * row..][..out.len()]);
                            Ok(())
                        },
                    )
                    .unwrap_or_else(|_| panic!("f32 {width}x{height} spp {spp} band {band_rows}"));
                    for pyramid in [&narrow, &widened] {
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
    }

    #[test]
    fn stream_reads_every_input_row_exactly_once_in_order() {
        let width = 8u32;
        for height in [8u32, 9, 70, 200] {
            for band_rows in [1u32, 2, 5, 32, 64] {
                let mut reads: Vec<Rect> = Vec::new();
                AlignPyramid::stream::<u16, ()>((width, height, 1), 1, band_rows, |rect, out| {
                    reads.push(rect);
                    out.fill(7);
                    Ok(())
                })
                .unwrap_or_else(|_| panic!("{width}x{height} band {band_rows}"));

                let mut next = 0u32;
                for rect in &reads {
                    assert_eq!(
                        (rect.x, rect.width, rect.y),
                        (0, width, next),
                        "{width}x{height} band {band_rows}: reads abut in order"
                    );
                    assert!(
                        rect.height >= 1 && rect.height <= 2 * band_rows + 5,
                        "{width}x{height} band {band_rows}: read of {} rows",
                        rect.height
                    );
                    next += rect.height;
                }
                assert_eq!(next, height, "{width}x{height} band {band_rows}: rows read");
            }
        }
    }

    #[test]
    fn a_failing_later_band_surfaces_as_a_read_error() {
        let mut calls = 0u32;
        let result = AlignPyramid::stream((13, 40, 1), 1, 2, |_: Rect, out: &mut [u16]| {
            calls += 1;
            out.fill(1);
            if calls == 2 { Err("disk") } else { Ok(()) }
        });
        assert!(matches!(result, Err(StreamError::Read("disk"))));
        assert_eq!(calls, 2, "no read follows the failed one");
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

    #[test]
    fn stream_of_f32_equals_the_u16_stream_on_integral_input() {
        let (width, height, spp) = (37u32, 23u32, 3u32);
        let samples = noise(width, height, spp, 11);
        let wide: Vec<f32> = samples.iter().map(|s| f32::from(*s)).collect();
        let source = |rect: Rect| {
            let mut rows = Vec::new();
            for y in rect.y..rect.y + rect.height {
                let start = (y * width + rect.x) as usize * spp as usize;
                rows.push(start..start + rect.width as usize * spp as usize);
            }
            rows
        };
        let narrow = AlignPyramid::stream::<_, ()>((width, height, spp), 2, 3, |rect, out| {
            let mut at = 0;
            for range in source(rect) {
                out[at..at + range.len()].copy_from_slice(&samples[range.clone()]);
                at += range.len();
            }
            Ok(())
        })
        .unwrap_or_else(|_| panic!("u16 stream"));
        let widened =
            AlignPyramid::stream::<_, ()>((width, height, spp), 2, 3, |rect, out: &mut [f32]| {
                let mut at = 0;
                for range in source(rect) {
                    out[at..at + range.len()].copy_from_slice(&wide[range.clone()]);
                    at += range.len();
                }
                Ok(())
            })
            .unwrap_or_else(|_| panic!("f32 stream"));
        for k in 1..=2 {
            assert_eq!(narrow.level(k), widened.level(k), "level {k}");
        }
    }

    #[test]
    fn stream_of_f32_keeps_values_above_65535() {
        let pyramid = AlignPyramid::stream::<_, ()>((16, 12, 1), 1, 2, |_, out: &mut [f32]| {
            out.fill(98_304.0);
            Ok(())
        })
        .unwrap_or_else(|_| panic!("f32 stream"));
        assert!(pyramid.level(1).data().iter().all(|v| *v == 98_304.0));
    }
}
