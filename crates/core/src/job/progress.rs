// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobProgress`], the plain snapshot [`super::JobControl::progress`]
//! returns by value.

use super::JobStage;

/// A polled snapshot of one job's progress. **Polled, not pushed**: a
/// callback would run caller code on a rayon worker thread (reentrancy into
/// the scheduler, or a GUI repainting from a worker), so `core` hands this
/// back by value and never invokes anything with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JobProgress {
    pub stage: JobStage,
    pub units_done: usize,
    pub unit_total: usize,
    /// How many frames' registration has completed during the align stage.
    /// `0` outside a computed alignment (no align stage, a cached alignment,
    /// or a resumed one) -- the counter is never reset once the stage ends,
    /// so a snapshot taken during a later stage still carries the align
    /// stage's own final tally.
    pub align_done: usize,
    /// The frame count a computed align stage started with, `0` when no
    /// frame is being counted.
    pub align_total: usize,
    /// How many units of brightness equalization have completed during the
    /// prepare stage: strips read, or frames for a RAW source. `0` when the
    /// run does not equalize; like `align_done` it keeps its final tally once
    /// the stage ends.
    pub prepare_done: usize,
    /// The unit count the prepare stage started with, `0` when the run does
    /// not equalize.
    pub prepare_total: usize,
    /// How many RAW frames have been developed into scratch during the
    /// develop stage. `0` on a run without the pass; like `align_done` it
    /// keeps its final tally once the stage ends.
    pub develop_done: usize,
    /// The frame count the develop stage started with, `0` on a run without
    /// the pass.
    pub develop_total: usize,
}
