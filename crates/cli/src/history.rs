// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli history`: every checkpoint operation reachable headless --
//! list, materialize one to a file, apply an op list read from a file
//! (previewing it, sealing it as a new checkpoint, or both), and delete a
//! checkpoint.
//!
//! Each subcommand is a thin adapter over one `focusforge_core::project`
//! entry point: argument parsing, a report and an exit code, no replay or
//! storage logic of its own.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, Subcommand};
use focusforge_core::project::{self, Checkpoint, History, Op, Project, ProjectError};
use serde::{Deserialize, Serialize};

use crate::retouch::would_replace;

#[derive(Subcommand)]
pub enum HistoryCommand {
    /// List every visible checkpoint, newest first.
    List(ListArgs),
    /// Write one checkpoint's Z-Map to a file.
    Materialize(MaterializeArgs),
    /// Apply an op list onto a checkpoint: `--save` seals it as a new child,
    /// `-o` writes the edited map without sealing. At least one is required;
    /// given both, the map written is the sealed checkpoint's own.
    Apply(ApplyArgs),
    /// Delete one checkpoint: it is hidden, and removed from the file once no
    /// visible checkpoint is built on it. Refuses the last visible checkpoint.
    Delete(DeleteArgs),
}

#[derive(Args)]
pub struct ListArgs {
    /// The project to list.
    project: PathBuf,
    /// Emit the frozen JSON document instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct MaterializeArgs {
    /// The project to read.
    project: PathBuf,
    /// The checkpoint to materialize -- any unique prefix of its id.
    #[arg(long)]
    checkpoint: String,
    /// Where to write the Z-Map.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: PathBuf,
    /// Emit the frozen JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// The project to apply onto.
    project: PathBuf,
    /// The op list to apply: `{"ops":[...]}`.
    #[arg(long, value_name = "FILE")]
    ops: PathBuf,
    /// The checkpoint to apply onto -- any unique prefix of its id. Defaults
    /// to the project's current head.
    #[arg(long)]
    from: Option<String>,
    /// Seal the result as a new checkpoint, a child of `--from`.
    #[arg(long)]
    save: bool,
    /// Write the edited Z-Map to this file. With `--save`, the map written
    /// is the sealed checkpoint's own; alone, the map is never sealed.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: Option<PathBuf>,
    /// Emit the frozen JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// The project to delete from.
    project: PathBuf,
    /// The checkpoint to delete -- any unique prefix of its id.
    #[arg(long)]
    checkpoint: String,
    /// Emit the frozen JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,
}

pub fn run(command: HistoryCommand, accept_changed_sources: bool) -> ExitCode {
    match command {
        HistoryCommand::List(args) => list(&args.project, args.json, accept_changed_sources),
        HistoryCommand::Materialize(args) => materialize(
            &args.project,
            &args.checkpoint,
            &args.output,
            args.json,
            accept_changed_sources,
        ),
        HistoryCommand::Apply(args) => apply(
            &args.project,
            &args.ops,
            args.from.as_deref(),
            args.save,
            args.output.as_deref(),
            args.json,
            accept_changed_sources,
        ),
        HistoryCommand::Delete(args) => delete(
            &args.project,
            &args.checkpoint,
            args.json,
            accept_changed_sources,
        ),
    }
}

