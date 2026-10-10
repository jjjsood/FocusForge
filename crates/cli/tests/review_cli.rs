// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli review`'s whole surface: one
//! record per manifest entry, the record's fields, the failed record, the
//! completed-but-unmeasured record, the unopenable project, the unresolved
//! sources, the wrapped `--json` document, the table/JSON agreement, the
//! write-nothing assertion and the refusal to group.
//!
//! Two costs, paid once each: [`shared_laplacian_run`] is the **one** real
//! `laplacian` fusion over `fixtures/mantis-512` this file pays for
//! (~25 s), reused by every test that needs a genuinely measured sharpness
//! ratio and Z-Map spread to compare against. Every other test either drives
//! `dummy-stack` over tiny synthetic frames (milliseconds) or seals a
//! project directly through `focusforge_core::project::seal` with
//! hand-picked `Metrics`, which is cheaper still and gives exact control
//! over the numbers a test needs to assert on.
//!
//! Every test isolates `.scratch/` with its own per-process temp directory
//! and `.current_dir(dir)`, since parallel tests must never share a
//! directory. `review` writes no scratch state, so most tests share the
//! project `seal_project` built; only `review_writes_nothing` needs its own.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};

use common::{CliOutput, mantis_512_dir};
use focusforge_core::io::{TiffSpec, TiffWriter, test_frames::write_frame_with_capture_time};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{self, Metrics, PROJECT_EXTENSION, SealRequest};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

/// A private copy of the built CLI binary, frozen at the moment it is taken.
///
/// `CARGO_BIN_EXE_focusforge-cli` resolves to one fixed path in the shared
/// `target/`; a test shelling out to it twice, minutes apart, can see it
/// rebuilt from under it by a concurrent `cargo build` -- observed as
/// `review` failing "unrecognized subcommand" mid-run with no local edit.
fn snapshot_bin(dir: &Path) -> PathBuf {
    let dst = dir.join("focusforge-cli-snapshot");
    std::fs::copy(env!("CARGO_BIN_EXE_focusforge-cli"), &dst)
        .expect("must snapshot the CLI binary");
    dst
}

/// `dir` is already a fresh, per-test working directory (see the module
/// doc comment), so its own `ff-data` subdirectory is a fresh data
/// directory too -- never the tester's real one.
fn run_bin(dir: &Path, path: &Path, args: &[&str]) -> CliOutput {
    common::run_in(
        Command::new(path).env("FOCUSFORGE_DATA_DIR", dir.join("ff-data")),
        dir,
        args,
    )
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    common::run_in(
        common::cli_bin("review_cli").env("FOCUSFORGE_DATA_DIR", dir.join("ff-data")),
        dir,
        args,
    )
}

/// The one `laplacian` run this file pays for: `fixtures/mantis-512`, no
/// grouping split (one project, so a single-group manifest), reused by
/// every test that needs a genuinely measured sharpness ratio and Z-Map
/// spread.
struct SharedRun {
    /// The run's own working directory, held alive for the process's
    /// lifetime: every test that reads [`SharedRun::out`] does so long after
    /// this struct's builder closure returns, so the directory must outlive
    /// the `OnceLock` cell, not just that closure. The `OnceLock` static is
    /// never dropped, so `common::TmpDir::drop`'s `remove_dir_all` never
    /// runs for this one.
    cwd: common::TmpDir,
    /// The `-o` directory, also `review`'s target.
    out: PathBuf,
}

fn shared_laplacian_run() -> &'static SharedRun {
    static ONCE: OnceLock<SharedRun> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cwd = common::tmp_dir("review_cli_shared_laplacian");
        let out = cwd.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let mut args = vec![
            "stack".to_string(),
            mantis_512_dir().to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_cli_in(&cwd, &args);
        assert!(
            result.success,
            "shared laplacian run must succeed -- stderr: {}",
            result.stderr
        );
        SharedRun { cwd, out }
    })
}

/// The shared run's own `review --json` document, computed once and reused
/// by every test that needs a real, independently measured sharpness ratio
/// to compare a hand-built project against.
fn shared_review_document() -> &'static serde_json::Value {
    static ONCE: OnceLock<serde_json::Value> = OnceLock::new();
    ONCE.get_or_init(|| {
        let run = shared_laplacian_run();
        let result = run_cli_in(&run.cwd, &["review", run.out.to_str().unwrap(), "--json"]);
        assert!(result.success, "stderr: {}", result.stderr);
        serde_json::from_str(&result.stdout).expect("review --json must parse")
    })
}

