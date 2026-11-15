// SPDX-License-Identifier: GPL-3.0-or-later

//! A freehand stroke in progress: the raw image-space trace a drag delivers,
//! resampled into the storage shape `core::retouch` expects only once, on
//! release -- plus the brush cursor's `Path` geometry. Plain Rust, no Slint
//! types.

use focusforge_core::project::{BrushMode, Point, StrokeSource};
use focusforge_core::tile::Rect;

pub use focusforge_core::retouch::preview::BrushPreviewSpec;

/// The preview brush of the one-point stroke a press at the source point
/// `(sx, sy)` would record: its centre is the integer point the stroke stores.
pub fn preview_spec(
    (sx, sy): (f64, f64),
    radius: f32,
    softness: f32,
    mode: BrushMode,
    opacity: f32,
) -> BrushPreviewSpec {
    BrushPreviewSpec {
        centre: Point {
            x: (sx - 0.5).round() as i32,
            y: (sy - 0.5).round() as i32,
        },
        radius,
        softness,
        mode,
        opacity,
    }
}

/// One freehand stroke while its drag is still live: the raw `f32` trace in
/// image coordinates, the index of the last point a tick has already
/// consumed, and the footprint of each tick applied so far (for the commit
/// pass's dilation). It also holds the preview the press showed until the
/// stroke's first painted region is on the Composite.
#[derive(Debug, Clone)]
pub struct LiveStroke {
    source: StrokeSource,
    radius: f32,
    softness: f32,
    mode: BrushMode,
    opacity: f32,
    points: Vec<(f32, f32)>,
    flushed: usize,
    /// The preview at the press point; `None` when the press was off the
    /// image.
    preview: Option<BrushPreviewSpec>,
    /// The resident tiles' generation once the first of this stroke's own
    /// ticks was installed.
    painted_at: Option<u64>,
    /// The newest generation the Composite has published since the press.
    published: u64,
    /// Every tick's own re-rendered region, oldest first -- empty until a
    /// caller records one.
    pub tick_rects: Vec<Rect>,
}

impl LiveStroke {
    /// Opens a stroke at `(x, y)` (image coordinates), freezing the brush's
    /// source, radius, softness, blend mode and opacity for the stroke's
    /// whole lifetime -- a wheel turned or a toolbar control changed
    /// mid-drag affects only the next stroke.
    pub fn new(
        source: StrokeSource,
        radius: f32,
        softness: f32,
        mode: BrushMode,
        opacity: f32,
        x: f32,
        y: f32,
    ) -> Self {
        Self {
            source,
            radius,
            softness,
            mode,
            opacity,
            points: vec![(x, y)],
            flushed: 0,
            preview: None,
            painted_at: None,
            published: 0,
            tick_rects: Vec::new(),
        }
    }

    /// Sets the preview the press showed, `None` for a press off the image.
    pub fn with_preview(mut self, preview: Option<BrushPreviewSpec>) -> Self {
        self.preview = preview;
        self
    }

    /// The press's preview while the stroke has not yet painted the
    /// Composite, `None` once it has.
    pub fn held_preview(&self) -> Option<BrushPreviewSpec> {
        let shown = self.painted_at.is_some_and(|at| self.published >= at);
        self.preview.filter(|_| !shown)
    }

    /// Records the first install of one of this stroke's own ticks, which left
    /// the resident tiles at `generation`.
    pub(crate) fn painted(&mut self, generation: u64) {
        self.painted_at.get_or_insert(generation);
    }

    /// Records that the Composite now shows the resident tiles as of
    /// `generation`.
    pub(crate) fn composite_published(&mut self, generation: u64) {
        self.published = self.published.max(generation);
    }

    pub fn source(&self) -> &StrokeSource {
        &self.source
    }

    pub fn radius(&self) -> f32 {
        self.radius
    }

    pub fn softness(&self) -> f32 {
        self.softness
    }

    pub fn mode(&self) -> BrushMode {
        self.mode
    }

    /// The stroke's opacity in `(0, 1]`.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// Integrates one motion delta (image-space) into the raw trace.
    pub fn push(&mut self, dx: f32, dy: f32) {
        let (x, y) = *self
            .points
            .last()
            .expect("constructed with at least one point");
        self.points.push((x + dx, y + dy));
    }

    /// The rounded, de-duplicated points from the last-flushed point to the
    /// newest -- repeated, so consecutive segments join at a shared point --
    /// or `None` when no motion arrived since the last call.
    pub fn take_segment(&mut self) -> Option<Vec<Point>> {
        let last = self.points.len() - 1;
        if last == self.flushed {
            return None;
        }
        let segment = round_and_dedup(&self.points[self.flushed..=last]);
        self.flushed = last;
        Some(segment)
    }

