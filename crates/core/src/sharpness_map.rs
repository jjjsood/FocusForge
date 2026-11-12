// SPDX-License-Identifier: GPL-3.0-or-later
//! Sharpness maps on one scale for a whole stack: the depth map's focus score
//! of a pixel divided by [`stack_sharpness_scale`]. Equal values mean equal
//! sharpness in every frame and in a composite. Everything runs tile by tile
//! over [`sharpness_plan`], and no value depends on the tile size.

use std::sync::Mutex;

use rayon::prelude::*;

use crate::algorithm::TileReader;
use crate::fuse::sharpness::DETAIL_REACH;
use crate::gpu::SharpnessBackend;
pub use crate::sharp_areas::sharpness_plan;
use crate::sharp_areas::{
    SCORE_RADIUS, SharpAreasError, eligible_backend, for_frames_of_tile, frame_workers,
    score_frames_of_tile, score_tile,
};
use crate::tile::{PlannedTile, Rect, TileControl, TilePlan};

/// The share of pixels whose peak focus score lies at or below the scale.
pub const SHARPNESS_PERCENTILE: f64 = 0.99;

const BIN_SHIFT: u32 = 12;

/// Histogram bins over the bit patterns of finite non-negative `f32` scores
/// (`to_bits() >> 12`): a bin is 2^-11 wide relative to its value.
pub const BINS: usize = ((f32::MAX.to_bits() >> BIN_SHIFT) + 1) as usize;

/// The most first-pass candidates, as a share of the pixels, that the exact
/// re-check of a device scale recomputes. A candidate re-reads about an 18x18
/// window per frame, about 324 pixels' share of a CPU pass, so 1e-4 keeps the
/// re-check near 3 % of one CPU pass; above it the scale runs on the CPU.
pub const MAX_CANDIDATE_SHARE: f64 = 1e-4;

/// The most passes the re-check takes to settle the deciding bin.
const MAX_RECHECK_PASSES: usize = 64;

// Rounding budget of the detail plane `base - expand(reduce(base))` for 16-bit
// input: reduce is exact and every other operand is below 2^16, where an f32
// ulp is 2^-8.
const ULP_BELOW_2_16: f64 = 1.0 / 256.0;
// One 1D expand rounds at most 5 times on the device (any contraction or
// reassociation, 1 ulp each) and 3 times on the CPU (0.75 * mid and two adds,
// half an ulp each); two passes, then the subtraction.
const EXPAND_ROUNDINGS_DEVICE: f64 = 5.0;
const EXPAND_ROUNDINGS_CPU: f64 = 3.0;
const DETAIL_DEVICE: f64 = (2.0 * EXPAND_ROUNDINGS_DEVICE + 1.0) * ULP_BELOW_2_16;
const DETAIL_CPU: f64 = (2.0 * EXPAND_ROUNDINGS_CPU + 1.0) * ULP_BELOW_2_16 / 2.0;
// The CPU rounds each |L| to a multiple of 2^-20 before summing.
const CPU_QUANTIZATION: f64 = 1.0 / 2_097_152.0;
// One f32 rounding is at most 2^-23 of its result (1 ulp, device adds); the
// CPU's single final conversion is half of it.
const ULP_RELATIVE: f64 = 1.0 / 8_388_608.0;
const HALF_ULP_RELATIVE: f64 = ULP_RELATIVE / 2.0;

/// The bound `a + b * v` on `|device peak - CPU peak|` for CPU peak `v`. It
/// holds when the input is `u16`, every device operation is within 1 ulp,
/// one 1D expand has at most 5 inexact device operations, and the CPU path
/// is not contracted (no fused multiply-add). `a` is the detail error of both
/// paths plus the CPU's `|L|` quantisation over `n = (2 * SCORE_RADIUS +
/// 1)^2 * spp` terms; `b` is the relative error of the two box sums.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeakErrorBound {
    pub a: f64,
    pub b: f64,
}

impl PeakErrorBound {
    pub fn for_spp(spp: u32) -> Self {
        let side = f64::from(2 * SCORE_RADIUS + 1);
        let n = side * side * f64::from(spp);
        // Row sum: side * spp terms; column sum: side terms; each first add is exact.
        let adds = side * f64::from(spp) - 1.0 + (side - 1.0);
        let gamma = adds * ULP_RELATIVE / (1.0 - adds * ULP_RELATIVE);
        Self {
            a: n * (DETAIL_DEVICE + DETAIL_CPU + CPU_QUANTIZATION) * (1.0 + gamma),
            b: gamma * (1.0 + HALF_ULP_RELATIVE) + HALF_ULP_RELATIVE,
        }
    }

    /// The bound at CPU peak `v`.
    pub fn at(&self, v: f64) -> f64 {
        self.a + self.b * v
    }

    #[cfg(test)]
    fn none() -> Self {
        Self { a: 0.0, b: 0.0 }
    }
}

/// `peaks[i] = max(peaks[i], score)` over `interior`, which lies inside
/// `read`: `scores` cover `read` and `peaks` cover `interior`, both row-major.
/// The CPU twin of the device fold kernel.
pub fn fold_peaks(peaks: &mut [f32], scores: &[f32], read: Rect, interior: Rect) {
    for (row, peak_row) in peaks.chunks_exact_mut(interior.width as usize).enumerate() {
        let start = ((interior.y - read.y) as usize + row) * read.width as usize
            + (interior.x - read.x) as usize;
        for (peak, score) in peak_row.iter_mut().zip(&scores[start..]) {
            *peak = peak.max(*score);
        }
    }
}

/// Counts each of `peaks` in its bin of `histogram` (`BINS` counts). The CPU
/// twin of the device bin kernel.
pub fn bin_peaks(histogram: &mut [u64], peaks: &[f32]) {
    for peak in peaks {
        histogram[(peak.to_bits() >> BIN_SHIFT) as usize] += 1;
    }
}

fn scale_rank(pixels: u64) -> u64 {
    ((SHARPNESS_PERCENTILE * pixels as f64).ceil() as u64).max(1)
}

/// The first bin whose running count reaches the rank of the percentile.
fn scale_bin(histogram: &[u64], pixels: u64) -> u32 {
    let rank = scale_rank(pixels);
    let mut seen = 0;
    histogram
        .iter()
        .position(|count| {
            seen += count;
            seen >= rank
        })
        .expect("the histogram holds every pixel, and the rank is at most the pixel count")
        as u32
}

