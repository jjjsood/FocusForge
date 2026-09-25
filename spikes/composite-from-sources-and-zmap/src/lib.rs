// SPDX-License-Identifier: GPL-3.0-or-later

//! Measures whether a fused composite can be reconstructed from source
//! frames and a stored per-pixel index map alone, instead of being kept as
//! its own rendered plane.
//!
//! Three renderers share one shape -- an interior rectangle grown by a
//! margin, read from every source frame, collapsed back to that interior --
//! so a full-image render, a tiled render and a small region re-render are
//! all the same function called at a different rectangle:
//!
//! - [`untiled_composite`] / [`shipped_composite_tiled`]: today's algorithm,
//!   unmodified, used only as the baseline every other renderer is checked
//!   against.
//! - [`render_b`]: reconstructs a composite from a stored **per-level**
//!   winning-frame index (one `u16` per pixel per pyramid level) plus the
//!   coarsest residual, recomputed or stored.
//! - [`render_a`]: reconstructs a composite from only the **level-0** index
//!   map, blending every frame's Laplacian pyramid by a Gaussian-pyramid
//!   mask built from that one map.
//! - [`render_gather`]: the plain per-pixel gather `source[index[p]][p]`,
//!   with no pyramid at all.

use std::io::Write;
use std::path::{Path, PathBuf};

use focusforge_core::algorithm::{StackAlgorithm, TileCtx};
use focusforge_core::fuse::laplacian::{Laplacian, score_plane, score_radius};
use focusforge_core::fuse::{Plane, Pyramid, build, collapse, collapse_into, gaussian_levels};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter, frame_paths};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::blob::ZSTD_LEVEL;
use focusforge_core::tile::{Tile, TilePlan};

/// Every error this crate's functions can return, boxed.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------
// Frame sets
// ---------------------------------------------------------------------

/// One frame set opened for measurement: the backing stack plus the extent
/// and channel count every frame in it shares.
pub struct FrameSet {
    pub stack: TiffStack,
    pub width: u32,
    pub height: u32,
    pub spp: u32,
}

impl FrameSet {
    pub fn open(paths: &[PathBuf]) -> Result<Self> {
        let stack = TiffStack::open_files(paths)?;
        let (width, height) = stack.dimensions();
        let spp = stack.header(0).samples_per_pixel;
        Ok(FrameSet {
            stack,
            width,
            height,
            spp,
        })
    }

    pub fn frame_count(&self) -> usize {
        self.stack.len()
    }
}

/// The frame paths of a fixture directory's `frames/` subdirectory.
pub fn fixture_frame_paths(fixture_dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(frame_paths(fixture_dir)?)
}

/// A reference-stack directory's source frames, filtered to the ones whose
/// name carries `IMG_` so the stacked result sitting in the same directory
/// never joins the set.
pub fn reference_frame_paths(stack_dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(frame_paths(stack_dir)?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .collect())
}

fn levels_cfg(levels: u32) -> ParamValues {
    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(levels as i64));
    cfg
}

// ---------------------------------------------------------------------
// Buffers and index planes
// ---------------------------------------------------------------------

/// A full-extent sample buffer, chunky, row-major, `u16` -- what a
/// composite or a Z-Map looks like once every tile's interior is stitched
/// together.
#[derive(Clone)]
pub struct Buffer {
    pub width: u32,
    pub height: u32,
    pub spp: u32,
    pub samples: Vec<u16>,
}

impl Buffer {
    fn new(width: u32, height: u32, spp: u32) -> Self {
        Buffer {
            width,
            height,
            spp,
            samples: vec![0u16; width as usize * height as usize * spp as usize],
        }
    }

    fn paste(&mut self, rect: Rect, samples: &[u16]) {
        paste(&mut self.samples, self.width, rect, self.spp, samples);
    }

    /// A sub-rectangle of this buffer as its own buffer -- the crop a test
    /// needs to compare a region render against the matching slice of a
    /// full render.
    pub fn crop(&self, rect: Rect) -> Buffer {
        Buffer {
            width: rect.width,
            height: rect.height,
            spp: self.spp,
            samples: crop(
                &self.samples,
                self.width,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                self.spp,
            ),
        }
    }
}

/// A single pyramid level's winning-frame index, full extent: `width x
/// height` is that level's own `ceil`-halved size, one `u16` per pixel.
#[derive(Clone)]
pub struct IndexPlane {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u16>,
}

/// Every pyramid level's winning-frame index, full extent, plus the
/// coarsest residual -- everything a per-level render needs to reconstruct
/// a composite with no rendered plane stored anywhere.
pub struct Selection {
    pub levels: u32,
    pub width: u32,
    pub height: u32,
    pub spp: u32,
    /// `per_level[k]` is level `k`'s winning-frame index, `len() == levels`.
    pub per_level: Vec<IndexPlane>,
    pub residual: Plane,
}

// ---------------------------------------------------------------------
// Level geometry shared by every stitching and cropping step
// ---------------------------------------------------------------------

