// SPDX-License-Identifier: GPL-3.0-or-later

//! Canvas state as plain Rust: no `use slint::` and no `use wgpu::`
//! anywhere in this file. The `AppController` methods this state backs
//! live in
//! `crates/gui/src/controller.rs` (so they can reach `ShellState`'s private
//! field); this module owns only the types and the pure functions. The
//! wgpu half (texture creation, `slint::Image::try_from`) is
//! `crates/gui/src/gpu_canvas.rs`, the only module in the crate naming
//! `wgpu` or `slint::wgpu_29`.

use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use focusforge_core::TiffError;
use focusforge_core::algorithm::{AlgorithmError, read_tile_step_into};
use focusforge_core::align::{Transform, WarpingReader};
use focusforge_core::display::alpha_from_opacity;
use focusforge_core::equalize::GainReader;
use focusforge_core::io::{FrameError, FrameReader, TiffStack, open_frame_set};
use focusforge_core::project::Op;
use focusforge_core::tile::{HostBudget, Rect};
use focusforge_core::timing::{TimingCounter, measurement_scope, record_timing};
use focusforge_core::zmap::{ZMapError, ZMapReader, overlay_table};

use crate::viewport::PaneDraw;

/// What the open renderer cell should do for the frame about to draw --
/// [`crate::controller::AppController::canvas_feed`]'s answer, decided with
/// no GPU and no window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasFeed {
    /// The renderer already holds the active tab's planes; draw as is.
    Keep,
    /// No tab is active, or the active tab has nothing sealed to show yet.
    Clear,
    /// Open `composite`, and `zmap` when the row has a readable one, as
    /// `tab`'s own planes.
    Load {
        tab: usize,
        composite: PathBuf,
        zmap: Option<PathBuf>,
    },
}

/// Which pane a piece of canvas state belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneSource {
    Frame,
    Composite,
}

/// The display texture's size, the whole "allocated once, re-allocated only
/// on a size change" rule reduced to one pure comparison
/// [`needs_realloc`] drives with no GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureSpec {
    pub width: u32,
    pub height: u32,
}

/// Whether a display texture of `current`'s spec must be re-allocated to
/// show `wanted`. `None` (no texture yet) always needs one.
pub fn needs_realloc(current: Option<TextureSpec>, wanted: TextureSpec) -> bool {
    current != Some(wanted)
}

/// The one format Slint's texture importer accepts that this crate uses
/// (`focusforge_gpu::display::DISPLAY_TEXTURE_FORMAT`, named here too so a
/// wrong format is refused by name at this seam rather than surfacing only
/// as `TextureImportError::InvalidFormat` at `try_from`).
pub const DISPLAY_FORMAT: &str = "Rgba8Unorm";

/// What can go wrong validating the display format, or finding a loaded
/// project's two panes at different sizes.
#[derive(Debug, thiserror::Error)]
pub enum CanvasError {
    #[error("unsupported display format {got:?}, only \"Rgba8Unorm\" is supported")]
    UnsupportedDisplayFormat { got: String },
    /// One viewport over two panes, enforced rather than assumed: a
    /// composite and a Z-Map of different
    /// sizes have no one rect a shared viewport could mean.
    #[error("composite is {composite:?}, Z-Map is {zmap:?} -- both panes must share one geometry")]
    PaneDimensionsDiffer {
        composite: (u32, u32),
        zmap: (u32, u32),
    },
}

/// Refuses any format name other than [`DISPLAY_FORMAT`], by name, rather
/// than leaving that to `slint::Image::try_from`'s
/// `TextureImportError::InvalidFormat`.
pub fn validate_display_format(name: &str) -> Result<(), CanvasError> {
    if name == DISPLAY_FORMAT {
        Ok(())
    } else {
        Err(CanvasError::UnsupportedDisplayFormat {
            got: name.to_string(),
        })
    }
}

/// Refuses a composite/Z-Map size mismatch by name -- one viewport over
/// two panes is enforced, not assumed.
pub fn check_pane_dimensions(composite: (u32, u32), zmap: (u32, u32)) -> Result<(), CanvasError> {
    if composite == zmap {
        Ok(())
    } else {
        Err(CanvasError::PaneDimensionsDiffer { composite, zmap })
    }
}

