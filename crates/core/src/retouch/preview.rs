// SPDX-License-Identifier: GPL-3.0-or-later

//! The brush-preview layer: what a stroke at the cursor would do to the
//! displayed result, as RGBA8 texels over the brush disc.
//!
//! Display is `mix(result, preview, alpha)`. The RGB is the stroke's
//! full-strength output for its mode, tone-mapped like the composite pane; the
//! alpha is the stroke's mask scaled by its opacity and by a strength. The mask,
//! the per-mode arithmetic and Detail's blur are [`blend`]'s, so the layer
//! cannot drift from the stroke a press would record.
//!
//! With `m` the Q16 mask (`0..=65536`), `o` the Q16 opacity ([`blend::opacity_q16`])
//! and `s` the Q16 strength, and `rnd16(x) = (x + 32768) >> 16`:
//!
//! ```text
//! a16 = rnd16(rnd16(m * o) * s)        a8 = (a16 * 255 + 32768) >> 16
//! ```
//!
//! Products are summed in 64 bits here. A 32-bit mirror of
//! [`blend::scaled_alpha`] must return the other factor when one factor is
//! 65536, the only case whose product reaches 2^32: with both factors at most
//! 65535, `a * b + 32768` stays below 2^32.

use std::num::NonZeroU32;

use crate::display::tonemap_cpu;
use crate::project::{BrushMode, Op, Point, StrokeSource};
use crate::tile::Rect;
use crate::zmap::ZMapInfo;

use super::blend;

/// Q16 scale of masks, opacities and strengths: 1.0.
const OPAQUE: u32 = 1 << 16;

/// The strength the displayed preview uses, 0.8 in Q16.
pub const BRUSH_PREVIEW_STRENGTH_Q16: u32 = 52429;

/// Dither period of the display reduction, in texels.
const DITHER: u32 = 4;

/// Samples of an image region on a lattice: sample `(i, j)` is the image pixel
/// `(rect.x + i * step, rect.y + j * step)`, with `rect.width.div_ceil(step)`
/// columns, row-major, `samples_per_pixel` 16-bit samples each.
#[derive(Debug, Clone, Copy)]
pub struct BrushPreviewGrid<'a> {
    pub rect: Rect,
    pub step: NonZeroU32,
    pub samples_per_pixel: u32,
    pub samples: &'a [u16],
}

impl BrushPreviewGrid<'_> {
    fn columns(&self) -> u32 {
        self.rect.width.div_ceil(self.step.get())
    }

    fn rows(&self) -> u32 {
        self.rect.height.div_ceil(self.step.get())
    }

    /// Whether the buffer holds exactly the lattice's samples.
    fn is_complete(&self) -> bool {
        self.samples.len()
            == self.columns() as usize * self.rows() as usize * self.samples_per_pixel as usize
    }

    /// The sample nearest the image pixel `(x, y)`: column
    /// `min((x - rect.x + (step - 1) / 2) / step, columns - 1)`, rows alike, so a
    /// pixel exactly between two samples reads the lower index and the last
    /// partial cell reads the last sample. `None` outside `rect`.
    pub fn nearest(&self, x: u32, y: u32) -> Option<&[u16]> {
        let rect = self.rect;
        if x < rect.x || y < rect.y || x - rect.x >= rect.width || y - rect.y >= rect.height {
            return None;
        }
        let step = self.step.get();
        let pick = |offset: u32, count: u32| {
            (offset.saturating_add((step - 1) / 2) / step).min(count - 1) as usize
        };
        let (i, j) = (
            pick(x - rect.x, self.columns()),
            pick(y - rect.y, self.rows()),
        );
        let spp = self.samples_per_pixel as usize;
        let at = (j * self.columns() as usize + i) * spp;
        self.samples.get(at..at + spp)
    }
}

/// The brush at the cursor: the integer centre a one-point stroke stores, the
/// stroke's radius, softness, mode and opacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushPreviewSpec {
    pub centre: Point,
    pub radius: f32,
    pub softness: f32,
    pub mode: BrushMode,
    pub opacity: f32,
}

/// The one-point stroke a press at the spec's centre records.
fn one_point_stroke(spec: &BrushPreviewSpec) -> Op {
    Op::BrushStroke {
        source: StrokeSource::Frame(0),
        path: vec![spec.centre],
        radius: spec.radius,
        softness: spec.softness,
        mode: spec.mode,
        opacity: spec.opacity,
    }
}

/// Width of each of Detail's three box passes on a lattice of `step`:
/// `max(1, (p + step / 2) / step)` samples for the stroke's pass radius `p`.
fn pass_samples(pass: u32, step: NonZeroU32) -> u32 {
    let step = step.get();
    ((pass + step / 2) / step).max(1)
}

