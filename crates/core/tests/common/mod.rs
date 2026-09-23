// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a test
//! binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};

#[path = "reference_stack.rs"]
mod reference_stack;
#[allow(unused_imports)]
pub use reference_stack::reference_stack_dir;

/// `crates/core/../..` -- the workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `crates/core/../../fixtures` -- the workspace's committed fixture trees.
pub fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}

/// `fixtures/mantis-512`, shared by every test file driven off that crop.
///
/// `#[allow(dead_code)]`: each integration test file compiles this shared
/// module into its own binary crate, and not every one of this crate's test
/// binaries calls this helper.
#[allow(dead_code)]
pub fn mantis_512_dir() -> PathBuf {
    fixtures_dir().join("mantis-512")
}
