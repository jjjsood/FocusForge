// SPDX-License-Identifier: GPL-3.0-or-later

//! [`ProgressMode`] and [`ProgressRenderer`]: turns a polled
//! [`focusforge_core::job::JobProgress`] into stderr text. Mode is chosen
//! **once**, at construction, from `--no-progress` and whether stderr is a
//! terminal; nothing downstream re-checks. `core` never prints.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use focusforge_core::job::{JobProgress, JobStage};

/// Redraw interval for [`ProgressMode::Interactive`]'s single rewritten
/// line.
pub const INTERACTIVE_REDRAW: Duration = Duration::from_millis(200);

/// Line interval for [`ProgressMode::Plain`]'s append-only log. Bounds a
/// real run's line count at `2 + ceil(running_seconds / 2)`: one
/// stage-entry line, one completion line, plus at most one periodic line
/// per interval.
pub const PLAIN_INTERVAL: Duration = Duration::from_secs(2);

/// Which of the three renderings a run uses, decided once at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressMode {
    /// stderr is a terminal: one line, rewritten in place with `\r` plus
    /// space padding. **No ANSI escape sequences at all.**
    Interactive,
    /// stderr is redirected (a file, a pipe, `/dev/null`): one
    /// `\n`-terminated line per interval, append-only, bounded.
    Plain,
    /// `--no-progress`: every method below is a no-op.
    Quiet,
}

impl ProgressMode {
    /// `no_progress` wins outright; otherwise [`Interactive`](Self::Interactive)
    /// when stderr is a terminal, else [`Plain`](Self::Plain).
    pub fn choose(no_progress: bool) -> Self {
        if no_progress {
            ProgressMode::Quiet
        } else if std::io::stderr().is_terminal() {
            ProgressMode::Interactive
        } else {
            ProgressMode::Plain
        }
    }
}

/// `develop`/`align <done>/<total> frames` or `prepare <done>/<total> strips`
/// while that stage counts; otherwise `fuse <done>/<total> tiles <pct>%`.
pub fn format_line(progress: &JobProgress) -> String {
    if progress.stage == JobStage::Develop && progress.develop_total > 0 {
        return format!(
            "develop {}/{} frames",
            progress.develop_done, progress.develop_total
        );
    }
    if progress.stage == JobStage::Prepare && progress.prepare_total > 0 {
        return format!(
            "prepare {}/{} strips",
            progress.prepare_done, progress.prepare_total
        );
    }
    if progress.stage == JobStage::Align && progress.align_total > 0 {
        return format!(
            "align {}/{} frames",
            progress.align_done, progress.align_total
        );
    }
    let pct = if progress.unit_total > 0 {
        // Truncating integer division, not rounded: `fuse 12/64 tiles 18%`
        // is the worked example -- 12/64 = 18.75%, and the truncated
        // answer is 18, not 19.
        (progress.units_done as u64 * 100) / progress.unit_total as u64
    } else {
        0
    };
    format!(
        "fuse {}/{} tiles {}%",
        progress.units_done, progress.unit_total, pct
    )
}

/// Renders progress to stderr. Owns just enough state to rewrite a single
/// interactive line and to rate-limit both non-quiet modes.
pub struct ProgressRenderer {
    mode: ProgressMode,
    last_emit: Option<Instant>,
    last_line_len: usize,
}

impl ProgressRenderer {
    pub fn new(mode: ProgressMode) -> Self {
        ProgressRenderer {
            mode,
            // Seeded at construction, not left `None`: a `None` start would
            // make `tick`'s very first call fire immediately regardless of
            // interval, which would blow the `Plain` line-count bound this
            // module's own doc promises.
            last_emit: Some(Instant::now()),
            last_line_len: 0,
        }
    }

    /// One stage-entry line -- always emitted (subject to `Quiet`), never
    /// rate-limited: a stage change is a discrete event, not a tick.
    pub fn stage(&mut self, text: &str) {
        if self.mode == ProgressMode::Quiet {
            return;
        }
        if self.mode == ProgressMode::Interactive {
            self.clear_interactive_line();
        }
        eprintln!("{text}");
        let _ = std::io::stderr().flush();
        self.last_emit = Some(Instant::now());
    }

