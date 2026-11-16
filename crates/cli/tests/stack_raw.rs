// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

#[path = "../../core/tests/common/raw_dng.rs"]
mod raw_dng;

use std::process::Command;

#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::process::Stdio;

use focusforge_core::algorithm::TileReader;
use focusforge_core::io::{RawStack, Rect, TiffStack};
use raw_dng::{DngSpec, write_dng};

/// A little-endian classic TIFF with its `DateTime` value (tag 306, the export
/// time) zeroed, so two exports made a second apart compare equal.
#[cfg(unix)]
fn without_date_time(mut tiff: Vec<u8>) -> Vec<u8> {
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes(b[at..at + 2].try_into().expect("u16"));
    let u32_at =
        |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().expect("u32")) as usize;
    let ifd = u32_at(&tiff, 4);
    for entry in 0..usize::from(u16_at(&tiff, ifd)) {
        let at = ifd + 2 + entry * 12;
        if u16_at(&tiff, at) == 306 {
            let (count, value) = (u32_at(&tiff, at + 4), u32_at(&tiff, at + 8));
            assert!(count > 4, "DateTime is stored out of line");
            tiff[value..value + count].fill(0);
        }
    }
    tiff
}

fn run(dir: &std::path::Path, args: &[String]) -> std::process::Output {
    common::cli_bin("stack_raw")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run CLI")
}

#[test]
fn cache_refusal_message_states_the_limit_in_mb() {
    let cwd = common::tmp_dir("stack_raw_cache_refusal");
    let mut args = vec![
        "stack".to_string(),
        common::fixtures_dir()
            .join("blackmagic-micro-dng")
            .display()
            .to_string(),
        "-o".to_string(),
        cwd.display().to_string(),
        "--algorithm".to_string(),
        "laplacian".to_string(),
        "--no-align".to_string(),
        "--no-crop".to_string(),
        "--max-host-memory".to_string(),
        "1".to_string(),
    ];
    if cfg!(feature = "gpu") {
        args.push("--cpu".to_string());
    }
    let output = run(&cwd, &args);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("frame.dng"), "{stderr}");
    assert!(stderr.contains("MB of working memory"), "{stderr}");
    assert!(stderr.contains("memory limit allows 0 MB"), "{stderr}");
    assert!(
        stderr.contains("Raise the memory limit to at least"),
        "{stderr}"
    );
    assert!(!stderr.contains("bytes"), "{stderr}");
    assert!(!stderr.contains("cache bound"), "{stderr}");
}

#[test]
fn raw_dry_run_reports_the_run_budget() {
    let input = common::tmp_dir("stack_raw_dry_run_budget");
    let cwd = common::tmp_dir("stack_raw_dry_run_budget_cwd");
    let source = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    for index in 0..2 {
        std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
            .expect("copy RAW frame");
    }
    let reservation = 8 * 8 * 30 + std::fs::metadata(source.path()).expect("DNG size").len();
    let dry_run = |budget: u64| {
        let mut args = vec![
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            cwd.display().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
            "--dry-run".to_string(),
            "--json".to_string(),
            "--max-host-memory".to_string(),
            budget.to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
        }
        run(&cwd, &args)
    };

    let budget = 2 * reservation;
    let output = dry_run(budget);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the dry run prints JSON");
    assert_eq!(plan["max_bytes_in_flight"], budget, "{plan}");
    assert!(
        matches!(
            plan["developed_store"].as_str(),
            Some("memory" | "scratch_file")
        ),
        "a RAW dry run develops: {plan}"
    );

    let refused = dry_run(reservation - 1);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("MB of working memory"), "{stderr}");
    assert!(stderr.contains(".dng"), "{stderr}");
}

#[test]
fn identical_raw_frames_stack_to_the_frame() {
    let cwd = common::tmp_dir("stack_raw_identical");
    let source = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    for count in [3, 8] {
        let input = cwd.join(format!("input-{count}"));
        let output = cwd.join(format!("output-{count}"));
        std::fs::create_dir(&input).expect("input directory");
        std::fs::create_dir(&output).expect("output directory");
        for index in 0..count {
            std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
                .expect("copy RAW frame");
        }

        let mut args = vec![
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
        }
        let stacked = run(&cwd, &args);
        assert!(
            stacked.status.success(),
            "stack stderr: {}",
            String::from_utf8_lossy(&stacked.stderr)
        );
        let project = output.join(format!("input-{count}.ffp"));
        assert!(project.is_file());
        let composite = output.join("composite.tif");
        let exported = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args([
                "export",
                project.to_str().expect("UTF-8 project"),
                "-o",
                composite.to_str().expect("UTF-8 composite"),
            ])
            .output()
            .expect("run export");
        assert!(
            exported.status.success(),
            "export stderr: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
        let output_stack = TiffStack::open_files(&[composite]).expect("exported composite");
        let (width, height) = output_stack.dimensions();
        assert_eq!(
            output_stack.header(0).bits_per_sample.as_ref(),
            &[16, 16, 16]
        );
        let composite = output_stack
            .read_tile(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            )
            .expect("read composite")
            .samples;
        let source_reader = RawStack::open_files(&[input.join("00.dng")]).expect("source RAW");
        let mut developed = vec![0; composite.len()];
        source_reader
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                &mut developed,
            )
            .expect("develop source RAW");
        let max_diff = composite
            .iter()
            .zip(&developed)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        eprintln!("raw stack frames={count} max_diff={max_diff}");
        assert!(max_diff <= 1, "{count} frames: max difference {max_diff}");

        let review = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args(["review", output.to_str().expect("UTF-8 output"), "--json"])
            .output()
            .expect("run review");
        assert!(review.status.success());
        assert!(String::from_utf8_lossy(&review.stdout).contains("\"outcome\": \"completed\""));
    }
}

