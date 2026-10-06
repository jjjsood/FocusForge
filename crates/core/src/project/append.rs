// SPDX-License-Identifier: GPL-3.0-or-later

//! [`append`]: writes a new checkpoint onto an already-sealed project,
//! extending its chain rather than starting a fresh container.
//! [`append_ops`]: records a non-empty op list as a checkpoint of its own,
//! one record and no blob.
//!
//! Every save adds its records to the end of the project file and commits
//! them with one footer ([`super::container`]): the previous checkpoint and
//! every blob it names stay where they are, and a save that fails leaves the
//! file as it was. A fused checkpoint's blobs are staged with
//! [`super::blob::stage_plane`] exactly as [`super::seal::seal`] does.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::blob::{self, Plane, StagedBlob, ZSTD_LEVEL};
use super::checkpoint::{Checkpoint, FuseRecord};
use super::error::ProjectError;
use super::manifest::{
    AlignRecord, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef, SourceRef,
};
use super::seal::{composite_blob, now_unix, record, source_ref};
use super::{HASH_ALGORITHM, Op, Project, SealedProject, full_blob, fused_extent};
use crate::align::Registration;
use crate::params::ParamValues;
use crate::zmap::ZMapInfo;

/// Everything one append needs -- [`super::seal::SealRequest`]'s own field
/// set, with `project_path` naming a container that already exists.
pub struct AppendRequest<'a> {
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
    /// Exactly the `ImageDescription` the staged composite carries.
    pub composite_description: String,
    pub label: Option<String>,
    pub metrics: Metrics,
}

/// Appends a new checkpoint onto `request.project_path`, stamping the
/// current wall clock.
pub fn append(request: AppendRequest<'_>) -> Result<SealedProject, ProjectError> {
    append_at(request, now_unix())
}

/// [`append`] with the creation timestamp supplied rather than read from the
/// clock -- the reproducibility check's own entry point, exactly as
/// [`super::seal::seal_at`] is for a first seal.
pub fn append_at(
    request: AppendRequest<'_>,
    created_at: i64,
) -> Result<SealedProject, ProjectError> {
    let mut project = Project::open(request.project_path)?;
    let sealed = append_onto(&mut project, &request, None, created_at, None)?;
    project.commit()?;
    Ok(sealed)
}

/// [`append_at`] onto an open project, under an explicit `parent` checkpoint
/// instead of the project's head; `None` is the head. The caller has checked
/// that `parent` exists and commits the project. A `composite` blob already
/// staged is written as it is and stays where it is.
pub(super) fn append_onto(
    project: &mut Project,
    request: &AppendRequest<'_>,
    parent: Option<&str>,
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
    let result = append_staged(
        project,
        request,
        parent,
        created_at,
        composite,
        &staging,
        &mut staged,
    );
    for blob in &staged {
        blob.remove();
    }
    result
}

fn append_staged(
    project: &mut Project,
    request: &AppendRequest<'_>,
    parent: Option<&str>,
    created_at: i64,
    composite: Option<&StagedBlob>,
    staging: &Path,
    staged: &mut Vec<StagedBlob>,
) -> Result<SealedProject, ProjectError> {
    let project_dir = project
        .path()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let mut sources = Vec::with_capacity(request.sources.len());
    for path in request.sources {
        sources.push(source_ref(path, &project_dir)?);
    }

    let composite = composite_blob(composite, request.staged_composite, staging, staged)?;
    let zmap = blob::stage_plane(request.staged_zmap, Plane::ZMap, ZSTD_LEVEL, staging)?;
    staged.push(zmap.clone());
    let frame_count = blob::zmap_frame_count(request.staged_zmap)?;

    write_fused(
        project,
        FusedWrite {
            parent,
            composite: &composite,
            zmap: &zmap,
            frame_count,
            sources,
            algorithm: request.algorithm,
            params: request.params,
            align: request.align.clone(),
            registration: request.registration.clone(),
            composite_description: request.composite_description.clone(),
            label: request.label.clone(),
            metrics: request.metrics,
            created_at,
        },
    )
}

