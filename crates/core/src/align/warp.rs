// SPDX-License-Identifier: GPL-3.0-or-later

//! The tiled warp: a destination-driven bicubic Catmull-Rom resampler,
//! held to **1 LSB** at 16-bit against its GPU twin. Every ordering,
//! rounding and border choice below is binding for that parity test, not
//! incidental.
//!
//! # Kernel
//!
//! Bicubic Catmull-Rom, `a = -0.5`, four taps per axis, weights in Horner
//! form (`t` the `f32` fractional part of the source coordinate):
//!
//! ```text
//! w[-1] = t*(t*(-0.5*t + 1.0) - 0.5)
//! w[ 0] = t*t*(1.5*t - 2.5) + 1.0
//! w[ 1] = t*(t*(-1.5*t + 2.0) + 0.5)
//! w[ 2] = t*t*(0.5*t - 0.5)
//! ```
//!
//! The four weights sum to exactly `1` for every `t`, and are exactly
//! `[0, 1, 0, 0]` at `t == 0` -- which is what makes an identity warp and an
//! integer-pixel translation bit-exact: at `t == 0` the kernel degenerates
//! to picking the single sample at `w[0]`'s tap with no blending.
//!
//! # Accumulation order and precision
//!
//! Coordinates are `f64` (`Transform::map_point`); the fractional part `t`
//! is formed in `f64` and cast to `f32` **exactly once**. Sample
//! accumulation is `f32` throughout, in a **pinned order** because `f32`
//! addition is not associative: horizontal first, four taps in increasing
//! tap index, then the four row results combined vertically in increasing
//! tap index -- mirroring the pass order `crate::fuse::pyramid` pins.
//!
//! The parity contract this precision scheme buys is **measured, not
//! structural**: "≤ 1 LSB on the fixture," because WGSL has no `f64`, so an
//! `f64` coordinate path can never be bit-exact against its GPU twin --
//! only close enough for the fixtures actually tested. It can exceed 1 LSB
//! where large coordinates meet a steep gradient: `ulp(8192.0f32) ~= 9.8e-4`
//! px of coordinate rounding, against a ~1117 LSB/px neighbour delta (the
//! fixture's measured worst case), is already ~1 LSB of sample error from
//! rounding alone. The 512 px fixture never reaches that regime, which is
//! why the parity test (`crates/core/tests/warp.rs`,
//! `coordinate_precision_worst_case_is_measured`) uses a far-origin
//! synthetic profile instead. Re-measure this claim whenever the kernel
//! changes.
//!
//! Out-conversion is copied verbatim from `crate::fuse::pyramid`:
//!
//! ```text
//! (v.clamp(0.0, 65535.0) + 0.5).floor() as u16
//! ```
//!
//! written as that expression and **never** as the word "round" --
//! Rust's `f32::round` is half-away-from-zero, WGSL's `round()` is
//! ties-to-even, and the word alone is a **1 LSB** parity failure by
//! construction. `u16 as f32` in is lossless.
//!
//! # Border taps and coverage
//!
//! A destination pixel's coverage is decided by its **centre**:
//! `map_point` returning `None`, or a mapped coordinate outside
//! `0.0 ..= (dim - 1) as f64` on the source frame, makes the pixel
//! uncovered and its sample `0` -- independent of the kernel radius. A
//! covered pixel's own 4x4 tap window may still reach past the source
//! frame edge; those tap *indices* are clamped to the edge exactly as
//! `crate::fuse::pyramid` clamps its reduce/expand taps, and the pixel
//! stays covered.

use crate::algorithm::{AlgorithmError, TileReader};
use crate::align::error::AlignError;
use crate::align::transform::Transform;
use crate::tile::{Rect, Tile};

/// The four Catmull-Rom (`a = -0.5`) weights for fractional offset `t`
/// (`0.0 <= t < 1.0`), in tap order `[-1, 0, 1, 2]` relative to
/// `floor(coordinate)`.
///
/// Written in Horner form; sums to exactly `1.0` for every `t`, and is
/// exactly `[0.0, 1.0, 0.0, 0.0]` at `t == 0.0`.
///
/// `pub(crate)`: `crate::align::ecc`'s inner-loop sampler reuses this
/// kernel, so there is one interpolation kernel in the crate.
pub(crate) fn catmull_rom_weights(t: f32) -> [f32; 4] {
    [
        t * (t * (-0.5 * t + 1.0) - 0.5),
        t * t * (1.5 * t - 2.5) + 1.0,
        t * (t * (-1.5 * t + 2.0) + 0.5),
        t * t * (0.5 * t - 0.5),
    ]
}

