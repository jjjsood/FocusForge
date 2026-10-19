// SPDX-License-Identifier: GPL-3.0-or-later

//! [`seal`]: the one function `crates/cli` calls to turn a finished run into
//! a project.
//!
//! **A project cannot be written tile by tile.** A blob's record name
//! *is* the content hash of the finished plane, which is unknown until the
//! last tile, and zstd exposes no seekable format. So fusion keeps writing
//! its two staged TIFFs under a scratch directory, and the project is
//! sealed **after** the job completes, by compressing the two files into
//! records of a new project. Sealing never touches the ledger, the
//! resume path, the coverage refusal or the cancel semantics, and it is
//! the only shape the format allows.
//!
//! This function is where the format commitments are made concrete at once:
//! the record layout, the [`BlobKind`] it records on every blob, the hash it
//! takes over *uncompressed* bytes with its algorithm named, and the fully
//! serialized [`Checkpoint`] -- `parent: None`, `ops: []` -- it writes
//! beside them.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::blob::{self, Plane, StagedBlob, ZSTD_LEVEL};
use super::checkpoint::{Checkpoint, FuseRecord, Op};
use super::container::{ContainerWriter, PartialFile};
use super::error::ProjectError;
use super::manifest::{
    AlignRecord, BlobKind, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef, SourceRef,
    SourceResolution,
};
use super::{HASH_ALGORITHM, Project, blob_entry, checkpoint_entry};
use crate::align::Registration;
use crate::params::ParamValues;

/// Everything one seal needs. Borrowed rather than owned: the caller
/// already holds every one of these.
pub struct SealRequest<'a> {
    /// Where the container goes. Its **parent directory** is also what
    /// "relative" is measured against.
    pub project_path: &'a Path,
    pub staged_composite: &'a Path,
    pub staged_zmap: &'a Path,
    /// In frame order, always -- the fingerprint depends on it.
    pub sources: &'a [PathBuf],
    pub algorithm: &'a str,
    pub params: &'a ParamValues,
    pub align: Option<AlignRecord>,
    /// Every frame's transform and the applied crop.
    pub registration: Registration,
    /// The `ImageDescription` an export of the fused checkpoint's unedited
    /// result image writes into its TIFF's IFD.
    pub composite_description: String,
    pub label: Option<String>,
    pub metrics: Metrics,
}

/// What a successful [`seal`] produced, so a caller can report it without
/// re-opening the container.
#[derive(Debug, Clone)]
pub struct SealedProject {
    pub path: PathBuf,
    pub composite: BlobRecord,
    pub zmap: BlobRecord,
    pub checkpoint: String,
}

/// Seals a finished run into a project at `request.project_path`, stamping
/// the current wall clock.
pub fn seal(request: SealRequest<'_>) -> Result<SealedProject, ProjectError> {
    seal_at(request, now_unix())
}

/// [`seal`] with the creation timestamp supplied rather than read from the
/// clock.
///
/// `created_at` is the **only** thing that differs between two seals of the
/// same inputs, which makes this the entry point a reproducibility check
/// uses.
pub fn seal_at(request: SealRequest<'_>, created_at: i64) -> Result<SealedProject, ProjectError> {
    seal_with(request, created_at, None)
}

/// [`seal_at`] with the composite's blob already staged: it is written as
/// it is and stays where it is.
pub(super) fn seal_with(
    request: SealRequest<'_>,
    created_at: i64,
    composite: Option<&StagedBlob>,
) -> Result<SealedProject, ProjectError> {
    let staging = request
        .staged_composite
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let mut staged: Vec<StagedBlob> = Vec::new();
    let result = seal_staged(&request, created_at, composite, &staging, &mut staged);
    for blob in &staged {
        blob.remove();
    }
    result
}

/// The composite's blob: `ready` as it is, else `file` compressed into
/// `staging` and recorded in `staged` for cleanup.
pub(super) fn composite_blob(
    ready: Option<&StagedBlob>,
    file: &Path,
    staging: &Path,
    staged: &mut Vec<StagedBlob>,
) -> Result<StagedBlob, ProjectError> {
    if let Some(blob) = ready {
        return Ok(blob.clone());
    }
    let blob = blob::stage_plane(file, Plane::Composite, ZSTD_LEVEL, staging)?;
    staged.push(blob.clone());
    Ok(blob)
}

