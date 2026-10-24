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

/// `admission_cap`'s "no cap" value, the one [`TileControl::admission_cap`]
/// defaults to.
const NO_CAP: usize = usize::MAX;

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
    /// How many frames' registration has completed during the align stage --
    /// shared with the driver through [`Self::begin_align`], since the
    /// alignment side of a run credits it directly rather than through
    /// [`Self::unit_completed`].
    align_done: Arc<AtomicUsize>,
    /// The frame count a computed align stage started with; `0` until
    /// [`Self::begin_align`] is called.
    align_total: AtomicUsize,
    /// How many frames' equalization histogram is complete during the
    /// prepare stage -- shared through [`Self::begin_prepare`].
    prepare_done: Arc<AtomicUsize>,
    /// The frame count the prepare stage started with; `0` until
    /// [`Self::begin_prepare`] is called.
    prepare_total: AtomicUsize,
    /// Offset from [`Self::epoch`] at which the prepare stage began; the
    /// running clock does not tick before fuse, so the prepare ETA needs its
    /// own origin.
    prepare_start_nanos: AtomicU64,
    /// The live admission cap; [`NO_CAP`] when none is set.
    admission_cap: AtomicUsize,
    /// Nanoseconds of running time every fuse-stage admission check waits
    /// for; `0` means none.
    min_running_nanos: AtomicU64,
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
            align_done: Arc::new(AtomicUsize::new(0)),
            align_total: AtomicUsize::new(0),
            prepare_done: Arc::new(AtomicUsize::new(0)),
            prepare_total: AtomicUsize::new(0),
            prepare_start_nanos: AtomicU64::new(0),
            admission_cap: AtomicUsize::new(NO_CAP),
            min_running_nanos: AtomicU64::new(0),
        })
    }

    /// Caps the units this job may hold in flight (fusion tiles and
    /// alignment registrations alike), floored at one so a capped run still
    /// progresses; `None` lifts the cap. Takes effect at the next admission
    /// check of either stage.
    pub fn set_admission_cap(&self, cap: Option<usize>) {
        self.admission_cap
            .store(cap.map_or(NO_CAP, |c| c.max(1)), Ordering::SeqCst);
    }

    /// Holds the fuse stage until this job's running time (pauses excluded)
    /// reaches `floor`: [`TileControl::should_stop`] waits in short slices
    /// while the job is running and below it, and returns at once on a
    /// pause or cancel. The output is unchanged. `Duration::ZERO` lifts it.
    pub fn set_min_running(&self, floor: Duration) {
        let nanos = u64::try_from(floor.as_nanos()).unwrap_or(u64::MAX);
        self.min_running_nanos.store(nanos, Ordering::SeqCst);
    }

    /// The live admission cap; `usize::MAX` when none is set.
    pub fn admission_cap(&self) -> usize {
        self.admission_cap.load(Ordering::SeqCst)
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
    ///
    /// During the align stage the unit is one frame's registration instead
    /// of one fuse tile: each align worker tests this state before it
    /// starts registering a frame, so at most one further registration per
    /// align worker completes after the store.
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

    /// Starts a computed align stage: stores `frames` as its total and hands
    /// back the shared counter the alignment driver credits as each frame's
    /// registration completes.
    pub(crate) fn begin_align(&self, frames: usize) -> Arc<AtomicUsize> {
        self.align_total.store(frames, Ordering::SeqCst);
        Arc::clone(&self.align_done)
    }

    /// Starts the prepare stage: stores `frames` as its total, zeroes the
    /// counter, stamps the stage's start and hands back the shared counter
    /// the equalization credits as each frame's histogram completes.
    pub(crate) fn begin_prepare(&self, frames: usize) -> Arc<AtomicUsize> {
        self.prepare_done.store(0, Ordering::SeqCst);
        self.prepare_total.store(frames, Ordering::SeqCst);
        self.prepare_start_nanos
            .store(self.epoch.elapsed().as_nanos() as u64, Ordering::SeqCst);
        Arc::clone(&self.prepare_done)
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
        let stage = JobStage::from_u8(self.stage.load(Ordering::SeqCst));
        let prepare_done = self.prepare_done.load(Ordering::SeqCst);
        let prepare_total = self.prepare_total.load(Ordering::SeqCst);
        let eta = if stage == JobStage::Prepare {
            let started = self.prepare_start_nanos.load(Ordering::SeqCst);
            let elapsed = self.epoch.elapsed().as_nanos() as u64;
            eta(
                prepare_done as u64,
                prepare_total as u64,
                Duration::from_nanos(elapsed.saturating_sub(started)),
            )
        } else {
            eta(pixels_done, pixel_total, running)
        };
        JobProgress {
            stage,
            units_done,
            unit_total,
            pixels_done,
            pixel_total,
            running,
            eta,
            align_done: self.align_done.load(Ordering::SeqCst),
            align_total: self.align_total.load(Ordering::SeqCst),
            prepare_done,
            prepare_total,
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
        const SLICE: Duration = Duration::from_millis(50);
        let floor = Duration::from_nanos(self.min_running_nanos.load(Ordering::SeqCst));
        loop {
            if self.state() != ControlState::Running {
                return true;
            }
            let short = floor.saturating_sub(self.running_now());
            if short.is_zero() {
                return false;
            }
            std::thread::sleep(short.min(SLICE));
        }
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

    fn admission_cap(&self) -> usize {
        self.admission_cap.load(Ordering::SeqCst)
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
    fn an_admission_cap_floors_at_one_and_none_lifts_it() {
        let control = JobControl::new();
        assert_eq!(control.admission_cap(), usize::MAX);
        control.set_admission_cap(Some(0));
        assert_eq!(control.admission_cap(), 1);
        control.set_admission_cap(Some(8));
        assert_eq!(control.admission_cap(), 8);
        control.set_admission_cap(None);
        assert_eq!(control.admission_cap(), usize::MAX);
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
    fn begin_align_sets_the_total_and_hands_back_a_counter_progress_reads() {
        let control = JobControl::new();
        assert_eq!(control.progress().align_total, 0);

        let counter = control.begin_align(8);
        assert_eq!(control.progress().align_total, 8);
        assert_eq!(control.progress().align_done, 0);

        counter.fetch_add(3, Ordering::SeqCst);
        assert_eq!(control.progress().align_done, 3);
    }

    #[test]
    fn prepare_eta_follows_the_frame_rate() {
        let control = JobControl::new();
        assert_eq!(control.progress().prepare_total, 0);

        let counter = control.begin_prepare(4);
        control.set_stage(JobStage::Prepare);
        let p = control.progress();
        assert_eq!((p.prepare_done, p.prepare_total), (0, 4));
        assert_eq!(p.eta, None, "no frame has completed");

        std::thread::sleep(Duration::from_millis(5));
        counter.fetch_add(1, Ordering::SeqCst);
        let p = control.progress();
        assert_eq!(p.prepare_done, 1);
        assert!(p.eta.is_some_and(|d| d > Duration::ZERO), "{:?}", p.eta);

        control.set_stage(JobStage::Fuse);
        assert_eq!(
            control.progress().eta,
            None,
            "outside Prepare the ETA is the pixel one"
        );
    }

    #[test]
    fn begin_prepare_restarts_the_counter() {
        let control = JobControl::new();
        control.begin_prepare(3).fetch_add(3, Ordering::SeqCst);
        let counter = control.begin_prepare(5);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(control.progress().prepare_total, 5);
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
    fn a_running_floor_holds_should_stop_until_the_segment_has_run_that_long() {
        for floor_ms in [100u64, 300] {
            let control = JobControl::new();
            control.set_min_running(Duration::from_millis(floor_ms));
            control.begin_segment();
            let started = Instant::now();
            assert!(!control.should_stop());
            let held = started.elapsed();
            assert!(
                held >= Duration::from_millis(floor_ms),
                "floor {floor_ms} ms released after {held:?}"
            );
            assert!(
                held < Duration::from_millis(floor_ms + 200),
                "floor {floor_ms} ms held for {held:?}"
            );
        }
    }

    #[test]
    fn pause_and_cancel_end_a_floor_hold_promptly() {
        for cancel in [false, true] {
            let control = JobControl::new();
            control.set_min_running(Duration::from_secs(30));
            control.begin_segment();
            let held = Arc::clone(&control);
            let waiter = std::thread::spawn(move || {
                let started = Instant::now();
                (held.should_stop(), started.elapsed())
            });
            std::thread::sleep(Duration::from_millis(100));
            let signalled = Instant::now();
            if cancel {
                control.cancel();
            } else {
                control.pause();
            }
            let (stopped, _) = waiter.join().unwrap();
            let lag = signalled.elapsed();
            assert!(stopped, "a pause or cancel must make should_stop true");
            assert!(lag < Duration::from_secs(1), "released after {lag:?}");
        }
    }

    #[test]
    fn a_floor_counts_running_time_only_not_the_time_between_segments() {
        let control = JobControl::new();
        let floor = Duration::from_millis(150);
        control.set_min_running(floor);
        control.begin_segment();
        std::thread::sleep(Duration::from_millis(20));
        control.end_segment();
        // A sleep can overrun on a loaded host; the figure the floor still
        // owes is what the control itself measured.
        let ran = control.running_now();
        if ran >= floor {
            eprintln!("the first segment overran the floor ({ran:?}); nothing left to observe");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
        control.begin_segment();
        let started = Instant::now();
        assert!(!control.should_stop());
        let held = started.elapsed();
        let owed = floor - ran;
        assert!(
            held >= owed.saturating_sub(Duration::from_millis(20)),
            "the paused gap counted toward the floor: held {held:?}, owed {owed:?}"
        );
    }

    #[test]
    fn control_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JobControl>();
        assert_send_sync::<Arc<JobControl>>();
    }
}
