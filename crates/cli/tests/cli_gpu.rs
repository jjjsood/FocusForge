// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level GPU coverage of `focusforge-cli stack`/`align`: each test
//! here spawns `env!("CARGO_BIN_EXE_focusforge-cli")`
//! (`crates/cli/tests/stack_cli.rs`'s own pattern) with outputs under
//! `CARGO_TARGET_TMPDIR`. Every assertion is about a separate CLI
//! **process**, which brings up its own device in its own address space --
//! this test binary itself creates no device, so the one-device-per-process
//! rule holds without this file joining
//! `crates/gpu/tests/harness.rs`'s one-binary-one-device convention.
//!
//! Each test here loads the GPU (through the spawned process) and is meant
//! to be run deliberately, alongside the other `--features gpu-tests`
//! runs, not as part of a routine `cargo test` invocation.
//!
//! Every spawn isolates `.scratch/` with its own per-process temp directory
//! and `.current_dir(cwd)`, separate from any `-o` directory it points at.
//! Each `stack` run gets its own fresh `out_dir`; a test needing pixel
//! bytes asks `export` for them, since `stack` writes no composite file.

#![cfg(feature = "gpu-tests")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{CliOutput, mantis_512_dir, read_whole};

fn bin() -> Command {
    common::cli_bin("cli_gpu")
}

/// The largest absolute per-sample difference between two same-length
/// sample buffers.
fn max_abs_diff(a: &[u16], b: &[u16]) -> i32 {
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| (i32::from(x) - i32::from(y)).abs())
        .max()
        .unwrap_or(0)
}

fn run(dir: &Path, args: &[&str]) -> CliOutput {
    common::run_in(&mut bin(), dir, args)
}

/// Shared `stack` invocation for the `--max-vram` tests below: laplacian +
/// pyramidal-ecc, `--lax --json`, against the standard fixture -- `extra`
/// appends whatever each test varies (`--max-vram`, `--no-gpu`, ...).
/// `output_dir` is the directory `-o` collects this run's project into, not
/// a file path -- `stack`'s own contract.
fn max_vram_stack_args(output_dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "stack".to_string(),
        mantis_512_dir().to_str().unwrap().to_string(),
        "-o".to_string(),
        output_dir.to_str().unwrap().to_string(),
        "--algo".to_string(),
        "laplacian".to_string(),
        "--align-algo".to_string(),
        "pyramidal-ecc".to_string(),
        "--lax".to_string(),
        "--json".to_string(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

/// `run`, taking the owned `Vec<String>` `max_vram_stack_args` builds.
fn run_owned(dir: &Path, args: &[String]) -> CliOutput {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(dir, &refs)
}

/// A fresh subdirectory of `parent`, created for one `stack` run's own `-o`
/// -- `check_output_dir` refuses any `-o` that is not an already-existing
/// directory and never creates one itself.
fn out_dir(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).expect("test output directory must be creatable");
    dir
}

/// Arguments that plan four tiles of the 512x512 fixture, and the
/// `--max-vram` that admits one such tile on the device at a time, never two,
/// warped or not, while still covering the rect the run asks the device
/// about. `focusforge-gpu`'s `budget` tests pin the cap against the model.
const FOUR_TILE_ARGS: [&str; 4] = ["--tile-size", "576", "--set", "levels=5"];
const ONE_TILE_CAP: &str = "55MB";

/// `FOUR_TILE_ARGS` followed by `extra`.
fn four_tile_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    FOUR_TILE_ARGS
        .iter()
        .copied()
        .chain(extra.iter().copied())
        .collect()
}

/// The composite of `project`, exported beside it as
/// `<project-file-name>.composite.tif` for byte comparison.
fn export_composite(cwd: &Path, project: &Path) -> PathBuf {
    let composite = project.with_extension("composite.tif");
    let out = run(
        cwd,
        &[
            "export",
            project.to_str().unwrap(),
            "-o",
            composite.to_str().unwrap(),
        ],
    );
    assert!(out.success, "export stderr: {}", out.stderr);
    composite
}

