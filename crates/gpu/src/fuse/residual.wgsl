// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of the residual accumulation: `sum[i] += value[i]` in
// ascending frame order, then `out[i] = sum[i] / frame_count`. `sum` is
// zero-initialised on the host side and every frame, including frame 0,
// is accumulated through `accumulate` -- `0.0 + x == x` exactly in `f32`,
// so this uniform treatment is bit-identical to the CPU twin's "seed from
// frame 0, then add the rest" order.

struct Params {
    len: u32,
    frame_count: f32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var<storage, read> value: array<f32>;
@group(0) @binding(1) var<storage, read_write> sum: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(64)
fn accumulate(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.len) {
        return;
    }
    sum[i] = sum[i] + value[i];
}

@compute @workgroup_size(64)
fn mean(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.len) {
        return;
    }
    sum[i] = sum[i] / params.frame_count;
}
