// SPDX-License-Identifier: GPL-3.0-or-later
//! Times the three sharpness stages on the CPU as shipped against a GPU
//! prototype, end to end, and checks the GPU result against the CPU twin.
//!
//! `sharpness-gpu <dir> [--crop x,y,w,h] [--frames N] [--iters N] [--stages a,b,c,e] [--eps-scale K]`
//!
//! Stage `e` is stage (a) with an exact CPU re-check at the deciding bin edges.

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
    eps_scale: f64,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        dir: PathBuf::new(),
        crop: None,
        frames: None,
        iters: 4,
        stages: "abc".into(),
        probe: false,
        eps_scale: 1.0,
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
            "--eps-scale" => {
                args.eps_scale = value("--eps-scale")?
                    .parse()
                    .map_err(|_| "--eps-scale is a number")?
            }
            _ => args.dir = PathBuf::from(arg),
        }
    }
    if args.dir.as_os_str().is_empty() {
        return Err(
            "usage: sharpness-gpu <dir> [--crop x,y,w,h] [--frames N] [--iters N] [--stages abce] [--eps-scale K]"
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

// Rounding budget of the detail plane `base - expand(reduce(base))` for 16-bit input
// (reduce is exact; every other operand is below 2^16, where an f32 ulp is 2^-8).
const ULP_BELOW_2_16: f64 = 1.0 / 256.0;
// One 1D expand: at most 5 roundings on the GPU (any contraction or reassociation, 1 ulp each),
// 3 on the CPU (0.75 * mid and two adds, half an ulp each); two passes, then the subtraction.
const EXPAND_ROUNDINGS_GPU: f64 = 5.0;
const EXPAND_ROUNDINGS_CPU: f64 = 3.0;
const DETAIL_GPU: f64 = (2.0 * EXPAND_ROUNDINGS_GPU + 1.0) * ULP_BELOW_2_16;
const DETAIL_CPU: f64 = (2.0 * EXPAND_ROUNDINGS_CPU + 1.0) * ULP_BELOW_2_16 / 2.0;
// The CPU rounds each |L| to a multiple of 2^-20 before summing.
const CPU_QUANTIZATION: f64 = 1.0 / 2_097_152.0;
// One f32 rounding is at most 2^-23 of its result (1 ulp, GPU adds); the CPU's single final
// conversion is half of it.
const ULP_RELATIVE: f64 = 1.0 / 8_388_608.0;
const HALF_ULP_RELATIVE: f64 = ULP_RELATIVE / 2.0;

/// `|GPU peak - CPU peak| <= a + b * (CPU peak)` for 16-bit input, scaled by `--eps-scale`.
struct Bound {
    a: f64,
    b: f64,
}

impl Bound {
    fn new(spp: u32, scale: f64) -> Self {
        let side = f64::from(2 * SCORE_RADIUS + 1);
        let samples = side * side * f64::from(spp);
        // Row sum: side * spp terms; column sum: side terms; the first add of each is exact.
        let adds = side * f64::from(spp) - 1.0 + (side - 1.0);
        let gamma = adds * ULP_RELATIVE / (1.0 - adds * ULP_RELATIVE);
        Self {
            a: scale * samples * (DETAIL_GPU + DETAIL_CPU + CPU_QUANTIZATION) * (1.0 + gamma),
            b: scale * (gamma * (1.0 + HALF_ULP_RELATIVE) + HALF_ULP_RELATIVE),
        }
    }

    fn at(&self, v: f64) -> f64 {
        self.a + self.b * v
    }
}

/// `c` pushed by `delta` (a bound) as far as an f32 can go without leaving it.
fn pushed(c: f32, delta: f64) -> f32 {
    let target = f64::from(c) + delta;
    if target <= 0.0 {
        return 0.0;
    }
    let g = target as f32;
    if (f64::from(g) - f64::from(c)).abs() <= delta.abs() {
        g
    } else if delta > 0.0 {
        f32::from_bits(g.to_bits() - 1)
    } else {
        f32::from_bits(g.to_bits() + 1)
    }
}

/// Read reach of a peak: detail reach 4 plus score radius 4.
const PEAK_REACH: u32 = 8;

/// Exact peaks of single pixels, recomputed on the CPU along the shipped path.
struct Exact<'a> {
    reader: &'a dyn TileReader,
    frames: usize,
    spp: u32,
    extent: (u32, u32),
    pool: &'a rayon::ThreadPool,
}

impl Exact<'_> {
    fn peaks(&self, at: &[(u32, u32)]) -> Result<Vec<f32>, String> {
        let (w, h) = self.extent;
        self.pool.install(|| {
            at.par_iter()
                .map_init(Vec::new, |samples, &(x, y)| {
                    // Even origin keeps the 2:1 decimation grid of the tiled path.
                    let x0 = x.saturating_sub(PEAK_REACH) & !1;
                    let y0 = y.saturating_sub(PEAK_REACH) & !1;
                    let read = Rect {
                        x: x0,
                        y: y0,
                        width: (x + PEAK_REACH + 1).min(w) - x0,
                        height: (y + PEAK_REACH + 1).min(h) - y0,
                    };
                    let index = ((y - y0) * read.width + (x - x0)) as usize;
                    let mut peak = 0.0f32;
                    for frame in 0..self.frames {
                        let scores =
                            cpu_scores(self.reader, frame, read, self.spp, samples, false)?;
                        peak = peak.max(scores[index]);
                    }
                    Ok(peak)
                })
                .collect()
        })
    }
}

