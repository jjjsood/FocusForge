// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli retouch`: an op list appended to a project as one state
//! per op, each the previous one's child, on the head or on `--from`.
//!
//! Holds no blend or editing semantics of its own: the ops are validated and
//! recorded through `focusforge_core::project::append_op_chain`, and a state
//! renders through the same function `export` uses.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use focusforge_core::project::{self, Op, Project};
use serde::Serialize;

use crate::history::{compact_after_save, resolve_checkpoint};

/// The `retouch --json` report.
#[derive(Serialize)]
struct RetouchReport {
    project: String,
    /// The state the first op was appended under.
    from: String,
    /// The new states' ids, in op order.
    checkpoints: Vec<String>,
    ops: usize,
    wall_seconds: f64,
}

/// Reads the op list: a bare JSON array of ops.
fn read_ops(path: &Path) -> Result<Vec<Op>, String> {
    let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|err| format!("{}: {err}", path.display()))
}

/// Runs the subcommand: nothing is written unless every op is valid.
pub fn run(
    project_path: &Path,
    ops_path: &Path,
    from: Option<&str>,
    name: Option<&str>,
    json: bool,
) -> ExitCode {
    let start = Instant::now();
    let ops = match read_ops(ops_path) {
        Ok(ops) => ops,
        Err(message) => return fail(message),
    };
    let ops_count = ops.len();

    let from_id = {
        let project = match Project::open(project_path) {
            Ok(project) => project,
            Err(err) => return fail(err),
        };
        match from {
            Some(prefix) => match resolve_checkpoint(project.history(), project_path, prefix) {
                Ok(checkpoint) => checkpoint.id.clone(),
                Err(message) => return fail(message),
            },
            None => project.checkpoint().id.clone(),
        }
    };

    let checkpoints = match project::append_op_chain(
        project_path,
        Some(&from_id),
        ops,
        name.map(str::to_string),
        project::now_unix(),
    ) {
        Ok(ids) => ids,
        Err(err) => return fail(err),
    };
    compact_after_save(project_path);

    let report = RetouchReport {
        project: project_path.display().to_string(),
        from: from_id,
        checkpoints,
        ops: ops_count,
        wall_seconds: start.elapsed().as_secs_f64(),
    };
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!("{}", render(&report));
    }
    ExitCode::SUCCESS
}

/// Whether writing to `target` would replace the file `existing` names.
///
/// Canonicalizes `target` itself to catch a shared symlink, and
/// canonicalizes its parent with the file name joined back on to catch a
/// `..` detour or a symlinked directory before `target` exists. When
/// `existing` cannot be canonicalized -- it does not exist yet, such as an
/// `--out` not yet written -- it is resolved the same parent-plus-name way
/// instead of being assumed collision-free.
pub(crate) fn would_replace(target: &Path, existing: &Path) -> bool {
    let Some(existing) = resolve(existing) else {
        return false;
    };
    if std::fs::canonicalize(target).is_ok_and(|target| target == existing) {
        return true;
    }
    resolve(target).is_some_and(|target| target == existing)
}

/// Canonicalizes `path` directly, falling back to its canonicalized parent
/// joined with its own file name when `path` itself does not exist yet.
fn resolve(path: &Path) -> Option<std::path::PathBuf> {
    if let Ok(path) = std::fs::canonicalize(path) {
        return Some(path);
    }
    let (parent, name) = (path.parent()?, path.file_name()?);
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    std::fs::canonicalize(parent)
        .ok()
        .map(|parent| parent.join(name))
}

/// Human rendering: one fact per line, unit-testable without capturing
/// stdout -- the shape `export::render`/`stack::render` use.
fn render(report: &RetouchReport) -> String {
    [
        report.project.clone(),
        format!("from: {}", report.from),
        format!("checkpoints: {}", report.checkpoints.join(", ")),
        format!("ops: {}", report.ops),
        format!("wall: {:.3}s", report.wall_seconds),
    ]
    .join("\n")
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    crate::fail(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_names_every_fact_one_per_line() {
        let text = render(&RetouchReport {
            project: "a.ffp".to_string(),
            from: "root".to_string(),
            checkpoints: vec!["c1".to_string(), "c2".to_string()],
            ops: 2,
            wall_seconds: 0.125,
        });
        for needle in [
            "a.ffp",
            "from: root",
            "checkpoints: c1, c2",
            "ops: 2",
            "wall: 0.125s",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }
}
