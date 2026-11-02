// SPDX-License-Identifier: GPL-3.0-or-later

//! Small progress images of a running stack: the frame whose registration
//! just finished, the composite growing tile by tile and its sharpness
//! heatmap, requested per kind through [`PreviewKinds`]. Workers only write
//! shared state under a short lock; the poll thread's
//! [`PreviewFeed::deliver`] hands out at most one changed image per kind.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::align::AlignPyramid;
use crate::align::Transform;
use crate::display::{Downsampler, reduce16to8};
use crate::tile::Rect;

/// The longest edge, in pixels, of any preview image.
const PREVIEW_LONG_EDGE: u32 = 512;

/// Which previews a front end wants. All off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewKinds {
    pub aligned_frame: bool,
    pub composite: bool,
    pub heat: bool,
}

/// What a [`StagePreview`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    /// The frame at this index, warped into the reference frame's geometry.
    AlignedFrame { frame: usize },
    /// The composite fused so far.
    Composite,
    /// The winning sharpness of the tiles fused so far, as grey heat values
    /// (R = G = B), on the composite's extent and cell grid.
    Heat,
}

/// One preview image: `width * height` RGBA8 pixels, alpha 0 where the image
/// has no content yet (an unfused tile, the margin outside a warped frame).
/// `extent` is the rect the image covers, in reference-frame pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagePreview {
    pub kind: PreviewKind,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub extent: Rect,
}

/// `(width, height)` scaled so the longer edge is at most
/// [`PREVIEW_LONG_EDGE`]; never upscaled, never below one pixel.
fn fit_long_edge(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= PREVIEW_LONG_EDGE {
        return (width, height);
    }
    let scale = |edge: u32| {
        (u64::from(edge) * u64::from(PREVIEW_LONG_EDGE) / u64::from(long)).max(1) as u32
    };
    (scale(width), scale(height))
}

/// The shared preview state of one run: one optional piece per requested
/// kind.
pub struct PreviewFeed {
    aligned: Option<AlignedFrameSink>,
    want_composite: bool,
    composite: OnceLock<Arc<PreviewMosaic>>,
    want_heat: bool,
    heat: OnceLock<Arc<PreviewMosaic>>,
}

impl PreviewFeed {
    /// `None` when `kinds` requests nothing, so an unsubscribed run holds no
    /// feed at all.
    pub fn new(kinds: PreviewKinds, frame_size: (u32, u32)) -> Option<Self> {
        if !kinds.aligned_frame && !kinds.composite && !kinds.heat {
            return None;
        }
        Some(PreviewFeed {
            aligned: kinds
                .aligned_frame
                .then(|| AlignedFrameSink::new(frame_size)),
            want_composite: kinds.composite,
            composite: OnceLock::new(),
            want_heat: kinds.heat,
            heat: OnceLock::new(),
        })
    }

    /// The aligned-frame state, when that kind was requested.
    pub fn aligned_sink(&self) -> Option<&AlignedFrameSink> {
        self.aligned.as_ref()
    }

    /// Creates the composite mosaic for an output of `extent` (reference-frame
    /// pixels) and `samples_per_pixel`, when that kind was requested. A second
    /// call returns the mosaic the first one made.
    pub fn begin_composite(
        &self,
        extent: Rect,
        samples_per_pixel: u32,
    ) -> Option<Arc<PreviewMosaic>> {
        self.want_composite.then(|| {
            Arc::clone(self.composite.get_or_init(|| {
                Arc::new(PreviewMosaic::new(
                    PreviewKind::Composite,
                    extent,
                    samples_per_pixel,
                ))
            }))
        })
    }

    /// Creates the heat mosaic for an output of `extent`, when that kind was
    /// requested; the same extent as [`Self::begin_composite`] gives the same
    /// cell grid. A second call returns the mosaic the first one made.
    pub fn begin_heat(&self, extent: Rect) -> Option<Arc<PreviewMosaic>> {
        self.want_heat.then(|| {
            Arc::clone(
                self.heat
                    .get_or_init(|| Arc::new(PreviewMosaic::new(PreviewKind::Heat, extent, 1))),
            )
        })
    }

