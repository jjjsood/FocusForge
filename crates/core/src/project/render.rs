// SPDX-License-Identifier: GPL-3.0-or-later

//! Renders a checkpoint's or a held result's image, band by band.
//!
//! One render serves the view ([`render_checkpoint`]), the export
//! ([`super::export::export_checkpoint`], [`super::export::export_held`])
//! and any tile of either. A fused checkpoint streams its stored composite
//! and a held result its composite TIFF, reading no source frame; an
//! op-bearing checkpoint splices its replayed-Z-Map edits over its nearest
//! fused ancestor's composite.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::align::{Registration, WarpingReader};
use crate::equalize::GainReader;
use crate::io::{FrameSet, TiffSpec, TiffStack, TiffWriter, open_frame_set};
use crate::retouch::{RetouchError, Sources, recomposite, recomposite_bands};
use crate::tile::{HostBudget, Rect};
use crate::zmap::{NO_SOURCE, ZMapReader};

use super::checkpoint::{Checkpoint, FuseRecord};
use super::container::partial_path;
use super::error::ProjectError;
use super::manifest::PlaneRef;
use super::seal::HeldCheckpoint;
use super::validate::check_sources;
use super::{Project, READ_BAND_BYTES, materialize_zmap};

/// Rows per band of the export gather.
const GATHER_ROWS: u32 = 256;

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
    /// Streams a held result's composite TIFF; no project is read.
    Held(PathBuf),
    /// Gathers a caller-supplied Z-Map file over `source`'s sources -- no
    /// project is read at all. Only [`CheckpointImage::prepare_zmap`]
    /// produces this variant, and only [`CheckpointImage::render_external`]
    /// reads it.
    External {
        zmap_path: PathBuf,
        source: ZmapSource,
    },
}

