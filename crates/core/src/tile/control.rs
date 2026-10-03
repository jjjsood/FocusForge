// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-unit control seam [`super::scheduler::TileScheduler`] consults at
//! the top of every work unit. Same shape as `FramePreprocess` /
//! `NoopPreprocess`, for the same reason: the scheduler stays typed on a
//! trait object, and the *policy* -- what "stop" means, who requested it --
//! lives in `core::job`, which `core::tile` never names.

/// How often a waiter re-reads [`TileControl::admission_cap`] while the cap is
/// the binding term; lifting a cap wakes nobody.
pub(crate) const ADMISSION_RECHECK: std::time::Duration = std::time::Duration::from_millis(10);

/// Consulted once, as the first statement of every tile's work
/// (`TileScheduler::run_tiles_controlled`), before the tile counts as in
/// flight. `true` means "do not start this unit; leave the run early
/// instead."
pub trait TileControl: Send + Sync {
    fn should_stop(&self) -> bool;

    /// Called once per unit that **finished successfully**, from the rayon
    /// worker that ran it, immediately after the same unit is recorded as
    /// complete. `pixels` is that unit's read-rect area
    /// (`PlannedTile::read()`'s `width * height`), the quantity
    /// `crate::job::JobProgress` weights its ETA by.
    ///
    /// The default is a no-op, so [`NoControl`] and every test control stay
    /// one-method implementations. `crate::job::JobControl` overrides it,
    /// and that override is the *only* reason a polled progress snapshot
    /// advances while a run is still in flight rather than jumping from
    /// `0/n` to `n/n` when the segment ends.
    ///
    /// Runs on a worker thread and must therefore stay cheap and
    /// non-blocking -- an atomic add, nothing that could re-enter the
    /// scheduler. It is never handed caller code: `core` polls, it never
    /// pushes (`crate::job::JobProgress`' own docs).
    fn unit_completed(&self, pixels: u64) {
        let _ = pixels;
    }

    /// The most units this run may hold in flight right now, read afresh on
    /// every admission check. It only ever lowers admission: the gate admits
    /// `min(its own limit, cap)`, so a cap never raises a limit and never
    /// masks a pressure back-off. `usize::MAX` means no cap.
    fn admission_cap(&self) -> usize {
        usize::MAX
    }
}

/// The control [`super::scheduler::TileScheduler::run`] and
/// `run_tiles` use: never stops.
pub struct NoControl;

impl TileControl for NoControl {
    fn should_stop(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_control_never_stops() {
        assert!(!NoControl.should_stop());
    }

    #[test]
    fn no_control_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoControl>();
    }
}
