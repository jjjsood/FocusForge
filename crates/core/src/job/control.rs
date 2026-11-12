// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobControl`]: the `Arc`-shared handle a different thread uses to
//! request pause, resume or cancel, and that a caller polls for progress.
//!
//! **One flag with three states, not two independent signals**: pause and
//! cancel both leave the running `try_for_each` early, differing only in
//! the [`super::JobOutcome`] [`super::Job::run`] reports. The flag, counters
//! and clocks are atomics; the stage ledger and the fuse cost table sit behind
//! mutexes held only for a read or a short update, so `JobControl` is `Send +
//! Sync` by construction -- no `unsafe impl` anywhere in this module.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::tile::TileControl;
use crate::tile::plan::PlannedTile;
use crate::timing::{Stage, StageTimings};

use super::JobStage;
use super::progress::{JobProgress, StagePlan, StageState, eta, live_eta, stage_pace};

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

/// The stages the ledger tracks, one per [`Stage`].
const STAGES: usize = 6;

/// Fixed-point scale of the fuse tile costs, which are summed in atomics.
const COST_SCALE: f64 = 1e6;

/// Per-stage predicted seconds for the whole-run estimate.
#[derive(Debug)]
struct Ledger {
    /// Whether a prediction was seeded; without one there is no total.
    seeded: bool,
    stages: [StagePlan; STAGES],
    open: Option<Stage>,
    /// Offset from [`JobControl::epoch`] at which the open stage was entered.
    entered_nanos: u64,
    /// [`JobControl::hold_nanos`] when the open stage was entered.
    entered_hold_nanos: u64,
    /// Observed over predicted seconds of the first develop or align that
    /// credited every frame, at most 1; scales every estimate. `None` until
    /// one closes.
    pace: Option<f64>,
}

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
    /// Offset from [`Self::epoch`] at which the align stage began (or last
    /// restarted); like prepare, align runs before the running clock ticks.
    align_start_nanos: AtomicU64,
    /// Nanoseconds the job has spent held by a pause, across every stage.
    hold_nanos: AtomicU64,
    /// [`Self::hold_nanos`] when the align stage began (or last restarted);
    /// what accrued since is kept out of the align ETA's rate.
    align_hold_base: AtomicU64,
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
    /// How many RAW frames are developed into scratch during the develop
    /// stage -- shared through [`Self::begin_develop`].
    develop_done: Arc<AtomicUsize>,
    /// The frame count the develop stage started with; `0` until
    /// [`Self::begin_develop`] is called.
    develop_total: AtomicUsize,
    /// Offset from [`Self::epoch`] at which the develop stage began; like
    /// prepare, it runs before the running clock ticks.
    develop_start_nanos: AtomicU64,
    /// The live admission cap; [`NO_CAP`] when none is set.
    admission_cap: AtomicUsize,
    /// Whether every admission check waits until released.
    held: AtomicBool,
    /// Nanoseconds of running time every fuse-stage admission check waits
    /// for; `0` means none.
    min_running_nanos: AtomicU64,
    /// Fixed-point cost of each fuse tile, by plan index; empty when fuse
    /// progress is measured in area.
    fuse_costs: Mutex<Vec<u64>>,
    /// Sum of [`Self::fuse_costs`], the fuse stage's planned cost.
    fuse_cost_total: AtomicU64,
    /// Sum of the costs of the tiles completed so far.
    fuse_cost_done: AtomicU64,
    ledger: Mutex<Ledger>,
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
            align_start_nanos: AtomicU64::new(0),
            hold_nanos: AtomicU64::new(0),
            align_hold_base: AtomicU64::new(0),
            prepare_done: Arc::new(AtomicUsize::new(0)),
            prepare_total: AtomicUsize::new(0),
            prepare_start_nanos: AtomicU64::new(0),
            develop_done: Arc::new(AtomicUsize::new(0)),
            develop_total: AtomicUsize::new(0),
            develop_start_nanos: AtomicU64::new(0),
            admission_cap: AtomicUsize::new(NO_CAP),
            held: AtomicBool::new(false),
            min_running_nanos: AtomicU64::new(0),
            fuse_costs: Mutex::new(Vec::new()),
            fuse_cost_total: AtomicU64::new(0),
            fuse_cost_done: AtomicU64::new(0),
            ledger: Mutex::new(Ledger {
                seeded: false,
                stages: [StagePlan {
                    state: StageState::Pending,
                    predicted: 0.0,
                }; STAGES],
                open: None,
                entered_nanos: 0,
                entered_hold_nanos: 0,
                pace: None,
            }),
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

    /// While held, [`TileControl::should_stop`] waits in short slices before
    /// letting any further unit start, and returns at once on a pause or
    /// cancel; work already begun finishes. Releasing resumes where it stood.
    pub fn set_held(&self, held: bool) {
        self.held.store(held, Ordering::SeqCst);
    }

    /// Whether the control is held, see [`Self::set_held`].
    pub fn held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
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

    /// Starts a computed align stage: stores `frames` as its total, restarts
    /// the counter and clock, and hands back the shared counter the
    /// alignment driver credits as each frame's registration completes.
    pub(crate) fn begin_align(&self, frames: usize) -> Arc<AtomicUsize> {
        self.align_total.store(frames, Ordering::SeqCst);
        self.restart_align();
        Arc::clone(&self.align_done)
    }

    /// Zeroes the align counter and restarts its hold total, then stamps the
    /// stage's start, so the align ETA measures only what follows; used when
    /// the whole stage reruns.
    pub(crate) fn restart_align(&self) {
        self.align_done.store(0, Ordering::SeqCst);
        self.align_hold_base
            .store(self.hold_nanos.load(Ordering::SeqCst), Ordering::SeqCst);
        self.align_start_nanos
            .store(self.epoch.elapsed().as_nanos() as u64, Ordering::SeqCst);
    }

    /// Records time the job spent held by a pause, so no stage's clock
    /// counts it.
    pub(crate) fn add_hold(&self, held: Duration) {
        let nanos = u64::try_from(held.as_nanos()).unwrap_or(u64::MAX);
        self.hold_nanos.fetch_add(nanos, Ordering::SeqCst);
    }

    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .expect("no thread panics while holding the stage ledger")
    }

    /// Seeds each stage's predicted seconds; a stage the prediction omits is
    /// predicted zero. Marks the job seeded, which gives it a total ETA.
    pub(crate) fn seed_prediction(&self, predicted: &StageTimings) {
        let seconds = [
            predicted.setup,
            predicted.develop,
            predicted.align,
            predicted.fuse,
            predicted.warp_write,
            predicted.finalize,
        ];
        let mut ledger = self.ledger();
        for (slot, seconds) in ledger.stages.iter_mut().zip(seconds) {
            slot.predicted = seconds.unwrap_or(0.0);
        }
        ledger.seeded = true;
    }

    /// Leaves the open stage and enters `stage`.
    pub(crate) fn enter_stage(&self, stage: Stage) {
        let mut ledger = self.ledger();
        self.close_open_stage(&mut ledger);
        ledger.stages[stage as usize].state = StageState::Running;
        ledger.open = Some(stage);
        ledger.entered_nanos = self.epoch.elapsed().as_nanos() as u64;
        ledger.entered_hold_nanos = self.hold_nanos.load(Ordering::SeqCst);
    }

    /// Finishes the open stage. The first develop or align that closes with
    /// every frame credited sets the run's pace from its span, pauses
    /// excluded.
    fn close_open_stage(&self, ledger: &mut Ledger) {
        let Some(open) = ledger.open.take() else {
            return;
        };
        ledger.stages[open as usize].state = StageState::Finished;
        let (done, total) = match open {
            Stage::Develop => (&self.develop_done, &self.develop_total),
            Stage::Align => (&self.align_done, &self.align_total),
            _ => return,
        };
        let total = total.load(Ordering::SeqCst);
        if ledger.pace.is_some() || total == 0 || done.load(Ordering::SeqCst) != total {
            return;
        }
        let held = self
            .hold_nanos
            .load(Ordering::SeqCst)
            .saturating_sub(ledger.entered_hold_nanos);
        let span = (self.epoch.elapsed().as_nanos() as u64)
            .saturating_sub(ledger.entered_nanos)
            .saturating_sub(held);
        ledger.pace = stage_pace(
            ledger.stages[open as usize].predicted,
            Duration::from_nanos(span).as_secs_f64(),
        );
    }

    /// Predicts `stage` zero seconds: its work turned out to be served from
    /// a cache or a previous run.
    pub(crate) fn drop_prediction(&self, stage: Stage) {
        self.ledger().stages[stage as usize].predicted = 0.0;
    }

    /// Closes the open stage and retires every stage never entered, so a
    /// finished run has nothing left to estimate.
    pub(crate) fn finish_stages(&self) {
        let mut ledger = self.ledger();
        self.close_open_stage(&mut ledger);
        for slot in &mut ledger.stages {
            if slot.state == StageState::Pending {
                slot.predicted = 0.0;
                slot.state = StageState::Finished;
            }
        }
    }

    /// Weights the fuse stage's progress by `costs`, one per plan tile in
    /// index order, instead of read-rect area. Called before the job credits
    /// any tile.
    pub(crate) fn set_fuse_costs(&self, costs: Vec<f64>) {
        let fixed: Vec<u64> = costs
            .iter()
            .map(|cost| (cost * COST_SCALE).round() as u64)
            .collect();
        self.fuse_cost_total
            .store(fixed.iter().sum(), Ordering::SeqCst);
        self.fuse_cost_done.store(0, Ordering::SeqCst);
        *self
            .fuse_costs
            .lock()
            .expect("no thread panics while holding the cost table") = fixed;
    }

    /// The fuse stage's completed and planned quantity: tile cost when a
    /// cost table is set, read-rect area otherwise.
    fn fuse_progress(&self) -> (u64, u64) {
        let cost_total = self.fuse_cost_total.load(Ordering::SeqCst);
        if cost_total > 0 {
            (self.fuse_cost_done.load(Ordering::SeqCst), cost_total)
        } else {
            (
                self.pixels_done.load(Ordering::SeqCst),
                self.pixel_total.load(Ordering::SeqCst),
            )
        }
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

    /// Starts the develop stage: stores `frames` as its total, zeroes the
    /// counter, stamps the stage's start and hands back the shared counter
    /// the develop pass credits as each frame lands in scratch.
    pub(crate) fn begin_develop(&self, frames: usize) -> Arc<AtomicUsize> {
        self.develop_done.store(0, Ordering::SeqCst);
        self.develop_total.store(frames, Ordering::SeqCst);
        self.develop_start_nanos
            .store(self.epoch.elapsed().as_nanos() as u64, Ordering::SeqCst);
        Arc::clone(&self.develop_done)
    }

    /// A plain snapshot, returned by value -- polled, never pushed.
    ///
    /// Every field is **live**: `units_done`/`pixels_done` are credited by
    /// [`TileControl::unit_completed`] as each unit finishes, and `running`
    /// adds the executing segment's elapsed time to the accumulated total,
    /// so a caller polling from another thread watches the counters climb
    /// during the run rather than seeing them jump at the end of it.
    ///
    /// With no prediction seeded, `eta` follows the stage: the per-frame
    /// rate (pauses excluded) during Develop, Prepare and a computed Align,
    /// `None` once every align frame is done, and the cost or pixel rate
    /// otherwise; `total_eta` is `None`. Seeded, both come from
    /// `live_eta` over the ledger's open stage.
    pub fn progress(&self) -> JobProgress {
        let units_done = self.units_done.load(Ordering::SeqCst);
        let unit_total = self.unit_total.load(Ordering::SeqCst);
        let pixels_done = self.pixels_done.load(Ordering::SeqCst);
        let pixel_total = self.pixel_total.load(Ordering::SeqCst);
        let running = self.running_now();
        let stage = JobStage::from_u8(self.stage.load(Ordering::SeqCst));
        let prepare_done = self.prepare_done.load(Ordering::SeqCst);
        let prepare_total = self.prepare_total.load(Ordering::SeqCst);
        let develop_done = self.develop_done.load(Ordering::SeqCst);
        let develop_total = self.develop_total.load(Ordering::SeqCst);
        let align_done = self.align_done.load(Ordering::SeqCst);
        let align_total = self.align_total.load(Ordering::SeqCst);
        let ledger = self.ledger();
        let (eta, total_eta) = if ledger.seeded {
            let (fraction, observed) = match ledger.open {
                Some(Stage::Setup) if stage == JobStage::Prepare => self.prepare_rate(),
                Some(Stage::Develop) => self.develop_rate(),
                Some(Stage::Align) => self.align_rate(),
                Some(Stage::Fuse) => self.fuse_rate(running),
                _ => (None, None),
            };
            let live = live_eta(
                &ledger.stages,
                fraction,
                observed,
                ledger.pace.unwrap_or(1.0),
            );
            (live.stage, Some(live.total))
        } else {
            let (_, observed) = match stage {
                JobStage::Align => self.align_rate(),
                JobStage::Prepare => self.prepare_rate(),
                JobStage::Develop => self.develop_rate(),
                _ => self.fuse_rate(running),
            };
            (observed, None)
        };
        drop(ledger);
        JobProgress {
            stage,
            units_done,
            unit_total,
            pixels_done,
            pixel_total,
            running,
            eta,
            total_eta,
            align_done,
            align_total,
            prepare_done,
            prepare_total,
            develop_done,
            develop_total,
        }
    }

    /// The align stage's progress fraction and its remaining time at the
    /// frame rate, holds excluded. The remainder is `None` once every frame
    /// is done: the stage has nothing left to estimate.
    fn align_rate(&self) -> (Option<f64>, Option<Duration>) {
        let done = self.align_done.load(Ordering::SeqCst);
        let total = self.align_total.load(Ordering::SeqCst);
        let observed = (done < total).then(|| {
            let started = self.align_start_nanos.load(Ordering::SeqCst);
            let held = self
                .hold_nanos
                .load(Ordering::SeqCst)
                .saturating_sub(self.align_hold_base.load(Ordering::SeqCst));
            let elapsed = self.epoch.elapsed().as_nanos() as u64;
            Duration::from_nanos(elapsed.saturating_sub(started).saturating_sub(held))
        });
        (
            fraction(done as u64, total as u64),
            observed.and_then(|clock| eta(done as u64, total as u64, clock)),
        )
    }

    /// The prepare stage's progress fraction and remaining time at the
    /// frame rate. The stage never pauses -- its only stop check is a cancel
    /// -- so the time since it began is its running clock and no hold needs
    /// excluding.
    fn prepare_rate(&self) -> (Option<f64>, Option<Duration>) {
        let done = self.prepare_done.load(Ordering::SeqCst) as u64;
        let total = self.prepare_total.load(Ordering::SeqCst) as u64;
        let started = self.prepare_start_nanos.load(Ordering::SeqCst);
        let elapsed = self.epoch.elapsed().as_nanos() as u64;
        let clock = Duration::from_nanos(elapsed.saturating_sub(started));
        (fraction(done, total), eta(done, total, clock))
    }

    /// The develop stage's progress fraction and remaining time at the
    /// frame rate. Like prepare, it never pauses, so the time since it began
    /// is its running clock and no hold needs excluding.
    fn develop_rate(&self) -> (Option<f64>, Option<Duration>) {
        let done = self.develop_done.load(Ordering::SeqCst) as u64;
        let total = self.develop_total.load(Ordering::SeqCst) as u64;
        let started = self.develop_start_nanos.load(Ordering::SeqCst);
        let elapsed = self.epoch.elapsed().as_nanos() as u64;
        let clock = Duration::from_nanos(elapsed.saturating_sub(started));
        (fraction(done, total), eta(done, total, clock))
    }

    /// The fuse stage's progress fraction and remaining time at its cost or
    /// pixel rate over the running clock, which excludes paused time.
    fn fuse_rate(&self, running: Duration) -> (Option<f64>, Option<Duration>) {
        let (done, total) = self.fuse_progress();
        (fraction(done, total), eta(done, total, running))
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

/// `done / total`, or `None` when nothing is planned.
fn fraction(done: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| done as f64 / total as f64)
}

impl TileControl for JobControl {
    fn should_stop(&self) -> bool {
        const SLICE: Duration = Duration::from_millis(50);
        let floor = Duration::from_nanos(self.min_running_nanos.load(Ordering::SeqCst));
        loop {
            if self.state() != ControlState::Running {
                return true;
            }
            if self.held() {
                std::thread::sleep(SLICE);
                continue;
            }
            let short = floor.saturating_sub(self.running_now());
            if short.is_zero() {
                return false;
            }
            std::thread::sleep(short.min(SLICE));
        }
    }

    /// Credits one finished unit, from the worker that ran it or for a tile
    /// a previous process completed. **Pixels first, then units**: a poller
    /// sees at worst "pixels counted, unit not yet", so `units_done > 0`
    /// implies `pixels_done > 0` and an `eta` that is `Some`.
    fn unit_completed(&self, tile: &PlannedTile) {
        let read = tile.read();
        self.pixels_done.fetch_add(
            u64::from(read.width) * u64::from(read.height),
            Ordering::SeqCst,
        );
        let cost = self
            .fuse_costs
            .lock()
            .expect("no thread panics while holding the cost table")
            .get(tile.index())
            .copied();
        if let Some(cost) = cost {
            self.fuse_cost_done.fetch_add(cost, Ordering::SeqCst);
        }
        self.units_done.fetch_add(1, Ordering::SeqCst);
    }

    fn admission_cap(&self) -> usize {
        self.admission_cap.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::prediction::fuse_tile_cost;
    use crate::pipeline::fuse_costs;
    use crate::tile::plan::TilePlan;
    use crate::timing::StageTimings;

    fn plan_512() -> TilePlan {
        TilePlan::new(512, 512, 200, 3).expect("a 512 px frame plans at tile size 200")
    }

    fn area(tile: &PlannedTile) -> u64 {
        u64::from(tile.read().width) * u64::from(tile.read().height)
    }

    /// A control whose totals match `plan`, with `costs` as the fuse cost
    /// table when given.
    fn control_over(plan: &TilePlan, costs: Option<Vec<f64>>) -> Arc<JobControl> {
        let control = JobControl::new();
        control.set_unit_total(plan.len());
        control.set_pixel_total(plan.tiles().iter().map(area).sum());
        if let Some(costs) = costs {
            control.set_fuse_costs(costs);
        }
        control
    }

    fn timings(
        setup: f64,
        develop: Option<f64>,
        align: Option<f64>,
        fuse: f64,
        finalize: f64,
    ) -> StageTimings {
        StageTimings {
            setup: Some(setup),
            develop,
            align,
            fuse: Some(fuse),
            warp_write: None,
            finalize: Some(finalize),
        }
    }

    fn fuse_fraction(control: &JobControl) -> Option<f64> {
        control.fuse_rate(Duration::ZERO).0
    }

    fn predicted(control: &JobControl, stage: Stage) -> f64 {
        control.ledger.lock().unwrap().stages[stage as usize].predicted
    }

    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-5,
            "{actual} vs expected {expected}"
        );
    }

    #[test]
    fn seeding_maps_each_prediction_key_to_its_stage() {
        let control = JobControl::new();
        control.seed_prediction(&timings(1.0, Some(2.0), None, 4.0, 5.0));
        let stages = [
            (Stage::Setup, 1.0),
            (Stage::Develop, 2.0),
            (Stage::Align, 0.0),
            (Stage::Fuse, 4.0),
            (Stage::WarpWrite, 0.0),
            (Stage::Finalize, 5.0),
        ];
        for (stage, seconds) in stages {
            assert_eq!(predicted(&control, stage), seconds, "{stage:?}");
        }
    }

    #[test]
    fn seeding_survives_a_restarted_develop() {
        let control = JobControl::new();
        control.seed_prediction(&timings(0.0, Some(8.0), None, 2.0, 1.0));
        control.enter_stage(Stage::Develop);
        let _ = control.begin_develop(5);
        let _ = control.begin_develop(5);
        control.set_stage(JobStage::Develop);

        assert_eq!(predicted(&control, Stage::Develop), 8.0);
        let p = control.progress();
        assert_eq!(p.eta, Some(Duration::from_secs(8)), "the prediction");
        assert_eq!(p.total_eta, Some(Duration::from_secs(11)));
    }

    #[test]
    fn a_seeded_stage_estimate_runs_from_its_prediction_to_its_observation() {
        let control = JobControl::new();
        control.seed_prediction(&timings(0.0, Some(8.0), None, 0.0, 0.0));
        control.enter_stage(Stage::Develop);
        let counter = control.begin_develop(4);
        control.set_stage(JobStage::Develop);
        assert_eq!(control.progress().eta, Some(Duration::from_secs(8)));

        std::thread::sleep(Duration::from_millis(5));
        counter.fetch_add(4, Ordering::SeqCst);
        assert_eq!(control.progress().eta, Some(Duration::ZERO));
    }

    #[test]
    fn dropping_a_stage_prediction_removes_it_from_the_total() {
        let control = JobControl::new();
        control.seed_prediction(&timings(1.0, None, Some(4.0), 2.0, 0.5));
        control.enter_stage(Stage::Setup);
        approx(control.progress().total_eta.unwrap().as_secs_f64(), 7.5);

        control.drop_prediction(Stage::Align);
        approx(control.progress().total_eta.unwrap().as_secs_f64(), 3.5);
    }

    #[test]
    fn finishing_the_stages_leaves_nothing_to_estimate() {
        let control = JobControl::new();
        control.seed_prediction(&timings(1.0, Some(2.0), Some(3.0), 4.0, 5.0));
        control.enter_stage(Stage::Setup);
        control.enter_stage(Stage::Finalize);
        control.finish_stages();
        let p = control.progress();
        assert_eq!(
            (p.eta, p.total_eta),
            (Some(Duration::ZERO), Some(Duration::ZERO))
        );
    }

    #[test]
    fn an_unseeded_control_reports_no_total() {
        let control = JobControl::new();
        control.enter_stage(Stage::Setup);
        assert_eq!(control.progress().total_eta, None);
    }

    #[test]
    fn stages_to_come_keep_their_prediction() {
        // Setup took 5x, then 1/10 of its prediction; the stages after it are
        // priced as seeded either way.
        for (setup_predicted, setup_slept_ms) in [(0.005, 25), (0.25, 25)] {
            let control = JobControl::new();
            control.seed_prediction(&timings(setup_predicted, None, Some(3.79), 24.94, 1.24));
            control.enter_stage(Stage::Setup);
            std::thread::sleep(Duration::from_millis(setup_slept_ms));
            control.enter_stage(Stage::Align);
            approx(control.progress().total_eta.unwrap().as_secs_f64(), 29.97);
            control.enter_stage(Stage::Fuse);
            approx(control.progress().total_eta.unwrap().as_secs_f64(), 26.18);
        }
    }

    #[test]
    fn short_run_fuse_entry_is_the_fuse_and_finalize_prediction() {
        let control = JobControl::new();
        control.seed_prediction(&timings(0.00267, None, Some(0.0309), 0.2181, 0.0217));
        control.enter_stage(Stage::Setup);
        std::thread::sleep(Duration::from_millis(50));
        control.enter_stage(Stage::Align);
        std::thread::sleep(Duration::from_millis(100));
        control.enter_stage(Stage::Fuse);
        approx(control.progress().total_eta.unwrap().as_secs_f64(), 0.2398);

        // An align that credits all its frames, but slower than predicted,
        // has a pace clamped at 1.
        let control = JobControl::new();
        control.seed_prediction(&timings(0.00267, None, Some(0.0309), 0.2181, 0.0217));
        control.enter_stage(Stage::Setup);
        align_over(&control, 30, 30, 100);
        control.enter_stage(Stage::Fuse);
        assert_eq!(pace(&control), Some(1.0));
        approx(total(&control), 0.2398);
    }

    fn pace(control: &JobControl) -> Option<f64> {
        control.ledger().pace
    }

    fn total(control: &JobControl) -> f64 {
        control.progress().total_eta.unwrap().as_secs_f64()
    }

    /// Enters align over `frames` frames, runs `millis` ms, then credits `credited` of them.
    fn align_over(control: &JobControl, frames: usize, credited: usize, millis: u64) {
        control.enter_stage(Stage::Align);
        let counter = control.begin_align(frames);
        std::thread::sleep(Duration::from_millis(millis));
        counter.fetch_add(credited, Ordering::SeqCst);
    }

    #[test]
    fn a_completed_align_faster_than_predicted_scales_what_follows() {
        let plan = plan_512();
        let control = control_over(&plan, None);
        control.seed_prediction(&timings(0.0, None, Some(1.242), 0.9391, 0.0113));
        control.enter_stage(Stage::Setup);
        align_over(&control, 30, 30, 30);
        let seeded = 0.9391 + 0.0113;
        approx(total(&control), seeded);

        control.enter_stage(Stage::Fuse);
        let pace = pace(&control).expect("every frame was credited");
        assert!((0.02..0.5).contains(&pace), "{pace}");
        let mut previous = total(&control);
        approx(previous, pace * seeded);
        for tile in plan.tiles() {
            control.unit_completed(tile);
            let now = total(&control);
            assert!(now < previous, "{now} after {previous}");
            previous = now;
        }
    }

    #[test]
    fn the_develop_pace_holds_through_the_second_setup_and_align() {
        let control = JobControl::new();
        control.seed_prediction(&timings(0.0, Some(0.940), Some(0.6900), 0.8819, 0.0287));
        control.enter_stage(Stage::Setup);
        control.enter_stage(Stage::Develop);
        let counter = control.begin_develop(25);
        std::thread::sleep(Duration::from_millis(21));
        counter.fetch_add(25, Ordering::SeqCst);

        control.enter_stage(Stage::Setup);
        let held = pace(&control).expect("every frame was developed");
        assert!((0.02..0.5).contains(&held), "{held}");
        let ahead = 0.6900 + 0.8819 + 0.0287;
        approx(total(&control), held * ahead);

        std::thread::sleep(Duration::from_millis(25));
        control.enter_stage(Stage::Align);
        let _ = control.begin_align(25);
        assert_eq!(pace(&control), Some(held));
        approx(total(&control), held * ahead);
    }

    #[test]
    fn a_stage_slower_than_predicted_leaves_the_prediction() {
        let control = JobControl::new();
        control.seed_prediction(&timings(0.0, None, Some(0.0134), 0.3389, 0.0112));
        control.enter_stage(Stage::Setup);
        align_over(&control, 30, 30, 20);
        control.enter_stage(Stage::Fuse);
        assert_eq!(pace(&control), Some(1.0));
        approx(total(&control), 0.3389 + 0.0112);
    }

    #[test]
    fn a_pause_inside_the_stage_does_not_count_toward_its_pace() {
        let align_pace = |held: Duration| {
            let control = JobControl::new();
            control.seed_prediction(&timings(0.0, None, Some(1.0), 1.0, 0.1));
            control.enter_stage(Stage::Align);
            let counter = control.begin_align(4);
            std::thread::sleep(Duration::from_millis(100));
            control.add_hold(held);
            counter.fetch_add(4, Ordering::SeqCst);
            control.enter_stage(Stage::Fuse);
            pace(&control).expect("every frame was credited")
        };
        assert!(align_pace(Duration::ZERO) >= 0.1);
        assert!(align_pace(Duration::from_millis(90)) < 0.06);
    }

    #[test]
    fn no_pace_without_every_frame_or_a_prediction() {
        let seeded = |develop| {
            let control = JobControl::new();
            control.seed_prediction(&timings(0.001, develop, Some(1.0), 5.0, 0.5));
            control.enter_stage(Stage::Setup);
            control
        };

        let control = seeded(None);
        align_over(&control, 30, 29, 10);
        control.enter_stage(Stage::Fuse);
        assert_eq!(pace(&control), None, "29 of 30 frames");
        approx(total(&control), 5.5);

        let control = seeded(None);
        align_over(&control, 30, 30, 10);
        control.drop_prediction(Stage::Align);
        control.enter_stage(Stage::Fuse);
        assert_eq!(pace(&control), None, "a dropped prediction");
        approx(total(&control), 5.5);

        let control = seeded(None);
        let counter = control.begin_develop(5);
        control.enter_stage(Stage::Develop);
        counter.fetch_add(5, Ordering::SeqCst);
        control.enter_stage(Stage::Setup);
        assert_eq!(pace(&control), None, "a develop predicted zero");

        let control = seeded(None);
        std::thread::sleep(Duration::from_millis(10));
        control.enter_stage(Stage::Fuse);
        std::thread::sleep(Duration::from_millis(10));
        control.enter_stage(Stage::Finalize);
        assert_eq!(pace(&control), None, "only setup and fuse closed");
        approx(total(&control), 1.5);
    }

    #[test]
    fn a_fast_near_zero_align_lowers_what_follows() {
        let control = JobControl::new();
        control.seed_prediction(&timings(0.00267, None, Some(0.309), 0.2181, 0.0217));
        control.enter_stage(Stage::Setup);
        std::thread::sleep(Duration::from_millis(25));
        align_over(&control, 3, 3, 2);
        control.enter_stage(Stage::Fuse);
        let pace = pace(&control).expect("every frame was credited");
        assert!(pace < 0.5, "{pace}");
        approx(total(&control), pace * 0.2398);
    }

    #[test]
    fn the_total_stays_positive_through_a_scaled_finalize() {
        let control = JobControl::new();
        control.seed_prediction(&timings(0.0, None, Some(1.0), 1.0, 0.4));
        control.enter_stage(Stage::Setup);
        align_over(&control, 30, 30, 20);
        control.enter_stage(Stage::Fuse);
        control.enter_stage(Stage::Finalize);
        let pace = pace(&control).expect("every frame was credited");
        assert!(pace < 0.5, "{pace}");
        approx(total(&control), pace * 0.4);
        assert!(control.progress().total_eta.unwrap() > Duration::ZERO);

        control.finish_stages();
        assert_eq!(control.progress().total_eta, Some(Duration::ZERO));
    }

    #[test]
    fn fuse_fraction_is_the_area_fraction_without_a_cost_table() {
        let plan = plan_512();
        let control = control_over(&plan, None);
        assert_eq!(fuse_fraction(&control), Some(0.0));

        control.unit_completed(&plan.tiles()[0]);
        let total: u64 = plan.tiles().iter().map(area).sum();
        approx(
            fuse_fraction(&control).unwrap(),
            area(&plan.tiles()[0]) as f64 / total as f64,
        );
    }

    #[test]
    fn fuse_fraction_is_the_area_fraction_over_uniform_tile_costs() {
        // A frame twice the interior stride wide plans a 2 x 2 grid of equal reads.
        let plan = TilePlan::new(224, 224, 160, 3).expect("a 224 px frame plans at tile size 160");
        let tiles = plan.tiles();
        assert!(
            tiles.iter().all(|tile| area(tile) == area(&tiles[0])),
            "the grid's tiles must be equal in size"
        );
        let total: u64 = tiles.iter().map(area).sum();
        for gpu in [false, true] {
            // Every tile of the grid touches an edge; the second table prices
            // the same tiles without the edge surcharge.
            let all_edge = fuse_costs(&plan, gpu, 7, (224, 224));
            let interior: Vec<f64> = tiles
                .iter()
                .map(|tile| fuse_tile_cost(gpu, 7, area(tile), false))
                .collect();
            for costs in [all_edge, interior] {
                let control = control_over(&plan, Some(costs));
                let mut done = 0;
                for index in [1, 3, 0] {
                    control.unit_completed(&tiles[index]);
                    done += area(&tiles[index]);
                    approx(fuse_fraction(&control).unwrap(), done as f64 / total as f64);
                }
            }
        }
    }

    #[test]
    fn fuse_fraction_follows_the_edge_cost_weights_on_both_backends() {
        let plan = plan_512();
        let tiles = plan.tiles();
        let total_area: u64 = tiles.iter().map(area).sum();
        for gpu in [false, true] {
            let costs = fuse_costs(&plan, gpu, 10, (512, 512));
            let control = control_over(&plan, Some(costs.clone()));
            // A resume credits its completed tiles one by one, as a run does.
            let done = [0, tiles.len() - 1];
            for &index in &done {
                control.unit_completed(&tiles[index]);
            }
            let by_cost = done.iter().map(|&i| costs[i]).sum::<f64>() / costs.iter().sum::<f64>();
            let by_area =
                done.iter().map(|&i| area(&tiles[i])).sum::<u64>() as f64 / total_area as f64;
            approx(fuse_fraction(&control).unwrap(), by_cost);
            assert!(
                (by_cost - by_area).abs() > 1e-3,
                "gpu {gpu}: cost {by_cost} and area {by_area} fractions coincide"
            );
            let p = control.progress();
            assert_eq!(
                p.pixels_done,
                done.iter().map(|&i| area(&tiles[i])).sum::<u64>()
            );
            assert_eq!(p.units_done, done.len());
        }
    }

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
        let plan = plan_512();
        let tiles = plan.tiles();
        let control = control_over(&plan, None);

        for tile in &tiles[..4] {
            control.unit_completed(tile);
        }
        let p1 = control.progress();
        assert_eq!(p1.unit_total, tiles.len());
        assert_eq!(p1.units_done, 4);
        assert_eq!(p1.pixels_done, tiles[..4].iter().map(area).sum::<u64>());

        for tile in &tiles[4..7] {
            control.unit_completed(tile);
        }
        let p2 = control.progress();
        assert_eq!(
            p2.unit_total,
            tiles.len(),
            "unit_total must never reset across segments"
        );
        assert_eq!(p2.units_done, 7);
        assert_eq!(p2.pixels_done, tiles[..7].iter().map(area).sum::<u64>());
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
    fn develop_eta_follows_the_frame_rate() {
        let control = JobControl::new();
        assert_eq!(control.progress().develop_total, 0);

        let counter = control.begin_develop(5);
        control.set_stage(JobStage::Develop);
        let p = control.progress();
        assert_eq!(
            (p.stage, p.develop_done, p.develop_total),
            (JobStage::Develop, 0, 5)
        );
        assert_eq!(p.eta, None, "no frame has completed");

        std::thread::sleep(Duration::from_millis(5));
        counter.fetch_add(1, Ordering::SeqCst);
        let p = control.progress();
        assert_eq!(p.develop_done, 1);
        assert!(p.eta.is_some_and(|d| d > Duration::ZERO), "{:?}", p.eta);

        counter.fetch_add(4, Ordering::SeqCst);
        assert_eq!(control.progress().eta, Some(Duration::ZERO));

        let restarted = control.begin_develop(2);
        assert_eq!(restarted.load(Ordering::SeqCst), 0);
        assert_eq!(control.progress().develop_total, 2);

        control.set_stage(JobStage::Fuse);
        assert_eq!(
            control.progress().eta,
            None,
            "outside Develop the ETA is the pixel one"
        );
    }

    #[test]
    fn align_eta_follows_the_frame_rate() {
        let control = JobControl::new();
        control.begin_align(3).fetch_add(3, Ordering::SeqCst);
        let counter = control.begin_align(4);
        control.set_stage(JobStage::Align);
        let p = control.progress();
        assert_eq!((p.align_done, p.align_total), (0, 4));
        assert_eq!(p.eta, None, "no frame has completed");

        std::thread::sleep(Duration::from_millis(5));
        counter.fetch_add(1, Ordering::SeqCst);
        let p = control.progress();
        assert!(p.eta.is_some_and(|d| d > Duration::ZERO), "{:?}", p.eta);
    }

    #[test]
    fn align_eta_excludes_a_pause() {
        let control = JobControl::new();
        let counter = control.begin_align(4);
        control.set_stage(JobStage::Align);
        std::thread::sleep(Duration::from_millis(5));
        counter.fetch_add(1, Ordering::SeqCst);
        let span = Instant::now();
        let before = control.progress().eta.expect("one frame is done");

        let held = Instant::now();
        std::thread::sleep(Duration::from_millis(50));
        let hold = held.elapsed();
        control.add_hold(hold);
        let after = control.progress().eta.expect("one frame is done");
        let span = span.elapsed();
        // With one of four frames done the ETA is three times the counted
        // time, and between the two reads at most `span - hold` of it was
        // not held. A clock that ignored the hold grows by at least
        // `3 * hold` instead, however long the sleep overran.
        let unheld = span.saturating_sub(hold);
        assert!(
            after <= before + unheld * 3 + Duration::from_micros(1),
            "{after:?} vs {before:?}, hold {hold:?}, span {span:?}"
        );
    }

    #[test]
    fn align_eta_is_none_when_every_frame_is_done() {
        let control = JobControl::new();
        let counter = control.begin_align(4);
        control.set_stage(JobStage::Align);
        std::thread::sleep(Duration::from_millis(5));
        counter.fetch_add(4, Ordering::SeqCst);
        assert_eq!(control.progress().eta, None);
    }

    #[test]
    fn align_eta_restarts_with_the_counter() {
        let control = JobControl::new();
        let counter = control.begin_align(4);
        control.set_stage(JobStage::Align);
        std::thread::sleep(Duration::from_millis(100));
        control.add_hold(Duration::from_millis(30));
        counter.fetch_add(2, Ordering::SeqCst);
        assert!(control.progress().eta.is_some());

        let before_restart = Instant::now();
        control.restart_align();
        let after_restart = Instant::now();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(control.progress().eta, None);
        std::thread::sleep(Duration::from_millis(5));
        counter.fetch_add(1, Ordering::SeqCst);
        let at_least = after_restart.elapsed();
        let eta = control.progress().eta.expect("one frame is done");
        let at_most = before_restart.elapsed();
        // With one of four frames done the ETA is three times the time
        // counted since the restart, which lies between the two bounds. A
        // clock that kept the 100 ms lands above them; one that still
        // subtracted the 30 ms hold lands below them, or gives no ETA.
        let slack = Duration::from_micros(1);
        assert!(
            eta + slack >= at_least * 3 && eta <= at_most * 3 + slack,
            "{eta:?} outside 3 * [{at_least:?}, {at_most:?}]"
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
        let plan = plan_512();
        let tiles = plan.tiles();
        let control = control_over(&plan, None);

        control.unit_completed(&tiles[0]);
        let p = control.progress();
        assert_eq!(p.units_done, 1);
        assert_eq!(p.pixels_done, area(&tiles[0]));

        control.unit_completed(&tiles[1]);
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
