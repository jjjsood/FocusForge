// SPDX-License-Identifier: GPL-3.0-or-later

//! The two algorithm families: a stacking (fusion) trait and an alignment
//! trait, sharing one shape -- `id()`, `display_name()`, `params()` and one
//! work method -- so a rayon tile scheduler can hold either behind a shared
//! reference and a registry can hold a `&'static dyn` of either.
//!
//! An implementor of either trait may not take a whole frame (the context is
//! the only pixel source, and it is tile-shaped, never a whole-frame
//! buffer), may not require a GPU device (`crates/core` links neither `wgpu`
//! nor a device crate; the GPU twin reaches `core` through a trait `core`
//! owns), and may not read a setting that is not declared in
//! its own `params()` (`ParamDesc` is the single source of truth for every
//! setting -- a struct field that is not a descriptor is invisible to both
//! the CLI and the GUI).

mod error;

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::gpu::AlignBackend;
pub use crate::params::StackContext;
use crate::params::{ParamDesc, ParamValue, ParamValues};
use crate::tile::plan::TileSupport;
use crate::tile::{FramePreprocess, Rect, Sample, SampleFormat, Tile};
use crate::timing::{TimingCounter, record_timing};

pub use error::AlgorithmError;

/// The output of one [`StackAlgorithm::fuse`] call: the fused pixels for one
/// tile, plus a Z-Map naming which source frame each output pixel came from.
///
/// Invariant: `zmap.len() == tile.rect.width as usize * tile.rect.height as
/// usize` -- one index per **pixel**, never per sample. `Tile::samples` is
/// `width * height * samples_per_pixel` long, but the Z-Map names a source
/// *frame*, which is a per-pixel fact, not a per-sample one. Fields are
/// public and there is no validating constructor here: the Z-Map as a
/// document with its own storage and invariants lives in [`crate::zmap`]
/// (`ZMapInfo`, `crate::zmap::validate_tile`, `crate::zmap::ZMapWriter`) --
/// a checked constructor on `TileOutput` itself still waits for its first
/// real caller.
#[derive(Debug, Clone, PartialEq)]
pub struct TileOutput<S = u16> {
    pub tile: Tile<S>,
    pub zmap: Vec<u16>,
}

/// The per-tile view across every frame of a stack that
/// [`StackAlgorithm::fuse`] reads from.
///
/// A visitor, not random access: [`Self::for_each_frame`] hands every frame's
/// tile to a callback in ascending frame order. [`Self::new`] borrows
/// frame-tiles that were already read; [`Self::on_demand`] reads each frame
/// from a [`TileReader`] only when the visitor reaches it, into one reused
/// buffer, so a method with running accumulators fuses an N-frame stack in the
/// memory of one frame-tile. [`Tile`] carries no frame index.
pub struct TileCtx<'a> {
    rect: Rect,
    backing: Backing<'a>,
    frames_read: AtomicUsize,
}

enum Backing<'a> {
    Resident {
        tiles: &'a [Tile],
        coverage: &'a [Option<Vec<bool>>],
    },
    OnDemand {
        frame_count: usize,
        samples_per_pixel: u32,
        reader: &'a dyn TileReader,
        preprocess: &'a dyn FramePreprocess,
    },
}

impl<'a> TileCtx<'a> {
    /// Builds a context over `tiles`, one per frame, all covering `rect`.
    ///
    /// Debug-only: every tile's own `rect` must equal `rect` exactly --
    /// enforced with `debug_assert!` rather than a `Result`, since a
    /// mismatch here is a caller bug in code this crate itself controls, not
    /// a runtime condition an algorithm needs to recover from.
    pub fn new(rect: Rect, tiles: &'a [Tile]) -> Self {
        debug_assert!(
            tiles.iter().all(|tile| tile.rect == rect),
            "every frame-tile handed to TileCtx::new must cover the same rect"
        );
        TileCtx {
            rect,
            backing: Backing::Resident {
                tiles,
                coverage: &[],
            },
            frames_read: AtomicUsize::new(0),
        }
    }

    /// A context that reads `frame_count` frames of `rect` from `reader` as
    /// the visitor reaches them, runs `preprocess` over each, and reports the
    /// reader's per-frame coverage. `samples_per_pixel` sizes the one tile
    /// buffer.
    pub fn on_demand(
        rect: Rect,
        frame_count: usize,
        samples_per_pixel: u32,
        reader: &'a dyn TileReader,
        preprocess: &'a dyn FramePreprocess,
    ) -> Self {
        TileCtx {
            rect,
            backing: Backing::OnDemand {
                frame_count,
                samples_per_pixel,
                reader,
                preprocess,
            },
            frames_read: AtomicUsize::new(0),
        }
    }

