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
//! No undo/redo kept across a session, no checkpoint DAG beyond a single
//! chain, no branch-from-any-checkpoint, no browser.
//!
//! # What `core` does not do
//!
//! Nothing here prints. Unresolved sources come back as **data**
//! ([`Project::unresolved_sources`]) and reporting them is the CLI's.

pub mod append;
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

pub use append::{
    AppendRequest, SNAPSHOT_BYTE_BUDGET, SNAPSHOT_CHECKPOINT_CAP, ZMapChain, ZMapStorage, append,
    append_at, append_ops, append_ops_at,
};
pub use blob::{Plane, StagedBlob, hash_file, short_hash, stack_fingerprint};
pub use checkpoint::{Checkpoint, Op, Point};
pub use container::{ProjectWriter, partial_path};
pub use error::ProjectError;
pub use manifest::{
    AlignRecord, BlobKind, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef, SourceRef,
    SourceResolution,
};
pub use review::ReviewMetrics;
pub use seal::{HeldCheckpoint, SealRequest, SealedProject, seal, seal_at, seal_held};

/// Decompresses `which` into an uncompressed TIFF (the composite) or Z-Map
/// file at `path`: writes a `.partial` sibling first and renames it onto
/// `path` only once [`Project::read_plane_into`] has verified the blob's
/// hash, so a tampered blob leaves nothing behind rather than a corrupt
/// image.
pub fn materialize_plane(
    project: &mut Project,
    which: Plane,
    path: &Path,
) -> Result<(), ProjectError> {
    match which {
        Plane::Composite => {
            let plane_ref = project.plane(which).clone();
            let partial = partial_path(path);
            let writer = crate::io::TiffWriter::create(
                &partial,
                crate::io::TiffSpec {
                    width: plane_ref.width,
                    height: plane_ref.height,
                    samples_per_pixel: plane_ref.samples_per_pixel,
                    description: Some(plane_ref.description.clone()),
                },
            )
            .map_err(|err| ProjectError::container(&partial, err))?;
            stream_plane(project, which, &partial, path, writer)
        }
        // The head's own Z-Map may be stored as a chain of patches over an
        // ancestor's materialized map -- exactly what `materialize_zmap`
        // already resolves for any checkpoint, head included.
        Plane::ZMap => {
            let head = project.checkpoint().clone();
            materialize_zmap(project, &head, path)
        }
    }
}

/// The `finish()`/`write_tile()` half of both writers [`materialize_plane`]
/// drives, so the streaming loop below has one shape rather than two
/// near-copies.
trait PlaneWriter {
    fn write_tile(&mut self, rect: crate::io::Rect, samples: &[u16]) -> Result<(), String>;
    fn finish(self) -> Result<(), String>;
}

impl PlaneWriter for crate::io::TiffWriter<std::fs::File> {
    fn write_tile(&mut self, rect: crate::io::Rect, samples: &[u16]) -> Result<(), String> {
        crate::io::TiffWriter::write_tile(self, rect, samples).map_err(|err| err.to_string())
    }
    fn finish(self) -> Result<(), String> {
        crate::io::TiffWriter::finish(self).map_err(|err| err.to_string())
    }
}

impl PlaneWriter for crate::zmap::ZMapWriter<std::fs::File> {
    fn write_tile(&mut self, rect: crate::io::Rect, samples: &[u16]) -> Result<(), String> {
        crate::zmap::ZMapWriter::write_tile(self, rect, samples).map_err(|err| err.to_string())
    }
    fn finish(self) -> Result<(), String> {
        crate::zmap::ZMapWriter::finish(self).map_err(|err| err.to_string())
    }
}

fn stream_plane<W: PlaneWriter>(
    project: &mut Project,
    which: Plane,
    partial: &Path,
    final_path: &Path,
    writer: W,
) -> Result<(), ProjectError> {
    let plane_ref = project.plane(which).clone();
    let outcome = stream_bands(project, &plane_ref, final_path, writer)
        .and_then(|writer| {
            writer
                .finish()
                .map_err(|err| ProjectError::container(final_path, err))
        })
        .and_then(|()| {
            std::fs::rename(partial, final_path).map_err(|err| ProjectError::io(final_path, err))
        });
    if outcome.is_err() {
        let _ = std::fs::remove_file(partial);
    }
    outcome
}

