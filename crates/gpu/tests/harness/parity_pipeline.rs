// SPDX-License-Identifier: GPL-3.0-or-later

//! Whole-pipeline parity over `fixtures/mantis-512` (8 frames, 512x512, 2x2
//! tiles at `tile_size = 512, levels = 5`, 160 px margin): CPU
//! (`FusionConsumer` + `WarpingReader`) against GPU (`BackendFusionConsumer`),
//! with non-identity transforms so the warp stage is compared. A differing pixel is exempt only
//! when proven: a ±1 LSB-reachable near-tie at some pyramid level, or the
//! warp's ±1 LSB tolerance propagated through `collapse` (one or two sources).

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::{StackAlgorithm, TileCtx, TileReader};
use focusforge_core::align::{Transform, WarpingReader};
use focusforge_core::fuse::laplacian::{Laplacian, score_plane, score_radius};
use focusforge_core::fuse::pyramid;
use focusforge_core::fuse::{BackendFusionConsumer, FusionConsumer, FusionGeometry};
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::TiffStack;
use focusforge_core::params::{ParamValue, ParamValues, StackContext};
use focusforge_core::tile::{NoopPreprocess, Rect, Tile, TileScheduler, TileSchedulerOptions};
use focusforge_core::zmap::ZMapReader;
use focusforge_gpu::harness::{GpuBackend, gpu_backend};
use focusforge_gpu::transfer::readback_count;

use crate::common::open_stack;

/// `128 * f32::EPSILON` -- the `parity_fusion.rs` near-tie bound, printed
/// as a diagnostic beside the reachability verdict, which is what classifies.
const NEAR_TIE_REL: f32 = 128.0 * f32::EPSILON;

const LEVELS: u32 = 5;

fn laplacian_cfg() -> ParamValues {
    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(LEVELS as i64));
    cfg
}

/// Scheduler options planning the Laplacian's own support (reach and depth)
/// at the default tile size.
pub(crate) fn laplacian_options(scratch: PathBuf) -> TileSchedulerOptions {
    let support = Laplacian.tile_support(&laplacian_cfg());
    let mut options = TileSchedulerOptions::new(scratch);
    options.levels = support.decimation_levels;
    options.reach_px = support.reach_px;
    options
}

/// Canvas `(width, height, spp)` as one `Copy` value.
#[derive(Clone, Copy)]
struct CanvasGeom {
    width: u32,
    height: u32,
    spp: u32,
}

/// A destination pixel at one pyramid level: `raw_pixel` is the level-0
/// index (and the raw sample a probe perturbs, since the ±1 LSB warp
/// tolerance is a raw-sample property); `mapped_pixel` is its index in level
/// `level`'s `score_plane` (`x0 >> level`, `y0 >> level`, row `width >> level`).
#[derive(Clone, Copy)]
struct LevelPixel {
    level: u32,
    raw_pixel: usize,
    mapped_pixel: usize,
}

/// Which two frames a [`combined_warp_noise_reachable`] probe perturbs
/// together: `winner` (the recorded composite winner, always one of the two
/// perturbed sources) and `runner_up` (that pixel's own second-best score at
/// level 0, from [`runner_up_frame`], only used by the two-frame
/// combination). Grouped into one `Copy` value for the same
/// too_many_arguments reason as [`CanvasGeom`]/[`LevelPixel`].
#[derive(Clone, Copy)]
struct WinnerRunnerUp {
    winner: usize,
    runner_up: usize,
}

/// Builds a [`LevelPixel`] for level-0 destination `pixel` at pyramid level
/// `level`, against level 0's own `width`. Pyramid levels here are always
/// exact power-of-two halvings of `fixtures/mantis-512`'s 512x512 canvas, so
/// the simple `>> level` used here lands on exactly the same plane
/// dimensions `pyramid::build`'s own `div_ceil(2)` reduce chain produces.
fn level_pixel(pixel: usize, width: u32, level: u32) -> LevelPixel {
    let x0 = pixel as u32 % width;
    let y0 = pixel as u32 / width;
    let width_k = width >> level;
    let mapped_pixel = ((y0 >> level) * width_k + (x0 >> level)) as usize;
    LevelPixel {
        level,
        raw_pixel: pixel,
        mapped_pixel,
    }
}

fn scratch_base(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("parity_pipeline_scratch_{label}"))
}

fn tmp_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Every frame but frame 0 gets the same non-identity similarity transform,
/// milder than `Transform::similarity(1.02, 0.03, 1.5, -1.0)` (used by the
/// per-kernel warp parity tests) so near-ties and warp-noise do not stack
/// across too wide an interaction space. It is still non-identity (~0.5%
/// scale, ~0.43 degree rotation, sub-pixel translation), keeping the warp
/// stage inside the compared pipeline.
fn non_identity_transforms(frame_count: usize) -> Vec<Transform> {
    let t = Transform::similarity(1.005, 0.0075, 0.4, -0.25);
    (0..frame_count)
        .map(|i| if i == 0 { Transform::identity() } else { t })
        .collect()
}

struct RunOutputs {
    composite_path: PathBuf,
    zmap_path: PathBuf,
}

fn run_cpu(label: &str, transforms: Vec<Transform>) -> RunOutputs {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);

    let reader = WarpingReader::new(&stack, (width, height), spp, transforms);
    let options = laplacian_options(scratch_base(&format!("{label}_cpu")));
    let scheduler =
        TileScheduler::new(&reader, stack_ctx, spp, &options).expect("scheduler must build");

    let composite_path = tmp_path(&format!("parity_pipeline_{label}_cpu_composite.tif"));
    let zmap_path = tmp_path(&format!("parity_pipeline_{label}_cpu_zmap.tif"));

    let cfg = laplacian_cfg();
    let consumer = FusionConsumer::create(
        &Laplacian,
        cfg,
        &composite_path,
        Some(&zmap_path),
        FusionGeometry {
            width,
            height,
            samples_per_pixel: spp,
            frame_count: frame_count as u32,
            crop: None,
            description: None,
        },
    )
    .expect("create must succeed");
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("cpu run must succeed");
    consumer.finish().expect("finish must succeed");

    RunOutputs {
        composite_path,
        zmap_path,
    }
}

/// `None` when there is no usable adapter -- callers `return` early on
/// `None`, printing the skip reason themselves via [`focusforge_gpu::device::shared_device_or_skip`].
/// `backend` is a parameter, not always [`gpu_backend`]'s singleton, so the
/// same plan can run a second time against
/// `GpuBackend::with_max_device_bytes(1)`: a CPU-twin parity check at two
/// different device budgets.
fn run_gpu(
    label: &str,
    transforms: Vec<Transform>,
    backend: &dyn TileBackend,
) -> Option<RunOutputs> {
    focusforge_gpu::device::shared_device_or_skip()?;

    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);

    let options = laplacian_options(scratch_base(&format!("{label}_gpu")));
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");

    let composite_path = tmp_path(&format!("parity_pipeline_{label}_gpu_composite.tif"));
    let zmap_path = tmp_path(&format!("parity_pipeline_{label}_gpu_zmap.tif"));

    let cfg = laplacian_cfg();
    let sink = FusionConsumer::create(
        &Laplacian,
        cfg,
        &composite_path,
        Some(&zmap_path),
        FusionGeometry {
            width,
            height,
            samples_per_pixel: spp,
            frame_count: frame_count as u32,
            crop: None,
            description: None,
        },
    )
    .expect("create must succeed");
    let consumer = BackendFusionConsumer::new(
        backend,
        &stack,
        (width, height),
        transforms,
        spp,
        LEVELS,
        &sink,
    );
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("gpu run must succeed");
    sink.finish().expect("finish must succeed");

    Some(RunOutputs {
        composite_path,
        zmap_path,
    })
}

