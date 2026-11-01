// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli stack`: a successful run, every
//! rejection path, the Z-Map every project carries, the report's fields and
//! the library-versus-CLI agreement.
//!
//! Every test isolates `.scratch/` with its own per-process temp directory
//! and `.current_dir(dir)`: `stack`'s scratch tree is cwd-relative, so
//! concurrent test runs sharing one `target/` -- or two tests in the same
//! run under cargo's default parallel execution -- must never share a
//! working directory either. The working directory a spawned process runs
//! in is never a directory a test also feeds in as input, names as `-o`, or
//! enumerates; it holds only the test's own named children and `.scratch/`.
//!
//! Every rejection path below runs against `dummy-stack` (registered in this
//! crate's test binary through `crates/cli/Cargo.toml`'s `[dev-dependencies]`
//! feature, as `crates/cli/tests/algorithms_cmd.rs` documents) and costs
//! nothing; a parameter-parse rejection (`out_of_range_parameter_is_rejected`,
//! `refuses_levels_the_tile_geometry_cannot_plan`) is cheap too even though
//! it names `laplacian`, since it never reaches a tile read. Only
//! `stacks_the_fixture_set` and `cli_output_matches_the_library_result` pay
//! for a real `laplacian` fusion, shared through a `OnceLock` so it runs
//! exactly twice in this whole file: once through the binary, once in
//! process as the library oracle.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};

use common::{CliOutput, TmpDir, mantis_512_dir, read_whole, tmp_dir};

fn bin() -> Command {
    common::cli_bin("stack_cli")
}

/// Both payloads of `project`, exported beside it as `<stem>-composite.tif`
/// and `<stem>-zmap.tif`.
///
/// `stack` writes a project, so every test that needs the *files* asks
/// `export` for them. One invocation emits both, so the container is opened
/// once.
fn export_both(cwd: &Path, project: &Path) -> (PathBuf, PathBuf) {
    let composite = project.with_extension("composite.tif");
    let zmap = project.with_extension("zmap.tif");
    let out = run_cli_in(
        cwd,
        &[
            "export",
            project.to_str().unwrap(),
            "-o",
            composite.to_str().unwrap(),
            "--zmap",
            zmap.to_str().unwrap(),
        ],
    );
    assert!(out.success, "export stderr: {}", out.stderr);
    (composite, zmap)
}

fn partial_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    common::run_in(&mut bin(), dir, args)
}

/// The one `laplacian` run driven through the built binary: `-o` and no
/// `--set`, so `levels` resolves through its `auto` hook. Shared by every
/// test that needs its output, its Z-Map or its report.
///
/// The run itself writes a **project**; `output` and `zmap` are what one
/// `export` call then brings back out as files, and the ~50 s fusion is
/// paid exactly once.
struct SharedRun {
    /// The `.ffp` the run wrote.
    #[allow(dead_code)]
    project: PathBuf,
    output: PathBuf,
    zmap: PathBuf,
    report: serde_json::Value,
    /// The run's own working directory, held alive for the process's
    /// lifetime: `stacks_the_fixture_set` and
    /// `cli_output_matches_the_library_result` read `output`/`zmap` long
    /// after this function returns, so the directory must outlive the
    /// `OnceLock` cell, not just the closure that fills it. The `OnceLock`
    /// static is never dropped, so `TmpDir::drop`'s `remove_dir_all` never
    /// runs for this one.
    #[allow(dead_code)]
    _cwd: TmpDir,
}

fn shared_laplacian_run() -> &'static SharedRun {
    static ONCE: OnceLock<SharedRun> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cwd = tmp_dir("stack_cli_shared_laplacian");
        let out_dir = cwd.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let project = out_dir.join("mantis-512.ffp");
        let output = out_dir.join("out.tif");
        let zmap = out_dir.join("out-zmap.tif");
        let mut args = vec![
            "stack".to_string(),
            mantis_512_dir().to_str().unwrap().to_string(),
            "-o".to_string(),
            out_dir.to_str().unwrap().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--json".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
            "--set".to_string(),
            "equalize=false".to_string(),
        ];
        // Verifies CLI plumbing reproduces `library_oracle`'s in-process,
        // CPU-only call exactly, not GPU-vs-CPU numerics (that's the
        // whole-pipeline `parity_pipeline::*`'s job). Without `--cpu`,
        // `stack --algorithm laplacian` takes the GPU path on any adapter-equipped
        // machine, breaking this test's exact-byte equality via the ±1 LSB
        // GPU/CPU budget; the flag only exists when `gpu` is compiled in.
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
        let report: serde_json::Value =
            serde_json::from_str(&result.stdout).expect("--json must parse");

        // One `export` emits both payloads from the container the run wrote.
        let exported = run_cli_in(
            &cwd,
            &[
                "export",
                project.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--zmap",
                zmap.to_str().unwrap(),
            ],
        );
        assert!(
            exported.success,
            "export must succeed -- stderr: {}",
            exported.stderr
        );
        SharedRun {
            project,
            output,
            zmap,
            report,
            _cwd: cwd,
        }
    })
}

/// `laplacian` driven directly through the library, at the exact geometry
/// the CLI runs at (`DEFAULT_TILE_SIZE` 512, resolved `levels` 5) -- the
/// oracle `cli_output_matches_the_library_result` compares the CLI's own
/// output against. This, plus [`shared_laplacian_run`], is the whole cost of
/// this file: `laplacian` runs exactly twice.
struct OracleResult {
    composite: Vec<u16>,
    zmap: Vec<u16>,
}

