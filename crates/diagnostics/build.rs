// SPDX-License-Identifier: GPL-3.0-or-later

//! Bakes the short commit hash of the checkout being built into
//! `FOCUSFORGE_COMMIT_HASH`, or the literal `unknown` when git cannot
//! answer. Never fails the build.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    for path in watched_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rustc-env=FOCUSFORGE_COMMIT_HASH={}", commit_hash());
}

fn commit_hash() -> String {
    git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
}

/// The files whose change makes the baked hash stale: `HEAD`, the ref it
/// names, and `packed-refs` for the case where that ref is packed rather
/// than loose. Each is resolved through `git rev-parse --git-path` rather
/// than assembled from a `.git` directory, because `.git` can be a file
/// pointing elsewhere, with `HEAD` and the branch ref living in two
/// different directories.
fn watched_paths() -> Vec<PathBuf> {
    let mut candidates = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(head_ref) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        candidates.push(head_ref);
    }
    candidates
        .iter()
        .filter_map(|name| git(&["rev-parse", "--git-path", name]))
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

/// The command's trimmed stdout, or `None` when git is absent, the command
/// failed or it printed nothing.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}