#[test]
fn non_uniform_raw_group_is_refused_by_name() {
    let cwd = common::tmp_dir("stack_raw_non_uniform");
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&output).expect("output directory");
    let first = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    std::fs::copy(first.path(), input.join("00.dng")).expect("copy first RAW");
    let cases = [
        (
            "CFA",
            DngSpec::new(12, 64, 4095, [2, 1, 1, 0], vec![512; 64]),
        ),
        ("camera", {
            let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]);
            spec.model = "Different Bayer".to_string();
            spec
        }),
        ("dimensions", {
            let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]);
            spec.crop = Some([0, 0, 4, 4]);
            spec
        }),
    ];
    for (property, spec) in cases {
        let second = write_dng(&spec, false);
        std::fs::copy(second.path(), input.join("01.dng")).expect("copy second RAW");
        let result = run(
            &cwd,
            &[
                "stack".to_string(),
                input.display().to_string(),
                "-o".to_string(),
                output.display().to_string(),
                "--algorithm".to_string(),
                "dummy-stack".to_string(),
                "--no-align".to_string(),
                "--no-crop".to_string(),
            ],
        );
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("01.dng") && stderr.contains(&format!("RAW {property}")),
            "{stderr}"
        );
    }
}

#[test]
fn forced_mixed_group_is_refused_by_name() {
    let cwd = common::tmp_dir("stack_raw_mixed");
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&output).expect("output directory");
    let raw = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    std::fs::copy(raw.path(), input.join("00.dng")).expect("copy RAW");
    std::fs::copy(
        common::fixtures_dir().join("mantis-64/frames/00.tif"),
        input.join("01.tif"),
    )
    .expect("copy TIFF");
    let groups = cwd.join("mixed.toml");
    std::fs::write(
        &groups,
        "[[group]]\nname = \"mixed\"\nframes = [\"00.dng\", \"01.tif\"]\n",
    )
    .expect("write group file");
    let result = run(
        &cwd,
        &[
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--groups".to_string(),
            groups.display().to_string(),
            "--algorithm".to_string(),
            "dummy-stack".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ],
    );
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("01.tif") && stderr.contains("TIFF"),
        "{stderr}"
    );
}

#[test]
fn info_and_stack_refuse_a_directory_with_only_prior_tiff_outputs() {
    let cwd = common::tmp_dir("stack_raw_prior_outputs");
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&output).expect("output directory");
    for name in ["composite.tif", "zmap.tif"] {
        std::fs::write(input.join(name), b"prior output").expect("write output marker");
    }

    let info = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
        .current_dir(&*cwd)
        .args(["info", input.to_str().expect("UTF-8 input"), "--json"])
        .output()
        .expect("run info");
    assert!(!info.status.success());
    assert!(
        String::from_utf8_lossy(&info.stderr).contains("frames found"),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );

    let stacked = run(
        &cwd,
        &[
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--algorithm".to_string(),
            "dummy-stack".to_string(),
        ],
    );
    assert!(!stacked.status.success());
    assert!(String::from_utf8_lossy(&stacked.stderr).contains("frames found"));
}

/// `stack --json --no-align --no-crop --algorithm laplacian` over `input`.
fn stack_json_args(
    input: &std::path::Path,
    output: &std::path::Path,
    extra: &[&str],
) -> Vec<String> {
    let mut args: Vec<String> = [
        "stack",
        input.to_str().expect("UTF-8 input"),
        "-o",
        output.to_str().expect("UTF-8 output"),
        "--json",
        "--no-align",
        "--no-crop",
        "--algorithm",
        "laplacian",
    ]
    .into_iter()
    .chain(extra.iter().copied())
    .map(str::to_string)
    .collect();
    if cfg!(feature = "gpu") {
        args.push("--cpu".to_string());
    }
    args
}

fn develop_counts_of(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stack stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    report
        .get("develop_counts")
        .expect("develop_counts is always present")
        .clone()
}

