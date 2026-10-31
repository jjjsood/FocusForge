// SPDX-License-Identifier: GPL-3.0-or-later

//! Parity between `crates/gpu`'s `f32` paths and their `crates/core` twins on
//! `f32` input that reaches above 65535 and below 0. The tolerance is an
//! absolute 1.0 on the 16-bit scale (65535.0 is white), above white too.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput, TileReader};
use focusforge_core::align::warp::warp_tile_samples;
use focusforge_core::align::{CpuMomentSession, Transform, WarpingReader};
use focusforge_core::fuse::laplacian::Laplacian;
use focusforge_core::gpu::{EccSessionRequest, TileBackend};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::tile::{NoopPreprocess, Rect, SampleFormat};
use focusforge_gpu::budget::device_bytes_per_tile;
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::harness::{GpuBackend, gpu_backend};
use focusforge_gpu::stack_tile::upload_frame_f32;
use focusforge_gpu::transfer::download_f32;
use focusforge_gpu::warp::{WarpPipelines, warp_tile_f32_gpu};

use crate::budget_gpu::sample_allocator;
use crate::common::{MantisF32, assert_matches_cpu_outside_near_ties};

/// Offsets that push `mantis-512` above 65535 and below 0.
const OFFSETS: [f32; 2] = [40_000.0, -40_000.0];

fn worst_f32(gpu: &[f32], cpu: &[f32]) -> (f32, usize) {
    assert_eq!(gpu.len(), cpu.len());
    let mut worst = (0.0f32, 0usize);
    for (i, (g, c)) in gpu.iter().zip(cpu).enumerate() {
        let delta = (g - c).abs();
        if delta > worst.0 {
            worst = (delta, i);
        }
    }
    worst
}

/// One single-channel frame held in memory, served as `f32`.
struct FrameF32 {
    width: u32,
    samples: Vec<f32>,
    reads: AtomicUsize,
}

impl FrameF32 {
    fn new(width: u32, height: u32, value: impl Fn(u32, u32) -> f32) -> Self {
        let samples = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| value(x, y))
            .collect();
        Self {
            width,
            samples,
            reads: AtomicUsize::new(0),
        }
    }
}

impl TileReader for FrameF32 {
    fn read_tile_into(&self, _: usize, _: Rect, _: &mut [u16]) -> Result<(), AlgorithmError> {
        unreachable!("the f32 paths read f32")
    }

    fn sample_format(&self) -> SampleFormat {
        SampleFormat::F32
    }

    fn read_tile_f32_into(
        &self,
        _: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        for row in 0..rect.height as usize {
            let from = (rect.y as usize + row) * self.width as usize + rect.x as usize;
            let to = row * rect.width as usize;
            out[to..to + rect.width as usize]
                .copy_from_slice(&self.samples[from..from + rect.width as usize]);
        }
        Ok(())
    }
}

/// GPU and CPU `f32` warps of `dst` out of `reader` frame 0, compared.
/// Returns the GPU samples.
fn assert_warp_matches_cpu(
    reader: &dyn TileReader,
    frame: (u32, u32),
    spp: u32,
    dst: Rect,
    t: &Transform,
    tolerance: f32,
    what: &str,
) -> Vec<f32> {
    let shared = shared_device_or_skip().expect("gpu device");
    let pipelines = WarpPipelines::new();
    let (cpu, cpu_covered) =
        warp_tile_samples::<f32>(reader, 0, frame, spp, dst, t).expect("cpu f32 warp");
    let (gpu, gpu_covered) =
        warp_tile_f32_gpu(shared, &pipelines, reader, 0, frame, spp, dst, t).expect("gpu f32 warp");
    assert_eq!(gpu_covered, cpu_covered, "{what}: coverage");
    let (worst, at) = worst_f32(&gpu, &cpu);
    println!("{what}: worst |delta|={worst} at sample index {at}");
    assert!(
        worst <= tolerance,
        "{what}: worst={worst} at index {at}, tolerance {tolerance}"
    );
    gpu
}

