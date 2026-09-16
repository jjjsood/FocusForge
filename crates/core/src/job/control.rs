// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobControl`]: the `Arc`-shared handle a different thread uses to
//! request pause, resume or cancel, and that a caller polls for progress.
//!
//! **One flag with three states, not two independent signals**: pause and
//! cancel both leave the running `try_for_each` early, differing only in
//! the [`super::JobOutcome`] [`super::Job::run`] reports. Every field is an
//! atomic, so `JobControl` is `Send + Sync` by construction -- no `unsafe
//! impl` anywhere in this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::tile::TileControl;

use super::JobStage;
use super::progress::{JobProgress, eta};

const RUNNING: u8 = 0;
const PAUSE_REQUESTED: u8 = 1;
const CANCEL_REQUESTED: u8 = 2;

/// `segment_start_nanos`' "no segment is executing right now" sentinel. A
/// real offset from [`JobControl::epoch`] can never reach it (it would take
/// 584 years), and a sentinel keeps the whole handle lock-free -- see
/// [`JobControl::progress`].
const NO_SEGMENT: u64 = u64::MAX;

/// The three states one job's control flag can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    Running,
    PauseRequested,
    CancelRequested,
}

impl ControlState {
    fn from_u8(v: u8) -> Self {
        match v {
            RUNNING => ControlState::Running,
            PAUSE_REQUESTED => ControlState::PauseRequested,
            CANCEL_REQUESTED => ControlState::CancelRequested,
            _ => unreachable!("JobControl's state is only ever written through its own setters"),
        }
    }
}

/// `Arc`-shared control-and-progress handle for one [`super::Job`].
#[derive(Debug)]
pub struct JobControl {
    state: AtomicU8,
    units_done: AtomicUsize,
    unit_total: AtomicUsize,
    pixels_done: AtomicU64,
    pixel_total: AtomicU64,
    stage: AtomicU8,
    /// Running time of every segment that has already **ended**. The
    /// currently executing segment's own elapsed time is added on the fly by
    /// [`Self::progress`], never stored here until [`Self::end_segment`].
    running_nanos: AtomicU64,
    /// Fixed reference point, so a live segment's elapsed time is expressible
    /// as two `u64` offsets rather than a lock around an `Instant`.
    epoch: Instant,
    /// Offset from [`Self::epoch`] at which the executing segment started, or
    /// [`NO_SEGMENT`] when none is executing.
    segment_start_nanos: AtomicU64,
}

impl JobControl {
    /// A fresh, running control with zero progress. `stage` starts at
    /// [`JobStage::Fuse`]: the job unit scope is fuse tiles only -- a caller
    /// that also drives an alignment stage calls [`Self::set_stage`] with
    /// [`JobStage::Align`] first and [`JobStage::Fuse`] again once fusion
    /// starts.
    pub fn new() -> Arc<Self> {
        Arc::new(JobControl {
            state: AtomicU8::new(RUNNING),
            units_done: AtomicUsize::new(0),
            unit_total: AtomicUsize::new(0),
            pixels_done: AtomicU64::new(0),
            pixel_total: AtomicU64::new(0),
            stage: AtomicU8::new(JobStage::Fuse.to_u8()),
            running_nanos: AtomicU64::new(0),
            epoch: Instant::now(),
            segment_start_nanos: AtomicU64::new(NO_SEGMENT),
        })
    }

    pub fn state(&self) -> ControlState {
        ControlState::from_u8(self.state.load(Ordering::SeqCst))
    }

