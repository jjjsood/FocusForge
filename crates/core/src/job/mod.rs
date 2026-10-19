// SPDX-License-Identifier: GPL-3.0-or-later

//! Pause, resume and cancel over the tile scheduler's fuse-tile units.
//!
//! # What a unit is
//!
//! One unit is one whole fuse tile -- one [`crate::tile::PlannedTile`], the
//! same thing [`crate::tile::TileScheduler`] already hands its consumer once
//! per `par_iter` item. Never a frame, never a pyramid level: a frame is
//! tens of seconds of work at reference scale, and a tile is what makes
//! pause/cancel land in a bounded, small amount of further work instead.
//! Per-frame alignment is *not* a job unit -- [`JobStage::Align`] is
//! reported, never controlled, and a pause or cancel requested during
//! alignment takes effect at the first fuse unit.
//!
//! # The seam, and why there is only one
//!
//! [`crate::tile::TileControl`] is consulted as the first statement of every
//! tile's work, in [`crate::tile::TileScheduler::run_tiles_controlled`],
//! before the tile counts as in flight, and notified again
//! ([`crate::tile::TileControl::unit_completed`]) the moment that tile has
//! finished. Control in, progress out, over the one trait object the
//! scheduler already carries -- which is why [`JobProgress`] advances during
//! a run rather than in one jump when the segment ends. [`JobControl`] is the
//! only `TileControl` this crate ships outside tests -- pause, resume and
//! cancel are all expressed as writes to the one atomic flag it wraps, and
//! `should_stop` is the one place that flag is read back. A second seam (inside
//! `TileConsumer::consume`, for a finer-grained pause) is deliberately not
//! added: everything a consumer does happens inside one `consume` call this
//! module cannot see into, and nothing on this branch needs finer latency
//! than one tile.
//!
//! # The pause bound
//!
//! See [`crate::tile::TileScheduler::run_tiles_controlled`]'s rustdoc for
//! the full derivation: once `pause()`'s store has returned, at most
//! `TileScheduler::in_flight_limit()` further units complete, and exactly at
//! most 1 when the scheduler is budgeted down to a single worker.
//!
//! # Why a cancel preserves state
//!
//! A clean cancel is not an error, so [`Job::run`] calls
//! [`crate::tile::TileConsumer::abort`] only on a genuine `Err(TileError)`,
//! never on `Paused` or `Cancelled`. The `.partial` output file a caller
//! (`crates/cli`) is writing to survives on purpose: it is the resumable
//! state a later `run_tiles` call or the resume ledger continues from.
//! `core::job` neither creates nor renames output files.
//!
//! # Data, not presentation
//!
//! `core` never prints and never depends on a terminal. [`JobProgress`] is a
//! plain value a caller polls; nothing here is a callback, so nothing here
//! ever runs caller code on a rayon worker thread.

pub mod control;
pub mod fingerprint;
pub mod ledger;
pub mod progress;
pub mod record;
pub mod stage_cache;

pub use control::{ControlState, JobControl};
pub use fingerprint::{FingerprintMismatch, FrameCount, FrameFingerprint, StackFingerprint};
pub use ledger::{Ledger, RecordingConsumer, Resumable, ResumeError, ResumeStore, read_alignment};
pub use progress::JobProgress;
pub use record::{ALIGN_SOLVER_VERSION, AlignIdentity, DRIVER_POLICY_FIELDS, JobRecord};
pub use stage_cache::{
    CachedAlignment, CachedStage, FrameKey, StageCache, StageCacheError, StageKey,
};

use std::collections::HashSet;
use std::sync::Arc;

use crate::tile::plan::PlannedTile;
use crate::tile::{FramePreprocess, TileConsumer, TileError, TileScheduler};

/// A job's current phase, reported as data.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStage {
    /// Brightness preparation of the sources, before alignment: reported,
    /// never controlled.
    Prepare,
    /// Per-frame alignment: reported, never controlled.
    Align,
    /// Fuse-tile units: the only stage [`Job::run`] itself drives.
    Fuse,
}

impl JobStage {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            JobStage::Align => 0,
            JobStage::Fuse => 1,
            JobStage::Prepare => 2,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => JobStage::Align,
            1 => JobStage::Fuse,
            2 => JobStage::Prepare,
            _ => unreachable!("JobStage is only ever written through JobControl::set_stage"),
        }
    }
}

/// How one [`Job::run`] segment ended. Returned as `Ok`, always --
/// `Err(TileError)` means failure and nothing else, which is what makes
/// "cancel is not an error" structural rather than a convention.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// Every submitted unit ran; nothing was stopped early.
    Completed,
    /// A pause was observed; `remaining` units are unrun. Resume with
    /// `control.clear()` then [`Job::run`] again, same consumer.
    Paused,
    /// A cancel was observed; `remaining` units are unrun. The consumer's
    /// output sink was left exactly as it was -- neither finished nor
    /// aborted.
    Cancelled,
}

