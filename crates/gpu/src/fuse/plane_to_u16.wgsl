// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of `Plane::to_u16_into`: `floor(clamp(v, 0.0, 65535.0) +
// 0.5)`, never WGSL's `round` (the same reason `harness_gain.wgsl`'s
// `apply_gain` gives). One invocation per packed output word (two `u16`
// samples).

struct Params {
    word_count: u32,
    sample_count: u32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var<storage, read> src: array<f32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> params: Params;

fn to_u16(v: f32) -> u32 {
    let scaled = clamp(v, 0.0, 65535.0);
    return u32(floor(scaled + 0.5));
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.word_count) {
        return;
    }

    let lo_index = i * 2u;
    let lo = to_u16(src[lo_index]);

    var hi: u32 = 0u;
    let hi_index = lo_index + 1u;
    if (hi_index < params.sample_count) {
        hi = to_u16(src[hi_index]);
    }

    dst[i] = lo | (hi << 16u);
}
