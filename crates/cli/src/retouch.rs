// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli retouch`: a Z-Map and an op list in, an edited Z-Map out.
//!
//! Holds no index arithmetic or editing semantics of its own: every op is
//! applied through `focusforge_core::retouch`.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use focusforge_core::project::{self, Op};
use focusforge_core::retouch::{self, EditSession};
use focusforge_core::zmap::{ZMapReader, ZMapWriter};
use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::Serialize;

/// Rows per band [`focusforge_core::retouch::write_edited`] streams at a
/// time -- small enough that a run's peak memory never scales with the
/// map's height.
const ROWS_PER_PASS: u32 = 256;

/// The frozen `retouch --json` schema, additive-only -- the same contract
/// `ExportReport` and `StackReport` carry.
#[derive(Serialize)]
pub struct RetouchReport {
    pub zmap: String,
    pub ops: String,
    pub ops_in_list: usize,
    pub ops_applied: usize,
    pub pixels_changed: u64,
    pub wrote: String,
    pub wall_seconds: f64,
}

/// Runs the subcommand: writes nothing until every op has been read,
/// validated and applied to the resident session.
pub fn run(
    zmap: &Path,
    ops_path: &Path,
    out: &Path,
    undo: usize,
    redo: usize,
    json: bool,
) -> ExitCode {
    let start = Instant::now();

    if out_collides_with_zmap(out, zmap) {
        return fail(format!(
            "--out {} is also --zmap: refusing to overwrite the input",
            out.display()
        ));
    }

    let ops: Vec<Op> = {
        let file = match std::fs::File::open(ops_path) {
            Ok(file) => file,
            Err(err) => return fail(format!("{}: {err}", ops_path.display())),
        };
        match serde_json::from_reader(std::io::BufReader::new(file)) {
            Ok(ops) => ops,
            Err(err) => return fail(format!("{}: {err}", ops_path.display())),
        }
    };

    if undo > ops.len() {
        return fail(format!(
            "--undo {undo} is greater than the op list's length ({})",
            ops.len()
        ));
    }
    if redo > undo {
        return fail(format!("--redo {redo} is greater than --undo ({undo})"));
    }

    let reader = match ZMapReader::open(zmap) {
        Ok(reader) => reader,
        Err(err) => return fail(err),
    };
    let info = reader.info();

    let tile = match retouch::session_tile(&ops, info) {
        Ok(tile) => tile,
        Err((index, err)) => return fail(op_error(index, &ops[index], err)),
    };

    let mut indices = vec![0u16; info.tile_len(tile)];
    if let Err(err) = reader.read_tile_into(tile, &mut indices) {
        return fail(err);
    }

    let mut session = match EditSession::new(info, tile, indices) {
        Ok(session) => session,
        Err(err) => return fail(err),
    };
    for (index, op) in ops.iter().enumerate() {
        if let Err(err) = session.append(op.clone()) {
            return fail(op_error(index, op, err));
        }
    }
    for _ in 0..undo {
        session.undo();
    }
    for _ in 0..redo {
        session.redo();
    }

    let partial = project::partial_path(out);
    let mut writer = match ZMapWriter::create(&partial, info) {
        Ok(writer) => writer,
        Err(err) => return fail(err),
    };
    let pixels_changed = match retouch::write_edited(&reader, &session, ROWS_PER_PASS, &mut writer)
    {
        Ok(changed) => changed,
        Err(err) => {
            let _ = std::fs::remove_file(&partial);
            return fail(err);
        }
    };
    if let Err(err) = writer.finish() {
        let _ = std::fs::remove_file(&partial);
        return fail(err);
    }
    if let Err(err) = std::fs::rename(&partial, out) {
        let _ = std::fs::remove_file(&partial);
        return fail(format!("{}: {err}", out.display()));
    }

    let report = RetouchReport {
        zmap: zmap.display().to_string(),
        ops: ops_path.display().to_string(),
        ops_in_list: ops.len(),
        ops_applied: session.op_count(),
        pixels_changed,
        wrote: out.display().to_string(),
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

/// Whether renaming onto `out` would replace the file `zmap` resolves to.
///
/// Canonicalizes `out` itself to catch a shared symlink, and canonicalizes
/// its parent with the file name joined back on to catch a `..` detour or
/// a symlinked directory before `out` exists. A missing parent cannot
/// collide and fails later, at creation.
fn out_collides_with_zmap(out: &Path, zmap: &Path) -> bool {
    let Ok(zmap) = std::fs::canonicalize(zmap) else {
        return false;
    };
    if std::fs::canonicalize(out).is_ok_and(|out| out == zmap) {
        return true;
    }
    let (Some(parent), Some(name)) = (out.parent(), out.file_name()) else {
        return false;
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    std::fs::canonicalize(parent).is_ok_and(|parent| parent.join(name) == zmap)
}

/// One op's refusal as `"op #i: <message>"`, except
/// [`retouch::RetouchError::Unsupported`], which names the op's own JSON
/// tag instead of the generic message that variant carries.
fn op_error(index: usize, op: &Op, err: retouch::RetouchError) -> String {
    if matches!(err, retouch::RetouchError::Unsupported) {
        let tag = serde_json::to_value(op)
            .ok()
            .and_then(|value| value.as_object().and_then(|map| map.keys().next().cloned()))
            .unwrap_or_else(|| "op".to_string());
        format!("op #{index}: {tag} is not supported")
    } else {
        format!("op #{index}: {err}")
    }
}

/// Human rendering: one fact per line, unit-testable without capturing
/// stdout -- the shape `export::render`/`stack::render` use.
fn render(report: &RetouchReport) -> String {
    [
        format!("zmap: {}", report.zmap),
        format!(
            "ops: {} ({} in list, {} applied)",
            report.ops, report.ops_in_list, report.ops_applied
        ),
        format!("pixels changed: {}", report.pixels_changed),
        format!("wrote: {}", report.wrote),
        format!("wall: {:.3}s", report.wall_seconds),
    ]
    .join("\n")
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    tracing::error!(target: MIRRORED_OUTPUT_TARGET, "error: {message}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_names_every_fact_one_per_line() {
        let text = render(&RetouchReport {
            zmap: "base.tif".to_string(),
            ops: "ops.json".to_string(),
            ops_in_list: 3,
            ops_applied: 2,
            pixels_changed: 42,
            wrote: "out.tif".to_string(),
            wall_seconds: 0.125,
        });
        for needle in [
            "zmap: base.tif",
            "ops: ops.json (3 in list, 2 applied)",
            "pixels changed: 42",
            "wrote: out.tif",
            "wall: 0.125s",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }
}
