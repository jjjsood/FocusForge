// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-gui` installs the same subscriber and panic hook as the CLI
//! before its window opens. `--settings-section nope` exits through
//! `std::process::exit` and `FF_FORCE_CRASH=panic` panics, both before any
//! window is created (`crates/gui/src/main.rs`), so this runs on a headless
//! host, unlike `window.rs`.

mod common;

#[test]
fn forced_panic_writes_a_report() {
    let mut cmd = common::gui_bin("diagnostics_forced_panic");
    let data_dir = cmd
        .get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(std::path::PathBuf::from)
        .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");

    let output = cmd
        .env("FF_FORCE_CRASH", "panic")
        .output()
        .expect("failed to run focusforge-gui");

    assert_eq!(output.status.code(), Some(101));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("panicked at"),
        "the default panic text must still reach stderr, got: {stderr}"
    );

    let crashes = data_dir.join("focusforge").join("crashes");
    let reports: Vec<std::path::PathBuf> = std::fs::read_dir(&crashes)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", crashes.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(reports.len(), 1, "exactly one report, got {reports:?}");

    let name = reports[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.contains("focusforge-gui") && !name.contains(':'),
        "the file name names the binary and carries no colon: {name}"
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&reports[0]).unwrap())
            .expect("the report is one JSON object");
    assert_eq!(report["format"], 1);
    assert_eq!(report["thread"], "main");
    assert!(!report["backtrace"].as_str().unwrap().is_empty());
}

#[test]
fn startup_line_reaches_the_log_file_before_the_settings_section_exit() {
    let mut cmd = common::gui_bin("diagnostics_startup");
    let data_dir = cmd
        .get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(std::path::PathBuf::from)
        .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");

    let output = cmd
        .args(["--settings-section", "nope"])
        .output()
        .expect("failed to run focusforge-gui");

    assert_eq!(output.status.code(), Some(2));

    let log_path = data_dir
        .join("focusforge")
        .join("logs")
        .join("focusforge.jsonl");
    let contents = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("log file must exist at {}: {e}", log_path.display()));
    assert!(
        contents
            .lines()
            .any(|l| l.contains("\"target\":\"focusforge_diagnostics\"")),
        "expected a startup line from the diagnostics target, got: {contents}"
    );
}

#[test]
fn the_settings_section_refusal_reaches_the_log_and_prints_once() {
    let mut cmd = common::gui_bin("diagnostics_refusal");
    let data_dir = cmd
        .get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(std::path::PathBuf::from)
        .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");

    let output = cmd
        .args(["--settings-section", "nope"])
        .output()
        .expect("failed to run focusforge-gui");

    let refusal = "--settings-section accepts only \"input-map\", got \"nope\"";
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches(refusal).count(),
        1,
        "the refusal must print exactly once, got: {stderr}"
    );

    let log_path = data_dir
        .join("focusforge")
        .join("logs")
        .join("focusforge.jsonl");
    let contents = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("log file must exist at {}: {e}", log_path.display()));
    assert!(
        contents.lines().any(|line| {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("every log line is JSON");
            record["level"] == "ERROR" && record["message"].as_str().is_some_and(|m| m == refusal)
        }),
        "no ERROR record carries the refusal: {contents}"
    );
}
