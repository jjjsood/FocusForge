// SPDX-License-Identifier: GPL-3.0-or-later

// The brush mask in the kernels' 32-bit integers, shared by the blend and the
// preview kernels. It declares no binding: the ramp arrives as arguments.
// Shaders include it ahead of their own source.

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
fn alpha_at(d16: u32, radius_16: u32, core_16: u32) -> u32 {
    if (d16 >= radius_16) {
        return 0u;
    }
    if (d16 <= core_16) {
        return OPAQUE;
    }
    return ((radius_16 - d16) << 16u) / (radius_16 - core_16);
}

// `a` scaled by the Q16 factor `q`, rounded to nearest. `a` is at most 65536
// and a scaling `q` at most 65535, so the product plus the rounding term stays
// below 2^32; a `q` of 65536 leaves `a` as it is.
fn scaled_q16(a: u32, q: u32) -> u32 {
    if (q == OPAQUE) {
        return a;
    }
    return (a * q + 32768u) >> 16u;
}
