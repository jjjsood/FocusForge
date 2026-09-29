// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli export`: a project in, an image file out.
//!
//! This is the **only** thing in the workspace that writes a TIFF from a
//! stack result. `stack` prepares projects for review; turning one into an
//! image is a separate, explicit step.
//!
//! Two properties this module exists to keep:
//!
//! - **Byte-identical.** The composite is re-emitted from manifest data --
//!   the exact `ImageDescription` the run recorded, the same twelve-tag IFD
//!   `TiffWriter` always writes -- so an exported file equals the plane
//!   fusion staged, byte for byte.
//! - **A warning, never a silence.** A project whose sources cannot be
//!   resolved relatively names every one of them on stderr and still writes
//!   the image. `core` returns them as data; printing them is this
//!   module's.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use focusforge_core::algorithm::TileReader;
use focusforge_core::align::WarpingReader;
use focusforge_core::host;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter, open_frame_set};
use focusforge_core::project::{self, Plane, Project};
use focusforge_core::tile::HostBudget;
use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::Serialize;

/// Rows per streamed band.
const FRAME_BAND_ROWS: u32 = 256;

/// The frozen `export --json` schema, additive-only -- the same contract
/// `InfoReport` and `StackReport` carry.
#[derive(Serialize)]
pub struct ExportReport {
    pub project: String,
    /// Final paths only -- never the `.partial` names a run writes to first.
    pub wrote: Vec<String>,
    /// Every source the project could not record relative to its own
    /// directory. Present and empty is a positive statement, not an
    /// omission.
    pub unresolved_sources: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub frame_count: u32,
    pub algorithm: String,
    /// The source frame index written "as fused", if `--frame` was given.
    pub frame: Option<usize>,
    pub wall_seconds: f64,
}