/// A `stack` run against an absent WGPU backend (`dx12` does not exist on
/// Linux -- `Backends::from_env()`, `crates/gpu/src/device.rs`), so the
/// process-level fallback path is exercised without needing a machine that
/// genuinely lacks a GPU. Only meaningful on Linux: a non-Linux host prints
/// a skip instead of asserting a different platform's `WGPU_BACKEND`
/// vocabulary.
fn run_with_no_adapter(dir: &Path, args: &[&str]) -> Option<CliOutput> {
    if !cfg!(target_os = "linux") {
        println!("SKIP: WGPU_BACKEND=dx12 only names an absent backend on Linux");
        return None;
    }
    let output = bin()
        .args(args)
        .current_dir(dir)
        .env("WGPU_BACKEND", "dx12")
        .output()
        .expect("failed to run binary");
    Some(CliOutput::from(output))
}

/// A `stack` run with a usable adapter (no flag, the GPU path) must produce
/// a composite matching an explicit `--no-gpu` run on the same input.
///
/// This drives real `pyramidal-ecc` alignment over real `mantis-512`
/// frames, which produces near-tie composite disagreements far above 1 LSB
/// (real alignment, not GPU error); `STACK_TOLERANCE` is that measured bound.
#[test]
fn stack_gpu_matches_stack_no_gpu() {
    /// 512 LSB, measured on this fixture/algo/`--lax` combination: real
    /// alignment can flip a near-tied Laplacian-selection winner between GPU
    /// and CPU, and the runner-up can differ by hundreds of LSB.
    const STACK_TOLERANCE: i32 = 512;
    let cwd = common::tmp_dir("cli_gpu_stack_matches_cwd");
    let dir = common::tmp_dir("cli_gpu_stack_matches");
    let gpu_dir = out_dir(&dir, "gpu");
    let cpu_dir = out_dir(&dir, "cpu");

    let gpu = run(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            gpu_dir.to_str().unwrap(),
            "--algo",
            "laplacian",
            // Disambiguates the alignment algorithm: this test binary's
            // dev-dependency enables `focusforge-core`'s `test-algorithms`
            // feature, which registers `dummy-align` alongside the real
            // `pyramidal-ecc` -- the same reason `stack_cli.rs`/`align_cli.rs`
            // always pass one of `--align-algo`/`--no-align` explicitly.
            "--align-algo",
            "pyramidal-ecc",
            "--lax",
            "--json",
        ],
    );
    assert!(
        gpu.success,
        "gpu run must succeed -- stderr: {}",
        gpu.stderr
    );
    let gpu_report: serde_json::Value =
        serde_json::from_str(&gpu.stdout).expect("gpu run --json must parse");
    assert_ne!(
        gpu_report["backend"], "cpu",
        "a usable adapter must select the GPU path with no flag: {gpu_report}"
    );

    let cpu = run(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            cpu_dir.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--align-algo",
            "pyramidal-ecc",
            "--lax",
            "--json",
            "--no-gpu",
        ],
    );
    assert!(
        cpu.success,
        "cpu run must succeed -- stderr: {}",
        cpu.stderr
    );
    let cpu_report: serde_json::Value =
        serde_json::from_str(&cpu.stdout).expect("cpu run --json must parse");
    assert_eq!(cpu_report["backend"], "cpu", "report: {cpu_report}");

    let gpu_composite = export_composite(&cwd, &gpu_dir.join("mantis-512.ffp"));
    let cpu_composite = export_composite(&cwd, &cpu_dir.join("mantis-512.ffp"));
    let (gpu_samples, gw, gh, gspp) = read_whole(&gpu_composite);
    let (cpu_samples, cw, ch, cspp) = read_whole(&cpu_composite);
    assert_eq!(
        (gw, gh, gspp),
        (cw, ch, cspp),
        "gpu and cpu composites must have the same shape"
    );
    let worst = max_abs_diff(&gpu_samples, &cpu_samples);
    eprintln!("stack_gpu_matches_stack_no_gpu: worst composite delta {worst} LSB");
    assert!(
        worst <= STACK_TOLERANCE,
        "gpu and cpu composites differ by up to {worst} LSB, tolerance is {STACK_TOLERANCE}"
    );
}