    /// Attaches per-frame coverage masks to a context built by [`Self::new`],
    /// index == frame index. `None` (or a frame past the end of `coverage`)
    /// means the frame has a real sample at every pixel.
    pub fn with_coverage(mut self, coverage: &'a [Option<Vec<bool>>]) -> Self {
        debug_assert!(
            coverage
                .iter()
                .flatten()
                .all(|mask| { mask.len() == self.rect.width as usize * self.rect.height as usize }),
            "every coverage mask must have one entry per pixel of the tile rect"
        );
        let Backing::Resident { coverage: slot, .. } = &mut self.backing else {
            unreachable!("with_coverage follows TileCtx::new, which builds a resident backing");
        };
        *slot = coverage;
        self
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn frame_count(&self) -> usize {
        match &self.backing {
            Backing::Resident { tiles, .. } => tiles.len(),
            Backing::OnDemand { frame_count, .. } => *frame_count,
        }
    }

    /// Samples per pixel of the stack's frames. A `Result`, so an
    /// algorithm's `?` carries the failure straight out unchanged: a context
    /// with no frames is [`AlgorithmError::FrameOutOfRange`] for frame 0.
    pub fn samples_per_pixel(&self) -> Result<u32, AlgorithmError> {
        let empty = AlgorithmError::FrameOutOfRange {
            frame: 0,
            frame_count: 0,
        };
        match &self.backing {
            Backing::Resident { tiles, .. } => tiles
                .first()
                .map(|tile| tile.samples_per_pixel)
                .ok_or(empty),
            Backing::OnDemand {
                frame_count,
                samples_per_pixel,
                ..
            } => (*frame_count > 0)
                .then_some(*samples_per_pixel)
                .ok_or(empty),
        }
    }

    /// Calls `visit(frame, tile, coverage)` for every frame in ascending
    /// order and stops at the first error. `coverage` is the frame's mask
    /// over [`Self::rect`], `None` when the frame is covered everywhere. The
    /// tile and mask are only valid for the call; an on-demand context
    /// overwrites the same tile buffer for the next frame.
    pub fn for_each_frame(
        &self,
        mut visit: impl FnMut(usize, &Tile, Option<&[bool]>) -> Result<(), AlgorithmError>,
    ) -> Result<(), AlgorithmError> {
        match &self.backing {
            Backing::Resident { tiles, coverage } => {
                for (f, tile) in tiles.iter().enumerate() {
                    self.frames_read.fetch_add(1, Ordering::Relaxed);
                    visit(f, tile, coverage.get(f).and_then(|mask| mask.as_deref()))?;
                }
            }
            Backing::OnDemand {
                frame_count,
                samples_per_pixel,
                reader,
                preprocess,
            } => {
                let len = self.rect.width as usize
                    * self.rect.height as usize
                    * *samples_per_pixel as usize;
                let mut tile = Tile {
                    rect: self.rect,
                    samples_per_pixel: *samples_per_pixel,
                    samples: vec![0; len],
                };
                record_timing(
                    TimingCounter::AllocationBytes,
                    (tile.samples.capacity() * std::mem::size_of::<u16>()) as u64,
                );
                record_timing(TimingCounter::AllocationOperations, 1);
                for f in 0..*frame_count {
                    let coverage =
                        reader.read_tile_coverage_into(f, self.rect, &mut tile.samples)?;
                    self.frames_read.fetch_add(1, Ordering::Relaxed);
                    preprocess.preprocess(f, &mut tile)?;
                    visit(f, &tile, coverage.as_deref())?;
                }
            }
        }
        Ok(())
    }

    /// The sample type this context's frames are read in: `u16` for resident
    /// tiles, the reader's native type for an on-demand context.
    pub fn sample_format(&self) -> SampleFormat {
        match &self.backing {
            Backing::Resident { .. } => SampleFormat::U16,
            Backing::OnDemand { reader, .. } => reader.sample_format(),
        }
    }

    /// [`Self::for_each_frame`] handing each frame's samples in sample type
    /// `S`: an on-demand context reads `S` from its reader and runs the
    /// preprocess stage over it; a resident one widens its `u16` tiles when
    /// `S` is `f32`. An algorithm picks `S` from [`Self::sample_format`].
    pub fn for_each_frame_as<S: Sample>(
        &self,
        mut visit: impl FnMut(usize, &[S], Option<&[bool]>) -> Result<(), AlgorithmError>,
    ) -> Result<(), AlgorithmError> {
        match &self.backing {
            Backing::Resident { tiles, coverage } => {
                for (f, tile) in tiles.iter().enumerate() {
                    self.frames_read.fetch_add(1, Ordering::Relaxed);
                    let samples = S::from_u16(&tile.samples);
                    visit(
                        f,
                        &samples,
                        coverage.get(f).and_then(|mask| mask.as_deref()),
                    )?;
                }
            }
            Backing::OnDemand {
                frame_count,
                samples_per_pixel,
                reader,
                preprocess,
            } => {
                let len = self.rect.width as usize
                    * self.rect.height as usize
                    * *samples_per_pixel as usize;
                let mut samples = vec![S::default(); len];
                record_timing(
                    TimingCounter::AllocationBytes,
                    (samples.capacity() * std::mem::size_of::<S>()) as u64,
                );
                record_timing(TimingCounter::AllocationOperations, 1);
                for f in 0..*frame_count {
                    let coverage = S::read_tile_coverage(*reader, f, self.rect, &mut samples)?;
                    self.frames_read.fetch_add(1, Ordering::Relaxed);
                    samples = S::preprocess(
                        *preprocess,
                        f,
                        self.rect,
                        *samples_per_pixel,
                        std::mem::take(&mut samples),
                    )?;
                    visit(f, &samples, coverage.as_deref())?;
                }
            }
        }
        Ok(())
    }

    /// How many frame-tiles [`Self::for_each_frame`] has produced so far,
    /// across every call.
    pub fn frames_read(&self) -> usize {
        self.frames_read.load(Ordering::Relaxed)
    }
}

/// One tile-shaped pixel source. Mirrors
/// [`crate::io::TiffStack::read_tile_into`] exactly, so an
/// [`AlignAlgorithm`] reaches real pixels with no frame buffer and no
/// device.
///
/// `out.len()` must equal exactly `rect.width * rect.height *
/// samples_per_pixel`; a rect that does not fit fully inside the frame is an
/// error, **never clamped** -- a caller that wants a clamped rect clamps it
/// at its own call site.
pub trait TileReader: Send + Sync {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError>;

    /// [`Self::read_tile_into`], also returning which pixels of `rect` carry
    /// real samples. `None` means every pixel does; a reader that can leave
    /// pixels without a source (a warp reaching past the frame) returns the
    /// per-pixel mask, row-major, `rect.width * rect.height` long. Uncovered
    /// pixels read as zero in `out`.
    fn read_tile_coverage_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
        self.read_tile_into(frame, rect, out)?;
        Ok(None)
    }

    /// [`Self::read_tile_into`] with every sample multiplied by the Q16
    /// `gain` ([`crate::equalize::UNIT_GAIN`] is 1.0). The default scales the
    /// encoded samples; a reader whose samples are not linear light overrides
    /// this to apply the gain to its linear values.
    fn read_tile_gained_into(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.read_tile_into(frame, rect, out)?;
        crate::equalize::apply_gain(out, gain);
        Ok(())
    }