/// Converts one accumulated `f32` sample to `u16`: clamp to `[0, 65535]`,
/// then round half-up. Copied verbatim from `crate::fuse::pyramid::Plane::to_u16_into`.
#[inline]
fn to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 65535.0) + 0.5).floor() as u16
}

/// The output of a tiled warp: the resampled [`Tile`] plus per-pixel
/// coverage beside it. `covered.len() == rect.width * rect.height`, one
/// entry per **pixel**, never per sample -- the same invariant and wording
/// as `crate::algorithm::TileOutput`'s Z-Map. `Tile` itself carries no
/// coverage; a destination pixel this warp could not cover has sample `0`
/// in `tile` and `false` at the matching index in `covered`.
#[derive(Debug, Clone, PartialEq)]
pub struct WarpedTile {
    pub tile: Tile,
    pub covered: Vec<bool>,
}

/// Clamps a signed tap coordinate to `0..dim` (clamp-to-edge), returning it
/// as `i64` for further arithmetic.
#[inline]
pub fn clamp_tap(v: i64, dim: u32) -> i64 {
    v.clamp(0, dim as i64 - 1)
}

/// Warps `dst` out of `src` (whose own `rect` is in the same absolute
/// `frame`-pixel coordinate space) through `t`, writing samples into
/// `out_samples` and coverage into `out_covered`.
///
/// `out_samples.len()` must equal exactly
/// `dst.width * dst.height * src.samples_per_pixel`
/// ([`AlignError::BufferSize`] otherwise) and `out_covered.len()` must equal
/// exactly `dst.width * dst.height`. A covered destination pixel whose
/// (edge-clamped) 4x4 tap window is not fully inside `src.rect` is
/// [`AlignError::SourceTileTooSmall`] -- never clamped to `src`, never
/// silently partial.
pub fn warp_from_tile_into(
    src: &Tile,
    frame: (u32, u32),
    dst: Rect,
    t: &Transform,
    out_samples: &mut [u16],
    out_covered: &mut [bool],
) -> Result<(), AlignError> {
    let spp = src.samples_per_pixel as usize;
    let pixel_count = dst.width as usize * dst.height as usize;
    let expected_samples = pixel_count * spp;
    if out_samples.len() != expected_samples {
        return Err(AlignError::BufferSize {
            expected: expected_samples,
            got: out_samples.len(),
        });
    }
    if out_covered.len() != pixel_count {
        return Err(AlignError::BufferSize {
            expected: pixel_count,
            got: out_covered.len(),
        });
    }

    let (frame_w, frame_h) = frame;
    let src_stride = src.rect.width as usize * spp;

    // Reads one sample of channel `c` at absolute frame coordinates
    // `(col, row)`, which must already be edge-clamped into `0..frame_w` /
    // `0..frame_h` and verified inside `src.rect`.
    let sample = |col: i64, row: i64, c: usize| -> f32 {
        let x = (col as u32 - src.rect.x) as usize;
        let y = (row as u32 - src.rect.y) as usize;
        f32::from(src.samples[y * src_stride + x * spp + c])
    };

    for dy in 0..dst.height {
        for dx in 0..dst.width {
            let pixel_idx = dy as usize * dst.width as usize + dx as usize;
            let ddx = (dst.x + dx) as f64;
            let ddy = (dst.y + dy) as f64;

            let mapped = t.map_point(ddx, ddy);
            let covered_centre = mapped.is_some_and(|(sx, sy)| {
                sx >= 0.0 && sx <= (frame_w - 1) as f64 && sy >= 0.0 && sy <= (frame_h - 1) as f64
            });

            if !covered_centre {
                out_covered[pixel_idx] = false;
                for c in 0..spp {
                    out_samples[pixel_idx * spp + c] = 0;
                }
                continue;
            }

            let (sx, sy) = mapped.expect("covered_centre implies mapped is Some");
            let ix = sx.floor() as i64;
            let iy = sy.floor() as i64;
            let tx = (sx - ix as f64) as f32;
            let ty = (sy - iy as f64) as f32;

            let wx = catmull_rom_weights(tx);
            let wy = catmull_rom_weights(ty);

            let cols: [i64; 4] = [
                clamp_tap(ix - 1, frame_w),
                clamp_tap(ix, frame_w),
                clamp_tap(ix + 1, frame_w),
                clamp_tap(ix + 2, frame_w),
            ];
            let rows: [i64; 4] = [
                clamp_tap(iy - 1, frame_h),
                clamp_tap(iy, frame_h),
                clamp_tap(iy + 1, frame_h),
                clamp_tap(iy + 2, frame_h),
            ];

            let needed = Rect {
                x: cols[0] as u32,
                y: rows[0] as u32,
                width: (cols[3] - cols[0] + 1) as u32,
                height: (rows[3] - rows[0] + 1) as u32,
            };
            let fits = needed.x >= src.rect.x
                && needed.y >= src.rect.y
                && needed.x + needed.width <= src.rect.x + src.rect.width
                && needed.y + needed.height <= src.rect.y + src.rect.height;
            if !fits {
                return Err(AlignError::SourceTileTooSmall {
                    needed,
                    have: src.rect,
                });
            }

            for c in 0..spp {
                let mut row_results = [0f32; 4];
                for (j, &row) in rows.iter().enumerate() {
                    row_results[j] = wx[0] * sample(cols[0], row, c)
                        + wx[1] * sample(cols[1], row, c)
                        + wx[2] * sample(cols[2], row, c)
                        + wx[3] * sample(cols[3], row, c);
                }
                let value = wy[0] * row_results[0]
                    + wy[1] * row_results[1]
                    + wy[2] * row_results[2]
                    + wy[3] * row_results[3];
                out_samples[pixel_idx * spp + c] = to_u16(value);
            }
            out_covered[pixel_idx] = true;
        }
    }

    Ok(())
}

