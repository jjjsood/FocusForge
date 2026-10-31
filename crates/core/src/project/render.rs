// SPDX-License-Identifier: GPL-3.0-or-later

//! Renders a checkpoint's or a held result's image, band by band.
//!
//! One render serves the view ([`render_checkpoint`]), the export
//! ([`super::export::export_checkpoint`], [`super::export::export_held`])
//! and any tile of either. A fused checkpoint streams its stored composite
//! and a held result its composite TIFF, reading no source frame; an
//! op-bearing checkpoint blends its brush strokes, in log order, over its
//! nearest fused ancestor's composite, reading source frames only for a tile
//! some stroke meets. A [`PendingState`] -- a composite plus strokes not yet
//! saved -- renders the same way, with no project.

use std::path::{Path, PathBuf};

use crate::align::{Registration, WarpingReader};
use crate::equalize::GainReader;
use crate::io::{FrameSet, TiffSpec, TiffStack, TiffWriter, open_frame_set};
use crate::retouch::blend::{self, Blend};
use crate::tile::{HostBudget, Rect, Sample, SampleFormat};
use crate::zmap::ZMapInfo;

use super::checkpoint::{Checkpoint, FuseRecord, Op};
use super::container::partial_path;
use super::error::ProjectError;
use super::manifest::PlaneRef;
use super::seal::HeldCheckpoint;
use super::validate::check_sources;
use super::{LeSample, Project, READ_BAND_BYTES};

/// A checkpoint's rendered image, resolved and shape-checked but not yet
/// streamed: every refusal [`Self::prepare`] or [`Self::prepare_held`] can
/// produce has already happened by the time this exists, so a caller can
/// allocate an output from [`Self::width`]/[`Self::height`] before
/// [`Self::render`] reads or writes a single sample.
pub struct CheckpointImage {
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub description: String,
    /// What the image renders in: `U16` for a TIFF project, `F32` for one
    /// whose document keeps values outside the 16-bit range.
    pub sample_format: SampleFormat,
    body: Body,
}

enum Body {
    /// Streams `composite` directly; no Z-Map is read and no source frame
    /// is opened.
    Fused(PlaneRef),
    /// Blends `strokes` over `ancestor`'s composite; source frames are
    /// opened only when a stroke meets the rendered tile.
    Ops {
        checkpoint: Box<Checkpoint>,
        ancestor: Box<FuseRecord>,
        strokes: Vec<PlacedStroke>,
    },
    /// Streams a held result's composite TIFF; no project is read.
    Held(PathBuf),
    /// Blends `strokes` over the TIFF at `composite`, reading `sources` only
    /// for a tile some stroke meets; no project is read.
    Pending {
        composite: PathBuf,
        sources: Vec<PathBuf>,
        registration: Registration,
        info: ZMapInfo,
        strokes: Vec<PlacedStroke>,
    },
}

/// A brush stroke with the frame it blends and the part of the image it can
/// change.
struct PlacedStroke {
    op: Op,
    source: usize,
    footprint: Rect,
}

/// An image not yet saved: the TIFF at `composite` with the brush strokes
/// `ops` applied over it, in order, from `sources`' frames read through
/// `registration`. It has no project of its own.
#[derive(Debug, Clone)]
pub struct PendingState {
    pub composite: PathBuf,
    pub ops: Vec<Op>,
    /// The source frames, in frame order.
    pub sources: Vec<PathBuf>,
    pub registration: Registration,
    /// Index into `sources` the export's EXIF and ICC are read from.
    pub reference: usize,
    /// The exact `ImageDescription` the export's TIFF IFD (or another
    /// format's equivalent field) carries.
    pub description: String,
}

/// The band sink every render path hands its bands to: samples plus the
/// rect they cover, in image coordinates.
pub(super) type RenderSink<'a, S = u16> = dyn FnMut(&[S], Rect) -> Result<(), ProjectError> + 'a;

/// A sample type the render function runs in.
trait DocumentSample: Blend + LeSample {}
impl<S: Blend + LeSample> DocumentSample for S {}

/// Bound to whatever produced the [`CheckpointImage`] being written -- a
/// live project or a [`PendingState`] -- so a writer function need not know
/// which; built once per render/export call as `|sink| image.render(...)`.
pub(super) type Renderer<'a> = dyn FnMut(&mut RenderSink<'_>) -> Result<(), ProjectError> + 'a;