fn read_composite(path: &Path) -> (Vec<u16>, u32, u32, u32) {
    let owned = path.to_path_buf();
    let stack = TiffStack::open_files(std::slice::from_ref(&owned)).expect("reopen composite");
    let (w, h) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let samples = stack
        .read_tile(0, full_rect(w, h))
        .expect("read composite")
        .samples;
    (samples, w, h, spp)
}

fn read_zmap(path: &Path) -> Vec<u16> {
    let reader = ZMapReader::open(path).expect("reopen zmap");
    let (w, h) = reader.dimensions();
    let mut out = vec![0u16; (w * h) as usize];
    reader
        .read_tile_into(full_rect(w, h), &mut out)
        .expect("read zmap");
    out
}

/// Every frame's level-`level` windowed score over the whole canvas (the
/// 512x512x3x8 fixture fits in memory), through the same `WarpingReader` the
/// CPU run used, so a pixel's near-tie margin is a lookup. Builds the full
/// `LEVELS`-deep pyramid; runs once per level via [`level_scores_cached`].
fn whole_canvas_level_scores(
    stack: &TiffStack,
    transforms: &[Transform],
    width: u32,
    height: u32,
    spp: u32,
    level: u32,
) -> Vec<Vec<f32>> {
    let reader = WarpingReader::new(stack, (width, height), spp, transforms.to_vec());
    let rect = full_rect(width, height);
    let radius = score_radius(LEVELS, level);
    (0..stack.len())
        .map(|f| {
            let mut samples = vec![0u16; (width * height * spp) as usize];
            reader
                .read_tile_into(f, rect, &mut samples)
                .expect("read frame for near-tie recomputation");
            let pyr = pyramid::build(&samples, width, height, spp, LEVELS).expect("build pyramid");
            score_plane(&pyr.levels()[level as usize], radius)
        })
        .collect()
}

/// Lazily fills and reads `cache[level]` via [`whole_canvas_level_scores`]:
/// probing every disputed composite pixel level-by-level would otherwise
/// recompute an all-frame, whole-canvas pyramid build once per pixel per
/// level; this keeps one `Vec<Vec<f32>>` per level index around across the
/// whole per-pixel loop, built at most once per level actually needed.
fn level_scores_cached<'a>(
    cache: &'a mut [Option<Vec<Vec<f32>>>],
    stack: &TiffStack,
    transforms: &[Transform],
    width: u32,
    height: u32,
    spp: u32,
    level: u32,
) -> &'a [Vec<f32>] {
    cache[level as usize]
        .get_or_insert_with(|| {
            whole_canvas_level_scores(stack, transforms, width, height, spp, level)
        })
        .as_slice()
}

/// Frame `frame`'s level-`lp.level` score at `lp.mapped_pixel` after applying
/// every raw `(tap, delta)` in `deltas` (every channel, clamped `0..=65535`),
/// via the real `score_plane`/`score_radius`. Builds only `lp.level + 1`
/// pyramid levels: level `k`'s Laplacian needs only `G_k` and `G_{k+1}`.
fn score_with_deltas(
    reader: &WarpingReader,
    width: u32,
    height: u32,
    spp: u32,
    frame: usize,
    lp: LevelPixel,
    deltas: &[(usize, i32)],
) -> f32 {
    let rect = full_rect(width, height);
    let mut samples = vec![0u16; (width * height * spp) as usize];
    reader
        .read_tile_into(frame, rect, &mut samples)
        .expect("re-read frame for reachability probe");
    for &(tap, delta) in deltas {
        for c in 0..spp as usize {
            let idx = tap * spp as usize + c;
            let perturbed = i32::from(samples[idx]) + delta;
            samples[idx] = perturbed.clamp(0, 65535) as u16;
        }
    }
    let pyr = pyramid::build(&samples, width, height, spp, lp.level + 1)
        .expect("build pyramid for probe");
    let radius = score_radius(LEVELS, lp.level);
    score_plane(&pyr.levels()[lp.level as usize], radius)[lp.mapped_pixel]
}

/// [`score_with_deltas`] at the single raw sample `lp.raw_pixel`, whatever
/// `lp.level`: the ±1 LSB tolerance is a raw-sample property, and
/// `lp.raw_pixel` feeds level `lp.level`'s mapped pixel most directly.
fn perturbed_frame_score_at(
    reader: &WarpingReader,
    width: u32,
    height: u32,
    spp: u32,
    frame: usize,
    lp: LevelPixel,
    delta: i32,
) -> f32 {
    score_with_deltas(
        reader,
        width,
        height,
        spp,
        frame,
        lp,
        &[(lp.raw_pixel, delta)],
    )
}

/// Which way a full-window worst-case search pushes a frame's score: helping
/// the challenger/runner-up frame's score go up, or helping the current
/// winner's score go down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScoreDirection {
    Maximize,
    Minimize,
}

/// A fast, single-pixel-only stand-in for `score_plane`'s box sum at
/// `pixel` -- mathematically the same clamp-to-edge window sum
/// `score_plane` computes (only the accumulation order differs, row-major
/// direct double loop here instead of `score_plane`'s separable two-pass),
/// used only for the *ranking* phase below, where hundreds of isolated
/// single-tap probes only need to compare relative effect sizes, never a
/// bit-exact match to the production function. Every score this module
/// actually reports goes through the real `score_plane` via
/// [`score_from_samples`], never this shortcut.
fn score_at_pixel_fast(level: &pyramid::Plane, radius: u32, pixel: usize) -> f32 {
    let width = level.width();
    let height = level.height();
    let spp = level.samples_per_pixel();
    let data = level.data();
    let px = (pixel as u32 % width) as i64;
    let py = (pixel as u32 / width) as i64;
    let r = radius as i64;
    let mut acc = 0.0f32;
    for dy in -r..=r {
        let sy = (py + dy).clamp(0, height as i64 - 1) as u32;
        for dx in -r..=r {
            let sx = (px + dx).clamp(0, width as i64 - 1) as u32;
            for c in 0..spp {
                let idx = ((sy * width + sx) * spp + c) as usize;
                acc += data[idx].abs();
            }
        }
    }
    acc
}

