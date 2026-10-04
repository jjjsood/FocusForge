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
//! # Format commitments
//!
//! The format enumerates four commitments rather than leaving them to
//! whoever built the writer. Each is pinned by a named test in
//! `crates/core/tests/project_format.rs`:
//!
//! | Commitment | Where it lives | Test |
//! |---|---|---|
//! | 1. the entry layout | [`container`] | `container_layout_is_exactly_the_named_entries` |
//! | 2. a blob kind field | [`BlobRecord::kind`] | `blob_kind_is_recorded_and_required` |
//! | 3. a hash over uncompressed content, algorithm named | [`blob::stage_plane`], [`Manifest::hash_algorithm`] | `hash_is_over_uncompressed_content` |
//! | 4. a fully serialized `Checkpoint` with all four `Op` variants | [`Checkpoint`], [`Op`] | `checkpoint_serializes_every_field_with_no_parent_and_no_ops`, `every_op_variant_round_trips` |
//!
//! Two of the four are **invisible** failures rather than loud ones -- an
//! absent blob kind or a hash taken over compressed bytes produces a
//! project that opens without error and cannot be extended later.
//!
//! # What this format deliberately does not do
//!
//! No undo/redo kept across a session, no merging two branches, no drawn
//! tree or graph, no browser.
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
pub mod export;
pub mod manifest;
mod render;
pub mod review;
pub mod seal;
pub mod validate;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub use append::{
    AppendRequest, Deleted, SNAPSHOT_BYTE_BUDGET, SNAPSHOT_CHECKPOINT_CAP, ZMapChain, ZMapStorage,
    append, append_at, append_ops, append_ops_at, append_ops_onto, delete_checkpoint,
    replay_ops_to,
};
pub use blob::{Plane, StagedBlob, hash_file, short_hash, stack_fingerprint};
pub use checkpoint::{Checkpoint, FuseRecord, History, Op, Point};
pub use container::{ProjectWriter, partial_path};
pub use error::ProjectError;
pub use export::{export_checkpoint, export_zmap};
pub use manifest::{
    AlignRecord, BlobKind, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef, SourceRef,
    SourceResolution,
};
pub use render::{CheckpointImage, ZmapSource, render_checkpoint};
pub use review::ReviewMetrics;
pub use seal::{
    HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, ParentRef, SealRequest, SealedProject,
    kept_entries, now_unix, seal, seal_at, seal_in_memory,
};
pub use validate::{
    FingerprintMismatch, Opened, PendingProject, SourceCheck, Validation, check_sources,
};

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

/// The composite extent (width, height, frame count) of `id`'s fused
/// ancestor.
pub(super) fn fused_extent(project: &Project, id: &str) -> Result<(u32, u32, u32), ProjectError> {
    let record = project.fuse_state(id)?;
    Ok((
        record.composite.width,
        record.composite.height,
        record.composite.frame_count,
    ))
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
    let mut plane_ref = project.manifest().zmap.clone();
    (plane_ref.width, plane_ref.height, plane_ref.frame_count) =
        fused_extent(project, &checkpoint.id)?;
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
pub(super) fn resolve_chain<'a>(
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
                    None => Err(ProjectError::MissingEntry {
                        path: path.to_path_buf(),
                        entry: format!("blobs/{current}.zst"),
                    }),
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
/// was prepared rather than any one algorithm's own settings: `align`, then
/// `auto-crop`. Project values, not algorithm parameters -- rendered by the
/// GUI's settings panel through the same generator every algorithm's schema
/// goes through, and read by `focusforge-cli stack`'s `--no-align` and
/// `--no-crop` flags so the two never drift.
pub static PROJECT_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "align".to_string(),
            label: "Enabled".to_string(),
            help: "Registers every frame onto the reference before fusing. \
                   Off fuses the frames as read."
                .to_string(),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(true),
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "auto-crop".to_string(),
            label: "Auto-crop".to_string(),
            help: "Trims the composite and Z-Map to the region every aligned \
                   frame actually covers. Off keeps the full frame extent."
                .to_string(),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(true),
            auto: None,
            host_default: None,
        },
    ]
});

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
    history: History,
    /// Every blob hashed since open, in the order it was read: `(hash,
    /// uncompressed bytes)`. Verification is lazy -- a blob outside this
    /// list has not been checked yet.
    verified: Vec<(String, u64)>,
    /// The fingerprint mismatch [`PendingProject::proceed`] accepted to
    /// produce this project, carried forward as a warning.
    accepted_mismatch: Option<FingerprintMismatch>,
    /// A folder [`Self::relocate_sources`] pointed every source at, in
    /// memory only -- never written to the `.ffp`.
    relocated: Option<PathBuf>,
}