/// The `read_plane_ref_into`/`write_tile` loop [`stream_plane`] and
/// [`materialize_zmap`] both drive, stopping short of `finish()` -- the
/// step where their two callers part ways: one renames the `.partial`
/// immediately afterward, the other still has patches to apply to it first.
fn stream_bands<W: PlaneWriter>(
    project: &mut Project,
    plane_ref: &PlaneRef,
    final_path: &Path,
    mut writer: W,
) -> Result<W, ProjectError> {
    let mut failure: Option<String> = None;
    project.read_plane_ref_into(plane_ref, &mut |band, rect| {
        if failure.is_some() {
            return;
        }
        if let Err(err) = writer.write_tile(rect, band) {
            failure = Some(err);
        }
    })?;
    match failure {
        Some(err) => Err(ProjectError::container(final_path, err)),
        None => Ok(writer),
    }
}

/// Streams a Z-Map that is not necessarily the project's head: `checkpoint`
/// names its own `zmap_blob`, resolved against [`Manifest::blobs`] to
/// either a `full` record read directly or a chain of `sparse-delta`
/// records patched, oldest first, over their materialized base -- never
/// more than one full map resident at a time. What a `ResetRegion` needs to
/// replay against an ancestor other than the immediate parent, and what
/// [`materialize_plane`]`(Plane::ZMap)` delegates to for the head.
pub fn materialize_zmap(
    project: &mut Project,
    checkpoint: &Checkpoint,
    path: &Path,
) -> Result<(), ProjectError> {
    let plane_ref = project.manifest().zmap.clone();
    let blobs = project.manifest().blobs.clone();
    let (base, deltas) = resolve_chain(&blobs, &checkpoint.zmap_blob, project.path())?;

    let mut base_ref = plane_ref.clone();
    base_ref.blob = base.hash.clone();

    let partial = partial_path(path);
    let info = crate::zmap::ZMapInfo::new(plane_ref.width, plane_ref.height, plane_ref.frame_count)
        .map_err(|err| ProjectError::container(&partial, err))?;
    let writer = crate::zmap::ZMapWriter::create(&partial, info)
        .map_err(|err| ProjectError::container(&partial, err))?;

    let outcome = materialize_zmap_onto_partial(
        project, &base_ref, &deltas, &plane_ref, path, &partial, writer,
    );
    if outcome.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    outcome
}

fn materialize_zmap_onto_partial(
    project: &mut Project,
    base_ref: &PlaneRef,
    deltas: &[&BlobRecord],
    plane_ref: &PlaneRef,
    final_path: &Path,
    partial: &Path,
    writer: crate::zmap::ZMapWriter<File>,
) -> Result<(), ProjectError> {
    let writer = stream_bands(project, base_ref, final_path, writer)?;
    writer
        .finish()
        .map_err(|err| ProjectError::container(final_path, err))?;

    let whole = Rect {
        x: 0,
        y: 0,
        width: plane_ref.width,
        height: plane_ref.height,
    };
    let info = crate::zmap::ZMapInfo::new(plane_ref.width, plane_ref.height, plane_ref.frame_count)
        .map_err(|err| ProjectError::container(final_path, err))?;

    for delta in deltas {
        let bytes = project.read_blob_whole(&delta.hash)?;
        let decoded = blob::decode_delta(&bytes, final_path)?;
        if decoded.map_width != plane_ref.width || decoded.map_height != plane_ref.height {
            return Err(ProjectError::PatchGeometryMismatch {
                path: final_path.to_path_buf(),
                blob: delta.hash.clone(),
                found_width: decoded.map_width,
                found_height: decoded.map_height,
                expected_width: plane_ref.width,
                expected_height: plane_ref.height,
            });
        }
        let mut patch_writer = crate::zmap::ZMapWriter::reopen(partial, info)
            .map_err(|err| ProjectError::container(final_path, err))?;
        patch_writer.seed_coverage(whole);
        patch_writer
            .write_tile(decoded.tile, &decoded.indices)
            .map_err(|err| ProjectError::container(final_path, err))?;
        patch_writer
            .finish()
            .map_err(|err| ProjectError::container(final_path, err))?;
    }

    std::fs::rename(partial, final_path).map_err(|err| ProjectError::io(final_path, err))
}

