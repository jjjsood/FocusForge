// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli review`: one record per result found (or expected) under
//! a directory of projects, or a batch run's output directory.
//!
//! `review` is a **reader**, never a pipeline: it never groups, never
//! re-fuses, never writes a project, an image or any scratch state. The
//! sharpness ratio divides two numbers the run already stored in the
//! project's manifest; only the Z-Map is streamed, because no run stores a
//! spread number. A stack the run attempted and did not finish is
//! still a record -- outcome `failed`, its own error string -- because the
//! manifest lists it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use focusforge_core::project::{PROJECT_EXTENSION, Project, ReviewMetrics};
use serde::{Deserialize, Serialize};

use crate::groups;
use crate::status::StatusRecord;

/// One result's review record: identity, outcome and error through the same
/// type the `--status` stream uses, plus the two fields this command adds.
#[derive(Serialize, Deserialize, Debug)]
pub struct ReviewRecord {
    #[serde(flatten)]
    pub base: StatusRecord,
    /// Absent, never zeroed, when a metric could not be produced -- a zero
    /// sorts like a bad result and an absent value does not.
    /// `#[serde(default)]`: a plain `StatusRecord` line, which carries none
    /// of this struct's three added fields, must still decode as a
    /// `ReviewRecord`.
    #[serde(default)]
    pub metrics: Option<ReviewMetrics>,
    /// Why `metrics` is absent. `None` exactly when `metrics` is `Some`.
    #[serde(default)]
    pub metrics_reason: Option<String>,
    /// Every source this project cannot resolve; empty when all resolve.
    /// `#[serde(default)]` so a record without the field decodes as empty.
    #[serde(default)]
    pub unresolved_sources: Vec<String>,
}

/// One wrapped `--json` document, matching `stack --json`'s convention that
/// a document on stdout means a completed run.
#[derive(Serialize, Deserialize, Debug)]
pub struct ReviewDocument {
    pub results: Vec<ReviewRecord>,
}

/// Runs the subcommand. Writes nothing: every filesystem call below is a
/// read.
pub fn run(dir: &Path, json: bool) -> ExitCode {
    let (label, records) = match scan(dir) {
        Ok(ScanSource::Manifest { path, manifest }) => (
            path.display().to_string(),
            manifest
                .groups
                .iter()
                .map(|entry| record_from_manifest_entry(dir, entry))
                .collect(),
        ),
        Ok(ScanSource::Projects(paths)) => (
            dir.display().to_string(),
            paths.iter().map(|p| record_from_project_only(p)).collect(),
        ),
        Err(message) => return crate::fail(message),
    };

    let document = ReviewDocument { results: records };
    if json {
        match serde_json::to_string_pretty(&document) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!("{}", render(&label, &document));
    }
    ExitCode::SUCCESS
}

/// Where `review` found its entries: a manifest, preferred, or a bare
/// listing of `*.ffp` files when no manifest is present.
enum ScanSource {
    Manifest {
        path: PathBuf,
        manifest: groups::RunManifest,
    },
    Projects(Vec<PathBuf>),
}

/// Manifest-first: `<dir>/manifest.json` (the `-o` placement), else
/// `<dir>/<dir-basename>-run.json` (the beside-the-frames placement,
/// `crate::stack::manifest_path`'s own naming rule, mirrored rather than
/// re-derived), else a non-recursive `*.ffp` glob, sorted by file name so
/// output order is stable. Refuses, naming `stack`'s grouping flags, when
/// none of the three is present -- `review` never walks frames and never
/// groups.
fn scan(dir: &Path) -> Result<ScanSource, String> {
    let out_manifest = dir.join("manifest.json");
    if out_manifest.is_file() {
        return load_manifest(&out_manifest).map(|manifest| ScanSource::Manifest {
            path: out_manifest,
            manifest,
        });
    }
    let beside_manifest = crate::stack::manifest_path(None, dir);
    if beside_manifest.is_file() {
        return load_manifest(&beside_manifest).map(|manifest| ScanSource::Manifest {
            path: beside_manifest,
            manifest,
        });
    }

    let mut projects: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| format!("{}: {err}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == PROJECT_EXTENSION))
        .collect();
    projects.sort();
    if !projects.is_empty() {
        return Ok(ScanSource::Projects(projects));
    }

    Err(format!(
        "{}: no run manifest and no *.{PROJECT_EXTENSION} project found here. Run \
         `focusforge-cli stack {} --algorithm <id>` first -- add `--groups`/`--write-groups` \
         to control how frames are grouped into stacks. `review` reports on a run's \
         results; it never groups frames itself.",
        dir.display(),
        dir.display(),
    ))
}

fn load_manifest(path: &Path) -> Result<groups::RunManifest, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let manifest: groups::RunManifest =
        serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(manifest)
}

