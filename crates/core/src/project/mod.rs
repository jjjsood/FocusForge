// SPDX-License-Identifier: GPL-3.0-or-later

//! The project container: what a stacking run writes instead of an image.
//!
//! A project is one file. [`container`] describes its layout: a header,
//! records appended one after another and a footer that indexes the live
//! ones and commits them. Three kinds of record exist:
//!
//! ```text
//! manifest.json                  the head's description
//! blobs/<hash>.zst               a plane, zstd-compressed
//! checkpoints/<id>.json          one checkpoint
//! ```
//!
//! A save appends records and a footer and never rewrites a byte; a torn tail
//! after the last valid footer is ignored on open and cut off by the next
//! save. Nothing outside those three paths is ever written, and a test asserts
//! it (`container_layout_is_exactly_the_named_entries`). Two seals of the same
//! inputs produce identical bytes (`the_container_is_reproducible`).
//!
//! # Format commitments
//!
//! The format enumerates four commitments rather than leaving them to
//! whoever built the writer. Each is pinned by a named test in
//! `crates/core/tests/project_format.rs`:
//!
//! | Commitment | Where it lives | Test |
//! |---|---|---|
//! | 1. the record layout | [`container`] | `container_layout_is_exactly_the_named_entries` |
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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub use append::{
    AppendRequest, Deleted, append, append_at, append_op_chain, append_ops, append_ops_at,
    append_ops_onto, delete_checkpoint,
};
pub use blob::{FileStamp, Plane, StagedBlob, short_hash, source_hash, stack_fingerprint};
pub use checkpoint::{Checkpoint, FuseRecord, History, Op, Point};
pub use container::partial_path;
use container::{ContainerWriter, Index};
pub use error::ProjectError;
pub use export::{export_checkpoint, export_held, export_pending};
pub use manifest::{
    AlignRecord, BlobKind, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef, SourceRef,
    SourceResolution,
};
pub use render::{CheckpointImage, PendingState, render_checkpoint};
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
            stream_plane(project, &plane_ref, &partial, path, writer)
        }
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
    plane_ref: &PlaneRef,
    partial: &Path,
    final_path: &Path,
    writer: W,
) -> Result<(), ProjectError> {
    let outcome = stream_bands(project, plane_ref, final_path, writer)
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

/// The `read_plane_ref_into`/`write_tile` loop [`stream_plane`] drives,
/// stopping short of `finish()`.
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
/// names its own `zmap_blob`, resolved against [`Manifest::blobs`] to its
/// `full` record. What [`materialize_plane`]`(Plane::ZMap)` delegates to for
/// the head.
pub fn materialize_zmap(
    project: &mut Project,
    checkpoint: &Checkpoint,
    path: &Path,
) -> Result<(), ProjectError> {
    let mut plane_ref = project.manifest().zmap.clone();
    (plane_ref.width, plane_ref.height, plane_ref.frame_count) =
        fused_extent(project, &checkpoint.id)?;
    plane_ref.blob = full_blob(
        &project.manifest().blobs,
        &checkpoint.zmap_blob,
        project.path(),
    )?
    .hash
    .clone();

    let partial = partial_path(path);
    let info = crate::zmap::ZMapInfo::new(plane_ref.width, plane_ref.height, plane_ref.frame_count)
        .map_err(|err| ProjectError::container(&partial, err))?;
    let writer = crate::zmap::ZMapWriter::create(&partial, info)
        .map_err(|err| ProjectError::container(&partial, err))?;
    stream_plane(project, &plane_ref, &partial, path, writer)
}

/// The `full` [`BlobRecord`] named `hash`, or [`ProjectError::MissingEntry`]
/// when the project lists none.
pub(super) fn full_blob<'a>(
    blobs: &'a [BlobRecord],
    hash: &str,
    path: &Path,
) -> Result<&'a BlobRecord, ProjectError> {
    blobs
        .iter()
        .find(|b| b.kind == BlobKind::Full && b.hash == hash)
        .ok_or_else(|| ProjectError::MissingEntry {
            path: path.to_path_buf(),
            entry: blob_entry(hash),
        })
}