#[test]
fn parity_f32_warp_identity_is_exact() {
    if shared_device_or_skip().is_none() {
        return;
    }
    for offset in OFFSETS {
        let reader = MantisF32::open(offset);
        let (w, h) = reader.stack.dimensions();
        let spp = reader.stack.header(0).samples_per_pixel;
        for dst in [
            Rect {
                x: 200,
                y: 200,
                width: 96,
                height: 96,
            },
            Rect {
                x: w - 96,
                y: h - 96,
                width: 96,
                height: 96,
            },
        ] {
            let gpu = assert_warp_matches_cpu(
                &reader,
                (w, h),
                spp,
                dst,
                &Transform::identity(),
                0.0,
                &format!("identity offset {offset} {dst:?}"),
            );
            let mut direct = vec![0f32; gpu.len()];
            reader.read_tile_f32_into(0, dst, &mut direct).unwrap();
            assert_eq!(gpu, direct, "identity warp is the reader's samples");
        }
    }
}

#[test]
fn parity_f32_warp_similarity_within_one_lsb() {
    if shared_device_or_skip().is_none() {
        return;
    }
    for offset in OFFSETS {
        let reader = MantisF32::open(offset);
        let (w, h) = reader.stack.dimensions();
        let spp = reader.stack.header(0).samples_per_pixel;
        let dst = Rect {
            x: 128,
            y: 128,
            width: 200,
            height: 200,
        };
        let gpu = assert_warp_matches_cpu(
            &reader,
            (w, h),
            spp,
            dst,
            &Transform::similarity(1.02, 0.03, 1.5, -1.0),
            1.0,
            &format!("similarity offset {offset}"),
        );
        // Nothing is clamped: the output keeps the overshoot.
        if offset > 0.0 {
            assert!(gpu.iter().any(|&v| v > 65_535.0));
        } else {
            assert!(gpu.iter().any(|&v| v < 0.0));
        }
    }
}

#[test]
fn parity_f32_warp_edge_crossing_source_within_one_lsb() {
    if shared_device_or_skip().is_none() {
        return;
    }
    for offset in OFFSETS {
        let reader = MantisF32::open(offset);
        let (w, h) = reader.stack.dimensions();
        let spp = reader.stack.header(0).samples_per_pixel;
        let dst = Rect {
            x: 400,
            y: 200,
            width: 96,
            height: 32,
        };
        assert_warp_matches_cpu(
            &reader,
            (w, h),
            spp,
            dst,
            &Transform::translation(50.0, 0.0),
            1.0,
            &format!("edge crossing offset {offset}"),
        );
    }
}

#[test]
fn parity_f32_warp_out_of_footprint_is_zero_and_uncovered() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    for offset in OFFSETS {
        let reader = MantisF32::open(offset);
        let (w, h) = reader.stack.dimensions();
        let spp = reader.stack.header(0).samples_per_pixel;
        let pipelines = WarpPipelines::new();

        // Pixels the edge-crossing warp leaves uncovered read 0.0, not the offset.
        let dst = Rect {
            x: 400,
            y: 200,
            width: 96,
            height: 32,
        };
        let t = Transform::translation(50.0, 0.0);
        let (gpu, covered) =
            warp_tile_f32_gpu(shared, &pipelines, &reader, 0, (w, h), spp, dst, &t).unwrap();
        assert!(covered.contains(&false) && covered.contains(&true));
        for (i, &c) in covered.iter().enumerate() {
            if !c {
                let from = i * spp as usize;
                assert!(gpu[from..from + spp as usize].iter().all(|&v| v == 0.0));
            }
        }

        // A destination that pulls entirely from outside the frame reads nothing.
        let frame = FrameF32::new(64, 64, |x, y| x as f32 + y as f32 + offset);
        let far = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        let (samples, covered) = warp_tile_f32_gpu(
            shared,
            &pipelines,
            &frame,
            0,
            (64, 64),
            1,
            far,
            &Transform::translation(600.0, 0.0),
        )
        .unwrap();
        assert!(covered.iter().all(|&c| !c));
        assert!(samples.iter().all(|&v| v == 0.0));
        assert_eq!(frame.reads.load(Ordering::SeqCst), 0, "no read attempted");
    }
}

#[test]
fn parity_f32_warp_projective_within_one_lsb() {
    if shared_device_or_skip().is_none() {
        return;
    }
    // w = 1 + 0.001*x, so the divide is exercised.
    let t = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.001, 0.0, 1.0]]);
    for offset in OFFSETS {
        let frame = FrameF32::new(64, 64, |x, y| ((x * 200) ^ y) as f32 + offset);
        for dst in [
            Rect {
                x: 20,
                y: 20,
                width: 4,
                height: 4,
            },
            Rect {
                x: 8,
                y: 8,
                width: 40,
                height: 40,
            },
        ] {
            assert_warp_matches_cpu(
                &frame,
                (64, 64),
                1,
                dst,
                &t,
                1.0,
                &format!("projective offset {offset} {dst:?}"),
            );
        }
    }
}

