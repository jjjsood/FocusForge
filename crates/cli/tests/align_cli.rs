// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli align`: the human report, the
//! `--json` report, `--write-warped`, the default-tolerant and strict exit
//! policy, and how `--set` and `--max-host-memory` reach the aligner.
//!
//! Four tests run the real aligner over `fixtures/mantis-512`, one per argv
//! shape; every other test gets its wiring from `dummy-align` or a synthetic
//! stack. Registered against the middle anchor, frames 0, 1 and 7 of the
//! fixture diverge and frame 6 fails validation: four identity fallbacks. The
//! default chain policy falls back on frames 0 and 1, and its fallback rows
//! keep the composed chain transform.

mod common;
#[path = "../../core/tests/common/raw_dng.rs"]
mod raw_dng;

use common::{CliOutput, bin, mantis_512_dir, tmp_dir};
use raw_dng::{DngSpec, write_dng};

const ANCHOR_FALLBACK_FRAMES: [u64; 4] = [0, 1, 6, 7];

fn run_cli(args: &[&str]) -> CliOutput {
    bin()
        .args(args)
        .output()
        .expect("failed to run binary")
        .into()
}

fn json_report(out: &CliOutput) -> serde_json::Value {
    assert!(out.success, "stderr: {}", out.stderr);
    serde_json::from_str(&out.stdout).expect("valid JSON")
}

/// The plain report against the middle anchor: one row per frame, the
/// reference marked, the four fallbacks shown as identity rows, and a
/// fallback-count warning that `--no-progress` leaves alone because it
/// describes the result.
#[test]
fn plain_report_marks_the_reference_and_the_identity_fallbacks() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--set",
        "reference-policy=anchor",
        "--no-progress",
    ]);
    assert!(out.success, "stdout: {} stderr: {}", out.stdout, out.stderr);
    assert_eq!(out.code, Some(0));
    assert!(
        out.stderr.contains("4 frame(s) failed to register"),
        "the failure count must be reported on stderr: {}",
        out.stderr
    );

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
    assert!(
        out.stdout.contains("rho"),
        "must label the residual so it is not mistaken for a pixel distance: {}",
        out.stdout
    );

    let rows: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.contains("tx="))
        .collect();
    let identity = |row: &str| row.contains("tx=+0.00 ty=+0.00 scale=1.000000 rot=+0.0000deg");
    let fallbacks: Vec<&str> = rows
        .iter()
        .copied()
        .filter(|r| r.contains("[fallback]"))
        .collect();
    assert_eq!(fallbacks.len(), 4, "fallback rows: {}", out.stdout);
    assert!(
        fallbacks.iter().all(|r| identity(r)),
        "a fallback row carries the identity transform: {}",
        out.stdout
    );
    assert_eq!(
        rows.iter().filter(|r| identity(r)).count(),
        5,
        "identity rows are the reference plus the four fallbacks: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("align 8 frames"),
        "--no-progress suppresses the stage line: {}",
        out.stderr
    );
}

/// The `--json` report: schema, the anchor policy's exact failure count, the
/// worker bound, and the stage timings an align run enters.
#[test]
fn json_report_pins_the_anchor_policys_four_failures() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--set",
        "reference-policy=anchor",
        "--json",
    ]);
    let report = json_report(&out);

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
    assert_eq!(report["reference_policy"], "anchor");
    assert_eq!(report["failure_policy"], "lax");
    assert_eq!(report["failure_count"], 4, "{report}");

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

    let fallback_frames: Vec<u64> = alignments
        .iter()
        .filter(|e| e["fallback"] == true)
        .map(|e| e["frame"].as_u64().expect("frame index"))
        .collect();
    assert_eq!(fallback_frames, ANCHOR_FALLBACK_FRAMES, "{report}");
    let frame0 = &alignments[0];
    assert_eq!(
        frame0["status"], "diverged",
        "frame 0 must report its own divergence: {frame0}"
    );
    assert!(
        frame0.get("reason").is_some(),
        "a diverged status must carry `reason`: {frame0}"
    );
    assert!(
        report["crop"].is_object(),
        "crop must be a non-null object for this fixture"
    );

    // 8-frame fixture: 7 registrations, so the bound is capped there on any
    // host with >= 7 cores.
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
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

    // `align` never fuses or develops, and it has no `--resume`, no stage
    // cache and no `--no-align`, so it always computes. `warp_write` is
    // `null` without `--write-warped`: "not requested" never reads as
    // "instant".
    for key in ["fuse", "develop", "warp_write"] {
        assert_eq!(
            report["stages"][key],
            serde_json::Value::Null,
            "stages.{key} is not entered by a plain align: {report}"
        );
    }
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

/// Strict refuses every failed frame and names it, exiting 1 -- our own
/// refusal, never clap's usage error (exit 2) -- after the per-frame report
/// has reached stdout.
#[test]
fn strict_refuses_the_fixtures_failed_frame_by_name() {
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--strict",
    ]);
    assert_eq!(
        out.code,
        Some(1),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stderr.contains("00.tif"),
        "must name the failed frame by file name: {}",
        out.stderr
    );
    assert!(out.stdout.contains("00.tif"), "stdout: {}", out.stdout);
}

