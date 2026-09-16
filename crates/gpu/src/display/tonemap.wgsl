// SPDX-License-Identifier: GPL-3.0-or-later
//
// The display kernel: a 16-to-8-bit reduction with ordered dither and no
// tone curve, one invocation per output pixel. Reproduces
// `crates/core/src/display/mod.rs` operator for operator -- integer
// throughout, no `f32` in the reduction path itself -- so the expected
// GPU/CPU delta is exactly 0.
//
// One kernel serves both panes: `samples_per_pixel == 1` is the Z-Map path
// (`NO_SOURCE` -> black, otherwise `min(65535, index * index_scale)`
// reduced and replicated to R/G/B); `samples_per_pixel == 3` is the
// composite path (`index_scale` ignored, each channel reduced
// independently). Alpha is always opaque.

struct Params {
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    index_scale: u32,
}

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<uniform> params: Params;
@group(0) @binding(2) var dst: texture_storage_2d<rgba8unorm, write>;

// The 4x4 ordered (Bayer) dither matrix -- `crate::display::BAYER_4X4`'s
// WGSL twin, values `0..=15`.
const BAYER_4X4 = array<u32, 16>(
    0u, 8u, 2u, 10u,
    12u, 4u, 14u, 6u,
    3u, 11u, 1u, 9u,
    15u, 7u, 13u, 5u,
);

// `crate::zmap::NO_SOURCE`'s WGSL twin: the reserved "no single source"
// index.
const NO_SOURCE: u32 = 65535u;

fn dither_threshold(x: u32, y: u32) -> u32 {
    return BAYER_4X4[(y % 4u) * 4u + (x % 4u)] * 16u + 8u;
}

// `crate::display::reduce16to8`'s WGSL twin.
fn reduce16to8(v: u32, x: u32, y: u32) -> u32 {
    let hi = v >> 8u;
    let lo = v & 0xffu;
    var bumped = hi;
    if (lo > dither_threshold(x, y)) {
        bumped = bumped + 1u;
    }
    return min(bumped, 255u);
}

// One packed-`u32` word holds two `u16` samples, low half first
// (`crates/gpu/src/pack.rs`'s convention).
fn sample_at(index: u32) -> u32 {
    let word = src[index / 2u];
    if (index % 2u == 0u) {
        return word & 0xffffu;
    }
    return word >> 16u;
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.width * params.height) {
        return;
    }
    let x = i % params.width;
    let y = i / params.width;

    var r: u32;
    var g: u32;
    var b: u32;

    if (params.samples_per_pixel == 1u) {
        let index = sample_at(i);
        if (index == NO_SOURCE) {
            r = 0u;
            g = 0u;
            b = 0u;
        } else {
            let level = min(index * params.index_scale, 65535u);
            let v = reduce16to8(level, x, y);
            r = v;
            g = v;
            b = v;
        }
    } else {
        let base = i * 3u;
        r = reduce16to8(sample_at(base), x, y);
        g = reduce16to8(sample_at(base + 1u), x, y);
        b = reduce16to8(sample_at(base + 2u), x, y);
    }

    let color = vec4<f32>(f32(r) / 255.0, f32(g) / 255.0, f32(b) / 255.0, 1.0);
    textureStore(dst, vec2<i32>(i32(x), i32(y)), color);
}
