// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli history`: every checkpoint operation reachable headless --
//! list, delete a checkpoint, and compact a project to its live checkpoints.
//!
//! Each subcommand is a thin adapter over one `focusforge_core::project`
//! entry point: argument parsing, a report and an exit code, no storage logic
//! of its own.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use focusforge_core::project::{self, Checkpoint, History, Project, ProjectError};
use serde::Serialize;

#[derive(Subcommand)]
pub enum HistoryCommand {
    /// List every visible checkpoint, newest first.
    List(ListArgs),
    /// Delete one checkpoint: it is hidden, and dropped from the project once no
    /// visible checkpoint is built on it; its space is reclaimed by
    /// `history compact`. Refuses the last visible checkpoint.
    Delete(DeleteArgs),
    /// Rewrite the project to its live checkpoints only and report the bytes
    /// reclaimed.
    Compact(CompactArgs),
}

#[derive(Args)]
pub struct ListArgs {
    /// The project to list.
    project: PathBuf,
    /// Emit the JSON document instead of human-readable text.
    #[arg(long)]
    json: bool,
    /// Accept changed source frames, printing their warning instead of refusing.
    #[arg(long = "accept-changed-sources")]
    accept_changed_sources: bool,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// The project to delete from.
    project: PathBuf,
    /// The checkpoint to delete -- any unique prefix of its id.
    #[arg(long)]
    checkpoint: String,
    /// Emit the JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,
    /// Accept changed source frames, printing their warning instead of refusing.
    #[arg(long = "accept-changed-sources")]
    accept_changed_sources: bool,
}

#[derive(Args)]
pub struct CompactArgs {
    /// The project to compact.
    project: PathBuf,
    /// Emit the JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,
}

pub fn run(command: HistoryCommand) -> ExitCode {
    match command {
        HistoryCommand::List(args) => list(&args.project, args.json, args.accept_changed_sources),
        HistoryCommand::Delete(args) => delete(
            &args.project,
            &args.checkpoint,
            args.json,
            args.accept_changed_sources,
        ),
        HistoryCommand::Compact(args) => compact(&args.project, args.json),
    }
}

/// Maps a refusal to its own exit code: 3 not a project, 4 truncated, 6 a
/// missing entry, 7 a blob hash mismatch, 8 a structural link failure (a
/// dangling parent, a parent cycle, a fingerprint disagreement, ops with no
/// parent to record them against). Anything else stays the generic 1.
pub(crate) fn exit_code_for(err: &ProjectError) -> u8 {
    match err {
        ProjectError::NotAProject { .. } => 3,
        ProjectError::Truncated { .. } => 4,
        ProjectError::MissingEntry { .. } => 6,
        ProjectError::HashMismatch { .. } => 7,
        ProjectError::DanglingParent { .. }
        | ProjectError::ParentCycle { .. }
        | ProjectError::FingerprintDisagrees { .. }
        | ProjectError::OpsWithoutParent { .. } => 8,
        _ => 1,
    }
}

