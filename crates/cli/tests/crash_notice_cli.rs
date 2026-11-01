// SPDX-License-Identifier: GPL-3.0-or-later

//! The crash notice a start prints when earlier runs crashed: one stderr
//! line naming the count, gone once the evidence has been packaged, and
//! unaffected by a crash file that cannot be read.
//!
//! Every case seeds a temporary data directory through `common::cli_bin`,
//! whose `FOCUSFORGE_DATA_DIR` override keeps the run off the tester's own.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

/// The data directory `cli_bin` pointed the child at, with an empty crash
/// directory in it.
fn data_dir(cmd: &Command) -> PathBuf {
    let dir = common::data_dir_of(cmd).join("focusforge");
    std::fs::create_dir_all(dir.join("crashes")).unwrap();
    dir
}

fn seed_panic_report(dir: &Path, stem: &str) {
    std::fs::write(
        dir.join(format!("crashes/{stem}.json")),
        "{\"format\":1,\"payload\":\"boom\",\"backtrace\":\"at main\"}",
    )
    .unwrap();
}

/// A minidump, with the metadata the monitor writes beside it when it can.
fn seed_native_crash(dir: &Path, stem: &str, metadata: bool) {
    std::fs::write(dir.join(format!("crashes/{stem}.dmp")), b"MDMP\0\0\0\0").unwrap();
    if metadata {
        std::fs::write(
            dir.join(format!("crashes/{stem}.json")),
            format!("{{\"format\":1,\"signal\":11,\"minidump\":\"{stem}.dmp\"}}"),
        )
        .unwrap();
    }
}

/// Every stderr line pointing at the packaging command.
fn hints(stderr: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| line.contains("focusforge-cli diagnostics"))
        .map(str::to_string)
        .collect()
}

#[test]
fn hint_names_the_count_once() {
    let mut cmd = common::cli_bin("crash_notice_cli_hint");
    let dir = data_dir(&cmd);
    seed_panic_report(&dir, "20260918T010203Z-focusforge-cli-7");
    seed_native_crash(&dir, "20260918T010204Z-focusforge-cli-8", true);
    let crashed = cmd.arg("algorithms").output().expect("failed to run");

    let mut clean_cmd = common::cli_bin("crash_notice_cli_hint_clean");
    data_dir(&clean_cmd);
    let clean = clean_cmd.arg("algorithms").output().expect("failed to run");

    let lines = hints(&crashed.stderr);
    assert_eq!(
        lines.len(),
        1,
        "exactly one hint per start, got: {}",
        String::from_utf8_lossy(&crashed.stderr)
    );
    assert!(
        lines[0].contains("2 earlier runs crashed"),
        "the hint names the count, got: {}",
        lines[0]
    );
    assert!(
        hints(&clean.stderr).is_empty(),
        "a clean data directory hints nothing, got: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert_eq!(
        crashed.stdout, clean.stdout,
        "the hint changes no stdout byte"
    );
    assert_eq!(crashed.status.code(), clean.status.code());
}

#[test]
fn no_hint_when_clean_or_marked() {
    let mut clean_cmd = common::cli_bin("crash_notice_cli_silent_clean");
    data_dir(&clean_cmd);
    let clean = clean_cmd.arg("algorithms").output().expect("failed to run");
    assert!(
        clean.stderr.is_empty(),
        "a run with no crash evidence leaves stderr untouched, got: {}",
        String::from_utf8_lossy(&clean.stderr)
    );

    let mut marked_cmd = common::cli_bin("crash_notice_cli_silent_marked");
    let dir = data_dir(&marked_cmd);
    seed_panic_report(&dir, "20260918T010203Z-focusforge-cli-7");
    focusforge_diagnostics::crash_notice::mark_reported(
        &dir,
        &["20260918T010203Z-focusforge-cli-7".to_string()],
    )
    .unwrap();
    let marked = marked_cmd
        .arg("algorithms")
        .output()
        .expect("failed to run");
    assert!(
        hints(&marked.stderr).is_empty(),
        "an already reported crash hints nothing, got: {}",
        String::from_utf8_lossy(&marked.stderr)
    );
    assert!(
        dir.join("crashes/20260918T010203Z-focusforge-cli-7.json")
            .is_file(),
        "marking keeps the evidence itself"
    );

    let mut quiet_cmd = common::cli_bin("crash_notice_cli_quiet");
    let quiet_dir = data_dir(&quiet_cmd);
    seed_panic_report(&quiet_dir, "20260918T010203Z-focusforge-cli-7");
    let quiet = quiet_cmd
        .args(["-q", "algorithms"])
        .output()
        .expect("failed to run");
    assert_eq!(
        hints(&quiet.stderr).len(),
        1,
        "a quiet run still surfaces the hint, got: {}",
        String::from_utf8_lossy(&quiet.stderr)
    );
}

#[test]
fn a_bundle_write_clears_the_hint() {
    let mut cmd = common::cli_bin("crash_notice_cli_bundle");
    let dir = data_dir(&cmd);
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    seed_native_crash(&dir, "20260918T010204Z-focusforge-cli-8", true);
    seed_native_crash(&dir, "20260918T010205Z-focusforge-cli-9", false);
    let out = dir.join("bundle.zip");

    let bundled = cmd
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(bundled.status.success(), "{bundled:?}");
    assert_eq!(
        hints(&bundled.stderr).len(),
        1,
        "the packaging run itself still hints, got: {}",
        String::from_utf8_lossy(&bundled.stderr)
    );

    let mut next = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"));
    next.env("FOCUSFORGE_DATA_DIR", common::data_dir_of(&cmd));
    let lines = hints(
        &next
            .arg("algorithms")
            .output()
            .expect("failed to run")
            .stderr,
    );
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("1 earlier run crashed"),
        "only the dump with no metadata is left, got: {}",
        lines[0]
    );
    assert!(
        dir.join("crashes/20260918T010204Z-focusforge-cli-8.dmp")
            .is_file(),
        "a bundle deletes no evidence"
    );
}

#[test]
fn an_unreadable_crash_file_still_hints() {
    let mut cmd = common::cli_bin("crash_notice_cli_unreadable");
    let dir = data_dir(&cmd);
    std::fs::write(dir.join("crashes/truncated.json"), "{\"format\":1,").unwrap();
    std::fs::write(dir.join("crashes/frame.tif"), b"II*\0").unwrap();
    #[cfg(unix)]
    let (expected, _locked) = {
        let record = dir.join("crashes/20260918T010203Z-focusforge-cli-7.json");
        std::fs::write(&record, "{\"format\":1,\"payload\":\"boom\"}").unwrap();
        (2, common::UnreadableFile::lock(&record).unwrap())
    };
    #[cfg(not(unix))]
    let expected = 1;

    let output = cmd.arg("algorithms").output().expect("failed to run");
    assert!(
        output.status.success(),
        "an unreadable crash file must not stop the run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("laplacian"),
        "the command still prints its usual output"
    );
    let lines = hints(&output.stderr);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains(&format!("{expected} earlier run")),
        "a stray file counts as nothing, an unreadable one as a crash, got: {}",
        lines[0]
    );
}