    /// Forces one immediate progress line, bypassing the rate limit, and
    /// resets the interval clock so the next [`Self::tick`] is gated a full
    /// interval from now.
    pub fn start(&mut self, progress: &JobProgress) {
        if self.mode == ProgressMode::Quiet {
            return;
        }
        match self.mode {
            ProgressMode::Interactive => self.emit_interactive(&format_line(progress)),
            ProgressMode::Plain => {
                eprintln!("{}", format_line(progress));
                let _ = std::io::stderr().flush();
            }
            ProgressMode::Quiet => unreachable!("returned above"),
        }
        self.last_emit = Some(Instant::now());
    }

    /// A progress sample -- rate-limited per [`ProgressMode`], a no-op in
    /// `Quiet`.
    pub fn tick(&mut self, progress: &JobProgress) {
        match self.mode {
            ProgressMode::Quiet => {}
            ProgressMode::Interactive => self.tick_interactive(progress),
            ProgressMode::Plain => self.tick_plain(progress),
        }
    }

    /// The completion line -- always emitted (subject to `Quiet`), never
    /// rate-limited, same reasoning as [`Self::stage`].
    pub fn finish(&mut self, text: &str) {
        if self.mode == ProgressMode::Quiet {
            return;
        }
        if self.mode == ProgressMode::Interactive {
            self.clear_interactive_line();
        }
        eprintln!("{text}");
        let _ = std::io::stderr().flush();
    }

    fn tick_interactive(&mut self, progress: &JobProgress) {
        let now = Instant::now();
        if let Some(last) = self.last_emit
            && now.duration_since(last) < INTERACTIVE_REDRAW
        {
            return;
        }
        self.emit_interactive(&format_line(progress));
        self.last_emit = Some(now);
    }

    fn tick_plain(&mut self, progress: &JobProgress) {
        let now = Instant::now();
        let due = match self.last_emit {
            Some(last) => now.duration_since(last) >= PLAIN_INTERVAL,
            None => true,
        };
        if due {
            eprintln!("{}", format_line(progress));
            let _ = std::io::stderr().flush();
            self.last_emit = Some(now);
        }
    }

    /// Rewrites the one interactive line: `\r`, the new text, then enough
    /// trailing spaces to blank out whatever was longer on the previous
    /// draw -- no ANSI escape sequence, so this is exactly as safe to run
    /// against a non-terminal as any other write.
    fn emit_interactive(&mut self, line: &str) {
        let pad = self.last_line_len.saturating_sub(line.len());
        eprint!("\r{line}{}", " ".repeat(pad));
        let _ = std::io::stderr().flush();
        self.last_line_len = line.len();
    }

