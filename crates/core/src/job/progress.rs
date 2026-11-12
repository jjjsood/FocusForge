// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobProgress`], the plain snapshot [`super::JobControl::progress`]
//! returns by value, and the ETA functions every snapshot consults: [`eta`]
//! for a rate observed on one stage, `live_eta` to blend it with the
//! prediction. They are kept in one place so the policy is replaceable
//! without touching the scheduler and unit-testable without one.

use std::time::Duration;

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
    /// The sum of every *completed* unit's read-rect area
    /// (`PlannedTile::read().area()`, i.e. `width * height`), not a second
    /// unit count -- see [`eta`].
    pub pixels_done: u64,
    /// The sum of every *planned* unit's read-rect area.
    pub pixel_total: u64,
    /// Accumulated wall-clock time actually spent running -- never
    /// including time spent paused (`JobControl::begin_segment`/
    /// `end_segment` bracket exactly one segment's own elapsed time).
    pub running: Duration,
    /// The running stage's remaining time.
    pub eta: Option<Duration>,
    /// The whole run's remaining time: the running stage's remainder plus
    /// the predicted time of every stage still to come. `None` while no
    /// prediction is seeded.
    pub total_eta: Option<Duration>,
    /// How many frames' registration has completed during the align stage.
    /// `0` outside a computed alignment (no align stage, a cached alignment,
    /// or a resumed one) -- the counter is never reset once the stage ends,
    /// so a snapshot taken during a later stage still carries the align
    /// stage's own final tally.
    pub align_done: usize,
    /// The frame count a computed align stage started with, `0` when no
    /// frame is being counted.
    pub align_total: usize,
    /// How many frames' brightness equalization has completed during the
    /// prepare stage. `0` when the run does not equalize; like `align_done`
    /// it keeps its final tally once the stage ends.
    pub prepare_done: usize,
    /// The frame count the prepare stage started with, `0` when the run does
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

/// Area-weighted mean ETA:
/// `(pixel_total - pixels_done) * running / pixels_done`, **not** a plain
/// per-unit mean, so the estimate does not lurch on the ragged edge and
/// absorbed-remainder tiles near the end of a run (at the test geometry the
/// last interior is 56 px against a 152 px interior; at reference geometry a
/// 440 px tail against 512). Consumes the *accumulated* running time, never
/// `Instant::now() - start`, so a caller-side pause does not inflate the
/// estimate -- it is the caller's job (`JobControl::begin_segment`/
/// `end_segment`) to make sure `running` only ever grows while a segment is
/// actually executing.
///
/// `None` while `pixels_done == 0` **or** while `running` is zero -- an
/// `Option`, never a zero standing in for "unknown". Both guards are needed
/// and neither implies the other: a resumed run opens with `pixels_done`
/// already credited for the units a previous process completed
/// ([`super::Job::resuming`]) and a running clock that has not started, and
/// extrapolating a rate from zero elapsed time would make the first line of
/// every resumed run claim the job is about to finish.
pub fn eta(pixels_done: u64, pixel_total: u64, running: Duration) -> Option<Duration> {
    if pixels_done == 0 || running.is_zero() {
        return None;
    }
    if pixels_done >= pixel_total {
        return Some(Duration::ZERO);
    }
    let remaining_pixels = (pixel_total - pixels_done) as f64;
    let secs = running.as_secs_f64() * remaining_pixels / pixels_done as f64;
    Some(Duration::from_secs_f64(secs.max(0.0)))
}

/// Where a ledger stage stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageState {
    /// Not entered yet.
    Pending,
    /// The stage in progress; at most one at a time.
    Running,
    /// Left behind, though a stage may be entered again.
    Finished,
}

/// One stage's predicted and observed wall seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StagePlan {
    pub state: StageState,
    pub predicted: f64,
    /// Seconds spent in the stage so far, holds excluded.
    pub observed: f64,
}

/// The two estimates [`live_eta`] returns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LiveEta {
    /// The running stage's remainder; `None` when it has neither a
    /// prediction nor an observation, or when no stage runs and one is
    /// still to come.
    pub stage: Option<Duration>,
    /// The running stage's remainder plus every pending stage's prediction.
    pub total: Duration,
}

fn seconds(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
    } else {
        Duration::ZERO
    }
}

