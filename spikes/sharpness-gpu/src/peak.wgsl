// SPDX-License-Identifier: GPL-3.0-or-later
//
// Per-pixel peak of the focus score over the frames of one tile, and the
// histogram of those peaks over `bits >> 12`, the bins `stack_sharpness_scale`
// uses. Scores are finite and non-negative, so the `u32` bit pattern of an
// `f32` orders like the value and `atomicMax` on it is a float max.

struct Params {
    width: u32,
    height: u32,
    ix: u32,
    iy: u32,
    iw: u32,
    ih: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<storage, read> scores: array<f32>;
@group(0) @binding(1) var<storage, read_write> peaks: array<atomic<u32>>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(3) var<storage, read_write> hist: array<atomic<u32>>;

// One invocation per pixel of the read rect: peaks = max(peaks, scores).
@compute @workgroup_size(64)
fn fold(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.width * params.height) {
        return;
    }
    atomicMax(&peaks[i], bitcast<u32>(scores[i]));
}

// One invocation per pixel of the interior: counts its peak's bin.
@compute @workgroup_size(64)
fn bin(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.iw * params.ih) {
        return;
    }
    let x = params.ix + i % params.iw;
    let y = params.iy + i / params.iw;
    let peak = atomicLoad(&peaks[y * params.width + x]);
    atomicAdd(&hist[peak >> 12u], 1u);
}