/// The pixel box [`brush_preview`] needs both grids to cover when the result
/// grid has lattice `step`: the stroke's footprint, grown for Detail by its
/// three [`pass_samples`] box passes, clipped to the image. Grids that start
/// at or before the box's corner and reach its far edges answer; one that
/// starts later does not. `None` when the image or the brush is out of range
/// or the footprint holds no pixel. Allocates nothing.
pub fn brush_preview_bounds(
    image_size: (u32, u32),
    spec: &BrushPreviewSpec,
    step: NonZeroU32,
) -> Option<Rect> {
    let info = ZMapInfo::new(image_size.0, image_size.1, 1).ok()?;
    let stroke = blend::validate_parts(
        &StrokeSource::Frame(0),
        blend::Stroke {
            path: std::slice::from_ref(&spec.centre),
            radius: spec.radius,
            softness: spec.softness,
            mode: spec.mode,
            opacity: spec.opacity,
        },
        info,
    )
    .ok()?;
    let footprint = blend::stroke_footprint(&stroke, info)?;
    let reach = match spec.mode {
        BrushMode::Detail => 3u32
            .saturating_mul(pass_samples(blend::stroke_pass_radius(&stroke), step))
            .saturating_mul(step.get()),
        _ => 0,
    };
    Some(blend::grow(footprint, reach, info))
}

/// RGBA8 texels, row-major, over `width` x `height` lattice samples of the
/// result grid starting at lattice sample (`column`, `row`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrushPreview {
    pub column: u32,
    pub row: u32,
    pub width: u32,
    pub height: u32,
    pub texels: Vec<u8>,
}

/// The preview layer of `spec` over `result`, with `source` read by
/// [`BrushPreviewGrid::nearest`] at each result sample's pixel.
///
/// The box starts on a multiple of 4 samples on both axes and covers every
/// lattice sample of the brush's footprint, so each texel reduces to 8 bits as
/// the same sample of the whole result grid does. A texel outside the disc
/// carries the result's display at alpha 0; alpha follows the formula in the
/// module docs, with `strength_q16` at most 65536. RGB is the mode's output
/// at full coverage ([`blend::blend_pixel`]); Detail's three box passes are
/// `max(1, (p + step / 2) / step)` lattice samples wide for the stroke's pass
/// radius `p`, and read three passes past the footprint, clamped to the map.
///
/// `None` when the layer is empty or cannot be computed: either grid has other
/// than 3 samples per pixel or a buffer of the wrong length, `result` is not
/// inside the image or does not hold the footprint (plus Detail's margin),
/// `source` does not answer for a texel inside the disc, the footprint holds
/// no lattice sample with a nonzero mask, or the radius, softness, opacity or
/// strength is out of range.
pub fn brush_preview(
    image_size: (u32, u32),
    result: &BrushPreviewGrid,
    source: &BrushPreviewGrid,
    spec: &BrushPreviewSpec,
    strength_q16: u32,
) -> Option<BrushPreview> {
    if strength_q16 > OPAQUE {
        return None;
    }
    for grid in [result, source] {
        if grid.samples_per_pixel != 3 || !grid.is_complete() {
            return None;
        }
    }
    let info = ZMapInfo::new(image_size.0, image_size.1, 1).ok()?;
    if !info.contains(result.rect) {
        return None;
    }
    let op = one_point_stroke(spec);
    let footprint = blend::footprint(&op, info).ok()??;
    if result.rect.intersect(footprint) != Some(footprint) {
        return None;
    }

    let step = result.step.get();
    let columns = result.columns() as usize;
    let lattice = |from: u32, origin: u32| (from - origin).div_ceil(step);
    let (i0, i1) = (
        lattice(footprint.x, result.rect.x),
        lattice(footprint.x + footprint.width, result.rect.x),
    );
    let (j0, j1) = (
        lattice(footprint.y, result.rect.y),
        lattice(footprint.y + footprint.height, result.rect.y),
    );
    if i0 >= i1 || j0 >= j1 {
        return None;
    }
    let (column, row) = (i0 / DITHER * DITHER, j0 / DITHER * DITHER);
    let (width, height) = (i1 - column, j1 - row);
    let pixel = |i: u32, j: u32| (result.rect.x + i * step, result.rect.y + j * step);
    let result_at = |i: u32, j: u32| (j as usize * columns + i as usize) * 3;

    let mut mask = Vec::with_capacity(width as usize * height as usize);
    for j in row..j1 {
        for i in column..i1 {
            let (x, y) = pixel(i, j);
            mask.push(blend::alpha(&op, info, x, y).ok()?);
        }
    }
    if mask.iter().all(|&m| m == 0) {
        return None;
    }

    // Detail mixes toward its own target, over the footprint's samples.
    let target = if spec.mode == BrushMode::Detail {
        Some(detail_target(
            &op,
            info,
            (result, source),
            Rect {
                x: i0,
                y: j0,
                width: i1 - i0,
                height: j1 - j0,
            },
        )?)
    } else {
        None
    };

    let mut tile = Vec::with_capacity(mask.len() * 3);
    for j in row..j1 {
        let at = result_at(column, j);
        tile.extend_from_slice(&result.samples[at..at + width as usize * 3]);
    }
    for (k, &m) in mask.iter().enumerate() {
        if m == 0 {
            continue;
        }
        let (i, j) = (column + k as u32 % width, row + k as u32 / width);
        let pixel_at = k * 3;
        let src = match &target {
            Some(target) => {
                let at = ((j - j0) as usize * (i1 - i0) as usize + (i - i0) as usize) * 3;
                &target[at..at + 3]
            }
            None => {
                let (x, y) = pixel(i, j);
                source.nearest(x, y)?
            }
        };
        blend::blend_pixel(spec.mode, &mut tile[pixel_at..pixel_at + 3], src, OPAQUE);
    }

    let mut texels = vec![0u8; mask.len() * 4];
    tonemap_cpu(&tile, width, height, 3, 0, &mut texels);
    let opacity = blend::opacity_q16(spec.opacity);
    for (texel, &m) in texels.chunks_exact_mut(4).zip(&mask) {
        let a16 = blend::scaled_alpha(blend::scaled_alpha(m, opacity), strength_q16);
        texel[3] = ((u64::from(a16) * 255 + 32768) >> 16) as u8;
    }
    Some(BrushPreview {
        column,
        row,
        width,
        height,
        texels,
    })
}

