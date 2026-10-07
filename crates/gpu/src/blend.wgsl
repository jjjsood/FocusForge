// SPDX-License-Identifier: GPL-3.0-or-later

// GPU twin of `focusforge_core::retouch::blend::apply`: one invocation per
// pixel of the active rectangle. The host keeps coordinates below 2^14 and the
// mask radius below 2^16 sixteenths of a pixel, so every intermediate fits its
// 32-bit word; a squared distance past u32 saturates, which is past every
// radius. `out` and `src` hold one u32 per sample, so invocations never share
// a word.

struct Params {
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
    spp: u32,
    radius_16: u32,
    core_16: u32,
    segments: u32,
}

@group(0) @binding(0) var<storage, read_write> out: array<u32>;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read> segs: array<vec4<i32>>;
@group(0) @binding(3) var<uniform> params: Params;

const SATURATED: u32 = 0xffffffffu;
const OPAQUE: u32 = 65536u;

// 256 * (dx^2 + dy^2). The sum is below 2^31; from 2^24 up the product
// leaves u32.
fn point_d2_256(dx: i32, dy: i32) -> u32 {
    let s = u32(dx * dx + dy * dy);
    if (s >= 0x1000000u) {
        return SATURATED;
    }
    return s << 8u;
}

// c * c for c < 2^31, as (high, low) words.
fn square64(c: u32) -> vec2<u32> {
    let ch = c >> 16u;
    let cl = c & 0xffffu;
    let mid = 2u * ch * cl;
    let lo = cl * cl;
    let low = lo + ((mid & 0xffffu) << 16u);
    let carry = select(0u, 1u, low < lo);
    return vec2<u32>(ch * ch + (mid >> 16u) + carry, low);
}

// floor(256 * cross^2 / len2), saturated. Long division of the 64-bit square
// by len2 < 2^31: the remainder stays below len2, so doubling it fits u32.
// The quotient of cross^2 / len2 is at most the squared distance to the
// segment's start, below 2^31, so its low word holds it all.
fn projected_d2_256(cross: u32, len2: u32) -> u32 {
    let sq = square64(cross);
    var r = 0u;
    var q = 0u;
    for (var i = 63; i >= 0; i = i - 1) {
        let word = select(sq.y, sq.x, i >= 32);
        r = (r << 1u) | ((word >> u32(i & 31)) & 1u);
        q = q << 1u;
        if (r >= len2) {
            r = r - len2;
            q = q | 1u;
        }
    }
    if (q >= 0x1000000u) {
        return SATURATED;
    }
    // Eight more quotient bits give the 256ths.
    var f = 0u;
    for (var k = 0; k < 8; k = k + 1) {
        r = r << 1u;
        f = f << 1u;
        if (r >= len2) {
            r = r - len2;
            f = f | 1u;
        }
    }
    return (q << 8u) | f;
}

// 256 times the squared distance from (px, py) to the segment, rounded down.
fn segment_d2_256(px: i32, py: i32, s: vec4<i32>) -> u32 {
    let dx = s.z - s.x;
    let dy = s.w - s.y;
    let ux = px - s.x;
    let uy = py - s.y;
    let len2 = dx * dx + dy * dy;
    let along = ux * dx + uy * dy;
    if (len2 == 0 || along <= 0) {
        return point_d2_256(ux, uy);
    }
    if (along >= len2) {
        return point_d2_256(px - s.z, py - s.w);
    }
    return projected_d2_256(u32(abs(ux * dy - uy * dx)), u32(len2));
}

// floor(sqrt(value)), digit by digit.
fn isqrt(value: u32) -> u32 {
    var n = value;
    var res = 0u;
    var bit = 1u << 30u;
    while (bit > n) {
        bit = bit >> 2u;
    }
    while (bit != 0u) {
        if (n >= res + bit) {
            n = n - (res + bit);
            res = (res >> 1u) + bit;
        } else {
            res = res >> 1u;
        }
        bit = bit >> 2u;
    }
    return res;
}

// Q16 alpha at distance d16 sixteenths: zero from the radius out, opaque
// inside the core, linear between. `radius - d16` is below 2^16, so the
// shifted numerator fits u32.
fn alpha_at(d16: u32) -> u32 {
    if (d16 >= params.radius_16) {
        return 0u;
    }
    if (d16 <= params.core_16) {
        return OPAQUE;
    }
    return ((params.radius_16 - d16) << 16u) / (params.radius_16 - params.core_16);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    if (global_id.x >= params.width || global_id.y >= params.height) {
        return;
    }
    let px = i32(params.origin_x + global_id.x);
    let py = i32(params.origin_y + global_id.y);
    var d2 = SATURATED;
    for (var i = 0u; i < params.segments; i = i + 1u) {
        d2 = min(d2, segment_d2_256(px, py, segs[i]));
    }
    let alpha = alpha_at(isqrt(d2));
    if (alpha == 0u) {
        return;
    }
    // max(out, src) * 65536 + 32768 < 2^32 for 16-bit samples.
    let base = (global_id.y * params.width + global_id.x) * params.spp;
    for (var k = 0u; k < params.spp; k = k + 1u) {
        let o = out[base + k];
        let s = src[base + k];
        out[base + k] = (o * (OPAQUE - alpha) + s * alpha + 32768u) >> 16u;
    }
}
