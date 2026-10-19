// SPDX-License-Identifier: GPL-3.0-or-later

//! The blend kernel against `focusforge_core::retouch::blend::apply`, over
//! `fixtures/mantis-512`. Integer throughout on both sides, so the expected
//! worst |delta| is exactly 0.

use focusforge_core::algorithm::TileReader;
use focusforge_core::project::{Op, Point};
use focusforge_core::retouch::blend;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapInfo;
use focusforge_gpu::blend::{BlendError, Blender};
use focusforge_gpu::device::{SharedDevice, shared_device_or_skip};

use crate::common::{self, worst_delta};

const OUT_FRAME: usize = 1;
const SRC_FRAME: usize = 6;

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn pt(x: i32, y: i32) -> Point {
    Point { x, y }
}

fn stroke(path: Vec<Point>, radius: f32, softness: f32) -> Op {
    Op::BrushStroke {
        source_id: SRC_FRAME as u16,
        path,
        radius,
        softness,
    }
}

fn blender(shared: &SharedDevice) -> Blender {
    Blender::new(shared.device(), shared.queue())
}

/// `rect`'s samples of `frame`, at `spp` samples a pixel: the fixture's own,
/// or its first channel alone.
fn frame_samples(
    stack: &impl TileReader,
    frame: usize,
    rect: Rect,
    spp: usize,
    native: usize,
) -> Vec<u16> {
    let mut all = vec![0u16; rect.width as usize * rect.height as usize * native];
    stack
        .read_tile_into(frame, rect, &mut all)
        .expect("read fixture tile");
    if spp == native {
        all
    } else {
        all.chunks_exact(native)
            .flat_map(|px| px[..spp].to_vec())
            .collect()
    }
}

/// Blends `op` over `out_rect`'s samples, on the device and on the CPU, from
/// the fixture, and returns the worst |delta| between them.
fn parity_case(
    blender: &Blender,
    label: &str,
    op: &Op,
    out_rect: Rect,
    region: Rect,
    spp: usize,
) -> i32 {
    let stack = common::open_stack();
    let native = stack.header(0).samples_per_pixel as usize;
    let info = ZMapInfo::new(512, 512, 8).unwrap();
    let base = frame_samples(&stack, OUT_FRAME, out_rect, spp, native);
    let src = frame_samples(&stack, SRC_FRAME, region, spp, native);

    let mut cpu = base.clone();
    blend::apply(op, info, out_rect, &mut cpu, region, &src, spp).expect("cpu blend");
    let mut gpu = base.clone();
    blender
        .apply(op, info, out_rect, &mut gpu, region, &src, spp)
        .expect("gpu blend");

    let (worst, at) = worst_delta(&gpu, &cpu);
    let changed = cpu.iter().zip(&base).filter(|(c, b)| c != b).count();
    println!("blend {label}: worst |delta|={worst} at sample {at}, {changed} samples changed");
    worst
}

#[test]
fn blend_parity_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = blender(shared);

    // A region at the origin, and one at a non-zero origin inside a larger
    // output rect.
    let regions = [
        ("origin", rect(0, 0, 128, 96), rect(0, 0, 128, 96)),
        ("offset", rect(40, 30, 300, 200), rect(97, 61, 150, 120)),
    ];
    let mut touched = 0;
    for (place, out_rect, region) in regions {
        let (x, y) = (region.x as i32, region.y as i32);
        let (w, h) = (region.width as i32, region.height as i32);
        let paths = [
            (
                "crossing the edge",
                vec![
                    pt(x - 20, y + h / 2),
                    pt(x + w / 2, y + h / 3),
                    pt(x + w + 20, y + h / 2),
                ],
            ),
            ("one point", vec![pt(x + w / 2, y + h / 2)]),
            (
                "doubling back",
                vec![
                    pt(x + 10, y + 10),
                    pt(x + w - 10, y + h - 10),
                    pt(x + 10, y + 10),
                    pt(x + w / 2, y + h - 5),
                ],
            ),
        ];
        for spp in [3, 1] {
            for softness in [0.0f32, 0.5, 1.0] {
                for radius in [1.5f32, 12.0, 40.0] {
                    for (shape, path) in &paths {
                        let op = stroke(path.clone(), radius, softness);
                        let label =
                            format!("{place} spp {spp} soft {softness} radius {radius} {shape}");
                        let worst = parity_case(&blender, &label, &op, out_rect, region, spp);
                        assert_eq!(worst, 0, "{label}: expected bit-exact agreement");
                        touched += 1;
                    }
                }
            }
        }
    }
    println!("blend parity: {touched} cases, all exact");
}

