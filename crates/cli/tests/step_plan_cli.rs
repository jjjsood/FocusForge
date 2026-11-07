// SPDX-License-Identifier: GPL-3.0-or-later

//! `step-plan`: the printed plan, its JSON form, the violation report for
//! bad settings and the parameter listing in `--help`.

mod common;

use std::process::Output;

use focusforge_core::params::{ParamValue, ParamValues, resolve_in, resolved_values};
use focusforge_core::step_plan::{Optics, PLAN_PARAMS, StepPlan, plan};

fn run(test: &str, args: &[&str]) -> Output {
    common::cli_bin(test)
        .arg("step-plan")
        .args(args)
        .output()
        .expect("the CLI binary starts")
}

fn set_args<'a>(settings: &[&'a str]) -> Vec<&'a str> {
    settings.iter().flat_map(|s| ["--set", s]).collect()
}

fn expected_plan(settings: &[(&str, ParamValue)]) -> StepPlan {
    let explicit: ParamValues = settings
        .iter()
        .map(|(id, value)| ((*id).to_string(), value.clone()))
        .collect();
    let resolved = resolve_in(
        &PLAN_PARAMS,
        &explicit,
        None,
        &focusforge_core::host::snapshot(),
    )
    .expect("settings are inside the schema bounds");
    plan(&resolved_values(&resolved))
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn step_plan_prints_text_and_json() {
    let lens = [
        "magnification=1",
        "optics=lens",
        "aperture=8",
        "overlap=0.25",
        "range_mm=5",
        "frame_seconds=3",
        "frame_mb=50",
    ];
    let output = run("step_plan_prints_text", &set_args(&lens));
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for expected in [
        "lens, f/8",
        "563.2 µm",
        "422.4 µm (422400 nm)",
        "frames:    13",
        "39 s",
        "650 MB",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }

    let mut args = set_args(&lens);
    args.push("--json");
    let output = run("step_plan_prints_lens_json", &args);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one JSON object");
    assert_eq!(value["optics"]["kind"], "lens");
    assert!(value["optics"].get("na").is_none());
    let parsed: StepPlan = serde_json::from_value(value).expect("a StepPlan");
    assert_eq!(
        parsed,
        expected_plan(&[
            ("magnification", ParamValue::Float(1.0)),
            ("optics", ParamValue::Enum("lens".into())),
            ("aperture", ParamValue::Float(8.0)),
            ("overlap", ParamValue::Float(0.25)),
            ("range_mm", ParamValue::Float(5.0)),
            ("frame_seconds", ParamValue::Float(3.0)),
            ("frame_mb", ParamValue::Float(50.0)),
        ])
    );

    let objective = ["optics=objective", "na=0.25", "overlap=0.25", "range_mm=1"];
    let output = run("step_plan_prints_objective_text", &set_args(&objective));
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("objective, NA 0.25"),
        "objective line in:\n{text}"
    );

    let mut args = set_args(&objective);
    args.push("--json");
    let output = run("step_plan_prints_objective_json", &args);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one JSON object");
    assert_eq!(value["optics"]["kind"], "objective");
    assert!(value["optics"].get("aperture").is_none());
    let parsed: StepPlan = serde_json::from_value(value).expect("a StepPlan");
    assert_eq!(parsed.optics, Optics::Objective { na: 0.25 });
    assert_eq!((parsed.step_nm, parsed.frames), (6600, 153));
}

#[test]
fn step_plan_rejects_bad_settings() {
    let bad = [
        "nonsense=1",
        "overlap=1",
        "overlap=0.95",
        "overlap=NaN",
        "aperture=0",
        "range_mm=-1",
        "magnification=abc",
        "optics=prism",
    ];
    for setting in bad {
        let output = run("step_plan_rejects_one", &set_args(&[setting]));
        assert_eq!(output.status.code(), Some(1), "{setting}");
        let key = setting.split('=').next().unwrap();
        let err = stderr(&output);
        assert!(err.starts_with("error: "), "{setting}: {err}");
        assert!(err.contains(key), "{setting}: {err}");
        assert!(!err.contains("panicked"), "{setting}: {err}");
        assert!(output.stdout.is_empty(), "{setting}");
    }

    let output = run("step_plan_rejects_range", &set_args(&["overlap=0.95"]));
    assert_eq!(
        stderr(&output),
        "error: overlap: value 0.95 is outside the allowed range [0, 0.9]\n"
    );

    let output = run(
        "step_plan_rejects_three",
        &set_args(&["nonsense=1", "overlap=1", "aperture=0"]),
    );
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    for key in ["nonsense", "overlap", "aperture"] {
        assert!(err.contains(key), "missing `{key}` in:\n{err}");
    }
}

#[test]
fn step_plan_help_lists_every_parameter() {
    let output = run("step_plan_help", &["--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let help = stdout(&output);
    for desc in PLAN_PARAMS.iter() {
        assert!(help.contains(&desc.id), "missing id `{}`", desc.id);
        assert!(help.contains(&desc.label), "missing label `{}`", desc.label);
        let default = match &desc.default {
            ParamValue::Float(v) => v.to_string(),
            ParamValue::Enum(v) => v.clone(),
            other => panic!("unexpected default {other:?}"),
        };
        assert!(
            help.contains(&format!("default {default}")),
            "missing default {default} of `{}`:\n{help}",
            desc.id
        );
    }
    assert!(
        help.contains("DOF = 0.55 µm / NA²"),
        "optics model:\n{help}"
    );

    let root = common::cli_bin("step_plan_root_help")
        .arg("--help")
        .output()
        .expect("the CLI binary starts");
    assert!(root.status.success(), "{}", stderr(&root));
    let root = stdout(&root);
    let group = root
        .split("Inspection:")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .unwrap_or_else(|| panic!("no Inspection group in:\n{root}"));
    assert!(group.contains("step-plan"), "Inspection group:\n{group}");
}
