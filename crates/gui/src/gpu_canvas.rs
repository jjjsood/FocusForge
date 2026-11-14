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
use focusforge_core::retouch::preview::{
    BRUSH_PREVIEW_STRENGTH_Q16, BrushPreviewGrid, BrushPreviewSpec, brush_preview_bounds,
};
use focusforge_core::tile::{HostBudget, Rect};
use focusforge_core::zmap::ZMapReader;
use focusforge_gpu::display::preview::{BrushPreviewRenderer, plan as plan_brush_preview};
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

/// Which pair of textures answered [`CanvasRenderer::brush_preview_grids`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewTier {
    /// The frame and composite panes at the current view.
    Exact,
    /// The two whole-image backdrops.
    Backdrop,
}

/// The samples a brush preview is built from, borrowed from the renderer.
#[derive(Debug, Clone, Copy)]
pub struct PreviewGrids<'a> {
    pub tier: PreviewTier,
    /// The composite's size in pixels.
    pub image_size: (u32, u32),
    pub result: BrushPreviewGrid<'a>,
    pub source: BrushPreviewGrid<'a>,
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
    /// The source-type samples `coverage` was drawn from (for the composite,
    /// the merged result before any overlay colouring), `retained_spp` per
    /// texel; empty until a publish and dropped with the pane.
    retained: Vec<u16>,
    retained_spp: u32,
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

    /// Keeps the samples this texture was just drawn from: `merged` when the
    /// composite was merged with the edit session's tiles, else `samples`,
    /// copied into the existing allocation.
    fn retain(&mut self, merged: Option<Vec<u16>>, samples: &[u16], samples_per_pixel: u32) {
        match merged {
            Some(merged) => self.retained = merged,
            None => {
                self.retained.clear();
                self.retained.extend_from_slice(samples);
            }
        }
        self.retained_spp = samples_per_pixel;
    }

    /// The retained samples as a preview grid over the pane's covered source
    /// rect; `None` before the first publish.
    fn retained_grid(&self) -> Option<BrushPreviewGrid<'_>> {
        let ((x, y, width, height), step, _) = self.coverage.as_ref()?;
        if self.retained.is_empty() {
            return None;
        }
        Some(BrushPreviewGrid {
            rect: Rect {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            },
            step: *step,
            samples_per_pixel: self.retained_spp,
            samples: &self.retained,
        })
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

    type Shared = &'static focusforge_gpu::device::SharedDevice;

    fn import_failure_keeps_the_previous_image_and_records_the_spec(shared: Shared) {
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

    fn lifecycle_keeps_readers_and_rebuilds_gpu_draws(shared: Shared) {
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
            checkpoint_colors: Arc::default(),
            alpha: focusforge_core::display::alpha_from_opacity(0.65),
            origin: None,
            checkpoint_sources: Default::default(),
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

    fn a_guarded_resize_or_source_replacement_releases_the_superseded_texture(shared: Shared) {
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
        let scratch = tempfile::tempdir().unwrap();
        let replacement = scratch.path().join("composite.tif");
        std::fs::copy(&composite, &replacement).unwrap();
        renderer
            .set_planes(&replacement, Some(&zmap))
            .expect("replacement planes open");
        assert!(renderer.composite_pane.display_texture.is_none());
        assert!(renderer.composite_pane.image.is_none());
    }

    fn each_attach_builds_one_display_renderer_whose_source_buffer_follows_guarded_draws(
        shared: Shared,
    ) {
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
                4,
                "attach {attach} builds the display pipeline and the three brush-preview ones"
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

    fn a_lost_device_creates_no_texture_buffer_or_render(shared: Shared) {
        let mut renderer = CanvasRenderer::new();
        renderer.attach_device(shared.device().clone(), shared.queue().clone());
        renderer
            .set_frame(&fixture_frame(), None, crate::job::host_budget())
            .expect("fixture frame opens");

        let draw = draw_of((200, 200, 64, 64), 1);
        let mut preview = renderer_with(shared, &fixture_512_frame());
        publish_exact(&mut preview, &draw);

        focusforge_gpu::errors::install_handlers(shared.device());
        shared.device().destroy();
        // wgpu delivers the device-lost callback from a later poll, and a
        // software adapter can need several, so poll until the flag latches.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !focusforge_gpu::errors::device_lost() && std::time::Instant::now() < deadline {
            let _ = shared.device().poll(wgpu::PollType::Poll);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            focusforge_gpu::errors::device_lost(),
            "a destroyed device reports itself lost"
        );

        assert!(
            preview
                .brush_preview_layer(&brush(232, 232, 10.0, BrushMode::Lighten), &draw)
                .is_none(),
            "no preview layer on a lost device"
        );
        assert!(preview.brush_preview_pane.display_texture.is_none());

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

    use focusforge_core::io::{TiffSpec, TiffWriter};
    use focusforge_core::project::{BrushMode, Point};
    use focusforge_core::retouch::preview::{
        BRUSH_PREVIEW_STRENGTH_Q16, brush_preview, brush_preview_layout,
    };

    const SIDE: u32 = 512;

    fn tuple(rect: Rect) -> (u32, u32, u32, u32) {
        (rect.x, rect.y, rect.width, rect.height)
    }

    fn draw_of(src: (u32, u32, u32, u32), step: u32) -> PaneDraw {
        PaneDraw {
            src,
            dst: (0.0, 0.0, src.2 as f32, src.3 as f32),
            crop: (0.0, 0.0, src.2 as f32, src.3 as f32),
            pixelated: false,
            step: NonZeroU32::new(step).expect("a step is nonzero"),
        }
    }

    fn brush(x: i32, y: i32, radius: f32, mode: BrushMode) -> BrushPreviewSpec {
        BrushPreviewSpec {
            centre: Point { x, y },
            radius,
            softness: 0.3,
            mode,
            opacity: 1.0,
        }
    }

    /// Distinct, deterministic samples for `len` values.
    fn pattern(len: usize, seed: u32) -> Vec<u16> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed * 7919) >> 16) as u16)
            .collect()
    }

    fn composite_file() -> PathBuf {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        golden_plane_files(&fixtures).0
    }

    /// A renderer on `shared` with `frame` and the 512 px golden composite open.
    fn renderer_with(shared: Shared, frame: &Path) -> CanvasRenderer {
        let mut renderer = CanvasRenderer::new();
        renderer.attach_device(shared.device().clone(), shared.queue().clone());
        renderer
            .set_frame(frame, None, crate::job::host_budget())
            .expect("frame opens");
        renderer
            .set_planes(&composite_file(), None)
            .expect("composite opens");
        renderer
    }

    /// Publishes `pattern` samples sized to `job` and returns them.
    fn publish_pattern(renderer: &mut CanvasRenderer, job: &PaneJob, seed: u32) -> Vec<u16> {
        let (width, height) = job.draw.extent();
        let samples = pattern(
            width as usize * height as usize * job.reader.samples_per_pixel() as usize,
            seed,
        );
        assert!(
            renderer.publish(job, &samples).is_some(),
            "the job publishes"
        );
        samples
    }

    /// What `publish_all` published, per texture.
    struct Published {
        frame: Vec<u16>,
        composite: Vec<u16>,
        frame_backdrop: Vec<u16>,
        composite_backdrop: Vec<u16>,
    }

    /// Both exact panes around (232, 232) and both backdrops (steps 4 and 8)
    /// of a 512 px image, the exact panes' guarded `src` being
    /// (136, 136, 192, 192).
    fn publish_all(renderer: &mut CanvasRenderer) -> Published {
        let visible = draw_of((200, 200, 64, 64), 1);
        let job = renderer.request_frame(&visible).expect("frame job");
        let frame = publish_pattern(renderer, &job, 1);
        let job = renderer.request_planes(&visible, None, None).expect("job");
        let composite = publish_pattern(renderer, &job, 2);
        let job = renderer
            .request_frame_backdrop(&draw_of((0, 0, SIDE, SIDE), 4))
            .expect("frame backdrop job");
        let frame_backdrop = publish_pattern(renderer, &job, 3);
        let job = renderer
            .request_plane_backdrop(&draw_of((0, 0, SIDE, SIDE), 8), None, None)
            .expect("composite backdrop job");
        let composite_backdrop = publish_pattern(renderer, &job, 4);
        Published {
            frame,
            composite,
            frame_backdrop,
            composite_backdrop,
        }
    }

    fn retained_lengths(renderer: &CanvasRenderer) -> [usize; 4] {
        [
            renderer.frame_pane.retained.len(),
            renderer.composite_pane.retained.len(),
            renderer.frame_backdrop.retained.len(),
            renderer.composite_backdrop.retained.len(),
        ]
    }

    fn one_spp_frame(dir: &Path) -> PathBuf {
        let path = dir.join("frame.tif");
        let mut writer = TiffWriter::create(
            &path,
            TiffSpec {
                width: SIDE,
                height: SIDE,
                samples_per_pixel: 1,
                description: None,
            },
        )
        .expect("the frame file is created");
        let rect = Rect {
            x: 0,
            y: 0,
            width: SIDE,
            height: SIDE,
        };
        writer
            .write_tile(rect, &pattern((SIDE * SIDE) as usize, 9))
            .expect("the frame tile is written");
        writer.finish().expect("the frame file is finished");
        path
    }

    #[test]
    fn retained_exact_samples_match_the_published_ones() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let one_spp = one_spp_frame(scratch.path());
        let steps = [(1, 1), (1, 2)];
        let cases = [
            (fixture_512_frame(), steps[0]),
            (fixture_512_frame(), steps[1]),
            (one_spp, steps[0]),
        ];
        let spec = brush(232, 232, 10.0, BrushMode::Lighten);
        for (frame, (frame_step, composite_step)) in cases {
            let mut renderer = renderer_with(shared, &frame);
            let frame_spp = renderer.frame.samples_per_pixel().expect("a frame is open");
            let frame_job = renderer
                .request_frame(&draw_of(
                    (200, 200, 64 * frame_step, 64 * frame_step),
                    frame_step,
                ))
                .expect("frame job");
            let frame_samples = publish_pattern(&mut renderer, &frame_job, 1);
            let composite_job = renderer
                .request_planes(
                    &draw_of(
                        (200, 200, 64 * composite_step, 64 * composite_step),
                        composite_step,
                    ),
                    None,
                    None,
                )
                .expect("composite job");
            let composite_samples = publish_pattern(&mut renderer, &composite_job, 2);
            let label = format!("frame {frame_spp} spp, steps {frame_step}/{composite_step}");

            let bounds = brush_preview_bounds(
                (SIDE, SIDE),
                &spec,
                NonZeroU32::new(composite_step).expect("a step is nonzero"),
            )
            .expect("the brush has a box");
            assert!(
                rect_contains(composite_job.draw.src, tuple(bounds)),
                "{label}"
            );
            assert!(rect_contains(frame_job.draw.src, tuple(bounds)), "{label}");

            let grids = renderer
                .brush_preview_grids(&spec)
                .unwrap_or_else(|| panic!("{label}: the exact panes answer"));
            assert_eq!(grids.tier, PreviewTier::Exact, "{label}");
            assert_eq!(grids.image_size, (SIDE, SIDE), "{label}");
            assert_eq!(grids.result.samples, composite_samples, "{label}: result");
            assert_eq!(grids.source.samples, frame_samples, "{label}: source");
            assert_eq!(tuple(grids.result.rect), composite_job.draw.src, "{label}");
            assert_eq!(tuple(grids.source.rect), frame_job.draw.src, "{label}");
            assert_eq!(grids.result.step, composite_job.draw.step, "{label}");
            assert_eq!(grids.source.step, frame_job.draw.step, "{label}");
            assert_eq!(grids.result.samples_per_pixel, 3, "{label}");
            assert_eq!(grids.source.samples_per_pixel, frame_spp, "{label}");
            let preview = brush_preview(
                grids.image_size,
                &grids.result,
                &grids.source,
                &spec,
                BRUSH_PREVIEW_STRENGTH_Q16,
            );
            assert_eq!(preview.is_some(), frame_spp == 3, "{label}: the preview");
        }

        // Under an edit generation the result is the merged composite.
        let mut renderer = renderer_with(shared, &fixture_512_frame());
        let mut tiles = crate::canvas::ResidentTiles::new(16, SIDE, SIDE, 3);
        let region = Rect {
            x: 224,
            y: 224,
            width: 8,
            height: 8,
        };
        tiles.install(region, &vec![65_000u16; 8 * 8 * 3]);
        let view = EditedView { tiles: &tiles };
        let visible = draw_of((200, 200, 64, 64), 1);
        let frame_job = renderer.request_frame(&visible).expect("frame job");
        publish_pattern(&mut renderer, &frame_job, 1);
        let job = renderer
            .request_planes(&visible, Some(tiles.generation()), None)
            .expect("composite job");
        let (width, height) = job.draw.extent();
        let sealed = pattern(width as usize * height as usize * 3, 2);
        assert!(
            renderer
                .publish_edited(&job, &sealed, &[], Some(&view))
                .is_some()
        );
        let mut merged = sealed.clone();
        let (x, y, w, h) = job.draw.src;
        crate::canvas::overlay_edited(
            &mut merged,
            Rect {
                x,
                y,
                width: w,
                height: h,
            },
            job.draw.step,
            3,
            &tiles,
        );
        assert_ne!(merged, sealed, "the edit changes samples");
        let grids = renderer.brush_preview_grids(&spec).expect("answers");
        assert_eq!(grids.result.samples, merged, "the merged samples");
    }

    /// Publishes pattern samples for `draw` to both exact panes.
    fn publish_exact(renderer: &mut CanvasRenderer, draw: &PaneDraw) {
        let frame_job = renderer.request_frame(draw).expect("frame job");
        publish_pattern(renderer, &frame_job, 1);
        let job = renderer
            .request_planes(draw, None, None)
            .expect("composite job");
        publish_pattern(renderer, &job, 2);
    }

    #[test]
    fn brush_preview_kernel_draws_on_the_calling_thread() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let mut renderer = renderer_with(shared, &fixture_512_frame());
        let draw = draw_of((200, 200, 64, 64), 1);
        publish_exact(&mut renderer, &draw);
        let spec = brush(232, 232, 10.0, BrushMode::Lighten);

        let (image, place) = renderer
            .brush_preview_layer(&spec, &draw)
            .expect("the layer draws");
        let grids = renderer
            .brush_preview_grids(&spec)
            .expect("the panes answer");
        let layout = brush_preview_layout(grids.image_size, &grids.result, &spec)
            .expect("the brush has a layout");
        assert_eq!((layout.column % 4, layout.row % 4), (0, 0));
        assert_eq!(
            (image.size().width, image.size().height),
            (layout.width, layout.height),
            "the image is the layout's texel size"
        );
        let step = grids.result.step.get();
        let (x, y) = (
            grids.result.rect.x + layout.column * step,
            grids.result.rect.y + layout.row * step,
        );
        assert_eq!(
            place,
            (
                (x - 200) as f32,
                (y - 200) as f32,
                (layout.width * step) as f32,
                (layout.height * step) as f32
            ),
            "the layer lands on its pixels under the draw"
        );

        let texture = |renderer: &CanvasRenderer| {
            renderer
                .brush_preview_pane
                .display_texture
                .as_ref()
                .expect("the layer has a texture")
                .texture()
                .clone()
        };
        let first = texture(&renderer);
        renderer
            .brush_preview_layer(&spec, &draw)
            .expect("the layer draws again");
        assert!(
            texture(&renderer) == first,
            "the same texel size reuses the texture"
        );
    }

    #[test]
    fn brush_preview_kernel_returns_none_on_refusal_or_lost_device() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let draw = draw_of((200, 200, 64, 64), 1);
        let spec = brush(232, 232, 10.0, BrushMode::Lighten);

        // A one-sample frame has no layer, and allocates no texture.
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let mut grey = renderer_with(shared, &one_spp_frame(scratch.path()));
        publish_exact(&mut grey, &draw);
        assert!(
            grey.brush_preview_grids(&spec).is_some(),
            "the panes answer"
        );
        assert!(grey.brush_preview_layer(&spec, &draw).is_none());
        assert!(grey.brush_preview_pane.display_texture.is_none());

        // A brush outside the retained grids keeps the previous image.
        let mut renderer = renderer_with(shared, &fixture_512_frame());
        publish_exact(&mut renderer, &draw);
        let (image, _) = renderer
            .brush_preview_layer(&spec, &draw)
            .expect("the layer draws");
        let before = renderer
            .brush_preview_pane
            .display_texture
            .as_ref()
            .expect("the layer has a texture")
            .texture()
            .clone();
        let outside = brush(400, 400, 10.0, BrushMode::Lighten);
        assert!(renderer.brush_preview_grids(&outside).is_none());
        assert!(renderer.brush_preview_layer(&outside, &draw).is_none());
        let after = renderer
            .brush_preview_pane
            .display_texture
            .as_ref()
            .expect("the texture stays")
            .texture()
            .clone();
        assert!(after == before, "the texture is the previous one");
        assert_eq!(renderer.brush_preview_pane.image().size(), image.size());
    }

    #[test]
    fn retained_query_falls_back_to_the_backdrops_then_to_nothing() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let mut renderer = renderer_with(shared, &fixture_512_frame());
        let published = publish_all(&mut renderer);
        let tier = |renderer: &CanvasRenderer, spec: &BrushPreviewSpec| {
            renderer.brush_preview_grids(spec).map(|grids| grids.tier)
        };

        let inside = brush(232, 232, 10.0, BrushMode::Lighten);
        let grids = renderer.brush_preview_grids(&inside).expect("answers");
        assert_eq!(grids.tier, PreviewTier::Exact);
        assert_eq!(grids.result.samples, published.composite);
        assert_eq!(grids.source.samples, published.frame);

        // 14 px from the exact `src` edge: Lighten fits, Detail's reach does not.
        let near_edge = |mode| brush(150, 232, 10.0, mode);
        assert_eq!(
            tier(&renderer, &near_edge(BrushMode::Lighten)),
            Some(PreviewTier::Exact)
        );
        let detail = near_edge(BrushMode::Detail);
        let grids = renderer.brush_preview_grids(&detail).expect("a backdrop");
        assert_eq!(grids.tier, PreviewTier::Backdrop);

        let outside = brush(20, 20, 5.0, BrushMode::Lighten);
        let grids = renderer.brush_preview_grids(&outside).expect("a backdrop");
        assert_eq!(grids.tier, PreviewTier::Backdrop);
        assert_eq!(grids.result.samples, published.composite_backdrop);
        assert_eq!(grids.source.samples, published.frame_backdrop);
        assert_eq!(
            (grids.result.step.get(), grids.source.step.get()),
            (8, 4),
            "each half keeps its own step"
        );
        assert_eq!(tuple(grids.result.rect), (0, 0, SIDE, SIDE));
        assert_eq!(tuple(grids.source.rect), (0, 0, SIDE, SIDE));
        assert_ne!(published.frame, published.frame_backdrop);

        // No backdrops: a brush outside the exact panes finds nothing, and so
        // does one whose backdrop pair is incomplete.
        let mut exact_only = renderer_with(shared, &fixture_512_frame());
        let visible = draw_of((200, 200, 64, 64), 1);
        let job = exact_only.request_frame(&visible).expect("frame job");
        publish_pattern(&mut exact_only, &job, 1);
        let job = exact_only
            .request_planes(&visible, None, None)
            .expect("job");
        publish_pattern(&mut exact_only, &job, 2);
        assert_eq!(tier(&exact_only, &inside), Some(PreviewTier::Exact));
        assert_eq!(tier(&exact_only, &outside), None);
        let job = exact_only
            .request_plane_backdrop(&draw_of((0, 0, SIDE, SIDE), 8), None, None)
            .expect("composite backdrop job");
        publish_pattern(&mut exact_only, &job, 4);
        assert_eq!(tier(&exact_only, &outside), None, "no frame backdrop");
    }

    #[test]
    fn a_reset_or_stale_job_leaves_no_retained_samples() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let spec = brush(232, 232, 10.0, BrushMode::Lighten);
        let other_frame = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/mantis-512/frames/01.tif");
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let other_planes = scratch.path().join("composite.tif");
        std::fs::copy(composite_file(), &other_planes).expect("the composite copies");

        type Reset<'a> = Box<dyn Fn(&mut CanvasRenderer) + 'a>;
        let resets: [(&str, Reset, [bool; 4]); 5] = [
            (
                "set_frame",
                Box::new(|r| {
                    r.set_frame(&other_frame, None, crate::job::host_budget())
                        .expect("frame opens");
                }),
                [false, true, false, true],
            ),
            (
                "clear_frame",
                Box::new(|r| r.clear_frame()),
                [false, true, false, true],
            ),
            (
                "set_planes",
                Box::new(|r| r.set_planes(&other_planes, None).expect("composite opens")),
                [true, false, true, false],
            ),
            (
                "clear_planes",
                Box::new(|r| r.clear_planes()),
                [true, false, true, false],
            ),
            ("detach_device", Box::new(|r| r.detach_device()), [false; 4]),
        ];
        for (name, reset, kept) in resets {
            let mut renderer = renderer_with(shared, &fixture_512_frame());
            publish_all(&mut renderer);
            assert!(retained_lengths(&renderer).iter().all(|&len| len > 0));
            assert!(renderer.brush_preview_grids(&spec).is_some(), "{name}");
            reset(&mut renderer);
            let lengths = retained_lengths(&renderer);
            for (i, (len, keep)) in lengths.iter().zip(kept).enumerate() {
                assert_eq!(
                    *len > 0,
                    keep,
                    "{name}: texture {i} (frame, composite, backdrops)"
                );
            }
            assert!(renderer.brush_preview_grids(&spec).is_none(), "{name}");
        }

        // A job issued before the reset and published after it keeps nothing.
        let visible = draw_of((200, 200, 64, 64), 1);
        let mut renderer = renderer_with(shared, &fixture_512_frame());
        let frame_job = renderer.request_frame(&visible).expect("frame job");
        let frame_backdrop_job = renderer
            .request_frame_backdrop(&draw_of((0, 0, SIDE, SIDE), 4))
            .expect("frame backdrop job");
        let composite_job = renderer.request_planes(&visible, None, None).expect("job");
        renderer
            .set_frame(&other_frame, None, crate::job::host_budget())
            .expect("frame opens");
        renderer.clear_planes();
        for job in [&frame_job, &frame_backdrop_job, &composite_job] {
            let (width, height) = job.draw.extent();
            let samples = pattern(width as usize * height as usize * 3, 5);
            assert!(renderer.publish(job, &samples).is_none(), "{:?}", job.pane);
        }
        assert_eq!(retained_lengths(&renderer), [0; 4]);

        // A job a newer one superseded is refused too.
        let mut renderer = renderer_with(shared, &fixture_512_frame());
        let old = renderer.request_frame(&visible).expect("frame job");
        let newer = renderer
            .request_frame(&draw_of((300, 300, 64, 64), 1))
            .expect("frame job");
        let kept = publish_pattern(&mut renderer, &newer, 6);
        let stale = pattern(
            old.draw.extent().0 as usize * old.draw.extent().1 as usize * 3,
            7,
        );
        assert!(renderer.publish(&old, &stale).is_none());
        assert_eq!(renderer.frame_pane.retained, kept);
    }

    #[test]
    fn retained_query_clips_at_the_image_edge_and_needs_a_publish() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        // Exact panes at the top-left and the bottom-right corner.
        for (corner, spec_at) in [(0, 3), (448, 508)] {
            let mut renderer = renderer_with(shared, &fixture_512_frame());
            let visible = draw_of((corner, corner, 64, 64), 1);
            let job = renderer.request_frame(&visible).expect("frame job");
            publish_pattern(&mut renderer, &job, 1);
            let job = renderer.request_planes(&visible, None, None).expect("job");
            publish_pattern(&mut renderer, &job, 2);
            for mode in [BrushMode::Lighten, BrushMode::Detail] {
                let spec = brush(spec_at, spec_at, 12.0, mode);
                let grids = renderer.brush_preview_grids(&spec).unwrap_or_else(|| {
                    panic!("{mode:?} at {spec_at}: clipped to the image, still answered")
                });
                assert_eq!(grids.tier, PreviewTier::Exact);
            }
        }

        // No device: nothing to publish, nothing to answer.
        let mut no_device = CanvasRenderer::new();
        no_device
            .set_frame(&fixture_512_frame(), None, crate::job::host_budget())
            .expect("frame opens");
        no_device
            .set_planes(&composite_file(), None)
            .expect("composite opens");
        let spec = brush(232, 232, 10.0, BrushMode::Lighten);
        assert!(no_device.brush_preview_grids(&spec).is_none());
        assert!(
            no_device
                .request_frame(&draw_of((200, 200, 64, 64), 1))
                .is_none()
        );

        // A publish that returns `None` retains nothing.
        let mut with_device = renderer_with(shared, &fixture_512_frame());
        let job = with_device
            .request_frame(&draw_of((200, 200, 64, 64), 1))
            .expect("frame job");
        let samples = pattern(192 * 192 * 3, 8);
        assert!(no_device.publish(&job, &samples).is_none());
        assert_eq!(retained_lengths(&no_device), [0; 4]);
        assert!(
            with_device.brush_preview_grids(&spec).is_none(),
            "no publish yet"
        );
    }

    #[test]
    fn retained_composite_ignores_the_overlay_and_follows_the_generation() {
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        let mut renderer = renderer_with(shared, &fixture_512_frame());
        let overlay = crate::canvas::OverlayDraw::for_opacity(8, 0.5);
        let mut tiles = crate::canvas::ResidentTiles::new(16, SIDE, SIDE, 3);
        let visible = draw_of((200, 200, 64, 64), 1);
        let mut publish = |tiles: &crate::canvas::ResidentTiles, seed: u32| {
            let job = renderer
                .request_planes(&visible, Some(tiles.generation()), Some(&overlay))
                .expect("composite job");
            assert!(job.state.overlay.is_some());
            let (width, height) = job.draw.extent();
            let texels = width as usize * height as usize;
            let sealed = pattern(texels * 3, seed);
            let colors = vec![[255u8, 0, 0]; texels];
            let view = EditedView { tiles };
            assert!(
                renderer
                    .publish_edited(&job, &sealed, &colors, Some(&view))
                    .is_some()
            );
            let (x, y, w, h) = job.draw.src;
            let mut merged = sealed.clone();
            crate::canvas::overlay_edited(
                &mut merged,
                Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
                job.draw.step,
                3,
                tiles,
            );
            (sealed, merged, renderer.composite_pane.retained.clone())
        };

        tiles.install(
            Rect {
                x: 224,
                y: 224,
                width: 8,
                height: 8,
            },
            &vec![65_000u16; 8 * 8 * 3],
        );
        let (sealed, merged, retained) = publish(&tiles, 1);
        assert_ne!(merged, sealed);
        assert_eq!(retained, merged, "the merged samples, not overlay colours");

        tiles.install(
            Rect {
                x: 240,
                y: 240,
                width: 8,
                height: 8,
            },
            &vec![1_000u16; 8 * 8 * 3],
        );
        let (_, newer, retained) = publish(&tiles, 1);
        assert_ne!(newer, merged);
        assert_eq!(retained, newer, "a newer generation replaces the copy");
    }

    /// Runs `body`, re-raising a failure with `name` in front of its message.
    fn block(name: &str, body: impl FnOnce()) {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(ToString::to_string))
                .unwrap_or_default();
            panic!("{name}: {message}");
        }
    }

    /// One child, one device: timing accounting and device loss are
    /// process-global, so the blocks share the child and the device-loss block
    /// runs last.
    #[test]
    fn canvas_device_blocks_run_in_one_isolated_child() {
        if !in_isolated_child("canvas_device_blocks_run_in_one_isolated_child") {
            return;
        }
        focusforge_core::timing::enable_trace_accounting();
        let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
            return;
        };
        block("import failure", || {
            import_failure_keeps_the_previous_image_and_records_the_spec(shared)
        });
        block("lifecycle", || {
            lifecycle_keeps_readers_and_rebuilds_gpu_draws(shared)
        });
        block("guarded resize", || {
            a_guarded_resize_or_source_replacement_releases_the_superseded_texture(shared)
        });
        block("attach accounting", || {
            each_attach_builds_one_display_renderer_whose_source_buffer_follows_guarded_draws(
                shared,
            )
        });
        block("lost device", || {
            a_lost_device_creates_no_texture_buffer_or_render(shared)
        });
    }
}

