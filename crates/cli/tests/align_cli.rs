// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli align`: the human report, the
//! `--json` report, both solver modes, `--write-warped`, and the
//! default-tolerant and strict exit policy.
//!
//! Frame 0 of `fixtures/mantis-512` genuinely diverges against the default
//! middle anchor. `strict_path_exits_one_on_the_fixture` exercises refusal.

mod common;

use std::process::Command;

use common::{CliOutput, mantis_512_dir, tmp_dir};

fn bin() -> Command {
    common::cli_bin("align_cli")
}

fn run_cli(args: &[&str]) -> CliOutput {
    bin()
        .args(args)
        .output()
        .expect("failed to run binary")
        .into()
}

#[test]
fn align_no_progress_suppresses_progress() {
    let quiet = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--no-progress",
    ]);
    assert!(quiet.success, "stderr: {}", quiet.stderr);
    // `--no-progress` suppresses the stage line, never the fallback-count
    // warning: that is a diagnostic about the result, not progress.
    assert!(
        !quiet.stderr.contains("align 8 frames"),
        "stderr: {}",
        quiet.stderr
    );

    let loud = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
    ]);
    assert!(loud.success, "stderr: {}", loud.stderr);
    assert!(
        loud.stderr.contains("align 8 frames"),
        "stderr: {}",
        loud.stderr
    );
    // `--no-progress` must not change stdout, other than the trailing `stages:`
    // line: it carries real per-run wall-clock figures, so two separately
    // timed runs of the same alignment never render it identically even
    // though every other line is deterministic.
    fn strip_stages(s: &str) -> Vec<&str> {
        s.lines()
            .filter(|line| !line.starts_with("stages:"))
            .collect()
    }
    assert_eq!(
        strip_stages(&quiet.stdout),
        strip_stages(&loud.stdout),
        "--no-progress must not change stdout (aside from the timing-dependent stages: line)"
    );
}

#[test]
fn align_prints_a_stage_line_on_stderr() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("align 8 frames"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn align_reports_one_row_per_frame() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);

    let lines: Vec<&str> = out.stdout.lines().collect();
    for i in 0..8 {
        let name = format!("{i:02}.tif");
        assert!(
            lines.iter().any(|l| l.contains(&name)),
            "missing a row for {name}: {}",
            out.stdout
        );
    }
    assert!(
        lines
            .iter()
            .any(|l| l.contains("03.tif") && l.contains("(reference)")),
        "frame 3 (the middle of 8) must be marked as the reference: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("auto-crop:"),
        "must print the crop rect on its own final line: {}",
        out.stdout
    );
    // Frame 0 also shows identity: it diverges against the middle anchor
    // and IdentityFallback substitutes identity, so there is more than one
    // identity row.
    assert!(
        out.stdout.contains("rho"),
        "must label the residual so it is not mistaken for a pixel distance: {}",
        out.stdout
    );
}

#[test]
fn align_json_schema_is_stable() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--json",
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
        "reference",
        "reference_policy",
        "failure_policy",
        "failure_count",
        "alignments",
        "crop",
        "warped_dir",
        "wall_seconds",
        "stages",
        "align_source",
        "align_workers",
        "peak_align_workers",
        "align_accounted_bytes",
    ] {
        assert!(report.get(key).is_some(), "missing key {key:?} in {report}");
    }

    assert_eq!(report["frame_count"], 8);
    assert_eq!(report["reference"], 3);
    assert_eq!(report["failure_policy"], "lax");
    assert!(report["failure_count"].as_u64().unwrap() >= 1);

    let alignments = report["alignments"].as_array().expect("alignments array");
    assert_eq!(alignments.len(), 8);
    for entry in alignments {
        for key in [
            "frame",
            "name",
            "transform",
            "status",
            "iterations",
            "residual",
            "fallback",
            "scale",
            "rotation_deg",
            "tx",
            "ty",
        ] {
            assert!(
                entry.get(key).is_some(),
                "missing key {key:?} in alignment entry {entry}"
            );
        }
        assert!(
            entry["transform"].get("m").is_some(),
            "transform must carry `m`: {entry}"
        );
    }

    let frame0 = &alignments[0];
    assert_eq!(
        frame0["status"], "diverged",
        "frame 0 must report its own divergence: {frame0}"
    );
    assert!(
        frame0.get("reason").is_some(),
        "a diverged status must carry `reason`: {frame0}"
    );
    assert_eq!(frame0["fallback"], true);

    assert!(
        report["crop"].is_object(),
        "crop must be a non-null object for this fixture"
    );
}