/// A pyramid level `k`'s own size for a `full`-px axis: `ceil(full /
/// 2^k)`, the same halving [`gaussian_levels`] applies at every step.
fn full_level_dim(full: u32, k: u32) -> u32 {
    full.div_ceil(1u32 << k)
}

/// Where a region `[start, start + size)` of a `full`-px axis lands on
/// pyramid level `k`'s own pixel grid: `(start >> k, size_k)`.
///
/// Exact whenever `start` is a multiple of `2^k` -- true for every tile
/// interior and every read rectangle this crate builds, since both a tile
/// plan's interior origins and its read origins sit on the `2^levels`
/// decimation grid. A region that reaches the axis's own far edge is
/// widened to that level's own `ceil`-rounded edge rather than computed by
/// division, so a ragged trailing tile still lands on the exact boundary a
/// whole-extent build would put there.
fn level_range(full: u32, start: u32, size: u32, k: u32) -> (u32, u32) {
    let start_k = start >> k;
    let full_k = full_level_dim(full, k);
    let end_k = if start + size >= full {
        full_k
    } else {
        (start + size) >> k
    };
    (start_k, end_k - start_k)
}

/// Copies a `w x h` window at `(x0, y0)` out of a `src_width`-wide,
/// `channels`-per-pixel buffer.
fn crop<T: Copy + Default>(
    src: &[T],
    src_width: u32,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    channels: u32,
) -> Vec<T> {
    let stride = channels as usize;
    let mut out = vec![T::default(); w as usize * h as usize * stride];
    for r in 0..h {
        let src_start = ((y0 + r) * src_width + x0) as usize * stride;
        let dst_start = (r * w) as usize * stride;
        let len = w as usize * stride;
        out[dst_start..dst_start + len].copy_from_slice(&src[src_start..src_start + len]);
    }
    out
}

/// Writes a `w x h`, `channels`-per-pixel `src` into a `dst_width`-wide
/// buffer at `(x0, y0)`.
#[allow(clippy::too_many_arguments)]
fn paste_into<T: Copy>(
    dst: &mut [T],
    dst_width: u32,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    channels: u32,
    src: &[T],
) {
    let stride = channels as usize;
    for r in 0..h {
        let dst_start = ((y0 + r) * dst_width + x0) as usize * stride;
        let src_start = (r * w) as usize * stride;
        let len = w as usize * stride;
        dst[dst_start..dst_start + len].copy_from_slice(&src[src_start..src_start + len]);
    }
}

/// [`paste_into`] specialised to a [`Rect`] and `u16` samples -- the shape
/// [`Buffer::paste`] and every tile-stitching loop below needs.
fn paste(dst: &mut [u16], dst_width: u32, rect: Rect, channels: u32, src: &[u16]) {
    paste_into(
        dst,
        dst_width,
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        channels,
        src,
    );
}

/// The read rectangle a render at `interior` grown by `margin` needs: the
/// same grow-and-clamp arithmetic [`TilePlan::new`] applies to every
/// planned tile, so an origin already on the `2^levels` decimation grid
/// stays on it (`margin` is always a multiple of `2^levels` in this
/// crate's own callers).
fn grow_and_clamp(interior: Rect, margin: u32, width: u32, height: u32) -> Rect {
    let x = interior.x.saturating_sub(margin);
    let y = interior.y.saturating_sub(margin);
    let x_end = (interior.x + interior.width + margin).min(width);
    let y_end = (interior.y + interior.height + margin).min(height);
    Rect {
        x,
        y,
        width: x_end - x,
        height: y_end - y,
    }
}

/// Builds a zero pyramid at `(width, height, levels)` and overwrites every
/// level and the residual with the caller's own planes -- the only way to
/// assemble a [`Pyramid`] from parts this crate's public API offers, since
/// [`Pyramid`] has no from-parts constructor of its own.
fn assemble_pyramid(
    width: u32,
    height: u32,
    spp: u32,
    levels: Vec<Plane>,
    residual: Plane,
) -> Result<Pyramid> {
    let n = levels.len() as u32;
    let zeros = vec![0u16; width as usize * height as usize * spp as usize];
    let mut pyramid = build(&zeros, width, height, spp, n)?;
    for (k, plane) in levels.into_iter().enumerate() {
        *pyramid.level_mut(k) = plane;
    }
    *pyramid.residual_mut() = residual;
    Ok(pyramid)
}

// ---------------------------------------------------------------------
// Today's composite -- the shipped algorithm, tiled and untiled
// ---------------------------------------------------------------------

/// Runs the shipped `laplacian` algorithm over the whole extent in one
/// pass, with no tile plan at all -- the untiled target every tiled
/// variant is checked against.
pub fn untiled_composite(frames: &FrameSet, levels: u32) -> Result<(Buffer, Buffer)> {
    let full = Rect {
        x: 0,
        y: 0,
        width: frames.width,
        height: frames.height,
    };
    let cfg = levels_cfg(levels);
    let tiles: Vec<Tile> = (0..frames.frame_count())
        .map(|f| frames.stack.read_tile(f, full))
        .collect::<std::result::Result<_, _>>()?;
    let ctx = TileCtx::new(full, &tiles);
    let out = Laplacian.fuse(&ctx, &cfg)?;
    let composite = Buffer {
        width: frames.width,
        height: frames.height,
        spp: frames.spp,
        samples: out.tile.samples,
    };
    let zmap = Buffer {
        width: frames.width,
        height: frames.height,
        spp: 1,
        samples: out.zmap,
    };
    Ok((composite, zmap))
}

