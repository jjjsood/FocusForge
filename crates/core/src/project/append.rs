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

use std::fs::File;
use std::path::{Path, PathBuf};

use super::blob::{self, Plane, StagedBlob, ZSTD_LEVEL};
use super::checkpoint::Checkpoint;
use super::container::ProjectWriter;
use super::error::ProjectError;
use super::manifest::{AlignRecord, ByteOrderTag, Manifest, Metrics, PlaneRef};
use super::read_json;
use super::seal::{now_unix, record, source_ref};
use super::{FORMAT_VERSION, HASH_ALGORITHM, Op, Project, SealedProject, materialize_zmap};
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
        format_version: FORMAT_VERSION,
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
    };

    let mut writer = ProjectWriter::create(request.project_path)?;
    let manifest_text = serde_json::to_string_pretty(&manifest)
        .map_err(|err| ProjectError::container(request.project_path, err))?;
    writer.add_text("manifest.json", &manifest_text)?;
    copy_forward(&mut old_archive, &mut writer, request.project_path)?;
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

/// Copies every existing zip entry except `manifest.json`, byte for byte,
/// preserving their names and archive order. The append-only discipline: a
/// re-serialize through serde would silently rewrite an old checkpoint's
/// bytes the moment a new `#[serde(default)]` field is added.
fn copy_forward(
    old_archive: &mut zip::ZipArchive<File>,
    writer: &mut ProjectWriter,
    path: &Path,
) -> Result<(), ProjectError> {
    let names: Vec<String> = old_archive.file_names().map(str::to_string).collect();
    for name in names {
        if name == "manifest.json" {
            continue;
        }
        let mut entry = old_archive
            .by_name(&name)
            .map_err(|err| ProjectError::container(path, format!("{name}: {err}")))?;
        writer.add_stream(&name, &mut entry)?;
    }
    Ok(())
}

/// Seals `ops` as a new checkpoint onto `project_path`'s head, stamping the
/// current wall clock. Refuses an empty list ([`ProjectError::EmptyOpList`])
/// rather than writing an op-less checkpoint with no composite. Sources are
/// never opened or rehashed: the new checkpoint's `stack_fingerprint`,
/// `algorithm` and `params` are carried forward from the head unchanged, and
/// the manifest's `composite` keeps naming the fused ancestor's plane -- only
/// a new Z-Map blob is written.
pub fn append_ops(
    project_path: &Path,
    ops: Vec<Op>,
    label: Option<String>,
) -> Result<SealedProject, ProjectError> {
    append_ops_at(project_path, ops, label, now_unix())
}

/// [`append_ops`] with the creation timestamp supplied rather than read from
/// the clock.
pub fn append_ops_at(
    project_path: &Path,
    ops: Vec<Op>,
    label: Option<String>,
    created_at: i64,
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
    let result = append_ops_staged(project_path, &ops, label, created_at, &staging);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn append_ops_staged(
    project_path: &Path,
    ops: &[Op],
    label: Option<String>,
    created_at: i64,
    staging: &Path,
) -> Result<SealedProject, ProjectError> {
    let mut project = Project::open(project_path)?;
    let head = project.checkpoint().clone();
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
        let chain = project.checkpoint_chain()?;
        let fused_ancestor = chain
            .into_iter()
            .find(|c| c.ops.is_empty())
            .ok_or_else(|| {
                ProjectError::container(project_path, "no fused ancestor in the checkpoint chain")
            })?;
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

    let zmap = blob::stage_plane(&new_zmap_path, Plane::ZMap, ZSTD_LEVEL, staging)?;
    let zmap_record = record(&zmap);

    let checkpoint_id =
        Checkpoint::derive_child_id(&head.id, &head.stack_fingerprint, &zmap.hash, created_at);

    let mut blobs = manifest.blobs.clone();
    if !blobs.iter().any(|b| b.hash == zmap_record.hash) {
        blobs.push(zmap_record.clone());
    }

    let new_manifest = Manifest {
        format_version: manifest.format_version,
        producer: manifest.producer.clone(),
        created_at,
        hash_algorithm: manifest.hash_algorithm.clone(),
        stack_fingerprint: manifest.stack_fingerprint.clone(),
        algorithm: manifest.algorithm.clone(),
        params: manifest.params.clone(),
        align: manifest.align.clone(),
        sources: manifest.sources.clone(),
        composite: manifest.composite.clone(),
        zmap: PlaneRef {
            blob: zmap.hash.clone(),
            ..manifest.zmap.clone()
        },
        blobs,
        metrics: manifest.metrics,
        checkpoint: checkpoint_id.clone(),
        crop: manifest.crop,
    };

    let checkpoint = Checkpoint {
        id: checkpoint_id.clone(),
        parent: Some(head.id.clone()),
        ops: ops.to_vec(),
        zmap_blob: zmap.hash.clone(),
        stack_fingerprint: head.stack_fingerprint.clone(),
        algorithm: head.algorithm.clone(),
        params: head.params.clone(),
        created_at,
        label,
    };

    let old_file = File::open(project_path).map_err(|err| ProjectError::io(project_path, err))?;
    let mut old_archive =
        zip::ZipArchive::new(old_file).map_err(|err| ProjectError::container(project_path, err))?;
    verify_blobs_present(&mut old_archive, &manifest, project_path)?;

    let mut writer = ProjectWriter::create(project_path)?;
    let manifest_text = serde_json::to_string_pretty(&new_manifest)
        .map_err(|err| ProjectError::container(project_path, err))?;
    writer.add_text("manifest.json", &manifest_text)?;
    copy_forward(&mut old_archive, &mut writer, project_path)?;
    writer.add_blob(&zmap.hash, &zmap.path)?;
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
        .find(|b| b.hash == manifest.composite.blob)
        .cloned()
        .expect("manifest.composite.blob always names an entry in manifest.blobs");

    Ok(SealedProject {
        path,
        composite: composite_record,
        zmap: zmap_record,
        checkpoint: checkpoint_id,
    })
}
