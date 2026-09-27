// SPDX-License-Identifier: GPL-3.0-or-later

//! The wgpu half of the canvas: texture creation and
//! `slint::Image::try_from`, the only module in the crate naming `wgpu` or
//! `slint::wgpu_29` -- the way `src/bridge.rs` is the only one naming
//! `winit`. `CanvasRenderer` reads pixels
//! tile-shaped off the mmap'd TIFF/Z-Map for whichever rect the viewport is
//! currently showing and re-uploads a *sub*-rect per gesture.

use std::path::{Path, PathBuf};

use focusforge_core::TiffError;
use focusforge_core::display::zmap_index_scale;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapError, ZMapReader};
use focusforge_gpu::display::{DisplayTexture, render};
use slint::wgpu_29::wgpu;

use crate::canvas::TextureSpec;
use crate::viewport::PaneDraw;

/// One pane's wgpu-owned state: the display texture, and the `slint::Image`
/// wrapping it -- re-created only when the texture is (re)allocated
/// (`crate::canvas::needs_realloc`), never every frame: `try_from` takes
/// the texture by value, so the image property is set once at setup and
/// never again.
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

    /// The single point where both a (re)allocation and a render happen, so
    /// one early return covers both halves of a lost device: no new
    /// `DisplayTexture` and no `render` call once the device is known gone.
    #[allow(clippy::too_many_arguments)]
    fn ensure_and_render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        src: &[u16],
        samples_per_pixel: u32,
        index_scale: u32,
    ) {
        if focusforge_gpu::errors::device_lost() {
            return;
        }
        let wanted = TextureSpec { width, height };
        if crate::canvas::needs_realloc(self.spec, wanted) {
            let texture = DisplayTexture::new(device, width, height);
            let imported = slint::Image::try_from(texture.texture().clone());
            self.apply_import(imported, texture, wanted);
        }
        let Some(texture) = self.display_texture.as_ref() else {
            // The only newly (re)allocated texture failed import: nothing to
            // render into this frame, and the previous image is kept as-is.
            return;
        };
        render(device, queue, texture, src, samples_per_pixel, index_scale);
    }

    /// Applies the outcome of importing a newly (re)allocated texture. On
    /// success the pane's texture, image and spec all advance together; on
    /// failure the previous image is kept and only `wanted` is recorded, so
    /// the same failing size is not retried on the next redraw.
    fn apply_import(
        &mut self,
        imported: Result<slint::Image, slint::wgpu_29::TextureImportError>,
        texture: DisplayTexture,
        wanted: TextureSpec,
    ) {
        match imported {
            Ok(image) => {
                self.image = Some(image);
                self.display_texture = Some(texture);
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    width = wanted.width,
                    height = wanted.height,
                    "display texture import failed; keeping the previous frame"
                );
            }
        }
        self.spec = Some(wanted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn golden_plane_files(fixtures: &std::path::Path) -> (PathBuf, PathBuf) {
        let mut composite = None;
        let mut zmap = None;
        for entry in
            std::fs::read_dir(fixtures.join("mantis-512/golden")).expect("golden directory opens")
        {
            let path = entry.expect("golden directory entry reads").path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("tif") {
                continue;
            }
            if ZMapReader::open(&path).is_ok() {
                assert!(zmap.replace(path).is_none(), "one Z-Map golden is required");
            } else {
                assert!(
                    composite.replace(path).is_none(),
                    "one composite golden is required"
                );
            }
        }
        (
            composite.expect("composite golden exists"),
            zmap.expect("Z-Map golden exists"),
        )
    }

    #[test]
    fn import_failure_keeps_the_previous_image_and_records_the_spec() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let mut pane = PaneTexture::default();
        let texture = DisplayTexture::new(shared.device(), 4, 4);
        let wanted = crate::canvas::TextureSpec {
            width: 4,
            height: 4,
        };

        pane.apply_import(
            Err(slint::wgpu_29::TextureImportError::InvalidFormat),
            texture,
            wanted,
        );

        assert!(
            pane.display_texture.is_none(),
            "a failed import must not store the texture it was given"
        );
        assert_eq!(
            pane.spec,
            Some(wanted),
            "the failing size must still be recorded"
        );
        assert!(
            !crate::canvas::needs_realloc(pane.spec, wanted),
            "the same failing size must not be retried on the next redraw"
        );
    }

    #[test]
    fn lifecycle_keeps_readers_and_rebuilds_gpu_draws() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        let frame = fixtures.join("mantis-64/frames/00.tif");
        let (composite, zmap) = golden_plane_files(&fixtures);
        let draw = PaneDraw {
            src: (0, 0, 64, 64),
            dst: (0.0, 0.0, 64.0, 64.0),
            pixelated: false,
            step: NonZeroU32::MIN,
        };
        let shared = focusforge_gpu::device::shared_device_or_skip()
            .expect("this GPU lifecycle test requires a device");

        let mut empty = CanvasRenderer::new();
        empty.attach_device(shared.device().clone(), shared.queue().clone());
        empty.detach_device();
        empty.detach_device();
        assert!(empty.device_queue().is_none());

        let mut frame_only = CanvasRenderer::new();
        frame_only.set_frame(&frame).expect("fixture frame opens");
        assert!(frame_only.redraw_frame(&draw).is_none());
        frame_only.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(frame_only.redraw_frame(&draw).is_some());
        frame_only.detach_device();
        frame_only.detach_device();
        assert_eq!(frame_only.frame_path(), Some(frame.as_path()));
        assert!(frame_only.frame_pane.display_texture.is_none());
        assert!(frame_only.frame_pane.image.is_none());
        assert!(frame_only.last_frame.is_none());
        frame_only.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(frame_only.redraw_frame(&draw).is_some());

        let mut planes_and_frame = CanvasRenderer::new();
        planes_and_frame
            .set_frame(&frame)
            .expect("fixture frame opens");
        planes_and_frame
            .set_planes(&composite, &zmap)
            .expect("fixture planes open");
        assert!(planes_and_frame.redraw_frame(&draw).is_none());
        assert!(planes_and_frame.redraw_planes(&draw).is_none());
        planes_and_frame.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(planes_and_frame.redraw_frame(&draw).is_some());
        assert!(planes_and_frame.redraw_planes(&draw).is_some());
        planes_and_frame.detach_device();
        planes_and_frame.detach_device();
        assert_eq!(planes_and_frame.frame_path(), Some(frame.as_path()));
        assert!(planes_and_frame.composite.is_some());
        assert!(planes_and_frame.zmap.is_some());
        assert!(planes_and_frame.composite_pane.display_texture.is_none());
        assert!(planes_and_frame.composite_pane.image.is_none());
        assert!(planes_and_frame.zmap_pane.display_texture.is_none());
        assert!(planes_and_frame.zmap_pane.image.is_none());
        assert!(planes_and_frame.frame_pane.display_texture.is_none());
        assert!(planes_and_frame.frame_pane.image.is_none());
        assert!(planes_and_frame.last_planes.is_none());
        assert!(planes_and_frame.last_frame.is_none());
        planes_and_frame.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(planes_and_frame.redraw_frame(&draw).is_some());
        assert!(planes_and_frame.redraw_planes(&draw).is_some());
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
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    composite: Option<TiffStack>,
    zmap: Option<ZMapReader>,
    composite_pane: PaneTexture,
    zmap_pane: PaneTexture,
    index_scale: Option<u32>,
    last_planes: Option<PaneDraw>,
    frame: crate::canvas::FrameSource,
    frame_pane: PaneTexture,
    last_frame: Option<PaneDraw>,
}

