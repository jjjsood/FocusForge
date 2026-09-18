// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `--set key=value`, validated against
//! `dummy-stack`'s and `dummy-align`'s real schemas
//! (`crates/core/src/test_algorithms/`), reached here because
//! `crates/cli/Cargo.toml`'s `[dev-dependencies]` enables
//! `focusforge-core/test-algorithms` for `CARGO_BIN_EXE_focusforge-cli`.

mod common;

use std::process::Command;

fn bin() -> Command {
    common::cli_bin("set_args")
}

#[test]
fn accepts_valid_settings() {
    let output = bin()
        .args([
            "algorithms",
            "dummy-stack",
            "--set",
            "threshold=0.25",
            "--set",
            "mode=slow",
        ])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("value 0.25 (Explicit)"), "stdout: {stdout}");
    assert!(
        stdout.contains("value \"slow\" (Explicit)"),
        "stdout: {stdout}"
    );
}

#[test]
fn repeated_key_last_wins() {
    let output = bin()
        .args([
            "algorithms",
            "dummy-stack",
            "--set",
            "threshold=0.1",
            "--set",
            "threshold=0.9",
        ])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("value 0.9 (Explicit)"), "stdout: {stdout}");
    assert!(
        !stdout.contains("value 0.1 (Explicit)"),
        "the earlier value must not survive: {stdout}"
    );
}

#[test]
fn rejects_malformed_pair() {
    let output = bin()
        .args(["algorithms", "dummy-stack", "--set", "nope-no-equals"])
        .output()
        .expect("failed to run binary");
    // clap rejects this in its own value_parser, which exits 2 -- never
    // assert code() == Some(1) here.
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
}

#[test]
fn set_without_an_algorithm_id_is_an_error() {
    let output = bin()
        .args(["algorithms", "--set", "threshold=0.25"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--set requires an algorithm id"),
        "stderr: {stderr}"
    );
}

#[test]
fn rejects_unknown_key() {
    let output = bin()
        .args(["algorithms", "dummy-stack", "--set", "nope=1"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown parameter \"nope\", declared parameters: threshold, mode"),
        "stderr: {stderr}"
    );
}

#[test]
fn rejects_out_of_range_value() {
    let output = bin()
        .args(["algorithms", "dummy-stack", "--set", "threshold=5"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("threshold: value 5 is outside the allowed range [0, 1]"),
        "stderr: {stderr}"
    );
}

#[test]
fn rejects_unknown_enum_variant() {
    let output = bin()
        .args(["algorithms", "dummy-stack", "--set", "mode=turbo"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mode: unknown variant \"turbo\", allowed: fast, slow"),
        "stderr: {stderr}"
    );
}

#[test]
fn rejects_a_value_of_the_wrong_kind() {
    let output = bin()
        .args(["algorithms", "dummy-align", "--set", "max-shift=2.5"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("max-shift: \"2.5\" does not parse as int"),
        "stderr: {stderr}"
    );
}

#[test]
fn reports_every_bad_setting() {
    let output = bin()
        .args([
            "algorithms",
            "dummy-stack",
            "--set",
            "threshold=5",
            "--set",
            "mode=turbo",
            "--set",
            "nope=1",
        ])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a single non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("threshold: value 5 is outside the allowed range [0, 1]"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("mode: unknown variant \"turbo\", allowed: fast, slow"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("unknown parameter \"nope\", declared parameters: threshold, mode"),
        "stderr: {stderr}"
    );
}

#[test]
fn messages_come_from_the_schema() {
    // A concrete rendered sentence, verbatim -- proves the CLI carries no
    // second copy of the range-check wording.
    let output = bin()
        .args(["algorithms", "dummy-stack", "--set", "threshold=5"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("threshold: value 5 is outside the allowed range [0, 1]"),
        "stderr must contain the OutOfRange violation verbatim: {stderr}"
    );
}
