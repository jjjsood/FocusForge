// SPDX-License-Identifier: GPL-3.0-or-later

//! The wgpu half of the canvas: texture creation and
//! `slint::Image::try_from`, the only module in the crate naming `wgpu` or
//! `slint::wgpu_29` -- the way `src/bridge.rs` is the only one naming
//! `winit`. `CanvasRenderer` reads pixels
//! tile-shaped off the mmap'd TIFF/Z-Map for whichever rect the viewport is
//! currently showing and re-uploads a *sub*-rect per gesture.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use focusforge_core::timing::measurement_scope;

use focusforge_core::TiffError;
use focusforge_core::io::{FrameError, TiffStack};
use focusforge_core::retouch::overlay_colors;
use focusforge_core::tile::{HostBudget, Rect};
use focusforge_core::zmap::ZMapReader;
use focusforge_gpu::display::{DisplayRenderer, DisplayTexture, render_overlay};
use slint::wgpu_29::wgpu;

use crate::canvas::{
    EditState, EditedView, FrameView, OverlayDraw, Pane, PaneJob, PaneReader, StampFloor,
    TextureSpec,
};
use crate::viewport::{ImageSize, PaneDraw};

/// The exact content a [`PaneTexture`] was last drawn from: `draw.src`,
/// `draw.step` and the edit state, never `dst`/`pixelated` (placement,
/// applied by the caller regardless of whether this pane actually
/// re-rendered).
type Coverage = ((u32, u32, u32, u32), NonZeroU32, EditState);

fn rect_contains(outer: (u32, u32, u32, u32), inner: (u32, u32, u32, u32)) -> bool {
    let end = |pos: u32, len: u32| u64::from(pos) + u64::from(len);
    outer.0 <= inner.0
        && outer.1 <= inner.1
        && end(outer.0, outer.2) >= end(inner.0, inner.2)
        && end(outer.1, outer.3) >= end(inner.1, inner.3)
}

fn visible_draw_fits(source_size: (u32, u32), draw: &PaneDraw) -> bool {
    crate::canvas::draw_rect_fits(source_size, draw.src)
}

/// One pane's wgpu-owned state: the display texture, the `slint::Image`
/// wrapping it -- re-created only when the texture is (re)allocated
/// (`crate::canvas::needs_realloc`), never every frame: `try_from` takes
/// the texture by value, so the image property is set once at setup and
/// never again -- and the content coverage it was last drawn from, plus the
/// stamp floor that refuses results issued before its content last changed.
#[derive(Default)]
pub struct PaneTexture {
    display_texture: Option<DisplayTexture>,
    spec: Option<TextureSpec>,
    image: Option<slint::Image>,
    coverage: Option<Coverage>,
    /// The content last requested for a backdrop texture, so one still in
    /// flight is not requested again; unused by exact textures.
    requested: Option<Coverage>,
    floor: StampFloor,
}

impl PaneTexture {
    /// An empty pane that refuses every stamp up to `stamp`.
    fn fresh(stamp: u64) -> Self {
        let mut pane = Self::default();
        pane.floor.reset(stamp);
        pane
    }

    /// The `slint::Image` to bind to `FFPane::source` -- a default (empty)
    /// image before the first render.
    pub fn image(&self) -> slint::Image {
        self.image.clone().unwrap_or_default()
    }

    /// Whether this pane's retained source rect contains `src` at the same
    /// sampling step and edit state.
    fn covers(&self, src: (u32, u32, u32, u32), step: NonZeroU32, state: &EditState) -> bool {
        self.coverage
            .as_ref()
            .is_some_and(|(s, t, e)| rect_contains(*s, src) && *t == step && e == state)
    }

    /// Whether a backdrop read of exactly this content was already issued.
    fn requested(&self, src: (u32, u32, u32, u32), step: NonZeroU32, state: &EditState) -> bool {
        self.requested
            .as_ref()
            .is_some_and(|(s, t, e)| *s == src && *t == step && e == state)
    }

    fn mark_covered(&mut self, src: (u32, u32, u32, u32), step: NonZeroU32, state: EditState) {
        self.coverage = Some((src, step, state));
    }