/// The anchor arm's worker bound and the peak it actually reached land on
/// the `align --json` report alongside every key
/// `align_json_schema_is_stable` already pins.
#[test]
fn worker_limit_in_report() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--json",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");

    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // 8-frame fixture: 7 registrations, so the bound is capped there on any
    // host with >= 7 cores.
    let ceiling = available.min(7) as u64;

    for key in ["align_workers", "peak_align_workers"] {
        let value = report[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key} must be a number: {report}"));
        assert!(
            (1..=ceiling).contains(&value),
            "{key}={value} must be in 1..={ceiling}: {report}"
        );
    }
}

/// `--max-host-memory` lowers the budget the alignment driver divides into
/// workers, and the report states the bytes it accounted for.
#[test]
fn max_host_memory_bounds_the_alignment_workers() {
    use focusforge_core::align::{bytes_per_registration, bytes_shared_reference};
    use focusforge_core::params::StackContext;
    use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;

    let stack = StackContext::new(8, 512, 512, 3);
    let shared = bytes_shared_reference(stack);
    let per_registration = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as u64;

    let workers_at = |cap: u64| {
        let out = run_cli(&[
            "align",
            mantis_512_dir().to_str().unwrap(),
            "--algorithm",
            "pyramidal-ecc",
            "--json",
            "--set",
            "max-iterations=20",
            "--set",
            "reference-policy=anchor",
            "--max-host-memory",
            &cap.to_string(),
        ]);
        assert!(out.success, "stderr: {}", out.stderr);
        let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
        let workers = report["align_workers"].as_u64().expect("align_workers");
        let accounted = report["align_accounted_bytes"]
            .as_u64()
            .expect("align_accounted_bytes");
        assert_eq!(accounted, shared + workers * per_registration, "{report}");
        workers
    };

    assert_eq!(workers_at(1), 1, "a floored budget runs one worker");
    assert_eq!(workers_at(shared + 2 * per_registration), available.min(2));
}

/// Structural, not magnitude-based: `mode=translation` moves only 2 degrees
/// of freedom, so every frame decomposes to exactly unit scale and zero
/// rotation; `mode=rotation-scale` moves 4, so at least one converged frame
/// must show a non-unit scale.
#[test]
fn align_modes_differ() {
    let translation = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--set",
        "mode=translation",
        "--json",
    ]);
    assert!(translation.success, "stderr: {}", translation.stderr);
    let t_report: serde_json::Value = serde_json::from_str(&translation.stdout).unwrap();
    for entry in t_report["alignments"].as_array().unwrap() {
        assert_eq!(
            entry["scale"].as_f64().unwrap(),
            1.0,
            "translation mode must decompose to exactly unit scale: {entry}"
        );
        assert_eq!(
            entry["rotation_deg"].as_f64().unwrap(),
            0.0,
            "translation mode must decompose to exactly zero rotation: {entry}"
        );
    }

    let rotation_scale = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--set",
        "mode=rotation-scale",
        "--json",
    ]);
    assert!(rotation_scale.success, "stderr: {}", rotation_scale.stderr);
    let rs_report: serde_json::Value = serde_json::from_str(&rotation_scale.stdout).unwrap();
    let converged_nonunit_scale = rs_report["alignments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["status"] == "converged" && e["scale"].as_f64().unwrap() != 1.0);
    assert!(
        converged_nonunit_scale,
        "rotation-scale mode must recover a non-unit scale for at least one converged frame: {rs_report}"
    );
}

