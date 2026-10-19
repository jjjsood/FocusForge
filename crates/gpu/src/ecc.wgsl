// SPDX-License-Identifier: GPL-3.0-or-later
//
// One ECC iteration's sampling and masked reductions; the CPU twin is
// `focusforge_core::align::CpuMomentSession`. Region pixels map to window
// coordinates in f32 relative to the window origin; a sample whose 4x4
// Catmull-Rom taps leave the window is invalid.

// Four dispatches per iteration, each a fixed grid-stride count of 256-wide
// workgroups: `sample_moments` and `template_moments` write one partial row of
// 18 slots per workgroup, `reduce_sample` and `reduce_template` fold the rows
// into `result`. Slots follow the host's moment order: [n, sum s, sum s^2,
// sum t*s, sum g_k*s (4), sum t, sum t^2, sum g_k*t (4), sum g_k (4)], with s
// and t taken minus `shift`.

struct Params {
    a: f32,
    b: f32,
    bx: f32,
    shift: f32,
    c: f32,
    d: f32,
    by: f32,
    pad0: f32,
    rw: u32,
    n: u32,
    win_w: u32,
    win_h: u32,
    groups: u32,
    cols: u32,
    full_n: f32,
    pad1: u32,
}

const STRIDE: u32 = 18u;
const WG: u32 = 256u;

@group(0) @binding(0) var<storage, read> moving: array<f32>;
@group(0) @binding(1) var<storage, read> tmpl: array<f32>;
@group(0) @binding(2) var<storage, read> jac: array<f32>;
@group(0) @binding(3) var<storage, read_write> partials: array<f32>;
@group(0) @binding(4) var<storage, read_write> result: array<f32>;
@group(0) @binding(5) var<uniform> p: Params;

var<workgroup> sh: array<f32, 2560>;

fn catmull_rom_weights(t: f32) -> array<f32, 4> {
    return array<f32, 4>(
        t * (t * (-0.5 * t + 1.0) - 0.5),
        t * t * (1.5 * t - 2.5) + 1.0,
        t * (t * (-1.5 * t + 2.0) + 0.5),
        t * t * (0.5 * t - 0.5),
    );
}

struct Loc {
    ok: bool,
    ix: i32,
    iy: i32,
    tx: f32,
    ty: f32,
}

fn locate(pix: u32) -> Loc {
    let i = f32(pix % p.rw);
    let j = f32(pix / p.rw);
    let lx = p.a * i + p.b * j + p.bx;
    let ly = p.c * i + p.d * j + p.by;
    let fx = floor(lx);
    let fy = floor(ly);
    var loc: Loc;
    loc.ok = (fx >= 1.0) && (fx + 2.0 <= f32(p.win_w) - 1.0)
        && (fy >= 1.0) && (fy + 2.0 <= f32(p.win_h) - 1.0);
    if loc.ok {
        loc.ix = i32(fx);
        loc.iy = i32(fy);
        loc.tx = lx - fx;
        loc.ty = ly - fy;
    }
    return loc;
}

// The horizontal-then-vertical accumulation order of the CPU sampler.
fn sample_at(loc: Loc) -> f32 {
    let wx = catmull_rom_weights(loc.tx);
    let wy = catmull_rom_weights(loc.ty);
    var rows: array<f32, 4>;
    for (var r = 0; r < 4; r = r + 1) {
        let base = u32(loc.iy - 1 + r) * p.win_w + u32(loc.ix - 1);
        rows[r] = wx[0] * moving[base]
            + wx[1] * moving[base + 1u]
            + wx[2] * moving[base + 2u]
            + wx[3] * moving[base + 3u];
    }
    return wy[0] * rows[0] + wy[1] * rows[1] + wy[2] * rows[2] + wy[3] * rows[3];
}

// Sums the first `count` shared columns across the workgroup into
// `sh[v * WG]` and returns once the sum is in place for `tid == 0`.
fn tree(tid: u32, count: u32) {
    for (var s = WG / 2u; s > 0u; s = s >> 1u) {
        workgroupBarrier();
        if tid < s {
            for (var v = 0u; v < count; v = v + 1u) {
                sh[v * WG + tid] = sh[v * WG + tid] + sh[v * WG + tid + s];
            }
        }
    }
    workgroupBarrier();
}