/// Resolves one manifest entry's stored output path. Absolute, it is used
/// as given. Relative, it is usually already a full path from the original
/// `stack` invocation's own cwd (`out_dir.join(name)`, where `out_dir` was
/// whatever `-o` string the user passed) -- when `review` runs from that
/// same cwd, the common case, it already resolves there with no help. Only
/// when it does **not** resolve as given is it joined onto the directory
/// `review` was pointed at.
fn resolve_output(reviewed_dir: &Path, output: &str) -> PathBuf {
    let path = Path::new(output);
    if path.is_absolute() || path.is_file() {
        return path.to_path_buf();
    }
    reviewed_dir.join(path)
}

/// Why `outcome_reason` for one of the three non-`completed` manifest
/// outcomes.
fn outcome_reason(outcome: &str) -> String {
    match outcome {
        "failed" => "the run failed before a project was written".to_string(),
        "dry-run" => "dry run: no project was written".to_string(),
        "cancelled" => "cancelled".to_string(),
        other => format!("outcome {other:?}: no project was written"),
    }
}

fn base_record(
    identity: String,
    outcome: &str,
    algorithm: Option<String>,
    outputs: Vec<String>,
    wall_seconds: f64,
    error: Option<String>,
) -> StatusRecord {
    StatusRecord {
        identity,
        command: "review".to_string(),
        outcome: outcome.to_string(),
        algorithm,
        outputs,
        wall_seconds,
        error,
    }
}

/// A record whose metrics could not be produced -- every non-`Some(metrics)`
/// outcome shares this shape, so the reason is the only thing each call site
/// has to state.
fn no_metrics_record(
    base: StatusRecord,
    reason: impl Into<String>,
    unresolved_sources: Vec<String>,
) -> ReviewRecord {
    ReviewRecord {
        base,
        metrics: None,
        metrics_reason: Some(reason.into()),
        unresolved_sources,
    }
}

/// One manifest entry's record. `outcome`, `error` and `wall_seconds` come
/// from the manifest entry itself -- a `failed` group is a record because
/// the manifest lists it.
fn record_from_manifest_entry(reviewed_dir: &Path, entry: &groups::ManifestEntry) -> ReviewRecord {
    let identity = entry.identity.clone();
    if entry.outcome != "completed" {
        return no_metrics_record(
            base_record(
                identity,
                &entry.outcome,
                None,
                Vec::new(),
                entry.wall_seconds,
                entry.error.clone(),
            ),
            outcome_reason(&entry.outcome),
            Vec::new(),
        );
    }

    let Some(first_output) = entry.outputs.first() else {
        return no_metrics_record(
            base_record(
                identity,
                "failed",
                None,
                Vec::new(),
                entry.wall_seconds,
                Some("completed in the manifest but lists no output".to_string()),
            ),
            "no project path recorded for this result",
            Vec::new(),
        );
    };
    let project_path = resolve_output(reviewed_dir, first_output);
    open_and_measure(identity, entry.wall_seconds, project_path)
}

/// A record built straight from a `*.ffp` file when no manifest exists to
/// carry an identity or a wall time -- `review`'s own elapsed time is what
/// `wall_seconds` reports here, since there is no other source for it.
fn record_from_project_only(path: &Path) -> ReviewRecord {
    let start = Instant::now();
    let identity = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut record = open_and_measure(identity, 0.0, path.to_path_buf());
    record.base.wall_seconds = start.elapsed().as_secs_f64();
    record
}

