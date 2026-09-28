// SPDX-License-Identifier: GPL-3.0-or-later

//! [`append`]: writes a new checkpoint onto an already-sealed project,
//! extending its chain rather than starting a fresh container.
//! [`append_ops`]: seals a non-empty op list onto the head as a checkpoint of
//! its own, replaying it against the head's Z-Map rather than staging a
//! freshly fused composite.
//!
//! Reuses [`super::container::ProjectWriter`]'s `.partial`-then-rename
//! discipline and [`super::blob::stage_plane`]'s staging exactly as
//! [`super::seal::seal`] does. The previous checkpoint and every blob it
//! names survive the write because every existing entry is copied into the
//! new container byte for byte before the new ones are added
//! (copy_forward), and every read of the old container happens before
//! the writer is finished -- the rename that replaces it lands after the
//! last read.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};

use super::blob::{self, Plane, StagedBlob, ZSTD_LEVEL};
use super::checkpoint::{Checkpoint, FuseRecord};
use super::container::ProjectWriter;
use super::error::ProjectError;
use super::manifest::{
    AlignRecord, BlobKind, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef,
};
use super::read_json;
use super::seal::{now_unix, record, source_ref};
use super::{
    HASH_ALGORITHM, Op, Project, SealedProject, format_version_for, materialize_zmap, resolve_chain,
};
use crate::params::ParamValues;
use crate::retouch::{replay, write_edited};
use crate::tile::Rect;
use crate::zmap::{ZMapError, ZMapInfo, ZMapReader, ZMapWriter};

/// Rows per band [`write_edited`] streams a new full Z-Map at -- bounded
/// memory regardless of the map's height, the same discipline
/// `focusforge-cli retouch` uses.
const ROWS_PER_PASS: u32 = 256;

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
    /// The composite's extent after any crop was applied.
    pub crop: Option<Rect>,
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
    let staging = request
        .staged_composite
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let mut staged: Vec<StagedBlob> = Vec::new();
    let result = append_staged(&request, created_at, &staging, &mut staged);
    for blob in &staged {
        blob.remove();
    }
    result
}

fn append_staged(
    request: &AppendRequest<'_>,
    created_at: i64,
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

    let composite = blob::stage_plane(
        request.staged_composite,
        Plane::Composite,
        ZSTD_LEVEL,
        staging,
    )?;
    staged.push(composite.clone());
    let zmap = blob::stage_plane(request.staged_zmap, Plane::ZMap, ZSTD_LEVEL, staging)?;
    staged.push(zmap.clone());
    let frame_count = blob::zmap_frame_count(request.staged_zmap)?;

    let composite_record = record(&composite);
    let zmap_record = record(&zmap);

    let old_file = File::open(request.project_path)
        .map_err(|err| ProjectError::io(request.project_path, err))?;
    let mut old_archive = zip::ZipArchive::new(old_file)
        .map_err(|err| ProjectError::container(request.project_path, err))?;
    let old_manifest: Manifest =
        read_json(&mut old_archive, request.project_path, "manifest.json")?;
    verify_blobs_present(&mut old_archive, &old_manifest, request.project_path)?;
    let old_head_id = old_manifest.checkpoint.clone();

    let checkpoint_id =
        Checkpoint::derive_child_id(&old_head_id, &stack_fingerprint, &zmap.hash, created_at);

    let mut blobs = old_manifest.blobs.clone();
    for rec in [composite_record.clone(), zmap_record.clone()] {
        if !blobs.iter().any(|b| b.hash == rec.hash) {
            blobs.push(rec);
        }
    }

    let manifest = Manifest {
        format_version: format_version_for(&blobs),
        producer: format!("FocusForge {}", env!("CARGO_PKG_VERSION")),
        created_at,
        hash_algorithm: HASH_ALGORITHM.to_string(),
        stack_fingerprint: stack_fingerprint.clone(),
        algorithm: request.algorithm.to_string(),
        params: request.params.clone(),
        align: request.align.clone(),
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
        crop: request.crop,
    };

    let checkpoint = Checkpoint {
        id: checkpoint_id.clone(),
        parent: Some(old_head_id),
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
            crop: manifest.crop,
            sources: manifest.sources.clone(),
            metrics: manifest.metrics,
        }),
    };

    let mut writer = ProjectWriter::create(request.project_path)?;
    let manifest_text = serde_json::to_string_pretty(&manifest)
        .map_err(|err| ProjectError::container(request.project_path, err))?;
    writer.add_text("manifest.json", &manifest_text)?;
    copy_forward(&mut old_archive, &mut writer, request.project_path, |_| {
        true
    })?;
    writer.add_blob(&composite.hash, &composite.path)?;
    writer.add_blob(&zmap.hash, &zmap.path)?;
    let checkpoint_text = serde_json::to_string_pretty(&checkpoint)
        .map_err(|err| ProjectError::container(request.project_path, err))?;
    writer.add_text(
        &format!("checkpoints/{checkpoint_id}.json"),
        &checkpoint_text,
    )?;

    let path = writer.finish()?;

    Ok(SealedProject {
        path,
        composite: composite_record,
        zmap: zmap_record,
        checkpoint: checkpoint_id,
    })
}

