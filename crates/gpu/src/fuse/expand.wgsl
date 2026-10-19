// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of `expand`, split into its two pinned separable passes,
// target dimensions explicit (never inferred): `horizontal` goes
// `src_width x src_height` -> `out_width x src_height`, `vertical` then
// `out_width x src_height` -> `out_width x out_height`. The even/odd phase
// split and operand order (`0.125*lo + 0.75*mid + 0.125*hi`,
// `0.5*lo + 0.5*hi`) are transcribed verbatim from `expand_1d`.
//
// One struct serves both entry points: `horizontal` reads `src_width` (its
// own 1D source length) and writes to `out_width`, leaving `src_height`
// (the plane's untouched height) and `spp` shared; `vertical` reads
// `src_height` (its own 1D source length, the *original* pre-expand
// height) and writes to `out_height`, over the already-widened `out_width`
// stride `horizontal` produced.

struct Params {
    src_width: u32,
    src_height: u32,
    spp: u32,
    out_width: u32,
    out_height: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<storage, read> src: array<f32>;
@group(0) @binding(1) var<storage, read_write> dst: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

fn clamp_index(v: i32, dim: u32) -> u32 {
    return u32(clamp(v, 0, i32(dim) - 1));
}

@compute @workgroup_size(64)
fn horizontal(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    let total = params.src_height * params.out_width * params.spp;
    if (i >= total) {
        return;
    }
    let c = i % params.spp;
    let x = (i / params.spp) % params.out_width;
    let y = i / (params.spp * params.out_width);

    var value: f32;
    let xi = i32(x);
    if (xi % 2 == 0) {
        let cc = xi / 2;
        let lo_x = clamp_index(cc - 1, params.src_width);
        let mid_x = clamp_index(cc, params.src_width);
        let hi_x = clamp_index(cc + 1, params.src_width);
        let lo = src[(y * params.src_width + lo_x) * params.spp + c];
        let mid = src[(y * params.src_width + mid_x) * params.spp + c];
        let hi = src[(y * params.src_width + hi_x) * params.spp + c];
        value = 0.125 * lo + 0.75 * mid + 0.125 * hi;
    } else {
        let lo_x = clamp_index((xi - 1) / 2, params.src_width);
        let hi_x = clamp_index((xi + 1) / 2, params.src_width);
        let lo = src[(y * params.src_width + lo_x) * params.spp + c];
        let hi = src[(y * params.src_width + hi_x) * params.spp + c];
        value = 0.5 * lo + 0.5 * hi;
    }

    let out_idx = (y * params.out_width + x) * params.spp + c;
    dst[out_idx] = value;
}

@compute @workgroup_size(64)
fn vertical(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    let total = params.out_height * params.out_width * params.spp;
    if (i >= total) {
        return;
    }
    let c = i % params.spp;
    let x = (i / params.spp) % params.out_width;
    let y = i / (params.spp * params.out_width);

    var value: f32;
    let yi = i32(y);
    if (yi % 2 == 0) {
        let cc = yi / 2;
        let lo_y = clamp_index(cc - 1, params.src_height);
        let mid_y = clamp_index(cc, params.src_height);
        let hi_y = clamp_index(cc + 1, params.src_height);
        let lo = src[(lo_y * params.out_width + x) * params.spp + c];
        let mid = src[(mid_y * params.out_width + x) * params.spp + c];
        let hi = src[(hi_y * params.out_width + x) * params.spp + c];
        value = 0.125 * lo + 0.75 * mid + 0.125 * hi;
    } else {
        let lo_y = clamp_index((yi - 1) / 2, params.src_height);
        let hi_y = clamp_index((yi + 1) / 2, params.src_height);
        let lo = src[(lo_y * params.out_width + x) * params.spp + c];
        let hi = src[(hi_y * params.out_width + x) * params.spp + c];
        value = 0.5 * lo + 0.5 * hi;
    }

    let out_idx = (y * params.out_width + x) * params.spp + c;
    dst[out_idx] = value;
}
