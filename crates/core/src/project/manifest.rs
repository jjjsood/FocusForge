// SPDX-License-Identifier: GPL-3.0-or-later

//! `manifest.json`: the one entry every reader of a project opens first.
//!
//! Two format commitments live in this file's field set -- the blob `kind`
//! and `hash_algorithm` naming the hash rather than leaving it implied --
//! each with its own test in `crates/core/tests/project_format.rs`.
//!
//! **Open refuses, it does not default.** A manifest missing a required
//! field fails deserialization.
//! `deny_unknown_fields` is deliberately absent.

use serde::{Deserialize, Serialize};

use super::blob::FileStamp;
use crate::align::Registration;
use crate::params::ParamValues;
use crate::tile::{Rect, SampleFormat};

/// A project's `manifest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// `"FocusForge <CARGO_PKG_VERSION>"`.
    pub producer: String,
    /// Unix seconds, UTC. Not a formatted timestamp: the workspace has no
    /// date crate, and a display string is derivable from it.
    pub created_at: i64,
    /// The hash algorithm is *named*, not implied.
    pub hash_algorithm: String,
    /// A blake3 content hash over the frame count and every source's own
    /// content hash -- deliberately **not** the cheap header-only
    /// [`crate::job::StackFingerprint`]. The two are different values by
    /// design.
    pub stack_fingerprint: String,
    pub algorithm: String,
    pub params: ParamValues,
    /// `None` when the run was `--no-align`.
    pub align: Option<AlignRecord>,
    /// Every frame's transform and the applied crop, so a frame can be read
    /// "as fused" without re-deriving either.
    pub registration: Registration,
    pub sources: Vec<SourceRef>,
    pub composite: PlaneRef,
    pub zmap: PlaneRef,
    pub blobs: Vec<BlobRecord>,
    pub metrics: Metrics,
    /// The head checkpoint's id, i.e. `checkpoints/<id>.json`. The rest of
    /// the chain, if any, is reached by walking
    /// [`super::checkpoint::Checkpoint::parent`] from there.
    pub checkpoint: String,
}

/// The alignment half of a run, recorded so a project says how its frames
/// were registered without re-deriving it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignRecord {
    pub algorithm: String,
    pub params: ParamValues,
    /// Index into [`Manifest::sources`] of the reference frame.
    pub reference: usize,
}

/// Where one payload plane lives and what shape it has. The blob holds the
/// **raw pixel plane**, never the finished TIFF: the composite TIFF's
/// `ImageDescription` embeds `CARGO_PKG_VERSION`, so storing the TIFF would
/// degrade content-addressing into version-addressing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaneRef {
    /// The content hash, i.e. `blobs/<hash>.zst`.
    pub blob: String,
    pub width: u32,
    pub height: u32,
    /// 3 for the composite, 1 for the Z-Map.
    pub samples_per_pixel: u32,
    /// Declared, never assumed: a content-addressed store whose bytes have
    /// an undeclared endianness is one big-endian producer away from two
    /// hashes for one image.
    pub byte_order: ByteOrderTag,
    /// The exact `ImageDescription` an export of this plane writes into the
    /// TIFF's IFD.
    pub description: String,
    /// The source-frame count. Z-Map meaning; carried on both planes so
    /// there is one shape rather than two.
    pub frame_count: u32,
}

/// The byte order of a blob's raw `u16` samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ByteOrderTag {
    Little,
    Big,
}

/// One content-addressed blob. The blob-kind commitment is [`Self::kind`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobRecord {
    /// blake3 of the **uncompressed** bytes, hex -- this blob's own entry
    /// name, `blobs/<hash>.zst`.
    pub hash: String,
    /// Commitment 2. Never `#[serde(default)]`: a record without it is
    /// refused on open rather than silently read as `full`.
    pub kind: BlobKind,
    /// `"zstd"`.
    pub compression: String,
    pub uncompressed_len: u64,
    pub compressed_len: u64,
}

/// What a blob contains. `#[non_exhaustive]` so a future variant can be
/// added without breaking existing matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum BlobKind {
    /// Little-endian `u16` samples.
    Full,
    /// Little-endian `f32` samples on the 16-bit scale, unclipped.
    FullF32,
}

impl BlobKind {
    /// The sample type the blob's bytes hold.
    pub fn sample_format(self) -> SampleFormat {
        match self {
            BlobKind::Full => SampleFormat::U16,
            BlobKind::FullF32 => SampleFormat::F32,
        }
    }
}

/// One source frame, referenced -- never embedded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    /// Relative to the project's own directory when the frame is a
    /// descendant of it, otherwise the absolute path as it was given.
    pub path: String,
    pub resolution: SourceResolution,
    /// blake3 of the whole file.
    pub hash: String,
    pub bytes: u64,
    /// Modification time when the hash was taken, nanoseconds since the Unix
    /// epoch.
    pub modified_ns: u64,
    /// `[device, inode]` when the hash was taken; `None` where the platform
    /// has no stable file id.
    pub file_id: Option<[u64; 2]>,
}

impl SourceRef {
    /// The file stamp this record's hash was taken at.
    pub fn stamp(&self) -> FileStamp {
        FileStamp {
            bytes: self.bytes,
            modified_ns: self.modified_ns,
            file_id: self.file_id,
        }
    }
}

/// How [`SourceRef::path`] should be read. "Relative" means **descendant of
/// the project's own directory**: a naive "any two paths have some
/// relative form via `../`" reading would make the unresolvable state
/// unreachable on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceResolution {
    Relative,
    AsGiven,
}

/// The sharpness numbers a run can honestly fill, and the window they were
/// measured over.
///
/// All three are `Option` because a run may legitimately not have measured
/// them. [`Self::lapvar_window`] is what keeps the manifest honest: the
/// numbers are `crate::fuse::metrics::laplacian_variance` over **a named
/// central window**, applied identically to the composite and to every
/// source frame, not over a whole assembled frame -- which
/// `crate::io`'s standing rule forbids and which would cost a second full
/// stack read. `None` means the whole plane. Dividing them, rendering them and
/// scanning a directory of projects are [`super::review`]'s job.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub composite_lapvar: Option<f64>,
    pub max_input_lapvar: Option<f64>,
    pub lapvar_window: Option<Rect>,
}
