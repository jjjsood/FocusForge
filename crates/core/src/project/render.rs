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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::align::{Registration, WarpingReader};
use crate::equalize::GainReader;
use crate::io::raw::{HighlightClip, document_clip, narrow_document_band};
use crate::io::{FrameSet, TiffSpec, TiffStack, TiffWriter, open_frame_set};
use crate::retouch::blend::{self, Blend};
use crate::tile::{HostBudget, Rect, Sample, SampleFormat};
use crate::zmap::ZMapInfo;

use super::checkpoint::{Checkpoint, FuseRecord, Op, StrokeSource};
use super::container::partial_path;
use super::error::ProjectError;
use super::manifest::PlaneRef;
use super::pixel_grid::PixelGrid;
use super::seal::{
    HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, ParentRef, held_index, stroke_chain,
};
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
    /// Ceiling on the memory the source frame set may take when a stroke
    /// opens it; `None` takes the host-derived budget.
    memory_cap: Option<u64>,
    /// Most bytes of checkpoint-sourced stroke samples a render holds at
    /// once; see [`Self::with_source_bytes`].
    source_bytes: u64,
    /// How a RAW document's `f32` samples narrow to 16 bits; `None` clamps
    /// each sample.
    clip: Option<HighlightClip>,
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
        /// The images the checkpoint-sourced `strokes` blend from.
        checkpoints: Vec<CheckpointImage>,
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
        /// The images the checkpoint-sourced `strokes` blend from.
        checkpoints: Vec<CheckpointImage>,
    },
}

/// A brush stroke with where it blends from and the part of the image it can
/// change. The source is a frame, or a slot in the body's `checkpoints`
/// ([`blend::PlacedSource::Rendered`]).
struct PlacedStroke {
    op: Op,
    source: blend::PlacedSource,
    footprint: Rect,
}

/// The default for [`CheckpointImage::with_source_bytes`]: a tile whose
/// checkpoint-sourced strokes need more is rendered as row halves, and each
/// half streams the referenced plane again. On a synthetic 8192x5464x3 u16
/// project (release) one 200 px stroke renders in 1.51 s from a frame and
/// 1.65 s from the fused base.
const CHECKPOINT_SOURCE_BYTES: u64 = 64 << 20;

/// What a render may use: the frame-set memory ceiling and the byte budget of
/// checkpoint-sourced samples held at once.
#[derive(Clone, Copy)]
struct Limits {
    memory_cap: Option<u64>,
    source_bytes: u64,
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
    /// The unsaved checkpoints a stroke may paint from, as
    /// `StrokeSource::Checkpoint(held_key(i))` for entry `i`.
    pub held: Vec<InMemoryCheckpoint>,
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
    fn shaped(
        composite: &PlaneRef,
        sample_format: SampleFormat,
        clip: Option<HighlightClip>,
        body: Body,
    ) -> Self {
        CheckpointImage {
            width: composite.width,
            height: composite.height,
            samples_per_pixel: composite.samples_per_pixel,
            description: composite.description.clone(),
            sample_format,
            body,
            memory_cap: None,
            source_bytes: CHECKPOINT_SOURCE_BYTES,
            clip,
        }
    }

    /// The render of a saved checkpoint. Resolves `id` against `project`,
    /// refusing an unknown id ([`ProjectError::UnknownCheckpoint`]). A fused
    /// checkpoint needs no source frames; an op-bearing one is also refused
    /// for unrecorded alignment ([`ProjectError::RenderAlignmentUnrecorded`]),
    /// an op that is no valid brush stroke ([`ProjectError::Op`]) and a stroke
    /// painted from a checkpoint that cannot be rendered under it
    /// ([`ProjectError::CheckpointSourceRefused`]: another pixel grid or
    /// sample type), and its render for sources missing or changed on disk
    /// ([`ProjectError::RenderSourcesUnavailable`]) when a stroke meets the
    /// tile.
    pub fn prepare(project: &Project, id: &str) -> Result<Self, ProjectError> {
        Self::prepare_with(project, id, Vec::new(), Held::NONE)
    }

