// SPDX-License-Identifier: GPL-3.0-or-later

//! [`ZMapReader`]: opens a Z-Map through the existing, mmap-backed
//! [`crate::io::TiffStack`] and validates it against the frame count its own
//! `ImageDescription` claims. `open` never scans the whole map -- validation
//! is per-tile on [`ZMapReader::read_tile_into`], plus the explicit opt-in
//! full scan [`ZMapReader::validate_all`].

use std::num::NonZeroU32;
use std::path::Path;

use crate::io::TiffStack;
use crate::tile::Rect;

use super::description::parse_description;
use super::{ZMapError, ZMapInfo, validate_tile};

/// A Z-Map opened for tile-wise reading.
#[derive(Debug)]
pub struct ZMapReader {
    stack: TiffStack,
    info: ZMapInfo,
}

impl ZMapReader {
    /// Open `path` as a single-file stack (which enforces the mmap fast
    /// path), check it is single-channel, and parse its frame count out of
    /// its authoritative `ImageDescription`.
    pub fn open(path: &Path) -> Result<Self, ZMapError> {
        let stack = TiffStack::open_files(&[path.to_path_buf()])?;
        let header = stack.header(0);

        if header.samples_per_pixel != 1 {
            return Err(ZMapError::NotSingleChannel {
                path: header.path.clone(),
                samples_per_pixel: header.samples_per_pixel,
            });
        }

        let frame_count = parse_description(&header.path, header.image_description.as_deref())?;

        let (width, height) = stack.dimensions();
        let info = ZMapInfo::new(width, height, frame_count)?;

        Ok(ZMapReader { stack, info })
    }

    pub fn info(&self) -> ZMapInfo {
        self.info
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.info.width(), self.info.height())
    }

    pub fn frame_count(&self) -> u32 {
        self.info.frame_count()
    }

    /// Read `rect`'s indices into `out` (exactly `rect.width * rect.height`
    /// `u16`s), then validate them against the map's frame count. No
    /// `read_tile -> Tile` convenience: the caller owns the buffer, and this
    /// one accessor is what keeps the "no whole map" grep criterion honest.
    pub fn read_tile_into(&self, rect: Rect, out: &mut [u16]) -> Result<(), ZMapError> {
        self.read_tile_step_into(rect, NonZeroU32::MIN, out)
    }

    /// Read every `step`-th row and column of `rect` into `out` (exactly
    /// `rect.width.div_ceil(step) * rect.height.div_ceil(step)` `u16`s), then
    /// validate them. Decimation picks indices, never blends them, so every
    /// value returned is one the map holds.
    pub fn read_tile_step_into(
        &self,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), ZMapError> {
        self.stack.read_tile_step_into(0, rect, step, out)?;
        validate_tile(out, self.info.frame_count())?;
        Ok(())
    }

    /// The explicit opt-in full scan: walks full-width bands of
    /// `rows_per_pass` rows through one reused buffer, first offender wins.
    /// `rows_per_pass == 0` is [`ZMapError::RowsPerPassZero`]. Never called
    /// by [`Self::open`] -- eager whole-map validation would be exactly the
    /// whole-frame materialization this type exists to forbid.
    pub fn validate_all(&self, rows_per_pass: u32) -> Result<(), ZMapError> {
        if rows_per_pass == 0 {
            return Err(ZMapError::RowsPerPassZero);
        }

        let width = self.info.width();
        let height = self.info.height();
        let frame_count = self.info.frame_count();
        let mut buf = vec![0u16; width as usize * rows_per_pass as usize];

        let mut y = 0u32;
        while y < height {
            let band_height = rows_per_pass.min(height - y);
            let rect = Rect {
                x: 0,
                y,
                width,
                height: band_height,
            };
            let len = width as usize * band_height as usize;
            self.stack.read_tile_into(0, rect, &mut buf[..len])?;
            validate_tile(&buf[..len], frame_count)?;
            y += band_height;
        }
        Ok(())
    }
}
