// SPDX-License-Identifier: GPL-3.0-or-later

//! Renders any checkpoint's image, fused or op-bearing, band by band.
//!
//! The view render ([`CheckpointImage::prepare`], [`render_checkpoint`])
//! streams a fused checkpoint's own composite, splicing an op-bearing one's
//! replayed-Z-Map edits over its nearest fused ancestor's composite. The
//! export render ([`CheckpointImage::prepare_export`],
//! [`super::export::export_checkpoint`]) reads no stored composite: every
//! checkpoint, fused included, gathers its own Z-Map over that ancestor's
//! aligned, cropped sources.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::align::{Transform, WarpingReader};
use crate::io::{FrameSet, TiffSpec, TiffWriter, open_frame_set};
use crate::retouch::{RetouchError, Sources, recomposite, recomposite_bands};
use crate::tile::{HostBudget, Rect};
use crate::zmap::{NO_SOURCE, ZMapReader};

use super::checkpoint::{Checkpoint, FuseRecord};
use super::container::partial_path;
use super::error::ProjectError;
use super::manifest::PlaneRef;
use super::validate::check_sources;
use super::{Project, materialize_zmap};

/// Rows per band of the export gather.
const GATHER_ROWS: u32 = 256;

/// A checkpoint's rendered image, resolved and shape-checked but not yet
/// streamed: every refusal [`Self::prepare`] or [`Self::prepare_export`] can
/// produce has already happened by the time this exists, so a caller can
/// allocate an output from [`Self::width`]/[`Self::height`] before
/// [`Self::render`] reads or writes a single sample.
pub struct CheckpointImage {
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub description: String,
    body: Body,
}

enum Body {
    /// Streams `composite` directly; no Z-Map is read and no source frame
    /// is opened.
    Fused(PlaneRef),
    OpBearing {
        checkpoint: Box<Checkpoint>,
        ancestor_checkpoint: Box<Checkpoint>,
        ancestor: Box<FuseRecord>,
    },
    /// Gathers `checkpoint`'s own Z-Map over `record`'s sources; no stored
    /// composite is read.
    Gather {
        checkpoint: Box<Checkpoint>,
        record: Box<FuseRecord>,
    },
}

/// The band sink every render path hands its bands to: samples plus the
/// rect they cover, in image coordinates.
type RenderSink<'a> = dyn FnMut(&[u16], Rect) -> Result<(), ProjectError> + 'a;

impl CheckpointImage {
    fn shaped(composite: &PlaneRef, body: Body) -> Self {
        CheckpointImage {
            width: composite.width,
            height: composite.height,
            samples_per_pixel: composite.samples_per_pixel,
            description: composite.description.clone(),
            body,
        }
    }

    /// The view render. Resolves `id` against `project`, refusing an unknown
    /// id ([`ProjectError::UnknownCheckpoint`]) or a fuse ancestor with no
    /// recorded state ([`ProjectError::FuseStateUnknown`]). A fused
    /// checkpoint needs no source frames; an op-bearing one is also refused
    /// for unrecorded alignment ([`ProjectError::RenderAlignmentUnrecorded`])
    /// or sources missing or changed on disk
    /// ([`ProjectError::RenderSourcesUnavailable`]).
    pub fn prepare(project: &Project, id: &str) -> Result<Self, ProjectError> {
        let checkpoint = project.history().get(id)?.clone();
        let ancestor_checkpoint = project.fused_ancestor(id)?.clone();
        let record = project.fuse_state(id)?;

        let composite = record.composite.clone();
        if ancestor_checkpoint.id == checkpoint.id {
            return Ok(Self::shaped(&composite, Body::Fused(record.composite)));
        }

        check_renderable(project, &checkpoint, &record)?;
        Ok(Self::shaped(
            &composite,
            Body::OpBearing {
                checkpoint: Box::new(checkpoint),
                ancestor_checkpoint: Box::new(ancestor_checkpoint),
                ancestor: Box::new(record),
            },
        ))
    }