/// `#<index> <path>` for each changed source, comma-separated -- what both
/// the exit-9 refusal and an accepted mismatch's warning name.
fn describe_changed(changed: &[(usize, PathBuf)]) -> String {
    changed
        .iter()
        .map(|(index, path)| format!("#{index} {}", path.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Opens `project_path` the way every user-facing `history` subcommand does:
/// [`Project::open_checked`], printing every warning it carries on stderr,
/// and refusing on its own exit code rather than the generic one. A changed
/// source exits 9 naming `--accept-changed-sources` unless
/// `accept_changed_sources` is set, in which case it is accepted and its own
/// warning line takes the refusal's place.
fn open_for_history(
    project_path: &Path,
    accept_changed_sources: bool,
) -> Result<Project, ExitCode> {
    let opened = Project::open_checked(project_path)
        .map_err(|err| fail_with_code(err.to_string(), exit_code_for(&err)))?;
    let project = match opened {
        project::Opened::Ready(project) => project,
        project::Opened::NeedsChoice(pending) => {
            if !accept_changed_sources {
                return Err(fail_with_code(
                    format!(
                        "{} source frame(s) changed since this project was written: {} -- pass \
                         --accept-changed-sources to proceed anyway; its Z-Map indices would \
                         select pixels from frames it was not stacked from",
                        pending.changed().len(),
                        describe_changed(pending.changed()),
                    ),
                    9,
                ));
            }
            pending.proceed()
        }
    };
    let validation = project.validation();
    if validation.unresolved_sources > 0 {
        eprintln!(
            "warning: {} source frame(s) unresolved -- the history and Z-Map are intact; \
             viewing an edited checkpoint's image or re-fusing needs them",
            validation.unresolved_sources
        );
    }
    if let Some(mismatch) = &validation.accepted_mismatch {
        eprintln!(
            "warning: {} source frame(s) changed since this project was written: {} -- its \
             Z-Map indices may select pixels from frames it was not stacked from",
            mismatch.changed.len(),
            describe_changed(&mismatch.changed),
        );
    }
    Ok(project)
}

/// Resolves `id_or_prefix` to exactly one visible checkpoint of `history`: an
/// exact id first, else the unique visible checkpoint whose id starts with
/// it. An ambiguous prefix names every match; no match, and a hidden id,
/// reuse [`ProjectError::UnknownCheckpoint`]'s own wording.
pub(crate) fn resolve_checkpoint<'a>(
    history: &'a History,
    project_path: &Path,
    id_or_prefix: &str,
) -> Result<&'a Checkpoint, String> {
    if let Ok(checkpoint) = history.get(id_or_prefix)
        && !checkpoint.hidden
    {
        return Ok(checkpoint);
    }
    let matches: Vec<&Checkpoint> = history
        .visible()
        .into_iter()
        .filter(|c| c.id.starts_with(id_or_prefix))
        .collect();
    match matches.as_slice() {
        [] => Err(ProjectError::UnknownCheckpoint {
            path: project_path.to_path_buf(),
            id: id_or_prefix.to_string(),
        }
        .to_string()),
        [only] => Ok(only),
        many => Err(format!(
            "{}: {id_or_prefix} matches {} checkpoints: {}",
            project_path.display(),
            many.len(),
            many.iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// One checkpoint's listed identity: never a storage-kind field, which
/// `--json` output must never reveal.
#[derive(Serialize)]
struct CheckpointRecord {
    id: String,
    parent: Option<String>,
    algorithm: String,
    created_at: i64,
}

/// `{"checkpoints":[...]}`.
#[derive(Serialize)]
struct ListDocument {
    checkpoints: Vec<CheckpointRecord>,
}

fn list(project_path: &Path, json: bool, accept_changed_sources: bool) -> ExitCode {
    let project = match open_for_history(project_path, accept_changed_sources) {
        Ok(project) => project,
        Err(exit) => return exit,
    };
    let history = project.history();
    let checkpoints: Vec<CheckpointRecord> = history
        .visible()
        .into_iter()
        .map(|c| CheckpointRecord {
            id: c.id.clone(),
            parent: history
                .ancestors(&c.id)
                .expect("open validated every parent link")
                .into_iter()
                .skip(1)
                .find(|ancestor| !ancestor.hidden)
                .map(|ancestor| ancestor.id.clone()),
            algorithm: c.algorithm.clone(),
            created_at: c.created_at,
        })
        .collect();
    let document = ListDocument { checkpoints };
    if json {
        match serde_json::to_string_pretty(&document) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        for c in &document.checkpoints {
            println!(
                "{} parent={} algorithm={} created={}",
                c.id,
                c.parent.as_deref().unwrap_or("-"),
                c.algorithm,
                focusforge_diagnostics::rfc3339(c.created_at),
            );
        }
    }
    ExitCode::SUCCESS
}

#[derive(Serialize)]
struct DeleteReport {
    project: String,
    hidden: String,
    removed: Vec<String>,
    head: String,
}

fn delete(
    project_path: &Path,
    checkpoint_id: &str,
    json: bool,
    accept_changed_sources: bool,
) -> ExitCode {
    let target = {
        let project = match open_for_history(project_path, accept_changed_sources) {
            Ok(project) => project,
            Err(exit) => return exit,
        };
        match resolve_checkpoint(project.history(), project_path, checkpoint_id) {
            Ok(checkpoint) => checkpoint.id.clone(),
            Err(message) => return fail(message),
        }
    };

    let deleted = match project::delete_checkpoint(project_path, &target) {
        Ok(deleted) => deleted,
        Err(err) => return fail(err),
    };
    compact_after_save(project_path);

    let report = DeleteReport {
        project: project_path.display().to_string(),
        hidden: deleted.hidden,
        removed: deleted.removed,
        head: deleted.head,
    };
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!(
            "{}\nhidden: {}\nremoved: {}\nhead: {}",
            report.project,
            report.hidden,
            report.removed.join(", "),
            report.head
        );
    }
    ExitCode::SUCCESS
}

/// Compacts the project when most of it is dead data. The save already
/// committed, so a failure here is a warning and the command still succeeds.
pub(crate) fn compact_after_save(project_path: &Path) {
    match project::container::compact_if_mostly_dead(project_path) {
        Ok(Some(reclaimed)) => eprintln!(
            "compacted {}: {reclaimed} bytes reclaimed",
            project_path.display()
        ),
        Ok(None) => {}
        Err(err) => eprintln!(
            "warning: could not compact {}: {err}",
            project_path.display()
        ),
    }
}

#[derive(Serialize)]
struct CompactReport {
    project: String,
    reclaimed_bytes: u64,
}

fn compact(project_path: &Path, json: bool) -> ExitCode {
    let reclaimed_bytes = match project::container::compact(project_path) {
        Ok(reclaimed) => reclaimed,
        Err(err) => return fail_with_code(&err, exit_code_for(&err)),
    };
    let report = CompactReport {
        project: project_path.display().to_string(),
        reclaimed_bytes,
    };
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!(
            "{}\nreclaimed: {} bytes",
            report.project, report.reclaimed_bytes
        );
    }
    ExitCode::SUCCESS
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    crate::fail(message)
}

fn fail_with_code(message: impl std::fmt::Display, code: u8) -> ExitCode {
    crate::fail_with_code(message, code)
}
