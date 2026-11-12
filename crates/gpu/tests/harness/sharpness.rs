// SPDX-License-Identifier: GPL-3.0-or-later

//! The GPU score chain behind the stack sharpness maps and the sharp-area
//! count against their CPU twins: `sharpness_maps` and
//! `count_sharp_areas_reporting_with_backend` through the device, compared
//! with `sharpness_map_tile` and `count_sharp_areas_reporting`.

use crate::full_frame_render_identity::core_common;

use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::gpu::{ScalePeaks, SharpnessBackend};
use focusforge_core::io::{TiffStack, frame_paths};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::sharp_areas::{
    SharpAreas, count_sharp_areas_reporting, count_sharp_areas_reporting_with_backend,
};
use focusforge_core::sharpness_map::{
    BINS, PeakErrorBound, bin_peaks, fold_peaks, sharpness_map_tile, sharpness_maps,
    sharpness_plan, stack_sharpness_scale, stack_sharpness_scale_with_backend,
};
use focusforge_core::tile::{NoControl, PlannedTile, Rect, TilePlan};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::harness::{gpu_backend, sharpness_backend_for};

/// Relative bound of a per-pixel GPU score against the CPU score.
const EPSILON: f32 = 1e-4;

/// The device backend with the production threshold lowered to zero, counting
/// the tiles it served.
struct Forced {
    inner: &'static dyn SharpnessBackend,
    served: AtomicUsize,
}

