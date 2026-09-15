// SPDX-License-Identifier: GPL-3.0-or-later

//! The stage-timing type itself, no fixture and no process --
//! [`focusforge_core::timing::StageTimer`]/[`focusforge_core::timing::StageTimings`]
//! in isolation. The CLI-report end (the two `--json` schemas, the
//! accounting identity over a real run) lives in `crates/cli/tests/`.

use std::thread::sleep;
use std::time::Duration;

use focusforge_core::timing::{Stage, StageTimer};

/// A tiny, deliberate sleep between marks so two spans are provably
/// distinct on any clock resolution this project runs on -- not a
/// correctness requirement (`Instant` is monotonic regardless), just what
/// keeps "greater than" assertions honest instead of racing clock
/// granularity.
fn tick() {
    sleep(Duration::from_millis(2));
}

#[test]
fn stages_accumulate_independently() {
    let mut timer = StageTimer::new();
    tick();
    timer.mark(Stage::Setup);
    tick();
    timer.mark(Stage::Align);
    tick();
    timer.mark(Stage::Fuse);
    tick();
    timer.mark(Stage::Finalize);

    let timings = timer.finish();
    assert!(timings.setup.unwrap() > 0.0);
    assert!(timings.align.unwrap() > 0.0);
    assert!(timings.fuse.unwrap() > 0.0);
    assert!(timings.finalize.unwrap() > 0.0);
    // Never entered on this run.
    assert_eq!(timings.warp_write, None);
}

#[test]
fn a_stage_marked_twice_sums_rather_than_overwrites() {
    let mut timer = StageTimer::new();
    tick();
    timer.mark(Stage::Fuse);
    tick();
    timer.mark(Stage::Fuse);
    let total = timer.total_seconds();
    let timings = timer.finish();
    let summed = timings.fuse.unwrap();

    // `Fuse` is the only stage ever marked, so if marking twice summed
    // rather than overwrote, its accumulator equals the timer's own total
    // -- an overwrite would instead leave only the second, smaller span.
    assert!((summed - total).abs() < 1e-6);
    // And it must reflect two real sleeps, not one.
    assert!(summed >= Duration::from_millis(3).as_secs_f64());
}

#[test]
fn a_stage_never_entered_is_null_not_zero() {
    let mut timer = StageTimer::new();
    timer.mark(Stage::Setup);
    let timings = timer.finish();

    let value = serde_json::to_value(timings).expect("StageTimings must serialize");
    assert!(
        value["setup"].is_number(),
        "an entered stage must serialize as a number: {value}"
    );
    assert_eq!(
        value["align"],
        serde_json::Value::Null,
        "a stage never entered must serialize as null, not 0.0: {value}"
    );
    assert_eq!(value["fuse"], serde_json::Value::Null);
    assert_eq!(value["warp_write"], serde_json::Value::Null);
    assert_eq!(value["finalize"], serde_json::Value::Null);

    // `Some(0.0)` is distinguishable from `None`: a stage entered and
    // exited with (as close to) no measurable time still reports a number,
    // never null.
    assert!(!value["setup"].is_null());
}

#[test]
fn clock_reads_are_bounded_by_the_mark_count() {
    let mut timer = StageTimer::new();
    for _ in 0..4 {
        timer.mark(Stage::Fuse);
    }
    assert_eq!(
        timer.clock_reads(),
        5,
        "one read at new(), one per mark -- never a function of work done inside a stage"
    );

    let mut timer = StageTimer::new();
    for _ in 0..4096 {
        timer.mark(Stage::Fuse);
    }
    assert_eq!(
        timer.clock_reads(),
        4097,
        "4096 marks read the clock 4096 times plus the one at new() -- the count tracks marks, \
         never the 4096 simulated units a caller might be marking on behalf of"
    );
}

#[test]
fn marked_stages_sum_to_the_timer_total() {
    let mut timer = StageTimer::new();
    tick();
    timer.mark(Stage::Setup);
    tick();
    timer.mark(Stage::Align);
    tick();
    timer.mark(Stage::Fuse);
    tick();
    timer.mark(Stage::Finalize);
    let total = timer.total_seconds();
    let timings = timer.finish();

    let sum = timings.setup.unwrap()
        + timings.align.unwrap()
        + timings.fuse.unwrap()
        + timings.finalize.unwrap();
    assert!(
        (sum - total).abs() < 1e-6,
        "sum {sum} must equal the timer's own total {total} -- the spans are contiguous and \
         disjoint by construction"
    );
}