/// Plain-mode progress text and `stack --json` timings of a RAW run: the
/// develop pass is announced and counted before any fuse line, and its
/// timing key joins the sum.
#[test]
fn raw_stack_reports_the_develop_stage() {
    let cwd = common::tmp_dir("stack_raw_develop_stage");
    let source = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    for count in [3usize, 6] {
        let input = cwd.join(format!("input-{count}"));
        let output = cwd.join(format!("output-{count}"));
        std::fs::create_dir(&input).expect("input directory");
        std::fs::create_dir(&output).expect("output directory");
        for index in 0..count {
            std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
                .expect("copy RAW frame");
        }
        let stacked = run(&cwd, &stack_json_args(&input, &output, &[]));
        assert!(
            stacked.status.success(),
            "stack stderr: {}",
            String::from_utf8_lossy(&stacked.stderr)
        );
        let stderr = String::from_utf8_lossy(&stacked.stderr).into_owned();
        let position = |needle: &str| {
            stderr
                .find(needle)
                .unwrap_or_else(|| panic!("no {needle:?} in: {stderr}"))
        };
        let entry = position(&format!("develop {count} frames (RAW)"));
        let first = position(&format!("develop 0/{count} frames"));
        assert!(entry < first, "entry line precedes the count: {stderr}");
        assert!(
            first < position("fuse "),
            "the develop line precedes every fuse line: {stderr}"
        );

        let report: serde_json::Value =
            serde_json::from_slice(&stacked.stdout).expect("JSON report");
        let stages = &report["stages"];
        assert!(stages["develop"].is_number(), "{report}");
        let sum: f64 = [
            "setup",
            "develop",
            "align",
            "fuse",
            "warp_write",
            "finalize",
        ]
        .iter()
        .filter_map(|key| stages[key].as_f64())
        .sum();
        let wall = report["wall_seconds"].as_f64().expect("wall_seconds");
        assert!(
            (sum - wall).abs() <= 1e-6,
            "sum {sum} vs wall {wall}: {report}"
        );

        let human_output = cwd.join(format!("human-{count}"));
        std::fs::create_dir(&human_output).expect("output directory");
        let mut human_args = stack_json_args(&input, &human_output, &[]);
        human_args.retain(|arg| arg != "--json");
        let human = run(&cwd, &human_args);
        assert!(
            human.status.success(),
            "stack stderr: {}",
            String::from_utf8_lossy(&human.stderr)
        );
        let text = String::from_utf8_lossy(&human.stdout);
        let stages_line = text
            .lines()
            .find(|line| line.starts_with("stages:"))
            .unwrap_or_else(|| panic!("no stages line in: {text}"));
        assert!(stages_line.contains("develop="), "{stages_line}");
    }
}

#[test]
fn raw_develop_counts_are_one_per_frame() {
    let cwd = common::tmp_dir("stack_raw_develop_counts");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], Vec::new());
    spec.width = 256;
    spec.height = 256;
    spec.samples = (0..256u32 * 256)
        .map(|i| (((i % 256) + (i / 256)) * 8 % 4000) as u16)
        .collect();
    let source = write_dng(&spec, false);
    for count in [3usize, 6] {
        let input = cwd.join(format!("input-{count}"));
        std::fs::create_dir(&input).expect("input directory");
        for index in 0..count {
            std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
                .expect("copy RAW frame");
        }
        let output = cwd.join(format!("roomy-{count}"));
        std::fs::create_dir(&output).expect("output directory");
        let roomy = run(&cwd, &stack_json_args(&input, &output, &[]));
        let counts = develop_counts_of(&roomy);
        assert_eq!(counts, serde_json::json!(vec![1; count]), "no ceiling");

        let output = cwd.join(format!("tight-{count}"));
        std::fs::create_dir(&output).expect("output directory");
        let tight = run(
            &cwd,
            &stack_json_args(
                &input,
                &output,
                &[
                    "--tile-size",
                    "160",
                    "--set",
                    "levels=3",
                    "--max-host-memory",
                    "12582912",
                ],
            ),
        );
        let counts = develop_counts_of(&tight);
        assert_eq!(
            counts,
            serde_json::json!(vec![1; count]),
            "a ceiling below the stack develops each frame once"
        );
    }
}