impl Project {
    /// Opens `path`: refuses a foreign file, a truncated container or a
    /// manifest or checkpoint that does not parse; refuses a structurally
    /// broken history (a missing blob, an unresolvable Z-Map chain, an
    /// op-less checkpoint without a fuse record, a `stack_fingerprint`
    /// disagreeing with its sources); verifies the head's own Z-Map chain
    /// by hash. Never reads a source frame.
    pub fn open(path: &Path) -> Result<Self, ProjectError> {
        let mut file = File::open(path).map_err(|source| ProjectError::io(path, source))?;
        let mut magic = [0u8; 4];
        match file.read_exact(&mut magic) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(ProjectError::NotAProject {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => return Err(ProjectError::io(path, source)),
        }
        if magic != *b"PK\x03\x04" && magic != *b"PK\x05\x06" {
            return Err(ProjectError::NotAProject {
                path: path.to_path_buf(),
            });
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|source| ProjectError::io(path, source))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|_| ProjectError::Truncated {
            path: path.to_path_buf(),
        })?;

        if archive.index_for_name("manifest.json").is_none() {
            return Err(ProjectError::NotAProject {
                path: path.to_path_buf(),
            });
        }
        let mut manifest_text = String::new();
        archive
            .by_name("manifest.json")
            .map_err(|err| ProjectError::container(path, format!("manifest.json: {err}")))?
            .read_to_string(&mut manifest_text)
            .map_err(|source| ProjectError::io(path, source))?;

        let manifest: Manifest = serde_json::from_str(&manifest_text)
            .map_err(|err| ProjectError::container(path, format!("manifest.json: {err}")))?;

        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        let mut checkpoints = Vec::new();
        for name in names {
            if let Some(rest) = name.strip_prefix("checkpoints/")
                && rest.ends_with(".json")
            {
                checkpoints.push(read_json::<Checkpoint>(&mut archive, path, &name)?);
            }
        }
        let history = History::build(checkpoints, path)?;
        let checkpoint = history
            .get(&manifest.checkpoint)
            .map_err(|_| ProjectError::MissingEntry {
                path: path.to_path_buf(),
                entry: format!("checkpoints/{}.json", manifest.checkpoint),
            })?
            .clone();

        append::verify_blobs_present(&mut archive, &manifest, path)?;
        for c in history.all() {
            resolve_chain(&manifest.blobs, &c.zmap_blob, path)?;
            match &c.fuse {
                Some(fuse) => {
                    if !manifest.blobs.iter().any(|b| b.hash == fuse.composite.blob) {
                        return Err(ProjectError::MissingEntry {
                            path: path.to_path_buf(),
                            entry: format!("blobs/{}.zst", fuse.composite.blob),
                        });
                    }
                    if c.stack_fingerprint != blob::stack_fingerprint(&fuse.sources) {
                        return Err(ProjectError::FingerprintDisagrees {
                            path: path.to_path_buf(),
                            checkpoint: c.id.clone(),
                        });
                    }
                }
                None if c.ops.is_empty() => {
                    return Err(ProjectError::container(
                        path,
                        format!(
                            "checkpoints/{}.json: an op-less checkpoint has no fuse record",
                            c.id
                        ),
                    ));
                }
                None => {
                    let Some(parent_id) = c.parent.as_deref() else {
                        return Err(ProjectError::OpsWithoutParent {
                            path: path.to_path_buf(),
                            checkpoint: c.id.clone(),
                        });
                    };
                    let parent = history.get(parent_id)?;
                    if c.stack_fingerprint != parent.stack_fingerprint {
                        return Err(ProjectError::FingerprintDisagrees {
                            path: path.to_path_buf(),
                            checkpoint: c.id.clone(),
                        });
                    }
                }
            }
        }
        if manifest.stack_fingerprint != checkpoint.stack_fingerprint
            || manifest.stack_fingerprint != blob::stack_fingerprint(&manifest.sources)
        {
            return Err(ProjectError::FingerprintDisagrees {
                path: path.to_path_buf(),
                checkpoint: checkpoint.id.clone(),
            });
        }

        let (base, deltas) = resolve_chain(&manifest.blobs, &checkpoint.zmap_blob, path)?;
        let mut base_ref = manifest.zmap.clone();
        base_ref.blob = base.hash.clone();
        let delta_hashes: Vec<String> = deltas.iter().map(|d| d.hash.clone()).collect();

        let mut project = Project {
            path: path.to_path_buf(),
            archive,
            manifest,
            checkpoint,
            history,
            verified: Vec::new(),
            accepted_mismatch: None,
            relocated: None,
        };
        project.read_plane_ref_into(&base_ref, &mut |_, _| {})?;
        for hash in &delta_hashes {
            project.read_blob_whole(hash)?;
        }
        Ok(project)
    }

