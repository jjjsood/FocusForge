// SPDX-License-Identifier: GPL-3.0-or-later

//! Parity between the ECC alignment kernel and its CPU twin
//! (`CpuMomentSession`, which runs the solver's own `f64` sampler and sums),
//! plus the transfer, synchronisation and admission bounds of a session.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use focusforge_core::algorithm::{AlgorithmError, AlignAlgorithm, AlignStatus, TileReader};
use focusforge_core::align::{
    AlignDriverOptions, CpuMomentSession, FailurePolicy, PyramidalEcc, StackAlignment, Transform,
    align_stack_with,
};
use focusforge_core::gpu::{AlignBackend, ECC_MOMENTS, EccMomentSession, EccSessionRequest};
use focusforge_core::params::{ParamValue, StackContext, resolve, resolved_values};
use focusforge_core::tile::Rect;
use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::harness::{GpuBackend, gpu_backend};

/// One level's solve inputs, owned: a template and Jacobian over the region
/// and a moving window whose top-left pixel is `origin`.
struct Level {
    region: Rect,
    columns: usize,
    template: Vec<f32>,
    jacobian: Vec<f64>,
    moving: Vec<f32>,
    window: (u32, u32),
    origin: (u32, u32),
    shift: f64,
}

/// A smooth scene on the 16-bit scale: gradients of a few thousand levels per
/// pixel at most, so a sub-hundredth-pixel position error stays well under a
/// level.
fn texture(x: f64, y: f64, phase: f64) -> f32 {
    (30_000.0
        + 12_000.0 * (0.093 * x + 0.071 * y + phase).sin()
        + 8_000.0 * (0.047 * x - 0.113 * y + 2.0 * phase).cos()) as f32
}

const ORIGIN: (u32, u32) = (10, 7);
/// Pixels of window around the region on every side.
const PAD: u32 = 40;

fn level(region_size: (u32, u32), columns: usize) -> Level {
    let (rw, rh) = region_size;
    let window = (rw + 2 * PAD, rh + 2 * PAD);
    let region = Rect {
        x: ORIGIN.0 + PAD,
        y: ORIGIN.1 + PAD,
        width: rw,
        height: rh,
    };
    let moving = (0..window.1)
        .flat_map(|j| (0..window.0).map(move |i| (i, j)))
        .map(|(i, j)| texture(f64::from(ORIGIN.0 + i), f64::from(ORIGIN.1 + j), 0.4))
        .collect();
    let mut template = Vec::new();
    let mut jacobian = Vec::new();
    for j in 0..rh {
        for i in 0..rw {
            let (x, y) = (f64::from(region.x + i), f64::from(region.y + j));
            template.push(texture(x, y, 0.0));
            for k in 0..columns {
                jacobian.push(40.0 * (0.37 * x + 0.21 * y + k as f64).sin());
            }
        }
    }
    let shift = template.iter().map(|&v| f64::from(v)).sum::<f64>() / template.len() as f64;
    Level {
        region,
        columns,
        template,
        jacobian,
        moving,
        window,
        origin: ORIGIN,
        shift,
    }
}

static NEXT_TEMPLATE_ID: AtomicU64 = AtomicU64::new(1);

/// Every test here drives the device, and three read deltas of the
/// process-wide timing counters that any device call moves, so the tests must
/// not overlap.
fn exclusive() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Level {
    fn request(&self, template_id: u64) -> EccSessionRequest<'_> {
        EccSessionRequest {
            template_id,
            region: self.region,
            columns: self.columns,
            template: &self.template,
            jacobian: &self.jacobian,
            shift: self.shift,
            moving: &self.moving,
            moving_dims: self.window,
            moving_origin: self.origin,
        }
    }

    fn fresh_request(&self) -> EccSessionRequest<'_> {
        self.request(NEXT_TEMPLATE_ID.fetch_add(1, Ordering::SeqCst))
    }
}

/// A warp that moves every region pixel by a few pixels, far less than the
/// window padding: rotation and scale act about the frame origin, so their
/// size follows the region's far edge `extent`.
fn full_overlap_warp(extent: u32) -> Transform {
    let k = 87.0 / f64::from(extent);
    Transform::similarity(1.0 + 0.02 * k, 0.03 * k, 1.5, -1.0)
}

