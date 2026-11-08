// SPDX-License-Identifier: GPL-3.0-or-later

//! The soft brush stroke: `out = mix(out, source, alpha)` over a disc of
//! `radius` pixels around the stroke's path, in integer fixed point.
//!
//! The mask is full inside the core, `radius * (1 - softness)`, and falls
//! linearly to zero at `radius`, measured in sixteenths of a pixel from the
//! exact point-to-segment distance, so it is a pure function of the stroke and
//! the pixel. Alpha is Q16 (`0..=65536`); the blend rounds to nearest.

use std::cmp::Ordering;
use std::num::NonZeroU32;

use crate::algorithm::TileReader;
use crate::project::{BrushMode, Op, Point};
use crate::tile::{Rect, Sample};
use crate::zmap::ZMapInfo;

use super::RetouchError;

/// One pixel in the distance unit of the mask.
const SIXTEENTHS: f64 = 16.0;
/// Alpha of a fully covered sample.
const OPAQUE: u32 = 1 << 16;
/// Caps the radius so the ramp's products stay far inside `u64`.
const MAX_RADIUS_16: u64 = 1 << 40;

/// A validated brush stroke: [`Op::BrushStroke`]'s path, radius, softness,
/// mode and opacity, checked against the map they will be applied to.
pub(super) struct Stroke<'a> {
    pub(super) path: &'a [Point],
    pub(super) radius: f32,
    pub(super) softness: f32,
    pub(super) mode: BrushMode,
    pub(super) opacity: f32,
}

/// Refuses, in order: an empty path; `source_id >= info.frame_count()`; a
/// non-finite or non-positive radius; a softness outside `0.0..=1.0`; an
/// opacity outside `0 < x <= 1`.
pub(super) fn validate<'a>(op: &'a Op, info: ZMapInfo) -> Result<Stroke<'a>, RetouchError> {
    let Op::BrushStroke {
        source_id,
        path,
        radius,
        softness,
        mode,
        opacity,
    } = op;
    if path.is_empty() {
        return Err(RetouchError::EmptyPath);
    }
    if u32::from(*source_id) >= info.frame_count() {
        return Err(RetouchError::SourceOutOfRange {
            source_id: *source_id,
            frame_count: info.frame_count(),
        });
    }
    if !radius.is_finite() || *radius <= 0.0 {
        return Err(RetouchError::InvalidRadius { radius: *radius });
    }
    if !(0.0..=1.0).contains(softness) {
        return Err(RetouchError::InvalidSoftness {
            softness: *softness,
        });
    }
    if !(*opacity > 0.0 && *opacity <= 1.0) {
        return Err(RetouchError::InvalidOpacity { opacity: *opacity });
    }
    Ok(Stroke {
        path: path.as_slice(),
        radius: *radius,
        softness: *softness,
        mode: *mode,
        opacity: *opacity,
    })
}

/// The dirty region a stroke reports: its path's bounding box widened by
/// `radius`, clamped to the map's extent. `None` means an empty region --
/// every pixel of the path lies outside the map, or the path's own extent
/// clamps to nothing.
pub fn footprint(op: &Op, info: ZMapInfo) -> Result<Option<Rect>, RetouchError> {
    let stroke = validate(op, info)?;
    Ok(stroke_footprint(&stroke, info))
}