fn seal_staged(
    request: &SealRequest<'_>,
    created_at: i64,
    composite: Option<&StagedBlob>,
    staging: &Path,
    staged: &mut Vec<StagedBlob>,
) -> Result<SealedProject, ProjectError> {
    let project_dir = request
        .project_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut sources = Vec::with_capacity(request.sources.len());
    for path in request.sources {
        sources.push(source_ref(path, project_dir)?);
    }
    let stack_fingerprint = blob::stack_fingerprint(&sources);

    let composite = composite_blob(composite, request.staged_composite, staging, staged)?;
    let zmap = blob::stage_plane(request.staged_zmap, Plane::ZMap, ZSTD_LEVEL, staging)?;
    staged.push(zmap.clone());
    let frame_count = blob::zmap_frame_count(request.staged_zmap)?;

    let composite_record = record(&composite);
    let zmap_record = record(&zmap);
    let checkpoint_id = Checkpoint::derive_id(&stack_fingerprint, &zmap.hash, created_at);
    let blobs = blob_records(&composite_record, &zmap_record);

    let manifest = Manifest {
        producer: format!("FocusForge {}", env!("CARGO_PKG_VERSION")),
        created_at,
        hash_algorithm: HASH_ALGORITHM.to_string(),
        stack_fingerprint: stack_fingerprint.clone(),
        algorithm: request.algorithm.to_string(),
        params: request.params.clone(),
        align: request.align.clone(),
        registration: request.registration.clone(),
        sources,
        composite: PlaneRef {
            blob: composite.hash.clone(),
            width: composite.width,
            height: composite.height,
            samples_per_pixel: composite.samples_per_pixel,
            byte_order: ByteOrderTag::Little,
            description: request.composite_description.clone(),
            frame_count,
        },
        zmap: PlaneRef {
            blob: zmap.hash.clone(),
            width: zmap.width,
            height: zmap.height,
            samples_per_pixel: 1,
            byte_order: ByteOrderTag::Little,
            description: request.composite_description.clone(),
            frame_count,
        },
        blobs,
        metrics: request.metrics,
        checkpoint: checkpoint_id.clone(),
    };

    let checkpoint = Checkpoint {
        id: checkpoint_id.clone(),
        parent: None,
        ops: Vec::new(),
        zmap_blob: zmap.hash.clone(),
        stack_fingerprint,
        algorithm: request.algorithm.to_string(),
        params: request.params.clone(),
        created_at,
        label: request.label.clone(),
        fuse: Some(FuseRecord {
            composite: manifest.composite.clone(),
            align: manifest.align.clone(),
            registration: manifest.registration.clone(),
            sources: manifest.sources.clone(),
            metrics: manifest.metrics,
        }),
        hidden: false,
    };

    let mut writer = ContainerWriter::create(request.project_path)?;
    for staged in [&composite, &zmap] {
        let mut source =
            File::open(&staged.path).map_err(|err| ProjectError::io(&staged.path, err))?;
        writer.put(&blob_entry(&staged.hash), &mut source)?;
    }
    let to_json = |err| ProjectError::container(request.project_path, err);
    writer.put_bytes(
        &checkpoint_entry(&checkpoint_id),
        &serde_json::to_vec(&checkpoint).map_err(to_json)?,
    )?;
    writer.put_bytes(
        "manifest.json",
        &serde_json::to_vec(&manifest).map_err(to_json)?,
    )?;
    writer.finish()?;

    Ok(SealedProject {
        path: request.project_path.to_path_buf(),
        composite: composite_record,
        zmap: zmap_record,
        checkpoint: checkpoint_id,
    })
}

