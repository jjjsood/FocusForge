// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of the Laplacian level's elementwise combine:
// `L_k = G_k - expand(G_{k+1})` and, on collapse, `A_k = L_k +
// expand(A_{k+1})`. Two entry points rather than one signed-scale variant,
// because `a + (-1.0)*b` and `a - b` are the same value but not the same
// expression -- this file's whole job is to be the same expression as
// those two formulas, not an equivalent one.

struct Params {
    len: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<storage, read> a: array<f32>;
@group(0) @binding(1) var<storage, read> b: array<f32>;
@group(0) @binding(2) var<storage, read_write> out: array<f32>;
@group(0) @binding(3) var<uniform> params: Params;

@compute @workgroup_size(64)
fn sub(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.len) {
        return;
    }
    out[i] = a[i] - b[i];
}

@compute @workgroup_size(64)
fn add(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.len) {
        return;
    }
    out[i] = a[i] + b[i];
}
