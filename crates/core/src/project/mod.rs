// SPDX-License-Identifier: GPL-3.0-or-later

//! The project container: what a stacking run writes instead of an image.
//!
//! A project is a zip holding exactly three kinds of entry:
//!
//! ```text
//! $ unzip -l mantis-512.ffp   # a real run over fixtures/mantis-512
//!   Length      Date    Time    Name
//! ---------  ---------- -----   ----
//!      3804  1980-01-01 00:00   manifest.json
//!   1426980  1980-01-01 00:00   blobs/d24eca28….zst        the composite plane
//!     40385  1980-01-01 00:00   blobs/7108de1d….zst        the Z-Map plane
//!       359  1980-01-01 00:00   checkpoints/5b71adbf….json
//! ```
//!
//! Every entry is `Stored`, because the blobs are already zstd-compressed.
//! Every timestamp is the DOS epoch, because the `zip` dependency enables no
//! clock feature -- which is what makes two seals of the same inputs produce
//! identical container bytes (`the_container_is_reproducible`).
//! Nothing outside those three paths is ever written, and a test asserts it
//! (`container_layout_is_exactly_the_named_entries`).
//!
//! # The format is frozen from its first write
//!
//! Every project file has to open in every later release, so the format
//! enumerates four commitments rather than leaving them to whoever built
//! the writer. Each is pinned by a named test in
//! `crates/core/tests/project_format.rs`:
//!
//! | Commitment | Where it lives | Test |
//! |---|---|---|
//! | 1. the layout plus a `format_version` | [`Manifest::format_version`] | `manifest_carries_a_format_version` |
//! | 2. a blob kind field | [`BlobRecord::kind`] | `blob_kind_is_recorded_and_required` |
//! | 3. a hash over uncompressed content, algorithm named | [`blob::stage_plane`], [`Manifest::hash_algorithm`] | `hash_is_over_uncompressed_content` |
//! | 4. a fully serialized `Checkpoint` with all four `Op` variants | [`Checkpoint`], [`Op`] | `checkpoint_serializes_every_field_with_no_parent_and_no_ops`, `every_op_variant_round_trips` |
//!
//! Three of the four are **invisible** failures rather than loud ones -- a
//! missing `format_version`, an absent blob kind, or a hash taken over
//! compressed bytes all produce a project that opens without error and
//! cannot be extended later.
//!
//! # What this format deliberately does not do
//!
//! No op log replay, no undo/redo, no checkpoint DAG, no sparse deltas, no
//! branch-from-any-checkpoint, no browser. A project written here contains
//! one parentless, op-less checkpoint.
//!
//! # What `core` does not do
//!
//! Nothing here prints. Unresolved sources come back as **data**
//! ([`Project::unresolved_sources`]) and reporting them is the CLI's.

pub mod blob;
pub mod checkpoint;
pub mod container;
pub mod error;
pub mod manifest;
pub mod review;
pub mod seal;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub use blob::{Plane, StagedBlob, hash_file, short_hash, stack_fingerprint};
pub use checkpoint::{Checkpoint, Op, Point};
pub use container::{ProjectWriter, partial_path};
pub use error::ProjectError;
pub use manifest::{
    AlignRecord, BlobKind, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef, SourceRef,
    SourceResolution,
};
pub use review::ReviewMetrics;
pub use seal::{SealRequest, SealedProject, seal, seal_at};

use crate::params::{ParamDesc, ParamKind, ParamValue};
use crate::tile::Rect;

/// The project-level parameter schema: values that describe how a project
/// was prepared rather than any one algorithm's own settings. One
/// descriptor today, `auto-crop`, a project value rather than an algorithm
/// parameter -- rendered by the GUI's settings panel through the same
/// generator every algorithm's schema goes through, and read by
/// `focusforge-cli stack`'s `--no-crop` flag so the two never drift.
pub static PROJECT_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![ParamDesc {
        id: "auto-crop".to_string(),
        label: "Auto-crop".to_string(),
        help: "Trims the composite and Z-Map to the region every aligned \
               frame actually covers. Off keeps the full frame extent."
            .to_string(),
        kind: ParamKind::Bool,
        default: ParamValue::Bool(true),
        auto: None,
        host_default: None,
    }]
});