fn library_oracle() -> &'static OracleResult {
    static ONCE: OnceLock<OracleResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
        let (width, height) = stack.dimensions();
        let spp = stack.header(0).samples_per_pixel;
        let frame_count = stack.len();
        let stack_ctx = StackContext::new(frame_count, width, height, spp);

        let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
        let resolved = params::resolve(algo.params(), &ParamValues::new(), &stack_ctx)
            .expect("resolve must succeed");
        let values = params::resolved_values(&resolved);
        let support = algo.tile_support(&values);

        // This function spawns nothing, so `dir` need not outlive the
        // closure: both TIFFs below are read into `Vec<u16>` before it
        // returns, and `dir`'s own `Drop` then cleans up the scratch
        // directory.
        let dir = tmp_dir("stack_cli_oracle");
        let composite_path = dir.join("oracle.tif");
        let zmap_path = dir.join("oracle-zmap.tif");
        let mut options = TileSchedulerOptions::explicit_budget(dir.join("scratch"), 1 << 30);
        options.apply_support(support);
        options.tile_size = 512;
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

        let (composite, ..) = read_whole(&composite_path);
        let (zmap, ..) = read_whole(&zmap_path);
        OracleResult { composite, zmap }
    })
}

#[test]
fn stacks_the_fixture_set() {
    let run = shared_laplacian_run();
    let (_, w, h, spp) = read_whole(&run.output);
    assert_eq!((w, h, spp), (512, 512, 3));
    assert!(
        !partial_of(&run.output).exists(),
        "no .partial may remain beside a successful output"
    );
}

#[test]
fn cli_output_matches_the_library_result() {
    let run = shared_laplacian_run();
    let oracle = library_oracle();

    let (cli_composite, ..) = read_whole(&run.output);
    assert_eq!(
        cli_composite, oracle.composite,
        "the CLI's `export -o` output must equal the library's fused composite"
    );

    let (cli_zmap, ..) = read_whole(&run.zmap);
    assert_eq!(
        cli_zmap, oracle.zmap,
        "the CLI Z-Map must equal a library run driven at the CLI's own geometry"
    );

    let golden_zmap = common::fixtures_dir()
        .join("mantis-512")
        .join("golden")
        .join("laplacian-zmap.tif");
    let (golden, ..) = read_whole(&golden_zmap);
    assert_eq!(
        cli_zmap, golden,
        "the CLI Z-Map must equal the pinned golden map byte for byte -- the Z-Map \
         is invariant to tile geometry, the composite is not"
    );
}

#[test]
fn unknown_algorithm_is_rejected_early() {
    let missing_dir = "/definitely/does/not/exist/xyz-stack-cli-unknown-algo";
    let cwd = tmp_dir("unknown_algorithm_is_rejected_early");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            missing_dir,
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "no-such-algorithm",
        ],
    );
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(
        out.stderr
            .contains("unknown algorithm \"no-such-algorithm\""),
        "stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("known algorithms:"),
        "stderr: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains(missing_dir),
        "the missing directory must never be named -- the algorithm id is checked first: {}",
        out.stderr
    );
}

#[test]
fn unknown_parameter_key_is_rejected() {
    let missing_dir = "/definitely/does/not/exist/xyz-stack-cli-unknown-key";
    let cwd = tmp_dir("unknown_parameter_key_is_rejected");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            missing_dir,
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--set",
            "bogus=1",
        ],
    );
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(out.stderr.contains("bogus"), "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("threshold") && out.stderr.contains("mode"),
        "must name dummy-stack's declared keys: {}",
        out.stderr
    );
    assert!(!out.stderr.contains(missing_dir), "stderr: {}", out.stderr);
}

#[test]
fn out_of_range_parameter_is_rejected() {
    let missing_dir = "/definitely/does/not/exist/xyz-stack-cli-out-of-range";
    let cwd = tmp_dir("out_of_range_parameter_is_rejected");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let args = common::push_no_gpu(vec![
        "stack",
        missing_dir,
        "-o",
        out_dir.to_str().unwrap(),
        "--algorithm",
        "laplacian",
        "--set",
        "levels=999",
    ]);
    let out = run_cli_in(&cwd, &args);
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(out.stderr.contains("999"), "stderr: {}", out.stderr);
    assert!(out.stderr.contains('1'), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("16"), "stderr: {}", out.stderr);
    assert!(!out.stderr.contains(missing_dir), "stderr: {}", out.stderr);
}

/// `levels=7` is in schema range (`[1, 16]`) but deeper than a 512 px tile
/// can plan (margin sized for 5): `laplacian::fuse` and the
/// scheduler's margin must agree, so this must reach
/// `TileError::NoInterior`, naming the tile size that would work, before
/// any tile is read.
#[test]
fn refuses_levels_the_tile_geometry_cannot_plan() {
    let cwd = tmp_dir("refuses_levels_the_tile_geometry_cannot_plan");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let input = mantis_512_dir();
    let output = out_dir.join("refuses_levels_the_tile_geometry_cannot_plan.tif");
    let zmap = out_dir.join("refuses_levels_the_tile_geometry_cannot_plan-zmap.tif");
    let args = common::push_no_gpu(vec![
        "stack",
        input.to_str().unwrap(),
        "-o",
        out_dir.to_str().unwrap(),
        "--algorithm",
        "laplacian",
        "--set",
        "levels=7",
        "--tile-size",
        "512",
        "--no-align",
        "--no-crop",
    ]);
    let out = run_cli_in(&cwd, &args);
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(
        out.stderr.contains("tile_size >= 2112"),
        "must read as advice naming the smallest workable tile size: {}",
        out.stderr
    );
    assert!(!output.exists(), "no composite may be created");
    assert!(!zmap.exists(), "no zmap may be created");
    assert!(!partial_of(&output).exists());
    assert!(!partial_of(&zmap).exists());
}

#[test]
fn reports_resolved_parameter_values() {
    let run = shared_laplacian_run();
    let params = run.report["params"].as_array().expect("params array");
    let levels = params
        .iter()
        .find(|p| p["id"] == "levels")
        .expect("levels param present");
    assert_eq!(levels["value"], 5, "report: {}", run.report);
    assert_eq!(levels["provenance"], "Auto", "report: {}", run.report);
}

