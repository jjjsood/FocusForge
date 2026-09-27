// SPDX-License-Identifier: GPL-3.0-or-later

//! The 16-to-8-bit display reduction and the constants both sides of the
//! GPU/CPU parity claim share -- no tone curve, ordered dither only. Every
//! frame source is a darktable export and is therefore already
//! display-referred, so a second correction here would wash out exactly the
//! midtones the product targets.
//!
//! Unconditional and `wgpu`-free: `crates/gpu`'s WGSL kernel
//! (`crates/gpu/src/display/tonemap.wgsl`) reproduces every expression here
//! operator for operator, integer throughout -- no `f32`, no curve -- which
//! is what makes the two sides agree bit-exactly rather than merely within
//! the standing 1 LSB tolerance.

use rayon::prelude::*;

use crate::zmap::NO_SOURCE;

pub mod downsample;
pub use downsample::{Downsampler, center_crop};
pub mod overlay;
pub use overlay::{alpha_from_opacity, overlay_cpu};

/// The 4x4 ordered (Bayer) dither matrix, values `0..=15`. Ordered, not
/// random, so a still image dithers the same way on every frame and does
/// not shimmer.
#[rustfmt::skip]
pub const BAYER_4X4: [u8; 16] = [
     0,  8,  2, 10,
    12,  4, 14,  6,
     3, 11,  1,  9,
    15,  7, 13,  5,
];

/// The dither threshold at `(x, y)`: [`BAYER_4X4`]'s value for this pixel,
/// scaled into the low byte's `0..=255` range and offset by half a step.
pub fn dither_threshold(x: u32, y: u32) -> u16 {
    u16::from(BAYER_4X4[((y % 4) * 4 + (x % 4)) as usize]) * 16 + 8
}

/// Reduces one 16-bit sample to 8 bits: the high byte, bumped by one if the
/// low byte exceeds this pixel's dither threshold. Integer throughout -- no
/// `f32`, no curve.
pub fn reduce16to8(v: u16, x: u32, y: u32) -> u8 {
    let hi = v >> 8;
    let lo = v & 0xff;
    let bumped = hi + u16::from(lo > dither_threshold(x, y));
    bumped.min(255) as u8
}

/// The Z-Map index-scale uniform: `65535 / (frame_count - 1)`
/// under integer division, `0` when `frame_count <= 1` so a single-frame
/// map divides by nothing. `frame_count` is never `0`
/// (`crate::zmap::ZMapInfo::new` refuses it).
pub fn zmap_index_scale(frame_count: u32) -> u32 {
    if frame_count <= 1 {
        0
    } else {
        65_535 / (frame_count - 1)
    }
}

/// The shared sentinel: what [`crate::zmap::NO_SOURCE`] displays
/// as, read next to it and used by both this twin and the WGSL kernel.
pub const NO_SOURCE_DISPLAY: u8 = 0;

/// The rayon CPU twin of `crates/gpu`'s display kernel, in the RGBA8-output
/// shape [`crate::gpu::self_check_cpu`] already uses.
///
/// `samples_per_pixel == 1` is the Z-Map path: [`NO_SOURCE`] maps straight
/// to [`NO_SOURCE_DISPLAY`] (never through the reduction, so it can never
/// wrap); every other index scales to `min(65535, index * index_scale)`
/// and that 16-bit level goes through [`reduce16to8`], replicated to R, G
/// and B. `samples_per_pixel == 3` is the composite path: `index_scale` is
/// ignored and each channel goes straight through [`reduce16to8`]. Alpha is
/// always `255`.
///
/// `out` must be exactly `width * height * 4` bytes (RGBA8); `src` must be
/// exactly `width * height * samples_per_pixel` `u16` samples.
pub fn tonemap_cpu(
    src: &[u16],
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    index_scale: u32,
    out: &mut [u8],
) {
    debug_assert_eq!(
        src.len(),
        width as usize * height as usize * samples_per_pixel as usize
    );
    debug_assert_eq!(out.len(), width as usize * height as usize * 4);
    out.par_chunks_exact_mut(4)
        .enumerate()
        .for_each(|(i, pixel)| {
            let x = (i as u32) % width;
            let y = (i as u32) / width;
            let (r, g, b) = if samples_per_pixel == 1 {
                let index = src[i];
                if index == NO_SOURCE {
                    (NO_SOURCE_DISPLAY, NO_SOURCE_DISPLAY, NO_SOURCE_DISPLAY)
                } else {
                    let level = (u32::from(index) * index_scale).min(u32::from(u16::MAX)) as u16;
                    let v = reduce16to8(level, x, y);
                    (v, v, v)
                }
            } else {
                let base = i * 3;
                (
                    reduce16to8(src[base], x, y),
                    reduce16to8(src[base + 1], x, y),
                    reduce16to8(src[base + 2], x, y),
                )
            };
            pixel[0] = r;
            pixel[1] = g;
            pixel[2] = b;
            pixel[3] = 255;
        });
}
