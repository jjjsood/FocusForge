// SPDX-License-Identifier: GPL-3.0-or-later
//
// Windowed focus score in fixed point: each `|L|` is rounded to a multiple of
// 2^-20, the window is an exact integer sum (lo/hi `u32` pair), converted to
// `f32` once with round-to-nearest-even, so the result is order independent.

struct Params {
    width: u32,
    height: u32,
    spp: u32,
    radius: u32,
}

@group(0) @binding(0) var<storage, read> src: array<f32>;
@group(0) @binding(1) var<storage, read_write> sums: array<vec2<u32>>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(3) var<storage, read_write> scores: array<f32>;

fn clamp_index(v: i32, dim: u32) -> u32 {
    return u32(clamp(v, 0, i32(dim) - 1));
}

// `|v|` rounded half away from zero to a multiple of 2^-20, as (lo, hi).
fn quantize(v: f32) -> vec2<u32> {
    let x = min(abs(v), 1073741824.0) * 1048576.0;
    var r = x;
    if (x < 8388608.0) {
        r = floor(x + 0.5);
    }
    let hi = floor(r / 4294967296.0);
    let lo = r - hi * 4294967296.0;
    return vec2<u32>(u32(lo), u32(hi));
}

fn add64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
    let lo = a.x + b.x;
    var carry = 0u;
    if (lo < a.x) {
        carry = 1u;
    }
    return vec2<u32>(lo, a.y + b.y + carry);
}

// `(hi, lo)` as `f32`, rounded to nearest even; the value is below 2^46.
fn to_f32(a: vec2<u32>) -> f32 {
    let lo = a.x;
    let hi = a.y;
    var n = 32u - countLeadingZeros(lo);
    if (hi > 0u) {
        n = 64u - countLeadingZeros(hi);
    }
    if (n <= 24u) {
        return f32(lo);
    }
    let shift = n - 24u;
    var m = (hi << (32u - shift)) | (lo >> shift);
    let rem = lo & ((1u << shift) - 1u);
    let half = 1u << (shift - 1u);
    if (rem > half || (rem == half && (m & 1u) == 1u)) {
        m = m + 1u;
    }
    return f32(m) * f32(1u << shift);
}

// Reads the detail level (binding 0) and writes row sums (binding 1).
@compute @workgroup_size(64)
fn rows(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.width * params.height) {
        return;
    }
    let x = i % params.width;
    let y = i / params.width;

    var acc = vec2<u32>(0u, 0u);
    let r = i32(params.radius);
    for (var dx: i32 = -r; dx <= r; dx = dx + 1) {
        let sx = clamp_index(i32(x) + dx, params.width);
        for (var c: u32 = 0u; c < params.spp; c = c + 1u) {
            acc = add64(acc, quantize(src[(y * params.width + sx) * params.spp + c]));
        }
    }
    sums[i] = acc;
}

// Sums the row sums (binding 1) over y and writes the scores (binding 3).
@compute @workgroup_size(64)
fn cols(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.width * params.height) {
        return;
    }
    let x = i % params.width;
    let y = i / params.width;

    var acc = vec2<u32>(0u, 0u);
    let r = i32(params.radius);
    for (var dy: i32 = -r; dy <= r; dy = dy + 1) {
        let sy = clamp_index(i32(y) + dy, params.height);
        acc = add64(acc, sums[sy * params.width + x]);
    }
    scores[i] = to_f32(acc) * 0.00000095367431640625;
}