/// `align --write-warped` with a usable adapter must produce the same
/// warped frames as an explicit `--no-gpu` run, to within 1 LSB at 16-bit.
///
/// Unlike `stack_gpu_matches_stack_no_gpu`, this is a pure warp comparison
/// with no fusion or Laplacian-selection winner involved, so the
/// per-kernel ≤1 LSB warp parity claim applies directly -- no near-tie
/// concern here.
#[test]
fn align_write_warped_gpu_matches_no_gpu() {
    const WARP_TOLERANCE: i32 = 1;
    let cwd = common::tmp_dir("cli_gpu_align_warped_cwd");
    let gpu_dir = common::tmp_dir("cli_gpu_align_warped_gpu");
    let cpu_dir = common::tmp_dir("cli_gpu_align_warped_cpu");

    let gpu = run(
        &cwd,
        &[
            "align",
            mantis_512_dir().to_str().unwrap(),
            // Disambiguates the alignment algorithm -- see the comment on
            // `stack_gpu_matches_stack_no_gpu`'s own `--align-algo`.
            "--algo",
            "pyramidal-ecc",
            "--lax",
            "--write-warped",
            gpu_dir.to_str().unwrap(),
        ],
    );
    assert!(
        gpu.success,
        "gpu align must succeed -- stderr: {}",
        gpu.stderr
    );

    let cpu = run(
        &cwd,
        &[
            "align",
            mantis_512_dir().to_str().unwrap(),
            "--algo",
            "pyramidal-ecc",
            "--lax",
            "--write-warped",
            cpu_dir.to_str().unwrap(),
            "--no-gpu",
        ],
    );
    assert!(
        cpu.success,
        "cpu align must succeed -- stderr: {}",
        cpu.stderr
    );

    let mut gpu_entries: Vec<_> = std::fs::read_dir(&*gpu_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let mut cpu_entries: Vec<_> = std::fs::read_dir(&*cpu_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    gpu_entries.sort();
    cpu_entries.sort();
    assert_eq!(
        gpu_entries, cpu_entries,
        "the same set of frames must be written"
    );

    for name in gpu_entries {
        let (gpu_samples, gw, gh, gspp) = read_whole(&gpu_dir.join(&name));
        let (cpu_samples, cw, ch, cspp) = read_whole(&cpu_dir.join(&name));
        assert_eq!(
            (gw, gh, gspp),
            (cw, ch, cspp),
            "{name:?}: gpu and cpu warped frames must have the same shape"
        );
        let worst = max_abs_diff(&gpu_samples, &cpu_samples);
        eprintln!("align_write_warped_gpu_matches_no_gpu: {name:?} worst delta {worst} LSB");
        assert!(
            worst <= WARP_TOLERANCE,
            "{name:?}: gpu and cpu warped frames differ by up to {worst} LSB, tolerance is {WARP_TOLERANCE}"
        );
    }
}

/// With no usable adapter, a plain `stack` run (no flag) falls back to the
/// CPU path, names the missing adapter and the fallback on stderr, and
/// still exits 0 with a correct result.
#[test]
fn auto_falls_back_without_adapter() {
    let cwd = common::tmp_dir("cli_gpu_auto_falls_back_cwd");
    let dir = common::tmp_dir("cli_gpu_auto_falls_back");
    let output = dir.join("mantis-512.ffp");
    let Some(result) = run_with_no_adapter(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            output.parent().unwrap().to_str().unwrap(),
            "--algo",
            "laplacian",
            "--align-algo",
            "pyramidal-ecc",
            "--lax",
            "--json",
        ],
    ) else {
        return;
    };
    assert!(
        result.success,
        "a missing adapter must fall back, not fail -- stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.to_lowercase().contains("adapter"),
        "stderr must name the missing adapter: {}",
        result.stderr
    );
    assert!(
        result.stderr.to_lowercase().contains("cpu"),
        "stderr must name the cpu fallback: {}",
        result.stderr
    );
    let report: serde_json::Value =
        serde_json::from_str(&result.stdout).expect("--json must parse");
    assert_eq!(report["backend"], "cpu", "report: {report}");
    assert!(output.exists(), "the composite must still be written");
}

/// The fallback is never silent: a run that produced a CPU result with no
/// usable adapter must have printed a message about it.
#[test]
fn fallback_emits_a_message() {
    let cwd = common::tmp_dir("cli_gpu_fallback_emits_a_message_cwd");
    let dir = common::tmp_dir("cli_gpu_fallback_emits_a_message");
    let output = dir.join("mantis-512.ffp");
    let Some(result) = run_with_no_adapter(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            output.parent().unwrap().to_str().unwrap(),
            "--algo",
            "laplacian",
            "--align-algo",
            "pyramidal-ecc",
            "--lax",
            "--json",
        ],
    ) else {
        return;
    };
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(
        !result.stderr.trim().is_empty(),
        "a CPU result produced with no usable adapter must not be silent"
    );
}