/// Opens `project_path`, reports [`Project::unresolved_sources`] and calls
/// [`focusforge_core::project::review::measure`]. Never a crash and never a
/// skip on any failure: the record keeps its identity and reports the
/// failure as data.
fn open_and_measure(identity: String, wall_seconds: f64, project_path: PathBuf) -> ReviewRecord {
    let display = project_path.display().to_string();
    let mut project = match Project::open(&project_path) {
        Ok(project) => project,
        Err(err) => {
            return no_metrics_record(
                base_record(
                    identity,
                    "failed",
                    None,
                    Vec::new(),
                    wall_seconds,
                    Some(err.to_string()),
                ),
                "the project could not be opened",
                Vec::new(),
            );
        }
    };

    let unresolved: Vec<String> = project
        .unresolved_sources()
        .map(|s| s.path.clone())
        .collect();
    let algorithm = Some(project.manifest().algorithm.clone());

    match focusforge_core::project::review::measure(&mut project) {
        Ok(metrics) if metrics.composite_lapvar.is_some() && metrics.max_input_lapvar.is_some() => {
            ReviewRecord {
                base: base_record(
                    identity,
                    "completed",
                    algorithm,
                    vec![display],
                    wall_seconds,
                    None,
                ),
                metrics: Some(metrics),
                metrics_reason: None,
                unresolved_sources: unresolved,
            }
        }
        // A `completed` result whose run stored no sharpness numbers reports
        // the same way a failure's absent metrics do -- never recomputed
        // from the composite blob.
        Ok(_) => no_metrics_record(
            base_record(
                identity,
                "completed",
                algorithm,
                vec![display],
                wall_seconds,
                None,
            ),
            "the run recorded no sharpness numbers",
            unresolved,
        ),
        // A corrupt Z-Map blob is a `ProjectError` from `read_plane_into`'s own
        // blake3 check, never a silently wrong number.
        Err(err) => no_metrics_record(
            base_record(
                identity,
                "failed",
                algorithm,
                Vec::new(),
                wall_seconds,
                Some(err.to_string()),
            ),
            "the project's Z-Map could not be measured",
            unresolved,
        ),
    }
}