/// Refuses (naming the missing blob) when the manifest declares a blob that
/// has no matching zip entry -- what a raw [`copy_forward`] would otherwise
/// silently drop, since it only ever copies entries that are actually there.
fn verify_blobs_present(
    old_archive: &mut zip::ZipArchive<File>,
    old_manifest: &Manifest,
    path: &Path,
) -> Result<(), ProjectError> {
    for blob in &old_manifest.blobs {
        let name = format!("blobs/{}.zst", blob.hash);
        old_archive
            .by_name(&name)
            .map_err(|err| ProjectError::container(path, format!("{name}: {err}")))?;
    }
    Ok(())
}

/// Copies every existing zip entry except `manifest.json` and any name
/// `keep` refuses, byte for byte, preserving names and archive order. The
/// append-only discipline: a re-serialize through serde would silently
/// rewrite an old checkpoint's bytes the moment a new `#[serde(default)]`
/// field is added.
fn copy_forward(
    old_archive: &mut zip::ZipArchive<File>,
    writer: &mut ProjectWriter,
    path: &Path,
    keep: impl Fn(&str) -> bool,
) -> Result<(), ProjectError> {
    let names: Vec<String> = old_archive.file_names().map(str::to_string).collect();
    for name in names {
        if name == "manifest.json" || !keep(&name) {
            continue;
        }
        let mut entry = old_archive
            .by_name(&name)
            .map_err(|err| ProjectError::container(path, format!("{name}: {err}")))?;
        writer.add_stream(&name, &mut entry)?;
    }
    Ok(())
}

/// Whether a checkpoint's Z-Map is stored as a patch against its parent's
/// materialized map (whichever compresses smaller), the same choice capped
/// by the snapshot cadence, or always in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZMapStorage {
    /// Store whichever of a full map and a patch against the parent
    /// compresses smaller, with no cap on how long the resulting chain
    /// grows.
    Smallest,
    /// [`Self::Smallest`], except a full map is stored whenever the
    /// parent's [`ZMapChain::needs_snapshot`] is true.
    Bounded,
    /// Always store the full map.
    Full,
}

/// The byte-budget half of the snapshot cadence: a checkpoint is forced
/// full once the deltas already written since the last snapshot outweigh
/// that snapshot's own compressed size by this multiple. A ratio rather
/// than a fixed byte count, so the same rule self-tunes across map sizes
/// that differ by orders of magnitude instead of being tuned for one.
pub const SNAPSHOT_BYTE_BUDGET: u64 = 1;

/// The count half of the snapshot cadence: a checkpoint is forced full once
/// this many deltas already follow the last snapshot, regardless of their
/// bytes -- the case the byte budget alone cannot see, a long run of edits
/// each too small to ever outweigh the snapshot.
pub const SNAPSHOT_CHECKPOINT_CAP: usize = 32;

/// What [`super::Project::zmap_chain`] reports about the path from a
/// checkpoint's Z-Map back to its nearest full snapshot: the snapshot's own
/// compressed size, and the count and total compressed size of the deltas
/// resolved on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZMapChain {
    pub snapshot_bytes: u64,
    pub deltas: usize,
    pub delta_bytes: u64,
}

impl ZMapChain {
    /// Whether the cadence forces the next checkpoint on this chain to be a
    /// full snapshot: [`SNAPSHOT_CHECKPOINT_CAP`] deltas already follow the
    /// last snapshot, or their bytes already exceed it by
    /// [`SNAPSHOT_BYTE_BUDGET`].
    pub fn needs_snapshot(&self) -> bool {
        self.deltas >= SNAPSHOT_CHECKPOINT_CAP
            || self.delta_bytes > self.snapshot_bytes.saturating_mul(SNAPSHOT_BYTE_BUDGET)
    }
}