/// A frame's reference-to-frame [`Transform`], crop origin and composite
/// extent and equalization gain (Q16, unit when the run was not equalized),
/// as recorded by the fuse. [`FrameSource::read_into`] warps every read
/// through this so the left pane lines up with the composite. `None` means
/// the raw frame, read through identity at `(0, 0)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameView {
    pub transform: Transform,
    pub gain: u32,
    pub origin: (u32, u32),
    pub extent: (u32, u32),
}

/// The one open source-frame handle the left canvas pane draws from -- never
/// more than one frame set opened at once. Sixty-one frames held open at
/// the same time would map the whole reference stack, which is why
/// [`Self::set`] drops whatever was open before opening the next. Opened
/// through [`open_frame_set`], so a compressed frame is shown exactly as the
/// fuse itself would read it, not narrowed to the mmap fast path.
#[derive(Default)]
pub struct FrameSource {
    open: Option<(PathBuf, Arc<dyn FrameReader>, Option<FrameView>)>,
}

impl FrameSource {
    /// Opens `path` under `budget`, dropping whatever was open first, and
    /// records `view` for [`Self::read_into`] to warp through. Nothing is
    /// left open on failure.
    pub fn set(
        &mut self,
        path: &Path,
        view: Option<FrameView>,
        budget: HostBudget,
    ) -> Result<(), FrameError> {
        self.open = None;
        let set = open_frame_set(std::slice::from_ref(&path.to_path_buf()), budget)?;
        self.open = Some((path.to_path_buf(), Arc::from(set.reader), view));
        Ok(())
    }

    /// Drops the open handle, if any.
    pub fn clear(&mut self) {
        self.open = None;
    }

    /// The currently open frame's own path, if any.
    pub fn path(&self) -> Option<&Path> {
        self.open.as_ref().map(|(path, ..)| path.as_path())
    }

    /// The currently recorded view, if any.
    pub fn view(&self) -> Option<FrameView> {
        self.open.as_ref().and_then(|(_, _, view)| *view)
    }

    /// The frame-as-fused extent when a view is set, else the frame's own
    /// size, if any frame is open.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.open.as_ref().map(|(_, reader, view)| {
            view.map(|v| v.extent)
                .unwrap_or_else(|| reader.dimensions())
        })
    }

    /// The open frame's samples per pixel, if any.
    pub fn samples_per_pixel(&self) -> Option<u32> {
        self.open
            .as_ref()
            .map(|(_, reader, _)| reader.frame(0).samples_per_pixel)
    }

    /// The open frame as [`Self::read_into`] reads it -- warped by the
    /// recorded view -- shared with a preparation worker's reads.
    pub fn reader(&self) -> Option<Arc<dyn PaneReader>> {
        self.open.as_ref().map(|(_, reader, view)| {
            Arc::new(WarpedFrame {
                reader: reader.clone(),
                view: *view,
            }) as Arc<dyn PaneReader>
        })
    }

    /// Reads every `step`-th row and column of `rect` into `out`, warped by
    /// the recorded view's transform and offset by its crop origin -- an
    /// identity transform at origin `(0, 0)` with no view set. Callers check
    /// [`Self::dimensions`] first, so a frame is always open by the time
    /// this runs.
    pub fn read_into(
        &self,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let (_, reader, view) = self
            .open
            .as_ref()
            .expect("read_into is only called once a frame is confirmed open");
        read_warped(&**reader, *view, rect, step, out)
    }
}

