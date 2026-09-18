// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli align`: the human report, the
//! frozen `--json` schema, both solver modes, `--write-warped`, and the
//! strict-versus-`--lax` exit policy.
//!
//! Every recipe here that reaches a real solve passes `--lax`: frame 0 of
//! `fixtures/mantis-512` genuinely diverges against the default middle
//! anchor, so the strict default would refuse the fixture.
//! `strict_path_exits_one_on_the_fixture` is the one test that keeps the
//! strict default itself exercised on real data.

mod common;

use std::path::PathBuf;
use std::process::Command;

fn bin() -> Command {
    common::cli_bin("align_cli")
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

#[test]
fn align_quiet_prints_no_progress() {
    let quiet = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--lax",
        "--quiet",
    ]);
    assert!(quiet.success, "stderr: {}", quiet.stderr);
    // `--quiet` suppresses the stage line, never the `--lax` failure-count
    // warning: that is a diagnostic about the result, not progress.
    assert!(
        !quiet.stderr.contains("frames (no unit progress)"),
        "stderr: {}",
        quiet.stderr
    );

    let loud = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--lax",
    ]);
    assert!(loud.success, "stderr: {}", loud.stderr);
    assert!(
        loud.stderr.contains("frames (no unit progress)"),
        "stderr: {}",
        loud.stderr
    );
    // `--quiet` must not change stdout, other than the trailing `stages:`
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
        "--quiet must not change stdout (aside from the timing-dependent stages: line)"
    );
}

#[test]
fn align_prints_a_stage_line_on_stderr() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--lax",
    ]);
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("align 8 frames (no unit progress)"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn align_reports_one_row_per_frame() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--lax",
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
        "--algo",
        "pyramidal-ecc",
        "--lax",
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
/// the frozen `align --json` report, additively, alongside every key
/// `align_json_schema_is_stable` already pins.
#[test]
fn worker_limit_in_report() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--lax",
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

/// Structural, not magnitude-based: `mode=translation` moves only 2 degrees
/// of freedom, so every frame decomposes to exactly unit scale and zero
/// rotation; `mode=rotation-scale` moves 4, so at least one converged frame
/// must show a non-unit scale.
#[test]
fn align_modes_differ() {
    let translation = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--set",
        "mode=translation",
        "--lax",
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
        "--algo",
        "pyramidal-ecc",
        "--set",
        "mode=rotation-scale",
        "--lax",
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
        "--algo",
        "pyramidal-ecc",
        "--lax",
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
        "--algo",
        "pyramidal-ecc",
        "--lax",
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
        "--algo",
        "pyramidal-ecc",
        "--lax",
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
        "--algo",
        "pyramidal-ecc",
        "--lax",
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

/// Strict is the default: a frame that fails to register exits 1 and names
/// every failed frame. Frame 0 of this fixture is the free, honest case.
#[test]
fn align_reports_a_failed_frame() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
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

/// The strict default, exercised on real data: the fixture exits 1.
#[test]
fn strict_path_exits_one_on_the_fixture() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
    ]);
    assert_eq!(
        out.code,
        Some(1),
        "strict (the default) must exit 1 on this fixture"
    );
}

#[test]
fn lax_reports_the_failure_and_exits_zero() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algo",
        "pyramidal-ecc",
        "--lax",
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
