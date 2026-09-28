// SPDX-License-Identifier: GPL-3.0-or-later

//! Renders any checkpoint's image, fused or op-bearing, band by band.
//!
//! A fused checkpoint streams its own [`FuseRecord::composite`]. An
//! op-bearing checkpoint splices its nearest fused ancestor's composite,
//! replacing pixels where its replayed Z-Map disagrees with a gather over
//! the ancestor's aligned, cropped sources.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::align::{Transform, WarpingReader};
use crate::io::{TiffSpec, TiffStack, TiffWriter};
use crate::retouch::{Sources, recomposite};
use crate::tile::Rect;
use crate::zmap::{NO_SOURCE, ZMapReader};

use super::checkpoint::{Checkpoint, FuseRecord};
use super::container::partial_path;
use super::error::ProjectError;
use super::manifest::PlaneRef;
use super::validate::check_sources;
use super::{Project, materialize_zmap};

/// A checkpoint's rendered image, resolved and shape-checked but not yet
/// streamed: every refusal [`Self::prepare`] can produce has already
/// happened by the time this exists, so a caller can allocate an output
/// from [`Self::width`]/[`Self::height`] before [`Self::render`] reads or
/// writes a single sample.
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
}

/// The band sink every render path hands its bands to: samples plus the
/// rect they cover, in image coordinates.
type RenderSink<'a> = dyn FnMut(&[u16], Rect) -> Result<(), ProjectError> + 'a;

impl CheckpointImage {
    /// Resolves `id` against `project`, refusing an unknown id
    /// ([`ProjectError::UnknownCheckpoint`]), a fuse ancestor with no
    /// recorded state ([`ProjectError::FuseStateUnknown`]), unrecorded
    /// alignment ([`ProjectError::RenderAlignmentUnrecorded`]), or sources
    /// missing or changed on disk
    /// ([`ProjectError::RenderSourcesUnavailable`]).
    pub fn prepare(project: &Project, id: &str) -> Result<Self, ProjectError> {
        let checkpoint = project.history().get(id)?.clone();
        let ancestor_checkpoint = project.fused_ancestor(id)?.clone();
        let record = project.fuse_state(id)?;

        if ancestor_checkpoint.id == checkpoint.id {
            return Ok(CheckpointImage {
                width: record.composite.width,
                height: record.composite.height,
                samples_per_pixel: record.composite.samples_per_pixel,
                description: record.composite.description.clone(),
                body: Body::Fused(record.composite),
            });
        }

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

        Ok(CheckpointImage {
            width: record.composite.width,
            height: record.composite.height,
            samples_per_pixel: record.composite.samples_per_pixel,
            description: record.composite.description.clone(),
            body: Body::OpBearing {
                checkpoint: Box::new(checkpoint),
                ancestor_checkpoint: Box::new(ancestor_checkpoint),
                ancestor: Box::new(record),
            },
        })
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
        }
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

        let source_paths: Vec<PathBuf> = ancestor
            .sources
            .iter()
            .map(|s| project.source_path(s))
            .collect();
        let stack = TiffStack::open_files(&source_paths)
            .map_err(|err| ProjectError::container(&project_path, err))?;
        let spp = stack.header(0).samples_per_pixel;
        if stack.len() != ancestor.sources.len() || spp != ancestor.composite.samples_per_pixel {
            return Err(ProjectError::container(
                &project_path,
                format!(
                    "checkpoint {}'s source stack disagrees with its fuse record's shape",
                    checkpoint.id
                ),
            ));
        }
        let frame = stack.dimensions();
        let transforms = match &ancestor.align {
            Some(align) => align.transforms.clone(),
            None => vec![Transform::identity(); ancestor.sources.len()],
        };
        let warping = WarpingReader::new(&stack, frame, spp, transforms);
        let origin = ancestor.crop.map(|r| (r.x, r.y)).unwrap_or((0, 0));
        let sources = Sources {
            reader: &warping,
            frame_count: ancestor.sources.len() as u32,
            samples_per_pixel: spp,
            origin,
        };

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

/// The TIFF wrapper: [`CheckpointImage::prepare`] then
/// [`CheckpointImage::render`] into a `.partial` sibling of `path`, renamed
/// onto it only once every band has been written; removed instead on any
/// error, so a refused or interrupted render leaves nothing behind.
pub fn render_checkpoint(project: &mut Project, id: &str, path: &Path) -> Result<(), ProjectError> {
    let image = CheckpointImage::prepare(project, id)?;
    let partial = partial_path(path);
    let outcome = (|| -> Result<(), ProjectError> {
        let mut writer = TiffWriter::create(
            &partial,
            TiffSpec {
                width: image.width,
                height: image.height,
                samples_per_pixel: image.samples_per_pixel,
                description: Some(image.description.clone()),
            },
        )
        .map_err(|err| ProjectError::container(&partial, err))?;
        image.render(project, &mut |band, rect| {
            writer
                .write_tile(rect, band)
                .map_err(|err| ProjectError::container(path, err))
        })?;
        writer
            .finish()
            .map_err(|err| ProjectError::container(path, err))?;
        std::fs::rename(&partial, path).map_err(|err| ProjectError::io(path, err))
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    outcome
}
