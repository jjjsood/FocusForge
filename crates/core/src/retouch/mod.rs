// SPDX-License-Identifier: GPL-3.0-or-later

//! Applying an edit op to a Z-Map's indices. A stroke paints `u16` source
//! indices into the region it covers and touches nothing else -- the
//! document this module edits is the map, never an image.

pub mod brush;
pub mod recomposite;
pub mod replay;
pub mod session;
pub mod zmap_io;

use crate::project::Point;
use crate::tile::Rect;
use crate::zmap::ZMapError;

pub use brush::{apply, footprint};
pub use recomposite::{Sources, named_frames, recomposite, recomposite_bands};
pub use replay::{replay, replay_tile};
pub use session::EditSession;
pub use zmap_io::{session_tile, write_edited};

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

    /// A recomposite's source reader failed to produce a frame's pixels.
    #[error(transparent)]
    Source(#[from] crate::algorithm::AlgorithmError),

    /// A [`crate::project::Op::ResetRegion`] rect lies outside the map --
    /// including an empty rect.
    #[error("reset region {rect:?} lies outside the map")]
    RegionOutsideMap { rect: Rect },

    /// A [`crate::project::Op::FloodFill`] seed lies outside the map.
    #[error("flood fill seed {seed:?} lies outside the map")]
    SeedOutsideMap { seed: Point },

    /// [`crate::project::Op::FloodFill`] has no defined replay semantics: no
    /// editor control produces its `tolerance` field.
    #[error("flood-fill has no defined replay semantics")]
    FloodFillNotReplayable,

    /// [`crate::project::Op::Refuse`] is never replayed from an op list -- a
    /// re-fuse is recorded as its own checkpoint via `append`.
    #[error("re-fuse needs its sources; it is recorded as its own checkpoint")]
    RefuseNotReplayable,

    /// Reading the nearest fused ancestor's map while replaying a
    /// [`crate::project::Op::ResetRegion`] failed.
    #[error("reading the fused ancestor's map: {0}")]
    Fused(#[from] ZMapError),
}
