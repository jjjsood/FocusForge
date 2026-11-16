// SPDX-License-Identifier: GPL-3.0-or-later

//! Replaces defect pixels from their neighbourhood while frames are read:
//! [`RepairReader`] stacks under [`crate::equalize::GainReader`], so every
//! later reader sees repaired samples. Each channel of a defect pixel takes
//! the lower median of the non-defect pixels in the smallest square window
//! around it that holds any, an existing sample of that channel.

use std::cmp::Ordering;
use std::path::PathBuf;

use serde::Serialize;

use crate::algorithm::{AlgorithmError, TileReader};
use crate::defect_mask::DefectMask;
use crate::tile::{Rect, SampleFormat};

/// The largest distance, in pixels along either axis, from a defect to the
/// nearest pixel that is not one. It bounds the halo a tile read adds, so a
/// mask that covers most of a frame is refused instead of read whole.
pub const MAX_REPAIR_REACH: u32 = 64;

/// The radius of the disc around a detected pixel that demosaicing a colour
/// filter array leaves tinted: its direct neighbours, measured on real Bayer
/// frames.
pub const DEMOSAIC_REACH: u32 = 1;

/// What a run repairs and where the defects come from.
#[derive(Debug, Clone, PartialEq)]
pub struct DefectSettings {
    /// Finds hot pixels with this threshold (see [`crate::hot_pixels`]);
    /// `None` skips the detection.
    pub hot_pixel_threshold: Option<f64>,
    /// A painted defect mask file ([`crate::defect_mask::load`]).
    pub mask: Option<PathBuf>,
    /// Pixels the painted mask is widened by.
    pub grow_radius: u32,
}

/// How many pixels a run repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DefectReport {
    /// Positions in the union of both sources.
    pub repaired: usize,
    /// Positions the hot-pixel detection found; `None` when it did not run.
    pub detected: Option<usize>,
    /// Positions of the grown mask file; `None` when there was none.
    pub file: Option<usize>,
}

/// A defect with no non-defect pixel within [`MAX_REPAIR_REACH`] of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "the defect at ({x}, {y}) is more than {MAX_REPAIR_REACH} pixels from a pixel that is not a \
     defect; the mask covers too much of the frame"
)]
pub struct ReachError {
    pub x: u32,
    pub y: u32,
}

/// Calls `visit` with every in-frame pixel at Chebyshev distance `distance`
/// from `(x, y)`.
fn for_ring(
    (x, y): (u32, u32),
    distance: u32,
    (width, height): (u32, u32),
    mut visit: impl FnMut(u32, u32),
) {
    let (x, y, d) = (i64::from(x), i64::from(y), i64::from(distance));
    let columns = (x - d).max(0)..=(x + d).min(i64::from(width) - 1);
    let rows = (y - d + 1).max(0)..=(y + d - 1).min(i64::from(height) - 1);
    for edge in [y - d, y + d] {
        if (0..i64::from(height)).contains(&edge) {
            columns
                .clone()
                .for_each(|column| visit(column as u32, edge as u32));
        }
    }
    for edge in [x - d, x + d] {
        if (0..i64::from(width)).contains(&edge) {
            rows.clone().for_each(|row| visit(edge as u32, row as u32));
        }
    }
}

/// The largest distance from a defect of `mask` to the nearest pixel that is
/// not one; 0 for an empty mask.
pub fn reach(mask: &DefectMask) -> Result<u32, ReachError> {
    let size = (mask.width(), mask.height());
    let mut reach = 0;
    for (x, y) in mask.positions() {
        let distance = (1..=MAX_REPAIR_REACH).find(|&distance| {
            let mut healthy = false;
            for_ring((x, y), distance, size, |nx, ny| {
                healthy |= !mask.contains(nx, ny)
            });
            healthy
        });
        reach = reach.max(distance.ok_or(ReachError { x, y })?);
    }
    Ok(reach)
}

/// A [`TileReader`] over `inner` that replaces every pixel of `mask`. A read
/// takes the halo it needs from `inner`, so the result does not depend on
/// tile size or tile edges.
pub struct RepairReader<'a> {
    inner: &'a dyn TileReader,
    mask: &'a DefectMask,
    reach: u32,
    samples_per_pixel: usize,
}

impl<'a> RepairReader<'a> {
    /// A reader of `inner`'s frames of `samples_per_pixel` interleaved
    /// samples, sized like `mask`; an error when a defect lies beyond
    /// [`MAX_REPAIR_REACH`] of every pixel that is not one.
    pub fn new(
        inner: &'a dyn TileReader,
        mask: &'a DefectMask,
        samples_per_pixel: u32,
    ) -> Result<Self, ReachError> {
        Ok(RepairReader {
            inner,
            mask,
            reach: reach(mask)?,
            samples_per_pixel: samples_per_pixel as usize,
        })
    }

    fn fits(&self, rect: Rect) -> bool {
        u64::from(rect.x) + u64::from(rect.width) <= u64::from(self.mask.width())
            && u64::from(rect.y) + u64::from(rect.height) <= u64::from(self.mask.height())
    }

