// SPDX-License-Identifier: GPL-3.0-or-later

//! The viewport as pure arithmetic: no `use slint::`, no `use wgpu::`, and
//! deliberately no dependency on the core algorithm crate --
//! `checkpoint_panel_touches_no_storage` scans every `src/*.rs` outside a
//! four-file allow-list for that crate's name,
//! and this module staying free of it keeps the allow-list from growing.
//! Its tile-rect type is constructed at the `crate::canvas` seam, not here.
//!
//! `f64` throughout: at 500 % over 8192 px the `f32` ulp is already
//! ~0.0005 source px, and the anchor criterion is stated in whole screen
//! pixels, so `f64` costs nothing and removes the question.

use std::num::NonZeroU32;

/// One pane in canvas-ground logical pixels -- the coordinate space
/// `TouchArea::mouse-x` reports in. A shared viewport projects its source
/// centre into each pane's own centre, so only `width` and `height` affect
/// the arithmetic here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Device pixels per logical pixel of the window the pane is in; a
    /// non-finite or non-positive value reads as 1.
    pub scale_factor: f32,
}

/// The pane's device scale factor, or 1 when it is not a positive finite number.
fn device_scale(pane: PaneRect) -> f64 {
    let factor = f64::from(pane.scale_factor);
    if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    }
}

/// The loaded project's composite/Z-Map size, in source pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

/// `scale` is screen logical px per source px; `centre` is the source-space
/// point drawn at every pane's geometric centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    scale: f64,
    centre: (f64, f64),
    /// Whether this viewport is exactly the one `fit` last produced and has
    /// not been zoomed or panned away from since -- distinct from "does
    /// `scale` currently equal `fit_scale`", which a zoom can land on by
    /// coincidence. `resize` is the only reader, for sticky fit.
    at_fit: bool,
}

/// `min(pane.w / img.w, pane.h / img.h)` -- the scale at which the whole
/// image fits inside `pane` on its constraining axis.
pub fn fit_scale(image: ImageSize, pane: PaneRect) -> f64 {
    let sx = pane.width as f64 / image.width as f64;
    let sy = pane.height as f64 / image.height as f64;
    sx.min(sy)
}

/// The whole image read at the smallest step whose decimated extent fits
/// `pane` on both axes -- the low-detail view that backs a pane while its
/// exact texture covers only part of the image. Only `src` and `step` are
/// meaningful: the texture is placed through the viewport's own
/// [`PaneDraw::place`], never through this draw's `dst`, which is the
/// texture's own extent. `None` for an empty image or pane.
pub fn backdrop_draw(image: ImageSize, pane: PaneRect) -> Option<PaneDraw> {
    if image.width == 0 || image.height == 0 || pane.width <= 0.0 || pane.height <= 0.0 {
        return None;
    }
    let device = device_scale(pane);
    let axis = |image: u32, pane: f32| (image as f64 / (pane as f64 * device)).ceil() as u32;
    let step = axis(image.width, pane.width)
        .max(axis(image.height, pane.height))
        .max(1);
    let step = NonZeroU32::new(step).expect("step is clamped to at least 1");
    let draw = PaneDraw {
        src: (0, 0, image.width, image.height),
        dst: (0.0, 0.0, 0.0, 0.0),
        pixelated: false,
        step,
        crop: (0.0, 0.0, 0.0, 0.0),
    };
    let (width, height) = draw.extent();
    Some(PaneDraw {
        dst: (0.0, 0.0, width as f32, height as f32),
        crop: (0.0, 0.0, width as f32, height as f32),
        ..draw
    })
}

/// The shared Fit scale: the smaller pane Fit keeps the complete image in
/// both panes.
pub fn fit_scale_two(image: ImageSize, left: PaneRect, right: PaneRect) -> f64 {
    fit_scale(image, left).min(fit_scale(image, right))
}

/// Wheel zoom moves through this fixed stop ladder.
pub const FIXED_STOPS: [f64; 11] = [0.125, 0.25, 0.33, 0.50, 0.67, 1.0, 1.5, 2.0, 3.0, 4.0, 5.0];

/// The one step below fit -- a halving is unmistakable as a deliberate step
/// rather than a rendering wobble.
pub const FIT_STEP_DOWN: f64 = 0.5;

/// `[fit * FIT_STEP_DOWN, fit]` followed by every `FIXED_STOPS` entry
/// strictly greater than `fit`, ascending. A fixed stop *below* fit is
/// dropped, so the minimum is always `fit * FIT_STEP_DOWN`.
pub fn stops(image: ImageSize, pane: PaneRect) -> Vec<f64> {
    stops_two(image, pane, pane)
}