/// Reads every `step`-th row and column of `rect` from `reader`'s frame 0,
/// warped by `view`'s transform and offset by its crop origin. Without a
/// `view` the frame is read directly: an identity warp returns the same
/// samples but reads and converts about twice the data.
fn read_warped(
    reader: &dyn FrameReader,
    view: Option<FrameView>,
    rect: Rect,
    step: NonZeroU32,
    out: &mut [u16],
) -> Result<(), AlgorithmError> {
    let samples_per_pixel = reader.frame(0).samples_per_pixel;
    let Some(view) = view else {
        return reader.read_tile_step_into(0, rect, step, samples_per_pixel, out);
    };
    let (transform, origin) = (view.transform, view.origin);
    let gains = [view.gain];
    let gained = GainReader::new(reader, &gains);
    let warped = WarpingReader::new(
        &gained,
        reader.dimensions(),
        samples_per_pixel,
        vec![transform],
    );
    let shifted = Rect {
        x: rect.x + origin.0,
        y: rect.y + origin.1,
        ..rect
    };
    read_tile_step_into(&warped, 0, shifted, step, samples_per_pixel, out)
}

/// One open frame plus the view it is warped through, readable from a
/// worker thread with exactly [`FrameSource::read_into`]'s semantics.
struct WarpedFrame {
    reader: Arc<dyn FrameReader>,
    view: Option<FrameView>,
}

impl PaneReader for WarpedFrame {
    fn samples_per_pixel(&self) -> u32 {
        self.reader.frame(0).samples_per_pixel
    }

    fn read_step_into(
        &self,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), PaneReadError> {
        Ok(read_warped(&*self.reader, self.view, rect, step, out)?)
    }
}

/// Collapses many repaint requests into one scheduled repaint: `request`
/// answers `true` only for the first request since the last `take`, which is
/// the caller's cue to schedule the one repaint that will `take` it.
#[derive(Debug, Default)]
pub struct RepaintGate {
    pending: Cell<bool>,
}

impl RepaintGate {
    /// Marks a repaint as wanted; `true` when none was pending yet.
    pub fn request(&self) -> bool {
        !self.pending.replace(true)
    }

    /// Clears the pending mark; `true` when a repaint was pending.
    pub fn take(&self) -> bool {
        self.pending.replace(false)
    }
}

/// Whether `src`'s rect lies fully inside a `frame`-sized image -- the same
/// bound `TiffStack::read_tile_into` enforces, decided before any read
/// happens so an out-of-range viewport rect never reaches the mmap.
pub fn draw_rect_fits(frame: (u32, u32), src: (u32, u32, u32, u32)) -> bool {
    let (frame_width, frame_height) = frame;
    let (x, y, width, height) = src;
    width > 0
        && height > 0
        && x.checked_add(width).is_some_and(|xw| xw <= frame_width)
        && y.checked_add(height).is_some_and(|yh| yh <= frame_height)
}

/// Characters `FFPaneLabel`'s 120 px pill holds, at `Theme.text-body` size,
/// with room left for the pill's own rounded edge -- measured against the
/// shipped Inter body face, where the pill's own width fits roughly 20
/// characters edge to edge.
const PANE_LABEL_MAX_CHARS: usize = 16;

/// Fits `label` inside `FFPaneLabel`'s pill by keeping its tail: a label
/// short enough already passes through unchanged; a longer one is cut down
/// to a fixed character budget with a leading `…` standing in for the
/// dropped head, since a long filename's distinguishing part is its end,
/// not its shared prefix.
pub fn truncate_pane_label(label: &str) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= PANE_LABEL_MAX_CHARS {
        return label.to_string();
    }
    let kept = PANE_LABEL_MAX_CHARS - 1;
    let tail: String = chars[chars.len() - kept..].iter().collect();
    format!("\u{2026}{tail}")
}

/// Grows `scratch` to at least `len` elements, recording [`SampleBytes`]
/// only when its capacity actually grows -- a draw whose sample count stays
/// within an already-reached capacity costs nothing here.
///
/// [`SampleBytes`]: TimingCounter::SampleBytes
pub(crate) fn grow_scratch(scratch: &mut Vec<u16>, len: usize) -> &mut [u16] {
    let capacity_before = scratch.capacity();
    scratch.resize(len, 0);
    if scratch.capacity() > capacity_before {
        record_timing(
            TimingCounter::SampleBytes,
            (scratch.capacity() * std::mem::size_of::<u16>()) as u64,
        );
    }
    &mut scratch[..len]
}