#[test]
fn align_write_warped_writes_one_tiff_per_frame() {
    let dir = tmp_dir("align_write_warped_writes_one_tiff_per_frame");
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--write-warped",
        dir.to_str().unwrap(),
    ]);
    assert!(out.success, "stderr: {}", out.stderr);

    for i in 0..8 {
        let path = dir.join(format!("{i:02}.tif"));
        assert!(path.is_file(), "missing warped frame {}", path.display());
        let header = focusforge_core::io::TiffHeader::open(&path)
            .unwrap_or_else(|e| panic!("warped frame {} must be readable: {e}", path.display()));
        assert_eq!(
            (header.width, header.height),
            (512, 512),
            "warped frames are written at source dimensions, uncropped"
        );
    }
}

/// `align` never fuses: `fuse` is always `null`, and every run computes
/// (`align` has no `--resume`, no stage cache and no `--no-align`), so
/// `align_source` is always `"computed"`.
#[test]
fn stage_timings_report_fuse_as_not_entered() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--json",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert_eq!(
        report["stages"]["fuse"],
        serde_json::Value::Null,
        "align never fuses: {report}"
    );
    for key in ["setup", "align", "finalize"] {
        assert!(
            report["stages"][key].is_number(),
            "stages.{key} must be a number for an entered stage: {report}"
        );
    }
    assert_eq!(
        report["align_source"], "computed",
        "align always computes: {report}"
    );
}

/// `warp_write` reports `null` when `--write-warped` was not given -- "not
/// requested" must never read as "instant" -- and a number when it was.
#[test]
fn stage_timings_report_warp_write_only_under_the_flag() {
    let without = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--json",
    ]);
    assert!(without.success, "stderr: {}", without.stderr);
    let without_report: serde_json::Value = serde_json::from_str(&without.stdout).unwrap();
    assert_eq!(
        without_report["stages"]["warp_write"],
        serde_json::Value::Null,
        "no --write-warped, no mark: {without_report}"
    );

    let dir = tmp_dir("stage_timings_report_warp_write_only_under_the_flag");
    let with = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--write-warped",
        dir.to_str().unwrap(),
        "--json",
    ]);
    assert!(with.success, "stderr: {}", with.stderr);
    let with_report: serde_json::Value = serde_json::from_str(&with.stdout).unwrap();
    assert!(
        with_report["stages"]["warp_write"].is_number(),
        "--write-warped must produce a warp_write number: {with_report}"
    );
}

/// Strict opt-in refuses every failed frame and names it. Frame 0 of this
/// fixture is the free, honest case.
#[test]
fn align_reports_a_failed_frame() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--strict",
    ]);
    assert!(
        !out.success,
        "stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert_eq!(
        out.code,
        Some(1),
        "our own refusal exits 1, never clap's usage error (exit 2): stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("00.tif"),
        "must name the failed frame by file name: {}",
        out.stderr
    );
    // Per-frame outcomes are printed in both modes -- the report still
    // reaches stdout before the refusal.
    assert!(out.stdout.contains("00.tif"), "stdout: {}", out.stdout);
}

/// The strict opt-in, exercised on real data: the fixture exits 1.
#[test]
fn registration_strict_path_exits_one_on_the_fixture() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--strict",
    ]);
    assert_eq!(out.code, Some(1), "strict must exit 1 on this fixture");
}

#[test]
fn registration_default_reports_the_failure_and_exits_zero() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
    ]);
    assert!(out.success, "stdout: {} stderr: {}", out.stdout, out.stderr);
    assert_eq!(out.code, Some(0));
    assert!(
        out.stderr.contains("frame(s) failed to register"),
        "the failure count must be reported on stderr: {}",
        out.stderr
    );
    assert!(out.stdout.contains("00.tif"), "stdout: {}", out.stdout);
}
