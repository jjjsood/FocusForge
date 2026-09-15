// SPDX-License-Identifier: GPL-3.0-or-later

//! Canvas state as plain Rust: no `use slint::` and no `use wgpu::`
//! anywhere in this file. The `AppController` methods this state backs
//! live in
//! `crates/gui/src/controller.rs` (so they can reach `ShellState`'s private
//! field); this module owns only the types and the pure functions. The
//! wgpu half (texture creation, `slint::Image::try_from`) is
//! `crates/gui/src/gpu_canvas.rs`, the only module in the crate naming
//! `wgpu` or `slint::wgpu_29`.

use std::path::{Path, PathBuf};

use focusforge_core::tile::Rect;

use crate::controller::AppController;
use crate::project_stub::{ProjectStub, ProjectStubError};

/// Which pane a piece of canvas state belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneSource {
    Composite,
    ZMap,
}

/// The visible region a pane uploads to the GPU: the fixture currently used
/// fits entirely in view, so `rect` is the full extent; a viewport that
/// scrolls past the fixture moves it and pays a re-upload per step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub rect: Rect,
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

/// What can go wrong opening a project or validating the display format,
/// named rather than left as a panic or a stringly-typed error.
#[derive(Debug, thiserror::Error)]
pub enum CanvasError {
    #[error("unsupported display format {got:?}, only \"Rgba8Unorm\" is supported")]
    UnsupportedDisplayFormat { got: String },
    #[error(transparent)]
    Project(#[from] ProjectStubError),
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

/// What `AppController` knows about the loaded project's canvas, once
/// `AppController::open_project` has succeeded. `Default` so `ShellState`
/// (`controller.rs`) can keep deriving it.
#[derive(Debug, Clone, Default)]
pub struct CanvasState {
    pub(crate) project: Option<ProjectStub>,
}

impl AppController {
    /// Opens `path` as a project directory (`crate::project_stub::ProjectStub`
    /// -- a stub project format), naming a
    /// missing composite or Z-Map file by name rather than a generic I/O
    /// error, and refusing a composite/Z-Map size mismatch by name too
    /// ("one viewport over two panes" is enforced, not
    /// assumed). Sets the controller's image size and clears any prior
    /// viewport; the next `set_pane_geometry` call fits it (`open_project`
    /// is never handed a pane rect of its own).
    pub fn open_project(&mut self, path: &Path) -> Result<(), CanvasError> {
        let project = ProjectStub::open(path)?;
        let composite = project.dimensions(PaneSource::Composite);
        let zmap = project.dimensions(PaneSource::ZMap);
        if composite != zmap {
            return Err(CanvasError::PaneDimensionsDiffer { composite, zmap });
        }
        self.canvas_mut().project = Some(project);
        self.set_project_image(crate::viewport::ImageSize {
            width: composite.0,
            height: composite.1,
        });
        Ok(())
    }

    /// The clamped visible rect the current viewport is showing for `pane`
    pub fn viewport(&self, pane: PaneSource) -> Option<Viewport> {
        let project = self.canvas().project.as_ref()?;
        if let Some(draw) = self.pane_draw() {
            let (x, y, width, height) = draw.src;
            return Some(Viewport {
                rect: Rect {
                    x,
                    y,
                    width,
                    height,
                },
            });
        }
        let (width, height) = project.dimensions(pane);
        Some(Viewport {
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        })
    }

    /// The composite/Z-Map file paths a loaded project resolved, for
    /// `crate::gpu_canvas` to read tiles from.
    pub fn project_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.canvas().project.as_ref().map(|p| {
            (
                p.composite_path().to_path_buf(),
                p.zmap_path().to_path_buf(),
            )
        })
    }
}