/// Seals `ops` as a new checkpoint onto `project_path`'s head, stamping the
/// current wall clock. Refuses an empty list ([`ProjectError::EmptyOpList`])
/// rather than writing an op-less checkpoint with no composite. Sources are
/// never opened or rehashed: the new checkpoint's `stack_fingerprint`,
/// `algorithm` and `params` are carried forward from the head unchanged, and
/// the manifest's `composite` keeps naming the fused ancestor's plane -- only
/// a new Z-Map blob is written, as whichever of [`ZMapStorage::Bounded`]'s
/// two encodings compresses smaller, or in full once the snapshot cadence
/// requires it.
pub fn append_ops(
    project_path: &Path,
    ops: Vec<Op>,
    label: Option<String>,
) -> Result<SealedProject, ProjectError> {
    append_ops_at(project_path, ops, label, now_unix(), ZMapStorage::Bounded)
}

/// [`append_ops`] with the creation timestamp supplied rather than read from
/// the clock, and the Z-Map storage choice supplied rather than always
/// [`ZMapStorage::Bounded`]. Seals onto the project's current head.
pub fn append_ops_at(
    project_path: &Path,
    ops: Vec<Op>,
    label: Option<String>,
    created_at: i64,
    storage: ZMapStorage,
) -> Result<SealedProject, ProjectError> {
    let project = Project::open(project_path)?;
    let parent = project.checkpoint().id.clone();
    append_ops_onto(project_path, &parent, ops, label, created_at, storage)
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
    storage: ZMapStorage,
) -> Result<SealedProject, ProjectError> {
    if ops.is_empty() {
        return Err(ProjectError::EmptyOpList {
            path: project_path.to_path_buf(),
        });
    }
    let dir = project_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging_name = format!(
        "{}.ops-staging",
        project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    let staging = dir.join(staging_name);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|err| ProjectError::io(&staging, err))?;
    let result = append_ops_staged(
        project_path,
        parent,
        &ops,
        label,
        created_at,
        &staging,
        storage,
    );
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// The smaller-of-two choice [`ZMapStorage::Smallest`] and
/// [`ZMapStorage::Bounded`] share once a delta is worth trying at all.
fn stage_smallest_delta(
    full: &StagedBlob,
    info: ZMapInfo,
    tile: Rect,
    indices: &[u16],
    staging: &Path,
) -> Result<Option<StagedBlob>, ProjectError> {
    let delta = blob::stage_delta(
        info.width(),
        info.height(),
        tile,
        indices,
        ZSTD_LEVEL,
        staging,
    )?;
    Ok((delta.compressed_len < full.compressed_len).then_some(delta))
}

fn append_ops_staged(
    project_path: &Path,
    parent: &str,
    ops: &[Op],
    label: Option<String>,
    created_at: i64,
    staging: &Path,
    storage: ZMapStorage,
) -> Result<SealedProject, ProjectError> {
    let mut project = Project::open(project_path)?;
    let head = project.history().get(parent)?.clone();
    let manifest = project.manifest().clone();
    let info = ZMapInfo::new(
        manifest.zmap.width,
        manifest.zmap.height,
        manifest.zmap.frame_count,
    )
    .map_err(|err| ProjectError::container(project_path, err))?;

    let tile = replay::replay_tile(ops, info).map_err(|(index, source)| ProjectError::Op {
        index,
        source: Box::new(source),
    })?;

    let head_zmap_path = staging.join("head-zmap.tif");
    materialize_zmap(&mut project, &head, &head_zmap_path)?;
    let head_reader = ZMapReader::open(&head_zmap_path)
        .map_err(|err| ProjectError::container(&head_zmap_path, err))?;

    let mut indices = vec![0u16; info.tile_len(tile)];
    head_reader
        .read_tile_into(tile, &mut indices)
        .map_err(|err| ProjectError::container(&head_zmap_path, err))?;

    let needs_fused = ops.iter().any(|op| matches!(op, Op::ResetRegion { .. }));
    let fused_path = if needs_fused && !head.ops.is_empty() {
        let fused_ancestor = project.fused_ancestor(parent)?.clone();
        let path = staging.join("fused-zmap.tif");
        materialize_zmap(&mut project, &fused_ancestor, &path)?;
        Some(path)
    } else {
        None
    };
    let fused_reader = match &fused_path {
        Some(path) => {
            Some(ZMapReader::open(path).map_err(|err| ProjectError::container(path, err))?)
        }
        None => None,
    };

    let mut fused = |rect: Rect, buf: &mut [u16]| -> Result<(), ZMapError> {
        match &fused_reader {
            Some(reader) => reader.read_tile_into(rect, buf),
            None => head_reader.read_tile_into(rect, buf),
        }
    };

    replay::replay(ops, info, tile, &mut indices, &mut fused).map_err(|(index, source)| {
        ProjectError::Op {
            index,
            source: Box::new(source),
        }
    })?;

    let new_zmap_path = staging.join("new-zmap.tif");
    {
        let mut writer = ZMapWriter::create(&new_zmap_path, info)
            .map_err(|err| ProjectError::container(&new_zmap_path, err))?;
        write_edited(&head_reader, tile, &indices, ROWS_PER_PASS, &mut writer)
            .map_err(|err| ProjectError::container(&new_zmap_path, err))?;
        writer
            .finish()
            .map_err(|err| ProjectError::container(&new_zmap_path, err))?;
    }

    let full = blob::stage_plane(&new_zmap_path, Plane::ZMap, ZSTD_LEVEL, staging)?;

    // A patch against the parent's own materialized map, kept only when it
    // compresses smaller than storing the whole map again -- never for
    // `ZMapStorage::Full`, never when the edit touched nearly every pixel,
    // which a patch cannot beat, and never past `ZMapStorage::Bounded`'s
    // snapshot cadence, which forces a full map instead.
    let delta = match storage {
        ZMapStorage::Smallest => stage_smallest_delta(&full, info, tile, &indices, staging)?,
        ZMapStorage::Bounded => {
            if project.zmap_chain(&head)?.needs_snapshot() {
                None
            } else {
                stage_smallest_delta(&full, info, tile, &indices, staging)?
            }
        }
        ZMapStorage::Full => None,
    };

    let (stored, stored_record) = match &delta {
        Some(delta) => (
            delta,
            BlobRecord {
                hash: delta.hash.clone(),
                kind: BlobKind::SparseDelta,
                compression: "zstd".to_string(),
                uncompressed_len: delta.uncompressed_len,
                compressed_len: delta.compressed_len,
                base: Some(head.zmap_blob.clone()),
                target: Some(full.hash.clone()),
            },
        ),
        None => (&full, record(&full)),
    };

    let checkpoint_id =
        Checkpoint::derive_child_id(&head.id, &head.stack_fingerprint, &full.hash, created_at);

    let mut blobs = manifest.blobs.clone();
    let already_present = blobs.iter().any(|b| {
        b.hash == stored_record.hash
            || (stored_record.target.is_some() && b.target == stored_record.target)
    });
    if !already_present {
        blobs.push(stored_record.clone());
    }

    // The checkpoint sealed here sits under `parent`'s own fuse, which is
    // not necessarily the project's current head's -- so the manifest's
    // composite, alignment, crop, sources and metrics come from `parent`'s
    // fused ancestor, not from the manifest already open.
    let fuse = project.fuse_state(parent)?;

    let new_manifest = Manifest {
        format_version: format_version_for(&blobs),
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
            blob: full.hash.clone(),
            ..manifest.zmap.clone()
        },
        blobs,
        metrics: fuse.metrics,
        checkpoint: checkpoint_id.clone(),
        crop: fuse.crop,
    };

    let checkpoint = Checkpoint {
        id: checkpoint_id.clone(),
        parent: Some(head.id.clone()),
        ops: ops.to_vec(),
        zmap_blob: full.hash.clone(),
        stack_fingerprint: head.stack_fingerprint.clone(),
        algorithm: head.algorithm.clone(),
        params: head.params.clone(),
        created_at,
        label,
        fuse: None,
    };

    let old_file = File::open(project_path).map_err(|err| ProjectError::io(project_path, err))?;
    let mut old_archive =
        zip::ZipArchive::new(old_file).map_err(|err| ProjectError::container(project_path, err))?;
    verify_blobs_present(&mut old_archive, &manifest, project_path)?;

    let mut writer = ProjectWriter::create(project_path)?;
    let manifest_text = serde_json::to_string_pretty(&new_manifest)
        .map_err(|err| ProjectError::container(project_path, err))?;
    writer.add_text("manifest.json", &manifest_text)?;
    copy_forward(&mut old_archive, &mut writer, project_path, |_| true)?;
    writer.add_blob(&stored.hash, &stored.path)?;
    let checkpoint_text = serde_json::to_string_pretty(&checkpoint)
        .map_err(|err| ProjectError::container(project_path, err))?;
    writer.add_text(
        &format!("checkpoints/{checkpoint_id}.json"),
        &checkpoint_text,
    )?;
    let path = writer.finish()?;

    let composite_record = manifest
        .blobs
        .iter()
        .find(|b| b.hash == fuse.composite.blob)
        .cloned()
        .expect("manifest.blobs always names a fuse's own composite blob");

    Ok(SealedProject {
        path,
        composite: composite_record,
        zmap: stored_record,
        checkpoint: checkpoint_id,
    })
}