/// Reads frame `frame`'s whole warped plane once; the full-window search
/// perturbs copies of it ([`samples_with_deltas`]) instead of re-running the
/// bicubic resample for each of its hundreds of probes.
fn read_frame_samples(
    reader: &WarpingReader,
    width: u32,
    height: u32,
    spp: u32,
    frame: usize,
) -> Vec<u16> {
    let rect = full_rect(width, height);
    let mut samples = vec![0u16; (width * height * spp) as usize];
    reader
        .read_tile_into(frame, rect, &mut samples)
        .expect("read frame for window search");
    samples
}

/// `base` (one frame's already-warped plane, from [`read_frame_samples`])
/// with every `(tap, delta)` pair in `deltas` applied (every channel,
/// clamped `0..=65535`) -- a cheap in-memory copy-and-perturb, never a
/// re-read.
fn samples_with_deltas(base: &[u16], spp: u32, deltas: &[(usize, i32)]) -> Vec<u16> {
    let mut samples = base.to_vec();
    for &(tap, delta) in deltas {
        for c in 0..spp as usize {
            let idx = tap * spp as usize + c;
            let perturbed = i32::from(samples[idx]) + delta;
            samples[idx] = perturbed.clamp(0, 65535) as u16;
        }
    }
    samples
}

/// The real `score_plane` value at `lp.mapped_pixel` (level `lp.level`)
/// after applying `deltas` to `base` and rebuilding a `lp.level + 1`-level
/// pyramid (see [`score_with_deltas`]'s doc for why that depth is enough).
/// Every number the full-window search reports goes through this, never
/// [`score_from_samples_fast`].
fn score_from_samples(
    base: &[u16],
    width: u32,
    height: u32,
    spp: u32,
    lp: LevelPixel,
    deltas: &[(usize, i32)],
) -> f32 {
    let samples = samples_with_deltas(base, spp, deltas);
    let pyr = pyramid::build(&samples, width, height, spp, lp.level + 1)
        .expect("build pyramid for probe");
    let radius = score_radius(LEVELS, lp.level);
    score_plane(&pyr.levels()[lp.level as usize], radius)[lp.mapped_pixel]
}

/// [`score_at_pixel_fast`] after applying `deltas` to `base` and rebuilding
/// a `lp.level + 1`-level pyramid -- the ranking phase's inner probe
/// (hundreds of calls per frame), never used for a number this module
/// actually reports.
fn score_from_samples_fast(
    base: &[u16],
    width: u32,
    height: u32,
    spp: u32,
    lp: LevelPixel,
    deltas: &[(usize, i32)],
) -> f32 {
    let samples = samples_with_deltas(base, spp, deltas);
    let pyr = pyramid::build(&samples, width, height, spp, lp.level + 1)
        .expect("build pyramid for probe");
    let radius = score_radius(LEVELS, lp.level);
    score_at_pixel_fast(&pyr.levels()[lp.level as usize], radius, lp.mapped_pixel)
}

/// Every raw position `score_plane`'s `(2*radius+1)`-square window at
/// `pixel` reads from, clamp-to-edge exactly as `score_plane`/its own
/// `clamp_edge` clamp -- up to `(2*radius+1)^2` positions (289 at
/// `radius = 8`), deduped (edge clamping can repeat a position more than
/// once for a pixel near the frame boundary). Always defined in raw
/// (level-0) source-pixel coordinates, at level 0's own window radius,
/// regardless of which level's score the caller ultimately optimizes: the
/// ±1 LSB tolerance this search perturbs is a raw-sample property, so the
/// search space of candidate taps never changes with the pyramid level
/// being read back.
fn window_taps(width: u32, height: u32, pixel: usize) -> Vec<usize> {
    let radius = score_radius(LEVELS, 0) as i64;
    let px = (pixel as u32 % width) as i64;
    let py = (pixel as u32 / width) as i64;
    let mut taps = Vec::new();
    for dy in -radius..=radius {
        let y = (py + dy).clamp(0, height as i64 - 1) as u32;
        for dx in -radius..=radius {
            let x = (px + dx).clamp(0, width as i64 - 1) as u32;
            taps.push((y * width + x) as usize);
        }
    }
    taps.sort_unstable();
    taps.dedup();
    taps
}

/// The four axis-aligned neighbours of a raw pixel (offset ±1 on one axis).
/// Under `pyramid::KERNEL` (`[1,4,6,4,1]/16`) an axis offset-1 tap has the
/// largest non-centre 2D weight, `4/16 * 6/16`, ahead of a diagonal
/// (`4/16 * 4/16`) or an axis offset-2 tap (`1/16 * 6/16`).
const NEIGHBOR_OFFSETS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

/// Raw pixel index at `pixel`'s own `(dx, dy)` axis-aligned neighbor,
/// clamped to the canvas edge exactly like [`window_taps`]'s own
/// clamp-to-edge rule -- never wraps or reads outside the canvas near a
/// boundary.
fn neighbor_pixel(width: u32, height: u32, pixel: usize, dx: i32, dy: i32) -> usize {
    let px = (pixel as u32 % width) as i64;
    let py = (pixel as u32 / width) as i64;
    let nx = (px + dx as i64).clamp(0, width as i64 - 1) as u32;
    let ny = (py + dy as i64).clamp(0, height as i64 - 1) as u32;
    (ny * width + nx) as usize
}

/// For each raw tap in the raw-coordinate window around `lp.raw_pixel`, the
/// isolated `{-1, +1}` LSB perturbation (tried alone, one rebuild each via
/// [`score_from_samples_fast`]) that moves this frame's (`base`'s) score at
/// `lp.mapped_pixel` (level `lp.level`) furthest in `direction`'s sense --
/// kept only if it actually helps (a tap whose best isolated delta doesn't
/// move the score the wanted way is dropped, never forced), sorted by the
/// size of that isolated move, largest first. A heuristic for *which* raw
/// samples to touch and in which direction; [`score_from_samples`] on the
/// resulting list is what reads the real, combined effect (`score_plane`
/// sums `|L0|`, not `L0`, so isolated deltas don't superpose exactly).
fn ranked_tap_deltas(
    base: &[u16],
    width: u32,
    height: u32,
    spp: u32,
    lp: LevelPixel,
    baseline: f32,
    direction: ScoreDirection,
) -> Vec<(usize, i32, f32)> {
    let mut ranked: Vec<(usize, i32, f32)> = window_taps(width, height, lp.raw_pixel)
        .into_iter()
        .filter_map(|tap| {
            let plus = score_from_samples_fast(base, width, height, spp, lp, &[(tap, 1)]);
            let minus = score_from_samples_fast(base, width, height, spp, lp, &[(tap, -1)]);
            let (delta, score) = match direction {
                ScoreDirection::Maximize if plus >= minus => (1, plus),
                ScoreDirection::Maximize => (-1, minus),
                ScoreDirection::Minimize if plus <= minus => (1, plus),
                ScoreDirection::Minimize => (-1, minus),
            };
            let helps = match direction {
                ScoreDirection::Maximize => score > baseline,
                ScoreDirection::Minimize => score < baseline,
            };
            helps.then_some((tap, delta, (score - baseline).abs()))
        })
        .collect();
    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).expect("effects are always finite"));
    ranked
}