use crate::algorithm::StackAlgorithm;
use crate::equalize::EqualizeSettings;
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues, validate};
use crate::tile::Rect;

/// The alignment switch: `align`. The settings panel's align section renders
/// exactly this schema, ahead of the algorithm's own parameters.
pub static ALIGN_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![ParamDesc {
        id: "align".to_string(),
        label: "Enabled".to_string(),
        help: "Registers every frame onto the reference before fusing; \
               off fuses the frames as read."
            .to_string(),
        kind: ParamKind::Bool,
        default: ParamValue::Bool(true),
        advanced: false,
        auto: None,
        host_default: None,
    }]
});

/// The project parameters that act after fusing: `auto-crop`. The settings
/// panel's finish section renders exactly this schema.
pub static FINISH_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![ParamDesc {
        id: "auto-crop".to_string(),
        label: "Auto-crop".to_string(),
        help: "Trims the result to the area every aligned frame covers; turn \
                   it off to keep the full frame."
            .to_string(),
        kind: ParamKind::Bool,
        default: ParamValue::Bool(true),
        advanced: false,
        auto: None,
        host_default: None,
    }]
});

/// The project parameters that change the frames before alignment and
/// fusing: the brightness-equalization parameters. The settings panel's
/// prepare section renders exactly this schema.
pub static PREPARE_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "equalize".to_string(),
            label: "Equalize brightness".to_string(),
            help: "Gives every frame one gain, the same on every channel, \
                   bringing its mean brightness to one target shared by all \
                   frames before they are aligned; scales encoded code values \
                   as delivered and is not a linear-light correction."
                .to_string(),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(true),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "equalize-max-gain".to_string(),
            label: "Max brightness gain".to_string(),
            help: "Only used when brightness equalization is on; limits a \
                   frame's gain on both sides, so 1.25 allows gains from 0.8 \
                   to 1.25; scales encoded code values as delivered and is \
                   not a linear-light correction."
                .to_string(),
            kind: ParamKind::Float {
                min: 1.01,
                max: 2.0,
                step: 0.01,
                logarithmic: false,
            },
            default: ParamValue::Float(1.25),
            advanced: true,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "equalize-target".to_string(),
            label: "Target brightness (stops)".to_string(),
            help: "Only used when brightness equalization is on; 0.0 keeps the \
                   mean encoded brightness of the stacked sources and this \
                   value scales that shared target in stops (one stop doubles \
                   it), the same for every source; scales encoded code values \
                   as delivered and is not a linear-light correction, and \
                   source files stay unchanged."
                .to_string(),
            kind: ParamKind::Float {
                min: -0.5,
                max: 0.5,
                step: 0.1,
                logarithmic: false,
            },
            default: ParamValue::Float(0.0),
            advanced: true,
            auto: None,
            host_default: None,
        },
    ]
});

/// The project-level parameter schema, [`ALIGN_PARAMS`], [`FINISH_PARAMS`] then
/// [`PREPARE_PARAMS`]: values that describe how a project was prepared rather
/// than any one algorithm's own settings. `focusforge-cli stack` reads it
/// (`--no-crop`, `--set`) and the `algorithms` listing prints it.
pub static PROJECT_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    ALIGN_PARAMS
        .iter()
        .chain(FINISH_PARAMS.iter())
        .chain(PREPARE_PARAMS.iter())
        .cloned()
        .collect()
});

/// The project parameter schema for a run fused by `method`:
/// [`PROJECT_PARAMS`] with the defaults the method declares through
/// [`StackAlgorithm::project_defaults`]. Every front end resolves project
/// parameters against this schema, so a default is the same in the settings
/// panel, on the command line and in a listing.
pub fn params_for(method: &dyn StackAlgorithm) -> Vec<ParamDesc> {
    params_for_group(&PROJECT_PARAMS, method)
}