/// What a successful [`delete_checkpoint`] removed, and the head it leaves
/// the project pointing at.
pub struct Deleted {
    /// `id` plus every checkpoint that descended from it.
    pub removed: Vec<String>,
    pub head: String,
}

/// Deletes `id` and every checkpoint descended from it. Refuses a parentless
/// `id` ([`ProjectError::RootDelete`]) before anything is written, since
/// removing the project's one root would leave the format's required
/// checkpoint field with nothing to name. When the removed set takes the
/// project's current head, the new head is the newest surviving checkpoint
/// by `created_at`, and the manifest's checkpoint, Z-Map blob, fingerprint,
/// algorithm, params, composite, alignment, crop, sources and metrics are
/// rebuilt from it via [`Project::fuse_state`] -- which refuses by name
/// ([`ProjectError::FuseStateUnknown`]) before any write when that survivor's
/// own fuse cannot be recovered. A blob is dropped only when every removed
/// checkpoint's own reference to it (its resolved Z-Map chain, or its fuse
/// record's composite) is gone and no surviving checkpoint still needs it.
pub fn delete_checkpoint(project_path: &Path, id: &str) -> Result<Deleted, ProjectError> {
    let project = Project::open(project_path)?;
    let target = project.history().get(id)?;
    if target.parent.is_none() {
        return Err(ProjectError::RootDelete {
            path: project_path.to_path_buf(),
            checkpoint: id.to_string(),
        });
    }

    let mut removed: Vec<String> = project
        .history()
        .descendants(id)
        .into_iter()
        .map(|c| c.id.clone())
        .collect();
    removed.push(id.to_string());
    let removed_set: HashSet<&str> = removed.iter().map(String::as_str).collect();

    let all = project.history().all();
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
    let head_removed = removed_set.contains(old_head_id.as_str());

    let new_manifest = if head_removed {
        let new_head = survivors
            .first()
            .expect("deleting the root is already refused, so a survivor always remains");
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
            crop: fuse.crop,
            ..manifest.clone()
        }
    } else {
        manifest.clone()
    };

    let removed_blobs =
        referenced_blobs(&manifest.blobs, removed_checkpoints.iter(), project_path)?;
    let survivor_blobs = referenced_blobs(&manifest.blobs, survivors.iter(), project_path)?;
    let kept_blobs: Vec<BlobRecord> = manifest
        .blobs
        .iter()
        .filter(|b| !removed_blobs.contains(&b.hash) || survivor_blobs.contains(&b.hash))
        .cloned()
        .collect();
    let kept_blob_names: HashSet<String> = kept_blobs
        .iter()
        .map(|b| format!("blobs/{}.zst", b.hash))
        .collect();
    let removed_checkpoint_names: HashSet<String> = removed
        .iter()
        .map(|id| format!("checkpoints/{id}.json"))
        .collect();

    let final_manifest = Manifest {
        format_version: format_version_for(&kept_blobs),
        blobs: kept_blobs,
        ..new_manifest
    };

    let old_file = File::open(project_path).map_err(|err| ProjectError::io(project_path, err))?;
    let mut old_archive =
        zip::ZipArchive::new(old_file).map_err(|err| ProjectError::container(project_path, err))?;

    let mut writer = ProjectWriter::create(project_path)?;
    let manifest_text = serde_json::to_string_pretty(&final_manifest)
        .map_err(|err| ProjectError::container(project_path, err))?;
    writer.add_text("manifest.json", &manifest_text)?;
    copy_forward(&mut old_archive, &mut writer, project_path, |name| {
        if removed_checkpoint_names.contains(name) {
            return false;
        }
        if name.starts_with("blobs/") {
            return kept_blob_names.contains(name);
        }
        true
    })?;
    writer.finish()?;

    Ok(Deleted {
        removed,
        head: final_manifest.checkpoint,
    })
}

/// Every blob hash `checkpoints` reaches: each one's resolved Z-Map chain
/// ([`resolve_chain`]'s base plus every delta on the way to it) and, when it
/// carries one, its own [`FuseRecord::composite`].
fn referenced_blobs<'a>(
    blobs: &[BlobRecord],
    checkpoints: impl Iterator<Item = &'a Checkpoint>,
    path: &Path,
) -> Result<HashSet<String>, ProjectError> {
    let mut set = HashSet::new();
    for checkpoint in checkpoints {
        let (base, deltas) = resolve_chain(blobs, &checkpoint.zmap_blob, path)?;
        set.insert(base.hash.clone());
        for delta in deltas {
            set.insert(delta.hash.clone());
        }
        if let Some(fuse) = &checkpoint.fuse {
            set.insert(fuse.composite.blob.clone());
        }
    }
    Ok(set)
}