fn far_edge(level: &Level) -> u32 {
    level.region.x + level.region.width
}

/// A warp that carries the right third of the region out of the window.
fn partial_overlap_warp(region_width: u32) -> Transform {
    Transform::similarity(0.99, -0.02, f64::from(PAD + region_width / 3), 2.0)
}

fn small() -> (u32, u32) {
    (37, 23)
}

#[test]
fn ecc_samples_match_cpu_within_one_lsb() {
    let _serial = exclusive();
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");
    for columns in [2, 4] {
        let level = level(small(), columns);
        let request = level.fresh_request();
        let cpu = CpuMomentSession::new(&request);
        for warp in [
            full_overlap_warp(far_edge(&level)),
            Transform::similarity(0.97, -0.05, 2.0, 3.0),
        ] {
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
            println!("ecc sample parity: worst |delta| {worst} levels");
            assert!(worst <= 1.0, "worst sample delta {worst} levels");
        }
    }
}

#[test]
fn ecc_valid_mask_matches_cpu_at_window_edges() {
    let _serial = exclusive();
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");
    let level = level(small(), 4);
    let request = level.fresh_request();
    let cpu = CpuMomentSession::new(&request);
    let (mut saw_valid, mut saw_invalid) = (false, false);
    // Slide the region over the window's right and bottom edges in steps
    // that land its taps at unrelated fractions of a pixel.
    for step in 0..48 {
        let warp = Transform::similarity(
            1.0 + 0.0007 * f64::from(step),
            0.011,
            f64::from(PAD) - 14.0 + 0.371 * f64::from(step),
            f64::from(PAD) - 9.0 + 0.293 * f64::from(step),
        );
        let expected = cpu.samples(&warp);
        let got = backend
            .ecc_sample_values(&request, &warp)
            .expect("gpu samples");
        let pattern =
            |samples: &[Option<f32>]| samples.iter().map(Option::is_some).collect::<Vec<_>>();
        assert_eq!(pattern(&got), pattern(&expected), "step {step}");
        saw_valid |= expected.iter().any(Option::is_some);
        saw_invalid |= expected.iter().any(Option::is_none);
    }
    assert!(
        saw_valid && saw_invalid,
        "the slide crosses the window edge"
    );
}

/// Relative bound on a moment's absolute error: `BOUND * valid * magnitude`,
/// where `magnitude` is the largest absolute value one summand can take.
/// Single-precision sampling error stays near 1e-7 of the summand scale
/// (the solver's `rho` agrees to 1.1e-7); measured worst cases sit near 3e-7,
/// and the bound leaves more than an order of magnitude of headroom for the
/// order of summation.
const MOMENT_BOUND: f64 = 1e-5;

fn assert_moments_match(level: &Level, warp: &Transform, label: &str, partial: bool) {
    let backend = gpu_backend().expect("gpu backend");
    let request = level.fresh_request();
    let mut cpu = CpuMomentSession::new(&request);
    let mut gpu = backend
        .open_ecc(&request)
        .expect("open")
        .expect("the level fits the device");
    let expected = cpu.moments(warp).expect("cpu moments");
    let got = gpu.moments(warp).expect("gpu moments");

    let valid = expected[0];
    assert_eq!(got[0], valid, "{label}: valid count");
    assert_eq!(
        valid < level.template.len() as f64,
        partial,
        "{label}: overlap"
    );

    let amplitude =
        |values: &mut dyn Iterator<Item = f64>| values.fold(0.0f64, |a, v| a.max(v.abs()));
    let s_max = amplitude(
        &mut CpuMomentSession::new(&request)
            .samples(warp)
            .into_iter()
            .flatten()
            .map(|s| f64::from(s) - level.shift),
    );
    let t_max = amplitude(&mut level.template.iter().map(|&t| f64::from(t) - level.shift));
    let g_max = amplitude(&mut level.jacobian.iter().copied());
    let mut magnitude = [0.0f64; ECC_MOMENTS];
    magnitude[1] = s_max;
    magnitude[2] = s_max * s_max;
    magnitude[3] = t_max * s_max;
    magnitude[8] = t_max;
    magnitude[9] = t_max * t_max;
    for k in 0..4 {
        magnitude[4 + k] = g_max * s_max;
        magnitude[10 + k] = g_max * t_max;
        magnitude[14 + k] = g_max;
    }

    let mut worst = 0.0f64;
    // Under full overlap the kernel leaves the template-side sums unwritten.
    let compared = if partial { ECC_MOMENTS } else { 8 };
    for i in 1..compared {
        let error = (got[i] - expected[i]).abs();
        let tolerance = MOMENT_BOUND * valid * magnitude[i];
        if magnitude[i] > 0.0 {
            worst = worst.max(error / (valid * magnitude[i]));
        }
        assert!(
            error <= tolerance,
            "{label}: moment {i} gpu {} against cpu {} (error {error}, bound {tolerance})",
            got[i],
            expected[i]
        );
    }
    if !partial {
        assert!(got[8..].iter().all(|&v| v == 0.0), "{label}: template sums");
    }
    // Entries for columns past the request's are zero on both sides.
    for k in level.columns..4 {
        assert_eq!(got[4 + k], 0.0, "{label}: g{k}*s");
        assert_eq!(got[10 + k], 0.0, "{label}: g{k}*t");
        assert_eq!(got[14 + k], 0.0, "{label}: g{k}");
    }
    println!(
        "ecc moments parity {label}: worst relative error {worst:.2e} (bound {MOMENT_BOUND:e})"
    );
}