#[test]
fn parity_f32_warp_far_origin_within_one_lsb() {
    if shared_device_or_skip().is_none() {
        return;
    }
    // A hard edge 1117 levels per pixel at frame x = 8192, where ulp(8192.0f32)
    // is already about one level of coordinate rounding.
    let origin = 8176u32;
    for offset in OFFSETS {
        let frame = FrameF32::new(origin + 32 + 16, 1, |x, _| {
            x.saturating_sub(origin) as f32 * 1117.0 + offset
        });
        assert_warp_matches_cpu(
            &frame,
            (origin + 32 + 16, 1),
            1,
            Rect {
                x: 8192,
                y: 0,
                width: 1,
                height: 1,
            },
            &Transform::translation(0.00045, 0.0),
            1.0,
            &format!("far origin offset {offset}"),
        );
    }
}

#[test]
fn parity_f32_plane_round_trip_is_exact() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let round_trip = |reader: &dyn TileReader, rect: Rect, spp: u32, what: &str| {
        let mut expected = vec![0f32; rect.width as usize * rect.height as usize * spp as usize];
        reader.read_tile_f32_into(0, rect, &mut expected).unwrap();
        let buffer = upload_frame_f32(shared, reader, 0, rect, spp).unwrap();
        let got = download_f32(shared.device(), shared.queue(), &buffer, expected.len()).unwrap();
        assert_eq!(
            got, expected,
            "{what}: base plane equals the reader's samples"
        );
    };
    for offset in OFFSETS {
        // 3 samples per pixel, 33 x 17 x 3 = 1683 samples (odd).
        let mantis = MantisF32::open(offset);
        let spp = mantis.stack.header(0).samples_per_pixel;
        let rect = Rect {
            x: 100,
            y: 60,
            width: 33,
            height: 17,
        };
        round_trip(&mantis, rect, spp, &format!("mantis offset {offset}"));
        // 1 sample per pixel, 31 x 13 = 403 samples (odd).
        let frame = FrameF32::new(64, 32, |x, y| (x * 977 + y * 31) as f32 + offset);
        let rect = Rect {
            x: 5,
            y: 3,
            width: 31,
            height: 13,
        };
        round_trip(&frame, rect, 1, &format!("single channel offset {offset}"));
    }
}

/// `reader` frames served as `u16` clipped at the read, as a reader without an
/// `f32` form would.
struct ClippedAtRead<'a>(&'a MantisF32);

impl TileReader for ClippedAtRead<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let mut wide = vec![0f32; out.len()];
        self.0.read_tile_f32_into(frame, rect, &mut wide)?;
        for (o, v) in out.iter_mut().zip(wide) {
            *o = v.clamp(0.0, 65_535.0).round() as u16;
        }
        Ok(())
    }
}

const FUSE_LEVELS: u32 = 4;

fn laplacian_config(levels: u32) -> ParamValues {
    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(i64::from(levels)));
    cfg
}

/// `Laplacian` over a `TileCtx::on_demand` of `reader` through a
/// `WarpingReader`, the `f32` frames it fused, and the GPU backend's tile.
fn fuse_both(
    reader: &MantisF32,
    transforms: &[Transform],
    rect: Rect,
) -> (TileOutput, TileOutput, Vec<Vec<f32>>) {
    let (w, h) = reader.stack.dimensions();
    let spp = reader.stack.header(0).samples_per_pixel;
    let warping = WarpingReader::new(reader, (w, h), spp, transforms.to_vec());
    let ctx = TileCtx::on_demand(rect, transforms.len(), spp, &warping, &NoopPreprocess);
    let cpu = Laplacian
        .fuse(&ctx, &laplacian_config(FUSE_LEVELS))
        .expect("cpu f32 fuse");
    let gpu = gpu_backend()
        .expect("gpu backend")
        .warp_fuse_tile(reader, (w, h), transforms, rect, spp, FUSE_LEVELS)
        .expect("gpu f32 fuse");
    (
        cpu,
        gpu,
        warped_frames(&warping, transforms.len(), rect, spp),
    )
}

