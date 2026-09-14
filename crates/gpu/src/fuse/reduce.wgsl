// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of `reduce`, split into its two pinned separable passes:
// `horizontal` filters and decimates along x, `vertical` then along y. One
// invocation per output sample, tap order ascending `k` into one `f32`
// accumulator, clamp-to-edge on the signed source index -- the same order
// and border rule as the CPU twin. `params.height` is the height the data
// this pass reads actually has: the plane's unchanged height for both passes.

const KERNEL: array<f32, 5> = array<f32, 5>(
    1.0 / 16.0,
    4.0 / 16.0,
    6.0 / 16.0,
    4.0 / 16.0,
    1.0 / 16.0,
);

struct Params {
    width: u32,
    height: u32,
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

// Filter-and-decimate along x: `width x height` -> `out_width x height`.
@compute @workgroup_size(64)
fn horizontal(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    let total = params.height * params.out_width * params.spp;
    if (i >= total) {
        return;
    }
    let c = i % params.spp;
    let x = (i / params.spp) % params.out_width;
    let y = i / (params.spp * params.out_width);

    var acc: f32 = 0.0;
    for (var k: u32 = 0u; k < 5u; k = k + 1u) {
        let offset = i32(k) - 2;
        let src_x = clamp_index(i32(2u * x) + offset, params.width);
        let idx = (y * params.width + src_x) * params.spp + c;
        acc = acc + KERNEL[k] * src[idx];
    }
    let out_idx = (y * params.out_width + x) * params.spp + c;
    dst[out_idx] = acc;
}

// Filter-and-decimate along y: `width x height` -> `width x out_height`.
// `width` here is the caller's *already-halved* width (the horizontal
// pass's own `out_width`, passed in this pass's `params.width`); `height`
// is the pre-decimation height horizontal left untouched.
@compute @workgroup_size(64)
fn vertical(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    let total = params.out_height * params.width * params.spp;
    if (i >= total) {
        return;
    }
    let c = i % params.spp;
    let x = (i / params.spp) % params.width;
    let y = i / (params.spp * params.width);

    var acc: f32 = 0.0;
    for (var k: u32 = 0u; k < 5u; k = k + 1u) {
        let offset = i32(k) - 2;
        let src_y = clamp_index(i32(2u * y) + offset, params.height);
        let idx = (src_y * params.width + x) * params.spp + c;
        acc = acc + KERNEL[k] * src[idx];
    }
    let out_idx = (y * params.width + x) * params.spp + c;
    dst[out_idx] = acc;
}
