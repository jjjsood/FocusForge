// SPDX-License-Identifier: GPL-3.0-or-later
//
// Per-pixel peak of the focus score over a tile's frames (`atomicMax` on the
// score's bits, which order like the finite non-negative values) and the
// histogram of the peaks over `bits >> 12`. CPU twins:
// `sharpness_map::{fold_peaks, bin_peaks}`.

struct Params {
    // Read rect of the tile.
    width: u32,
    height: u32,
    // Interior of the tile, relative to the read rect.
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
