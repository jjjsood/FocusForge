// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR`, the idiom `crates/core/tests/common/mod.rs`
//! already established -- never a literal `../..` string.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `crates/gui/../..` -- the workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `crates/gui/ui` -- the `.slint` source tree.
///
/// `#[allow(dead_code)]`: each integration test file compiles this shared
/// module into its own binary crate, and not every one of this crate's test
/// binaries calls every helper here (`icons.rs` does not need `ui_dir`).
#[allow(dead_code)]
pub fn ui_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui")
}

/// `assets/fonts` at the workspace root.
#[allow(dead_code)]
pub fn fonts_dir() -> PathBuf {
    workspace_root().join("assets").join("fonts")
}

/// `assets/icons` at the workspace root.
#[allow(dead_code)]
pub fn icons_dir() -> PathBuf {
    workspace_root().join("assets").join("icons")
}

/// `crates/gui/src` -- this crate's own hand-written Rust, for source scans
/// (`controller_api_is_slint_free`, `checkpoint_panel_touches_no_storage`).
#[allow(dead_code)]
pub fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.slint` file under `dir`, recursively -- no external crate, this
/// crate's own `.slint` tree is small.
#[allow(dead_code)]
pub fn find_slint_files(dir: &Path) -> Vec<PathBuf> {
    find_files_with_extension(dir, "slint")
}

/// Every `.rs` file under `dir`, recursively.
#[allow(dead_code)]
pub fn find_rust_files(dir: &Path) -> Vec<PathBuf> {
    find_files_with_extension(dir, "rs")
}

/// `contents` with every line whose trimmed start is `//` dropped -- code
/// only, so a source-scanning test does not trip on what a comment merely
/// talks about.
#[allow(dead_code)]
pub fn strip_line_comments(contents: &str) -> String {
    contents
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_files_with_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(find_files_with_extension(&path, ext));
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
    out
}

/// The built GUI, with `FOCUSFORGE_DATA_DIR` pointed at a fresh directory
/// under `CARGO_TARGET_TMPDIR`, named for `test_name` and this process --
/// mirrors `crates/cli/tests/common/mod.rs::cli_bin`, so a windowed test run
/// never touches the developer's own data directory.
#[allow(dead_code)]
pub fn gui_bin(test_name: &str) -> Command {
    let data_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{test_name}-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_focusforge-gui"));
    cmd.env("FOCUSFORGE_DATA_DIR", &data_dir);
    cmd
}

/// `true` when this host can plausibly open a window -- a test that opens
/// the real binary's window skips with an explicit line rather than
/// reddening `cargo test --workspace` on a headless gate host.
#[allow(dead_code)]
pub fn has_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
