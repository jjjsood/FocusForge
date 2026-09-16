// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR`, the idiom `crates/core/tests/common/mod.rs`
//! already established -- never a literal `../..` string.

use std::path::{Path, PathBuf};

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