/// Every run writes a Z-Map, because a project carries its own: the map
/// is the document, so a run that produced none would produce nothing
/// worth keeping. Asserts that a run writes **no image at all**.
#[test]
fn every_run_carries_a_zmap_and_writes_no_image() {
    let run = shared_laplacian_run();
    assert!(
        run.zmap.exists(),
        "a project carries its own Z-Map, and `export --zmap` must produce it"
    );

    let cwd = tmp_dir("writes_the_zmap_on_request_off");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let output = out_dir.join("mantis-512.ffp");
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let mut entries: Vec<String> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["manifest.json".to_string(), "mantis-512.ffp".to_string()],
        "one project and its run manifest, nothing else -- no image, no partial: {entries:?}"
    );
    let (_, zmap) = export_both(&cwd, &output);
    let (_, zw, zh, zspp) = read_whole(&zmap);
    assert_eq!(
        (zw, zh, zspp),
        (512, 512, 1),
        "the map is one index per pixel"
    );
}

#[test]
fn refuses_empty_dir_and_unwritable_output() {
    let cwd = tmp_dir("refuses_empty_dir_and_unwritable_output");
    let empty_dir = cwd.join("empty");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let output_a = out_dir.join("refuses_empty_dir_and_unwritable_output_a.tif");
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            empty_dir.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
        ],
    );
    assert!(!out.success, "an empty directory must be refused");
    assert!(!output_a.exists());

    let missing_dir = "/definitely/does/not/exist/xyz-stack-cli-unwritable-output";
    let bad_output = PathBuf::from("/definitely/does/not/exist/xyz-output-parent/out.tif");
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            missing_dir,
            "-o",
            bad_output.parent().unwrap().to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
        ],
    );
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(
        out.stderr.contains("xyz-output-parent"),
        "must name the output's missing parent, not the missing input directory: {}",
        out.stderr
    );
}

#[test]
fn report_contains_every_field() {
    let cwd = tmp_dir("report_contains_every_field");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    for needle in [
        "8 frames",
        "512x512",
        "dummy-stack",
        "Dummy Stack (test-only)",
        "threshold",
        "mode",
        "tile_size=",
        "levels=",
        "margin=",
        "tiles=",
        "in_flight_limit=",
        "max_bytes_in_flight=",
        "budget=",
        "backed_off=",
        "wrote:",
        "wall:",
        "stages:",
    ] {
        assert!(
            out.stdout.contains(needle),
            "missing {needle:?} in: {}",
            out.stdout
        );
    }
}

#[test]
fn algo_is_required() {
    let missing_dir = "/definitely/does/not/exist/xyz-stack-cli-algo-required";
    let cwd = tmp_dir("algo_is_required");
    let output = cwd.join("algo_is_required.tif");
    let out = run_cli_in(
        &cwd,
        &["stack", missing_dir, "-o", output.to_str().unwrap()],
    );
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert_eq!(
        out.code,
        Some(1),
        "our own refusal exits 1, never clap's own usage error (exit 2): stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("--algorithm is required"),
        "stderr: {}",
        out.stderr
    );
    assert!(out.stderr.contains("dummy-stack"), "stderr: {}", out.stderr);
    assert!(!out.stderr.contains(missing_dir), "stderr: {}", out.stderr);
}

#[test]
fn working_space_names_the_encoding_of_a_tiff_stack() {
    let cwd = tmp_dir("working_space");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mantis_64 = common::fixtures_dir().join("mantis-64");
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_64.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--json",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert_eq!(report["working_space"], "encoded-unknown-transfer");
}

#[test]
fn report_json_has_every_field() {
    let cwd = tmp_dir("report_json_has_every_field");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--json",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    for key in [
        "input",
        "frame_count",
        "frames",
        "width",
        "height",
        "samples_per_pixel",
        "working_space",
        "algorithm",
        "display_name",
        "params",
        "tile_size",
        "levels",
        "margin",
        "tiles",
        "frame_tiles_read",
        "in_flight_limit",
        "peak_in_flight",
        "max_bytes_in_flight",
        "accounted_bytes_in_flight",
        "budget_provenance",
        "backed_off",
        "outputs",
        "wall_seconds",
        "stages",
        "align_workers",
        "peak_align_workers",
        "align_accounted_bytes",
        "device_peak_in_flight",
        "gpu_fallback",
    ] {
        assert!(report.get(key).is_some(), "missing key {key:?} in {report}");
    }
    // `dummy-stack` never resolves a backend, so this is the CPU half of the
    // check even on a `gpu` build: both fields must be exactly `null`.
    for key in ["device_peak_in_flight", "gpu_fallback"] {
        assert_eq!(report[key], serde_json::Value::Null, "report: {report}");
    }
    let params = report["params"].as_array().expect("params array");
    assert!(!params.is_empty());
    for p in params {
        for key in ["id", "value", "provenance"] {
            assert!(p.get(key).is_some(), "missing key {key:?} in param {p}");
        }
    }
}

/// Every entered stage reports a number, `warp_write` is `null`
/// (`stack` never enters it), and the human-text report carries the same
/// breakdown on a `stages:` line. `dummy-align` is a trivial fixed-point
/// algorithm that converges immediately, so this pays nothing for a real
/// ECC solve.
#[test]
fn stage_timings_are_reported_for_every_entered_stage() {
    let cwd = tmp_dir("stage_timings_every_entered_stage");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--align-algorithm",
            "dummy-align",
            "--no-crop",
            "--no-cache",
            "--json",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    let stages = &report["stages"];
    for key in ["setup", "align", "fuse", "finalize"] {
        assert!(
            stages[key].is_number(),
            "stages.{key} must be a number for an entered stage: {report}"
        );
    }
    assert_eq!(
        stages["warp_write"],
        serde_json::Value::Null,
        "stack never enters warp_write: {report}"
    );
    assert_eq!(
        report["align_source"], "computed",
        "a fresh alignment must report align_source=\"computed\": {report}"
    );

    let human_out_dir = cwd.join("out_human");
    std::fs::create_dir_all(&human_out_dir).unwrap();
    let human = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            human_out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--align-algorithm",
            "dummy-align",
            "--no-crop",
            "--no-cache",
        ],
    );
    assert!(human.success, "stderr: {}", human.stderr);
    assert!(
        human.stdout.contains("stages:"),
        "human report must carry a stages: line: {}",
        human.stdout
    );
    assert!(
        human.stdout.contains("finalize="),
        "human report must name each entered stage: {}",
        human.stdout
    );
}

