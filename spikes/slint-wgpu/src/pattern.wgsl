// SPDX-License-Identifier: GPL-3.0-or-later
// Spike shader: a gradient plus a hard-edged off-centre square, written into a
// storage texture that Slint imports zero-copy via slint::Image::try_from(wgpu::Texture).
//
// Pattern (matches src/pattern_cpu.rs exactly, so --verify-pattern is a real check):
//   red   ramps left -> right, animated by the time uniform (wraps every 1.0)
//   green ramps top -> bottom, static
//   blue  is 1.0 inside a hard-edged rectangle placed off-centre in both axes, else 0.0
//   alpha is always 1.0

const WIDTH: u32 = 512u;
const HEIGHT: u32 = 512u;
const SQ_X0: u32 = 350u;
const SQ_X1: u32 = 450u;
const SQ_Y0: u32 = 120u;
const SQ_Y1: u32 = 220u;

struct Params {
    t: f32,
}

@group(0) @binding(0) var out_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(1) var<uniform> params: Params;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let y = gid.y;
    if x >= WIDTH || y >= HEIGHT {
        return;
    }

    let fx = f32(x) / f32(WIDTH - 1u);
    let fy = f32(y) / f32(HEIGHT - 1u);

    var r = fx + params.t;
    r = r - floor(r);
    let g = fy;

    var b = 0.0;
    if x >= SQ_X0 && x < SQ_X1 && y >= SQ_Y0 && y < SQ_Y1 {
        b = 1.0;
    }

    textureStore(out_tex, vec2<i32>(i32(x), i32(y)), vec4<f32>(r, g, b, 1.0));
}