/// `--write-warped` writes one uncropped, source-sized TIFF per frame and
/// the report then carries a `warp_write` timing.
#[test]
fn write_warped_writes_one_tiff_per_frame_and_times_it() {
    let dir = tmp_dir("align_write_warped_writes_one_tiff_per_frame");
    let out = run_cli(&[
        "align",
        mantis_512_dir().to_str().unwrap(),
        "--algorithm",
        "pyramidal-ecc",
        "--write-warped",
        dir.to_str().unwrap(),
        "--json",
    ]);
    let report = json_report(&out);
    assert!(
        report["stages"]["warp_write"].is_number(),
        "--write-warped must produce a warp_write number: {report}"
    );

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

/// `--set mode=<value>` reaches the aligner as an explicit parameter for each
/// mode it offers, and an unknown value is refused by parameter name. What
/// each mode computes is core's claim.
#[test]
fn set_mode_is_recorded_as_an_explicit_param_and_unknown_modes_are_refused() {
    let dir = tmp_dir("align_set_mode_provenance");
    common::write_synthetic_stack(&dir, 64, 64, &[(0.0, 0.0), (2.5, -1.5)]);
    let input = dir.to_str().unwrap();

    for mode in ["translation", "rotation-scale"] {
        let report = json_report(&run_cli(&[
            "align",
            input,
            "--algorithm",
            "pyramidal-ecc",
            "--set",
            &format!("mode={mode}"),
            "--json",
        ]));
        let entry = report["params"]
            .as_array()
            .expect("params array")
            .iter()
            .find(|p| p["id"] == "mode")
            .unwrap_or_else(|| panic!("no `mode` among the params: {report}"));
        assert_eq!(entry["value"], mode, "{report}");
        assert_eq!(entry["provenance"], "Explicit", "{report}");
    }

    let default = json_report(&run_cli(&[
        "align",
        input,
        "--algorithm",
        "pyramidal-ecc",
        "--json",
    ]));
    let default_mode = default["params"]
        .as_array()
        .expect("params array")
        .iter()
        .find(|p| p["id"] == "mode")
        .expect("`mode` among the params");
    assert_ne!(
        default_mode["provenance"], "Explicit",
        "an unset mode must not read as explicit: {default}"
    );

    let refused = run_cli(&[
        "align",
        input,
        "--algorithm",
        "pyramidal-ecc",
        "--set",
        "mode=skew",
    ]);
    assert!(!refused.success, "stdout: {}", refused.stdout);
    assert!(
        refused.stderr.contains("mode"),
        "the refusal names the parameter: {}",
        refused.stderr
    );
}

/// `align` reads RAW frames through the decoded-frame cache, which holds one
/// frame's decode reservation: a budget of two reservations aligns, one byte
/// under one refuses naming the frame.
#[test]
fn raw_align_at_capped_budget() {
    let (width, height) = (128, 128);
    let samples = (0..width * height)
        .map(|i| (1_000 + (i % width) * 8 + (i / width) * 5 + ((i / 3) % 7) * 40) as u16)
        .collect();
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], samples);
    spec.width = width;
    spec.height = height;
    let source = write_dng(&spec, false);
    let dir = tmp_dir("align_raw_capped_budget");
    for index in 0..2 {
        std::fs::copy(source.path(), dir.join(format!("{index:02}.dng"))).unwrap();
    }
    let reservation =
        u64::from(width * height) * 30 + std::fs::metadata(source.path()).unwrap().len();
    let align_at = |budget: u64| {
        run_cli(&[
            "align",
            dir.to_str().unwrap(),
            "--json",
            "--max-host-memory",
            &budget.to_string(),
        ])
    };

    let fits = align_at(2 * reservation);
    assert!(fits.success, "stderr: {}", fits.stderr);

    let refused = align_at(reservation - 1);
    assert!(!refused.success, "stdout: {}", refused.stdout);
    assert!(refused.stderr.contains(".dng"), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("MB of working memory"),
        "{}",
        refused.stderr
    );
}

/// `--max-host-memory` lowers the budget the alignment driver divides into
/// workers, and the report states the bytes it accounted for. The worker
/// budget does not depend on the algorithm.
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
            "dummy-align",
            "--json",
            "--max-host-memory",
            &cap.to_string(),
        ]);
        let report = json_report(&out);
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

/// `--no-progress` drops the stage line from stderr and changes nothing on
/// stdout but the timing-dependent `stages:` line.
#[test]
fn no_progress_suppresses_the_stage_line_and_leaves_stdout_alone() {
    let input = mantis_512_dir();
    let run = |extra: &[&str]| {
        let mut args = vec![
            "align",
            input.to_str().unwrap(),
            "--algorithm",
            "dummy-align",
        ];
        args.extend_from_slice(extra);
        let out = run_cli(&args);
        assert!(out.success, "stderr: {}", out.stderr);
        out
    };
    let quiet = run(&["--no-progress"]);
    let loud = run(&[]);

    assert!(
        !quiet.stderr.contains("align 8 frames"),
        "stderr: {}",
        quiet.stderr
    );
    assert!(
        loud.stderr.contains("align 8 frames"),
        "stderr: {}",
        loud.stderr
    );
    // The `stages:` line carries per-run wall-clock figures, so two timed
    // runs never render it identically; every other line is deterministic.
    fn strip_stages(s: &str) -> Vec<&str> {
        s.lines()
            .filter(|line| !line.starts_with("stages:"))
            .collect()
    }
    assert_eq!(strip_stages(&quiet.stdout), strip_stages(&loud.stdout));
}
