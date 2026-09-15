// SPDX-License-Identifier: GPL-3.0-or-later

//! Everything that can go wrong writing or reading a project container.
//!
//! Same path convention as this crate's other error types (`io`, `zmap`):
//! a variant that concerns one *file* carries `path: PathBuf`, a variant
//! that concerns one *value* does not.

use std::path::PathBuf;

/// A failure sealing, opening or streaming a project.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProjectError {
    /// An I/O failure against a named file.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The container itself could not be written, opened or navigated --
    /// a malformed zip, a missing entry, a manifest that will not parse.
    #[error("{}: {message}", path.display())]
    Container { path: PathBuf, message: String },

    /// A blob's decompressed bytes do not hash to its entry name. The read
    /// is refused rather than handing a caller bytes it would write into an
    /// image -- a stored content hash is only worth anything if it is
    /// *checked*.
    #[error(
        "{}: blob {blob} failed its blake3 hash check -- \
         the decompressed bytes hash to {actual}",
        path.display()
    )]
    HashMismatch {
        path: PathBuf,
        blob: String,
        actual: String,
    },

    /// The manifest declares a `format_version` newer than this build
    /// understands. Both versions are named: "too new" without saying how
    /// new leaves a user with nothing to act on.
    #[error(
        "{}: project format_version {found} is newer than this build understands \
         (it writes and reads format_version {supported})",
        path.display()
    )]
    FormatVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
}

impl ProjectError {
    /// An I/O failure against `path`.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        ProjectError::Io {
            path: path.into(),
            source,
        }
    }

    /// A container-level failure against `path`, from any error type that
    /// renders -- the one place a `TiffError`, a `ZMapError`, a `zip::result::ZipError`
    /// or a `serde_json::Error` becomes a project failure that still names
    /// the file the caller handed in.
    pub(crate) fn container(path: impl Into<PathBuf>, message: impl std::fmt::Display) -> Self {
        ProjectError::Container {
            path: path.into(),
            message: message.to_string(),
        }
    }
}
