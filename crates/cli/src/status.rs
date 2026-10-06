// SPDX-License-Identifier: GPL-3.0-or-later

//! [`StatusSink`]: the `--status <path>` NDJSON stream -- one record per
//! stack, appended and flushed as it happens.
//!
//! Three streams, three owners, never mixed: `--json`'s document owns
//! stdout, progress output owns stderr, and this file owns the status
//! records. A script driving many invocations branches on these records
//! instead of parsing progress text.
//!
//! Every terminal path writes exactly one record: [`StatusSink::fail`]
//! records, then refuses like [`crate::fail`], and [`StatusSink`]'s [`Drop`]
//! `debug_assert!`s that a record was produced.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::{Deserialize, Serialize};

/// One stack's verdict. `outcome` follows the exit codes: 0 -> `completed`,
/// 130 -> `cancelled`, 1 -> `failed`, plus `dry-run` for a run that
/// computed nothing.
#[derive(Serialize, Deserialize, Debug)]
pub struct StatusRecord {
    /// The identity of the one stack this record describes -- the input
    /// directory as the user wrote it for a single-group run, matching
    /// `StackReport::input`, or the group name for a multi-group one.
    pub identity: String,
    pub command: String,
    pub outcome: String,
    pub algorithm: Option<String>,
    pub outputs: Vec<String>,
    pub wall_seconds: f64,
    pub error: Option<String>,
}

pub struct StatusSink<'a> {
    path: Option<&'a Path>,
    identity: String,
    command: &'static str,
    start: Instant,
    algorithm: RefCell<Option<String>>,
    written: Cell<bool>,
}

impl<'a> StatusSink<'a> {
    /// With `path: None` every method below is a no-op writer, exactly as
    /// `ProgressMode::Quiet` is for `ProgressRenderer` -- the
    /// record-was-produced bookkeeping still runs, so the `Drop` assertion
    /// covers runs that never passed `--status` too.
    pub fn new(path: Option<&'a Path>, identity: String, command: &'static str) -> Self {
        StatusSink {
            path,
            identity,
            command,
            start: Instant::now(),
            algorithm: RefCell::new(None),
            written: Cell::new(false),
        }
    }

    /// Records which algorithm this run resolved, as soon as it is known --
    /// before that, a refusal's record carries `algorithm: null`.
    pub fn set_algorithm(&self, id: &str) {
        *self.algorithm.borrow_mut() = Some(id.to_string());
    }

    /// One `serde_json::to_string` plus `'\n'`, appended and flushed per
    /// record, so a script tailing the file sees each stack's verdict as it
    /// happens rather than at process exit.
    ///
    /// A write failure warns on stderr and is not itself a refusal: a
    /// `--status` path that cannot be written must not turn a completed
    /// stack into a failed one, and a refusal that failed to record must
    /// still report the refusal it was called for.
    fn emit(&self, outcome: &str, outputs: Vec<String>, error: Option<String>) {
        debug_assert!(
            !self.written.get(),
            "exactly one status record per run; a second was emitted as {outcome:?}"
        );
        self.written.set(true);
        let Some(path) = self.path else {
            return;
        };
        let record = StatusRecord {
            identity: self.identity.clone(),
            command: self.command.to_string(),
            outcome: outcome.to_string(),
            algorithm: self.algorithm.borrow().clone(),
            outputs,
            wall_seconds: self.start.elapsed().as_secs_f64(),
            error,
        };
        if let Err(err) = append(path, &record) {
            eprintln!("warning: --status {}: {err}", path.display());
            tracing::warn!(
                target: MIRRORED_OUTPUT_TARGET,
                "warning: --status {}: {err}",
                path.display()
            );
        }
    }

    /// Records `failed` with `message`, then refuses exactly as
    /// [`crate::fail`] does: `error: <message>` on stderr, exit 1.
    pub fn fail(&self, message: impl std::fmt::Display) -> ExitCode {
        let message = message.to_string();
        self.emit("failed", Vec::new(), Some(message.clone()));
        crate::fail(message)
    }

    /// Records `cancelled`. The caller returns 130 itself: a cancel is not a
    /// refusal and prints its own resume recipe.
    pub fn cancelled(&self) {
        self.emit("cancelled", Vec::new(), None);
    }

    pub fn completed(&self, outputs: &[String]) {
        self.emit("completed", outputs.to_vec(), None);
    }

    /// Records `dry-run`: `outputs` are the paths the run *would* have
    /// written, never paths that exist.
    pub fn dry_run(&self, would_write: &[String]) {
        self.emit("dry-run", would_write.to_vec(), None);
    }
}

/// Appends one record as a single line, opened per record: a file reopened
/// in append mode lets a script rotate or truncate the stream between
/// invocations without the next run writing into a stale offset.
fn append(path: &Path, record: &StatusRecord) -> Result<(), String> {
    use std::io::Write;

    let mut line = serde_json::to_string(record).map_err(|err| err.to_string())?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| err.to_string())?;
    file.write_all(line.as_bytes())
        .map_err(|err| err.to_string())?;
    file.flush().map_err(|err| err.to_string())
}

impl Drop for StatusSink<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        debug_assert!(
            self.written.get(),
            "every terminal path must write exactly one status record; \
             this run ended without one"
        );
    }
}