// --- dummy-stack multi-group helpers, milliseconds each ---------------------

fn frame(dir: &Path, name: &str, when: Option<&str>) {
    std::fs::create_dir_all(dir).unwrap();
    write_frame_with_capture_time(&dir.join(name), 8, 8, when).expect("test frame must write");
}

/// Three subjects, twenty minutes apart -- three groups on the capture-time
/// signal alone.
fn three_group_dump(name: &str) -> common::TmpDir {
    let dir = common::tmp_dir(name);
    for (file, when) in [
        ("IMG_9801.tif", "2026:08:29 18:42:41"),
        ("IMG_9802.tif", "2026:08:29 18:42:42"),
        ("IMG_9910.tif", "2026:08:29 19:02:55"),
        ("IMG_9911.tif", "2026:08:29 19:02:56"),
        ("IMG_9950.tif", "2026:08:29 19:22:10"),
        ("IMG_9951.tif", "2026:08:29 19:22:11"),
    ] {
        frame(&dir, file, Some(when));
    }
    dir
}

fn dummy_stack_args<'a>(dir: &'a str, out: &'a str) -> Vec<&'a str> {
    vec![
        "stack",
        dir,
        "-o",
        out,
        "--algorithm",
        "dummy-stack",
        "--no-align",
        "--no-crop",
    ]
}

// --- direct project sealing, for exact control over Metrics/Z-Map ----------

const W: u32 = 32;
const H: u32 = 24;
const SPP: u32 = 3;

fn full(width: u32, height: u32) -> focusforge_core::io::Rect {
    focusforge_core::io::Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Stages a composite and a Z-Map into `dir`, the Z-Map filled entirely with
/// `source_index` when `single_source` is `true` (a degenerate, one-source
/// map) or a round-robin over `frame_count` sources otherwise.
fn stage_planes(dir: &Path, frame_count: u32, single_source: Option<u16>) -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
    let mut writer = TiffWriter::create(
        &composite,
        TiffSpec {
            width: W,
            height: H,
            samples_per_pixel: SPP,
            description: Some("test composite".to_string()),
        },
    )
    .unwrap();
    let samples: Vec<u16> = (0..(W as usize * H as usize * SPP as usize))
        .map(|i| (i % 65_536) as u16)
        .collect();
    writer.write_tile(full(W, H), &samples).unwrap();
    writer.finish().unwrap();

    let mut map =
        ZMapWriter::create(&zmap, ZMapInfo::new(W, H, frame_count.max(1)).unwrap()).unwrap();
    let zmap_samples: Vec<u16> = (0..(W as usize * H as usize))
        .map(|i| single_source.unwrap_or((i as u32 % frame_count.max(1)) as u16))
        .collect();
    map.write_tile(full(W, H), &zmap_samples).unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

fn write_sources(dir: &Path, count: usize) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    (0..count)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut w = TiffWriter::create(
                &path,
                TiffSpec {
                    width: W,
                    height: H,
                    samples_per_pixel: SPP,
                    description: None,
                },
            )
            .unwrap();
            let samples: Vec<u16> = (0..(W as usize * H as usize * SPP as usize))
                .map(|j| ((j + i * 13) % 65_536) as u16)
                .collect();
            w.write_tile(full(W, H), &samples).unwrap();
            w.finish().unwrap();
            path
        })
        .collect()
}

/// Seals one project directly (no `stack` run at all), with the caller's own
/// `Metrics` and Z-Map -- the cheap, exact-control alternative to a real
/// fusion run for every test that only needs `review` to report numbers
/// faithfully, not to reproduce a specific algorithm's output.
fn seal_project(
    dir_name: &str,
    metrics: Metrics,
    frame_count: u32,
    single_source: Option<u16>,
) -> (common::TmpDir, PathBuf) {
    let dir = common::tmp_dir(dir_name);
    let staging = dir.join("staging");
    let (composite, zmap) = stage_planes(&staging, frame_count, single_source);
    let sources = write_sources(&dir, frame_count.max(1) as usize);
    let values = ParamValues::new();
    let project_path = dir.join(format!("demo.{PROJECT_EXTENSION}"));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &values,
        align: None,
        registration: focusforge_core::align::Registration::default(),
        composite_description: "test composite".to_string(),
        label: None,
        metrics,
    })
    .expect("seal must succeed");
    (dir, project_path)
}

