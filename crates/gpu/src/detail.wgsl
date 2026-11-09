// SPDX-License-Identifier: GPL-3.0-or-later

// GPU twin of `focusforge_core::retouch::blend::detail_output`: Detail's box
// blur and its target, both over buffers of one u32 per sample. A window sum
// of 2p + 1 samples of at most 65535 stays below 2^28 for the host's p <= 1365,
// so every sum and quotient is exact in u32.

struct BoxParams {
    width: u32,
    height: u32,
    spp: u32,
    p: u32,
    // 1 slides along x, 0 along y.
    horizontal: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0) var<storage, read> box_src: array<u32>;
@group(0) @binding(1) var<storage, read_write> box_dst: array<u32>;
@group(0) @binding(2) var<uniform> box_params: BoxParams;

// Pixel index of position `along` on `line`.
fn box_pixel(along: u32, line: u32) -> u32 {
    if (box_params.horizontal == 1u) {
        return line * box_params.width + along;
    }
    return along * box_params.width + line;
}

// The sample at `a` along the line, repeating the buffer's edge sample past it.
fn box_at(a: i32, n: i32, line: u32, channel: u32) -> u32 {
    let i = u32(clamp(a, 0, n - 1));
    return box_src[box_pixel(i, line) * box_params.spp + channel];
}

// One box pass of radius p: an invocation slides the window along one line of
// one channel. Invocation x is the line, y the channel.
@compute @workgroup_size(8, 8)
fn box_pass(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let horizontal = box_params.horizontal == 1u;
    let lines = select(box_params.width, box_params.height, horizontal);
    let n = i32(select(box_params.height, box_params.width, horizontal));
    let line = global_id.x;
    let channel = global_id.y;
    if (line >= lines || channel >= box_params.spp) {
        return;
    }
    let p = i32(box_params.p);
    var sum = 0u;
    for (var j = -p; j <= p; j = j + 1) {
        sum = sum + box_at(j, n, line, channel);
    }
    for (var k = 0; k < n; k = k + 1) {
        box_dst[box_pixel(u32(k), line) * box_params.spp + channel] =
            (sum + box_params.p) / (2u * box_params.p + 1u);
        sum = sum + box_at(k + p + 1, n, line, channel) - box_at(k - p, n, line, channel);
    }
}

struct TargetParams {
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
    spp: u32,
    result_x: u32,
    result_y: u32,
    result_width: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    pad0: u32,
}

@group(0) @binding(3) var<storage, read> blurred_result: array<u32>;
@group(0) @binding(4) var<storage, read> source: array<u32>;
@group(0) @binding(5) var<storage, read> blurred_source: array<u32>;
@group(0) @binding(6) var<storage, read_write> detail: array<u32>;
@group(0) @binding(7) var<uniform> target_params: TargetParams;

// The Detail target over the active rectangle, one invocation a pixel:
// blurred result + source - blurred source, clamped to the 16-bit range.
@compute @workgroup_size(8, 8)
fn detail_target(@builtin(global_invocation_id) global_id: vec3<u32>) {
    if (global_id.x >= target_params.width || global_id.y >= target_params.height) {
        return;
    }
    let x = target_params.origin_x + global_id.x;
    let y = target_params.origin_y + global_id.y;
    let spp = target_params.spp;
    let result_at = ((y - target_params.result_y) * target_params.result_width
        + (x - target_params.result_x)) * spp;
    let source_at = ((y - target_params.source_y) * target_params.source_width
        + (x - target_params.source_x)) * spp;
    let out_at = (global_id.y * target_params.width + global_id.x) * spp;
    for (var c = 0u; c < spp; c = c + 1u) {
        let v = i32(blurred_result[result_at + c]) + i32(source[source_at + c])
            - i32(blurred_source[source_at + c]);
        detail[out_at + c] = u32(clamp(v, 0, 65535));
    }
}