/// A finished run kept outside any container: its two planes as TIFF files
/// plus everything [`seal_at`] and [`super::append_at`] record beside them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldCheckpoint {
    pub composite: PathBuf,
    pub zmap: PathBuf,
    /// The composite's extent in pixels, after any crop.
    pub width: u32,
    pub height: u32,
    /// In frame order, always -- the fingerprint depends on it.
    pub sources: Vec<PathBuf>,
    pub algorithm: String,
    pub params: ParamValues,
    pub align: Option<AlignRecord>,
    /// Every frame's transform and the applied crop.
    pub registration: Registration,
    /// Exactly the `ImageDescription` the composite carries.
    pub composite_description: String,
    pub metrics: Metrics,
    /// Unix seconds, UTC, stamped when the run finished.
    pub created_at: i64,
    /// The composite already compressed and hashed, beside the held planes.
    /// A save writes it as it is instead of reading the composite again. Not
    /// persisted: the file it names belongs to the process that made it.
    #[serde(skip)]
    pub compressed_composite: Option<Box<StagedBlob>>,
}

/// Where an in-memory checkpoint is recorded: under a checkpoint the project
/// file already holds, or under an earlier entry of the same list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentRef {
    Saved(String),
    /// Index of an earlier entry of the list being written.
    Entry(usize),
}

/// What an in-memory checkpoint holds.
#[derive(Debug, Clone)]
// A project holds a handful of entries; boxing the fuse would touch every constructor.
#[allow(clippy::large_enum_variant)]
pub enum InMemoryKind {
    /// A finished fuse's planes.
    Fused(HeldCheckpoint),
    /// Ops rendered over the parent's result image.
    Ops { ops: Vec<Op>, created_at: i64 },
}

/// One checkpoint kept outside any container, with the parent it is written
/// under. Only a never-saved project's first entry has no parent.
#[derive(Debug, Clone)]
pub struct InMemoryCheckpoint {
    pub kind: InMemoryKind,
    pub parent: Option<ParentRef>,
}

impl InMemoryCheckpoint {
    /// Unix seconds, UTC, stamped when the checkpoint was taken.
    pub fn created_at(&self) -> i64 {
        match &self.kind {
            InMemoryKind::Fused(held) => held.created_at,
            InMemoryKind::Ops { created_at, .. } => *created_at,
        }
    }
}

/// Which of `entries` [`seal_in_memory`] writes: every one except an entry
/// `deletes` names that is not `keep` and that no written entry is recorded
/// under.
pub fn kept_entries(
    entries: &[InMemoryCheckpoint],
    deletes: &[ParentRef],
    keep: Option<&ParentRef>,
) -> Vec<bool> {
    let mut written = vec![true; entries.len()];
    for i in (0..entries.len()).rev() {
        let this = ParentRef::Entry(i);
        written[i] = !deletes.contains(&this)
            || keep == Some(&this)
            || entries[i + 1..]
                .iter()
                .zip(&written[i + 1..])
                .any(|(entry, written)| *written && entry.parent.as_ref() == Some(&this));
    }
    written
}

/// Writes the [`kept_entries`], each under its own parent, hides `deletes`
/// (see [`super::delete_checkpoint`]) and stops listing hidden checkpoints
/// nothing visible is built on, sparing `keep` and its ancestors. Everything
/// is appended to `target` (the project there when `onto_existing`) and
/// committed at once. Returns the id written per written entry. On error
/// `target` is left as it was; bad references and unknown deletes are refused
/// before any write.
pub fn seal_in_memory(
    target: &Path,
    entries: &[InMemoryCheckpoint],
    deletes: &[ParentRef],
    keep: Option<&ParentRef>,
    onto_existing: bool,
) -> Result<Vec<String>, ProjectError> {
    let kept = kept_entries(entries, deletes, keep);
    let mut ids: Vec<Option<String>> = vec![None; entries.len()];
    if onto_existing {
        let mut project = Project::open(target)?;
        plan_in_memory(target, entries, deletes, keep, Some(&project))?;
        write_entries(&mut project, entries, &kept, &mut ids)?;
        apply_deletes(&mut project, deletes, keep, &ids)?;
        project.commit()?;
    } else {
        plan_in_memory(target, entries, deletes, keep, None)?;
        let guard = PartialFile::new(target);
        let InMemoryKind::Fused(root) = &entries[0].kind else {
            unreachable!("plan_in_memory refuses a first entry that is not a fuse");
        };
        ids[0] = Some(seal_root(guard.path(), root)?);
        if kept[1..].iter().any(|&written| written) || !deletes.is_empty() {
            let mut project = Project::open(guard.path())?;
            write_entries(&mut project, entries, &kept, &mut ids)?;
            apply_deletes(&mut project, deletes, keep, &ids)?;
            project.commit()?;
        }
        guard
            .commit()
            .map_err(|source| ProjectError::io(target, source))?;
    }
    Ok(ids.into_iter().flatten().collect())
}