    /// The sample type this reader serves natively. A reader that serves
    /// `f32` overrides the `f32` reads below; every other reader's `f32`
    /// reads widen its `u16` ones.
    fn sample_format(&self) -> SampleFormat {
        SampleFormat::U16
    }

    /// [`Self::read_tile_into`] in `f32` samples on the 16-bit scale, which a
    /// native `f32` reader serves without clipping to 65535.
    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        widened(out, |scratch| self.read_tile_into(frame, rect, scratch))
    }

    /// [`Self::read_tile_coverage_into`] in `f32` samples. A native `f32`
    /// reader without a coverage mask of its own reports full coverage.
    fn read_tile_coverage_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
        if self.sample_format() == SampleFormat::F32 {
            return self.read_tile_f32_into(frame, rect, out).map(|()| None);
        }
        widened(out, |scratch| {
            self.read_tile_coverage_into(frame, rect, scratch)
        })
    }

    /// [`Self::read_tile_gained_into`] in `f32` samples.
    fn read_tile_gained_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        widened(out, |scratch| {
            self.read_tile_gained_into(frame, rect, gain, scratch)
        })
    }

    /// The mean of each channel of `frame` in linear light, for a reader
    /// whose samples are a nonlinear look of linear data. `None` for a reader
    /// whose samples are the linear ones; an error when the frame cannot be
    /// read.
    fn linear_means(&self, _frame: usize) -> Result<Option<[f64; 3]>, AlgorithmError> {
        Ok(None)
    }

    /// The heap bytes one [`Self::read_tile_into`] of a `width x height` rect
    /// allocates besides `out`: decode buffers, warp sources and the like.
    /// Memory the reader keeps resident between reads (a decoded-segment
    /// cache) is not scratch. Readers that allocate nothing keep the default.
    fn read_scratch_bytes(&self, _width: u32, _height: u32) -> u64 {
        0
    }

    /// Every `step`-th row and column of `rect`, as [`read_tile_step_into`].
    /// A reader that can drop unsampled columns while it converts overrides
    /// this; the default reads each sampled row whole.
    fn read_tile_step_into(
        &self,
        frame: usize,
        rect: Rect,
        step: NonZeroU32,
        samples_per_pixel: u32,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        read_tile_step_into(self, frame, rect, step, samples_per_pixel, out)
    }
}

/// Fills `out` with what `read` writes into a `u16` scratch of the same
/// length; `out` is untouched when `read` fails.
pub(crate) fn widened<T>(
    out: &mut [f32],
    read: impl FnOnce(&mut [u16]) -> Result<T, AlgorithmError>,
) -> Result<T, AlgorithmError> {
    let mut scratch = vec![0u16; out.len()];
    let result = read(&mut scratch)?;
    for (wide, narrow) in out.iter_mut().zip(&scratch) {
        *wide = f32::from(*narrow);
    }
    Ok(result)
}

/// Reads every `step`-th row and column of `rect` from `frame` through
/// `reader`, same output contract as
/// [`crate::io::TiffStack::read_tile_step_into`]: `out.len()` must equal
/// `rect.width.div_ceil(step) * rect.height.div_ceil(step) *
/// samples_per_pixel`. One 1-row read per sampled row, keeping every
/// `step`-th column: `step` more reads, not `step^2`.
pub fn read_tile_step_into<R: TileReader + ?Sized>(
    reader: &R,
    frame: usize,
    rect: Rect,
    step: NonZeroU32,
    samples_per_pixel: u32,
    out: &mut [u16],
) -> Result<(), AlgorithmError> {
    let step = step.get();
    if step == 1 {
        return reader.read_tile_into(frame, rect, out);
    }

    let spp = samples_per_pixel as usize;
    let out_width = rect.width.div_ceil(step);
    let out_height = rect.height.div_ceil(step);
    let expected = out_width as usize * out_height as usize * spp;
    if out.len() != expected {
        return Err(AlgorithmError::Failed {
            algorithm: "read_tile_step_into".to_string(),
            detail: format!(
                "buffer size mismatch: expected {expected} u16 samples, got {}",
                out.len()
            ),
        });
    }

    let mut row_buf = vec![0u16; rect.width as usize * spp];
    for out_row in 0..out_height {
        let row_rect = Rect {
            x: rect.x,
            y: rect.y + out_row * step,
            width: rect.width,
            height: 1,
        };
        reader.read_tile_into(frame, row_rect, &mut row_buf)?;
        let dest_start = (out_row * out_width) as usize * spp;
        for out_col in 0..out_width {
            let src = out_col as usize * step as usize * spp;
            let dest = dest_start + out_col as usize * spp;
            out[dest..dest + spp].copy_from_slice(&row_buf[src..src + spp]);
        }
    }
    Ok(())
}

/// What an [`AlignAlgorithm::align_frame`] call sees: the facts about the
/// stack, which frame is the alignment reference, a handle onto real pixels
/// through a [`TileReader`], and (in the anchor arm only) a driver-prepared
/// value from [`AlignAlgorithm::prepare_run`].
///
/// `Send + Sync` on [`TileReader`] is what keeps `AlignContext<'_>` `Sync`,
/// which is what lets `align_frame` run across frames under rayon. `Send +
/// Sync` on the erased `prepared` value is the same argument applied a
/// second time: a `dyn Any + Send + Sync` behind a shared `&'a` reference is
/// exactly as safe to read from multiple rayon workers at once as the
/// reader already is, and neither type offers any way to mutate through a
/// shared reference, so there is nothing here that would need a lock.
/// The reader field stays private -- no accessor -- so exposing one later is
/// an additive change rather than a removal.
pub struct AlignContext<'a> {
    stack: StackContext,
    reference: usize,
    reader: &'a dyn TileReader,
    prepared: Option<&'a (dyn std::any::Any + Send + Sync)>,
    align_backend: Option<&'a dyn AlignBackend>,
}

