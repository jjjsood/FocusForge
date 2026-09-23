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
/// `TouchArea::mouse-x` reports in. Both panes are always the same size,
/// differing only in `x`/`y` (`ui/shell/canvas.slint`), so every function
/// below that takes a `PaneRect` only reads its `width`/`height`; the
/// controller (`crate::controller::AppController::anchor`) is what picks
/// *which* pane's `x`/`y` a raw cursor position is relative to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The loaded project's composite/Z-Map size, in source pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

/// `scale` is screen logical px per source px; `origin` is the source-space
/// point drawn at the pane's **top-left**. Offset-form rather than
/// centre-form because every rule below (clamp, the visible rect, the
/// anchor) is one subtraction in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    scale: f64,
    origin: (f64, f64),
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

/// Wheel zoom moves through this fixed stop ladder.
pub const FIXED_STOPS: [f64; 11] = [0.125, 0.25, 0.33, 0.50, 0.67, 1.0, 1.5, 2.0, 3.0, 4.0, 5.0];

/// The one step below fit -- a halving is unmistakable as a deliberate step
/// rather than a rendering wobble.
pub const FIT_STEP_DOWN: f64 = 0.5;

/// `[fit * FIT_STEP_DOWN, fit]` followed by every `FIXED_STOPS` entry
/// strictly greater than `fit`, ascending. A fixed stop *below* fit is
/// dropped, so the minimum is always `fit * FIT_STEP_DOWN`.
pub fn stops(image: ImageSize, pane: PaneRect) -> Vec<f64> {
    let fit = fit_scale(image, pane);
    let mut out = vec![fit * FIT_STEP_DOWN, fit];
    out.extend(FIXED_STOPS.iter().copied().filter(|&s| s > fit));
    out
}

/// A viewport showing the whole image, centred in `pane`.
pub fn fit(image: ImageSize, pane: PaneRect) -> Viewport {
    let scale = fit_scale(image, pane);
    let origin = centred_origin(image, pane, scale);
    Viewport {
        scale,
        origin,
        at_fit: true,
    }
}