/// What went wrong reading one pane's samples.
#[derive(Debug, thiserror::Error)]
pub enum PaneReadError {
    #[error(transparent)]
    Tiff(#[from] TiffError),
    #[error(transparent)]
    ZMap(#[from] ZMapError),
    #[error(transparent)]
    Frame(#[from] AlgorithmError),
}

/// A source one pane draws from, readable from a worker thread.
pub trait PaneReader: Send + Sync {
    /// Samples per pixel of what [`Self::read_step_into`] writes.
    fn samples_per_pixel(&self) -> u32;

    /// Reads every `step`-th row and column of `rect` into `out`
    /// (`rect.width.div_ceil(step) * rect.height.div_ceil(step)` pixels).
    fn read_step_into(
        &self,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), PaneReadError>;
}

impl PaneReader for TiffStack {
    fn samples_per_pixel(&self) -> u32 {
        self.header(0).samples_per_pixel
    }

    fn read_step_into(
        &self,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), PaneReadError> {
        Ok(self.read_tile_step_into(0, rect, step, out)?)
    }
}

impl PaneReader for ZMapReader {
    fn samples_per_pixel(&self) -> u32 {
        1
    }

    fn read_step_into(
        &self,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), PaneReadError> {
        Ok(self.read_tile_step_into(rect, step, out)?)
    }
}

/// The canvas surfaces that draw from a [`PaneReader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Frame,
    Composite,
}

/// One pane's view to read: what to read, the stamp it was issued under and
/// what the display needs to render it.
#[derive(Clone)]
pub struct PaneJob {
    pub pane: Pane,
    pub draw: PaneDraw,
    pub stamp: u64,
    pub reader: Arc<dyn PaneReader>,
    /// The sealed Z-Map, read alongside a composite whose edit generation or
    /// overlay [`Self::state`] names; `None` when the sealed pixels draw as is.
    pub indices: Option<Arc<dyn PaneReader>>,
    /// The edit generation and overlay this job's result is drawn under.
    pub state: EditState,
    /// Whether this is the low-detail whole-image read behind the pane's
    /// exact texture rather than the exact view.
    pub backdrop: bool,
}

/// What besides `src`/`step` a pane's content depends on: the edit session's
/// generation and the composite overlay. Equal states draw equal pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditState {
    pub generation: Option<u64>,
    pub overlay: Option<OverlayDraw>,
}

impl PaneJob {
    /// Sizes `samples` to the draw's texture and reads the job's source into it.
    pub(crate) fn read_into(&self, samples: &mut Vec<u16>) -> Result<(), PaneReadError> {
        let (x, y, width, height) = self.draw.src;
        let (tex_w, tex_h) = self.draw.extent();
        let len = tex_w as usize * tex_h as usize * self.reader.samples_per_pixel() as usize;
        let out = grow_scratch(samples, len);
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        self.reader.read_step_into(rect, self.draw.step, out)
    }

    /// Sizes `indices` to the draw's texture and reads the sealed Z-Map into
    /// it; empties it when the job carries no index source.
    pub(crate) fn read_sealed_indices(&self, indices: &mut Vec<u16>) -> Result<(), PaneReadError> {
        let Some(reader) = &self.indices else {
            indices.clear();
            return Ok(());
        };
        let (x, y, width, height) = self.draw.src;
        let (tex_w, tex_h) = self.draw.extent();
        let out = grow_scratch(indices, tex_w as usize * tex_h as usize);
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        reader.read_step_into(rect, self.draw.step, out)
    }
}

/// A [`PaneJob`] with its samples read.
pub struct Prepared {
    pub job: PaneJob,
    pub samples: Vec<u16>,
    /// The sealed Z-Map over the same texels; empty when the job has none.
    pub indices: Vec<u16>,
}

/// Accepts only stamps above the newest one it has seen or been reset to,
/// so a result issued before a newer publish or a reset is refused whatever
/// order results arrive in.
#[derive(Debug, Default, Clone, Copy)]
pub struct StampFloor(u64);

impl StampFloor {
    /// Whether `stamp` is above the floor; raises the floor to it if so.
    pub fn accept(&mut self, stamp: u64) -> bool {
        let fresh = stamp > self.0;
        if fresh {
            self.0 = stamp;
        }
        fresh
    }