impl<'a> AlignContext<'a> {
    pub fn new(stack: StackContext, reference: usize, reader: &'a dyn TileReader) -> Self {
        AlignContext {
            stack,
            reference,
            reader,
            prepared: None,
            align_backend: None,
        }
    }

    /// Attaches the optional accelerator the registration may offer its
    /// large solves to; `None` keeps every solve on the CPU.
    pub fn with_align_backend(mut self, backend: Option<&'a dyn AlignBackend>) -> Self {
        self.align_backend = backend;
        self
    }

    /// The accelerator attached with [`Self::with_align_backend`], if any.
    pub fn align_backend(&self) -> Option<&'a dyn AlignBackend> {
        self.align_backend
    }

    /// Attaches the driver's [`AlignAlgorithm::prepare_run`] output, if any.
    /// Takes the `Option` rather than a bare reference so a driver can
    /// thread `prepare_run`'s own return value straight through with no
    /// branch of its own.
    pub fn with_prepared(
        mut self,
        prepared: Option<&'a (dyn std::any::Any + Send + Sync)>,
    ) -> Self {
        self.prepared = prepared;
        self
    }

    /// The driver-prepared value for this run, if `prepare_run` returned
    /// one. An algorithm that `downcast_ref`s this to a type it does not
    /// recognise -- someone else's prepared state, or none at all -- gets
    /// `None` and **rebuilds**; that is the normal case, never an error.
    pub fn prepared(&self) -> Option<&'a (dyn std::any::Any + Send + Sync)> {
        self.prepared
    }

    pub fn stack(&self) -> &StackContext {
        &self.stack
    }

    pub fn reference(&self) -> usize {
        self.reference
    }

    /// Reads `rect` from `frame` into `out`, through the reader this
    /// context was built with. Same contract as [`TileReader::read_tile_into`]:
    /// `out.len()` must equal `rect.width * rect.height * samples_per_pixel`
    /// exactly, and an out-of-bounds rect is an error, never clamped.
    pub fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.reader.read_tile_into(frame, rect, out)
    }

    /// The sample type the context's reader serves natively.
    pub fn sample_format(&self) -> SampleFormat {
        self.reader.sample_format()
    }

    /// [`Self::read_tile_into`] in `f32` samples on the 16-bit scale.
    pub fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        self.reader.read_tile_f32_into(frame, rect, out)
    }
}

/// A stacking (fusion) algorithm: fuses one tile across every frame of a
/// stack into a composite tile plus a Z-Map naming the contributing source
/// frame per pixel.
///
/// The Z-Map indices `fuse` returns in its [`TileOutput`] are frame indices
/// into the [`TileCtx`] the call was given, `0..ctx.frame_count()` -- which
/// is what makes the `u16` meaningful to anything downstream that reads it.
///
/// `fuse` receives a fully resolved [`ParamValues`] -- the caller runs
/// [`crate::params::resolve`] plus [`crate::params::resolved_values`] first --
/// and a key its own schema declares but the map omits is a named
/// [`AlgorithmError`], never an index panic.
pub trait StackAlgorithm: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn params(&self) -> &[ParamDesc];
    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError>;

    /// [`Self::fuse`] with the composite left in `f32`: nothing is clamped or
    /// rounded to 16 bits. A method without an `f32` output refuses.
    fn fuse_f32(
        &self,
        _ctx: &TileCtx<'_>,
        _cfg: &ParamValues,
    ) -> Result<TileOutput<f32>, AlgorithmError> {
        Err(AlgorithmError::Failed {
            algorithm: self.id().to_string(),
            detail: "this method has no f32 output".to_string(),
        })
    }

    /// Registered for tests only: resolvable by id, never chosen as a default.
    fn test_only(&self) -> bool {
        false
    }

    /// The family's default when a caller names no algorithm; see
    /// `crate::registry::default_stack`.
    fn is_default(&self) -> bool {
        false
    }

    /// Whether this algorithm is a specialist choice: front ends list it after
    /// the others. The registry default is never advanced.
    fn advanced(&self) -> bool {
        false
    }

    /// Defaults this method sets for run-level parameters
    /// (`crate::project::PROJECT_PARAMS`) where the shared default does not
    /// suit it, as `(parameter id, default)` pairs. Read through
    /// `crate::project::params_for`, never directly: that is the one place a
    /// project parameter's effective default is computed.
    fn project_defaults(&self) -> &[(&'static str, ParamValue)] {
        &[]
    }

    /// What the tile scheduler must plan around a tile for this algorithm's
    /// fully resolved `cfg` (`crate::params::resolve` plus
    /// `crate::params::resolved_values`, same as `fuse` receives): the pixel
    /// reach of its output and its decimation depth. The caller applies it
    /// with `crate::tile::TileSchedulerOptions::apply_support`.
    ///
    /// The default reads the well-known key `"levels"` out of `cfg` as the
    /// decimation depth, with no reach beyond the pyramid margin, or `0` if
    /// `cfg` has no such key: an algorithm with no pyramid declares no
    /// `"levels"` parameter and plans no margin, and one that spells its
    /// depth `"levels"` stays synchronized with `fuse`, which reads the same
    /// resolved map. Algorithms with a reach of their own override this.
    fn tile_support(&self, cfg: &ParamValues) -> TileSupport {
        TileSupport::from(
            cfg.get("levels")
                .and_then(ParamValue::as_i64)
                .map(|levels| levels.max(0) as u32)
                .unwrap_or(0),
        )
    }

    /// Heap bytes per pixel of the read rect that one `fuse` call keeps live
    /// beyond the frame-tile and coverage mask the scheduler already counts:
    /// accumulators, per-frame planes and output, for `samples_per_pixel`
    /// samples per pixel, the resolved `cfg` and the stack's sample `format`
    /// (an `F32` stack fuses into an `f32` output). The scheduler adds this
    /// to its per-tile bytes. The default of 0 suits an algorithm that
    /// allocates nothing of its own.
    fn working_bytes_per_pixel(
        &self,
        _cfg: &ParamValues,
        _samples_per_pixel: u32,
        _format: SampleFormat,
    ) -> u64 {
        0
    }
}