    /// (Re)allocates this pane's texture for `width` x `height` if needed,
    /// importing it as the `slint::Image` [`Self::image`] returns -- the
    /// allocation half [`Self::ensure_and_render`] and
    /// [`Self::ensure_and_render_overlay`] both share. `None` once the
    /// device is known gone, or when a freshly (re)allocated texture's
    /// import fails; the previous image is kept either way.
    fn ensure_texture(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Option<&DisplayTexture> {
        if focusforge_gpu::errors::device_lost() {
            return None;
        }
        let wanted = TextureSpec { width, height };
        if crate::canvas::needs_realloc(self.spec, wanted) {
            let texture = {
                let _scope = measurement_scope("gui_display_texture_allocate");
                DisplayTexture::new(device, width, height)
            };
            let imported = {
                let _scope = measurement_scope("gui_display_texture_import");
                slint::Image::try_from(texture.texture().clone())
            };
            self.apply_import(imported, texture, wanted);
        }
        self.display_texture.as_ref()
    }

    /// The single point where both a (re)allocation and a plain render
    /// happen, so one early return covers both halves of a lost device: no
    /// new `DisplayTexture` and no `render` call once the device is known
    /// gone.
    #[allow(clippy::too_many_arguments)]
    fn ensure_and_render(
        &mut self,
        renderer: &mut DisplayRenderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        src: &[u16],
        samples_per_pixel: u32,
    ) {
        let Some(texture) = self.ensure_texture(device, width, height) else {
            return;
        };
        renderer.render(device, queue, texture, src, samples_per_pixel, 0);
    }

    /// [`Self::ensure_and_render`]'s overlay-blend twin: the composite pane's
    /// own render call when the rainbow overlay is on.
    #[allow(clippy::too_many_arguments)]
    fn ensure_and_render_overlay(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        composite: &[u16],
        colors: &[[u8; 3]],
        alpha: u8,
    ) {
        let Some(texture) = self.ensure_texture(device, width, height) else {
            return;
        };
        render_overlay(device, queue, texture, composite, colors, alpha);
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
            crop: (0.0, 0.0, 64.0, 64.0),
            pixelated: false,
            step: NonZeroU32::MIN,
        };
        let shared = focusforge_gpu::device::shared_device_or_skip()
            .expect("this GPU lifecycle test requires a device");

        let mut empty = CanvasRenderer::new();
        assert!(empty.display_renderer.is_none());
        empty.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(empty.display_renderer.is_some());
        empty.detach_device();
        assert!(empty.display_renderer.is_none());
        empty.detach_device();
        assert!(empty.device_queue().is_none());
        assert!(empty.display_renderer.is_none());

        let mut frame_only = CanvasRenderer::new();
        frame_only
            .set_frame(&frame, None, crate::job::host_budget())
            .expect("fixture frame opens");
        assert!(frame_only.redraw_frame(&draw).is_none());
        frame_only.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(frame_only.redraw_frame(&draw).is_some());
        frame_only.detach_device();
        frame_only.detach_device();
        assert_eq!(frame_only.frame_path(), Some(frame.as_path()));
        assert!(frame_only.frame_pane.display_texture.is_none());
        assert!(frame_only.frame_pane.image.is_none());
        assert!(frame_only.frame_pane.coverage.is_none());
        frame_only.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(frame_only.redraw_frame(&draw).is_some());

        let mut planes_and_frame = CanvasRenderer::new();
        planes_and_frame
            .set_frame(&frame, None, crate::job::host_budget())
            .expect("fixture frame opens");
        planes_and_frame
            .set_planes(&composite, Some(&zmap))
            .expect("fixture planes open");
        assert!(planes_and_frame.redraw_frame(&draw).is_none());
        assert!(planes_and_frame.redraw_planes(&draw, None, None).is_none());
        planes_and_frame.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(planes_and_frame.redraw_frame(&draw).is_some());
        assert!(planes_and_frame.redraw_planes(&draw, None, None).is_some());
        planes_and_frame.detach_device();
        planes_and_frame.detach_device();
        assert_eq!(planes_and_frame.frame_path(), Some(frame.as_path()));
        assert!(planes_and_frame.composite.is_some());
        assert!(planes_and_frame.zmap.is_some());
        assert!(planes_and_frame.composite_pane.display_texture.is_none());
        assert!(planes_and_frame.composite_pane.image.is_none());
        assert!(planes_and_frame.frame_pane.display_texture.is_none());
        assert!(planes_and_frame.frame_pane.image.is_none());
        assert!(planes_and_frame.composite_pane.coverage.is_none());
        assert!(planes_and_frame.frame_pane.coverage.is_none());
        planes_and_frame.attach_device(shared.device().clone(), shared.queue().clone());
        assert!(planes_and_frame.redraw_frame(&draw).is_some());
        assert!(planes_and_frame.redraw_planes(&draw, None, None).is_some());

        let overlay = crate::canvas::OverlayDraw {
            table: focusforge_core::zmap::overlay_table(1, None),
            ops: Arc::from([]),
            alpha: focusforge_core::display::alpha_from_opacity(0.65),
        };
        assert!(
            planes_and_frame
                .redraw_planes(&draw, None, Some(&overlay))
                .is_some(),
            "the composite pane still draws with the overlay on"
        );
        assert!(
            planes_and_frame.redraw_planes(&draw, None, None).is_some(),
            "and turning it back off"
        );
    }