/// A fused checkpoint whose planes are already staged: what
/// [`write_fused`] records under its parent.
struct FusedWrite<'a> {
    /// `None` is the project's head.
    parent: Option<&'a str>,
    composite: &'a StagedBlob,
    zmap: &'a StagedBlob,
    frame_count: u32,
    sources: Vec<SourceRef>,
    algorithm: &'a str,
    params: &'a ParamValues,
    align: Option<AlignRecord>,
    registration: Registration,
    composite_description: String,
    label: Option<String>,
    metrics: Metrics,
    created_at: i64,
}

fn write_fused(
    project: &mut Project,
    write: FusedWrite<'_>,
) -> Result<SealedProject, ProjectError> {
    let FusedWrite {
        parent,
        composite,
        zmap,
        frame_count,
        sources,
        algorithm,
        params,
        align,
        registration,
        composite_description,
        label,
        metrics,
        created_at,
    } = write;
    let stack_fingerprint = blob::stack_fingerprint(&sources);
    let composite_record = record(composite);
    let zmap_record = record(zmap);

    let parent_id = parent.map_or_else(|| project.manifest().checkpoint.clone(), str::to_string);
    let checkpoint_id =
        Checkpoint::derive_child_id(&parent_id, &stack_fingerprint, &zmap.hash, created_at);

    let mut blobs = project.manifest().blobs.clone();
    for rec in [composite_record.clone(), zmap_record.clone()] {
        if !blobs.iter().any(|b| b.hash == rec.hash) {
            blobs.push(rec);
        }
    }

    let manifest = Manifest {
        producer: format!("FocusForge {}", env!("CARGO_PKG_VERSION")),
        created_at,
        hash_algorithm: HASH_ALGORITHM.to_string(),
        stack_fingerprint: stack_fingerprint.clone(),
        algorithm: algorithm.to_string(),
        params: params.clone(),
        align,
        registration,
        sources,
        composite: PlaneRef {
            blob: composite.hash.clone(),
            width: composite.width,
            height: composite.height,
            samples_per_pixel: composite.samples_per_pixel,
            byte_order: ByteOrderTag::Little,
            description: composite_description.clone(),
            frame_count,
        },
        zmap: PlaneRef {
            blob: zmap.hash.clone(),
            width: zmap.width,
            height: zmap.height,
            samples_per_pixel: 1,
            byte_order: ByteOrderTag::Little,
            description: composite_description,
            frame_count,
        },
        blobs,
        metrics,
        checkpoint: checkpoint_id.clone(),
    };

    let checkpoint = Checkpoint {
        id: checkpoint_id.clone(),
        parent: Some(parent_id),
        ops: Vec::new(),
        zmap_blob: zmap.hash.clone(),
        stack_fingerprint,
        algorithm: algorithm.to_string(),
        params: params.clone(),
        created_at,
        label,
        fuse: Some(FuseRecord {
            composite: manifest.composite.clone(),
            align: manifest.align.clone(),
            registration: manifest.registration.clone(),
            sources: manifest.sources.clone(),
            metrics: manifest.metrics,
        }),
        hidden: false,
    };

    project.put_blob(composite)?;
    project.put_blob(zmap)?;
    project.put_checkpoint(&checkpoint)?;
    project.set_manifest(manifest)?;

    Ok(SealedProject {
        path: project.path().to_path_buf(),
        composite: composite_record,
        zmap: zmap_record,
        checkpoint: checkpoint_id,
    })
}

/// Records `ops` as a new checkpoint onto `project_path`'s head, stamping the
/// current wall clock. Refuses an empty list ([`ProjectError::EmptyOpList`])
/// and a list holding anything but a valid brush stroke
/// ([`ProjectError::Op`], naming the first bad op) before writing. Sources are
/// never opened or rehashed: the new checkpoint's `stack_fingerprint`,
/// `algorithm` and `params` are carried forward from the head unchanged, and
/// its `zmap_blob` and the manifest's planes name the fused ancestor's. The
/// append writes the checkpoint record and no blob.
pub fn append_ops(
    project_path: &Path,
    ops: Vec<Op>,
    label: Option<String>,
) -> Result<SealedProject, ProjectError> {
    append_ops_at(project_path, ops, label, now_unix())
}

/// [`append_ops`] with the creation timestamp supplied rather than read from
/// the clock. Seals onto the project's current head.
pub fn append_ops_at(
    project_path: &Path,
    ops: Vec<Op>,
    label: Option<String>,
    created_at: i64,
) -> Result<SealedProject, ProjectError> {
    append_ops_saved(project_path, None, ops, label, created_at)
}