/// The stack-wide sharpness scale: the [`SHARPNESS_PERCENTILE`] of each
/// pixel's peak focus score over `frames` frames of `extent`, read tile by
/// tile of `tile_size`. Taken over per-pixel peaks, it does not move with the
/// frame count. It is the lower edge of its histogram bin (relative width
/// 2^-11), identical at every tile size and admission cap; 0.0 without frames
/// or detail. `control` stops the pass with [`SharpAreasError::Stopped`].
pub fn stack_sharpness_scale(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    tile_size: u32,
    control: &dyn TileControl,
) -> Result<f32, SharpAreasError> {
    let plan = sharpness_plan(extent, tile_size)?;
    if frames == 0 {
        return Ok(0.0);
    }
    let (pool, gate) = frame_workers()?;
    let mut histogram = vec![0u64; BINS];
    for tile in plan.tiles() {
        let (read, interior) = (tile.read(), tile.interior());
        let peaks = Mutex::new(vec![
            0.0f32;
            interior.width as usize * interior.height as usize
        ]);
        score_frames_of_tile(
            &pool,
            &gate,
            reader,
            read,
            spp,
            control,
            None,
            &mut vec![(); frames],
            |(), scores| fold_peaks(&mut peaks.lock().unwrap(), scores, read, interior),
        )?;
        bin_peaks(&mut histogram, &peaks.into_inner().unwrap());
    }
    let pixels = u64::from(extent.0) * u64::from(extent.1);
    Ok(f32::from_bits(scale_bin(&histogram, pixels) << BIN_SHIFT))
}

/// A stack's sharpness scale and how it was found.
#[derive(Debug, Clone, PartialEq)]
pub struct SharpnessScale {
    pub value: f32,
    /// `Some` when the device's peaks decided the scale and the exact
    /// re-check settled it; `None` when the CPU pass produced it.
    pub recheck: Option<Recheck>,
}

/// The re-check of a scale taken from device peaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recheck {
    /// Passes over the deciding bin, the first included.
    pub iterations: usize,
    /// The peaks recomputed exactly on each pass.
    pub candidates: Vec<u64>,
}

/// Why a device scale is not used.
#[derive(Debug)]
enum Decline {
    /// The deciding bin is narrower than the error bound at its upper edge.
    NarrowBin,
    /// The first pass has more candidates than the share limit allows.
    CandidateShare,
    /// The deciding bin did not settle.
    Unsettled,
}

/// The exact peaks of the pixels at the given coordinates.
type PeaksAt<'a> = dyn Fn(&[(u32, u32)]) -> Result<Vec<f32>, SharpAreasError> + 'a;

enum Rechecked {
    Settled { value: f32, recheck: Recheck },
    Declined(Decline),
}

/// [`stack_sharpness_scale`] from `backend`'s peaks: the device folds and bins
/// each pixel's peak, and every peak within [`PeakErrorBound`] of the deciding
/// bin's edges is recomputed exactly, so the value has the CPU pass's bits.
/// The CPU pass runs instead (`recheck` is `None`) below the backend's
/// [`SharpnessBackend::min_pixel_frames`], on decline or device loss, when the
/// bin is narrower than the bound or candidates exceed [`MAX_CANDIDATE_SHARE`].
pub fn stack_sharpness_scale_with_backend(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    tile_size: u32,
    control: &dyn TileControl,
    backend: Option<&dyn SharpnessBackend>,
) -> Result<SharpnessScale, SharpAreasError> {
    scale_with_backend(
        reader,
        frames,
        extent,
        spp,
        tile_size,
        control,
        backend,
        &PeakErrorBound::for_spp(spp),
        MAX_CANDIDATE_SHARE,
    )
}

#[allow(clippy::too_many_arguments)]
fn scale_with_backend(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    tile_size: u32,
    control: &dyn TileControl,
    backend: Option<&dyn SharpnessBackend>,
    bound: &PeakErrorBound,
    max_share: f64,
) -> Result<SharpnessScale, SharpAreasError> {
    let eligible = eligible_backend(backend, frames, extent)
        .filter(|backend| frames > 0 && !backend.device_lost());
    if let Some(backend) = eligible
        && let Some(scale) = device_scale(
            reader, frames, extent, spp, tile_size, control, backend, bound, max_share,
        )?
    {
        return Ok(scale);
    }
    let value = stack_sharpness_scale(reader, frames, extent, spp, tile_size, control)?;
    Ok(SharpnessScale {
        value,
        recheck: None,
    })
}

/// A backend error after which its device is lost leaves the scale to the
/// CPU (`Ok(None)`); any other error fails the run.
fn lost_or_fail(
    backend: &dyn SharpnessBackend,
    error: SharpAreasError,
) -> Result<Option<SharpnessScale>, SharpAreasError> {
    match error {
        SharpAreasError::Read(error) if backend.device_lost() => {
            tracing::info!(%error, "device lost; the sharpness scale finishes on the CPU");
            Ok(None)
        }
        other => Err(other),
    }
}

/// The scale from `backend`'s session, or `Ok(None)` where the CPU pass takes over.
#[allow(clippy::too_many_arguments)]
fn device_scale(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    tile_size: u32,
    control: &dyn TileControl,
    backend: &dyn SharpnessBackend,
    bound: &PeakErrorBound,
    max_share: f64,
) -> Result<Option<SharpnessScale>, SharpAreasError> {
    let plan = sharpness_plan(extent, tile_size)?;
    let mut session = match backend.open_scale(&plan, spp) {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(None),
        Err(error) => return lost_or_fail(backend, error.into()),
    };
    let (pool, gate) = frame_workers()?;
    for (index, tile) in plan.tiles().iter().enumerate() {
        if let Err(error) = for_frames_of_tile(
            &pool,
            &gate,
            control,
            tile.read(),
            &mut vec![(); frames],
            |frame, (), _| Ok(session.fold(index, reader, frame)?),
        ) {
            return lost_or_fail(backend, error);
        }
    }
    let histogram = match session.histogram() {
        Ok(histogram) if !backend.device_lost() => histogram,
        Ok(_) => return Ok(None),
        Err(error) => return lost_or_fail(backend, error.into()),
    };
    let pixels = u64::from(extent.0) * u64::from(extent.1);
    let outcome = recheck_scale(
        &histogram,
        &plan,
        pixels,
        bound,
        max_share,
        &|at| exact_peaks(reader, frames, extent, spp, &pool, control, at),
        &mut |tile| Ok(session.interior_peaks(tile)?),
    );
    match outcome {
        Ok(Rechecked::Settled { value, recheck }) if !backend.device_lost() => {
            Ok(Some(SharpnessScale {
                value,
                recheck: Some(recheck),
            }))
        }
        Ok(Rechecked::Settled { .. }) => Ok(None),
        Ok(Rechecked::Declined(reason)) => {
            tracing::info!(
                ?reason,
                "device peaks not used; the sharpness scale runs on the CPU"
            );
            Ok(None)
        }
        Err(error) => lost_or_fail(backend, error),
    }
}

