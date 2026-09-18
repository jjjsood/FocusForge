// SPDX-License-Identifier: GPL-3.0-or-later

//! Flags and small values `stack`/`align` pass down into
//! `focusforge_core::pipeline::run`, which owns the segment-drive loop.
//!
//! Pause (SIGUSR1) has no Windows equivalent -- `crate::signals::install`
//! only ever arms it `#[cfg(unix)]`; elsewhere the flag this module polls is
//! simply never set, so `drive` never observes a pause request there.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use focusforge_core::job::{ControlState, Job, JobControl, JobOutcome};
use focusforge_core::tile::{FramePreprocess, TileConsumer, TileError};

use crate::progress::ProgressRenderer;
use crate::signals::Signals;

/// How often the pipeline's poll thread ticks progress and this crate's
/// signal flags. Cancel latency is therefore `POLL` plus at most
/// `TileScheduler::in_flight_limit()` further units; with
/// `--max-host-memory 1` that is one tile, which is what makes the
/// SIGINT tests deterministic.
pub const POLL: Duration = Duration::from_millis(100);

/// `--resume`, `--quiet`, `--tile-size` and `--no-cache`, bundled to keep
/// `stack::run`'s argument count down.
pub struct JobOptions {
    pub resume: bool,
    pub quiet: bool,
    pub tile_size: Option<u32>,
    /// `true` unless `--no-cache` was given. Disables **both** the stage
    /// cache read and the write -- a user reaching for `--no-cache`
    /// suspects the cache, so leaving a fresh entry behind would be a
    /// surprise.
    pub cache: bool,
}

/// A resumable candidate's already-recorded alignment, peeked off
/// `.scratch/resume/*/job.json`'s sibling `alignment.json` before
/// `align_stack` has run -- see
/// `crate::stack::peek_resumable_crop_and_alignment` for why this exists:
/// avoiding the expensive ECC solve is the entire point of `--resume`
/// skipping alignment, and `auto_crop` (cheap, pure geometry over
/// transforms) is recomputed fresh from this value either way, so no
/// `crop` needs peeking at all -- only the transforms genuinely gate on
/// having run `align_stack`.
#[derive(Debug, Clone)]
pub struct PeekedResume {
    pub alignment: Option<focusforge_core::align::StackAlignment>,
}

/// Runs `job` to completion: repeatedly runs one segment (`Job::run`
/// returns after one), translating flags into `JobControl` calls on a
/// caller-side poll loop, and looping again on `JobOutcome::Paused` per
/// the resume contract, with the same `consumer` both times.
pub fn drive(
    job: &mut Job<'_>,
    control: &Arc<JobControl>,
    preprocess: &dyn FramePreprocess,
    consumer: &dyn TileConsumer,
    renderer: &mut ProgressRenderer,
    signals: &Signals,
) -> Result<JobOutcome, TileError> {
    loop {
        let outcome = run_segment(job, control, preprocess, consumer, renderer, signals)?;
        match outcome {
            JobOutcome::Completed | JobOutcome::Cancelled => return Ok(outcome),
            JobOutcome::Paused => {
                if control.state() == ControlState::CancelRequested {
                    // A cancel raced the pause within the same segment:
                    // nothing to wait for, the next segment observes it
                    // immediately.
                    continue;
                }
                renderer.stage("paused -- send SIGUSR1 to resume (Ctrl-C to cancel)");
                wait_for_resume_or_cancel(control, signals);
            }
        }
    }
}

/// Runs exactly one [`Job::run`] segment on a scoped thread, polling
/// `signals`/`control.progress()` from this thread every [`POLL`] -- the
/// same `std::thread::scope` shape the resume tests already use against
/// this API.
fn run_segment(
    job: &mut Job<'_>,
    control: &Arc<JobControl>,
    preprocess: &dyn FramePreprocess,
    consumer: &dyn TileConsumer,
    renderer: &mut ProgressRenderer,
    signals: &Signals,
) -> Result<JobOutcome, TileError> {
    let mut desired_pause = false;
    // Span context does not cross onto the segment thread by itself: the
    // stage span current here is captured and re-entered there, so every
    // event the scheduler raises names the stage that drives it.
    let parent = tracing::Span::current();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let _parent = parent.enter();
            job.run(preprocess, consumer)
        });
        loop {
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(POLL);
            // `cancel` is only ever *read* here -- see `crate::signals`'
            // module docs for why resetting it would disarm the
            // second-Ctrl-C kill.
            if signals.cancel.load(Ordering::SeqCst) {
                control.cancel();
            }
            if signals.pause.swap(false, Ordering::SeqCst) {
                desired_pause = !desired_pause;
                if desired_pause {
                    control.pause();
                } else {
                    control.clear();
                }
            }
            renderer.tick(&control.progress());
        }
        handle.join().expect("job segment thread must not panic")
    })
}

/// Blocks (polling every [`POLL`]) until either a fresh SIGUSR1 toggles the
/// pause back off (`control.clear()`, ready for the next segment) or SIGINT
/// requests a cancel (`control.cancel()`, so the next segment observes it
/// immediately and returns `Cancelled` without running any further tile).
fn wait_for_resume_or_cancel(control: &Arc<JobControl>, signals: &Signals) {
    loop {
        std::thread::sleep(POLL);
        if signals.cancel.load(Ordering::SeqCst) {
            control.cancel();
            return;
        }
        if signals.pause.swap(false, Ordering::SeqCst) {
            control.clear();
            return;
        }
    }
}

/// `<argv> --resume`, verbatim, minus any `--resume` already present --
/// printed on a cancel so the exact recipe to continue is right there in
/// the terminal.
pub fn resume_recipe(args: &[String]) -> String {
    let mut parts: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--resume")
        .cloned()
        .collect();
    parts.push("--resume".to_string());
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_recipe_appends_resume_once() {
        let args = vec!["stack".to_string(), "dir".to_string(), "-o".to_string()];
        assert_eq!(resume_recipe(&args), "stack dir -o --resume");
    }

    #[test]
    fn resume_recipe_does_not_duplicate_an_existing_resume_flag() {
        let args = vec!["stack".to_string(), "--resume".to_string()];
        assert_eq!(resume_recipe(&args), "stack --resume");
    }
}