/// Writes a hand-built `manifest.json` in the shape `groups::RunManifest`
/// reads: `root`, one `groups` entry per
/// `(name, identity, outcome, outputs, error)`.
fn write_manifest(out_dir: &Path, root: &str, entries: &[serde_json::Value]) {
    let manifest = serde_json::json!({
        "root": root,
        "groups": entries,
    });
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn manifest_entry(
    name: &str,
    outcome: &str,
    outputs: Vec<String>,
    wall_seconds: f64,
    error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "identity": name,
        "frame_count": 1,
        "outcome": outcome,
        "outputs": outputs,
        "wall_seconds": wall_seconds,
        "error": error,
    })
}

#[test]
fn manifest_entry_without_identity_refuses_the_whole_manifest() {
    let (dir, project_path) =
        seal_project("review_cli_missing_identity", Metrics::default(), 1, None);
    let valid = manifest_entry(
        "valid",
        "completed",
        vec![project_path.display().to_string()],
        0.1,
        None,
    );
    let mut missing = manifest_entry("missing", "completed", Vec::new(), 0.1, None);
    missing.as_object_mut().unwrap().remove("identity");
    write_manifest(&dir, dir.to_str().unwrap(), &[valid, missing]);

    let review = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    assert!(!review.success);
    assert!(review.stdout.is_empty(), "stdout: {}", review.stdout);
    assert!(review.stderr.contains("manifest.json"), "{}", review.stderr);
    assert!(review.stderr.contains("identity"), "{}", review.stderr);
}

#[test]
fn manifest_entry_keeps_an_explicit_empty_identity() {
    let (dir, project_path) =
        seal_project("review_cli_empty_identity", Metrics::default(), 1, None);
    let mut entry = manifest_entry(
        "display name",
        "completed",
        vec![project_path.display().to_string()],
        0.1,
        None,
    );
    entry["identity"] = serde_json::Value::String(String::new());
    write_manifest(&dir, dir.to_str().unwrap(), &[entry]);

    let review = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    assert!(review.success, "stderr: {}", review.stderr);
    let document: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    assert_eq!(document["results"][0]["identity"], "");
}

/// Every file under `dir`, recursively, as `(relative path, len, modified)`
/// triples -- the `walk` idiom in `crates/cli/tests/project_cli.rs`,
/// extended with size and mtime so `review_writes_nothing` catches a file
/// rewritten in place, not just one added or removed.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let meta = entry.metadata().unwrap();
                out.push((
                    path.strip_prefix(dir).unwrap().to_path_buf(),
                    meta.len(),
                    meta.modified().unwrap(),
                ));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// --- tests -------------------------------------------------------------

#[test]
fn review_of_a_frame_directory_refuses_and_names_stack() {
    let dir = common::tmp_dir("review_cli_frame_dir");
    frame(&dir, "IMG_0001.tif", None);
    frame(&dir, "IMG_0002.tif", None);
    let result = run_cli_in(&dir, &["review", dir.to_str().unwrap()]);
    assert!(!result.success, "a bare frame directory must be refused");
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains("focusforge-cli stack"),
        "the refusal must name `stack`: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("--groups") || result.stderr.contains("--write-groups"),
        "the refusal must name `--groups` / `--write-groups`: {}",
        result.stderr
    );
}

#[test]
fn review_emits_one_record_per_manifest_entry() {
    let dump = three_group_dump("review_cli_three_groups");
    let out = common::tmp_dir("review_cli_three_groups_out");
    let cwd = common::tmp_dir("review_cli_three_groups_cwd");
    let result = run_cli_in(
        &cwd,
        &dummy_stack_args(dump.to_str().unwrap(), out.to_str().unwrap()),
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let review = run_cli_in(&cwd, &["review", out.to_str().unwrap(), "--json"]);
    assert!(review.success, "stderr: {}", review.stderr);
    let doc: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "one record per group: {results:?}");
    assert_eq!(results[0]["identity"], "IMG_9801-9802");
    assert_eq!(results[1]["identity"], "IMG_9910-9911");
    assert_eq!(results[2]["identity"], "IMG_9950-9951");
}