/// Repair acts on the developed frame: each masked pixel of the result is the
/// lower median of its eight neighbours in the unrepaired run, and the
/// develops per frame do not change.
#[test]
fn raw_stack_repairs_after_develop() {
    let cwd = common::tmp_dir("stack_raw_repair");
    let input = cwd.join("input");
    std::fs::create_dir(&input).expect("input directory");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], Vec::new());
    spec.width = 256;
    spec.height = 256;
    spec.samples = (0..256u32 * 256)
        .map(|i| {
            ((i % 256) * 9 + (i / 256) * 5 + i.wrapping_mul(2_654_435_761) % 300) as u16 % 4000
        })
        .collect();
    let source = write_dng(&spec, false);
    for index in 0..2 {
        std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
            .expect("copy RAW frame");
    }
    let masked = [(40u32, 30u32), (100, 128), (200, 220), (130, 60)];
    let mask = cwd.join("mask.png");
    common::write_defect_mask(&mask, (256, 256), &masked);

    let stack = |label: &str, extra: &[&str]| {
        let output = cwd.join(label);
        std::fs::create_dir(&output).expect("output directory");
        let stacked = run(&cwd, &stack_json_args(&input, &output, extra));
        let counts = develop_counts_of(&stacked);
        let project = common::only_project(&output);
        (
            counts,
            common::read_whole(&common::export_composite(&cwd, &project)),
        )
    };
    let (off_counts, (off, width, _, spp)) = stack("off", &[]);
    let (on_counts, (on, ..)) = stack(
        "on",
        &[
            "--set",
            "defect-removal=true",
            "--set",
            "hot-pixels=false",
            "--defect-mask",
            mask.to_str().expect("UTF-8 mask"),
        ],
    );
    assert_eq!(on_counts, off_counts, "repair develops nothing extra");

    let mut changed = 0;
    for &(x, y) in &masked {
        for c in 0..spp {
            let at = |x: u32, y: u32| off[((y * width + x) * spp + c) as usize];
            let mut around: Vec<u16> = (y - 1..=y + 1)
                .flat_map(|ny| (x - 1..=x + 1).map(move |nx| (nx, ny)))
                .filter(|&p| p != (x, y))
                .map(|(nx, ny)| at(nx, ny))
                .collect();
            around.sort_unstable();
            assert_eq!(
                on[((y * width + x) * spp + c) as usize],
                around[3],
                "pixel ({x}, {y}) channel {c}"
            );
            changed += usize::from(around[3] != at(x, y));
        }
    }
    assert!(changed > 0, "the repair changed some masked sample");
    let on_elsewhere = on
        .iter()
        .zip(&off)
        .enumerate()
        .filter(|&(i, (a, b))| {
            a != b
                && !masked.contains(&(
                    ((i / spp as usize) % width as usize) as u32,
                    ((i / spp as usize) / width as usize) as u32,
                ))
        })
        .count();
    assert_eq!(on_elsewhere, 0, "only the masked pixels change");
}

#[test]
fn raw_develop_counts_absent_for_tiff() {
    let cwd = common::tmp_dir("stack_raw_develop_counts_tiff");
    for (name, input) in [
        (
            "mantis-64",
            common::fixtures_dir().join("mantis-64").join("frames"),
        ),
        ("mantis-512", common::mantis_512_dir()),
    ] {
        let output = cwd.join(name);
        std::fs::create_dir(&output).expect("output directory");
        let stacked = run(&cwd, &stack_json_args(&input, &output, &[]));
        assert_eq!(
            develop_counts_of(&stacked),
            serde_json::Value::Null,
            "{name}"
        );
    }
}

