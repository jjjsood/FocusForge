// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of the two-pass windowed sharpness score, `score_plane`.
// `rows`: `dx` ascending then `c` ascending, `|L|`, one `f32` accumulator,
// clamp-to-edge on x. `cols`: `dy` ascending over the rows plane,
// clamp-to-edge on y. `radius` is a host-computed uniform from the `pub`
// `score_radius(levels, k)`; this shader never re-derives it.

struct Params {
    width: u32,
    height: u32,
    spp: u32,
    radius: u32,
}

@group(0) @binding(0) var<storage, read> src: array<f32>;
@group(0) @binding(1) var<storage, read_write> dst: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

fn clamp_index(v: i32, dim: u32) -> u32 {
    return u32(clamp(v, 0, i32(dim) - 1));
}

// Row pass: reads the `width*height*spp` Laplacian level, writes one `f32`
// per pixel (row sums) into `dst`.
@compute @workgroup_size(64)
fn rows(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    let total = params.width * params.height;
    if (i >= total) {
        return;
    }
    let x = i % params.width;
    let y = i / params.width;

    var acc: f32 = 0.0;
    let r = i32(params.radius);
    for (var dx: i32 = -r; dx <= r; dx = dx + 1) {
        let sx = clamp_index(i32(x) + dx, params.width);
        for (var c: u32 = 0u; c < params.spp; c = c + 1u) {
            let idx = (y * params.width + sx) * params.spp + c;
            acc = acc + abs(src[idx]);
        }
    }
    dst[i] = acc;
}

// Column pass: reads the row-sum plane (one `f32` per pixel) produced by
// `rows`, writes the final per-pixel score.
@compute @workgroup_size(64)
fn cols(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    let total = params.width * params.height;
    if (i >= total) {
        return;
    }
    let x = i % params.width;
    let y = i / params.width;

    var acc: f32 = 0.0;
    let r = i32(params.radius);
    for (var dy: i32 = -r; dy <= r; dy = dy + 1) {
        let sy = clamp_index(i32(y) + dy, params.height);
        acc = acc + src[sy * params.width + x];
    }
    dst[i] = acc;
}