#[test]
fn max_host_memory_lowers_never_raises_and_does_not_move_pixels() {
    let cwd = tmp_dir("max_host_memory_ceiling");
    let baseline_dir = cwd.join("baseline");
    let capped_dir = cwd.join("capped");
    let uncapped_dir = cwd.join("uncapped");
    std::fs::create_dir_all(&baseline_dir).unwrap();
    std::fs::create_dir_all(&capped_dir).unwrap();
    std::fs::create_dir_all(&uncapped_dir).unwrap();

    // `stack` writes a project into the directory `-o` names, not a bare
    // image, so each run gets its own output directory and
    // the pixels are compared through `export_both` below.
    let common_args = |output_dir: &Path| {
        vec![
            "stack".to_string(),
            mantis_512_dir().to_str().unwrap().to_string(),
            "-o".to_string(),
            output_dir.to_str().unwrap().to_string(),
            "--algorithm".to_string(),
            "dummy-stack".to_string(),
            "--json".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ]
    };

    let baseline_args = common_args(&baseline_dir);
    let baseline_args: Vec<&str> = baseline_args.iter().map(String::as_str).collect();
    let baseline = run_cli_in(&cwd, &baseline_args);
    assert!(baseline.success, "stderr: {}", baseline.stderr);

    let mut capped_args = common_args(&capped_dir);
    capped_args.push("--max-host-memory".to_string());
    capped_args.push("4MiB".to_string());
    let capped_args: Vec<&str> = capped_args.iter().map(String::as_str).collect();
    let capped = run_cli_in(&cwd, &capped_args);
    assert!(capped.success, "stderr: {}", capped.stderr);
    let capped_report: serde_json::Value =
        serde_json::from_str(&capped.stdout).expect("--json must parse");
    assert_eq!(capped_report["max_bytes_in_flight"], 4_194_304);
    assert!(
        capped_report["accounted_bytes_in_flight"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0),
        "the scheduler's accounted bytes must be reported: {capped_report}"
    );
    assert_eq!(capped_report["budget_provenance"], "Explicit");

    let mut uncapped_args = common_args(&uncapped_dir);
    uncapped_args.push("--max-host-memory".to_string());
    uncapped_args.push("1TiB".to_string());
    let uncapped_args: Vec<&str> = uncapped_args.iter().map(String::as_str).collect();
    let uncapped = run_cli_in(&cwd, &uncapped_args);
    assert!(uncapped.success, "stderr: {}", uncapped.stderr);
    let uncapped_report: serde_json::Value =
        serde_json::from_str(&uncapped.stdout).expect("--json must parse");
    // Budgets are not compared across processes: live host memory moves by
    // hundreds of MB between launches. A ceiling that did not bite leaves the
    // provenance non-`Explicit` and the budget far below the 1 TiB ceiling.
    assert_ne!(
        uncapped_report["budget_provenance"], "Explicit",
        "a ceiling above the adaptive result must not turn the budget explicit"
    );
    let uncapped_bytes_in_flight = uncapped_report["max_bytes_in_flight"]
        .as_u64()
        .expect("max_bytes_in_flight must be a number");
    assert!(
        uncapped_bytes_in_flight < (1u64 << 40),
        "a 1TiB ceiling must not itself become the reported budget: {uncapped_bytes_in_flight}"
    );

    let (baseline_composite, _) = export_both(&cwd, &baseline_dir.join("mantis-512.ffp"));
    let (capped_composite, _) = export_both(&cwd, &capped_dir.join("mantis-512.ffp"));
    let (uncapped_composite, _) = export_both(&cwd, &uncapped_dir.join("mantis-512.ffp"));
    let baseline_bytes = std::fs::read(&baseline_composite).expect("baseline output must exist");
    let capped_bytes = std::fs::read(&capped_composite).expect("capped output must exist");
    let uncapped_bytes = std::fs::read(&uncapped_composite).expect("uncapped output must exist");
    assert_eq!(
        baseline_bytes, capped_bytes,
        "lowering the budget must not move a single pixel"
    );
    assert_eq!(
        baseline_bytes, uncapped_bytes,
        "a ceiling above the adaptive result must not move a single pixel"
    );
}

/// The accounting identity: the non-null stages sum to `wall_seconds`
/// exactly (up to `f64` addition rounding), because the spans are
/// contiguous, disjoint, and share the run's own clock.
#[test]
fn stage_timings_account_for_the_wall_clock() {
    let cwd = tmp_dir("stage_timings_account_for_wall_clock");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--align-algorithm",
            "dummy-align",
            "--no-crop",
            "--json",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    let stages = &report["stages"];
    let sum: f64 = ["setup", "align", "fuse", "finalize", "warp_write"]
        .iter()
        .filter_map(|key| stages[key].as_f64())
        .sum();
    let wall = report["wall_seconds"].as_f64().expect("wall_seconds");
    assert!(
        (sum - wall).abs() <= 1e-6,
        "stage sum {sum} must equal wall_seconds {wall}: {report}"
    );
}

/// `--no-align` never enters the align stage at all: `null`, not `0.0`, and
/// `align_source` names the skip.
#[test]
fn stage_timings_mark_a_skipped_align_as_not_entered() {
    let cwd = tmp_dir("stage_timings_skipped_align");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--json",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert_eq!(
        report["stages"]["align"],
        serde_json::Value::Null,
        "no unit of alignment work ran: {report}"
    );
    assert_eq!(
        report["align_source"], "skipped",
        "must name the skip rather than leave the null unexplained: {report}"
    );
}

/// The `align` project value switches alignment off exactly like `--no-align`.
#[test]
fn set_align_false_skips_the_align_stage() {
    let cwd = tmp_dir("set_align_false");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--set",
            "align=false",
            "--no-crop",
            "--json",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert_eq!(report["align_source"], "skipped", "{report}");
}

