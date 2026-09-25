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

use focusforge_core::TiffError;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;

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
    open: Option<(PathBuf, TiffStack)>,
}

impl FrameSource {
    /// Opens `path`, dropping whatever was open first. Nothing is left open
    /// on failure.
    pub fn set(&mut self, path: &Path) -> Result<(), TiffError> {
        self.open = None;
        let stack = TiffStack::open_files(std::slice::from_ref(&path.to_path_buf()))?;
        self.open = Some((path.to_path_buf(), stack));
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