/// The shared wheel ladder for two panes.
pub fn stops_two(image: ImageSize, left: PaneRect, right: PaneRect) -> Vec<f64> {
    let fit = fit_scale_two(image, left, right);
    let mut out = vec![fit * FIT_STEP_DOWN, fit];
    out.extend(FIXED_STOPS.iter().copied().filter(|&s| s > fit));
    out
}

/// A viewport showing the whole image in one pane.
pub fn fit(image: ImageSize, pane: PaneRect) -> Viewport {
    fit_two(image, pane, pane)
}

/// A viewport showing the whole image in both panes.
pub fn fit_two(image: ImageSize, left: PaneRect, right: PaneRect) -> Viewport {
    let scale = fit_scale_two(image, left, right);
    Viewport {
        scale,
        centre: (image.width as f64 / 2.0, image.height as f64 / 2.0),
        at_fit: true,
    }
}

fn origin_for(centre: (f64, f64), pane: PaneRect, scale: f64) -> (f64, f64) {
    (
        centre.0 - pane.width as f64 / (2.0 * scale),
        centre.1 - pane.height as f64 / (2.0 * scale),
    )
}

/// The ladder index whose value is closest to `scale` -- exact when `scale`
/// was itself set from `stops(..)` (always true except right after a
/// non-sticky [`Viewport::resize`], where only the ladder's two pane-derived
/// entries can have moved).
fn ladder_index(ladder: &[f64], scale: f64) -> usize {
    ladder
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (**a - scale)
                .abs()
                .partial_cmp(&(**b - scale).abs())
                .expect("scale is never NaN")
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// The visible source rect (whole pixels) and its projection into pane-local
/// logical px, for `crate::gpu_canvas`'s re-upload and `ui/shell/canvas.slint`'s
/// `FFPane` placement properties.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneDraw {
    /// `(x, y, width, height)` in source pixels -- what `crate::canvas`
    /// hands the composite/Z-Map tile readers' stepped reads.
    pub src: (u32, u32, u32, u32),
    /// `(x, y, width, height)` in pane-local logical px -- `FFPane::image-x/
    /// image-y/image-w/image-h`. Each pane draws its own visible source rect
    /// around the shared centre at the shared scale; a pane larger than the
    /// image shows black beyond the image edge.
    pub dst: (f32, f32, f32, f32),
    /// `(x, y, width, height)` in pane-local logical px for the exact,
    /// continuous visible source rect. `FFPane` uses this to clip `dst` and
    /// retained coverage. It deliberately does not follow `src`'s outward
    /// rounding, so a subpixel pan cannot make a crop edge jump a pixel.
    pub crop: (f32, f32, f32, f32),
    /// `pixelated` at or above 100 %, `smooth` below it.
    pub pixelated: bool,
    /// Every `step`-th source row and column of `src` is read:
    /// `max(1, floor(1 / (scale * factor)))` for the pane's device scale
    /// factor, so a zoomed-out draw reads at least one sample per device pixel
    /// and under two per axis, and 1 once a source pixel covers a device pixel.
    pub step: NonZeroU32,
}

/// Extra output texels retained around a visible draw so ordinary pans can
/// reuse the current texture while the next source read remains asynchronous.
pub const DISPLAY_GUARD: u32 = 64;

fn source_rect_fits(image: ImageSize, src: (u32, u32, u32, u32)) -> bool {
    let (x, y, width, height) = src;
    width > 0
        && height > 0
        && x.checked_add(width).is_some_and(|end| end <= image.width)
        && y.checked_add(height).is_some_and(|end| end <= image.height)
}

impl PaneDraw {
    /// The decimated texture size, `src`'s sides divided by `step`, rounded
    /// up. `extent * step` overshoots `src` by under one step, which is under
    /// one logical pixel once stretched over `dst`.
    pub fn extent(&self) -> (u32, u32) {
        let step = self.step.get();
        (self.src.2.div_ceil(step), self.src.3.div_ceil(step))
    }

    /// The pane-local rect a texture covering the source rect `src` occupies
    /// under this draw's scale and offset; `place(self.src) == self.dst`.
    pub fn place(&self, src: (u32, u32, u32, u32)) -> (f32, f32, f32, f32) {
        let axis = |dst_pos: f32, dst_len: f32, own: (u32, u32), (pos, len): (u32, u32)| {
            let scale = f64::from(dst_len) / f64::from(own.1);
            let offset = (f64::from(pos) - f64::from(own.0)) * scale;
            (
                (f64::from(dst_pos) + offset) as f32,
                (f64::from(dst_len) * f64::from(len) / f64::from(own.1)) as f32,
            )
        };
        let (x, w) = axis(
            self.dst.0,
            self.dst.2,
            (self.src.0, self.src.2),
            (src.0, src.2),
        );
        let (y, h) = axis(
            self.dst.1,
            self.dst.3,
            (self.src.1, self.src.3),
            (src.1, src.3),
        );
        (x, y, w, h)
    }