/// [`append_ops_at`] against an explicit `parent` rather than always the
/// project's current head -- sealing onto an older checkpoint, which is
/// what branching is, mechanically.
pub fn append_ops_onto(
    project_path: &Path,
    parent: &str,
    ops: Vec<Op>,
    label: Option<String>,
    created_at: i64,
) -> Result<SealedProject, ProjectError> {
    append_ops_saved(project_path, Some(parent), ops, label, created_at)
}

/// Opens `project_path`, seals `ops` under `parent` (`None` is the head) and
/// commits.
fn append_ops_saved(
    project_path: &Path,
    parent: Option<&str>,
    ops: Vec<Op>,
    label: Option<String>,
    created_at: i64,
) -> Result<SealedProject, ProjectError> {
    let mut project = Project::open(project_path)?;
    let parent = parent.map_or_else(|| project.checkpoint().id.clone(), str::to_string);
    let sealed = append_ops_on(&mut project, &parent, ops, label, created_at)?;
    project.commit()?;
    Ok(sealed)
}

/// Appends `ops` as a chain of states, one per op, each the previous one's
/// child, the first under `parent` (`None` is the head), and commits once.
/// The ids come back in order and `label` names the last state only. Every op
/// is checked before anything is written: the first invalid one is
/// [`ProjectError::Op`] with its position and the file is left as it was; an
/// empty list is [`ProjectError::EmptyOpList`].
pub fn append_op_chain(
    project_path: &Path,
    parent: Option<&str>,
    ops: Vec<Op>,
    label: Option<String>,
    created_at: i64,
) -> Result<Vec<String>, ProjectError> {
    if ops.is_empty() {
        return Err(ProjectError::EmptyOpList {
            path: project_path.to_path_buf(),
        });
    }
    let mut project = Project::open(project_path)?;
    let mut parent = parent.map_or_else(|| project.checkpoint().id.clone(), str::to_string);
    checked_parent(&project, &parent, &ops)?;
    let last = ops.len() - 1;
    let mut ids = Vec::with_capacity(ops.len());
    for (index, op) in ops.into_iter().enumerate() {
        let label = if index == last { label.clone() } else { None };
        let sealed = append_ops_on(&mut project, &parent, vec![op], label, created_at)?;
        parent.clone_from(&sealed.checkpoint);
        ids.push(sealed.checkpoint);
    }
    project.commit()?;
    Ok(ids)
}

/// [`append_ops_onto`] on an open project, left for the caller to commit.
pub(super) fn append_ops_on(
    project: &mut Project,
    parent: &str,
    ops: Vec<Op>,
    label: Option<String>,
    created_at: i64,
) -> Result<SealedProject, ProjectError> {
    if ops.is_empty() {
        return Err(ProjectError::EmptyOpList {
            path: project.path().to_path_buf(),
        });
    }
    let head = checked_parent(project, parent, &ops)?;
    let manifest = project.manifest().clone();
    let fuse = project.fuse_state(parent)?;
    let zmap_blob = project.fused_ancestor(parent)?.zmap_blob.clone();
    let zmap = full_blob(&manifest.blobs, &zmap_blob, project.path())?.clone();
    let composite = manifest
        .blobs
        .iter()
        .find(|b| b.hash == fuse.composite.blob)
        .cloned()
        .expect("manifest.blobs always names a fuse's own composite blob");

    let ops_json =
        serde_json::to_vec(&ops).map_err(|err| ProjectError::container(project.path(), err))?;
    let checkpoint_id = Checkpoint::derive_child_id(
        &head.id,
        &head.stack_fingerprint,
        &blake3::hash(&ops_json).to_hex(),
        created_at,
    );

    // The checkpoint sits under `parent`'s own fuse, which is not necessarily
    // the project's current head's, so the planes the manifest names come
    // from `parent`'s fused ancestor.
    let new_manifest = Manifest {
        producer: manifest.producer.clone(),
        created_at,
        hash_algorithm: manifest.hash_algorithm.clone(),
        stack_fingerprint: head.stack_fingerprint.clone(),
        algorithm: head.algorithm.clone(),
        params: head.params.clone(),
        align: fuse.align.clone(),
        sources: fuse.sources.clone(),
        composite: fuse.composite.clone(),
        zmap: PlaneRef {
            blob: zmap_blob.clone(),
            width: fuse.composite.width,
            height: fuse.composite.height,
            frame_count: fuse.composite.frame_count,
            ..manifest.zmap.clone()
        },
        blobs: manifest.blobs.clone(),
        metrics: fuse.metrics,
        checkpoint: checkpoint_id.clone(),
        registration: fuse.registration.clone(),
    };

    let checkpoint = Checkpoint {
        id: checkpoint_id.clone(),
        parent: Some(head.id.clone()),
        ops,
        zmap_blob,
        stack_fingerprint: head.stack_fingerprint.clone(),
        algorithm: head.algorithm.clone(),
        params: head.params.clone(),
        created_at,
        label,
        fuse: None,
        hidden: false,
    };
    project.put_checkpoint(&checkpoint)?;
    project.set_manifest(new_manifest)?;

    Ok(SealedProject {
        path: project.path().to_path_buf(),
        composite,
        zmap,
        checkpoint: checkpoint_id,
    })
}