#[test]
fn review_reports_a_failed_group_as_a_failed_record() {
    let dump = common::tmp_dir("review_cli_failing_group");
    for (file, when) in [
        ("IMG_9801.tif", "2026:08:29 18:42:41"),
        ("IMG_9802.tif", "2026:08:29 18:42:42"),
    ] {
        frame(&dump, file, Some(when));
    }
    // A differently sized frame in the second group: the strict uniformity
    // guard refuses that group and only that group.
    write_frame_with_capture_time(
        &dump.join("IMG_9910.tif"),
        8,
        8,
        Some("2026:08:29 19:02:55"),
    )
    .unwrap();
    write_frame_with_capture_time(
        &dump.join("IMG_9911.tif"),
        16,
        8,
        Some("2026:08:29 19:02:56"),
    )
    .unwrap();

    let out = common::tmp_dir("review_cli_failing_group_out");
    let cwd = common::tmp_dir("review_cli_failing_group_cwd");
    let result = run_cli_in(
        &cwd,
        &dummy_stack_args(dump.to_str().unwrap(), out.to_str().unwrap()),
    );
    assert!(!result.success, "one group failed, so the run exits 1");

    let review = run_cli_in(&cwd, &["review", out.to_str().unwrap(), "--json"]);
    assert!(
        review.success,
        "review itself must exit 0 even when a reviewed run failed: {}",
        review.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "every attempted group is a record");
    assert_eq!(results[0]["identity"], "IMG_9801-9802");
    assert_eq!(results[0]["outcome"], "completed");
    assert_eq!(results[1]["identity"], "IMG_9910-9911");
    assert_eq!(results[1]["outcome"], "failed");
    assert!(
        results[1]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "the failure is named: {}",
        results[1]
    );

    // Absent, never zeroed.
    assert!(results[1]["metrics"].is_null());
    assert!(
        results[1]["metrics_reason"]
            .as_str()
            .is_some_and(|r| !r.is_empty())
    );
    let text = run_cli_in(&cwd, &["review", out.to_str().unwrap()]).stdout;
    assert!(
        !text.contains("sharpness=0 ") && !text.contains("coherence=0.0000"),
        "the table must never render an absent metric as a zero: {text}"
    );
}

#[test]
fn a_completed_but_unmeasured_result_reports_a_metrics_reason() {
    let (dir, project_path) = seal_project("review_cli_unmeasured", Metrics::default(), 3, None);
    write_manifest(
        &dir,
        dir.to_str().unwrap(),
        &[manifest_entry(
            "unmeasured",
            "completed",
            vec![project_path.display().to_string()],
            0.1,
            None,
        )],
    );

    let review = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    assert!(review.success, "stderr: {}", review.stderr);
    let doc: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["outcome"], "completed");
    assert!(results[0]["metrics"].is_null(), "{}", results[0]);
    assert_eq!(
        results[0]["metrics_reason"],
        "the run recorded no sharpness numbers"
    );
}

