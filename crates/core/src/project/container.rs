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
use std::io::Write;
use std::path::{Path, PathBuf};

use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use super::error::ProjectError;

/// `<path>.partial`: the suffix appended to the file name, in `path`'s own
/// directory.
pub(crate) fn partial_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    path.with_file_name(name)
}

/// Writes one project container. Entries go in a fixed order -- the
/// manifest, then the blobs, then the checkpoint -- so two seals of the same
/// inputs produce identical bytes.
pub struct ProjectWriter {
    zip: Option<ZipWriter<File>>,
    partial: PathBuf,
    final_path: PathBuf,
    written: BTreeSet<String>,
}

impl ProjectWriter {
    /// Opens `<final_path>.partial` for writing. Nothing appears at
    /// `final_path` until [`Self::finish`] succeeds.
    pub fn create(final_path: &Path) -> Result<Self, ProjectError> {
        let partial = partial_path(final_path);
        let file = File::create(&partial).map_err(|source| ProjectError::io(&partial, source))?;
        Ok(ProjectWriter {
            zip: Some(ZipWriter::new(file)),
            partial,
            final_path: final_path.to_path_buf(),
            written: BTreeSet::new(),
        })
    }

    fn options() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
    }

    /// Adds one text entry -- `manifest.json` or `checkpoints/<id>.json`.
    pub fn add_text(&mut self, name: &str, text: &str) -> Result<(), ProjectError> {
        let zip = self.zip.as_mut().expect("writer is live until finish");
        zip.start_file(name, Self::options())
            .map_err(|err| ProjectError::container(&self.partial, err))?;
        zip.write_all(text.as_bytes())
            .map_err(|source| ProjectError::io(&self.partial, source))?;
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
        let zip = self.zip.as_mut().expect("writer is live until finish");
        zip.start_file(&name, Self::options())
            .map_err(|err| ProjectError::container(&self.partial, err))?;
        let mut source = File::open(compressed).map_err(|err| ProjectError::io(compressed, err))?;
        std::io::copy(&mut source, zip).map_err(|err| ProjectError::io(compressed, err))?;
        self.written.insert(name);
        Ok(())
    }

    /// Finishes the archive, flushes it to disk and renames the partial onto
    /// the named path.
    pub fn finish(mut self) -> Result<PathBuf, ProjectError> {
        let zip = self.zip.take().expect("writer is live until finish");
        let file = zip
            .finish()
            .map_err(|err| ProjectError::container(&self.partial, err))?;
        file.sync_all()
            .map_err(|source| ProjectError::io(&self.partial, source))?;
        drop(file);
        std::fs::rename(&self.partial, &self.final_path)
            .map_err(|source| ProjectError::io(&self.partial, source))?;
        Ok(self.final_path.clone())
    }
}

impl Drop for ProjectWriter {
    /// Removes the partial unless [`Self::finish`] already renamed it away.
    /// This is what makes "a failed seal leaves neither a project nor a
    /// temp file" true on **every** error path, including one that returns
    /// through a `?` three functions down.
    fn drop(&mut self) {
        self.zip.take();
        let _ = std::fs::remove_file(&self.partial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_path_appends_the_suffix_to_the_file_name() {
        assert_eq!(
            partial_path(Path::new("/tmp/demo.ffproj")),
            Path::new("/tmp/demo.ffproj.partial")
        );
    }
}