    /// The export render. Resolves `id` like [`Self::prepare`], but refuses
    /// unrecorded alignment and missing or changed sources for every
    /// checkpoint, fused included, since each one is gathered from its own
    /// Z-Map.
    pub fn prepare_export(project: &Project, id: &str) -> Result<Self, ProjectError> {
        let checkpoint = project.history().get(id)?.clone();
        let record = project.fuse_state(id)?;
        check_renderable(project, &checkpoint, &record)?;
        let composite = record.composite.clone();
        Ok(Self::shaped(
            &composite,
            Body::Gather {
                checkpoint: Box::new(checkpoint),
                record: Box::new(record),
            },
        ))
    }

    /// Streams the image top-to-bottom, full-width band by band. `sink`
    /// receives each band's samples and the rect they cover, in image
    /// coordinates.
    pub fn render(
        &self,
        project: &mut Project,
        sink: &mut RenderSink<'_>,
    ) -> Result<(), ProjectError> {
        match &self.body {
            Body::Fused(composite) => stream_plane_to_sink(project, composite, sink),
            Body::OpBearing {
                checkpoint,
                ancestor_checkpoint,
                ancestor,
            } => render_op_bearing(project, checkpoint, ancestor_checkpoint, ancestor, sink),
            Body::Gather { checkpoint, record } => render_gather(project, checkpoint, record, sink),
        }
    }
}

/// Refuses a gather over `record`'s sources when its alignment names no
/// per-frame transforms, or when any source is missing or changed on disk
/// (one `missing: <path>` or `changed: <path> (found at <path>)` line each).
fn check_renderable(
    project: &Project,
    checkpoint: &Checkpoint,
    record: &FuseRecord,
) -> Result<(), ProjectError> {
    if let Some(align) = &record.align
        && align.transforms.len() != record.sources.len()
    {
        return Err(ProjectError::RenderAlignmentUnrecorded {
            path: project.path().to_path_buf(),
            checkpoint: checkpoint.id.clone(),
        });
    }
    let check = check_sources(&record.sources, |s| project.source_path(s))?;
    if !check.unresolved.is_empty() || !check.changed.is_empty() {
        let mut lines: Vec<String> = check
            .unresolved
            .iter()
            .map(|&idx| format!("missing: {}", record.sources[idx].path))
            .collect();
        lines.extend(check.changed.iter().map(|(idx, found)| {
            format!(
                "changed: {} (found at {})",
                record.sources[*idx].path,
                found.display()
            )
        }));
        return Err(ProjectError::RenderSourcesUnavailable {
            path: project.path().to_path_buf(),
            checkpoint: checkpoint.id.clone(),
            detail: lines.join("\n"),
        });
    }
    Ok(())
}

/// Opens `record`'s sources with the reader their headers permit (mmap
/// TIFF, decoded TIFF or RAW) and checks them against the record's shape.
fn open_sources(
    project: &Project,
    checkpoint: &Checkpoint,
    record: &FuseRecord,
) -> Result<FrameSet, ProjectError> {
    let paths: Vec<PathBuf> = record
        .sources
        .iter()
        .map(|s| project.source_path(s))
        .collect();
    let set = open_frame_set(&paths, HostBudget::host_derived(&crate::host::snapshot()))
        .map_err(|err| ProjectError::container(project.path(), err))?;
    if set.reader.len() != record.sources.len()
        || set.reader.stack_context().samples_per_pixel != record.composite.samples_per_pixel
    {
        return Err(ProjectError::container(
            project.path(),
            format!(
                "checkpoint {}'s source stack disagrees with its fuse record's shape",
                checkpoint.id
            ),
        ));
    }
    Ok(set)
}

/// `set`'s frames under `record`'s per-frame alignment (identity when the
/// fuse was not aligned).
fn warp<'a>(set: &'a FrameSet, record: &FuseRecord) -> WarpingReader<'a> {
    let transforms = match &record.align {
        Some(align) => align.transforms.clone(),
        None => vec![Transform::identity(); record.sources.len()],
    };
    WarpingReader::new(
        &*set.reader,
        set.reader.dimensions(),
        record.composite.samples_per_pixel,
        transforms,
    )
}