/// Runs the subcommand.
///
/// At least one of `-o`/`--zmap`/`--frame` is required **by name**, checked
/// here so the error names the missing flag instead of clap's generic exit
/// 2. `--frame-output` without `--frame` is refused the same way; `--frame`
/// without `--frame-output` is already a clap usage error (`requires`).
pub fn run(
    project_path: &Path,
    output: Option<&Path>,
    zmap: Option<&Path>,
    frame: Option<usize>,
    frame_output: Option<&Path>,
    json: bool,
) -> ExitCode {
    let start = Instant::now();
    if output.is_none() && zmap.is_none() && frame_output.is_none() {
        return fail(
            "export needs somewhere to write: -o <file> for the composite, \
             --zmap <file> for the Z-Map, --frame <index> --frame-output <file> for one \
             source frame, or any combination",
        );
    }
    if frame_output.is_some() && frame.is_none() {
        return fail("--frame-output requires --frame <index>");
    }

    let mut project = match Project::open(project_path) {
        Ok(project) => project,
        Err(err) => return fail(err),
    };

    // Reported before anything is written: a user who is about to discover
    // their sources moved should learn it at the top of the output, not
    // after a minute of writing.
    let unresolved: Vec<String> = project
        .unresolved_sources()
        .map(|s| s.path.clone())
        .collect();
    if !unresolved.is_empty() {
        let warning = format!(
            "warning: {} source frame(s) unresolved -- not reachable from this project, either \
             because the path was recorded as given or because it is no longer there. The \
             export below is unaffected; re-fusing or retouching would not be:",
            unresolved.len()
        );
        eprintln!("{warning}");
        for path in &unresolved {
            eprintln!("  {path}");
        }
        tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "{warning}");
    }

    let source_count = project.manifest().sources.len();
    if let Some(index) = frame
        && index >= source_count
    {
        return fail(format!(
            "frame {index} is out of range: this project has frames 0-{}",
            source_count.saturating_sub(1)
        ));
    }

    let composite_ref = project.plane(Plane::Composite).clone();
    let zmap_ref = project.plane(Plane::ZMap).clone();
    let algorithm = project.manifest().algorithm.clone();

    let mut wrote = Vec::new();
    if let Some(path) = output {
        if let Err(err) = project::materialize_plane(&mut project, Plane::Composite, path) {
            return fail(err);
        }
        wrote.push(path.display().to_string());
    }
    if let Some(path) = zmap {
        if let Err(err) = project::materialize_plane(&mut project, Plane::ZMap, path) {
            return fail(err);
        }
        wrote.push(path.display().to_string());
    }
    if let (Some(index), Some(path)) = (frame, frame_output) {
        if let Err(err) = export_frame(&project, index, path) {
            return fail(err);
        }
        wrote.push(path.display().to_string());
    }

    let report = ExportReport {
        project: project_path.display().to_string(),
        wrote,
        unresolved_sources: unresolved,
        width: composite_ref.width,
        height: composite_ref.height,
        samples_per_pixel: composite_ref.samples_per_pixel,
        frame_count: zmap_ref.frame_count,
        algorithm,
        frame,
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

/// Human rendering: one fact per line, unit-testable without capturing
/// stdout -- the shape `stack::render` uses.
fn render(report: &ExportReport) -> String {
    let mut lines = vec![
        report.project.clone(),
        format!(
            "{}x{} {}ch, {} frames",
            report.width, report.height, report.samples_per_pixel, report.frame_count
        ),
        format!("algorithm: {}", report.algorithm),
    ];
    if let Some(index) = report.frame {
        lines.push(format!("frame: {index}"));
    }
    if !report.unresolved_sources.is_empty() {
        lines.push(format!(
            "unresolved sources: {}",
            report.unresolved_sources.len()
        ));
    }
    for path in &report.wrote {
        lines.push(format!("wrote: {path}"));
    }
    lines.push(format!("wall: {:.3}s", report.wall_seconds));
    lines.join("\n")
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    tracing::error!(target: MIRRORED_OUTPUT_TARGET, "error: {message}");
    ExitCode::FAILURE
}

/// Writes source frame `index` "as fused" to `path`: warped by its recorded
/// transform and offset by the applied crop, at the composite's own width
/// and height. `index` must already be checked in range.
fn export_frame(project: &Project, index: usize, path: &Path) -> Result<(), String> {
    let composite = project.plane(Plane::Composite).clone();
    let registration = project.manifest().registration.clone();
    let paths: Vec<PathBuf> = project
        .manifest()
        .sources
        .iter()
        .map(|source| project.source_path(source))
        .collect();

    let budget = HostBudget::host_derived(&host::snapshot());
    let frame_set = open_frame_set(&paths, budget).map_err(|err| err.to_string())?;
    let frame_dims = frame_set.reader.dimensions();
    let transforms = registration.transforms(frame_set.reader.len());
    let reader = WarpingReader::new(
        frame_set.reader.as_ref(),
        frame_dims,
        composite.samples_per_pixel,
        transforms,
    );
    let origin = registration.origin();

    let partial = project::partial_path(path);
    let result = (|| {
        let mut writer = TiffWriter::create(
            &partial,
            TiffSpec {
                width: composite.width,
                height: composite.height,
                samples_per_pixel: composite.samples_per_pixel,
                description: Some(composite.description.clone()),
            },
        )
        .map_err(|err| err.to_string())?;

        let spp = composite.samples_per_pixel;
        let mut buf =
            vec![0u16; composite.width as usize * FRAME_BAND_ROWS as usize * spp as usize];
        let mut y = 0u32;
        while y < composite.height {
            let rows = FRAME_BAND_ROWS.min(composite.height - y);
            let rect = Rect {
                x: 0,
                y,
                width: composite.width,
                height: rows,
            };
            let source_rect = Rect {
                x: rect.x + origin.0,
                y: rect.y + origin.1,
                width: rect.width,
                height: rect.height,
            };
            let len = (rect.width * rows * spp) as usize;
            reader
                .read_tile_into(index, source_rect, &mut buf[..len])
                .map_err(|err| err.to_string())?;
            writer
                .write_tile(rect, &buf[..len])
                .map_err(|err| err.to_string())?;
            y += rows;
        }
        writer.finish().map_err(|err| err.to_string())
    })();

    match result.and_then(|()| {
        std::fs::rename(&partial, path).map_err(|err| format!("{}: {err}", path.display()))
    }) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&partial);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_names_every_fact_one_per_line() {
        let text = render(&ExportReport {
            project: "demo.ffp".to_string(),
            wrote: vec!["out.tif".to_string(), "out-zmap.tif".to_string()],
            unresolved_sources: vec!["/elsewhere/00.tif".to_string()],
            width: 512,
            height: 512,
            samples_per_pixel: 3,
            frame_count: 8,
            algorithm: "example-stack".to_string(),
            frame: None,
            wall_seconds: 0.25,
        });
        for needle in [
            "demo.ffp",
            "512x512",
            "3ch",
            "8 frames",
            "algorithm: example-stack",
            "unresolved sources: 1",
            "wrote: out.tif",
            "wrote: out-zmap.tif",
            "wall: 0.250s",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }
}
