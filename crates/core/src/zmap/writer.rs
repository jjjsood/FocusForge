// SPDX-License-Identifier: GPL-3.0-or-later

//! [`ZMapWriter`]: a Z-Map written tile by tile through the existing,
//! tag-frozen [`crate::io::TiffWriter`] -- never a new format, never a new
//! tag.

use std::fs::File;
use std::io::{Seek, Write};
use std::path::Path;

use crate::io::{TiffSpec, TiffWriter};
use crate::tile::Rect;

use super::description::format_description;
use super::{ZMapError, ZMapInfo, validate_tile};

/// Writes a Z-Map tile by tile as a single-channel 16-bit TIFF, with its
/// frame count carried in `ImageDescription` under the pinned grammar.
/// Generic over the sink for the same reason [`TiffWriter`] is: a
/// `Cursor<Vec<u8>>` sink makes a unit test a round trip with no
/// filesystem.
pub struct ZMapWriter<W: Write + Seek> {
    inner: TiffWriter<W>,
    info: ZMapInfo,
}

impl ZMapWriter<File> {
    /// Create `path` and write its header now, ready for
    /// [`write_tile`](Self::write_tile) calls.
    pub fn create(path: &Path, info: ZMapInfo) -> Result<Self, ZMapError> {
        let inner = TiffWriter::create(
            path,
            TiffSpec {
                width: info.width(),
                height: info.height(),
                samples_per_pixel: 1,
                description: Some(format_description(info.frame_count())),
            },
        )?;
        Ok(ZMapWriter { inner, info })
    }

    /// Reopens `path` -- delegates to [`TiffWriter::reopen`], re-deriving
    /// the expected `ImageDescription` through
    /// [`format_description`]`(info.frame_count())` so the description
    /// check inside `reopen` is the exact same one [`Self::create`] wrote.
    pub fn reopen(path: &Path, info: ZMapInfo) -> Result<Self, ZMapError> {
        let inner = TiffWriter::reopen(
            path,
            TiffSpec {
                width: info.width(),
                height: info.height(),
                samples_per_pixel: 1,
                description: Some(format_description(info.frame_count())),
            },
        )?;
        Ok(ZMapWriter { inner, info })
    }
}

impl<W: Write + Seek> ZMapWriter<W> {
    /// Write `indices` (one `u16` per pixel of `rect`, `rect.width *
    /// rect.height` long) into the map. Validated against the map's frame
    /// count *before* delegating to [`TiffWriter::write_tile`], which owns
    /// the out-of-bounds and wrong-length refusals.
    pub fn write_tile(&mut self, rect: Rect, indices: &[u16]) -> Result<(), ZMapError> {
        validate_tile(indices, self.info.frame_count())?;
        self.inner.write_tile(rect, indices)?;
        Ok(())
    }

    /// Delegates to [`TiffWriter::seed_coverage`] -- marks `rect`'s rows as
    /// already covered on a reopened writer, without writing any bytes.
    pub(crate) fn seed_coverage(&mut self, rect: Rect) {
        self.inner.seed_coverage(rect);
    }

    /// Delegates to [`TiffWriter::finish`], which already refuses a
    /// partially covered frame with `TiffError::IncompleteFrame` -- this
    /// writer does not reimplement that refusal, only inherits it through
    /// `#[from]`.
    pub fn finish(self) -> Result<(), ZMapError>
    where
        W: 'static,
    {
        self.inner.finish()?;
        Ok(())
    }
}
