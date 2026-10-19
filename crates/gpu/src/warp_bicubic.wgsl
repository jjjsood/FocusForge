// SPDX-License-Identifier: GPL-3.0-or-later
//
// The GPU twin of `warp_from_tile_into`'s bicubic Catmull-Rom resampler.
// The host (`warp_coords::map_coords`, via core's `Transform::map_point`
// and its w/MIN_W guard) has already decided coverage and the mapped source
// coordinate in f64; this kernel only interpolates. One invocation per
// packed output word (two samples share a `u32`), as in `plane_to_u16.wgsl`.

struct Coord {
    ix: i32,
    iy: i32,
    tx: f32,
    ty: f32,
    covered: u32,
    _pad: u32,
}

struct Params {
    word_count: u32,
    sample_count: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    spp: u32,
    frame_w: u32,
    frame_h: u32,
}

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read> coords: array<Coord>;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;
@group(0) @binding(3) var<uniform> params: Params;

// The four Catmull-Rom (a = -0.5) weights for fractional offset `t`, in tap
// order [-1, 0, 1, 2] -- transcribed term for term from the CPU twin's
// weight computation, not rearranged.
fn catmull_rom_weights(t: f32) -> array<f32, 4> {
    return array<f32, 4>(
        t * (t * (-0.5 * t + 1.0) - 0.5),
        t * t * (1.5 * t - 2.5) + 1.0,
        t * (t * (-1.5 * t + 2.0) + 0.5),
        t * t * (0.5 * t - 0.5),
    );
}

fn clamp_tap(v: i32, dim: u32) -> i32 {
    return clamp(v, 0, i32(dim) - 1);
}

// Reads one sample of channel `c` at absolute frame coordinates
// `(col, row)`. `col`/`row` are already edge-clamped into `0..frame_w` /
// `0..frame_h` by `warp_sample` below; the `u32` subtraction here cannot
// wrap because the host's precheck (`warp_coords::map_coords`) already
// refused every covered pixel whose tap window reaches outside the source
// tile before this kernel was ever dispatched.
fn sample(col: i32, row: i32, c: u32) -> f32 {
    let x = u32(col) - params.src_x;
    let y = u32(row) - params.src_y;
    let idx = y * (params.src_w * params.spp) + x * params.spp + c;
    let word = src[idx / 2u];
    let v = select(word >> 16u, word & 0xffffu, (idx & 1u) == 0u);
    return f32(v);
}

// One sample `s` (`s = pixel * spp + c`) of the warped output: the CPU
// twin's uncovered-pixel zero, or its bicubic accumulation transcribed
// with the same pinned order -- horizontal taps ascending, then vertical
// rows ascending -- never `dot()`, never a loop accumulator, never a
// hand-written `fma`.
fn warp_sample(s: u32) -> f32 {
    let pixel = s / params.spp;
    let c = s % params.spp;
    let coord = coords[pixel];
    if (coord.covered == 0u) {
        return 0.0;
    }

    let wx = catmull_rom_weights(coord.tx);
    let wy = catmull_rom_weights(coord.ty);

    let cols = array<i32, 4>(
        clamp_tap(coord.ix - 1, params.frame_w),
        clamp_tap(coord.ix, params.frame_w),
        clamp_tap(coord.ix + 1, params.frame_w),
        clamp_tap(coord.ix + 2, params.frame_w),
    );
    let rows = array<i32, 4>(
        clamp_tap(coord.iy - 1, params.frame_h),
        clamp_tap(coord.iy, params.frame_h),
        clamp_tap(coord.iy + 1, params.frame_h),
        clamp_tap(coord.iy + 2, params.frame_h),
    );

    let row0 = wx[0] * sample(cols[0], rows[0], c) + wx[1] * sample(cols[1], rows[0], c)
        + wx[2] * sample(cols[2], rows[0], c) + wx[3] * sample(cols[3], rows[0], c);
    let row1 = wx[0] * sample(cols[0], rows[1], c) + wx[1] * sample(cols[1], rows[1], c)
        + wx[2] * sample(cols[2], rows[1], c) + wx[3] * sample(cols[3], rows[1], c);
    let row2 = wx[0] * sample(cols[0], rows[2], c) + wx[1] * sample(cols[1], rows[2], c)
        + wx[2] * sample(cols[2], rows[2], c) + wx[3] * sample(cols[3], rows[2], c);
    let row3 = wx[0] * sample(cols[0], rows[3], c) + wx[1] * sample(cols[1], rows[3], c)
        + wx[2] * sample(cols[2], rows[3], c) + wx[3] * sample(cols[3], rows[3], c);

    return wy[0] * row0 + wy[1] * row1 + wy[2] * row2 + wy[3] * row3;
}

// `(v.clamp(0, 65535) + 0.5).floor()`, never WGSL's ties-to-even `round()`:
// Rust's `f32::round` (the CPU twin) is ties-away-from-zero, a 1-LSB parity
// failure by construction if spelled "round".
fn to_u16(v: f32) -> u32 {
    return u32(floor(clamp(v, 0.0, 65535.0) + 0.5));
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let i = global_id.x;
    if (i >= params.word_count) {
        return;
    }

    let lo = to_u16(warp_sample(2u * i));

    var hi: u32 = 0u;
    if (2u * i + 1u < params.sample_count) {
        hi = to_u16(warp_sample(2u * i + 1u));
    }

    dst[i] = lo | (hi << 16u);
}
