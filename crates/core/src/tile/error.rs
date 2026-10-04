// SPDX-License-Identifier: GPL-3.0-or-later

//! Failures the tile scheduler and its tiling plan can produce.
//!
//! Modelled on `crate::io::error` and `crate::algorithm::error`: every
//! variant names its offending value. A module-owned error, not a new
//! `AlgorithmError` variant -- `AlgorithmError` is what an *algorithm*
//! returns, and the scheduler is the algorithm's caller: it wraps, it does
//! not join.

use std::path::PathBuf;

use crate::algorithm::AlgorithmError;
use crate::tile::Rect;

/// Everything that can go wrong building a [`super::plan::TilePlan`] or
/// running [`super::scheduler::TileScheduler`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TileError {
    /// A tiling plan was requested over a zero-area extent.
    #[error("extent {width}x{height} is empty")]
    EmptyExtent { width: u32, height: u32 },

    /// A tiling plan was requested with `tile_size == 0`.
    #[error("tile_size must be non-zero")]
    TileSizeZero,

    /// `levels` exceeds [`super::plan::MAX_LEVELS`], the cap that keeps
    /// `1u32 << levels` from overflowing.
    #[error("levels {levels} exceeds the maximum of {max}")]
    LevelsTooDeep { levels: u32, max: u32 },

    /// The requested tile size cannot fit a nonempty interior once the
    /// margin for `levels` is subtracted from both sides, **or** is smaller
    /// than the geometry that margin was measured in. Never auto-grown and
    /// never silently clamped. `min_tile_size` is
    /// `max(2 * margin + 2^levels, 4 * margin + 64)` for a nonzero margin:
    /// the smaller term alone is not enough, because `2 * margin + 2^levels`
    /// can be smaller than the `4 * margin + 64` buffer the margin table
    /// was actually measured against, and that direction needs *more*
    /// margin, not less. The message spells out both ways out, since a
    /// 512 px tile at `levels = 7` is the first combination a reader hits:
    /// use a tile size of at least `min_tile_size`, or ask for fewer
    /// levels.
    #[error(
        "tile_size {tile_size} leaves no interior at margin {margin} for levels={levels}; \
         use tile_size >= {min_tile_size}, or request fewer levels"
    )]
    NoInterior {
        tile_size: u32,
        margin: u32,
        levels: u32,
        min_tile_size: u32,
    },

    /// The consumer's `consume` call failed for a tile, including a failed
    /// read of one of its frame-tiles.
    #[error("consumer failed for tile {rect:?}: {source}")]
    Consumer {
        rect: Rect,
        #[source]
        source: AlgorithmError,
    },

    /// Creating or removing the run-scoped scratch directory failed.
    #[error("scratch directory {}: {source}", path.display())]
    Scratch {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Building the scheduler-owned rayon thread pool failed.
    #[error("building the tile scheduler's thread pool: {source}")]
    ThreadPool {
        #[source]
        source: rayon::ThreadPoolBuildError,
    },

    /// [`crate::tile::Tile::crop_into`] was asked for a rect that is not
    /// fully inside the tile it is cropping from. Never clamped -- same
    /// discipline as `TiffStack::read_tile_into`.
    #[error("crop rect {rect:?} is outside tile {tile:?}")]
    CropOutOfBounds { rect: Rect, tile: Rect },

    /// [`crate::tile::Tile::crop_into`]'s output buffer is the wrong length
    /// for the requested rect.
    #[error("crop buffer size mismatch: expected {expected} u16 samples, got {got}")]
    CropBufferSize { expected: usize, got: usize },

    /// A host-derived, uncapped memory budget (`HostBudget::refuses_below_
    /// one_tile`) is smaller than one tile -- refused by name rather
    /// than silently floored or deadlocked. An
    /// explicitly supplied budget or a `--max-host-memory` ceiling still
    /// floors at 1 and runs -- this variant is host-derived-only. The
    /// message deliberately does not name `--max-host-memory`, which only
    /// lowers the budget and can never fix this.
    #[error(
        "the host-derived memory budget of {budget_bytes} B is smaller than one tile \
         ({bytes_per_tile} B at tile_size {tile_size}); free memory on this machine or use a \
         smaller tile_size"
    )]
    HostMemoryTooSmall {
        budget_bytes: u64,
        bytes_per_tile: u64,
        tile_size: u32,
    },
}
