// SPDX-License-Identifier: GPL-3.0-or-later
//! Times the three sharpness stages on the CPU as shipped against a GPU
//! prototype, end to end, and checks the GPU result against the CPU twin.
//!
//! `sharpness-gpu <dir> [--crop x,y,w,h] [--frames N] [--iters N] [--stages a,b,c]`

mod gpu;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::fuse::laplacian::score_plane;
use focusforge_core::io::{FrameReader, open_frame_set};
use focusforge_core::job::JobControl;
use focusforge_core::params::ParamValues;
use focusforge_core::sharp_areas::{
    PairOverlap, SHARP_AREA_PARAMS, SharpAreas, count_sharp_areas_reporting, sharpness_plan,
};
use focusforge_core::sharpness_map::{sharpness_map_tile, stack_sharpness_scale};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, PlannedTile, Rect, TilePlan};
use focusforge_gpu::budget::{ProbeOutcome, VramBudget, probe_budget};
use focusforge_gpu::device::{adapter_report, shared_device};
use focusforge_gpu::vram::WgpuAllocProbe;
use gpu::{BIN_SHIFT, BINS, Gpu, SCORE_RADIUS, Times, Variant};
use rayon::prelude::*;

/// A crop of the opened frames: tile rects are offset into the frame.
struct Crop<'a> {
    inner: &'a dyn FrameReader,
    x: u32,
    y: u32,
}

impl TileReader for Crop<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let shifted = Rect {
            x: rect.x + self.x,
            y: rect.y + self.y,
            ..rect
        };
        self.inner.read_tile_into(frame, shifted, out)
    }
}

static CPU: LazyLock<Times> = LazyLock::new(Times::default);

struct Args {
    dir: PathBuf,
    crop: Option<[u32; 4]>,
    frames: Option<usize>,
    iters: usize,
    stages: String,
    probe: bool,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        dir: PathBuf::new(),
        crop: None,
        frames: None,
        iters: 4,
        stages: "abc".into(),
        probe: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--crop" => {
                let v: Vec<u32> = value("--crop")?
                    .split(',')
                    .map(|n| n.parse().map_err(|_| "--crop is x,y,w,h".to_string()))
                    .collect::<Result<_, _>>()?;
                args.crop = Some(v.try_into().map_err(|_| "--crop is x,y,w,h".to_string())?);
            }
            "--frames" => {
                args.frames = Some(
                    value("--frames")?
                        .parse()
                        .map_err(|_| "--frames is a number")?,
                )
            }
            "--iters" => {
                args.iters = value("--iters")?
                    .parse()
                    .map_err(|_| "--iters is a number")?
            }
            "--stages" => args.stages = value("--stages")?,
            "--probe" => args.probe = true,
            _ => args.dir = PathBuf::from(arg),
        }
    }
    if args.dir.as_os_str().is_empty() {
        return Err(
            "usage: sharpness-gpu <dir> [--crop x,y,w,h] [--frames N] [--iters N] [--stages abc]"
                .into(),
        );
    }
    Ok(args)
}

fn frame_paths(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = if dir.join("frames").is_dir() {
        dir.join("frames")
    } else {
        dir.to_path_buf()
    };
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("tif") || x.eq_ignore_ascii_case("tiff"))
        })
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no tif frames in {}", dir.display()));
    }
    Ok(paths)
}

fn hwm_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.split_whitespace().next().and_then(|n| n.parse().ok()))
        })
        .unwrap_or(0)
}

fn rss_reset() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// Runs `f` `iters` times (iteration 0 is the cold one) and prints one line each.
fn bench(
    name: &str,
    iters: usize,
    times: Option<&Times>,
    mut f: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    rss_reset();
    for iter in 0..iters {
        if let Some(times) = times {
            times.reset();
        }
        let start = Instant::now();
        f()?;
        let wall = start.elapsed().as_secs_f64() * 1e3;
        println!(
            "RESULT stage={name} iter={iter} wall_ms={wall:.1} {}",
            times.map(Times::line).unwrap_or_default()
        );
    }
    println!("RSS stage={name} hwm_kb={}", hwm_kb());
    Ok(())
}

