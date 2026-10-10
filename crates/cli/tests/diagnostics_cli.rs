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

#[cfg(unix)]
#[test]
fn unwritable_data_directory_falls_back_to_stderr_alone() {
    let mut cmd = common::cli_bin("diagnostics_cli_unwritable");
    let Some(data_dir) = common::readonly_dir("diagnostics_cli_unwritable", 0o555, |_| {}) else {
        return;
    };
    cmd.env("FOCUSFORGE_DATA_DIR", &*data_dir);

    let output = cmd
        .args(["info", fixture().to_str().unwrap()])
        .output()
        .expect("failed to run binary");

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

/// A `stack` invocation that warns (its `--status` path cannot be written)
/// and then refuses (no `--algorithm`), so one spawn carries both a mirrored
/// warning and a mirrored refusal. `None` when this filesystem does not
/// enforce the read-only directory the warning depends on.
#[cfg(unix)]
fn warn_and_refuse(
    label: &str,
    verbose: bool,
) -> Option<(std::process::Command, common::ReadOnlyDir, PathBuf)> {
    let dir = common::readonly_dir(label, 0o555, |_| {})?;
    let status = dir.join("status.ndjson");
    let mut cmd = common::cli_bin(label);
    if verbose {
        cmd.arg("-v");
    }
    cmd.args([
        "stack",
        fixture().to_str().unwrap(),
        "--status",
        status.to_str().unwrap(),
    ]);
    Some((cmd, dir, status))
}

fn count_lines(stderr: &str, needle: &str) -> usize {
    stderr.lines().filter(|line| line.contains(needle)).count()
}

#[cfg(unix)]
#[test]
fn mirrored_output_prints_once() {
    for (label, verbose) in [
        ("diagnostics_cli_mirror_default", false),
        ("diagnostics_cli_mirror_verbose", true),
    ] {
        let Some((mut cmd, _dir, status)) = warn_and_refuse(label, verbose) else {
            return;
        };
        let output = cmd.output().expect("failed to run binary");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            count_lines(&stderr, &format!("warning: --status {}", status.display())),
            1,
            "the --status warning must appear exactly once, got: {stderr}"
        );
        assert_eq!(
            count_lines(&stderr, "error: --algorithm is required"),
            1,
            "the refusal must appear exactly once, got: {stderr}"
        );

        let mut seen: Vec<&str> = Vec::new();
        for line in stderr.lines().filter(|l| !l.trim().is_empty()) {
            assert!(
                !seen.contains(&line),
                "stderr repeats a line: {line}\nfull stderr: {stderr}"
            );
            seen.push(line);
        }
    }
}

#[cfg(unix)]
#[test]
fn refusal_reaches_the_log() {
    let Some((mut cmd, _dir, status)) = warn_and_refuse("diagnostics_cli_refusal_log", false)
    else {
        return;
    };
    let data_dir = common::data_dir_of(&cmd);
    let output = cmd.output().expect("failed to run binary");
    assert!(!output.status.success());

    let path = log_path(&data_dir);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("log file must exist at {}: {e}", path.display()));
    let records: Vec<serde_json::Value> = contents
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every log line is JSON"))
        .collect();

    let refusal = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find(|l| l.starts_with("error: "))
        .expect("the run must refuse on stderr")
        .to_string();
    assert!(
        records
            .iter()
            .any(|r| r["level"] == "ERROR" && r["message"] == refusal.as_str()),
        "no ERROR record carries the refusal {refusal:?}: {contents}"
    );

    let warning = format!("warning: --status {}", status.display());
    assert!(
        records.iter().any(|r| {
            r["level"] == "WARN"
                && r["message"]
                    .as_str()
                    .is_some_and(|m| m.starts_with(&warning))
        }),
        "no WARN record carries the --status warning: {contents}"
    );
}

#[test]
fn spans_nest() {
    let mut cmd = common::cli_bin("diagnostics_cli_spans");
    let data_dir = common::data_dir_of(&cmd);
    let out = common::tmp_dir("diagnostics_cli_spans_out");
    let fixture = common::fixtures_dir().join("mantis-512");
    let args = common::push_no_gpu(vec![
        "stack",
        fixture.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-cache",
        "--quiet",
    ]);
    let output = cmd.args(args).output().expect("failed to run binary");
    assert!(
        output.status.success(),
        "the run must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let path = log_path(&data_dir);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("log file must exist at {}: {e}", path.display()));
    let span_names: Vec<Vec<String>> = contents
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("every log line is JSON"))
        .filter_map(|record| {
            let spans = record.get("spans")?.as_array()?.clone();
            Some(
                spans
                    .iter()
                    .filter_map(|s| s.get("name")?.as_str().map(str::to_string))
                    .collect(),
            )
        })
        .collect();

    assert!(
        span_names.iter().any(|names| {
            names.iter().any(|n| n == "job")
                && names.iter().any(|n| n == "stack")
                && names.iter().any(|n| n == "stage")
        }),
        "no log line carries the job, stack and stage spans: {contents}"
    );
    assert!(
        span_names
            .iter()
            .any(|names| names.iter().any(|n| n == "tile")),
        "no log line carries a tile span: {contents}"
    );
}
