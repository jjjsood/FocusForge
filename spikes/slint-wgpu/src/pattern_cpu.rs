// SPDX-License-Identifier: GPL-3.0-or-later
//! CPU twin of `pattern.wgsl`. Every constant and every arithmetic step here must mirror the
//! shader exactly, so `--verify-pattern` is a real parity check and not a tautology.

pub const WIDTH: u32 = 512;
pub const HEIGHT: u32 = 512;
const SQ_X0: u32 = 350;
const SQ_X1: u32 = 450;
const SQ_Y0: u32 = 120;
const SQ_Y1: u32 = 220;

/// Recomputes the shader's pattern in `f32` and quantizes to 8-bit RGBA, tightly packed
/// (`width * 4` bytes per row), matching the `Rgba8Unorm` texture layout.
pub fn cpu_pattern(width: u32, height: u32, t: f32) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let fx = x as f32 / (width - 1) as f32;
            let fy = y as f32 / (height - 1) as f32;

            let mut r = fx + t;
            r -= r.floor();
            let g = fy;

            let b = if x >= SQ_X0 && x < SQ_X1 && y >= SQ_Y0 && y < SQ_Y1 {
                1.0
            } else {
                0.0
            };

            let idx = ((y * width + x) * 4) as usize;
            out[idx] = quantize(r);
            out[idx + 1] = quantize(g);
            out[idx + 2] = quantize(b);
            out[idx + 3] = 255;
        }
    }
    out
}

fn quantize(v: f32) -> u8 {
    (v * 255.0 + 0.5) as u8
}