impl CheckpointImage {
    fn shaped(composite: &PlaneRef, sample_format: SampleFormat, body: Body) -> Self {
        CheckpointImage {
            width: composite.width,
            height: composite.height,
            samples_per_pixel: composite.samples_per_pixel,
            description: composite.description.clone(),
            sample_format,
            body,
        }
    }

    /// The render of a saved checkpoint. Resolves `id` against `project`,
    /// refusing an unknown id ([`ProjectError::UnknownCheckpoint`]). A fused
    /// checkpoint needs no source frames; an op-bearing one is also refused
    /// for unrecorded alignment ([`ProjectError::RenderAlignmentUnrecorded`])
    /// or an op that is no valid brush stroke ([`ProjectError::Op`]), and
    /// its render for sources missing or changed on disk
    /// ([`ProjectError::RenderSourcesUnavailable`]) when a stroke meets the
    /// tile.
    pub fn prepare(project: &Project, id: &str) -> Result<Self, ProjectError> {
        let checkpoint = project.history().get(id)?.clone();
        let record = project.fuse_state(id)?;

        let composite = record.composite.clone();
        let format = project.plane_format(&composite)?;
        let ops = project.history().op_chain(id)?;
        if ops.is_empty() {
            return Ok(Self::shaped(
                &composite,
                format,
                Body::Fused(record.composite),
            ));
        }

        check_alignment(project, &checkpoint, &record)?;
        let info = ZMapInfo::new(
            composite.width,
            composite.height,
            record.sources.len() as u32,
        )
        .map_err(|err| ProjectError::container(project.path(), err))?;
        let strokes = place_strokes(ops.into_iter(), info)?;
        Ok(Self::shaped(
            &composite,
            format,
            Body::Ops {
                checkpoint: Box::new(checkpoint),
                ancestor: Box::new(record),
                strokes,
            },
        ))
    }

    /// The render of a held result: its composite TIFF, with no project and
    /// no source frame.
    pub fn prepare_held(held: &HeldCheckpoint) -> Result<Self, ProjectError> {
        let stack = TiffStack::open_staged(&held.composite)
            .map_err(|err| ProjectError::container(&held.composite, err))?;
        if stack.dimensions() != (held.width, held.height) {
            let (width, height) = stack.dimensions();
            return Err(ProjectError::container(
                &held.composite,
                format!(
                    "the held composite is {width}x{height}, not the recorded {}x{}",
                    held.width, held.height
                ),
            ));
        }
        Ok(CheckpointImage {
            width: held.width,
            height: held.height,
            samples_per_pixel: stack.header(0).samples_per_pixel,
            description: held.composite_description.clone(),
            sample_format: stack.sample_format(),
            body: Body::Held(held.composite.clone()),
        })
    }

    /// The render of a [`PendingState`]: its composite with every stroke
    /// blended over it. Refuses an op that is no valid brush stroke
    /// ([`ProjectError::Op`]), a missing source
    /// ([`ProjectError::ExportSourcesUnavailable`]) and a composite or source
    /// that cannot be read. A source has no stored fingerprint to check
    /// against, so a changed file is not detected.
    pub fn prepare_pending(state: &PendingState) -> Result<Self, ProjectError> {
        let composite = &state.composite;
        let stack = TiffStack::open_staged(composite)
            .map_err(|err| ProjectError::container(composite, err))?;
        let (width, height) = stack.dimensions();
        let samples_per_pixel = stack.header(0).samples_per_pixel;
        check_pending_sources(&state.sources, composite)?;
        open_pending_sources(&state.sources, composite, samples_per_pixel)?;
        let info = ZMapInfo::new(width, height, state.sources.len() as u32)
            .map_err(|err| ProjectError::container(composite, err))?;
        let strokes = place_strokes(state.ops.iter(), info)?;
        Ok(CheckpointImage {
            width,
            height,
            samples_per_pixel,
            description: state.description.clone(),
            sample_format: stack.sample_format(),
            body: Body::Pending {
                composite: composite.clone(),
                sources: state.sources.clone(),
                registration: state.registration.clone(),
                info,
                strokes,
            },
        })
    }

