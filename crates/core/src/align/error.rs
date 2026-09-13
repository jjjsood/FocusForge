// SPDX-License-Identifier: GPL-3.0-or-later

//! One module-owned error for `crate::align`.
//!
//! Modelled on `crate::tile::error` and `crate::fuse::error`: every variant
//! names its offending value. `#[non_exhaustive]` so new variants can be
//! added without a breaking change.

use thiserror::Error;

use crate::algorithm::AlgorithmError;
use crate::tile::Rect;

/// Everything that can go wrong building, inverting or applying a
/// [`super::Transform`], or running [`super::warp`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AlignError {
    /// [`super::Transform::inverse`] was called on a matrix whose
    /// determinant is (numerically) zero.
    #[error("transform is not invertible: determinant {det} is numerically zero")]
    NonInvertible { det: f64 },

    /// A destination rect's corners do not all lie in the positive-`w`
    /// half-space, so its source footprint has no finite bounding box.
    #[error("destination rect {rect:?} has a corner beyond the horizon (w <= 0)")]
    HorizonCrossesRect { rect: Rect },

    /// The pure warp was handed a source [`crate::tile::Tile`] that does not
    /// contain the taps a covered destination pixel needs. A caller bug,
    /// never clamped -- the same discipline as
    /// [`crate::tile::TileError::CropOutOfBounds`].
    #[error(
        "source tile does not contain the taps a covered pixel needs: need {needed:?}, have {have:?}"
    )]
    SourceTileTooSmall { needed: Rect, have: Rect },

    /// An `_into` call's output buffer is the wrong length for the
    /// requested rect.
    #[error("buffer size mismatch: expected {expected} u16 samples, got {got}")]
    BufferSize { expected: usize, got: usize },

    /// A [`crate::algorithm::TileReader`] read failed.
    #[error("reading frame {frame} tile {rect:?}: {source}")]
    Read {
        frame: usize,
        rect: Rect,
        #[source]
        source: AlgorithmError,
    },
}