/// `rect` widened by `margin` on every side, clamped to the map.
pub fn grow(rect: Rect, margin: u32, info: ZMapInfo) -> Rect {
    let x0 = rect.x.saturating_sub(margin);
    let y0 = rect.y.saturating_sub(margin);
    let x1 = rect.x.saturating_add(rect.width).saturating_add(margin);
    let y1 = rect.y.saturating_add(rect.height).saturating_add(margin);
    let (x1, y1) = (x1.min(info.width()), y1.min(info.height()));
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// The part of the map a render of `write` must read, given the `strokes`
/// placed on it in order: walking backward, each Detail stroke whose footprint
/// meets the extent so far grows it by [`read_margin`] around that overlap, so
/// overlapping chains add up. Without one the result is `write`. Every stroke
/// must be valid for `info`, as [`footprint`] checked.
pub fn read_extent(info: ZMapInfo, strokes: &[PlacedStroke<'_>], write: Rect) -> Rect {
    // ponytail: no cap. N overlapping Detail strokes of margin m reach about
    // N * m past `write`, up to one plane (about 540 MB at 44.8 MP x 3 x f32);
    // a render in several passes would bound it.
    let mut extent = write;
    for stroke in strokes.iter().rev() {
        let Some(overlap) = stroke.footprint.intersect(extent) else {
            continue;
        };
        let margin = read_margin(stroke.op, info).expect("a placed stroke is valid");
        if margin == 0 {
            continue;
        }
        let grown = grow(overlap, margin, info);
        let x0 = extent.x.min(grown.x);
        let y0 = extent.y.min(grown.y);
        let x1 = (extent.x + extent.width).max(grown.x + grown.width);
        let y1 = (extent.y + extent.height).max(grown.y + grown.height);
        extent = Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        };
    }
    extent
}

pub(super) fn stroke_footprint(stroke: &Stroke, info: ZMapInfo) -> Option<Rect> {
    let r = f64::from(stroke.radius);
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for p in stroke.path {
        let x = f64::from(p.x);
        let y = f64::from(p.y);
        min_x = min_x.min((x - r).floor() as i64);
        min_y = min_y.min((y - r).floor() as i64);
        // `+ 1`: an exclusive bound, so a pixel at exactly `x + r` is not
        // excluded when `x + r` is itself an integer.
        max_x = max_x.max(((x + r).floor() as i64).saturating_add(1));
        max_y = max_y.max(((y + r).floor() as i64).saturating_add(1));
    }
    let x0 = min_x.clamp(0, i64::from(info.width()));
    let y0 = min_y.clamp(0, i64::from(info.height()));
    let x1 = max_x.clamp(0, i64::from(info.width()));
    let y1 = max_y.clamp(0, i64::from(info.height()));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rect {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}

/// Segment `i` of the stroke's path; a one-point path is one zero-length
/// segment.
pub(super) fn segment(stroke: &Stroke, i: usize) -> (Point, Point) {
    match stroke.path {
        [p] => (*p, *p),
        path => (path[i], path[i + 1]),
    }
}

/// Square cells over `region`, each listing the path segments that can reach
/// one of its pixels. A pixel is reached only within `radius` of a segment,
/// so a segment is listed in every cell its bounding box, widened by `radius`
/// plus one pixel of rounding slack, touches.
pub(super) struct SegmentGrid {
    region: Rect,
    cell: u32,
    columns: usize,
    cells: Vec<Vec<u32>>,
}

impl SegmentGrid {
    pub(super) fn new(stroke: &Stroke, region: Rect) -> Self {
        let r = f64::from(stroke.radius);
        let cell = r
            .ceil()
            .max(64.0)
            .min(f64::from(region.width.max(region.height))) as u32;
        let columns = region.width.div_ceil(cell) as usize;
        let rows = region.height.div_ceil(cell) as usize;
        let mut cells = vec![Vec::new(); columns * rows];
        let reach = r + 1.0;
        let cell_range = |lo: f64, hi: f64, origin: u32, count: usize| {
            let first = ((lo - reach - f64::from(origin)) / f64::from(cell)).floor();
            let last = ((hi + reach - f64::from(origin)) / f64::from(cell)).floor();
            (last >= 0.0 && first < count as f64)
                .then(|| (first.max(0.0) as usize)..=(last.min(count as f64 - 1.0) as usize))
        };
        for i in 0..stroke.path.len().saturating_sub(1).max(1) {
            let (a, b) = segment(stroke, i);
            let (ax, ay, bx, by) = (
                f64::from(a.x),
                f64::from(a.y),
                f64::from(b.x),
                f64::from(b.y),
            );
            let (Some(xs), Some(ys)) = (
                cell_range(ax.min(bx), ax.max(bx), region.x, columns),
                cell_range(ay.min(by), ay.max(by), region.y, rows),
            ) else {
                continue;
            };
            for cy in ys {
                for cx in xs.clone() {
                    cells[cy * columns + cx].push(i as u32);
                }
            }
        }
        Self {
            region,
            cell,
            columns,
            cells,
        }
    }

    /// The path segments listed for the cell holding the pixel `(x, y)`,
    /// which must lie in the grid's region.
    pub(super) fn segments_at(&self, x: u32, y: u32) -> &[u32] {
        let column = ((x - self.region.x) / self.cell) as usize;
        let row = ((y - self.region.y) / self.cell) as usize;
        &self.cells[row * self.columns + column]
    }

    /// Calls `visit(x, y, segments)` for every pixel of the region lying in a
    /// cell some segment can reach, with that cell's segment indices.
    pub(super) fn walk(&self, mut visit: impl FnMut(u32, u32, &[u32])) {
        let region = self.region;
        for y in region.y..region.y + region.height {
            let row = ((y - region.y) / self.cell) as usize;
            for column in 0..self.columns {
                let segments = &self.cells[row * self.columns + column];
                if segments.is_empty() {
                    continue;
                }
                let x0 = region.x + column as u32 * self.cell;
                let x1 = (x0 + self.cell).min(region.x + region.width);
                for x in x0..x1 {
                    visit(x, y, segments);
                }
            }
        }
    }
}

/// 256 times the squared distance from the pixel `(px, py)` to segment
/// `a..b` (a zero-length segment is its own point), rounded down. A distance
/// beyond what the arithmetic holds saturates, which is far outside any ramp.
fn segment_d2_256(px: i64, py: i64, a: Point, b: Point) -> u128 {
    let (ax, ay) = (i128::from(a.x), i128::from(a.y));
    let (bx, by) = (i128::from(b.x), i128::from(b.y));
    let (px, py) = (i128::from(px), i128::from(py));
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let dot = (px - ax) * dx + (py - ay) * dy;
    let point_d2 = |x: i128, y: i128| 256 * ((px - x).pow(2) + (py - y).pow(2)) as u128;
    if len2 == 0 || dot <= 0 {
        return point_d2(ax, ay);
    }
    if dot >= len2 {
        return point_d2(bx, by);
    }
    let cross = ((px - ax) * dy - (py - ay) * dx).unsigned_abs();
    cross
        .checked_mul(cross)
        .and_then(|c2| c2.checked_mul(256))
        .map_or(u128::MAX, |n| n / len2 as u128)
}

/// The distance of the pixel `(x, y)` from `segments` of `stroke`, in
/// sixteenths of a pixel (floored): the nearest one's.
fn distance_16(stroke: &Stroke, x: u32, y: u32, segments: impl Iterator<Item = usize>) -> u64 {
    let d2 = segments
        .map(|i| {
            let (a, b) = segment(stroke, i);
            segment_d2_256(i64::from(x), i64::from(y), a, b)
        })
        .min()
        .unwrap_or(u128::MAX);
    d2.isqrt() as u64
}

/// The radius and core of a stroke's mask, in sixteenths of a pixel.
struct Ramp {
    radius: u64,
    core: u64,
}

impl Ramp {
    /// Rounds the stored `f32` fields once; a stroke narrower than 1/32 px
    /// has a zero radius and covers nothing.
    fn new(stroke: &Stroke) -> Self {
        let radius = ((f64::from(stroke.radius) * SIXTEENTHS).round() as u64).min(MAX_RADIUS_16);
        let softness = (f64::from(stroke.softness) * f64::from(OPAQUE)).round() as u64;
        Self {
            radius,
            core: (radius * (u64::from(OPAQUE) - softness)) >> 16,
        }
    }

    /// Q16 alpha at distance `d16`: zero from the radius out, opaque inside
    /// the core, linear between.
    fn alpha(&self, d16: u64) -> u32 {
        if d16 >= self.radius {
            0
        } else if d16 <= self.core {
            OPAQUE
        } else {
            ((self.radius - d16) * u64::from(OPAQUE) / (self.radius - self.core)) as u32
        }
    }
}

/// The radius and core of `op`'s mask in sixteenths of a pixel: the rounded
/// values [`apply`] measures its distances against, for a blend that
/// reproduces it.
pub fn mask_ramp(op: &Op, info: ZMapInfo) -> Result<(u64, u64), RetouchError> {
    let ramp = Ramp::new(&validate(op, info)?);
    Ok((ramp.radius, ramp.core))
}

/// Largest Detail pass radius: beyond it a window sum would not fit `i64`.
const MAX_PASS_RADIUS: u64 = 1 << 20;

/// The radius `p` of each of Detail's three box passes:
/// `max(1, (radius16 + 24) / 48)`, `radius16` being the stroke's radius in
/// sixteenths of a pixel as [`mask_ramp`] rounds it, so three passes reach
/// about the brush radius. Capped at 2^20.
pub fn pass_radius(op: &Op, info: ZMapInfo) -> Result<u32, RetouchError> {
    let radius16 = Ramp::new(&validate(op, info)?).radius;
    Ok(((radius16 + 24) / 48).clamp(1, MAX_PASS_RADIUS) as u32)
}

/// How far outside its footprint a stroke reads the result and its source
/// frame: `3 * p` pixels for [`BrushMode::Detail`] (`p` from [`pass_radius`]),
/// zero for every other mode.
pub fn read_margin(op: &Op, info: ZMapInfo) -> Result<u32, RetouchError> {
    match validate(op, info)?.mode {
        BrushMode::Detail => Ok(3 * pass_radius(op, info)?),
        _ => Ok(0),
    }
}

/// `r` grown by `p` along one axis, kept inside `within`.
fn grow_axis(r: Rect, horizontal: bool, p: u32, within: Rect) -> Rect {
    let (start, len, lo, hi) = if horizontal {
        (
            r.x,
            r.width,
            within.x,
            within.x.saturating_add(within.width),
        )
    } else {
        (
            r.y,
            r.height,
            within.y,
            within.y.saturating_add(within.height),
        )
    };
    let from = start.saturating_sub(p).max(lo);
    let to = start.saturating_add(len).saturating_add(p).min(hi);
    if horizontal {
        Rect {
            x: from,
            width: to - from,
            ..r
        }
    } else {
        Rect {
            y: from,
            height: to - from,
            ..r
        }
    }
}

/// One 1-D box pass of radius `p` over `dst_rect`, along x or y. `src` holds
/// `src_rect`, which covers every window except where it ends on the buffer's
/// edge; a window reaching past that edge repeats the edge sample. Each output
/// is `(sum + p).div_euclid(2p + 1)` of an exact `i64` window sum, slid along
/// the line, so it does not depend on where the line starts.
fn box_pass(
    p: u32,
    horizontal: bool,
    src: &[i64],
    src_rect: Rect,
    dst_rect: Rect,
    spp: usize,
) -> Vec<i64> {
    let (lo, hi, first, along, lines_from, lines) = if horizontal {
        (
            src_rect.x,
            src_rect.x + src_rect.width - 1,
            dst_rect.x,
            dst_rect.width,
            dst_rect.y,
            dst_rect.height,
        )
    } else {
        (
            src_rect.y,
            src_rect.y + src_rect.height - 1,
            dst_rect.y,
            dst_rect.height,
            dst_rect.x,
            dst_rect.width,
        )
    };
    let (lo, hi, p) = (i64::from(lo), i64::from(hi), i64::from(p));
    let at = |a: i64, line: u32, c: usize| {
        let a = a.clamp(lo, hi) as u32;
        let (x, y) = if horizontal { (a, line) } else { (line, a) };
        let row = (y - src_rect.y) as usize * src_rect.width as usize;
        src[(row + (x - src_rect.x) as usize) * spp + c]
    };
    let width = dst_rect.width as usize;
    let mut dst = vec![0i64; width * dst_rect.height as usize * spp];
    for line in 0..lines {
        for c in 0..spp {
            let l = lines_from + line;
            let start = i64::from(first);
            let (a, b) = (start - p, start + p);
            // The part of the first window past an edge repeats that edge's sample.
            let mut sum = (lo - a).max(0) * at(lo, l, c) + (b - hi).max(0) * at(hi, l, c);
            for j in a.max(lo)..=b.min(hi) {
                sum += at(j, l, c);
            }
            for k in 0..along {
                let pos = start + i64::from(k);
                let (x, y) = if horizontal { (k, line) } else { (line, k) };
                dst[(y as usize * width + x as usize) * spp + c] = (sum + p).div_euclid(2 * p + 1);
                sum += at(pos + p + 1, l, c) - at(pos - p, l, c);
            }
        }
    }
    dst
}

/// The fixed-point blur of `buf` (the samples of `rect`) over `at`: three
/// passes, each a horizontal then a vertical box pass.
fn blur_fixed<S: Blend>(p: u32, (rect, buf): (Rect, &[S]), at: Rect, spp: usize) -> Vec<i64> {
    // `rects[i]` is what stage `i` (H, V, H, V, H, V) reads; `rects[6]` is `at`.
    let mut rects = [at; 7];
    for i in (0..6).rev() {
        rects[i] = grow_axis(rects[i + 1], i % 2 == 0, p, rect);
    }
    let first = rects[0];
    let mut cur = Vec::with_capacity(first.width as usize * first.height as usize * spp);
    for y in first.y..first.y + first.height {
        let from =
            ((y - rect.y) as usize * rect.width as usize + (first.x - rect.x) as usize) * spp;
        cur.extend(
            buf[from..from + first.width as usize * spp]
                .iter()
                .map(|s| s.to_fixed()),
        );
    }
    for i in 0..6 {
        cur = box_pass(p, i % 2 == 0, &cur, rects[i], rects[i + 1], spp);
    }
    cur
}

/// Detail's target over `at` into `out` (row-major): `r + (B(r) - Q(r)) + (Q(s) - B(s))` for result
/// `r` and source `s`. `B` is three passes, each a horizontal then a vertical box of radius `p`
/// ([`pass_radius`]) giving `(sum + p).div_euclid(2p + 1)` of an exact `i64` sum; edges replicate,
/// so the buffers must reach `3p` past `at` wherever the map does. `Q` is a `u16` as is or
/// `round(f32 * 65536)`. A `u16` target clamps to `0..=65535`; an `f32` one is
/// `r + correction * 2^-16` in `f64`, unclamped. Constant `r` and `s` give back `r`.
pub fn detail_output<S: Blend>(
    p: u32,
    (result_rect, result): (Rect, &[S]),
    (source_rect, source): (Rect, &[S]),
    at: Rect,
    spp: usize,
    out: &mut [S],
) {
    let samples = |rect: Rect| rect.width as usize * rect.height as usize * spp;
    assert!(
        result_rect.intersect(at) == Some(at) && source_rect.intersect(at) == Some(at),
        "detail_output: `at` lies inside both buffers"
    );
    assert!(
        result.len() == samples(result_rect)
            && source.len() == samples(source_rect)
            && out.len() == samples(at),
        "detail_output: each slice holds its rect's samples"
    );
    let blurred_result = blur_fixed(p, (result_rect, result), at, spp);
    let blurred_source = blur_fixed(p, (source_rect, source), at, spp);
    let mut i = 0;
    for y in at.y..at.y + at.height {
        for x in at.x..at.x + at.width {
            let r_at = ((y - result_rect.y) as usize * result_rect.width as usize
                + (x - result_rect.x) as usize)
                * spp;
            let s_at = ((y - source_rect.y) as usize * source_rect.width as usize
                + (x - source_rect.x) as usize)
                * spp;
            for c in 0..spp {
                let r = result[r_at + c];
                let correction = (blurred_result[i] - r.to_fixed())
                    + (source[s_at + c].to_fixed() - blurred_source[i]);
                out[i] = r.corrected(correction);
                i += 1;
            }
        }
    }
}

/// A stroke's `opacity` as a Q16 factor (`0..=65536`): `opacity * 65536`
/// rounded to nearest, ties away from zero. `1.0` gives `65536`; anything
/// below `0.5 / 65536` (about `7.6e-6`) gives `0`. [`apply`] scales each mask
/// alpha `a` by it as `(a * q + 32768) >> 16`, and a blend that reproduces
/// [`apply`] must convert and scale the same way.
pub fn opacity_q16(opacity: f32) -> u32 {
    (f64::from(opacity) * f64::from(OPAQUE)).round() as u32
}

/// The Q16 alpha (`0..=65536`) the stroke `op` gives the pixel `(x, y)`,
/// testing every segment: the definition [`apply`] reproduces with segment
/// culling.
pub fn alpha(op: &Op, info: ZMapInfo, x: u32, y: u32) -> Result<u32, RetouchError> {
    let stroke = validate(op, info)?;
    let segments = stroke.path.len().saturating_sub(1).max(1);
    Ok(Ramp::new(&stroke).alpha(distance_16(&stroke, x, y, 0..segments)))
}

/// `out` mixed toward `src` by the Q16 `alpha`, rounded to nearest: `out` at
/// alpha 0, `src` at 65536. The sum reaches 2^32 for two full-scale samples,
/// so it is summed in `u64`.
pub fn blend_sample(out: u16, src: u16, alpha: u32) -> u16 {
    debug_assert!(alpha <= OPAQUE, "alpha is Q16 in 0..=65536");
    let (out, src, alpha) = (u64::from(out), u64::from(src), u64::from(alpha));
    ((out * (u64::from(OPAQUE) - alpha) + src * alpha + 32768) >> 16) as u16
}

/// A sample type a stroke blends in. `u16` uses [`blend_sample`]'s integer
/// rule; `f32` mixes linearly with no clamp, so values outside `0..=65535`
/// survive.
pub trait Blend: Sample {
    /// `out` mixed toward `src` by the Q16 `alpha`: `out` at 0, `src` at 65536.
    fn blend(out: Self, src: Self, alpha: u32) -> Self;

    /// How the sum over all samples of `src` compares with that of `out`
    /// (the two slices are one pixel each); `None` when either sum is NaN.
    /// Lighten and Darken decide on this order.
    fn sum_order(src: &[Self], out: &[Self]) -> Option<Ordering>;

    /// The sample in the fixed point Detail's blur sums in.
    fn to_fixed(self) -> i64;

    /// `self` plus a Detail `correction` in that fixed point, clamped to the
    /// sample type's range where it has one.
    fn corrected(self, correction: i64) -> Self;
}

impl Blend for u16 {
    fn blend(out: Self, src: Self, alpha: u32) -> Self {
        blend_sample(out, src, alpha)
    }

    fn sum_order(src: &[Self], out: &[Self]) -> Option<Ordering> {
        let sum = |pixel: &[Self]| pixel.iter().map(|&s| u64::from(s)).sum::<u64>();
        Some(sum(src).cmp(&sum(out)))
    }

    fn to_fixed(self) -> i64 {
        i64::from(self)
    }

    fn corrected(self, correction: i64) -> Self {
        (i64::from(self) + correction).clamp(0, i64::from(u16::MAX)) as u16
    }
}

impl Blend for f32 {
    /// Sums in `f64` in channel order, unclamped, so in-range values add
    /// without rounding.
    fn sum_order(src: &[Self], out: &[Self]) -> Option<Ordering> {
        let sum = |pixel: &[Self]| pixel.iter().map(|&s| f64::from(s)).sum::<f64>();
        sum(src).partial_cmp(&sum(out))
    }

    fn blend(out: Self, src: Self, alpha: u32) -> Self {
        debug_assert!(alpha <= OPAQUE, "alpha is Q16 in 0..=65536");
        let alpha = alpha as f32 / OPAQUE as f32;
        out * (1.0 - alpha) + src * alpha
    }

    /// `round(v * 65536)`, kept within `+-2^40` so window sums cannot
    /// overflow; NaN is 0.
    fn to_fixed(self) -> i64 {
        const LIMIT: f64 = (1u64 << 40) as f64;
        (f64::from(self) * 65536.0).round().clamp(-LIMIT, LIMIT) as i64
    }

    fn corrected(self, correction: i64) -> Self {
        (f64::from(self) + correction as f64 / 65536.0) as f32
    }
}

/// `out` (the pre-stroke pixel) mixed toward `src` by the Q16 `alpha`, every
/// sample by the same `alpha`. Normal always mixes; Lighten only where
/// [`Blend::sum_order`] puts `src` strictly above `out`, Darken strictly
/// below (a tie or NaN sum leaves `out`). Detail mixes like Normal: its `src`
/// is the target [`detail_output`] computes, not the source frame.
pub fn blend_pixel<S: Blend>(mode: BrushMode, out: &mut [S], src: &[S], alpha: u32) {
    let acts = match mode {
        BrushMode::Normal | BrushMode::Detail => true,
        BrushMode::Lighten => S::sum_order(src, out) == Some(Ordering::Greater),
        BrushMode::Darken => S::sum_order(src, out) == Some(Ordering::Less),
    };
    if acts {
        for (o, &s) in out.iter_mut().zip(src) {
            *o = S::blend(*o, s, alpha);
        }
    }
}

/// Whether a stroke with Q16 `alpha` shows its own source at a pixel: more
/// than half of full coverage. The one rule [`top_source`] and
/// [`overlay_colors`] share.
fn shows_source(alpha: u32) -> bool {
    alpha > OPAQUE / 2
}

/// `table`'s colour for `index`, clamped to the last entry.
fn table_color(table: &[[u8; 3]], index: u16) -> [u8; 3] {
    table[usize::from(index).min(table.len() - 1)]
}

/// The source of the latest valid stroke in `ops` whose alpha at `(x, y)`
/// exceeds half of full coverage, `None` when no stroke reaches that far.
pub fn top_source(ops: &[Op], info: ZMapInfo, x: u32, y: u32) -> Option<u16> {
    ops.iter().rev().find_map(|op| {
        let Op::BrushStroke { source_id, .. } = op;
        shows_source(alpha(op, info, x, y).ok()?).then_some(*source_id)
    })
}

/// Fills `out` with each sample's overlay colour over the `step`-th rows and
/// columns of `rect`: `table`'s colour for the stored index in `indices`
/// (clamped to the last entry), then, per valid stroke of `ops` in order, the
/// stroke source's colour wherever the stroke shows its source (the rule of
/// [`top_source`]). Cost follows the samples near each path.
pub fn overlay_colors(
    ops: &[Op],
    info: ZMapInfo,
    rect: Rect,
    step: NonZeroU32,
    table: &[[u8; 3]],
    indices: &[u16],
    out: &mut [[u8; 3]],
) {
    let step = step.get();
    let columns = rect.width.div_ceil(step) as usize;
    let rows = rect.height.div_ceil(step) as usize;
    debug_assert_eq!(indices.len(), columns * rows);
    debug_assert_eq!(out.len(), columns * rows);
    for (color, &index) in out.iter_mut().zip(indices) {
        *color = table_color(table, index);
    }
    for op in ops {
        let Op::BrushStroke { source_id, .. } = op;
        let Ok(stroke) = validate(op, info) else {
            continue;
        };
        let Some(active) = stroke_footprint(&stroke, info).and_then(|fp| rect.intersect(fp)) else {
            continue;
        };
        let over = table_color(table, *source_id);
        let ramp = Ramp::new(&stroke);
        let grid = SegmentGrid::new(&stroke, active);
        // Lattice samples inside `active`: sample `i` sits at `rect.x + i * step`.
        let first = |origin: u32, from: u32| (from - origin).div_ceil(step);
        let end = |origin: u32, to: u32| (to - origin).div_ceil(step);
        for j in first(rect.y, active.y)..end(rect.y, active.y + active.height) {
            let y = rect.y + j * step;
            for i in first(rect.x, active.x)..end(rect.x, active.x + active.width) {
                let x = rect.x + i * step;
                let segments = grid.segments_at(x, y);
                if segments.is_empty() {
                    continue;
                }
                let a = ramp.alpha(distance_16(
                    &stroke,
                    x,
                    y,
                    segments.iter().map(|&s| s as usize),
                ));
                if shows_source(a) {
                    out[j as usize * columns + i as usize] = over;
                }
            }
        }
    }
}

/// Blends the stroke `op`'s source samples over `out`. `out` holds the
/// samples of `out_rect` (row-major, `spp` samples a pixel) and `src` those of
/// `region`, the part of `out_rect` the caller read the source frame for;
/// only pixels inside `region` and the stroke's footprint change.
///
/// A [`BrushMode::Detail`] stroke mixes toward [`detail_output`] of `out` and
/// `src`, whose blur reads [`read_margin`] pixels past the footprint: `region`
/// should reach that far wherever `out_rect` does, and `out_rect` further by
/// the margins of the strokes before it.
///
/// Cost follows the area near the path, not the path's bounding box.
pub fn apply<S: Blend>(
    op: &Op,
    info: ZMapInfo,
    out_rect: Rect,
    out: &mut [S],
    region: Rect,
    src: &[S],
    spp: usize,
) -> Result<(), RetouchError> {
    let stroke = validate(op, info)?;
    if !info.contains(out_rect) || out_rect.intersect(region) != Some(region) {
        return Err(RetouchError::TileOutsideMap { tile: region });
    }
    let samples = |rect: Rect| rect.width as usize * rect.height as usize * spp;
    for (rect, len) in [(out_rect, out.len()), (region, src.len())] {
        if len != samples(rect) {
            return Err(RetouchError::TileBufferSize {
                expected: samples(rect),
                actual: len,
            });
        }
    }
    let Some(active) = stroke_footprint(&stroke, info).and_then(|fp| region.intersect(fp)) else {
        return Ok(());
    };
    let ramp = Ramp::new(&stroke);
    let opacity = u64::from(opacity_q16(stroke.opacity));
    // Detail mixes toward its own target, one pixel per sample of `active`.
    let mut target = Vec::new();
    let (src, src_rect) = if stroke.mode == BrushMode::Detail {
        target.resize(samples(active), S::default());
        let p = pass_radius(op, info)?;
        detail_output(p, (out_rect, out), (region, src), active, spp, &mut target);
        (&target[..], active)
    } else {
        (src, region)
    };
    SegmentGrid::new(&stroke, active).walk(|x, y, segments| {
        let a = ramp.alpha(distance_16(
            &stroke,
            x,
            y,
            segments.iter().map(|&i| i as usize),
        ));
        let a = ((u64::from(a) * opacity + 32768) >> 16) as u32;
        if a == 0 {
            return;
        }
        let out_at =
            ((y - out_rect.y) as usize * out_rect.width as usize + (x - out_rect.x) as usize) * spp;
        let src_at =
            ((y - src_rect.y) as usize * src_rect.width as usize + (x - src_rect.x) as usize) * spp;
        blend_pixel(
            stroke.mode,
            &mut out[out_at..out_at + spp],
            &src[src_at..src_at + spp],
            a,
        );
    });
    Ok(())
}

/// One stroke of [`apply_strokes`]: the op, the frame it blends from and the
/// part of the map it can change ([`footprint`]).
#[derive(Clone, Copy)]
pub struct PlacedStroke<'a> {
    pub op: &'a Op,
    pub frame: usize,
    pub footprint: Rect,
}

/// Blends each of `strokes`, in order, over `out`, the samples of `out_rect`
/// (row-major, `spp` samples a pixel). A stroke reads its frame from `frames`
/// over the part of `out_rect` its footprint meets, widened by
/// [`read_margin`] for a Detail stroke, shifted by `origin` (the crop offset
/// between map and frame coordinates); a stroke whose footprint is clear of
/// `out_rect` reads nothing. A failed frame read is [`RetouchError::Source`]
/// and leaves `out` as the strokes before it made it. Each stroke is blended
/// by `blend`, called as [`apply`] is: [`apply`] itself, or a blend with the
/// same result.
#[allow(clippy::too_many_arguments)]
pub fn apply_strokes<S: Sample>(
    info: ZMapInfo,
    out_rect: Rect,
    out: &mut [S],
    strokes: &[PlacedStroke<'_>],
    frames: &dyn TileReader,
    origin: (u32, u32),
    spp: usize,
    mut blend: impl FnMut(&Op, ZMapInfo, Rect, &mut [S], Rect, &[S], usize) -> Result<(), RetouchError>,
) -> Result<(), RetouchError> {
    let mut frame: Vec<S> = Vec::new();
    for stroke in strokes {
        if stroke.footprint.intersect(out_rect).is_none() {
            continue;
        }
        let margin = read_margin(stroke.op, info)?;
        let region = grow(stroke.footprint, margin, info)
            .intersect(out_rect)
            .expect("the footprint meets out_rect");
        frame.resize(
            region.width as usize * region.height as usize * spp,
            S::default(),
        );
        let at_frame = Rect {
            x: region.x + origin.0,
            y: region.y + origin.1,
            ..region
        };
        S::read_tile(frames, stroke.frame, at_frame, &mut frame)?;
        blend(stroke.op, info, out_rect, out, region, &frame, spp)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    fn stroke_op(source_id: u16, path: Vec<Point>, radius: f32, softness: f32) -> Op {
        Op::BrushStroke {
            source_id,
            path,
            radius,
            softness,
            mode: crate::project::BrushMode::Normal,
            opacity: 1.0,
        }
    }

    fn info(width: u32, height: u32) -> ZMapInfo {
        ZMapInfo::new(width, height, 4).unwrap()
    }

    #[test]
    fn opacity_q16() {
        assert_eq!(super::opacity_q16(1.0), 65536);
        assert_eq!(super::opacity_q16(0.5), 32768);
        assert_eq!(super::opacity_q16(1.0 - f32::EPSILON / 2.0), 65536);
        assert_eq!(super::opacity_q16(1e-9), 0);
        assert_eq!(super::opacity_q16(7.6e-6), 0, "just below 0.5 / 65536");
        assert_eq!(super::opacity_q16(7.7e-6), 1, "just above 0.5 / 65536");
        let mut last = 0;
        for i in 1..=4096u32 {
            let opacity = i as f32 / 4096.0;
            let q = super::opacity_q16(opacity);
            assert!(q >= last, "monotone at {opacity}");
            let exact = f64::from(opacity) * 65536.0;
            assert!((f64::from(q) - exact).abs() <= 0.5, "{opacity}: {q}");
            last = q;
        }
    }

    #[test]
    fn opacity_scales_mask() {
        let info = info(40, 30);
        let out_rect = rect(0, 0, 40, 30);
        let region = rect(8, 6, 20, 16);
        for spp in [1usize, 3] {
            let base: Vec<u16> = (0..40 * 30 * spp)
                .map(|i| (i * 37 % 65536) as u16)
                .collect();
            let src: Vec<u16> = (0..20 * 16 * spp)
                .map(|i| (i * 91 % 65536) as u16)
                .collect();
            let base_f: Vec<f32> = base.iter().map(|&v| f32::from(v) + 0.25).collect();
            let src_f: Vec<f32> = src.iter().map(|&v| f32::from(v) - 0.5).collect();
            for opacity in [0.01f32, 0.5, 1.0] {
                let op = Op::BrushStroke {
                    source_id: 2,
                    path: vec![pt(10, 10), pt(25, 15)],
                    radius: 8.0,
                    softness: 0.5,
                    mode: BrushMode::Normal,
                    opacity,
                };
                let q = u64::from(super::opacity_q16(opacity));
                let mut out = base.clone();
                apply(&op, info, out_rect, &mut out, region, &src, spp).unwrap();
                let mut out_f = base_f.clone();
                apply(&op, info, out_rect, &mut out_f, region, &src_f, spp).unwrap();
                let mut changed = 0;
                for y in 0..30u32 {
                    for x in 0..40u32 {
                        let in_region = (8..28).contains(&x) && (6..22).contains(&y);
                        let a = alpha(&op, info, x, y).unwrap();
                        let a = ((u64::from(a) * q + 32768) >> 16) as u32;
                        for c in 0..spp {
                            let i = (y as usize * 40 + x as usize) * spp + c;
                            let (want, want_f) = if in_region {
                                let at = ((y - 6) as usize * 20 + (x - 8) as usize) * spp + c;
                                (
                                    blend_sample(base[i], src[at], a),
                                    <f32 as Blend>::blend(base_f[i], src_f[at], a),
                                )
                            } else {
                                (base[i], base_f[i])
                            };
                            let label = format!("spp {spp} opacity {opacity} ({x}, {y}, {c})");
                            assert_eq!(out[i], want, "u16 {label}");
                            assert_eq!(out_f[i], want_f, "f32 {label}");
                            changed += usize::from(out[i] != base[i]);
                        }
                    }
                }
                assert!(changed > 0, "spp {spp} opacity {opacity} changes something");
                if opacity == 1.0 {
                    let plain = stroke_op(2, vec![pt(10, 10), pt(25, 15)], 8.0, 0.5);
                    let mut again = base.clone();
                    apply(&plain, info, out_rect, &mut again, region, &src, spp).unwrap();
                    assert_eq!(out, again, "opacity 1.0 is the stroke without the fields");
                }
            }
        }
    }

    /// A one-pixel map under a stroke that covers it fully, blended with `src`
    /// over `out` in `mode` at full opacity.
    fn one_pixel<S: Blend>(mode: BrushMode, out: &[S], src: &[S]) -> Vec<S> {
        let op = Op::BrushStroke {
            source_id: 0,
            path: vec![pt(0, 0)],
            radius: 2.0,
            softness: 0.0,
            mode,
            opacity: 1.0,
        };
        let spp = out.len();
        let mut pixels = out.to_vec();
        let whole = rect(0, 0, 1, 1);
        apply(&op, info(1, 1), whole, &mut pixels, whole, src, spp).unwrap();
        pixels
    }

    #[test]
    fn lighten_darken_equal_sums_and_gray() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        // One sample: that sample decides.
        assert_eq!(<u16 as Blend>::sum_order(&[9], &[7]), Some(Greater));
        assert_eq!(<u16 as Blend>::sum_order(&[7], &[9]), Some(Less));
        assert_eq!(<u16 as Blend>::sum_order(&[7], &[7]), Some(Equal));
        // Two samples: the sum decides although one channel disagrees.
        assert_eq!(<u16 as Blend>::sum_order(&[10, 0], &[3, 6]), Some(Greater));
        // Four samples: alpha counts like the colour channels.
        let src = [0, 0, 0, 65535];
        let out = [100, 100, 100, 0];
        assert_eq!(<u16 as Blend>::sum_order(&src, &out), Some(Greater));
        assert_eq!(<u16 as Blend>::sum_order(&out, &src), Some(Less));
        // Equal sums over different channels are a tie.
        assert_eq!(
            <u16 as Blend>::sum_order(&[1, 5, 3], &[3, 3, 3]),
            Some(Equal)
        );
        assert_eq!(
            <f32 as Blend>::sum_order(&[1.0, 5.0, 3.0], &[3.0, 3.0, 3.0]),
            Some(Equal)
        );

        // Equal sums over different channels, 1 to 4 samples: no mode acts.
        for (out, src) in [
            (vec![5u16], vec![5]),
            (vec![1, 9], vec![9, 1]),
            (vec![1, 5, 3], vec![3, 3, 3]),
            (vec![1, 5, 3, 7], vec![7, 3, 5, 1]),
        ] {
            for mode in [BrushMode::Lighten, BrushMode::Darken] {
                assert_eq!(one_pixel(mode, &out, &src), out, "{mode:?} {out:?}");
            }
        }
        // Gray: the one sample decides. 4 samples: alpha decides with the rest.
        assert_eq!(one_pixel(BrushMode::Lighten, &[5u16], &[8]), [8]);
        assert_eq!(one_pixel(BrushMode::Darken, &[5u16], &[8]), [5]);
        assert_eq!(one_pixel(BrushMode::Lighten, &[5u16, 5], &[0, 9]), [5, 5]);
        assert_eq!(one_pixel(BrushMode::Darken, &[5u16, 5], &[0, 9]), [0, 9]);
        let (out, src) = ([9u16, 9, 9, 0], [0u16, 0, 0, 40]);
        assert_eq!(one_pixel(BrushMode::Lighten, &out, &src), src);
        assert_eq!(one_pixel(BrushMode::Darken, &out, &src), out);
    }

    #[test]
    fn lighten_darken_f32_compare_unclamped() {
        use std::cmp::Ordering::Greater;
        // Negative and above-range samples compare as they are.
        assert_eq!(
            <f32 as Blend>::sum_order(&[-5.0, -5.0], &[-20.0, 3.0]),
            Some(Greater)
        );
        assert_eq!(
            <f32 as Blend>::sum_order(&[70000.0, 0.0, 0.0], &[65535.0, 4000.0, 0.0]),
            Some(Greater)
        );
        // The sum is exact: an f32 accumulator would round both sums to 196605.
        let ulp = 1.0 / 256.0;
        assert_eq!(
            <f32 as Blend>::sum_order(&[65535.0; 3], &[65535.0, 65535.0, 65535.0 - ulp]),
            Some(Greater)
        );
        // A NaN sum on either side orders nothing.
        assert_eq!(
            <f32 as Blend>::sum_order(&[f32::NAN, 1.0], &[0.0, 0.0]),
            None
        );
        assert_eq!(
            <f32 as Blend>::sum_order(&[0.0, 0.0], &[1.0, f32::NAN]),
            None
        );

        // Full alpha lands on the source, outside 0..=65535 too.
        let (out, src) = ([-30.0f32, 1.0, 2.0], [70000.0f32, -9.0, 3.0]);
        assert_eq!(one_pixel(BrushMode::Lighten, &out, &src), src);
        assert_eq!(one_pixel(BrushMode::Darken, &out, &src), out);
        let (out, src) = ([70000.0f32, 0.0], [-5.0f32, 4.0]);
        assert_eq!(one_pixel(BrushMode::Darken, &out, &src), src);
        assert_eq!(one_pixel(BrushMode::Lighten, &out, &src), out);
        // A NaN sum acts in neither mode.
        for (out, src) in [([f32::NAN, 1.0], [5.0, 5.0]), ([1.0, 1.0], [f32::NAN, 5.0])] {
            for mode in [BrushMode::Lighten, BrushMode::Darken] {
                let got = one_pixel(mode, &out, &src);
                assert_eq!(
                    got.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
                    out.map(f32::to_bits),
                    "{mode:?}"
                );
            }
        }
    }

    /// A 40 x 30 map and the samples of a 20 x 16 source region at (8, 6), in
    /// which some pixels are brighter than the map and some darker.
    fn varied_maps(spp: usize) -> (Vec<u16>, Vec<u16>) {
        let base = (0..40 * 30 * spp)
            .map(|i| (i * 37 % 65536) as u16)
            .collect();
        let src = (0..20 * 16 * spp)
            .map(|i| ((i as u64 * 2_654_435_761) >> 7) as u16)
            .collect();
        (base, src)
    }

    fn mode_stroke(mode: BrushMode, opacity: f32) -> Op {
        Op::BrushStroke {
            source_id: 2,
            path: vec![pt(10, 10), pt(25, 15)],
            radius: 8.0,
            softness: 0.5,
            mode,
            opacity,
        }
    }

    /// The Q16 alpha `op` gives `(x, y)` after the opacity scaling.
    fn scaled_alpha(op: &Op, info: ZMapInfo, x: u32, y: u32) -> u32 {
        let Op::BrushStroke { opacity, .. } = op;
        let q = u64::from(super::opacity_q16(*opacity));
        ((u64::from(alpha(op, info, x, y).unwrap()) * q + 32768) >> 16) as u32
    }

    #[test]
    fn mode_output_is_the_one_formula() {
        let info = info(40, 30);
        let (out_rect, region) = (rect(0, 0, 40, 30), rect(8, 6, 20, 16));
        let spp = 3;
        let (base, src) = varied_maps(spp);
        for mode in [BrushMode::Normal, BrushMode::Lighten, BrushMode::Darken] {
            for opacity in [0.5f32, 1.0] {
                let op = mode_stroke(mode, opacity);
                let mut out = base.clone();
                apply(&op, info, out_rect, &mut out, region, &src, spp).unwrap();
                let mut changed = 0;
                for y in 6..22u32 {
                    for x in 8..28u32 {
                        let a = scaled_alpha(&op, info, x, y);
                        let i = (y as usize * 40 + x as usize) * spp;
                        let at = ((y - 6) as usize * 20 + (x - 8) as usize) * spp;
                        let mut want = base[i..i + spp].to_vec();
                        if a > 0 {
                            blend_pixel(mode, &mut want, &src[at..at + spp], a);
                        }
                        let label = format!("{mode:?} opacity {opacity} ({x}, {y})");
                        assert_eq!(out[i..i + spp], want[..], "{label}");
                        changed += usize::from(want[..] != base[i..i + spp]);
                    }
                }
                assert!(changed > 0, "{mode:?} opacity {opacity} changes something");
            }
        }
    }

    #[test]
    fn lighten_moves_every_channel_by_one_alpha() {
        // The source is darker in the middle channel but brighter by sum.
        let (out, src) = ([100u16, 50000, 100], [40000u16, 10, 40000]);
        let a = 20000;
        let mut lightened = out;
        blend_pixel(BrushMode::Lighten, &mut lightened, &src, a);
        for c in 0..3 {
            assert_eq!(lightened[c], blend_sample(out[c], src[c], a), "channel {c}");
        }
        assert!(lightened[1] < out[1], "the darker channel moves down too");
        let mut darkened = out;
        blend_pixel(BrushMode::Darken, &mut darkened, &src, a);
        assert_eq!(darkened, out, "Darken leaves a brighter-by-sum pixel");
        // Swapped roles: the source is darker by sum.
        let mut darkened = src;
        blend_pixel(BrushMode::Darken, &mut darkened, &out, a);
        let want = [0, 1, 2].map(|c| blend_sample(src[c], out[c], a));
        assert_eq!(darkened, want, "Darken blends a darker-by-sum source");
        let mut lightened = src;
        blend_pixel(BrushMode::Lighten, &mut lightened, &out, a);
        assert_eq!(lightened, src, "Lighten leaves a darker-by-sum pixel");
    }

    fn sum16(pixel: &[u16]) -> u32 {
        pixel.iter().map(|&s| u32::from(s)).sum()
    }

    #[test]
    fn lighten_darken_u16_follow_the_channel_sum() {
        let info = info(40, 30);
        let (out_rect, region) = (rect(0, 0, 40, 30), rect(8, 6, 20, 16));
        for spp in 1..=4usize {
            let (base, src) = varied_maps(spp);
            for mode in [BrushMode::Lighten, BrushMode::Darken] {
                for opacity in [0.5f32, 1.0] {
                    let op = mode_stroke(mode, opacity);
                    let mut out = base.clone();
                    apply(&op, info, out_rect, &mut out, region, &src, spp).unwrap();
                    let mut changed = 0;
                    let mut skipped = 0;
                    for y in 0..30u32 {
                        for x in 0..40u32 {
                            let i = (y as usize * 40 + x as usize) * spp;
                            let before = &base[i..i + spp];
                            let mut want = before.to_vec();
                            let in_region = (8..28).contains(&x) && (6..22).contains(&y);
                            let a = scaled_alpha(&op, info, x, y);
                            if in_region && a > 0 {
                                let at = ((y - 6) as usize * 20 + (x - 8) as usize) * spp;
                                let s = &src[at..at + spp];
                                let qualifies = match mode {
                                    BrushMode::Lighten => sum16(s) > sum16(before),
                                    _ => sum16(s) < sum16(before),
                                };
                                if qualifies {
                                    for c in 0..spp {
                                        want[c] = blend_sample(before[c], s[c], a);
                                    }
                                } else {
                                    skipped += 1;
                                }
                            }
                            let label = format!("spp {spp} {mode:?} opacity {opacity} ({x}, {y})");
                            assert_eq!(out[i..i + spp], want[..], "{label}");
                            changed += usize::from(want[..] != *before);
                        }
                    }
                    let label = format!("spp {spp} {mode:?} opacity {opacity}");
                    assert!(changed > 0, "{label} changes something");
                    assert!(skipped > 0, "{label} leaves covered pixels alone");
                }
            }
        }
    }

    #[test]
    fn lighten_darken_scale_by_opacity() {
        let info = info(40, 30);
        let (out_rect, region) = (rect(0, 0, 40, 30), rect(8, 6, 20, 16));
        let spp = 3;
        let (base, src) = varied_maps(spp);
        let mut normal = base.clone();
        apply(
            &mode_stroke(BrushMode::Normal, 0.5),
            info,
            out_rect,
            &mut normal,
            region,
            &src,
            spp,
        )
        .unwrap();
        for mode in [BrushMode::Lighten, BrushMode::Darken] {
            let op = mode_stroke(mode, 0.5);
            let mut out = base.clone();
            apply(&op, info, out_rect, &mut out, region, &src, spp).unwrap();
            let mut covered = 0;
            for y in 6..22u32 {
                for x in 8..28u32 {
                    let i = (y as usize * 40 + x as usize) * spp;
                    let at = ((y - 6) as usize * 20 + (x - 8) as usize) * spp;
                    let (s, before) = (&src[at..at + spp], &base[i..i + spp]);
                    let qualifies = match mode {
                        BrushMode::Lighten => sum16(s) > sum16(before),
                        _ => sum16(s) < sum16(before),
                    };
                    let want = if qualifies {
                        &normal[i..i + spp]
                    } else {
                        before
                    };
                    assert_eq!(out[i..i + spp], *want, "{mode:?} ({x}, {y})");
                    let full = alpha(&op, info, x, y).unwrap() == OPAQUE;
                    covered += usize::from(full && qualifies);
                }
            }
            assert!(covered > 0, "{mode:?}: a fully covered pixel qualifies");
        }
    }

    #[test]
    fn darken_decides_on_the_pre_stroke_result() {
        let info = info(40, 30);
        let (out_rect, region) = (rect(0, 0, 40, 30), rect(8, 6, 20, 16));
        let spp = 3;
        let (base, src) = varied_maps(spp);
        let other: Vec<u16> = (0..20 * 16 * spp)
            .map(|i| (i * 53 % 65536) as u16)
            .collect();
        let op = mode_stroke(BrushMode::Darken, 1.0);
        let mut stacked = base.clone();
        let first = mode_stroke(BrushMode::Normal, 1.0);
        apply(&first, info, out_rect, &mut stacked, region, &src, spp).unwrap();
        let after_normal = stacked.clone();
        apply(&op, info, out_rect, &mut stacked, region, &other, spp).unwrap();
        // The same Darken stroke deciding against the untouched map instead.
        let mut wrong = after_normal.clone();
        for y in 6..22u32 {
            for x in 8..28u32 {
                let i = (y as usize * 40 + x as usize) * spp;
                let at = ((y - 6) as usize * 20 + (x - 8) as usize) * spp;
                let a = scaled_alpha(&op, info, x, y);
                let s = &other[at..at + spp];
                let (mid, orig) = (&after_normal[i..i + spp], &base[i..i + spp]);
                let want = if a > 0 && sum16(s) < sum16(mid) {
                    [0, 1, 2].map(|c| blend_sample(mid[c], s[c], a))
                } else {
                    [mid[0], mid[1], mid[2]]
                };
                assert_eq!(stacked[i..i + spp], want, "({x}, {y})");
                if a > 0 && sum16(s) < sum16(orig) {
                    for c in 0..spp {
                        wrong[i + c] = blend_sample(mid[c], s[c], a);
                    }
                }
            }
        }
        assert_ne!(stacked, wrong, "the pre-stroke result changes the outcome");
    }

    /// A pixel's samples, `Normal`-blended toward `target` by `alpha`.
    fn mixed<S: Blend>(before: &[S], target: &[S], alpha: u32) -> Vec<S> {
        let mut pixel = before.to_vec();
        blend_pixel(BrushMode::Normal, &mut pixel, target, alpha);
        pixel
    }

    #[test]
    fn detail_of_constant_input_keeps_the_result() {
        let info = info(40, 30);
        let whole = rect(0, 0, 40, 30);
        let op = Op::BrushStroke {
            source_id: 0,
            path: vec![pt(20, 15)],
            radius: 8.0,
            softness: 0.0,
            mode: BrushMode::Detail,
            opacity: 1.0,
        };
        let normal = Op::BrushStroke {
            source_id: 0,
            path: vec![pt(20, 15)],
            radius: 8.0,
            softness: 0.0,
            mode: BrushMode::Normal,
            opacity: 1.0,
        };
        let covered = (0..30u32)
            .flat_map(|y| (0..40u32).map(move |x| (x, y)))
            .filter(|&(x, y)| alpha(&op, info, x, y).unwrap() == OPAQUE)
            .count();
        assert!(covered > 0);
        for spp in [1usize, 3] {
            let channels = |base: f32, step: f32| -> Vec<f32> {
                (0..40 * 30 * spp)
                    .map(|i| base + step * (i % spp) as f32)
                    .collect()
            };
            let (result, source) = (channels(12345.0, 1000.0), channels(54321.0, -500.0));
            let to_u16 = |v: &[f32]| v.iter().map(|&s| s as u16).collect::<Vec<_>>();
            let (result_u, source_u) = (to_u16(&result), to_u16(&source));
            let mut out = result_u.clone();
            apply(&op, info, whole, &mut out, whole, &source_u, spp).unwrap();
            assert_eq!(out, result_u, "u16 spp {spp}");
            let mut control = result_u.clone();
            apply(&normal, info, whole, &mut control, whole, &source_u, spp).unwrap();
            assert_ne!(control, result_u, "Normal changes the same pixels");

            let fractional =
                |v: &[f32], shift: f32| v.iter().map(|&s| s + shift).collect::<Vec<_>>();
            let (result_f, source_f) = (fractional(&result, 0.3), fractional(&source, -70_000.7));
            let mut out_f = result_f.clone();
            apply(&op, info, whole, &mut out_f, whole, &source_f, spp).unwrap();
            let bits = |v: &[f32]| v.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&out_f), bits(&result_f), "f32 spp {spp}");
        }
    }

    #[test]
    fn detail_apply_blends_toward_the_naive_target() {
        let info = info(40, 30);
        let (out_rect, region) = (rect(0, 0, 40, 30), rect(8, 6, 20, 16));
        for spp in [1usize, 3] {
            let (base, mut src) = varied_maps(spp);
            // Saturated source samples push the target past both ends.
            for (i, s) in src.iter_mut().enumerate() {
                match i % 7 {
                    0 => *s = 0,
                    3 => *s = 65535,
                    _ => {}
                }
            }
            let (base_f, src_f) = f32_maps(spp);
            for opacity in [0.5f32, 1.0] {
                let op = mode_stroke(BrushMode::Detail, opacity);
                let p = pass_radius(&op, info).unwrap();
                let active = footprint(&op, info)
                    .unwrap()
                    .unwrap()
                    .intersect(region)
                    .unwrap();
                let mut out = base.clone();
                apply(&op, info, out_rect, &mut out, region, &src, spp).unwrap();
                let mut out_f = base_f.clone();
                apply(&op, info, out_rect, &mut out_f, region, &src_f, spp).unwrap();
                let target = naive_detail(p, (out_rect, &base), (region, &src), active, spp);
                let target_f = naive_detail(p, (out_rect, &base_f), (region, &src_f), active, spp);
                let (mut clamped_low, mut clamped_high, mut beyond) = (0, 0, 0);
                for y in 0..30u32 {
                    for x in 0..40u32 {
                        let i = (y as usize * 40 + x as usize) * spp;
                        let (mut want, mut want_f) =
                            (base[i..i + spp].to_vec(), base_f[i..i + spp].to_vec());
                        if active.intersect(rect(x, y, 1, 1)).is_some() {
                            let a = scaled_alpha(&op, info, x, y);
                            let t = ((y - active.y) * active.width + (x - active.x)) as usize * spp;
                            if a > 0 {
                                want = mixed(&want, &target[t..t + spp], a);
                                want_f = mixed(&want_f, &target_f[t..t + spp], a);
                            }
                            clamped_low += target[t..t + spp].iter().filter(|&&v| v == 0).count();
                            clamped_high +=
                                target[t..t + spp].iter().filter(|&&v| v == 65535).count();
                            beyond += target_f[t..t + spp]
                                .iter()
                                .filter(|&&v| !(0.0..=65535.0).contains(&v))
                                .count();
                        }
                        let label = format!("spp {spp} opacity {opacity} ({x}, {y})");
                        assert_eq!(out[i..i + spp], want[..], "u16 {label}");
                        assert_eq!(out_f[i..i + spp], want_f[..], "f32 {label}");
                    }
                }
                assert!(
                    clamped_low > 0 && clamped_high > 0,
                    "u16 targets hit both clamps"
                );
                assert!(beyond > 0, "f32 targets leave 0..=65535");
                assert_ne!(out, base);
            }
        }
    }

    /// The documented blur by direct window sums, one pass at a time over the
    /// whole buffer, edges clamped to the buffer.
    fn naive_blur(p: u32, rect: Rect, fixed: &[i64], spp: usize) -> Vec<i64> {
        let (w, h) = (rect.width as i64, rect.height as i64);
        let p = i64::from(p);
        let mut cur = fixed.to_vec();
        for _pass in 0..3 {
            for horizontal in [true, false] {
                let prev = cur.clone();
                for y in 0..h {
                    for x in 0..w {
                        for c in 0..spp {
                            let sum: i64 = (-p..=p)
                                .map(|d| {
                                    let (xx, yy) = if horizontal {
                                        ((x + d).clamp(0, w - 1), y)
                                    } else {
                                        (x, (y + d).clamp(0, h - 1))
                                    };
                                    prev[(yy * w + xx) as usize * spp + c]
                                })
                                .sum();
                            cur[(y * w + x) as usize * spp + c] = (sum + p).div_euclid(2 * p + 1);
                        }
                    }
                }
            }
        }
        cur
    }

    /// `r + (B(r) - Q(r)) + (Q(s) - B(s))` for every sample of `at`, by
    /// [`naive_blur`] over each whole buffer.
    fn naive_detail<S: Blend>(
        p: u32,
        (r_rect, r): (Rect, &[S]),
        (s_rect, s): (Rect, &[S]),
        at: Rect,
        spp: usize,
    ) -> Vec<S> {
        let fixed = |v: &[S]| v.iter().map(|&x| x.to_fixed()).collect::<Vec<_>>();
        let (qr, qs) = (fixed(r), fixed(s));
        let (br, bs) = (
            naive_blur(p, r_rect, &qr, spp),
            naive_blur(p, s_rect, &qs, spp),
        );
        let mut out = Vec::new();
        for y in at.y..at.y + at.height {
            for x in at.x..at.x + at.width {
                for c in 0..spp {
                    let ri = ((y - r_rect.y) * r_rect.width + (x - r_rect.x)) as usize * spp + c;
                    let si = ((y - s_rect.y) * s_rect.width + (x - s_rect.x)) as usize * spp + c;
                    out.push(r[ri].corrected((br[ri] - qr[ri]) + (qs[si] - bs[si])));
                }
            }
        }
        out
    }

    fn f32_maps(spp: usize) -> (Vec<f32>, Vec<f32>) {
        let (base, src) = varied_maps(spp);
        (
            base.iter().map(|&v| f32::from(v) * 1.1 - 3000.25).collect(),
            src.iter().map(|&v| f32::from(v) * 0.9 + 0.5).collect(),
        )
    }

    #[test]
    fn detail_blur_is_three_h_then_v_box_passes() {
        // One row: the vertical passes see a constant column. Hand-computed
        // for p = 1: each pass is `(a + b + c + 1) / 3`, edges replicated.
        let line = rect(0, 0, 7, 1);
        let result = [0u16, 0, 0, 3000, 0, 0, 0];
        let source = [0u16; 7];
        let mut out = [0u16; 7];
        detail_output(1, (line, &result), (line, &source), line, 1, &mut out);
        assert_eq!(out, [111, 333, 667, 778, 667, 333, 111]);
    }

    #[test]
    fn detail_output_matches_the_naive_definition() {
        let (out_rect, region) = (rect(0, 0, 40, 30), rect(8, 6, 20, 16));
        for spp in [1usize, 3] {
            let (base, src) = varied_maps(spp);
            let (base_f, src_f) = f32_maps(spp);
            for p in [1u32, 2, 4, 9] {
                for at in [rect(10, 8, 10, 8), region, rect(8, 6, 7, 16)] {
                    let label = format!("spp {spp} p {p} at {at:?}");
                    let want = naive_detail(p, (out_rect, &base), (region, &src), at, spp);
                    let mut got = vec![0u16; want.len()];
                    detail_output(p, (out_rect, &base), (region, &src), at, spp, &mut got);
                    assert_eq!(got, want, "u16 {label}");
                    let want = naive_detail(p, (out_rect, &base_f), (region, &src_f), at, spp);
                    let mut got = vec![0f32; want.len()];
                    detail_output(p, (out_rect, &base_f), (region, &src_f), at, spp, &mut got);
                    assert_eq!(got, want, "f32 {label}");
                }
            }
        }
    }

    #[test]
    fn detail_u16_clamps_and_f32_does_not() {
        // A bright result under a source spike: the high-pass pushes past both ends.
        let line = rect(0, 0, 9, 1);
        let result = [65535u16, 65535, 65535, 65535, 65535, 0, 0, 0, 0];
        let source = [0u16, 0, 0, 65535, 0, 0, 0, 0, 65535];
        let mut out = [0u16; 9];
        detail_output(1, (line, &result), (line, &source), line, 1, &mut out);
        assert_eq!(
            out,
            naive_detail(1, (line, &result), (line, &source), line, 1)[..]
        );
        assert!(out.contains(&65535) && out.contains(&0), "{out:?}");
        let to_f = |v: &[u16]| v.iter().map(|&s| f32::from(s)).collect::<Vec<_>>();
        let (rf, sf) = (to_f(&result), to_f(&source));
        let mut out_f = [0f32; 9];
        detail_output(1, (line, &rf), (line, &sf), line, 1, &mut out_f);
        assert!(out_f.iter().any(|&v| v > 65535.0), "{out_f:?}");
        assert!(out_f.iter().any(|&v| v < 0.0), "{out_f:?}");
    }

    #[test]
    fn detail_pass_radius_and_margin_follow_the_radius() {
        let map = info(64, 64);
        let detail = |radius| Op::BrushStroke {
            source_id: 0,
            path: vec![pt(5, 5)],
            radius,
            softness: 0.5,
            mode: BrushMode::Detail,
            opacity: 1.0,
        };
        for (radius, p) in [
            (0.5f32, 1u32),
            (1.0, 1),
            (3.0, 1),
            (7.5, 3),
            (100.0, 33),
            (5000.0, 1667),
        ] {
            assert_eq!(pass_radius(&detail(radius), map).unwrap(), p, "{radius}");
            assert_eq!(
                read_margin(&detail(radius), map).unwrap(),
                3 * p,
                "{radius}"
            );
        }
        for mode in [BrushMode::Normal, BrushMode::Lighten, BrushMode::Darken] {
            let op = Op::BrushStroke {
                source_id: 0,
                path: vec![pt(5, 5)],
                radius: 100.0,
                softness: 0.5,
                mode,
                opacity: 1.0,
            };
            assert_eq!(read_margin(&op, map).unwrap(), 0, "{mode:?}");
        }
        let bad = stroke_op(9, vec![pt(5, 5)], 3.0, 0.5);
        assert!(matches!(
            read_margin(&bad, map),
            Err(RetouchError::SourceOutOfRange { .. })
        ));
    }

    fn place<'a>(info: ZMapInfo, ops: &'a [Op]) -> Vec<PlacedStroke<'a>> {
        ops.iter()
            .map(|op| {
                let Op::BrushStroke { source_id, .. } = op;
                PlacedStroke {
                    op,
                    frame: usize::from(*source_id),
                    footprint: footprint(op, info).unwrap().unwrap(),
                }
            })
            .collect()
    }

    fn mode_at(mode: BrushMode, x: i32, y: i32) -> Op {
        Op::BrushStroke {
            source_id: 0,
            path: vec![pt(x, y)],
            radius: 7.5,
            softness: 0.5,
            mode,
            opacity: 1.0,
        }
    }

    #[test]
    fn read_extent_without_a_detail_stroke_is_the_write_rect() {
        let map = info(400, 300);
        let write = rect(100, 100, 10, 10);
        let ops = [
            mode_at(BrushMode::Normal, 105, 105),
            mode_at(BrushMode::Lighten, 105, 105),
            mode_at(BrushMode::Darken, 108, 102),
            mode_at(BrushMode::Detail, 300, 250),
        ];
        let placed = place(map, &ops);
        assert_eq!(read_extent(map, &placed, write), write);
        assert_eq!(read_extent(map, &[], write), write);
        assert_eq!(read_extent(map, &placed[..3], write), write);
    }

    #[test]
    fn read_extent_grows_by_each_detail_margin_along_the_chain() {
        let map = info(400, 300);
        let write = rect(100, 100, 10, 10);
        // Radius 7.5 gives p = 3, so every Detail stroke adds 9. The first
        // stroke is clear of `write` and of the extent after the last one.
        let ops = [
            mode_at(BrushMode::Detail, 135, 105),
            mode_at(BrushMode::Detail, 125, 105),
            mode_at(BrushMode::Detail, 105, 105),
        ];
        let placed = place(map, &ops);
        assert_eq!(placed[0].footprint.intersect(write), None);
        assert_eq!(read_extent(map, &placed[2..], write), rect(91, 91, 28, 28));
        assert_eq!(read_extent(map, &placed[1..], write), rect(91, 88, 37, 34));
        assert_eq!(read_extent(map, &placed, write), rect(91, 88, 46, 34));
        // A Normal stroke between them widens nothing and does not break the chain.
        let with_normal = [
            ops[0].clone(),
            mode_at(BrushMode::Normal, 120, 105),
            ops[1].clone(),
            ops[2].clone(),
        ];
        let between = place(map, &with_normal);
        assert_eq!(read_extent(map, &between, write), rect(91, 88, 46, 34));
    }

    #[test]
    fn read_extent_is_clamped_to_the_map() {
        let map = info(400, 300);
        let ops = [mode_at(BrushMode::Detail, 2, 2)];
        assert_eq!(
            read_extent(map, &place(map, &ops), rect(0, 0, 5, 5)),
            rect(0, 0, 14, 14)
        );
        let ops = [mode_at(BrushMode::Detail, 397, 297)];
        assert_eq!(
            read_extent(map, &place(map, &ops), rect(395, 295, 5, 5)),
            rect(386, 286, 14, 14)
        );
    }

    /// A frame source that refuses a read outside its `width` x `height`.
    struct Framed {
        width: u32,
        height: u32,
    }

    impl crate::algorithm::TileReader for Framed {
        fn read_tile_into(
            &self,
            frame: usize,
            r: Rect,
            out: &mut [u16],
        ) -> Result<(), crate::algorithm::AlgorithmError> {
            if r.x + r.width > self.width || r.y + r.height > self.height {
                return Err(crate::algorithm::AlgorithmError::Failed {
                    algorithm: "test frame".into(),
                    detail: format!(
                        "read {r:?} outside the {}x{} frame",
                        self.width, self.height
                    ),
                });
            }
            for (i, o) in out.iter_mut().enumerate() {
                let x = u64::from(r.x + i as u32 % r.width);
                let y = u64::from(r.y + i as u32 / r.width);
                *o = (((x * 7919 + y * 104_729 + frame as u64 * 31) * 2_654_435_761) >> 9) as u16;
            }
            Ok(())
        }
    }

    #[test]
    fn detail_reads_its_margin_at_every_map_edge() {
        let map = info(40, 30);
        let whole = rect(0, 0, 40, 30);
        let origin = (7, 3);
        let frames = Framed {
            width: 47,
            height: 33,
        };
        let base: Vec<u16> = (0..40 * 30).map(|i| (i * 37 % 65536) as u16).collect();
        let mut full = vec![0u16; 40 * 30];
        crate::algorithm::TileReader::read_tile_into(&frames, 1, rect(7, 3, 40, 30), &mut full)
            .unwrap();
        let edges = [
            (-2, 15),
            (0, 0),
            (20, -3),
            (39, 29),
            (42, 15),
            (20, 32),
            (0, 15),
            (39, 15),
            (20, 0),
            (20, 29),
        ];
        for (x, y) in edges {
            let op = Op::BrushStroke {
                source_id: 1,
                path: vec![pt(x, y)],
                radius: 7.5,
                softness: 0.5,
                mode: BrushMode::Detail,
                opacity: 0.75,
            };
            let at = footprint(&op, map).unwrap().unwrap();
            let p = pass_radius(&op, map).unwrap();
            let target = naive_detail(p, (whole, &base), (whole, &full), at, 1);
            let mut want = base.clone();
            for py in at.y..at.y + at.height {
                for px in at.x..at.x + at.width {
                    let i = (py * 40 + px) as usize;
                    let t = ((py - at.y) * at.width + (px - at.x)) as usize;
                    let a = scaled_alpha(&op, map, px, py);
                    want[i] = mixed(&want[i..i + 1], &target[t..t + 1], a)[0];
                }
            }
            assert_ne!(want, base, "stroke at ({x}, {y}) changes something");
            let ops = [op];
            let mut out = base.clone();
            apply_strokes(
                map,
                whole,
                &mut out,
                &place(map, &ops),
                &frames,
                origin,
                1,
                apply,
            )
            .unwrap_or_else(|e| panic!("stroke at ({x}, {y}): {e}"));
            assert!(out == want, "stroke at ({x}, {y})");
        }
    }

    fn stroke_with(mode: BrushMode, opacity: f32) -> Op {
        Op::BrushStroke {
            source_id: 0,
            path: vec![pt(1, 1)],
            radius: 3.0,
            softness: 0.0,
            mode,
            opacity,
        }
    }

    const MODES: [BrushMode; 4] = [
        BrushMode::Normal,
        BrushMode::Lighten,
        BrushMode::Darken,
        BrushMode::Detail,
    ];

    #[test]
    fn opacity_bounds() {
        let info = info(32, 32);
        for opacity in [
            0.0,
            -0.5,
            1.0001,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            for mode in MODES {
                let op = stroke_with(mode, opacity);
                assert!(
                    matches!(
                        validate(&op, info),
                        Err(RetouchError::InvalidOpacity { .. })
                    ),
                    "{mode:?} {opacity}"
                );
                assert!(matches!(
                    footprint(&op, info),
                    Err(RetouchError::InvalidOpacity { .. })
                ));
            }
        }
        for opacity in [1e-9, 1e-6, 0.5, 1.0] {
            for mode in MODES {
                let op = stroke_with(mode, opacity);
                assert!(validate(&op, info).is_ok(), "{mode:?} {opacity}");
                assert!(footprint(&op, info).is_ok(), "{mode:?} {opacity}");
            }
        }
        // The earlier checks keep their place in the order.
        let bad = |path: Vec<Point>, source_id, radius, softness| Op::BrushStroke {
            source_id,
            path,
            radius,
            softness,
            mode: BrushMode::Normal,
            opacity: 0.0,
        };
        assert!(matches!(
            validate(&bad(vec![], 0, 3.0, 0.0), info),
            Err(RetouchError::EmptyPath)
        ));
        assert!(matches!(
            validate(&bad(vec![pt(1, 1)], 9, 3.0, 0.0), info),
            Err(RetouchError::SourceOutOfRange { .. })
        ));
        assert!(matches!(
            validate(&bad(vec![pt(1, 1)], 0, 0.0, 0.0), info),
            Err(RetouchError::InvalidRadius { .. })
        ));
        assert!(matches!(
            validate(&bad(vec![pt(1, 1)], 0, 3.0, 2.0), info),
            Err(RetouchError::InvalidSoftness { .. })
        ));
    }

    #[test]
    fn f32_blend_keeps_the_endpoints_and_does_not_clamp() {
        assert_eq!(<f32 as Blend>::blend(70_000.5, 12.0, 0), 70_000.5);
        assert_eq!(<f32 as Blend>::blend(70_000.5, 12.0, OPAQUE), 12.0);
        assert_eq!(<f32 as Blend>::blend(-300.25, 90_000.0, OPAQUE), 90_000.0);
        let mixed = <f32 as Blend>::blend(100_000.0, 20_000.0, OPAQUE / 4);
        assert_eq!(mixed, 80_000.0, "a quarter of the way from 100000 to 20000");
        let below = <f32 as Blend>::blend(-500.0, 100.0, OPAQUE / 2);
        assert_eq!(below, -200.0, "negative samples blend unclamped");
    }

    #[test]
    fn u16_blend_is_the_integer_formula() {
        for (out, src, alpha) in [(0, 65_535, 1), (65_535, 0, 65_535), (123, 456, 32_768)] {
            assert_eq!(
                <u16 as Blend>::blend(out, src, alpha),
                blend_sample(out, src, alpha)
            );
        }
    }

    #[test]
    fn apply_blends_f32_samples_without_a_clamp() {
        let map = info(4, 1);
        let op = stroke_op(0, vec![pt(0, 0), pt(3, 0)], 10.0, 0.0);
        let whole = rect(0, 0, 4, 1);
        let mut out = vec![90_000.0f32; 4];
        apply(&op, map, whole, &mut out, whole, &[70_000.0f32; 4], 1).unwrap();
        assert_eq!(
            out,
            vec![70_000.0f32; 4],
            "an opaque stroke copies the source"
        );
        let mut out = vec![70_000.0f32; 4];
        apply(&op, map, whole, &mut out, whole, &[-20.0f32; 4], 1).unwrap();
        assert_eq!(out, vec![-20.0f32; 4]);
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// A deterministic pseudo-random walk of `count` points starting at
    /// `(x, y)`, each step at most `step` pixels along either axis.
    fn walk(seed: u64, count: usize, (mut x, mut y): (i32, i32), step: i32) -> Vec<Point> {
        let mut state = seed;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as i32).rem_euclid(2 * step + 1) - step
        };
        (0..count)
            .map(|_| {
                x += next();
                y += next();
                pt(x, y)
            })
            .collect()
    }

    #[test]
    fn ends_are_exact() {
        for v in 0..=u16::MAX {
            // The other operand is multiplied by zero at an end, so any
            // partner proves the end for `v`.
            for other in [0, 1, 32768, u16::MAX, !v, v.wrapping_mul(31)] {
                assert_eq!(blend_sample(v, other, 0), v, "alpha 0 keeps {v}");
                assert_eq!(blend_sample(other, v, 65536), v, "alpha 65536 gives {v}");
            }
            for alpha in [1, 255, 32768, 65535] {
                assert_eq!(blend_sample(v, v, alpha), v, "equal samples stay put");
            }
        }
        assert_eq!(blend_sample(0, u16::MAX, 32768), 32768, "half rounds up");

        let info = info(64, 64);
        for softness in [0.0, 0.5, 1.0] {
            let op = stroke_op(0, vec![pt(30, 30), pt(40, 30)], 8.0, softness);
            assert_eq!(alpha(&op, info, 35, 30).unwrap(), 65536, "on the path");
            assert_eq!(alpha(&op, info, 35, 38).unwrap(), 0, "at the radius");
            assert_eq!(alpha(&op, info, 35, 50).unwrap(), 0, "beyond the radius");
        }
        let hard = stroke_op(0, vec![pt(30, 30)], 8.0, 0.0);
        for (x, y) in [(30, 37), (35, 35), (37, 30), (23, 30)] {
            assert_eq!(alpha(&hard, info, x, y).unwrap(), 65536, "({x}, {y})");
        }
    }

    #[test]
    fn mask_is_monotone_and_dither_free() {
        let info = info(200, 200);
        for radius in [1.5f32, 12.0, 40.0] {
            for softness in [0.0f32, 0.3, 1.0] {
                let op = stroke_op(0, vec![pt(100, 100)], radius, softness);
                let core = f64::from(radius) * (1.0 - f64::from(softness));
                let reach = radius as u32 + 3;
                let mut last = u32::MAX;
                for dx in 0..=reach {
                    let a = alpha(&op, info, 100 + dx, 100).unwrap();
                    assert!(a <= last, "r {radius} s {softness}: alpha rises at {dx}");
                    if f64::from(dx) <= core - 0.1 && f64::from(dx) < f64::from(radius) {
                        assert_eq!(a, 65536, "r {radius} s {softness}: inside the core at {dx}");
                    }
                    if f64::from(dx) >= f64::from(radius) + 0.1 {
                        assert_eq!(a, 0, "r {radius} s {softness}: outside at {dx}");
                    }
                    last = a;
                }
                // Equal squared distance, equal alpha: no coordinate pattern.
                let mut by_d2 = std::collections::BTreeMap::new();
                for y in 100 - reach..=100 + reach {
                    for x in 100 - reach..=100 + reach {
                        let d2 = (x as i64 - 100).pow(2) + (y as i64 - 100).pow(2);
                        let a = alpha(&op, info, x, y).unwrap();
                        assert_eq!(
                            *by_d2.entry(d2).or_insert(a),
                            a,
                            "r {radius} s {softness}: ({x}, {y}) differs from its distance class"
                        );
                    }
                }
                if softness > 0.0 && radius >= 12.0 {
                    assert!(
                        by_d2.values().any(|&a| a > 0 && a < 65536),
                        "r {radius} s {softness}: a soft stroke has a ramp"
                    );
                }
            }
        }
    }

    #[test]
    fn mask_covers_only_inside_the_footprint() {
        let info = info(120, 90);
        let op = stroke_op(1, vec![pt(-10, 30), pt(60, 45), pt(130, 80)], 17.5, 0.4);
        let fp = footprint(&op, info).unwrap().unwrap();
        for y in 0..90 {
            for x in 0..120 {
                let inside = x >= fp.x && x < fp.x + fp.width && y >= fp.y && y < fp.y + fp.height;
                if !inside {
                    assert_eq!(alpha(&op, info, x, y).unwrap(), 0, "({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn mask_refuses_what_validation_refuses() {
        let info = info(32, 32);
        let ops = [
            stroke_op(0, vec![], 3.0, 0.0),
            stroke_op(4, vec![pt(1, 1)], 3.0, 0.0),
            stroke_op(0, vec![pt(1, 1)], 0.0, 0.0),
            stroke_op(0, vec![pt(1, 1)], f32::NAN, 0.0),
            stroke_op(0, vec![pt(1, 1)], 3.0, 1.5),
        ];
        for op in &ops {
            assert!(alpha(op, info, 1, 1).is_err(), "{op:?}");
            let mut out = vec![7u16; 32 * 32];
            let src = vec![9u16; 32 * 32];
            assert!(
                apply(
                    op,
                    info,
                    rect(0, 0, 32, 32),
                    &mut out,
                    rect(0, 0, 32, 32),
                    &src,
                    1
                )
                .is_err()
            );
            assert!(out.iter().all(|&s| s == 7), "a refused op writes nothing");
        }
    }

    #[test]
    fn mask_apply_blends_only_where_alpha_is_nonzero() {
        let info = info(40, 30);
        let out_rect = rect(0, 0, 40, 30);
        let op = stroke_op(2, vec![pt(10, 10), pt(25, 15)], 6.0, 0.5);
        let spp = 3;
        let base: Vec<u16> = (0..40 * 30 * spp)
            .map(|i| (i * 37 % 65536) as u16)
            .collect();
        let src: Vec<u16> = (0..40 * 30 * spp)
            .map(|i| (i * 91 % 65536) as u16)
            .collect();
        let mut out = base.clone();
        apply(&op, info, out_rect, &mut out, out_rect, &src, spp).unwrap();
        let mut changed = 0;
        for y in 0..30u32 {
            for x in 0..40u32 {
                let a = alpha(&op, info, x, y).unwrap();
                for c in 0..spp {
                    let i = (y as usize * 40 + x as usize) * spp + c;
                    assert_eq!(out[i], blend_sample(base[i], src[i], a), "({x}, {y}, {c})");
                    changed += usize::from(out[i] != base[i]);
                }
            }
        }
        assert!(changed > 0, "the stroke changes something");

        // A sub-region: only its own pixels may change.
        let region = rect(8, 8, 12, 9);
        let sub_src: Vec<u16> = (0..12 * 9 * spp).map(|i| (i * 13 % 65536) as u16).collect();
        let mut out = base.clone();
        apply(&op, info, out_rect, &mut out, region, &sub_src, spp).unwrap();
        for y in 0..30u32 {
            for x in 0..40u32 {
                let in_region = (8..20).contains(&x) && (8..17).contains(&y);
                for c in 0..spp {
                    let i = (y as usize * 40 + x as usize) * spp + c;
                    if in_region {
                        let a = alpha(&op, info, x, y).unwrap();
                        let s = sub_src[((y - 8) as usize * 12 + (x - 8) as usize) * spp + c];
                        assert_eq!(out[i], blend_sample(base[i], s, a));
                    } else {
                        assert_eq!(out[i], base[i], "({x}, {y}) is outside the region");
                    }
                }
            }
        }

        let mut short = vec![0u16; 5];
        assert!(matches!(
            apply(&op, info, out_rect, &mut short, out_rect, &src, spp),
            Err(RetouchError::TileBufferSize { .. })
        ));
        assert!(matches!(
            apply(
                &op,
                info,
                out_rect,
                &mut out,
                rect(30, 20, 20, 20),
                &src,
                spp
            ),
            Err(RetouchError::TileOutsideMap { .. })
        ));
    }

    /// The unculled stroke: every pixel of `tile` is tested against every
    /// path segment through [`alpha`].
    fn apply_reference(op: &Op, info: ZMapInfo, tile: Rect, out: &mut [u16], src: &[u16]) {
        for y in tile.y..tile.y + tile.height {
            for x in tile.x..tile.x + tile.width {
                let a = alpha(op, info, x, y).unwrap();
                let i = ((y - tile.y) * tile.width + (x - tile.x)) as usize;
                out[i] = blend_sample(out[i], src[i], a);
            }
        }
    }

    fn assert_matches_reference(op: &Op, info: ZMapInfo, tile: Rect, label: &str) {
        let len = (tile.width * tile.height) as usize;
        let base: Vec<u16> = (0..len).map(|i| (i * 7 % 65536) as u16).collect();
        let src: Vec<u16> = (0..len).map(|i| (65535 - i * 11 % 65536) as u16).collect();
        let mut culled = base.clone();
        let mut naive = base.clone();
        apply(op, info, tile, &mut culled, tile, &src, 1).unwrap();
        apply_reference(op, info, tile, &mut naive, &src);
        assert_ne!(naive, base, "{label}: the stroke must blend something");
        assert!(
            culled == naive,
            "{label}: culled blend differs from the reference"
        );
    }

    #[test]
    fn mask_culled_blend_equals_the_unculled_stroke() {
        let info = info(400, 300);
        let map = rect(0, 0, 400, 300);
        // A self-crossing walk that leaves the map on every side.
        let long = walk(7, 60, (200, 150), 90);
        assert!(
            long.iter()
                .any(|p| p.x < 0 || p.x >= 400 || p.y < 0 || p.y >= 300)
        );
        for softness in [0.3, 0.0] {
            let op = stroke_op(3, long.clone(), 64.0, softness);
            assert_matches_reference(&op, info, map, &format!("long walk, softness {softness}"));
        }
        let corners = stroke_op(
            3,
            vec![
                pt(-30, -30),
                pt(40, 10),
                pt(399, 299),
                pt(420, 320),
                pt(0, 299),
            ],
            64.0,
            0.3,
        );
        assert_matches_reference(&corners, info, map, "clipped at the map corners");
        // The widened bounding box ends exactly on a cell boundary.
        for path in [
            vec![pt(100, 150), pt(164, 150)],
            vec![pt(200, 40), pt(200, 104)],
        ] {
            assert_matches_reference(&stroke_op(1, path, 64.0, 0.0), info, map, "cell boundary");
        }
        assert_matches_reference(
            &stroke_op(2, vec![pt(200, 150)], 64.0, 0.3),
            info,
            map,
            "dot",
        );
        assert_matches_reference(
            &stroke_op(2, vec![pt(10, 10), pt(11, 10)], 0.5, 0.0),
            info,
            map,
            "sub-pixel radius",
        );
        let tile = rect(123, 77, 150, 111);
        for softness in [0.3, 0.0] {
            let op = stroke_op(3, long.clone(), 64.0, softness);
            assert_matches_reference(&op, info, tile, &format!("sub-rect, softness {softness}"));
        }
    }

    #[test]
    fn mask_a_long_stroke_over_a_large_map_is_not_quadratic() {
        const SIDE: u32 = 1280;
        let map = rect(0, 0, SIDE, SIDE);
        // Serpentine sweeps of 50 points, 64 px apart, across the whole map.
        let serpentine = |sweeps: i32| -> Vec<Point> {
            (0..sweeps * 50)
                .map(|i| {
                    let along = (i % 50) * SIDE as i32 / 49;
                    let x = if (i / 50) % 2 == 0 {
                        along
                    } else {
                        SIDE as i32 - along
                    };
                    pt(x, (i / 50) * 64 + 10)
                })
                .collect()
        };
        // Segments the grid walk hands the blend, summed over the pixels it visits.
        let work = |path: &[Point]| -> u64 {
            let stroke = Stroke {
                path,
                radius: 16.0,
                softness: 0.3,
                mode: BrushMode::Normal,
                opacity: 1.0,
            };
            let mut total = 0;
            SegmentGrid::new(&stroke, map).walk(|_, _, segments| total += segments.len() as u64);
            total
        };
        let short = work(&serpentine(5));
        let long = work(&serpentine(20));
        assert!(short > 0, "the short stroke reaches no pixel");
        // 4x the points cost about 4x the work if linear, about 16x if quadratic.
        assert!(long < 8 * short, "250 points: {short}, 1000 points: {long}");
    }

    /// Frames of a `width`-wide plane, one sample a pixel, each a distinct
    /// ramp; a frame past `frames` is out of range.
    struct Ramps {
        width: u32,
        frames: usize,
    }

    impl crate::algorithm::TileReader for Ramps {
        fn read_tile_into(
            &self,
            frame: usize,
            r: Rect,
            out: &mut [u16],
        ) -> Result<(), crate::algorithm::AlgorithmError> {
            if frame >= self.frames {
                return Err(crate::algorithm::AlgorithmError::FrameOutOfRange {
                    frame,
                    frame_count: self.frames,
                });
            }
            for (i, o) in out.iter_mut().enumerate() {
                let x = r.x + i as u32 % r.width;
                let y = r.y + i as u32 / r.width;
                *o = ((y * self.width + x) * (frame as u32 + 3) % 65536) as u16;
            }
            Ok(())
        }
    }

    #[test]
    fn apply_strokes_hands_each_region_to_the_blend_it_is_given() {
        let info = info(40, 30);
        let out_rect = rect(5, 4, 30, 20);
        let frames = Ramps {
            width: 64,
            frames: 3,
        };
        let ops = [
            stroke_op(0, vec![pt(12, 10), pt(24, 14)], 6.0, 0.5),
            stroke_op(2, vec![pt(20, 8), pt(14, 18)], 5.0, 1.0),
        ];
        let placed: Vec<PlacedStroke> = ops
            .iter()
            .zip([0, 2])
            .map(|(op, frame)| PlacedStroke {
                op,
                frame,
                footprint: footprint(op, info).unwrap().unwrap(),
            })
            .collect();
        let mut out = vec![9u16; 30 * 20];
        let mut calls = Vec::new();
        apply_strokes(
            info,
            out_rect,
            &mut out,
            &placed,
            &frames,
            (0, 0),
            1,
            |op, _, _, out, region, src, spp| {
                calls.push((op.clone(), region, src.len(), spp));
                out.fill(1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.len(), 2, "one call per stroke");
        for ((op, region, len, spp), stroke) in calls.iter().zip(&placed) {
            assert_eq!(op, stroke.op);
            assert_eq!(*region, stroke.footprint.intersect(out_rect).unwrap());
            assert_eq!(*len, (region.width * region.height) as usize);
            assert_eq!(*spp, 1);
        }
        assert!(
            out.iter().all(|&s| s == 1),
            "the injected blend wrote `out`"
        );
    }

    #[test]
    fn mask_ramp_is_the_ramp_the_blend_uses() {
        let info = info(64, 64);
        for radius in [1.5f32, 12.0, 40.0] {
            for softness in [0.0f32, 0.3, 1.0] {
                let op = stroke_op(0, vec![pt(10, 10)], radius, softness);
                let ramp = Ramp::new(&validate(&op, info).unwrap());
                assert_eq!(
                    mask_ramp(&op, info).unwrap(),
                    (ramp.radius, ramp.core),
                    "radius {radius} softness {softness}"
                );
            }
        }
        let op = stroke_op(0, vec![pt(10, 10)], 12.0, 0.0);
        assert_eq!(mask_ramp(&op, info).unwrap(), (192, 192));
        let op = stroke_op(0, vec![pt(10, 10)], 12.0, 1.0);
        assert_eq!(mask_ramp(&op, info).unwrap(), (192, 0));
        let bad = stroke_op(9, vec![pt(10, 10)], 12.0, 0.5);
        assert!(matches!(
            mask_ramp(&bad, info),
            Err(RetouchError::SourceOutOfRange { .. })
        ));
    }

    #[test]
    fn strokes_blend_in_order_from_their_frames_at_the_crop_origin() {
        let info = info(40, 30);
        let out_rect = rect(5, 4, 30, 20);
        let frames = Ramps {
            width: 64,
            frames: 3,
        };
        let origin = (7, 3);
        let ops = [
            stroke_op(0, vec![pt(12, 10), pt(24, 14)], 6.0, 0.5),
            stroke_op(2, vec![pt(20, 8), pt(14, 18)], 5.0, 1.0),
        ];
        let placed: Vec<PlacedStroke> = ops
            .iter()
            .map(|op| PlacedStroke {
                op,
                frame: {
                    let Op::BrushStroke { source_id, .. } = op;
                    usize::from(*source_id)
                },
                footprint: footprint(op, info).unwrap().unwrap(),
            })
            .collect();
        let base: Vec<u16> = (0..30 * 20).map(|i| (i * 37 % 65536) as u16).collect();

        let mut expected = base.clone();
        for stroke in &placed {
            let region = stroke.footprint.intersect(out_rect).unwrap();
            let mut frame = vec![0u16; (region.width * region.height) as usize];
            let at_frame = rect(
                region.x + origin.0,
                region.y + origin.1,
                region.width,
                region.height,
            );
            crate::algorithm::TileReader::read_tile_into(
                &frames,
                stroke.frame,
                at_frame,
                &mut frame,
            )
            .unwrap();
            apply(stroke.op, info, out_rect, &mut expected, region, &frame, 1).unwrap();
        }
        assert_ne!(expected, base, "the strokes must blend something");

        let mut out = base.clone();
        apply_strokes(info, out_rect, &mut out, &placed, &frames, origin, 1, apply).unwrap();
        assert_eq!(out, expected);

        let mut untouched = base.clone();
        let far = [PlacedStroke {
            op: &ops[0],
            frame: 0,
            footprint: rect(0, 0, 3, 3),
        }];
        apply_strokes(
            info,
            out_rect,
            &mut untouched,
            &far,
            &frames,
            origin,
            1,
            apply,
        )
        .unwrap();
        assert_eq!(untouched, base, "a stroke off the rect writes nothing");
    }

    #[test]
    fn a_failed_frame_read_surfaces_as_a_source_error() {
        let info = info(40, 30);
        let out_rect = rect(0, 0, 40, 30);
        let frames = Ramps {
            width: 64,
            frames: 1,
        };
        let op = stroke_op(2, vec![pt(10, 10)], 4.0, 0.0);
        let placed = [PlacedStroke {
            op: &op,
            frame: 2,
            footprint: footprint(&op, info).unwrap().unwrap(),
        }];
        let mut out = vec![5u16; 40 * 30];
        let err = apply_strokes(info, out_rect, &mut out, &placed, &frames, (0, 0), 1, apply)
            .unwrap_err();
        assert!(matches!(err, RetouchError::Source(_)), "{err:?}");
        assert!(out.iter().all(|&s| s == 5), "a failed read writes nothing");
    }

    fn palette() -> Vec<[u8; 3]> {
        crate::zmap::overlay_table(4, None)
    }

    /// Stored indices that differ from sample to sample, `NO_SOURCE` among them.
    fn stored(len: usize) -> Vec<u16> {
        (0..len)
            .map(|i| {
                if i % 11 == 0 {
                    crate::zmap::NO_SOURCE
                } else {
                    (i % 4) as u16
                }
            })
            .collect()
    }

    /// `(buffer index, x, y)` of every lattice sample of `rect` at `step`.
    fn lattice(rect: Rect, step: u32) -> impl Iterator<Item = (usize, u32, u32)> {
        let columns = rect.width.div_ceil(step);
        let rows = rect.height.div_ceil(step);
        (0..rows).flat_map(move |j| {
            (0..columns).map(move |i| {
                (
                    (j * columns + i) as usize,
                    rect.x + i * step,
                    rect.y + j * step,
                )
            })
        })
    }

    fn samples(rect: Rect, step: u32) -> usize {
        (rect.width.div_ceil(step) * rect.height.div_ceil(step)) as usize
    }

    fn colors_over(
        ops: &[Op],
        map: ZMapInfo,
        rect: Rect,
        step: u32,
        indices: &[u16],
    ) -> Vec<[u8; 3]> {
        let mut out = vec![[0u8; 3]; indices.len()];
        overlay_colors(
            ops,
            map,
            rect,
            NonZeroU32::new(step).unwrap(),
            &palette(),
            indices,
            &mut out,
        );
        out
    }

    /// Two overlapping soft strokes from frames 1 and 3 and one the map's
    /// frame count refuses.
    fn overlapping_strokes() -> Vec<Op> {
        vec![
            stroke_op(1, vec![pt(3, 5), pt(30, 20)], 7.0, 0.7),
            stroke_op(3, vec![pt(8, 24), pt(35, 3)], 6.0, 0.5),
            stroke_op(9, vec![pt(10, 10)], 4.0, 0.5),
        ]
    }

    #[test]
    fn overlay_colors_match_brute_force_cutoff() {
        let map = info(40, 30);
        let table = palette();
        let last = table.len() - 1;
        let mut ops = overlapping_strokes();
        ops.push(stroke_op(2, vec![pt(-50, -50), pt(-40, -50)], 5.0, 0.5));
        let rect = rect(2, 1, 35, 27);
        for step in [1u32, 4] {
            let indices = stored(samples(rect, step));
            let out = colors_over(&ops, map, rect, step, &indices);
            let mut painted = 0;
            for (at, x, y) in lattice(rect, step) {
                let mut expected = table[usize::from(indices[at]).min(last)];
                for op in &ops {
                    let Ok(a) = alpha(op, map, x, y) else {
                        continue;
                    };
                    if a > 32768 {
                        let Op::BrushStroke { source_id, .. } = op;
                        expected = table[usize::from(*source_id).min(last)];
                        painted += 1;
                    }
                }
                assert_eq!(out[at], expected, "step {step} at ({x}, {y})");
            }
            assert!(painted > 20, "the strokes cover samples at step {step}");
        }
    }

    #[test]
    fn overlay_colors_edges() {
        let map = info(40, 30);
        let table = palette();
        let last = table.len() - 1;
        let rect = rect(0, 0, 40, 30);
        let indices = stored(40 * 30);
        let plain: Vec<[u8; 3]> = indices
            .iter()
            .map(|&z| table[usize::from(z).min(last)])
            .collect();
        assert!(indices.contains(&crate::zmap::NO_SOURCE));
        assert_eq!(colors_over(&[], map, rect, 1, &indices), plain, "no ops");

        let refused = [
            stroke_op(4, vec![pt(10, 10)], 5.0, 0.5),
            stroke_op(1, Vec::new(), 5.0, 0.5),
            stroke_op(1, vec![pt(10, 10)], -2.0, 0.5),
            stroke_op(1, vec![pt(10, 10)], 5.0, 1.5),
        ];
        assert_eq!(colors_over(&refused, map, rect, 1, &indices), plain);

        let mut ops = refused.to_vec();
        ops.push(stroke_op(2, vec![pt(-3, 15), pt(4, 15)], 6.0, 0.4));
        let out = colors_over(&ops, map, rect, 1, &indices);
        let mut painted = 0;
        for y in 0..30u32 {
            for x in 0..40u32 {
                let at = (y * 40 + x) as usize;
                if out[at] != plain[at] {
                    painted += 1;
                    assert_eq!(out[at], table[2], "({x}, {y})");
                    assert!(x < 12, "paint stays inside the stroke's reach: ({x}, {y})");
                }
            }
        }
        assert!(
            painted > 0,
            "the half-outside stroke paints its in-map part"
        );
        assert_eq!(out[15 * 40], table[2], "the in-map end is covered");
    }

    #[test]
    fn top_source_takes_the_latest_majority_cover() {
        let map = info(60, 30);
        // Radius 10 with full softness: alpha falls from 65536 at the point.
        let one = [stroke_op(1, vec![pt(10, 15)], 10.0, 1.0)];
        let flip = (10..40)
            .find(|&x| alpha(&one[0], map, x, 15).unwrap() <= 32768)
            .expect("the edge falls to half");
        for x in 10..flip {
            assert_eq!(top_source(&one, map, x, 15), Some(1), "inside at {x}");
        }
        for x in flip..40 {
            assert_eq!(top_source(&one, map, x, 15), None, "outside at {x}");
        }

        let two = [
            stroke_op(1, vec![pt(10, 15)], 10.0, 1.0),
            stroke_op(3, vec![pt(16, 15)], 10.0, 1.0),
        ];
        // The later stroke wins where it covers more than half, the earlier
        // one where only it does.
        assert_eq!(top_source(&two, map, 13, 15), Some(3), "both over half");
        assert_eq!(top_source(&two, map, 11, 15), Some(1), "only the earlier");
        assert_eq!(top_source(&two, map, 20, 15), Some(3), "only the later");
        assert_eq!(top_source(&two, map, 21, 15), None);

        let refused = [
            stroke_op(1, vec![pt(10, 15)], 10.0, 1.0),
            stroke_op(9, vec![pt(10, 15)], 10.0, 1.0),
        ];
        assert_eq!(
            top_source(&refused, map, 10, 15),
            Some(1),
            "refused op skipped"
        );
    }

    #[test]
    fn top_source_agrees_with_overlay_colors() {
        let map = info(40, 30);
        let table = palette();
        let last = table.len() - 1;
        let ops = overlapping_strokes();
        let rect = rect(2, 1, 35, 27);
        for step in [1u32, 4] {
            let indices = stored(samples(rect, step));
            let out = colors_over(&ops, map, rect, step, &indices);
            let mut shown = 0;
            for (at, x, y) in lattice(rect, step) {
                let expected = match top_source(&ops, map, x, y) {
                    Some(source) => {
                        shown += 1;
                        table[usize::from(source).min(last)]
                    }
                    None => table[usize::from(indices[at]).min(last)],
                };
                assert_eq!(out[at], expected, "step {step} at ({x}, {y})");
            }
            assert!(shown > 20, "the strokes cover samples at step {step}");
        }
    }
}
