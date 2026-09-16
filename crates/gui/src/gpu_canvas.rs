// SPDX-License-Identifier: GPL-3.0-or-later

//! The wgpu half of the canvas: texture creation and
//! `slint::Image::try_from`, the only module in the crate naming `wgpu` or
//! `slint::wgpu_29` -- the way `src/bridge.rs` is the only one naming
//! `winit`. No `set_rendering_notifier` anywhere (slint-ui/slint#12030: the
//! `Manual` device path does not need one). `CanvasRenderer` reads pixels
//! tile-shaped off the mmap'd TIFF/Z-Map for whichever rect the viewport is
//! currently showing and re-uploads a *sub*-rect per gesture.

use std::path::{Path, PathBuf};

use focusforge_core::TiffError;
use focusforge_core::display::zmap_index_scale;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapError, ZMapReader};
use focusforge_gpu::device::SharedDevice;
use focusforge_gpu::display::{DisplayTexture, render};

use crate::canvas::TextureSpec;
use crate::viewport::PaneDraw;

/// One pane's wgpu-owned state: the display texture, and the `slint::Image`
/// wrapping it -- re-created only when the texture is (re)allocated
/// (`crate::canvas::needs_realloc`), never every frame (`try_from` takes
/// the texture by value; the spike set the image property once at setup
/// and never again).
#[derive(Default)]
pub struct PaneTexture {
    display_texture: Option<DisplayTexture>,
    spec: Option<TextureSpec>,
    image: Option<slint::Image>,
}

impl PaneTexture {
    /// The `slint::Image` to bind to `FFPane::source` -- a default (empty)
    /// image before the first render.
    pub fn image(&self) -> slint::Image {
        self.image.clone().unwrap_or_default()
    }

    fn ensure_and_render(
        &mut self,
        shared: &SharedDevice,
        width: u32,
        height: u32,
        src: &[u16],
        samples_per_pixel: u32,
        index_scale: u32,
    ) {
        let wanted = TextureSpec { width, height };
        if crate::canvas::needs_realloc(self.spec, wanted) {
            let texture = DisplayTexture::new(shared.device(), width, height);
            self.image = Some(
                slint::Image::try_from(texture.texture().clone())
                    .expect("display texture must satisfy slint's import contract"),
            );
            self.display_texture = Some(texture);
            self.spec = Some(wanted);
        }
        let texture = self
            .display_texture
            .as_ref()
            .expect("ensure_and_render always allocates before this point");
        render(shared, texture, src, samples_per_pixel, index_scale);
    }
}

/// What can go wrong opening the two files a [`CanvasRenderer`] holds open
/// for the life of the loaded project.
#[derive(Debug, thiserror::Error)]
pub enum CanvasRendererError {
    #[error("failed to open composite {}: {source}", path.display())]
    Composite {
        path: PathBuf,
        #[source]
        source: TiffError,
    },
    #[error("failed to open Z-Map {}: {source}", path.display())]
    ZMap {
        path: PathBuf,
        #[source]
        source: ZMapError,
    },
}

/// The two planes' GPU-owned state, the frame pane's own, and the last
/// [`PaneDraw`] rendered per pane -- `redraw_planes`/`redraw_frame` are
/// no-ops when nothing changed. Reads pixels tile-shaped off the mmap'd
/// TIFF/Z-Map/frame for whichever rect its own pane's viewport is currently
/// showing, re-uploading only the visible rect rather than the whole image.
/// The planes are optional: a tab with nothing sealed yet still gets a
/// renderer, for its frame pane alone.
pub struct CanvasRenderer {
    shared: &'static SharedDevice,
    composite: TiffStack,
    zmap: ZMapReader,
    composite_pane: PaneTexture,
    zmap_pane: PaneTexture,
    index_scale: u32,
    last: Option<PaneDraw>,
}

impl CanvasRenderer {
    /// Opens the composite and the Z-Map once, naming whichever fails by
    /// name rather than a generic I/O error.
    pub fn open(
        shared: &'static SharedDevice,
        composite_path: &Path,
        zmap_path: &Path,
    ) -> Result<Self, CanvasRendererError> {
        let composite = TiffStack::open_files(std::slice::from_ref(&composite_path.to_path_buf()))
            .map_err(|source| CanvasRendererError::Composite {
                path: composite_path.to_path_buf(),
                source,
            })?;
        let zmap = ZMapReader::open(zmap_path).map_err(|source| CanvasRendererError::ZMap {
            path: zmap_path.to_path_buf(),
            source,
        })?;
        let index_scale = zmap_index_scale(zmap.frame_count());
        Ok(Self {
            shared,
            composite,
            zmap,
            composite_pane: PaneTexture::default(),
            zmap_pane: PaneTexture::default(),
            index_scale,
            last: None,
        })
    }

    /// Re-reads and re-uploads `draw.src` for both panes, returning the two
    /// `slint::Image`s to bind to `FFPane::source`. A no-op (besides
    /// re-cloning the two already-current images) when `draw` is exactly
    /// what was last drawn -- a gesture that changes nothing costs nothing.
    pub fn redraw(&mut self, draw: &PaneDraw) -> (slint::Image, slint::Image) {
        if self.last.as_ref() == Some(draw) {
            return (self.composite_pane.image(), self.zmap_pane.image());
        }

        let (x, y, width, height) = draw.src;
        let rect = Rect {
            x,
            y,
            width,
            height,
        };

        let samples_per_pixel = self.composite.header(0).samples_per_pixel;
        let mut samples = vec![0u16; (width * height * samples_per_pixel) as usize];
        self.composite
            .read_tile_into(0, rect, &mut samples)
            .expect("read composite tile for display");
        self.composite_pane.ensure_and_render(
            self.shared,
            width,
            height,
            &samples,
            samples_per_pixel,
            0,
        );

        let mut indices = vec![0u16; (width * height) as usize];
        self.zmap
            .read_tile_into(rect, &mut indices)
            .expect("read zmap tile for display");
        self.zmap_pane
            .ensure_and_render(self.shared, width, height, &indices, 1, self.index_scale);

        self.last = Some(*draw);
        (self.composite_pane.image(), self.zmap_pane.image())
    }
}