    /// Request a pause. A no-op if the job is already paused or already has
    /// a cancel outstanding -- pause never downgrades a cancel, and pausing
    /// twice is not an error.
    ///
    /// # The pause bound
    ///
    /// Every worker tests [`Self::state`] (through
    /// [`crate::tile::TileControl::should_stop`]) at the top of every unit,
    /// before that unit counts as in flight. So once this call's store has
    /// returned, each worker already holding a unit can still finish the
    /// one it holds, but no worker can pick up another -- at most
    /// `TileScheduler::in_flight_limit()` further units complete, and
    /// exactly at most 1 when a scheduler is budgeted down to a single
    /// permit.
    pub fn pause(&self) {
        let _ = self.state.compare_exchange(
            RUNNING,
            PAUSE_REQUESTED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Request a cancel. Legal from `Running` *or* `PauseRequested` --
    /// "cancel a paused job" is one store into the same flag.
    pub fn cancel(&self) {
        self.state.store(CANCEL_REQUESTED, Ordering::SeqCst);
    }

    /// Back to `Running` -- what a resume calls before driving
    /// [`super::Job::run`] again. Idempotent: calling it while already
    /// `Running` is a no-op.
    pub fn clear(&self) {
        self.state.store(RUNNING, Ordering::SeqCst);
    }

    pub fn set_stage(&self, stage: JobStage) {
        self.stage.store(stage.to_u8(), Ordering::SeqCst);
    }

    /// A plain snapshot, returned by value -- polled, never pushed.
    ///
    /// Every field is **live**: `units_done`/`pixels_done` are credited by
    /// [`TileControl::unit_completed`] as each unit finishes, and `running`
    /// adds the executing segment's elapsed time to the accumulated total,
    /// so a caller polling from another thread watches the counters climb
    /// during the run rather than seeing them jump at the end of it.
    pub fn progress(&self) -> JobProgress {
        let units_done = self.units_done.load(Ordering::SeqCst);
        let unit_total = self.unit_total.load(Ordering::SeqCst);
        let pixels_done = self.pixels_done.load(Ordering::SeqCst);
        let pixel_total = self.pixel_total.load(Ordering::SeqCst);
        let running = self.running_now();
        JobProgress {
            stage: JobStage::from_u8(self.stage.load(Ordering::SeqCst)),
            units_done,
            unit_total,
            pixels_done,
            pixel_total,
            running,
            eta: eta(pixels_done, pixel_total, running),
        }
    }

    /// Seeds the whole job's totals -- called once, by [`super::Job::new`].
    /// `unit_total` never resets to a remaining count across segments:
    /// progress reads `done/total` of the *whole* job.
    pub(crate) fn set_unit_total(&self, total: usize) {
        self.unit_total.store(total, Ordering::SeqCst);
    }

    pub(crate) fn set_pixel_total(&self, total: u64) {
        self.pixel_total.store(total, Ordering::SeqCst);
    }

    /// Credits units and pixels a segment never ran itself -- the units a
    /// previous process already completed, seeded once by
    /// [`super::Job::resuming`]. Units completed by *this* process are
    /// credited one at a time by [`TileControl::unit_completed`] instead.
    pub(crate) fn add_progress(&self, units: usize, pixels: u64) {
        self.pixels_done.fetch_add(pixels, Ordering::SeqCst);
        self.units_done.fetch_add(units, Ordering::SeqCst);
    }

    /// Starts the running clock for one segment. Paired with
    /// [`Self::end_segment`] by [`super::Job::run`], which is the only
    /// caller; between the two, [`Self::progress`] reports the accumulated
    /// running time **plus** this segment's elapsed time so far.
    pub(crate) fn begin_segment(&self) {
        self.segment_start_nanos
            .store(self.epoch.elapsed().as_nanos() as u64, Ordering::SeqCst);
    }

    /// Folds the executing segment's own elapsed time into the accumulated
    /// total and stops the clock -- so time spent *paused* between two
    /// segments is never counted, which is what keeps the ETA honest across
    /// a pause.
    pub(crate) fn end_segment(&self) {
        let start = self.segment_start_nanos.swap(NO_SEGMENT, Ordering::SeqCst);
        if start == NO_SEGMENT {
            return;
        }
        let now = self.epoch.elapsed().as_nanos() as u64;
        self.running_nanos
            .fetch_add(now.saturating_sub(start), Ordering::SeqCst);
    }

    fn running_now(&self) -> Duration {
        let accumulated = self.running_nanos.load(Ordering::SeqCst);
        let start = self.segment_start_nanos.load(Ordering::SeqCst);
        if start == NO_SEGMENT {
            return Duration::from_nanos(accumulated);
        }
        let now = self.epoch.elapsed().as_nanos() as u64;
        Duration::from_nanos(accumulated.saturating_add(now.saturating_sub(start)))
    }
}

impl TileControl for JobControl {
    fn should_stop(&self) -> bool {
        self.state() != ControlState::Running
    }

    /// Credits one finished unit, from the rayon worker that finished it.
    /// **Pixels first, then units**: a poller that catches the two adds
    /// mid-flight then sees at worst "pixels counted, unit not yet", never
    /// the reverse -- so `units_done > 0` always implies `pixels_done > 0`,
    /// and therefore an `eta` that is `Some` (`super::progress::eta`
    /// returns `None` only while `pixels_done == 0`).
    fn unit_completed(&self, pixels: u64) {
        self.pixels_done.fetch_add(pixels, Ordering::SeqCst);
        self.units_done.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_control_is_running_with_zero_progress() {
        let control = JobControl::new();
        assert_eq!(control.state(), ControlState::Running);
        let p = control.progress();
        assert_eq!(p.units_done, 0);
        assert_eq!(p.unit_total, 0);
        assert_eq!(p.eta, None);
    }

    #[test]
    fn pause_moves_running_to_pause_requested() {
        let control = JobControl::new();
        control.pause();
        assert_eq!(control.state(), ControlState::PauseRequested);
    }

    #[test]
    fn pause_while_paused_is_a_no_op() {
        let control = JobControl::new();
        control.pause();
        control.pause();
        assert_eq!(control.state(), ControlState::PauseRequested);
    }

    #[test]
    fn pause_never_downgrades_an_outstanding_cancel() {
        let control = JobControl::new();
        control.cancel();
        control.pause();
        assert_eq!(control.state(), ControlState::CancelRequested);
    }

    #[test]
    fn cancel_from_paused_moves_straight_to_cancel_requested() {
        let control = JobControl::new();
        control.pause();
        control.cancel();
        assert_eq!(control.state(), ControlState::CancelRequested);
    }

    #[test]
    fn resume_while_running_is_a_no_op() {
        let control = JobControl::new();
        assert_eq!(control.state(), ControlState::Running);
        control.clear();
        assert_eq!(control.state(), ControlState::Running);
    }

    #[test]
    fn clear_returns_a_paused_or_cancelled_control_to_running() {
        let control = JobControl::new();
        control.pause();
        control.clear();
        assert_eq!(control.state(), ControlState::Running);

        control.cancel();
        control.clear();
        assert_eq!(control.state(), ControlState::Running);
    }

    #[test]
    fn should_stop_reflects_any_non_running_state() {
        let control = JobControl::new();
        assert!(!control.should_stop());
        control.pause();
        assert!(control.should_stop());
        control.clear();
        control.cancel();
        assert!(control.should_stop());
    }

    #[test]
    fn progress_accumulates_across_two_segments_with_a_constant_total() {
        let control = JobControl::new();
        control.set_unit_total(10);
        control.set_pixel_total(1000);

        control.add_progress(4, 400);
        let p1 = control.progress();
        assert_eq!(p1.unit_total, 10);
        assert_eq!(p1.units_done, 4);
        assert_eq!(p1.pixels_done, 400);

        control.add_progress(3, 300);
        let p2 = control.progress();
        assert_eq!(
            p2.unit_total, 10,
            "unit_total must never reset across segments"
        );
        assert_eq!(p2.units_done, 7);
        assert_eq!(p2.pixels_done, 700);
    }

    #[test]
    fn unit_completed_credits_one_unit_and_its_pixels_immediately() {
        let control = JobControl::new();
        control.set_unit_total(4);
        control.set_pixel_total(400);

        control.unit_completed(100);
        let p = control.progress();
        assert_eq!(p.units_done, 1);
        assert_eq!(p.pixels_done, 100);

        control.unit_completed(100);
        assert_eq!(control.progress().units_done, 2);
    }

    #[test]
    fn running_grows_inside_an_open_segment_and_freezes_once_it_ends() {
        let control = JobControl::new();
        assert_eq!(
            control.progress().running,
            Duration::ZERO,
            "no segment has been opened yet"
        );

        control.begin_segment();
        let first = control.progress().running;
        std::thread::sleep(Duration::from_millis(5));
        let second = control.progress().running;
        assert!(
            second > first,
            "an open segment's elapsed time must be visible to a poller: {first:?} -> {second:?}"
        );

        control.end_segment();
        let frozen = control.progress().running;
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            control.progress().running,
            frozen,
            "a closed segment's clock must not keep running -- that is the time spent paused"
        );
    }

    #[test]
    fn control_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JobControl>();
        assert_send_sync::<Arc<JobControl>>();
    }
}
