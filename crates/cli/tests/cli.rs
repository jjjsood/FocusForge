// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
}

#[test]
fn version_flag_prints_package_version() {
    let output = bin()
        .arg("--version")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout did not contain package version: {stdout:?}"
    );
}

#[test]
fn info_lists_fixture_directory() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_dir = manifest_dir.join("../../fixtures/mantis-512");
    let output = bin()
        .arg("info")
        .arg(&fixture_dir)
        .output()
        .expect("failed to run binary");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let has_tif_with_size = stdout
        .lines()
        .any(|line| line.contains(".tif") && line.split('\t').nth(1).is_some());
    assert!(
        has_tif_with_size,
        "no .tif entry with byte count in stdout: {stdout:?}"
    );
}