    /// The rounded, de-duplicated trace from the start to the last point a
    /// tick consumed: the segments [`Self::take_segment`] returned, joined.
    pub fn path_so_far(&self) -> Vec<Point> {
        round_and_dedup(&self.points[..=self.flushed])
    }

    /// The stroke's stored shape: the raw trace resampled at `radius / 4`
    /// arc-length spacing, RDP-simplified at 0.5 px, rounded to whole pixels
    /// with consecutive duplicates dropped. Independent of how many events
    /// the drag delivered.
    pub fn finish(&self) -> Vec<Point> {
        let spacing = (self.radius / 4.0).max(f32::EPSILON);
        let resampled = resample(&self.points, spacing);
        let simplified = rdp(&resampled, 0.5);
        round_and_dedup(&simplified)
    }
}

/// The dashed ring's dash and gap length, in the same on-screen px the
/// `commands` strings below are built in: fixed, not derived from the brush
/// size.
const CURSOR_DASH: f32 = 11.0;
const CURSOR_GAP: f32 = 11.0;
/// The mode chip's side and its clearance from the ring's 45° point, in the
/// same on-screen px; `FFBrushCursor` draws the chip at `tag_origin`.
const CURSOR_TAG_SIDE: f32 = 16.0;
const CURSOR_TAG_GAP: f32 = 2.0;

/// The brush cursor's own `Path` geometry: a square `extent` long that the
/// `FFBrushCursor` viewbox matches 1:1, the `commands` strings for its ring
/// and core, and the top-left of the mode chip that sits outside the ring at
/// its lower-right 45° point.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorPaths {
    pub extent: f32,
    /// A dashed ring at `radius`, one sub-path per dash -- empty for a hard
    /// brush (`softness == 0`), which draws its edge as `core` instead.
    pub ring: String,
    /// A solid circle: the full `radius` for a hard brush, or
    /// `radius * (1 - softness)` behind the dashed ring otherwise.
    pub core: String,
    /// The chip's top-left, on both axes of the square.
    pub tag_origin: f32,
}

/// Builds [`CursorPaths`] for an on-screen `radius` (image-space radius
/// already multiplied by the viewport's scale) and a `0.0..=1.0` softness
/// fraction.
pub fn cursor_paths(radius: f32, softness: f32) -> CursorPaths {
    let diagonal = std::f32::consts::FRAC_1_SQRT_2 * radius;
    let half = (radius + 1.0).max(diagonal + CURSOR_TAG_GAP + CURSOR_TAG_SIDE);
    let extent = 2.0 * half;
    let center = half;
    let tag_origin = center + diagonal + CURSOR_TAG_GAP;
    if softness <= 0.0 {
        CursorPaths {
            extent,
            ring: String::new(),
            core: full_circle(center, radius),
            tag_origin,
        }
    } else {
        let core_radius = radius * (1.0 - softness);
        let core = if core_radius > 0.01 {
            full_circle(center, core_radius)
        } else {
            String::new()
        };
        CursorPaths {
            extent,
            ring: dashed_ring(center, radius),
            core,
            tag_origin,
        }
    }
}

/// A full circle of radius `r` centred at `(center, center)`, as two
/// semicircle arcs -- SVG's `A` command cannot sweep a full 360°.
fn full_circle(center: f32, r: f32) -> String {
    format!(
        "M {:.2} {:.2} A {r:.2} {r:.2} 0 1 1 {:.2} {:.2} A {r:.2} {r:.2} 0 1 1 {:.2} {:.2}",
        center - r,
        center,
        center + r,
        center,
        center - r,
        center
    )
}