/// The on-disk format version this build writes and reads. A project
/// declaring a **higher** one is refused by name rather than
/// half-understood; a lower one is accepted.
pub const FORMAT_VERSION: u32 = 1;

/// The project file's extension.
pub const PROJECT_EXTENSION: &str = "ffp";

/// The content-hash algorithm, named in every manifest rather than implied.
pub const HASH_ALGORITHM: &str = "blake3";

/// Roughly how many decompressed bytes one band of [`Project::read_plane_into`]
/// holds. A reusable buffer of this size, never a whole plane -- the same
/// discipline [`blob::stage_plane`] uses on the way in.
const READ_BAND_BYTES: usize = 4 << 20;

/// A project opened for reading: enough to prove the round-trip and to drive
/// `focusforge-cli export`. Editing is not implemented by this type.
pub struct Project {
    path: PathBuf,
    archive: zip::ZipArchive<File>,
    manifest: Manifest,
    checkpoint: Checkpoint,
}

impl Project {
    /// Opens `path`, reads its manifest and its single checkpoint, and
    /// refuses a `format_version` newer than [`FORMAT_VERSION`].
    pub fn open(path: &Path) -> Result<Self, ProjectError> {
        let file = File::open(path).map_err(|source| ProjectError::io(path, source))?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|err| ProjectError::container(path, err))?;

        let manifest: Manifest = read_json(&mut archive, path, "manifest.json")?;
        if manifest.format_version > FORMAT_VERSION {
            return Err(ProjectError::FormatVersion {
                path: path.to_path_buf(),
                found: manifest.format_version,
                supported: FORMAT_VERSION,
            });
        }
        let checkpoint: Checkpoint = read_json(
            &mut archive,
            path,
            &format!("checkpoints/{}.json", manifest.checkpoint),
        )?;

        Ok(Project {
            path: path.to_path_buf(),
            archive,
            manifest,
            checkpoint,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }

    /// Where one recorded source is looked for: relative to the project's
    /// own directory when it was recorded as a descendant, otherwise the
    /// absolute path exactly as it was given.
    pub fn source_path(&self, source: &SourceRef) -> PathBuf {
        match source.resolution {
            SourceResolution::Relative => self
                .path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&source.path),
            SourceResolution::AsGiven => PathBuf::from(&source.path),
        }
    }

    /// Every source this project cannot resolve, for either of the two
    /// reasons a project has:
    ///
    /// - it was recorded [`SourceResolution::AsGiven`], because it was not a
    ///   descendant of the project's own directory and so was never portable
    ///   in the first place -- what an `-o <dir>` run produces by
    ///   construction; or
    /// - it was recorded relative and **is not there**, which is what a user
    ///   hits first on copying a project to another disk without its frames.
    ///
    /// Both are the same fact to whoever opens the project -- "the pixels
    /// these indices point at are not reachable from here" -- so they are one
    /// list rather than two.
    ///
    /// Returned as data, never printed: `core` neither prints nor links the
    /// GUI. The CLI is what names them on stderr, as a **warning**: a
    /// project with unresolvable sources says so on open, rather than
    /// refusing to open.
    pub fn unresolved_sources(&self) -> impl Iterator<Item = &SourceRef> {
        self.manifest
            .sources
            .iter()
            .filter(|s| s.resolution == SourceResolution::AsGiven || !self.source_path(s).is_file())
    }

    /// The [`PlaneRef`] of one payload plane.
    pub fn plane(&self, which: Plane) -> &PlaneRef {
        match which {
            Plane::Composite => &self.manifest.composite,
            Plane::ZMap => &self.manifest.zmap,
        }
    }