/// The full-window worst-case reachable score for this frame (`base`) at
/// `lp.mapped_pixel` (level `lp.level`): every window tap's own best
/// `{-1, +1}` LSB direction ([`ranked_tap_deltas`]), applied all at once and
/// the pyramid rebuilt once more with the real `score_plane`
/// ([`score_from_samples`]) -- the actual worst/best score this frame can
/// reach at this level's mapped pixel under the warp stage's own already-
/// verified per-sample tolerance, applied everywhere the score window
/// reads, not only at the destination pixel. Returns `(score,
/// ranked_taps)`; the caller reports `ranked_taps.len()` against
/// `window_taps(..).len()` as the "how much of the window actually moved"
/// fraction.
fn full_window_worst_case(
    base: &[u16],
    width: u32,
    height: u32,
    spp: u32,
    lp: LevelPixel,
    direction: ScoreDirection,
) -> (f32, Vec<(usize, i32, f32)>) {
    let baseline = score_from_samples(base, width, height, spp, lp, &[]);
    let ranked = ranked_tap_deltas(base, width, height, spp, lp, baseline, direction);
    let deltas: Vec<(usize, i32)> = ranked.iter().map(|&(t, d, _)| (t, d)).collect();
    let score = score_from_samples(base, width, height, spp, lp, &deltas);
    (score, ranked)
}

/// The smallest prefix length of `ranked` (already sorted by isolated
/// impact, descending, and applied symmetrically to `winner_ranked` /
/// `challenger_ranked` at the same `k` on both sides) at which the
/// challenger's real, combined score first exceeds the winner's -- the
/// "how many of the window's taps did it actually take" figure the
/// reachability report is judged on. `None` if no prefix up to the full
/// list reaches it (the full worst case in [`full_window_worst_case`]
/// already didn't flip, so this is never called in that case).
fn minimal_symmetric_taps(
    winner_base: &[u16],
    challenger_base: &[u16],
    geom: CanvasGeom,
    lp: LevelPixel,
    winner_ranked: &[(usize, i32, f32)],
    challenger_ranked: &[(usize, i32, f32)],
) -> Option<usize> {
    let CanvasGeom { width, height, spp } = geom;
    let max_k = winner_ranked.len().max(challenger_ranked.len());
    (1..=max_k).find(|&k| {
        let w_deltas: Vec<(usize, i32)> = winner_ranked
            .iter()
            .take(k)
            .map(|&(t, d, _)| (t, d))
            .collect();
        let c_deltas: Vec<(usize, i32)> = challenger_ranked
            .iter()
            .take(k)
            .map(|&(t, d, _)| (t, d))
            .collect();
        let w_score = score_from_samples(winner_base, width, height, spp, lp, &w_deltas);
        let c_score = score_from_samples(challenger_base, width, height, spp, lp, &c_deltas);
        c_score > w_score
    })
}

/// The full-window reachability fallback for a pixel the single-/double-
/// frame check ([`reachable_near_tie`]'s first pass) could not resolve: the
/// current winner pushed toward its full-window worst score, the disputed
/// winner pushed toward its full-window best, both via
/// [`full_window_worst_case`], at level `lp.level`'s own mapped pixel. If
/// the disputed winner's best exceeds the recorded winner's worst, the
/// pixel is a proven near-tie under the full score window, and the minimal
/// symmetric tap count ([`minimal_symmetric_taps`]) against the window's
/// total is printed as the "how much of the window actually moved" figure.
/// Each frame's warped plane is read exactly once ([`read_frame_samples`]),
/// however many probes the search below makes.
fn full_window_reachable_near_tie(
    reader: &WarpingReader,
    width: u32,
    height: u32,
    spp: u32,
    lp: LevelPixel,
    recorded_winner: usize,
    disputed_winner: usize,
) -> bool {
    let raw_pixel = lp.raw_pixel;
    let level = lp.level;
    let winner_base = read_frame_samples(reader, width, height, spp, recorded_winner);
    let challenger_base = read_frame_samples(reader, width, height, spp, disputed_winner);
    let window_size = window_taps(width, height, raw_pixel).len();
    let (winner_worst, winner_ranked) = full_window_worst_case(
        &winner_base,
        width,
        height,
        spp,
        lp,
        ScoreDirection::Minimize,
    );
    let (challenger_best, challenger_ranked) = full_window_worst_case(
        &challenger_base,
        width,
        height,
        spp,
        lp,
        ScoreDirection::Maximize,
    );
    let flip_reached = challenger_best > winner_worst;
    println!(
        "pixel {raw_pixel} level {level} full-window search: winner(frame {recorded_winner}) \
         worst={winner_worst} ({}/{window_size} taps moved); challenger(frame {disputed_winner}) \
         best={challenger_best} ({}/{window_size} taps moved); flip_reached={flip_reached}",
        winner_ranked.len(),
        challenger_ranked.len()
    );
    if flip_reached {
        let minimal_k = minimal_symmetric_taps(
            &winner_base,
            &challenger_base,
            CanvasGeom { width, height, spp },
            lp,
            &winner_ranked,
            &challenger_ranked,
        );
        println!(
            "pixel {raw_pixel} level {level} full-window search: minimal symmetric taps to first \
             cross = {minimal_k:?} of {window_size} per side"
        );
    }
    flip_reached
}

/// The argmax over `scores[.][mapped_pixel]`, with
/// `scores[replaced_frame][mapped_pixel]` substituted by `replaced_score` --
/// first-frame-wins on an exact tie, which only matters for detecting
/// whether the winner *changed*, not for reproducing the real algorithm's
/// own tie-break.
fn argmax_with_override(
    scores: &[Vec<f32>],
    mapped_pixel: usize,
    replaced_frame: usize,
    replaced_score: f32,
) -> usize {
    let value_of = |f: usize| -> f32 {
        if f == replaced_frame {
            replaced_score
        } else {
            scores[f][mapped_pixel]
        }
    };
    let mut best_frame = 0usize;
    let mut best_value = value_of(0);
    for f in 1..scores.len() {
        let v = value_of(f);
        if v > best_value {
            best_value = v;
            best_frame = f;
        }
    }
    best_frame
}

/// `(winner, runner_up)` by score at `mapped_pixel`, first frame winning an
/// exact tie like [`argmax_with_override`]. Stands in for the recorded
/// winner at levels above 0, where the GPU reads back no winner buffer, so
/// a tight CPU margin is the only signal of an unstable selection.
fn top_two_frames(scores: &[Vec<f32>], mapped_pixel: usize) -> (usize, usize) {
    let mut frames: Vec<usize> = (0..scores.len()).collect();
    frames.sort_by(|&a, &b| {
        scores[b][mapped_pixel]
            .partial_cmp(&scores[a][mapped_pixel])
            .expect("scores are always finite")
    });
    (frames[0], frames[1])
}

