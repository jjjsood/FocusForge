// SPDX-License-Identifier: GPL-3.0-or-later

//! The blend kernel against `focusforge_core::retouch::blend::apply`, over
//! `fixtures/mantis-512`. Integer throughout on both sides, so the expected
//! worst |delta| is exactly 0.

use focusforge_core::algorithm::TileReader;
use focusforge_core::io::TiffStack;
use focusforge_core::project::{BrushMode, Op, Point, StrokeSource};
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
    Op::frame_stroke(SRC_FRAME as u16, path, radius, softness)
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

/// What one device-versus-CPU blend left behind.
struct Outcome {
    /// Worst |delta| between device and CPU samples.
    worst: i32,
    /// Samples the CPU blend changed.
    changed: usize,
    /// Pixels of `region` with a non-zero scaled alpha that the CPU left as
    /// they were.
    untouched: usize,
}

/// `region`'s samples of the source frame, at `spp` samples a pixel.
fn source_frame(stack: &TiffStack, region: Rect, spp: usize) -> Vec<u16> {
    let native = stack.header(0).samples_per_pixel as usize;
    frame_samples(stack, SRC_FRAME, region, spp, native)
}

/// `region`'s samples of a rendered checkpoint: the output frame with a
/// stroke from the source frame already blended in, so it equals neither.
fn checkpoint_samples(stack: &TiffStack, region: Rect, spp: usize) -> Vec<u16> {
    let native = stack.header(0).samples_per_pixel as usize;
    let info = ZMapInfo::new(512, 512, 8).unwrap();
    let mut rendered = frame_samples(stack, OUT_FRAME, region, spp, native);
    let frame = source_frame(stack, region, spp);
    let (x, y) = (region.x as i32, region.y as i32);
    let (w, h) = (region.width as i32, region.height as i32);
    let earlier = styled(
        vec![pt(x + w / 4, y + h / 2), pt(x + 3 * w / 4, y + h / 2)],
        30.0,
        0.5,
        BrushMode::Normal,
        0.7,
    );
    blend::apply(&earlier, info, region, &mut rendered, region, &frame, spp)
        .expect("render the checkpoint");
    assert_ne!(rendered, frame, "the checkpoint differs from the frame");
    rendered
}

/// Blends `op` over `out_rect`'s samples, on the device and on the CPU,
/// reading `src` over `region`.
#[allow(clippy::too_many_arguments)]
fn parity_case(
    stack: &TiffStack,
    blender: &Blender,
    label: &str,
    op: &Op,
    out_rect: Rect,
    region: Rect,
    src: &[u16],
    spp: usize,
) -> Outcome {
    let native = stack.header(0).samples_per_pixel as usize;
    let info = ZMapInfo::new(512, 512, 8).unwrap();
    let base = frame_samples(stack, OUT_FRAME, out_rect, spp, native);

    let mut cpu = base.clone();
    blend::apply(op, info, out_rect, &mut cpu, region, src, spp).expect("cpu blend");
    let mut gpu = base.clone();
    blender
        .apply(op, info, out_rect, &mut gpu, region, src, spp)
        .expect("gpu blend");

    let (worst, at) = worst_delta(&gpu, &cpu);
    let changed = cpu.iter().zip(&base).filter(|(c, b)| c != b).count();
    let Op::BrushStroke { opacity, .. } = op else {
        unreachable!("parity cases are brush strokes")
    };
    let q = u64::from(blend::opacity_q16(*opacity));
    let mut untouched = 0;
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            let alpha = u64::from(blend::alpha(op, info, x, y).unwrap());
            if (alpha * q + 32768) >> 16 == 0 {
                continue;
            }
            let at = ((y - out_rect.y) as usize * out_rect.width as usize
                + (x - out_rect.x) as usize)
                * spp;
            untouched += usize::from(cpu[at..at + spp] == base[at..at + spp]);
        }
    }
    println!("blend {label}: worst |delta|={worst} at sample {at}, {changed} samples changed");
    Outcome {
        worst,
        changed,
        untouched,
    }
}

type Place = (&'static str, Rect, Rect, Vec<(&'static str, Vec<Point>)>);