/// `align_workers`/`peak_align_workers` are numbers only when
/// `align_source == "computed"` -- a cached, resumed or `--no-align` run
/// never earns a worker count it did not spend.
#[test]
fn align_worker_fields_follow_align_source() {
    let cwd = tmp_dir("align_worker_fields_follow_align_source");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let computed = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--align-algorithm",
            "dummy-align",
            "--no-crop",
            "--no-cache",
            "--json",
        ],
    );
    assert!(computed.success, "stderr: {}", computed.stderr);
    let report: serde_json::Value = serde_json::from_str(&computed.stdout).expect("valid JSON");
    assert_eq!(report["align_source"], "computed");
    for key in ["align_workers", "peak_align_workers"] {
        assert!(
            report[key].is_number(),
            "{key} must be a number when align_source == \"computed\": {report}"
        );
    }

    let skipped_out_dir = cwd.join("out_skipped");
    std::fs::create_dir_all(&skipped_out_dir).unwrap();
    let skipped = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            skipped_out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--json",
        ],
    );
    assert!(skipped.success, "stderr: {}", skipped.stderr);
    let report: serde_json::Value = serde_json::from_str(&skipped.stdout).expect("valid JSON");
    assert_eq!(report["align_source"], "skipped");
    for key in ["align_workers", "peak_align_workers"] {
        assert_eq!(
            report[key],
            serde_json::Value::Null,
            "{key} must be null under --no-align: {report}"
        );
    }
}

/// The instrumentation does not perturb the output: two runs of the same
/// fixture and parameters produce byte-identical composites.
#[test]
fn stage_timings_do_not_change_the_output() {
    let cwd = tmp_dir("stage_timings_do_not_change_the_output");
    let out_a = cwd.join("a");
    let out_b = cwd.join("b");
    for out in [&out_a, &out_b] {
        std::fs::create_dir_all(out).unwrap();
    }
    let args = |out: &Path| {
        vec![
            "stack".to_string(),
            mantis_512_dir().to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--algorithm".to_string(),
            "dummy-stack".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
            "--json".to_string(),
        ]
    };
    let a_args = args(&out_a);
    let b_args = args(&out_b);
    let a = run_cli_in(&cwd, &a_args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(a.success, "stderr: {}", a.stderr);
    let b = run_cli_in(&cwd, &b_args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(b.success, "stderr: {}", b.stderr);

    let (a_composite, _) = export_both(&cwd, &out_a.join("mantis-512.ffp"));
    let (b_composite, _) = export_both(&cwd, &out_b.join("mantis-512.ffp"));
    assert_eq!(
        std::fs::read(&a_composite).unwrap(),
        std::fs::read(&b_composite).unwrap(),
        "stage timing must not perturb the composite the run writes"
    );
}

/// A **failed seal** leaves neither a project at the named path nor a
/// `.partial` beside it, and removes its staging directory.
///
/// A read-only `-o` directory passes the checks before the run (it *is*
/// a directory), lets fusion finish, and only fails when sealing tries
/// to create the container's `.partial`.
#[cfg(unix)]
#[test]
fn failed_seal_leaves_no_output() {
    let Some(out_dir) = common::readonly_dir("failed_seal_leaves_no_output", 0o555, |_| {}) else {
        return;
    };
    let cwd = tmp_dir("failed_seal_leaves_no_output");
    let output = out_dir.join("mantis-512.ffp");

    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );

    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(!output.exists(), "no project may survive a failed seal");
    assert!(
        !partial_of(&output).exists(),
        "no .partial may survive a failed seal"
    );
    assert!(
        std::fs::read_dir(&*out_dir).unwrap().next().is_none(),
        "the failed seal left something behind in the output directory"
    );
}

#[test]
fn refuses_a_non_uniform_frame_set() {
    // `reference.tif` and `reference-MM-rps8.tif` are pixel-identical and
    // differ only in byte order and RowsPerStrip -- exactly the two fields
    // `check_uniform` deliberately does not compare (crates/core/src/io/tiff/
    // stack.rs). A genuine mismatch needs a frame that differs in a field
    // the check does cover, so this writes one at a different resolution
    // rather than reusing that pair.
    let mantis_64 = common::fixtures_dir().join("mantis-64");
    let cwd = tmp_dir("refuses_a_non_uniform_frame_set");
    let frames = cwd.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    std::fs::copy(
        mantis_64.join("reference.tif"),
        frames.join("reference.tif"),
    )
    .unwrap();

    let outlier = frames.join("outlier.tif");
    let mut writer = TiffWriter::create(
        &outlier,
        TiffSpec {
            width: 32,
            height: 32,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .expect("outlier writer must create");
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
            &vec![0u16; 32 * 32 * 3],
        )
        .expect("outlier writer must accept its one tile");
    writer.finish().expect("outlier writer must finish");

    let output = frames.join("mantis-512.ffp");
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            frames.to_str().unwrap(),
            "-o",
            frames.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
        ],
    );
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(
        out.stderr.contains("outlier.tif"),
        "must name the outlier: {}",
        out.stderr
    );
    assert!(!output.exists());
}