    /// Refuses every stamp up to and including `stamp`.
    pub fn reset(&mut self, stamp: u64) {
        self.0 = self.0.max(stamp);
    }
}

/// Buffers kept for reuse: one active request's worth plus the completed
/// slot's, two panes each plus the composite's sealed indices.
const POOLED_BUFFERS: usize = 6;

#[derive(Default)]
struct PreparerState {
    pending: Option<Vec<PaneJob>>,
    /// Backdrop reads, served only while `pending` is empty.
    pending_backdrop: Vec<PaneJob>,
    /// An exact read is running: set when the worker takes `pending`, cleared
    /// under the lock that stores its result.
    reading_exact: bool,
    done: Option<Vec<Prepared>>,
    /// Finished backdrop reads, one per pane at most; never replaces `done`.
    done_backdrop: Vec<Prepared>,
    pool: Vec<Vec<u16>>,
    shutdown: bool,
    spawned: bool,
}

impl PreparerState {
    fn pool_buffer(&mut self, buffer: Vec<u16>) {
        if self.pool.len() < POOLED_BUFFERS {
            self.pool.push(buffer);
        }
    }
}

struct PreparerShared {
    state: Mutex<PreparerState>,
    wakeup: Condvar,
}

impl PreparerShared {
    fn lock(&self) -> MutexGuard<'_, PreparerState> {
        self.state
            .lock()
            .expect("no code panics while holding the preparer lock")
    }
}

/// Reads panes' samples off the calling thread: one active request, one
/// replaceable newest pending request and one completed slot, all bounded.
/// Dropping it stops the worker after its current read without waiting for
/// it and without a wake.
pub struct Preparer {
    shared: Arc<PreparerShared>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Preparer {
    /// `wake` runs on the worker thread each time the completed slot goes
    /// from empty to full.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(PreparerShared {
                state: Mutex::new(PreparerState::default()),
                wakeup: Condvar::new(),
            }),
            wake: Arc::new(wake),
        }
    }

    /// Replaces the pending request with `jobs`; never blocks on a read.
    pub fn submit(&self, jobs: Vec<PaneJob>) {
        let mut state = self.shared.lock();
        state.pending = Some(jobs);
        self.start_worker(&mut state);
    }

    /// Queues low-priority backdrop reads: the worker takes them only while
    /// no exact request is pending. A job replaces a pending one for the
    /// same pane and leaves the other panes' alone; never blocks on a read.
    pub fn submit_backdrop(&self, jobs: Vec<PaneJob>) {
        let mut state = self.shared.lock();
        for job in jobs {
            state
                .pending_backdrop
                .retain(|queued| queued.pane != job.pane);
            state.pending_backdrop.push(job);
        }
        self.start_worker(&mut state);
    }

    fn start_worker(&self, state: &mut PreparerState) {
        if !state.spawned {
            state.spawned = true;
            let shared = self.shared.clone();
            let wake = self.wake.clone();
            std::thread::Builder::new()
                .name("canvas-prepare".into())
                .spawn(move || prepare_loop(&shared, &*wake))
                .expect("the OS starts the canvas preparation thread");
        }
        self.shared.wakeup.notify_one();
    }

    /// The newest completed exact request, then any completed backdrop
    /// reads, if some landed since the last call.
    pub fn take(&self) -> Option<Vec<Prepared>> {
        let mut state = self.shared.lock();
        let mut done = state.done.take().unwrap_or_default();
        done.append(&mut state.done_backdrop);
        (!done.is_empty()).then_some(done)
    }

    /// Whether no exact work is left: nothing queued, no exact read running
    /// and its result taken. Backdrop reads are ignored.
    pub fn exact_idle(&self) -> bool {
        let state = self.shared.lock();
        state.pending.is_none() && !state.reading_exact && state.done.is_none()
    }

    /// Hands consumed buffers back for the next request to read into.
    pub fn recycle(&self, spent: Vec<Prepared>) {
        let mut state = self.shared.lock();
        for prepared in spent {
            state.pool_buffer(prepared.samples);
            if prepared.indices.capacity() > 0 {
                state.pool_buffer(prepared.indices);
            }
        }
    }
}