struct Recheck {
    scale: f32,
    iterations: usize,
    /// Candidates recomputed on each pass.
    candidates: Vec<u64>,
    /// Selection plus exact recompute, nanoseconds.
    ns: u64,
}

const MAX_RECHECK_PASSES: usize = 64;

/// The shipped scale decided from `hist`'s deciding bin, with every peak that `bound` leaves
/// on either side of the bin's edges recomputed exactly. `peaks_of(tile)` gives a tile's
/// interior peaks, one tile in memory at a time.
fn recheck_scale(
    hist: &[u64],
    plan: &TilePlan,
    pixels: u64,
    bound: &Bound,
    exact: &Exact,
    peaks_of: &mut dyn FnMut(usize) -> Result<Vec<f32>, String>,
) -> Result<Recheck, String> {
    let rank = ((0.99 * pixels as f64).ceil() as u64).max(1);
    let mut bin = scale_from_hist(hist, pixels).to_bits() >> BIN_SHIFT;
    let mut out = Recheck {
        scale: 0.0,
        iterations: 0,
        candidates: Vec::new(),
        ns: 0,
    };
    for pass in 1..=MAX_RECHECK_PASSES {
        let edge = |b: u32| f64::from(f32::from_bits(b << BIN_SHIFT));
        let (lo, hi) = (edge(bin), edge(bin + 1));
        let (lo_eps, hi_eps) = (bound.at(lo), bound.at(hi));
        let (mut below_lo, mut below_hi) = (0u64, 0u64);
        let mut unsure: Vec<(u32, u32)> = Vec::new();
        for (index, tile) in plan.tiles().iter().enumerate() {
            let peaks = peaks_of(index)?;
            let t = Instant::now();
            let interior = tile.interior();
            for (k, &g) in peaks.iter().enumerate() {
                let g = f64::from(g);
                // Some(true): certainly below the edge; Some(false): certainly not.
                let side = |e: f64, eps: f64| {
                    (g < e - eps)
                        .then_some(true)
                        .or((g >= e + eps).then_some(false))
                };
                match (side(lo, lo_eps), side(hi, hi_eps)) {
                    (Some(a), Some(b)) => {
                        below_lo += u64::from(a);
                        below_hi += u64::from(b);
                    }
                    _ => unsure.push((
                        interior.x + k as u32 % interior.width,
                        interior.y + k as u32 / interior.width,
                    )),
                }
            }
            out.ns += t.elapsed().as_nanos() as u64;
        }
        let t = Instant::now();
        for c in exact.peaks(&unsure)? {
            below_lo += u64::from(f64::from(c) < lo);
            below_hi += u64::from(f64::from(c) < hi);
        }
        out.ns += t.elapsed().as_nanos() as u64;
        out.iterations = pass;
        out.candidates.push(unsure.len() as u64);
        if below_lo >= rank {
            bin = bin
                .checked_sub(1)
                .ok_or("the deciding bin moved below bin 0")?;
        } else if below_hi < rank {
            bin += 1;
        } else {
            out.scale = f32::from_bits(bin << BIN_SHIFT);
            return Ok(out);
        }
    }
    Err(format!(
        "the deciding bin did not settle in {MAX_RECHECK_PASSES} passes"
    ))
}