    /// [`Self::prepare`] of checkpoint `id` with `extra` ops applied after its
    /// own, `held` resolving the unsaved entries they paint from.
    fn prepare_with(
        project: &Project,
        id: &str,
        extra: Vec<Op>,
        held: Held<'_>,
    ) -> Result<Self, ProjectError> {
        let checkpoint = project.history().get(id)?.clone();
        let record = project.fuse_state(id)?;

        let composite = record.composite.clone();
        let format = project.plane_format(&composite)?;
        let first_source = record.sources.first().map(|s| project.source_path(s));
        let clip = document_clip(
            first_source.as_deref(),
            record.registration.gains(),
            format,
            composite.samples_per_pixel,
        );
        let mut ops = project.history().op_chain(id)?;
        ops.extend(&extra);
        if ops.is_empty() {
            return Ok(Self::shaped(
                &composite,
                format,
                clip,
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
        let edited = project.pixel_grid(&record);
        let mut references = References::default();
        let strokes = place_strokes(ops.into_iter(), info, &mut |index, reference| {
            references.slot(reference, || {
                if let Some(j) = held_index(reference) {
                    let at = (project.path(), index, reference);
                    return Self::prepare_held_source(
                        held,
                        j,
                        Some(project),
                        at,
                        (&edited, format),
                    );
                }
                if let Some(reason) = unpaintable_reason(project, id, reference)? {
                    return Err(refused(project.path(), index, reference, reason));
                }
                Self::prepare(project, reference)
            })
        })?;
        Ok(Self::shaped(
            &composite,
            format,
            clip,
            Body::Ops {
                checkpoint: Box::new(checkpoint),
                ancestor: Box::new(record),
                strokes,
                checkpoints: references.images,
            },
        ))
    }

    /// The render of checkpoint `reference` for the stroke at op `index`,
    /// refused when it holds other samples than `format`.
    fn prepare_reference(
        project: &Project,
        path: &Path,
        index: usize,
        reference: &str,
        format: SampleFormat,
    ) -> Result<Self, ProjectError> {
        let image = Self::prepare(project, reference)?;
        if image.sample_format != format {
            return Err(refused(
                path,
                index,
                reference,
                sample_mismatch(image.sample_format, format),
            ));
        }
        Ok(image)
    }

    /// The render of unsaved entry `j` for the stroke at op `index` of the
    /// render of `path` (`at`), refused when `j` is not an earlier entry of
    /// `held`, is on another pixel grid than `edited` or holds other samples
    /// than `format`.
    fn prepare_held_source(
        held: Held<'_>,
        j: usize,
        project: Option<&Project>,
        (path, index, reference): (&Path, usize, &str),
        (edited, format): (&PixelGrid<'_>, SampleFormat),
    ) -> Result<Self, ProjectError> {
        let refuse = |reason: &str| refused(path, index, reference, reason);
        if j >= held.entries.len() {
            return Err(refuse("is not in this project"));
        }
        if j >= held.painting {
            return Err(refuse("is not before the checkpoint painting from it"));
        }
        let mismatch = match base_of(held.entries, j) {
            Some(Base::Fuse(fuse)) => edited.mismatch(&PixelGrid::held(fuse)),
            Some(Base::Saved(id)) => {
                let project =
                    project.ok_or_else(|| refuse("needs its project, and none is open"))?;
                let fuse = project.fuse_state(id)?;
                edited.mismatch(&project.pixel_grid(&fuse))
            }
            None => None,
        };
        if let Some(mismatch) = mismatch {
            return Err(refuse(&format!("has {mismatch}")));
        }
        let image = Self::prepare_entry(held.entries, j, project, path)?;
        if image.sample_format != format {
            return Err(refuse(&sample_mismatch(image.sample_format, format)));
        }
        Ok(image)
    }

    /// The render of entry `index` of the unsaved list `entries`: a held
    /// fuse's composite, or the ops of the entry and of the op-bearing
    /// entries it descends from over the fuse they are recorded on. Needs
    /// `project` when that fuse is a saved checkpoint. Refuses an `index`
    /// past the list, and a saved fuse without a `project`, as
    /// [`ProjectError::Container`].
    pub fn prepare_unsaved(
        entries: &[InMemoryCheckpoint],
        index: usize,
        project: Option<&Project>,
    ) -> Result<Self, ProjectError> {
        let path = project.map_or_else(|| Path::new(""), |project| project.path());
        if index >= entries.len() {
            return Err(ProjectError::container(
                path,
                format!(
                    "checkpoint {index} is not in the list of {} unsaved checkpoints",
                    entries.len()
                ),
            ));
        }
        Self::prepare_entry(entries, index, project, path)
    }

    /// The render of unsaved entry `j`: a held fuse's composite, or its own
    /// ops and those of the op-bearing entries it descends from over the
    /// fuse they are recorded on, never the strokes of the session. Strokes
    /// of those ops paint from the entries before `j`.
    fn prepare_entry(
        entries: &[InMemoryCheckpoint],
        j: usize,
        project: Option<&Project>,
        path: &Path,
    ) -> Result<Self, ProjectError> {
        let no_fuse =
            || ProjectError::container(path, format!("checkpoint {j} has no fuse to render over"));
        let Some(chain) = stroke_chain(entries, j) else {
            let InMemoryKind::Fused(fuse) = &entries[j].kind else {
                return Err(no_fuse());
            };
            return Self::prepare_held(fuse);
        };
        let held = Held {
            entries,
            painting: j,
        };
        match base_of(entries, j) {
            Some(Base::Fuse(fuse)) => {
                let state = PendingState {
                    composite: fuse.composite.clone(),
                    ops: chain,
                    sources: fuse.sources.clone(),
                    registration: fuse.registration.clone(),
                    reference: 0,
                    description: fuse.composite_description.clone(),
                    held: Vec::new(),
                };
                Self::prepare_pending_in(&state, project, held)
            }
            Some(Base::Saved(id)) => {
                let project = project.ok_or_else(|| {
                    ProjectError::container(
                        path,
                        format!("checkpoint {j} is recorded on a saved checkpoint, and no project is open"),
                    )
                })?;
                Self::prepare_with(project, id, chain, held)
            }
            None => Err(no_fuse()),
        }
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
        let samples_per_pixel = stack.header(0).samples_per_pixel;
        let clip = document_clip(
            held.sources.first().map(PathBuf::as_path),
            held.registration.gains(),
            stack.sample_format(),
            samples_per_pixel,
        );
        Ok(CheckpointImage {
            width: held.width,
            height: held.height,
            samples_per_pixel,
            description: held.composite_description.clone(),
            sample_format: stack.sample_format(),
            body: Body::Held(held.composite.clone()),
            memory_cap: None,
            source_bytes: CHECKPOINT_SOURCE_BYTES,
            clip,
        })
    }

    /// The render of a [`PendingState`]: its composite with every stroke
    /// blended over it. Refuses an op that is no valid brush stroke
    /// ([`ProjectError::Op`]), a missing source
    /// ([`ProjectError::ExportSourcesUnavailable`]) and a composite or source
    /// that cannot be read. A source has no stored fingerprint to check
    /// against, so a changed file is not detected. A stroke painted from a
    /// checkpoint needs `project` to read it from; it is refused with
    /// [`ProjectError::CheckpointSourceRefused`] when there is none, when the
    /// project has no such checkpoint, when that checkpoint is on another
    /// pixel grid or holds other samples than the composite.
    pub fn prepare_pending(
        state: &PendingState,
        project: Option<&Project>,
    ) -> Result<Self, ProjectError> {
        let held = Held {
            entries: &state.held,
            painting: state.held.len(),
        };
        Self::prepare_pending_in(state, project, held)
    }

    /// [`Self::prepare_pending`] with `held` resolving the unsaved entries
    /// its strokes paint from.
    fn prepare_pending_in(
        state: &PendingState,
        project: Option<&Project>,
        held: Held<'_>,
    ) -> Result<Self, ProjectError> {
        let composite = &state.composite;
        let stack = TiffStack::open_staged(composite)
            .map_err(|err| ProjectError::container(composite, err))?;
        let (width, height) = stack.dimensions();
        let samples_per_pixel = stack.header(0).samples_per_pixel;
        check_pending_sources(&state.sources, composite)?;
        open_pending_sources(&state.sources, composite, samples_per_pixel, None)?;
        let info = ZMapInfo::new(width, height, state.sources.len() as u32)
            .map_err(|err| ProjectError::container(composite, err))?;
        let format = stack.sample_format();
        let clip = document_clip(
            state.sources.first().map(PathBuf::as_path),
            state.registration.gains(),
            format,
            samples_per_pixel,
        );
        let grid = PixelGrid {
            hashes: None,
            paths: state.sources.clone(),
            registration: &state.registration,
            width,
            height,
        };
        let mut references = References::default();
        let strokes = place_strokes(state.ops.iter(), info, &mut |index, reference| {
            if let Some(j) = held_index(reference) {
                return references.slot(reference, || {
                    let at = (composite.as_path(), index, reference);
                    Self::prepare_held_source(held, j, project, at, (&grid, format))
                });
            }
            let refuse = |reason: &str| refused(composite, index, reference, reason);
            let project = project.ok_or_else(|| refuse("needs its project, and none is open"))?;
            references.slot(reference, || {
                if project.history().get(reference).is_err() {
                    return Err(refuse("is not in this project"));
                }
                let fuse = project.fuse_state(reference)?;
                if let Some(mismatch) = grid.mismatch(&project.pixel_grid(&fuse)) {
                    return Err(refuse(&format!("has {mismatch}")));
                }
                Self::prepare_reference(project, composite, index, reference, format)
            })
        })?;
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
                checkpoints: references.images,
            },
            memory_cap: None,
            source_bytes: CHECKPOINT_SOURCE_BYTES,
            clip,
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
    /// is rendered in `f32` and each band narrowed to 16 bits: a RAW
    /// document's highlights clip in camera space, so a pixel whose sensor
    /// saturated comes out neutral; any other samples clamp onto the scale.
    pub fn render(
        &self,
        project: Option<&mut Project>,
        tile: Rect,
        sink: &mut RenderSink<'_>,
    ) -> Result<(), ProjectError> {
        match self.sample_format {
            SampleFormat::U16 => self.render_as::<u16>(project, tile, self.limits(), sink),
            SampleFormat::F32 => {
                let mut narrowed: Vec<u16> = Vec::new();
                self.render_as::<f32>(project, tile, self.limits(), &mut |band, rect| {
                    narrow_document_band(self.clip.as_ref(), band, &mut narrowed);
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
        self.render_as::<f32>(project, tile, self.limits(), sink)
    }

    /// Renders with at most `bytes` of checkpoint-sourced stroke samples held
    /// at once (the default is 64 MiB): a tile whose strokes need more is
    /// rendered as row halves, down to single rows. The samples are the same
    /// for any budget.
    pub fn with_source_bytes(mut self, bytes: u64) -> Self {
        self.source_bytes = bytes;
        self
    }

    /// Lowers the memory the source frames of an op-bearing image may take
    /// when a render opens them to `cap` bytes; `None` takes the
    /// host-derived budget. A cap below the frames' decode reservation
    /// refuses the render before any frame is decoded.
    pub fn with_memory_cap(mut self, cap: Option<u64>) -> Self {
        self.memory_cap = cap;
        self
    }

    fn limits(&self) -> Limits {
        Limits {
            memory_cap: self.memory_cap,
            source_bytes: self.source_bytes,
        }
    }

    /// [`Self::render`] in the image's own sample type; `limits` bound every
    /// source frame set the render opens and every checkpoint it blends from.
    fn render_as<S: DocumentSample>(
        &self,
        project: Option<&mut Project>,
        tile: Rect,
        limits: Limits,
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
                checkpoints,
            } => {
                let project = project.expect("a saved checkpoint renders against its project");
                render_ops(
                    project,
                    (checkpoint, ancestor),
                    (strokes, checkpoints),
                    tile,
                    limits,
                    sink,
                )
            }
            Body::Held(composite) => render_held(composite, spp, tile, sink),
            Body::Pending {
                composite,
                sources,
                registration,
                info,
                strokes,
                checkpoints,
            } => render_pending(
                (composite, sources, registration),
                (*info, spp),
                (strokes, checkpoints),
                project,
                limits,
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
fn open_sources(
    paths: &[PathBuf],
    context_path: &Path,
    memory_cap: Option<u64>,
) -> Result<FrameSet, ProjectError> {
    let budget = HostBudget::host_derived(&crate::host::snapshot()).capped_to(memory_cap);
    open_frame_set(paths, budget).map_err(|err| ProjectError::container(context_path, err))
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
    memory_cap: Option<u64>,
) -> Result<FrameSet, ProjectError> {
    let set = open_sources(paths, project.path(), memory_cap)?;
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
/// samples per pixel differ from `composite`'s `samples_per_pixel`, under
/// `memory_cap` as [`open_sources`] takes it.
fn open_pending_sources(
    paths: &[PathBuf],
    composite: &Path,
    samples_per_pixel: u32,
    memory_cap: Option<u64>,
) -> Result<FrameSet, ProjectError> {
    let set = open_sources(paths, composite, memory_cap)?;
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

/// The unsaved entries a render resolves a `held_key` source against, and the
/// index of the entry whose strokes are being resolved: a stroke may only
/// paint from an entry before it.
#[derive(Clone, Copy)]
struct Held<'a> {
    entries: &'a [InMemoryCheckpoint],
    painting: usize,
}

impl Held<'static> {
    const NONE: Held<'static> = Held {
        entries: &[],
        painting: 0,
    };
}

/// What an unsaved op-bearing entry is recorded over.
enum Base<'a> {
    Fuse(&'a HeldCheckpoint),
    Saved(&'a str),
}

/// What entry `at` of `entries` renders over: its own fuse, or the one of the
/// nearest fused or saved ancestor. `None` for a chain that ends in no fuse.
fn base_of(entries: &[InMemoryCheckpoint], mut at: usize) -> Option<Base<'_>> {
    loop {
        let entry = entries.get(at)?;
        match (&entry.kind, &entry.parent) {
            (InMemoryKind::Fused(fuse), _) => return Some(Base::Fuse(fuse)),
            (_, Some(ParentRef::Saved(id))) => return Some(Base::Saved(id)),
            (_, Some(ParentRef::Entry(parent))) if *parent < at => at = *parent,
            _ => return None,
        }
    }
}

/// The checkpoint images a render blends from, one per distinct id, in the
/// order their ids were first met.
#[derive(Default)]
struct References {
    ids: Vec<String>,
    images: Vec<CheckpointImage>,
}

impl References {
    /// The slot of `id`'s image, prepared by `prepare` the first time `id` is
    /// met.
    fn slot(
        &mut self,
        id: &str,
        prepare: impl FnOnce() -> Result<CheckpointImage, ProjectError>,
    ) -> Result<usize, ProjectError> {
        if let Some(slot) = self.ids.iter().position(|known| known == id) {
            return Ok(slot);
        }
        self.images.push(prepare()?);
        self.ids.push(id.to_string());
        Ok(self.images.len() - 1)
    }
}

/// Why a checkpoint holding `held` samples cannot be painted onto an image of
/// `wanted` samples.
fn sample_mismatch(held: SampleFormat, wanted: SampleFormat) -> String {
    let name = |f: SampleFormat| {
        if f == SampleFormat::U16 {
            "16-bit"
        } else {
            "f32"
        }
    };
    format!("holds {} samples, not {}", name(held), name(wanted))
}

/// Why checkpoint `reference` cannot be painted from while `edited` is being
/// edited, `None` when it can: another pixel grid, or other samples than
/// `edited`'s composite.
pub(super) fn unpaintable_reason(
    project: &Project,
    edited: &str,
    reference: &str,
) -> Result<Option<String>, ProjectError> {
    if let Some(mismatch) = project.grid_mismatch(edited, reference)? {
        return Ok(Some(format!("has {mismatch}")));
    }
    let format_of = |id: &str| project.plane_format(&project.fuse_state(id)?.composite);
    let (held, wanted) = (format_of(reference)?, format_of(edited)?);
    Ok((held != wanted).then(|| sample_mismatch(held, wanted)))
}

fn refused(path: &Path, index: usize, checkpoint: &str, reason: impl Into<String>) -> ProjectError {
    ProjectError::CheckpointSourceRefused {
        path: path.to_path_buf(),
        index,
        checkpoint: checkpoint.to_string(),
        reason: reason.into(),
    }
}

/// Validates each of `ops` over `info`'s map and places those that change
/// something, in log order. `resolve` maps (op index, checkpoint id) of a
/// checkpoint-sourced stroke to its image's slot.
fn place_strokes<'a>(
    ops: impl Iterator<Item = &'a Op>,
    info: ZMapInfo,
    resolve: &mut dyn FnMut(usize, &str) -> Result<usize, ProjectError>,
) -> Result<Vec<PlacedStroke>, ProjectError> {
    let mut strokes = Vec::new();
    for (index, op) in ops.enumerate() {
        let footprint = blend::footprint(op, info).map_err(|err| ProjectError::Op {
            index,
            source: Box::new(err),
        })?;
        let Some(footprint) = footprint else {
            continue;
        };
        let Op::BrushStroke { source, .. } = op;
        let source = match source {
            StrokeSource::Frame(frame) => blend::PlacedSource::Frame(usize::from(*frame)),
            StrokeSource::Checkpoint(id) => {
                blend::PlacedSource::Rendered(resolve(index, id.as_str())?)
            }
        };
        strokes.push(PlacedStroke {
            op: op.clone(),
            source,
            footprint,
        });
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

/// What a render of one tile reads: its extent, the strokes meeting it and
/// the part of each referenced checkpoint its checkpoint-sourced strokes
/// blend.
struct Plan<'a> {
    extent: Rect,
    touching: Vec<blend::PlacedStroke<'a>>,
    /// (checkpoint slot, area to render), where entry `k` feeds the stroke
    /// placed as [`blend::PlacedSource::Rendered`]`(k)`.
    areas: Vec<(usize, Rect)>,
}

impl Plan<'_> {
    /// Bytes of the checkpoint-sourced samples, in `S`, of `spp` samples a pixel.
    fn source_bytes<S: DocumentSample>(&self, spp: u32) -> u64 {
        self.areas
            .iter()
            .map(|(_, a)| {
                u64::from(a.width) * u64::from(a.height) * u64::from(spp) * S::FORMAT.bytes()
            })
            .sum()
    }
}

/// The extent a render of `tile` reads, and the strokes meeting it, ready for
/// [`blend_bands`]. No stroke meeting it means `tile` itself and an empty list.
fn plan<'a>(info: ZMapInfo, strokes: &'a [PlacedStroke], tile: Rect) -> Plan<'a> {
    let placed: Vec<blend::PlacedStroke> = strokes
        .iter()
        .map(|stroke| blend::PlacedStroke {
            op: &stroke.op,
            source: stroke.source,
            footprint: stroke.footprint,
        })
        .collect();
    let extent = blend::read_extent(info, &placed, tile);
    let mut areas = Vec::new();
    let touching = placed
        .into_iter()
        .filter(|stroke| stroke.footprint.intersect(extent).is_some())
        .map(|mut stroke| {
            if let blend::PlacedSource::Rendered(slot) = stroke.source {
                let margin = blend::read_margin(stroke.op, info).expect("a placed stroke is valid");
                let area = blend::grow(stroke.footprint, margin, info)
                    .intersect(extent)
                    .expect("the footprint meets the extent");
                stroke.source = blend::PlacedSource::Rendered(areas.len());
                areas.push((slot, area));
            }
            stroke
        })
        .collect();
    Plan {
        extent,
        touching,
        areas,
    }
}

/// `tile` as row chunks, top to bottom, each costing at most `budget` by
/// `cost`: halved until it fits or is one row.
fn fit_rows(tile: Rect, budget: u64, cost: &dyn Fn(Rect) -> u64) -> Vec<Rect> {
    if tile.height <= 1 || cost(tile) <= budget {
        return vec![tile];
    }
    let top = Rect {
        height: tile.height / 2,
        ..tile
    };
    let bottom = Rect {
        y: tile.y + top.height,
        height: tile.height - top.height,
        ..tile
    };
    let mut chunks = fit_rows(top, budget, cost);
    chunks.extend(fit_rows(bottom, budget, cost));
    chunks
}

/// Renders each of `areas` from its referenced checkpoint, in `S`.
fn render_areas<S: DocumentSample>(
    mut project: Option<&mut Project>,
    checkpoints: &[CheckpointImage],
    areas: &[(usize, Rect)],
    spp: u32,
    limits: Limits,
) -> Result<Vec<(Rect, Vec<S>)>, ProjectError> {
    let spp = spp as usize;
    areas
        .iter()
        .map(|&(slot, area)| {
            let mut samples = vec![S::default(); area.width as usize * area.height as usize * spp];
            checkpoints[slot].render_as::<S>(
                project.as_deref_mut(),
                area,
                limits,
                &mut |band, rect| {
                    let len = rect.width as usize * spp;
                    for (r, line) in band.chunks_exact(len).enumerate() {
                        let at = ((rect.y - area.y) as usize + r) * area.width as usize
                            + (rect.x - area.x) as usize;
                        samples[at * spp..at * spp + len].copy_from_slice(line);
                    }
                    Ok(())
                },
            )?;
            Ok((area, samples))
        })
        .collect()
}

/// A sink for the rows of `extent` that blends `strokes` over `tile` in log
/// order, frames read through `warping` at `origin` and checkpoint areas taken
/// from `rendered`, and hands `sink` the
/// tile's bands. A band waits for the rows its [`blend::read_extent`] needs;
/// rows no later band needs are dropped. `context` names a failed frame read.
#[allow(clippy::too_many_arguments)]
fn blend_bands<'a, 'b, S: DocumentSample>(
    strokes: &'a [blend::PlacedStroke<'a>],
    rendered: &'a [(Rect, Vec<S>)],
    info: ZMapInfo,
    warping: &'a WarpingReader<'a>,
    origin: (u32, u32),
    samples_per_pixel: usize,
    context: &'a Path,
    (extent, tile): (Rect, Rect),
    sink: &'a mut RenderSink<'b, S>,
) -> impl FnMut(&[S], Rect) -> Result<(), ProjectError> + 'a {
    let spp = samples_per_pixel;
    let row = extent.width as usize * spp;
    let tile_end = tile.y + tile.height;
    let mut window: Vec<S> = Vec::new();
    let mut top = extent.y;
    let mut queued: VecDeque<Rect> = VecDeque::new();
    let mut blended: Vec<S> = Vec::new();
    move |band, rect| {
        window.extend_from_slice(band);
        let arrived = rect.y + rect.height;
        if let Some(part) = tile.intersect(rect) {
            queued.push_back(part);
        }
        while let Some(&out) = queued.front() {
            let need = blend::read_extent(info, strokes, out);
            if need.y + need.height > arrived {
                break;
            }
            queued.pop_front();
            blended.clear();
            for y in need.y..need.y + need.height {
                let start = (y - top) as usize * row + (need.x - extent.x) as usize * spp;
                blended.extend_from_slice(&window[start..start + need.width as usize * spp]);
            }
            blend::apply_strokes(
                info,
                need,
                &mut blended,
                strokes,
                warping,
                origin,
                spp,
                rendered,
                blend::apply::<S>,
            )
            .map_err(|err| ProjectError::container(context, err))?;
            crop_bands(out, samples_per_pixel as u32, &mut *sink)(&blended, need)?;
        }
        let next = queued.front().map_or(arrived.max(tile.y), |out| out.y);
        let keep = if next < tile_end {
            let rest = Rect {
                height: tile_end - next,
                y: next,
                ..tile
            };
            blend::read_extent(info, strokes, rest).y
        } else {
            arrived
        };
        let keep = keep.clamp(top, arrived);
        window.drain(..(keep - top) as usize * row);
        top = keep;
        Ok(())
    }
}

/// `tile` of the TIFF at `composite` (`info` its map's shape, `spp` its samples
/// per pixel), each of `strokes` the tile meets blended in over it in log
/// order from its frame, read through the gains and alignment of
/// `registration`, or from its referenced image in `checkpoints`, read from
/// `project`.
#[allow(clippy::too_many_arguments)]
fn render_pending<S: DocumentSample>(
    (composite, sources, registration): (&Path, &[PathBuf], &Registration),
    (info, spp): (ZMapInfo, u32),
    (strokes, checkpoints): (&[PlacedStroke], &[CheckpointImage]),
    mut project: Option<&mut Project>,
    limits: Limits,
    tile: Rect,
    sink: &mut RenderSink<'_, S>,
) -> Result<(), ProjectError> {
    let cost = |r: Rect| plan(info, strokes, r).source_bytes::<S>(spp);
    for chunk in fit_rows(tile, limits.source_bytes, &cost) {
        let Plan {
            extent,
            touching,
            areas,
        } = plan(info, strokes, chunk);
        if touching.is_empty() {
            render_held(composite, spp, chunk, sink)?;
            continue;
        }
        let rendered = render_areas::<S>(project.as_deref_mut(), checkpoints, &areas, spp, limits)?;
        let set = open_pending_sources(sources, composite, spp, limits.memory_cap)?;
        let gained = GainReader::new(&*set.reader, registration.gains());
        let warping = warp(&set, &gained, registration, sources.len(), spp);
        let mut blend = blend_bands(
            &touching,
            &rendered,
            info,
            &warping,
            registration.origin(),
            spp as usize,
            composite,
            (extent, chunk),
            &mut *sink,
        );
        render_held(composite, spp, extent, &mut blend)?;
    }
    Ok(())
}

/// `ancestor`'s stored composite over `tile`, each stroke the tile meets
/// blended in over it in log order from its frame, read through the gains and
/// alignment the fusion used, or from its referenced image in `checkpoints`.
/// Every area of a referenced checkpoint is rendered, under its own frame set,
/// before this render opens its set, so one set is open at a time.
fn render_ops<S: DocumentSample>(
    project: &mut Project,
    (checkpoint, ancestor): (&Checkpoint, &FuseRecord),
    (strokes, checkpoints): (&[PlacedStroke], &[CheckpointImage]),
    tile: Rect,
    limits: Limits,
    sink: &mut RenderSink<'_, S>,
) -> Result<(), ProjectError> {
    let spp = ancestor.composite.samples_per_pixel;
    let info = ZMapInfo::new(
        ancestor.composite.width,
        ancestor.composite.height,
        ancestor.sources.len() as u32,
    )
    .expect("prepare built the same extent");
    let cost = |r: Rect| plan(info, strokes, r).source_bytes::<S>(spp);
    for chunk in fit_rows(tile, limits.source_bytes, &cost) {
        let Plan {
            extent,
            touching,
            areas,
        } = plan(info, strokes, chunk);
        if touching.is_empty() {
            stream_plane_to_sink(
                project,
                &ancestor.composite,
                &mut crop_bands(chunk, spp, &mut *sink),
            )?;
            continue;
        }
        let rendered = render_areas::<S>(Some(&mut *project), checkpoints, &areas, spp, limits)?;

        check_sources_available(project, checkpoint, ancestor)?;
        let paths: Vec<PathBuf> = ancestor
            .sources
            .iter()
            .map(|s| project.source_path(s))
            .collect();
        let set =
            open_checkpoint_sources(project, checkpoint, &paths, ancestor, limits.memory_cap)?;
        let gained = GainReader::new(&*set.reader, ancestor.registration.gains());
        let warping = warp(&set, &gained, &ancestor.registration, paths.len(), spp);
        let project_path = project.path().to_path_buf();
        let mut blend = blend_bands(
            &touching,
            &rendered,
            info,
            &warping,
            ancestor.registration.origin(),
            spp as usize,
            &project_path,
            (extent, chunk),
            &mut *sink,
        );
        stream_plane_to_sink(
            project,
            &ancestor.composite,
            &mut crop_bands(extent, spp, &mut blend),
        )?;
    }
    Ok(())
}

/// A saved checkpoint's image as a TIFF holding the image's own samples: a
/// 16-bit file for a `u16` image, a 32-bit float file for an `f32` one, values
/// above white included. [`CheckpointImage::prepare`] then the render, into a
/// `.partial` sibling of `path`, renamed onto it only once every band has
/// been written; removed instead on any error, so a refused or interrupted
/// render leaves nothing behind. `memory_cap` lowers the budget of the source
/// frame set an op-bearing checkpoint opens; a cap below the frames' decode
/// reservation refuses the render.
pub fn render_checkpoint(
    project: &mut Project,
    id: &str,
    path: &Path,
    memory_cap: Option<u64>,
) -> Result<(), ProjectError> {
    let image = CheckpointImage::prepare(project, id)?.with_memory_cap(memory_cap);
    match image.sample_format {
        SampleFormat::U16 => write_tiff(
            &image,
            &mut |sink| image.render(Some(&mut *project), image.full(), sink),
            path,
        ),
        SampleFormat::F32 => write_tiff_f32(&image, Some(project), path),
    }
}

/// [`render_checkpoint`] for entry `index` of the unsaved list `entries`
/// ([`CheckpointImage::prepare_unsaved`]). `project` is needed when the entry
/// hangs from a saved checkpoint.
pub fn render_unsaved_checkpoint(
    entries: &[InMemoryCheckpoint],
    index: usize,
    mut project: Option<&mut Project>,
    path: &Path,
    memory_cap: Option<u64>,
) -> Result<(), ProjectError> {
    let image = CheckpointImage::prepare_unsaved(entries, index, project.as_deref())?
        .with_memory_cap(memory_cap);
    match image.sample_format {
        SampleFormat::U16 => write_tiff(
            &image,
            &mut |sink| image.render(project.as_deref_mut(), image.full(), sink),
            path,
        ),
        SampleFormat::F32 => write_tiff_f32(&image, project, path),
    }
}

/// [`write_tiff`] for an `f32` image: its unclipped render as a 32-bit float
/// TIFF.
fn write_tiff_f32(
    image: &CheckpointImage,
    project: Option<&mut Project>,
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
        image.render_f32(project, image.full(), &mut |band, rect| {
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

#[cfg(test)]
mod tests {
    use super::super::checkpoint::{BrushMode, Point};
    use super::*;

    fn tile(y: u32, height: u32) -> Rect {
        Rect {
            x: 5,
            y,
            width: 13,
            height,
        }
    }

    /// Four bytes a pixel.
    fn cost(r: Rect) -> u64 {
        u64::from(r.width) * u64::from(r.height) * 4
    }

    fn assert_covers(chunks: &[Rect], whole: Rect) {
        let mut y = whole.y;
        for chunk in chunks {
            assert_eq!((chunk.x, chunk.width, chunk.y), (whole.x, whole.width, y));
            assert!(chunk.height > 0);
            y += chunk.height;
        }
        assert_eq!(y, whole.y + whole.height, "the chunks reach the tile's end");
    }

    #[test]
    fn source_budget_halves_tile_rows() {
        let whole = tile(3, 47);
        let row = cost(tile(0, 1));

        assert_eq!(fit_rows(whole, cost(whole), &cost), vec![whole]);

        let chunks = fit_rows(whole, row * 10, &cost);
        assert_covers(&chunks, whole);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| cost(*c) <= row * 10), "{chunks:?}");
        assert!(
            chunks.iter().any(|c| c.height > 5),
            "a tile is halved only as far as it must be: {chunks:?}"
        );

        let chunks = fit_rows(whole, row - 1, &cost);
        assert_covers(&chunks, whole);
        assert_eq!(chunks.len(), 47, "below one row, one row each");

        assert_eq!(fit_rows(tile(9, 1), 0, &cost), vec![tile(9, 1)]);
    }

    const SIZE: (u32, u32) = (23, 17);
    const SPP: usize = 3;

    fn whole() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: SIZE.0,
            height: SIZE.1,
        }
    }

    fn write_image(dir: &Path, name: &str, wide: bool, seed: usize) -> PathBuf {
        let path = dir.join(name);
        let spec = TiffSpec {
            width: SIZE.0,
            height: SIZE.1,
            samples_per_pixel: SPP as u32,
            description: Some("held composite".to_string()),
        };
        let len = SIZE.0 as usize * SIZE.1 as usize * SPP;
        if wide {
            let samples: Vec<f32> = (0..len)
                .map(|i| {
                    (i * 37 * seed % 1000) as f32 + 0.25 + if i % 11 == 0 { 5000.0 } else { 0.0 }
                })
                .collect();
            let mut writer = TiffWriter::create_f32(&path, spec).unwrap();
            writer.write_tile_f32(whole(), &samples).unwrap();
            writer.finish().unwrap();
        } else {
            let samples: Vec<u16> = (0..len).map(|i| (i * 977 * seed % 65_536) as u16).collect();
            let mut writer = TiffWriter::create(&path, spec).unwrap();
            writer.write_tile(whole(), &samples).unwrap();
            writer.finish().unwrap();
        }
        path
    }

    /// A held fuse over one source frame, then a retouch held on it.
    fn held_list(dir: &Path, wide: bool) -> Vec<InMemoryCheckpoint> {
        let fuse = HeldCheckpoint {
            composite: write_image(dir, "composite.tif", wide, 1),
            zmap: dir.join("zmap.tif"),
            width: SIZE.0,
            height: SIZE.1,
            sources: vec![write_image(dir, "frame.tif", false, 5)],
            algorithm: "laplacian".to_string(),
            params: Default::default(),
            align: None,
            registration: Registration::default(),
            composite_description: "held composite".to_string(),
            metrics: Default::default(),
            created_at: 900,
            compressed_composite: None,
        };
        let ops = vec![Op::BrushStroke {
            source: StrokeSource::Frame(0),
            path: vec![Point { x: 4, y: 5 }, Point { x: 17, y: 11 }],
            radius: 5.0,
            softness: 0.5,
            mode: BrushMode::Normal,
            opacity: 1.0,
        }];
        vec![
            InMemoryCheckpoint {
                kind: InMemoryKind::Fused(fuse),
                parent: None,
            },
            InMemoryCheckpoint {
                kind: InMemoryKind::Ops {
                    ops,
                    created_at: 1_000,
                },
                parent: Some(ParentRef::Entry(0)),
            },
        ]
    }

    /// The samples `image` streams, in the sample type it holds.
    fn streamed<S: Copy + Default>(
        stream: impl FnOnce(&mut RenderSink<'_, S>) -> Result<(), ProjectError>,
    ) -> Vec<S> {
        let mut out = vec![S::default(); SIZE.0 as usize * SIZE.1 as usize * SPP];
        stream(&mut |band: &[S], rect: Rect| {
            let len = rect.width as usize * SPP;
            for (r, line) in band.chunks_exact(len).enumerate() {
                let at = (rect.y as usize + r) * SIZE.0 as usize + rect.x as usize;
                out[at * SPP..at * SPP + len].copy_from_slice(line);
            }
            Ok(())
        })
        .unwrap();
        out
    }

    fn read_back<S: Sample + Default + Clone>(path: &Path) -> Vec<S> {
        let stack = TiffStack::open_staged(path).unwrap();
        let mut out = vec![S::default(); SIZE.0 as usize * SIZE.1 as usize * SPP];
        S::read_tile(&stack, 0, whole(), &mut out).unwrap();
        out
    }

    #[test]
    fn unsaved_checkpoint_tiff() {
        for wide in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let entries = held_list(dir.path(), wide);
            let mut written: Vec<Vec<u32>> = Vec::new();
            for index in [0, 1] {
                let name = format!("entry {index}, f32 {wide}");
                let path = dir.path().join(format!("out-{index}.tif"));
                render_unsaved_checkpoint(&entries, index, None, &path, None)
                    .unwrap_or_else(|err| panic!("{name}: {err}"));
                assert!(!partial_path(&path).exists(), "{name}: no partial left");

                let image = CheckpointImage::prepare_unsaved(&entries, index, None).unwrap();
                if wide {
                    let want = streamed(|sink| image.render_f32(None, whole(), sink));
                    assert!(read_back::<f32>(&path) == want, "{name}");
                    written.push(
                        read_back::<f32>(&path)
                            .iter()
                            .map(|v| v.to_bits())
                            .collect(),
                    );
                } else {
                    let want = streamed(|sink| image.render(None, whole(), sink));
                    assert!(read_back::<u16>(&path) == want, "{name}");
                    written.push(
                        read_back::<u16>(&path)
                            .iter()
                            .map(|&v| u32::from(v))
                            .collect(),
                    );
                }
            }
            assert!(
                written[0] != written[1],
                "f32 {wide}: the retouch changes the image"
            );

            let absent = dir.path().join("absent.tif");
            assert!(matches!(
                render_unsaved_checkpoint(&entries, 2, None, &absent, None),
                Err(ProjectError::Container { message, .. }) if message.contains("is not in the list")
            ));
            assert!(!absent.exists());
        }
    }
}