/// The shipped selection over the whole extent, but with one winning frame
/// per channel: each channel's coefficients are scored and picked on their
/// own, then collapsed over the same ascending-order mean residual.
pub fn per_channel_composite(frames: &FrameSet, levels: u32) -> Result<Buffer> {
    let full = Rect {
        x: 0,
        y: 0,
        width: frames.width,
        height: frames.height,
    };
    let spp = frames.spp as usize;
    let channel_scores = |pyramid: &Pyramid| -> Result<Vec<Vec<Vec<f32>>>> {
        let mut per_level = Vec::new();
        for (k, plane) in pyramid.levels().iter().enumerate() {
            let mut per_channel = Vec::new();
            for c in 0..spp {
                let channel: Vec<f32> = plane.data().iter().skip(c).step_by(spp).copied().collect();
                let single = Plane::from_f32(&channel, plane.width(), plane.height(), 1)?;
                per_channel.push(score_plane(&single, score_radius(levels, k as u32)));
            }
            per_level.push(per_channel);
        }
        Ok(per_level)
    };

    let tile0 = frames.stack.read_tile(0, full)?;
    let mut best = build(&tile0.samples, full.width, full.height, frames.spp, levels)?;
    let mut best_scores = channel_scores(&best)?;
    let mut residual_sum: Vec<f32> = best.residual().data().to_vec();

    for f in 1..frames.frame_count() {
        let tile = frames.stack.read_tile(f, full)?;
        let current = build(&tile.samples, full.width, full.height, frames.spp, levels)?;
        for (sum, &value) in residual_sum.iter_mut().zip(current.residual().data()) {
            *sum += value;
        }
        let scores = channel_scores(&current)?;
        for k in 0..levels as usize {
            let src = current.levels()[k].data();
            let dst = best.level_mut(k).data_mut();
            for c in 0..spp {
                for (p, &score) in scores[k][c].iter().enumerate() {
                    if score > best_scores[k][c][p] {
                        best_scores[k][c][p] = score;
                        dst[p * spp + c] = src[p * spp + c];
                    }
                }
            }
        }
    }
    let n = frames.frame_count() as f32;
    for (out, &sum) in best.residual_mut().data_mut().iter_mut().zip(&residual_sum) {
        *out = sum / n;
    }

    let mut out = Buffer::new(full.width, full.height, frames.spp);
    collapse_into(&best, &mut out.samples)?;
    Ok(out)
}

/// Runs the shipped `laplacian` algorithm over a [`TilePlan`], stitching
/// every tile's interior into full-extent composite and Z-Map buffers --
/// what a real fuse run over this extent actually produces.
pub fn shipped_composite_tiled(
    frames: &FrameSet,
    levels: u32,
    tile_size: u32,
) -> Result<(Buffer, Buffer)> {
    let plan = TilePlan::new(frames.width, frames.height, tile_size, levels)?;
    let cfg = levels_cfg(levels);
    let mut composite = Buffer::new(frames.width, frames.height, frames.spp);
    let mut zmap = Buffer::new(frames.width, frames.height, 1);

    for planned in plan.tiles() {
        let read = planned.read();
        let interior = planned.interior();
        let tiles: Vec<Tile> = (0..frames.frame_count())
            .map(|f| frames.stack.read_tile(f, read))
            .collect::<std::result::Result<_, _>>()?;
        let ctx = TileCtx::new(read, &tiles);
        let out = Laplacian.fuse(&ctx, &cfg)?;

        let mut composite_crop =
            vec![0u16; interior.width as usize * interior.height as usize * frames.spp as usize];
        out.tile.crop_into(interior, &mut composite_crop)?;
        composite.paste(interior, &composite_crop);

        let zmap_tile = Tile {
            rect: read,
            samples_per_pixel: 1,
            samples: out.zmap,
        };
        let mut zmap_crop = vec![0u16; interior.width as usize * interior.height as usize];
        zmap_tile.crop_into(interior, &mut zmap_crop)?;
        zmap.paste(interior, &zmap_crop);
    }
    Ok((composite, zmap))
}

// ---------------------------------------------------------------------
// The rebuilt selection: every level's winner, not only level 0
// ---------------------------------------------------------------------