/// Blends each stage's prediction with what the run has observed. The speed
/// factor (observed over predicted seconds of the finished stages that had a
/// prediction, 1 before any) scales every predicted second to come. The
/// running stage's remainder is `(1 - f)^2 * predicted * speed + f *
/// observed_remaining` for progress fraction `f`; with no observation it is
/// the scaled prediction, and a stage predicted zero reports its observation.
pub(crate) fn live_eta(
    stages: &[StagePlan],
    fraction: Option<f64>,
    observed_remaining: Option<Duration>,
) -> LiveEta {
    let (observed, predicted) = stages
        .iter()
        .filter(|s| s.state == StageState::Finished && s.predicted > 0.0)
        .fold((0.0, 0.0), |(o, p), s| (o + s.observed, p + s.predicted));
    let speed = if predicted > 0.0 {
        observed / predicted
    } else {
        1.0
    };

    let running = stages.iter().find(|s| s.state == StageState::Running);
    let stage = match running {
        Some(running) => {
            let f = fraction.map(|f| f.clamp(0.0, 1.0));
            let scaled = running.predicted * speed;
            match (f, observed_remaining) {
                (Some(f), Some(observed)) if running.predicted > 0.0 => Some(seconds(
                    (1.0 - f) * (1.0 - f) * scaled + f * observed.as_secs_f64(),
                )),
                (_, Some(observed)) => Some(observed),
                (Some(f), None) if running.predicted > 0.0 => Some(seconds((1.0 - f) * scaled)),
                (None, None) if running.predicted > 0.0 => Some(seconds(scaled)),
                _ => None,
            }
        }
        None if stages.iter().all(|s| s.state != StageState::Pending) => Some(Duration::ZERO),
        None => None,
    };
    let ahead: f64 = stages
        .iter()
        .filter(|s| s.state == StageState::Pending)
        .map(|s| s.predicted * speed)
        .sum();
    LiveEta {
        stage,
        total: stage.unwrap_or_default() + seconds(ahead),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_is_absent_until_a_unit_completes() {
        assert_eq!(eta(0, 100, Duration::from_secs(5)), None);
        assert_eq!(eta(0, 0, Duration::ZERO), None);
    }

    #[test]
    fn eta_is_area_weighted_not_a_plain_per_unit_mean() {
        // 2 pixels done out of 10 total, running 4s:
        // (10 - 2) * 4 / 2 = 16s -- the area-weighted mean, not a per-unit
        // mean that would ignore how large each done/remaining unit was.
        assert_eq!(
            eta(2, 10, Duration::from_secs(4)),
            Some(Duration::from_secs(16))
        );
    }

    #[test]
    fn eta_is_absent_until_some_running_time_has_been_observed() {
        // A resumed run starts with `pixels_done` already credited for the
        // units a previous process finished (`Job::resuming`) but with a
        // running clock of zero -- no throughput has been observed yet, so
        // there is nothing to extrapolate from. Returning `Some(ZERO)` here
        // makes the first line of every resumed run claim the job is about
        // to finish.
        assert_eq!(eta(4, 10, Duration::ZERO), None);
    }

    #[test]
    fn eta_is_zero_once_every_planned_pixel_is_done() {
        assert_eq!(eta(10, 10, Duration::from_secs(4)), Some(Duration::ZERO));
    }

    fn secs(d: Option<Duration>) -> Option<f64> {
        d.map(|d| d.as_secs_f64())
    }

    fn assert_secs(actual: Option<Duration>, expected: f64) {
        let actual = secs(actual).expect("an estimate is present");
        assert!(
            (actual - expected).abs() < 1e-9,
            "{actual} s, expected {expected} s"
        );
    }

    fn plan(state: StageState, predicted: f64, observed: f64) -> StagePlan {
        StagePlan {
            state,
            predicted,
            observed,
        }
    }

    /// Setup, develop, align, fuse, warp-write, finalize with `fuse` running.
    fn running_fuse(finished: [(f64, f64); 3], fuse: f64, finalize: f64) -> [StagePlan; 6] {
        let done = |(predicted, observed)| plan(StageState::Finished, predicted, observed);
        [
            done(finished[0]),
            done(finished[1]),
            done(finished[2]),
            plan(StageState::Running, fuse, 0.0),
            plan(StageState::Pending, 0.0, 0.0),
            plan(StageState::Pending, finalize, 0.0),
        ]
    }

    #[test]
    fn the_observation_weighs_nothing_at_the_start_and_everything_at_the_end() {
        let stages = running_fuse([(0.0, 0.0); 3], 10.0, 0.0);
        let observed = Some(Duration::from_secs(20));
        // Predicted remainder is (1 - fraction) * 10 s; the blend is
        // (1 - w) * that + w * 20 s with w = fraction.
        for (fraction, expected) in [(0.0, 10.0), (0.3, 0.7 * 7.0 + 0.3 * 20.0), (1.0, 20.0)] {
            let live = live_eta(&stages, Some(fraction), observed);
            assert_secs(live.stage, expected);
        }
    }

    #[test]
    fn without_an_observed_remainder_the_stage_is_its_scaled_predicted_remainder() {
        let stages = running_fuse([(1.0, 3.0), (0.0, 0.0), (0.0, 0.0)], 10.0, 0.0);
        assert_secs(live_eta(&stages, Some(0.0), None).stage, 30.0);
        assert_secs(live_eta(&stages, Some(0.3), None).stage, 21.0);
        assert_secs(live_eta(&stages, None, None).stage, 30.0);
    }

    #[test]
    fn a_stage_predicted_zero_estimates_from_its_observation_alone() {
        let stages = running_fuse([(0.0, 0.0); 3], 0.0, 0.0);
        assert_eq!(live_eta(&stages, Some(0.4), None).stage, None);
        assert_eq!(live_eta(&stages, None, None).stage, None);
        let observed = Some(Duration::from_secs(5));
        assert_eq!(live_eta(&stages, Some(0.4), observed).stage, observed);
    }

    #[test]
    fn the_speed_factor_follows_a_slower_and_a_faster_machine() {
        let total = |observed: f64| {
            let stages = running_fuse([(1.0, observed), (0.0, 0.0), (0.0, 0.0)], 10.0, 1.0);
            live_eta(&stages, Some(0.0), None)
        };
        let slow = total(2.0);
        assert_secs(slow.stage, 20.0);
        assert_secs(Some(slow.total), 22.0);
        let fast = total(0.5);
        assert_secs(fast.stage, 5.0);
        assert_secs(Some(fast.total), 5.5);
    }

    #[test]
    fn a_skipped_stage_leaves_the_speed_factor_alone() {
        // Align was skipped: predicted zero, with whatever time it spent.
        let stages = running_fuse([(1.0, 1.0), (0.0, 0.0), (0.0, 7.0)], 10.0, 0.0);
        assert_secs(live_eta(&stages, Some(0.0), None).stage, 10.0);
    }

    #[test]
    fn nothing_predicted_gives_a_zero_total_without_dividing() {
        let stages = running_fuse([(0.0, 2.0); 3], 0.0, 0.0);
        let live = live_eta(&stages, Some(0.5), None);
        assert_eq!((live.stage, live.total), (None, Duration::ZERO));
    }

    #[test]
    fn an_idle_run_has_either_nothing_left_or_only_stages_to_come() {
        let finished = plan(StageState::Finished, 4.0, 4.0);
        let done = live_eta(&[finished; 6], None, None);
        assert_eq!(
            (done.stage, done.total),
            (Some(Duration::ZERO), Duration::ZERO)
        );

        let mut ahead = [finished; 6];
        ahead[3] = plan(StageState::Pending, 6.0, 0.0);
        let live = live_eta(&ahead, None, None);
        assert_eq!(live.stage, None);
        assert_secs(Some(live.total), 6.0);
    }

    #[test]
    fn the_total_is_never_below_the_stage_estimate() {
        let stages = running_fuse([(1.0, 2.0), (0.0, 0.0), (3.0, 1.0)], 10.0, 0.5);
        for fraction in [0.0, 0.2, 0.9, 1.0] {
            let live = live_eta(&stages, Some(fraction), Some(Duration::from_secs(40)));
            assert!(live.total >= live.stage.expect("a fuse prediction exists"));
        }
    }

    #[test]
    fn eta_never_counts_the_still_running_wall_clock() {
        // A caller must feed `eta` the *accumulated* running time of
        // completed segments only; this test pins that the function itself
        // does no clock reading of its own -- same `running` in, same `eta`
        // out, regardless of how much real time has since elapsed.
        let first = eta(1, 4, Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(5));
        let second = eta(1, 4, Duration::from_secs(1));
        assert_eq!(first, second);
    }
}
