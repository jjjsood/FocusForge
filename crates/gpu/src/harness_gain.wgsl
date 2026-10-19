// SPDX-License-Identifier: GPL-3.0-or-later
//
// This kernel exists only to exercise the upload/dispatch/readback path,
// not to do anything a real fusion or warp kernel needs. One invocation
// per packed word (two u16 samples), applying a per-sample gain and clamp.
//
// `floor(clamp(x, 0.0, 65535.0) + 0.5)`, never WGSL's `round`: WGSL's
// `round` is ties-to-even and Rust's `f32::round` (the CPU twin,
// `focusforge_core::gpu::self_check_cpu`) is ties-away-from-zero. The two
// disagree by 1 LSB on an exact tie; `floor(x + 0.5)` is the same
// expression in both languages, so the expected GPU/CPU delta is exactly 0.

struct Params {
    word_count: u32,
    sample_count: u32,
    gain: f32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> params: Params;

fn apply_gain(sample: u32) -> u32 {
    let scaled = clamp(f32(sample) * params.gain, 0.0, 65535.0);
    return u32(floor(scaled + 0.5));
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.word_count) {
        return;
    }

    let word = src[i];
    let lo = word & 0xffffu;
    let hi = word >> 16u;

    let new_lo = apply_gain(lo);
    let new_hi = apply_gain(hi);

    dst[i] = new_lo | (new_hi << 16u);
}