/// One tile's selection loop, copied from the shipped algorithm's own
/// (frame 0 seeds the running best, a later frame wins a level only by a
/// strictly greater windowed score, the residual is the ascending-order
/// mean) but keeping every level's winning index instead of collapsing to
/// pixel values -- variant B never stores a rendered coefficient, only
/// which frame won it.
///
/// Returns `(winner, residual_sum)`: `winner[k]` is level `k`'s
/// winning-frame index at this tile's own read-rect size, and
/// `residual_sum` is the coarsest Gaussian levels' sum in ascending frame
/// order, not yet divided by the frame count.
fn select_tile(
    tiles: &[Tile],
    width: u32,
    height: u32,
    spp: u32,
    levels: u32,
) -> Result<(Vec<Vec<u16>>, Vec<f32>)> {
    let frame0 = build(&tiles[0].samples, width, height, spp, levels)?;
    let mut best_scores: Vec<Vec<f32>> = frame0
        .levels()
        .iter()
        .enumerate()
        .map(|(k, plane)| score_plane(plane, score_radius(levels, k as u32)))
        .collect();
    let mut winner: Vec<Vec<u16>> = best_scores.iter().map(|s| vec![0u16; s.len()]).collect();
    let mut residual_sum: Vec<f32> = frame0.residual().data().to_vec();

    for (f, tile) in tiles.iter().enumerate().skip(1) {
        let current = build(&tile.samples, width, height, spp, levels)?;
        for (sum, &value) in residual_sum.iter_mut().zip(current.residual().data()) {
            *sum += value;
        }
        for k in 0..levels as usize {
            let current_level = &current.levels()[k];
            let current_score = score_plane(current_level, score_radius(levels, k as u32));
            for p in 0..current_score.len() {
                if current_score[p] > best_scores[k][p] {
                    best_scores[k][p] = current_score[p];
                    winner[k][p] = f as u16;
                }
            }
        }
    }
    Ok((winner, residual_sum))
}

/// [`select_tile`]'s loop, but also keeping the winning pixel *values* --
/// exactly what the shipped algorithm's own `best` pyramid holds. A tile's
/// own read rect gives its own interior enough margin for an exact
/// selection, never a neighbouring tile's interior: reconstructing a tile
/// from a value pyramid built this way, tile by tile, reproduces the
/// shipped per-tile composite with no cross-tile map sharing at all.
fn rebuild_tile(
    tiles: &[Tile],
    width: u32,
    height: u32,
    spp: u32,
    levels: u32,
) -> Result<(Vec<Vec<u16>>, Pyramid)> {
    let mut best = build(&tiles[0].samples, width, height, spp, levels)?;
    let mut best_scores: Vec<Vec<f32>> = best
        .levels()
        .iter()
        .enumerate()
        .map(|(k, plane)| score_plane(plane, score_radius(levels, k as u32)))
        .collect();
    let mut winner: Vec<Vec<u16>> = best_scores.iter().map(|s| vec![0u16; s.len()]).collect();
    let mut residual_sum: Vec<f32> = best.residual().data().to_vec();
    let spp_usize = spp as usize;

    for (f, tile) in tiles.iter().enumerate().skip(1) {
        let current = build(&tile.samples, width, height, spp, levels)?;
        for (sum, &value) in residual_sum.iter_mut().zip(current.residual().data()) {
            *sum += value;
        }
        for k in 0..levels as usize {
            let current_level = &current.levels()[k];
            let current_score = score_plane(current_level, score_radius(levels, k as u32));
            for p in 0..current_score.len() {
                if current_score[p] > best_scores[k][p] {
                    let src = &current_level.data()[p * spp_usize..(p + 1) * spp_usize];
                    best.level_mut(k).data_mut()[p * spp_usize..(p + 1) * spp_usize]
                        .copy_from_slice(src);
                    best_scores[k][p] = current_score[p];
                    winner[k][p] = f as u16;
                }
            }
        }
    }
    let n = tiles.len() as f32;
    for (out, &sum) in best
        .residual_mut()
        .data_mut()
        .iter_mut()
        .zip(residual_sum.iter())
    {
        *out = sum / n;
    }
    Ok((winner, best))
}

/// Runs the rebuilt selection loop per tile of a [`TilePlan`], collapsing
/// each tile's own value pyramid directly and stitching interiors -- no
/// stored index map, no cross-tile sharing, so this reproduces
/// [`shipped_composite_tiled`] exactly whenever the rebuilt loop is a
/// faithful copy of the shipped one.
pub fn rebuild_composite_tiled(
    frames: &FrameSet,
    levels: u32,
    tile_size: u32,
) -> Result<(Buffer, Buffer)> {
    let plan = TilePlan::new(frames.width, frames.height, tile_size, levels)?;
    let mut composite = Buffer::new(frames.width, frames.height, frames.spp);
    let mut zmap = Buffer::new(frames.width, frames.height, 1);

    for planned in plan.tiles() {
        let read = planned.read();
        let interior = planned.interior();
        let tiles: Vec<Tile> = (0..frames.frame_count())
            .map(|f| frames.stack.read_tile(f, read))
            .collect::<std::result::Result<_, _>>()?;
        let (winner, best) = rebuild_tile(&tiles, read.width, read.height, frames.spp, levels)?;
        let collapsed = collapse(&best).to_u16();

        let ox = interior.x - read.x;
        let oy = interior.y - read.y;
        let composite_crop = crop(
            &collapsed,
            read.width,
            ox,
            oy,
            interior.width,
            interior.height,
            frames.spp,
        );
        composite.paste(interior, &composite_crop);

        let zmap_crop = crop(
            &winner[0],
            read.width,
            ox,
            oy,
            interior.width,
            interior.height,
            1,
        );
        zmap.paste(interior, &zmap_crop);
    }
    Ok((composite, zmap))
}

