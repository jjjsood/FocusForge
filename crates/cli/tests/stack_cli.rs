// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli stack`: a successful run, every
//! rejection path, the Z-Map switch, the report's fields and the
//! library-versus-CLI agreement.
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
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
}

fn mantis_512_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-512")
}

fn tmp_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = tmp_path(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Both payloads of `project`, exported beside it as `<stem>-composite.tif`
/// and `<stem>-zmap.tif`.
///
/// `stack` writes a project, so every test that needs the *files* asks
/// `export` for them. One invocation emits both, so the container is opened
/// once.
fn export_both(project: &Path) -> (PathBuf, PathBuf) {
    let composite = project.with_extension("composite.tif");
    let zmap = project.with_extension("zmap.tif");
    let out = run_cli(&[
        "export",
        project.to_str().unwrap(),
        "-o",
        composite.to_str().unwrap(),
        "--zmap",
        zmap.to_str().unwrap(),
    ]);
    assert!(out.success, "export stderr: {}", out.stderr);
    (composite, zmap)
}

fn partial_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn read_whole(path: &Path) -> (Vec<u16>, u32, u32, u32) {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).expect("output must reopen");
    let (w, h) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let samples = stack
        .read_tile(0, full_rect(w, h))
        .expect("read must succeed")
        .samples;
    (samples, w, h, spp)
}

struct CliOutput {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cli(args: &[&str]) -> CliOutput {
    let output = bin().args(args).output().expect("failed to run binary");
    CliOutput {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The one `laplacian` run driven through the built binary: `-o`, `--zmap`
/// and no `--set`, so `levels` resolves through its `auto` hook. Shared by
/// every test that needs its output, its Z-Map or its report.
///
/// The run itself writes a **project**; `output` and `zmap` are what one
/// `export` call then brings back out as files, so every downstream
/// assertion below is unchanged by the move to a container and the ~50 s
/// fusion is still paid exactly once.
struct SharedRun {
    /// The `.ffproj` the run wrote -- the thing `stack` now produces.
    #[allow(dead_code)]
    project: PathBuf,
    output: PathBuf,
    zmap: PathBuf,
    report: serde_json::Value,
}

fn shared_laplacian_run() -> &'static SharedRun {
    static ONCE: OnceLock<SharedRun> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = tmp_dir("stack_cli_shared_laplacian");
        let project = dir.join("mantis-512.ffproj");
        let output = dir.join("out.tif");
        let zmap = dir.join("out-zmap.tif");
        let mut args = vec![
            "stack".to_string(),
            mantis_512_dir().to_str().unwrap().to_string(),
            "-o".to_string(),
            dir.to_str().unwrap().to_string(),
            "--algo".to_string(),
            "laplacian".to_string(),
            "--json".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ];
        // Verifies CLI plumbing reproduces `library_oracle`'s in-process,
        // CPU-only call exactly, not GPU-vs-CPU numerics (that's the
        // whole-pipeline `parity_pipeline::*`'s job). Without `--no-gpu`,
        // `stack --algo laplacian` takes the GPU path on any adapter-equipped
        // machine, breaking this test's exact-byte equality via the ±1 LSB
        // GPU/CPU budget; the flag only exists when `gpu` is compiled in.
        if cfg!(feature = "gpu") {
            args.push("--no-gpu".to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_cli(&args);
        assert!(
            result.success,
            "shared laplacian run must succeed -- stderr: {}",
            result.stderr
        );
        let report: serde_json::Value =
            serde_json::from_str(&result.stdout).expect("--json must parse");

        // One `export`, emitting both payloads from the one container the
        // run wrote. That is the whole migration: the bytes are the same,
        // they just come out one command later.
        let exported = run_cli(&[
            "export",
            project.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--zmap",
            zmap.to_str().unwrap(),
        ]);
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
        let levels = algo.tile_levels(&values);

        let dir = tmp_dir("stack_cli_oracle");
        let composite_path = dir.join("oracle.tif");
        let zmap_path = dir.join("oracle-zmap.tif");
        let options = TileSchedulerOptions {
            tile_size: 512,
            levels,
            max_bytes_in_flight: 1 << 30,
            scratch_base: tmp_path("stack_cli_oracle_scratch"),
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
        "the CLI composite must equal a library run driven at the CLI's own geometry"
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
    let output = tmp_path("unknown_algorithm_is_rejected_early.tif");
    let out = run_cli(&[
        "stack",
        missing_dir,
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "no-such-algorithm",
    ]);
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
    let output = tmp_path("unknown_parameter_key_is_rejected.tif");
    let out = run_cli(&[
        "stack",
        missing_dir,
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "dummy-stack",
        "--set",
        "bogus=1",
    ]);
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
    let output = tmp_path("out_of_range_parameter_is_rejected.tif");
    let out = run_cli(&[
        "stack",
        missing_dir,
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "laplacian",
        "--set",
        "levels=999",
    ]);
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

/// `levels=7` is in schema range (`[1, 16]`) but deeper than the shipped
/// 512 px tile can plan (margin sized for 5): `laplacian::fuse` and the
/// scheduler's margin must agree, so this must reach
/// `TileError::NoInterior`, naming the tile size that would work, before
/// any tile is read.
#[test]
fn refuses_levels_the_tile_geometry_cannot_plan() {
    let output = tmp_path("refuses_levels_the_tile_geometry_cannot_plan.tif");
    let zmap = tmp_path("refuses_levels_the_tile_geometry_cannot_plan-zmap.tif");
    let _ = std::fs::remove_file(&output);
    let _ = std::fs::remove_file(&zmap);
    let out = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "laplacian",
        "--set",
        "levels=7",
        "--no-align",
        "--no-crop",
    ]);
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

    let dir = tmp_dir("writes_the_zmap_on_request_off");
    let output = dir.join("mantis-512.ffproj");
    let out = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "dummy-stack",
        "--no-align",
        "--no-crop",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);
    let entries: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec!["mantis-512.ffproj".to_string()],
        "one project and nothing else -- no image, no partial: {entries:?}"
    );
    let (_, zmap) = export_both(&output);
    let (_, zw, zh, zspp) = read_whole(&zmap);
    assert_eq!(
        (zw, zh, zspp),
        (512, 512, 1),
        "the map is one index per pixel"
    );
}

#[test]
fn refuses_empty_dir_and_unwritable_output() {
    let empty_dir = tmp_dir("refuses_empty_dir_and_unwritable_output_empty");
    let output_a = tmp_path("refuses_empty_dir_and_unwritable_output_a.tif");
    let _ = std::fs::remove_file(&output_a);
    let out = run_cli(&[
        "stack",
        empty_dir.to_str().unwrap(),
        "-o",
        output_a.parent().unwrap().to_str().unwrap(),
        "--algo",
        "dummy-stack",
    ]);
    assert!(!out.success, "an empty directory must be refused");
    assert!(!output_a.exists());

    let missing_dir = "/definitely/does/not/exist/xyz-stack-cli-unwritable-output";
    let bad_output = PathBuf::from("/definitely/does/not/exist/xyz-output-parent/out.tif");
    let out = run_cli(&[
        "stack",
        missing_dir,
        "-o",
        bad_output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "dummy-stack",
    ]);
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
    let dir = tmp_dir("report_contains_every_field");
    let output = dir.join("mantis-512.ffproj");
    let out = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "dummy-stack",
        "--no-align",
        "--no-crop",
    ]);
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
        "wrote:",
        "wall:",
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
    let output = tmp_path("algo_is_required.tif");
    let out = run_cli(&["stack", missing_dir, "-o", output.to_str().unwrap()]);
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
        out.stderr.contains("--algo is required"),
        "stderr: {}",
        out.stderr
    );
    assert!(out.stderr.contains("dummy-stack"), "stderr: {}", out.stderr);
    assert!(!out.stderr.contains(missing_dir), "stderr: {}", out.stderr);
}