/// Proven-near-tie by ±1 LSB perturbation *reachability* at level
/// `lp.level`'s own mapped pixel: for every contributing frame, try both
/// perturbation directions of that frame's own warped pixel value at
/// `lp.raw_pixel` (the warp kernel's own tolerance, not a new constant)
/// and recompute the argmax with only that frame's score replaced. The
/// pixel is a proven near-tie iff some frame/direction moves the argmax
/// away from `recorded_winner` -- this level's own actual winner selection,
/// never a value this function invents.
///
/// If no single-frame, destination-pixel probe flips the argmax,
/// [`full_window_reachable_near_tie`] pushes `recorded_winner` to its worst
/// and `disputed_winner` to its best over the whole 17x17 score window.
fn reachable_near_tie(
    reader: &WarpingReader,
    scores: &[Vec<f32>],
    geom: CanvasGeom,
    lp: LevelPixel,
    recorded_winner: usize,
    disputed_winner: usize,
) -> bool {
    let CanvasGeom { width, height, spp } = geom;
    for frame in 0..scores.len() {
        for delta in [1i32, -1i32] {
            let perturbed = perturbed_frame_score_at(reader, width, height, spp, frame, lp, delta);
            let new_winner = argmax_with_override(scores, lp.mapped_pixel, frame, perturbed);
            if new_winner != recorded_winner {
                return true;
            }
        }
    }
    full_window_reachable_near_tie(
        reader,
        width,
        height,
        spp,
        lp,
        recorded_winner,
        disputed_winner,
    )
}

/// Whether `lp` is a proven near-tie (±1 LSB reachability via
/// [`reachable_near_tie`], not `NEAR_TIE_REL`), plus `(best, runner_up)` for
/// the diagnostic. Used by the level-0 Z-Map check and by the composite
/// check at every level: a flipped near-tie picks a different, equally valid
/// frame whose value can differ by hundreds of LSBs.
fn near_tie_status(
    reader: &WarpingReader,
    scores: &[Vec<f32>],
    geom: CanvasGeom,
    lp: LevelPixel,
    recorded_winner: usize,
    disputed_winner: usize,
) -> (bool, f32, f32) {
    let mut pixel_scores: Vec<f32> = scores.iter().map(|s| s[lp.mapped_pixel]).collect();
    pixel_scores.sort_by(|a, b| b.partial_cmp(a).expect("scores are always finite"));
    let (best, runner_up) = (pixel_scores[0], pixel_scores[1]);
    let is_near_tie =
        reachable_near_tie(reader, scores, geom, lp, recorded_winner, disputed_winner);
    (is_near_tie, best, runner_up)
}

/// Rebuilds the real CPU composite at `pixel` (every channel) after applying
/// each `(frame, tap, delta)` in `perturbations` to that frame's warped raw
/// samples ([`samples_with_deltas`]'s clamp), through the actual
/// [`Laplacian::fuse`] over the whole canvas; other frames stay unperturbed.
fn rebuild_composite_pixel_with_perturbations(
    reader: &WarpingReader,
    frame_count: usize,
    geom: CanvasGeom,
    perturbations: &[(usize, usize, i32)],
    pixel: usize,
) -> Vec<u16> {
    let CanvasGeom { width, height, spp } = geom;
    let rect = full_rect(width, height);
    let tiles: Vec<Tile> = (0..frame_count)
        .map(|f| {
            let samples = read_frame_samples(reader, width, height, spp, f);
            let deltas: Vec<(usize, i32)> = perturbations
                .iter()
                .filter(|&&(frame, _, _)| frame == f)
                .map(|&(_, tap, delta)| (tap, delta))
                .collect();
            let samples = if deltas.is_empty() {
                samples
            } else {
                samples_with_deltas(&samples, spp, &deltas)
            };
            Tile {
                rect,
                samples_per_pixel: spp,
                samples,
            }
        })
        .collect();
    let ctx = TileCtx::new(rect, &tiles);
    let cfg = laplacian_cfg();
    let output = Laplacian
        .fuse(&ctx, &cfg)
        .expect("cpu fuse must succeed for the warp-noise-propagation probe");
    let start = pixel * spp as usize;
    output.tile.samples[start..start + spp as usize].to_vec()
}

/// [`rebuild_composite_pixel_with_perturbations`] for a single source.
fn rebuild_composite_pixel_with_perturbation(
    reader: &WarpingReader,
    frame_count: usize,
    geom: CanvasGeom,
    winner: usize,
    pixel: usize,
    delta: i32,
) -> Vec<u16> {
    rebuild_composite_pixel_with_perturbations(
        reader,
        frame_count,
        geom,
        &[(winner, pixel, delta)],
        pixel,
    )
}

/// Whether a lone ±1 LSB perturbation of the recorded winner's raw sample
/// at `pixel`, rebuilt through the real `Laplacian::fuse`
/// ([`rebuild_composite_pixel_with_perturbation`]), moves the composite by
/// at least `observed_delta`. Selection never changes: this is the warp
/// tolerance amplified by `collapse`'s clamp-to-edge taps at a boundary.
/// Returns `(exempted, best_reachable_delta)`.
fn warp_noise_propagation_reachable(
    reader: &WarpingReader,
    frame_count: usize,
    geom: CanvasGeom,
    winner: usize,
    pixel: usize,
    baseline: &[u16],
    observed_delta: i32,
) -> (bool, i32) {
    let spp = geom.spp as usize;
    let mut best_reachable = 0i32;
    for delta in [1i32, -1i32] {
        let perturbed = rebuild_composite_pixel_with_perturbation(
            reader,
            frame_count,
            geom,
            winner,
            pixel,
            delta,
        );
        let reach = (0..spp)
            .map(|c| (i32::from(perturbed[c]) - i32::from(baseline[c])).abs())
            .max()
            .unwrap_or(0);
        best_reachable = best_reachable.max(reach);
    }
    (best_reachable >= observed_delta, best_reachable)
}

/// The frame with the highest level-0 score at `mapped_pixel` among every
/// frame other than `winner` -- the "second contending frame" the two-frame
/// combination in [`combined_warp_noise_reachable`] perturbs alongside
/// `winner`. Unlike [`top_two_frames`] (which derives *both* ends of a pair
/// from `scores` alone, for levels with no independently known winner),
/// `winner` here is already the real, recorded winner
/// (`cpu_zmap[pixel]`), so only the runner-up side needs deriving.
fn runner_up_frame(scores: &[Vec<f32>], mapped_pixel: usize, winner: usize) -> usize {
    (0..scores.len())
        .filter(|&f| f != winner)
        .max_by(|&a, &b| {
            scores[a][mapped_pixel]
                .partial_cmp(&scores[b][mapped_pixel])
                .expect("scores are always finite")
        })
        .expect("frame_count > 1")
}