/// A 64 x 64 fixture tile placed at `at` in a map as large as the kernel's
/// coordinate range, blended on the device and on the CPU.
fn bounds_case(
    blender: &Blender,
    label: &str,
    at: (u32, u32),
    op: &Op,
) -> Result<(i32, Vec<u16>), BlendError> {
    let stack = common::open_stack();
    let native = stack.header(0).samples_per_pixel as usize;
    let info = ZMapInfo::new(16_384, 16_384, 8).unwrap();
    let tile = rect(at.0, at.1, 64, 64);
    let fixture = rect(0, 0, 64, 64);
    let base = frame_samples(&stack, OUT_FRAME, fixture, native, native);
    let src = frame_samples(&stack, SRC_FRAME, fixture, native, native);

    let mut gpu = base.clone();
    blender.apply(op, info, tile, &mut gpu, tile, &src, native)?;
    let mut cpu = base.clone();
    blend::apply(op, info, tile, &mut cpu, tile, &src, native).expect("cpu blend");
    let (worst, _) = worst_delta(&gpu, &cpu);
    let changed = cpu.iter().zip(&base).filter(|(c, b)| c != b).count();
    println!("blend bounds {label}: worst |delta|={worst}, {changed} samples changed");
    assert_ne!(changed, 0, "{label}: the stroke must blend something");
    Ok((worst, base))
}

#[test]
fn blend_parity_at_the_kernel_bounds() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = blender(shared);
    let lim = 16_383;

    let accepted = [
        (
            "far corner",
            (16_320, 16_320),
            stroke(
                vec![pt(lim, lim), pt(16_340, 16_350), pt(lim, 16_330)],
                12.0,
                0.5,
            ),
        ),
        (
            "negative extreme, long segment",
            (0, 0),
            stroke(vec![pt(-lim, -lim), pt(30, 30)], 40.0, 1.0),
        ),
        (
            "anti-diagonal across the range",
            (0, 0),
            stroke(vec![pt(lim, -lim), pt(-lim, lim)], 40.0, 0.5),
        ),
        (
            "radius under 4096, soft",
            (0, 0),
            stroke(vec![pt(2_800, 2_800)], 4_095.9, 1.0),
        ),
        (
            "radius under 4096, long segment",
            (0, 0),
            stroke(vec![pt(-2_000, 3_000), pt(2_800, 2_800)], 4_095.9, 0.3),
        ),
    ];
    for (label, at, op) in &accepted {
        let (worst, _) =
            bounds_case(&blender, label, *at, op).unwrap_or_else(|err| panic!("{label}: {err}"));
        assert_eq!(worst, 0, "{label}: expected bit-exact agreement");
    }

    let refused = [
        (
            "radius 4096",
            (0, 0),
            stroke(vec![pt(30, 30)], 4_096.0, 0.5),
            "radius",
        ),
        (
            "endpoint at 2^14",
            (0, 0),
            stroke(vec![pt(30, 30), pt(16_384, 30)], 40.0, 0.5),
            "coordinate",
        ),
        (
            "endpoint at -2^14",
            (0, 0),
            stroke(vec![pt(30, 30), pt(30, -16_384)], 40.0, 0.5),
            "coordinate",
        ),
    ];
    for (label, at, op, limit) in &refused {
        match bounds_case(&blender, label, *at, op) {
            Err(BlendError::Refused { limit: got, .. }) => assert_eq!(got, *limit, "{label}"),
            Err(other) => panic!("{label}: expected a refusal, got {other}"),
            Ok(_) => panic!("{label}: expected a refusal"),
        }
    }

    // A region pixel at 2^14 is refused too, with `out` untouched.
    let info = ZMapInfo::new(16_400, 64, 8).unwrap();
    let tile = rect(16_380, 0, 8, 8);
    let op = stroke(vec![pt(16_383, 4)], 6.0, 0.5);
    let base: Vec<u16> = (0..8 * 8).map(|i| i * 9).collect();
    let mut out = base.clone();
    let err = blender
        .apply(&op, info, tile, &mut out, tile, &[7; 64], 1)
        .expect_err("region reaching 2^14");
    assert!(
        matches!(
            err,
            BlendError::Refused {
                limit: "coordinate",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(out, base, "a refused stroke leaves `out` untouched");
}

#[test]
fn blend_refuses_oversized_region() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = blender(shared);
    let limit = shared.limits().max_storage_buffer_binding_size;

    // One more sample a pixel than a binding holds over a 512 x 512 region.
    let pixels = 512u64 * 512;
    let spp = (limit / (pixels * 4) + 1) as usize;
    let region = rect(0, 0, 512, 512);
    let info = ZMapInfo::new(512, 512, 8).unwrap();
    let op = stroke(vec![pt(256, 256)], 400.0, 0.5);
    let len = pixels as usize * spp;
    let base: Vec<u16> = (0..len).map(|i| (i % 65_536) as u16).collect();
    let mut out = base.clone();
    let err = blender
        .apply(&op, info, region, &mut out, region, &vec![9; len], spp)
        .expect_err("a region past the binding limit");
    match err {
        BlendError::Refused {
            limit: "buffer", ..
        } => {}
        other => panic!("expected a buffer refusal, got {other}"),
    }
    assert_eq!(out, base, "a refused region is not truncated or blended");
}