#[test]
fn review_record_has_identity_outcome_sharpness_and_spread() {
    let doc = shared_review_document();
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "one group, one record: {results:?}");
    let record = &results[0];
    assert!(record["identity"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(record["outcome"], "completed");
    let metrics = &record["metrics"];
    assert!(!metrics.is_null(), "{record}");
    let ratio = metrics["sharpness_ratio"].as_f64().expect("a ratio");
    assert!(ratio > 0.0, "sharpness ratio must be positive: {ratio}");
    assert_eq!(metrics["distinct_sources"], 8);
    let coherence = metrics["coherence"].as_f64().expect("a coherence value");
    assert!(
        (0.0..=1.0).contains(&coherence),
        "coherence must be a fraction: {coherence}"
    );
}

#[test]
fn review_values_do_not_depend_on_the_directory_name() {
    let metrics = Metrics {
        composite_lapvar: Some(500_000.0),
        max_input_lapvar: Some(400_000.0),
        lapvar_window: None,
    };
    let (dir_a, project_a) = seal_project("review_cli_dirname_alpha", metrics, 4, None);
    let (dir_b, project_b) = seal_project("review_cli_dirname_bravo_longer_name", metrics, 4, None);
    write_manifest(
        &dir_a,
        dir_a.to_str().unwrap(),
        &[manifest_entry(
            "one",
            "completed",
            vec![project_a.display().to_string()],
            1.0,
            None,
        )],
    );
    write_manifest(
        &dir_b,
        dir_b.to_str().unwrap(),
        &[manifest_entry(
            "one",
            "completed",
            vec![project_b.display().to_string()],
            1.0,
            None,
        )],
    );

    let review_a = run_cli_in(&dir_a, &["review", dir_a.to_str().unwrap(), "--json"]);
    let review_b = run_cli_in(&dir_b, &["review", dir_b.to_str().unwrap(), "--json"]);
    assert!(review_a.success && review_b.success);
    let doc_a: serde_json::Value = serde_json::from_str(&review_a.stdout).unwrap();
    let doc_b: serde_json::Value = serde_json::from_str(&review_b.stdout).unwrap();
    assert_eq!(
        doc_a["results"][0]["metrics"], doc_b["results"][0]["metrics"],
        "the same project's metrics must not depend on the reviewing directory's name"
    );
}

#[test]
fn a_degenerate_stack_scores_lower_on_sharpness_and_narrower_on_spread() {
    let real = shared_review_document();
    let real_ratio = real["results"][0]["metrics"]["sharpness_ratio"]
        .as_f64()
        .expect("the real fixture must carry a sharpness ratio");

    let degenerate_metrics = Metrics {
        composite_lapvar: Some(1_000.0),
        max_input_lapvar: Some(50_000.0),
        lapvar_window: None,
    };
    let (dir, project_path) = seal_project(
        "review_cli_degenerate",
        degenerate_metrics,
        4,
        Some(0), // every pixel picks the same one source -- one frame repeated.
    );
    write_manifest(
        &dir,
        dir.to_str().unwrap(),
        &[manifest_entry(
            "degenerate",
            "completed",
            vec![project_path.display().to_string()],
            0.05,
            None,
        )],
    );
    let review = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    assert!(review.success, "stderr: {}", review.stderr);
    let doc: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    let metrics = &doc["results"][0]["metrics"];
    let degenerate_ratio = metrics["sharpness_ratio"].as_f64().unwrap();
    assert!(
        degenerate_ratio < real_ratio,
        "a degenerate stack must score a strictly lower sharpness ratio: \
         degenerate={degenerate_ratio} real={real_ratio}"
    );
    assert_eq!(
        metrics["distinct_sources"], 1,
        "one frame repeated uses exactly one distinct source"
    );
    // Coherence is deliberately *not* asserted lower here: a single-source
    // map is perfectly coherent at 1.0, so coherence ranks results only
    // alongside the source count, never alone.
}

#[test]
fn json_is_one_wrapped_document() {
    let (dir, project_path) = seal_project(
        "review_cli_json_wrapped",
        Metrics {
            composite_lapvar: Some(10_000.0),
            max_input_lapvar: Some(8_000.0),
            lapvar_window: None,
        },
        2,
        None,
    );
    write_manifest(
        &dir,
        dir.to_str().unwrap(),
        &[manifest_entry(
            "one",
            "completed",
            vec![project_path.display().to_string()],
            0.1,
            None,
        )],
    );
    let review = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    assert!(review.success, "stderr: {}", review.stderr);

    let mut de =
        serde_json::Deserializer::from_str(&review.stdout).into_iter::<serde_json::Value>();
    let first = de.next().expect("stdout must hold one JSON value").unwrap();
    assert!(de.next().is_none(), "stdout must hold exactly one document");
    assert!(first.get("format").is_none());
    assert!(first["results"].is_array());
}

#[test]
fn table_and_json_carry_the_same_values() {
    let (dir, project_path) = seal_project(
        "review_cli_table_json",
        Metrics {
            composite_lapvar: Some(123_456.0),
            max_input_lapvar: Some(100_000.0),
            lapvar_window: None,
        },
        4,
        None,
    );
    write_manifest(
        &dir,
        dir.to_str().unwrap(),
        &[manifest_entry(
            "measured",
            "completed",
            vec![project_path.display().to_string()],
            0.1,
            None,
        )],
    );
    let json = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    let table = run_cli_in(&dir, &["review", dir.to_str().unwrap()]);
    assert!(json.success && table.success);
    let doc: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let metrics = &doc["results"][0]["metrics"];
    let ratio = metrics["sharpness_ratio"].as_f64().unwrap();
    let sources = metrics["distinct_sources"].as_u64().unwrap();
    let coherence = metrics["coherence"].as_f64().unwrap();
    for needle in [
        format!("sharpness={ratio:.4}"),
        format!("sources={sources}"),
        format!("coherence={coherence:.4}"),
    ] {
        assert!(
            table.stdout.contains(&needle),
            "table must carry {needle:?}: {}",
            table.stdout
        );
    }
}

#[test]
fn an_unopenable_project_is_a_failed_record() {
    let (dir, good_project) = seal_project(
        "review_cli_unopenable",
        Metrics {
            composite_lapvar: Some(10_000.0),
            max_input_lapvar: Some(8_000.0),
            lapvar_window: None,
        },
        2,
        None,
    );
    let truncated = dir.join("truncated.ffp");
    let bytes = std::fs::read(&good_project).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();

    write_manifest(
        &dir,
        dir.to_str().unwrap(),
        &[
            manifest_entry(
                "good",
                "completed",
                vec![good_project.display().to_string()],
                0.1,
                None,
            ),
            manifest_entry(
                "corrupt",
                "completed",
                vec![truncated.display().to_string()],
                0.1,
                None,
            ),
        ],
    );

    let review = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    assert!(
        review.success,
        "an unopenable project must not crash review: {}",
        review.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["identity"], "good");
    assert_eq!(results[0]["outcome"], "completed");
    assert!(!results[0]["metrics"].is_null());
    assert_eq!(results[1]["identity"], "corrupt");
    assert_eq!(results[1]["outcome"], "failed");
    assert!(
        results[1]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{}",
        results[1]
    );
}

#[test]
fn unresolved_sources_are_listed_not_fatal() {
    let (dir, project_path) = seal_project(
        "review_cli_unresolved_sources",
        Metrics {
            composite_lapvar: Some(10_000.0),
            max_input_lapvar: Some(8_000.0),
            lapvar_window: None,
        },
        3,
        None,
    );
    // Remove one source frame after sealing -- the project still opens; the
    // source is simply unresolved.
    let missing = dir.join("frame-01.tif");
    assert!(missing.is_file());
    std::fs::remove_file(&missing).unwrap();

    write_manifest(
        &dir,
        dir.to_str().unwrap(),
        &[manifest_entry(
            "one",
            "completed",
            vec![project_path.display().to_string()],
            0.1,
            None,
        )],
    );
    let review = run_cli_in(&dir, &["review", dir.to_str().unwrap(), "--json"]);
    assert!(review.success, "stderr: {}", review.stderr);
    let doc: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    let record = &doc["results"][0];
    assert_eq!(record["outcome"], "completed");
    assert!(
        !record["metrics"].is_null(),
        "metrics must survive: {record}"
    );
    let unresolved = record["unresolved_sources"].as_array().unwrap();
    assert_eq!(unresolved.len(), 1, "{record}");
    assert!(
        unresolved[0]
            .as_str()
            .is_some_and(|p| p.contains("frame-01.tif")),
        "{record}"
    );
}

#[test]
fn review_writes_nothing() {
    let (dir, project_path) = seal_project(
        "review_cli_writes_nothing",
        Metrics {
            composite_lapvar: Some(10_000.0),
            max_input_lapvar: Some(8_000.0),
            lapvar_window: None,
        },
        2,
        None,
    );
    write_manifest(
        &dir,
        dir.to_str().unwrap(),
        &[manifest_entry(
            "one",
            "completed",
            vec![project_path.display().to_string()],
            0.1,
            None,
        )],
    );

    // The snapshotted directory must never double as the cwd `review` runs
    // in, so a stray write into cwd would be caught by the comparison below
    // rather than silently landing inside the very tree being compared.
    let cwd = common::tmp_dir("review_cli_writes_nothing_cwd");
    let before = snapshot(&dir);
    let json = run_cli_in(&cwd, &["review", dir.to_str().unwrap(), "--json"]);
    let table = run_cli_in(&cwd, &["review", dir.to_str().unwrap()]);
    assert!(json.success && table.success);
    let after = snapshot(&dir);
    assert_eq!(
        before, after,
        "review must write nothing -- no project, no image, no scratch state"
    );
}

// --- ignored: the reference stack ---------------------------------------

/// `<dir>/frames/` holding a symlink to each of the reference stack's 61 `IMG_`
/// frames -- the reference stack directory also holds `Mantis_Stacked.tif`, the
/// reference *result*, which `focusforge-cli stack` would otherwise take for a
/// 62nd frame.
fn link_reference_frames(dir: &Path, stack_dir: &Path) -> PathBuf {
    let frames = dir.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    let mut count = 0;
    for entry in std::fs::read_dir(stack_dir).expect("the reference stack must be readable") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.contains("IMG_") || !name.ends_with(".tif") {
            continue;
        }
        std::os::unix::fs::symlink(&path, frames.join(&name)).unwrap();
        count += 1;
    }
    assert_eq!(count, 61, "IMG_-filtered list must be the 61 source frames");
    frames
}