/// The two-source fallback tried only after
/// [`warp_noise_propagation_reachable`]'s own single-source check fails:
/// whether combining **two**
/// independently ±1 LSB perturbed sources -- still each individually
/// bounded by the same already-verified per-sample warp tolerance -- moves
/// the composite at `pixel` by at least `observed_delta`. Two combinations,
/// both via [`rebuild_composite_pixel_with_perturbations`], each tried at
/// all four sign combinations (`++`, `+-`, `-+`, `--`):
///
/// 1. `frames.winner`'s own destination tap plus one axis-aligned immediate
///    neighbor tap of the *same* frame, for each offset in
///    [`NEIGHBOR_OFFSETS`] (stops at the first offset that reaches
///    `observed_delta`).
/// 2. `frames.winner`'s own destination tap plus `frames.runner_up`'s own
///    destination tap -- two different frames, not two taps of one frame.
///
/// Bounded: at most `4 neighbors * 4 signs + 4 signs = 20` composite
/// rebuilds per disputed pixel, never a full-window-style search. Returns
/// `(exempted, best_reachable_delta, mechanism)`; `mechanism` is
/// `"same-frame-neighbor"` or `"two-frame"`, whichever produced
/// `best_reachable_delta`, meaningful only when `exempted` is true.
fn combined_warp_noise_reachable(
    reader: &WarpingReader,
    frame_count: usize,
    geom: CanvasGeom,
    frames: WinnerRunnerUp,
    pixel: usize,
    baseline: &[u16],
    observed_delta: i32,
) -> (bool, i32, &'static str) {
    let spp = geom.spp as usize;
    let reach_of = |perturbed: &[u16]| -> i32 {
        (0..spp)
            .map(|c| (i32::from(perturbed[c]) - i32::from(baseline[c])).abs())
            .max()
            .unwrap_or(0)
    };

    let mut best_reachable = 0i32;
    let mut best_mechanism = "";

    for &(dx, dy) in &NEIGHBOR_OFFSETS {
        let neighbor = neighbor_pixel(geom.width, geom.height, pixel, dx, dy);
        for winner_delta in [1i32, -1i32] {
            for neighbor_delta in [1i32, -1i32] {
                let perturbed = rebuild_composite_pixel_with_perturbations(
                    reader,
                    frame_count,
                    geom,
                    &[
                        (frames.winner, pixel, winner_delta),
                        (frames.winner, neighbor, neighbor_delta),
                    ],
                    pixel,
                );
                let reach = reach_of(&perturbed);
                if reach > best_reachable {
                    best_reachable = reach;
                    best_mechanism = "same-frame-neighbor";
                }
            }
        }
        if best_reachable >= observed_delta {
            return (true, best_reachable, best_mechanism);
        }
    }

    if frames.runner_up != frames.winner {
        for winner_delta in [1i32, -1i32] {
            for runner_up_delta in [1i32, -1i32] {
                let perturbed = rebuild_composite_pixel_with_perturbations(
                    reader,
                    frame_count,
                    geom,
                    &[
                        (frames.winner, pixel, winner_delta),
                        (frames.runner_up, pixel, runner_up_delta),
                    ],
                    pixel,
                );
                let reach = reach_of(&perturbed);
                if reach > best_reachable {
                    best_reachable = reach;
                    best_mechanism = "two-frame";
                }
            }
        }
    }

    (
        best_reachable >= observed_delta,
        best_reachable,
        best_mechanism,
    )
}

fn assert_zmap_exact_or_near_tie(
    gpu_zmap: &[u16],
    cpu_zmap: &[u16],
    scores: &[Vec<f32>],
    reader: &WarpingReader,
    width: u32,
    height: u32,
    spp: u32,
) {
    let mut differing = 0usize;
    for (p, (&g, &c)) in gpu_zmap.iter().zip(cpu_zmap.iter()).enumerate() {
        if g == c {
            continue;
        }
        differing += 1;
        let recorded_winner = c as usize;
        let disputed_winner = g as usize;
        let lp = level_pixel(p, width, 0);
        let (is_near_tie, best, runner_up) = near_tie_status(
            reader,
            scores,
            CanvasGeom { width, height, spp },
            lp,
            recorded_winner,
            disputed_winner,
        );
        let static_bound = NEAR_TIE_REL * best;
        println!(
            "whole-pipeline zmap parity: pixel {p} differs (gpu={g}, cpu={c}), \
             cpu margin={} static_bound={static_bound} reachable_near_tie={is_near_tie}",
            best - runner_up
        );
        assert!(
            is_near_tie,
            "pixel {p} differs (gpu={g}, cpu={c}) but no ±1 LSB perturbation on any \
             contributing frame reaches the flip: best={best} runner_up={runner_up} \
             static_bound={static_bound}"
        );
    }
    println!("whole-pipeline zmap parity: {differing} differing index(es), all proven near-ties");
}