impl Default for CanvasRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl CanvasRenderer {
    /// A renderer with nothing open yet.
    pub fn new() -> Self {
        Self {
            device: None,
            queue: None,
            composite: None,
            zmap: None,
            composite_pane: PaneTexture::default(),
            zmap_pane: PaneTexture::default(),
            index_scale: None,
            last_planes: None,
            frame: crate::canvas::FrameSource::default(),
            frame_pane: PaneTexture::default(),
            last_frame: None,
        }
    }

    /// Uses Slint's surface-compatible device for future canvas draws.
    pub fn attach_device(&mut self, device: wgpu::Device, queue: wgpu::Queue) {
        self.detach_device();
        self.device = Some(device);
        self.queue = Some(queue);
    }

    /// Releases resources bound to Slint's device while keeping source readers.
    pub fn detach_device(&mut self) {
        self.composite_pane = PaneTexture::default();
        self.zmap_pane = PaneTexture::default();
        self.frame_pane = PaneTexture::default();
        self.last_planes = None;
        self.last_frame = None;
        self.device = None;
        self.queue = None;
    }

    fn device_queue(&self) -> Option<(wgpu::Device, wgpu::Queue)> {
        Some((self.device.as_ref()?.clone(), self.queue.as_ref()?.clone()))
    }

    /// Opens `composite`/`zmap` as this renderer's planes, replacing
    /// whichever pair was open before, naming whichever file fails by name
    /// rather than a generic I/O error. Atomic: a failed open leaves the
    /// previous pair, if any, untouched.
    pub fn set_planes(
        &mut self,
        composite_path: &Path,
        zmap_path: &Path,
    ) -> Result<(), CanvasRendererError> {
        let composite = TiffStack::open_files(std::slice::from_ref(&composite_path.to_path_buf()))
            .map_err(|source| CanvasRendererError::Composite {
                path: composite_path.to_path_buf(),
                source,
            })?;
        let zmap = ZMapReader::open(zmap_path).map_err(|source| CanvasRendererError::ZMap {
            path: zmap_path.to_path_buf(),
            source,
        })?;
        self.index_scale = Some(zmap_index_scale(zmap.frame_count()));
        self.composite = Some(composite);
        self.zmap = Some(zmap);
        self.composite_pane = PaneTexture::default();
        self.zmap_pane = PaneTexture::default();
        self.last_planes = None;
        Ok(())
    }

