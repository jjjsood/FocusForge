// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of `Plane::from_u16`: `u16 as f32`, lossless. One
// invocation per packed word (two `u16` samples), following the
// packed-`u32` transport's unpack shape (`harness_gain.wgsl`'s `main`).

struct Params {
    word_count: u32,
    sample_count: u32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.word_count) {
        return;
    }

    let word = src[i];
    let lo = word & 0xffffu;
    let hi = word >> 16u;

    let lo_index = i * 2u;
    dst[lo_index] = f32(lo);

    let hi_index = lo_index + 1u;
    if (hi_index < params.sample_count) {
        dst[hi_index] = f32(hi);
    }
}
