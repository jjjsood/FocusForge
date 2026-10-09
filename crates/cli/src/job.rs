// SPDX-License-Identifier: GPL-3.0-or-later

//! Flags and small values `stack`/`align` pass down into
//! `focusforge_core::pipeline::run`, which owns the segment-drive loop.
//!
//! Pause (SIGUSR1) has no Windows equivalent -- `crate::signals::install`
//! only ever arms it `#[cfg(unix)]`; elsewhere the flag a
//! [`focusforge_core::pipeline::StageObserver`] polls is simply never set,
//! so a run there never observes a pause request.

use std::time::Duration;

/// How often the pipeline's poll thread ticks progress and this crate's
/// signal flags. Cancel latency is therefore `POLL` plus at most
/// `TileScheduler::in_flight_limit()` further units; with
/// `--max-host-memory 1` that is one tile, which is what makes the
/// SIGINT tests deterministic.
pub const POLL: Duration = Duration::from_millis(100);

/// `--resume`, `--no-progress`, `--tile-size` and `--no-cache`, bundled to keep
/// `stack::run`'s argument count down.
pub struct JobOptions {
    pub resume: bool,
    pub quiet: bool,
    pub no_progress: bool,
    pub tile_size: Option<u32>,
    /// `true` unless `--no-cache` was given. Disables **both** the stage
    /// cache read and the write -- a user reaching for `--no-cache`
    /// suspects the cache, so leaving a fresh entry behind would be a
    /// surprise.
    pub cache: bool,
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
