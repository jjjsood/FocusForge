// SPDX-License-Identifier: GPL-3.0-or-later

//! What a crash leaves behind, seen from outside the process: the panic
//! report, the default panic text the previously installed hook still
//! prints, the exit code, a batch run whose first stack panics while the
//! rest run, and -- for a native crash no panic hook can see -- the
//! minidump and metadata pair the monitor process writes.
//!
//! Every case spawns a child through `common::cli_bin`, whose
//! `FOCUSFORGE_DATA_DIR` override keeps reports out of the tester's own
//! data directory, and forces the crash with `FF_FORCE_CRASH`.

// The native-crash cases are Unix-only; their helpers and imports have no user on Windows.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

    assert!(report.get("format").is_none());
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

#[cfg(unix)]
#[test]
fn unwritable_crash_dir() {
    let Some(readonly) = common::readonly_dir("crash_cli_unwritable", 0o500, |_| {}) else {
        return;
    };
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
            "--algorithm",
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

/// The minidump and its metadata, polled for up to five seconds: the
/// monitor writes them after the application process is already dead, and
/// the metadata is written last, so its presence marks the pair complete.
#[cfg(unix)]
fn native_crash_files(dir: &Path) -> (PathBuf, serde_json::Value) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        let of_extension = |ext: &str| -> Vec<PathBuf> {
            paths
                .iter()
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
                .cloned()
                .collect()
        };
        let dumps = of_extension("dmp");
        let metadata = of_extension("json");
        if dumps.len() == 1
            && metadata.len() == 1
            && let Ok(body) = std::fs::read_to_string(&metadata[0])
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
        {
            assert_eq!(
                dumps[0].file_stem(),
                metadata[0].file_stem(),
                "the dump and its metadata share one stem"
            );
            let bytes = std::fs::read(&dumps[0]).expect("the minidump must be readable");
            assert!(
                bytes.starts_with(b"MDMP"),
                "{} must begin with the minidump magic, got {:?}",
                dumps[0].display(),
                &bytes[..bytes.len().min(4)]
            );
            return (dumps[0].clone(), parsed);
        }
        assert!(
            Instant::now() < deadline,
            "no minidump and metadata pair under {} within five seconds, got {paths:?}",
            dir.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs the CLI with `FF_FORCE_CRASH` set to `kind`, returning the child's
/// pid and the signal it died from.
#[cfg(unix)]
fn crash_child(cmd: &mut Command, kind: &str) -> (u32, i32) {
    use std::os::unix::process::ExitStatusExt;

    let mut child = cmd
        .env("FF_FORCE_CRASH", kind)
        .arg("algorithms")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to run the binary");
    let pid = child.id();
    let status = child.wait().expect("failed to wait for the binary");
    let signal = status
        .signal()
        .unwrap_or_else(|| panic!("the child must die from a signal, got {status:?}"));
    (pid, signal)
}

/// The metadata's own account of the crash, checked against the child that
/// produced it.
#[cfg(unix)]
fn assert_metadata(metadata: &serde_json::Value, pid: u32, reason: &str, dump: &Path) {
    assert_eq!(metadata["binary"], "focusforge-cli");
    assert_eq!(metadata["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(metadata["pid"], pid);
    assert_eq!(metadata["reason"], reason);
    assert_eq!(
        metadata["minidump"],
        dump.file_name().unwrap().to_string_lossy().as_ref()
    );
    assert!(
        metadata["timestamp"]
            .as_str()
            .is_some_and(|t| t.contains(':') && t.ends_with('Z')),
        "the metadata keeps full RFC 3339 precision: {}",
        metadata["timestamp"]
    );
}

#[cfg(unix)]
#[test]
fn native_segfault() {
    let mut cmd = common::cli_bin("crash_cli_native_segfault");
    let (pid, signal) = crash_child(&mut cmd, "segv");
    assert_eq!(signal, 11, "a forced segfault dies from SIGSEGV");

    let (dump, metadata) = native_crash_files(&crashes_dir(&cmd));
    assert_metadata(&metadata, pid, "SIGSEGV", &dump);
    assert!(
        dump.file_name()
            .unwrap()
            .to_string_lossy()
            .contains("focusforge-cli"),
        "the dump names the binary: {}",
        dump.display()
    );
}

#[cfg(unix)]
#[test]
fn native_stack_overflow() {
    let mut cmd = common::cli_bin("crash_cli_native_stack_overflow");
    let (pid, signal) = crash_child(&mut cmd, "stack-overflow");
    // On macOS the capture answers the Mach exception and the faulting
    // instruction runs again; the runtime's guard-page handler then aborts.
    let (expected, name) = if cfg!(target_os = "macos") {
        (6, "SIGABRT")
    } else {
        (11, "SIGSEGV")
    };
    assert_eq!(signal, expected, "an overflowed stack dies from {name}");

    let (dump, metadata) = native_crash_files(&crashes_dir(&cmd));
    assert_metadata(&metadata, pid, "SIGSEGV", &dump);
}

#[cfg(unix)]
#[test]
fn native_abort() {
    let mut cmd = common::cli_bin("crash_cli_native_abort");
    let (pid, signal) = crash_child(&mut cmd, "abort");
    assert_eq!(signal, 6, "a forced abort dies from SIGABRT");

    let (dump, metadata) = native_crash_files(&crashes_dir(&cmd));
    assert_metadata(&metadata, pid, "SIGABRT", &dump);
}

/// Every live process whose environment carries both `data_dir` and the
/// monitor marker -- the monitor spawned for a run under that directory,
/// and nothing else on the machine.
#[cfg(target_os = "linux")]
fn monitors_under(data_dir: &Path) -> Vec<u32> {
    let expected = format!("FOCUSFORGE_DATA_DIR={}", data_dir.display());
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        let vars: Vec<&[u8]> = environ.split(|b| *b == 0).collect();
        if vars.contains(&expected.as_bytes())
            && vars.iter().any(|v| v.starts_with(b"FF_CRASH_MONITOR="))
        {
            found.push(pid);
        }
    }
    found
}

#[test]
fn monitor_exits_with_app() {
    #[cfg(not(target_os = "linux"))]
    println!("skipped: the monitor's lifetime is read from /proc");

    #[cfg(target_os = "linux")]
    {
        let mut monitored = common::cli_bin("crash_cli_monitor_exits_monitored");
        let with_monitor = monitored
            .arg("algorithms")
            .output()
            .expect("failed to run the binary");

        let mut plain = common::cli_bin("crash_cli_monitor_exits_plain");
        let without_monitor = plain
            .env("FF_CRASH_MONITOR", "off")
            .arg("algorithms")
            .output()
            .expect("failed to run the binary");

        assert_eq!(with_monitor.status.code(), without_monitor.status.code());
        assert_eq!(with_monitor.stdout, without_monitor.stdout);
        assert_eq!(
            String::from_utf8_lossy(&with_monitor.stderr),
            String::from_utf8_lossy(&without_monitor.stderr)
        );

        let data_dir = common::data_dir_of(&monitored);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = monitors_under(&data_dir);
            if left.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the monitor outlived the run it belongs to: {left:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn monitor_runs_with_the_argument_it_is_spawned_with() {
    #[cfg(not(target_os = "linux"))]
    println!("skipped: a bound socket is read from /proc/net/unix");

    #[cfg(target_os = "linux")]
    {
        let socket = format!("focusforge-crash-cli-{}", std::process::id());
        let mut monitor = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .arg("--focusforge-crash-monitor")
            .env("FF_CRASH_MONITOR", &socket)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to run the binary");

        let deadline = Instant::now() + Duration::from_secs(5);
        let bound = loop {
            let sockets = std::fs::read_to_string("/proc/net/unix").unwrap_or_default();
            if sockets.contains(&format!("@{socket}")) {
                break true;
            }
            if monitor.try_wait().is_ok_and(|exited| exited.is_some()) || Instant::now() >= deadline
            {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = monitor.kill();
        let output = monitor
            .wait_with_output()
            .expect("failed to reap the monitor");
        assert!(
            bound,
            "a monitor spawned with that argument must bind its socket, not parse it: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn monitor_unavailable() {
    let unbindable = common::tmp_dir("crash_cli_monitor_unavailable")
        .join("absent")
        .join("crash.sock");

    let mut cmd = common::cli_bin("crash_cli_monitor_unavailable_run");
    let output = cmd
        .env("FF_CRASH_SOCKET", &unbindable)
        .arg("algorithms")
        .output()
        .expect("failed to run the binary");

    assert!(
        output.status.success(),
        "a monitor that cannot start must not fail the run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("laplacian"),
        "the command still prints its usual output: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let log = common::data_dir_of(&cmd)
        .join("focusforge")
        .join("logs")
        .join("focusforge.jsonl");
    let contents = std::fs::read_to_string(&log)
        .unwrap_or_else(|e| panic!("log file must exist at {}: {e}", log.display()));
    let warnings: Vec<&str> = contents
        .lines()
        .filter(|line| {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("every log line is JSON");
            record["level"] == "WARN"
                && record["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("native crashes"))
        })
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "exactly one WARN record names the missing native-crash capture, got {warnings:?}"
    );
}
