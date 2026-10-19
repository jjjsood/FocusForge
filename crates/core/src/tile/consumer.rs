// SPDX-License-Identifier: GPL-3.0-or-later

//! The scheduler-owned consumer interface: `TileConsumer`, not
//! `&dyn StackAlgorithm`.
//!
//! `&self`, never `&mut self` -- the consumer owns its own output sink and
//! its own interior mutability, forced by `TiffWriter::write_tile` taking
//! `&mut self` and `finish` consuming it: parallel tiles cannot write
//! concurrently, so something must own the serialization point, and that
//! something is the consumer, never the scheduler.

use std::path::Path;

use crate::algorithm::{AlgorithmError, TileCtx};
use crate::tile::Rect;
use crate::tile::plan::PlannedTile;

/// Everything one call to [`TileConsumer::consume`] sees: the planned tile
/// (its interior and read rect), the per-frame tiles for that rect, and the
/// run's scratch directory.
pub struct TileJob<'a> {
    tile: &'a PlannedTile,
    ctx: TileCtx<'a>,
    scratch: &'a Path,
}

impl<'a> TileJob<'a> {
    pub fn new(tile: &'a PlannedTile, ctx: TileCtx<'a>, scratch: &'a Path) -> Self {
        TileJob { tile, ctx, scratch }
    }

    /// A job for a consumer that declared [`TileConsumer::supplies_own_pixels`]
    /// `true`: no frame-tile was read by the scheduler, so the context is
    /// built over an empty tile slice. A consumer built this way must not
    /// call [`Self::ctx`] -- an empty context's `frame(0)` is a named
    /// [`crate::algorithm::AlgorithmError::FrameOutOfRange`]
    /// (`{ frame: 0, frame_count: 0 }`), never a panic, but it is never the
    /// right answer for a pixel-supplying consumer either.
    pub fn without_frame_tiles(tile: &'a PlannedTile, scratch: &'a Path) -> Self {
        TileJob {
            tile,
            ctx: TileCtx::new(tile.read(), &[]),
            scratch,
        }
    }

    pub fn tile(&self) -> &PlannedTile {
        self.tile
    }

    pub fn ctx(&self) -> &TileCtx<'a> {
        &self.ctx
    }

    /// The rectangle this job's tile alone owns in the assembled output.
    pub fn interior(&self) -> Rect {
        self.tile.interior()
    }

    /// The rectangle read from every frame to fuse [`Self::interior`]
    /// without a seam.
    pub fn read_rect(&self) -> Rect {
        self.tile.read()
    }

    /// The run-scoped scratch directory, shared by every tile of this run.
    pub fn scratch(&self) -> &Path {
        self.scratch
    }
}

/// The scheduler-owned sink a [`super::scheduler::TileScheduler`] calls
/// once per tile, with every frame's tile for that rect already read and
/// preprocessed.
pub trait TileConsumer: Send + Sync {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError>;

    /// Called at most once, only when a run ends in a genuine error --
    /// **never on pause, never on cancel**. `crate::job::Job::run`
    /// is this hook's only caller. The default is a no-op, which is exactly
    /// right for a consumer with no resource worth releasing early; a
    /// consumer that owns an output sink (`crate::fuse::FusionConsumer`)
    /// overrides it to take that sink out of its mutex and drop it, so a
    /// later `finish()` cannot be attempted against a half-covered file.
    fn abort(&self) {}

    /// `true` when this consumer pulls its own pixels (a GPU backend's
    /// footprint-grown source rects, never the scheduler's own destination
    /// read rect) instead of relying on the scheduler's host read loop.
    /// Default `false`. A consumer answering `true` is handed a
    /// [`TileJob::without_frame_tiles`] and must not call [`TileJob::ctx`].
    fn supplies_own_pixels(&self) -> bool {
        false
    }
}