/// Every frame of `warping` over `rect`, read as `f32`.
fn warped_frames(
    warping: &WarpingReader<'_>,
    frame_count: usize,
    rect: Rect,
    spp: u32,
) -> Vec<Vec<f32>> {
    (0..frame_count)
        .map(|f| {
            let mut samples = vec![0f32; rect.width as usize * rect.height as usize * spp as usize];
            warping.read_tile_f32_into(f, rect, &mut samples).unwrap();
            samples
        })
        .collect()
}

#[test]
fn parity_f32_fuse_keeps_headroom() {
    if shared_device_or_skip().is_none() {
        return;
    }
    let reader = MantisF32::open(30_000.0);
    let (w, h) = reader.stack.dimensions();
    let spp = reader.stack.header(0).samples_per_pixel;
    // The tile sits on the frame corner so the similarity warp leaves pixels uncovered.
    let rect = Rect {
        x: 0,
        y: 0,
        width: 200,
        height: 168,
    };
    let transforms = [
        Transform::identity(),
        Transform::similarity(1.0, 0.02, -6.0, -4.0),
        Transform::identity(),
    ];
    let warping = WarpingReader::new(&reader, (w, h), spp, transforms.to_vec());
    let ctx = TileCtx::on_demand(rect, transforms.len(), spp, &warping, &NoopPreprocess);
    let cpu = Laplacian
        .fuse_f32(&ctx, &laplacian_config(FUSE_LEVELS))
        .expect("cpu f32 fuse");
    let gpu = gpu_backend()
        .expect("gpu backend")
        .warp_fuse_tile_f32(&reader, (w, h), &transforms, rect, spp, FUSE_LEVELS)
        .expect("gpu f32 fuse");

    let mut scratch = vec![0f32; rect.width as usize * rect.height as usize * spp as usize];
    let coverage = warping
        .read_tile_coverage_f32_into(1, rect, &mut scratch)
        .unwrap()
        .expect("the warped frame reports coverage");
    assert!(
        coverage.contains(&false),
        "the warp leaves uncovered pixels"
    );
    for (what, output) in [("cpu", &cpu), ("gpu", &gpu)] {
        assert!(
            output.tile.samples.iter().any(|&s| s > 65_535.0),
            "{what}: samples above white survive the fusion"
        );
    }
    let frames = warped_frames(&warping, transforms.len(), rect, spp);
    let frames: Vec<&[f32]> = frames.iter().map(Vec::as_slice).collect();
    assert_matches_cpu_outside_near_ties(
        "f32 fuse, unclipped",
        &cpu,
        &gpu,
        &frames,
        rect,
        spp,
        FUSE_LEVELS,
    );
}

#[test]
fn parity_f32_fuse_matches_cpu_outside_near_ties() {
    if shared_device_or_skip().is_none() {
        return;
    }
    let reader = MantisF32::open(30_000.0);
    let spp = reader.stack.header(0).samples_per_pixel;
    // The tile sits on the frame corner so the similarity warp leaves pixels uncovered.
    let rect = Rect {
        x: 0,
        y: 0,
        width: 200,
        height: 168,
    };
    let transforms = [
        Transform::identity(),
        Transform::similarity(1.0, 0.02, -6.0, -4.0),
        Transform::identity(),
    ];
    let (cpu, gpu, frames) = fuse_both(&reader, &transforms, rect);
    let warped = WarpingReader::new(&reader, reader.stack.dimensions(), spp, transforms.to_vec());
    let mut scratch = vec![0f32; frames[1].len()];
    let coverage = warped
        .read_tile_coverage_f32_into(1, rect, &mut scratch)
        .unwrap()
        .expect("the warped frame reports coverage");
    assert!(
        coverage.contains(&false),
        "the warp leaves uncovered pixels"
    );
    let frames: Vec<&[f32]> = frames.iter().map(Vec::as_slice).collect();
    assert_matches_cpu_outside_near_ties("f32 fuse", &cpu, &gpu, &frames, rect, spp, FUSE_LEVELS);
}

