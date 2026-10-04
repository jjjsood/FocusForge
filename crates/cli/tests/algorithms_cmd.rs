// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `focusforge-cli algorithms`: the listing, the
//! single-algorithm schema dump, the auto-hook wording, the unknown id, and
//! `--json` in both modes. Runs against `dummy-stack` and `dummy-align`
//! (`crates/core/src/test_algorithms/`), reached here because
//! `crates/cli/Cargo.toml`'s `[dev-dependencies]` enables
//! `focusforge-core/test-algorithms` for `CARGO_BIN_EXE_focusforge-cli`.

mod common;

use std::process::Command;

use focusforge_core::registry;

fn bin() -> Command {
    common::cli_bin("algorithms_cmd")
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

/// The real stack ids, found through the registry so no id is spelled here:
/// every non-test stack algorithm.
fn real_stack_ids() -> Vec<String> {
    let ids: Vec<String> = registry::stack_algorithms()
        .iter()
        .filter(|a| !a.test_only())
        .map(|a| a.id().to_string())
        .collect();
    assert_eq!(ids.len(), 3, "three real stack algorithms: {ids:?}");
    ids
}

/// Each parameter's help is printed on its own indented line directly under
/// the parameter's header line. The schemas differ in length, so a fixed
/// line offset cannot satisfy all of them.
#[test]
fn a_schema_prints_each_parameters_help_under_it() {
    let mut lengths = Vec::new();
    for id in real_stack_ids() {
        let output = bin()
            .args(["algorithms", &id])
            .output()
            .expect("failed to run binary");
        assert!(output.status.success(), "{id}: {:?}", output.status);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        let entry = registry::find(&id).expect("registered");
        lengths.push(entry.params().len());
        for desc in entry.params() {
            let header = lines
                .iter()
                .position(|l| l.starts_with(&format!("  {}: ", desc.id)))
                .unwrap_or_else(|| panic!("{id}: no header for {} in {stdout}", desc.id));
            assert_eq!(
                lines.get(header + 1).copied(),
                Some(format!("    {}", desc.help).as_str()),
                "{id}/{}: help is the line under the header: {stdout}",
                desc.id
            );
        }
    }
    lengths.sort_unstable();
    lengths.dedup();
    assert!(
        lengths.len() > 1,
        "the schemas differ in length: {lengths:?}"
    );
}

/// `--json` keeps `help` and carries `advanced` for every parameter.
#[test]
fn json_carries_help_and_advanced_per_parameter() {
    for id in real_stack_ids() {
        let output = bin()
            .args(["algorithms", &id, "--json"])
            .output()
            .expect("failed to run binary");
        assert!(output.status.success(), "{id}: {:?}", output.status);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
        let params = report["algorithms"][0]["params"]
            .as_array()
            .expect("params array");
        let entry = registry::find(&id).expect("registered");
        assert_eq!(params.len(), entry.params().len(), "{id}");
        for (json, desc) in params.iter().zip(entry.params()) {
            assert_eq!(json["help"], desc.help.as_str(), "{id}/{}", desc.id);
            assert_eq!(json["advanced"], desc.advanced, "{id}/{}", desc.id);
        }
    }
}

/// The project-level descriptors reach `algorithms --json` as an additive
/// `project_params` array, help included.
#[test]
fn project_params_json_lists_every_project_descriptor() {
    let output = bin()
        .args(["algorithms", "--json"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "{:?}", output.status);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let params = report["project_params"]
        .as_array()
        .expect("a project_params array");
    let ids: Vec<&str> = params.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["auto-crop", "equalize", "equalize-max-gain"]);
    for (json, desc) in params
        .iter()
        .zip(focusforge_core::project::PROJECT_PARAMS.iter())
    {
        assert_eq!(json["help"], desc.help.as_str(), "{}", desc.id);
        assert_eq!(json["advanced"], desc.advanced, "{}", desc.id);
        assert_eq!(json["value"], serde_json::to_value(&desc.default).unwrap());
    }
    let help = params[1]["help"].as_str().unwrap();
    assert!(help.contains("not a linear-light correction"), "{help}");
}

/// Each fusion method's entry carries the project defaults a run with that
/// method resolves; an alignment algorithm carries none.
#[test]
fn project_defaults_json_follow_the_fusion_method() {
    let output = bin()
        .args(["algorithms", "--json"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "{:?}", output.status);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    for algorithm in report["algorithms"].as_array().expect("algorithms array") {
        let id = algorithm["id"].as_str().unwrap();
        let entry = registry::find(id).expect("registered");
        match entry {
            registry::AlgorithmRef::Align(_) => {
                assert!(algorithm.get("project_defaults").is_none(), "{id}");
            }
            registry::AlgorithmRef::Stack(method) => {
                let defaults = &algorithm["project_defaults"];
                for desc in focusforge_core::project::params_for(method) {
                    assert_eq!(
                        defaults[desc.id.as_str()],
                        serde_json::to_value(&desc.default).unwrap(),
                        "{id}/{}",
                        desc.id
                    );
                }
            }
        }
    }
    let equalize = |id: &str| {
        report["algorithms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap_or_else(|| panic!("no {id} entry"))["project_defaults"]["equalize"]
            .clone()
    };
    assert_eq!(equalize("depth-map"), true);
    assert_eq!(equalize("laplacian"), true);
}

#[test]
fn project_params_text_listing_names_them_after_the_algorithms() {
    let output = bin()
        .arg("algorithms")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_project = stdout.find("equalize - ").expect("equalize is listed");
    for entry in registry::algorithms() {
        let at = stdout.find(entry.id()).expect("every algorithm is listed");
        assert!(
            at < first_project,
            "{} precedes the project settings",
            entry.id()
        );
    }
    for id in ["auto-crop", "equalize-max-gain"] {
        assert!(stdout.contains(&format!("{id} - ")), "{id} in {stdout}");
    }
}