#[test]
fn a_header_only_raw_frame_fails_the_run_by_name_without_panicking() {
    let cwd = common::tmp_dir("stack_raw_header_only_frame");
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&output).expect("output directory");
    let spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]);
    let good = write_dng(&spec, false);
    let bad = write_dng(&spec, true);
    std::fs::copy(good.path(), input.join("00.dng")).expect("copy RAW frame");
    std::fs::copy(bad.path(), input.join("01.dng")).expect("copy header-only RAW frame");
    std::fs::copy(good.path(), input.join("02.dng")).expect("copy RAW frame");
    let stacked = run(&cwd, &stack_json_args(&input, &output, &[]));
    let stderr = String::from_utf8_lossy(&stacked.stderr).into_owned();
    assert!(
        !stacked.status.success(),
        "a header-only frame cannot be developed"
    );
    assert!(stderr.contains("01.dng"), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    let entry = stderr
        .find("develop 3 frames (RAW)")
        .unwrap_or_else(|| panic!("no develop entry line in: {stderr}"));
    assert!(
        entry < stderr.find("01.dng").expect("checked above"),
        "the develop entry line precedes the error: {stderr}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires a long RAW decode run"]
fn raw_stack_resumes_and_hits_stage_cache() {
    let cwd = common::tmp_dir("stack_raw_resume");
    let input = cwd.join("input");
    let reference_out = cwd.join("reference");
    let resumed_out = cwd.join("resumed");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&reference_out).expect("reference output directory");
    std::fs::create_dir(&resumed_out).expect("resumed output directory");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 1024 * 1024]);
    spec.width = 1024;
    spec.height = 1024;
    let source = write_dng(&spec, false);
    for index in 0..3 {
        std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
            .expect("copy RAW frame");
    }
    let mut args = vec![
        "stack".to_string(),
        input.display().to_string(),
        "-o".to_string(),
        resumed_out.display().to_string(),
        "--algorithm".to_string(),
        "laplacian".to_string(),
        "--no-align".to_string(),
        "--no-crop".to_string(),
        "--tile-size".to_string(),
        "160".to_string(),
        "--set".to_string(),
        "levels=3".to_string(),
        "--max-host-memory".to_string(),
        "536870912".to_string(),
    ];
    if cfg!(feature = "gpu") {
        args.push("--cpu".to_string());
    }
    let reference_args = {
        let mut reference = args.clone();
        let output_index = reference.iter().position(|arg| arg == "-o").expect("-o");
        reference[output_index + 1] = reference_out.display().to_string();
        reference
    };
    let reference = run(&cwd, &reference_args);
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );

    let mut child = common::cli_bin("stack_raw_resume")
        .current_dir(&*cwd)
        .args(&args)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stack");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    loop {
        line.clear();
        stderr.read_line(&mut line).expect("read stderr");
        assert!(!line.is_empty(), "stack ended before fusion: {line}");
        if line.starts_with("fuse ") {
            break;
        }
    }
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(killed.success());
    assert_eq!(child.wait().expect("wait stack").code(), Some(130));

    args.push("--resume".to_string());
    let resumed = run(&cwd, &args);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let reference_project = reference_out.join("input.ffp");
    let resumed_project = resumed_out.join("input.ffp");
    let reference_tiff = reference_out.join("reference.tif");
    let resumed_tiff = resumed_out.join("resumed.tif");
    for (project, output) in [
        (&reference_project, &reference_tiff),
        (&resumed_project, &resumed_tiff),
    ] {
        let export = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args([
                "export",
                project.to_str().expect("project"),
                "-o",
                output.to_str().expect("output"),
            ])
            .output()
            .expect("export composite");
        assert!(
            export.status.success(),
            "{}",
            String::from_utf8_lossy(&export.stderr)
        );
    }
    assert_eq!(
        without_date_time(std::fs::read(reference_tiff).expect("reference TIFF")),
        without_date_time(std::fs::read(resumed_tiff).expect("resumed TIFF"))
    );

    let cache_a = cwd.join("cache-a");
    let cache_b = cwd.join("cache-b");
    let cache_c = cwd.join("cache-c");
    for output in [&cache_a, &cache_b, &cache_c] {
        std::fs::create_dir(output).expect("cache output directory");
    }
    let cache_args = |output: &std::path::Path| {
        let mut args = vec![
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--align-algorithm".to_string(),
            "dummy-align".to_string(),
            "--no-crop".to_string(),
            "--tile-size".to_string(),
            "160".to_string(),
            "--set".to_string(),
            "levels=3".to_string(),
            "--max-host-memory".to_string(),
            "536870912".to_string(),
            "--json".to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
        }
        args
    };
    let first_cache = run(&cwd, &cache_args(&cache_a));
    assert!(
        first_cache.status.success(),
        "{}",
        String::from_utf8_lossy(&first_cache.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first_cache.stdout).expect("first JSON")["align_cached"],
        false
    );
    let second_cache = run(&cwd, &cache_args(&cache_b));
    assert!(
        second_cache.status.success(),
        "{}",
        String::from_utf8_lossy(&second_cache.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&second_cache.stdout).expect("second JSON")["align_cached"],
        true
    );
    assert!(String::from_utf8_lossy(&second_cache.stderr).contains("reused from the stage cache"));

    spec.serial_number = Some("changed-body".to_string());
    let changed = write_dng(&spec, false);
    std::fs::copy(changed.path(), input.join("00.dng")).expect("replace metadata");
    let invalidated = run(&cwd, &cache_args(&cache_c));
    assert!(
        invalidated.status.success(),
        "{}",
        String::from_utf8_lossy(&invalidated.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&invalidated.stdout).expect("invalidated JSON")
            ["align_cached"],
        false
    );
}

