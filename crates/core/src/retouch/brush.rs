// SPDX-License-Identifier: GPL-3.0-or-later

//! A stroke covers a disc of `radius` image pixels around its path and its
//! segments. The hard core, `radius * (1 - softness)`, always takes the new
//! index; the band out to `radius` takes it where an 8x8 Bayer threshold keyed
//! on the pixel's own coordinate says so. `softness` is a `0.0..=1.0` fraction
//! of `radius`. Indices are never blended: the midpoint of two indices names
//! an unrelated frame, so a pixel takes the new index or keeps its own.

use crate::project::{Op, Point};
use crate::tile::Rect;
use crate::zmap::ZMapInfo;

use super::RetouchError;

/// The standard recursive 8x8 Bayer index matrix, holding `0..=63` each
/// exactly once, keyed `[y & 7][x & 7]`.
const BAYER_8X8: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// A validated brush stroke: [`Op::BrushStroke`]'s fields, checked against
/// the map they will be applied to.
struct Stroke<'a> {
    source_id: u16,
    path: &'a [Point],
    radius: f32,
    softness: f32,
}

/// Refuses, in order: any op other than [`Op::BrushStroke`]; an empty path;
/// `source_id >= info.frame_count()`; a non-finite or non-positive radius;
/// a softness outside `0.0..=1.0`.
fn validate<'a>(op: &'a Op, info: ZMapInfo) -> Result<Stroke<'a>, RetouchError> {
    let Op::BrushStroke {
        source_id,
        path,
        radius,
        softness,
    } = op
    else {
        return Err(RetouchError::Unsupported);
    };
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
        source_id: *source_id,
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

fn stroke_footprint(stroke: &Stroke, info: ZMapInfo) -> Option<Rect> {
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
        // `+ 1`: an exclusive bound, so a boundary pixel at exactly `x + r`
        // (covered by `covers`'s `<=`) is not excluded when `x + r` is itself
        // an integer.
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

/// Squared distance from `(px, py)` to the nearest point of segment `a..b`
/// (a zero-length segment is its own point).
fn segment_dist2(px: f64, py: f64, a: Point, b: Point) -> f64 {
    let ax = f64::from(a.x);
    let ay = f64::from(a.y);
    let bx = f64::from(b.x);
    let by = f64::from(b.y);
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    };
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    let ddx = px - cx;
    let ddy = py - cy;
    ddx * ddx + ddy * ddy
}

/// Whether pixel `(x, y)` (absolute map coordinates) lies under `stroke`.
fn covers(stroke: &Stroke, x: u32, y: u32) -> bool {
    let px = f64::from(x);
    let py = f64::from(y);
    let mut d2 = f64::INFINITY;
    if stroke.path.len() < 2 {
        let p = stroke.path[0];
        d2 = segment_dist2(px, py, p, p);
    } else {
        for w in stroke.path.windows(2) {
            d2 = d2.min(segment_dist2(px, py, w[0], w[1]));
        }
    }
    let r = f64::from(stroke.radius);
    let softness = f64::from(stroke.softness);
    let core = r * (1.0 - softness);
    if d2 <= core * core {
        return true;
    }
    if d2 >= r * r {
        return false;
    }
    let t = (d2.sqrt() - core) / (r - core);
    let bayer = f64::from(BAYER_8X8[(y & 7) as usize][(x & 7) as usize]);
    (1.0 - t) * 64.0 > bayer + 0.5
}

