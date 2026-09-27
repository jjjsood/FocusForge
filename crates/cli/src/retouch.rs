// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli retouch`: a Z-Map and an op list in, an edited Z-Map out.
//!
//! Holds no index arithmetic or editing semantics of its own: every op is
//! applied through `focusforge_core::retouch`.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use focusforge_core::io::{TiffSpec, TiffStack, TiffWriter, frame_paths};
use focusforge_core::project::{self, Op};
use focusforge_core::retouch::{self, EditSession};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};
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
    /// The `--render` output path, when one was written.
    pub rendered: Option<String>,
}

/// Runs the subcommand: writes nothing until every op has been read,
/// validated and applied to the resident session.
#[allow(clippy::too_many_arguments)]
pub fn run(
    zmap: &Path,
    ops_path: &Path,
    out: &Path,
    undo: usize,
    redo: usize,
    frames: Option<&Path>,
    render_out: Option<&Path>,
    json: bool,
) -> ExitCode {
    let start = Instant::now();

    if would_replace(out, zmap) {
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

    if let Some(render_path) = render_out {
        let frames_dir =
            frames.expect("clap `requires = \"frames\"` on --render guarantees --frames is set");
        if let Err(exit) = validate_render_args(zmap, out, frames_dir, render_path, info) {
            return exit;
        }
    }

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

    let rendered = match render_out {
        Some(render_path) => {
            let frames_dir = frames
                .expect("clap `requires = \"frames\"` on --render guarantees --frames is set");
            match write_render(out, frames_dir, render_path) {
                Ok(path) => Some(path),
                Err(exit) => return exit,
            }
        }
        None => None,
    };

    let report = RetouchReport {
        zmap: zmap.display().to_string(),
        ops: ops_path.display().to_string(),
        ops_in_list: ops.len(),
        ops_applied: session.op_count(),
        pixels_changed,
        wrote: out.display().to_string(),
        wall_seconds: start.elapsed().as_secs_f64(),
        rendered,
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
fn would_replace(target: &Path, existing: &Path) -> bool {
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

/// Checks `--render`'s inputs before anything is written: `render` must not
/// collide with `zmap`, `out` or any source frame, and `--frames` must hold
/// the frame count and dimensions `info` (the not-yet-edited map, which
/// editing never resizes) claims.
fn validate_render_args(
    zmap: &Path,
    out: &Path,
    frames_dir: &Path,
    render_path: &Path,
    info: ZMapInfo,
) -> Result<(), ExitCode> {
    if would_replace(render_path, zmap) {
        return Err(fail(format!(
            "--render {} is also --zmap: refusing to overwrite the input",
            render_path.display()
        )));
    }
    if would_replace(render_path, out) {
        return Err(fail(format!(
            "--render {} is also --out: refusing to overwrite the edited map",
            render_path.display()
        )));
    }

    let paths = match frame_paths(frames_dir) {
        Ok(paths) => paths,
        Err(err) => return Err(fail(err)),
    };
    for frame in &paths {
        if would_replace(render_path, frame) {
            return Err(fail(format!(
                "--render {} is also a source frame: refusing to overwrite it",
                render_path.display()
            )));
        }
    }
    let stack = match TiffStack::open_files(&paths) {
        Ok(stack) => stack,
        Err(err) => return Err(fail(err)),
    };
    if stack.len() as u32 != info.frame_count() {
        return Err(fail(format!(
            "--frames holds {} frames but the map claims {}",
            stack.len(),
            info.frame_count()
        )));
    }
    let (frame_width, frame_height) = stack.dimensions();
    if frame_width != info.width() || frame_height != info.height() {
        return Err(fail(format!(
            "--frames holds {frame_width}x{frame_height} frames but the map is {}x{}",
            info.width(),
            info.height()
        )));
    }
    Ok(())
}

/// Recomposites the just-written `out` map against `frames_dir`, writing
/// the 16-bit RGB TIFF `render` names. Collisions and frame-count/dimension
/// mismatches are already refused by [`validate_render_args`] before `out`
/// is written; this reopens both fresh and streams the render.
fn write_render(out: &Path, frames_dir: &Path, render_path: &Path) -> Result<String, ExitCode> {
    let paths = match frame_paths(frames_dir) {
        Ok(paths) => paths,
        Err(err) => return Err(fail(err)),
    };
    let stack = match TiffStack::open_files(&paths) {
        Ok(stack) => stack,
        Err(err) => return Err(fail(err)),
    };
    let reader = match ZMapReader::open(out) {
        Ok(reader) => reader,
        Err(err) => return Err(fail(err)),
    };
    let info = reader.info();

    let spp = stack.header(0).samples_per_pixel;
    let sources = retouch::Sources {
        reader: &stack,
        frame_count: stack.len() as u32,
        samples_per_pixel: spp,
        origin: (0, 0),
    };

    let width = info.width();
    let height = info.height();
    let partial = project::partial_path(render_path);
    let spec = TiffSpec {
        width,
        height,
        samples_per_pixel: spp,
        description: None,
    };
    let mut writer = match TiffWriter::create(&partial, spec) {
        Ok(writer) => writer,
        Err(err) => return Err(fail(err)),
    };

    let mut indices = vec![0u16; width as usize * ROWS_PER_PASS as usize];
    let mut samples = vec![0u16; width as usize * ROWS_PER_PASS as usize * spp as usize];
    let mut y = 0u32;
    while y < height {
        let band_height = ROWS_PER_PASS.min(height - y);
        let band = Rect {
            x: 0,
            y,
            width,
            height: band_height,
        };
        let len = (width * band_height) as usize;
        if let Err(err) = reader.read_tile_into(band, &mut indices[..len]) {
            let _ = std::fs::remove_file(&partial);
            return Err(fail(err));
        }
        if let Err(err) = retouch::recomposite(
            &indices[..len],
            band,
            &sources,
            &mut samples[..len * spp as usize],
        ) {
            let _ = std::fs::remove_file(&partial);
            return Err(fail(err));
        }
        if let Err(err) = writer.write_tile(band, &samples[..len * spp as usize]) {
            let _ = std::fs::remove_file(&partial);
            return Err(fail(err));
        }
        y += band_height;
    }
    if let Err(err) = writer.finish() {
        let _ = std::fs::remove_file(&partial);
        return Err(fail(err));
    }
    if let Err(err) = std::fs::rename(&partial, render_path) {
        let _ = std::fs::remove_file(&partial);
        return Err(fail(format!("{}: {err}", render_path.display())));
    }
    Ok(render_path.display().to_string())
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
    let mut lines = vec![
        format!("zmap: {}", report.zmap),
        format!(
            "ops: {} ({} in list, {} applied)",
            report.ops, report.ops_in_list, report.ops_applied
        ),
        format!("pixels changed: {}", report.pixels_changed),
        format!("wrote: {}", report.wrote),
        format!("wall: {:.3}s", report.wall_seconds),
    ];
    if let Some(rendered) = &report.rendered {
        lines.push(format!("rendered: {rendered}"));
    }
    lines.join("\n")
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
            rendered: None,
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
