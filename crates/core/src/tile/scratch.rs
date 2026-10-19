// SPDX-License-Identifier: GPL-3.0-or-later

//! A run-scoped scratch directory, RAII-created and RAII-removed. No
//! mapping: no intermediate here outlives a single tile, and `MmapMut`
//! would need a second `unsafe` site, which the workspace denies. No
//! `tempfile` dependency either: the uniqueness rule below is small enough
//! to own outright and avoids a licensing decision for one directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tile::TileError;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many `create_dir` collisions [`ScratchDir::create_in`] tolerates
/// before giving up. A collision here means another process or thread
/// produced the exact same pid/nanos/counter tuple, which is astronomically
/// unlikely; the bound exists so a persistent failure surfaces as an error
/// rather than looping forever.
const MAX_RETRIES: u32 = 64;

/// Creates `base` (and its ancestors) if needed, then a unique child
/// directory `focusforge-<pid>-<nanos>-<counter>` under it, using
/// `create_dir` (which fails rather than silently reusing an existing
/// directory) in a bounded retry loop. The directory is not removed on
/// drop; the error carries the path that failed.
pub(crate) fn unique_child_dir(base: &Path) -> Result<PathBuf, (PathBuf, std::io::Error)> {
    fs::create_dir_all(base).map_err(|source| (base.to_path_buf(), source))?;

    let pid = process::id();
    let mut last_err = None;
    for _ in 0..MAX_RETRIES {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = base.join(format!("focusforge-{pid}-{nanos}-{counter}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(source) => last_err = Some((candidate, source)),
        }
    }
    Err(last_err.expect("MAX_RETRIES > 0"))
}

/// A uniquely named, run-scoped directory under some `base`, removed
/// (recursively, best-effort) when this value is dropped.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
    /// Set by [`Self::close`] so `Drop` does not try to remove a directory
    /// the caller already removed explicitly.
    closed: bool,
}

impl ScratchDir {
    /// Create `base` (and its ancestors) if it does not exist, then a
    /// unique child directory under it named
    /// `focusforge-<pid>-<nanos>-<counter>`, using `create_dir` (which fails
    /// rather than silently reusing an existing directory) in a bounded
    /// retry loop.
    pub fn create_in(base: &Path) -> Result<Self, TileError> {
        let path =
            unique_child_dir(base).map_err(|(path, source)| TileError::Scratch { path, source })?;
        Ok(ScratchDir {
            path,
            closed: false,
        })
    }

    /// The directory's path, valid until this value is dropped or
    /// [`Self::close`]d.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the directory now, reporting any I/O error, instead of
    /// waiting for `Drop` (which cannot report one).
    pub fn close(mut self) -> Result<(), TileError> {
        self.closed = true;
        fs::remove_dir_all(&self.path).map_err(|source| TileError::Scratch {
            path: self.path.clone(),
            source,
        })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        // A `Drop` cannot report failure; best-effort cleanup, error
        // swallowed deliberately.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_in_makes_the_directory() {
        let tmp = std::env::temp_dir().join(format!("ff-scratch-test-{}", process::id()));
        let base = tmp.join("base-exists");
        let scratch = ScratchDir::create_in(&base).unwrap();
        assert!(scratch.path().is_dir());
        drop(scratch);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn drop_removes_the_directory_and_its_contents() {
        let tmp = std::env::temp_dir().join(format!("ff-scratch-test-{}-drop", process::id()));
        let path;
        {
            let scratch = ScratchDir::create_in(&tmp).unwrap();
            path = scratch.path().to_path_buf();
            fs::write(path.join("a-file"), b"hello").unwrap();
            assert!(path.is_dir());
        }
        assert!(!path.exists(), "directory must be gone after drop");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn two_scratch_dirs_under_one_base_never_collide() {
        let tmp = std::env::temp_dir().join(format!("ff-scratch-test-{}-collide", process::id()));
        let a = ScratchDir::create_in(&tmp).unwrap();
        let b = ScratchDir::create_in(&tmp).unwrap();
        assert_ne!(a.path(), b.path());
        drop(a);
        drop(b);
        let _ = fs::remove_dir_all(&tmp);
    }
}
