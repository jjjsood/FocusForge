// SPDX-License-Identifier: GPL-3.0-or-later

// GPU twin of `focusforge_core::retouch::blend::apply`: one invocation per
// pixel of the active rectangle. The host keeps coordinates below 2^14 and the
// mask radius below 2^16 sixteenths of a pixel, so every intermediate fits its
// 32-bit word; a squared distance past u32 saturates, which is past every
// radius. `out` and `src` hold one u32 per sample, so invocations never share
// a word. `mask.wgsl` precedes this file and holds the mask.

struct Params {
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
    spp: u32,
    radius_16: u32,
    core_16: u32,
    segments: u32,
    // 0 mixes toward `src`; 1 mixes only where its channel sum is above
    // `out`'s (Lighten), 2 only where it is below (Darken).
    mode: u32,
    opacity_q16: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<storage, read_write> out: array<u32>;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read> segs: array<vec4<i32>>;
@group(0) @binding(3) var<uniform> params: Params;

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
    var alpha = alpha_at(isqrt(d2), params.radius_16, params.core_16);
    alpha = scaled_q16(alpha, params.opacity_q16);
    if (alpha == 0u) {
        return;
    }
    let base = (global_id.y * params.width + global_id.x) * params.spp;
    if (params.mode != 0u) {
        // The host bounds spp by 65537, so a channel sum of 16-bit samples
        // stays within u32.
        var out_sum = 0u;
        var src_sum = 0u;
        for (var k = 0u; k < params.spp; k = k + 1u) {
            out_sum = out_sum + out[base + k];
            src_sum = src_sum + src[base + k];
        }
        let acts = select((src_sum < out_sum), (src_sum > out_sum), params.mode == 1u);
        if (!acts) {
            return;
        }
    }
    // max(out, src) * 65536 + 32768 < 2^32 for 16-bit samples.
    for (var k = 0u; k < params.spp; k = k + 1u) {
        let o = out[base + k];
        let s = src[base + k];
        out[base + k] = (o * (OPAQUE - alpha) + s * alpha + 32768u) >> 16u;
    }
}
