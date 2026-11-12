// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobProgress`], the plain snapshot [`super::JobControl::progress`]
//! returns by value, and the ETA functions every snapshot consults: [`eta`]
//! for a rate observed on one stage, `live_eta` to count a stage's prediction
//! down as its units are credited. They are kept in one place so the policy is
//! replaceable without touching the scheduler and unit-testable without one.

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

/// One stage's predicted wall seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StagePlan {
    pub state: StageState,
    pub predicted: f64,
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

/// The pace a finished stage gives the rest of the run: its elapsed seconds
/// over its predicted seconds, at most 1 so that a stage slower than
/// predicted leaves every prediction as it is. `None` unless both are finite
/// and positive.
pub(crate) fn stage_pace(predicted: f64, elapsed: f64) -> Option<f64> {
    (predicted.is_finite() && predicted > 0.0 && elapsed.is_finite() && elapsed > 0.0)
        .then(|| (elapsed / predicted).min(1.0))
}

fn seconds(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
    } else {
        Duration::ZERO
    }
}

/// The running stage's remainder is its unfinished share of its own
/// prediction, `(1 - f) * predicted` for credited fraction `f` (0 without
/// one), so it falls only when a unit is credited and never with wall time;
/// a stage predicted zero reports `observed_remaining`. Every pending stage
/// adds its plain prediction. A `pace` (see [`stage_pace`]; 1 until a stage
/// has set one) multiplies every predicted part, never an observed remainder.
pub(crate) fn live_eta(
    stages: &[StagePlan],
    fraction: Option<f64>,
    observed_remaining: Option<Duration>,
    pace: f64,
) -> LiveEta {
    let running = stages.iter().find(|s| s.state == StageState::Running);
    // ponytail: the pace only lowers the estimate. A later stage slower than
    // the pace under-reads and one faster over-reads until it ends; a latched
    // blend with the observation would correct both.
    let stage = match running {
        Some(running) if running.predicted > 0.0 => {
            let f = fraction.map_or(0.0, |f| f.clamp(0.0, 1.0));
            Some(seconds((1.0 - f) * running.predicted * pace))
        }
        Some(_) => observed_remaining,
        None if stages.iter().all(|s| s.state != StageState::Pending) => Some(Duration::ZERO),
        None => None,
    };
    let ahead: f64 = stages
        .iter()
        .filter(|s| s.state == StageState::Pending)
        .map(|s| s.predicted)
        .sum();
    LiveEta {
        stage,
        total: stage.unwrap_or_default() + seconds(ahead * pace),
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

    fn plan(state: StageState, predicted: f64) -> StagePlan {
        StagePlan { state, predicted }
    }

    /// Setup, develop, align, fuse, warp-write, finalize with `fuse` running.
    fn running_fuse(finished: [f64; 3], fuse: f64, finalize: f64) -> [StagePlan; 6] {
        let done = |predicted| plan(StageState::Finished, predicted);
        [
            done(finished[0]),
            done(finished[1]),
            done(finished[2]),
            plan(StageState::Running, fuse),
            plan(StageState::Pending, 0.0),
            plan(StageState::Pending, finalize),
        ]
    }

    #[test]
    fn the_running_stage_is_its_unfinished_share_of_its_prediction() {
        let stages = running_fuse([0.0; 3], 10.0, 0.0);
        // The observation of 20 s does not enter a stage with a prediction.
        let observed = Some(Duration::from_secs(20));
        for (fraction, expected) in [(0.0, 10.0), (0.3, 7.0), (1.0, 0.0)] {
            let live = live_eta(&stages, Some(fraction), observed, 1.0);
            assert_secs(live.stage, expected);
        }
    }

    #[test]
    fn without_an_observed_remainder_the_stage_is_its_predicted_remainder() {
        let stages = running_fuse([1.0, 0.0, 0.0], 10.0, 0.0);
        assert_secs(live_eta(&stages, Some(0.0), None, 1.0).stage, 10.0);
        assert_secs(live_eta(&stages, Some(0.3), None, 1.0).stage, 7.0);
        assert_secs(live_eta(&stages, None, None, 1.0).stage, 10.0);
    }

    #[test]
    fn a_stage_predicted_zero_estimates_from_its_observation_alone() {
        let stages = running_fuse([0.0; 3], 0.0, 0.0);
        assert_eq!(live_eta(&stages, Some(0.4), None, 1.0).stage, None);
        assert_eq!(live_eta(&stages, None, None, 1.0).stage, None);
        let observed = Some(Duration::from_secs(5));
        assert_eq!(live_eta(&stages, Some(0.4), observed, 1.0).stage, observed);
    }

    #[test]
    fn a_stalled_fraction_does_not_raise_the_estimate() {
        // `align_rate` gives `elapsed * (1 - f) / f` while the frame counter
        // stalls; elapsed runs past the prediction.
        for (predicted, fraction, expected) in [(20.0, 0.25, 15.0), (8.0, 0.5, 4.0)] {
            let stages = running_fuse([0.0; 3], predicted, 0.0);
            for step in 0..=20 {
                let elapsed = f64::from(step) * predicted / 10.0;
                let observed = Duration::from_secs_f64(elapsed * (1.0 - fraction) / fraction);
                let live = live_eta(&stages, Some(fraction), Some(observed), 1.0);
                assert_secs(live.stage, expected);
            }
        }
    }

    #[test]
    fn nothing_predicted_gives_a_zero_total_without_dividing() {
        let stages = running_fuse([0.0; 3], 0.0, 0.0);
        let live = live_eta(&stages, Some(0.5), None, 1.0);
        assert_eq!((live.stage, live.total), (None, Duration::ZERO));
    }

    #[test]
    fn an_idle_run_has_either_nothing_left_or_only_stages_to_come() {
        let finished = plan(StageState::Finished, 4.0);
        let done = live_eta(&[finished; 6], None, None, 1.0);
        assert_eq!(
            (done.stage, done.total),
            (Some(Duration::ZERO), Duration::ZERO)
        );

        let mut ahead = [finished; 6];
        ahead[3] = plan(StageState::Pending, 6.0);
        let live = live_eta(&ahead, None, None, 1.0);
        assert_eq!(live.stage, None);
        assert_secs(Some(live.total), 6.0);
    }

    #[test]
    fn the_total_is_never_below_the_stage_estimate() {
        let stages = running_fuse([1.0, 0.0, 3.0], 10.0, 0.5);
        for fraction in [0.0, 0.2, 0.9, 1.0] {
            let live = live_eta(&stages, Some(fraction), Some(Duration::from_secs(40)), 1.0);
            assert!(live.total >= live.stage.expect("a fuse prediction exists"));
        }
    }

    #[test]
    fn the_pace_is_observed_over_predicted_seconds_never_above_one() {
        for (predicted, elapsed, expected) in [
            (12.42, 3.11, 0.2504),
            (9.40, 2.11, 0.2245),
            (13.39, 15.53, 1.0),
            (0.0309, 0.1, 1.0),
        ] {
            let pace = stage_pace(predicted, elapsed).expect("both inputs are usable");
            assert!((pace - expected).abs() < 1e-3, "{pace} vs {expected}");
        }
    }

    #[test]
    fn no_pace_without_a_positive_finite_prediction_and_span() {
        for (predicted, elapsed) in [
            (0.0, 1.0),
            (-1.0, 1.0),
            (4.0, 0.0),
            (4.0, -1.0),
            (f64::NAN, 1.0),
            (4.0, f64::NAN),
            (f64::INFINITY, 1.0),
            (4.0, f64::INFINITY),
        ] {
            assert_eq!(
                stage_pace(predicted, elapsed),
                None,
                "{predicted} {elapsed}"
            );
        }
    }

    #[test]
    fn a_pace_scales_the_running_stage_and_the_stages_to_come() {
        let stages = running_fuse([1.0, 0.0, 3.0], 10.0, 2.0);
        for (fraction, stage, total) in [(0.0, 2.5, 3.0), (0.4, 1.5, 2.0), (1.0, 0.0, 0.5)] {
            let live = live_eta(&stages, Some(fraction), Some(Duration::from_secs(40)), 0.25);
            assert_secs(live.stage, stage);
            assert_secs(Some(live.total), total);
        }
    }

    #[test]
    fn a_pace_never_scales_an_observed_remainder() {
        let stages = running_fuse([0.0; 3], 0.0, 4.0);
        let observed = Some(Duration::from_secs(5));
        let live = live_eta(&stages, Some(0.4), observed, 0.25);
        assert_eq!(live.stage, observed);
        assert_secs(Some(live.total), 6.0);
    }

    #[test]
    fn a_paced_total_still_falls_with_the_fraction() {
        let stages = running_fuse([0.0; 3], 10.0, 2.0);
        let totals: Vec<Duration> = (0..=10)
            .map(|step| live_eta(&stages, Some(f64::from(step) / 10.0), None, 0.25).total)
            .collect();
        assert!(totals.windows(2).all(|pair| pair[1] <= pair[0]));
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
