// SPDX-License-Identifier: GPL-3.0-or-later

// The GPU twins of the coverage rule in `focusforge_core::fuse::laplacian::
// Laplacian::fuse`; `per_pixel` is one `u32` per pixel, `data` `spp` `f32`.
// `exclude` writes `fill` over the pixels whose `per_pixel` is 0 (`-inf` into
// a score, `0.0` into a residual); `mean_by_count` divides by `per_pixel`,
// leaving 0 where it is 0.

struct Params {
    len: u32,
    spp: u32,
    fill: f32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> per_pixel: array<u32>;
@group(0) @binding(1) var<storage, read_write> data: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(64)
fn exclude(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.len) {
        return;
    }
    if (per_pixel[i / params.spp] == 0u) {
        data[i] = params.fill;
    }
}

@compute @workgroup_size(64)
fn mean_by_count(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.len) {
        return;
    }
    let n = per_pixel[i / params.spp];
    if (n == 0u) {
        data[i] = 0.0;
    } else {
        data[i] = data[i] / f32(n);
    }
}