/// `parent`'s checkpoint, once every op of `ops` is valid against the Z-Map
/// geometry of `parent`'s fused ancestor: the first op that is not is
/// [`ProjectError::Op`] with its position.
fn checked_parent(project: &Project, parent: &str, ops: &[Op]) -> Result<Checkpoint, ProjectError> {
    let head = project.history().get(parent)?.clone();
    let (width, height, frame_count) = fused_extent(project, parent)?;
    let info = ZMapInfo::new(width, height, frame_count)
        .map_err(|err| ProjectError::container(project.path(), err))?;
    for (index, op) in ops.iter().enumerate() {
        crate::retouch::footprint(op, info).map_err(|source| ProjectError::Op {
            index,
            source: Box::new(source),
        })?;
    }
    Ok(head)
}

/// What a successful [`delete_checkpoint`] hid and stopped listing, and the
/// head it leaves the project pointing at.
pub struct Deleted {
    /// The checkpoint the call hid.
    pub hidden: String,
    /// Every checkpoint the project no longer lists: `hidden` and the
    /// checkpoints hidden earlier, whichever have no visible checkpoint built
    /// on them. Their bytes stay in the file as dead data.
    pub removed: Vec<String>,
    pub head: String,
}

/// Hides `id`, then stops listing every hidden checkpoint no visible one is
/// built on, with the blobs only those needed; nothing is re-parented.
/// Refuses an unknown or hidden `id` ([`ProjectError::UnknownCheckpoint`]) and
/// the last visible checkpoint ([`ProjectError::LastVisibleDelete`]) before
/// writing. When the head goes, the newest visible checkpoint becomes the head
/// and the manifest is rebuilt from it ([`Project::fuse_state`]).
pub fn delete_checkpoint(project_path: &Path, id: &str) -> Result<Deleted, ProjectError> {
    let mut project = Project::open(project_path)?;
    let (removed, head) = hide_and_collect(&mut project, Some(id), None)?;
    project.commit()?;
    Ok(Deleted {
        hidden: id.to_string(),
        removed,
        head,
    })
}