fn selection_from_winner(
    width: u32,
    height: u32,
    spp: u32,
    levels: u32,
    winner: Vec<Vec<u16>>,
    residual_mean: Vec<f32>,
) -> Result<Selection> {
    let per_level = winner
        .into_iter()
        .enumerate()
        .map(|(k, data)| IndexPlane {
            width: full_level_dim(width, k as u32),
            height: full_level_dim(height, k as u32),
            data,
        })
        .collect();
    let res_w = full_level_dim(width, levels);
    let res_h = full_level_dim(height, levels);
    let residual = Plane::from_f32(&residual_mean, res_w, res_h, spp)?;
    Ok(Selection {
        levels,
        width,
        height,
        spp,
        per_level,
        residual,
    })
}

/// The rebuilt selection over the whole extent, no tiling -- variant B's
/// exactness target.
pub fn untiled_selection(frames: &FrameSet, levels: u32) -> Result<Selection> {
    let full = Rect {
        x: 0,
        y: 0,
        width: frames.width,
        height: frames.height,
    };
    let tiles: Vec<Tile> = (0..frames.frame_count())
        .map(|f| frames.stack.read_tile(f, full))
        .collect::<std::result::Result<_, _>>()?;
    let (winner, residual_sum) =
        select_tile(&tiles, frames.width, frames.height, frames.spp, levels)?;
    let n = frames.frame_count() as f32;
    let mean: Vec<f32> = residual_sum.iter().map(|&s| s / n).collect();
    selection_from_winner(
        frames.width,
        frames.height,
        frames.spp,
        levels,
        winner,
        mean,
    )
}

/// The rebuilt selection over a [`TilePlan`], every tile's per-level
/// winners and residual stitched into full-extent planes.
pub fn tiled_selection(frames: &FrameSet, levels: u32, tile_size: u32) -> Result<Selection> {
    let plan = TilePlan::new(frames.width, frames.height, tile_size, levels)?;
    let mut per_level_data: Vec<Vec<u16>> = (0..levels)
        .map(|k| {
            let w = full_level_dim(frames.width, k);
            let h = full_level_dim(frames.height, k);
            vec![0u16; w as usize * h as usize]
        })
        .collect();
    let res_w = full_level_dim(frames.width, levels);
    let res_h = full_level_dim(frames.height, levels);
    let mut residual_data = vec![0.0f32; res_w as usize * res_h as usize * frames.spp as usize];

    for planned in plan.tiles() {
        let read = planned.read();
        let interior = planned.interior();
        let tiles: Vec<Tile> = (0..frames.frame_count())
            .map(|f| frames.stack.read_tile(f, read))
            .collect::<std::result::Result<_, _>>()?;
        let (winner, residual_sum) =
            select_tile(&tiles, read.width, read.height, frames.spp, levels)?;
        let n = frames.frame_count() as f32;

        for k in 0..levels as usize {
            let local_w = full_level_dim(read.width, k as u32);
            let (sx, sw) = level_range(frames.width, interior.x, interior.width, k as u32);
            let (sy, sh) = level_range(frames.height, interior.y, interior.height, k as u32);
            let ox = (interior.x - read.x) >> k;
            let oy = (interior.y - read.y) >> k;
            let cropped = crop(&winner[k], local_w, ox, oy, sw, sh, 1);
            paste_into(
                &mut per_level_data[k],
                full_level_dim(frames.width, k as u32),
                sx,
                sy,
                sw,
                sh,
                1,
                &cropped,
            );
        }

        let local_res_w = full_level_dim(read.width, levels);
        let (rsx, rsw) = level_range(frames.width, interior.x, interior.width, levels);
        let (rsy, rsh) = level_range(frames.height, interior.y, interior.height, levels);
        let rox = (interior.x - read.x) >> levels;
        let roy = (interior.y - read.y) >> levels;
        let mean: Vec<f32> = residual_sum.iter().map(|&s| s / n).collect();
        let cropped_res = crop(&mean, local_res_w, rox, roy, rsw, rsh, frames.spp);
        paste_into(
            &mut residual_data,
            res_w,
            rsx,
            rsy,
            rsw,
            rsh,
            frames.spp,
            &cropped_res,
        );
    }

    selection_from_winner(
        frames.width,
        frames.height,
        frames.spp,
        levels,
        per_level_data,
        residual_data,
    )
}

// ---------------------------------------------------------------------
// Variant B: per-level index render
// ---------------------------------------------------------------------

/// Where variant B's coarsest residual comes from for one render:
/// recomputed from every source, or cropped from a plane already held.
#[derive(Clone, Copy)]
pub enum Residual<'a> {
    Recompute,
    Stored(&'a Plane),
}

