// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a test
//! binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};
use std::process::Command;

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
/// binaries calls this helper (`acceptance.rs` reads `.scratch/` and the
/// reference stack directly, never a fixture).
#[allow(dead_code)]
pub fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}

/// A fresh, empty directory, removed again when the test that made it ends
/// -- used as a spawned process's own working directory, so `.scratch/`
/// lands here and never collides with another test's.
///
/// Derefs to `Path`, so `&dir` and `dir.join(..)` read exactly as they did
/// when a test file's own copy of this type was a bare `PathBuf`.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls
/// [`tmp_dir`].
#[allow(dead_code)]
pub struct TmpDir {
    path: PathBuf,
}

impl std::ops::Deref for TmpDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        // A failed test keeps its evidence -- the partials, the ledger and
        // the outputs are exactly what a failure needs to be diagnosed.
        if std::thread::panicking() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// **Named per process, not just per test.** `CARGO_TARGET_TMPDIR` can be
/// shared by concurrent `cargo test` runs, so without the pid two runs of
/// the same test name raced on the same `.scratch/resume/` ledger: `error:
/// .scratch/resume…: No such file or directory` and a `stack` run exiting 1
/// where it should exit 0, reproduced 3 rounds out of 3.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn tmp_dir(name: &str) -> TmpDir {
    let path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    TmpDir { path }
}

/// Appends `--no-gpu` to `args` when this crate's own `gpu` feature is
/// compiled in -- a no-op build has no such flag at all, not even as a
/// no-op (`crates/cli/src/main.rs`'s own `#[cfg(feature = "gpu")]` on the
/// arg). `gpu` became a default build feature, so any test that names a
/// real algorithm (`laplacian`, `depth-map`) and depends on running the CPU
/// tile scheduler -- exact-byte equality against a CPU oracle, deterministic
/// single-worker pause/resume timing -- must call this or silently start
/// exercising the GPU path's ±1 LSB budget instead.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn push_no_gpu(mut args: Vec<&str>) -> Vec<&str> {
    if cfg!(feature = "gpu") {
        args.push("--no-gpu");
    }
    args
}

/// The built CLI, with `FOCUSFORGE_DATA_DIR` pointed at a fresh directory
/// per `test_name` and process, so a spawned run never touches the
/// developer's own data directory and two parallel test binaries never
/// share one.
#[allow(dead_code)]
pub fn cli_bin(test_name: &str) -> Command {
    let data_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{test_name}-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"));
    cmd.env("FOCUSFORGE_DATA_DIR", &data_dir);
    cmd
}