fn gather_sources<'a>(warping: &'a WarpingReader<'a>, record: &FuseRecord) -> Sources<'a> {
    Sources {
        reader: warping,
        frame_count: record.sources.len() as u32,
        samples_per_pixel: record.composite.samples_per_pixel,
        origin: record.crop.map(|r| (r.x, r.y)).unwrap_or((0, 0)),
    }
}

/// [`Project::read_plane_ref_into`], with a sink that can fail -- what the
/// fused body streams into, and what the op-bearing body reads the
/// ancestor's composite bands through.
fn stream_plane_to_sink(
    project: &mut Project,
    plane_ref: &PlaneRef,
    sink: &mut RenderSink<'_>,
) -> Result<(), ProjectError> {
    let mut failure: Option<ProjectError> = None;
    project.read_plane_ref_into(plane_ref, &mut |band, rect| {
        if failure.is_some() {
            return;
        }
        if let Err(err) = sink(band, rect) {
            failure = Some(err);
        }
    })?;
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// A process-wide counter, so two renders started in the same process in
/// the same second never share a temp Z-Map path.
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn temp_zmap_path(zmap_blob: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "focusforge-render-{}-{zmap_blob}-{id}.tif",
        std::process::id()
    ))
}

/// What a gather band can fail with: the gather itself, or the sink.
enum GatherError {
    Retouch(RetouchError),
    Sink(ProjectError),
}

impl From<RetouchError> for GatherError {
    fn from(err: RetouchError) -> Self {
        GatherError::Retouch(err)
    }
}

/// `checkpoint`'s own materialized Z-Map, gathered band by band over
/// `record`'s aligned, cropped sources.
fn render_gather(
    project: &mut Project,
    checkpoint: &Checkpoint,
    record: &FuseRecord,
    sink: &mut RenderSink<'_>,
) -> Result<(), ProjectError> {
    let zmap_path = temp_zmap_path(&checkpoint.zmap_blob);
    let outcome = (|| -> Result<(), ProjectError> {
        materialize_zmap(project, checkpoint, &zmap_path)?;
        let zmap =
            ZMapReader::open(&zmap_path).map_err(|err| ProjectError::container(&zmap_path, err))?;
        let set = open_sources(project, checkpoint, record)?;
        let warping = warp(&set, record);
        let sources = gather_sources(&warping, record);
        recomposite_bands(&zmap, &sources, GATHER_ROWS, |rect, _, samples| {
            sink(samples, rect).map_err(GatherError::Sink)
        })
        .map_err(|err| match err {
            GatherError::Retouch(err) => ProjectError::container(project.path(), err),
            GatherError::Sink(err) => err,
        })
    })();
    let _ = std::fs::remove_file(&zmap_path);
    outcome
}