/// `--no-gpu` forces the CPU path even when an adapter is available, and
/// never even attempts (let alone reports) adapter selection.
#[test]
fn no_gpu_flag_forces_cpu_even_with_adapter() {
    let cwd = common::tmp_dir("cli_gpu_no_gpu_forces_cpu_cwd");
    let dir = common::tmp_dir("cli_gpu_no_gpu_forces_cpu");
    let output = dir.join("mantis-512.ffp");
    let result = run(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            output.parent().unwrap().to_str().unwrap(),
            "--algo",
            "laplacian",
            "--align-algo",
            "pyramidal-ecc",
            "--lax",
            "--json",
            "--no-gpu",
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    let report: serde_json::Value =
        serde_json::from_str(&result.stdout).expect("--json must parse");
    assert_eq!(report["backend"], "cpu", "report: {report}");
    assert!(
        result.stderr.contains("--no-gpu"),
        "stderr must name --no-gpu as the reason: {}",
        result.stderr
    );
    assert!(
        !result.stderr.to_lowercase().contains("adapter selected"),
        "a forced CPU run must never report an adapter as selected: {}",
        result.stderr
    );
}

/// `--max-vram` is a ceiling only: a value below the probed budget lowers it
/// and reports `Explicit`; a value above it changes nothing, including its
/// provenance. `stack --json` reports both `max_device_bytes_in_flight` and
/// `device_budget_provenance` on the GPU path, and `null` for both on the
/// CPU path.
#[test]
fn max_vram_lowers_never_raises_and_json_reports_both_fields() {
    let cwd = common::tmp_dir("cli_gpu_max_vram_cwd");
    let dir = common::tmp_dir("cli_gpu_max_vram");

    let probed_dir = out_dir(&dir, "probed");
    let probed = run_owned(&cwd, &max_vram_stack_args(&probed_dir, &[]));
    assert!(probed.success, "stderr: {}", probed.stderr);
    let probed_report: serde_json::Value =
        serde_json::from_str(&probed.stdout).expect("--json must parse");
    if probed_report["backend"] == "cpu" {
        println!("SKIP: no usable adapter on this machine, cannot exercise --max-vram");
        return;
    }
    let probed_bytes = probed_report["max_device_bytes_in_flight"]
        .as_u64()
        .expect("GPU path must report a non-null max_device_bytes_in_flight");
    assert_ne!(
        probed_report["device_budget_provenance"],
        serde_json::Value::Null,
        "GPU path must report a non-null device_budget_provenance: {probed_report}"
    );

    // A ceiling well below the probed value lowers it and reports Explicit.
    let ceiling: u64 = 256 << 20; // 256MiB -- the probe's own floor.
    assert!(
        ceiling < probed_bytes,
        "fixture assumption: probed budget ({probed_bytes}) must exceed the 256MiB ceiling used below"
    );
    let low_dir = out_dir(&dir, "low");
    let low = run_owned(
        &cwd,
        &max_vram_stack_args(&low_dir, &["--max-vram", "256MiB"]),
    );
    assert!(low.success, "stderr: {}", low.stderr);
    let low_report: serde_json::Value =
        serde_json::from_str(&low.stdout).expect("--json must parse");
    assert_eq!(
        low_report["max_device_bytes_in_flight"], ceiling,
        "report: {low_report}"
    );
    assert_eq!(
        low_report["device_budget_provenance"], "Explicit",
        "report: {low_report}"
    );

    // A ceiling above the probed value changes nothing, provenance included.
    let high_dir = out_dir(&dir, "high");
    let high = run_owned(
        &cwd,
        &max_vram_stack_args(&high_dir, &["--max-vram", "64GiB"]),
    );
    assert!(high.success, "stderr: {}", high.stderr);
    let high_report: serde_json::Value =
        serde_json::from_str(&high.stdout).expect("--json must parse");
    assert_eq!(
        high_report["max_device_bytes_in_flight"], probed_report["max_device_bytes_in_flight"],
        "a ceiling above the probed value must not change it: {high_report}"
    );
    assert_eq!(
        high_report["device_budget_provenance"], probed_report["device_budget_provenance"],
        "a ceiling above the probed value must not change its provenance: {high_report}"
    );

    // CPU path: both fields null.
    let cpu_dir = out_dir(&dir, "cpu");
    let cpu = run_owned(&cwd, &max_vram_stack_args(&cpu_dir, &["--no-gpu"]));
    assert!(cpu.success, "stderr: {}", cpu.stderr);
    let cpu_report: serde_json::Value =
        serde_json::from_str(&cpu.stdout).expect("--json must parse");
    assert_eq!(
        cpu_report["max_device_bytes_in_flight"],
        serde_json::Value::Null,
        "report: {cpu_report}"
    );
    assert_eq!(
        cpu_report["device_budget_provenance"],
        serde_json::Value::Null,
        "report: {cpu_report}"
    );
}

/// `device_peak_in_flight` is the device budget's own high-water tile count
/// for this run -- distinct from `peak_in_flight`, the host scheduler's own
/// count, which `--max-vram` cannot move. `null` on the CPU path.
#[test]
fn device_peak_in_flight_is_the_device_gates_own_high_water() {
    let cwd = common::tmp_dir("cli_gpu_device_peak_in_flight_cwd");
    let dir = common::tmp_dir("cli_gpu_device_peak_in_flight");

    // Uncapped GPU run.
    let uncapped_dir = out_dir(&dir, "uncapped");
    let uncapped = run_owned(
        &cwd,
        &max_vram_stack_args(&uncapped_dir, &four_tile_args(&[])),
    );
    assert!(uncapped.success, "stderr: {}", uncapped.stderr);
    let uncapped_report: serde_json::Value =
        serde_json::from_str(&uncapped.stdout).expect("--json must parse");
    if uncapped_report["backend"] == "cpu" {
        println!("SKIP: no usable adapter on this machine, cannot exercise device_peak_in_flight");
        return;
    }
    let uncapped_peak = uncapped_report["device_peak_in_flight"]
        .as_u64()
        .expect("GPU path must report a non-null device_peak_in_flight");
    assert!(uncapped_peak >= 1, "report: {uncapped_report}");
    let host_peak = uncapped_report["peak_in_flight"]
        .as_u64()
        .expect("report must carry peak_in_flight");
    assert!(
        uncapped_peak <= host_peak,
        "device concurrency must never exceed host concurrency: {uncapped_report}"
    );

    // A cap between one and two tiles' device bytes admits the run to the
    // device one tile at a time, on any machine, without touching the
    // host-side `in_flight_limit`.
    let capped_dir = out_dir(&dir, "capped");
    let capped = run_owned(
        &cwd,
        &max_vram_stack_args(&capped_dir, &four_tile_args(&["--max-vram", ONE_TILE_CAP])),
    );
    assert!(capped.success, "stderr: {}", capped.stderr);
    let capped_report: serde_json::Value =
        serde_json::from_str(&capped.stdout).expect("--json must parse");
    assert_eq!(
        capped_report["device_peak_in_flight"], 1,
        "report: {capped_report}"
    );
    assert_eq!(
        capped_report["in_flight_limit"], uncapped_report["in_flight_limit"],
        "--max-vram must not move the host scheduler's own limit: {capped_report}"
    );

    // `--no-gpu`: the field is null.
    let cpu_dir = out_dir(&dir, "cpu");
    let cpu = run_owned(
        &cwd,
        &max_vram_stack_args(&cpu_dir, &four_tile_args(&["--no-gpu"])),
    );
    assert!(cpu.success, "stderr: {}", cpu.stderr);
    let cpu_report: serde_json::Value =
        serde_json::from_str(&cpu.stdout).expect("--json must parse");
    assert_eq!(
        cpu_report["device_peak_in_flight"],
        serde_json::Value::Null,
        "report: {cpu_report}"
    );
}

/// One tile above the device budget runs on the host, chosen at plan time: a
/// run capped at 1 MiB exits 0, reports the CPU backend and no device
/// figures, names the fallback on stderr and in `gpu_fallback`, and writes
/// the planes a `--no-gpu` run writes.
#[test]
fn a_device_budget_below_one_tile_falls_back_to_the_cpu_path() {
    let cwd = common::tmp_dir("cli_gpu_budget_fallback_cwd");
    let dir = common::tmp_dir("cli_gpu_budget_fallback");
    let capped_dir = out_dir(&dir, "capped");
    let cpu_dir = out_dir(&dir, "cpu");

    let capped = run_owned(
        &cwd,
        &max_vram_stack_args(&capped_dir, &["--max-vram", "1MiB"]),
    );
    assert!(capped.success, "stderr: {}", capped.stderr);
    if !capped.stderr.contains("using the GPU path") {
        println!("SKIP: no usable adapter on this machine, cannot exercise the device fallback");
        return;
    }
    let report: serde_json::Value =
        serde_json::from_str(&capped.stdout).expect("--json must parse");
    assert_eq!(report["backend"], "cpu", "report: {report}");
    for key in [
        "max_device_bytes_in_flight",
        "device_budget_provenance",
        "device_peak_in_flight",
    ] {
        assert_eq!(report[key], serde_json::Value::Null, "{key} in {report}");
    }
    let reason = report["gpu_fallback"]
        .as_str()
        .expect("a fallback run must name its reason");
    assert!(reason.contains("CPU path"), "reason: {reason}");
    assert!(
        capped.stderr.contains(reason),
        "stderr must carry the reason: {}",
        capped.stderr
    );

    let cpu = run_owned(&cwd, &max_vram_stack_args(&cpu_dir, &["--no-gpu"]));
    assert!(cpu.success, "cpu run stderr: {}", cpu.stderr);
    let cpu_report: serde_json::Value =
        serde_json::from_str(&cpu.stdout).expect("--json must parse");
    assert_eq!(
        cpu_report["gpu_fallback"],
        serde_json::Value::Null,
        "report: {cpu_report}"
    );

    let (capped_composite, capped_zmap) = export_both(&cwd, &capped_dir.join("mantis-512.ffp"));
    let (cpu_composite, cpu_zmap) = export_both(&cwd, &cpu_dir.join("mantis-512.ffp"));
    assert_eq!(
        std::fs::read(&capped_composite).expect("read capped composite"),
        std::fs::read(&cpu_composite).expect("read cpu composite"),
        "the fallback composite must equal a --no-gpu run's"
    );
    assert_eq!(
        std::fs::read(&capped_zmap).expect("read capped zmap"),
        std::fs::read(&cpu_zmap).expect("read cpu zmap"),
        "the fallback Z-Map must equal a --no-gpu run's"
    );
}

/// Builds a two-group `stack` root: `a-many-tiles/` (the 8 frames of
/// `fixtures/mantis-512/frames`, several tiles at the tile size the tests
/// pass) and `b-one-tile/` (the 3 frames of `fixtures/mantis-64/frames`, one
/// tile at any tile size). No frame carries a capture time, so `propose`
/// groups by directory, and its depth-first bytewise walk yields exactly
/// these two groups, `a-many-tiles` first.
fn build_two_group_root(parent: &Path) -> PathBuf {
    let root = parent.join("in");
    let mantis_64 = mantis_512_dir().with_file_name("mantis-64");
    for (name, source) in [
        ("a-many-tiles", mantis_512_dir()),
        ("b-one-tile", mantis_64),
    ] {
        let group = root.join(name);
        std::fs::create_dir_all(&group).expect("group directory must be creatable");
        for entry in std::fs::read_dir(source.join("frames")).expect("fixture frames dir") {
            let src = entry.expect("fixture frame entry").path();
            if src.extension().and_then(|e| e.to_str()) == Some("tif") {
                let name = src.file_name().expect("fixture frame must have a name");
                std::fs::copy(&src, group.join(name)).expect("frame copy must succeed");
            }
        }
    }
    root
}

/// Two groups in one `stack` invocation report independent device peaks, not
/// a running maximum: the many-tile group admits two or more tiles to the
/// device at once, then the one-tile group, which can never have two in
/// flight, reports exactly 1.
#[test]
fn device_peak_in_flight_is_scoped_to_its_own_group() {
    let cwd = common::tmp_dir("cli_gpu_device_peak_two_group_cwd");
    let in_holder = common::tmp_dir("cli_gpu_device_peak_two_group_in");
    let out_holder = common::tmp_dir("cli_gpu_device_peak_two_group_out");
    let root = build_two_group_root(&in_holder);
    let out = out_dir(&out_holder, "out");

    let output = run(
        &cwd,
        &[
            "stack",
            root.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-align",
            "--no-crop",
            "--tile-size",
            "448",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let doc: serde_json::Value = serde_json::from_str(&output.stdout).expect("--json must parse");
    assert_eq!(doc["format"], 1, "doc: {doc}");
    let groups = doc["groups"].as_array().expect("groups must be an array");

    if groups[0]["backend"] == "cpu" {
        println!("SKIP: no usable adapter on this machine, cannot exercise device_peak_in_flight");
        return;
    }
    if groups[0]["in_flight_limit"].as_u64().unwrap_or(0) < 2 {
        println!(
            "SKIP: host scheduler caps this machine at one in-flight tile, no per-group difference is observable"
        );
        return;
    }

    assert_eq!(groups.len(), 2, "doc: {doc}");
    let many_peak = groups[0]["device_peak_in_flight"]
        .as_u64()
        .expect("the many-tile group must report a non-null device_peak_in_flight");
    let one_peak = groups[1]["device_peak_in_flight"]
        .as_u64()
        .expect("the one-tile group must report a non-null device_peak_in_flight");
    assert!(
        many_peak >= 2,
        "the many-tile group must admit two tiles at once: {doc}"
    );
    assert_eq!(
        one_peak, 1,
        "the one-tile group's own peak, not a running maximum: {doc}"
    );
}

/// The JSON Lines log a spawned run writes under its own data directory.
fn log_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join("focusforge")
        .join("logs")
        .join("focusforge.jsonl")
}

/// Both payloads of `project`, exported beside it in one invocation.
fn export_both(cwd: &Path, project: &Path) -> (PathBuf, PathBuf) {
    let composite = project.with_extension("composite.tif");
    let zmap = project.with_extension("zmap.tif");
    let out = run(
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

/// `stack` over the standard fixture with alignment off and a tile size
/// small enough to plan several tiles -- the shape both sides of the
/// device-loss comparison run with. Alignment off means identity transforms
/// on both sides, so the per-kernel one-LSB claim and the exact-Z-Map claim
/// apply directly instead of through real alignment's near-tie
/// disagreements.
fn device_loss_stack_args(output_dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "stack".to_string(),
        mantis_512_dir().to_str().unwrap().to_string(),
        "-o".to_string(),
        output_dir.to_str().unwrap().to_string(),
        "--algo".to_string(),
        "laplacian".to_string(),
        "--no-align".to_string(),
        // Four tiles, so destroying the device after the first leaves work
        // for the host.
        "--json".to_string(),
    ];
    args.extend(FOUR_TILE_ARGS.iter().map(|s| s.to_string()));
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

/// A device destroyed after the first fused tile finishes the run on the
/// host: exit 0, the loss and its reason in the log, the frame tiles the
/// host path actually read reported, and both planes equal to a host-only
/// run's.
#[test]
fn device_lost_falls_back() {
    let cwd = common::tmp_dir("cli_gpu_device_lost_cwd");
    let dir = common::tmp_dir("cli_gpu_device_lost");
    let gpu_dir = out_dir(&dir, "gpu");
    let cpu_dir = out_dir(&dir, "cpu");

    let mut cmd = common::cli_bin("cli_gpu_device_lost");
    let data_dir = common::data_dir_of(&cmd);
    // A device budget between one and two tiles' footprints keeps one tile
    // in flight, so the first tile is genuinely finished -- and the device
    // genuinely destroyed -- before the second is dispatched. Without it all
    // four tiles are in flight at once and the destroy lands after the last
    // of them has already been fused.
    let gpu_args = device_loss_stack_args(&gpu_dir, &["--max-vram", ONE_TILE_CAP]);
    let gpu = CliOutput::from(
        cmd.args(gpu_args.iter().map(String::as_str))
            .current_dir(&*cwd)
            .env("FF_FORCE_DEVICE_LOST", "1")
            .output()
            .expect("failed to run binary"),
    );
    assert!(
        gpu.success,
        "a lost device must still finish the run -- stderr: {}",
        gpu.stderr
    );
    let gpu_report: serde_json::Value =
        serde_json::from_str(&gpu.stdout).expect("gpu run --json must parse");
    assert_ne!(
        gpu_report["backend"], "cpu",
        "the run must have started on the GPU path: {gpu_report}"
    );
    assert!(
        gpu_report["frame_tiles_read"]
            .as_u64()
            .is_some_and(|read| read > 0),
        "the tiles finished on the host must be counted: {gpu_report}"
    );

    let log = std::fs::read_to_string(log_path(&data_dir)).expect("the run must write a log");
    assert!(
        log.contains("graphics device lost"),
        "the log must name the device loss: {log}"
    );
    assert!(
        log.contains("Destroyed"),
        "the log must carry the loss's reason: {log}"
    );

    let cpu = run_owned(&cwd, &device_loss_stack_args(&cpu_dir, &["--no-gpu"]));
    assert!(cpu.success, "cpu run stderr: {}", cpu.stderr);

    let (gpu_composite, gpu_zmap) = export_both(&cwd, &gpu_dir.join("mantis-512.ffp"));
    let (cpu_composite, cpu_zmap) = export_both(&cwd, &cpu_dir.join("mantis-512.ffp"));

    let (gpu_samples, gpu_w, gpu_h, gpu_spp) = read_whole(&gpu_composite);
    let (cpu_samples, cpu_w, cpu_h, cpu_spp) = read_whole(&cpu_composite);
    assert_eq!((gpu_w, gpu_h, gpu_spp), (cpu_w, cpu_h, cpu_spp));
    let diff = max_abs_diff(&gpu_samples, &cpu_samples);
    assert!(
        diff <= 1,
        "a run finished on the host must stay within 1 LSB of a host-only run, got {diff}"
    );

    assert_eq!(
        std::fs::read(&gpu_zmap).expect("read gpu zmap"),
        std::fs::read(&cpu_zmap).expect("read cpu zmap"),
        "the Z-Map must be identical on both sides"
    );
}

/// The adapter and its limits are recorded once per process, whatever the
/// number of stacks, and neither record reaches stderr -- the one
/// `adapter selected: ` line stays the whole of a run's adapter output
/// there.
#[test]
fn adapter_logged_once() {
    let cwd = common::tmp_dir("cli_gpu_adapter_logged_cwd");
    let in_holder = common::tmp_dir("cli_gpu_adapter_logged_in");
    let out_holder = common::tmp_dir("cli_gpu_adapter_logged_out");
    let root = build_two_group_root(&in_holder);
    let out = out_dir(&out_holder, "out");

    let mut cmd = common::cli_bin("cli_gpu_adapter_logged");
    let data_dir = common::data_dir_of(&cmd);
    let output = CliOutput::from(
        cmd.args([
            "stack",
            root.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-align",
            "--lax",
            "--json",
        ])
        .current_dir(&*cwd)
        .output()
        .expect("failed to run binary"),
    );
    assert!(output.success, "stderr: {}", output.stderr);

    let log = std::fs::read_to_string(log_path(&data_dir)).expect("the run must write a log");
    assert_eq!(
        log.matches("graphics adapter selected").count(),
        1,
        "a two-group batch must record the adapter exactly once: {log}"
    );
    assert_eq!(
        log.matches("graphics adapter and device limits").count(),
        1,
        "a two-group batch must record the limits exactly once: {log}"
    );
    assert_eq!(
        output.stderr.matches("adapter selected: ").count(),
        1,
        "stderr must still carry exactly one adapter line: {}",
        output.stderr
    );
}