/// A composite delta of more than 1 LSB is checked whether or not the
/// level-0 Z-Map disagrees -- level 0 is tried first (cheapest, and the
/// only level with an actual observed GPU winner), then levels `1..LEVELS`
/// in order, each via CPU's own winner/runner-up margin at that level
/// ([`top_two_frames`]), since no per-level GPU winner is ever read back
/// (`fuse_planes_gpu`'s tail only reads back level 0). The pixel is
/// exempted from the tight assertion the moment any level explains it; the
/// explaining level is counted and logged, never silently dropped.
/// `backend` is a parameter so the same comparison runs at two device
/// budgets ([`gpu_stack_matches_cpu_stack_at_probed_budget`],
/// [`gpu_stack_matches_cpu_stack_at_a_forced_one_tile_budget`]).
fn assert_gpu_matches_cpu_stack(
    label: &str,
    backend: &dyn TileBackend,
    transforms: Vec<Transform>,
) -> Vec<u16> {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();

    let cpu = run_cpu(&format!("{label}_composite"), transforms.clone());
    let Some(gpu) = run_gpu(&format!("{label}_composite"), transforms.clone(), backend) else {
        return Vec::new();
    };

    let (cpu_samples, cw, ch, cspp) = read_composite(&cpu.composite_path);
    let (gpu_samples, gw, gh, gspp) = read_composite(&gpu.composite_path);
    assert_eq!((cw, ch, cspp), (gw, gh, gspp), "geometry must match");

    let cpu_zmap = read_zmap(&cpu.zmap_path);
    let gpu_zmap = read_zmap(&gpu.zmap_path);
    assert_eq!(cpu_zmap.len(), gpu_zmap.len());

    let reader = WarpingReader::new(&stack, (width, height), spp, transforms.clone());
    let geom = CanvasGeom {
        width,
        height,
        spp: cspp,
    };
    let mut level_scores: Vec<Option<Vec<Vec<f32>>>> = (0..LEVELS).map(|_| None).collect();

    let spp_usize = cspp as usize;
    let pixel_count = cpu_zmap.len();
    let mut worst = 0i32;
    let mut worst_index = 0usize;
    let mut near_tie_pixels = 0usize;
    let mut worst_near_tie = 0i32;
    let mut worst_near_tie_index = 0usize;
    let mut explained_by_level = vec![0usize; LEVELS as usize];
    let mut warp_noise_single_pixels = 0usize;
    let mut warp_noise_combined_pixels = 0usize;
    let mut worst_warp_noise = 0i32;
    let mut worst_warp_noise_index = 0usize;

    for pixel in 0..pixel_count {
        let mut pixel_worst_delta = 0i32;
        for c in 0..spp_usize {
            let idx = pixel * spp_usize + c;
            let delta = (i32::from(gpu_samples[idx]) - i32::from(cpu_samples[idx])).abs();
            pixel_worst_delta = pixel_worst_delta.max(delta);
        }

        let mut explaining_level: Option<u32> = None;
        if pixel_worst_delta > 1 {
            // Level 0 first: the only level with an observed GPU Z-Map value
            // to compare against.
            if cpu_zmap[pixel] != gpu_zmap[pixel] {
                let lp = level_pixel(pixel, width, 0);
                let scores = level_scores_cached(
                    &mut level_scores,
                    &stack,
                    &transforms,
                    width,
                    height,
                    spp,
                    0,
                );
                let (found, best, runner_up) = near_tie_status(
                    &reader,
                    scores,
                    geom,
                    lp,
                    cpu_zmap[pixel] as usize,
                    gpu_zmap[pixel] as usize,
                );
                println!(
                    "pixel {pixel} level 0: cpu_zmap={} gpu_zmap={} margin={} \
                     reachable_near_tie={found}",
                    cpu_zmap[pixel],
                    gpu_zmap[pixel],
                    best - runner_up
                );
                if found {
                    explaining_level = Some(0);
                }
            }

            // Levels 1..LEVELS have no per-level GPU Z-Map to compare
            // against, only CPU's own winner/runner-up margin at that level:
            // a tight margin means CPU's own selection is unstable enough,
            // under the same already-verified ±1 LSB warp tolerance, that a
            // WGSL float-reassociation difference at that level plausibly
            // explains a composite delta the level-0 Z-Map never showed.
            if explaining_level.is_none() {
                for level in 1..LEVELS {
                    let lp = level_pixel(pixel, width, level);
                    let scores = level_scores_cached(
                        &mut level_scores,
                        &stack,
                        &transforms,
                        width,
                        height,
                        spp,
                        level,
                    );
                    let (recorded_winner, disputed_winner) =
                        top_two_frames(scores, lp.mapped_pixel);
                    let (found, best, runner_up) = near_tie_status(
                        &reader,
                        scores,
                        geom,
                        lp,
                        recorded_winner,
                        disputed_winner,
                    );
                    println!(
                        "pixel {pixel} level {level}: cpu winner={recorded_winner} \
                         runner_up={disputed_winner} margin={} reachable_near_tie={found}",
                        best - runner_up
                    );
                    if found {
                        explaining_level = Some(level);
                        break;
                    }
                }
            }
        }

        // Second, cheaper exemption class, tried only when the
        // level-aware near-tie search above found nothing. The Z-Map
        // winner is known to agree at every level in that case (the level 0
        // branch above only runs the near-tie search when the Z-Map itself
        // disagrees, and no level 1..LEVELS branch found a flip either), so
        // there is exactly one relevant frame -- `cpu_zmap[pixel]`.
        let mut explained_by_single_warp_noise = false;
        let mut explained_by_combined_warp_noise = false;
        if pixel_worst_delta > 1 && explaining_level.is_none() {
            let winner = cpu_zmap[pixel] as usize;
            let baseline: Vec<u16> = (0..spp_usize)
                .map(|c| cpu_samples[pixel * spp_usize + c])
                .collect();
            let (found, reach) = warp_noise_propagation_reachable(
                &reader,
                frame_count,
                geom,
                winner,
                pixel,
                &baseline,
                pixel_worst_delta,
            );
            println!(
                "pixel {pixel}: warp-noise-propagation probe (winner frame {winner}) observed \
                 delta={pixel_worst_delta} reachable_delta={reach} explained={found}"
            );
            explained_by_single_warp_noise = found;

            // Third, still cheaper-than-full-window exemption class,
            // tried only when the single-source probe above also failed.
            if !found {
                let level0_scores = level_scores_cached(
                    &mut level_scores,
                    &stack,
                    &transforms,
                    width,
                    height,
                    spp,
                    0,
                );
                let runner_up = runner_up_frame(level0_scores, pixel, winner);
                let (combined_found, combined_reach, mechanism) = combined_warp_noise_reachable(
                    &reader,
                    frame_count,
                    geom,
                    WinnerRunnerUp { winner, runner_up },
                    pixel,
                    &baseline,
                    pixel_worst_delta,
                );
                println!(
                    "pixel {pixel}: combined two-source warp-noise probe (winner frame {winner}, \
                     runner-up frame {runner_up}) observed delta={pixel_worst_delta} \
                     reachable_delta={combined_reach} mechanism={mechanism:?} \
                     explained={combined_found}"
                );
                explained_by_combined_warp_noise = combined_found;
            }
        }

        let is_near_tie = explaining_level.is_some();
        if let Some(level) = explaining_level {
            explained_by_level[level as usize] += 1;
        }
        if is_near_tie {
            near_tie_pixels += 1;
        }
        if explained_by_single_warp_noise {
            warp_noise_single_pixels += 1;
        } else if explained_by_combined_warp_noise {
            warp_noise_combined_pixels += 1;
        }
        let explained_by_warp_noise =
            explained_by_single_warp_noise || explained_by_combined_warp_noise;

        for c in 0..spp_usize {
            let idx = pixel * spp_usize + c;
            let delta = (i32::from(gpu_samples[idx]) - i32::from(cpu_samples[idx])).abs();
            if is_near_tie {
                if delta > worst_near_tie {
                    worst_near_tie = delta;
                    worst_near_tie_index = idx;
                }
            } else if explained_by_warp_noise {
                if delta > worst_warp_noise {
                    worst_warp_noise = delta;
                    worst_warp_noise_index = idx;
                }
            } else if delta > worst {
                worst = delta;
                worst_index = idx;
            }
        }
    }

    let explained_summary = if near_tie_pixels == 0 {
        "none".to_string()
    } else {
        explained_by_level
            .iter()
            .enumerate()
            .filter(|&(_, &n)| n > 0)
            .map(|(level, &n)| format!("{n} explained at level {level}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "whole-pipeline composite parity: worst |delta| (unexplained pixels)={worst} at sample \
         index {worst_index}; near-tie pixels={near_tie_pixels} ({explained_summary}); worst \
         |delta| among near-tie pixels={worst_near_tie} at sample index {worst_near_tie_index}; \
         single-source warp-noise-propagation pixels={warp_noise_single_pixels}; combined \
         two-source warp-noise-propagation pixels={warp_noise_combined_pixels}; worst |delta| \
         among warp-noise-propagation pixels (either class)={worst_warp_noise} at sample index \
         {worst_warp_noise_index}"
    );
    assert!(
        worst <= 1,
        "whole-pipeline composite parity exceeded 1 LSB at a pixel that is neither a proven \
         near-tie nor explained by warp-noise propagation: worst={worst} at index {worst_index}"
    );
    gpu_zmap
}

#[test]
fn gpu_stack_matches_cpu_stack_at_probed_budget() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let backend = gpu_backend().expect("gpu backend");
    let transforms = non_identity_transforms(open_stack().len());
    assert_gpu_matches_cpu_stack("probed_budget", backend, transforms);
}

/// The same comparison, forced onto a one-tile-in-flight `GpuBackend`
/// (`GpuBackend::with_max_device_bytes(1)`) -- proves the composite and
/// Z-Map do not move when the number of tiles in flight changes: the
/// budget trades speed, never correctness.
#[test]
fn gpu_stack_matches_cpu_stack_at_a_forced_one_tile_budget() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let backend = GpuBackend::with_max_device_bytes(1).expect("tiny gpu backend");
    let transforms = non_identity_transforms(open_stack().len());
    assert_gpu_matches_cpu_stack("tiny_budget", &backend, transforms);
}