/// Applies `op` over `tile` (an `info.tile_len(tile)`-long slice of `tile`'s
/// own indices, row-major), writing `source_id` where the stroke covers a
/// pixel. Returns how many pixels differed from their prior value. Every
/// refusal happens before the first write: `tile` must satisfy
/// `info.contains(tile)`, and `indices.len()` must equal
/// `info.tile_len(tile)`.
pub fn apply(
    op: &Op,
    info: ZMapInfo,
    tile: Rect,
    indices: &mut [u16],
) -> Result<u64, RetouchError> {
    let stroke = validate(op, info)?;
    if !info.contains(tile) {
        return Err(RetouchError::TileOutsideMap { tile });
    }
    let expected = info.tile_len(tile);
    if indices.len() != expected {
        return Err(RetouchError::TileBufferSize {
            expected,
            actual: indices.len(),
        });
    }
    let Some(footprint) = stroke_footprint(&stroke, info) else {
        return Ok(0);
    };
    let Some(region) = tile.intersect(footprint) else {
        return Ok(0);
    };
    let mut changed = 0u64;
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            if covers(&stroke, x, y) {
                let idx = ((y - tile.y) * tile.width + (x - tile.x)) as usize;
                if indices[idx] != stroke.source_id {
                    indices[idx] = stroke.source_id;
                    changed += 1;
                }
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zmap::{NO_SOURCE, validate_tile};

    const WIDTH: u32 = 37;
    const HEIGHT: u32 = 23;
    const FRAMES: u32 = 10;

    fn info() -> ZMapInfo {
        ZMapInfo::new(WIDTH, HEIGHT, FRAMES).unwrap()
    }

    fn whole_map() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        }
    }

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

    /// A base full of a distinguishable ramp, so "unchanged elsewhere" is
    /// checkable rather than assumed.
    fn ramp_base(info: ZMapInfo) -> Vec<u16> {
        (0..info.tile_len(whole_map()))
            .map(|i| (i as u32 % info.frame_count()) as u16)
            .collect()
    }

    fn valid(indices: &[u16], frame_count: u32) {
        validate_tile(indices, frame_count).expect("output holds only valid indices");
    }

    #[test]
    fn brush_stroke_writes_inside_and_only_inside() {
        let info = info();
        let base = ramp_base(info);
        // A bent path, and one lying off-canvas at negative coordinates whose
        // disc reaches across the map's left edge.
        let cases = [
            (7, vec![pt(5, 5), pt(20, 5), pt(20, 15)], 3),
            (2, vec![pt(-20, -20), pt(-5, -5), pt(-3, 30)], 8),
        ];
        for (source_id, path, radius) in cases {
            let op = stroke_op(source_id, path.clone(), radius as f32, 0.0);
            let mut indices = base.clone();
            let changed = apply(&op, info, whole_map(), &mut indices).unwrap();
            assert!(changed > 0, "{path:?} must paint something");
            valid(&indices, FRAMES);
            let fp = footprint(&op, info).unwrap().unwrap();
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let i = (y * WIDTH + x) as usize;
                    let in_fp =
                        x >= fp.x && x < fp.x + fp.width && y >= fp.y && y < fp.y + fp.height;
                    if !in_fp {
                        assert_eq!(indices[i], base[i], "({x},{y}) outside {fp:?} changed");
                    }
                    // softness 0: the hard core is the whole disc d² <= r².
                    if within(&path, x, y, radius) {
                        assert_eq!(indices[i], source_id, "{path:?}: ({x},{y}) not painted");
                    } else {
                        assert_eq!(indices[i], base[i], "{path:?}: ({x},{y}) beyond r written");
                    }
                }
            }
        }
    }

    /// Whether lattice point `(x, y)` lies within `r` of `path`, in exact
    /// integer arithmetic: endpoint distance outside a segment's span,
    /// `cross² <= r²·len²` inside it.
    fn within(path: &[Point], x: u32, y: u32, r: i64) -> bool {
        let (px, py) = (i128::from(x), i128::from(y));
        let r2 = i128::from(r * r);
        let near = |a: Point, b: Point| {
            let (ax, ay) = (i128::from(a.x), i128::from(a.y));
            let (dx, dy) = (i128::from(b.x) - ax, i128::from(b.y) - ay);
            let (vx, vy) = (px - ax, py - ay);
            let len2 = dx * dx + dy * dy;
            let dot = vx * dx + vy * dy;
            if dot <= 0 || len2 == 0 {
                vx * vx + vy * vy <= r2
            } else if dot >= len2 {
                let (wx, wy) = (px - i128::from(b.x), py - i128::from(b.y));
                wx * wx + wy * wy <= r2
            } else {
                let cross = vx * dy - vy * dx;
                cross * cross <= r2 * len2
            }
        };
        near(path[0], path[0]) || path.windows(2).any(|w| near(w[0], w[1]))
    }

    #[test]
    fn apply_is_deterministic_and_replayable() {
        let info = info();
        let base = ramp_base(info);
        let op = stroke_op(3, vec![pt(2, 2), pt(30, 18)], 5.0, 0.4);

        let mut once = base.clone();
        apply(&op, info, whole_map(), &mut once).unwrap();
        let mut twice = base.clone();
        apply(&op, info, whole_map(), &mut twice).unwrap();
        apply(&op, info, whole_map(), &mut twice).unwrap();
        valid(&once, FRAMES);
        valid(&twice, FRAMES);
        assert_eq!(once, twice, "applying the same op twice must be idempotent");

        let ops = vec![
            stroke_op(1, vec![pt(1, 1), pt(10, 10)], 3.0, 0.5),
            stroke_op(4, vec![pt(20, 5)], 6.0, 1.0),
            stroke_op(9, vec![pt(0, 0), pt(36, 22)], 2.0, 0.0),
        ];
        let mut sequential = base.clone();
        for op in &ops {
            apply(op, info, whole_map(), &mut sequential).unwrap();
        }
        let mut replayed = base.clone();
        for op in &ops {
            apply(op, info, whole_map(), &mut replayed).unwrap();
        }
        valid(&sequential, FRAMES);
        valid(&replayed, FRAMES);
        assert_eq!(sequential, replayed, "replaying the same list must match");

        // Tile-by-tile over a grid of non-8-aligned tiles equals one
        // whole-map apply.
        let mut tiled = base.clone();
        let tile_w = 6u32;
        let tile_h = 5u32;
        let mut y = 0;
        while y < HEIGHT {
            let h = tile_h.min(HEIGHT - y);
            let mut x = 0;
            while x < WIDTH {
                let w = tile_w.min(WIDTH - x);
                let rect = Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                };
                let mut buf: Vec<u16> = (0..h)
                    .flat_map(|row| {
                        let start = ((y + row) * WIDTH + x) as usize;
                        tiled[start..start + w as usize].to_vec()
                    })
                    .collect();
                for op in &ops {
                    apply(op, info, rect, &mut buf).unwrap();
                }
                valid(&buf, FRAMES);
                for row in 0..h {
                    let start = ((y + row) * WIDTH + x) as usize;
                    let buf_start = (row * w) as usize;
                    tiled[start..start + w as usize]
                        .copy_from_slice(&buf[buf_start..buf_start + w as usize]);
                }
                x += w;
            }
            y += h;
        }
        valid(&tiled, FRAMES);
        assert_eq!(
            tiled, sequential,
            "tile-by-tile apply over a non-8-aligned grid must equal a whole-map apply"
        );
    }

    #[test]
    fn out_of_range_source_is_refused() {
        let info = info();
        let base = ramp_base(info);
        for bad_id in [FRAMES as u16, NO_SOURCE] {
            let op = stroke_op(bad_id, vec![pt(5, 5)], 3.0, 0.0);
            match footprint(&op, info) {
                Err(RetouchError::SourceOutOfRange {
                    source_id,
                    frame_count,
                }) => {
                    assert_eq!(source_id, bad_id);
                    assert_eq!(frame_count, FRAMES);
                }
                other => panic!("expected SourceOutOfRange, got {other:?}"),
            }
            let mut indices = base.clone();
            match apply(&op, info, whole_map(), &mut indices) {
                Err(RetouchError::SourceOutOfRange { .. }) => {}
                other => panic!("expected SourceOutOfRange, got {other:?}"),
            }
            valid(&indices, FRAMES);
            assert_eq!(indices, base, "a refused op must not touch the buffer");
        }
    }

    #[test]
    fn stroke_outside_extent_is_a_clipped_no_op() {
        let info = info();
        let base = ramp_base(info);

        // Runs off the edge: clips rather than failing.
        let edge_op = stroke_op(6, vec![pt(-2, 5), pt(2, 5)], 3.0, 0.0);
        let mut indices = base.clone();
        let changed = apply(&edge_op, info, whole_map(), &mut indices).unwrap();
        assert!(changed > 0);
        valid(&indices, FRAMES);

        // Entirely outside the extent.
        let far_op = stroke_op(6, vec![pt(1_000, 1_000)], 3.0, 0.0);
        assert_eq!(footprint(&far_op, info).unwrap(), None);
        let mut far_indices = base.clone();
        let changed = apply(&far_op, info, whole_map(), &mut far_indices).unwrap();
        assert_eq!(changed, 0);
        valid(&far_indices, FRAMES);
        assert_eq!(far_indices, base);
    }

    #[test]
    fn softness_never_invents_an_index() {
        let info = info();
        let mut base = ramp_base(info);
        // Mix in NO_SOURCE.
        for (i, v) in base.iter_mut().enumerate() {
            if i % 5 == 0 {
                *v = NO_SOURCE;
            }
        }
        let op = stroke_op(8, vec![pt(10, 10), pt(25, 12)], 6.0, 0.6);
        let mut indices = base.clone();
        apply(&op, info, whole_map(), &mut indices).unwrap();

        valid(&indices, FRAMES);

        let mut saw_overwritten_no_source = false;
        let mut saw_untouched_no_source = false;
        for i in 0..indices.len() {
            let out = indices[i];
            assert!(
                out == 8 || out == base[i],
                "pixel {i} holds {out}, neither the new index nor its base value"
            );
            if base[i] == NO_SOURCE {
                if out == 8 {
                    saw_overwritten_no_source = true;
                } else {
                    saw_untouched_no_source = true;
                }
            }
        }
        assert!(
            saw_overwritten_no_source,
            "hard core must overwrite NO_SOURCE"
        );
        assert!(
            saw_untouched_no_source,
            "the dither must leave some NO_SOURCE pixels alone"
        );
    }

    #[test]
    fn dither_is_a_function_of_coordinate_only() {
        let info = info();
        let op = stroke_op(5, vec![pt(15, 10)], 8.0, 0.5);

        let mut whole = vec![0u16; info.tile_len(whole_map())];
        apply(&op, info, whole_map(), &mut whole).unwrap();

        // Same op, applied through a grid of tiles at different (non-8-aligned)
        // origins, must cover exactly the same set.
        let mut tiled = vec![0u16; info.tile_len(whole_map())];
        for (rx, ry, rw, rh) in [(0u32, 0u32, 9u32, 7u32), (9, 0, 28, 7), (0, 7, 37, 16)] {
            let rect = Rect {
                x: rx,
                y: ry,
                width: rw,
                height: rh,
            };
            let mut buf = vec![0u16; info.tile_len(rect)];
            apply(&op, info, rect, &mut buf).unwrap();
            valid(&buf, FRAMES);
            for row in 0..rh {
                let start = ((ry + row) * WIDTH + rx) as usize;
                let buf_start = (row * rw) as usize;
                tiled[start..start + rw as usize]
                    .copy_from_slice(&buf[buf_start..buf_start + rw as usize]);
            }
        }
        valid(&whole, FRAMES);
        valid(&tiled, FRAMES);
        assert_eq!(whole, tiled);

        // Translating the path by (8, 16) translates the covered set by (8, 16).
        let translated_info = ZMapInfo::new(WIDTH + 8, HEIGHT + 16, FRAMES).unwrap();
        let translated_op = stroke_op(5, vec![pt(15 + 8, 10 + 16)], 8.0, 0.5);
        let translated_rect = Rect {
            x: 0,
            y: 0,
            width: WIDTH + 8,
            height: HEIGHT + 16,
        };
        let mut translated = vec![0u16; translated_info.tile_len(translated_rect)];
        apply(
            &translated_op,
            translated_info,
            translated_rect,
            &mut translated,
        )
        .unwrap();
        valid(&translated, FRAMES);

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let original = whole[(y * WIDTH + x) as usize];
                let shifted = translated[((y + 16) * (WIDTH + 8) + (x + 8)) as usize];
                assert_eq!(
                    original, shifted,
                    "({x},{y}) must match its (+8,+16) translate"
                );
            }
        }
    }

    #[test]
    fn softness_endpoints_and_monotonic_coverage() {
        let big = ZMapInfo::new(400, 400, FRAMES).unwrap();
        let big_rect = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 400,
        };
        let radius = 64.0f32;

        // softness = 0: exactly d2 <= r2.
        let hard = stroke_op(1, vec![pt(200, 200)], radius, 0.0);
        let mut hard_out = vec![0u16; big.tile_len(big_rect)];
        apply(&hard, big, big_rect, &mut hard_out).unwrap();
        valid(&hard_out, FRAMES);
        for y in 0..400u32 {
            for x in 0..400u32 {
                let dx = x as f64 - 200.0;
                let dy = y as f64 - 200.0;
                let d2 = dx * dx + dy * dy;
                let expected = if d2 <= f64::from(radius) * f64::from(radius) {
                    1
                } else {
                    0
                };
                assert_eq!(
                    hard_out[(y * 400 + x) as usize],
                    expected,
                    "({x},{y}) d2={d2}"
                );
            }
        }

        // softness = 1: the path point itself is still covered.
        let soft = stroke_op(2, vec![pt(200, 200)], radius, 1.0);
        let mut soft_out = vec![0u16; big.tile_len(big_rect)];
        apply(&soft, big, big_rect, &mut soft_out).unwrap();
        valid(&soft_out, FRAMES);
        assert_eq!(soft_out[(200 * 400 + 200) as usize], 2);

        // Monotonic coverage: the covered fraction of each quarter of the
        // band, counted over its own annulus, strictly falls from the inner
        // quarter to the outer one.
        let quarters = [(0.0f64, 16.0f64), (16.0, 32.0), (32.0, 48.0), (48.0, 64.0)];
        let mut fractions = Vec::new();
        for (lo, hi) in quarters {
            let mut covered = 0u32;
            let mut total = 0u32;
            for y in 130..270u32 {
                for x in 130..270u32 {
                    let dx = x as f64 - 200.0;
                    let dy = y as f64 - 200.0;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d >= lo && d < hi {
                        total += 1;
                        if soft_out[(y * 400 + x) as usize] == 2 {
                            covered += 1;
                        }
                    }
                }
            }
            assert!(total > 0);
            fractions.push(f64::from(covered) / f64::from(total));
        }
        for w in fractions.windows(2) {
            assert!(
                w[0] > w[1],
                "coverage fraction must strictly fall outward: {fractions:?}"
            );
        }
    }

    #[test]
    fn radius_and_softness_units() {
        let big = ZMapInfo::new(60, 60, FRAMES).unwrap();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 60,
        };
        for radius in [5.0f32, 12.0] {
            let op = stroke_op(3, vec![pt(30, 30)], radius, 0.0);
            let mut out = vec![0u16; big.tile_len(rect)];
            apply(&op, big, rect, &mut out).unwrap();
            valid(&out, FRAMES);
            for y in 0..60u32 {
                for x in 0..60u32 {
                    let dx = x as i64 - 30;
                    let dy = y as i64 - 30;
                    let d2 = (dx * dx + dy * dy) as f64;
                    let expected = if d2 <= f64::from(radius) * f64::from(radius) {
                        3
                    } else {
                        0
                    };
                    assert_eq!(
                        out[(y * 60 + x) as usize],
                        expected,
                        "radius {radius} ({x},{y})"
                    );
                }
            }
        }

        let info = info();
        for bad in [-0.1f32, 1.1] {
            let op = stroke_op(1, vec![pt(5, 5)], 3.0, bad);
            match footprint(&op, info) {
                Err(RetouchError::InvalidSoftness { softness }) => assert_eq!(softness, bad),
                other => panic!("expected InvalidSoftness, got {other:?}"),
            }
        }
    }

    #[test]
    fn degenerate_parameters_are_refused_or_documented_noop() {
        let info = info();

        for bad in [f32::NAN, f32::INFINITY, -f32::INFINITY] {
            let op = stroke_op(1, vec![pt(5, 5)], bad, 0.0);
            match footprint(&op, info) {
                Err(RetouchError::InvalidRadius { .. }) => {}
                other => panic!("radius {bad}: expected InvalidRadius, got {other:?}"),
            }
        }
        for bad in [0.0f32, -1.0] {
            let op = stroke_op(1, vec![pt(5, 5)], bad, 0.0);
            match footprint(&op, info) {
                Err(RetouchError::InvalidRadius { .. }) => {}
                other => panic!("radius {bad}: expected InvalidRadius, got {other:?}"),
            }
        }
        for bad in [f32::NAN, -0.1, 1.1] {
            let op = stroke_op(1, vec![pt(5, 5)], 3.0, bad);
            match footprint(&op, info) {
                Err(RetouchError::InvalidSoftness { .. }) => {}
                other => panic!("softness {bad}: expected InvalidSoftness, got {other:?}"),
            }
        }

        let empty = stroke_op(1, vec![], 3.0, 0.0);
        match footprint(&empty, info) {
            Err(RetouchError::EmptyPath) => {}
            other => panic!("expected EmptyPath, got {other:?}"),
        }

        // The all-default op.
        let default_op: Op = serde_json::from_str(r#"{"brush-stroke":{}}"#).unwrap();
        match footprint(&default_op, info) {
            Err(RetouchError::EmptyPath) => {}
            other => panic!("expected EmptyPath for the all-default op, got {other:?}"),
        }

        // A single-point path, and repeated identical points, both act as a disc.
        let single = stroke_op(2, vec![pt(10, 10)], 3.0, 0.0);
        let repeated = stroke_op(2, vec![pt(10, 10), pt(10, 10), pt(10, 10)], 3.0, 0.0);
        let mut single_out = vec![0u16; info.tile_len(whole_map())];
        let mut repeated_out = vec![0u16; info.tile_len(whole_map())];
        apply(&single, info, whole_map(), &mut single_out).unwrap();
        apply(&repeated, info, whole_map(), &mut repeated_out).unwrap();
        valid(&single_out, FRAMES);
        valid(&repeated_out, FRAMES);
        assert_eq!(single_out, repeated_out);
        assert!(single_out.contains(&2));

        // A huge finite radius: footprint clamps to the extent, no overflow.
        let huge = stroke_op(1, vec![pt(10, 10)], f32::MAX, 0.0);
        let fp = footprint(&huge, info).unwrap().unwrap();
        assert_eq!(fp, whole_map());

        // A tile outside the map, and a buffer of the wrong length.
        let op = stroke_op(1, vec![pt(5, 5)], 3.0, 0.0);
        let outside = Rect {
            x: WIDTH,
            y: 0,
            width: 1,
            height: 1,
        };
        let mut buf = vec![0u16; 1];
        match apply(&op, info, outside, &mut buf) {
            Err(RetouchError::TileOutsideMap { tile }) => assert_eq!(tile, outside),
            other => panic!("expected TileOutsideMap, got {other:?}"),
        }
        let mut wrong_len = vec![0u16; info.tile_len(whole_map()) + 1];
        match apply(&op, info, whole_map(), &mut wrong_len) {
            Err(RetouchError::TileBufferSize { expected, actual }) => {
                assert_eq!(expected, info.tile_len(whole_map()));
                assert_eq!(actual, wrong_len.len());
            }
            other => panic!("expected TileBufferSize, got {other:?}"),
        }
        valid(&buf, FRAMES);
        valid(&wrong_len, FRAMES);
    }

    #[test]
    fn dirty_region_bounds_the_change() {
        let info = info();
        let base = ramp_base(info);
        let op = stroke_op(4, vec![pt(3, 3), pt(18, 9), pt(2, 20)], 4.0, 0.7);
        let mut indices = base.clone();
        let changed = apply(&op, info, whole_map(), &mut indices).unwrap();
        let fp = footprint(&op, info).unwrap().unwrap();
        valid(&indices, FRAMES);

        let mut differing = 0u64;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let i = (y * WIDTH + x) as usize;
                if indices[i] != base[i] {
                    differing += 1;
                    assert!(
                        x >= fp.x && x < fp.x + fp.width && y >= fp.y && y < fp.y + fp.height,
                        "changed pixel ({x},{y}) lies outside the reported footprint {fp:?}"
                    );
                }
            }
        }
        assert_eq!(changed, differing);
    }

    #[test]
    fn other_op_variants_are_unsupported() {
        let info = info();
        let base = ramp_base(info);
        let ops = vec![
            Op::FloodFill {
                seed: pt(5, 5),
                source_id: 1,
                tolerance: 0.1,
            },
            Op::ResetRegion {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
            },
            Op::Refuse {
                algorithm: "laplacian".to_string(),
                params: crate::params::ParamValues::new(),
            },
        ];
        for op in ops {
            match footprint(&op, info) {
                Err(RetouchError::Unsupported) => {}
                other => panic!("expected Unsupported, got {other:?}"),
            }
            let mut indices = base.clone();
            match apply(&op, info, whole_map(), &mut indices) {
                Err(RetouchError::Unsupported) => {}
                other => panic!("expected Unsupported, got {other:?}"),
            }
            valid(&indices, FRAMES);
            assert_eq!(indices, base, "a refused op must not touch the buffer");
        }
    }
}