    /// Expands this visible draw to a bounded source rect while keeping its
    /// output extent within `max_texture_dimension`.
    pub fn guarded(&self, image: ImageSize, max_texture_dimension: u32) -> Option<Self> {
        if !source_rect_fits(image, self.src) {
            return None;
        }
        let (texture_width, texture_height) = self.extent();
        if texture_width > max_texture_dimension || texture_height > max_texture_dimension {
            return None;
        }
        let axis = |pos: u32, len: u32, limit: u32| {
            let texture_len = len.div_ceil(self.step.get());
            let guard = DISPLAY_GUARD
                .min(max_texture_dimension.saturating_sub(texture_len) / 2)
                .saturating_mul(self.step.get());
            let guarded_len = len.saturating_add(guard.saturating_mul(2)).min(limit);
            let start = pos.saturating_sub(guard).min(limit - guarded_len);
            (start, guarded_len)
        };
        let (x, width) = axis(self.src.0, self.src.2, image.width);
        let (y, height) = axis(self.src.1, self.src.3, image.height);
        let src = (x, y, width, height);
        Some(Self {
            src,
            dst: self.place(src),
            crop: self.crop,
            pixelated: self.pixelated,
            step: self.step,
        })
    }
}

impl Viewport {
    /// Screen logical px per source px.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// The source-space point under `local` in `pane`.
    pub fn to_source_in(&self, local: (f32, f32), pane: PaneRect) -> (f64, f64) {
        let (ox, oy) = origin_for(self.centre, pane, self.scale);
        (
            ox + f64::from(local.0) / self.scale,
            oy + f64::from(local.1) / self.scale,
        )
    }

    /// The pane-local point where `source` is drawn.
    pub fn from_source_in(&self, source: (f64, f64), pane: PaneRect) -> (f32, f32) {
        let (ox, oy) = origin_for(self.centre, pane, self.scale);
        (
            ((source.0 - ox) * self.scale) as f32,
            ((source.1 - oy) * self.scale) as f32,
        )
    }

    /// The anchor-exact wheel zoom: `delta` ladder positions from the
    /// current one (saturating at both ends), `anchor` (pane-local) held
    /// fixed in source space across the change.
    pub fn zoom_steps(&mut self, delta: i32, anchor: (f32, f32), image: ImageSize, pane: PaneRect) {
        self.zoom_steps_in(delta, anchor, image, pane, pane);
    }

    /// A wheel zoom from `gesture_pane`, anchored in that pane while both
    /// pane bounds constrain the resulting shared centre.
    pub fn zoom_steps_in(
        &mut self,
        delta: i32,
        anchor: (f32, f32),
        image: ImageSize,
        gesture_pane: PaneRect,
        other_pane: PaneRect,
    ) {
        let ladder = stops_two(image, gesture_pane, other_pane);
        if ladder.is_empty() {
            return;
        }
        let idx = ladder_index(&ladder, self.scale);
        let new_idx = (idx as i32 + delta).clamp(0, ladder.len() as i32 - 1) as usize;
        let new_scale = ladder[new_idx];

        let src = self.to_source_in(anchor, gesture_pane);
        self.scale = new_scale;
        self.centre = (
            src.0 - (anchor.0 as f64 - gesture_pane.width as f64 / 2.0) / new_scale,
            src.1 - (anchor.1 as f64 - gesture_pane.height as f64 / 2.0) / new_scale,
        );
        self.at_fit = false;
        self.clamp_two(image, gesture_pane, other_pane);
    }

    /// A screen-pixel pan. Sign convention: the image follows the pointer,
    /// so a drag right moves the view left.
    pub fn pan_by(&mut self, dx: f32, dy: f32, image: ImageSize, pane: PaneRect) {
        self.pan_by_two(dx, dy, image, pane, pane);
    }

    /// A screen-pixel pan constrained by both panes.
    pub fn pan_by_two(
        &mut self,
        dx: f32,
        dy: f32,
        image: ImageSize,
        left: PaneRect,
        right: PaneRect,
    ) {
        self.centre.0 -= dx as f64 / self.scale;
        self.centre.1 -= dy as f64 / self.scale;
        self.at_fit = false;
        self.clamp_two(image, left, right);
    }

    /// Per axis: when the visible rect is at least as big as the image, the
    /// image is centred and un-pannable on that axis; otherwise `origin` is
    /// clamped into `[0, image - visible]`. "Panning cannot lose the image
    /// off-screen" in two lines.
    fn clamp_two(&mut self, image: ImageSize, left: PaneRect, right: PaneRect) {
        let visible_w = left.width.min(right.width) as f64 / self.scale;
        let visible_h = left.height.min(right.height) as f64 / self.scale;
        self.centre.0 = if visible_w >= image.width as f64 {
            image.width as f64 / 2.0
        } else {
            self.centre
                .0
                .clamp(visible_w / 2.0, image.width as f64 - visible_w / 2.0)
        };
        self.centre.1 = if visible_h >= image.height as f64 {
            image.height as f64 / 2.0
        } else {
            self.centre
                .1
                .clamp(visible_h / 2.0, image.height as f64 - visible_h / 2.0)
        };
    }