fn plan_in_memory(
    target: &Path,
    entries: &[InMemoryCheckpoint],
    deletes: &[ParentRef],
    keep: Option<&ParentRef>,
    opened: Option<&Project>,
) -> Result<(), ProjectError> {
    let onto_existing = opened.is_some();
    let refuse = |message: String| ProjectError::container(target, message);
    if entries.is_empty() && (deletes.is_empty() || !onto_existing) {
        return Err(refuse("no checkpoint to save".to_string()));
    }

    for (i, entry) in entries.iter().enumerate() {
        let root = !onto_existing && i == 0;
        if root && (entry.parent.is_some() || !matches!(entry.kind, InMemoryKind::Fused(_))) {
            return Err(refuse(
                "the first checkpoint of a new project must be a fuse with no parent".to_string(),
            ));
        }
        match &entry.parent {
            None if !root => {
                return Err(refuse(format!("checkpoint {i} has no parent")));
            }
            None => {}
            Some(ParentRef::Entry(j)) if *j >= i => {
                return Err(refuse(format!(
                    "checkpoint {i} names checkpoint {j} as its parent, which is not before it"
                )));
            }
            Some(ParentRef::Entry(_)) => {}
            Some(ParentRef::Saved(id)) => match opened {
                Some(project) => {
                    project.history().get(id)?;
                }
                None => {
                    return Err(refuse(format!(
                        "checkpoint {i} names saved checkpoint {id}, but there is no saved project"
                    )));
                }
            },
        }
    }

    for reference in deletes.iter().chain(keep) {
        match (reference, opened) {
            (ParentRef::Entry(i), _) if *i >= entries.len() => {
                return Err(refuse(format!("checkpoint {i} is not in the list")));
            }
            (ParentRef::Entry(_), _) => {}
            (ParentRef::Saved(id), Some(project)) => {
                project.history().get(id)?;
            }
            (ParentRef::Saved(id), None) => {
                return Err(refuse(format!(
                    "saved checkpoint {id} is named, but there is no saved project"
                )));
            }
        }
    }
    if !onto_existing && !kept_entries(entries, deletes, keep)[0] {
        return Err(refuse("no checkpoint to save".to_string()));
    }
    Ok(())
}

/// Appends every `kept` entry not yet in `ids` to `project`, each under its
/// own parent, recording the id written.
fn write_entries(
    project: &mut Project,
    entries: &[InMemoryCheckpoint],
    kept: &[bool],
    ids: &mut [Option<String>],
) -> Result<(), ProjectError> {
    for (i, entry) in entries.iter().enumerate() {
        if !kept[i] || ids[i].is_some() {
            continue;
        }
        let parent: Option<String> = match &entry.parent {
            None => None,
            Some(ParentRef::Saved(id)) => Some(id.clone()),
            Some(ParentRef::Entry(j)) => Some(
                ids[*j]
                    .clone()
                    .expect("an entry kept for its child is written before it"),
            ),
        };
        let parent = parent
            .as_deref()
            .expect("an entry after the root has a parent");
        let id = match &entry.kind {
            InMemoryKind::Fused(held) => append_held(project, held, parent)?,
            InMemoryKind::Ops { ops, created_at } => {
                super::append::append_ops_on(project, parent, ops.clone(), None, *created_at)?
                    .checkpoint
            }
        };
        ids[i] = Some(id);
    }
    Ok(())
}