@compute @workgroup_size(256)
fn sample_moments(
    @builtin(local_invocation_index) tid: u32,
    @builtin(workgroup_id) wg: vec3<u32>,
) {
    var acc: array<f32, 8>;
    for (var v = 0u; v < 8u; v = v + 1u) {
        acc[v] = 0.0;
    }
    var pix = wg.x * WG + tid;
    let step = p.groups * WG;
    loop {
        if pix >= p.n {
            break;
        }
        let loc = locate(pix);
        if loc.ok {
            let s = sample_at(loc) - p.shift;
            let t = tmpl[pix] - p.shift;
            acc[0] = acc[0] + 1.0;
            acc[1] = acc[1] + s;
            acc[2] = acc[2] + s * s;
            acc[3] = acc[3] + t * s;
            for (var k = 0u; k < p.cols; k = k + 1u) {
                acc[4u + k] = acc[4u + k] + jac[pix * p.cols + k] * s;
            }
        }
        pix = pix + step;
    }
    for (var v = 0u; v < 8u; v = v + 1u) {
        sh[v * WG + tid] = acc[v];
    }
    tree(tid, 8u);
    if tid == 0u {
        for (var v = 0u; v < 8u; v = v + 1u) {
            partials[wg.x * STRIDE + v] = sh[v * WG];
        }
    }
}

// Every workgroup reads the valid count the previous reduction wrote and
// skips the pixel loop when the whole region sampled validly.
@compute @workgroup_size(256)
fn template_moments(
    @builtin(local_invocation_index) tid: u32,
    @builtin(workgroup_id) wg: vec3<u32>,
) {
    var acc: array<f32, 10>;
    for (var v = 0u; v < 10u; v = v + 1u) {
        acc[v] = 0.0;
    }
    let partial = result[0] < p.full_n - 0.5;
    if partial {
        var pix = wg.x * WG + tid;
        let step = p.groups * WG;
        loop {
            if pix >= p.n {
                break;
            }
            let loc = locate(pix);
            if loc.ok {
                let t = tmpl[pix] - p.shift;
                acc[0] = acc[0] + t;
                acc[1] = acc[1] + t * t;
                for (var k = 0u; k < p.cols; k = k + 1u) {
                    let g = jac[pix * p.cols + k];
                    acc[2u + k] = acc[2u + k] + g * t;
                    acc[6u + k] = acc[6u + k] + g;
                }
            }
            pix = pix + step;
        }
    }
    for (var v = 0u; v < 10u; v = v + 1u) {
        sh[v * WG + tid] = acc[v];
    }
    tree(tid, 10u);
    if tid == 0u {
        for (var v = 0u; v < 10u; v = v + 1u) {
            partials[wg.x * STRIDE + 8u + v] = sh[v * WG];
        }
    }
}

fn reduce_partials(tid: u32, first: u32, count: u32) {
    var acc: array<f32, 10>;
    for (var v = 0u; v < 10u; v = v + 1u) {
        acc[v] = 0.0;
    }
    for (var g = tid; g < p.groups; g = g + WG) {
        for (var v = 0u; v < count; v = v + 1u) {
            acc[v] = acc[v] + partials[g * STRIDE + first + v];
        }
    }
    for (var v = 0u; v < count; v = v + 1u) {
        sh[v * WG + tid] = acc[v];
    }
    tree(tid, count);
    if tid == 0u {
        for (var v = 0u; v < count; v = v + 1u) {
            result[first + v] = sh[v * WG];
        }
    }
}

@compute @workgroup_size(256)
fn reduce_sample(@builtin(local_invocation_index) tid: u32) {
    reduce_partials(tid, 0u, 8u);
}

@compute @workgroup_size(256)
fn reduce_template(@builtin(local_invocation_index) tid: u32) {
    reduce_partials(tid, 8u, 10u);
}

// Writes every region sample and its validity (1.0 or 0.0) to `result` as
// interleaved pairs; the parity tests read the sampler through it.
@compute @workgroup_size(256)
fn sample_values(
    @builtin(local_invocation_index) tid: u32,
    @builtin(workgroup_id) wg: vec3<u32>,
) {
    var pix = wg.x * WG + tid;
    let step = p.groups * WG;
    loop {
        if pix >= p.n {
            break;
        }
        let loc = locate(pix);
        var value = 0.0;
        var valid = 0.0;
        if loc.ok {
            value = sample_at(loc);
            valid = 1.0;
        }
        result[2u * pix] = value;
        result[2u * pix + 1u] = valid;
        pix = pix + step;
    }
}