/// Convenience over [`warp_from_tile_into`] that allocates its own output.
pub fn warp_from_tile(
    src: &Tile,
    frame: (u32, u32),
    dst: Rect,
    t: &Transform,
) -> Result<WarpedTile, AlignError> {
    let spp = src.samples_per_pixel;
    let pixel_count = dst.width as usize * dst.height as usize;
    let mut samples = vec![0u16; pixel_count * spp as usize];
    let mut covered = vec![false; pixel_count];
    warp_from_tile_into(src, frame, dst, t, &mut samples, &mut covered)?;
    Ok(WarpedTile {
        tile: Tile {
            rect: dst,
            samples_per_pixel: spp,
            samples,
        },
        covered,
    })
}

/// Warps destination tile `dst` of `frame_index` (a `frame`-sized frame)
/// through `t`, reading only the source rectangle
/// [`Transform::source_footprint`] reports.
///
/// On [`crate::align::SourceFootprint::Outside`], returns an all-uncovered,
/// all-zero [`WarpedTile`] **without reading anything**. Otherwise reads
/// exactly the reported rectangle through `reader` and resamples it. One
/// source tile, one destination tile, no whole-frame buffer anywhere.
///
/// No [`TileReader`] implementation lives here: that adapter is the
/// scheduler seam `crate::tile::scheduler` reserves.
pub fn warp_tile(
    reader: &dyn TileReader,
    frame_index: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    dst: Rect,
    t: &Transform,
) -> Result<WarpedTile, AlignError> {
    match t.source_footprint(dst, frame)? {
        crate::align::SourceFootprint::Outside => {
            let pixel_count = dst.width as usize * dst.height as usize;
            Ok(WarpedTile {
                tile: Tile {
                    rect: dst,
                    samples_per_pixel,
                    samples: vec![0u16; pixel_count * samples_per_pixel as usize],
                },
                covered: vec![false; pixel_count],
            })
        }
        crate::align::SourceFootprint::Covers { read, .. } => {
            let mut buf =
                vec![0u16; read.width as usize * read.height as usize * samples_per_pixel as usize];
            reader.read_tile_into(frame_index, read, &mut buf).map_err(
                |source: AlgorithmError| AlignError::Read {
                    frame: frame_index,
                    rect: read,
                    source,
                },
            )?;
            let src = Tile {
                rect: read,
                samples_per_pixel,
                samples: buf,
            };
            warp_from_tile(&src, frame, dst, t)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catmull_rom_weights_sum_to_one() {
        let mut t = 0.0f32;
        while t < 1.0 {
            let w = catmull_rom_weights(t);
            let sum = w[0] + w[1] + w[2] + w[3];
            assert!(
                (sum - 1.0).abs() < 1e-5,
                "t={t}: weights {w:?} sum to {sum}"
            );
            t += 0.01;
        }
    }

    #[test]
    fn catmull_rom_weights_are_one_hot_at_t_zero() {
        assert_eq!(catmull_rom_weights(0.0), [0.0, 1.0, 0.0, 0.0]);
    }
}