    /// Hands `send` every preview that changed since the last call, at most
    /// one per kind. Called from the run's poll thread only.
    pub fn deliver(&self, mut send: impl FnMut(&StagePreview)) {
        if let Some(preview) = self.aligned.as_ref().and_then(AlignedFrameSink::take_new) {
            send(&preview);
        }
        if let Some(preview) = self.composite.get().and_then(|m| m.take_new()) {
            send(&preview);
        }
        if let Some(preview) = self.heat.get().and_then(|m| m.take_new()) {
            send(&preview);
        }
    }
}

/// A box-reduced 8-bit luminance image of one frame.
struct Gray {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Gray {
    fn from_pyramid_level(level: &crate::fuse::pyramid::Plane) -> Self {
        let (width, height) = (level.width(), level.height());
        let (tw, th) = fit_long_edge(width, height);
        let samples: Vec<u16> = level
            .data()
            .iter()
            .map(|&v| v.round().clamp(0.0, f32::from(u16::MAX)) as u16)
            .collect();
        let mut reducer = Downsampler::new_full_source(width, height, 1, tw, th);
        reducer.add_band(
            &samples,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        );
        let data = reducer
            .finish()
            .iter()
            .enumerate()
            .map(|(i, &v)| reduce16to8(v, i as u32 % tw, i as u32 / tw))
            .collect();
        Gray {
            width: tw,
            height: th,
            data,
        }
    }