impl Drop for Preparer {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wakeup.notify_all();
    }
}

fn prepare_loop(shared: &PreparerShared, wake: &(dyn Fn() + Send + Sync)) {
    loop {
        let (jobs, backdrop, mut buffers) = {
            let mut state = shared.lock();
            let (jobs, backdrop) = loop {
                if state.shutdown {
                    return;
                }
                if let Some(jobs) = state.pending.take() {
                    state.reading_exact = true;
                    break (jobs, false);
                }
                if !state.pending_backdrop.is_empty() {
                    break (std::mem::take(&mut state.pending_backdrop), true);
                }
                state = shared
                    .wakeup
                    .wait(state)
                    .expect("no code panics while holding the preparer lock");
            };
            let wanted: usize = jobs
                .iter()
                .map(|job| 1 + usize::from(job.indices.is_some()))
                .sum();
            let keep = state.pool.len().saturating_sub(wanted);
            let buffers = state.pool.split_off(keep);
            (jobs, backdrop, buffers)
        };
        let prepared: Vec<Prepared> = jobs
            .into_iter()
            .filter_map(|job| {
                let samples = buffers.pop().unwrap_or_default();
                let indices = if job.indices.is_some() {
                    buffers.pop().unwrap_or_default()
                } else {
                    Vec::new()
                };
                let _scope = backdrop.then(|| measurement_scope("gui_canvas_backdrop_read"));
                read_job(job, samples, indices)
            })
            .collect();

        let mut state = shared.lock();
        if !backdrop {
            state.reading_exact = false;
        }
        if state.shutdown {
            return;
        }
        if prepared.is_empty() {
            continue;
        }
        let was_empty = state.done.is_none() && state.done_backdrop.is_empty();
        let mut stale = Vec::new();
        if backdrop {
            for fresh in prepared {
                let parked = &mut state.done_backdrop;
                if let Some(at) = parked.iter().position(|p| p.job.pane == fresh.job.pane) {
                    stale.push(parked.swap_remove(at));
                }
                parked.push(fresh);
            }
        } else {
            stale.extend(state.done.replace(prepared).into_iter().flatten());
        }
        for spent in stale {
            state.pool_buffer(spent.samples);
            if spent.indices.capacity() > 0 {
                state.pool_buffer(spent.indices);
            }
        }
        drop(state);
        if was_empty {
            wake();
        }
    }
}