#[test]
#[ignore = "reads the configured RAW corpus and may use the GPU"]
fn corpus_frame_stacks_on_default_backend() {
    let samples = std::env::var_os("FOCUSFORGE_RAW_SAMPLES")
        .expect("FOCUSFORGE_RAW_SAMPLES is required for the corpus stack test");
    let source = std::path::PathBuf::from(samples).join("canon-eos-20d.cr2");
    assert!(
        source.is_file(),
        "missing corpus RAW frame: {}",
        source.display()
    );
    let cwd = common::tmp_dir("stack_raw_corpus");
    let input = cwd.join("input");
    let default_out = cwd.join("default");
    let cpu_out = cwd.join("cpu");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&default_out).expect("default output directory");
    std::fs::create_dir(&cpu_out).expect("CPU output directory");
    for name in ["00.cr2", "01.cr2"] {
        std::fs::copy(&source, input.join(name)).expect("copy corpus RAW");
    }
    let base = vec![
        "stack".to_string(),
        input.display().to_string(),
        "-o".to_string(),
        default_out.display().to_string(),
        "--algorithm".to_string(),
        "laplacian".to_string(),
        "--no-align".to_string(),
        "--no-crop".to_string(),
        "--json".to_string(),
    ];
    let default = run(&cwd, &base);
    assert!(
        default.status.success(),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    let default_report: serde_json::Value =
        serde_json::from_slice(&default.stdout).expect("default stack JSON");
    let backend = default_report["backend"]
        .as_str()
        .expect("default report backend");
    let adapter_or_fallback = if backend == "cpu" {
        "cpu-fallback"
    } else {
        backend
    };
    eprintln!(
        "corpus selected_backend={backend} selected_adapter_or_fallback={adapter_or_fallback}"
    );
    let mut cpu_args = base.clone();
    let output_index = cpu_args.iter().position(|arg| arg == "-o").expect("-o");
    cpu_args[output_index + 1] = cpu_out.display().to_string();
    if cfg!(feature = "gpu") {
        cpu_args.push("--cpu".to_string());
    }
    let cpu = run(&cwd, &cpu_args);
    assert!(
        cpu.status.success(),
        "{}",
        String::from_utf8_lossy(&cpu.stderr)
    );
    let mut exported = Vec::new();
    for (out, name) in [(&default_out, "default.tif"), (&cpu_out, "cpu.tif")] {
        let project = out.join("input.ffp");
        let tiff = out.join(name);
        let export = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args([
                "export",
                project.to_str().expect("project"),
                "-o",
                tiff.to_str().expect("TIFF"),
            ])
            .output()
            .expect("export composite");
        assert!(
            export.status.success(),
            "{}",
            String::from_utf8_lossy(&export.stderr)
        );
        let stack = TiffStack::open_files(&[tiff]).expect("exported TIFF");
        let (width, height) = stack.dimensions();
        exported.push(
            stack
                .read_tile(
                    0,
                    Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                )
                .expect("read composite")
                .samples,
        );
    }
    let max_diff = exported[0]
        .iter()
        .zip(&exported[1])
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    eprintln!("corpus CPU/default max_diff={max_diff}");
    assert!(max_diff <= 1, "CPU/default max difference {max_diff}");

    let low_memory = run(
        &cwd,
        &[
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            cpu_out.display().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
            "--max-host-memory".to_string(),
            "1".to_string(),
        ],
    );
    assert!(!low_memory.status.success());
    let low_memory_stderr = String::from_utf8_lossy(&low_memory.stderr);
    eprintln!("corpus low_memory_refusal={low_memory_stderr}");
    assert!(
        low_memory_stderr.contains("memory limit"),
        "{low_memory_stderr}"
    );
}

/// Three 256 x 256 RAW frames in `<cwd>/input`, with their decode reservation
/// and the bytes they take developed.
fn gradient_raw_stack(cwd: &std::path::Path) -> (std::path::PathBuf, u64, u64) {
    let input = cwd.join("input");
    std::fs::create_dir(&input).expect("input directory");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], Vec::new());
    spec.width = 256;
    spec.height = 256;
    spec.samples = (0..256u32 * 256)
        .map(|i| (((i % 256) + (i / 256)) * 8 % 4000) as u16)
        .collect();
    let source = write_dng(&spec, false);
    for index in 0..3 {
        std::fs::copy(source.path(), input.join(format!("{index:02}.dng"))).expect("copy frame");
    }
    let raw = RawStack::open_files(&[input.join("00.dng")]).expect("source RAW");
    let metadata = raw.metadata(0).expect("frame 0 metadata");
    let reservation = u64::from(metadata.raw_width) * u64::from(metadata.raw_height) * 30
        + std::fs::metadata(source.path()).expect("DNG size").len();
    (input, reservation, 3 * 256 * 256 * 6)
}

const SMALL_TILES: [&str; 4] = ["--tile-size", "160", "--set", "levels=3"];

fn json_of(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON document")
}