#[test]
fn report_json_has_every_field() {
    let dir = tmp_dir("report_json_has_every_field");
    let output = dir.join("mantis-512.ffproj");
    let out = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "dummy-stack",
        "--json",
        "--no-align",
        "--no-crop",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    for key in [
        "input",
        "frame_count",
        "frames",
        "width",
        "height",
        "samples_per_pixel",
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
        "outputs",
        "wall_seconds",
    ] {
        assert!(report.get(key).is_some(), "missing key {key:?} in {report}");
    }
    let params = report["params"].as_array().expect("params array");
    assert!(!params.is_empty());
    for p in params {
        for key in ["id", "value", "provenance"] {
            assert!(p.get(key).is_some(), "missing key {key:?} in param {p}");
        }
    }
}

/// A **failed seal** leaves neither a project at the named path nor a
/// `.partial` beside it, and removes its staging directory.
///
/// A read-only `-o` directory passes the checks before the run (it *is*
/// a directory), lets fusion finish, and only fails when sealing tries
/// to create the container's `.partial`.
#[test]
fn failed_seal_leaves_no_output() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tmp_dir("failed_seal_leaves_no_output");
    let out_dir = dir.join("readonly");
    std::fs::create_dir_all(&out_dir).unwrap();
    let output = out_dir.join("mantis-512.ffproj");
    std::fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let out = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        out_dir.to_str().unwrap(),
        "--algo",
        "dummy-stack",
        "--no-align",
        "--no-crop",
    ]);
    // Restore before asserting, so a failed assertion still leaves a
    // removable directory behind.
    std::fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

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
        std::fs::read_dir(&out_dir).unwrap().next().is_none(),
        "the failed seal left something behind in the output directory"
    );
}