impl Forced {
    fn new(inner: &'static dyn SharpnessBackend) -> Self {
        Self {
            inner,
            served: AtomicUsize::new(0),
        }
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

impl SharpnessBackend for Forced {
    fn min_pixel_frames(&self) -> u64 {
        0
    }

    fn device_lost(&self) -> bool {
        self.inner.device_lost()
    }

    fn score_tile(
        &self,
        reader: &dyn TileReader,
        frame: usize,
        read: Rect,
        spp: u32,
    ) -> Result<Option<Vec<f32>>, AlgorithmError> {
        let scores = self.inner.score_tile(reader, frame, read, spp)?;
        if scores.is_some() {
            self.served.fetch_add(1, Ordering::SeqCst);
        }
        Ok(scores)
    }

    fn open_scale(
        &self,
        plan: &TilePlan,
        spp: u32,
    ) -> Result<Option<Box<dyn ScalePeaks + '_>>, AlgorithmError> {
        self.inner.open_scale(plan, spp)
    }
}

fn device_backend() -> Option<Forced> {
    shared_device_or_skip()?;
    Some(Forced::new(
        gpu_backend().expect("the shared device probes a backend"),
    ))
}

/// `|gpu - cpu| * 65535 / max(cpu, 1)`: the 16-bit LSB metric.
fn lsb(gpu: f32, cpu: f32) -> f32 {
    (gpu - cpu).abs() * 65535.0 / cpu.max(1.0)
}

/// Maps `sharpness_maps` writes through `backend`, each checked against
/// `sharpness_map_tile` on the same tile: the worst LSB and the tiles seen.
fn worst_map_lsb(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    scale: f32,
    tile_size: u32,
    backend: &dyn SharpnessBackend,
) -> (f32, usize) {
    let plan = sharpness_plan(extent, tile_size).unwrap();
    let (mut worst, mut tiles) = (0.0f32, 0);
    sharpness_maps(
        reader,
        frames,
        extent,
        spp,
        scale,
        tile_size,
        &NoControl,
        Some(backend),
        |frame,
         interior: Rect,
         gpu: &[f32]|
         -> Result<(), focusforge_core::sharp_areas::SharpAreasError> {
            let planned = plan
                .tiles()
                .iter()
                .find(|tile| tile.interior() == interior)
                .expect("a written rect is a planned interior");
            let cpu = sharpness_map_tile(reader, frame, planned, spp, scale)?;
            assert_eq!(gpu.len(), cpu.len());
            for (g, c) in gpu.iter().zip(&cpu) {
                worst = worst.max(lsb(*g, *c));
            }
            tiles += 1;
            Ok(())
        },
    )
    .unwrap();
    (worst, tiles)
}

fn values(pairs: &[(&str, ParamValue)]) -> ParamValues {
    pairs
        .iter()
        .map(|(id, value)| ((*id).to_string(), value.clone()))
        .collect()
}

fn count_both(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    values: &ParamValues,
    tile_size: u32,
    backend: &dyn SharpnessBackend,
) -> (SharpAreas, SharpAreas) {
    let run = |backend: Option<&dyn SharpnessBackend>| {
        count_sharp_areas_reporting_with_backend(
            reader,
            frames,
            extent,
            spp,
            values,
            tile_size,
            &NoControl,
            &mut |_| {},
            backend,
        )
        .unwrap()
    };
    (run(Some(backend)), run(None))
}

#[test]
fn parity_sharpness_map_within_one_lsb() {
    let Some(backend) = device_backend() else {
        return;
    };
    let stack = crate::common::open_stack();
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    for tile_size in [512, 200] {
        let scale =
            stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
        assert!(scale > 0.0, "the fixture has detail");
        let tiles = sharpness_plan(extent, tile_size).unwrap().tiles().len();
        for scale in [scale, 0.0, -1.0] {
            let served = backend.served();
            let (worst, seen) =
                worst_map_lsb(&stack, frames, extent, spp, scale, tile_size, &backend);
            assert_eq!(seen, frames * tiles, "tile {tile_size}: every frame tile");
            assert_eq!(
                backend.served() - served,
                frames * tiles,
                "tile {tile_size}: every frame tile on the device"
            );
            assert!(worst <= 1.0, "tile {tile_size}, scale {scale}: {worst} LSB");
        }
    }
}

#[test]
fn parity_sharp_areas_tallies_equal() {
    let Some(backend) = device_backend() else {
        return;
    };
    let stack = crate::common::open_stack();
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    for tile_size in [512, 200] {
        let (worst, _) = worst_map_lsb(&stack, frames, extent, spp, 1.0, tile_size, &backend);
        assert!(
            worst > 0.0,
            "the device scores differ from the CPU bit for bit"
        );
        let mut worst_relative = 0.0f32;
        let plan = sharpness_plan(extent, tile_size).unwrap();
        for planned in plan.tiles() {
            let (read, interior) = (planned.read(), planned.interior());
            let gpu = backend.score_tile(&stack, 0, read, spp).unwrap().unwrap();
            let cpu = sharpness_map_tile(&stack, 0, planned, spp, 1.0).unwrap();
            for y in 0..interior.height {
                for x in 0..interior.width {
                    let at =
                        ((y + interior.y - read.y) * read.width + x + interior.x - read.x) as usize;
                    let c = cpu[(y * interior.width + x) as usize];
                    worst_relative = worst_relative.max((gpu[at] - c).abs() / c.max(1.0));
                }
            }
        }
        assert!(
            worst_relative <= EPSILON,
            "tile {tile_size}: {worst_relative}"
        );
        for overlap in [None, Some(0.5), Some(1.0)] {
            let params = match overlap {
                Some(share) => values(&[("overlap-threshold", ParamValue::Float(share))]),
                None => ParamValues::new(),
            };
            let (gpu, cpu) = count_both(&stack, frames, extent, spp, &params, tile_size, &backend);
            assert_eq!(gpu, cpu, "tile {tile_size}, overlap-threshold {overlap:?}");
        }
    }
    assert!(backend.served() > 0);
}

/// A grey-textured stack whose second frame is flat from pixel `k` on in
/// row-major order, so frame 1's cell score rises with `k`.
struct Synthetic {
    frames: Vec<Vec<u16>>,
    side: u32,
}

const SIDE: u32 = 512;

impl TileReader for Synthetic {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        for (row, chunk) in out.chunks_exact_mut(rect.width as usize).enumerate() {
            let start = (rect.y as usize + row) * self.side as usize + rect.x as usize;
            chunk.copy_from_slice(&self.frames[frame][start..start + chunk.len()]);
        }
        Ok(())
    }
}

fn texture(x: u32, y: u32) -> u16 {
    let h = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)).wrapping_mul(2_246_822_519);
    ((h >> 16) as u16) | 0x4000
}

fn textured() -> Vec<u16> {
    (0..SIDE * SIDE)
        .map(|i| texture(i % SIDE, i / SIDE))
        .collect()
}

fn flat_from(k: usize) -> Vec<u16> {
    let mut frame = textured();
    frame[k..].fill(30_000);
    frame
}

