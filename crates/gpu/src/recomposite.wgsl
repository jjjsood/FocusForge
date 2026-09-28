// SPDX-License-Identifier: GPL-3.0-or-later

// GPU twin of `focusforge_core::retouch::recomposite::recomposite`: one
// dispatch per named frame; an invocation writes its pixel's samples only
// where the pixel's index equals this dispatch's `params.frame`, so
// `NO_SOURCE` matches none and its output word stays the zero the host
// wrote first. `out` holds one `u32` per sample, never packed two per
// word: a packed word would need a non-atomic read-modify-write to update.

struct Params {
    width: u32,
    height: u32,
    spp: u32,
    frame: u32,
}

@group(0) @binding(0) var<storage, read> indices: array<u32>;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read_write> out: array<u32>;
@group(0) @binding(3) var<uniform> params: Params;

// One packed-`u32` word holds two `u16` samples, low half first
// (`crates/gpu/src/pack.rs`'s convention).
fn index_at(i: u32) -> u32 {
    let word = indices[i / 2u];
    if (i % 2u == 0u) {
        return word & 0xffffu;
    }
    return word >> 16u;
}

fn src_at(i: u32) -> u32 {
    let word = src[i / 2u];
    if (i % 2u == 0u) {
        return word & 0xffffu;
    }
    return word >> 16u;
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let x = global_id.x;
    let y = global_id.y;
    if (x >= params.width || y >= params.height) {
        return;
    }
    let i = y * params.width + x;
    if (index_at(i) != params.frame) {
        return;
    }
    let base = i * params.spp;
    for (var c: u32 = 0u; c < params.spp; c = c + 1u) {
        out[base + c] = src_at(base + c);
    }
}
