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
//!   fusion staged, byte for byte. That is what makes "the change in where
//!   they live costs nothing in what they are" a checkable claim rather
//!   than a hope.
//! - **A warning, never a silence.** A project whose sources cannot be
//!   resolved relatively names every one of them on stderr and still writes
//!   the image. `core` returns them as data; printing them is this
//!   module's.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use focusforge_core::io::{TiffSpec, TiffWriter};
use focusforge_core::project::{Plane, PlaneRef, Project, ProjectError, partial_path};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use serde::Serialize;

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
    pub wall_seconds: f64,
}

/// Runs the subcommand.
///
/// At least one of `-o`/`--zmap` is required **by name**, here rather than
/// by clap -- the same posture `--algo` uses on `stack`: a usage error from
/// clap exits 2 knowing nothing about what the command is for.
pub fn run(
    project_path: &Path,
    output: Option<&Path>,
    zmap: Option<&Path>,
    json: bool,
) -> ExitCode {
    let start = Instant::now();
    if output.is_none() && zmap.is_none() {
        return fail(
            "export needs somewhere to write: -o <file> for the composite, \
             --zmap <file> for the Z-Map, or both",
        );
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
        eprintln!(
            "warning: {} source frame(s) unresolved -- not reachable from this project, either \
             because the path was recorded as given or because it is no longer there. The \
             export below is unaffected; re-fusing or retouching would not be:",
            unresolved.len()
        );
        for path in &unresolved {
            eprintln!("  {path}");
        }
    }

    let composite_ref = project.plane(Plane::Composite).clone();
    let zmap_ref = project.plane(Plane::ZMap).clone();
    let algorithm = project.manifest().algorithm.clone();

    let mut wrote = Vec::new();
    if let Some(path) = output {
        if let Err(msg) = write_composite(&mut project, &composite_ref, path) {
            return fail(msg);
        }
        wrote.push(path.display().to_string());
    }
    if let Some(path) = zmap {
        if let Err(msg) = write_zmap(&mut project, &zmap_ref, path) {
            return fail(msg);
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

fn write_composite(project: &mut Project, plane: &PlaneRef, path: &Path) -> Result<(), String> {
    let partial = partial_path(path);
    let writer = TiffWriter::create(
        &partial,
        TiffSpec {
            width: plane.width,
            height: plane.height,
            samples_per_pixel: plane.samples_per_pixel,
            description: Some(plane.description.clone()),
        },
    )
    .map_err(|err| err.to_string())?;
    stream(
        project,
        Plane::Composite,
        &partial,
        path,
        writer,
        |w, rect, band| w.write_tile(rect, band).map_err(|err| err.to_string()),
    )
}

fn write_zmap(project: &mut Project, plane: &PlaneRef, path: &Path) -> Result<(), String> {
    let partial = partial_path(path);
    let info = ZMapInfo::new(plane.width, plane.height, plane.frame_count)
        .map_err(|err| err.to_string())?;
    let writer = ZMapWriter::create(&partial, info).map_err(|err| err.to_string())?;
    stream(
        project,
        Plane::ZMap,
        &partial,
        path,
        writer,
        |w, rect, band| w.write_tile(rect, band).map_err(|err| err.to_string()),
    )
}

/// The one band loop both payloads share: stream the blob into `writer`,
/// finish it, and rename the partial onto the named path -- **only** once
/// [`Project::read_plane_into`] has verified the blob's hash, which it does
/// at the end of the stream. A tampered blob therefore leaves nothing
/// behind rather than a corrupt image.
fn stream<W: Finishable>(
    project: &mut Project,
    which: Plane,
    partial: &Path,
    final_path: &Path,
    mut writer: W,
    mut write: impl FnMut(&mut W, focusforge_core::io::Rect, &[u16]) -> Result<(), String>,
) -> Result<(), String> {
    let mut failure: Option<String> = None;
    let read = project.read_plane_into(which, &mut |band, rect| {
        if failure.is_some() {
            return;
        }
        if let Err(err) = write(&mut writer, rect, band) {
            failure = Some(err);
        }
    });
    let outcome = read
        .map_err(|err: ProjectError| err.to_string())
        .and_then(|()| match failure {
            Some(err) => Err(err),
            None => writer.finish(),
        })
        .and_then(|()| {
            std::fs::rename(partial, final_path).map_err(|err| {
                format!(
                    "renaming {} to {}: {err}",
                    partial.display(),
                    final_path.display()
                )
            })
        });
    if outcome.is_err() {
        let _ = std::fs::remove_file(partial);
    }
    outcome
}

/// The `finish()` half of the two writers, so [`stream`] has one shape
/// rather than two near-copies.
trait Finishable: Sized {
    fn finish(self) -> Result<(), String>;
}

impl Finishable for TiffWriter<std::fs::File> {
    fn finish(self) -> Result<(), String> {
        TiffWriter::finish(self).map_err(|err| err.to_string())
    }
}

impl Finishable for ZMapWriter<std::fs::File> {
    fn finish(self) -> Result<(), String> {
        ZMapWriter::finish(self).map_err(|err| err.to_string())
    }
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
    ExitCode::FAILURE
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
