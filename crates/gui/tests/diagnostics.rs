// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-gui` installs the same subscriber and panic hook as the CLI
//! before its window opens. `--settings-section nope` ends with exit code 2
//! and `FF_FORCE_CRASH=panic` panics, both before any window is created
//! (`crates/gui/src/main.rs`), so this runs on a headless host, unlike
//! `window.rs`.

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
    assert!(report.get("format").is_none());
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

fn live_slugs() -> Vec<&'static str> {
    let slugs: Vec<&'static str> = focusforge_gui::SettingsSection::ALL
        .iter()
        .map(|section| section.slug())
        .collect();
    assert_eq!(
        slugs,
        [
            "performance",
            "appearance",
            "storage",
            "input-map",
            "diagnostics",
            "about"
        ]
    );
    slugs
}

#[test]
fn settings_section_argument_refuses_retired_and_unknown_slugs() {
    for value in ["general", "nope"] {
        let mut cmd = common::gui_bin(&format!("diagnostics_refusal_{value}"));
        let data_dir = cmd
            .get_envs()
            .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
            .and_then(|(_, v)| v)
            .map(std::path::PathBuf::from)
            .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");

        let output = cmd
            .args(["--settings-section", value])
            .output()
            .expect("failed to run focusforge-gui");
        assert_eq!(output.status.code(), Some(2), "{value}");

        let got = format!("got {value:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stderr.matches(&got).count(),
            1,
            "the refusal must print exactly once, got: {stderr}"
        );

        let log_path = data_dir
            .join("focusforge")
            .join("logs")
            .join("focusforge.jsonl");
        let contents = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|e| panic!("log file must exist at {}: {e}", log_path.display()));
        let errors: Vec<String> = contents
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("every log line is JSON")
            })
            .filter(|record| record["level"] == "ERROR")
            .map(|record| record["message"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(errors.len(), 1, "one ERROR record, got {errors:?}");
        for slug in live_slugs() {
            assert!(
                errors[0].contains(&format!("{slug:?}")),
                "the refusal must name {slug:?}: {}",
                errors[0]
            );
        }
        assert!(errors[0].ends_with(&got), "{}", errors[0]);
    }
}

#[test]
fn settings_section_argument_help_names_every_live_tab() {
    let output = common::gui_bin("diagnostics_help")
        .arg("--help")
        .output()
        .expect("failed to run focusforge-gui");
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8_lossy(&output.stdout);
    let section = help
        .split("--settings-section")
        .nth(1)
        .and_then(|rest| rest.split("--dock-tab").next())
        .expect("--help describes --settings-section");
    for slug in live_slugs() {
        assert!(section.contains(slug), "--help must name {slug}: {section}");
    }
}

#[test]
fn fuse_tabs_help_names_the_decision_wait() {
    let output = common::gui_bin("diagnostics_fuse_tabs_help")
        .arg("--help")
        .output()
        .expect("failed to run focusforge-gui");
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8_lossy(&output.stdout);
    let section = help
        .split("--fuse-tabs <")
        .nth(1)
        .and_then(|rest| rest.split("--slow-run <").next())
        .expect("--help describes --fuse-tabs")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        section.contains("waits until every earlier run has settled and any Keep/Discard question has been answered in the window"),
        "--fuse-tabs help must state the wait: {section}"
    );
}

/// Windowed: every accepted value goes on to open a real window, unlike the
/// refused ones above, so this needs a compositor (`common::has_display`).
#[test]
fn settings_section_argument_opens_every_live_tab() {
    for section in live_slugs() {
        if !common::has_display() {
            println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
            return;
        }
        let status = common::gui_bin("diagnostics_section_accept")
            .args(["--settings-section", section, "--exit-after", "500"])
            .status()
            .expect("failed to run focusforge-gui");
        assert!(
            status.success(),
            "--settings-section {section} must be accepted, got {status:?}"
        );
    }
}

#[test]
fn settings_section_reports_the_selected_backend_without_adapter_identity() {
    if !common::has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let output = common::gui_bin("diagnostics_backend_line")
        .args(["--settings-section", "input-map", "--exit-after", "500"])
        .output()
        .expect("failed to run focusforge-gui");
    assert!(
        output.status.success(),
        "the settings window must exit successfully, got {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let selected_backend = if cfg!(target_os = "linux") {
        if std::env::var_os("DISPLAY").is_some() {
            "winit-x11"
        } else {
            "winit-wayland"
        }
    } else {
        "winit"
    };
    assert!(
        stdout.contains(&format!(
            "selected backend: {selected_backend}, renderer: renderer-femtovg-wgpu (wgpu 29.0.4, Manual)"
        )),
        "startup output must report the selected backend: {stdout}"
    );
    #[cfg(target_os = "linux")]
    assert!(
        stdout.contains("resolved native display:"),
        "startup output must include the native-display diagnostic: {stdout}"
    );
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        stdout.trim(),
        "selected backend: winit, renderer: renderer-femtovg-wgpu (wgpu 29.0.4, Manual)"
    );
    assert!(
        !stdout.contains("adapter"),
        "startup output must not guess an adapter: {stdout}"
    );
}