fn scale_from_hist(hist: &[u64], pixels: u64) -> f32 {
    let rank = ((0.99 * pixels as f64).ceil() as u64).max(1);
    let mut seen = 0;
    let bin = hist
        .iter()
        .position(|c| {
            seen += c;
            seen >= rank
        })
        .expect("the histogram holds every pixel");
    f32::from_bits((bin as u32) << BIN_SHIFT)
}

/// The CPU twin's scores of one frame tile, rebuilt from the public pieces.
fn cpu_scores(
    reader: &dyn TileReader,
    frame: usize,
    read: Rect,
    spp: u32,
    samples: &mut Vec<u16>,
    split: bool,
) -> Result<Vec<f32>, String> {
    let t = Instant::now();
    samples.resize(read.width as usize * read.height as usize * spp as usize, 0);
    reader
        .read_tile_into(frame, read, samples)
        .map_err(|e| e.to_string())?;
    Times::add(&CPU.read, t);
    if split {
        return Ok(Vec::new());
    }
    let t = Instant::now();
    let pyramid = focusforge_core::fuse::build(&samples[..], read.width, read.height, spp, 1)
        .map_err(|e| e.to_string())?;
    let scores = score_plane(&pyramid.levels()[0], SCORE_RADIUS);
    Times::add(&CPU.compute, t);
    Ok(scores)
}

/// Interior peaks of one tile over all frames, CPU, frames in parallel.
fn cpu_tile_peaks(
    pool: &rayon::ThreadPool,
    reader: &dyn TileReader,
    tile: &PlannedTile,
    frames: usize,
    spp: u32,
    read_only: bool,
) -> Result<Vec<f32>, String> {
    let (read, interior) = (tile.read(), tile.interior());
    let peaks = Mutex::new(vec![
        0.0f32;
        interior.width as usize * interior.height as usize
    ]);
    pool.install(|| {
        (0..frames)
            .into_par_iter()
            .try_for_each_init(Vec::new, |samples, frame| {
                let scores = cpu_scores(reader, frame, read, spp, samples, read_only)?;
                if read_only {
                    return Ok(());
                }
                let mut peaks = peaks.lock().unwrap();
                for (row, peak_row) in peaks.chunks_exact_mut(interior.width as usize).enumerate() {
                    let start = ((interior.y - read.y) as usize + row) * read.width as usize
                        + (interior.x - read.x) as usize;
                    for (peak, score) in peak_row.iter_mut().zip(&scores[start..]) {
                        *peak = peak.max(*score);
                    }
                }
                Ok::<(), String>(())
            })
    })?;
    Ok(peaks.into_inner().unwrap())
}

struct Cells {
    cell: u32,
    contrast: f64,
    threshold: f64,
    columns: usize,
    cells: usize,
    area: Vec<f64>,
}

fn cells_of(extent: (u32, u32)) -> Cells {
    let default = |id: &str| {
        SHARP_AREA_PARAMS
            .iter()
            .find(|p| p.id == id)
            .expect("param exists")
            .default
            .clone()
    };
    let cell = default("cell-size").as_i64().expect("int") as u32;
    let (width, height) = extent;
    let columns = width.div_ceil(cell) as usize;
    let cells = columns * height.div_ceil(cell) as usize;
    let area = (0..cells)
        .map(|i| {
            let (c, r) = ((i % columns) as u32, (i / columns) as u32);
            f64::from(cell.min(width - c * cell)) * f64::from(cell.min(height - r * cell))
        })
        .collect();
    Cells {
        cell,
        contrast: default("contrast").as_f64().expect("float"),
        threshold: default("overlap-threshold").as_f64().expect("float"),
        columns,
        cells,
        area,
    }
}