/// What can go wrong opening the composite a [`CanvasRenderer`] holds open
/// for the life of the loaded project.
#[derive(Debug, thiserror::Error)]
pub enum CanvasRendererError {
    #[error("failed to open composite {}: {source}", path.display())]
    Composite {
        path: PathBuf,
        // Boxed: `TiffError` is larger on Windows, where an `OsString` is 32 bytes.
        #[source]
        source: Box<TiffError>,
    },
}

/// An open plane file as [`CanvasRenderer::set_planes`] tells two apart: its
/// path, size and last write time.
#[derive(Clone, PartialEq)]
struct PlaneFile {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl PlaneFile {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
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
    brush_preview: Option<BrushPreviewRenderer>,
    /// The texture [`Self::brush_preview_layer`] draws into.
    brush_preview_pane: PaneTexture,
    sample_scratch: Vec<u16>,
    composite: Option<Arc<TiffStack>>,
    zmap: Option<Arc<ZMapReader>>,
    /// The files [`Self::set_planes`] last opened.
    planes: Option<(PlaneFile, Option<PlaneFile>)>,
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
            brush_preview: None,
            brush_preview_pane: PaneTexture::default(),
            sample_scratch: Vec::new(),
            composite: None,
            zmap: None,
            planes: None,
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
        self.brush_preview = Some(BrushPreviewRenderer::new(&device));
        self.device = Some(device);
        self.queue = Some(queue);
    }

