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
/// Derefs to `Path`, so `&dir` and `dir.join(..)` read exactly as a bare
/// `PathBuf` would.
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
/// arg). `gpu` is a default build feature, so any test that names a
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

/// The `FOCUSFORGE_DATA_DIR` [`cli_bin`] set on `cmd`.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn data_dir_of(cmd: &Command) -> PathBuf {
    cmd.get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(PathBuf::from)
        .expect("cli_bin always sets FOCUSFORGE_DATA_DIR")
}

/// A directory built with `tempfile`, which honours `TMPDIR` and so lives on
/// a filesystem where a mode change actually takes effect. Restores a
/// writable mode before its own `TempDir` removes it.
#[allow(dead_code)]
#[cfg(unix)]
pub struct ReadOnlyDir {
    dir: tempfile::TempDir,
}

#[cfg(unix)]
impl std::ops::Deref for ReadOnlyDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.dir.path()
    }
}

#[cfg(unix)]
impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        let _ = set_mode(self.dir.path(), 0o755);
    }
}

#[cfg(unix)]
fn set_mode(dir: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
}

/// Tries to create and remove a marker file in `dir`; `true` means the
/// directory is still writable, so a mode change meant to block that either
/// failed or has no effect on this filesystem.
#[cfg(unix)]
fn still_writable(dir: &Path) -> bool {
    let marker = dir.join(".readonly-probe");
    match std::fs::write(&marker, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&marker);
            true
        }
        Err(_) => false,
    }
}

/// A fresh directory: `populate` fills it in while still writable, then
/// `mode` is applied and proven to actually block writes. When that proof
/// fails, undoes the mode change, prints a one-line skip notice to stderr,
/// and returns `None`.
#[allow(dead_code)]
#[cfg(unix)]
pub fn readonly_dir(label: &str, mode: u32, populate: impl FnOnce(&Path)) -> Option<ReadOnlyDir> {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp directory must be creatable");
    populate(dir.path());
    set_mode(dir.path(), mode).expect("mode must be settable");
    if still_writable(dir.path()) {
        let _ = set_mode(dir.path(), 0o755);
        eprintln!("skipping {label}: this filesystem does not enforce directory permissions");
        return None;
    }
    Some(ReadOnlyDir { dir })
}