/// How one [`AlignAlgorithm::align_frame`] call ended.
///
/// `#[non_exhaustive]`: a divergence reason is inherently
/// algorithm-specific -- `crate::align::DivergenceReason::LemmaOneRegion` is
/// named after one paper's Lemma 1, and a second aligner will name its own
/// failure modes after its own mathematics. That accumulation is
/// deliberate and is exactly what `#[non_exhaustive]` on both this type and
/// `DivergenceReason` is for; it is not a sign that the vocabulary needs
/// generalizing away. A reader adding a second aligner should read the
/// existing variant names as evidence of the pattern to follow, not as a
/// defect to fix.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "kebab-case")]
pub enum AlignStatus {
    /// The solver's step measure fell below its convergence threshold.
    Converged,
    /// The solver's iteration cap was reached with no convergence, and the
    /// last steps were still shrinking.
    HitCap,
    /// The solver's iteration cap was reached and its last steps were flat or
    /// oscillating; the best transform found is kept.
    Stalled,
    /// Registration failed; see [`crate::align::DivergenceReason`].
    Diverged(crate::align::DivergenceReason),
}

impl From<crate::align::EccStatus> for AlignStatus {
    fn from(status: crate::align::EccStatus) -> Self {
        match status {
            crate::align::EccStatus::Converged => AlignStatus::Converged,
            crate::align::EccStatus::HitCap => AlignStatus::HitCap,
            crate::align::EccStatus::Stalled => AlignStatus::Stalled,
            crate::align::EccStatus::Diverged(reason) => AlignStatus::Diverged(reason),
        }
    }
}

/// The outcome of one [`AlignAlgorithm::align_frame`] call: the recovered
/// transform plus enough structure to tell a converged frame from a capped
/// or diverged one without parsing an error string.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlignOutcome {
    /// Reference-to-frame, the same convention [`crate::align::Transform`]
    /// pins everywhere else in this crate.
    pub transform: crate::align::Transform,
    pub status: AlignStatus,
    pub iterations: u32,
    /// An ECC correlation `ρ ∈ [-1, 1]`, never an RMS residual.
    pub residual: f64,
}

/// An alignment algorithm: solves one frame's geometric transform against
/// the stack's reference frame, one call per frame.
pub trait AlignAlgorithm: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn params(&self) -> &[ParamDesc];
    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError>;

    /// Registered for tests only: resolvable by id, never chosen as a default.
    fn test_only(&self) -> bool {
        false
    }

    /// The family's default when a caller names no algorithm; see
    /// `crate::registry::default_align`.
    fn is_default(&self) -> bool {
        false
    }

    /// Called **at most once per run**, by the driver, **only in the anchor
    /// arm**, before any [`align_frame`](Self::align_frame) call of that
    /// run -- with the same fully resolved `cfg` every `align_frame` call of
    /// that run will receive. The returned value is opaque to the driver,
    /// which only owns it and hands it back to every `align_frame` call
    /// through [`AlignContext::prepared`]; nothing here interprets it.
    ///
    /// `Ok(None)` is always a correct implementation -- it costs only the
    /// rebuild it declines to avoid -- which is why every implementor except
    /// [`crate::align::PyramidalEcc`] inherits this default unchanged.
    fn prepare_run(
        &self,
        _ctx: &AlignContext<'_>,
        _cfg: &ParamValues,
    ) -> Result<Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>, AlgorithmError> {
        Ok(None)
    }
}

