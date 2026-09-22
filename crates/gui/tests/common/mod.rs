// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR`, the idiom `crates/core/tests/common/mod.rs`
//! already established -- never a literal `../..` string.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

/// `fixtures/<set>/frames/<index>.tif` -- `set` names a fixture directory
/// under `fixtures/` (`"mantis-64"`, `"mantis-512"`), `index` the frame's own
/// two-digit position in it.
#[allow(dead_code)]
pub fn fixture_frame(set: &str, index: usize) -> PathBuf {
    workspace_root()
        .join("fixtures")
        .join(set)
        .join("frames")
        .join(format!("{index:02}.tif"))
}

/// A fresh, empty directory under `CARGO_TARGET_TMPDIR`, named `name` --
/// removed first if an earlier run left one behind.
#[allow(dead_code)]
pub fn tree(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp tree must be creatable");
    dir
}

/// Polls `settle_job`/`job_view` until the run is no longer live, returning
/// the materialized plane paths from the call that actually settled it (if
/// any). The window's own timer does exactly this poll, just on a Slint
/// `Timer` instead of a plain loop.
#[allow(dead_code)]
pub fn wait_for_settle(
    controller: &mut focusforge_gui::AppController,
    timeout: Duration,
) -> Option<(PathBuf, PathBuf)> {
    let start = Instant::now();
    loop {
        if let Some(planes) = controller.settle_job() {
            return Some(planes);
        }
        if !focusforge_gui::job::is_live(&controller.job_view().state) {
            return None;
        }
        assert!(
            start.elapsed() < timeout,
            "job did not settle within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
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