fn recheck_line(label: &str, r: &Recheck, shipped: f32, pixels: u64) -> String {
    format!(
        "PARITY e {label} scale={:#010x} shipped={:#010x} equal={} iterations={} candidates_per_pass={:?} candidates_total={} share_pct_first_pass={:.4}",
        r.scale.to_bits(),
        shipped.to_bits(),
        r.scale.to_bits() == shipped.to_bits(),
        r.iterations,
        r.candidates,
        r.candidates.iter().sum::<u64>(),
        100.0 * r.candidates[0] as f64 / pixels as f64
    )
}

/// Untimed checks of the edge re-check: the bound against exact CPU peaks, the small-window
/// recompute, the `eps=0` run and the two extremes of the bound.
fn parity_e(
    gpu: &'static Gpu,
    exact: &Exact,
    plan: &TilePlan,
    pixels: u64,
    shipped: f32,
    eps_scale: f64,
) -> Result<(), String> {
    let (reader, frames, spp, pool) = (exact.reader, exact.frames, exact.spp, exact.pool);
    let mut kept = Vec::new();
    let hist = gpu::stage_a(
        gpu,
        reader,
        plan,
        frames,
        spp,
        pool,
        Variant::DeviceBin,
        None,
        Some(&mut kept),
    )?;
    let g_of = |i: usize| -> Result<Vec<f32>, String> {
        let t = &plan.tiles()[i];
        Ok(gpu::interior_peaks(
            &gpu.download_peaks(&kept[i], t.read()),
            t.read(),
            t.interior(),
        ))
    };
    let c_of = |i: usize| cpu_tile_peaks(pool, reader, &plan.tiles()[i], frames, spp, false);
    let bound = Bound::new(spp, 1.0);

    // The bound against the exact peaks, the bin flips, and the small-window recompute.
    const WINDOW_STRIDE: usize = 1021;
    let (mut flips, mut exceed, mut worst_ratio, mut worst_abs) = (0u64, 0u64, 0.0f64, 0.0f64);
    let (mut sampled, mut mismatches) = (0u64, 0u64);
    for (i, tile) in plan.tiles().iter().enumerate() {
        let (g, c) = (g_of(i)?, c_of(i)?);
        for (g, c) in g.iter().zip(&c) {
            flips += u64::from(g.to_bits() >> BIN_SHIFT != c.to_bits() >> BIN_SHIFT);
            let diff = f64::from((g - c).abs());
            let eps = bound.at(f64::from(*c));
            exceed += u64::from(diff > eps);
            worst_ratio = worst_ratio.max(diff / eps);
            worst_abs = worst_abs.max(diff);
        }
        let interior = tile.interior();
        let at: Vec<(u32, u32)> = (0..c.len())
            .step_by(WINDOW_STRIDE)
            .map(|k| {
                (
                    interior.x + k as u32 % interior.width,
                    interior.y + k as u32 / interior.width,
                )
            })
            .collect();
        let got = exact.peaks(&at)?;
        sampled += at.len() as u64;
        mismatches += got
            .iter()
            .enumerate()
            .filter(|&(j, v)| v.to_bits() != c[j * WINDOW_STRIDE].to_bits())
            .count() as u64;
    }
    println!(
        "PARITY e bound kernel=f32 pixels={pixels} a={:.4} b={:.4e} peaks_exceeding_eps={exceed} largest_diff={worst_abs:.4} largest_diff_over_eps={worst_ratio:.4} window_check sampled={sampled} mismatches={mismatches}",
        bound.a, bound.b
    );

    let timed = Bound::new(spp, eps_scale);
    let gpu_run = recheck_scale(&hist, plan, pixels, &timed, exact, &mut |i| g_of(i))?;
    println!(
        "{}",
        recheck_line(
            &format!("gpu-peaks eps_scale={eps_scale}"),
            &gpu_run,
            shipped,
            pixels
        )
    );
    let none = Bound::new(spp, 0.0);
    let zero = recheck_scale(&hist, plan, pixels, &none, exact, &mut |i| g_of(i))?;
    println!(
        "{} peaks_bin_flips={flips}",
        recheck_line("eps=0 gpu-peaks", &zero, shipped, pixels)
    );

    for (label, sign) in [("perturb=+eps", 1.0), ("perturb=-eps", -1.0)] {
        let push = |c: Vec<f32>| -> Vec<f32> {
            c.into_iter()
                .map(|v| pushed(v, sign * bound.at(f64::from(v))))
                .collect()
        };
        let mut pushed_hist = vec![0u64; BINS];
        for i in 0..plan.tiles().len() {
            for g in push(c_of(i)?) {
                pushed_hist[(g.to_bits() >> BIN_SHIFT) as usize] += 1;
            }
        }
        let run = recheck_scale(&pushed_hist, plan, pixels, &bound, exact, &mut |i| {
            Ok(push(c_of(i)?))
        })?;
        println!("{}", recheck_line(label, &run, shipped, pixels));
        // Control: the same pushed peaks without the re-check.
        let off = recheck_scale(&pushed_hist, plan, pixels, &none, exact, &mut |i| {
            Ok(push(c_of(i)?))
        })?;
        println!(
            "{}",
            recheck_line(&format!("{label} recheck=off"), &off, shipped, pixels)
        );
    }
    Ok(())
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

    if want('a') || want('e') {
        bench("a-cpu", args.iters, None, || {
            scale =
                stack_sharpness_scale(reader, frames, extent, spp, DEFAULT_TILE_SIZE, &*control)
                    .map_err(|e| e.to_string())?;
            Ok(())
        })?;
        println!("SCALE shipped bits={:#010x} value={scale}", scale.to_bits());
    }
    if want('a') {
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
    // The fixed-point kernel belongs to stages a-c; stage e measures the shipped f32 one only.
    let kernels: &[bool] = if "abc".chars().any(want) {
        &[false, true]
    } else {
        &[false]
    };
    let exact = Exact {
        reader,
        frames,
        spp,
        extent,
        pool: &pool,
    };
    if want('e') {
        gpu.fixed.store(false, Ordering::Relaxed);
        let bound = Bound::new(spp, args.eps_scale);
        println!(
            "EPS a={:.4} b={:.4e} eps_scale={} detail_gpu={DETAIL_GPU:.6} detail_cpu={DETAIL_CPU:.6} cpu_quantization={CPU_QUANTIZATION:.3e} window_samples={}",
            bound.a,
            bound.b,
            args.eps_scale,
            (2 * SCORE_RADIUS + 1).pow(2) * spp
        );
        let retained: u64 = plan
            .tiles()
            .iter()
            .map(|t| u64::from(t.read().width) * u64::from(t.read().height) * 4)
            .sum();
        println!(
            "RETAINED-PEAKS bytes={retained} tiles={} (sum of read rects x 4 B, held on the device between the histogram and the re-check)",
            plan.tiles().len()
        );
        bench("a-gpu-devicebin", args.iters, Some(&gpu.times), || {
            gpu::stage_a(
                gpu,
                reader,
                &plan,
                frames,
                spp,
                &pool,
                Variant::DeviceBin,
                None,
                None,
            )?;
            Ok(())
        })?;
        let mut last = None;
        bench("a-gpu-recheck", args.iters, Some(&gpu.times), || {
            let mut kept = Vec::new();
            let hist = gpu::stage_a(
                gpu,
                reader,
                &plan,
                frames,
                spp,
                &pool,
                Variant::DeviceBin,
                None,
                Some(&mut kept),
            )?;
            let run = recheck_scale(&hist, &plan, pixels, &bound, &exact, &mut |i| {
                let t = &plan.tiles()[i];
                Ok(gpu::interior_peaks(
                    &gpu.download_peaks(&kept[i], t.read()),
                    t.read(),
                    t.interior(),
                ))
            })?;
            gpu.times.recheck.fetch_add(run.ns, Ordering::Relaxed);
            last = Some(run);
            Ok(())
        })?;
        let run = last.expect("the bench ran at least once");
        println!(
            "SCALE recheck bits={:#010x} value={} equal_shipped={} iterations={} candidates_per_pass={:?}",
            run.scale.to_bits(),
            run.scale,
            run.scale.to_bits() == scale.to_bits(),
            run.iterations,
            run.candidates
        );
    }
    for &fixed in kernels {
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
                        gpu::stage_a(gpu, reader, &plan, frames, spp, &pool, variant, None, None)?;
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
    for &fixed in kernels {
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
                None,
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
    if want('e') {
        gpu.fixed.store(false, Ordering::Relaxed);
        parity_e(gpu, &exact, &plan, pixels, scale, args.eps_scale)?;
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