/// Detail's target over `at`, a box of result lattice samples: the blur window
/// grows by 3 passes of `max(1, (p + step / 2) / step)` samples, kept to the
/// samples inside the map, and both grids are resampled onto that box. `None`
/// when `result` lacks a sample of the box or `source` answers for none.
fn detail_target(
    op: &Op,
    info: ZMapInfo,
    (result, source): (&BrushPreviewGrid, &BrushPreviewGrid),
    at: Rect,
) -> Option<Vec<u16>> {
    let step = i64::from(result.step.get());
    let pass_samples = pass_samples(blend::pass_radius(op, info).ok()?, result.step);
    let reach = 3 * i64::from(pass_samples);
    // The lattice samples inside the map: pixel `origin + i * step` in `0..extent`.
    let inside = |origin: u32, extent: u32| {
        (
            -(i64::from(origin) / step),
            (i64::from(extent) - i64::from(origin) + step - 1) / step,
        )
    };
    let (min_i, max_i) = inside(result.rect.x, info.width());
    let (min_j, max_j) = inside(result.rect.y, info.height());
    let grown = |from: u32, to: u32, (min, max): (i64, i64)| {
        (
            (i64::from(from) - reach).max(min),
            (i64::from(to) + reach).min(max),
        )
    };
    let (gi0, gi1) = grown(at.x, at.x + at.width, (min_i, max_i));
    let (gj0, gj1) = grown(at.y, at.y + at.height, (min_j, max_j));
    if gi0 < 0 || gj0 < 0 || gi1 > i64::from(result.columns()) || gj1 > i64::from(result.rows()) {
        return None;
    }
    let window = Rect {
        x: gi0 as u32,
        y: gj0 as u32,
        width: (gi1 - gi0) as u32,
        height: (gj1 - gj0) as u32,
    };
    let columns = result.columns() as usize;
    let capacity = window.width as usize * window.height as usize * 3;
    let (mut from_result, mut from_source) =
        (Vec::with_capacity(capacity), Vec::with_capacity(capacity));
    for j in window.y..window.y + window.height {
        for i in window.x..window.x + window.width {
            let at = (j as usize * columns + i as usize) * 3;
            from_result.extend_from_slice(&result.samples[at..at + 3]);
            let (x, y) = (
                result.rect.x + i * result.step.get(),
                result.rect.y + j * result.step.get(),
            );
            from_source.extend_from_slice(source.nearest(x, y)?);
        }
    }
    let mut target = vec![0u16; at.width as usize * at.height as usize * 3];
    blend::detail_output(
        pass_samples,
        (window, &from_result),
        (window, &from_source),
        at,
        3,
        &mut target,
    );
    Some(target)
}