#[test]
fn raw_in_ram_frames_narrow_the_scheduler_budget() {
    let cwd = common::tmp_dir("stack_raw_in_ram_budget");
    let (input, reservation, frames) = gradient_raw_stack(&cwd);
    let ceiling = 1u64 << 30;
    let roomy = ceiling.to_string();
    // Room for a decode, not for the frames beside it.
    let tight = (reservation + frames / 2).to_string();
    let args = |output: &str, ceiling: &str, extra: &[&str]| {
        let output = cwd.join(output);
        std::fs::create_dir(&output).expect("output directory");
        let mut args = vec!["--max-host-memory", ceiling];
        args.extend(SMALL_TILES);
        args.extend(extra);
        stack_json_args(&input, &output, &args)
    };

    let stacked = json_of(&run(&cwd, &args("ram", &roomy, &[])));
    let dry = json_of(&run(&cwd, &args("ram-dry", &roomy, &["--dry-run"])));
    for (what, report) in [("run", &stacked), ("dry run", &dry)] {
        assert_eq!(report["developed_store"], "memory", "{what}: {report}");
        assert_eq!(
            report["max_bytes_in_flight"],
            ceiling - frames,
            "{what}: {report}"
        );
    }
    assert!(stacked["band_columns"].is_null(), "{stacked}");

    let stacked = json_of(&run(&cwd, &args("disk", &tight, &[])));
    let dry = json_of(&run(&cwd, &args("disk-dry", &tight, &["--dry-run"])));
    for (what, report) in [("run", &stacked), ("dry run", &dry)] {
        assert_eq!(
            report["developed_store"], "scratch_file",
            "{what}: {report}"
        );
        assert_eq!(
            report["max_bytes_in_flight"],
            reservation + frames / 2,
            "{what}: {report}"
        );
    }

    let tiff = cwd.join("tiff");
    std::fs::create_dir(&tiff).expect("output directory");
    let frames = common::fixtures_dir().join("mantis-64").join("frames");
    let stacked = json_of(&run(&cwd, &stack_json_args(&frames, &tiff, &[])));
    assert!(stacked["developed_store"].is_null(), "{stacked}");
}

#[test]
fn raw_run_and_dry_run_report_working_set_and_bound() {
    let cwd = common::tmp_dir("stack_raw_working_set_and_bound");
    let (input, reservation, frames) = gradient_raw_stack(&cwd);
    let roomy = (1u64 << 30).to_string();
    let tight = (reservation + frames / 2).to_string();
    let args = |output: &str, ceiling: &str, extra: &[&str]| {
        let output = cwd.join(output);
        std::fs::create_dir(&output).expect("output directory");
        let mut args = vec!["--max-host-memory", ceiling];
        args.extend(SMALL_TILES);
        args.extend(extra);
        stack_json_args(&input, &output, &args)
    };

    // (store, working set one tile's reads keep hot across all frames)
    for (name, ceiling, store, working_set) in [
        ("ram", &roomy, "memory", serde_json::Value::Null),
        ("disk", &tight, "scratch_file", serde_json::json!(frames)),
    ] {
        let stacked = json_of(&run(&cwd, &args(name, ceiling, &[])));
        let dry = json_of(&run(
            &cwd,
            &args(&format!("{name}-dry"), ceiling, &["--dry-run"]),
        ));
        for (what, report) in [("run", &stacked), ("dry run", &dry)] {
            assert_eq!(report["developed_store"], store, "{name} {what}: {report}");
            assert_eq!(
                report["page_cache_bytes_per_tile"], working_set,
                "{name} {what}: {report}"
            );
        }
        assert_eq!(
            dry["in_flight_bound"], stacked["in_flight_bound"],
            "{name}: plan and run name the same bound"
        );
        // The thread cap binds exactly when the budget would allow more tiles
        // than the limit, whatever the core count.
        let limit = stacked["in_flight_limit"].as_u64().expect("limit");
        let per_tile = stacked["accounted_bytes_in_flight"]
            .as_u64()
            .expect("bytes")
            / limit;
        let allowed = stacked["max_bytes_in_flight"].as_u64().expect("budget") / per_tile;
        let bound = if allowed > limit {
            "thread_cap"
        } else {
            "host_budget"
        };
        assert_eq!(stacked["in_flight_bound"], bound, "{name}: {stacked}");
    }
}