/// The one full [`BlobRecord`] `target` resolves to, plus every
/// `sparse-delta` on the path to it, **oldest first** -- so a caller applies
/// them over the base in the order they were originally recorded. Bounded
/// by `blobs.len()` steps: a chain that has not resolved by then can only be
/// a cycle, which [`Manifest::blobs`] should never contain.
fn resolve_chain<'a>(
    blobs: &'a [BlobRecord],
    target: &str,
    path: &Path,
) -> Result<(&'a BlobRecord, Vec<&'a BlobRecord>), ProjectError> {
    let mut newest_first: Vec<&BlobRecord> = Vec::new();
    let mut current = target.to_string();
    for _ in 0..=blobs.len() {
        if let Some(full) = blobs
            .iter()
            .find(|b| b.kind == BlobKind::Full && b.hash == current)
        {
            let mut deltas = newest_first;
            deltas.reverse();
            return Ok((full, deltas));
        }
        let delta = blobs.iter().find(|b| {
            b.kind == BlobKind::SparseDelta && b.target.as_deref() == Some(current.as_str())
        });
        match delta {
            Some(delta) => {
                let base = delta.base.clone().ok_or_else(|| {
                    ProjectError::container(
                        path,
                        format!(
                            "blob {} is stored as a patch with no base of its own",
                            delta.hash
                        ),
                    )
                })?;
                newest_first.push(delta);
                current = base;
            }
            None => {
                return match newest_first.last() {
                    Some(prev) => Err(ProjectError::PatchBaseMissing {
                        path: path.to_path_buf(),
                        blob: prev.hash.clone(),
                        base: current,
                    }),
                    None => Err(ProjectError::container(
                        path,
                        format!("no blob record names {current}"),
                    )),
                };
            }
        }
    }
    Err(ProjectError::container(
        path,
        format!("blob {current} sits in a chain longer than this project's own blob list"),
    ))
}

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
pub const FORMAT_VERSION: u32 = 2;

/// The format version a project keeps writing as long as every blob it
/// holds is [`BlobKind::Full`] -- what stays openable by a build that has
/// never heard of [`BlobKind::SparseDelta`].
pub const FULL_ONLY_FORMAT_VERSION: u32 = 1;

/// [`FULL_ONLY_FORMAT_VERSION`] while `blobs` holds only [`BlobKind::Full`]
/// records, [`FORMAT_VERSION`] the moment any of them is not -- the format
/// version decision folded into one place so every writer stamps the same
/// answer.
pub(crate) fn format_version_for(blobs: &[BlobRecord]) -> u32 {
    if blobs.iter().all(|b| b.kind == BlobKind::Full) {
        FULL_ONLY_FORMAT_VERSION
    } else {
        FORMAT_VERSION
    }
}

/// The project file's extension.
pub const PROJECT_EXTENSION: &str = "ffp";

/// The content-hash algorithm, named in every manifest rather than implied.
pub const HASH_ALGORITHM: &str = "blake3";

/// Roughly how many decompressed bytes one band of [`Project::read_plane_into`]
/// holds. A reusable buffer of this size, never a whole plane -- the same
/// discipline [`blob::stage_plane`] uses on the way in.
const READ_BAND_BYTES: usize = 4 << 20;