impl TileReader for crate::io::TiffStack {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        // Fully-qualified: the inherent `TiffStack::read_tile_into` and this
        // trait method share a name, and the inherent one wins at every
        // direct call site -- a plain `self.read_tile_into(...)` here would
        // recurse into this very impl instead of reaching the inherent
        // method.
        crate::io::TiffStack::read_tile_into(self, frame, rect, out)
            .map_err(crate::io::FrameError::from)
            .map_err(AlgorithmError::from)
    }

    fn read_tile_step_into(
        &self,
        frame: usize,
        rect: Rect,
        step: NonZeroU32,
        _samples_per_pixel: u32,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        crate::io::TiffStack::read_tile_step_into(self, frame, rect, step, out)
            .map_err(crate::io::FrameError::from)
            .map_err(AlgorithmError::from)
    }

    fn sample_format(&self) -> SampleFormat {
        crate::io::TiffStack::sample_format(self)
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        crate::io::TiffStack::read_tile_f32_into(self, frame, rect, out)
            .map_err(crate::io::FrameError::from)
            .map_err(AlgorithmError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, LazyLock};

    use super::*;
    use crate::align::Transform;
    use crate::io::{FrameError, TiffError};
    use crate::params::{EnumVariant, ParamKind, ParamValue, Provenance};

    #[test]
    fn a_stalled_status_serialises_as_its_kebab_name() {
        let json = serde_json::to_string(&AlignStatus::Stalled).expect("serialise");
        assert_eq!(json, r#"{"status":"stalled"}"#);
        let back: AlignStatus = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, AlignStatus::Stalled);
        assert_eq!(
            AlignStatus::from(crate::align::EccStatus::Stalled),
            AlignStatus::Stalled
        );
    }

    fn sample_tiles(frame_count: usize, width: u32, height: u32) -> (Rect, Vec<Tile>) {
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let spp = 3;
        let tiles = (0..frame_count)
            .map(|f| Tile {
                rect,
                samples_per_pixel: spp,
                samples: vec![f as u16; (width * height * spp) as usize],
            })
            .collect();
        (rect, tiles)
    }

    static DUMMY_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
        vec![
            ParamDesc {
                id: "count-based-threshold".to_string(),
                label: "Count-based threshold".to_string(),
                help: "derived from the stack's frame count".to_string(),
                kind: ParamKind::Float {
                    min: 0.0,
                    max: 1000.0,
                    step: 1.0,
                    logarithmic: false,
                },
                default: ParamValue::Float(0.0),
                advanced: false,
                auto: Some(Arc::new(|ctx: &StackContext| {
                    ParamValue::Float(ctx.frame_count as f64)
                })),
                host_default: None,
            },
            ParamDesc {
                id: "mode".to_string(),
                label: "Mode".to_string(),
                help: "dummy mode selector".to_string(),
                kind: ParamKind::Enum {
                    variants: vec![
                        EnumVariant {
                            id: "fast".to_string(),
                            label: "Fast".to_string(),
                        },
                        EnumVariant {
                            id: "slow".to_string(),
                            label: "Slow".to_string(),
                        },
                    ],
                },
                default: ParamValue::Enum("fast".to_string()),
                advanced: false,
                auto: None,
                host_default: None,
            },
        ]
    });

    /// A test-only fusion algorithm. `id` and `display_name` are owned
    /// `String` fields, not literals, so the trait's accessors genuinely
    /// borrow from `&self` rather than returning `&'static str` in
    /// disguise.
    struct DummyStack {
        id: String,
        display_name: String,
    }

    impl DummyStack {
        fn new() -> Self {
            DummyStack {
                id: "dummy-stack".to_string(),
                display_name: "Dummy Stack".to_string(),
            }
        }
    }

    impl StackAlgorithm for DummyStack {
        fn id(&self) -> &str {
            &self.id
        }

        fn display_name(&self) -> &str {
            &self.display_name
        }

        fn params(&self) -> &[ParamDesc] {
            &DUMMY_PARAMS
        }

        fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
            crate::params::validate(self.params(), cfg)?;
            ctx.samples_per_pixel()?;
            let mut first = None;
            ctx.for_each_frame(|f, tile, _| {
                if f == 0 {
                    first = Some(tile.clone());
                }
                Ok(())
            })?;
            let tile = first.expect("a context with a frame 0 visits it");
            let zmap = vec![0u16; (tile.rect.width * tile.rect.height) as usize];
            Ok(TileOutput { tile, zmap })
        }
    }

    /// A test-only alignment algorithm: identity for the reference frame, a
    /// translation for every other frame.
    struct DummyAlign;

    impl AlignAlgorithm for DummyAlign {
        fn id(&self) -> &str {
            "dummy-align"
        }

        fn display_name(&self) -> &str {
            "Dummy Align"
        }

        fn params(&self) -> &[ParamDesc] {
            &[]
        }

        fn align_frame(
            &self,
            ctx: &AlignContext<'_>,
            frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            let transform = if frame == ctx.reference() {
                Transform::identity()
            } else {
                let mut m = Transform::identity().m;
                m[0][2] = frame as f64 - ctx.reference() as f64;
                Transform { m }
            };
            Ok(AlignOutcome {
                transform,
                status: AlignStatus::Converged,
                iterations: 1,
                residual: 1.0,
            })
        }
    }

    /// A test-only alignment algorithm that always fails, proving a work
    /// method reports why rather than returning a poisoned output.
    struct FailingAlign;

    impl AlignAlgorithm for FailingAlign {
        fn id(&self) -> &str {
            "failing-align"
        }

        fn display_name(&self) -> &str {
            "Failing Align"
        }

        fn params(&self) -> &[ParamDesc] {
            &[]
        }

        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            _frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            Err(AlgorithmError::Failed {
                algorithm: self.id().to_string(),
                detail: "intentional test failure".to_string(),
            })
        }
    }

    /// A test-only, in-memory [`TileReader`]: one flat sample buffer per
    /// frame, chunky row-major, honouring the same exact-length and
    /// in-bounds contracts as [`crate::io::TiffStack::read_tile_into`].
    struct VecReader {
        frames: Vec<Vec<u16>>,
        width: u32,
        height: u32,
        spp: u32,
    }

    impl TileReader for VecReader {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            let in_bounds = rect.width > 0
                && rect.height > 0
                && rect
                    .x
                    .checked_add(rect.width)
                    .is_some_and(|xw| xw <= self.width)
                && rect
                    .y
                    .checked_add(rect.height)
                    .is_some_and(|yh| yh <= self.height);
            if !in_bounds {
                return Err(AlgorithmError::Frame(FrameError::Tiff(
                    TiffError::OutOfBounds {
                        path: PathBuf::from("<memory>"),
                        requested: rect,
                        frame: (self.width, self.height),
                    },
                )));
            }

            let expected_len = rect.width as usize * rect.height as usize * self.spp as usize;
            if out.len() != expected_len {
                return Err(AlgorithmError::Frame(FrameError::Tiff(
                    TiffError::BufferSize {
                        expected: expected_len,
                        got: out.len(),
                    },
                )));
            }

            let frame_samples = &self.frames[frame];
            let stride = self.width as usize * self.spp as usize;
            let row_len = rect.width as usize * self.spp as usize;
            let mut out_off = 0;
            for r in rect.y..(rect.y + rect.height) {
                let row_start = r as usize * stride + rect.x as usize * self.spp as usize;
                out[out_off..out_off + row_len]
                    .copy_from_slice(&frame_samples[row_start..row_start + row_len]);
                out_off += row_len;
            }
            Ok(())
        }
    }

    #[test]
    fn dummy_stack_algorithm_implements_the_trait() {
        let algo = DummyStack::new();
        assert_eq!(algo.id(), "dummy-stack");
        assert_eq!(algo.display_name(), "Dummy Stack");
        assert_eq!(algo.params().len(), 2);

        let (rect, tiles) = sample_tiles(2, 4, 4);
        let ctx = TileCtx::new(rect, &tiles);
        let out = algo.fuse(&ctx, &ParamValues::new()).unwrap();
        assert_eq!(out.tile.rect, rect);
        assert_eq!(out.zmap.len(), (rect.width * rect.height) as usize);
    }

    #[test]
    fn dummy_align_algorithm_implements_the_trait() {
        let reader = VecReader {
            frames: vec![vec![0u16; 4 * 4 * 3]; 3],
            width: 4,
            height: 4,
            spp: 3,
        };
        let stack_ctx = StackContext::new(3, 4, 4, 3);
        let ctx = AlignContext::new(stack_ctx, 1, &reader);
        let algo = DummyAlign;

        for frame in 0..stack_ctx.frame_count {
            let outcome = algo.align_frame(&ctx, frame, &ParamValues::new()).unwrap();
            if frame == ctx.reference() {
                assert_eq!(outcome.transform, Transform::identity());
            } else {
                assert_ne!(outcome.transform, Transform::identity());
            }
        }
    }

    #[test]
    fn transform_is_one_representation() {
        let translation = Transform {
            m: [[1.0, 0.0, 5.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        };
        // A real rotation, not a pure scale: the criterion is that one representation
        // covers both restricted modes, and a zero-angle rotation would not show that.
        let (s, c) = (0.3_f64.sin(), 0.3_f64.cos());
        let rotate_scale = Transform {
            m: [
                [2.0 * c, -2.0 * s, 0.0],
                [2.0 * s, 2.0 * c, 0.0],
                [0.0, 0.0, 1.0],
            ],
        };

        let a = translation.compose(&rotate_scale);
        let b = rotate_scale.compose(&translation);
        assert_ne!(a, b, "composition must not be commutative");

        assert_eq!(translation.compose(&Transform::identity()), translation);
    }

    #[test]
    fn traits_are_object_safe() {
        let stacks: Vec<Box<dyn StackAlgorithm>> = vec![Box::new(DummyStack::new())];
        for algo in &stacks {
            let _ = algo.id();
            let _ = algo.display_name();
            let _ = algo.params();
        }

        let aligns: Vec<Box<dyn AlignAlgorithm>> =
            vec![Box::new(DummyAlign), Box::new(FailingAlign)];
        for algo in &aligns {
            let _ = algo.id();
            let _ = algo.display_name();
            let _ = algo.params();
        }
    }

    #[test]
    fn algorithm_is_send_and_sync() {
        fn _assert<T: Send + Sync + 'static>() {}
        _assert::<Box<dyn StackAlgorithm>>();
        _assert::<Box<dyn AlignAlgorithm>>();

        let algo: Arc<dyn StackAlgorithm> = Arc::new(DummyStack::new());
        let (rect, tiles) = sample_tiles(2, 4, 4);
        let ctx = TileCtx::new(rect, &tiles);

        std::thread::scope(|s| {
            for _ in 0..4 {
                let algo = Arc::clone(&algo);
                let ctx = &ctx;
                s.spawn(move || {
                    let out = algo.fuse(ctx, &ParamValues::new()).unwrap();
                    assert_eq!(out.zmap.len(), (rect.width * rect.height) as usize);
                });
            }
        });
    }

    #[test]
    fn params_validate_against_the_schema() {
        let algo = DummyStack::new();
        let mut cfg = ParamValues::new();
        cfg.insert(
            "count-based-threshold".to_string(),
            ParamValue::Float(9999.0),
        );
        let direct_err = crate::params::validate(algo.params(), &cfg).unwrap_err();
        assert!(direct_err.to_string().contains("count-based-threshold"));

        let (rect, tiles) = sample_tiles(1, 2, 2);
        let ctx = TileCtx::new(rect, &tiles);
        match algo.fuse(&ctx, &cfg) {
            Err(AlgorithmError::Params(inner)) => {
                assert_eq!(inner.to_string(), direct_err.to_string());
            }
            other => panic!("expected AlgorithmError::Params, got {other:?}"),
        }
    }

    #[test]
    fn fusion_output_carries_a_z_map() {
        let algo = DummyStack::new();
        let (rect, tiles) = sample_tiles(3, 4, 4);
        let ctx = TileCtx::new(rect, &tiles);
        let out = algo.fuse(&ctx, &ParamValues::new()).unwrap();

        assert_eq!(out.zmap.len(), (rect.width * rect.height) as usize);
        assert!(
            out.zmap
                .iter()
                .all(|&index| (index as usize) < ctx.frame_count())
        );
    }

    #[test]
    fn an_empty_context_has_no_samples_per_pixel() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let empty = TileCtx::new(rect, &[]);
        let err = empty
            .samples_per_pixel()
            .expect_err("an empty context has no frame 0");
        assert!(matches!(
            err,
            AlgorithmError::FrameOutOfRange {
                frame: 0,
                frame_count: 0
            }
        ));

        let (rect, tiles) = sample_tiles(3, 2, 2);
        assert_eq!(TileCtx::new(rect, &tiles).samples_per_pixel().unwrap(), 3);
    }

    /// Serves frame `f` as `f + 1 + (offset within the rect)` and leaves the
    /// pixel at row 1, column 2 of frame 1 uncovered.
    struct MaskedReader;

    impl TileReader for MaskedReader {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            for (i, sample) in out.iter_mut().enumerate() {
                *sample = (frame * 1000 + rect.x as usize + rect.y as usize + i) as u16;
            }
            Ok(())
        }

        fn read_tile_coverage_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<Option<Vec<bool>>, AlgorithmError> {
            self.read_tile_into(frame, rect, out)?;
            Ok((frame == 1).then(|| {
                let mut mask = vec![true; rect.width as usize * rect.height as usize];
                mask[rect.width as usize + 2] = false;
                mask
            }))
        }
    }

    #[test]
    fn on_demand_visits_every_frame_in_order_with_its_coverage() {
        let rect = Rect {
            x: 3,
            y: 5,
            width: 37,
            height: 23,
        };
        let frame_count = 4;
        let len = (rect.width * rect.height) as usize;

        let tiles: Vec<Tile> = (0..frame_count)
            .map(|f| {
                let mut samples = vec![0u16; len];
                MaskedReader.read_tile_into(f, rect, &mut samples).unwrap();
                Tile {
                    rect,
                    samples_per_pixel: 1,
                    samples,
                }
            })
            .collect();
        let coverage: Vec<Option<Vec<bool>>> = (0..frame_count)
            .map(|f| {
                MaskedReader
                    .read_tile_coverage_into(f, rect, &mut vec![0u16; len])
                    .unwrap()
            })
            .collect();
        let resident = TileCtx::new(rect, &tiles).with_coverage(&coverage);
        let on_demand = TileCtx::on_demand(
            rect,
            frame_count,
            1,
            &MaskedReader,
            &crate::tile::NoopPreprocess,
        );
        assert_eq!(on_demand.frame_count(), frame_count);
        assert_eq!(on_demand.samples_per_pixel().unwrap(), 1);

        type Seen = Vec<(usize, Vec<u16>, Option<Vec<bool>>)>;
        let collect = |ctx: &TileCtx<'_>| -> (Seen, Vec<*const u16>) {
            let mut seen = Seen::new();
            let mut buffers = Vec::new();
            ctx.for_each_frame(|f, tile, covered| {
                assert_eq!(tile.rect, rect);
                buffers.push(tile.samples.as_ptr());
                seen.push((f, tile.samples.clone(), covered.map(<[bool]>::to_vec)));
                Ok(())
            })
            .unwrap();
            (seen, buffers)
        };
        let (expected, _) = collect(&resident);
        let (streamed, buffers) = collect(&on_demand);

        assert_eq!(streamed, expected);
        assert_eq!(
            streamed.iter().map(|(f, ..)| *f).collect::<Vec<_>>(),
            (0..frame_count).collect::<Vec<_>>()
        );
        assert!(streamed[1].2.is_some() && streamed[0].2.is_none());
        assert_eq!(on_demand.frames_read(), frame_count);
        assert!(
            buffers.iter().all(|&ptr| ptr == buffers[0]),
            "one tile buffer serves every frame"
        );
    }

    #[test]
    fn a_visitor_error_stops_the_pass() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        let ctx = TileCtx::on_demand(rect, 5, 1, &MaskedReader, &crate::tile::NoopPreprocess);
        let mut visited = 0;
        let err = ctx
            .for_each_frame(|f, _, _| {
                visited += 1;
                if f == 2 {
                    return Err(AlgorithmError::Failed {
                        algorithm: "test".to_string(),
                        detail: "stop".to_string(),
                    });
                }
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(err, AlgorithmError::Failed { .. }));
        assert_eq!(visited, 3);
        assert_eq!(ctx.frames_read(), 3);
    }

    #[test]
    fn work_method_reports_failure() {
        let reader = VecReader {
            frames: vec![vec![0u16; 4 * 4 * 3]],
            width: 4,
            height: 4,
            spp: 3,
        };
        let stack_ctx = StackContext::new(1, 4, 4, 3);
        let ctx = AlignContext::new(stack_ctx, 0, &reader);

        let err = FailingAlign
            .align_frame(&ctx, 0, &ParamValues::new())
            .unwrap_err();
        match &err {
            AlgorithmError::Failed { algorithm, detail } => {
                let msg = err.to_string();
                assert!(msg.contains(algorithm));
                assert!(msg.contains(detail));
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        let rect = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let mut too_small = vec![0u16; 1];
        let read_err = reader.read_tile_into(0, rect, &mut too_small).unwrap_err();
        let expected_source = TiffError::BufferSize {
            expected: 4 * 4 * 3,
            got: 1,
        };
        match &read_err {
            AlgorithmError::Frame(FrameError::Tiff(inner)) => {
                assert_eq!(inner.to_string(), expected_source.to_string());
            }
            other => panic!("expected Tiff, got {other:?}"),
        }
    }

    #[test]
    fn tile_support_default_is_zero_when_the_schema_declares_no_levels_key() {
        // `DummyStack`'s schema is `count-based-threshold` + `mode`, never
        // `levels`, so the resolved map never carries that key -- the
        // default answers "no pyramid, no margin".
        let algo = DummyStack::new();
        assert_eq!(algo.tile_support(&ParamValues::new()), TileSupport::from(0));
    }

    #[test]
    fn tile_support_default_reads_the_levels_key_when_present() {
        let algo = DummyStack::new();
        let mut cfg = ParamValues::new();
        cfg.insert("levels".to_string(), ParamValue::Int(7));
        assert_eq!(algo.tile_support(&cfg), TileSupport::from(7));
    }

    #[test]
    fn auto_hook_reads_the_stack_context() {
        let ctx: StackContext = crate::params::StackContext::new(5, 10, 10, 3);
        let algo = DummyStack::new();
        let resolved = crate::params::resolve(algo.params(), &ParamValues::new(), &ctx).unwrap();
        assert_eq!(
            resolved["count-based-threshold"].provenance,
            Provenance::Auto
        );
        assert_eq!(
            resolved["count-based-threshold"].value,
            ParamValue::Float(5.0)
        );
    }

    #[test]
    fn align_context_reads_tiles() {
        let samples: Vec<u16> = (0..(4 * 4 * 3)).map(|i| i as u16).collect();
        let reader = VecReader {
            frames: vec![samples],
            width: 4,
            height: 4,
            spp: 3,
        };
        let stack_ctx = StackContext::new(1, 4, 4, 3);
        let ctx = AlignContext::new(stack_ctx, 0, &reader);

        let rect = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let mut out = vec![0u16; 2 * 2 * 3];
        ctx.read_tile_into(0, rect, &mut out).unwrap();

        let mut direct = vec![0u16; 2 * 2 * 3];
        reader.read_tile_into(0, rect, &mut direct).unwrap();
        assert_eq!(out, direct);

        let mut wrong = vec![0u16; 3];
        match ctx.read_tile_into(0, rect, &mut wrong) {
            Err(AlgorithmError::Frame(FrameError::Tiff(TiffError::BufferSize { .. }))) => {}
            other => panic!("expected BufferSize error, got {other:?}"),
        }
        assert_eq!(
            wrong,
            vec![0u16; 3],
            "a rejected buffer must not be partially written"
        );
    }
}
