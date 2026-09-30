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

    /// A checkpoint's `parent` names an id no checkpoint in the project
    /// carries.
    #[error("{}: checkpoint {checkpoint} names parent {parent}, which is not in this project", path.display())]
    DanglingParent {
        path: PathBuf,
        checkpoint: String,
        parent: String,
    },

    /// Walking `parent` links from `checkpoint` returns to a checkpoint
    /// already on the path -- a cycle rather than a chain.
    #[error("{}: checkpoint {checkpoint} sits in a parent cycle", path.display())]
    ParentCycle { path: PathBuf, checkpoint: String },

    /// An id handed to [`super::checkpoint::History::get`] names no
    /// checkpoint in this project.
    #[error("{}: no checkpoint named {id}", path.display())]
    UnknownCheckpoint { path: PathBuf, id: String },

    /// A delete named the project's one root checkpoint, which would leave
    /// the format's required single checkpoint with nothing to name.
    #[error(
        "{}: checkpoint {checkpoint} is the project's root and cannot be deleted",
        path.display()
    )]
    RootDelete { path: PathBuf, checkpoint: String },

    /// `checkpoint`'s nearest fused ancestor carries no [`FuseRecord`] of
    /// its own and is not the project's current head's fused ancestor, so
    /// its composite, alignment, crop and sources cannot be recovered.
    ///
    /// [`FuseRecord`]: super::checkpoint::FuseRecord
    #[error(
        "{}: checkpoint {checkpoint}'s fuse state is unrecorded and cannot be recovered",
        path.display()
    )]
    FuseStateUnknown { path: PathBuf, checkpoint: String },

    /// `path` is not a FocusForge project: not a zip at all, or a zip with
    /// no `manifest.json`.
    #[error("{}: is not a FocusForge project", path.display())]
    NotAProject { path: PathBuf },

    /// `path` carries a valid zip signature but its central directory
    /// cannot be read -- the file was cut short.
    #[error(
        "{}: is truncated or incomplete -- the file was cut short",
        path.display()
    )]
    Truncated { path: PathBuf },

    /// `entry` is named by a record -- a blob, a checkpoint -- but is not in
    /// the container.
    #[error("{}: is missing {entry}, which it references", path.display())]
    MissingEntry { path: PathBuf, entry: String },

    /// `checkpoint`'s own `stack_fingerprint` disagrees with the sources it
    /// is recorded against: an op-bearing checkpoint's with its parent's, a
    /// fused checkpoint's with its own [`FuseRecord::sources`], or the
    /// manifest's with the head's and its own [`super::Manifest::sources`].
    ///
    /// [`FuseRecord::sources`]: super::checkpoint::FuseRecord::sources
    #[error(
        "{}: checkpoint {checkpoint}'s stack_fingerprint disagrees with the sources it is recorded against",
        path.display()
    )]
    FingerprintDisagrees { path: PathBuf, checkpoint: String },

    /// A checkpoint carries ops but no `fuse` record and no `parent` --
    /// there is nothing for its `stack_fingerprint` to be checked against.
    #[error(
        "{}: checkpoint {checkpoint} carries ops but has no parent to record them against",
        path.display()
    )]
    OpsWithoutParent { path: PathBuf, checkpoint: String },

    /// A render of `checkpoint` needs its fused ancestor's sources, and
    /// [`super::validate::check_sources`] found at least one missing or
    /// changed: `detail` names each, one per line.
    #[error(
        "{}: checkpoint {checkpoint}'s sources cannot be read:\n{detail}",
        path.display()
    )]
    RenderSourcesUnavailable {
        path: PathBuf,
        checkpoint: String,
        detail: String,
    },

    /// A render of `checkpoint` needs its fused ancestor's per-frame
    /// transforms, and its [`super::checkpoint::FuseRecord::align`] carries
    /// none despite naming an alignment algorithm.
    #[error(
        "{}: checkpoint {checkpoint} cannot be rendered -- its alignment was not saved with it, \
         so its source frames cannot be placed. Re-fuse this stack to record alignment.",
        path.display()
    )]
    RenderAlignmentUnrecorded { path: PathBuf, checkpoint: String },

    /// An [`super::export::ExportOptions`] value failed schema validation --
    /// an option for another format, a value outside its range, or an
    /// unknown enum variant.
    #[error("export options: {source}")]
    ExportOptions {
        #[source]
        source: crate::params::ParamError,
    },

    /// A JPEG export's image exceeds the format's 65535-pixel-per-side
    /// limit. Refused at `prepare` time, before any pixel is rendered.
    #[error(
        "{}: {width}x{height} is too large for {format} (limit {limit}px per side)",
        path.display()
    )]
    ExportTooLarge {
        path: PathBuf,
        format: String,
        width: u32,
        height: u32,
        limit: u32,
    },

    /// A PNG or JPEG export's image carries a samples-per-pixel neither
    /// format can encode (only 1 or 3 are supported). Refused at `prepare`
    /// time, before any pixel is rendered. TIFF has no such limit.
    #[error(
        "{}: {format} cannot encode {samples_per_pixel} samples per pixel (only 1 or 3 are supported)",
        path.display()
    )]
    ExportUnsupportedSamples {
        path: PathBuf,
        format: String,
        samples_per_pixel: u32,
    },

    /// [`super::render::CheckpointImage::prepare_zmap`] found at least one of
    /// `source.sources` missing on disk: `detail` names each, one per line,
    /// `missing: <path>`. There is no stored fingerprint to compare a
    /// present file against, unlike a checkpoint's own recorded sources, so
    /// this never reports `changed`.
    #[error(
        "{}: this export's sources cannot be read:\n{detail}",
        path.display()
    )]
    ExportSourcesUnavailable { path: PathBuf, detail: String },

    /// The export could not be written: its temporary `.partial` file
    /// could not be created or finished, or the finished file could not be
    /// renamed onto it.
    #[error("{}: could not be written: {source}", path.display())]
    ExportWriteFailed {
        path: PathBuf,
        #[source]
        source: Box<ProjectError>,
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