#[test]
fn ecc_moments_match_f64_reduction() {
    let _serial = exclusive();
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    for (size, name) in [(small(), "37x23"), ((1_100, 950), "1100x950")] {
        for columns in [2, 4] {
            let level = level(size, columns);
            assert_moments_match(
                &level,
                &full_overlap_warp(far_edge(&level)),
                &format!("{name} columns={columns} full"),
                false,
            );
            assert_moments_match(
                &level,
                &partial_overlap_warp(size.0),
                &format!("{name} columns={columns} partial"),
                true,
            );
        }
    }
}

#[test]
fn ecc_iteration_submits_once_and_reads_back_once() {
    let _serial = exclusive();
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    enable_trace_accounting();
    let backend = gpu_backend().expect("gpu backend");
    let level = level(small(), 4);
    let request = level.fresh_request();
    let mut session = backend
        .open_ecc(&request)
        .expect("open")
        .expect("the level fits the device");
    for warp in [
        full_overlap_warp(far_edge(&level)),
        partial_overlap_warp(small().0),
        full_overlap_warp(far_edge(&level)),
    ] {
        let before = timing_snapshot().expect("accounting is enabled");
        session.moments(&warp).expect("gpu moments");
        let after = timing_snapshot().expect("accounting is enabled");
        assert_eq!(after.cpu_submissions - before.cpu_submissions, 1);
        assert_eq!(
            after.readback_bytes - before.readback_bytes,
            72,
            "18 moments of 4 bytes"
        );
        assert_eq!(after.buffer_creations - before.buffer_creations, 0);
        assert_eq!(after.upload_bytes - before.upload_bytes, 64, "the params");
    }
}

#[test]
fn ecc_sessions_share_a_resident_template_and_are_admitted() {
    let _serial = exclusive();
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    enable_trace_accounting();
    let backend = GpuBackend::with_max_device_bytes(1 << 30).expect("a backend over the device");
    let level = level(small(), 2);
    let template_bytes = ((level.template.len() + level.jacobian.len()) * 4) as u64;
    // A session also uploads its zeroed 64-byte parameter block.
    let moving_bytes = (level.moving.len() * 4) as u64 + 64;
    let id = NEXT_TEMPLATE_ID.fetch_add(1, Ordering::SeqCst);
    let uploaded = |open: &mut dyn FnMut()| {
        let before = timing_snapshot().expect("accounting is enabled");
        open();
        timing_snapshot()
            .expect("accounting is enabled")
            .upload_bytes
            - before.upload_bytes
    };
    let open = || {
        backend
            .open_ecc(&level.request(id))
            .expect("open")
            .expect("the level fits the device")
    };

    let mut first = None;
    let mut second = None;
    assert_eq!(
        uploaded(&mut || first = Some(open())),
        template_bytes + moving_bytes,
        "the first session uploads the template and its window"
    );
    assert_eq!(
        uploaded(&mut || second = Some(open())),
        moving_bytes,
        "a live template is not uploaded again"
    );
    assert_eq!(backend.budget().peak_in_flight(), 2, "both are admitted");
    drop((first, second));
    assert_eq!(
        uploaded(&mut || drop(open())),
        template_bytes + moving_bytes,
        "a template no session holds is released"
    );
}