/// A project opened for reading: enough to prove the round-trip and to drive
/// `focusforge-cli export`. The GUI editor opens one for editing.
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

    /// The whole checkpoint chain, head first, walking [`Checkpoint::parent`]
    /// down to the original parentless one.
    pub fn checkpoint_chain(&mut self) -> Result<Vec<Checkpoint>, ProjectError> {
        checkpoint_chain_from(&mut self.archive, &self.path, &self.checkpoint.id)
    }

    /// The path from `checkpoint`'s own Z-Map back to its nearest full
    /// snapshot: the snapshot's own compressed size, and the count and
    /// total compressed size of the deltas resolved on top of it. Never
    /// refuses on length -- a chain longer than today's cap, written by an
    /// older or newer build, must still open.
    pub fn zmap_chain(&self, checkpoint: &Checkpoint) -> Result<ZMapChain, ProjectError> {
        let (base, deltas) =
            resolve_chain(&self.manifest.blobs, &checkpoint.zmap_blob, &self.path)?;
        Ok(ZMapChain {
            snapshot_bytes: base.compressed_len,
            deltas: deltas.len(),
            delta_bytes: deltas.iter().map(|d| d.compressed_len).sum(),
        })
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
        if which == Plane::ZMap {
            let head = self.checkpoint.clone();
            let stored_whole = self
                .manifest
                .blobs
                .iter()
                .any(|b| b.kind == BlobKind::Full && b.hash == head.zmap_blob);
            if !stored_whole {
                return self.read_zmap_head_via_materialize(&head, sink);
            }
        }
        let plane = self.plane(which).clone();
        self.read_plane_ref_into(&plane, sink)
    }

    /// [`Self::read_plane_into`]`(Plane::ZMap)` when the head is stored as a
    /// patch chain: materializes it into a uniquely named temp file (there
    /// is no blob entry named by `zmap_blob` to stream directly), streams
    /// that file back out in bands, then removes it either way.
    fn read_zmap_head_via_materialize(
        &mut self,
        head: &Checkpoint,
        sink: &mut impl FnMut(&[u16], Rect),
    ) -> Result<(), ProjectError> {
        let temp = std::env::temp_dir().join(format!(
            "focusforge-zmap-{}-{}.tif",
            std::process::id(),
            head.zmap_blob
        ));
        let result = materialize_zmap(self, head, &temp).and_then(|()| {
            let reader = crate::zmap::ZMapReader::open(&temp)
                .map_err(|err| ProjectError::container(&temp, err))?;
            let (width, height) = reader.dimensions();
            let row_samples = width as usize;
            let rows = (READ_BAND_BYTES / (row_samples * 2)).clamp(1, height as usize) as u32;
            let mut buf = vec![0u16; row_samples * rows as usize];
            let mut y = 0;
            while y < height {
                let band_rows = rows.min(height - y);
                let rect = Rect {
                    x: 0,
                    y,
                    width,
                    height: band_rows,
                };
                let len = row_samples * band_rows as usize;
                reader
                    .read_tile_into(rect, &mut buf[..len])
                    .map_err(|err| ProjectError::container(&temp, err))?;
                sink(&buf[..len], rect);
                y += band_rows;
            }
            Ok(())
        });
        let _ = std::fs::remove_file(&temp);
        result
    }

    /// Reads and zstd-decompresses one blob entry whole, verifying its
    /// blake3 hash against `expected_hash` -- for a patch blob, small
    /// enough to hold entirely in memory unlike a payload plane's own
    /// banded read.
    fn read_blob_whole(&mut self, expected_hash: &str) -> Result<Vec<u8>, ProjectError> {
        let entry = format!("blobs/{expected_hash}.zst");
        let file = self
            .archive
            .by_name(&entry)
            .map_err(|err| ProjectError::container(&self.path, format!("{entry}: {err}")))?;
        let mut decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|source| ProjectError::io(&self.path, source))?;
        let mut bytes = Vec::new();
        decoder
            .read_to_end(&mut bytes)
            .map_err(|source| ProjectError::io(&self.path, source))?;
        let actual = blake3::hash(&bytes).to_hex().to_string();
        if actual != expected_hash {
            return Err(ProjectError::HashMismatch {
                path: self.path.clone(),
                blob: expected_hash.to_string(),
                actual,
            });
        }
        Ok(bytes)
    }

    /// [`Self::read_plane_into`]'s body, over an arbitrary [`PlaneRef`]
    /// rather than one of `self.manifest`'s own two -- what
    /// [`materialize_zmap`] uses to read a non-head checkpoint's own Z-Map
    /// blob under the manifest's declared shape.
    fn read_plane_ref_into(
        &mut self,
        plane: &PlaneRef,
        sink: &mut impl FnMut(&[u16], Rect),
    ) -> Result<(), ProjectError> {
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

pub(super) fn read_json<T: serde::de::DeserializeOwned>(
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

/// The checkpoint chain reachable from `head_id`, head first: walks
/// [`Checkpoint::parent`] down to the original parentless checkpoint. A
/// repeated id names itself in a [`ProjectError::Container`] rather than
/// looping forever.
fn checkpoint_chain_from(
    archive: &mut zip::ZipArchive<File>,
    path: &Path,
    head_id: &str,
) -> Result<Vec<Checkpoint>, ProjectError> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current_id = head_id.to_string();
    loop {
        if !seen.insert(current_id.clone()) {
            return Err(ProjectError::container(
                path,
                format!("checkpoint chain names {current_id} more than once"),
            ));
        }
        let checkpoint: Checkpoint =
            read_json(archive, path, &format!("checkpoints/{current_id}.json"))?;
        let parent = checkpoint.parent.clone();
        chain.push(checkpoint);
        match parent {
            Some(next) => current_id = next,
            None => return Ok(chain),
        }
    }
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