/// `group` ([`ALIGN_PARAMS`], [`PREPARE_PARAMS`], [`FINISH_PARAMS`] or [`PROJECT_PARAMS`]) with
/// the defaults `method` declares through [`StackAlgorithm::project_defaults`].
/// The declarations are checked against the whole project schema, so one for a
/// parameter outside `group` is still validated.
pub fn params_for_group(group: &[ParamDesc], method: &dyn StackAlgorithm) -> Vec<ParamDesc> {
    let declared: ParamValues = method
        .project_defaults()
        .iter()
        .map(|(id, value)| (id.to_string(), value.clone()))
        .collect();
    validate(&PROJECT_PARAMS, &declared)
        .expect("a method's project defaults satisfy the project schema");
    group
        .iter()
        .map(|desc| {
            let mut desc = desc.clone();
            if let Some(default) = declared.get(&desc.id) {
                desc.default = default.clone();
            }
            desc
        })
        .collect()
}

/// The settings brightness equalization runs with, `None` when the
/// `equalize` switch is off. An absent key reads as its default in `schema`,
/// the schema [`params_for`] returns for the run's fusion method.
pub fn equalization_settings(
    schema: &[ParamDesc],
    values: &ParamValues,
) -> Option<EqualizeSettings> {
    let read = |id: &str| {
        let desc = schema
            .iter()
            .find(|d| d.id == id)
            .expect("the project schema declares the equalization parameters");
        values.get(id).unwrap_or(&desc.default).clone()
    };
    read("equalize").as_bool().filter(|&on| on).and_then(|_| {
        Some(EqualizeSettings {
            max_gain: read("equalize-max-gain").as_f64()?,
            target_stops: read("equalize-target").as_f64()?,
        })
    })
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
    file: File,
    /// The records of the committed state; [`Self::index`] adds a pending
    /// save's.
    index: Index,
    /// The appends of a save in progress, written by [`Self::commit`].
    pending: Option<ContainerWriter>,
    /// Whether `manifest` changed since it was last written.
    manifest_changed: bool,
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
    /// broken history (a missing blob, a Z-Map blob with no record, an
    /// op-less checkpoint without a fuse record, a `stack_fingerprint`
    /// disagreeing with its sources); verifies the head's own Z-Map
    /// by hash. Never reads a source frame.
    pub fn open(path: &Path) -> Result<Self, ProjectError> {
        let file = File::open(path).map_err(|source| ProjectError::io(path, source))?;
        let index = Index::read_from(&file, path)?;
        if !index.contains("manifest.json") {
            return Err(ProjectError::NotAProject {
                path: path.to_path_buf(),
            });
        }
        let manifest: Manifest = read_json(&file, &index, path, "manifest.json")?;

        let mut checkpoints = Vec::new();
        for name in index.names() {
            if let Some(rest) = name.strip_prefix("checkpoints/")
                && rest.ends_with(".json")
            {
                checkpoints.push(read_json::<Checkpoint>(&file, &index, path, name)?);
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

        for blob in &manifest.blobs {
            let name = blob_entry(&blob.hash);
            if !index.contains(&name) {
                return Err(ProjectError::MissingEntry {
                    path: path.to_path_buf(),
                    entry: name,
                });
            }
        }
        for c in history.all() {
            full_blob(&manifest.blobs, &c.zmap_blob, path)?;
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

        let mut head_ref = manifest.zmap.clone();
        head_ref.blob = checkpoint.zmap_blob.clone();

        let mut project = Project {
            path: path.to_path_buf(),
            file,
            index,
            pending: None,
            manifest_changed: false,
            manifest,
            checkpoint,
            history,
            verified: Vec::new(),
            accepted_mismatch: None,
            relocated: None,
        };
        project.read_plane_ref_into(&head_ref, &mut |_, _| {})?;
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

    /// The records of this project, a save in progress included.
    pub(super) fn index(&self) -> &Index {
        self.pending
            .as_ref()
            .map_or(&self.index, ContainerWriter::index)
    }

    fn writer(&mut self) -> Result<&mut ContainerWriter, ProjectError> {
        if self.pending.is_none() {
            self.pending = Some(ContainerWriter::append(&self.path, &self.index)?);
        }
        Ok(self.pending.as_mut().expect("the writer was just opened"))
    }

    /// Adds the staged blob to the pending save, unless the project already
    /// holds that hash.
    pub(super) fn put_blob(&mut self, blob: &StagedBlob) -> Result<(), ProjectError> {
        let mut source =
            File::open(&blob.path).map_err(|source| ProjectError::io(&blob.path, source))?;
        self.writer()?.put(&blob_entry(&blob.hash), &mut source)
    }

    /// Adds `checkpoint` to the pending save and to the history reads see.
    pub(super) fn put_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<(), ProjectError> {
        let bytes = serde_json::to_vec(checkpoint)
            .map_err(|err| ProjectError::container(&self.path, err))?;
        self.writer()?
            .put_bytes(&checkpoint_entry(&checkpoint.id), &bytes)?;
        let mut all: Vec<Checkpoint> = self
            .history
            .all()
            .into_iter()
            .filter(|c| c.id != checkpoint.id)
            .cloned()
            .collect();
        all.push(checkpoint.clone());
        self.history = History::build(all, &self.path)?;
        Ok(())
    }

    /// Makes `manifest` the project's manifest and its checkpoint the head;
    /// the record is written once, by [`Self::commit`]. The head's
    /// checkpoint must be in the history already.
    pub(super) fn set_manifest(&mut self, manifest: Manifest) -> Result<(), ProjectError> {
        self.checkpoint = self.history.get(&manifest.checkpoint)?.clone();
        self.manifest = manifest;
        self.manifest_changed = true;
        Ok(())
    }

    /// Stops listing the checkpoints `ids` in the pending save and in the
    /// history. A checkpoint is dropped with every one built on it.
    pub(super) fn drop_checkpoints(&mut self, ids: &[String]) -> Result<(), ProjectError> {
        for id in ids {
            self.writer()?.drop_entry(&checkpoint_entry(id));
        }
        let rest: Vec<Checkpoint> = self
            .history
            .all()
            .into_iter()
            .filter(|c| !ids.contains(&c.id))
            .cloned()
            .collect();
        self.history = History::build(rest, &self.path)?;
        Ok(())
    }

    /// Stops listing the blob `hash` in the pending save.
    pub(super) fn drop_blob(&mut self, hash: &str) -> Result<(), ProjectError> {
        self.writer()?.drop_entry(&blob_entry(hash));
        Ok(())
    }

    /// Writes the manifest if it changed, then the footer: every record put
    /// since the project was opened becomes the committed state at once. After
    /// an error the project is stale; reopen it.
    pub(super) fn commit(&mut self) -> Result<(), ProjectError> {
        if self.manifest_changed {
            let bytes = serde_json::to_vec(&self.manifest)
                .map_err(|err| ProjectError::container(&self.path, err))?;
            self.writer()?.put_bytes("manifest.json", &bytes)?;
            self.manifest_changed = false;
        }
        if let Some(writer) = self.pending.take() {
            self.index = writer.finish()?;
        }
        Ok(())
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
        self.read_plane_ref_into(&plane, sink)
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
        let expected_len =
            plane.width as u64 * plane.height as u64 * plane.samples_per_pixel as u64 * 2;

        let file = container::record_reader(
            &self.file,
            self.index(),
            &self.path,
            &blob_entry(&plane.blob),
        )?;
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

/// The record name of the blob with content hash `hash`.
pub(super) fn blob_entry(hash: &str) -> String {
    format!("blobs/{hash}.zst")
}

pub(super) fn checkpoint_entry(id: &str) -> String {
    format!("checkpoints/{id}.json")
}

fn read_json<T: serde::de::DeserializeOwned>(
    file: &File,
    index: &Index,
    path: &Path,
    entry: &str,
) -> Result<T, ProjectError> {
    let mut bytes = Vec::new();
    container::record_reader(file, index, path, entry)?
        .read_to_end(&mut bytes)
        .map_err(|source| ProjectError::io(path, source))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| ProjectError::container(path, format!("{entry}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str) -> &'static ParamDesc {
        PROJECT_PARAMS
            .iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("no project parameter {id}"))
    }

    #[test]
    fn project_params_hold_align_auto_crop_and_the_equalization_descriptors() {
        let ids: Vec<&str> = PROJECT_PARAMS.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "align",
                "auto-crop",
                "equalize",
                "equalize-max-gain",
                "equalize-target"
            ]
        );

        let align = desc("align");
        assert_eq!(align.label, "Enabled");
        assert_eq!(align.kind, ParamKind::Bool);
        assert_eq!(align.default, ParamValue::Bool(true));
        assert!(!align.advanced);

        let crop = desc("auto-crop");
        assert_eq!(crop.label, "Auto-crop");
        assert_eq!(crop.kind, ParamKind::Bool);
        assert_eq!(crop.default, ParamValue::Bool(true));
        assert!(!crop.advanced);

        let on = desc("equalize");
        assert_eq!(on.label, "Equalize brightness");
        assert_eq!(on.kind, ParamKind::Bool);
        assert_eq!(on.default, ParamValue::Bool(true));

        let bound = desc("equalize-max-gain");
        assert_eq!(bound.label, "Max brightness gain");
        assert_eq!(
            bound.kind,
            ParamKind::Float {
                min: 1.01,
                max: 2.0,
                step: 0.01,
                logarithmic: false
            }
        );
        assert_eq!(bound.default, ParamValue::Float(1.25));

        let target = desc("equalize-target");
        assert_eq!(target.label, "Target brightness (stops)");
        assert_eq!(
            target.kind,
            ParamKind::Float {
                min: -0.5,
                max: 0.5,
                step: 0.1,
                logarithmic: false
            }
        );
        assert_eq!(target.default, ParamValue::Float(0.0));

        for d in PROJECT_PARAMS.iter() {
            assert!(d.auto.is_none() && d.host_default.is_none(), "{}", d.id);
        }
    }

    #[test]
    fn the_equalization_descriptors_are_advanced_and_no_registered_id_collides() {
        for id in ["equalize", "equalize-max-gain", "equalize-target"] {
            assert!(desc(id).advanced, "{id}");
        }
        for algorithm in crate::registry::stack_algorithms() {
            for p in algorithm.params() {
                assert!(
                    PROJECT_PARAMS.iter().all(|d| d.id != p.id),
                    "{} declares the project parameter id {}",
                    algorithm.id(),
                    p.id
                );
            }
        }
    }

    #[test]
    fn the_equalization_help_states_what_it_is_and_is_not() {
        for id in ["equalize", "equalize-max-gain", "equalize-target"] {
            let help = desc(id).help.to_lowercase();
            assert!(help.contains("encoded code values"), "{id}: {help}");
            assert!(
                help.contains("not a linear-light correction"),
                "{id}: {help}"
            );
        }
        let switch = desc("equalize").help.to_lowercase();
        assert!(!switch.contains("depends on the fusion method"), "{switch}");
        let bound = &desc("equalize-max-gain").help;
        assert!(bound.contains("0.8") && bound.contains("1.25"), "{bound}");
        let target = &desc("equalize-target").help;
        assert!(
            target.contains("0.0 keeps the mean encoded brightness of the stacked sources")
                && target.contains("stops")
                && target.contains("source files stay unchanged"),
            "{target}"
        );
        assert!(
            !switch.contains("reference") && !switch.contains("mid-tone"),
            "{switch}"
        );
    }

    fn schema_for(id: &str) -> Vec<ParamDesc> {
        params_for(crate::registry::find_stack(id).expect("a registered fusion method"))
    }

    fn default_of(schema: &[ParamDesc], id: &str) -> ParamValue {
        schema
            .iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("no project parameter {id}"))
            .default
            .clone()
    }

    #[test]
    fn equalize_defaults_on_for_every_method() {
        for algorithm in crate::registry::stack_algorithms() {
            assert_eq!(
                default_of(&params_for(algorithm), "equalize"),
                ParamValue::Bool(true),
                "{}",
                algorithm.id()
            );
        }
    }

    fn ids(group: &[ParamDesc]) -> Vec<&str> {
        group.iter().map(|d| d.id.as_str()).collect()
    }

    #[test]
    fn align_prepare_and_finish_partition_the_project_params() {
        assert_eq!(ids(&ALIGN_PARAMS), ["align"]);
        assert_eq!(
            ids(&PREPARE_PARAMS),
            ["equalize", "equalize-max-gain", "equalize-target"]
        );
        assert_eq!(ids(&FINISH_PARAMS), ["auto-crop"]);
        assert_eq!(
            ids(&PROJECT_PARAMS),
            [
                "align",
                "auto-crop",
                "equalize",
                "equalize-max-gain",
                "equalize-target"
            ]
        );
        for d in PROJECT_PARAMS.iter() {
            let groups = [&*ALIGN_PARAMS, &*PREPARE_PARAMS, &*FINISH_PARAMS]
                .into_iter()
                .filter(|group| group.iter().any(|g| g.id == d.id))
                .count();
            assert_eq!(groups, 1, "{} sits in exactly one group", d.id);
        }
        assert_eq!(
            ALIGN_PARAMS.len() + PREPARE_PARAMS.len() + FINISH_PARAMS.len(),
            PROJECT_PARAMS.len()
        );

        let focus_map = crate::registry::find_stack("depth-map").expect("a registered method");
        let prepare = params_for_group(&PREPARE_PARAMS, focus_map);
        let finish = params_for_group(&FINISH_PARAMS, focus_map);
        assert_eq!(
            ids(&prepare),
            ["equalize", "equalize-max-gain", "equalize-target"]
        );
        assert_eq!(ids(&finish), ["auto-crop"]);
        assert_eq!(default_of(&prepare, "equalize"), ParamValue::Bool(true));
        assert_eq!(default_of(&finish, "auto-crop"), ParamValue::Bool(true));
        assert_eq!(
            default_of(&prepare, "equalize-max-gain"),
            ParamValue::Float(1.25)
        );
        assert_eq!(
            default_of(&prepare, "equalize-target"),
            ParamValue::Float(0.0)
        );
    }

    #[test]
    fn a_method_changes_only_the_defaults_it_declares() {
        for algorithm in crate::registry::stack_algorithms() {
            let schema = params_for(algorithm);
            assert_eq!(schema.len(), PROJECT_PARAMS.len(), "{}", algorithm.id());
            let declared = algorithm.project_defaults();
            for (own, base) in schema.iter().zip(PROJECT_PARAMS.iter()) {
                let expected = declared
                    .iter()
                    .find(|(id, _)| *id == base.id)
                    .map_or(&base.default, |(_, value)| value);
                assert_eq!(&own.default, expected, "{}/{}", algorithm.id(), base.id);
                assert_eq!(own.kind, base.kind, "{}/{}", algorithm.id(), base.id);
            }
        }
    }

    #[test]
    fn equalization_settings_follow_the_defaults_and_explicit_values_win() {
        let focus_map = schema_for("depth-map");
        let details = schema_for("laplacian");
        let default = EqualizeSettings {
            max_gain: 1.25,
            target_stops: 0.0,
        };
        let mut values = ParamValues::new();
        assert_eq!(equalization_settings(&focus_map, &values), Some(default));
        assert_eq!(equalization_settings(&details, &values), Some(default));

        values.insert("equalize".into(), ParamValue::Bool(false));
        assert_eq!(equalization_settings(&focus_map, &values), None);
        assert_eq!(equalization_settings(&details, &values), None);
        values.insert("equalize".into(), ParamValue::Bool(true));
        values.insert("equalize-max-gain".into(), ParamValue::Float(1.4));
        values.insert("equalize-target".into(), ParamValue::Float(-0.3));
        assert_eq!(
            equalization_settings(&details, &values),
            Some(EqualizeSettings {
                max_gain: 1.4,
                target_stops: -0.3
            })
        );
    }
}
