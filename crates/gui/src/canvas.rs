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

use focusforge_core::TiffError;
use focusforge_core::io::TiffStack;
use focusforge_core::retouch::{RetouchError, Sources, recomposite as recomposite_region};
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

/// Copies `rect`'s rows out of `indices`, a single-sample-per-pixel plane
/// `map_width` wide -- the same row-major layout an [`EditSession`]'s own
/// indices use.
///
/// [`EditSession`]: focusforge_core::retouch::EditSession
fn gather_rect(indices: &[u16], map_width: u32, rect: Rect) -> Vec<u16> {
    let mut out = Vec::with_capacity(rect.width as usize * rect.height as usize);
    for y in rect.y..rect.y + rect.height {
        let start = y as usize * map_width as usize + rect.x as usize;
        out.extend_from_slice(&indices[start..start + rect.width as usize]);
    }
    out
}

/// A resident cache of recomposited pixels over a fixed grid of tiles,
/// gathered only where a stroke has touched them -- a tile is allocated
/// zeroed on first touch and never seeded from the sealed composite, so a
/// pixel outside every touched tile carries no gathered sample at all
/// ([`overlay_edited`] falls back to the sealed pixel there). Memory is
/// proportional to the edited area, never to the image.
pub struct ResidentTiles {
    tile_size: u32,
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    tiles: HashMap<(u32, u32), Vec<u16>>,
    generation: u64,
    /// Every region recomposited since the last [`Self::take_recomposited`]
    /// -- a test's own window onto what actually redrew.
    recomposited: Vec<Rect>,
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
            recomposited: Vec::new(),
        }
    }

    /// Bumped once per [`Self::recomposite`] call -- the renderer's own cue
    /// that the resident cache changed since the last frame it drew.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Drains the log of every region recomposited since the last call.
    pub fn take_recomposited(&mut self) -> Vec<Rect> {
        std::mem::take(&mut self.recomposited)
    }

    /// The gathered pixel at `(x, y)`, if its tile has ever been touched --
    /// `None` outside every touched tile.
    fn pixel_at(&self, x: u32, y: u32) -> Option<&[u16]> {
        let key = (x / self.tile_size, y / self.tile_size);
        let tile = self.tiles.get(&key)?;
        let local_x = (x % self.tile_size) as usize;
        let local_y = (y % self.tile_size) as usize;
        let spp = self.samples_per_pixel as usize;
        let idx = (local_y * self.tile_size as usize + local_x) * spp;
        tile.get(idx..idx + spp)
    }

    /// Gathers `region`'s composite from the whole map's `indices` into
    /// every grid tile it meets, allocating a tile zeroed on first touch.
    /// Bumps [`Self::generation`] once and logs `region`, regardless of how
    /// many tiles it spans.
    pub fn recomposite(
        &mut self,
        region: Rect,
        indices: &[u16],
        sources: &Sources,
    ) -> Result<(), RetouchError> {
        let ts = self.tile_size;
        let spp = self.samples_per_pixel as usize;
        let x0 = region.x / ts;
        let y0 = region.y / ts;
        let x1 = (region.x + region.width - 1) / ts;
        let y1 = (region.y + region.height - 1) / ts;
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                let tile_rect = Rect {
                    x: tx * ts,
                    y: ty * ts,
                    width: ts.min(self.width.saturating_sub(tx * ts)),
                    height: ts.min(self.height.saturating_sub(ty * ts)),
                };
                let Some(part) = rect_intersect(region, tile_rect) else {
                    continue;
                };
                let part_indices = gather_rect(indices, self.width, part);
                let mut out = vec![0u16; part.width as usize * part.height as usize * spp];
                recomposite_region(&part_indices, part, sources, &mut out)?;
                let tile = self
                    .tiles
                    .entry((tx, ty))
                    .or_insert_with(|| vec![0u16; ts as usize * ts as usize * spp]);
                let local_x0 = (part.x - tile_rect.x) as usize;
                let local_y0 = (part.y - tile_rect.y) as usize;
                for row in 0..part.height as usize {
                    let src = row * part.width as usize * spp;
                    let dst = ((local_y0 + row) * ts as usize + local_x0) * spp;
                    tile[dst..dst + part.width as usize * spp]
                        .copy_from_slice(&out[src..src + part.width as usize * spp]);
                }
            }
        }
        self.generation += 1;
        self.recomposited.push(region);
        Ok(())
    }
}

/// The renderer's view of an open edit session: the whole map's edited
/// indices, the map's width (for indexing them), and the resident cache
/// [`ResidentTiles::recomposite`] filled.
pub struct EditedView<'a> {
    pub indices: &'a [u16],
    pub map_width: u32,
    pub tiles: &'a ResidentTiles,
}

/// Reads every `step`-th row and column of `rect` from an in-memory,
/// single-sample-per-pixel `plane` (row-major, `plane_width` wide) into
/// `out` -- the same decimation [`TiffStack::read_tile_step_into`] applies
/// to an mmap'd file, applied here to a resident buffer instead.
pub fn step_copy(plane: &[u16], plane_width: u32, rect: Rect, step: NonZeroU32, out: &mut [u16]) {
    let step = step.get();
    let out_w = rect.width.div_ceil(step);
    let mut oy = 0u32;
    let mut sy = rect.y;
    while sy < rect.y + rect.height {
        let mut ox = 0u32;
        let mut sx = rect.x;
        while sx < rect.x + rect.width {
            let src = sy as usize * plane_width as usize + sx as usize;
            let dst = (oy * out_w + ox) as usize;
            out[dst] = plane[src];
            ox += 1;
            sx += step;
        }
        oy += 1;
        sy += step;
    }
}

/// Overlays `samples` (already the sealed composite, decimated over `rect`
/// at `step`) with `tiles`' gathered pixel at every texel whose edited index
/// (`edited_indices`, the same decimation) differs from its sealed index
/// (`sealed_indices`) -- left as the sealed pixel everywhere else, including
/// a texel whose tile was never touched.
pub fn overlay_edited(
    samples: &mut [u16],
    sealed_indices: &[u16],
    edited_indices: &[u16],
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
            if sealed_indices[t] != edited_indices[t]
                && let Some(pixel) = tiles.pixel_at(sx, sy)
            {
                samples[t * spp..t * spp + spp].copy_from_slice(pixel);
            }
            ox += 1;
            sx += step;
        }
        oy += 1;
        sy += step;
    }
}