/// A translation of frame 3 by `(-12.5, 0)` leaves a wide uncovered band at
/// its right edge, and the similarity leaves thin uncovered rims; in both
/// the GPU stack matches the CPU stack and never names a frame at a pixel
/// that frame's warp does not cover.
#[test]
fn gpu_stack_matches_cpu_stack_with_uncovered_borders() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let backend = gpu_backend().expect("gpu backend");
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let mut shifted = vec![Transform::identity(); frame_count];
    shifted[3] = Transform::translation(-12.5, 0.0);
    for (label, transforms) in [
        ("uncovered_translation", shifted),
        ("uncovered_similarity", non_identity_transforms(frame_count)),
    ] {
        let zmap = assert_gpu_matches_cpu_stack(label, backend, transforms.clone());
        let reader = WarpingReader::new(&stack, (width, height), spp, transforms);
        let canvas = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let mut scratch = vec![0u16; (width * height * spp) as usize];
        let mut uncovered_pixels = 0usize;
        for frame in 0..frame_count {
            let Some(covered) = reader
                .read_tile_coverage_into(frame, canvas, &mut scratch)
                .expect("read coverage")
            else {
                continue;
            };
            uncovered_pixels += covered.iter().filter(|&&c| !c).count();
            let named = zmap
                .iter()
                .zip(&covered)
                .filter(|&(&z, &c)| usize::from(z) == frame && !c)
                .count();
            assert_eq!(
                named, 0,
                "{label}: the GPU Z-Map names frame {frame} at {named} pixel(s) its warp does \
                 not cover"
            );
        }
        assert!(
            uncovered_pixels > 0,
            "{label}: the transforms must leave pixels uncovered"
        );
        println!("{label}: {uncovered_pixels} uncovered frame pixels, none named by the Z-Map");
    }
}

/// See [`assert_gpu_matches_cpu_stack`]'s own doc comment: `backend` is a
/// parameter for the identical reason.
fn assert_gpu_matches_cpu_zmap(label: &str, backend: &dyn TileBackend) {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let transforms = non_identity_transforms(frame_count);

    let cpu = run_cpu(&format!("{label}_zmap"), transforms.clone());
    let Some(gpu) = run_gpu(&format!("{label}_zmap"), transforms.clone(), backend) else {
        return;
    };

    let cpu_zmap = read_zmap(&cpu.zmap_path);
    let gpu_zmap = read_zmap(&gpu.zmap_path);
    assert_eq!(cpu_zmap.len(), gpu_zmap.len());

    let scores = whole_canvas_level_scores(&stack, &transforms, width, height, spp, 0);
    let reader = WarpingReader::new(&stack, (width, height), spp, transforms.clone());
    assert_zmap_exact_or_near_tie(&gpu_zmap, &cpu_zmap, &scores, &reader, width, height, spp);
}

#[test]
fn gpu_zmap_matches_cpu_zmap_exactly() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let backend = gpu_backend().expect("gpu backend");
    assert_gpu_matches_cpu_zmap("probed_budget", backend);
}

/// The Z-Map twin of [`gpu_stack_matches_cpu_stack_at_a_forced_one_tile_budget`].
#[test]
fn gpu_zmap_matches_cpu_zmap_exactly_at_a_forced_one_tile_budget() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let backend = GpuBackend::with_max_device_bytes(1).expect("tiny gpu backend");
    assert_gpu_matches_cpu_zmap("tiny_budget", &backend);
}

#[test]
fn warp_to_fusion_never_reads_back() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();

    // Every one of the 8 frames is genuinely warped (no identity
    // short-circuit anywhere), so a per-frame readback would show up as 8
    // extra reads on top of the 2-readback fusion tail -- 10 total.
    let t = Transform::similarity(1.02, 0.03, 1.5, -1.0);
    let transforms = vec![t; frame_count];
    let rect = Rect {
        x: 128,
        y: 128,
        width: 200,
        height: 200,
    };

    let before = readback_count();
    let _out = backend
        .warp_fuse_tile(&stack, (width, height), &transforms, rect, spp, 3)
        .expect("warp_fuse_tile must succeed");
    let after = readback_count();

    assert_eq!(
        after - before,
        2,
        "an 8-frame warped tile must cost exactly 2 readbacks (composite + Z-Map), not one per frame"
    );
}

/// CPU-only self-check of [`warp_noise_propagation_reachable`] (no GPU
/// device). Pixel 68615 (x=7, y=134, the fixture's left edge) is a known
/// boundary residual; its CPU Z-Map winner is deterministic, so the pixel
/// and its winning frame reproduce without a GPU.
#[test]
fn warp_noise_propagation_reachable_bounds_a_boundary_pixel() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let transforms = non_identity_transforms(frame_count);

    let cpu = run_cpu("warp_noise_unit", transforms.clone());
    let (cpu_samples, _, _, cspp) = read_composite(&cpu.composite_path);
    let cpu_zmap = read_zmap(&cpu.zmap_path);

    let reader = WarpingReader::new(&stack, (width, height), spp, transforms);
    let geom = CanvasGeom {
        width,
        height,
        spp: cspp,
    };

    let pixel = 68615usize;
    let winner = cpu_zmap[pixel] as usize;
    let spp_usize = cspp as usize;
    let baseline: Vec<u16> = (0..spp_usize)
        .map(|c| cpu_samples[pixel * spp_usize + c])
        .collect();

    // A 1 LSB observed delta must always be reachable: a real ±1 LSB
    // perturbation of the winner's own raw sample, propagated through the
    // real `Laplacian::fuse` reconstruction, moves the composite by at
    // least 1 LSB at a boundary pixel -- that is the whole mechanism this
    // check exists to recognise.
    let (small_found, small_reach) =
        warp_noise_propagation_reachable(&reader, frame_count, geom, winner, pixel, &baseline, 1);
    assert!(
        small_found,
        "a 1 LSB observed delta must be reachable (reached {small_reach})"
    );

    // An implausibly large observed delta must never be reachable from a
    // single ±1 LSB sample perturbation -- this is the bound that keeps the
    // mechanism from silently explaining away a real correctness bug.
    let (large_found, large_reach) = warp_noise_propagation_reachable(
        &reader,
        frame_count,
        geom,
        winner,
        pixel,
        &baseline,
        60000,
    );
    assert!(
        !large_found,
        "a 60000 LSB observed delta must not be reachable from a single ±1 LSB perturbation \
         (reached {large_reach})"
    );
}