    /// The whole image as one tile.
    pub fn full(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    /// Streams `tile` of the image top-to-bottom, band by band. `sink`
    /// receives each band's samples and the rect they cover, in image
    /// coordinates, always inside `tile`. A saved image reads its whole
    /// plane even for a small tile, so its stored hash is still verified;
    /// a held or pending image reads only the rows `tile` covers. `project`
    /// is needed for a saved image and unused for the others. An `f32` image
    /// is rendered in `f32` and each band narrowed with `u16::narrow`.
    pub fn render(
        &self,
        project: Option<&mut Project>,
        tile: Rect,
        sink: &mut RenderSink<'_>,
    ) -> Result<(), ProjectError> {
        match self.sample_format {
            SampleFormat::U16 => self.render_as::<u16>(project, tile, sink),
            SampleFormat::F32 => {
                let mut narrowed: Vec<u16> = Vec::new();
                self.render_as::<f32>(project, tile, &mut |band, rect| {
                    narrowed.clear();
                    narrowed.extend(band.iter().map(|&v| u16::narrow(v)));
                    sink(&narrowed, rect)
                })
            }
        }
    }

    /// [`Self::render`] in `f32`, nothing clipped. Refuses an image that is
    /// not `f32`: a 16-bit image is neither widened nor narrowed here.
    pub fn render_f32(
        &self,
        project: Option<&mut Project>,
        tile: Rect,
        sink: &mut RenderSink<'_, f32>,
    ) -> Result<(), ProjectError> {
        if self.sample_format != SampleFormat::F32 {
            let path = match &self.body {
                Body::Held(path) => path.clone(),
                Body::Pending { composite, .. } => composite.clone(),
                Body::Fused(_) | Body::Ops { .. } => project
                    .as_deref()
                    .map_or_else(PathBuf::new, |p| p.path().to_path_buf()),
            };
            return Err(ProjectError::container(
                &path,
                "the image holds 16-bit samples, not f32",
            ));
        }
        self.render_as::<f32>(project, tile, sink)
    }

    fn render_as<S: DocumentSample>(
        &self,
        project: Option<&mut Project>,
        tile: Rect,
        sink: &mut RenderSink<'_, S>,
    ) -> Result<(), ProjectError> {
        assert_eq!(
            S::FORMAT,
            self.sample_format,
            "rendered in the image's own type"
        );
        assert_eq!(
            self.full().intersect(tile),
            Some(tile),
            "the tile must be a non-empty rectangle inside the image"
        );
        let spp = self.samples_per_pixel;
        match &self.body {
            Body::Fused(composite) => {
                let project = project.expect("a saved checkpoint renders against its project");
                stream_plane_to_sink(project, composite, &mut crop_bands(tile, spp, sink))
            }
            Body::Ops {
                checkpoint,
                ancestor,
                strokes,
            } => {
                let project = project.expect("a saved checkpoint renders against its project");
                render_ops(project, checkpoint, ancestor, strokes, tile, sink)
            }
            Body::Held(composite) => render_held(composite, spp, tile, sink),
            Body::Pending {
                composite,
                sources,
                registration,
                info,
                strokes,
            } => render_pending(
                composite,
                sources,
                registration,
                (*info, spp),
                strokes,
                tile,
                sink,
            ),
        }
    }
}

/// Refuses a render over `record`'s sources when its alignment names no
/// per-frame transforms.
fn check_alignment(
    project: &Project,
    checkpoint: &Checkpoint,
    record: &FuseRecord,
) -> Result<(), ProjectError> {
    if record.align.is_some() && record.registration.transforms.len() != record.sources.len() {
        return Err(ProjectError::RenderAlignmentUnrecorded {
            path: project.path().to_path_buf(),
            checkpoint: checkpoint.id.clone(),
        });
    }
    Ok(())
}

/// Refuses a render over `record`'s sources when any is missing or changed
/// on disk (one `missing: <path>` or `changed: <path> (found at <path>)` line
/// each).
fn check_sources_available(
    project: &Project,
    checkpoint: &Checkpoint,
    record: &FuseRecord,
) -> Result<(), ProjectError> {
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

/// Refuses `paths` (a pending state's sources, plain paths with no stored
/// fingerprint) when any is missing on disk, one `missing: <path>` line
/// each -- [`check_sources_available`]'s counterpart for a state that has no
/// checkpoint of its own to record a `changed` state against.
fn check_pending_sources(paths: &[PathBuf], composite: &Path) -> Result<(), ProjectError> {
    let detail: Vec<String> = paths
        .iter()
        .filter(|path| !path.is_file())
        .map(|path| format!("missing: {}", path.display()))
        .collect();
    if detail.is_empty() {
        return Ok(());
    }
    Err(ProjectError::ExportSourcesUnavailable {
        path: composite.to_path_buf(),
        detail: detail.join("\n"),
    })
}

/// Opens `paths` with the reader their headers permit (mmap TIFF, decoded
/// TIFF or RAW). `context_path` names the error when opening fails -- the
/// project's own path for a checkpoint's sources, the composite's for a
/// pending state, which has no project of its own.
fn open_sources(paths: &[PathBuf], context_path: &Path) -> Result<FrameSet, ProjectError> {
    open_frame_set(paths, HostBudget::host_derived(&crate::host::snapshot()))
        .map_err(|err| ProjectError::container(context_path, err))
}

/// [`open_sources`] plus the shape check a checkpoint's own recorded fuse
/// state can make (a pending state has no such expectation to check
/// against): the opened stack must hold exactly `record.sources.len()`
/// frames at `record.composite.samples_per_pixel`.
fn open_checkpoint_sources(
    project: &Project,
    checkpoint: &Checkpoint,
    paths: &[PathBuf],
    record: &FuseRecord,
) -> Result<FrameSet, ProjectError> {
    let set = open_sources(paths, project.path())?;
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

/// [`open_sources`] for a pending state's `paths`, refusing frames whose
/// samples per pixel differ from `composite`'s `samples_per_pixel`.
fn open_pending_sources(
    paths: &[PathBuf],
    composite: &Path,
    samples_per_pixel: u32,
) -> Result<FrameSet, ProjectError> {
    let set = open_sources(paths, composite)?;
    let found = set.reader.stack_context().samples_per_pixel;
    if found != samples_per_pixel {
        return Err(ProjectError::container(
            composite,
            format!(
                "the source frames hold {found} samples per pixel, the composite {samples_per_pixel}"
            ),
        ));
    }
    Ok(set)
}

/// Validates each of `ops` over `info`'s map and places those that change
/// something, in log order.
fn place_strokes<'a>(
    ops: impl Iterator<Item = &'a Op>,
    info: ZMapInfo,
) -> Result<Vec<PlacedStroke>, ProjectError> {
    let mut strokes = Vec::new();
    for (index, op) in ops.enumerate() {
        let footprint = blend::footprint(op, info).map_err(|err| ProjectError::Op {
            index,
            source: Box::new(err),
        })?;
        let Op::BrushStroke { source_id, .. } = op;
        if let Some(footprint) = footprint {
            strokes.push(PlacedStroke {
                op: op.clone(),
                source: usize::from(*source_id),
                footprint,
            });
        }
    }
    Ok(strokes)
}

/// `set`'s frames, read through `gained` (its brightness gains applied), under
/// `registration`'s per-frame alignment (identity when there is none), for a
/// stack of `frame_count` frames each holding `samples_per_pixel` samples.
fn warp<'a>(
    set: &FrameSet,
    gained: &'a GainReader<'a>,
    registration: &Registration,
    frame_count: usize,
    samples_per_pixel: u32,
) -> WarpingReader<'a> {
    let transforms = registration.transforms(frame_count);
    WarpingReader::new(
        gained,
        set.reader.dimensions(),
        samples_per_pixel,
        transforms,
    )
}