/// The mean CPU focus score of the one cell `frame` of `stack` forms.
fn cell_mean(stack: &Synthetic, frame: usize) -> f64 {
    let extent = (SIDE, SIDE);
    let mut sum = 0.0f64;
    sharpness_maps(
        stack,
        stack.frames.len(),
        extent,
        1,
        1.0,
        SIDE,
        &NoControl,
        None,
        |index, _, map: &[f32]| -> Result<(), focusforge_core::sharp_areas::SharpAreasError> {
            if index == frame {
                sum += map.iter().map(|v| f64::from(*v)).sum::<f64>();
            }
            Ok(())
        },
    )
    .unwrap();
    sum / f64::from(SIDE * SIDE)
}

/// The stack of `others` flat frames after a textured frame 0 and a frame 1
/// whose cell score is `share` of frame 0's, to 2e-5 relative.
fn stack_with_share(share: f64, others: usize) -> Synthetic {
    let build = |k: usize| {
        let mut frames = vec![textured(), flat_from(k)];
        frames.extend((0..others).map(|_| vec![30_000; (SIDE * SIDE) as usize]));
        Synthetic { frames, side: SIDE }
    };
    let (mut low, mut high) = (0usize, (SIDE * SIDE) as usize);
    while high - low > 1 {
        let mid = (low + high) / 2;
        let stack = build(mid);
        if cell_mean(&stack, 1) / cell_mean(&stack, 0) < share {
            low = mid;
        } else {
            high = mid;
        }
    }
    let stack = build(high);
    let got = cell_mean(&stack, 1) / cell_mean(&stack, 0);
    assert!(
        (got - share).abs() / share < 2e-5,
        "built share {got} for {share}"
    );
    stack
}

fn one_cell_count(
    stack: &Synthetic,
    params: &ParamValues,
    backend: Option<&dyn SharpnessBackend>,
) -> SharpAreas {
    count_sharp_areas_reporting_with_backend(
        stack,
        stack.frames.len(),
        (SIDE, SIDE),
        1,
        params,
        SIDE,
        &NoControl,
        &mut |_| {},
        backend,
    )
    .unwrap()
}

#[test]
fn parity_sharp_areas_near_threshold() {
    let Some(backend) = device_backend() else {
        return;
    };
    let eps = f64::from(EPSILON);
    let one_cell = values(&[("cell-size", ParamValue::Int(512))]);
    // peak / floor of frames 0 and 1 at 2ε above and below `contrast` = 2.
    for (side, expected) in [(1.0 + 2.0 * eps, vec![1, 0]), (1.0 - 2.0 * eps, vec![0, 0])] {
        let stack = stack_with_share(1.0 / (2.0 * side), 0);
        let cpu = one_cell_count(&stack, &one_cell, None);
        assert_eq!(cpu.counts, expected, "the built cell at {side} × contrast");
        assert_eq!(one_cell_count(&stack, &one_cell, Some(&backend)), cpu);
    }
    // frame 1's score at 2ε above and below 0.8 × the peak, a flat frame 2
    // keeping the cell counted.
    for (side, sharp) in [(1.0 + 2.0 * eps, 1), (1.0 - 2.0 * eps, 0)] {
        let stack = stack_with_share(0.8 * side, 1);
        let cpu = one_cell_count(&stack, &one_cell, None);
        assert_eq!(cpu.counts, vec![1, 0, 0]);
        assert_eq!(
            cpu.overlaps[0].both, sharp,
            "the built cell at {side} × threshold"
        );
        assert_eq!(one_cell_count(&stack, &one_cell, Some(&backend)), cpu);
    }
    assert!(backend.served() > 0);
}

