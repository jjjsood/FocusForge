// SPDX-License-Identifier: GPL-3.0-or-later

//! Failures constructing, writing, reading or validating a Z-Map.
//!
//! Modelled file-for-file on `crate::tile::TileError` and on the path
//! convention stated at `crate::io::error`: a variant
//! that concerns one *file* carries `path: PathBuf`, a variant that concerns
//! one *value* does not. `TiffError` is not reused for index validation --
//! "index 97 names no frame in a 61-frame map" is not a TIFF failure -- but
//! the reader's own I/O and structural failures come straight through via
//! `#[from]`.

use std::path::PathBuf;

/// Everything that can go wrong building a [`super::ZMapInfo`], writing a
/// Z-Map with [`super::ZMapWriter`], or reading one back with
/// [`super::ZMapReader`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ZMapError {
    /// An index does not name any frame the map claims to have -- the
    /// reserved [`super::NO_SOURCE`] value aside. No `path`: the caller
    /// already knows which file or buffer it handed in (same shape as
    /// `TiffError::BufferSize`).
    #[error("index {index} is out of range for a {frame_count}-frame map")]
    IndexOutOfRange { index: u16, frame_count: u32 },

    /// A [`super::ZMapInfo`] was requested over a zero-area extent.
    #[error("extent {width}x{height} is empty")]
    EmptyExtent { width: u32, height: u32 },

    /// A [`super::ZMapInfo`] was requested with a frame count of `0` or
    /// greater than [`super::MAX_FRAMES`].
    #[error("frame_count {frame_count} is out of range (must be 1..={max})")]
    FrameCountOutOfRange { frame_count: u32, max: u32 },

    /// A Z-Map file has no `ImageDescription` tag at all.
    #[error("{}: no ImageDescription tag -- not a Z-Map", path.display())]
    MissingDescription { path: PathBuf },

    /// A Z-Map file's `ImageDescription` does not start with the pinned
    /// `FocusForge zmap v1` prefix.
    #[error("{}: description {description:?} is not a Z-Map description", path.display())]
    NotAZMap { path: PathBuf, description: String },

    /// A Z-Map file's description has the pinned prefix but no `frames=`
    /// token.
    #[error("{}: description {description:?} has no frames= token", path.display())]
    MissingFrameCount { path: PathBuf, description: String },

    /// A Z-Map file's `frames=` token does not parse as a `u32` within
    /// `1..=MAX_FRAMES`.
    #[error("{}: frames= token {token:?} does not parse as a valid frame count", path.display())]
    BadFrameCount { path: PathBuf, token: String },

    /// A composite (or any file with `samples_per_pixel != 1`) opened as a
    /// Z-Map.
    #[error(
        "{}: samples_per_pixel={samples_per_pixel}, a Z-Map is single-channel",
        path.display()
    )]
    NotSingleChannel {
        path: PathBuf,
        samples_per_pixel: u32,
    },

    /// [`super::ZMapReader::validate_all`] was called with `rows_per_pass ==
    /// 0`. Refused, never clamped -- same discipline as
    /// `TileError::TileSizeZero`.
    #[error("rows_per_pass must be non-zero")]
    RowsPerPassZero,

    /// A reader or writer failure from the underlying TIFF layer -- the only
    /// `#[from]` here, so `?` carries a reader failure out unchanged.
    #[error(transparent)]
    Tiff(#[from] crate::io::TiffError),
}