#[cfg(unix)]
#[test]
fn raw_stack_output_is_identical_in_ram_and_on_disk() {
    let cwd = common::tmp_dir("stack_raw_in_ram_identical");
    let (input, reservation, frames) = gradient_raw_stack(&cwd);
    let tight = (reservation + frames / 2).to_string();
    let mut composites = Vec::new();
    for (name, ceiling) in [("ram", None), ("disk", Some(tight))] {
        let output = cwd.join(name);
        std::fs::create_dir(&output).expect("output directory");
        let mut extra = SMALL_TILES.to_vec();
        if let Some(ceiling) = &ceiling {
            extra.extend(["--max-host-memory", ceiling]);
        }
        let report = json_of(&run(&cwd, &stack_json_args(&input, &output, &extra)));
        let want = if ceiling.is_some() {
            "scratch_file"
        } else {
            "memory"
        };
        assert_eq!(report["developed_store"], want, "{name}: {report}");
        let composite = cwd.join(format!("{name}.tif"));
        let exported = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args([
                "export",
                output.join("input.ffp").to_str().expect("UTF-8 project"),
                "-o",
                composite.to_str().expect("UTF-8 composite"),
            ])
            .output()
            .expect("run export");
        assert!(
            exported.status.success(),
            "export stderr: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
        composites.push(without_date_time(
            std::fs::read(composite).expect("exported composite"),
        ));
    }
    assert!(
        composites[0] == composites[1],
        "the stores gave different composites"
    );
}

/// Hot sites rising 1 DN at a time across the detection threshold of a flat
/// 14-bit RAW stack: rounding of the stored developed frames puts some of them
/// on the other side of it than the on-demand develop does. The `stack` report
/// counts the detected positions grown by the demosaic reach.
#[test]
fn hot_pixels_and_stack_agree_on_a_raw_stack() {
    use std::sync::atomic::AtomicUsize;

    use focusforge_core::defect_mask::DefectMask;
    use focusforge_core::defect_repair::DEMOSAIC_REACH;
    use focusforge_core::io::{DevelopStore, open_frame_set};
    use focusforge_core::tile::HostBudget;

    const SIZE: u32 = 128;
    const BACKGROUND: u16 = 7_500;
    const FIRST_RISE: u16 = 100;
    let cwd = common::tmp_dir("stack_raw_hot_pixels_agree");
    let input = cwd.join("input");
    std::fs::create_dir(&input).expect("input directory");
    let mut samples = vec![BACKGROUND; (SIZE * SIZE) as usize];
    for (rise, at) in (0..SIZE / 4 - 1)
        .flat_map(|c| (0..4).map(move |r| (c, r)))
        .enumerate()
    {
        let (x, y) = (4 + 4 * at.0, 4 + 4 * at.1);
        samples[(y * SIZE + x) as usize] = BACKGROUND + FIRST_RISE + rise as u16;
    }
    let mut spec = DngSpec::new(14, 0, 16_383, [0, 1, 1, 2], samples);
    spec.width = SIZE;
    spec.height = SIZE;
    let source = write_dng(&spec, false);
    let paths: Vec<std::path::PathBuf> = (0..3)
        .map(|index| {
            let path = input.join(format!("{index:02}.dng"));
            std::fs::copy(source.path(), &path).expect("copy RAW frame");
            path
        })
        .collect();

    let budget = HostBudget::explicit(1 << 30);
    let detected_in_process = |develop_first: bool| {
        let set = open_frame_set(&paths, budget).expect("the frames open");
        let reader = set.reader.as_ref();
        if develop_first {
            reader
                .develop_once(
                    &cwd,
                    DevelopStore::Memory,
                    budget,
                    1,
                    &|| false,
                    &AtomicUsize::new(0),
                )
                .expect("the frames develop");
        }
        let mask = focusforge_core::hot_pixels::detect(
            reader,
            paths.len(),
            reader.dimensions(),
            reader.frame(0).samples_per_pixel,
            7.0,
            1,
            &AtomicUsize::new(0),
            &|| false,
        )
        .expect("detection runs");
        let mut positions = mask.positions();
        positions.sort_unstable();
        positions
    };
    let on_demand = detected_in_process(false);
    let stored = detected_in_process(true);
    eprintln!(
        "detected {} on demand, {} after develop_once",
        on_demand.len(),
        stored.len()
    );
    assert_ne!(
        on_demand, stored,
        "the ramp straddles the threshold for one develop and not the other"
    );

    let listed = run(
        &cwd,
        &[
            "hot-pixels".to_string(),
            input.display().to_string(),
            "--json".to_string(),
        ],
    );
    assert!(
        listed.status.success(),
        "hot-pixels stderr: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("JSON listing");
    let mut listed_positions: Vec<(u32, u32)> = listed["pixels"]
        .as_array()
        .expect("a pixel list")
        .iter()
        .map(|p| {
            (
                p["x"].as_u64().expect("x") as u32,
                p["y"].as_u64().expect("y") as u32,
            )
        })
        .collect();
    listed_positions.sort_unstable();
    let only_listed: Vec<_> = listed_positions
        .iter()
        .filter(|at| !stored.contains(at))
        .collect();
    let only_stored: Vec<_> = stored
        .iter()
        .filter(|at| !listed_positions.contains(at))
        .collect();
    assert!(
        only_listed.is_empty() && only_stored.is_empty(),
        "the command lists what a stack run detects; only listed: {only_listed:?}, \
         only detected after develop_once: {only_stored:?}"
    );

    let output = cwd.join("output");
    std::fs::create_dir(&output).expect("output directory");
    let stacked = run(
        &cwd,
        &stack_json_args(&input, &output, &["--set", "defect-removal=true"]),
    );
    assert!(
        stacked.status.success(),
        "stack stderr: {}",
        String::from_utf8_lossy(&stacked.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&stacked.stdout).expect("JSON report");
    assert_eq!(listed["count"], stored.len(), "{listed}");
    let mut found = DefectMask::new(SIZE, SIZE);
    for &(x, y) in &listed_positions {
        found.insert(x, y);
    }
    assert_eq!(
        report["defects"]["detected"],
        found.grow(DEMOSAIC_REACH).count(),
        "{report}"
    );
}