/// Hides `deletes` on `project`, then stops listing hidden checkpoints
/// nothing visible is built on, sparing `keep` and its ancestors.
fn apply_deletes(
    project: &mut Project,
    deletes: &[ParentRef],
    keep: Option<&ParentRef>,
    ids: &[Option<String>],
) -> Result<(), ProjectError> {
    let keep_id: Option<&str> = keep.map(|reference| match reference {
        ParentRef::Saved(id) => id.as_str(),
        ParentRef::Entry(i) => ids[*i].as_deref().expect("the pinned entry is written"),
    });
    for delete in deletes {
        let id = match delete {
            ParentRef::Saved(id) => Some(id.as_str()),
            ParentRef::Entry(i) => ids[*i].as_deref(),
        };
        if let Some(id) = id {
            super::append::hide_and_collect(project, Some(id), keep_id)?;
        }
    }
    super::append::hide_and_collect(project, None, keep_id)?;
    Ok(())
}

/// Seals `held` as a new project's root at `path`.
fn seal_root(path: &Path, held: &HeldCheckpoint) -> Result<String, ProjectError> {
    Ok(seal_with(
        SealRequest {
            project_path: path,
            staged_composite: &held.composite,
            staged_zmap: &held.zmap,
            sources: &held.sources,
            algorithm: &held.algorithm,
            params: &held.params,
            align: held.align.clone(),
            registration: held.registration.clone(),
            composite_description: held.composite_description.clone(),
            label: None,
            metrics: held.metrics,
        },
        held.created_at,
        held.compressed_composite.as_deref(),
    )?
    .checkpoint)
}

/// Appends `held` to `project` as a fused checkpoint under `parent`.
fn append_held(
    project: &mut Project,
    held: &HeldCheckpoint,
    parent: &str,
) -> Result<String, ProjectError> {
    let path = project.path().to_path_buf();
    let written = super::append::append_onto(
        project,
        &super::AppendRequest {
            project_path: &path,
            staged_composite: &held.composite,
            staged_zmap: &held.zmap,
            sources: &held.sources,
            algorithm: &held.algorithm,
            params: &held.params,
            align: held.align.clone(),
            registration: held.registration.clone(),
            composite_description: held.composite_description.clone(),
            label: None,
            metrics: held.metrics,
        },
        Some(parent),
        held.created_at,
        held.compressed_composite.as_deref(),
    )?;
    Ok(written.checkpoint)
}

/// The manifest's blob list: one record per **distinct** hash, in the order
/// the container writes them. Two identical planes collapse to one record,
/// and the manifest must say the same thing the container does.
fn blob_records(composite: &BlobRecord, zmap: &BlobRecord) -> Vec<BlobRecord> {
    if composite.hash == zmap.hash {
        vec![composite.clone()]
    } else {
        vec![composite.clone(), zmap.clone()]
    }
}

pub(super) fn record(blob: &StagedBlob) -> BlobRecord {
    BlobRecord {
        hash: blob.hash.clone(),
        kind: BlobKind::Full,
        compression: "zstd".to_string(),
        uncompressed_len: blob.uncompressed_len,
        compressed_len: blob.compressed_len,
    }
}

/// One source frame's reference.
///
/// **Relative means descendant**: a frame under the project file's own
/// directory is recorded relative to it; anything else is recorded as an
/// absolute path with `AsGiven`, and reported on open. An `-o <dir>` run
/// therefore routinely produces `AsGiven` projects -- that is the intended
/// behaviour, not a defect.
pub(super) fn source_ref(path: &Path, project_dir: &Path) -> Result<SourceRef, ProjectError> {
    let (stamp, hash) = blob::source_hash(path, None)?;
    let absolute = std::path::absolute(path).map_err(|source| ProjectError::io(path, source))?;
    let base =
        std::path::absolute(project_dir).map_err(|source| ProjectError::io(project_dir, source))?;
    let (path, resolution) = match absolute.strip_prefix(&base) {
        Ok(relative) => (
            relative.to_string_lossy().into_owned(),
            SourceResolution::Relative,
        ),
        Err(_) => (
            absolute.to_string_lossy().into_owned(),
            SourceResolution::AsGiven,
        ),
    };
    Ok(SourceRef {
        path,
        resolution,
        hash,
        bytes: stamp.bytes,
        modified_ns: stamp.modified_ns,
        file_id: stamp.file_id,
    })
}

/// Unix seconds, UTC. `0` if the clock is before the epoch, which is not a
/// state worth failing a finished stack over.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