/// Maps a refusal to its own exit code: 3 not a project, 4 truncated, 6 a
/// missing entry, 7 a blob hash mismatch, 8 a structural link failure (a dangling parent, a
/// parent cycle, a fingerprint disagreement, a missing patch base, ops with
/// no parent to record them against). Anything else stays the generic 1.
fn exit_code_for(err: &ProjectError) -> u8 {
    match err {
        ProjectError::NotAProject { .. } => 3,
        ProjectError::Truncated { .. } => 4,
        ProjectError::MissingEntry { .. } => 6,
        ProjectError::HashMismatch { .. } => 7,
        ProjectError::DanglingParent { .. }
        | ProjectError::ParentCycle { .. }
        | ProjectError::FingerprintDisagrees { .. }
        | ProjectError::PatchBaseMissing { .. }
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

/// Refuses `output` when it resolves to `project_path` itself, the same
/// collision `retouch --out` refuses against `--zmap`.
fn refuse_if_names_project(output: &Path, project_path: &Path) -> Result<(), ExitCode> {
    if would_replace(output, project_path) {
        return Err(fail(format!(
            "-o {} is also the project file: refusing to overwrite it",
            output.display()
        )));
    }
    Ok(())
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
struct MaterializeReport {
    project: String,
    checkpoint: String,
    wrote: String,
    wall_seconds: f64,
}

fn materialize(
    project_path: &Path,
    checkpoint_id: &str,
    output: &Path,
    json: bool,
    accept_changed_sources: bool,
) -> ExitCode {
    let start = Instant::now();
    if let Err(exit) = refuse_if_names_project(output, project_path) {
        return exit;
    }
    let mut project = match open_for_history(project_path, accept_changed_sources) {
        Ok(project) => project,
        Err(exit) => return exit,
    };
    let checkpoint = match resolve_checkpoint(project.history(), project_path, checkpoint_id) {
        Ok(checkpoint) => checkpoint.clone(),
        Err(message) => return fail(message),
    };
    if let Err(err) = project::materialize_zmap(&mut project, &checkpoint, output) {
        return fail(err);
    }
    let report = MaterializeReport {
        project: project_path.display().to_string(),
        checkpoint: checkpoint.id,
        wrote: output.display().to_string(),
        wall_seconds: start.elapsed().as_secs_f64(),
    };
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!(
            "{}\nwrote: {} (checkpoint {})\nwall: {:.3}s",
            report.project, report.wrote, report.checkpoint, report.wall_seconds
        );
    }
    ExitCode::SUCCESS
}

/// The op file's wrapped shape: `{"ops":[...]}`. A checkpoint's own JSON --
/// which carries an `ops` field among others -- reads as one; a bare array
/// does not deserialize as this struct and is refused.
#[derive(Deserialize)]
struct OpFile {
    ops: Vec<Op>,
}

fn read_op_file(path: &Path) -> Result<Vec<Op>, String> {
    let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let doc: OpFile = serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(doc.ops)
}

/// Never `SealedProject.zmap`, whose `BlobRecord.kind` would leak the
/// storage kind.
#[derive(Serialize)]
struct ApplyReport {
    project: String,
    from: String,
    checkpoint: Option<String>,
    wrote: Option<String>,
    ops: usize,
    wall_seconds: f64,
}

fn apply(
    project_path: &Path,
    ops_path: &Path,
    from: Option<&str>,
    save: bool,
    output: Option<&Path>,
    json: bool,
    accept_changed_sources: bool,
) -> ExitCode {
    let start = Instant::now();
    if !save && output.is_none() {
        return fail(
            "apply needs somewhere to put its result: --save to seal a checkpoint, \
             -o <file> for the edited map, or both",
        );
    }
    if let Some(output) = output
        && let Err(exit) = refuse_if_names_project(output, project_path)
    {
        return exit;
    }

    let ops = match read_op_file(ops_path) {
        Ok(ops) => ops,
        Err(message) => return fail(message),
    };
    let ops_count = ops.len();

    let from_id = {
        let project = match open_for_history(project_path, accept_changed_sources) {
            Ok(project) => project,
            Err(exit) => return exit,
        };
        match from {
            Some(prefix) => match resolve_checkpoint(project.history(), project_path, prefix) {
                Ok(checkpoint) => checkpoint.id.clone(),
                Err(message) => return fail(message),
            },
            None => project.checkpoint().id.clone(),
        }
    };

    let mut checkpoint_out: Option<String> = None;
    let mut wrote_out: Option<String> = None;

    if save {
        let sealed = match project::append_ops_onto(
            project_path,
            &from_id,
            ops,
            None,
            project::now_unix(),
            project::ZMapStorage::Bounded,
        ) {
            Ok(sealed) => sealed,
            Err(err) => return fail(err),
        };
        if let Some(output) = output {
            let mut project = match Project::open(project_path) {
                Ok(project) => project,
                Err(err) => return fail(err),
            };
            let checkpoint = match project.history().get(&sealed.checkpoint) {
                Ok(checkpoint) => checkpoint.clone(),
                Err(err) => return fail(err),
            };
            if let Err(err) = project::materialize_zmap(&mut project, &checkpoint, output) {
                return fail(err);
            }
            wrote_out = Some(output.display().to_string());
        }
        checkpoint_out = Some(sealed.checkpoint);
    } else {
        let output = output.expect("checked above: --save or -o is required");
        if let Err(err) = project::replay_ops_to(project_path, &from_id, ops, output) {
            return fail(err);
        }
        wrote_out = Some(output.display().to_string());
    }

    let report = ApplyReport {
        project: project_path.display().to_string(),
        from: from_id,
        checkpoint: checkpoint_out,
        wrote: wrote_out,
        ops: ops_count,
        wall_seconds: start.elapsed().as_secs_f64(),
    };
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!(
            "{}\nfrom: {}\ncheckpoint: {}\nwrote: {}\nops: {}\nwall: {:.3}s",
            report.project,
            report.from,
            report.checkpoint.as_deref().unwrap_or("-"),
            report.wrote.as_deref().unwrap_or("-"),
            report.ops,
            report.wall_seconds,
        );
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

fn fail(message: impl std::fmt::Display) -> ExitCode {
    crate::fail(message)
}

fn fail_with_code(message: impl std::fmt::Display, code: u8) -> ExitCode {
    crate::fail_with_code(message, code)
}
