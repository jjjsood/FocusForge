// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli export`: a project in, an image out.
//!
//! These are what prove the container changed the *destination* and not the
//! *data*. One `laplacian` run over `fixtures/mantis-512` is shared through
//! a `OnceLock` -- the same discipline `stack_cli.rs` uses -- and the
//! library oracle beside it is the independent writer the exported composite
//! is compared against, whole file for whole file.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct CliOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_cli(args: &[&str]) -> CliOutput {
    let output = bin().args(args).output().expect("failed to run binary");
    CliOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn only_project(dir: &Path) -> PathBuf {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "ffproj"))
        .collect();
    assert_eq!(found.len(), 1, "exactly one project expected in {dir:?}");
    found.pop().unwrap()
}

/// One `stack` run over `fixtures/mantis-512`, collected into a `-o`
/// directory. ~50 s, paid once for this whole binary.
fn shared_project() -> &'static PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let out = tmp_dir("export_cli_shared_stack");
        let input = common::fixtures_dir().join("mantis-512");
        let mut args = vec![
            "stack".to_string(),
            input.to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--algo".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ];
        // The GPU path's ±1 LSB budget would break the exact-byte equality
        // this file asserts against a CPU library oracle -- the same reason
        // `stack_cli.rs`'s shared run pins `--no-gpu`.
        if cfg!(feature = "gpu") {
            args.push("--no-gpu".to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_cli(&args);
        assert!(
            result.success,
            "stack must succeed -- stderr: {}",
            result.stderr
        );
        only_project(&out)
    })
}

/// `laplacian` driven directly through the library at the CLI's own
/// geometry, writing its composite with `TiffWriter`'s own default
/// description -- exactly what the CLI resolves when a `--no-align` run
/// carries no alignment description of its own. The whole-file oracle.
fn library_composite() -> &'static PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let input = common::fixtures_dir().join("mantis-512");
        let stack = TiffStack::open(&input).expect("mantis-512 must open");
        let (width, height) = stack.dimensions();
        let spp = stack.header(0).samples_per_pixel;
        let frame_count = stack.len();
        let stack_ctx = StackContext::new(frame_count, width, height, spp);

        let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
        let resolved = params::resolve(algo.params(), &ParamValues::new(), &stack_ctx)
            .expect("resolve must succeed");
        let values = params::resolved_values(&resolved);
        let levels = algo.tile_levels(&values);

        let dir = tmp_dir("export_cli_oracle");
        let composite_path = dir.join("oracle.tif");
        let zmap_path = dir.join("oracle-zmap.tif");
        let options = TileSchedulerOptions {
            tile_size: 512,
            levels,
            max_bytes_in_flight: 1 << 30,
            scratch_base: dir.join("scratch"),
        };
        let scheduler =
            TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");
        let consumer = FusionConsumer::create(
            algo,
            values,
            &composite_path,
            Some(&zmap_path),
            FusionGeometry {
                width,
                height,
                samples_per_pixel: spp,
                frame_count: frame_count as u32,
                crop: None,
                description: None,
            },
        )
        .expect("consumer must create");
        scheduler
            .run(&NoopPreprocess, &consumer)
            .expect("run must succeed");
        consumer.finish().expect("finish must succeed");
        composite_path
    })
}

#[test]
fn exported_composite_is_byte_identical_to_the_staged_plane() {
    let project = shared_project();
    let dir = tmp_dir("export_cli_composite");
    let out = dir.join("out.tif");
    let result = run_cli(&[
        "export",
        project.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(result.success, "stderr: {}", result.stderr);

    let exported = std::fs::read(&out).expect("export must have written the file");
    let oracle = std::fs::read(library_composite()).expect("oracle composite");
    assert_eq!(
        exported.len(),
        oracle.len(),
        "the exported composite differs in length from the plane fusion wrote"
    );
    assert!(
        exported == oracle,
        "the exported composite is not byte-identical to the plane fusion wrote -- \
         same ImageDescription, same twelve-tag IFD and same pixels is the whole claim"
    );
    assert!(
        !dir.join("out.tif.partial").exists(),
        "a temp file survived a successful export"
    );
}

#[test]
fn exported_zmap_matches_the_golden_map() {
    let project = shared_project();
    let dir = tmp_dir("export_cli_zmap");
    let out = dir.join("out-zmap.tif");
    let result = run_cli(&[
        "export",
        project.to_str().unwrap(),
        "--zmap",
        out.to_str().unwrap(),
    ]);
    assert!(result.success, "stderr: {}", result.stderr);

    let golden = common::fixtures_dir()
        .join("mantis-512")
        .join("golden")
        .join("laplacian-zmap.tif");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&golden).unwrap(),
        "the exported Z-Map must equal the pinned golden map byte for byte -- the \
         Z-Map is invariant to tile geometry, the composite is not"
    );
}

#[test]
fn export_needs_at_least_one_output_flag() {
    let project = shared_project();
    let result = run_cli(&["export", project.to_str().unwrap()]);
    assert!(!result.success);
    assert!(
        result.stderr.contains("-o") && result.stderr.contains("--zmap"),
        "the refusal must name both flags: {}",
        result.stderr
    );
}

#[test]
fn export_warns_about_unresolved_sources_and_still_writes() {
    // A project written with `-o <dir>` records absolute, `as-given`
    // sources by construction: "relative" means descendant of the
    // project's own directory.
    let project = shared_project();
    let dir = tmp_dir("export_cli_unresolved");
    let out = dir.join("moved.tif");
    let result = run_cli(&[
        "export",
        project.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(
        result.success,
        "an unresolved source is a warning, not a refusal"
    );
    assert!(
        result.stderr.contains("unresolved") || result.stderr.contains("could not be resolved"),
        "the warning must appear on stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("00.tif") || result.stderr.contains(".tif"),
        "the warning must name the sources: {}",
        result.stderr
    );
    assert!(out.exists(), "the image is still written");
}

#[test]
fn export_json_reports_what_it_wrote() {
    let project = shared_project();
    let dir = tmp_dir("export_cli_json");
    let composite = dir.join("out.tif");
    let zmap = dir.join("out-zmap.tif");
    let result = run_cli(&[
        "export",
        project.to_str().unwrap(),
        "-o",
        composite.to_str().unwrap(),
        "--zmap",
        zmap.to_str().unwrap(),
        "--json",
    ]);
    assert!(result.success, "stderr: {}", result.stderr);
    let report: serde_json::Value = serde_json::from_str(&result.stdout).expect("--json parses");
    let wrote: Vec<String> = report["wrote"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        wrote.len(),
        2,
        "one invocation emitted both payloads: {wrote:?}"
    );
    assert_eq!(report["width"], serde_json::json!(512));
    assert_eq!(report["height"], serde_json::json!(512));
    assert_eq!(report["samples_per_pixel"], serde_json::json!(3));
    assert_eq!(report["algorithm"], serde_json::json!("laplacian"));
    assert!(report["frame_count"].as_u64().unwrap() >= 2);
    assert!(!report["unresolved_sources"].as_array().unwrap().is_empty());
    assert!(composite.exists() && zmap.exists());
}