/// The ancestor's stored composite, with every pixel whose replayed index
/// differs from the ancestor's own overwritten by a gather over the
/// ancestor's aligned, cropped sources.
fn render_op_bearing(
    project: &mut Project,
    checkpoint: &Checkpoint,
    ancestor_checkpoint: &Checkpoint,
    ancestor: &FuseRecord,
    sink: &mut RenderSink<'_>,
) -> Result<(), ProjectError> {
    let project_path = project.path().to_path_buf();
    let checkpoint_zmap_path = temp_zmap_path(&checkpoint.zmap_blob);
    let ancestor_zmap_path = temp_zmap_path(&ancestor_checkpoint.zmap_blob);

    let outcome = (|| -> Result<(), ProjectError> {
        materialize_zmap(project, checkpoint, &checkpoint_zmap_path)?;
        materialize_zmap(project, ancestor_checkpoint, &ancestor_zmap_path)?;

        let checkpoint_reader = ZMapReader::open(&checkpoint_zmap_path)
            .map_err(|err| ProjectError::container(&checkpoint_zmap_path, err))?;
        let ancestor_reader = ZMapReader::open(&ancestor_zmap_path)
            .map_err(|err| ProjectError::container(&ancestor_zmap_path, err))?;

        let set = open_sources(project, checkpoint, ancestor)?;
        let warping = warp(&set, ancestor);
        let sources = gather_sources(&warping, ancestor);

        let mut failure: Option<ProjectError> = None;
        let mut checkpoint_idx: Vec<u16> = Vec::new();
        let mut ancestor_idx: Vec<u16> = Vec::new();
        let mut masked: Vec<u16> = Vec::new();
        let mut gathered: Vec<u16> = Vec::new();
        let mut out_band: Vec<u16> = Vec::new();
        project.read_plane_ref_into(&ancestor.composite, &mut |band, rect| {
            if failure.is_some() {
                return;
            }
            let n = (rect.width * rect.height) as usize;
            checkpoint_idx.resize(n, 0);
            ancestor_idx.resize(n, 0);
            if let Err(err) = checkpoint_reader.read_tile_into(rect, &mut checkpoint_idx) {
                failure = Some(ProjectError::container(&checkpoint_zmap_path, err));
                return;
            }
            if let Err(err) = ancestor_reader.read_tile_into(rect, &mut ancestor_idx) {
                failure = Some(ProjectError::container(&ancestor_zmap_path, err));
                return;
            }
            masked.clear();
            masked.extend(
                checkpoint_idx
                    .iter()
                    .zip(ancestor_idx.iter())
                    .map(|(&c, &a)| if c != a { c } else { NO_SOURCE }),
            );
            gathered.resize(band.len(), 0);
            if let Err(err) = recomposite(&masked, rect, &sources, &mut gathered) {
                failure = Some(ProjectError::container(&project_path, err));
                return;
            }
            out_band.clear();
            out_band.extend_from_slice(band);
            let spp = sources.samples_per_pixel as usize;
            for i in 0..n {
                if checkpoint_idx[i] != ancestor_idx[i] {
                    out_band[i * spp..(i + 1) * spp]
                        .copy_from_slice(&gathered[i * spp..(i + 1) * spp]);
                }
            }
            if let Err(err) = sink(&out_band, rect) {
                failure = Some(err);
            }
        })?;
        match failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    })();

    let _ = std::fs::remove_file(&checkpoint_zmap_path);
    let _ = std::fs::remove_file(&ancestor_zmap_path);
    outcome
}

/// The view render as a TIFF: [`CheckpointImage::prepare`] then
/// [`CheckpointImage::render`] into a `.partial` sibling of `path`, renamed
/// onto it only once every band has been written; removed instead on any
/// error, so a refused or interrupted render leaves nothing behind.
pub fn render_checkpoint(project: &mut Project, id: &str, path: &Path) -> Result<(), ProjectError> {
    let image = CheckpointImage::prepare(project, id)?;
    write_tiff(project, &image, path)
}

/// The default 16-bit uncompressed TIFF encoder: the view render's only
/// encoder, and every export format's own `Tiff{Sixteen, None}` case.
pub(super) fn write_tiff(
    project: &mut Project,
    image: &CheckpointImage,
    path: &Path,
) -> Result<(), ProjectError> {
    write_via_partial(path, |partial| {
        let mut writer = TiffWriter::create(
            partial,
            TiffSpec {
                width: image.width,
                height: image.height,
                samples_per_pixel: image.samples_per_pixel,
                description: Some(image.description.clone()),
            },
        )
        .map_err(|err| ProjectError::container(partial, err))?;
        image.render(project, &mut |band, rect| {
            writer
                .write_tile(rect, band)
                .map_err(|err| ProjectError::container(path, err))
        })?;
        writer
            .finish()
            .map_err(|err| ProjectError::container(path, err))
    })
}

/// Writes through a `.partial` sibling of `path`, renamed onto it only once
/// `encode` returns `Ok`; removed instead on any error, so a refused or
/// interrupted write leaves nothing behind. Shared by the view render and
/// every export format's encoder.
pub(super) fn write_via_partial(
    path: &Path,
    encode: impl FnOnce(&Path) -> Result<(), ProjectError>,
) -> Result<(), ProjectError> {
    let partial = partial_path(path);
    let outcome = (|| -> Result<(), ProjectError> {
        encode(&partial)?;
        std::fs::rename(&partial, path).map_err(|err| ProjectError::io(path, err))
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    outcome
}