/// The CPU count's per-cell decision and pair tally over finished sums.
fn decide(sums: &[Vec<f64>], c: &Cells) -> SharpAreas {
    let frames = sums.len();
    let mut counts = vec![0u32; frames];
    let mut overlaps = vec![PairOverlap { both: 0, either: 0 }; frames.saturating_sub(1)];
    for index in 0..c.cells {
        let (mut peak, mut peak_frame, mut floor) = (f64::NEG_INFINITY, 0, f64::INFINITY);
        for (frame, s) in sums.iter().enumerate() {
            let mean = s[index] / c.area[index];
            if mean > peak {
                peak = mean;
                peak_frame = frame;
            }
            floor = floor.min(mean);
        }
        if peak > c.contrast * floor {
            counts[peak_frame] += 1;
            let sharp = |f: usize| sums[f][index] / c.area[index] >= c.threshold * peak;
            for (pair, overlap) in overlaps.iter_mut().enumerate() {
                let (a, b) = (sharp(pair), sharp(pair + 1));
                overlap.either += u32::from(a || b);
                overlap.both += u32::from(a && b);
            }
        }
    }
    SharpAreas {
        cells: c.cells as u32,
        counts,
        overlaps,
    }
}

fn run() -> Result<(), String> {
    let args = parse()?;
    let mut paths = frame_paths(&args.dir)?;
    if let Some(n) = args.frames {
        paths.truncate(n);
    }
    let set = open_frame_set(
        &paths,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .map_err(|e| e.to_string())?;
    let (fw, fh) = set.reader.dimensions();
    let spp = set.reader.frame(0).samples_per_pixel;
    let [cx, cy, w, h] = args.crop.unwrap_or([0, 0, fw, fh]);
    if cx + w > fw || cy + h > fh {
        return Err(format!(
            "crop {cx},{cy},{w},{h} is outside the {fw}x{fh} frames"
        ));
    }
    let crop = Crop {
        inner: set.reader.as_ref(),
        x: cx,
        y: cy,
    };
    let reader: &dyn TileReader = &crop;
    let frames = paths.len();
    let extent = (w, h);
    let plan: TilePlan = sharpness_plan(extent, DEFAULT_TILE_SIZE).map_err(|e| e.to_string())?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| e.to_string())?;
    let pixels = u64::from(w) * u64::from(h);
    let (max_w, max_h) = plan.tiles().iter().fold((0, 0), |(mw, mh), t| {
        (mw.max(t.read().width), mh.max(t.read().height))
    });

    println!(
        "INPUT dir={} frames={frames} frame_extent={fw}x{fh} crop={cx},{cy},{w},{h} spp={spp} tile={DEFAULT_TILE_SIZE} tiles={} max_read_rect={max_w}x{max_h}",
        args.dir.display(),
        plan.tiles().len()
    );
    let (rw, rh) = (u64::from(max_w), u64::from(max_h));
    let read_bytes = rw * rh * u64::from(spp) * 2;
    let worker_bytes = read_bytes + rw * rh * u64::from(spp) * 4 + rw * rh * 4;
    println!(
        "BOUND cpu threads={threads} per_worker_bytes={worker_bytes} (u16 read {read_bytes} + detail f32 {} + score f32 {}) in_flight_cap={threads} total_bytes={} scale_histogram_bytes={} ({BINS} bins x 8 B)",
        rw * rh * u64::from(spp) * 4,
        rw * rh * 4,
        worker_bytes * threads as u64,
        BINS * 8
    );

    println!("RSS stage=before-gpu-init hwm_kb={}", hwm_kb());
    let t = Instant::now();
    let device = shared_device();
    let gpu: Option<&'static Gpu> = match device {
        Err(err) => {
            println!("{err}");
            None
        }
        Ok(d) => {
            println!(
                "RESULT stage=gpu-init wall_ms={:.1}",
                t.elapsed().as_secs_f64() * 1e3
            );
            println!("{}", adapter_report(d.info()));
            let probed = args
                .probe
                .then(|| probe_budget(|bytes| WgpuAllocProbe::new(d.device()).try_reserve(bytes)));
            match probed {
                None => println!(
                    "VRAM-BUDGET not probed (--probe): the probe reserves up to 8 GiB and would show in the VRAM samples"
                ),
                Some(ProbeOutcome::Settled { bytes, attempts }) => {
                    let budget = VramBudget::new(bytes);
                    let per_worker = Gpu::bytes_per_worker(max_w, max_h, spp);
                    println!(
                        "VRAM-BUDGET device_bytes={bytes} attempts={attempts} limit_tiles_for_u16_read_rect={} limit_tiles_for_f32_read_rect={} gpu_per_worker_bytes={per_worker} workers={threads} gpu_total_bytes={} within_budget={}",
                        budget.limit_for(read_bytes),
                        budget.limit_for(rw * rh * u64::from(spp) * 4),
                        per_worker * threads as u64,
                        per_worker * threads as u64 <= bytes
                    );
                }
                Some(ProbeOutcome::Exhausted { floor, detail }) => {
                    println!("VRAM-BUDGET exhausted floor={floor} detail={detail}")
                }
            }
            println!("RSS stage=after-gpu-init hwm_kb={}", hwm_kb());
            Some(Gpu::new(d))
        }
    };

    let control = JobControl::new();
    let mut scale = 0.0f32;
    let want = |s: char| args.stages.contains(s);

    if want('a') {
        bench("a-cpu", args.iters, None, || {
            scale =
                stack_sharpness_scale(reader, frames, extent, spp, DEFAULT_TILE_SIZE, &*control)
                    .map_err(|e| e.to_string())?;
            Ok(())
        })?;
        println!("SCALE shipped bits={:#010x} value={scale}", scale.to_bits());
        bench("a-cpu-read", args.iters, Some(&CPU), || {
            for tile in plan.tiles() {
                cpu_tile_peaks(&pool, reader, tile, frames, spp, true)?;
            }
            Ok(())
        })?;
        bench("a-cpu-rebuilt", args.iters, Some(&CPU), || {
            for tile in plan.tiles() {
                cpu_tile_peaks(&pool, reader, tile, frames, spp, false)?;
            }
            Ok(())
        })?;
    } else {
        scale = stack_sharpness_scale(reader, frames, extent, spp, DEFAULT_TILE_SIZE, &*control)
            .map_err(|e| e.to_string())?;
    }
    if want('b') {
        bench("b-cpu", args.iters, None, || {
            for frame in 0..frames {
                for tile in plan.tiles() {
                    sharpness_map_tile(reader, frame, tile, spp, scale)
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        })?;
        bench("b-cpu-parallel", args.iters, None, || {
            for tile in plan.tiles() {
                pool.install(|| {
                    (0..frames).into_par_iter().try_for_each(|frame| {
                        sharpness_map_tile(reader, frame, tile, spp, scale)
                            .map(drop)
                            .map_err(|e| e.to_string())
                    })
                })?;
            }
            Ok(())
        })?;
    }
    let mut cpu_counts: Option<SharpAreas> = None;
    if want('c') {
        bench("c-cpu", args.iters, None, || {
            cpu_counts = Some(
                count_sharp_areas_reporting(
                    reader,
                    frames,
                    extent,
                    spp,
                    &ParamValues::new(),
                    DEFAULT_TILE_SIZE,
                    &*control,
                    &mut |_| {},
                )
                .map_err(|e| e.to_string())?,
            );
            Ok(())
        })?;
    }

    let Some(gpu) = gpu else {
        println!("GPU stages skipped: no usable GPU adapter; CPU timings above stand");
        return Ok(());
    };
    let cells = cells_of(extent);
    let tile0 = &plan.tiles()[0];
    for fixed in [false, true] {
        gpu.fixed.store(fixed, Ordering::Relaxed);
        let sfx = if fixed { "-fixed" } else { "" };
        if want('a') {
            for (name, variant) in [
                ("a-gpu-hostbin", Variant::HostBin),
                ("a-gpu-devicebin", Variant::DeviceBin),
            ] {
                bench(
                    &format!("{name}{sfx}"),
                    args.iters,
                    Some(&gpu.times),
                    || {
                        gpu::stage_a(gpu, reader, &plan, frames, spp, &pool, variant, None)?;
                        Ok(())
                    },
                )?;
            }
        }
        if want('b') {
            bench(&format!("b-gpu{sfx}"), args.iters, Some(&gpu.times), || {
                gpu::stage_b(gpu, reader, &plan, frames, spp, scale, &pool, &|_, _, _| {})
            })?;
        }
        if want('c') {
            bench(&format!("c-gpu{sfx}"), args.iters, Some(&gpu.times), || {
                let sums = gpu::stage_c(
                    gpu,
                    reader,
                    &plan,
                    frames,
                    spp,
                    cells.cell,
                    cells.columns,
                    cells.cells,
                    &pool,
                )?;
                let t = Instant::now();
                decide(&sums, &cells);
                Times::add(&gpu.times.host, t);
                Ok(())
            })?;
        }
    }

    // Parity, untimed.
    for fixed in [false, true] {
        gpu.fixed.store(fixed, Ordering::Relaxed);
        let label = if fixed { "fixed-point" } else { "f32" };
        {
            // Detail and score of tile 0, frame by frame, against the CPU twin.
            let read = tile0.read();
            let (mut detail_equal, mut detail_total, mut detail_worst) = (0u64, 0u64, 0.0f32);
            let (mut score_equal, mut score_total, mut score_worst) = (0u64, 0u64, 0.0f64);
            for frame in 0..frames {
                let mut samples =
                    vec![0u16; read.width as usize * read.height as usize * spp as usize];
                reader
                    .read_tile_into(frame, read, &mut samples)
                    .map_err(|e| e.to_string())?;
                let pyramid =
                    focusforge_core::fuse::build(&samples[..], read.width, read.height, spp, 1)
                        .map_err(|e| e.to_string())?;
                let cpu_detail = pyramid.levels()[0].data();
                let cpu_score = score_plane(&pyramid.levels()[0], SCORE_RADIUS);
                let gpu_detail = gpu.probe(reader, frame, read, spp, &gpu::Tail::Detail)?;
                let gpu_score = gpu.probe(reader, frame, read, spp, &gpu::Tail::Scores)?;
                for (g, c) in gpu_detail.iter().zip(cpu_detail) {
                    detail_total += 1;
                    detail_equal += u64::from(g.to_bits() == c.to_bits());
                    detail_worst = detail_worst.max((g - c).abs());
                }
                for (g, c) in gpu_score.iter().zip(&cpu_score) {
                    score_total += 1;
                    score_equal += u64::from(g.to_bits() == c.to_bits());
                    score_worst = score_worst
                        .max(f64::from((g - c).abs()) / f64::from(c.abs().max(f32::MIN_POSITIVE)));
                }
            }
            println!(
                "PARITY scores kernel={label} tile0_frames={frames} detail_bit_equal={detail_equal}/{detail_total} detail_worst_abs={detail_worst:.3e} score_bit_equal={score_equal}/{score_total} score_worst_rel={score_worst:.3e}"
            );
        }
        if want('a') {
            let mut cpu_hist = vec![0u64; BINS];
            let (mut flips, mut worst_rel) = (0u64, 0.0f64);
            let mut failure = None;
            let mut collect = |tile: &PlannedTile, gpu_peaks: &[f32]| match cpu_tile_peaks(
                &pool, reader, tile, frames, spp, false,
            ) {
                Ok(cpu_peaks) => {
                    for (g, c) in gpu_peaks.iter().zip(&cpu_peaks) {
                        cpu_hist[(c.to_bits() >> BIN_SHIFT) as usize] += 1;
                        flips += u64::from(g.to_bits() >> BIN_SHIFT != c.to_bits() >> BIN_SHIFT);
                        worst_rel = worst_rel.max(
                            f64::from((g - c).abs()) / f64::from(c.abs().max(f32::MIN_POSITIVE)),
                        );
                    }
                }
                Err(e) => failure = Some(e),
            };
            let host = gpu::stage_a(
                gpu,
                reader,
                &plan,
                frames,
                spp,
                &pool,
                Variant::HostBin,
                Some(&mut collect),
            )?;
            if let Some(e) = failure {
                return Err(e);
            }
            let device_hist = gpu::stage_a(
                gpu,
                reader,
                &plan,
                frames,
                spp,
                &pool,
                Variant::DeviceBin,
                None,
            )?;
            let differ = |a: &[u64], b: &[u64]| a.iter().zip(b).filter(|(x, y)| x != y).count();
            let (cpu_scale, host_scale, device_scale) = (
                scale_from_hist(&cpu_hist, pixels),
                scale_from_hist(&host, pixels),
                scale_from_hist(&device_hist, pixels),
            );
            println!(
                "PARITY a kernel={label} pixels={pixels} scale_shipped={:#010x} scale_cpu_rebuilt={:#010x} scale_gpu_hostbin={:#010x} scale_gpu_devicebin={:#010x} bins_differ_hostbin={} bins_differ_devicebin={} peaks_bin_flips={flips} worst_peak_rel_diff={worst_rel:.3e}",
                scale.to_bits(),
                cpu_scale.to_bits(),
                host_scale.to_bits(),
                device_scale.to_bits(),
                differ(&cpu_hist, &host),
                differ(&cpu_hist, &device_hist)
            );
        }
        if want('b') {
            let worst = Mutex::new((0.0f64, 0u64, 0u64));
            gpu::stage_b(
                gpu,
                reader,
                &plan,
                frames,
                spp,
                scale,
                &pool,
                &|frame, tile, map| {
                    let cpu =
                        sharpness_map_tile(reader, frame, tile, spp, scale).expect("cpu map tile");
                    let mut local = (0.0f64, 0u64, 0u64);
                    for (g, c) in map.iter().zip(&cpu) {
                        let lsb = f64::from((g - c).abs()) / (f64::from(c.max(1.0)) / 65535.0);
                        local.0 = local.0.max(lsb);
                        local.1 += u64::from(lsb > 1.0);
                        local.2 += 1;
                    }
                    let mut worst = worst.lock().unwrap();
                    worst.0 = worst.0.max(local.0);
                    worst.1 += local.1;
                    worst.2 += local.2;
                },
            )?;
            let (lsb, over, values) = *worst.lock().unwrap();
            println!(
                "PARITY b kernel={label} values={values} worst_diff_lsb16={lsb:.4} values_over_1_lsb={over}"
            );
        }
        if want('c') {
            let sums = gpu::stage_c(
                gpu,
                reader,
                &plan,
                frames,
                spp,
                cells.cell,
                cells.columns,
                cells.cells,
                &pool,
            )?;
            let got = decide(&sums, &cells);
            let want = cpu_counts.as_ref().expect("c-cpu ran");
            let count_diff: i64 = got
                .counts
                .iter()
                .zip(&want.counts)
                .map(|(a, b)| (i64::from(*a) - i64::from(*b)).abs())
                .sum();
            let overlap_diff: i64 = got
                .overlaps
                .iter()
                .zip(&want.overlaps)
                .map(|(a, b)| {
                    (i64::from(a.both) - i64::from(b.both)).abs()
                        + (i64::from(a.either) - i64::from(b.either)).abs()
                })
                .sum();
            println!(
                "PARITY c kernel={label} cells={} counts_equal={} overlaps_equal={} sum_abs_count_diff={count_diff} sum_abs_overlap_diff={overlap_diff} cpu_counts_total={} gpu_counts_total={}",
                cells.cells,
                got.counts == want.counts,
                got.overlaps == want.overlaps,
                want.counts.iter().sum::<u32>(),
                got.counts.iter().sum::<u32>()
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