/// `reference.tif` (RowsPerStrip=1, II) and `reference-MM-rps8.tif`
/// (RowsPerStrip=8, MM) are pixel-identical; `stack` accepts the mixed set
/// and its composite matches two copies of `reference.tif` alone. Neither
/// fixture carries capture-time metadata, so both land in one group.
#[test]
fn mixed_byte_order_and_rows_per_strip() {
    let mantis_64 = common::fixtures_dir().join("mantis-64");
    let cwd = tmp_dir("mixed_byte_order_and_rows_per_strip");

    let mixed_dir = cwd.join("mixed");
    std::fs::create_dir_all(&mixed_dir).unwrap();
    std::fs::copy(
        mantis_64.join("reference.tif"),
        mixed_dir.join("reference.tif"),
    )
    .unwrap();
    std::fs::copy(
        mantis_64.join("reference-MM-rps8.tif"),
        mixed_dir.join("reference-MM-rps8.tif"),
    )
    .unwrap();

    let baseline_dir = cwd.join("baseline");
    std::fs::create_dir_all(&baseline_dir).unwrap();
    std::fs::copy(
        mantis_64.join("reference.tif"),
        baseline_dir.join("reference-a.tif"),
    )
    .unwrap();
    std::fs::copy(
        mantis_64.join("reference.tif"),
        baseline_dir.join("reference-b.tif"),
    )
    .unwrap();

    let find_project = |dir: &Path| -> PathBuf {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "ffp"))
            .expect("stack must write exactly one project")
    };

    let mixed_out = run_cli_in(
        &cwd,
        &[
            "stack",
            mixed_dir.to_str().unwrap(),
            "-o",
            mixed_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(
        mixed_out.success,
        "stdout: {} stderr: {}",
        mixed_out.stdout, mixed_out.stderr
    );

    let baseline_out = run_cli_in(
        &cwd,
        &[
            "stack",
            baseline_dir.to_str().unwrap(),
            "-o",
            baseline_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(
        baseline_out.success,
        "stdout: {} stderr: {}",
        baseline_out.stdout, baseline_out.stderr
    );

    let mixed_project = find_project(&mixed_dir);
    let baseline_project = find_project(&baseline_dir);
    let (mixed_composite, _) = export_both(&cwd, &mixed_project);
    let (baseline_composite, _) = export_both(&cwd, &baseline_project);
    let (mixed_samples, mw, mh, mspp) = read_whole(&mixed_composite);
    let (baseline_samples, bw, bh, bspp) = read_whole(&baseline_composite);

    assert_eq!((mw, mh, mspp), (bw, bh, bspp));
    assert_eq!(
        mixed_samples, baseline_samples,
        "mixed byte order/RowsPerStrip must not change the composite"
    );
}

/// `stack` computes transforms and warps frame tiles through them before
/// fusing: an aligned run must produce a different composite than the
/// unaligned one. `--align-algorithm pyramidal-ecc` is explicit because
/// this test binary also registers `dummy-align` (`test-algorithms`
/// feature), so a bare default would hit the "more than one alignment
/// algorithm" refusal -- default fallback because frame 0 of this fixture diverges
/// against the default middle anchor.
#[test]
fn stack_aligns_before_fusing() {
    let cwd = tmp_dir("stack_aligns_before_fusing");
    let unaligned_dir = cwd.join("unaligned");
    let aligned_dir = cwd.join("aligned");
    std::fs::create_dir_all(&unaligned_dir).unwrap();
    std::fs::create_dir_all(&aligned_dir).unwrap();
    let unaligned = unaligned_dir.join("mantis-512.ffp");
    let aligned = aligned_dir.join("mantis-512.ffp");

    let out_a = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            unaligned_dir.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out_a.success, "stderr: {}", out_a.stderr);

    let out_b = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            aligned_dir.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "pyramidal-ecc",
            "--no-crop",
        ],
    );
    assert!(out_b.success, "stderr: {}", out_b.stderr);

    let (unaligned_composite, _) = export_both(&cwd, &unaligned);
    let (aligned_composite, _) = export_both(&cwd, &aligned);
    let (unaligned_samples, ..) = read_whole(&unaligned_composite);
    let (aligned_samples, ..) = read_whole(&aligned_composite);
    assert_ne!(
        unaligned_samples, aligned_samples,
        "an aligned run must differ from the unaligned one"
    );
}

#[test]
fn registration_default_writes_a_project_and_strict_does_not() {
    let cwd = tmp_dir("registration_default_writes_a_project");
    let tolerant_dir = cwd.join("tolerant");
    let strict_dir = cwd.join("strict");
    let frames = mantis_512_dir();
    std::fs::create_dir_all(&tolerant_dir).unwrap();
    std::fs::create_dir_all(&strict_dir).unwrap();
    let common = [
        "stack",
        frames.to_str().unwrap(),
        "--algorithm",
        "dummy-stack",
        "--align-algorithm",
        "pyramidal-ecc",
        "--json",
    ];

    let mut tolerant = common.to_vec();
    tolerant.extend(["-o", tolerant_dir.to_str().unwrap()]);
    let out = run_cli_in(&cwd, &tolerant);
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("failed to register and fell back to identity")
    );
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert_eq!(report["align"]["failure_policy"], "lax");
    assert!(report["align"]["failure_count"].as_u64().unwrap() > 0);
    assert!(
        report["align"]["alignments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["fallback"] == true)
    );
    assert!(tolerant_dir.join("mantis-512.ffp").exists());

    let mut strict = common.to_vec();
    strict.extend(["-o", strict_dir.to_str().unwrap(), "--strict"]);
    let out = run_cli_in(&cwd, &strict);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("00.tif"), "stderr: {}", out.stderr);
    assert!(!strict_dir.join("mantis-512.ffp").exists());
}

#[test]
fn registration_cache_keeps_fallback_facts_and_warns_once_per_run() {
    let cwd = tmp_dir("registration_cache_keeps_fallback_facts");
    let frames = mantis_512_dir();
    let first_dir = cwd.join("first");
    let cached_dir = cwd.join("cached");
    let text_dir = cwd.join("text");
    for dir in [&first_dir, &cached_dir, &text_dir] {
        std::fs::create_dir(dir).unwrap();
    }
    let base = [
        "stack",
        frames.to_str().unwrap(),
        "--algorithm",
        "dummy-stack",
        "--align-algorithm",
        "pyramidal-ecc",
    ];

    let mut first = base.to_vec();
    first.extend(["-o", first_dir.to_str().unwrap(), "--json"]);
    let first = run_cli_in(&cwd, &first);
    assert!(first.success, "stderr: {}", first.stderr);
    let first_report: serde_json::Value = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(first_report["align_source"], "computed", "{first_report}");

    let mut cached = base.to_vec();
    cached.extend(["-o", cached_dir.to_str().unwrap(), "--json"]);
    let cached = run_cli_in(&cwd, &cached);
    assert!(cached.success, "stderr: {}", cached.stderr);
    let cached_report: serde_json::Value = serde_json::from_str(&cached.stdout).unwrap();
    assert_eq!(cached_report["align_source"], "cached", "{cached_report}");
    assert_eq!(
        cached_report["align"]["failure_count"],
        first_report["align"]["failure_count"]
    );
    assert_eq!(
        cached_report["align"]["alignments"],
        first_report["align"]["alignments"]
    );
    assert_eq!(
        cached
            .stderr
            .matches("failed to register and fell back to identity")
            .count(),
        1,
        "stderr: {}",
        cached.stderr
    );
    assert!(cached_dir.join("mantis-512.ffp").exists());

    let mut text = base.to_vec();
    text.extend(["-o", text_dir.to_str().unwrap()]);
    let text = run_cli_in(&cwd, &text);
    assert!(text.success, "stderr: {}", text.stderr);
    assert_eq!(
        text.stderr
            .matches("failed to register and fell back to identity")
            .count(),
        1,
        "stderr: {}",
        text.stderr
    );
    assert!(
        text.stdout.contains("dummy-stack"),
        "stdout: {}",
        text.stdout
    );
    assert!(text_dir.join("mantis-512.ffp").exists());
}