/// Human rendering: one header line naming the scan source and the entry
/// count, then one row per record -- identity, outcome, sharpness, sources,
/// coherence, and the reason or error when metrics are absent. Stated
/// precision, not a formatter default: `{:.4}` for the ratio and the
/// coherence fraction, `{:.3}s` for wall; the absent case prints `--`,
/// never `0`. A `String`, not prints, so it is unit-testable without
/// capturing stdout.
fn render(source: &str, document: &ReviewDocument) -> String {
    let mut lines = vec![format!(
        "review {source}: {} result{}",
        document.results.len(),
        if document.results.len() == 1 { "" } else { "s" },
    )];
    for record in &document.results {
        let sharpness = record
            .metrics
            .and_then(|m| m.sharpness_ratio)
            .map(|v| format!("{v:.4}"))
            .unwrap_or_else(|| "--".to_string());
        let sources = record
            .metrics
            .map(|m| m.distinct_sources.to_string())
            .unwrap_or_else(|| "--".to_string());
        let coherence = record
            .metrics
            .map(|m| format!("{:.4}", m.coherence))
            .unwrap_or_else(|| "--".to_string());
        let note = record
            .metrics_reason
            .clone()
            .or_else(|| record.base.error.clone())
            .unwrap_or_default();
        lines.push(format!(
            "  {:<24} {:<9} sharpness={sharpness:<8} sources={sources:<3} coherence={coherence:<8} \
             wall={:.3}s{}",
            record.base.identity,
            record.base.outcome,
            record.base.wall_seconds,
            if note.is_empty() {
                String::new()
            } else {
                format!("  -- {note}")
            },
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> ReviewMetrics {
        ReviewMetrics {
            sharpness_ratio: Some(1.2169),
            composite_lapvar: Some(347_109.0),
            max_input_lapvar: Some(285_247.1),
            lapvar_window: None,
            distinct_sources: 8,
            coherence: 0.6376,
        }
    }

    #[test]
    fn render_names_the_source_the_count_and_every_field_or_a_dash() {
        let document = ReviewDocument {
            results: vec![
                ReviewRecord {
                    base: base_record(
                        "good".to_string(),
                        "completed",
                        Some("laplacian".to_string()),
                        vec!["good.ffp".to_string()],
                        1.5,
                        None,
                    ),
                    metrics: Some(metrics()),
                    metrics_reason: None,
                    unresolved_sources: Vec::new(),
                },
                ReviewRecord {
                    base: base_record(
                        "bad".to_string(),
                        "failed",
                        None,
                        Vec::new(),
                        0.2,
                        Some("frames disagree on size".to_string()),
                    ),
                    metrics: None,
                    metrics_reason: Some("the run failed before a project was written".to_string()),
                    unresolved_sources: Vec::new(),
                },
            ],
        };
        let text = render("manifest.json", &document);
        for needle in [
            "review manifest.json: 2 results",
            "good",
            "completed",
            "sharpness=1.2169",
            "sources=8",
            "coherence=0.6376",
            "bad",
            "failed",
            "sharpness=--",
            "sources=--",
            "coherence=--",
            "the run failed before a project was written",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(
            !text.contains("sharpness=0 ") && !text.contains("coherence=0.0000"),
            "an absent metric must never render as a zero:\n{text}"
        );
    }

    #[test]
    fn outcome_reason_names_each_non_completed_outcome() {
        assert_eq!(
            outcome_reason("failed"),
            "the run failed before a project was written"
        );
        assert_eq!(outcome_reason("dry-run"), "dry run: no project was written");
        assert_eq!(outcome_reason("cancelled"), "cancelled");
    }

    #[test]
    fn resolve_output_uses_an_absolute_path_as_given() {
        let resolved = resolve_output(Path::new("/reviewed"), "/absolute/thing.ffp");
        assert_eq!(resolved, PathBuf::from("/absolute/thing.ffp"));
    }

    /// Every `--status` consumer and the GUI can decode a plain
    /// `StatusRecord` line as a `ReviewRecord` and vice versa. This is a
    /// unit test, not an integration test, because this crate has no
    /// `[lib]` target: only code inside the crate can see both types.
    #[test]
    fn review_record_decodes_a_status_line() {
        let status = StatusRecord {
            identity: "fixtures/mantis-512".to_string(),
            command: "stack".to_string(),
            outcome: "completed".to_string(),
            algorithm: Some("laplacian".to_string()),
            outputs: vec!["mantis-512.ffp".to_string()],
            wall_seconds: 12.5,
            error: None,
        };
        let line = serde_json::to_string(&status).expect("StatusRecord must serialize");
        let decoded: ReviewRecord = serde_json::from_str(&line)
            .expect("a plain StatusRecord line must decode as a ReviewRecord");
        assert_eq!(decoded.base.identity, "fixtures/mantis-512");
        assert!(decoded.metrics.is_none());
        assert!(decoded.metrics_reason.is_none());
        assert!(decoded.unresolved_sources.is_empty());

        let record = ReviewRecord {
            base: base_record(
                "IMG_9801-9862".to_string(),
                "completed",
                Some("laplacian".to_string()),
                vec!["IMG_9801-9862.ffp".to_string()],
                3.2,
                None,
            ),
            metrics: Some(metrics()),
            metrics_reason: None,
            unresolved_sources: vec!["/elsewhere/00.tif".to_string()],
        };
        let text = serde_json::to_string(&record).expect("ReviewRecord must serialize");
        let back: StatusRecord =
            serde_json::from_str(&text).expect("a ReviewRecord must decode as a StatusRecord");
        assert_eq!(back.identity, "IMG_9801-9862");
        assert_eq!(back.outcome, "completed");
    }
}
