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

    /// An op refused before or during replay: its own 0-based index in the
    /// list, and why.
    #[error("op #{index}: {source}")]
    Op {
        index: usize,
        #[source]
        source: Box<crate::retouch::RetouchError>,
    },

    /// [`super::append::append_ops`] was handed an empty op list -- a
    /// checkpoint sealing nothing is not a real record.
    #[error("{}: an empty op list cannot be sealed", path.display())]
    EmptyOpList { path: PathBuf },

    /// A stored patch names a base blob that has no record in this project
    /// -- neither a full map nor a further patch. `blob` is the patch's own
    /// hash, `base` the missing one it names.
    #[error(
        "{}: blob {blob} names base {base}, which is not present in this project",
        path.display()
    )]
    PatchBaseMissing {
        path: PathBuf,
        blob: String,
        base: String,
    },

    /// A stored patch's own declared map shape does not match this
    /// project's Z-Map plane -- refused rather than applied.
    #[error(
        "{}: blob {blob} declares a {found_width}x{found_height} map, but this project's \
         Z-Map is {expected_width}x{expected_height}",
        path.display()
    )]
    PatchGeometryMismatch {
        path: PathBuf,
        blob: String,
        found_width: u32,
        found_height: u32,
        expected_width: u32,
        expected_height: u32,
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
