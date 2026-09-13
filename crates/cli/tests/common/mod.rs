// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a test
//! binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};

#[path = "../../../core/tests/common/reference_stack.rs"]
mod reference_stack;
#[allow(unused_imports)]
pub use reference_stack::reference_stack_dir;

/// `crates/cli/../..` -- the workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `crates/cli/../../fixtures` -- the workspace's committed fixture trees.
///
/// `#[allow(dead_code)]`: each integration test file compiles this shared
/// module into its own binary crate, and not every one of this crate's test
/// binaries calls this helper (only `cli.rs`'s ignored reference-stack test
/// does, transitively through `reference_stack_dir`).
#[allow(dead_code)]
pub fn sibling_data_root() -> Option<PathBuf> {
    let candidates = [
        workspace_root().join("..").join("..").join(".."),
        workspace_root().join("..").join("reference-data"),
    ];
    candidates
        .into_iter()
        .find(|p| p.join("tools").join("lib").join("rawtiff.py").is_file())
}

/// `<harness>/Stack`, if that directory exists.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn reference_stack_dir() -> Option<PathBuf> {
    let dir = sibling_data_root()?.join("Stack");
    dir.is_dir().then_some(dir)
}

pub fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}
