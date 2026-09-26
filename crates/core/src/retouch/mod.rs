// SPDX-License-Identifier: GPL-3.0-or-later

//! Applying an edit op to a Z-Map's indices. A stroke paints `u16` source
//! indices into the region it covers and touches nothing else -- the
//! document this module edits is the map, never an image.

pub mod brush;

use crate::tile::Rect;

pub use brush::{apply, footprint};

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

    /// The index buffer handed to `apply` does not match the tile's area.
    #[error("index buffer holds {actual} entries, expected {expected}")]
    TileBufferSize { expected: usize, actual: usize },
}
