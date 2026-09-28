// SPDX-License-Identifier: GPL-3.0-or-later

//! [`seal`]: the one function `crates/cli` calls to turn a finished run into
//! a project.
//!
//! **A project cannot be written tile by tile.** A blob's zip entry name
//! *is* the content hash of the finished plane, which is unknown until the
//! last tile, and zstd exposes no seekable format. So fusion keeps writing
//! its two staged TIFFs under a scratch directory, and the project is
//! sealed **after** the job completes, by staging then renaming the two
//! files onto the names a user gave. Sealing never touches the ledger, the
//! resume path, the coverage refusal or the cancel semantics, and it is
//! the only shape the format allows.
//!
//! This function is where all four format commitments are made concrete at
//! once: the [`Manifest::format_version`] it stamps, the [`BlobKind`] it
//! records on every blob, the hash it takes over *uncompressed* bytes with
//! its algorithm named, and the fully serialized [`Checkpoint`] --
//! `parent: None`, `ops: []` -- it writes beside them.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::blob::{self, Plane, StagedBlob, ZSTD_LEVEL};
use super::checkpoint::Checkpoint;
use super::container::ProjectWriter;
use super::error::ProjectError;
use super::manifest::{
    AlignRecord, BlobKind, BlobRecord, ByteOrderTag, Manifest, Metrics, PlaneRef, SourceRef,
    SourceResolution,
};
use super::{FORMAT_VERSION, HASH_ALGORITHM};
use crate::params::ParamValues;
use crate::tile::Rect;

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
    /// The composite's extent after any crop was applied.
    pub crop: Option<Rect>,
    /// Exactly the `ImageDescription` the staged composite carries, so
    /// `export` re-emits a byte-identical TIFF.
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

impl SealedProject {
    /// `uncompressed / compressed` for one blob -- the measured ratio, never
    /// an assumed one.
    pub fn compression_ratio(record: &BlobRecord) -> f64 {
        if record.compressed_len == 0 {
            return 0.0;
        }
        record.uncompressed_len as f64 / record.compressed_len as f64
    }
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
/// same inputs -- the `zip` dependency deliberately has no clock feature, so
/// every entry gets the default DOS timestamp -- which makes this the entry
/// point a reproducibility check uses.
pub fn seal_at(request: SealRequest<'_>, created_at: i64) -> Result<SealedProject, ProjectError> {
    let staging = request
        .staged_composite
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let mut staged: Vec<StagedBlob> = Vec::new();
    let result = seal_staged(&request, created_at, &staging, &mut staged);
    for blob in &staged {
        blob.remove();
    }
    result
}

fn seal_staged(
    request: &SealRequest<'_>,
    created_at: i64,
    staging: &Path,
    staged: &mut Vec<StagedBlob>,
) -> Result<SealedProject, ProjectError> {
    // The sources, hashed **sequentially**: `hash_file` maps the whole file,
    // so 61 reference frames hashed concurrently would map 16 GB at once.
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
    let checkpoint_id = Checkpoint::derive_id(&stack_fingerprint, &zmap.hash, created_at);

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
        blobs: blob_records(&composite_record, &zmap_record),
        metrics: request.metrics,
        checkpoint: checkpoint_id.clone(),
        crop: request.crop,
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
    };

    let mut writer = ProjectWriter::create(request.project_path)?;
    let manifest_text = serde_json::to_string_pretty(&manifest)
        .map_err(|err| ProjectError::container(request.project_path, err))?;
    writer.add_text("manifest.json", &manifest_text)?;
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

/// A finished run kept outside any container: its two planes as TIFF files
/// plus everything [`seal_at`] and [`super::append_at`] record beside them.
#[derive(Debug, Clone)]
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
    /// The composite's extent after any crop was applied.
    pub crop: Option<Rect>,
    /// Exactly the `ImageDescription` the composite carries.
    pub composite_description: String,
    pub metrics: Metrics,
    /// Unix seconds, UTC, stamped when the run finished.
    pub created_at: i64,
}

/// Writes `held`, in order and each with its own `created_at`, as one chain
/// at `target` -- onto the project there when `onto_existing`. Built at a
/// sibling temporary path, so relative sources resolve as at `target`, and
/// renamed over it once; on error `target` is left as it was.
pub fn seal_held(
    target: &Path,
    held: &[HeldCheckpoint],
    onto_existing: bool,
) -> Result<SealedProject, ProjectError> {
    if held.is_empty() {
        return Err(ProjectError::container(target, "no checkpoint to save"));
    }
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".saving");
    let temp = target.with_file_name(name);
    let result = seal_chain(target, &temp, held, onto_existing).and_then(|mut sealed| {
        std::fs::rename(&temp, target).map_err(|source| ProjectError::io(target, source))?;
        sealed.path = target.to_path_buf();
        Ok(sealed)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn seal_chain(
    target: &Path,
    temp: &Path,
    held: &[HeldCheckpoint],
    onto_existing: bool,
) -> Result<SealedProject, ProjectError> {
    if onto_existing {
        std::fs::copy(target, temp).map_err(|source| ProjectError::io(target, source))?;
    }
    let mut sealed = None;
    for (i, checkpoint) in held.iter().enumerate() {
        let written = if i == 0 && !onto_existing {
            seal_at(
                SealRequest {
                    project_path: temp,
                    staged_composite: &checkpoint.composite,
                    staged_zmap: &checkpoint.zmap,
                    sources: &checkpoint.sources,
                    algorithm: &checkpoint.algorithm,
                    params: &checkpoint.params,
                    align: checkpoint.align.clone(),
                    crop: checkpoint.crop,
                    composite_description: checkpoint.composite_description.clone(),
                    label: None,
                    metrics: checkpoint.metrics,
                },
                checkpoint.created_at,
            )?
        } else {
            super::append_at(
                super::AppendRequest {
                    project_path: temp,
                    staged_composite: &checkpoint.composite,
                    staged_zmap: &checkpoint.zmap,
                    sources: &checkpoint.sources,
                    algorithm: &checkpoint.algorithm,
                    params: &checkpoint.params,
                    align: checkpoint.align.clone(),
                    crop: checkpoint.crop,
                    composite_description: checkpoint.composite_description.clone(),
                    label: None,
                    metrics: checkpoint.metrics,
                },
                checkpoint.created_at,
            )?
        };
        sealed = Some(written);
    }
    Ok(sealed.expect("held is non-empty"))
}

/// The manifest's blob list: one record per **distinct** hash, in the order
/// the container writes them. Two identical planes collapse to one entry in
/// the archive, and the manifest must say the same thing the archive does.
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
    let hash = blob::hash_file(path)?;
    let bytes = std::fs::metadata(path)
        .map_err(|source| ProjectError::io(path, source))?
        .len();
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
        bytes,
    })
}

/// Unix seconds, UTC. `0` if the clock is before the epoch, which is not a
/// state worth failing a finished stack over.
pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
