// SPDX-License-Identifier: GPL-3.0-or-later

//! The soft brush stroke blended over an image from a source frame.

pub mod blend;

use crate::tile::Rect;
use crate::zmap::ZMapError;

pub use blend::{footprint, overlay_colors, top_source};

/// Failures validating or applying an op against a Z-Map.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RetouchError {
    /// `source_id` names no frame the map claims to have -- including
    /// [`crate::zmap::NO_SOURCE`], which a stroke never writes.
    #[error("source_id {source_id} is out of range for a {frame_count}-frame map")]
    SourceOutOfRange { source_id: u16, frame_count: u32 },

    /// `radius` is not finite or is `<= 0.0`.
    #[error("radius {radius} is not a positive finite value")]
    InvalidRadius { radius: f32 },

    /// `softness` is outside `0.0..=1.0` (also true for `NaN`).
    #[error("softness {softness} is not in 0.0..=1.0")]
    InvalidSoftness { softness: f32 },

    /// A stroke's path holds no points.
    #[error("path is empty")]
    EmptyPath,

    /// An op other than a brush stroke was handed to this apply path.
    #[error("this op is not supported here")]
    Unsupported,

    /// The tile handed to `apply` is not fully inside the map's extent.
    #[error("tile {tile:?} lies outside the map's extent")]
    TileOutsideMap { tile: Rect },

    /// A sample buffer handed to `apply` does not match its rect's area.
    #[error("sample buffer holds {actual} samples, expected {expected}")]
    TileBufferSize { expected: usize, actual: usize },

    /// A source reader failed to produce a frame's pixels.
    #[error(transparent)]
    Source(#[from] crate::algorithm::AlgorithmError),

    /// Reading a Z-Map failed.
    #[error("reading a Z-Map: {0}")]
    Fused(#[from] ZMapError),
}