/// The auto-crop is applied to `stack`'s output by default: composite and
/// Z-Map are both declared at the crop rect's dimensions, and the rect
/// matches what `align --json` reports for the same stack.
#[test]
fn stack_output_carries_the_auto_crop() {
    let cwd = tmp_dir("stack_output_carries_the_auto_crop");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let project = out_dir.join("mantis-512.ffp");
    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "pyramidal-ecc",
            "--json",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    let crop = report["crop"]
        .as_object()
        .expect("crop object present when aligned");

    let (output, zmap) = export_both(&cwd, &project);
    let (_, cw, ch, _) = read_whole(&output);
    assert_eq!(cw as u64, crop["width"].as_u64().unwrap());
    assert_eq!(ch as u64, crop["height"].as_u64().unwrap());

    let (_, zw, zh, _) = read_whole(&zmap);
    assert_eq!(
        (zw, zh),
        (cw, ch),
        "the Z-Map must be declared at the crop rect's dimensions too"
    );

    let align_out = run_cli_in(
        &cwd,
        &[
            "align",
            mantis_512_dir().to_str().unwrap(),
            "--algorithm",
            "pyramidal-ecc",
            "--json",
        ],
    );
    assert!(align_out.success, "stderr: {}", align_out.stderr);
    let align_report: serde_json::Value =
        serde_json::from_str(&align_out.stdout).expect("valid JSON");
    assert_eq!(
        report["crop"], align_report["crop"],
        "stack's crop must match align's crop for the same stack"
    );
}

/// `-o` naming an existing **file** -- an input frame being the sharpest
/// case -- is refused by name, before the input directory is read.
#[test]
fn refuses_an_output_path_that_is_an_existing_file() {
    let cwd = tmp_dir("refuses_an_output_path_that_is_an_existing_file");
    let frame = mantis_512_dir().join("frames").join("00.tif");

    let out = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            frame.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
        ],
    );
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert_eq!(out.code, Some(1), "never clap's exit 2");
    assert!(
        out.stderr.contains("existing directory"),
        "stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("focusforge-cli export"),
        "a `.tif` -o path must name the replacement: {}",
        out.stderr
    );
    assert!(
        frame.metadata().unwrap().len() > 0,
        "the refusal must not have touched the frame"
    );
}

// ---------------------------------------------------------------------
// Brightness equalization (project parameters, method defaults and `--set`).
// ---------------------------------------------------------------------

/// An `algo` stack of the fixture, unaligned and uncropped, reported as
/// JSON, with `extra` arguments appended.
fn equalization_run(label: &str, algo: &str, extra: &[&str]) -> serde_json::Value {
    let cwd = tmp_dir(label);
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let input = mantis_512_dir();
    let mut args = vec![
        "stack",
        input.to_str().unwrap(),
        "-o",
        out_dir.to_str().unwrap(),
        "--algorithm",
        algo,
        "--json",
        "--no-align",
        "--no-crop",
    ];
    args.extend_from_slice(extra);
    let out = run_cli_in(&cwd, &common::push_no_gpu(args));
    assert!(out.success, "stderr: {}", out.stderr);
    serde_json::from_str(&out.stdout).expect("--json must parse")
}

#[test]
fn equalization_is_null_when_switched_off() {
    assert!(
        shared_laplacian_run()
            .report
            .get("equalization")
            .is_some_and(serde_json::Value::is_null),
        "the key is present and null"
    );
}

#[test]
fn equalize_set_enables_it_and_reports_every_frames_gain() {
    let report = equalization_run(
        "stack_cli_equalize_set",
        "weighted-average",
        &["--set", "equalize=true"],
    );
    let eq = &report["equalization"];
    assert_eq!(eq["max_gain"], 1.25);
    assert_eq!(eq["target_stops"], 0.0);
    let (neutral, target) = (
        eq["neutral"].as_f64().unwrap(),
        eq["target"].as_f64().unwrap(),
    );
    assert!(neutral > 0.0 && (neutral - target).abs() < 1e-9, "{eq}");
    let frames = eq["frames"].as_array().expect("one entry per frame");
    assert_eq!(
        frames.len(),
        report["frame_count"].as_u64().unwrap() as usize
    );
    for frame in frames {
        for key in [
            "mean",
            "estimated",
            "applied",
            "gain",
            "clamped",
            "black",
            "clipped_samples",
            "output_mean",
            "residual",
        ] {
            assert!(frame.get(key).is_some(), "{key} in {frame}");
        }
        assert_eq!(frame["clamped"], false, "{frame}");
        assert_eq!(frame["black"], false, "{frame}");
        let applied = frame["applied"].as_f64().unwrap();
        assert!((0.9..1.1).contains(&applied), "{frame}");
        let residual = frame["residual"].as_f64().unwrap();
        assert!(
            residual.abs() <= 1.0 || frame["clipped_samples"] != 0,
            "{frame}"
        );
    }
}