    /// `frame_size`'s whole frame resampled to `out` pixels, each of which is
    /// looked up through `transform` (reference to frame); nearest sample,
    /// alpha 0 outside the frame or past the transform's horizon.
    fn warp(&self, frame_size: (u32, u32), out: (u32, u32), transform: &Transform) -> Vec<u8> {
        let (fw, fh) = (f64::from(frame_size.0), f64::from(frame_size.1));
        let mut rgba = vec![0u8; out.0 as usize * out.1 as usize * 4];
        for py in 0..out.1 {
            let ry = (f64::from(py) + 0.5) * fh / f64::from(out.1) - 0.5;
            for px in 0..out.0 {
                let rx = (f64::from(px) + 0.5) * fw / f64::from(out.0) - 0.5;
                let Some((fx, fy)) = transform.map_point(rx, ry) else {
                    continue;
                };
                let sx = ((fx + 0.5) * f64::from(self.width) / fw).floor();
                let sy = ((fy + 0.5) * f64::from(self.height) / fh).floor();
                if !(0.0..f64::from(self.width)).contains(&sx)
                    || !(0.0..f64::from(self.height)).contains(&sy)
                {
                    continue;
                }
                let v = self.data[sy as usize * self.width as usize + sx as usize];
                let at = (py as usize * out.0 as usize + px as usize) * 4;
                rgba[at..at + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        rgba
    }
}

/// Collects the aligned frames: a registration offers its coarse image, the
/// driver completes it with the final transform, and the poll thread takes
/// the newest completed frame.
pub struct AlignedFrameSink {
    frame_size: (u32, u32),
    state: Mutex<AlignedState>,
}

#[derive(Default)]
struct AlignedState {
    /// Images of registrations still in flight, by frame.
    pending: HashMap<usize, Arc<Gray>>,
    ready: Option<(usize, Arc<Gray>, Transform)>,
    completed: u64,
    delivered: u64,
}

impl AlignedFrameSink {
    fn new(frame_size: (u32, u32)) -> Self {
        AlignedFrameSink {
            frame_size,
            state: Mutex::new(AlignedState::default()),
        }
    }

    /// Keeps a reduced copy of `frame`'s coarsest pyramid level that is at
    /// least [`PREVIEW_LONG_EDGE`] long, or of level 1 when none is. A
    /// pyramid with no level offers nothing.
    pub(crate) fn offer(&self, frame: usize, pyramid: &AlignPyramid) {
        let depth = pyramid.depth() as usize;
        let Some(level) = (1..=depth)
            .rev()
            .map(|k| pyramid.level(k))
            .find(|plane| plane.width().max(plane.height()) >= PREVIEW_LONG_EDGE)
            .or_else(|| (depth > 0).then(|| pyramid.level(1)))
        else {
            return;
        };
        let gray = Arc::new(Gray::from_pyramid_level(level));
        self.state.lock().unwrap().pending.insert(frame, gray);
    }

    /// Marks `frame`'s registration finished with its final reference-to-frame
    /// `transform`; the image offered for it becomes the one to deliver. A
    /// frame that offered nothing is skipped.
    pub(crate) fn complete(&self, frame: usize, transform: Transform) {
        let mut state = self.state.lock().unwrap();
        if let Some(gray) = state.pending.remove(&frame) {
            state.ready = Some((frame, gray, transform));
            state.completed += 1;
        }
    }

    fn take_new(&self) -> Option<StagePreview> {
        let (frame, gray, transform) = {
            let mut state = self.state.lock().unwrap();
            if state.completed == state.delivered {
                return None;
            }
            state.delivered = state.completed;
            state.ready.clone()?
        };
        let (width, height) = fit_long_edge(self.frame_size.0, self.frame_size.1);
        Some(StagePreview {
            kind: PreviewKind::AlignedFrame { frame },
            width,
            height,
            rgba: gray.warp(self.frame_size, (width, height), &transform),
            extent: Rect {
                x: 0,
                y: 0,
                width: self.frame_size.0,
                height: self.frame_size.1,
            },
        })
    }
}

/// The heat of `score` on a logarithmic scale that reaches 65535 at
/// `max_score` and clamps beyond it. A score of zero or below, `-inf` and NaN
/// are 0, the coldest value.
pub fn heat16(score: f64, max_score: f64) -> u16 {
    if !(score > 0.0 && max_score > 0.0) {
        return 0;
    }
    let heat = score.ln_1p() / max_score.ln_1p() * f64::from(u16::MAX);
    heat.round().clamp(0.0, f64::from(u16::MAX)) as u16
}

/// A 16-bit heat value rounded to 8 bits.
pub fn heat8(heat: u16) -> u8 {
    ((u32::from(heat) + 128) / 257) as u8
}

/// One kind of image as fused so far, reduced to preview size: the composite
/// or its heat. Tiles fold in as they are written; cells no tile has reached
/// stay uncovered.
pub struct PreviewMosaic {
    kind: PreviewKind,
    extent: Rect,
    samples_per_pixel: u32,
    size: (u32, u32),
    state: Mutex<MosaicState>,
}

struct MosaicState {
    reducer: Downsampler,
    folded: u64,
    delivered: u64,
}

impl PreviewMosaic {
    fn new(kind: PreviewKind, extent: Rect, samples_per_pixel: u32) -> Self {
        let size = fit_long_edge(extent.width, extent.height);
        PreviewMosaic {
            kind,
            extent,
            samples_per_pixel,
            size,
            state: Mutex::new(MosaicState {
                reducer: Downsampler::new_full_source(
                    extent.width,
                    extent.height,
                    samples_per_pixel,
                    size.0,
                    size.1,
                ),
                folded: 0,
                delivered: 0,
            }),
        }
    }

    /// Folds one written tile in: `rect` in output coordinates (the origin is
    /// the output's own corner), `samples` its samples (16-bit heat values
    /// for the heat kind).
    pub(crate) fn fold(&self, rect: Rect, samples: &[u16]) {
        let mut state = self.state.lock().unwrap();
        state.reducer.add_band(samples, rect);
        state.folded += 1;
    }

    fn take_new(&self) -> Option<StagePreview> {
        let (samples, covered) = {
            let mut state = self.state.lock().unwrap();
            if state.folded == state.delivered {
                return None;
            }
            state.delivered = state.folded;
            state.reducer.snapshot()
        };
        let spp = self.samples_per_pixel as usize;
        let (width, height) = self.size;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for (cell, pixel) in rgba.chunks_exact_mut(4).enumerate() {
            if !covered[cell] {
                continue;
            }
            if self.kind == PreviewKind::Heat {
                let heat = heat8(samples[cell]);
                pixel.copy_from_slice(&[heat, heat, heat, 255]);
                continue;
            }
            let (x, y) = (cell as u32 % width, cell as u32 / width);
            let channel = |c: usize| reduce16to8(samples[cell * spp + c.min(spp - 1)], x, y);
            pixel.copy_from_slice(&[channel(0), channel(1), channel(2), 255]);
        }
        Some(StagePreview {
            kind: self.kind,
            width,
            height,
            rgba,
            extent: self.extent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn no_kind_requested_builds_no_feed() {
        assert!(PreviewFeed::new(PreviewKinds::default(), (64, 64)).is_none());
        let only_composite = PreviewKinds {
            composite: true,
            ..Default::default()
        };
        let feed = PreviewFeed::new(only_composite, (64, 64)).expect("one kind requested");
        assert!(feed.aligned_sink().is_none());
        assert!(feed.begin_composite(rect(0, 0, 64, 64), 3).is_some());
        assert!(feed.begin_heat(rect(0, 0, 64, 64)).is_none());
    }

    #[test]
    fn a_heat_only_subscription_builds_a_feed() {
        let only_heat = PreviewKinds {
            heat: true,
            ..Default::default()
        };
        let feed = PreviewFeed::new(only_heat, (64, 64)).expect("heat alone is a subscription");
        assert!(feed.begin_composite(rect(0, 0, 64, 64), 1).is_none());
        let mosaic = feed.begin_heat(rect(0, 0, 64, 64)).expect("heat requested");
        let again = feed.begin_heat(rect(0, 0, 64, 64)).unwrap();
        assert!(Arc::ptr_eq(&mosaic, &again), "one mosaic per run");
    }

    #[test]
    fn heat_follows_the_composite_in_a_delivery() {
        let all = PreviewKinds {
            aligned_frame: false,
            composite: true,
            heat: true,
        };
        let feed = PreviewFeed::new(all, (2, 1)).unwrap();
        let composite = feed.begin_composite(rect(0, 0, 2, 1), 1).unwrap();
        let heat = feed.begin_heat(rect(0, 0, 2, 1)).unwrap();
        composite.fold(rect(0, 0, 2, 1), &[1, 2]);
        heat.fold(rect(0, 0, 2, 1), &[3, 4]);
        let mut kinds = Vec::new();
        feed.deliver(|p| kinds.push(p.kind));
        assert_eq!(kinds, [PreviewKind::Composite, PreviewKind::Heat]);
    }

    #[test]
    fn sharpness_heatmap_scale_is_run_wide_heat16_is_monotone_and_pinned() {
        let max = 5.0e7;
        assert_eq!(heat16(0.0, max), 0);
        assert_eq!(heat16(max, max), 65535);
        let mut previous = 0;
        for i in 0..=400 {
            let score = max * (f64::from(i) / 400.0).powi(4);
            let heat = heat16(score, max);
            assert!(
                heat >= previous,
                "score {score} fell from {previous} to {heat}"
            );
            previous = heat;
        }
    }

    #[test]
    fn sharpness_heatmap_heat16_sends_ineligible_scores_to_cold() {
        for score in [f64::NEG_INFINITY, f64::NAN, -1.0, -0.0] {
            assert_eq!(heat16(score, 1.0e6), 0, "score {score}");
        }
        assert_eq!(heat16(f64::INFINITY, 1.0e6), 65535);
        assert_eq!(
            heat16(1.0e12, 1.0e6),
            65535,
            "a score past the scale clamps"
        );
        assert_eq!(heat16(5.0, 0.0), 0, "a degenerate scale stays cold");
        assert_eq!(heat16(5.0, f64::NAN), 0);
    }

    #[test]
    fn sharpness_heatmap_heat8_rounds_the_16_bit_range_onto_8_bits() {
        assert_eq!(heat8(0), 0);
        assert_eq!(heat8(128), 0);
        assert_eq!(heat8(129), 1);
        assert_eq!(heat8(32767), 127);
        assert_eq!(heat8(32768), 128);
        assert_eq!(heat8(65535), 255);
    }

    #[test]
    fn heat_mosaic_is_grey_and_transparent_where_unfused() {
        let mosaic = PreviewMosaic::new(PreviewKind::Heat, rect(5, 6, 4, 1), 1);
        mosaic.fold(rect(0, 0, 2, 1), &[0, 65535]);
        let preview = mosaic.take_new().expect("a tile was folded");
        assert_eq!(preview.kind, PreviewKind::Heat);
        assert_eq!(preview.extent, rect(5, 6, 4, 1));
        assert_eq!(
            preview.rgba,
            [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert!(mosaic.take_new().is_none(), "nothing changed since");
    }

    #[test]
    fn heat_mosaic_rounds_each_cell_without_dither() {
        let mosaic = PreviewMosaic::new(PreviewKind::Heat, rect(0, 0, 2, 1), 1);
        mosaic.fold(rect(0, 0, 2, 1), &[129, 128]);
        let grey: Vec<u8> = mosaic
            .take_new()
            .unwrap()
            .rgba
            .chunks_exact(4)
            .map(|p| p[0])
            .collect();
        assert_eq!(grey, [1, 0]);
    }

    #[test]
    fn long_edge_is_capped_and_never_upscaled() {
        assert_eq!(fit_long_edge(64, 40), (64, 40));
        assert_eq!(fit_long_edge(512, 512), (512, 512));
        assert_eq!(fit_long_edge(1000, 600), (512, 307));
        assert_eq!(fit_long_edge(600, 1000), (307, 512));
    }

    #[test]
    fn mono_composite_replicates_to_rgb() {
        let mosaic = PreviewMosaic::new(PreviewKind::Composite, rect(0, 0, 2, 1), 1);
        mosaic.fold(rect(0, 0, 2, 1), &[0x8000, 0x4000]);
        let preview = mosaic.take_new().expect("a tile was folded");
        assert_eq!(preview.rgba, [128, 128, 128, 255, 64, 64, 64, 255]);
    }

    #[test]
    fn colour_composite_keeps_its_channels() {
        let mosaic = PreviewMosaic::new(PreviewKind::Composite, rect(0, 0, 1, 1), 3);
        mosaic.fold(rect(0, 0, 1, 1), &[0x1000, 0x2000, 0x3000]);
        assert_eq!(mosaic.take_new().unwrap().rgba, [16, 32, 48, 255]);
    }

    #[test]
    fn unfused_cells_are_transparent_and_a_repeat_delivers_nothing() {
        let mosaic = PreviewMosaic::new(PreviewKind::Composite, rect(10, 20, 4, 2), 1);
        mosaic.fold(rect(0, 0, 2, 2), &[0x8000; 4]);
        let preview = mosaic.take_new().expect("a tile was folded");
        let alpha: Vec<u8> = preview.rgba.chunks_exact(4).map(|p| p[3]).collect();
        assert_eq!(alpha, [255, 255, 0, 0, 255, 255, 0, 0]);
        assert_eq!(preview.extent, rect(10, 20, 4, 2));
        assert!(mosaic.take_new().is_none(), "nothing changed since");
        mosaic.fold(rect(2, 0, 2, 2), &[0x8000; 4]);
        assert!(mosaic.take_new().is_some());
    }

    #[test]
    fn mosaic_of_a_large_output_is_capped() {
        let mosaic = PreviewMosaic::new(PreviewKind::Composite, rect(0, 0, 1000, 600), 1);
        mosaic.fold(rect(0, 0, 1000, 600), &vec![0u16; 1000 * 600]);
        let preview = mosaic.take_new().unwrap();
        assert_eq!((preview.width, preview.height), (512, 307));
        assert_eq!(preview.rgba.len(), 512 * 307 * 4);
    }

    fn ramp() -> Gray {
        Gray {
            width: 4,
            height: 2,
            data: vec![10, 20, 30, 40, 50, 60, 70, 80],
        }
    }

    fn translation(dx: f64) -> Transform {
        let mut t = Transform::identity();
        t.m[0][2] = dx;
        t
    }

    #[test]
    fn warp_by_identity_reproduces_the_image() {
        let rgba = ramp().warp((4, 2), (4, 2), &Transform::identity());
        let grey: Vec<u8> = rgba.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(grey, ramp().data);
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn warp_by_a_translation_shifts_and_leaves_a_transparent_margin() {
        let rgba = ramp().warp((4, 2), (4, 2), &translation(1.0));
        let pixels: Vec<(u8, u8)> = rgba.chunks_exact(4).map(|p| (p[0], p[3])).collect();
        assert_eq!(
            pixels,
            [
                (20, 255),
                (30, 255),
                (40, 255),
                (0, 0),
                (60, 255),
                (70, 255),
                (80, 255),
                (0, 0)
            ]
        );
    }
}
