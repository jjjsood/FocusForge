// SPDX-License-Identifier: GPL-3.0-or-later

//! The wgpu half of the canvas: texture creation and
//! `slint::Image::try_from`, the only module in the crate naming `wgpu` or
//! `slint::wgpu_29` -- the way `src/bridge.rs` is the only one naming
//! `winit`. No `set_rendering_notifier` anywhere (slint-ui/slint#12030:
//! the `Manual` device path does not need one). `CanvasRenderer`
//! reads pixels tile-shaped off the mmap'd TIFF/Z-Map for whichever rect the
//! viewport is currently showing and re-uploads on every change: a
//! *sub*-rect upload per gesture.

use std::path::Path;

use focusforge_core::display::zmap_index_scale;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapReader;
use focusforge_gpu::device::SharedDevice;
use focusforge_gpu::display::{DisplayTexture, render};

use crate::canvas::TextureSpec;

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

/// Reads the composite's full extent off the mmap'd TIFF
/// (`focusforge_core::io::TiffStack::read_tile_into`) and renders it into
/// `pane`'s texture.
pub fn render_composite(shared: &SharedDevice, pane: &mut PaneTexture, composite_path: &Path) {
    let stack =
        TiffStack::open_files(&[composite_path.to_path_buf()]).expect("open composite for display");
    let (width, height) = stack.dimensions();
    let samples_per_pixel = stack.header(0).samples_per_pixel;
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut samples = vec![0u16; (width * height * samples_per_pixel) as usize];
    stack
        .read_tile_into(0, rect, &mut samples)
        .expect("read composite tile for display");
    pane.ensure_and_render(shared, width, height, &samples, samples_per_pixel, 0);
}

/// Reads the Z-Map's full extent
/// (`focusforge_core::zmap::ZMapReader::read_tile_into`, which also
/// validates every index against the map's own frame count) and renders it
/// into `pane`'s texture.
pub fn render_zmap(shared: &SharedDevice, pane: &mut PaneTexture, zmap_path: &Path) {
    let reader = ZMapReader::open(zmap_path).expect("open zmap for display");
    let (width, height) = reader.dimensions();
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut indices = vec![0u16; (width * height) as usize];
    reader
        .read_tile_into(rect, &mut indices)
        .expect("read zmap tile for display");
    let index_scale = zmap_index_scale(reader.frame_count());
    pane.ensure_and_render(shared, width, height, &indices, 1, index_scale);
}