#[test]
fn sharpness_device_lost_finishes_on_the_cpu() {
    let Some(backend) = device_backend() else {
        return;
    };
    let stack = crate::common::open_stack();
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    let tile_size = 200;
    let (gpu, cpu) = count_both(
        &stack,
        frames,
        extent,
        spp,
        &ParamValues::new(),
        tile_size,
        &backend,
    );
    assert_eq!(gpu, cpu);
    assert!(
        backend.device_lost(),
        "FF_FORCE_DEVICE_LOST=3 destroys the device during the count"
    );
    let scale = stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
    let (worst, tiles) = worst_map_lsb(&stack, frames, extent, spp, scale, tile_size, &backend);
    assert!(tiles > 0);
    assert!(worst <= 1.0, "{worst} LSB after the loss");
    let counted = count_sharp_areas_reporting(
        &stack,
        frames,
        extent,
        spp,
        &ParamValues::new(),
        tile_size,
        &NoControl,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(counted, cpu);
}

fn open_reference_stack(dir: &std::path::Path) -> TiffStack {
    let paths: Vec<_> = frame_paths(dir)
        .expect("the reference stack directory must enumerate")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .collect();
    TiffStack::open_files(&paths).expect("the reference frames open")
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn reference_stack_sharpness_matches_cpu() {
    let Some(dir) = core_common::reference_stack_dir() else {
        return;
    };
    let Some(_) = shared_device_or_skip() else {
        return;
    };
    let backend = sharpness_backend_for(false, None).expect("the GPU is allowed");
    let stack = open_reference_stack(&dir);
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    let tile_size = focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
    let started = std::time::Instant::now();
    let scale = stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
    println!("scale {scale} in {:.1?}", started.elapsed());
    let (worst, tiles) = worst_map_lsb(&stack, frames, extent, spp, scale, tile_size, backend);
    println!(
        "maps: worst {worst} LSB over {tiles} frame tiles in {:.1?}",
        started.elapsed()
    );
    assert!(worst <= 1.0);
    let params = ParamValues::new();
    let started = std::time::Instant::now();
    let gpu = count_sharp_areas_reporting_with_backend(
        &stack,
        frames,
        extent,
        spp,
        &params,
        tile_size,
        &NoControl,
        &mut |_| {},
        Some(backend),
    )
    .unwrap();
    println!("gpu count in {:.1?}", started.elapsed());
    let started = std::time::Instant::now();
    let cpu = count_sharp_areas_reporting(
        &stack,
        frames,
        extent,
        spp,
        &params,
        tile_size,
        &NoControl,
        &mut |_| {},
    )
    .unwrap();
    println!("cpu count in {:.1?}", started.elapsed());
    assert_eq!(gpu, cpu);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn reference_stack_sharpness_stage_speedup() {
    let Some(dir) = core_common::reference_stack_dir() else {
        return;
    };
    let Some(_) = shared_device_or_skip() else {
        return;
    };
    let backend = sharpness_backend_for(false, None).expect("the GPU is allowed");
    let stack = open_reference_stack(&dir);
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    let tile_size = focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
    let scale = stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
    let params = ParamValues::new();

    let count = |backend: Option<&dyn SharpnessBackend>| {
        count_sharp_areas_reporting_with_backend(
            &stack,
            frames,
            extent,
            spp,
            &params,
            tile_size,
            &NoControl,
            &mut |_| {},
            backend,
        )
        .unwrap();
    };
    let maps = |backend: Option<&dyn SharpnessBackend>| {
        sharpness_maps(
            &stack,
            frames,
            extent,
            spp,
            scale,
            tile_size,
            &NoControl,
            backend,
            |_, _, _: &[f32]| -> Result<(), focusforge_core::sharp_areas::SharpAreasError> {
                Ok(())
            },
        )
        .unwrap();
    };
    // One untimed pass, then the timed one.
    let warm = |run: &dyn Fn()| {
        run();
        let started = std::time::Instant::now();
        run();
        started.elapsed()
    };

    for (stage, gpu, cpu) in [
        (
            "count",
            warm(&|| count(Some(backend))),
            warm(&|| count(None)),
        ),
        ("maps", warm(&|| maps(Some(backend))), warm(&|| maps(None))),
    ] {
        let ratio = cpu.as_secs_f64() / gpu.as_secs_f64();
        println!("{stage}: gpu {gpu:.2?}, cpu {cpu:.2?}, {ratio:.2}x");
        assert!(
            ratio >= 2.0,
            "{stage} stage: gpu {gpu:.2?} against cpu {cpu:.2?} is {ratio:.2}x, below 2.0x"
        );
    }
}

fn bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|v| v.to_bits()).collect()
}

/// Runs `body(frame)` for every frame on the machine's threads.
fn on_threads(frames: usize, body: impl Fn(usize) + Sync) {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(frames.max(1));
    std::thread::scope(|scope| {
        for first in 0..threads {
            let body = &body;
            scope.spawn(move || (first..frames).step_by(threads).for_each(body));
        }
    });
}

/// Folds every frame of every tile of the plan into `session`.
fn fold_all(session: &dyn ScalePeaks, reader: &dyn TileReader, frames: usize, tiles: usize) {
    for tile in 0..tiles {
        on_threads(frames, |frame| {
            session
                .fold(tile, reader, frame)
                .expect("the device folds the frame")
        });
    }
}

/// The exact CPU peaks of `planned`'s interior: the maximum over the frames
/// of the shipped scores.
fn exact_tile_peaks(
    reader: &dyn TileReader,
    frames: usize,
    planned: &PlannedTile,
    spp: u32,
) -> Vec<f32> {
    let interior = planned.interior();
    let peaks = std::sync::Mutex::new(vec![0.0f32; (interior.width * interior.height) as usize]);
    on_threads(frames, |frame| {
        let map = sharpness_map_tile(reader, frame, planned, spp, 1.0).unwrap();
        let mut peaks = peaks.lock().unwrap();
        for (peak, value) in peaks.iter_mut().zip(&map) {
            *peak = peak.max(*value);
        }
    });
    peaks.into_inner().unwrap()
}

/// The device peaks of a folded `session` against the exact CPU peaks: how
/// many lie farther than the error bound, and the largest `|g - c| / bound(c)`.
fn peaks_against_exact(
    session: &dyn ScalePeaks,
    reader: &dyn TileReader,
    frames: usize,
    spp: u32,
    plan: &TilePlan,
) -> (u64, f64) {
    let bound = PeakErrorBound::for_spp(spp);
    let (mut outside, mut worst) = (0, 0.0f64);
    for (index, planned) in plan.tiles().iter().enumerate() {
        let device = session.interior_peaks(index).unwrap();
        let exact = exact_tile_peaks(reader, frames, planned, spp);
        assert_eq!(device.len(), exact.len());
        for (g, c) in device.iter().zip(&exact) {
            let ratio = f64::from((g - c).abs()) / bound.at(f64::from(*c));
            outside += u64::from(ratio > 1.0);
            worst = worst.max(ratio);
        }
    }
    (outside, worst)
}

#[test]
fn parity_sharpness_scale() {
    let Some(backend) = device_backend() else {
        return;
    };
    let stack = crate::common::open_stack();
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    for tile_size in [512, 200] {
        let plan = sharpness_plan(extent, tile_size).unwrap();
        let tiles = plan.tiles().len();
        let mut session = backend
            .open_scale(&plan, spp)
            .unwrap()
            .expect("the device holds the fixture's peaks");
        fold_all(&*session, &stack, frames, tiles);

        // The kernels against their twins on the device's own scores.
        let mut twin_histogram = vec![0u64; BINS];
        for (index, planned) in plan.tiles().iter().enumerate() {
            let (read, interior) = (planned.read(), planned.interior());
            let mut twin = vec![0.0f32; (interior.width * interior.height) as usize];
            for frame in 0..frames {
                let scores = backend
                    .score_tile(&stack, frame, read, spp)
                    .unwrap()
                    .expect("the device scores the tile");
                fold_peaks(&mut twin, &scores, read, interior);
            }
            let device = session.interior_peaks(index).unwrap();
            assert_eq!(
                bits(&device),
                bits(&twin),
                "tile {tile_size}, tile {index}: fold"
            );
            bin_peaks(&mut twin_histogram, &twin);
        }
        assert_eq!(
            session.histogram().unwrap(),
            twin_histogram,
            "tile {tile_size}: bin"
        );

        // The device peaks against the exact CPU peaks.
        let (outside, worst) = peaks_against_exact(&*session, &stack, frames, spp, &plan);
        println!(
            "tile {tile_size}: {outside} peaks outside ε, largest |g - c| / ε(c) = {worst:.4}"
        );
        assert_eq!(outside, 0, "tile {tile_size}: peaks outside ε");
        drop(session);

        // The scale through the backend against the shipped scale.
        let shipped =
            stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
        let got = stack_sharpness_scale_with_backend(
            &stack,
            frames,
            extent,
            spp,
            tile_size,
            &NoControl,
            Some(&backend),
        )
        .unwrap();
        assert_eq!(
            got.value.to_bits(),
            shipped.to_bits(),
            "tile {tile_size}: scale"
        );
        let recheck = got
            .recheck
            .expect("the device path settled the scale and did not fall back");
        println!(
            "tile {tile_size}: scale {:#010x}, iterations {}, candidates {:?}",
            got.value.to_bits(),
            recheck.iterations,
            recheck.candidates
        );
    }
}

#[test]
fn sharpness_scale_device_lost() {
    let Some(backend) = device_backend() else {
        return;
    };
    let stack = crate::common::open_stack();
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    let tile_size = 200;
    let shipped =
        stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
    let got = stack_sharpness_scale_with_backend(
        &stack,
        frames,
        extent,
        spp,
        tile_size,
        &NoControl,
        Some(&backend),
    )
    .unwrap();
    assert!(
        backend.device_lost(),
        "FF_FORCE_DEVICE_LOST=3 destroys the device during the pass"
    );
    assert_eq!(got.value.to_bits(), shipped.to_bits());
    assert!(got.recheck.is_none(), "the CPU pass produced the scale");
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn reference_stack_sharpness_scale_matches_cpu() {
    let Some(dir) = core_common::reference_stack_dir() else {
        return;
    };
    let Some(_) = shared_device_or_skip() else {
        return;
    };
    let backend = sharpness_backend_for(false, None).expect("the GPU is allowed");
    let stack = open_reference_stack(&dir);
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    let tile_size = focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
    let shipped =
        stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
    let got = stack_sharpness_scale_with_backend(
        &stack,
        frames,
        extent,
        spp,
        tile_size,
        &NoControl,
        Some(backend),
    )
    .unwrap();
    let recheck = got
        .recheck
        .expect("the device path ran and did not fall back");
    println!(
        "scale {:#010x}, shipped {:#010x}, iterations {}, candidates {:?}",
        got.value.to_bits(),
        shipped.to_bits(),
        recheck.iterations,
        recheck.candidates
    );
    assert_eq!(got.value.to_bits(), shipped.to_bits());

    let plan = sharpness_plan(extent, tile_size).unwrap();
    let session = SharpnessBackend::open_scale(backend, &plan, spp)
        .unwrap()
        .expect("the reference stack's peaks fit the device");
    fold_all(&*session, &stack, frames, plan.tiles().len());
    let (outside, worst) = peaks_against_exact(&*session, &stack, frames, spp, &plan);
    println!("{outside} peaks outside ε, largest |g - c| / ε(c) = {worst:.4}");
    assert_eq!(outside, 0);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn reference_stack_sharpness_scale_speedup() {
    let Some(dir) = core_common::reference_stack_dir() else {
        return;
    };
    let Some(_) = shared_device_or_skip() else {
        return;
    };
    let backend = sharpness_backend_for(false, None).expect("the GPU is allowed");
    let stack = open_reference_stack(&dir);
    let (frames, extent, spp) = (
        stack.len(),
        stack.dimensions(),
        stack.header(0).samples_per_pixel,
    );
    let tile_size = focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
    let on_device = || {
        stack_sharpness_scale_with_backend(
            &stack,
            frames,
            extent,
            spp,
            tile_size,
            &NoControl,
            Some(backend),
        )
        .unwrap()
    };
    let on_cpu =
        || stack_sharpness_scale(&stack, frames, extent, spp, tile_size, &NoControl).unwrap();
    // One untimed pass of each, then the timed one.
    on_device();
    on_cpu();
    let started = std::time::Instant::now();
    let device = on_device();
    let gpu_time = started.elapsed();
    let started = std::time::Instant::now();
    let cpu = on_cpu();
    let cpu_time = started.elapsed();

    let recheck = device
        .recheck
        .expect("the device path ran and did not fall back");
    assert_eq!(device.value.to_bits(), cpu.to_bits());
    let ratio = cpu_time.as_secs_f64() / gpu_time.as_secs_f64();
    println!(
        "scale: gpu {gpu_time:.2?}, cpu {cpu_time:.2?}, {ratio:.2}x, iterations {}, candidates {:?}",
        recheck.iterations, recheck.candidates
    );
    assert!(
        ratio >= 2.0,
        "scale stage: gpu {gpu_time:.2?} against cpu {cpu_time:.2?} is {ratio:.2}x, below 2.0x"
    );
}