#[test]
fn parity_f32_fuse_clips_once_at_the_quantize() {
    if shared_device_or_skip().is_none() {
        return;
    }
    // Textured across 65535: the f32 composite keeps the highlights up to the quantize.
    let reader = MantisF32::open(30_000.0);
    let (w, h) = reader.stack.dimensions();
    let spp = reader.stack.header(0).samples_per_pixel;
    let rect = Rect {
        x: 100,
        y: 100,
        width: 200,
        height: 168,
    };
    let transforms = [
        Transform::identity(),
        Transform::similarity(1.005, 0.0075, 0.4, -0.25),
        Transform::identity(),
    ];
    let (cpu, gpu, frames) = fuse_both(&reader, &transforms, rect);
    let frames: Vec<&[f32]> = frames.iter().map(Vec::as_slice).collect();
    assert_matches_cpu_outside_near_ties("f32 fuse", &cpu, &gpu, &frames, rect, spp, FUSE_LEVELS);
    assert!(
        gpu.tile.samples.contains(&u16::MAX),
        "the highlights reach the quantize"
    );

    let clipped = gpu_backend()
        .expect("gpu backend")
        .warp_fuse_tile(
            &ClippedAtRead(&reader),
            (w, h),
            &transforms,
            rect,
            spp,
            FUSE_LEVELS,
        )
        .expect("gpu u16 fuse");
    let worst = gpu
        .tile
        .samples
        .iter()
        .zip(&clipped.tile.samples)
        .map(|(&a, &b)| (i32::from(a) - i32::from(b)).abs())
        .max()
        .unwrap();
    println!("f32 against clipped-at-read: worst |delta|={worst}");
    assert!(worst > 1, "clipping at the read must show in the composite");
}

/// ECC level inputs whose moving window is a smooth scene offset by `offset`.
struct EccInputs {
    region: Rect,
    columns: usize,
    template: Vec<f32>,
    jacobian: Vec<f64>,
    moving: Vec<f32>,
    window: (u32, u32),
}

const ECC_ORIGIN: (u32, u32) = (10, 7);
const ECC_PAD: u32 = 40;
/// Unique per prepared template for the life of the process; this module's
/// ids start above anything another module uses.
static NEXT_ECC_TEMPLATE_ID: AtomicU64 = AtomicU64::new(1 << 40);

fn scene(x: f64, y: f64, phase: f64) -> f32 {
    (30_000.0
        + 12_000.0 * (0.093 * x + 0.071 * y + phase).sin()
        + 8_000.0 * (0.047 * x - 0.113 * y + 2.0 * phase).cos()) as f32
}

impl EccInputs {
    fn new(columns: usize, offset: f32) -> Self {
        let (rw, rh) = (37u32, 23u32);
        let window = (rw + 2 * ECC_PAD, rh + 2 * ECC_PAD);
        let region = Rect {
            x: ECC_ORIGIN.0 + ECC_PAD,
            y: ECC_ORIGIN.1 + ECC_PAD,
            width: rw,
            height: rh,
        };
        let moving = (0..window.1)
            .flat_map(|j| (0..window.0).map(move |i| (i, j)))
            .map(|(i, j)| {
                scene(
                    f64::from(ECC_ORIGIN.0 + i),
                    f64::from(ECC_ORIGIN.1 + j),
                    0.4,
                ) + offset
            })
            .collect();
        let template = (0..rh)
            .flat_map(|j| (0..rw).map(move |i| (i, j)))
            .map(|(i, j)| scene(f64::from(region.x + i), f64::from(region.y + j), 0.0))
            .collect();
        Self {
            region,
            columns,
            template,
            jacobian: vec![1.0; (rw * rh) as usize * columns],
            moving,
            window,
        }
    }

    fn request(&self) -> EccSessionRequest<'_> {
        EccSessionRequest {
            template_id: NEXT_ECC_TEMPLATE_ID.fetch_add(1, Ordering::SeqCst),
            region: self.region,
            columns: self.columns,
            template: &self.template,
            jacobian: &self.jacobian,
            shift: 0.0,
            moving: &self.moving,
            moving_dims: self.window,
            moving_origin: ECC_ORIGIN,
        }
    }
}