    /// Drops the open planes, leaving the frame half untouched.
    pub fn clear_planes(&mut self) {
        self.composite = None;
        self.zmap = None;
        self.index_scale = None;
        self.composite_pane = PaneTexture::default();
        self.zmap_pane = PaneTexture::default();
        self.last_planes = None;
    }

    /// The frame pane's currently open path, if any.
    pub fn frame_path(&self) -> Option<&Path> {
        self.frame.path()
    }

    /// Opens `path` as the frame pane's source, dropping whatever was open
    /// first. `Err` leaves nothing open.
    pub fn set_frame(&mut self, path: &Path) -> Result<(), TiffError> {
        let result = self.frame.set(path);
        self.frame_pane = PaneTexture::default();
        self.last_frame = None;
        result
    }

    /// Drops the open frame, if any.
    pub fn clear_frame(&mut self) {
        self.frame.clear();
        self.frame_pane = PaneTexture::default();
        self.last_frame = None;
    }

    /// Re-reads and re-uploads `draw.src` for both planes, every `draw.step`-th
    /// row and column into a `draw.extent()`-sized texture, returning the two
    /// `slint::Image`s to bind to `FFPane::source`. A no-op (besides
    /// re-cloning the two already-current images) when `draw` is exactly
    /// what was last drawn -- a gesture that changes nothing costs nothing.
    /// `None` with no planes open, when `draw.src` falls outside the open
    /// composite, or either read fails; `self.last_planes` stays untouched
    /// on that path so a later, valid rect still renders.
    pub fn redraw_planes(&mut self, draw: &PaneDraw) -> Option<(slint::Image, slint::Image)> {
        let (device, queue) = self.device_queue()?;
        if self.composite.is_none() || self.zmap.is_none() {
            return None;
        }
        if self.last_planes.as_ref() == Some(draw) {
            return Some((self.composite_pane.image(), self.zmap_pane.image()));
        }
        let composite = self.composite.as_ref()?;
        let zmap = self.zmap.as_ref()?;
        let index_scale = self.index_scale?;

        let (x, y, width, height) = draw.src;
        let (frame_size, samples_per_pixel) = {
            let header = composite.header(0);
            ((header.width, header.height), header.samples_per_pixel)
        };
        if !crate::canvas::draw_rect_fits(frame_size, draw.src) {
            return None;
        }
        let rect = Rect {
            x,
            y,
            width,
            height,
        };

        let (tex_w, tex_h) = draw.extent();
        let texels = tex_w as usize * tex_h as usize;

        let mut samples = vec![0u16; texels * samples_per_pixel as usize];
        composite
            .read_tile_step_into(0, rect, draw.step, &mut samples)
            .ok()?;
        self.composite_pane.ensure_and_render(
            &device,
            &queue,
            tex_w,
            tex_h,
            &samples,
            samples_per_pixel,
            0,
        );

        let mut indices = vec![0u16; texels];
        zmap.read_tile_step_into(rect, draw.step, &mut indices)
            .ok()?;
        self.zmap_pane
            .ensure_and_render(&device, &queue, tex_w, tex_h, &indices, 1, index_scale);

        self.last_planes = Some(*draw);
        Some((self.composite_pane.image(), self.zmap_pane.image()))
    }

    /// Re-reads and re-uploads `draw.src` for the frame pane, decimated as in
    /// [`Self::redraw_planes`], returning the
    /// `slint::Image` to bind to `FFPane::source`. `None` with no frame
    /// open, when `draw.src` falls outside it, or the read fails;
    /// `self.last_frame` stays untouched on that path so a later, valid
    /// rect still renders. The same display path [`Self::redraw_planes`]
    /// uses, at an index scale of 0 -- a source frame carries no Z-Map
    /// indices.
    pub fn redraw_frame(&mut self, draw: &PaneDraw) -> Option<slint::Image> {
        let (device, queue) = self.device_queue()?;
        let frame_size = self.frame.dimensions()?;
        let samples_per_pixel = self.frame.samples_per_pixel()?;
        if self.last_frame.as_ref() == Some(draw) {
            return Some(self.frame_pane.image());
        }
        if !crate::canvas::draw_rect_fits(frame_size, draw.src) {
            return None;
        }
        let (x, y, width, height) = draw.src;
        let rect = Rect {
            x,
            y,
            width,
            height,
        };

        let (tex_w, tex_h) = draw.extent();
        let mut samples = vec![0u16; tex_w as usize * tex_h as usize * samples_per_pixel as usize];
        self.frame.read_into(rect, draw.step, &mut samples).ok()?;
        self.frame_pane.ensure_and_render(
            &device,
            &queue,
            tex_w,
            tex_h,
            &samples,
            samples_per_pixel,
            0,
        );

        self.last_frame = Some(*draw);
        Some(self.frame_pane.image())
    }
}