/// A Z-Map file's placement: the sources it was gathered from, in frame
/// order, and their alignment -- the live counterpart of a checkpoint's own
/// [`FuseRecord`], for a Z-Map that has no project of its own (a held row's
/// staged map, or an edit session's Z-Map written to a temp file).
#[derive(Debug, Clone)]
pub struct ZmapSource {
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
pub(super) type RenderSink<'a> = dyn FnMut(&[u16], Rect) -> Result<(), ProjectError> + 'a;

/// Bound to whatever produced the [`CheckpointImage`] being written -- a
/// live project or a caller-supplied Z-Map file -- so a writer function
/// need not know which; built once per render/export call as `|sink|
/// image.render(...)`.
pub(super) type Renderer<'a> = dyn FnMut(&mut RenderSink<'_>) -> Result<(), ProjectError> + 'a;

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

    /// The render of a saved checkpoint. Resolves `id` against `project`, refusing an unknown
    /// id ([`ProjectError::UnknownCheckpoint`]). A fused
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

    /// The render of a held result: its composite TIFF, with no project and
    /// no source frame.
    pub fn prepare_held(held: &HeldCheckpoint) -> Result<Self, ProjectError> {
        let stack = TiffStack::open_files(std::slice::from_ref(&held.composite))
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
            body: Body::Held(held.composite.clone()),
        })
    }

    /// The live export render, over a Z-Map file the caller already has
    /// (a held row's staged map, or an edit session's Z-Map written to a
    /// temp file) rather than a checkpoint history entry. Refuses a missing
    /// or unreadable source the way [`open_frame_set`] does; there is no
    /// stored fingerprint to check against, since `source.sources` are
    /// plain paths rather than hash-committed [`super::manifest::SourceRef`]s.
    pub fn prepare_zmap(zmap_path: &Path, source: &ZmapSource) -> Result<Self, ProjectError> {
        let zmap =
            ZMapReader::open(zmap_path).map_err(|err| ProjectError::container(zmap_path, err))?;
        let (width, height) = zmap.dimensions();
        check_zmap_sources(&source.sources, zmap_path)?;
        let set = open_sources(&source.sources, zmap_path)?;
        let samples_per_pixel = set.reader.stack_context().samples_per_pixel;
        Ok(CheckpointImage {
            width,
            height,
            samples_per_pixel,
            description: source.description.clone(),
            body: Body::External {
                zmap_path: zmap_path.to_path_buf(),
                source: source.clone(),
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
    /// a held image reads only the rows `tile` covers. `project` is needed
    /// for a saved image and unused for a held one. Only for an image
    /// [`Self::prepare`] or [`Self::prepare_held`] produced;
    /// [`Self::render_external`] is [`Self::prepare_zmap`]'s counterpart.
    pub fn render(
        &self,
        project: Option<&mut Project>,
        tile: Rect,
        sink: &mut RenderSink<'_>,
    ) -> Result<(), ProjectError> {
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
            Body::OpBearing {
                checkpoint,
                ancestor_checkpoint,
                ancestor,
            } => {
                let project = project.expect("a saved checkpoint renders against its project");
                render_op_bearing(
                    project,
                    checkpoint,
                    ancestor_checkpoint,
                    ancestor,
                    &mut crop_bands(tile, spp, sink),
                )
            }
            Body::Held(composite) => render_held(composite, spp, tile, sink),
            Body::External { .. } => {
                unreachable!(
                    "only prepare_zmap produces an External body, rendered through render_external"
                )
            }
        }
    }

    /// [`Self::render`]'s counterpart for an image [`Self::prepare_zmap`]
    /// produced: no project is read.
    pub fn render_external(&self, sink: &mut RenderSink<'_>) -> Result<(), ProjectError> {
        match &self.body {
            Body::External { zmap_path, source } => render_external_body(zmap_path, source, sink),
            _ => unreachable!("only an External body is rendered through render_external"),
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
    if record.align.is_some() && record.registration.transforms.len() != record.sources.len() {
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

/// Refuses `paths` (a live export's sources, plain paths with no stored
/// fingerprint) when any is missing on disk, one `missing: <path>` line
/// each -- [`check_renderable`]'s counterpart for a Z-Map that has no
/// checkpoint of its own to record a `changed` state against.
fn check_zmap_sources(paths: &[PathBuf], zmap_path: &Path) -> Result<(), ProjectError> {
    let detail: Vec<String> = paths
        .iter()
        .filter(|path| !path.is_file())
        .map(|path| format!("missing: {}", path.display()))
        .collect();
    if detail.is_empty() {
        return Ok(());
    }
    Err(ProjectError::ExportSourcesUnavailable {
        path: zmap_path.to_path_buf(),
        detail: detail.join("\n"),
    })
}

/// Opens `paths` with the reader their headers permit (mmap TIFF, decoded
/// TIFF or RAW). `context_path` names the error when opening fails -- the
/// project's own path for a checkpoint's sources, the Z-Map file's for a
/// caller-supplied one, which has no project of its own.
fn open_sources(paths: &[PathBuf], context_path: &Path) -> Result<FrameSet, ProjectError> {
    open_frame_set(paths, HostBudget::host_derived(&crate::host::snapshot()))
        .map_err(|err| ProjectError::container(context_path, err))
}

/// [`open_sources`] plus the shape check a checkpoint's own recorded fuse
/// state can make (a caller-supplied Z-Map has no such expectation to check
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

fn gather_sources<'a>(
    warping: &'a WarpingReader<'a>,
    frame_count: u32,
    samples_per_pixel: u32,
    registration: &Registration,
) -> Sources<'a> {
    Sources {
        reader: warping,
        frame_count,
        samples_per_pixel,
        origin: registration.origin(),
    }
}

/// A sink handing `sink` only the part of each band that lies inside `tile`.
fn crop_bands<'a, 'b>(
    tile: Rect,
    samples_per_pixel: u32,
    sink: &'a mut RenderSink<'b>,
) -> impl FnMut(&[u16], Rect) -> Result<(), ProjectError> + 'a {
    let spp = samples_per_pixel as usize;
    let mut scratch: Vec<u16> = Vec::new();
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
fn render_held(
    path: &Path,
    samples_per_pixel: u32,
    tile: Rect,
    sink: &mut RenderSink<'_>,
) -> Result<(), ProjectError> {
    let stack = TiffStack::open_files(&[path.to_path_buf()])
        .map_err(|err| ProjectError::container(path, err))?;
    let row_samples = tile.width as usize * samples_per_pixel as usize;
    let rows = (READ_BAND_BYTES / (row_samples * 2)).clamp(1, tile.height as usize) as u32;
    let mut band = vec![0u16; row_samples * rows as usize];
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
        stack
            .read_tile_into(0, rect, samples)
            .map_err(|err| ProjectError::container(path, err))?;
        sink(samples, rect)?;
        y += rect.height;
    }
    Ok(())
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

/// `zmap_path`'s Z-Map, gathered band by band over `source`'s sources.
fn render_external_body(
    zmap_path: &Path,
    source: &ZmapSource,
    sink: &mut RenderSink<'_>,
) -> Result<(), ProjectError> {
    let zmap =
        ZMapReader::open(zmap_path).map_err(|err| ProjectError::container(zmap_path, err))?;
    let set = open_sources(&source.sources, zmap_path)?;
    let samples_per_pixel = set.reader.stack_context().samples_per_pixel;
    let gained = GainReader::new(&*set.reader, source.registration.gains());
    let warping = warp(
        &set,
        &gained,
        &source.registration,
        source.sources.len(),
        samples_per_pixel,
    );
    let sources = gather_sources(
        &warping,
        source.sources.len() as u32,
        samples_per_pixel,
        &source.registration,
    );
    recomposite_bands(&zmap, &sources, GATHER_ROWS, |rect, _, samples| {
        sink(samples, rect).map_err(GatherError::Sink)
    })
    .map_err(|err| match err {
        GatherError::Retouch(err) => ProjectError::container(zmap_path, err),
        GatherError::Sink(err) => err,
    })
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

        let paths: Vec<PathBuf> = ancestor
            .sources
            .iter()
            .map(|s| project.source_path(s))
            .collect();
        let set = open_checkpoint_sources(project, checkpoint, &paths, ancestor)?;
        let gained = GainReader::new(&*set.reader, ancestor.registration.gains());
        let warping = warp(
            &set,
            &gained,
            &ancestor.registration,
            ancestor.sources.len(),
            ancestor.composite.samples_per_pixel,
        );
        let sources = gather_sources(
            &warping,
            ancestor.sources.len() as u32,
            ancestor.composite.samples_per_pixel,
            &ancestor.registration,
        );

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

/// A saved checkpoint's image as a TIFF: [`CheckpointImage::prepare`] then
/// [`CheckpointImage::render`] into a `.partial` sibling of `path`, renamed
/// onto it only once every band has been written; removed instead on any
/// error, so a refused or interrupted render leaves nothing behind.
pub fn render_checkpoint(project: &mut Project, id: &str, path: &Path) -> Result<(), ProjectError> {
    let image = CheckpointImage::prepare(project, id)?;
    write_tiff(
        &image,
        &mut |sink| image.render(Some(&mut *project), image.full(), sink),
        path,
    )
}

/// The default 16-bit uncompressed TIFF encoder: the view render's only
/// encoder, and every export format's own `Tiff{Sixteen, None}` case.
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
/// every export format's encoder. Any failure -- creating or finishing the
/// `.partial` file, or the final rename -- comes back as
/// [`ProjectError::ExportWriteFailed`], so a caller can tell "the
/// destination could not be written" apart from every other render failure
/// without inspecting what `encode` did internally.
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
    outcome.map_err(|err| ProjectError::ExportWriteFailed {
        path: path.to_path_buf(),
        source: Box::new(err),
    })
}
