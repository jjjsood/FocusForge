// SPDX-License-Identifier: GPL-3.0-or-later

// The brush-preview kernel: one invocation per output texel, the WGSL twin of
// `focusforge_core::retouch::preview::brush_preview`. A texel holds the mode's
// full-strength output, reduced to 8 bits with the display's dither, and an
// alpha that is the Q16 mask scaled by the opacity and the strength. Buffers
// hold one u32 per sample over a box of the result's lattice; the host keeps
// every lattice pixel below 2^14 and the mask radius below 2^16 sixteenths of
// a pixel. `mask.wgsl` and `dither.wgsl` precede this file.

struct Params {
    out_width: u32,
    out_height: u32,
    // Lattice position of the output box.
    column: u32,
    row: u32,
    // The lattice box the sample buffers cover, and the footprint box the
    // Detail target covers.
    upload_x: u32,
    upload_y: u32,
    upload_width: u32,
    footprint_x: u32,
    footprint_y: u32,
    footprint_width: u32,
    // Image pixel of lattice sample (0, 0), and the lattice step.
    origin_x: u32,
    origin_y: u32,
    step: u32,
    radius_16: u32,
    core_16: u32,
    // 0 mixes toward the source; 1 only where its channel sum is above the
    // result's (Lighten), 2 only where it is below (Darken).
    mode: u32,
    // 1 takes the Detail target in place of the source.
    detail: u32,
    opacity_q16: u32,
    strength_q16: u32,
    centre_x: i32,
    centre_y: i32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0) var<storage, read> samples_result: array<u32>;
@group(0) @binding(1) var<storage, read> samples_source: array<u32>;
@group(0) @binding(2) var<storage, read> samples_detail: array<u32>;
@group(0) @binding(3) var<uniform> params: Params;
@group(0) @binding(4) var dst: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= params.out_width || id.y >= params.out_height) {
        return;
    }
    let i = params.column + id.x;
    let j = params.row + id.y;
    let px = i32(params.origin_x + i * params.step);
    let py = i32(params.origin_y + j * params.step);
    let centre = vec4<i32>(params.centre_x, params.centre_y, params.centre_x, params.centre_y);
    let mask = alpha_at(
        isqrt(segment_d2_256(px, py, centre)),
        params.radius_16,
        params.core_16,
    );

    let at = ((j - params.upload_y) * params.upload_width + (i - params.upload_x)) * 3u;
    var rgb = vec3<u32>(
        samples_result[at],
        samples_result[at + 1u],
        samples_result[at + 2u],
    );
    if (mask != 0u) {
        var src = vec3<u32>(
            samples_source[at],
            samples_source[at + 1u],
            samples_source[at + 2u],
        );
        if (params.detail == 1u) {
            let t = ((j - params.footprint_y) * params.footprint_width + (i - params.footprint_x)) * 3u;
            src = vec3<u32>(
                samples_detail[t],
                samples_detail[t + 1u],
                samples_detail[t + 2u],
            );
        }
        let out_sum = rgb.x + rgb.y + rgb.z;
        let src_sum = src.x + src.y + src.z;
        var acts = true;
        if (params.mode == 1u) {
            acts = src_sum > out_sum;
        } else if (params.mode == 2u) {
            acts = src_sum < out_sum;
        }
        // Full coverage: the mix at alpha 65536 is the source itself.
        if (acts) {
            rgb = src;
        }
    }

    let a16 = scaled_q16(scaled_q16(mask, params.opacity_q16), params.strength_q16);
    let a8 = (a16 * 255u + 32768u) >> 16u;
    let color = vec4<f32>(
        f32(reduce16to8(rgb.x, id.x, id.y)) / 255.0,
        f32(reduce16to8(rgb.y, id.x, id.y)) / 255.0,
        f32(reduce16to8(rgb.z, id.x, id.y)) / 255.0,
        f32(a8) / 255.0,
    );
    textureStore(dst, vec2<i32>(i32(id.x), i32(id.y)), color);
}
