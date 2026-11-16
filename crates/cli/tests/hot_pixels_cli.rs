// SPDX-License-Identifier: GPL-3.0-or-later

//! `hot-pixels`: the printed count, its JSON form, `--set`, the refusals, and
//! the threshold's bounds as `stack` enforces them.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

fn run(test: &str, command: &str, dir: &Path, args: &[&str]) -> Output {
    common::cli_bin(test)
        .arg(command)
        .arg(dir)
        .args(args)
        .output()
        .expect("the CLI binary starts")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn frames_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-512-hot-pixels/frames")
}

/// The sidecar's positions carried by more than half the frames, row by row.
fn majority_positions() -> Vec<(u64, u64)> {
    let sidecar =
        std::fs::read(common::fixtures_dir().join("mantis-512-hot-pixels/hot-pixels.json"))
            .expect("the sidecar exists");
    let entries: serde_json::Value = serde_json::from_slice(&sidecar).expect("valid JSON");
    let mut positions: Vec<(u64, u64)> = entries
        .as_array()
        .expect("an array of defects")
        .iter()
        .filter(|entry| 2 * entry["frames"].as_array().expect("frames").len() > 8)
        .map(|entry| (entry["y"].as_u64().unwrap(), entry["x"].as_u64().unwrap()))
        .collect();
    positions.sort_unstable();
    positions.into_iter().map(|(y, x)| (x, y)).collect()
}

fn assert_refused(output: &Output, fragment: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    let err = stderr(output);
    assert!(err.starts_with("error: "), "{err}");
    assert!(err.contains(fragment), "missing `{fragment}` in: {err}");
    assert!(!err.contains("panicked"), "{err}");
    assert!(output.stdout.is_empty());
}

#[test]
fn hot_pixels_cli() {
    let expected = majority_positions();
    assert!(!expected.is_empty(), "the fixture has majority defects");

    let json = run(
        "hot_pixels_cli_json",
        "hot-pixels",
        &frames_dir(),
        &["--json"],
    );
    assert!(json.status.success(), "{}", stderr(&json));
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).expect("valid JSON");
    assert_eq!(value["count"], expected.len());
    assert_eq!(value["width"], 512);
    assert_eq!(value["height"], 512);
    let pixels: Vec<(u64, u64)> = value["pixels"]
        .as_array()
        .expect("a pixels array")
        .iter()
        .map(|p| (p["x"].as_u64().unwrap(), p["y"].as_u64().unwrap()))
        .collect();
    assert_eq!(pixels, expected, "the majority defects, row by row");
    assert_eq!(
        value["params"]["hot-pixel-threshold"],
        focusforge_core::project::HOT_PIXEL_THRESHOLD_DEFAULT
    );

    let text = run("hot_pixels_cli_text", "hot-pixels", &frames_dir(), &[]);
    assert!(text.status.success(), "{}", stderr(&text));
    assert_eq!(stdout(&text), format!("hot pixels: {}\n", expected.len()));

    let strict = run(
        "hot_pixels_cli_strict",
        "hot-pixels",
        &frames_dir(),
        &["--json", "--set", "hot-pixel-threshold=50"],
    );
    assert!(strict.status.success(), "{}", stderr(&strict));
    let strict: serde_json::Value = serde_json::from_slice(&strict.stdout).expect("valid JSON");
    assert_eq!(strict["pixels"], value["pixels"], "k = 50 finds the same");
    assert_eq!(strict["params"]["hot-pixel-threshold"], 50.0);

    let gone = common::tmp_dir("hot_pixels_cli_missing").join("gone");
    assert_refused(
        &run("hot_pixels_cli_missing", "hot-pixels", &gone, &[]),
        &gone.display().to_string(),
    );
}

#[test]
fn hot_pixels_cli_refuses_mismatched_frame_size() {
    let mixed = common::tmp_dir("hot_pixels_cli_mismatch");
    for name in ["00.tif", "01.tif"] {
        std::fs::copy(frames_dir().join(name), mixed.join(name)).unwrap();
    }
    let small = mixed.join("02.tif");
    std::fs::copy(
        common::fixtures_dir().join("mantis-64/frames/00.tif"),
        &small,
    )
    .unwrap();
    assert_refused(
        &run("hot_pixels_cli_mismatch", "hot-pixels", &mixed, &[]),
        &small.display().to_string(),
    );
}

#[test]
fn hot_pixel_params() {
    let gone = common::tmp_dir("hot_pixel_params").join("gone");
    for (value, fragment) in [
        (
            "60",
            "hot-pixel-threshold: value 60 is outside the allowed range [2, 50]",
        ),
        (
            "1",
            "hot-pixel-threshold: value 1 is outside the allowed range [2, 50]",
        ),
    ] {
        let set = format!("hot-pixel-threshold={value}");
        // `stack` may print the GPU adapter line before it validates, so only
        // its exit code and error are held to the refusal shape.
        let stack = run(
            "hot_pixel_params_stack",
            "stack",
            &gone,
            &common::push_no_gpu(vec!["--algorithm", "laplacian", "--set", &set]),
        );
        assert_eq!(stack.status.code(), Some(1), "{}", stderr(&stack));
        assert!(stderr(&stack).contains(fragment), "{}", stderr(&stack));
        assert!(
            !stderr(&stack).contains("no TIFF frames"),
            "{}",
            stderr(&stack)
        );
        assert_refused(
            &run(
                "hot_pixel_params_command",
                "hot-pixels",
                &gone,
                &["--set", &set],
            ),
            fragment,
        );
    }
}