    /// `rect` grown by the reach on every side and clamped to the frame.
    fn grown(&self, rect: Rect) -> Rect {
        let x = rect.x.saturating_sub(self.reach);
        let y = rect.y.saturating_sub(self.reach);
        Rect {
            x,
            y,
            width: rect
                .x
                .saturating_add(rect.width)
                .saturating_add(self.reach)
                .min(self.mask.width())
                - x,
            height: rect
                .y
                .saturating_add(rect.height)
                .saturating_add(self.reach)
                .min(self.mask.height())
                - y,
        }
    }

    fn holds_defect(&self, rect: Rect) -> bool {
        (rect.y..rect.y + rect.height)
            .any(|y| (rect.x..rect.x + rect.width).any(|x| self.mask.contains(x, y)))
    }

    /// Fills `out` with `read`'s samples of `rect`, defects replaced. A
    /// rect without a defect is read as it is; a rect outside the frame goes
    /// to `inner`, which refuses it.
    fn read_repaired<T: Copy + Default>(
        &self,
        rect: Rect,
        out: &mut [T],
        order: fn(&T, &T) -> Ordering,
        read: impl FnOnce(Rect, &mut [T]) -> Result<(), AlgorithmError>,
    ) -> Result<(), AlgorithmError> {
        if !self.fits(rect) || !self.holds_defect(rect) {
            return read(rect, out);
        }
        let grown = self.grown(rect);
        let spp = self.samples_per_pixel;
        let mut halo = vec![T::default(); grown.width as usize * grown.height as usize * spp];
        read(grown, &mut halo)?;
        let halo_row = grown.width as usize * spp;
        let out_row = rect.width as usize * spp;
        let first = ((rect.y - grown.y) as usize * grown.width as usize
            + (rect.x - grown.x) as usize)
            * spp;
        for (row, line) in out.chunks_exact_mut(out_row).enumerate() {
            line.copy_from_slice(&halo[first + row * halo_row..][..out_row]);
        }
        let size = (self.mask.width(), self.mask.height());
        let mut healthy = Vec::new();
        let mut values = Vec::new();
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                if !self.mask.contains(x, y) {
                    continue;
                }
                let found = (1..=self.reach).any(|distance| {
                    healthy.clear();
                    for_ring((x, y), distance, size, |nx, ny| {
                        if !self.mask.contains(nx, ny) {
                            healthy.push(
                                ((ny - grown.y) as usize * grown.width as usize
                                    + (nx - grown.x) as usize)
                                    * spp,
                            );
                        }
                    });
                    !healthy.is_empty()
                });
                assert!(found, "the reach covers every defect of the mask");
                let at =
                    ((y - rect.y) as usize * rect.width as usize + (x - rect.x) as usize) * spp;
                for channel in 0..spp {
                    values.clear();
                    values.extend(healthy.iter().map(|&pixel| halo[pixel + channel]));
                    let median = (values.len() - 1) / 2;
                    let (_, lower, _) = values.select_nth_unstable_by(median, order);
                    out[at + channel] = *lower;
                }
            }
        }
        Ok(())
    }

    /// The grown rect's size for a `width` × `height` read.
    fn grown_size(&self, width: u32, height: u32) -> (u32, u32) {
        let grow = |side: u32, frame: u32| side.saturating_add(2 * self.reach).min(frame);
        (
            grow(width, self.mask.width()),
            grow(height, self.mask.height()),
        )
    }
}

impl TileReader for RepairReader<'_> {
    fn sample_format(&self) -> SampleFormat {
        self.inner.sample_format()
    }

    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.read_repaired(rect, out, u16::cmp, |rect, out| {
            self.inner.read_tile_into(frame, rect, out)
        })
    }

    fn read_tile_gained_into(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.read_repaired(rect, out, u16::cmp, |rect, out| {
            self.inner.read_tile_gained_into(frame, rect, gain, out)
        })
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        self.read_repaired(rect, out, f32::total_cmp, |rect, out| {
            self.inner.read_tile_f32_into(frame, rect, out)
        })
    }

    fn read_tile_gained_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        self.read_repaired(rect, out, f32::total_cmp, |rect, out| {
            self.inner.read_tile_gained_f32_into(frame, rect, gain, out)
        })
    }

    fn linear_means(&self, frame: usize) -> Result<Option<[f64; 3]>, AlgorithmError> {
        self.inner.linear_means(frame)
    }

    fn read_scratch_bytes(&self, width: u32, height: u32) -> u64 {
        if self.reach == 0 {
            return self.inner.read_scratch_bytes(width, height);
        }
        let (grown_width, grown_height) = self.grown_size(width, height);
        let sample_bytes = match self.sample_format() {
            SampleFormat::U16 => 2,
            SampleFormat::F32 => 4,
        };
        self.inner.read_scratch_bytes(grown_width, grown_height)
            + u64::from(grown_width)
                * u64::from(grown_height)
                * self.samples_per_pixel as u64
                * sample_bytes
    }

    fn read_page_cache_bytes(&self, width: u32, height: u32) -> u64 {
        let (grown_width, grown_height) = self.grown_size(width, height);
        self.inner.read_page_cache_bytes(grown_width, grown_height)
    }
}