/// The regions and paths the parity cases cover: a region at the origin, and
/// one at a non-zero origin inside a larger output rect; each with a path
/// crossing the region's edge, a single point and a path doubling back.
fn places() -> Vec<Place> {
    let regions = [
        ("origin", rect(0, 0, 128, 96), rect(0, 0, 128, 96)),
        ("offset", rect(40, 30, 300, 200), rect(97, 61, 150, 120)),
    ];
    regions
        .into_iter()
        .map(|(place, out_rect, region)| {
            let (x, y) = (region.x as i32, region.y as i32);
            let (w, h) = (region.width as i32, region.height as i32);
            let paths = vec![
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
            (place, out_rect, region, paths)
        })
        .collect()
}

#[test]
fn blend_parity_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = blender(shared);

    let stack = common::open_stack();
    let mut touched = 0;
    for (place, out_rect, region, paths) in places() {
        for spp in [3, 1] {
            for softness in [0.0f32, 0.5, 1.0] {
                for radius in [1.5f32, 12.0, 40.0] {
                    for (shape, path) in &paths {
                        let op = stroke(path.clone(), radius, softness);
                        let label =
                            format!("{place} spp {spp} soft {softness} radius {radius} {shape}");
                        let src = source_frame(&stack, region, spp);
                        let case =
                            parity_case(&stack, &blender, &label, &op, out_rect, region, &src, spp);
                        assert_eq!(case.worst, 0, "{label}: expected bit-exact agreement");
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

fn styled(path: Vec<Point>, radius: f32, softness: f32, mode: BrushMode, opacity: f32) -> Op {
    Op::BrushStroke {
        source: StrokeSource::Frame(SRC_FRAME as u16),
        path,
        radius,
        softness,
        mode,
        opacity,
    }
}

/// Every blend mode at the opacity and softness extremes, over every place
/// and path, with `source` giving the samples read from. Returns the number of
/// cases, all bit-exact.
fn per_mode_cases(
    stack: &TiffStack,
    blender: &Blender,
    source: impl Fn(&TiffStack, Rect, usize) -> Vec<u16>,
) -> usize {
    let modes = [
        BrushMode::Normal,
        BrushMode::Lighten,
        BrushMode::Darken,
        BrushMode::Detail,
    ];
    // (opacity, softness): three opacities at softness 0.5, then the two
    // softness extremes at full opacity.
    let settings = [
        (1.0f32, 0.5f32),
        (0.5, 0.5),
        (0.01, 0.5),
        (1.0, 0.0),
        (1.0, 1.0),
    ];
    let mut cases = 0;
    for mode in modes {
        let mut changed = [0usize; 5];
        let mut untouched = 0;
        for (place, out_rect, region, paths) in places() {
            for spp in [3, 1] {
                for (n, (opacity, softness)) in settings.into_iter().enumerate() {
                    for radius in [1.5f32, 12.0, 40.0] {
                        for (shape, path) in &paths {
                            let op = styled(path.clone(), radius, softness, mode, opacity);
                            let label = format!(
                                "{mode:?} opacity {opacity} {place} spp {spp} soft {softness} radius {radius} {shape}"
                            );
                            let src = source(stack, region, spp);
                            let case = parity_case(
                                stack, blender, &label, &op, out_rect, region, &src, spp,
                            );
                            assert_eq!(case.worst, 0, "{label}: expected bit-exact agreement");
                            changed[n] += case.changed;
                            untouched += case.untouched;
                            cases += 1;
                        }
                    }
                }
            }
        }
        println!(
            "{mode:?}: changed samples per setting {changed:?}, {untouched} pixels left untouched"
        );
        assert!(
            changed.iter().all(|&c| c > 0),
            "{mode:?} changes samples at every setting"
        );
        if matches!(mode, BrushMode::Lighten | BrushMode::Darken) {
            assert!(
                untouched > 0,
                "{mode:?} leaves a pixel with alpha untouched"
            );
        }
    }
    cases
}

#[test]
fn blend_parity_per_mode_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = blender(shared);
    let stack = common::open_stack();

    let mut cases = per_mode_cases(&stack, &blender, source_frame);

    // The largest f32 below 1.0 is Q16 full: the kernel takes it, as the CPU blend does.
    let region = rect(0, 0, 128, 96);
    let op = styled(
        vec![pt(64, 48)],
        12.0,
        0.5,
        BrushMode::Normal,
        1.0 - f32::EPSILON / 2.0,
    );
    let src = source_frame(&stack, region, 3);
    let case = parity_case(
        &stack,
        &blender,
        "Normal at Q16 full",
        &op,
        region,
        region,
        &src,
        3,
    );
    assert_eq!(case.worst, 0, "an opacity that rounds to Q16 full is exact");
    cases += 1;
    println!("blend parity per mode: {cases} cases, all exact");
    assert_eq!(cases, 721);
}

#[test]
fn blend_parity_from_a_checkpoint_source() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = blender(shared);
    let stack = common::open_stack();

    let cases = per_mode_cases(&stack, &blender, checkpoint_samples);
    println!("blend parity from a checkpoint source: {cases} cases, all exact");
    assert_eq!(cases, 720);
}

#[test]
fn blend_refuses_a_detail_margin_past_the_coordinate_range() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = blender(shared);
    let info = ZMapInfo::new(16_400, 64, 8).unwrap();
    let tile = rect(16_370, 0, 30, 8);
    let base: Vec<u16> = (0..30 * 8).map(|i| i * 9).collect();
    let src = vec![7u16; 30 * 8];

    // The footprint ends below 2^14, so Normal blends there.
    let normal = styled(vec![pt(16_372, 4)], 6.0, 0.5, BrushMode::Normal, 1.0);
    let mut out = base.clone();
    blender
        .apply(&normal, info, tile, &mut out, tile, &src, 1)
        .expect("Normal blends below the coordinate range");
    assert_ne!(out, base, "the Normal stroke changed samples");

    let detail = styled(vec![pt(16_372, 4)], 6.0, 0.5, BrushMode::Detail, 1.0);
    let mut out = base.clone();
    let err = blender
        .apply(&detail, info, tile, &mut out, tile, &src, 1)
        .expect_err("a read margin reaching 2^14");
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