/// The origin that centres the visible rect (`pane.size / scale`) over
/// `image` on both axes -- shared by [`fit`] and [`Viewport::clamp`], whose
/// "image smaller than the pane" branch is the same formula.
fn centred_origin(image: ImageSize, pane: PaneRect, scale: f64) -> (f64, f64) {
    let visible_w = pane.width as f64 / scale;
    let visible_h = pane.height as f64 / scale;
    (
        (image.width as f64 - visible_w) / 2.0,
        (image.height as f64 - visible_h) / 2.0,
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
    /// image-y/image-w/image-h`. The two panes share one viewport, not one
    /// image extent: each is this same viewport's `scale`/`origin` drawn
    /// against that pane's own [`ImageSize`], so their own `x`/`y` (applied
    /// by Slint's own layout, not here) and their own `width`/`height` can
    /// both differ.
    pub dst: (f32, f32, f32, f32),
    /// `pixelated` at or above 100 %, `smooth` below it.
    pub pixelated: bool,
    /// Every `step`-th source row and column of `src` is read:
    /// `max(1, floor(1 / scale))`, so a zoomed-out draw reads at least one
    /// sample per pane pixel and under two per axis, and 1 at or above 100 %.
    pub step: NonZeroU32,
}

impl PaneDraw {
    /// The decimated texture size, `src`'s sides divided by `step`, rounded
    /// up. `extent * step` overshoots `src` by under one step, which is under
    /// one logical pixel once stretched over `dst`.
    pub fn extent(&self) -> (u32, u32) {
        let step = self.step.get();
        (self.src.2.div_ceil(step), self.src.3.div_ceil(step))
    }
}

impl Viewport {
    /// Screen logical px per source px.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// The source-space point drawn at the pane's top-left.
    pub fn origin(&self) -> (f64, f64) {
        self.origin
    }

    /// The anchor-exact wheel zoom: `delta` ladder positions from the
    /// current one (saturating at both ends), `anchor` (pane-local) held
    /// fixed in source space across the change.
    pub fn zoom_steps(&mut self, delta: i32, anchor: (f32, f32), image: ImageSize, pane: PaneRect) {
        let ladder = stops(image, pane);
        if ladder.is_empty() {
            return;
        }
        let idx = ladder_index(&ladder, self.scale);
        let new_idx = (idx as i32 + delta).clamp(0, ladder.len() as i32 - 1) as usize;
        let new_scale = ladder[new_idx];

        let ax = anchor.0 as f64;
        let ay = anchor.1 as f64;
        let src = (
            self.origin.0 + ax / self.scale,
            self.origin.1 + ay / self.scale,
        );
        self.scale = new_scale;
        self.origin = (src.0 - ax / new_scale, src.1 - ay / new_scale);
        self.at_fit = false;
        self.clamp(image, pane);
    }

    /// A screen-pixel pan. Sign convention: the image follows the pointer,
    /// so a drag right moves the view left.
    pub fn pan_by(&mut self, dx: f32, dy: f32, image: ImageSize, pane: PaneRect) {
        self.origin.0 -= dx as f64 / self.scale;
        self.origin.1 -= dy as f64 / self.scale;
        self.at_fit = false;
        self.clamp(image, pane);
    }

    /// Per axis: when the visible rect is at least as big as the image, the
    /// image is centred and un-pannable on that axis; otherwise `origin` is
    /// clamped into `[0, image - visible]`. "Panning cannot lose the image
    /// off-screen" in two lines.
    fn clamp(&mut self, image: ImageSize, pane: PaneRect) {
        let visible_w = pane.width as f64 / self.scale;
        let visible_h = pane.height as f64 / self.scale;
        let (cx, cy) = centred_origin(image, pane, self.scale);
        self.origin.0 = if visible_w >= image.width as f64 {
            cx
        } else {
            self.origin.0.clamp(0.0, image.width as f64 - visible_w)
        };
        self.origin.1 = if visible_h >= image.height as f64 {
            cy
        } else {
            self.origin.1.clamp(0.0, image.height as f64 - visible_h)
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
        stops(image, pane)
            .first()
            .is_some_and(|&s| (s - self.scale).abs() < 1e-9)
    }

    /// Whether `scale` is the ladder's last entry (the maximum: 500 % when
    /// fit does not already exceed it).
    pub fn at_max(&self, image: ImageSize, pane: PaneRect) -> bool {
        stops(image, pane)
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
        let visible_w = pane.width as f64 / self.scale;
        let visible_h = pane.height as f64 / self.scale;

        let x0 = self.origin.0.max(0.0);
        let y0 = self.origin.1.max(0.0);
        let x1 = (self.origin.0 + visible_w).min(image.width as f64);
        let y1 = (self.origin.1 + visible_h).min(image.height as f64);
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
            ((sx0 - self.origin.0) * self.scale) as f32,
            ((sy0 - self.origin.1) * self.scale) as f32,
            ((sx1 - sx0) * self.scale) as f32,
            ((sy1 - sy0) * self.scale) as f32,
        );
        // The epsilon lands the 50 % and 25 % stops on exactly 2 and 4.
        let step = (1.0 / self.scale + 1e-6).floor().max(1.0) as u32;
        Some(PaneDraw {
            src,
            dst,
            pixelated: self.scale >= 1.0,
            step: NonZeroU32::new(step).expect("step is clamped to at least 1"),
        })
    }

    /// Sticky fit: a viewport that was exactly at fit
    /// re-fits to the new pane; otherwise the scale is kept -- raised to the
    /// new minimum if the new pane made it fall below one -- and the offset
    /// is re-clamped.
    pub fn resize(&mut self, image: ImageSize, pane: PaneRect) {
        if self.at_fit {
            *self = fit(image, pane);
            return;
        }
        if let Some(&min) = stops(image, pane).first()
            && self.scale < min
        {
            self.scale = min;
        }
        self.clamp(image, pane);
    }
}