/// A sink handing `sink` only the part of each band that lies inside `tile`.
fn crop_bands<'a, 'b, S: Sample>(
    tile: Rect,
    samples_per_pixel: u32,
    sink: &'a mut RenderSink<'b, S>,
) -> impl FnMut(&[S], Rect) -> Result<(), ProjectError> + 'a {
    let spp = samples_per_pixel as usize;
    let mut scratch: Vec<S> = Vec::new();
    move |band, rect| {
        let Some(part) = tile.intersect(rect) else {
            return Ok(());
        };
        let row = rect.width as usize * spp;
        let first = (part.y - rect.y) as usize * row;
        if part.x == rect.x && part.width == rect.width {
            return sink(&band[first..first + part.height as usize * row], part);
        }
        let len = part.width as usize * spp;
        let column = (part.x - rect.x) as usize * spp;
        scratch.clear();
        for r in 0..part.height as usize {
            let start = first + r * row + column;
            scratch.extend_from_slice(&band[start..start + len]);
        }
        sink(&scratch, part)
    }
}

/// Streams `tile` of the TIFF at `path` in bands of at most
/// [`READ_BAND_BYTES`], reading only the rows the tile covers.
fn render_held<S: DocumentSample>(
    path: &Path,
    samples_per_pixel: u32,
    tile: Rect,
    sink: &mut RenderSink<'_, S>,
) -> Result<(), ProjectError> {
    let stack = TiffStack::open_staged(path).map_err(|err| ProjectError::container(path, err))?;
    let row_samples = tile.width as usize * samples_per_pixel as usize;
    let bytes = S::FORMAT.bytes() as usize;
    let rows = (READ_BAND_BYTES / (row_samples * bytes)).clamp(1, tile.height as usize) as u32;
    let mut band = vec![S::default(); row_samples * rows as usize];
    let end = tile.y + tile.height;
    let mut y = tile.y;
    while y < end {
        let rect = Rect {
            x: tile.x,
            y,
            width: tile.width,
            height: rows.min(end - y),
        };
        let samples = &mut band[..row_samples * rect.height as usize];
        S::read_tile(&stack, 0, rect, samples).map_err(|err| ProjectError::container(path, err))?;
        sink(samples, rect)?;
        y += rect.height;
    }
    Ok(())
}