/// A real batch run over the 16 GB reference stack, reviewed end to end:
/// `stack --no-align --no-crop` followed by `review --json` over its output
/// directory.
///
/// Ranges asserted are the ones recorded at full resolution on the same
/// unaligned geometry (tile 512, levels 5):
/// - composite Laplacian variance 374,720.21, all 61 of 61 frames used as
///   Z-Map sources -- so a sharpness ratio strictly above 1.0 (the
///   composite must be sharper than any single input frame) and
///   `distinct_sources` close to 61;
/// - coherence against the random baseline for a 61-way choice (~1.6%), not
///   the aligned 512 px fixture's `>= 0.55` floor -- an **unaligned**
///   8192x5464 stack measures meaningfully less spatially coherent than an
///   aligned 512 px crop (measured 0.2338 here).
///
/// Prints the wall clock `review` itself spent on this one result.
#[test]
#[ignore = "reads the 16 GB reference stack"]
fn review_of_a_reference_run() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };

    let dir = common::tmp_dir("review_cli_reference_run");
    let cli = snapshot_bin(&dir);
    let frames = link_reference_frames(&dir, &stack_dir);
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let stack_result = run_bin(
        &dir,
        &cli,
        &[
            "stack",
            frames.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(
        stack_result.success,
        "reference stack run must succeed -- stderr: {}",
        stack_result.stderr
    );

    let review_start = Instant::now();
    let review = run_bin(&dir, &cli, &["review", out.to_str().unwrap(), "--json"]);
    let review_elapsed = review_start.elapsed();
    assert!(review.success, "stderr: {}", review.stderr);

    let doc: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    let results = doc["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "review must find at least one detected stack: {results:?}"
    );
    assert!(
        results
            .iter()
            .all(|record| record["outcome"] == "completed"),
        "every attempted stack must succeed on a clean reference run: {results:?}"
    );

    let record = &results[0];
    let metrics = &record["metrics"];
    assert!(!metrics.is_null(), "{record}");
    let ratio = metrics["sharpness_ratio"].as_f64().expect("a ratio");
    let sources = metrics["distinct_sources"]
        .as_u64()
        .expect("a source count");
    let coherence = metrics["coherence"].as_f64().expect("a coherence value");

    // Printed before any assertion, so the measured numbers survive a
    // failing range.
    println!(
        "review_of_a_reference_run: results={} review_wall={:.3}s ratio={ratio:.4} sources={sources} coherence={coherence:.4}",
        results.len(),
        review_elapsed.as_secs_f64(),
    );

    assert!(
        ratio > 1.0 && ratio < 3.0,
        "sharpness ratio outside the recorded full-resolution range: {ratio}"
    );
    assert!(
        sources >= 40,
        "distinct sources far below the recorded full-resolution 61 of 61: {sources}"
    );
    // Sanity against the ~1.6% random baseline for 61 sources (see above).
    assert!(
        (0.1..=1.0).contains(&coherence),
        "coherence not meaningfully above the ~1.6% random baseline for 61 sources: {coherence}"
    );
}
