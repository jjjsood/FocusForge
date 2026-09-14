// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-unit control seam [`super::scheduler::TileScheduler`] consults at
//! the top of every work unit. Same shape as `FramePreprocess` /
//! `NoopPreprocess`, for the same reason: the scheduler stays typed on a
//! trait object, and the *policy* -- what "stop" means, who requested it --
//! lives in `core::job`, which `core::tile` never names.

/// Consulted once, as the first statement of every tile's work
/// (`TileScheduler::run_tiles_controlled`), before the tile counts as in
/// flight. `true` means "do not start this unit; leave the run early
/// instead."
pub trait TileControl: Send + Sync {
    fn should_stop(&self) -> bool;
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
