// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of the residual sum in `focusforge_core::fuse::laplacian`:
// `(hi, lo) += value[i]` per frame in ascending order as an exact two-sum, so
// `hi + lo` is the running sum beyond `f32` precision; `eligibility.wgsl`
// `mean_by_count` divides once. `hi` and `lo` start zeroed on the host.
// Multiplying the subtracted operands by `params.one` (1.0) keeps a
// reassociating compiler from folding the compensation term away.

struct Params {
    len: u32,
    one: f32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var<storage, read> value: array<f32>;
@group(0) @binding(1) var<storage, read_write> hi: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(3) var<storage, read_write> lo: array<f32>;

@compute @workgroup_size(64)
fn accumulate(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.len) {
        return;
    }
    let a = hi[i];
    let b = value[i];
    let s = a + b;
    let b_virtual = s - a * params.one;
    let a_virtual = s - b_virtual * params.one;
    hi[i] = s;
    lo[i] = lo[i] + ((a - a_virtual) + (b - b_virtual));
}
