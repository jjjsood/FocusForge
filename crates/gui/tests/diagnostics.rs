// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-gui` installs the same subscriber as the CLI before its
//! window opens. `--settings-section nope` exits through
//! `std::process::exit` before any window is created (`crates/gui/src/main.rs`),
//! so this runs on a headless host, unlike `window.rs`.

mod common;

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