    fn fixture_frame() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mantis-64/frames/00.tif")
    }

    fn fixture_512_frame() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mantis-512/frames/00.tif")
    }

    /// A step-1 draw of the `side`x`side` square at the source origin.
    fn square_draw(side: u32) -> PaneDraw {
        square_draw_at(0, 0, side)
    }

    fn square_draw_at(x: u32, y: u32, side: u32) -> PaneDraw {
        PaneDraw {
            src: (x, y, side, side),
            dst: (0.0, 0.0, side as _, side as _),
            crop: (0.0, 0.0, side as _, side as _),
            pixelated: false,
            step: NonZeroU32::MIN,
        }
    }

    /// Re-runs the test `name` alone in a child process, because timing
    /// accounting and device loss are process-global; returns `true` only
    /// inside that child, where the test body then runs.
    fn in_isolated_child(name: &str) -> bool {
        const CHILD: &str = "FOCUSFORGE_GUI_GPU_CANVAS_ISOLATED_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let output = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "--exact",
            &format!("gpu_canvas::tests::{name}"),
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .expect("the isolated test starts");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the isolated test {name} must run and pass:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    fn accounting_snapshot() -> focusforge_core::timing::TimingSnapshot {
        focusforge_core::timing::timing_snapshot().expect("trace accounting is enabled")
    }

    #[test]
    fn a_guarded_resize_or_source_replacement_releases_the_superseded_texture() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let (composite, zmap) =
            golden_plane_files(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"));
        let mut renderer = CanvasRenderer::new();
        renderer.attach_device(shared.device().clone(), shared.queue().clone());
        renderer
            .set_frame(&fixture_512_frame(), None, crate::job::host_budget())
            .expect("fixture frame opens");

        // A 64-pixel visible draw retains the 64-pixel guard, yielding a
        // 192x192 texture. A separate 8-pixel draw gets a 136x136 guarded
        // texture and must replace that larger allocation.
        assert!(renderer.redraw_frame(&square_draw_at(0, 160, 64)).is_some());
        let texture = renderer
            .frame_pane
            .display_texture
            .as_ref()
            .expect("the first guarded draw holds a texture");
        assert_eq!((texture.width(), texture.height()), (192, 192));

        assert!(
            renderer
                .redraw_frame(&square_draw_at(300, 160, 8))
                .is_some()
        );
        let texture = renderer
            .frame_pane
            .display_texture
            .as_ref()
            .expect("the resized guarded draw holds a texture");
        assert_eq!(
            (texture.width(), texture.height()),
            (136, 136),
            "the pane holds only the texture of the new guarded size"
        );

        renderer
            .set_frame(&fixture_512_frame(), None, crate::job::host_budget())
            .expect("fixture frame re-opens");
        assert!(renderer.frame_pane.display_texture.is_none());
        assert!(renderer.frame_pane.image.is_none());

        renderer
            .set_planes(&composite, Some(&zmap))
            .expect("fixture planes open");
        assert!(
            renderer
                .redraw_planes(&square_draw(8), None, None)
                .is_some()
        );
        assert!(renderer.composite_pane.display_texture.is_some());
        renderer
            .set_planes(&composite, Some(&zmap))
            .expect("fixture planes re-open");
        assert!(renderer.composite_pane.display_texture.is_none());
        assert!(renderer.composite_pane.image.is_none());
    }

    #[test]
    fn each_attach_builds_one_display_renderer_whose_source_buffer_follows_guarded_draws() {
        if !in_isolated_child(
            "each_attach_builds_one_display_renderer_whose_source_buffer_follows_guarded_draws",
        ) {
            return;
        }
        focusforge_core::timing::enable_trace_accounting();
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let mut renderer = CanvasRenderer::new();
        renderer
            .set_frame(&fixture_frame(), None, crate::job::host_budget())
            .expect("fixture frame opens");
        let spp = renderer.frame.samples_per_pixel().expect("a frame is open");
        let packed_bytes = |side: u32| u64::from(side * side * spp).div_ceil(2) * 4;

        for attach in 0..2 {
            let before = accounting_snapshot();
            renderer.attach_device(shared.device().clone(), shared.queue().clone());
            let after = accounting_snapshot();
            assert!(renderer.display_renderer.is_some());
            assert_eq!(
                after.pipeline_creations - before.pipeline_creations,
                1,
                "attach {attach} builds exactly one display pipeline"
            );
            assert_eq!(
                after.allocation_operations - before.allocation_operations,
                1,
                "attach {attach} allocates only the params buffer"
            );

            // The 64x64 fixture bounds every guarded draw here to the full
            // image. The first upload creates the 64x64 source buffer; all
            // later visible draws are covered by that guarded texture, so
            // neither a source buffer nor a display texture is replaced.
            for (side, recreated) in [(64, true), (8, false), (4, false), (3, false), (5, false)] {
                let before = accounting_snapshot();
                assert!(renderer.redraw_frame(&square_draw(side)).is_some());
                let after = accounting_snapshot();
                assert_eq!(
                    after.pipeline_creations - before.pipeline_creations,
                    0,
                    "side {side}: no pipeline is rebuilt"
                );
                assert_eq!(
                    after.allocation_operations - before.allocation_operations,
                    u64::from(recreated),
                    "side {side}: source buffer re-created = {recreated}"
                );
                assert_eq!(
                    after.allocation_bytes - before.allocation_bytes,
                    if recreated { packed_bytes(side) } else { 0 },
                    "side {side}: a re-created source buffer is sized to the draw"
                );
            }

            renderer.detach_device();
            assert!(renderer.display_renderer.is_none());
        }
    }

    #[test]
    fn a_lost_device_creates_no_texture_buffer_or_render() {
        if !in_isolated_child("a_lost_device_creates_no_texture_buffer_or_render") {
            return;
        }
        focusforge_core::timing::enable_trace_accounting();
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let mut renderer = CanvasRenderer::new();
        renderer.attach_device(shared.device().clone(), shared.queue().clone());
        renderer
            .set_frame(&fixture_frame(), None, crate::job::host_budget())
            .expect("fixture frame opens");

        focusforge_gpu::errors::install_handlers(shared.device());
        shared.device().destroy();
        let _ = shared.device().poll(wgpu::PollType::Poll);
        assert!(
            focusforge_gpu::errors::device_lost(),
            "a destroyed device reports itself lost"
        );

        let before = accounting_snapshot();
        let _ = renderer.redraw_frame(&square_draw(8));
        let after = accounting_snapshot();
        assert!(renderer.frame_pane.display_texture.is_none());
        assert!(renderer.frame_pane.image.is_none());
        assert_eq!(
            after.allocation_operations - before.allocation_operations,
            0,
            "no buffer is created on a lost device"
        );
        assert_eq!(
            after.packed_bytes - before.packed_bytes,
            0,
            "no render runs on a lost device"
        );
        assert_eq!(
            after.upload_bytes - before.upload_bytes,
            0,
            "nothing is uploaded to a lost device"
        );
    }
}