#[test]
fn ecc_open_declines_a_window_over_the_buffer_limit() {
    let _serial = exclusive();
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    enable_trace_accounting();
    let backend = GpuBackend::with_max_device_bytes(1 << 30).expect("a backend over the device");
    let limit = shared.limits().max_storage_buffer_binding_size;
    let side = ((limit / 4) as f64).sqrt().ceil() as u32 + 1;
    assert!(u64::from(side) * u64::from(side) * 4 > limit);
    let region = Rect {
        x: 4,
        y: 4,
        width: 8,
        height: 8,
    };
    let template = vec![1.0f32; 64];
    let jacobian = vec![1.0f64; 64 * 4];
    let moving = vec![0.5f32; side as usize * side as usize];
    let request = EccSessionRequest {
        template_id: NEXT_TEMPLATE_ID.fetch_add(1, Ordering::SeqCst),
        region,
        columns: 4,
        template: &template,
        jacobian: &jacobian,
        shift: 1.0,
        moving: &moving,
        moving_dims: (side, side),
        moving_origin: (0, 0),
    };
    let before = timing_snapshot().expect("accounting is enabled");
    assert!(
        backend
            .open_ecc(&request)
            .expect("a decline is not an error")
            .is_none()
    );
    let after = timing_snapshot().expect("accounting is enabled");
    assert_eq!(after.buffer_creations, before.buffer_creations);
    assert_eq!(after.upload_bytes, before.upload_bytes);
}

/// Frames in memory, one `u16` sample per pixel.
struct MemReader {
    width: u32,
    frames: Vec<Vec<u16>>,
}

impl TileReader for MemReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        for row in 0..rect.height as usize {
            let src = (rect.y as usize + row) * self.width as usize + rect.x as usize;
            out[row * rect.width as usize..][..rect.width as usize]
                .copy_from_slice(&self.frames[frame][src..src + rect.width as usize]);
        }
        Ok(())
    }
}

/// Sinusoids of 17 to 751 px wavelength: textured at every scale of the
/// alignment pyramid, with enough long-range structure to pull in a shift of
/// a couple of hundred pixels, and never periodic over a frame.
fn scene(x: f64, y: f64) -> f64 {
    const RIPPLES: [(f64, f64, f64, f64); 10] = [
        (1.0 / 613.0, 1.0 / 751.0, 0.7, 1.6),
        (-1.0 / 487.0, 1.0 / 529.0, 2.9, 1.4),
        (1.0 / 97.0, 1.0 / 131.0, 0.3, 1.0),
        (1.0 / 61.0, -1.0 / 83.0, 1.7, 0.8),
        (-1.0 / 47.0, 1.0 / 71.0, 4.1, 0.6),
        (1.0 / 113.0, 1.0 / 37.0, 2.4, 0.7),
        (1.0 / 23.0, 1.0 / 29.0, 0.9, 0.5),
        (-1.0 / 19.0, 1.0 / 17.0, 5.0, 0.4),
        (1.0 / 31.0, 1.0 / 43.0, 3.3, 0.5),
        (-1.0 / 41.0, -1.0 / 27.0, 1.1, 0.4),
    ];
    RIPPLES
        .iter()
        .map(|&(fx, fy, phase, amp)| {
            amp * (std::f64::consts::TAU * (fx * x + fy * y) + phase).sin()
        })
        .sum()
}

fn mem_reader(width: u32, height: u32, shifts: &[(f64, f64)]) -> MemReader {
    MemReader {
        width,
        frames: shifts
            .iter()
            .map(|&(dx, dy)| {
                (0..height)
                    .flat_map(|y| (0..width).map(move |x| (x, y)))
                    .map(|(x, y)| {
                        let v = scene(f64::from(x) - dx, f64::from(y) - dy);
                        (32768.0 + 6000.0 * v).clamp(0.0, 65535.0) as u16
                    })
                    .collect()
            })
            .collect(),
    }
}

/// Forwards to the device and counts the iterations whose valid count fell
/// short of the whole region.
struct PartialOverlapWatch<'a> {
    inner: &'a GpuBackend,
    partial_iterations: Arc<AtomicUsize>,
}