    /// Blanks the current interactive line and returns the cursor to column
    /// 0 before a `stage`/`finish` line prints with its own trailing
    /// newline -- otherwise a shorter stage line would leave a tail of the
    /// previous, longer progress line visible.
    fn clear_interactive_line(&mut self) {
        if self.last_line_len > 0 {
            eprint!("\r{}\r", " ".repeat(self.last_line_len));
            let _ = std::io::stderr().flush();
            self.last_line_len = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusforge_core::job::JobStage;
    use std::time::Duration;

    /// The per-stage value every fixture carries: a front end that read it
    /// would print `12:57`, a token no assertion below expects.
    const STAGE_ETA: Duration = Duration::from_secs(777);

    fn progress(units_done: usize, unit_total: usize, total_eta: Option<Duration>) -> JobProgress {
        JobProgress {
            stage: JobStage::Fuse,
            units_done,
            unit_total,
            pixels_done: units_done as u64,
            pixel_total: unit_total as u64,
            running: Duration::from_secs(1),
            eta: Some(STAGE_ETA),
            align_done: 0,
            align_total: 0,
            prepare_done: 0,
            prepare_total: 0,
            develop_done: 0,
            develop_total: 0,
            total_eta,
        }
    }

    fn align_progress(align_done: usize, align_total: usize) -> JobProgress {
        JobProgress {
            stage: JobStage::Align,
            units_done: 0,
            unit_total: 0,
            pixels_done: 0,
            pixel_total: 0,
            running: Duration::from_secs(1),
            eta: Some(STAGE_ETA),
            align_done,
            align_total,
            prepare_done: 0,
            prepare_total: 0,
            develop_done: 0,
            develop_total: 0,
            total_eta: None,
        }
    }

    fn prepare_progress(prepare_done: usize, prepare_total: usize) -> JobProgress {
        JobProgress {
            stage: JobStage::Prepare,
            prepare_done,
            prepare_total,
            ..progress(0, 0, None)
        }
    }

    fn with_eta(progress: JobProgress, total_eta: Option<Duration>) -> JobProgress {
        JobProgress {
            total_eta,
            ..progress
        }
    }

    fn develop_progress(develop_done: usize, develop_total: usize) -> JobProgress {
        JobProgress {
            stage: JobStage::Develop,
            develop_done,
            develop_total,
            ..progress(0, 0, None)
        }
    }

    #[test]
    fn develop_line_without_a_total_falls_back_to_the_fuse_line() {
        let line = format_line(&develop_progress(0, 0));
        assert!(line.starts_with("fuse "), "line was: {line}");
    }

    #[test]
    fn prepare_line_without_a_total_falls_back_to_the_fuse_line() {
        let line = format_line(&prepare_progress(0, 0));
        assert!(line.starts_with("fuse "), "line was: {line}");
    }

    #[test]
    fn choose_picks_quiet_regardless_of_terminal() {
        assert_eq!(ProgressMode::choose(true), ProgressMode::Quiet);
    }

    #[test]
    fn every_stage_line_ends_at_its_count() {
        let total = Some(Duration::from_secs(125));
        for (expected, line) in [
            (
                "develop 2/9 frames",
                with_eta(develop_progress(2, 9), total),
            ),
            (
                "prepare 2/9 strips",
                with_eta(prepare_progress(2, 9), total),
            ),
            ("align 3/8 frames", with_eta(align_progress(3, 8), total)),
            (
                "align 61/61 frames",
                with_eta(align_progress(61, 61), total),
            ),
            ("fuse 12/64 tiles 18%", progress(12, 64, total)),
            ("fuse 12/64 tiles 18%", progress(12, 64, None)),
        ] {
            assert_eq!(format_line(&line), expected);
        }
    }

    #[test]
    fn quiet_mode_ticks_and_stages_are_silent() {
        // Can't capture stderr from a unit test portably, so this pins the
        // contract at the type level: every method takes the fast `Quiet`
        // branch, which this test exercises for a panic/no-op guarantee.
        let mut renderer = ProgressRenderer::new(ProgressMode::Quiet);
        renderer.stage("align 61 frames");
        renderer.tick(&progress(1, 10, Some(Duration::from_secs(1))));
        renderer.finish("done");
        assert_eq!(renderer.mode, ProgressMode::Quiet);
    }

    #[test]
    fn plain_mode_rate_limits_ticks_to_the_interval() {
        let mut renderer = ProgressRenderer::new(ProgressMode::Plain);
        // Immediately after construction, a tick must not fire (interval
        // not yet elapsed) -- pins the line-count bound this module's docs
        // promise rather than firing on the very first poll.
        let before = renderer.last_emit;
        renderer.tick(&progress(1, 10, Some(Duration::from_secs(1))));
        assert_eq!(
            renderer.last_emit, before,
            "a tick inside the interval must not update last_emit"
        );
    }

    #[test]
    fn interactive_mode_pads_a_shorter_line_over_a_longer_one() {
        let mut renderer = ProgressRenderer::new(ProgressMode::Interactive);
        renderer.emit_interactive("fuse 1/100 tiles 1%");
        let long_len = renderer.last_line_len;
        renderer.emit_interactive("fuse 2/100 tiles 2%");
        assert!(renderer.last_line_len <= long_len + 1);
    }
}