#[test]
fn refuses_a_non_uniform_frame_set() {
    let mantis_64 = common::fixtures_dir().join("mantis-64");
    let dir = tmp_dir("refuses_a_non_uniform_frame_set");
    std::fs::copy(mantis_64.join("reference.tif"), dir.join("reference.tif")).unwrap();
    std::fs::copy(
        mantis_64.join("reference-MM-rps8.tif"),
        dir.join("reference-MM-rps8.tif"),
    )
    .unwrap();

    let output = dir.join("mantis-512.ffproj");
    let out = run_cli(&[
        "stack",
        dir.to_str().unwrap(),
        "-o",
        output.parent().unwrap().to_str().unwrap(),
        "--algo",
        "dummy-stack",
    ]);
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert!(
        out.stderr.contains("reference-MM-rps8.tif"),
        "must name the outlier: {}",
        out.stderr
    );
    assert!(!output.exists());
}

/// `stack` computes transforms and warps frame tiles through them before
/// fusing: an aligned run must produce a different composite than the
/// unaligned one. `--align-algo pyramidal-ecc --lax` is explicit because
/// this test binary also registers `dummy-align` (`test-algorithms`
/// feature), so a bare default would hit the "more than one alignment
/// algorithm" refusal -- `--lax` because frame 0 of this fixture diverges
/// against the default middle anchor.
#[test]
fn stack_aligns_before_fusing() {
    let dir = tmp_dir("stack_aligns_before_fusing");
    let unaligned_dir = dir.join("unaligned");
    let aligned_dir = dir.join("aligned");
    std::fs::create_dir_all(&unaligned_dir).unwrap();
    std::fs::create_dir_all(&aligned_dir).unwrap();
    let unaligned = unaligned_dir.join("mantis-512.ffproj");
    let aligned = aligned_dir.join("mantis-512.ffproj");

    let out_a = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        unaligned.parent().unwrap().to_str().unwrap(),
        "--algo",
        "laplacian",
        "--no-align",
        "--no-crop",
    ]);
    assert!(out_a.success, "stderr: {}", out_a.stderr);

    let out_b = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        aligned.parent().unwrap().to_str().unwrap(),
        "--algo",
        "laplacian",
        "--align-algo",
        "pyramidal-ecc",
        "--lax",
        "--no-crop",
    ]);
    assert!(out_b.success, "stderr: {}", out_b.stderr);

    let (unaligned_composite, _) = export_both(&unaligned);
    let (aligned_composite, _) = export_both(&aligned);
    let (unaligned_samples, ..) = read_whole(&unaligned_composite);
    let (aligned_samples, ..) = read_whole(&aligned_composite);
    assert_ne!(
        unaligned_samples, aligned_samples,
        "an aligned run must differ from the unaligned one"
    );
}

/// The auto-crop is applied to `stack`'s output by default: composite and
/// Z-Map are both declared at the crop rect's dimensions, and the rect
/// matches what `align --json` reports for the same stack.
#[test]
fn stack_output_carries_the_auto_crop() {
    let dir = tmp_dir("stack_output_carries_the_auto_crop");
    let project = dir.join("mantis-512.ffproj");
    let out = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        dir.to_str().unwrap(),
        "--algo",
        "laplacian",
        "--align-algo",
        "pyramidal-ecc",
        "--lax",
        "--json",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    let crop = report["crop"]
        .as_object()
        .expect("crop object present when aligned");

    let (output, zmap) = export_both(&project);
    let (_, cw, ch, _) = read_whole(&output);
    assert_eq!(cw as u64, crop["width"].as_u64().unwrap());
    assert_eq!(ch as u64, crop["height"].as_u64().unwrap());

    let (_, zw, zh, _) = read_whole(&zmap);
    assert_eq!(
        (zw, zh),
        (cw, ch),
        "the Z-Map must be declared at the crop rect's dimensions too"
    );

    let align_out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--lax",
        "--json",
    ]);
    assert!(align_out.success, "stderr: {}", align_out.stderr);
    let align_report: serde_json::Value =
        serde_json::from_str(&align_out.stdout).expect("valid JSON");
    assert_eq!(
        report["crop"], align_report["crop"],
        "stack's crop must match align's crop for the same stack"
    );
}

/// `-o` naming an existing **file** -- an input frame being the sharpest
/// case -- is refused by name, with the migration sentence, before the
/// input directory is read.
///
/// The old shape of this test pointed `--zmap` at an input frame; with the
/// flag gone the reachable case is `-o`, and the composite-collides-with-an-
/// input check stays in `run_group` as the defensive backstop it now is.
#[test]
fn refuses_an_output_path_that_is_an_existing_file() {
    let frame = mantis_512_dir().join("frames").join("00.tif");

    let out = run_cli(&[
        "stack",
        mantis_512_dir().to_str().unwrap(),
        "-o",
        frame.to_str().unwrap(),
        "--algo",
        "dummy-stack",
    ]);
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
