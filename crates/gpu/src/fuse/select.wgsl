// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of the per-level selection rule: strictly-greater,
// whole-pixel (never per-channel), frames ascending -- the lowest-indexed
// frame to reach a score wins any exact tie. One invocation per pixel of
// level `k` for one frame `f`.
//
// Binding budget: the natural five storage buffers (current level, current
// score, best level, best score, winner) exceed
// `max_storage_buffers_per_shader_stage = 4` in `downlevel_defaults()`.
// Best score and winner are interleaved into one `array<Best>` (8-byte
// stride) to stay at exactly four storage bindings plus the uniform.
//
// Frame 0 is not special-cased on the host side: `best` is seeded with
// `score = -infinity` before frame 0 runs, so frame 0's real (always
// finite, always >= 0) score is unconditionally greater and this same
// kernel seeds the running best -- bit-identical to the CPU twin's direct
// assignment, since no arithmetic happens on the winning path (only a copy
// and two stores).

struct Best {
    score: f32,
    winner: u32,
}

struct Params {
    width: u32,
    height: u32,
    spp: u32,
    frame: u32,
}

@group(0) @binding(0) var<storage, read> current_level: array<f32>;
@group(0) @binding(1) var<storage, read> current_score: array<f32>;
@group(0) @binding(2) var<storage, read_write> best_level: array<f32>;
@group(0) @binding(3) var<storage, read_write> best: array<Best>;
@group(0) @binding(4) var<uniform> params: Params;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let p = global_id.x;
    let total = params.width * params.height;
    if (p >= total) {
        return;
    }

    if (current_score[p] > best[p].score) {
        let base = p * params.spp;
        for (var c: u32 = 0u; c < params.spp; c = c + 1u) {
            best_level[base + c] = current_level[base + c];
        }
        best[p].score = current_score[p];
        best[p].winner = params.frame;
    }
}
