// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of the shared `tracing` subscriber: the default
//! stderr level, the `-v`/`-q` mapping, the JSON Lines file shape, and the
//! unwritable-data-directory boundary. Every spawn goes through
//! `common::cli_bin`, whose `FOCUSFORGE_DATA_DIR` override keeps a run from
//! ever touching the tester's own data directory.

mod common;

use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    common::fixtures_dir().join("mantis-64")
}

fn log_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join("focusforge")
        .join("logs")
        .join("focusforge.jsonl")
}

#[test]
fn default_level_prints_nothing_to_stderr() {
    let output = common::cli_bin("diagnostics_cli_default")
        .args(["info", fixture().to_str().unwrap()])
        .output()
        .expect("failed to run binary");

    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "default-level stderr must be empty, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn verbosity_v_adds_info_and_q_leaves_only_error() {
    let plain = common::cli_bin("diagnostics_cli_verbosity_plain")
        .args(["info", fixture().to_str().unwrap()])
        .output()
        .expect("failed to run binary");
    assert!(plain.stderr.is_empty());

    let verbose = common::cli_bin("diagnostics_cli_verbosity_v")
        .args(["-v", "info", fixture().to_str().unwrap()])
        .output()
        .expect("failed to run binary");
    let verbose_stderr = String::from_utf8_lossy(&verbose.stderr);
    assert!(
        verbose_stderr.contains("INFO"),
        "-v must add an INFO line, got: {verbose_stderr}"
    );

    let quiet = common::cli_bin("diagnostics_cli_verbosity_q")
        .args(["-q", "info", fixture().to_str().unwrap()])
        .output()
        .expect("failed to run binary");
    assert!(
        quiet.stderr.is_empty(),
        "-q must leave stderr empty on a run with no error, got: {}",
        String::from_utf8_lossy(&quiet.stderr)
    );

    let both = common::cli_bin("diagnostics_cli_verbosity_both")
        .args(["-v", "-q", "info", fixture().to_str().unwrap()])
        .output()
        .expect("failed to run binary");
    assert!(
        both.stderr.is_empty(),
        "-q must win when both -v and -q are given, got: {}",
        String::from_utf8_lossy(&both.stderr)
    );
}

#[test]
fn json_lines_every_line_parses_as_an_object_with_the_required_keys() {
    let mut cmd = common::cli_bin("diagnostics_cli_json_lines");
    cmd.args(["-v", "info", fixture().to_str().unwrap()]);
    let data_dir = common::data_dir_of(&cmd);

    let output = cmd.output().expect("failed to run binary");
    assert!(output.status.success());

    let path = log_path(&data_dir);
    let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("log file must exist at {}: {e}", path.display());
    });
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "log file must contain at least one line");

    for line in lines {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}"));
        let obj = value.as_object().expect("each line must be a JSON object");
        for key in ["timestamp", "level", "target", "message"] {
            assert!(obj.contains_key(key), "line missing {key}: {line}");
        }
    }
}

#[test]
fn unwritable_data_directory_falls_back_to_stderr_alone() {
    let mut cmd = common::cli_bin("diagnostics_cli_unwritable");
    let data_dir = common::data_dir_of(&cmd);
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut perms = std::fs::metadata(&data_dir).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o555);
    std::fs::set_permissions(&data_dir, perms).unwrap();

    let output = cmd
        .args(["info", fixture().to_str().unwrap()])
        .output()
        .expect("failed to run binary");

    let mut restore = std::fs::metadata(&data_dir).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut restore, 0o755);
    std::fs::set_permissions(&data_dir, restore).unwrap();

    assert!(
        output.status.success(),
        "an unwritable data directory must not change the command's own exit code"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.lines().count(),
        1,
        "expected exactly one note line, got: {stderr}"
    );
    assert!(stderr.contains("no log file"));
    assert!(!log_path(&data_dir).exists(), "no log file must be written");
}