/// A dashed ring of radius `r` centred at `(center, center)`: as many
/// [`CURSOR_DASH`]/[`CURSOR_GAP`]-period dashes as fit evenly around the
/// circumference, starting at the top and going clockwise, one `M`/`A`
/// sub-path per dash.
fn dashed_ring(center: f32, r: f32) -> String {
    let period = CURSOR_DASH + CURSOR_GAP;
    let n = ((std::f32::consts::TAU * r) / period).round().max(1.0) as usize;
    let angular_step = std::f32::consts::TAU / n as f32;
    let dash_angle = angular_step * (CURSOR_DASH / period);
    let start = -std::f32::consts::FRAC_PI_2;
    (0..n)
        .map(|i| {
            let theta0 = start + i as f32 * angular_step;
            let theta1 = theta0 + dash_angle;
            let (x0, y0) = (center + r * theta0.cos(), center + r * theta0.sin());
            let (x1, y1) = (center + r * theta1.cos(), center + r * theta1.sin());
            format!("M {x0:.2} {y0:.2} A {r:.2} {r:.2} 0 0 1 {x1:.2} {y1:.2}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn round_and_dedup(points: &[(f32, f32)]) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for &(x, y) in points {
        let p = Point {
            x: x.round() as i32,
            y: y.round() as i32,
        };
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    out
}

/// Resamples a polyline at a fixed arc-length `spacing`, always keeping the
/// last point exactly.
fn resample(points: &[(f32, f32)], spacing: f32) -> Vec<(f32, f32)> {
    if points.len() < 2 || spacing <= 0.0 {
        return points.to_vec();
    }
    let mut out = vec![points[0]];
    let mut remaining = spacing;
    for w in points.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        let dx = x1 - x0;
        let dy = y1 - y0;
        let seg_len = dx.hypot(dy);
        if seg_len == 0.0 {
            continue;
        }
        let mut travelled = 0.0f32;
        while remaining <= seg_len - travelled {
            travelled += remaining;
            let t = travelled / seg_len;
            out.push((x0 + dx * t, y0 + dy * t));
            remaining = spacing;
        }
        remaining -= seg_len - travelled;
    }
    let last = points[points.len() - 1];
    if out.last() != Some(&last) {
        out.push(last);
    }
    out
}

/// Perpendicular distance from `p` to the infinite line through `a`/`b`
/// (the point-to-point distance when `a == b`).
fn perpendicular_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py) = p;
    let (ax, ay) = a;
    let (bx, by) = b;
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return (px - ax).hypot(py - ay);
    }
    ((dy * px - dx * py + bx * ay - by * ax).abs()) / len2.sqrt()
}