/// The rectangle common to `a` and `b`, or `None` when they do not overlap.
fn rect_intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    if x1 <= x0 || y1 <= y0 {
        None
    } else {
        Some(Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

/// One resident tile: its samples, and which texels hold an edited pixel.
#[derive(Clone)]
struct Tile {
    samples: Vec<u16>,
    filled: Vec<bool>,
}

impl Tile {
    fn empty(tile_size: u32, samples_per_pixel: usize) -> Self {
        let texels = tile_size as usize * tile_size as usize;
        Self {
            samples: vec![0u16; texels * samples_per_pixel],
            filled: vec![false; texels],
        }
    }
}

/// A resident cache of the edited image's samples over a fixed grid of
/// tiles, installed only where a stroke has touched them -- a tile is
/// allocated empty on first install, so a texel no install has covered
/// carries no sample at all ([`overlay_edited`] shows the sealed pixel
/// there). Memory is proportional to the edited area, never to the image.
#[derive(Clone)]
pub struct ResidentTiles {
    tile_size: u32,
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    tiles: HashMap<(u32, u32), Tile>,
    generation: u64,
    /// Every region installed since the last [`Self::take_rendered_regions`]
    /// -- a test's own window onto what actually redrew.
    rendered_regions: Vec<Rect>,
}

impl ResidentTiles {
    /// An empty cache over a `width` x `height` map, gridded at `tile_size`.
    pub fn new(tile_size: u32, width: u32, height: u32, samples_per_pixel: u32) -> Self {
        Self {
            tile_size: tile_size.max(1),
            width,
            height,
            samples_per_pixel,
            tiles: HashMap::new(),
            generation: 0,
            rendered_regions: Vec::new(),
        }
    }

    /// Bumped once per [`Self::install`] and per [`Self::touch`] -- the
    /// renderer's own cue that the resident cache or the ops drawn over it
    /// changed since the last frame it drew.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Bumps [`Self::generation`] without changing a pixel: the ops
    /// drawn over these tiles changed, so the overlay over them must redraw.
    pub fn touch(&mut self) {
        self.generation += 1;
    }

    /// Drains the log of every region installed since the last call.
    pub fn take_rendered_regions(&mut self) -> Vec<Rect> {
        std::mem::take(&mut self.rendered_regions)
    }

    /// The edited pixel at `(x, y)`, if an install has covered it --
    /// `None` outside every installed region.
    fn pixel_at(&self, x: u32, y: u32) -> Option<&[u16]> {
        let key = (x / self.tile_size, y / self.tile_size);
        let tile = self.tiles.get(&key)?;
        let local_x = (x % self.tile_size) as usize;
        let local_y = (y % self.tile_size) as usize;
        let texel = local_y * self.tile_size as usize + local_x;
        if !*tile.filled.get(texel)? {
            return None;
        }
        let spp = self.samples_per_pixel as usize;
        tile.samples.get(texel * spp..texel * spp + spp)
    }

    /// Every grid tile `region` meets, with its rect inside the map.
    fn grid(&self, region: Rect) -> Vec<((u32, u32), Rect)> {
        let ts = self.tile_size;
        let (x1, y1) = (
            (region.x + region.width - 1) / ts,
            (region.y + region.height - 1) / ts,
        );
        let mut out = Vec::new();
        for ty in region.y / ts..=y1 {
            for tx in region.x / ts..=x1 {
                let rect = Rect {
                    x: tx * ts,
                    y: ty * ts,
                    width: ts.min(self.width.saturating_sub(tx * ts)),
                    height: ts.min(self.height.saturating_sub(ty * ts)),
                };
                out.push(((tx, ty), rect));
            }
        }
        out
    }

    /// The rect of every grid tile any of `regions` meets, each once.
    pub fn tile_rects(&self, regions: &[Rect]) -> Vec<Rect> {
        let mut tiles = std::collections::BTreeMap::new();
        for &region in regions {
            tiles.extend(self.grid(region));
        }
        tiles.into_values().collect()
    }

    /// A copy holding only the tiles `regions` meet.
    pub fn subset(&self, regions: &[Rect]) -> Self {
        let mut out = Self::new(
            self.tile_size,
            self.width,
            self.height,
            self.samples_per_pixel,
        );
        for &region in regions {
            for (key, _) in self.grid(region) {
                if let Some(tile) = self.tiles.get(&key) {
                    out.tiles.entry(key).or_insert_with(|| tile.clone());
                }
            }
        }
        out
    }

    /// Copies each tile `region` meets from `from` -- empty where `from`
    /// holds none -- unless this cache already holds it, so the first call
    /// per tile fixes what that tile looked like.
    pub fn capture(&mut self, from: &Self, region: Rect) {
        let (ts, spp) = (self.tile_size, self.samples_per_pixel as usize);
        for (key, _) in self.grid(region) {
            self.tiles.entry(key).or_insert_with(|| {
                from.tiles
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| Tile::empty(ts, spp))
            });
        }
    }

    /// Drops every tile.
    pub fn clear(&mut self) {
        self.tiles.clear();
    }

    /// Copies the filled texels inside `region` onto `out`, `region`'s own
    /// row-major samples, and leaves every other texel as it is.
    pub fn overlay_into(&self, region: Rect, out: &mut [u16]) {
        let (ts, spp) = (self.tile_size as usize, self.samples_per_pixel as usize);
        for (key, tile_rect) in self.grid(region) {
            let (Some(tile), Some(part)) =
                (self.tiles.get(&key), rect_intersect(region, tile_rect))
            else {
                continue;
            };
            for y in part.y..part.y + part.height {
                for x in part.x..part.x + part.width {
                    let local = (y - tile_rect.y) as usize * ts + (x - tile_rect.x) as usize;
                    if !tile.filled[local] {
                        continue;
                    }
                    let at = ((y - region.y) as usize * region.width as usize
                        + (x - region.x) as usize)
                        * spp;
                    out[at..at + spp]
                        .copy_from_slice(&tile.samples[local * spp..(local + 1) * spp]);
                }
            }
        }
    }

    /// Splices `samples` -- `region`'s edited composite, row-major --
    /// into every grid tile `region` meets, allocating a tile empty on first
    /// touch and marking the texels filled. Bumps [`Self::generation`] once
    /// and logs `region`, regardless of how many tiles it spans.
    pub fn install(&mut self, region: Rect, samples: &[u16]) {
        let ts = self.tile_size;
        let spp = self.samples_per_pixel as usize;
        for (key, tile_rect) in self.grid(region) {
            let Some(part) = rect_intersect(region, tile_rect) else {
                continue;
            };
            let tile = self
                .tiles
                .entry(key)
                .or_insert_with(|| Tile::empty(ts, spp));
            let local_x0 = (part.x - tile_rect.x) as usize;
            let local_y0 = (part.y - tile_rect.y) as usize;
            let row_len = part.width as usize;
            for row in 0..part.height as usize {
                let src_row = (part.y - region.y) as usize + row;
                let src = (src_row * region.width as usize + (part.x - region.x) as usize) * spp;
                let texel = (local_y0 + row) * ts as usize + local_x0;
                tile.samples[texel * spp..(texel + row_len) * spp]
                    .copy_from_slice(&samples[src..src + row_len * spp]);
                tile.filled[texel..texel + row_len].fill(true);
            }
        }
        self.generation += 1;
        self.rendered_regions.push(region);
    }
}

/// The renderer's view of an open edit session: the resident cache of
/// edited composite samples.
#[derive(Clone, Copy)]
pub struct EditedView<'a> {
    pub tiles: &'a ResidentTiles,
}