#[test]
fn parity_f32_ecc_samples_match_cpu() {
    if shared_device_or_skip().is_none() {
        return;
    }
    let backend = gpu_backend().expect("gpu backend");
    for offset in [45_000.0, -40_000.0] {
        for columns in [2, 4] {
            let inputs = EccInputs::new(columns, offset);
            let request = inputs.request();
            let cpu = CpuMomentSession::new(&request);
            let warp = Transform::similarity(1.02, 0.03, 1.5, -1.0);
            let expected = cpu.samples(&warp);
            let got = backend
                .ecc_sample_values(&request, &warp)
                .expect("gpu samples");
            assert_eq!(got.len(), expected.len());
            let mut worst = 0.0f32;
            for (i, (g, c)) in got.iter().zip(&expected).enumerate() {
                match (g, c) {
                    (Some(g), Some(c)) => worst = worst.max((g - c).abs()),
                    (None, None) => {}
                    _ => panic!("sample {i}: gpu {g:?} against cpu {c:?}"),
                }
            }
            println!("ecc f32 offset {offset}, {columns} columns: worst |delta|={worst}");
            assert!(worst <= 1.0, "worst sample delta {worst}");
        }
    }
}

const BUDGET_LEVELS: u32 = 5;

#[test]
fn parity_f32_budget_shortfall_at_the_model_bytes() {
    if shared_device_or_skip().is_none() {
        return;
    }
    let transforms = [
        Transform::identity(),
        Transform::similarity(1.005, 0.0075, 0.4, -0.25),
    ];
    let bytes = device_bytes_per_tile(512, 512, 3, BUDGET_LEVELS, true, SampleFormat::F32);

    let at_budget = GpuBackend::with_max_device_bytes(bytes).expect("gpu backend");
    assert_eq!(
        at_budget.tile_budget_shortfall(512, 512, 3, BUDGET_LEVELS, &transforms, SampleFormat::F32),
        None
    );
    let below = GpuBackend::with_max_device_bytes(bytes - 1).expect("gpu backend");
    let message = below
        .tile_budget_shortfall(512, 512, 3, BUDGET_LEVELS, &transforms, SampleFormat::F32)
        .expect("a budget one byte short is a shortfall");
    assert!(message.contains(&format!("needs {bytes} B")), "{message}");

    // The format reaches the model: an identity `f32` tile has no base-plane
    // term, so the same budget holds it where the `u16` model needs more.
    let identity = [Transform::identity(); 2];
    let f32_identity = device_bytes_per_tile(512, 512, 3, BUDGET_LEVELS, false, SampleFormat::F32);
    let u16_identity = device_bytes_per_tile(512, 512, 3, BUDGET_LEVELS, false, SampleFormat::U16);
    assert!(f32_identity < u16_identity);
    let between = GpuBackend::with_max_device_bytes(f32_identity).expect("gpu backend");
    assert_eq!(
        between.tile_budget_shortfall(512, 512, 3, BUDGET_LEVELS, &identity, SampleFormat::F32),
        None
    );
    assert!(
        between
            .tile_budget_shortfall(512, 512, 3, BUDGET_LEVELS, &identity, SampleFormat::U16)
            .is_some()
    );
}

#[test]
fn parity_f32_budget_tile_runs_at_its_model_bytes() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let reader = MantisF32::open(30_000.0);
    let (w, h) = reader.stack.dimensions();
    let spp = reader.stack.header(0).samples_per_pixel;
    let rect = Rect {
        x: 0,
        y: 0,
        width: 256,
        height: 256,
    };
    let transforms = [
        Transform::identity(),
        Transform::similarity(1.005, 0.0075, 0.4, -0.25),
        Transform::identity(),
    ];
    let bytes = device_bytes_per_tile(256, 256, spp, BUDGET_LEVELS, true, SampleFormat::F32);
    let backend = GpuBackend::with_max_device_bytes(bytes).expect("gpu backend");
    assert_eq!(
        backend.tile_budget_shortfall(256, 256, spp, BUDGET_LEVELS, &transforms, SampleFormat::F32),
        None
    );
    let (sample, output) = sample_allocator(shared, || {
        backend
            .warp_fuse_tile(&reader, (w, h), &transforms, rect, spp, BUDGET_LEVELS)
            .expect("an f32 tile at its model bytes runs on the device")
    });
    assert_eq!(output.zmap.len(), 256 * 256);
    assert_eq!(backend.budget().peak_in_flight(), 1);
    match sample {
        Some(sample) => println!(
            "f32 allocator report (device-wide, concurrent tests share this device): \
             baseline={} peak={} delta={} model={bytes}",
            sample.baseline,
            sample.peak,
            sample.peak.saturating_sub(sample.baseline),
        ),
        None => println!("f32 allocator report unavailable on this backend; model={bytes}"),
    }
}