    /// Streams one payload plane out of the container, band by band, in
    /// top-to-bottom order.
    ///
    /// `sink` is handed a `&[u16]` of `rect.width * rect.height *
    /// samples_per_pixel` samples and the rect those samples cover, in
    /// plane coordinates. Never a whole-plane buffer: a band-sized reusable
    /// one, the same discipline the writer used.
    ///
    /// The blake3 hash of the decompressed bytes is verified **as they
    /// pass**, and a mismatch is a [`ProjectError::HashMismatch`] at the end
    /// of the stream rather than a corrupt image. A caller that writes what
    /// the sink hands it must therefore write to a `.partial` and rename
    /// only once this call returns `Ok` -- which is exactly what `export`
    /// does.
    pub fn read_plane_into(
        &mut self,
        which: Plane,
        sink: &mut impl FnMut(&[u16], Rect),
    ) -> Result<(), ProjectError> {
        let plane = self.plane(which).clone();
        let entry = format!("blobs/{}.zst", plane.blob);
        let expected_len =
            plane.width as u64 * plane.height as u64 * plane.samples_per_pixel as u64 * 2;

        let file = self
            .archive
            .by_name(&entry)
            .map_err(|err| ProjectError::container(&self.path, format!("{entry}: {err}")))?;
        let mut decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|source| ProjectError::io(&self.path, source))?;

        let row_samples = plane.width as usize * plane.samples_per_pixel as usize;
        let rows = (READ_BAND_BYTES / (row_samples * 2)).clamp(1, plane.height as usize) as u32;

        let mut hasher = blake3::Hasher::new();
        let mut bytes = vec![0u8; row_samples * rows as usize * 2];
        let mut samples = vec![0u16; row_samples * rows as usize];
        let mut total = 0u64;

        let mut y = 0;
        while y < plane.height {
            let band_rows = rows.min(plane.height - y);
            let len = row_samples * band_rows as usize;
            decoder
                .read_exact(&mut bytes[..len * 2])
                .map_err(|source| ProjectError::io(&self.path, source))?;
            hasher.update(&bytes[..len * 2]);
            total += (len * 2) as u64;
            for (sample, chunk) in samples[..len]
                .iter_mut()
                .zip(bytes[..len * 2].chunks_exact(2))
            {
                *sample = u16::from_le_bytes([chunk[0], chunk[1]]);
            }
            sink(
                &samples[..len],
                Rect {
                    x: 0,
                    y,
                    width: plane.width,
                    height: band_rows,
                },
            );
            y += band_rows;
        }

        if total != expected_len {
            return Err(ProjectError::container(
                &self.path,
                format!(
                    "blob {} holds {total} bytes, not the {expected_len} its plane declares",
                    plane.blob
                ),
            ));
        }
        let actual = hasher.finalize().to_hex().to_string();
        if actual != plane.blob {
            return Err(ProjectError::HashMismatch {
                path: self.path.clone(),
                blob: plane.blob.clone(),
                actual,
            });
        }
        Ok(())
    }
}

fn read_json<T: serde::de::DeserializeOwned>(
    archive: &mut zip::ZipArchive<File>,
    path: &Path,
    entry: &str,
) -> Result<T, ProjectError> {
    let mut text = String::new();
    archive
        .by_name(entry)
        .map_err(|err| ProjectError::container(path, format!("{entry}: {err}")))?
        .read_to_string(&mut text)
        .map_err(|source| ProjectError::io(path, source))?;
    serde_json::from_str(&text)
        .map_err(|err| ProjectError::container(path, format!("{entry}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_params_holds_exactly_one_auto_crop_bool_descriptor() {
        assert_eq!(PROJECT_PARAMS.len(), 1);
        let desc = &PROJECT_PARAMS[0];
        assert_eq!(desc.id, "auto-crop");
        assert_eq!(desc.label, "Auto-crop");
        assert_eq!(desc.kind, ParamKind::Bool);
        assert_eq!(desc.default, ParamValue::Bool(true));
        assert!(desc.auto.is_none());
        assert!(desc.host_default.is_none());
    }
}