/// A job: the scheduler's fuse-tile units, plus the control handle a
/// different thread can use to request pause, resume or cancel.
///
/// `Job::new` seeds `remaining` from the scheduler's whole plan;
/// [`Job::run`] runs one **segment** -- a single
/// `run_tiles_controlled` call over whatever is left -- and resume is
/// `control.clear()` then `job.run(...)` again with the *same* consumer
/// instance, so one `TiffWriter` and one coverage tracker survive the
/// pause and `finish()` after the last segment refuses exactly as it would
/// after an uninterrupted run.
pub struct Job<'a> {
    scheduler: &'a TileScheduler<'a>,
    remaining: Vec<PlannedTile>,
    control: Arc<JobControl>,
}

impl<'a> Job<'a> {
    /// A fresh job over the scheduler's whole plan -- exactly
    /// [`Self::resuming`] with an empty `completed` set, since a fresh job
    /// is the degenerate case of a resume that has nothing to skip.
    pub fn new(scheduler: &'a TileScheduler<'a>) -> Self {
        Self::resuming(scheduler, &[])
    }

    /// Builds a job that resumes from a fresh process, over a control this
    /// call creates -- exactly [`Self::with_control`] with a fresh
    /// [`JobControl`], since most callers have no reason to hold a control
    /// before the job it belongs to exists.
    pub fn resuming(scheduler: &'a TileScheduler<'a>, completed: &[usize]) -> Self {
        Self::with_control(scheduler, completed, JobControl::new())
    }

    /// Builds a job over a `control` the caller already holds, so a caller
    /// driving an alignment stage can call [`JobControl::set_stage`] on it
    /// before this job exists. Same totals as [`Self::new`] with an empty
    /// `completed` set, except `remaining` excludes the completed tiles and
    /// `control` is pre-credited with their units and pixels, so
    /// `units_done`/`eta` stay honest across a restart.
    pub fn with_control(
        scheduler: &'a TileScheduler<'a>,
        completed: &[usize],
        control: Arc<JobControl>,
    ) -> Self {
        let tiles = scheduler.plan().tiles();
        let unit_total = tiles.len();
        let pixel_total: u64 = tiles.iter().map(tile_area).sum();

        control.set_unit_total(unit_total);
        control.set_pixel_total(pixel_total);

        let completed_set: HashSet<usize> = completed.iter().copied().collect();
        let remaining: Vec<PlannedTile> = tiles
            .iter()
            .filter(|tile| !completed_set.contains(&tile.index()))
            .copied()
            .collect();
        let completed_pixels: u64 = tiles
            .iter()
            .filter(|tile| completed_set.contains(&tile.index()))
            .map(tile_area)
            .sum();
        control.add_progress(completed_set.len(), completed_pixels);

        Job {
            scheduler,
            remaining,
            control,
        }
    }

    /// The `Arc`-shared control handle: clone it onto another thread to
    /// pause, resume or cancel this job, or to poll its progress, while
    /// [`Self::run`] is executing.
    pub fn control(&self) -> Arc<JobControl> {
        Arc::clone(&self.control)
    }

    /// Runs one segment: every unit not yet completed, until either every
    /// one of them runs, or a pause/cancel is observed. Resume is
    /// `control.clear()` then calling this again with the same `consumer`.
    pub fn run(
        &mut self,
        preprocess: &dyn FramePreprocess,
        consumer: &dyn TileConsumer,
    ) -> Result<JobOutcome, TileError> {
        // The running clock is bracketed rather than measured afterwards, so
        // `JobControl::progress` can add this segment's elapsed time to the
        // accumulated total while the segment is still executing -- an ETA
        // that only appeared after the run finished would be useless, and a
        // `running` of zero against a non-zero `pixels_done` renders as
        // `eta 0s`.
        self.control.begin_segment();
        let result = self.scheduler.run_tiles_controlled(
            &self.remaining,
            preprocess,
            consumer,
            self.control.as_ref(),
        );
        self.control.end_segment();

        let controlled = match result {
            Ok(controlled) => controlled,
            Err(e) => {
                // A genuine error, and only a genuine error, runs cleanup.
                consumer.abort();
                return Err(e);
            }
        };

        // No `add_progress` here: every unit this segment ran already
        // credited itself through `JobControl::unit_completed` via the
        // scheduler's own per-tile bookkeeping, so counting them again here
        // would double them.
        let completed_set: HashSet<usize> = controlled.completed.iter().copied().collect();
        self.remaining
            .retain(|tile| !completed_set.contains(&tile.index()));

        if !controlled.stopped {
            return Ok(JobOutcome::Completed);
        }

        // The stop was observed via `JobControl::should_stop`, so the
        // control state at this point names why -- `CancelRequested` picks
        // `Cancelled`; anything else (in practice, `PauseRequested`) picks
        // `Paused`, the conservative, non-destructive reading.
        Ok(match self.control.state() {
            ControlState::CancelRequested => JobOutcome::Cancelled,
            ControlState::PauseRequested | ControlState::Running => JobOutcome::Paused,
        })
    }
}

fn tile_area(tile: &PlannedTile) -> u64 {
    let rect = tile.read();
    rect.width as u64 * rect.height as u64
}
