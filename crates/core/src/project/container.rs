// SPDX-License-Identifier: GPL-3.0-or-later

//! The atomic zip container.
//!
//! **A `.partial` file plus a rename**, the same discipline
//! `crates/cli/src/stack.rs` established for a composite: the writer opens
//! `<name>.partial` in the *same directory* as the final path, so the
//! eventual rename is same-filesystem and atomic by construction, and the
//! path a user named is either absent or a complete, openable project --
//! never a half-written zip. Every failure path removes the partial,
//! including a panic or an early `?`, because that removal lives in `Drop`
//! rather than in each caller.
//!
//! **Every entry is `Stored`.** The blobs are already zstd-compressed,
//! which is also why the `zip` dependency enables no compression backend
//! at all.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use super::error::ProjectError;

/// `<path>.partial`: the suffix appended to the file name, in `path`'s own
/// directory. Shared with `crates/cli`, whose `export` writes the same
/// `.partial`-then-rename discipline for the image files it produces from a
/// project.
pub fn partial_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    path.with_file_name(name)
}

/// A `<final_path>.partial` file beside `final_path`: dropping the guard
/// removes it, [`Self::commit`] renames it onto the final path. The caller
/// creates the file at [`Self::path`] and closes it before the guard drops.
#[derive(Debug)]
pub struct PartialFile {
    partial: PathBuf,
    final_path: PathBuf,
}

impl PartialFile {
    pub fn new(final_path: &Path) -> Self {
        PartialFile {
            partial: partial_path(final_path),
            final_path: final_path.to_path_buf(),
        }
    }

    /// Where to write.
    pub fn path(&self) -> &Path {
        &self.partial
    }

    /// Renames the partial onto the final path.
    pub fn commit(self) -> std::io::Result<()> {
        std::fs::rename(&self.partial, &self.final_path)
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.partial);
    }
}

/// Writes one project container. Entries go in a fixed order -- the
/// manifest, then the blobs, then the checkpoint -- so two seals of the same
/// inputs produce identical bytes.
pub struct ProjectWriter {
    // Declared before `partial`: the archive handle closes before the guard
    // removes the file.
    zip: ZipWriter<File>,
    partial: PartialFile,
    written: BTreeSet<String>,
}

impl ProjectWriter {
    /// Opens `<final_path>.partial` for writing. Nothing appears at
    /// `final_path` until [`Self::finish`] succeeds.
    pub fn create(final_path: &Path) -> Result<Self, ProjectError> {
        let at = partial_path(final_path);
        let file = File::create(&at).map_err(|source| ProjectError::io(&at, source))?;
        let partial = PartialFile::new(final_path);
        Ok(ProjectWriter {
            zip: ZipWriter::new(file),
            partial,
            written: BTreeSet::new(),
        })
    }

    fn options() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
    }

    /// Adds one text entry -- `manifest.json` or `checkpoints/<id>.json`.
    pub fn add_text(&mut self, name: &str, text: &str) -> Result<(), ProjectError> {
        self.zip
            .start_file(name, Self::options())
            .map_err(|err| ProjectError::container(self.partial.path(), err))?;
        self.zip
            .write_all(text.as_bytes())
            .map_err(|source| ProjectError::io(self.partial.path(), source))?;
        self.written.insert(name.to_string());
        Ok(())
    }

    /// Copies an already-compressed plane into `blobs/<hash>.zst`.
    ///
    /// A hash already written is **skipped**: two identical planes collapse
    /// to one entry, which is what "content-addressed" means, and writing
    /// the same entry name twice would produce a zip with a duplicate.
    pub fn add_blob(&mut self, hash: &str, compressed: &Path) -> Result<(), ProjectError> {
        let name = format!("blobs/{hash}.zst");
        if self.written.contains(&name) {
            return Ok(());
        }
        self.zip
            .start_file(&name, Self::options())
            .map_err(|err| ProjectError::container(self.partial.path(), err))?;
        let mut source = File::open(compressed).map_err(|err| ProjectError::io(compressed, err))?;
        std::io::copy(&mut source, &mut self.zip)
            .map_err(|err| ProjectError::io(compressed, err))?;
        self.written.insert(name);
        Ok(())
    }

    /// Copies an entry from `source` byte for byte, under `name` -- what
    /// carries an existing archive entry (a blob, a checkpoint) from one
    /// container into another without decoding it. Skipped, like
    /// [`Self::add_blob`], when `name` was already written.
    pub fn add_stream(&mut self, name: &str, source: &mut impl Read) -> Result<(), ProjectError> {
        if self.written.contains(name) {
            return Ok(());
        }
        self.zip
            .start_file(name, Self::options())
            .map_err(|err| ProjectError::container(self.partial.path(), err))?;
        std::io::copy(source, &mut self.zip)
            .map_err(|err| ProjectError::io(self.partial.path(), err))?;
        self.written.insert(name.to_string());
        Ok(())
    }

    /// Finishes the archive, flushes it to disk and renames the partial onto
    /// the named path.
    ///
    /// A failed seal leaves neither a project nor a temp file on **every**
    /// error path, including one that returns through a `?` three
    /// functions down: the [`PartialFile`] removes the partial on drop.
    pub fn finish(self) -> Result<PathBuf, ProjectError> {
        let ProjectWriter { zip, partial, .. } = self;
        let file = zip
            .finish()
            .map_err(|err| ProjectError::container(partial.path(), err))?;
        file.sync_all()
            .map_err(|source| ProjectError::io(partial.path(), source))?;
        drop(file);
        let partial_at = partial.path().to_path_buf();
        let final_path = partial.final_path.clone();
        partial
            .commit()
            .map_err(|source| ProjectError::io(&partial_at, source))?;
        Ok(final_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_path_appends_the_suffix_to_the_file_name() {
        assert_eq!(
            partial_path(Path::new("/tmp/demo.ffp")),
            Path::new("/tmp/demo.ffp.partial")
        );
    }

    #[test]
    fn partial_file_drop_removes_the_partial_and_leaves_no_final() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.tar.tif");
        let guard = PartialFile::new(&final_path);
        assert_eq!(guard.path(), dir.path().join("out.tar.tif.partial"));
        std::fs::write(guard.path(), b"half a file").unwrap();
        drop(guard);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn project_writer_create_failure_leaves_a_preexisting_partial_alone() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("demo.ffp");
        let partial = partial_path(&final_path);
        std::fs::write(&partial, b"someone else's file").unwrap();
        let mut perms = std::fs::metadata(&partial).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&partial, perms).unwrap();

        assert!(ProjectWriter::create(&final_path).is_err());
        assert!(partial.exists());
    }

    #[test]
    fn partial_file_commit_leaves_the_final_file_and_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.tar.tif");
        let guard = PartialFile::new(&final_path);
        std::fs::write(guard.path(), b"whole file").unwrap();
        guard.commit().unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"whole file");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