    /// Percent with one decimal only when the value is not whole --
    /// `100 %`, `12.5 %`, `500 %`.
    pub fn readout(&self) -> String {
        let pct = self.scale * 100.0;
        if (pct - pct.round()).abs() < 1e-6 {
            format!("{} %", pct.round() as i64)
        } else {
            format!("{pct:.1} %")
        }
    }

    /// Whether `scale` is the ladder's first entry (the minimum: fit-to-window
    /// with one step below it).
    pub fn at_min(&self, image: ImageSize, pane: PaneRect) -> bool {
        self.at_min_two(image, pane, pane)
    }

    /// Whether the shared scale is at its shared ladder minimum.
    pub fn at_min_two(&self, image: ImageSize, left: PaneRect, right: PaneRect) -> bool {
        stops_two(image, left, right)
            .first()
            .is_some_and(|&s| (s - self.scale).abs() < 1e-9)
    }

    /// Whether `scale` is the ladder's last entry (the maximum: 500 % when
    /// fit does not already exceed it).
    pub fn at_max(&self, image: ImageSize, pane: PaneRect) -> bool {
        self.at_max_two(image, pane, pane)
    }

    /// Whether the shared scale is at its shared ladder maximum.
    pub fn at_max_two(&self, image: ImageSize, left: PaneRect, right: PaneRect) -> bool {
        stops_two(image, left, right)
            .last()
            .is_some_and(|&s| (s - self.scale).abs() < 1e-9)
    }

    /// The visible source rect, intersected with the image and expanded
    /// outward to whole pixels, plus its pane-local projection. `None` when
    /// the visible rect and the image do not overlap at all (never reached
    /// through `zoom_steps`/`pan_by`/`clamp`, which keep them overlapping by
    /// construction; guarded here rather than assumed).
    pub fn draw(&self, image: ImageSize, pane: PaneRect) -> Option<PaneDraw> {
        if image.width == 0 || image.height == 0 || pane.width <= 0.0 || pane.height <= 0.0 {
            return None;
        }
        let (origin_x, origin_y) = origin_for(self.centre, pane, self.scale);
        let visible_w = pane.width as f64 / self.scale;
        let visible_h = pane.height as f64 / self.scale;

        let x0 = origin_x.max(0.0);
        let y0 = origin_y.max(0.0);
        let x1 = (origin_x + visible_w).min(image.width as f64);
        let y1 = (origin_y + visible_h).min(image.height as f64);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }

        let sx0 = x0.floor();
        let sy0 = y0.floor();
        let sx1 = x1.ceil().min(image.width as f64);
        let sy1 = y1.ceil().min(image.height as f64);

        let src = (
            sx0 as u32,
            sy0 as u32,
            (sx1 - sx0) as u32,
            (sy1 - sy0) as u32,
        );
        let dst = (
            ((sx0 - origin_x) * self.scale) as f32,
            ((sy0 - origin_y) * self.scale) as f32,
            ((sx1 - sx0) * self.scale) as f32,
            ((sy1 - sy0) * self.scale) as f32,
        );
        let crop = (
            ((x0 - origin_x) * self.scale) as f32,
            ((y0 - origin_y) * self.scale) as f32,
            ((x1 - x0) * self.scale) as f32,
            ((y1 - y0) * self.scale) as f32,
        );
        // One sample per device pixel; the epsilon lands the 50 % and 25 %
        // stops on exactly 2 and 4 at factor 1.
        let step = (1.0 / (self.scale * device_scale(pane)) + 1e-6)
            .floor()
            .max(1.0) as u32;
        Some(PaneDraw {
            src,
            dst,
            crop,
            pixelated: self.scale >= 1.0,
            step: NonZeroU32::new(step).expect("step is clamped to at least 1"),
        })
    }

    /// Sticky fit: a viewport that was exactly at fit
    /// re-fits to the new pane; otherwise the scale is kept -- raised to the
    /// new minimum if the new pane made it fall below one -- and the offset
    /// is re-clamped.
    pub fn resize(&mut self, image: ImageSize, pane: PaneRect) {
        self.resize_two(image, pane, pane);
    }

    /// Reconciles a shared viewport after either pane changes geometry.
    pub fn resize_two(&mut self, image: ImageSize, left: PaneRect, right: PaneRect) {
        if self.at_fit {
            *self = fit_two(image, left, right);
            return;
        }
        if let Some(&min) = stops_two(image, left, right).first()
            && self.scale < min
        {
            self.scale = min;
        }
        self.clamp_two(image, left, right);
    }
}