/// The scale the CPU pass gives, decided from `histogram` of device peaks:
/// the deciding bin's edges are exact counts once every peak within `bound`
/// of them is recomputed by `exact`. `peaks_of(tile)` gives one tile's
/// interior peaks at a time. The bin moves one step while its edges do not
/// bracket the rank.
fn recheck_scale(
    histogram: &[u64],
    plan: &TilePlan,
    pixels: u64,
    bound: &PeakErrorBound,
    max_share: f64,
    exact: &PeaksAt,
    peaks_of: &mut dyn FnMut(usize) -> Result<Vec<f32>, SharpAreasError>,
) -> Result<Rechecked, SharpAreasError> {
    let rank = scale_rank(pixels);
    let mut bin = scale_bin(histogram, pixels);
    let mut candidates = Vec::new();
    for pass in 1..=MAX_RECHECK_PASSES {
        let edge = |b: u32| f64::from(f32::from_bits(b << BIN_SHIFT));
        let (lo, hi) = (edge(bin), edge(bin + 1));
        let (lo_eps, hi_eps) = (bound.at(lo), bound.at(hi));
        if pass == 1 && hi - lo < hi_eps {
            return Ok(Rechecked::Declined(Decline::NarrowBin));
        }
        let (mut below_lo, mut below_hi) = (0u64, 0u64);
        let mut unsure: Vec<(u32, u32)> = Vec::new();
        for (index, tile) in plan.tiles().iter().enumerate() {
            let interior = tile.interior();
            for (k, &g) in peaks_of(index)?.iter().enumerate() {
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
        }
        if pass == 1 && unsure.len() as f64 > max_share * pixels as f64 {
            return Ok(Rechecked::Declined(Decline::CandidateShare));
        }
        for c in exact(&unsure)? {
            below_lo += u64::from(f64::from(c) < lo);
            below_hi += u64::from(f64::from(c) < hi);
        }
        candidates.push(unsure.len() as u64);
        if below_lo >= rank {
            let Some(lower) = bin.checked_sub(1) else {
                break;
            };
            bin = lower;
        } else if below_hi < rank {
            bin += 1;
        } else {
            return Ok(Rechecked::Settled {
                value: f32::from_bits(bin << BIN_SHIFT),
                recheck: Recheck {
                    iterations: pass,
                    candidates,
                },
            });
        }
    }
    Ok(Rechecked::Declined(Decline::Unsettled))
}

/// The peak over `frames` frames of each pixel of `at`, computed in a small
/// window with the CPU path. The window origin is even so the 2:1
/// decimation grid is the tiled path's, and it reaches `DETAIL_REACH +
/// SCORE_RADIUS` px past the pixel on each side, clamped to `extent`.
fn exact_peaks(
    reader: &dyn TileReader,
    frames: usize,
    (width, height): (u32, u32),
    spp: u32,
    pool: &rayon::ThreadPool,
    control: &dyn TileControl,
    at: &[(u32, u32)],
) -> Result<Vec<f32>, SharpAreasError> {
    let reach = DETAIL_REACH + SCORE_RADIUS;
    pool.install(|| {
        at.par_iter()
            .map_init(Vec::new, |samples, &(x, y)| {
                if control.should_stop() {
                    return Err(SharpAreasError::Stopped);
                }
                let (x0, y0) = (x.saturating_sub(reach) & !1, y.saturating_sub(reach) & !1);
                let read = Rect {
                    x: x0,
                    y: y0,
                    width: (x + reach + 1).min(width) - x0,
                    height: (y + reach + 1).min(height) - y0,
                };
                let index = ((y - y0) * read.width + (x - x0)) as usize;
                let mut peak = 0.0f32;
                for frame in 0..frames {
                    peak = peak.max(score_tile(reader, frame, read, spp, samples)?[index]);
                }
                Ok(peak)
            })
            .collect()
    })
}

/// The sharpness map of `frame` over `tile`'s interior, row-major: each
/// pixel's focus score divided by `scale`. A value above 1.0 is sharper than
/// the scale and is kept. A `scale` that is not positive gives all zeros.
///
/// A composite image is mapped by calling this on a reader of the composite
/// (frame 0) with the frames' scale.
pub fn sharpness_map_tile(
    reader: &dyn TileReader,
    frame: usize,
    tile: &PlannedTile,
    spp: u32,
    scale: f32,
) -> Result<Vec<f32>, SharpAreasError> {
    let (read, interior) = (tile.read(), tile.interior());
    let scores = score_tile(reader, frame, read, spp, &mut Vec::new())?;
    Ok(interior_map(&scores, read, interior, scale))
}

/// The scores over `interior`, which lies inside `read`, divided by `scale`
/// (zeros for a `scale` that is not positive).
fn interior_map(scores: &[f32], read: Rect, interior: Rect, scale: f32) -> Vec<f32> {
    let mut map = Vec::with_capacity(interior.width as usize * interior.height as usize);
    for y in interior.y..interior.y + interior.height {
        let start = ((y - read.y) * read.width + (interior.x - read.x)) as usize;
        let row = &scores[start..start + interior.width as usize];
        map.extend(
            row.iter()
                .map(|score| if scale > 0.0 { score / scale } else { 0.0 }),
        );
    }
    map
}

/// The sharpness maps of `frames` frames of `extent` as [`sharpness_map_tile`]
/// gives them, computed tile by tile of `tile_size`: the frames of one tile
/// are scored in parallel (by `backend` when the run reaches its
/// [`SharpnessBackend::min_pixel_frames`]) and `write(frame, interior, map)`
/// is called for each frame in frame order before the next tile. `control`
/// stops the run with [`SharpAreasError::Stopped`].
#[allow(clippy::too_many_arguments)]
pub fn sharpness_maps<E: From<SharpAreasError>>(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    scale: f32,
    tile_size: u32,
    control: &dyn TileControl,
    backend: Option<&dyn SharpnessBackend>,
    mut write: impl FnMut(usize, Rect, &[f32]) -> Result<(), E>,
) -> Result<(), E> {
    let plan = sharpness_plan(extent, tile_size).map_err(SharpAreasError::from)?;
    let backend = eligible_backend(backend, frames, extent);
    let (pool, gate) = frame_workers().map_err(SharpAreasError::from)?;
    let mut maps = vec![Vec::new(); frames];
    for tile in plan.tiles() {
        let (read, interior) = (tile.read(), tile.interior());
        score_frames_of_tile(
            &pool,
            &gate,
            reader,
            read,
            spp,
            control,
            backend,
            &mut maps,
            |map, scores| *map = interior_map(scores, read, interior, scale),
        )?;
        for (frame, map) in maps.iter().enumerate() {
            write(frame, interior, map)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::algorithm::{AlgorithmError, TileReader};
    use crate::fuse::laplacian::score_plane;
    use crate::fuse::sharpness::DETAIL_REACH;
    use crate::fuse::sharpness::detail_plane;
    use crate::job::JobControl;
    use crate::sharp_areas::double;
    use crate::sharp_areas::{SCORE_RADIUS, SharpAreasError};
    use crate::tile::{NoControl, Rect, TileControl};

    struct Stack {
        width: u32,
        height: u32,
        spp: u32,
        frames: Vec<Vec<u16>>,
        reads: AtomicUsize,
    }

    impl TileReader for Stack {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let row_len = rect.width as usize * self.spp as usize;
            for (row, chunk) in out.chunks_exact_mut(row_len).enumerate() {
                let start = ((rect.y as usize + row) * self.width as usize + rect.x as usize)
                    * self.spp as usize;
                chunk.copy_from_slice(&self.frames[frame][start..start + row_len]);
            }
            Ok(())
        }
    }

    fn texture(x: u32, y: u32) -> i32 {
        let h =
            (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)).wrapping_mul(2_246_822_519);
        i32::from((h >> 16) as u16 | 0x4000)
    }

    /// Grey 30 000 plus texture scaled by `amplitude(x, y)` in `0..=1`; each
    /// channel of a pixel carries its own texture.
    fn plane(w: u32, h: u32, spp: u32, amplitude: impl Fn(u32, u32) -> f32) -> Vec<u16> {
        let mut samples = Vec::new();
        for y in 0..h {
            for x in 0..w {
                for c in 0..spp {
                    let swing = (texture(x + c * 7919, y) - 40_000) as f32 * amplitude(x, y);
                    samples.push((30_000.0 + swing).round() as u16);
                }
            }
        }
        samples
    }

    fn stack(w: u32, h: u32, spp: u32, frames: Vec<Vec<u16>>) -> Stack {
        Stack {
            width: w,
            height: h,
            spp,
            frames,
            reads: AtomicUsize::new(0),
        }
    }

    /// Frame `k`'s amplitude steps with the column band and `k`, so the
    /// frame that is sharpest changes along the image.
    fn graded(n: usize, w: u32, h: u32, spp: u32) -> Stack {
        let frames = (0..n)
            .map(|k| {
                plane(w, h, spp, |x, _| {
                    (1 + (x / 16 + k as u32) % (n as u32 + 1)) as f32 / (n as f32 + 2.0)
                })
            })
            .collect();
        stack(w, h, spp, frames)
    }

    fn whole_scores(stack: &Stack, frame: usize) -> Vec<f32> {
        let detail =
            detail_plane(&stack.frames[frame], stack.width, stack.height, stack.spp).unwrap();
        score_plane(&detail, SCORE_RADIUS)
    }

    /// The scale from whole-frame scores: sort the per-pixel peaks' bins and
    /// pick the percentile's rank.
    fn oracle_scale(stack: &Stack) -> f32 {
        let mut peaks = vec![0.0f32; (stack.width * stack.height) as usize];
        for frame in 0..stack.frames.len() {
            for (peak, score) in peaks.iter_mut().zip(whole_scores(stack, frame)) {
                *peak = peak.max(score);
            }
        }
        let mut bins: Vec<u32> = peaks.iter().map(|p| p.to_bits() >> 12).collect();
        bins.sort_unstable();
        let rank = ((0.99 * bins.len() as f64).ceil() as usize).max(1);
        f32::from_bits(bins[rank - 1] << 12)
    }

    fn scale_at(stack: &Stack, tile: u32, control: &dyn TileControl) -> f32 {
        stack_sharpness_scale(
            stack,
            stack.frames.len(),
            (stack.width, stack.height),
            stack.spp,
            tile,
            control,
        )
        .unwrap()
    }

    /// `frame`'s map stitched from the plan's tiles.
    fn stitched(stack: &Stack, frame: usize, tile: u32, scale: f32) -> Vec<f32> {
        let plan = sharpness_plan((stack.width, stack.height), tile).unwrap();
        let mut map = vec![f32::NAN; (stack.width * stack.height) as usize];
        for planned in plan.tiles() {
            let values = sharpness_map_tile(stack, frame, planned, stack.spp, scale).unwrap();
            let rect = planned.interior();
            for (row, chunk) in values.chunks_exact(rect.width as usize).enumerate() {
                let start = (rect.y as usize + row) * stack.width as usize + rect.x as usize;
                map[start..start + chunk.len()].copy_from_slice(chunk);
            }
        }
        map
    }

    fn bits(values: &[f32]) -> Vec<u32> {
        values.iter().map(|v| v.to_bits()).collect()
    }

    #[test]
    fn stack_sharpness_scale_matches_whole_frame_percentile() {
        for (w, h) in [(101, 67), (96, 80)] {
            for spp in [1, 3] {
                for n in [1, 2, 5] {
                    let stack = graded(n, w, h, spp);
                    let wanted = oracle_scale(&stack);
                    let at = format!("{n} frames, {w}x{h}, spp {spp}");
                    assert!(wanted > 0.0, "{at}: the stack has detail");
                    assert_eq!(wanted.to_bits() & 0xFFF, 0, "{at}: a bin's lower edge");
                    let got = scale_at(&stack, 80, &NoControl);
                    assert_eq!(got.to_bits(), wanted.to_bits(), "{at}: {got} vs {wanted}");
                }
            }
        }
    }

    /// `sharpness_maps` over `stack`, stitched per frame.
    fn mapped(
        stack: &Stack,
        tile: u32,
        scale: f32,
        backend: Option<&dyn SharpnessBackend>,
    ) -> Result<Vec<Vec<f32>>, SharpAreasError> {
        let (w, h) = (stack.width as usize, stack.height as usize);
        let mut maps = vec![vec![f32::NAN; w * h]; stack.frames.len()];
        sharpness_maps(
            stack,
            stack.frames.len(),
            (stack.width, stack.height),
            stack.spp,
            scale,
            tile,
            &NoControl,
            backend,
            |frame, rect: Rect, values: &[f32]| -> Result<(), SharpAreasError> {
                for (row, chunk) in values.chunks_exact(rect.width as usize).enumerate() {
                    let start = (rect.y as usize + row) * w + rect.x as usize;
                    maps[frame][start..start + chunk.len()].copy_from_slice(chunk);
                }
                Ok(())
            },
        )?;
        Ok(maps)
    }

    fn frame_tiles(stack: &Stack, tile: u32) -> usize {
        stack.frames.len()
            * sharpness_plan((stack.width, stack.height), tile)
                .unwrap()
                .tiles()
                .len()
    }

    fn assert_maps_close(got: &[Vec<f32>], want: &[Vec<f32>], at: &str) {
        for (frame, (got, want)) in got.iter().zip(want).enumerate() {
            for (i, (g, w)) in got.iter().zip(want).enumerate() {
                assert!(
                    (g - w).abs() <= 4e-6 * w.abs(),
                    "{at}: frame {frame} value {i}: {g} vs {w}"
                );
            }
        }
    }

    #[test]
    fn sharpness_backend_maps_match_the_per_tile_oracle() {
        for spp in [1, 3] {
            let stack = graded(3, 101, 67, spp);
            let scale = scale_at(&stack, 80, &NoControl);
            let want: Vec<Vec<f32>> = (0..3).map(|f| stitched(&stack, f, 80, scale)).collect();
            let at = format!("spp {spp}");
            assert_eq!(
                mapped(&stack, 80, scale, None).unwrap(),
                want,
                "{at}: the CPU maps"
            );
            let double = double::Double::default();
            let got = mapped(&stack, 80, scale, Some(&double)).unwrap();
            assert_maps_close(&got, &want, &at);
            assert_eq!(double.calls.load(Ordering::SeqCst), frame_tiles(&stack, 80));
            let zero = mapped(&stack, 80, 0.0, Some(&double)).unwrap();
            assert!(zero.iter().flatten().all(|v| *v == 0.0), "{at}: scale 0");
        }
    }

    #[test]
    fn sharpness_backend_maps_are_not_offered_work_under_its_threshold() {
        let stack = graded(3, 101, 67, 1);
        let scale = scale_at(&stack, 80, &NoControl);
        let double = double::Double {
            min_pixel_frames: 3 * 101 * 67 + 1,
            ..Default::default()
        };
        let got = mapped(&stack, 80, scale, Some(&double)).unwrap();
        assert_eq!(got, mapped(&stack, 80, scale, None).unwrap());
        assert_eq!(double.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn sharpness_backend_maps_finish_on_the_cpu_after_declines_and_losses() {
        for spp in [1, 3] {
            let stack = graded(3, 101, 67, spp);
            let scale = scale_at(&stack, 80, &NoControl);
            let want = mapped(&stack, 80, scale, None).unwrap();
            let declining = double::Double {
                decline_every_second: true,
                ..Default::default()
            };
            let got = mapped(&stack, 80, scale, Some(&declining)).unwrap();
            assert_maps_close(&got, &want, "declining");
            let losing = double::Double {
                fail_from: Some(3),
                fail_loses_device: true,
                ..Default::default()
            };
            let got = mapped(&stack, 80, scale, Some(&losing)).unwrap();
            assert_maps_close(&got, &want, "losing");
            assert_eq!(losing.calls.load(Ordering::SeqCst), 3);
            assert_eq!(losing.calls_after_loss.load(Ordering::SeqCst), 0);
            let failing = double::Double {
                fail_from: Some(3),
                ..Default::default()
            };
            let result = mapped(&stack, 80, scale, Some(&failing));
            assert!(
                matches!(result, Err(SharpAreasError::Read(_))),
                "{result:?}"
            );
        }
    }

    #[test]
    fn sharpness_backend_counts_on_three_channels_match_the_cpu() {
        let stack = graded(3, 101, 67, 3);
        let count = |backend: Option<&dyn SharpnessBackend>| {
            crate::sharp_areas::count_sharp_areas_reporting_with_backend(
                &stack,
                3,
                (101, 67),
                3,
                &crate::params::ParamValues::new(),
                80,
                &NoControl,
                &mut |_| {},
                backend,
            )
            .unwrap()
        };
        let double = double::Double::default();
        assert_eq!(count(Some(&double)), count(None));
        assert_eq!(double.calls.load(Ordering::SeqCst), frame_tiles(&stack, 80));
    }

    #[test]
    fn sharpness_maps_do_not_depend_on_tiling() {
        // A tile's interior is the tile size minus twice the reach, so tiles
        // of 96 and 112 cut 160x160 and 192x96 into equal 80 and 96 px interiors.
        let cases: [(u32, u32, u32, &[u32]); 3] = [
            (101, 67, 1, &[80, 96, 128]),
            (160, 160, 1, &[96, 80]),
            (192, 96, 3, &[112, 80]),
        ];
        for (w, h, spp, tiles) in cases {
            let stack = graded(3, w, h, spp);
            if w != 101 {
                let plan = sharpness_plan((w, h), tiles[0]).unwrap();
                let side = tiles[0] - 2 * (DETAIL_REACH + SCORE_RADIUS);
                assert!(plan.tiles().iter().all(|t| {
                    let r = t.interior();
                    r.width == side && r.height == side.min(h)
                }));
            }
            let one_tile = scale_at(&stack, 1000, &NoControl);
            let whole: Vec<f32> = whole_scores(&stack, 1)
                .iter()
                .map(|score| score / one_tile)
                .collect();
            for &tile in tiles {
                let at = format!("{w}x{h}, spp {spp}, tile {tile}");
                let scale = scale_at(&stack, tile, &NoControl);
                assert_eq!(scale.to_bits(), one_tile.to_bits(), "scale at {at}");
                let map = stitched(&stack, 1, tile, one_tile);
                assert_eq!(bits(&map), bits(&whole), "map at {at}");
            }
            let map = stitched(&stack, 1, 1000, one_tile);
            assert_eq!(bits(&map), bits(&whole), "map in one tile, {w}x{h}");
            for cap in [1, 4] {
                let control = JobControl::new();
                control.set_admission_cap(Some(cap));
                let scale = scale_at(&stack, tiles[0], &*control);
                assert_eq!(
                    scale.to_bits(),
                    one_tile.to_bits(),
                    "{w}x{h} scale at cap {cap}"
                );
            }
        }
    }

    #[test]
    fn frame_sharpness_map_is_score_over_stack_scale() {
        let (w, h) = (101, 67);
        let half = |gain: f32| move |x: u32, _| if x < w / 2 { gain } else { 0.0 };
        let stack = stack(
            w,
            h,
            1,
            vec![plane(w, h, 1, half(1.0)), plane(w, h, 1, half(0.25))],
        );
        let scale = scale_at(&stack, 80, &NoControl);
        assert!(scale > 0.0);
        let sharp = stitched(&stack, 0, 80, scale);
        let blurred = stitched(&stack, 1, 80, scale);
        let score = whole_scores(&stack, 0);
        assert_eq!(
            sharp[20 * w as usize + 20],
            score[20 * w as usize + 20] / scale
        );
        let mean = |map: &[f32]| {
            let region: Vec<f32> = (0..h)
                .flat_map(|y| (8..w / 2 - 8).map(move |x| (y * w + x) as usize))
                .map(|i| map[i])
                .collect();
            region.iter().sum::<f32>() / region.len() as f32
        };
        assert!(
            mean(&sharp) > 2.0 * mean(&blurred),
            "{} vs {}",
            mean(&sharp),
            mean(&blurred)
        );
        for map in [&sharp, &blurred] {
            for y in 0..h {
                for x in w / 2 + 12..w {
                    assert_eq!(map[(y * w + x) as usize], 0.0, "uniform region at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn composite_sharpness_map_uses_the_stack_scale() {
        let (w, h) = (101, 67);
        let frames: Vec<Vec<u16>> = (0..3)
            .map(|k| plane(w, h, 1, move |x, _| 0.15 * (1 + (x / 16 + k) % 3) as f32))
            .collect();
        let frames = stack(w, h, 1, frames);
        let scale = scale_at(&frames, 80, &NoControl);
        let above_one =
            |map: &[f32]| map.iter().filter(|v| **v > 1.0).count() as f32 / map.len() as f32;

        let same = stack(w, h, 1, vec![frames.frames[2].clone()]);
        assert_eq!(
            bits(&stitched(&same, 0, 80, scale)),
            bits(&stitched(&frames, 2, 80, scale)),
            "a composite identical to frame 2"
        );

        let sharper = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 1.0)]);
        let composite = stitched(&sharper, 0, 80, scale);
        assert!(above_one(&composite) > 0.9, "{}", above_one(&composite));
        for k in 0..3 {
            let share = above_one(&stitched(&frames, k, 80, scale));
            assert!(share < 0.05, "frame {k} reads above 1.0 in {share}");
        }
    }

    struct StopAfter {
        units: usize,
        done: AtomicUsize,
    }

    impl TileControl for StopAfter {
        fn should_stop(&self) -> bool {
            self.done.load(Ordering::SeqCst) >= self.units
        }

        fn unit_completed(&self, _: u64) {
            self.done.fetch_add(1, Ordering::SeqCst);
        }

        fn admission_cap(&self) -> usize {
            1
        }
    }

    #[test]
    fn sharpness_map_flat_stack_and_stop() {
        let (w, h) = (101, 67);
        let flat = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 0.0); 2]);
        assert_eq!(scale_at(&flat, 80, &NoControl), 0.0);
        let none = stack_sharpness_scale(&flat, 0, (w, h), 1, 80, &NoControl).unwrap();
        assert_eq!(none, 0.0, "a stack without frames");
        for map in [stitched(&flat, 0, 80, 0.0), {
            let textured = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 1.0)]);
            stitched(&textured, 0, 80, 0.0)
        }] {
            assert!(map.iter().all(|v| *v == 0.0), "scale 0 gives zeros");
        }

        for units in [0, 1] {
            let stack = graded(3, w, h, 1);
            let control = StopAfter {
                units,
                done: AtomicUsize::new(0),
            };
            let result = stack_sharpness_scale(&stack, 3, (w, h), 1, 80, &control);
            assert!(
                matches!(result, Err(SharpAreasError::Stopped)),
                "{result:?}"
            );
            assert_eq!(
                stack.reads.load(Ordering::SeqCst),
                units,
                "reads after the stop"
            );
        }
    }

    use std::sync::atomic::AtomicBool;

    use crate::gpu::ScalePeaks;
    use crate::io::{FrameError, TiffError};
    use crate::tile::TilePlan;

    /// How a test session reports the exact peaks it folded.
    #[derive(Default, Clone, Copy, PartialEq, Debug)]
    enum Push {
        #[default]
        Exact,
        /// Every peak `c` at `c + ε(c)`.
        Up,
        /// Every peak `c` at `c - ε(c)`.
        Down,
    }

    /// `c` moved by `delta` as far as an `f32` goes without leaving `delta`.
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

    /// A [`SharpnessBackend`] whose scale session folds the CPU scores of each
    /// tile and reports them as `push` says, with scripted declines and failures.
    #[derive(Default)]
    struct ScaleDouble {
        min_pixel_frames: u64,
        push: Push,
        decline_open: bool,
        /// From this fold call (1-based) on, every fold fails.
        fail_fold_from: Option<usize>,
        fail_loses_device: bool,
        opened: AtomicUsize,
        folds: AtomicUsize,
        lost: AtomicBool,
    }

    impl SharpnessBackend for ScaleDouble {
        fn min_pixel_frames(&self) -> u64 {
            self.min_pixel_frames
        }

        fn device_lost(&self) -> bool {
            self.lost.load(Ordering::SeqCst)
        }

        fn score_tile(
            &self,
            _: &dyn TileReader,
            _: usize,
            _: Rect,
            _: u32,
        ) -> Result<Option<Vec<f32>>, AlgorithmError> {
            Ok(None)
        }

        fn open_scale(
            &self,
            plan: &TilePlan,
            spp: u32,
        ) -> Result<Option<Box<dyn ScalePeaks + '_>>, AlgorithmError> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            if self.decline_open {
                return Ok(None);
            }
            let tiles: Vec<(Rect, Rect)> = plan
                .tiles()
                .iter()
                .map(|tile| (tile.read(), tile.interior()))
                .collect();
            let peaks = tiles
                .iter()
                .map(|(_, interior)| {
                    Mutex::new(vec![
                        0.0;
                        interior.width as usize * interior.height as usize
                    ])
                })
                .collect();
            Ok(Some(Box::new(ScaleSession {
                owner: self,
                tiles,
                peaks,
                spp,
                bound: PeakErrorBound::for_spp(spp),
            })))
        }
    }

    struct ScaleSession<'a> {
        owner: &'a ScaleDouble,
        tiles: Vec<(Rect, Rect)>,
        peaks: Vec<Mutex<Vec<f32>>>,
        spp: u32,
        bound: PeakErrorBound,
    }

    impl ScaleSession<'_> {
        fn reported(&self, tile: usize) -> Vec<f32> {
            let eps = |c: f32| self.bound.at(f64::from(c));
            let peaks = self.peaks[tile].lock().unwrap();
            peaks
                .iter()
                .map(|&c| match self.owner.push {
                    Push::Exact => c,
                    Push::Up => pushed(c, eps(c)),
                    Push::Down => pushed(c, -eps(c)),
                })
                .collect()
        }
    }

    impl ScalePeaks for ScaleSession<'_> {
        fn fold(
            &self,
            tile: usize,
            reader: &dyn TileReader,
            frame: usize,
        ) -> Result<(), AlgorithmError> {
            let call = self.owner.folds.fetch_add(1, Ordering::SeqCst) + 1;
            if self.owner.fail_fold_from.is_some_and(|from| call >= from) {
                self.owner
                    .lost
                    .store(self.owner.fail_loses_device, Ordering::SeqCst);
                return Err(AlgorithmError::Frame(FrameError::Tiff(
                    TiffError::BufferSize {
                        expected: 0,
                        got: 1,
                    },
                )));
            }
            let (read, interior) = self.tiles[tile];
            let scores =
                score_tile(reader, frame, read, self.spp, &mut Vec::new()).map_err(|error| {
                    match error {
                        SharpAreasError::Read(read) => read,
                        other => panic!("the double reads a valid tile: {other}"),
                    }
                })?;
            fold_peaks(
                &mut self.peaks[tile].lock().unwrap(),
                &scores,
                read,
                interior,
            );
            Ok(())
        }

        fn histogram(&mut self) -> Result<Vec<u64>, AlgorithmError> {
            let mut histogram = vec![0u64; BINS];
            for tile in 0..self.tiles.len() {
                bin_peaks(&mut histogram, &self.reported(tile));
            }
            Ok(histogram)
        }

        fn interior_peaks(&self, tile: usize) -> Result<Vec<f32>, AlgorithmError> {
            Ok(self.reported(tile))
        }
    }

    /// `stack_sharpness_scale_with_backend` with an explicit error bound and
    /// candidate share.
    fn scale_with(
        stack: &Stack,
        backend: &ScaleDouble,
        bound: &PeakErrorBound,
        max_share: f64,
    ) -> Result<SharpnessScale, SharpAreasError> {
        scale_with_backend(
            stack,
            stack.frames.len(),
            (stack.width, stack.height),
            stack.spp,
            80,
            &NoControl,
            Some(backend),
            bound,
            max_share,
        )
    }

    fn public_scale(
        stack: &Stack,
        backend: &ScaleDouble,
        control: &dyn TileControl,
    ) -> Result<SharpnessScale, SharpAreasError> {
        stack_sharpness_scale_with_backend(
            stack,
            stack.frames.len(),
            (stack.width, stack.height),
            stack.spp,
            80,
            control,
            Some(backend),
        )
    }

    /// Whether the bin `scale` opens is wider than the error bound at its upper edge.
    fn bin_is_wide(scale: f32, bound: &PeakErrorBound) -> bool {
        let hi = f64::from(f32::from_bits(
            ((scale.to_bits() >> BIN_SHIFT) + 1) << BIN_SHIFT,
        ));
        hi - f64::from(scale) >= bound.at(hi)
    }

    /// Stacks on which the device path runs: the matrix the re-check tests share.
    fn matrix() -> Vec<(String, Stack)> {
        let mut out = Vec::new();
        for spp in [1, 3] {
            for n in [2, 3, 5] {
                for (w, h) in [(101, 67), (96, 80), (123, 59)] {
                    out.push((
                        format!("{n} frames, {w}x{h}, spp {spp}"),
                        graded(n, w, h, spp),
                    ));
                }
            }
        }
        out
    }

    #[test]
    fn scale_recheck_error_bound_follows_the_derivation() {
        let bound = PeakErrorBound::for_spp(3);
        assert!((bound.a - 13.7638).abs() < 5e-5, "A = {}", bound.a);
        assert!((bound.b - 4.1127e-6).abs() < 5e-11, "B = {}", bound.b);
        assert_eq!(bound.at(1000.0), bound.a + bound.b * 1000.0);
        let one = PeakErrorBound::for_spp(1);
        assert!(
            (one.a / bound.a - 1.0 / 3.0).abs() < 1e-4,
            "n scales with spp"
        );
    }

    #[test]
    fn scale_recheck_bits_equal_the_shipped_scale_under_the_worst_case_error() {
        let (mut moved, mut with_candidates, mut differs_unchecked) = (0, 0, 0);
        for (at, stack) in matrix() {
            let shipped = scale_at(&stack, 80, &NoControl);
            let bound = PeakErrorBound::for_spp(stack.spp);
            assert!(bin_is_wide(shipped, &bound), "{at}: the bin clears ε");
            for push in [Push::Exact, Push::Up, Push::Down] {
                let backend = ScaleDouble {
                    push,
                    ..Default::default()
                };
                let got = scale_with(&stack, &backend, &bound, 1.0).unwrap();
                assert_eq!(got.value.to_bits(), shipped.to_bits(), "{at}, {push:?}");
                let recheck = got.recheck.expect("the device path ran");
                moved += usize::from(recheck.iterations > 1);
                with_candidates += usize::from(recheck.candidates[0] > 0);
                let off = scale_with(&stack, &backend, &PeakErrorBound::none(), 1.0).unwrap();
                assert_eq!(off.recheck.unwrap().candidates, [0], "nothing is rechecked");
                let exact_unchecked = off.value.to_bits() == shipped.to_bits();
                assert!(exact_unchecked || push != Push::Exact, "{at}: exact peaks");
                differs_unchecked += usize::from(!exact_unchecked);
            }
        }
        assert!(moved > 0, "a deciding bin moved in some case");
        assert!(with_candidates > 0, "some case had candidates");
        assert!(
            differs_unchecked > 0,
            "without the re-check a push changes the scale"
        );
    }

    #[test]
    fn scale_recheck_single_pixel_window_equals_the_tiled_peak() {
        let (pool, _) = frame_workers().unwrap();
        for spp in [1, 3] {
            let stack = graded(3, 101, 67, spp);
            for tile in [80, 112] {
                let plan = sharpness_plan((101, 67), tile).unwrap();
                for planned in plan.tiles() {
                    let (read, interior) = (planned.read(), planned.interior());
                    let mut peaks = vec![0.0f32; (interior.width * interior.height) as usize];
                    for frame in 0..3 {
                        let scores = score_tile(&stack, frame, read, spp, &mut Vec::new()).unwrap();
                        fold_peaks(&mut peaks, &scores, read, interior);
                    }
                    let at: Vec<(u32, u32)> = (0..peaks.len() as u32)
                        .map(|k| {
                            (
                                interior.x + k % interior.width,
                                interior.y + k / interior.width,
                            )
                        })
                        .collect();
                    let got =
                        exact_peaks(&stack, 3, (101, 67), spp, &pool, &NoControl, &at).unwrap();
                    assert_eq!(
                        bits(&got),
                        bits(&peaks),
                        "spp {spp}, tile {tile}, {interior:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn scale_recheck_runs_the_cpu_pass_in_every_ineligible_case() {
        let (w, h) = (101, 67);
        let check = |stack: &Stack, backend: &ScaleDouble, at: &str| {
            let got = public_scale(stack, backend, &NoControl).unwrap();
            let shipped = scale_at(stack, 80, &NoControl);
            assert_eq!(
                got.value.to_bits(),
                shipped.to_bits(),
                "{at}: the CPU scale"
            );
            assert!(got.recheck.is_none(), "{at}: no device result is used");
            got.value
        };
        let graded = graded(3, w, h, 1);

        let under = ScaleDouble {
            min_pixel_frames: 3 * 101 * 67 + 1,
            ..Default::default()
        };
        check(&graded, &under, "below the threshold");
        assert_eq!(under.opened.load(Ordering::SeqCst), 0, "never offered");

        let flat = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 0.0); 2]);
        let backend = ScaleDouble::default();
        assert_eq!(check(&flat, &backend, "a flat stack"), 0.0);
        assert!(
            backend.folds.load(Ordering::SeqCst) > 0,
            "the device pass ran"
        );

        let dim = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 0.01); 2]);
        let narrow = check(&dim, &ScaleDouble::default(), "a narrow bin");
        assert!(narrow > 0.0 && !bin_is_wide(narrow, &PeakErrorBound::for_spp(1)));

        let declining = ScaleDouble {
            decline_open: true,
            ..Default::default()
        };
        check(&graded, &declining, "a declined session");
        assert_eq!(declining.folds.load(Ordering::SeqCst), 0);

        let losing = ScaleDouble {
            fail_fold_from: Some(2),
            fail_loses_device: true,
            ..Default::default()
        };
        check(&graded, &losing, "a lost device");

        let lost_before = ScaleDouble::default();
        lost_before.lost.store(true, Ordering::SeqCst);
        check(&graded, &lost_before, "a device lost earlier");
        assert_eq!(lost_before.opened.load(Ordering::SeqCst), 0);

        let failing = ScaleDouble {
            fail_fold_from: Some(2),
            ..Default::default()
        };
        let result = public_scale(&graded, &failing, &NoControl);
        assert!(
            matches!(result, Err(SharpAreasError::Read(_))),
            "{result:?}"
        );

        let none = stack_sharpness_scale_with_backend(
            &graded,
            0,
            (w, h),
            1,
            80,
            &NoControl,
            Some(&ScaleDouble::default()),
        )
        .unwrap();
        assert_eq!((none.value, none.recheck), (0.0, None), "no frames");
    }

    #[test]
    fn scale_recheck_declines_when_the_candidate_share_is_over_the_limit() {
        let (at, stack, candidates) = matrix()
            .into_iter()
            .find_map(|(at, stack)| {
                let backend = ScaleDouble {
                    push: Push::Up,
                    ..Default::default()
                };
                let bound = PeakErrorBound::for_spp(stack.spp);
                let first = scale_with(&stack, &backend, &bound, 1.0)
                    .ok()?
                    .recheck?
                    .candidates[0];
                (first > 0).then_some((at, stack, first))
            })
            .expect("some stack has first-pass candidates");
        let bound = PeakErrorBound::for_spp(stack.spp);
        let pixels = f64::from(stack.width * stack.height);
        let backend = ScaleDouble {
            push: Push::Up,
            ..Default::default()
        };
        let shipped = scale_at(&stack, 80, &NoControl);
        let over =
            scale_with(&stack, &backend, &bound, (candidates as f64 - 0.5) / pixels).unwrap();
        assert_eq!(
            over.value.to_bits(),
            shipped.to_bits(),
            "{at}: the CPU scale"
        );
        assert!(
            over.recheck.is_none(),
            "{at}: {candidates} candidates are over the limit"
        );
        let at_limit =
            scale_with(&stack, &backend, &bound, (candidates as f64 + 0.5) / pixels).unwrap();
        assert_eq!(at_limit.recheck.unwrap().candidates[0], candidates, "{at}");
    }

    #[test]
    fn scale_recheck_stop_during_the_device_pass_returns_stopped() {
        for units in [0, 1] {
            let stack = graded(3, 101, 67, 1);
            let control = StopAfter {
                units,
                done: AtomicUsize::new(0),
            };
            let result = public_scale(&stack, &ScaleDouble::default(), &control);
            assert!(
                matches!(result, Err(SharpAreasError::Stopped)),
                "{result:?}"
            );
            assert_eq!(
                stack.reads.load(Ordering::SeqCst),
                units,
                "reads after the stop"
            );
        }
    }
}