    /// Releases resources bound to Slint's device while keeping source readers.
    pub fn detach_device(&mut self) {
        self.reset_planes();
        self.reset_frame();
        self.display_renderer = None;
        self.brush_preview = None;
        self.brush_preview_pane = PaneTexture::default();
        self.device = None;
        self.queue = None;
    }

    fn device_queue(&self) -> Option<(wgpu::Device, wgpu::Queue)> {
        Some((self.device.as_ref()?.clone(), self.queue.as_ref()?.clone()))
    }

    /// Opens `composite`, and `zmap` when given, as this renderer's planes,
    /// replacing whichever were open before. Files already open (same paths,
    /// sizes and write times) stay as they are, with the reads in flight for
    /// them still valid. A composite that fails to open is an error naming
    /// it, leaving the previous planes untouched. A Z-Map that fails to open
    /// is logged and left closed: the composite draws without it, and only
    /// the overlay is lost.
    pub fn set_planes(
        &mut self,
        composite_path: &Path,
        zmap_path: Option<&Path>,
    ) -> Result<(), CanvasRendererError> {
        let files = PlaneFile::of(composite_path)
            .map(|composite| (composite, zmap_path.and_then(PlaneFile::of)));
        if files.is_some() && files == self.planes {
            return Ok(());
        }
        let composite = TiffStack::open_staged(composite_path).map_err(|source| {
            CanvasRendererError::Composite {
                path: composite_path.to_path_buf(),
                source: Box::new(source),
            }
        })?;
        self.zmap = zmap_path.and_then(|path| match ZMapReader::open(path) {
            Ok(zmap) => Some(Arc::new(zmap)),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "the Z-Map could not be opened");
                None
            }
        });
        self.composite = Some(Arc::new(composite));
        self.planes = files;
        self.reset_planes();
        Ok(())
    }

    /// Drops the open planes, leaving the frame half untouched.
    pub fn clear_planes(&mut self) {
        self.composite = None;
        self.zmap = None;
        self.planes = None;
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
        let indices = overlay.and(self.zmap.clone());
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
        indices: Option<Arc<ZMapReader>>,
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
    /// the pane's image. `colors` is the overlay's colour per texel (empty
    /// when the job has no overlay). `edited`, present on a job
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
        colors: &[[u8; 3]],
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
        let merged = match job.pane {
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
                match &job.state.overlay {
                    Some(overlay) if !colors.is_empty() => {
                        texture.ensure_and_render_overlay(
                            &device,
                            &queue,
                            width,
                            height,
                            composite,
                            colors,
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
                merged
            }
            _ => {
                texture.ensure_and_render(
                    renderer,
                    &device,
                    &queue,
                    width,
                    height,
                    samples,
                    samples_per_pixel,
                );
                None
            }
        };
        texture.retain(merged, samples, samples_per_pixel);
        texture.mark_covered(job.draw.src, job.draw.step, job.state.clone());
        Some(texture.image())
    }

    /// The result and source samples behind a brush preview at `spec`: the
    /// pixels the panes last published, read on the calling thread with no new
    /// file read and no allocation. The exact panes answer when both retained
    /// grids cover [`brush_preview_bounds`] at the composite's step, else the
    /// two backdrops do, else `None`. Each half keeps its own source rect,
    /// step and samples per pixel; the image is the composite's.
    pub fn brush_preview_grids(&self, spec: &BrushPreviewSpec) -> Option<PreviewGrids<'_>> {
        Self::preview_grids(
            self.source_size(Pane::Composite)?,
            [
                &self.composite_pane,
                &self.frame_pane,
                &self.composite_backdrop,
                &self.frame_backdrop,
            ],
            spec,
        )
    }

    /// [`Self::brush_preview_grids`] over the four panes: the composite and
    /// frame panes, then their backdrops.
    fn preview_grids<'a>(
        image_size: (u32, u32),
        [composite, frame, composite_backdrop, frame_backdrop]: [&'a PaneTexture; 4],
        spec: &BrushPreviewSpec,
    ) -> Option<PreviewGrids<'a>> {
        [
            (PreviewTier::Exact, composite, frame),
            (PreviewTier::Backdrop, composite_backdrop, frame_backdrop),
        ]
        .into_iter()
        .find_map(|(tier, result, source)| {
            let (result, source) = (result.retained_grid()?, source.retained_grid()?);
            let bounds = brush_preview_bounds(image_size, spec, result.step)?;
            let bounds = (bounds.x, bounds.y, bounds.width, bounds.height);
            let covers = |grid: &BrushPreviewGrid| {
                let rect = grid.rect;
                rect_contains((rect.x, rect.y, rect.width, rect.height), bounds)
            };
            (covers(&result) && covers(&source)).then_some(PreviewGrids {
                tier,
                image_size,
                result,
                source,
            })
        })
    }

    /// The brush-preview layer of `spec` over the panes' last published
    /// pixels, drawn on the calling thread into a texture reused while the
    /// texel size holds, with its place under `draw`. It submits and returns:
    /// no readback, no wait. `None`, the previous image untouched, when the
    /// panes lack the brush's pixels, the kernel has no layer or refuses the
    /// brush, or the device is missing or lost.
    pub fn brush_preview_layer(
        &mut self,
        spec: &BrushPreviewSpec,
        draw: &PaneDraw,
    ) -> Option<(slint::Image, (f32, f32, f32, f32))> {
        let _scope = measurement_scope("gui_brush_preview_layer");
        let (device, queue) = self.device_queue()?;
        let image_size = self.source_size(Pane::Composite)?;
        let Self {
            composite_pane,
            frame_pane,
            composite_backdrop,
            frame_backdrop,
            brush_preview,
            brush_preview_pane,
            ..
        } = self;
        let renderer = brush_preview.as_mut()?;
        let grids = Self::preview_grids(
            image_size,
            [
                composite_pane,
                frame_pane,
                composite_backdrop,
                frame_backdrop,
            ],
            spec,
        )?;
        let max_binding = device.limits().max_storage_buffer_binding_size;
        let planned = match plan_brush_preview(
            grids.image_size,
            &grids.result,
            &grids.source,
            spec,
            BRUSH_PREVIEW_STRENGTH_Q16,
            max_binding,
        ) {
            Ok(planned) => planned?,
            Err(refusal) => {
                tracing::debug!(%refusal, "no brush preview layer");
                return None;
            }
        };
        let layout = planned.layout;
        let texture = brush_preview_pane.ensure_texture(&device, layout.width, layout.height)?;
        renderer.render(
            &device,
            &queue,
            texture,
            &planned,
            &grids.result,
            &grids.source,
        );

        let (rect, step) = (grids.result.rect, grids.result.step.get());
        let (x, y) = (rect.x + layout.column * step, rect.y + layout.row * step);
        let clipped = |from: u32, cells: u32, end: u32| (cells * step).min(end - from);
        let pixels = (
            x,
            y,
            clipped(x, layout.width, rect.x + rect.width),
            clipped(y, layout.height, rect.y + rect.height),
        );
        Some((brush_preview_pane.image(), draw.place(pixels)))
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
            .and_then(|()| {
                let colors = job.overlay_colors(&indices);
                self.publish_edited(&job, &scratch, &colors, edited)
            });
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
