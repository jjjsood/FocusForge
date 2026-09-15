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