    /// Every blob hashed since open, in the order it was verified: `(hash,
    /// uncompressed bytes)`. Verification is lazy, so a blob this project
    /// has not read yet is not in this list.
    pub fn verified_blobs(&self) -> &[(String, u64)] {
        &self.verified
    }

    /// The fingerprint mismatch a [`PendingProject::proceed`] accepted to
    /// produce this project, plus the count of sources this project cannot
    /// resolve, as data rather than a printed message.
    pub fn validation(&self) -> Validation {
        Validation {
            unresolved_sources: self.unresolved_sources().count(),
            accepted_mismatch: self.accepted_mismatch.clone(),
        }
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

    /// Every checkpoint of this project, indexed and validated.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// The whole checkpoint chain, head first, walking [`Checkpoint::parent`]
    /// down to the original parentless one.
    pub fn checkpoint_chain(&self) -> Result<Vec<Checkpoint>, ProjectError> {
        Ok(self
            .history
            .ancestors(&self.checkpoint.id)?
            .into_iter()
            .cloned()
            .collect())
    }

    /// The composite, alignment, crop, sources and metrics of the fuse
    /// `checkpoint` sits under: the nearest op-less ancestor's own
    /// [`Checkpoint::fuse`] record.
    pub fn fuse_state(&self, checkpoint: &str) -> Result<FuseRecord, ProjectError> {
        Ok(self
            .fused_ancestor(checkpoint)?
            .fuse
            .clone()
            .expect("open refuses an op-less checkpoint without a fuse record"))
    }

    /// The nearest ancestor of `id` (self included) whose `ops` is empty --
    /// the checkpoint whose fuse `id`'s Z-Map renders against.
    pub(super) fn fused_ancestor(&self, id: &str) -> Result<&Checkpoint, ProjectError> {
        self.history
            .ancestors(id)?
            .into_iter()
            .find(|c| c.ops.is_empty())
            .ok_or_else(|| {
                ProjectError::container(&self.path, "no fused ancestor in the checkpoint chain")
            })
    }

    /// The path from `checkpoint`'s own Z-Map back to its nearest full
    /// snapshot: the snapshot's own compressed size, and the count and
    /// total compressed size of the deltas resolved on top of it. Never
    /// refuses on length -- the chain-length cap applies when writing, not
    /// when reading.
    pub fn zmap_chain(&self, checkpoint: &Checkpoint) -> Result<ZMapChain, ProjectError> {
        let (base, deltas) =
            resolve_chain(&self.manifest.blobs, &checkpoint.zmap_blob, &self.path)?;
        Ok(ZMapChain {
            snapshot_bytes: base.compressed_len,
            deltas: deltas.len(),
            delta_bytes: deltas.iter().map(|d| d.compressed_len).sum(),
        })
    }

    /// Where one recorded source is looked for: under a located folder
    /// ([`Self::relocate_sources`]) when one is set, by its recorded file
    /// name; otherwise relative to the project's own directory when it was
    /// recorded as a descendant, or the absolute path exactly as it was
    /// given.
    pub fn source_path(&self, source: &SourceRef) -> PathBuf {
        if let Some(folder) = &self.relocated {
            let name = Path::new(&source.path)
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&source.path));
            return folder.join(name);
        }
        match source.resolution {
            SourceResolution::Relative => self
                .path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&source.path),
            SourceResolution::AsGiven => PathBuf::from(&source.path),
        }
    }

    /// Points every recorded source at `folder`, by file name, in memory
    /// only -- nothing is written to the `.ffp`. What a `Locate frames…`
    /// resolution applies once the folder has been verified
    /// ([`check_sources`]) against the sources' recorded content.
    pub fn relocate_sources(&mut self, folder: PathBuf) {
        self.relocated = Some(folder);
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
        self.verified
            .push((expected_hash.to_string(), bytes.len() as u64));
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
        self.verified.push((plane.blob.clone(), total));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_params_holds_the_align_and_auto_crop_bool_descriptors() {
        assert_eq!(PROJECT_PARAMS.len(), 2);
        for (desc, id, label) in [
            (&PROJECT_PARAMS[0], "align", "Enabled"),
            (&PROJECT_PARAMS[1], "auto-crop", "Auto-crop"),
        ] {
            assert_eq!(desc.id, id);
            assert_eq!(desc.label, label);
            assert_eq!(desc.kind, ParamKind::Bool);
            assert_eq!(desc.default, ParamValue::Bool(true));
            assert!(desc.auto.is_none());
            assert!(desc.host_default.is_none());
        }
    }
}
