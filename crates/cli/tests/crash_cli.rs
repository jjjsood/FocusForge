// SPDX-License-Identifier: GPL-3.0-or-later

//! What a panic leaves behind, seen from outside the process: the crash
//! report, the default panic text the previously installed hook still
//! prints, the exit code, and a batch run whose first stack panics while
//! the rest run.
//!
//! Every case spawns a child through `common::cli_bin`, whose
//! `FOCUSFORGE_DATA_DIR` override keeps reports out of the tester's own
//! data directory, and forces the panic with `FF_FORCE_CRASH`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::io::test_frames::write_frame_with_capture_time;

fn crashes_dir(cmd: &Command) -> PathBuf {
    common::data_dir_of(cmd).join("focusforge").join("crashes")
}

/// The one report under `dir`, as its path and its parsed body.
fn single_report(dir: &Path) -> (PathBuf, serde_json::Value) {
    let reports: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(reports.len(), 1, "exactly one report, got {reports:?}");
    let body = std::fs::read_to_string(&reports[0]).unwrap();
    let parsed =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("the report must be JSON: {e}"));
    (reports[0].clone(), parsed)
}

#[test]
fn panic_writes_report() {
    let mut cmd = common::cli_bin("crash_cli_panic_writes_report");
    let output = cmd
        .env("FF_FORCE_CRASH", "panic")
        .arg("algorithms")
        .output()
        .expect("failed to run the binary");
    assert_eq!(output.status.code(), Some(101));

    let (path, report) = single_report(&crashes_dir(&cmd));
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        !name.contains(':'),
        "the file name carries no colon: {name}"
    );
    assert!(name.ends_with(".json"), "{name}");
    assert!(
        name.contains("focusforge-cli"),
        "the file name names the binary: {name}"
    );

    assert_eq!(report["format"], 1);
    assert!(
        report["payload"].as_str().is_some_and(|p| !p.is_empty()),
        "payload: {}",
        report["payload"]
    );
    assert!(
        report["file"]
            .as_str()
            .is_some_and(|f| f.ends_with("lib.rs")),
        "file: {}",
        report["file"]
    );
    assert!(
        report["line"].as_u64().is_some(),
        "line: {}",
        report["line"]
    );
    assert!(
        report["column"].as_u64().is_some(),
        "column: {}",
        report["column"]
    );
    assert_eq!(report["thread"], "main");
    assert!(
        !report["backtrace"].as_str().unwrap().is_empty(),
        "the report carries a backtrace"
    );
    assert!(
        report["timestamp"]
            .as_str()
            .is_some_and(|t| t.contains(':') && t.ends_with('Z')),
        "the body keeps full RFC 3339 precision: {}",
        report["timestamp"]
    );
    assert!(
        report["ring"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().is_some_and(|l| l.contains("starting"))),
        "the ring dump carries the startup line: {}",
        report["ring"]
    );
}

#[test]
fn panic_on_pool_thread() {
    let mut cmd = common::cli_bin("crash_cli_panic_on_pool_thread");
    let output = cmd
        .env("FF_FORCE_CRASH", "pool")
        .arg("algorithms")
        .output()
        .expect("failed to run the binary");
    assert!(
        output.status.success(),
        "a panic rayon absorbs must not end the run: {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let (_, report) = single_report(&crashes_dir(&cmd));
    let thread = report["thread"].as_str().unwrap_or_default();
    assert!(
        thread.starts_with("focusforge-worker-"),
        "the report names the worker thread, got {thread:?}"
    );
}

#[test]
fn previous_hook_chained() {
    let mut cmd = common::cli_bin("crash_cli_previous_hook_chained");
    let output = cmd
        .env("FF_FORCE_CRASH", "panic")
        .arg("algorithms")
        .output()
        .expect("failed to run the binary");
    assert_eq!(output.status.code(), Some(101));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("panicked at"),
        "the default panic text must still reach stderr, got: {stderr}"
    );

    let (path, _) = single_report(&crashes_dir(&cmd));
    assert!(path.is_file());
}

#[test]
fn unwritable_crash_dir() {
    let base = common::tmp_dir("crash_cli_unwritable");
    let readonly = base.join("readonly");
    std::fs::create_dir_all(&readonly).unwrap();
    let mut permissions = std::fs::metadata(&readonly).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o500);
    }
    std::fs::set_permissions(&readonly, permissions).unwrap();

    let data_dir = readonly.join("data");
    let mut cmd = common::cli_bin("crash_cli_unwritable_run");
    let output = cmd
        .env("FOCUSFORGE_DATA_DIR", &data_dir)
        .env("FF_FORCE_CRASH", "panic")
        .arg("algorithms")
        .output()
        .expect("failed to run the binary");

    assert_eq!(output.status.code(), Some(101));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("panicked at"),
        "the default panic text must still reach stderr, got: {stderr}"
    );
    assert!(
        !data_dir.join("focusforge").join("crashes").exists(),
        "no report can be written under an unwritable directory"
    );
}

#[test]
fn panicking_stack_fails_alone() {
    let dump = common::tmp_dir("crash_cli_two_groups");
    for (file, when) in [
        ("IMG_9801.tif", "2026:08:29 18:42:41"),
        ("IMG_9802.tif", "2026:08:29 18:42:42"),
        ("IMG_9910.tif", "2026:08:29 19:02:55"),
        ("IMG_9911.tif", "2026:08:29 19:02:56"),
    ] {
        write_frame_with_capture_time(&dump.join(file), 8, 8, Some(when))
            .expect("test frame must write");
    }
    let out_dir = common::tmp_dir("crash_cli_two_groups_out");
    let cwd = common::tmp_dir("crash_cli_two_groups_cwd");
    let status = out_dir.join("status.ndjson");

    let mut cmd = common::cli_bin("crash_cli_two_groups_run");
    let output = cmd
        .env("FF_FORCE_CRASH", "stack")
        .current_dir(&*cwd)
        .args([
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algo",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--status",
            status.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run the binary");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a caught panic is a failed stack, not a crashed process: {stderr}"
    );

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&status)
        .expect("--status must be written")
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is one JSON record"))
        .collect();
    assert_eq!(records.len(), 2, "one record per group: {records:?}");
    assert_eq!(records[0]["outcome"], "failed");
    let error = records[0]["error"].as_str().unwrap_or_default();
    assert!(!error.is_empty(), "the payload is the record's error");
    assert_eq!(records[1]["outcome"], "completed");

    assert!(
        stderr.contains(&format!("error: {error}")),
        "stderr carries the payload as a refusal, got: {stderr}"
    );
    let (path, _) = single_report(&crashes_dir(&cmd));
    assert!(
        stderr.contains(&path.display().to_string()),
        "stderr names the crash report {}, got: {stderr}",
        path.display()
    );
}