/// Ramer-Douglas-Peucker simplification at tolerance `epsilon`.
fn rdp(points: &[(f32, f32)], epsilon: f32) -> Vec<(f32, f32)> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let (start, end) = (points[0], points[points.len() - 1]);
    let mut max_dist = 0.0f32;
    let mut split = 0;
    for (i, &p) in points.iter().enumerate().take(points.len() - 1).skip(1) {
        let d = perpendicular_distance(p, start, end);
        if d > max_dist {
            max_dist = d;
            split = i;
        }
    }
    if max_dist > epsilon {
        let mut left = rdp(&points[..=split], epsilon);
        let right = rdp(&points[split..], epsilon);
        left.pop();
        left.extend(right);
        left
    } else {
        vec![start, end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusforge_core::project::{BrushMode, Op, StrokeSource};
    use focusforge_core::retouch::blend::alpha;
    use focusforge_core::zmap::ZMapInfo;

    const W: u32 = 48;
    const H: u32 = 40;

    /// A deterministic wobbly trace of `len` deltas, some sub-pixel, some
    /// several pixels long, turning often.
    fn deltas(len: usize) -> Vec<(f32, f32)> {
        (0..len)
            .map(|i| {
                let t = i as f32;
                (1.3 * (t * 0.7).cos() + 0.4, 1.1 * (t * 0.45).sin())
            })
            .collect()
    }

    fn stroke_op(path: Vec<Point>) -> Op {
        Op::frame_stroke(3, path, 2.5, 0.4)
    }

    /// `alpha` of `path` at every pixel of the test map.
    fn alphas(path: Vec<Point>) -> Vec<u32> {
        let info = ZMapInfo::new(W, H, 4).unwrap();
        let op = stroke_op(path);
        (0..W * H)
            .map(|i| alpha(&op, info, i % W, i / W).unwrap())
            .collect()
    }

    #[test]
    fn take_segment_is_none_without_new_motion() {
        let mut stroke = LiveStroke::new(
            StrokeSource::Frame(1),
            2.5,
            0.4,
            BrushMode::Normal,
            1.0,
            5.0,
            5.0,
        );
        assert_eq!(stroke.take_segment(), None, "a fresh stroke has no motion");
        stroke.push(3.0, 1.0);
        assert!(stroke.take_segment().is_some());
        assert_eq!(stroke.take_segment(), None, "nothing arrived since");
    }

    #[test]
    fn segments_join_into_the_path_so_far_and_it_covers_each_segment() {
        let trace = deltas(60);
        let fresh = LiveStroke::new(
            StrokeSource::Frame(1),
            2.5,
            0.4,
            BrushMode::Normal,
            1.0,
            6.0,
            20.0,
        );
        assert_eq!(fresh.path_so_far(), vec![Point { x: 6, y: 20 }]);

        for flushes in [1usize, 3, 17] {
            let mut stroke = LiveStroke::new(
                StrokeSource::Frame(1),
                2.5,
                0.4,
                BrushMode::Normal,
                1.0,
                6.0,
                20.0,
            );
            let mut joined: Vec<Point> = Vec::new();
            for chunk in trace.chunks(trace.len().div_ceil(flushes)) {
                for &(dx, dy) in chunk {
                    stroke.push(dx, dy);
                }
                let segment = stroke.take_segment().expect("motion arrived");
                if joined.is_empty() {
                    joined.extend_from_slice(&segment);
                } else {
                    assert_eq!(
                        joined.last(),
                        segment.first(),
                        "{flushes} flushes: consecutive segments share their joint"
                    );
                    joined.extend_from_slice(&segment[1..]);
                }
                let so_far = stroke.path_so_far();
                assert_eq!(so_far, joined, "{flushes} flushes");
                let whole = alphas(so_far);
                for (pixel, (&all, &part)) in whole.iter().zip(&alphas(segment)).enumerate() {
                    assert!(
                        all >= part,
                        "{flushes} flushes: pixel {pixel}: {all} < {part}"
                    );
                }
            }
            let mut whole = LiveStroke::new(
                StrokeSource::Frame(1),
                2.5,
                0.4,
                BrushMode::Normal,
                1.0,
                6.0,
                20.0,
            );
            for &(dx, dy) in &trace {
                whole.push(dx, dy);
            }
            assert_eq!(stroke.path_so_far(), round_and_dedup(&whole.points));
        }
    }

    #[test]
    fn finish_resamples_and_simplifies() {
        // A straight run collapses to its two ends however many events it
        // arrived as.
        let mut straight = LiveStroke::new(
            StrokeSource::Frame(0),
            8.0,
            0.0,
            BrushMode::Normal,
            1.0,
            2.0,
            3.0,
        );
        for _ in 0..300 {
            straight.push(0.1, 0.05);
        }
        assert_eq!(
            straight.finish(),
            vec![Point { x: 2, y: 3 }, Point { x: 32, y: 18 }]
        );

        // An L keeps its corner and nothing near it lies off the shape.
        let mut corner = LiveStroke::new(
            StrokeSource::Frame(0),
            4.0,
            0.0,
            BrushMode::Normal,
            1.0,
            0.0,
            0.0,
        );
        for _ in 0..200 {
            corner.push(0.1, 0.0);
        }
        for _ in 0..200 {
            corner.push(0.0, 0.1);
        }
        let path = corner.finish();
        assert_eq!(path.first(), Some(&Point { x: 0, y: 0 }));
        assert_eq!(path.last(), Some(&Point { x: 20, y: 20 }));
        assert!(path.contains(&Point { x: 20, y: 0 }), "{path:?}");
        assert!(path.len() <= 4, "{path:?}");

        // A press with no motion is a one-point path.
        let dab = LiveStroke::new(
            StrokeSource::Frame(0),
            4.0,
            0.0,
            BrushMode::Normal,
            1.0,
            7.4,
            9.6,
        );
        assert_eq!(dab.finish(), vec![Point { x: 7, y: 10 }]);
    }

    #[test]
    fn cursor_square_holds_the_mode_tag() {
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;
        // 4: the chip bounds the square; 58: about the crossover; 120: the
        // ring bounds it.
        for radius in [4.0f32, 58.0, 120.0] {
            for softness in [0.0, 0.5] {
                let paths = cursor_paths(radius, softness);
                let half = (radius + 1.0).max(diagonal * radius + 18.0);
                assert_eq!(paths.extent, 2.0 * half, "r {radius} soft {softness}");
                assert_eq!(
                    paths.tag_origin,
                    half + diagonal * radius + CURSOR_TAG_GAP,
                    "r {radius} soft {softness}"
                );
                assert!(
                    paths.tag_origin + CURSOR_TAG_SIDE <= paths.extent,
                    "the chip lies inside the square: r {radius} soft {softness}"
                );
                let core_radius = radius * (1.0 - softness);
                let centred = format!("M {:.2} {:.2}", half - core_radius, half);
                assert!(
                    paths.core.starts_with(&centred),
                    "the core is centred at extent / 2: {} lacks {centred}",
                    paths.core
                );
            }
        }
        assert!(
            cursor_paths(4.0, 0.0).extent > 2.0 * (4.0 + 1.0),
            "chip-bound"
        );
        assert_eq!(cursor_paths(120.0, 0.0).extent, 2.0 * 121.0, "ring-bound");
    }

    #[test]
    fn cursor_core_is_the_whole_edge_for_a_hard_brush() {
        let hard = cursor_paths(10.0, 0.0);
        assert!(hard.ring.is_empty());
        assert_eq!(
            hard.extent,
            2.0 * (std::f32::consts::FRAC_1_SQRT_2 * 10.0 + 18.0)
        );
        assert!(hard.core.contains("A 10.00 10.00"), "{}", hard.core);

        let soft = cursor_paths(10.0, 0.5);
        assert!(soft.core.contains("A 5.00 5.00"), "{}", soft.core);
        assert_eq!(
            soft.ring.matches('M').count(),
            ((std::f32::consts::TAU * 10.0) / (CURSOR_DASH + CURSOR_GAP)).round() as usize
        );
    }
}
