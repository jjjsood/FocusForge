// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobProgress`], the plain snapshot [`super::JobControl::progress`]
//! returns by value, and the one ETA function every snapshot consults --
//! kept in one place so the policy is replaceable without touching the
//! scheduler and unit-testable without one.

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
    pub eta: Option<Duration>,
    /// How many frames' registration has completed during the align stage.
    /// `0` outside a computed alignment (no align stage, a cached alignment,
    /// or a resumed one) -- the counter is never reset once the stage ends,
    /// so a snapshot taken during a later stage still carries the align
    /// stage's own final tally.
    pub align_done: usize,
    /// The frame count a computed align stage started with, `0` when no
    /// frame is being counted.
    pub align_total: usize,
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