/// Reconstructs a composite at `interior`, grown by `margin`, from a
/// [`Selection`]'s per-level winning-frame indices: streams frames in
/// ascending order, copies each frame's Laplacian coefficients wherever it
/// won a level, adds its residual contribution (or crops a stored one),
/// and collapses. Only one frame's pyramid is live at a time.
pub fn render_b(
    frames: &FrameSet,
    selection: &Selection,
    residual: Residual<'_>,
    interior: Rect,
    margin: u32,
) -> Result<Buffer> {
    let levels = selection.levels;
    let spp = frames.spp as usize;
    let read = grow_and_clamp(interior, margin, frames.width, frames.height);

    let cropped_maps: Vec<(Vec<u16>, u32, u32)> = (0..levels)
        .map(|k| {
            let (sx, sw) = level_range(frames.width, read.x, read.width, k);
            let (sy, sh) = level_range(frames.height, read.y, read.height, k);
            let map = &selection.per_level[k as usize];
            (crop(&map.data, map.width, sx, sy, sw, sh, 1), sw, sh)
        })
        .collect();

    let mut present = vec![false; frames.frame_count()];
    for (data, _, _) in &cropped_maps {
        for &idx in data {
            present[idx as usize] = true;
        }
    }

    let mut level_acc: Vec<Vec<f32>> = cropped_maps
        .iter()
        .map(|(_, w, h)| vec![0.0f32; *w as usize * *h as usize * spp])
        .collect();

    let (res_x, res_w) = level_range(frames.width, read.x, read.width, levels);
    let (res_y, res_h) = level_range(frames.height, read.y, read.height, levels);
    let mut residual_acc = vec![0.0f32; res_w as usize * res_h as usize * spp];

    let recompute_residual = matches!(residual, Residual::Recompute);
    // `f` both indexes `present` and names the frame to read: not a single
    // collection walk an iterator adaptor could express.
    #[allow(clippy::needless_range_loop)]
    for f in 0..frames.frame_count() {
        if !present[f] && !recompute_residual {
            continue;
        }
        let tile = frames.stack.read_tile(f, read)?;
        let pyramid = build(&tile.samples, read.width, read.height, frames.spp, levels)?;

        if present[f] {
            for k in 0..levels as usize {
                let (map, w, h) = &cropped_maps[k];
                let plane = pyramid.levels()[k].data();
                let acc = &mut level_acc[k];
                for p in 0..(*w as usize * *h as usize) {
                    if map[p] as usize == f {
                        acc[p * spp..(p + 1) * spp].copy_from_slice(&plane[p * spp..(p + 1) * spp]);
                    }
                }
            }
        }
        if recompute_residual {
            for (sum, &value) in residual_acc.iter_mut().zip(pyramid.residual().data()) {
                *sum += value;
            }
        }
    }

    let residual_plane = match residual {
        Residual::Recompute => {
            let n = frames.frame_count() as f32;
            let mean: Vec<f32> = residual_acc.iter().map(|&s| s / n).collect();
            Plane::from_f32(&mean, res_w, res_h, frames.spp)?
        }
        Residual::Stored(stored) => {
            let cropped = crop(
                stored.data(),
                stored.width(),
                res_x,
                res_y,
                res_w,
                res_h,
                frames.spp,
            );
            Plane::from_f32(&cropped, res_w, res_h, frames.spp)?
        }
    };

    let mut level_planes = Vec::with_capacity(levels as usize);
    for (k, (_, w, h)) in cropped_maps.iter().enumerate() {
        level_planes.push(Plane::from_f32(&level_acc[k], *w, *h, frames.spp)?);
    }
    let pyramid = assemble_pyramid(
        read.width,
        read.height,
        frames.spp,
        level_planes,
        residual_plane,
    )?;
    let collapsed = collapse(&pyramid).to_u16();

    let ox = interior.x - read.x;
    let oy = interior.y - read.y;
    let cropped = crop(
        &collapsed,
        read.width,
        ox,
        oy,
        interior.width,
        interior.height,
        frames.spp,
    );
    Ok(Buffer {
        width: interior.width,
        height: interior.height,
        spp: frames.spp,
        samples: cropped,
    })
}

