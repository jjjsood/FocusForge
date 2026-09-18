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

/// A native crash fires no panic hook: the monitor process the GUI spawns
/// is what leaves the minidump and its metadata behind.
#[cfg(unix)]
#[test]
fn native_crash_writes_a_minidump() {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    let mut cmd = common::gui_bin("diagnostics_native_crash");
    let data_dir = cmd
        .get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(std::path::PathBuf::from)
        .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");

    let mut child = cmd
        .env("FF_FORCE_CRASH", "segv")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to run focusforge-gui");
    let pid = child.id();
    let status = child.wait().expect("failed to wait for focusforge-gui");
    assert_eq!(
        status.signal(),
        Some(11),
        "a forced segfault dies from SIGSEGV, got {status:?}"
    );

    let crashes = data_dir.join("focusforge").join("crashes");
    let deadline = Instant::now() + Duration::from_secs(5);
    let (dump, metadata) = loop {
        let paths: Vec<std::path::PathBuf> = std::fs::read_dir(&crashes)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        let dumps: Vec<_> = paths
            .iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("dmp"))
            .collect();
        if let Some(dump) = dumps.first()
            && let Ok(body) = std::fs::read_to_string(dump.with_extension("json"))
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
        {
            break ((*dump).clone(), parsed);
        }
        assert!(
            Instant::now() < deadline,
            "no minidump under {} within five seconds, got {paths:?}",
            crashes.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    let name = dump.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.contains("focusforge-gui") && !name.contains(':'),
        "the dump names the binary and carries no colon: {name}"
    );
    assert!(
        std::fs::read(&dump).unwrap().starts_with(b"MDMP"),
        "{name} must begin with the minidump magic"
    );
    assert_eq!(metadata["binary"], "focusforge-gui");
    assert_eq!(metadata["pid"], pid);
    assert_eq!(metadata["reason"], "SIGSEGV");
    assert_eq!(metadata["minidump"], name);
}
