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
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use focusforge_core::TiffError;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;
use focusforge_core::timing::{TimingCounter, record_timing};
use focusforge_core::zmap::{ZMapError, ZMapReader};

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
    /// Open `composite`/`zmap` as `tab`'s own planes.
    Load {
        tab: usize,
        composite: PathBuf,
        zmap: PathBuf,
    },
}

/// Which pane a piece of canvas state belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneSource {
    Composite,
    ZMap,
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

/// The one open source-frame handle the left canvas pane draws from -- never
/// more than one [`TiffStack`] mapped at once. Sixty-one frames held open at
/// the same time would map the whole reference stack, which is why
/// [`Self::set`] drops whatever was open before mapping the next.
#[derive(Default)]
pub struct FrameSource {
    open: Option<(PathBuf, Arc<TiffStack>)>,
}

impl FrameSource {
    /// Opens `path`, dropping whatever was open first. Nothing is left open
    /// on failure.
    pub fn set(&mut self, path: &Path) -> Result<(), TiffError> {
        self.open = None;
        let stack = TiffStack::open_files(std::slice::from_ref(&path.to_path_buf()))?;
        self.open = Some((path.to_path_buf(), Arc::new(stack)));
        Ok(())
    }

    /// Drops the open handle, if any.
    pub fn clear(&mut self) {
        self.open = None;
    }

    /// The currently open frame's own path, if any.
    pub fn path(&self) -> Option<&Path> {
        self.open.as_ref().map(|(path, _)| path.as_path())
    }

    /// The open frame's `(width, height)`, if any.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.open.as_ref().map(|(_, stack)| stack.dimensions())
    }

    /// The open frame's samples per pixel, if any.
    pub fn samples_per_pixel(&self) -> Option<u32> {
        self.open
            .as_ref()
            .map(|(_, stack)| stack.header(0).samples_per_pixel)
    }

    /// The open frame, shared with a preparation worker's reads.
    pub fn reader(&self) -> Option<Arc<TiffStack>> {
        self.open.as_ref().map(|(_, stack)| stack.clone())
    }

    /// Reads every `step`-th row and column of `rect` into `out`. Callers
    /// check [`Self::dimensions`] first, so a frame is always open by the
    /// time this runs.
    pub fn read_into(
        &self,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), TiffError> {
        let (_, stack) = self
            .open
            .as_ref()
            .expect("read_into is only called once a frame is confirmed open");
        stack.read_tile_step_into(0, rect, step, out)
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
    ZMap,
}

impl From<PaneSource> for Pane {
    fn from(plane: PaneSource) -> Self {
        match plane {
            PaneSource::Composite => Pane::Composite,
            PaneSource::ZMap => Pane::ZMap,
        }
    }
}

/// One pane's view to read: what to read, the stamp it was issued under and
/// what the display needs to render it.
#[derive(Clone)]
pub struct PaneJob {
    pub pane: Pane,
    pub draw: PaneDraw,
    pub stamp: u64,
    pub reader: Arc<dyn PaneReader>,
    /// Z-Map index scale for the display kernel; 0 for image sources.
    pub index_scale: u32,
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
}

/// A [`PaneJob`] with its samples read.
pub struct Prepared {
    pub job: PaneJob,
    pub samples: Vec<u16>,
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
/// slot's, two panes each.
const POOLED_BUFFERS: usize = 4;

#[derive(Default)]
struct PreparerState {
    pending: Option<Vec<PaneJob>>,
    done: Option<Vec<Prepared>>,
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

    /// The newest completed request, if one landed since the last call.
    pub fn take(&self) -> Option<Vec<Prepared>> {
        self.shared.lock().done.take()
    }

    /// Hands consumed buffers back for the next request to read into.
    pub fn recycle(&self, spent: Vec<Prepared>) {
        let mut state = self.shared.lock();
        for prepared in spent {
            state.pool_buffer(prepared.samples);
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
        let (jobs, mut buffers) = {
            let mut state = shared.lock();
            let jobs = loop {
                if state.shutdown {
                    return;
                }
                if let Some(jobs) = state.pending.take() {
                    break jobs;
                }
                state = shared
                    .wakeup
                    .wait(state)
                    .expect("no code panics while holding the preparer lock");
            };
            let keep = state.pool.len().saturating_sub(jobs.len());
            let buffers = state.pool.split_off(keep);
            (jobs, buffers)
        };
        let prepared: Vec<Prepared> = jobs
            .into_iter()
            .filter_map(|job| read_job(job, buffers.pop().unwrap_or_default()))
            .collect();

        let mut state = shared.lock();
        if state.shutdown {
            return;
        }
        if prepared.is_empty() {
            continue;
        }
        let was_empty = match state.done.replace(prepared) {
            Some(replaced) => {
                for stale in replaced {
                    state.pool_buffer(stale.samples);
                }
                false
            }
            None => true,
        };
        drop(state);
        if was_empty {
            wake();
        }
    }
}

fn read_job(job: PaneJob, mut samples: Vec<u16>) -> Option<Prepared> {
    match job.read_into(&mut samples) {
        Ok(()) => Some(Prepared { job, samples }),
        Err(error) => {
            tracing::warn!(%error, pane = ?job.pane, "pane read failed");
            None
        }
    }
}