/// Renders every interior of `plan` through [`render_b`] at `margin`,
/// reading only the stored per-level index map.
pub fn render_b_tiled(
    frames: &FrameSet,
    selection: &Selection,
    residual: Residual<'_>,
    plan: &TilePlan,
    margin: u32,
) -> Result<Buffer> {
    let mut out = Buffer::new(frames.width, frames.height, frames.spp);
    for tile in plan.tiles() {
        let piece = render_b(frames, selection, residual, tile.interior(), margin)?;
        out.paste(tile.interior(), &piece.samples);
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Variant A: Burt-Adelson multi-band blend from the level-0 map alone
// ---------------------------------------------------------------------

/// Reconstructs a composite at `interior`, grown by `margin`, from only the
/// level-0 winning-frame index: for every frame present in the cropped map,
/// builds a `{0, 1}` mask, Gaussian-blurs it to every pyramid level, and
/// blends that frame's Laplacian pyramid and residual by the blurred mask.
pub fn render_a(
    frames: &FrameSet,
    zmap0: &IndexPlane,
    levels: u32,
    interior: Rect,
    margin: u32,
) -> Result<Buffer> {
    let spp = frames.spp as usize;
    let read = grow_and_clamp(interior, margin, frames.width, frames.height);
    let (zx, zw) = level_range(frames.width, read.x, read.width, 0);
    let (zy, zh) = level_range(frames.height, read.y, read.height, 0);
    let cropped_zmap = crop(&zmap0.data, zmap0.width, zx, zy, zw, zh, 1);

    let mut present: Vec<usize> = Vec::new();
    let mut seen = vec![false; frames.frame_count()];
    for &idx in &cropped_zmap {
        let f = idx as usize;
        if !seen[f] {
            seen[f] = true;
            present.push(f);
        }
    }

    let res_w = full_level_dim(read.width, levels);
    let res_h = full_level_dim(read.height, levels);
    let mut level_acc: Vec<Vec<f32>> = (0..levels)
        .map(|k| {
            let w = full_level_dim(read.width, k);
            let h = full_level_dim(read.height, k);
            vec![0.0f32; w as usize * h as usize * spp]
        })
        .collect();
    let mut residual_acc = vec![0.0f32; res_w as usize * res_h as usize * spp];

    for f in present {
        let mask_data: Vec<f32> = cropped_zmap
            .iter()
            .map(|&idx| if idx as usize == f { 1.0 } else { 0.0 })
            .collect();
        let mask = Plane::from_f32(&mask_data, read.width, read.height, 1)?;
        let mask_gaussians = gaussian_levels(mask, levels)?;

        let tile = frames.stack.read_tile(f, read)?;
        let pyramid = build(&tile.samples, read.width, read.height, frames.spp, levels)?;

        for k in 0..levels as usize {
            let g = &mask_gaussians[k];
            let plane = pyramid.levels()[k].data();
            let acc = &mut level_acc[k];
            for p in 0..(g.width() * g.height()) as usize {
                let w = g.data()[p];
                if w == 0.0 {
                    continue;
                }
                for c in 0..spp {
                    acc[p * spp + c] += w * plane[p * spp + c];
                }
            }
        }
        let g_res = &mask_gaussians[levels as usize];
        for p in 0..(g_res.width() * g_res.height()) as usize {
            let w = g_res.data()[p];
            if w == 0.0 {
                continue;
            }
            for c in 0..spp {
                residual_acc[p * spp + c] += w * pyramid.residual().data()[p * spp + c];
            }
        }
    }

    let residual_plane = Plane::from_f32(&residual_acc, res_w, res_h, frames.spp)?;
    let mut level_planes = Vec::with_capacity(levels as usize);
    // `k` both derives this level's own dims and indexes `level_acc`.
    #[allow(clippy::needless_range_loop)]
    for k in 0..levels as usize {
        let w = full_level_dim(read.width, k as u32);
        let h = full_level_dim(read.height, k as u32);
        level_planes.push(Plane::from_f32(&level_acc[k], w, h, frames.spp)?);
    }
    let pyramid = assemble_pyramid(
        read.width,
        read.height,
        frames.spp,
        level_planes,
        residual_plane,
    )?;
    let collapsed = collapse(&pyramid).to_u16();

    let ox = interior.x - read.x;
    let oy = interior.y - read.y;
    let cropped = crop(
        &collapsed,
        read.width,
        ox,
        oy,
        interior.width,
        interior.height,
        frames.spp,
    );
    Ok(Buffer {
        width: interior.width,
        height: interior.height,
        spp: frames.spp,
        samples: cropped,
    })
}

/// Renders every tile of `plan` through [`render_a`], reading only the
/// level-0 index map.
pub fn render_a_tiled(
    frames: &FrameSet,
    zmap0: &IndexPlane,
    levels: u32,
    plan: &TilePlan,
) -> Result<Buffer> {
    let mut out = Buffer::new(frames.width, frames.height, frames.spp);
    for tile in plan.tiles() {
        let piece = render_a(frames, zmap0, levels, tile.interior(), plan.margin())?;
        out.paste(tile.interior(), &piece.samples);
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// The plain gather
// ---------------------------------------------------------------------

/// `source[index[p]][p]`: the region cost floor, no pyramid at all. Reads
/// only the frames the level-0 map actually names inside `interior`.
pub fn render_gather(frames: &FrameSet, zmap0: &IndexPlane, interior: Rect) -> Result<Buffer> {
    let spp = frames.spp as usize;
    let cropped_zmap = crop(
        &zmap0.data,
        zmap0.width,
        interior.x,
        interior.y,
        interior.width,
        interior.height,
        1,
    );

    let mut present: Vec<usize> = Vec::new();
    let mut seen = vec![false; frames.frame_count()];
    for &idx in &cropped_zmap {
        let f = idx as usize;
        if !seen[f] {
            seen[f] = true;
            present.push(f);
        }
    }

    let mut out = vec![0u16; interior.width as usize * interior.height as usize * spp];
    for f in present {
        let tile = frames.stack.read_tile(f, interior)?;
        for p in 0..(interior.width * interior.height) as usize {
            if cropped_zmap[p] as usize == f {
                out[p * spp..(p + 1) * spp].copy_from_slice(&tile.samples[p * spp..(p + 1) * spp]);
            }
        }
    }
    Ok(Buffer {
        width: interior.width,
        height: interior.height,
        spp: frames.spp,
        samples: out,
    })
}

// ---------------------------------------------------------------------
// Measurement helpers
// ---------------------------------------------------------------------

/// Maximum and mean absolute per-sample difference between two equal-length
/// sample buffers.
pub fn diff(a: &[u16], b: &[u16]) -> (u32, f64) {
    assert_eq!(a.len(), b.len(), "diff needs two buffers of equal length");
    let mut max = 0u32;
    let mut sum = 0u64;
    for (&x, &y) in a.iter().zip(b) {
        let d = (x as i32 - y as i32).unsigned_abs();
        max = max.max(d);
        sum += u64::from(d);
    }
    let mean = if a.is_empty() {
        0.0
    } else {
        sum as f64 / a.len() as f64
    };
    (max, mean)
}

/// A difference split into pixels within `band` px of an interior tile
/// edge that is not the image's own border, and pixels outside that band.
pub struct SeamBandDiff {
    pub band_max: u32,
    pub band_mean: f64,
    pub outside_max: u32,
}

/// Splits the per-pixel difference between two same-size buffers by
/// distance to the nearest interior tile boundary of `plan` -- the image's
/// own outer border is never counted as a seam.
pub fn seam_band_diff(a: &Buffer, b: &Buffer, plan: &TilePlan, band: u32) -> SeamBandDiff {
    assert_eq!((a.width, a.height, a.spp), (b.width, b.height, b.spp));
    let (width, height, spp) = (a.width, a.height, a.spp as usize);

    let mut seam_x: Vec<u32> = Vec::new();
    let mut seam_y: Vec<u32> = Vec::new();
    for tile in plan.tiles() {
        let r = tile.interior();
        if r.x > 0 {
            seam_x.push(r.x);
        }
        if r.x + r.width < width {
            seam_x.push(r.x + r.width);
        }
        if r.y > 0 {
            seam_y.push(r.y);
        }
        if r.y + r.height < height {
            seam_y.push(r.y + r.height);
        }
    }
    seam_x.sort_unstable();
    seam_x.dedup();
    seam_y.sort_unstable();
    seam_y.dedup();

    let near = |v: u32, seams: &[u32]| seams.iter().any(|&s| v.abs_diff(s) <= band);

    let mut band_max = 0u32;
    let mut band_sum = 0u64;
    let mut band_count = 0u64;
    let mut outside_max = 0u32;
    for y in 0..height {
        let in_band_y = near(y, &seam_y);
        for x in 0..width {
            let in_band = in_band_y || near(x, &seam_x);
            let base = (y as usize * width as usize + x as usize) * spp;
            let d = (0..spp)
                .map(|c| (a.samples[base + c] as i32 - b.samples[base + c] as i32).unsigned_abs())
                .max()
                .unwrap_or(0);
            if in_band {
                band_max = band_max.max(d);
                band_sum += u64::from(d);
                band_count += 1;
            } else {
                outside_max = outside_max.max(d);
            }
        }
    }
    let band_mean = if band_count > 0 {
        band_sum as f64 / band_count as f64
    } else {
        0.0
    };
    SeamBandDiff {
        band_max,
        band_mean,
        outside_max,
    }
}

/// `u16` samples as little-endian bytes -- the same layout the container's
/// own blob store hashes and compresses.
pub fn le_bytes(samples: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// The zstd-compressed length of `bytes` at the container's own level, the
/// same call [`focusforge_core::project::blob::stage_plane`] makes.
pub fn zstd3_len(bytes: &[u8]) -> Result<u64> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
    encoder.write_all(bytes)?;
    let out = encoder.finish()?;
    Ok(out.len() as u64)
}

/// Writes `buf` as a 16-bit uncompressed TIFF at `path` in one call.
pub fn write_tif(path: &Path, buf: &Buffer, description: Option<String>) -> Result<()> {
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: buf.width,
            height: buf.height,
            samples_per_pixel: buf.spp,
            description,
        },
    )?;
    writer.write_tile(
        Rect {
            x: 0,
            y: 0,
            width: buf.width,
            height: buf.height,
        },
        &buf.samples,
    )?;
    writer.finish()?;
    Ok(())
}

/// The process's peak resident set size in KiB, read from `/proc/self`.
/// `None` off Linux or if the file is unreadable -- a measurement
/// convenience, never a correctness dependency.
pub fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmHWM:")
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
    })
}

/// Resets the peak-RSS counter `/proc/self/status`'s `VmHWM` reports, so a
/// later [`peak_rss_kb`] call measures only what happened since this call.
pub fn clear_peak_rss() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}
