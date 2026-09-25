// SPDX-License-Identifier: GPL-3.0-or-later

//! [`append`]: writes a new checkpoint onto an already-sealed project,
//! extending its chain rather than starting a fresh container.
//!
//! Reuses [`super::container::ProjectWriter`]'s `.partial`-then-rename
//! discipline and [`super::blob::stage_plane`]'s staging exactly as
//! [`super::seal::seal`] does. The previous checkpoint and every blob it
//! names survive the write because the old container's own entries are
//! copied into the new one before the new ones are added, and every read of
//! the old container happens before the writer is finished -- the rename
//! that replaces it lands after the last read.

use std::fs::File;
use std::path::{Path, PathBuf};

use super::blob::{self, Plane, StagedBlob, ZSTD_LEVEL};
use super::checkpoint::Checkpoint;
use super::checkpoint_chain_from;
use super::container::ProjectWriter;
use super::error::ProjectError;
use super::manifest::{AlignRecord, ByteOrderTag, Manifest, Metrics, PlaneRef};
use super::read_json;
use super::seal::{now_unix, record, source_ref};
use super::{FORMAT_VERSION, HASH_ALGORITHM, SealedProject};
use crate::params::ParamValues;

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
    let old_head_id = old_manifest.checkpoint.clone();
    let chain = checkpoint_chain_from(&mut old_archive, request.project_path, &old_head_id)?;

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

    for old_blob in &old_manifest.blobs {
        let name = format!("blobs/{}.zst", old_blob.hash);
        let mut entry = old_archive.by_name(&name).map_err(|err| {
            ProjectError::container(request.project_path, format!("{name}: {err}"))
        })?;
        writer.add_stream(&name, &mut entry)?;
    }
    writer.add_blob(&composite.hash, &composite.path)?;
    writer.add_blob(&zmap.hash, &zmap.path)?;

    for old_checkpoint in chain.iter().rev() {
        let text = serde_json::to_string_pretty(old_checkpoint)
            .map_err(|err| ProjectError::container(request.project_path, err))?;
        writer.add_text(&format!("checkpoints/{}.json", old_checkpoint.id), &text)?;
    }
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
