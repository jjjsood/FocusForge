// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli algorithms`: the listing, the
//! single-algorithm schema dump, the auto-hook wording, the unknown id, and
//! `--json` in both modes. Runs against `dummy-stack` and `dummy-align`
//! (`crates/core/src/test_algorithms/`), reached here because
//! `crates/cli/Cargo.toml`'s `[dev-dependencies]` enables
//! `focusforge-core/test-algorithms` for `CARGO_BIN_EXE_focusforge-cli`.

use std::process::Command;

use focusforge_core::registry;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
}

/// All three real algorithms (`laplacian`, `weighted-average`,
/// `depth-map`) reach the CLI with no id named in CLI source -- the test
/// names no id itself, iterating `registry::algorithms()` instead.
#[test]
fn lists_every_registered_algorithm() {
    let output = bin()
        .arg("algorithms")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);

    for entry in registry::algorithms() {
        assert!(
            stdout.contains(entry.id()),
            "expected registered id {:?} in `algorithms` output: {stdout}",
            entry.id()
        );
    }
}

#[test]
fn lists_registered_algorithms() {
    let output = bin()
        .arg("algorithms")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dummy-align"), "stdout: {stdout}");
    assert!(stdout.contains("dummy-stack"), "stdout: {stdout}");
    // Sorted by id: dummy-align before dummy-stack.
    let align_pos = stdout.find("dummy-align").expect("dummy-align present");
    let stack_pos = stdout.find("dummy-stack").expect("dummy-stack present");
    assert!(
        align_pos < stack_pos,
        "expected dummy-align before dummy-stack: {stdout}"
    );
}

#[test]
fn prints_a_full_schema() {
    let output = bin()
        .args(["algorithms", "dummy-stack"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "threshold",
        "float",
        "0",
        "1",
        "0.5",
        "mode",
        "enum",
        "fast",
        "slow",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?} in: {stdout}");
    }
}

#[test]
fn reports_an_auto_hook_as_unevaluated() {
    let output = bin()
        .args(["algorithms", "dummy-stack"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("auto: present, not evaluated"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("auto: none"), "stdout: {stdout}");
    assert!(
        stdout.contains("threshold: ") && stdout.contains("(Default)"),
        "threshold's provenance with no --set must be Default, not Auto: {stdout}"
    );
    // A *stack* `auto` hook is still never evaluated here (no `(Auto)` ever
    // appears), but a host-derived default *is* evaluated -- `(Host)` is
    // expected to appear on `dummy-stack`'s `workers` parameter.
    assert!(
        !stdout.contains("(Auto)"),
        "no reported provenance may be Auto -- a stack auto hook is never evaluated here: {stdout}"
    );
    assert!(
        stdout.contains("workers: ") && stdout.contains("(Host)"),
        "dummy-stack's host-derived `workers` parameter must report (Host): {stdout}"
    );
}

#[test]
fn unknown_algorithm_is_an_error() {
    let output = bin()
        .args(["algorithms", "no-such-algorithm"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown algorithm \"no-such-algorithm\""),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("known algorithms:"), "stderr: {stderr}");
}

#[test]
fn json_output_parses() {
    let output = bin()
        .args(["algorithms", "--json"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let algorithms = report["algorithms"]
        .as_array()
        .expect("algorithms must be an array");
    assert!(!algorithms.is_empty(), "report: {report}");

    let dummy_stack = algorithms
        .iter()
        .find(|a| a["id"] == "dummy-stack")
        .expect("dummy-stack must be present");
    assert!(dummy_stack["display_name"].is_string(), "{dummy_stack}");
    assert!(dummy_stack["family"].is_string(), "{dummy_stack}");
    let params = dummy_stack["params"]
        .as_array()
        .expect("params must be an array");
    assert!(!params.is_empty(), "{dummy_stack}");
    for param in params {
        for key in ["id", "kind", "default", "has_auto", "value", "provenance"] {
            assert!(
                param.get(key).is_some(),
                "expected key {key:?} in param object {param}"
            );
        }
    }
}

#[test]
fn single_id_json_is_a_one_element_array() {
    let output = bin()
        .args(["algorithms", "dummy-stack", "--json"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let algorithms = report["algorithms"]
        .as_array()
        .expect("algorithms must be an array");
    assert_eq!(algorithms.len(), 1, "report: {report}");
    assert_eq!(algorithms[0]["id"], "dummy-stack", "report: {report}");
}

#[test]
fn listing_is_never_an_error() {
    let output = bin()
        .arg("algorithms")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    assert!(
        !output.stdout.is_empty(),
        "stdout must never be empty, whether or not algorithms are linked in"
    );
}
