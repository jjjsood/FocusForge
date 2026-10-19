// SPDX-License-Identifier: GPL-3.0-or-later

//! A band-accumulating box-filter reduction to a fixed target size, fed one
//! source band at a time so nothing here ever holds a whole frame -- the
//! same discipline `crates/core/src/io` and `crates/core/src/tile` enforce
//! for their own readers.

use crate::tile::Rect;

/// The source rect, centred within `(source_width, source_height)`, whose
/// aspect ratio matches `(target_width, target_height)` -- the portion of
/// the source a [`Downsampler`] actually reads; the rest is cropped away.
/// Crops the taller axis when the source is more square than the target,
/// the wider axis otherwise. Never wider or taller than the source itself.
pub fn center_crop(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Rect {
    let target_aspect = f64::from(target_width) / f64::from(target_height);
    let source_aspect = f64::from(source_width) / f64::from(source_height);
    if source_aspect > target_aspect {
        let width =
            ((f64::from(source_height) * target_aspect).round() as u32).clamp(1, source_width);
        Rect {
            x: (source_width - width) / 2,
            y: 0,
            width,
            height: source_height,
        }
    } else {
        let height =
            ((f64::from(source_width) / target_aspect).round() as u32).clamp(1, source_height);
        Rect {
            x: 0,
            y: (source_height - height) / 2,
            width: source_width,
            height,
        }
    }
}

/// Accumulates a source image into a fixed `target_width x target_height`
/// box-filter reduction, one band at a time: [`Self::add_band`] folds each
/// band's in-crop samples into their target cells, and [`Self::finish`]
/// takes the plain mean of each cell. Every source row and column inside the
/// crop maps onto exactly one target row and column, so no cell is ever
/// left empty regardless of the source/target ratio.
pub struct Downsampler {
    crop: Rect,
    target_width: u32,
    target_height: u32,
    samples_per_pixel: u32,
    sums: Vec<u64>,
    counts: Vec<u32>,
}

impl Downsampler {
    /// Seeds an accumulator for a `source_width x source_height` image with
    /// `samples_per_pixel` channels, reduced to `target_width x
    /// target_height` via [`center_crop`].
    pub fn new(
        source_width: u32,
        source_height: u32,
        samples_per_pixel: u32,
        target_width: u32,
        target_height: u32,
    ) -> Self {
        Self::with_source_rect(
            center_crop(source_width, source_height, target_width, target_height),
            samples_per_pixel,
            target_width,
            target_height,
        )
    }

    /// Seeds a reduction that maps the full original source into its target.
    /// The caller chooses an aspect-fitting target, so every source edge
    /// contributes before display adds its own letterboxing.
    pub fn new_full_source(
        source_width: u32,
        source_height: u32,
        samples_per_pixel: u32,
        target_width: u32,
        target_height: u32,
    ) -> Self {
        Self::with_source_rect(
            Rect {
                x: 0,
                y: 0,
                width: source_width,
                height: source_height,
            },
            samples_per_pixel,
            target_width,
            target_height,
        )
    }

    fn with_source_rect(
        crop: Rect,
        samples_per_pixel: u32,
        target_width: u32,
        target_height: u32,
    ) -> Self {
        let cells = target_width as usize * target_height as usize;
        Downsampler {
            crop,
            target_width,
            target_height,
            samples_per_pixel,
            sums: vec![0u64; cells * samples_per_pixel as usize],
            counts: vec![0u32; cells],
        }
    }

    /// The centre crop this accumulator reads from the source -- the region
    /// [`Self::add_band`] expects its bands to come from.
    pub fn crop(&self) -> Rect {
        self.crop
    }

    /// Folds one source band into the target: `band` is exactly
    /// `rect.width * rect.height * samples_per_pixel` samples, `rect` in
    /// full-source coordinates. A row or column outside [`Self::crop`] is
    /// skipped; a caller may hand bands that only partially overlap the
    /// crop, or none at all.
    pub fn add_band(&mut self, band: &[u16], rect: Rect) {
        let spp = self.samples_per_pixel as usize;
        debug_assert_eq!(band.len(), rect.width as usize * rect.height as usize * spp);
        for row in 0..rect.height {
            let src_y = rect.y + row;
            if src_y < self.crop.y || src_y >= self.crop.y + self.crop.height {
                continue;
            }
            let target_y = (u64::from(src_y - self.crop.y) * u64::from(self.target_height)
                / u64::from(self.crop.height)) as u32;
            let target_y = target_y.min(self.target_height - 1);
            for col in 0..rect.width {
                let src_x = rect.x + col;
                if src_x < self.crop.x || src_x >= self.crop.x + self.crop.width {
                    continue;
                }
                let target_x = (u64::from(src_x - self.crop.x) * u64::from(self.target_width)
                    / u64::from(self.crop.width)) as u32;
                let target_x = target_x.min(self.target_width - 1);
                let cell = target_y as usize * self.target_width as usize + target_x as usize;
                let src_base = (row as usize * rect.width as usize + col as usize) * spp;
                for c in 0..spp {
                    self.sums[cell * spp + c] += u64::from(band[src_base + c]);
                }
                self.counts[cell] += 1;
            }
        }
    }

    /// The reduced image: `target_width * target_height * samples_per_pixel`
    /// samples, each cell's plain mean over every source sample it received.
    /// A cell no band ever covered (a caller that fed no bands, or one that
    /// skipped part of the crop) reads back `0` rather than dividing by
    /// zero.
    pub fn finish(self) -> Vec<u16> {
        let spp = self.samples_per_pixel as usize;
        let mut out = vec![0u16; self.sums.len()];
        for (cell, &count) in self.counts.iter().enumerate() {
            if count == 0 {
                continue;
            }
            for c in 0..spp {
                out[cell * spp + c] =
                    (self.sums[cell * spp + c] / u64::from(count)).min(u64::from(u16::MAX)) as u16;
            }
        }
        out
    }
}
