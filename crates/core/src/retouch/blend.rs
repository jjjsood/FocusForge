// SPDX-License-Identifier: GPL-3.0-or-later

//! The soft brush stroke: `out = mix(out, source, alpha)` over a disc of
//! `radius` pixels around the stroke's path, in integer fixed point.
//!
//! The mask is full inside the core, `radius * (1 - softness)`, and falls
//! linearly to zero at `radius`, measured in sixteenths of a pixel from the
//! exact point-to-segment distance, so it is a pure function of the stroke and
//! the pixel. Alpha is Q16 (`0..=65536`); the blend rounds to nearest.

use std::num::NonZeroU32;

use crate::algorithm::TileReader;
use crate::project::{Op, Point};
use crate::tile::Rect;
use crate::zmap::ZMapInfo;

use super::RetouchError;

/// One pixel in the distance unit of the mask.
const SIXTEENTHS: f64 = 16.0;
/// Alpha of a fully covered sample.
const OPAQUE: u32 = 1 << 16;
/// Caps the radius so the ramp's products stay far inside `u64`.
const MAX_RADIUS_16: u64 = 1 << 40;

/// A validated brush stroke: [`Op::BrushStroke`]'s path, radius and
/// softness, checked against the map they will be applied to.
pub(super) struct Stroke<'a> {
    pub(super) path: &'a [Point],
    pub(super) radius: f32,
    pub(super) softness: f32,
}

/// Refuses, in order: an empty path; `source_id >= info.frame_count()`; a
/// non-finite or non-positive radius; a softness outside `0.0..=1.0`.
pub(super) fn validate<'a>(op: &'a Op, info: ZMapInfo) -> Result<Stroke<'a>, RetouchError> {
    let Op::BrushStroke {
        source_id,
        path,
        radius,
        softness,
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
    Ok(Stroke {
        path: path.as_slice(),
        radius: *radius,
        softness: *softness,
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
/// Cost follows the area near the path, not the path's bounding box.
pub fn apply(
    op: &Op,
    info: ZMapInfo,
    out_rect: Rect,
    out: &mut [u16],
    region: Rect,
    src: &[u16],
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
    SegmentGrid::new(&stroke, active).walk(|x, y, segments| {
        let a = ramp.alpha(distance_16(
            &stroke,
            x,
            y,
            segments.iter().map(|&i| i as usize),
        ));
        if a == 0 {
            return;
        }
        let out_at =
            ((y - out_rect.y) as usize * out_rect.width as usize + (x - out_rect.x) as usize) * spp;
        let src_at =
            ((y - region.y) as usize * region.width as usize + (x - region.x) as usize) * spp;
        for (o, &s) in out[out_at..out_at + spp]
            .iter_mut()
            .zip(&src[src_at..src_at + spp])
        {
            *o = blend_sample(*o, s, a);
        }
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
/// over the part of `out_rect` its footprint meets, shifted by `origin` (the
/// crop offset between map and frame coordinates); a stroke clear of
/// `out_rect` reads nothing. A failed frame read is [`RetouchError::Source`]
/// and leaves `out` as the strokes before it made it. Each stroke is blended
/// by `blend`, called as [`apply`] is: [`apply`] itself, or a blend with the
/// same result.
#[allow(clippy::too_many_arguments)]
pub fn apply_strokes(
    info: ZMapInfo,
    out_rect: Rect,
    out: &mut [u16],
    strokes: &[PlacedStroke<'_>],
    frames: &dyn TileReader,
    origin: (u32, u32),
    spp: usize,
    mut blend: impl FnMut(
        &Op,
        ZMapInfo,
        Rect,
        &mut [u16],
        Rect,
        &[u16],
        usize,
    ) -> Result<(), RetouchError>,
) -> Result<(), RetouchError> {
    let mut frame: Vec<u16> = Vec::new();
    for stroke in strokes {
        let Some(region) = stroke.footprint.intersect(out_rect) else {
            continue;
        };
        frame.resize(region.width as usize * region.height as usize * spp, 0);
        let at_frame = Rect {
            x: region.x + origin.0,
            y: region.y + origin.1,
            ..region
        };
        frames.read_tile_into(stroke.frame, at_frame, &mut frame)?;
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
        }
    }

    fn info(width: u32, height: u32) -> ZMapInfo {
        ZMapInfo::new(width, height, 4).unwrap()
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
        let info = info(4000, 4000);
        let map = rect(0, 0, 4000, 4000);
        // 20 sweeps of 50 points: a 1000-point path whose bounding box is the
        // whole map.
        let jitter = walk(11, 1000, (0, 0), 6);
        let path: Vec<Point> = (0..1000)
            .map(|i| {
                let sweep = i / 50;
                let along = (i % 50) * 4000 / 49;
                let x = if sweep % 2 == 0 { along } else { 4000 - along };
                pt(
                    x + jitter[i as usize].x % 6,
                    sweep * 200 + 10 + jitter[i as usize].y % 6,
                )
            })
            .collect();
        let op = stroke_op(2, path, 16.0, 0.3);
        let mut out = vec![0u16; 4000 * 4000];
        let src = vec![60_000u16; 4000 * 4000];
        let start = std::time::Instant::now();
        apply(&op, info, map, &mut out, map, &src, 1).unwrap();
        let elapsed = start.elapsed();
        assert!(out.iter().filter(|&&s| s != 0).count() > 1_000_000);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a 1000-point stroke took {elapsed:?}"
        );
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