struct WatchedSession {
    inner: Box<dyn EccMomentSession>,
    region_pixels: f64,
    partial_iterations: Arc<AtomicUsize>,
}

impl AlignBackend for PartialOverlapWatch<'_> {
    fn open_ecc(
        &self,
        request: &EccSessionRequest<'_>,
    ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
        Ok(self.inner.open_ecc(request)?.map(|inner| {
            Box::new(WatchedSession {
                inner,
                region_pixels: request.template.len() as f64,
                partial_iterations: Arc::clone(&self.partial_iterations),
            }) as Box<dyn EccMomentSession>
        }))
    }
}

impl EccMomentSession for WatchedSession {
    fn moments(&mut self, warp: &Transform) -> Result<[f64; ECC_MOMENTS], AlgorithmError> {
        let moments = self.inner.moments(warp)?;
        if moments[0] < self.region_pixels {
            self.partial_iterations.fetch_add(1, Ordering::SeqCst);
        }
        Ok(moments)
    }
}

fn corner_error(a: &Transform, b: &Transform, (width, height): (u32, u32)) -> f64 {
    let (right, bottom) = (f64::from(width - 1), f64::from(height - 1));
    [(0.0, 0.0), (right, 0.0), (0.0, bottom), (right, bottom)]
        .iter()
        .map(|&(x, y)| {
            let (ax, ay) = a.map_point(x, y).expect("a maps the corner");
            let (bx, by) = b.map_point(x, y).expect("b maps the corner");
            ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
        })
        .fold(0.0, f64::max)
}

#[test]
fn gpu_ecc_solve_meets_cpu_geometry() {
    let _serial = exclusive();
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    const W: u32 = 1501;
    const H: u32 = 1403;
    // The level-0 solve region is 1059x989 px, above the device threshold. The
    // last frame sits further right than the level-0 window's margin allows,
    // so part of its region samples fall outside the frame.
    let mem = mem_reader(W, H, &[(0.0, 0.0), (3.4, -2.1), (230.0, 5.0)]);
    let stack = StackContext::new(3, W, H, 1);
    let mut cfg = resolved_values(
        &resolve(PyramidalEcc.params(), &Default::default(), &stack).expect("default parameters"),
    );
    cfg.insert(
        "mode".to_string(),
        ParamValue::Enum("translation".to_string()),
    );
    let gpu = gpu_backend().expect("gpu backend");
    let run = |backend: Option<&dyn AlignBackend>| {
        align_stack_with(
            &PyramidalEcc,
            stack,
            &mem,
            &cfg,
            FailurePolicy::Strict,
            &AlignDriverOptions {
                align_backend: backend,
                ..AlignDriverOptions::default()
            },
        )
        .expect("alignment")
    };
    let watch = PartialOverlapWatch {
        inner: gpu,
        partial_iterations: Arc::new(AtomicUsize::new(0)),
    };
    let cpu_run = run(None);
    let gpu_run = run(Some(&watch));
    assert!(gpu_run.backend_used, "level 0 ran on the device");
    assert_eq!(gpu_run.rerun_reason, None);
    assert!(
        watch.partial_iterations.load(Ordering::SeqCst) > 0,
        "the shifted frame's level-0 samples partially left the window"
    );
    for run in [&cpu_run, &gpu_run] {
        let moved = &run.alignment.frames[2];
        assert!(
            !matches!(moved.status, AlignStatus::Diverged(_)),
            "{:?}",
            moved.status
        );
        assert!((moved.transform.m[0][2] - 226.6).abs() < 0.1);
    }

    let worst = |a: &StackAlignment, b: &StackAlignment| {
        a.frames
            .iter()
            .zip(&b.frames)
            .map(|(x, y)| corner_error(&x.transform, &y.transform, (W, H)))
            .fold(0.0, f64::max)
    };
    let error = worst(&gpu_run.alignment, &cpu_run.alignment);
    println!("gpu against cpu alignment: worst corner error {error:.5} px");
    for (g, c) in gpu_run
        .alignment
        .frames
        .iter()
        .zip(&cpu_run.alignment.frames)
    {
        assert_eq!(g.status, c.status, "frame {}", g.frame);
    }
    assert!(error <= 0.04, "corner error {error} px exceeds 0.04 px");
}