/// The trace's closing bracket is written from the flush guard's
/// destructor, so a run that ends before any window exists still has to
/// unwind out of `main` for the file to be readable.
#[cfg(feature = "trace")]
#[test]
fn trace_file_parses_after_early_exit() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("diagnostics_trace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("failed to create the trace directory");
    let trace_path = dir.join("trace.json");

    let output = common::gui_bin("diagnostics_trace")
        .args(["--settings-section", "nope"])
        .env("FOCUSFORGE_TRACE_FILE", &trace_path)
        .output()
        .expect("failed to run focusforge-gui");

    assert_eq!(output.status.code(), Some(2));
    let body = std::fs::read_to_string(&trace_path)
        .unwrap_or_else(|e| panic!("no trace at {}: {e}", trace_path.display()));
    let events: Vec<serde_json::Value> =
        serde_json::from_str(&body).expect("the trace is one closed JSON array");
    assert!(
        !events.is_empty(),
        "the run's own records must be in the trace, got: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs `focusforge-gui` with a device destroyed part-way through the
/// session (`FF_FORCE_DEVICE_LOST_MS`) and a watchdog well past it
/// (`--exit-after`), so an exit code of 0 would mean the response never
/// fired. Asserts the whole response as one shape: the documented exit
/// code, exactly one record of the loss itself and exactly one of the
/// session ending because of it, and no panic.
#[cfg(feature = "gpu-tests")]
fn assert_device_loss_response(extra_args: &[std::ffi::OsString]) {
    if !common::has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let mut cmd = common::gui_bin("diagnostics_device_loss");
    let data_dir = cmd
        .get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(std::path::PathBuf::from)
        .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");

    let output = cmd
        .env("FF_FORCE_DEVICE_LOST_MS", "1500")
        .args(extra_args)
        .args(["--exit-after", "15000"])
        .output()
        .expect("failed to run focusforge-gui");

    assert_eq!(
        output.status.code(),
        Some(3),
        "a run whose device is lost must end on exit code 3, not the watchdog's own 0 -- stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log_path = data_dir
        .join("focusforge")
        .join("logs")
        .join("focusforge.jsonl");
    let contents = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("log file must exist at {}: {e}", log_path.display()));
    let records: Vec<serde_json::Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).expect("every log line is JSON"))
        .collect();

    let loss_records = records
        .iter()
        .filter(|r| {
            r["level"] == "ERROR"
                && r["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("graphics device lost"))
        })
        .count();
    assert_eq!(
        loss_records, 1,
        "exactly one record of the loss itself, got {contents}"
    );

    let response_records = records
        .iter()
        .filter(|r| {
            r["level"] == "ERROR"
                && r["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("the graphics device is gone"))
        })
        .count();
    assert_eq!(
        response_records, 1,
        "exactly one record of the response, however many poll ticks passed, got {contents}"
    );

    assert!(
        !contents.contains("panicked at"),
        "the loss path must never panic, got: {contents}"
    );
}

#[cfg(feature = "gpu-tests")]
#[test]
fn device_loss_ends_the_session_with_a_project_open() {
    let project = common::workspace_root().join("fixtures").join("mantis-512");
    assert_device_loss_response(&["--open".into(), project.into_os_string()]);
}

#[cfg(feature = "gpu-tests")]
#[test]
fn device_loss_ends_the_session_with_no_project_open() {
    assert_device_loss_response(&[]);
}

#[cfg(feature = "gpu-tests")]
#[test]
fn device_loss_leaves_no_screenshot() {
    if !common::has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let mut cmd = common::gui_bin("diagnostics_device_loss_screenshot");
    let data_dir = cmd
        .get_envs()
        .find(|(key, _)| *key == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, value)| value)
        .map(std::path::PathBuf::from)
        .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");
    let screenshot = data_dir.join("after-loss.png");
    let output = cmd
        .env("FF_FORCE_DEVICE_LOST_MS", "1500")
        .args(["--exit-after", "15000", "--screenshot"])
        .arg(&screenshot)
        .output()
        .expect("failed to run focusforge-gui");

    assert_eq!(output.status.code(), Some(3));
    assert!(
        !screenshot.exists(),
        "a known device loss must leave no screenshot at {}",
        screenshot.display()
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

/// Without the attribute an installed Windows build opens a console window
/// behind the app. Only this binary may carry it: the CLI stays a console
/// program.
#[test]
fn windows_build_has_no_console_window() {
    let gui = include_str!("../src/main.rs");
    assert!(
        gui.lines()
            .any(|line| line == r#"#![cfg_attr(windows, windows_subsystem = "windows")]"#),
        "crates/gui/src/main.rs must start without a console on Windows"
    );
    let cli = include_str!("../../cli/src/main.rs");
    assert!(
        !cli.contains("windows_subsystem"),
        "the CLI must stay a console program"
    );
}