/// What can go wrong opening the composite a [`CanvasRenderer`] holds open
/// for the life of the loaded project.
#[derive(Debug, thiserror::Error)]
pub enum CanvasRendererError {
    #[error("failed to open composite {}: {source}", path.display())]
    Composite {
        path: PathBuf,
        #[source]
        source: TiffError,
    },
}

/// The composite pane's GPU-owned state, the frame pane's own, the one
/// [`DisplayRenderer`] every pane renders through, and the CPU scratch the
/// synchronous redraws read into. A pane's read is a [`PaneJob`] (none when
/// its [`PaneTexture`] already covers the draw) that [`Self::publish`] renders
/// once its samples exist; the `redraw_*` methods do both in place. The
/// sealed composite and Z-Map are optional: a tab with nothing sealed still
/// gets a frame pane. The Z-Map is the index source for the overlay and the
/// pipette and is never drawn as a pane of its own.
pub struct CanvasRenderer {
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    display_renderer: Option<DisplayRenderer>,
    sample_scratch: Vec<u16>,
    composite: Option<Arc<TiffStack>>,
    zmap: Option<Arc<ZMapReader>>,
    composite_pane: PaneTexture,
    frame: crate::canvas::FrameSource,
    frame_pane: PaneTexture,
    /// The low-detail whole-image textures drawn beneath each pane's exact
    /// one.
    frame_backdrop: PaneTexture,
    composite_backdrop: PaneTexture,
    /// The last stamp issued: every job takes the next one, and a reset
    /// refuses everything up to the current value.
    stamp: u64,
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
            display_renderer: None,
            sample_scratch: Vec::new(),
            composite: None,
            zmap: None,
            composite_pane: PaneTexture::default(),
            frame: crate::canvas::FrameSource::default(),
            frame_pane: PaneTexture::default(),
            frame_backdrop: PaneTexture::default(),
            composite_backdrop: PaneTexture::default(),
            stamp: 0,
        }
    }

    fn next_stamp(&mut self) -> u64 {
        self.stamp += 1;
        self.stamp
    }

    /// Empties the composite pane and its backdrop and refuses every job
    /// issued so far for them.
    fn reset_planes(&mut self) {
        let stamp = self.next_stamp();
        self.composite_pane = PaneTexture::fresh(stamp);
        self.composite_backdrop = PaneTexture::fresh(stamp);
    }

    /// Empties the frame pane and refuses every job issued so far for it.
    fn reset_frame(&mut self) {
        let stamp = self.next_stamp();
        self.frame_pane = PaneTexture::fresh(stamp);
        self.frame_backdrop = PaneTexture::fresh(stamp);
    }

    fn texture_mut(&mut self, pane: Pane, backdrop: bool) -> &mut PaneTexture {
        match (pane, backdrop) {
            (Pane::Frame, true) => &mut self.frame_backdrop,
            (Pane::Composite, true) => &mut self.composite_backdrop,
            (pane, false) => self.pane_mut(pane),
        }
    }

    fn pane_mut(&mut self, pane: Pane) -> &mut PaneTexture {
        match pane {
            Pane::Frame => &mut self.frame_pane,
            Pane::Composite => &mut self.composite_pane,
        }
    }

    fn pane_ref(&self, pane: Pane) -> &PaneTexture {
        match pane {
            Pane::Frame => &self.frame_pane,
            Pane::Composite => &self.composite_pane,
        }
    }

    fn source_size(&self, pane: Pane) -> Option<(u32, u32)> {
        match pane {
            Pane::Frame => self.frame.dimensions(),
            Pane::Composite => {
                let header = self.composite.as_ref()?.header(0);
                Some((header.width, header.height))
            }
        }
    }

    /// Uses Slint's surface-compatible device for future canvas draws,
    /// building the one shared [`DisplayRenderer`] every pane renders
    /// through.
    pub fn attach_device(&mut self, device: wgpu::Device, queue: wgpu::Queue) {
        self.detach_device();
        self.display_renderer = Some(DisplayRenderer::new(&device));
        self.device = Some(device);
        self.queue = Some(queue);
    }

    /// Releases resources bound to Slint's device while keeping source readers.
    pub fn detach_device(&mut self) {
        self.reset_planes();
        self.reset_frame();
        self.display_renderer = None;
        self.device = None;
        self.queue = None;
    }

    fn device_queue(&self) -> Option<(wgpu::Device, wgpu::Queue)> {
        Some((self.device.as_ref()?.clone(), self.queue.as_ref()?.clone()))
    }

    /// Opens `composite`, and `zmap` when given, as this renderer's planes,
    /// replacing whichever were open before. A composite that fails to open
    /// is an error naming it, leaving the previous planes untouched. A Z-Map
    /// that fails to open is logged and left closed: the composite draws
    /// without it, and only the overlay is lost.
    pub fn set_planes(
        &mut self,
        composite_path: &Path,
        zmap_path: Option<&Path>,
    ) -> Result<(), CanvasRendererError> {
        let composite = TiffStack::open_files(std::slice::from_ref(&composite_path.to_path_buf()))
            .map_err(|source| CanvasRendererError::Composite {
                path: composite_path.to_path_buf(),
                source,
            })?;
        self.zmap = zmap_path.and_then(|path| match ZMapReader::open(path) {
            Ok(zmap) => Some(Arc::new(zmap)),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "the Z-Map could not be opened");
                None
            }
        });
        self.composite = Some(Arc::new(composite));
        self.reset_planes();
        Ok(())
    }

    /// Drops the open planes, leaving the frame half untouched.
    pub fn clear_planes(&mut self) {
        self.composite = None;
        self.zmap = None;
        self.reset_planes();
    }

    /// The frame pane's currently open path, if any.
    pub fn frame_path(&self) -> Option<&Path> {
        self.frame.path()
    }

    /// The frame pane's currently recorded view, if any.
    pub fn frame_view(&self) -> Option<FrameView> {
        self.frame.view()
    }

    /// Opens `path` as the frame pane's source under `budget`, dropping
    /// whatever was open first, and records `view` for the frame pane's
    /// reads to warp through. `Err` leaves nothing open.
    pub fn set_frame(
        &mut self,
        path: &Path,
        view: Option<FrameView>,
        budget: HostBudget,
    ) -> Result<(), FrameError> {
        let result = self.frame.set(path, view, budget);
        self.reset_frame();
        result
    }

    /// Drops the open frame, if any.
    pub fn clear_frame(&mut self) {
        self.frame.clear();
        self.reset_frame();
    }

    /// The job that brings the composite pane to `draw`, `None` when
    /// that pane already covers `draw.src`/`draw.step` under the same edit
    /// generation and overlay (whatever `dst` and `pixelated` now are; work
    /// still in flight for it is retired), with no planes or device, or an
    /// out-of-bounds `draw.src`. Only a three-sample overlay over an open
    /// Z-Map also reads its indices; the composite draws without it.
    pub fn request_planes(
        &mut self,
        draw: &PaneDraw,
        generation: Option<u64>,
        overlay: Option<&OverlayDraw>,
    ) -> Option<PaneJob> {
        self.plane_job(draw, generation, overlay, false)
    }

    /// The job that brings the composite pane's backdrop to `draw`, the
    /// whole image at a coarse step. `None` when the backdrop covers that
    /// content or a job for it was already issued -- a pan or zoom never
    /// asks again -- and otherwise as [`Self::request_planes`].
    pub fn request_plane_backdrop(
        &mut self,
        draw: &PaneDraw,
        generation: Option<u64>,
        overlay: Option<&OverlayDraw>,
    ) -> Option<PaneJob> {
        self.plane_job(draw, generation, overlay, true)
    }

    fn plane_job(
        &mut self,
        draw: &PaneDraw,
        generation: Option<u64>,
        overlay: Option<&OverlayDraw>,
        backdrop: bool,
    ) -> Option<PaneJob> {
        self.device.as_ref()?;
        let composite = self.composite.as_ref()?.clone();
        let header = composite.header(0);
        let size = (header.width, header.height);
        let overlay = overlay.filter(|_| header.samples_per_pixel == 3);
        let indices = overlay
            .and(self.zmap.clone())
            .map(|zmap| zmap as Arc<dyn PaneReader>);
        let state = EditState {
            generation,
            overlay: overlay.cloned(),
        };
        self.request(
            Pane::Composite,
            backdrop,
            draw,
            size,
            composite,
            indices,
            state,
        )
    }

    /// The job that brings the frame pane to `draw`; `None` as
    /// [`Self::request_planes`], with no frame open in place of no planes.
    pub fn request_frame(&mut self, draw: &PaneDraw) -> Option<PaneJob> {
        self.frame_job(draw, false)
    }

    /// The frame pane's backdrop job; `None` as
    /// [`Self::request_plane_backdrop`].
    pub fn request_frame_backdrop(&mut self, draw: &PaneDraw) -> Option<PaneJob> {
        self.frame_job(draw, true)
    }

    fn frame_job(&mut self, draw: &PaneDraw, backdrop: bool) -> Option<PaneJob> {
        self.device.as_ref()?;
        let frame_size = self.frame.dimensions()?;
        let reader = self.frame.reader()?;
        self.request(
            Pane::Frame,
            backdrop,
            draw,
            frame_size,
            reader,
            None,
            EditState::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn request(
        &mut self,
        pane: Pane,
        backdrop: bool,
        draw: &PaneDraw,
        source_size: (u32, u32),
        reader: Arc<dyn PaneReader>,
        indices: Option<Arc<dyn PaneReader>>,
        state: EditState,
    ) -> Option<PaneJob> {
        let max_texture_dimension = self.device.as_ref()?.limits().max_texture_dimension_2d;
        if !visible_draw_fits(source_size, draw) {
            return None;
        }
        let texture = self.texture_mut(pane, backdrop);
        if backdrop
            && (texture.covers(draw.src, draw.step, &state)
                || texture.requested(draw.src, draw.step, &state))
        {
            return None;
        }
        let stamp = self.next_stamp();
        let texture = self.texture_mut(pane, backdrop);
        if texture.covers(draw.src, draw.step, &state) {
            texture.floor.reset(stamp);
            return None;
        }
        let guarded = draw.guarded(
            ImageSize {
                width: source_size.0,
                height: source_size.1,
            },
            max_texture_dimension,
        )?;
        if !visible_draw_fits(source_size, &guarded) {
            return None;
        }
        if backdrop {
            texture.requested = Some((draw.src, draw.step, state.clone()));
        }
        Some(PaneJob {
            pane,
            draw: guarded,
            stamp,
            reader,
            indices,
            state,
            backdrop,
        })
    }

    /// Renders `samples` (read for `job`) into the job's pane and returns
    /// the pane's image, as [`Self::publish_edited`] with no edit session.
    pub fn publish(&mut self, job: &PaneJob, samples: &[u16]) -> Option<slint::Image> {
        self.publish_edited(job, samples, &[], None)
    }

    /// Renders `samples` (read for `job`) into the job's pane and returns
    /// the pane's image. `sealed_indices` is the sealed Z-Map over the same
    /// texels (empty when the job read none). `edited`, present on a job
    /// issued under an edit generation, overlays the composite with the
    /// resident tiles' pixel wherever one is held; the job's
    /// own overlay blends the rainbow over a three-sample composite. `None`,
    /// with nothing rendered or uploaded, when the job is stale -- issued
    /// before a source, plane or device change or before a newer job for the
    /// pane published -- or when there is no device.
    pub fn publish_edited(
        &mut self,
        job: &PaneJob,
        samples: &[u16],
        sealed_indices: &[u16],
        edited: Option<&EditedView>,
    ) -> Option<slint::Image> {
        let (device, queue) = self.device_queue()?;
        if !self
            .texture_mut(job.pane, job.backdrop)
            .floor
            .accept(job.stamp)
        {
            return None;
        }
        let renderer = self.display_renderer.as_mut()?;
        let (width, height) = job.draw.extent();
        let texture = match (job.pane, job.backdrop) {
            (Pane::Frame, false) => &mut self.frame_pane,
            (Pane::Composite, false) => &mut self.composite_pane,
            (Pane::Frame, true) => &mut self.frame_backdrop,
            (Pane::Composite, true) => &mut self.composite_backdrop,
        };
        if texture.covers(job.draw.src, job.draw.step, &job.state) {
            return Some(texture.image());
        }
        let samples_per_pixel = job.reader.samples_per_pixel();
        let (x, y, src_width, src_height) = job.draw.src;
        let rect = Rect {
            x,
            y,
            width: src_width,
            height: src_height,
        };
        let step = job.draw.step;
        let edited = edited.filter(|_| job.state.generation.is_some());
        match job.pane {
            Pane::Composite => {
                let merged = edited.map(|view| {
                    let mut merged = samples.to_vec();
                    crate::canvas::overlay_edited(
                        &mut merged,
                        rect,
                        step,
                        samples_per_pixel,
                        view.tiles,
                    );
                    merged
                });
                let composite = merged.as_deref().unwrap_or(samples);
                match (&job.state.overlay, self.zmap.as_deref()) {
                    (Some(overlay), Some(zmap)) if !sealed_indices.is_empty() => {
                        let mut colors = vec![[0u8; 3]; sealed_indices.len()];
                        overlay_colors(
                            &overlay.ops,
                            zmap.info(),
                            rect,
                            step,
                            &overlay.table,
                            sealed_indices,
                            &mut colors,
                        );
                        texture.ensure_and_render_overlay(
                            &device,
                            &queue,
                            width,
                            height,
                            composite,
                            &colors,
                            overlay.alpha,
                        );
                    }
                    _ => texture.ensure_and_render(
                        renderer,
                        &device,
                        &queue,
                        width,
                        height,
                        composite,
                        samples_per_pixel,
                    ),
                }
            }
            _ => texture.ensure_and_render(
                renderer,
                &device,
                &queue,
                width,
                height,
                samples,
                samples_per_pixel,
            ),
        }
        texture.mark_covered(job.draw.src, job.draw.step, job.state.clone());
        Some(texture.image())
    }

    /// The image `pane` currently holds and where it lands under `draw`:
    /// the texture drawn at its own last `src`, scaled and offset to the new
    /// view. `None` while the pane holds nothing.
    pub fn placement(
        &self,
        pane: Pane,
        draw: &PaneDraw,
    ) -> Option<(slint::Image, (f32, f32, f32, f32))> {
        if !visible_draw_fits(self.source_size(pane)?, draw) {
            return None;
        }
        Self::placed(self.pane_ref(pane), draw)
    }

    /// [`Self::placement`] for `pane`'s backdrop: the whole-image texture at
    /// the rect `draw`'s view gives the whole image.
    pub fn backdrop_placement(
        &self,
        pane: Pane,
        draw: &PaneDraw,
    ) -> Option<(slint::Image, (f32, f32, f32, f32))> {
        let backdrop = match pane {
            Pane::Frame => &self.frame_backdrop,
            Pane::Composite => &self.composite_backdrop,
        };
        Self::placed(backdrop, draw)
    }

    fn placed(
        texture: &PaneTexture,
        draw: &PaneDraw,
    ) -> Option<(slint::Image, (f32, f32, f32, f32))> {
        let (src, ..) = texture.coverage.as_ref()?;
        texture.image.as_ref()?;
        Some((texture.image(), draw.place(*src)))
    }

    /// Reads a job's samples into the shared scratch and publishes them.
    fn read_and_publish(
        &mut self,
        job: PaneJob,
        edited: Option<&EditedView>,
    ) -> Option<slint::Image> {
        let mut scratch = std::mem::take(&mut self.sample_scratch);
        let mut indices = Vec::new();
        let image = job
            .read_into(&mut scratch)
            .and_then(|()| job.read_sealed_indices(&mut indices))
            .ok()
            .and_then(|()| self.publish_edited(&job, &scratch, &indices, edited));
        self.sample_scratch = scratch;
        image
    }

    /// Re-reads and re-uploads `draw.src` for the composite pane, returning
    /// its `slint::Image`. A no-op when that pane already covers
    /// `draw.src`/`draw.step` under the same edit generation and overlay,
    /// whatever `dst`/`pixelated` now are. `edited`, present, overlays the
    /// composite with the resident tiles' pixel wherever one is
    /// held; `overlay`, present on a three-sample composite, blends the
    /// rainbow overlay in place of the plain render. `None` with no planes
    /// open, an out-of-bounds `draw.src`, or a failed read (coverage stays
    /// untouched on that path).
    pub fn redraw_planes(
        &mut self,
        draw: &PaneDraw,
        edited: Option<EditedView>,
        overlay: Option<&OverlayDraw>,
    ) -> Option<slint::Image> {
        let _scope = measurement_scope("gui_plane_redraw");
        if !visible_draw_fits(self.source_size(Pane::Composite)?, draw) {
            return None;
        }
        let generation = edited.as_ref().map(|view| view.tiles.generation());
        match self.request_planes(draw, generation, overlay) {
            Some(job) => self.read_and_publish(job, edited.as_ref()),
            None => self.covered_image(Pane::Composite, draw),
        }
    }

    /// Re-reads and re-uploads `draw.src` for the frame pane, decimated as in
    /// [`Self::redraw_planes`], returning the
    /// `slint::Image` to bind to `FFPane::source`. `None` with no frame
    /// open, when `draw.src` falls outside it, or the read fails; the frame
    /// pane's coverage stays untouched on that path so a later, valid rect
    /// still renders. A no-op when the frame pane already covers
    /// `draw.src`/`draw.step`. The same display path [`Self::redraw_planes`]
    /// uses, at an index scale of 0 -- a source frame carries no Z-Map
    /// indices.
    pub fn redraw_frame(&mut self, draw: &PaneDraw) -> Option<slint::Image> {
        let _scope = measurement_scope("gui_frame_redraw");
        if !visible_draw_fits(self.source_size(Pane::Frame)?, draw) {
            return None;
        }
        match self.request_frame(draw) {
            Some(job) => self.read_and_publish(job, None),
            None => self.covered_image(Pane::Frame, draw),
        }
    }

    /// The pane's image when it already covers `draw`, `None` otherwise.
    fn covered_image(&self, pane: Pane, draw: &PaneDraw) -> Option<slint::Image> {
        let texture = self.pane_ref(pane);
        texture
            .coverage
            .as_ref()
            .is_some_and(|(src, step, _)| rect_contains(*src, draw.src) && *step == draw.step)
            .then(|| texture.image())
    }
}