/// [`Project::read_plane_ref_into`], with a sink that can fail -- what the
/// fused body streams into, and what the op-bearing body reads the
/// ancestor's composite bands through.
fn stream_plane_to_sink<S: DocumentSample>(
    project: &mut Project,
    plane_ref: &PlaneRef,
    sink: &mut RenderSink<'_, S>,
) -> Result<(), ProjectError> {
    let mut failure: Option<ProjectError> = None;
    project.read_plane_ref_as::<S>(plane_ref, &mut |band, rect| {
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

/// A sink blending each of `strokes` that meets a band over it, in log order,
/// from its frame read through `warping` at the crop `origin`, then handing the
/// band to `sink`. `context` names the error when a frame read fails.
fn blend_bands<'a, 'b, S: DocumentSample>(
    strokes: &'a [&'a PlacedStroke],
    info: ZMapInfo,
    warping: &'a WarpingReader<'a>,
    origin: (u32, u32),
    samples_per_pixel: usize,
    context: &'a Path,
    sink: &'a mut RenderSink<'b, S>,
) -> impl FnMut(&[S], Rect) -> Result<(), ProjectError> + 'a {
    let mut blended: Vec<S> = Vec::new();
    move |band, rect| {
        let placed: Vec<blend::PlacedStroke> = strokes
            .iter()
            .filter(|stroke| stroke.footprint.intersect(rect).is_some())
            .map(|stroke| blend::PlacedStroke {
                op: &stroke.op,
                frame: stroke.source,
                footprint: stroke.footprint,
            })
            .collect();
        if placed.is_empty() {
            return sink(band, rect);
        }
        blended.clear();
        blended.extend_from_slice(band);
        blend::apply_strokes(
            info,
            rect,
            &mut blended,
            &placed,
            warping,
            origin,
            samples_per_pixel,
            blend::apply::<S>,
        )
        .map_err(|err| ProjectError::container(context, err))?;
        sink(&blended, rect)
    }
}

/// `tile` of the TIFF at `composite` (`info` its map's shape, `spp` its samples
/// per pixel), each of `strokes` the tile meets blended in over it in log
/// order from its frame, read through the gains and alignment of
/// `registration`.
fn render_pending<S: DocumentSample>(
    composite: &Path,
    sources: &[PathBuf],
    registration: &Registration,
    (info, spp): (ZMapInfo, u32),
    strokes: &[PlacedStroke],
    tile: Rect,
    sink: &mut RenderSink<'_, S>,
) -> Result<(), ProjectError> {
    let touching: Vec<&PlacedStroke> = strokes
        .iter()
        .filter(|stroke| stroke.footprint.intersect(tile).is_some())
        .collect();
    if touching.is_empty() {
        return render_held(composite, spp, tile, sink);
    }
    let set = open_pending_sources(sources, composite, spp)?;
    let gained = GainReader::new(&*set.reader, registration.gains());
    let warping = warp(&set, &gained, registration, sources.len(), spp);
    let mut blend = blend_bands(
        &touching,
        info,
        &warping,
        registration.origin(),
        spp as usize,
        composite,
        sink,
    );
    render_held(composite, spp, tile, &mut blend)
}

/// `ancestor`'s stored composite over `tile`, each stroke the tile meets
/// blended in over it in log order from its frame, read through the gains and
/// alignment the fusion used.
fn render_ops<S: DocumentSample>(
    project: &mut Project,
    checkpoint: &Checkpoint,
    ancestor: &FuseRecord,
    strokes: &[PlacedStroke],
    tile: Rect,
    sink: &mut RenderSink<'_, S>,
) -> Result<(), ProjectError> {
    let spp = ancestor.composite.samples_per_pixel;
    let touching: Vec<&PlacedStroke> = strokes
        .iter()
        .filter(|stroke| stroke.footprint.intersect(tile).is_some())
        .collect();
    if touching.is_empty() {
        return stream_plane_to_sink(
            project,
            &ancestor.composite,
            &mut crop_bands(tile, spp, sink),
        );
    }

    check_sources_available(project, checkpoint, ancestor)?;
    let paths: Vec<PathBuf> = ancestor
        .sources
        .iter()
        .map(|s| project.source_path(s))
        .collect();
    let set = open_checkpoint_sources(project, checkpoint, &paths, ancestor)?;
    let gained = GainReader::new(&*set.reader, ancestor.registration.gains());
    let warping = warp(&set, &gained, &ancestor.registration, paths.len(), spp);
    let info = ZMapInfo::new(
        ancestor.composite.width,
        ancestor.composite.height,
        paths.len() as u32,
    )
    .expect("prepare built the same extent");
    let project_path = project.path().to_path_buf();
    let mut blend = blend_bands(
        &touching,
        info,
        &warping,
        ancestor.registration.origin(),
        spp as usize,
        &project_path,
        sink,
    );
    stream_plane_to_sink(
        project,
        &ancestor.composite,
        &mut crop_bands(tile, spp, &mut blend),
    )
}

/// A saved checkpoint's image as a TIFF holding the image's own samples: a
/// 16-bit file for a `u16` image, a 32-bit float file for an `f32` one, values
/// above white included. [`CheckpointImage::prepare`] then the render, into a
/// `.partial` sibling of `path`, renamed onto it only once every band has
/// been written; removed instead on any error, so a refused or interrupted
/// render leaves nothing behind.
pub fn render_checkpoint(project: &mut Project, id: &str, path: &Path) -> Result<(), ProjectError> {
    let image = CheckpointImage::prepare(project, id)?;
    match image.sample_format {
        SampleFormat::U16 => write_tiff(
            &image,
            &mut |sink| image.render(Some(&mut *project), image.full(), sink),
            path,
        ),
        SampleFormat::F32 => write_tiff_f32(&image, project, path),
    }
}

/// [`write_tiff`] for an `f32` image: its unclipped render as a 32-bit float
/// TIFF.
fn write_tiff_f32(
    image: &CheckpointImage,
    project: &mut Project,
    path: &Path,
) -> Result<(), ProjectError> {
    write_via_partial(path, |partial| {
        let mut writer = TiffWriter::create_f32(
            partial,
            TiffSpec {
                width: image.width,
                height: image.height,
                samples_per_pixel: image.samples_per_pixel,
                description: Some(image.description.clone()),
            },
        )
        .map_err(|err| ProjectError::container(partial, err))?;
        image.render_f32(Some(project), image.full(), &mut |band, rect| {
            writer
                .write_tile_f32(rect, band)
                .map_err(|err| ProjectError::container(path, err))
        })?;
        writer
            .finish()
            .map_err(|err| ProjectError::container(path, err))
    })
}

/// The default 16-bit uncompressed TIFF encoder: the view render's encoder
/// for a `u16` image, and every export format's own `Tiff{Sixteen, None}` case.
/// `render` streams `image`'s bands -- bound to whatever produced it, a
/// live project or a caller-supplied Z-Map file.
pub(super) fn write_tiff(
    image: &CheckpointImage,
    render: &mut Renderer<'_>,
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
        render(&mut |band, rect| {
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
/// every export format's encoder. Any other failure comes back as
/// [`ProjectError::ExportWriteFailed`], so a caller can tell "the destination
/// could not be written" apart from a render the project refuses
/// ([`ProjectError::RenderSourcesUnavailable`],
/// [`ProjectError::RenderAlignmentUnrecorded`], [`ProjectError::Op`]), which
/// passes through as it is.
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
    outcome.map_err(|err| match err {
        refusal @ (ProjectError::RenderSourcesUnavailable { .. }
        | ProjectError::RenderAlignmentUnrecorded { .. }
        | ProjectError::Op { .. }) => refusal,
        err => ProjectError::ExportWriteFailed {
            path: path.to_path_buf(),
            source: Box::new(err),
        },
    })
}