/// The `Composite` pane's overlay draw parameters: the colour table, the
/// strokes drawn over the stored Z-Map's colours and the alpha
/// `display::overlay_cpu` blends with.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayDraw {
    pub table: Vec<[u8; 3]>,
    pub ops: Arc<[Op]>,
    pub alpha: u8,
}

impl OverlayDraw {
    /// The overlay's own palette table for `sources` frames, with nothing
    /// soloed, no strokes and `opacity`'s (`0.0..=1.0`) alpha.
    pub fn for_opacity(sources: u32, opacity: f32) -> Self {
        Self {
            table: overlay_table(sources, None),
            ops: Arc::from([]),
            alpha: alpha_from_opacity(opacity),
        }
    }
}

fn read_job(job: PaneJob, mut samples: Vec<u16>, mut indices: Vec<u16>) -> Option<Prepared> {
    match job
        .read_into(&mut samples)
        .and_then(|()| job.read_sealed_indices(&mut indices))
    {
        Ok(()) => Some(Prepared {
            job,
            samples,
            indices,
        }),
        Err(error) => {
            tracing::warn!(%error, pane = ?job.pane, "pane read failed");
            None
        }
    }
}

/// Overlays `samples` (already the sealed composite, decimated over `rect`
/// at `step`) with `tiles`' edited pixel at every texel it holds one for --
/// left as the sealed pixel everywhere else.
pub fn overlay_edited(
    samples: &mut [u16],
    rect: Rect,
    step: NonZeroU32,
    samples_per_pixel: u32,
    tiles: &ResidentTiles,
) {
    let step = step.get();
    let spp = samples_per_pixel as usize;
    let out_w = rect.width.div_ceil(step);
    let mut oy = 0u32;
    let mut sy = rect.y;
    while sy < rect.y + rect.height {
        let mut ox = 0u32;
        let mut sx = rect.x;
        while sx < rect.x + rect.width {
            let t = (oy * out_w + ox) as usize;
            if let Some(pixel) = tiles.pixel_at(sx, sy) {
                samples[t * spp..t * spp + spp].copy_from_slice(pixel);
            }
            ox += 1;
            sx += step;
        }
        oy += 1;
        sy += step;
    }
}