#[test]
fn equalize_preset_key_enables_it_and_write_preset_writes_it_back() {
    let cwd = tmp_dir("stack_cli_equalize_preset");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let written = cwd.join("written.toml");
    let dry = run_cli_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "weighted-average",
            "--dry-run",
            "--set",
            "equalize=true",
            "--set",
            "equalize-max-gain=1.5",
            "--write-preset",
            written.to_str().unwrap(),
        ],
    );
    assert!(dry.success, "stderr: {}", dry.stderr);
    let text = std::fs::read_to_string(&written).expect("the preset is written");
    assert!(text.contains("equalize = true"), "{text}");
    assert!(text.contains("equalize-max-gain = 1.5"), "{text}");

    let report = equalization_run(
        "stack_cli_equalize_preset_run",
        "weighted-average",
        &["--preset", written.to_str().unwrap()],
    );
    assert_eq!(report["equalization"]["max_gain"], 1.5);
}

#[test]
fn equalization_is_on_by_default_for_every_fusion_method() {
    for algo in ["depth-map", "weighted-average", "laplacian"] {
        let on = equalization_run(&format!("stack_cli_equalize_default_{algo}"), algo, &[]);
        assert_eq!(on["equalization"]["max_gain"], 1.25, "{algo}: {on}");
    }
}

#[test]
fn an_explicit_equalize_false_wins_over_the_default() {
    for algo in ["depth-map", "laplacian"] {
        let report = equalization_run(
            &format!("stack_cli_equalize_false_{algo}"),
            algo,
            &["--set", "equalize=false"],
        );
        assert!(report["equalization"].is_null(), "{algo}: {report}");
    }
}

/// A preset written by a run reproduces that run's equalization, whether it
/// followed the method's default (nothing is written) or overrode it.
#[test]
fn a_written_preset_reproduces_the_focus_map_equalization_choice() {
    let cwd = tmp_dir("stack_cli_equalize_preset_focus_map");
    let input = mantis_512_dir();
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    for (name, extra, equalized) in [
        ("default", vec![], true),
        ("off", vec!["--set", "equalize=false"], false),
    ] {
        let written = cwd.join(format!("{name}.toml"));
        let mut args = vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "depth-map",
            "--dry-run",
            "--write-preset",
            written.to_str().unwrap(),
        ];
        args.extend(extra);
        let dry = run_cli_in(&cwd, &args);
        assert!(dry.success, "{name}: stderr: {}", dry.stderr);
        let text = std::fs::read_to_string(&written).expect("the preset is written");
        assert_eq!(
            text.contains("equalize = false"),
            !equalized,
            "{name}: {text}"
        );

        let report = equalization_run(
            &format!("stack_cli_equalize_preset_focus_map_{name}"),
            "depth-map",
            &["--preset", written.to_str().unwrap()],
        );
        assert_eq!(
            report["equalization"].is_null(),
            !equalized,
            "{name}: {report}"
        );
    }
}

#[test]
fn equalize_settings_out_of_range_or_of_the_wrong_kind_are_refused_before_any_frame_is_read() {
    let missing_dir = "/definitely/does/not/exist/xyz-stack-cli-equalize";
    let cwd = tmp_dir("stack_cli_equalize_refused");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    for (setting, names) in [
        ("equalize-max-gain=3", ["equalize-max-gain", "3", "2"]),
        ("equalize=maybe", ["equalize", "maybe", "bool"]),
        ("equalize-target=0.6", ["equalize-target", "0.6", "0.5"]),
        (
            "equalize-target=-0.51",
            ["equalize-target", "-0.51", "-0.5"],
        ),
        (
            "equalize-target=NaN",
            ["equalize-target", "NaN", "equalize-target"],
        ),
        (
            "equalize-target=inf",
            ["equalize-target", "inf", "equalize-target"],
        ),
        (
            "equalize-target=bright",
            ["equalize-target", "bright", "equalize-target"],
        ),
    ] {
        let out = run_cli_in(
            &cwd,
            &common::push_no_gpu(vec![
                "stack",
                missing_dir,
                "-o",
                out_dir.to_str().unwrap(),
                "--algorithm",
                "weighted-average",
                "--set",
                setting,
            ]),
        );
        assert!(!out.success, "{setting}: stdout {}", out.stdout);
        for name in names {
            assert!(
                out.stderr.contains(name),
                "{setting}: {name} in {}",
                out.stderr
            );
        }
        assert!(!out.stderr.contains(missing_dir), "{}", out.stderr);
    }
}

/// 999 copies of one constant frame stack to a composite equal to that
/// constant: the Details residual mean stays exact at a large frame count.
#[test]
fn laplacian_constant_stack_round_trips() {
    const CONSTANT: [u16; 3] = [39835, 49858, 60138];
    let cwd = tmp_dir("stack_cli_constant_stack");
    let frames = cwd.join("frames");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&frames).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();

    let (width, height) = (128u32, 128u32);
    let first = frames.join("frame-000.tif");
    let samples: Vec<u16> = CONSTANT
        .iter()
        .copied()
        .cycle()
        .take((width * height * 3) as usize)
        .collect();
    let mut writer = TiffWriter::create(
        &first,
        TiffSpec {
            width,
            height,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .expect("constant frame writer must create");
    writer
        .write_tile(common::full_rect(width, height), &samples)
        .expect("constant frame writer must accept the full tile");
    writer.finish().expect("constant frame writer must finish");
    for index in 1..999 {
        std::fs::hard_link(&first, frames.join(format!("frame-{index:03}.tif"))).unwrap();
    }

    let out = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            frames.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--no-align",
            "--no-crop",
            "--set",
            "equalize=false",
        ]),
    );
    assert!(out.success, "stack stderr: {}", out.stderr);

    let (composite, _zmap) = export_both(&cwd, &out_dir.join("frame-000-998.ffp"));
    let (samples, w, h, spp) = read_whole(&composite);
    assert_eq!((w, h, spp), (width, height, 3));
    for (i, &sample) in samples.iter().enumerate() {
        assert_eq!(sample, CONSTANT[i % 3], "sample {i}");
    }
}