/// [`delete_checkpoint`]'s body on an open project, left for the caller to
/// commit: hides `hide` when given, then stops listing every hidden
/// checkpoint nothing visible is built on, returning the ids dropped and the
/// head. `keep` counts as visible for that, so it and its ancestors stay
/// listed even when hidden. With nothing to hide and nothing to drop, nothing
/// is written.
pub(super) fn hide_and_collect(
    project: &mut Project,
    hide: Option<&str>,
    keep: Option<&str>,
) -> Result<(Vec<String>, String), ProjectError> {
    let project_path = project.path().to_path_buf();
    let history = project.history();
    let target: Option<Checkpoint> = hide.map(|id| history.get(id)).transpose()?.cloned();
    if let (Some(id), Some(target)) = (hide, &target) {
        if target.hidden {
            return Err(ProjectError::UnknownCheckpoint {
                path: project_path,
                id: id.to_string(),
            });
        }
        if history.visible().len() == 1 {
            return Err(ProjectError::LastVisibleDelete {
                path: project_path,
                checkpoint: id.to_string(),
            });
        }
    }

    let hidden_set: HashSet<&str> = history
        .all()
        .into_iter()
        .filter(|c| c.hidden)
        .map(|c| c.id.as_str())
        .chain(hide)
        .filter(|id| Some(*id) != keep)
        .collect();
    let removed: Vec<String> = history
        .all()
        .into_iter()
        .filter(|c| {
            hidden_set.contains(c.id.as_str())
                && history
                    .descendants(&c.id)
                    .iter()
                    .all(|d| hidden_set.contains(d.id.as_str()))
        })
        .map(|c| c.id.clone())
        .collect();
    let removed_set: HashSet<&str> = removed.iter().map(String::as_str).collect();
    if hide.is_none() && removed.is_empty() {
        return Ok((removed, project.checkpoint().id.clone()));
    }

    let all = history.all();
    let survivors: Vec<Checkpoint> = all
        .iter()
        .filter(|c| !removed_set.contains(c.id.as_str()))
        .map(|c| (*c).clone())
        .collect();
    let removed_checkpoints: Vec<Checkpoint> = all
        .iter()
        .filter(|c| removed_set.contains(c.id.as_str()))
        .map(|c| (*c).clone())
        .collect();

    let manifest = project.manifest().clone();
    let old_head_id = project.checkpoint().id.clone();
    let head_gone =
        hide == Some(old_head_id.as_str()) || removed_set.contains(old_head_id.as_str());

    let new_manifest = if head_gone {
        let new_head = survivors
            .iter()
            .find(|c| !c.hidden && Some(c.id.as_str()) != hide)
            .expect("a visible checkpoint other than the target remains");
        let fuse = project.fuse_state(&new_head.id)?;
        Manifest {
            created_at: new_head.created_at,
            stack_fingerprint: new_head.stack_fingerprint.clone(),
            algorithm: new_head.algorithm.clone(),
            params: new_head.params.clone(),
            align: fuse.align,
            sources: fuse.sources,
            composite: fuse.composite,
            zmap: PlaneRef {
                blob: new_head.zmap_blob.clone(),
                ..manifest.zmap.clone()
            },
            metrics: fuse.metrics,
            checkpoint: new_head.id.clone(),
            registration: fuse.registration.clone(),
            ..manifest.clone()
        }
    } else {
        manifest.clone()
    };

    let removed_blobs =
        referenced_blobs(&manifest.blobs, removed_checkpoints.iter(), &project_path)?;
    let survivor_blobs = referenced_blobs(&manifest.blobs, survivors.iter(), &project_path)?;
    let (kept_blobs, dropped_blobs): (Vec<BlobRecord>, Vec<BlobRecord>) = manifest
        .blobs
        .iter()
        .cloned()
        .partition(|b| !removed_blobs.contains(&b.hash) || survivor_blobs.contains(&b.hash));
    // A target that stays listed is rewritten with its flag set.
    let hidden = match (hide, target) {
        (Some(id), Some(target)) if !removed_set.contains(id) => Some(Checkpoint {
            hidden: true,
            ..target
        }),
        _ => None,
    };

    project.drop_checkpoints(&removed)?;
    if let Some(hidden) = &hidden {
        project.put_checkpoint(hidden)?;
    }
    for blob in &dropped_blobs {
        project.drop_blob(&blob.hash)?;
    }
    let head = new_manifest.checkpoint.clone();
    project.set_manifest(Manifest {
        blobs: kept_blobs,
        ..new_manifest
    })?;

    Ok((removed, head))
}

/// Every blob hash `checkpoints` reaches: each one's Z-Map blob
/// ([`full_blob`]) and, when it carries one, its own
/// [`FuseRecord::composite`].
fn referenced_blobs<'a>(
    blobs: &[BlobRecord],
    checkpoints: impl Iterator<Item = &'a Checkpoint>,
    path: &Path,
) -> Result<HashSet<String>, ProjectError> {
    let mut set = HashSet::new();
    for checkpoint in checkpoints {
        set.insert(full_blob(blobs, &checkpoint.zmap_blob, path)?.hash.clone());
        if let Some(fuse) = &checkpoint.fuse {
            set.insert(fuse.composite.blob.clone());
        }
    }
    Ok(set)
}
