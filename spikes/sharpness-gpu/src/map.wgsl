// SPDX-License-Identifier: GPL-3.0-or-later
//
// The sharpness map of one frame over a tile's interior: the focus score
// divided by the stack scale, zero for a scale that is not positive.

struct Params {
    width: u32,
    ix: u32,
    iy: u32,
    iw: u32,
    ih: u32,
    scale: f32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<storage, read> scores: array<f32>;
@group(0) @binding(1) var<storage, read_write> map: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.iw * params.ih) {
        return;
    }
    let x = params.ix + i % params.iw;
    let y = params.iy + i / params.iw;
    var value = 0.0;
    if (params.scale > 0.0) {
        value = scores[y * params.width + x] / params.scale;
    }
    map[i] = value;
}
