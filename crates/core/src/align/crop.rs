// SPDX-License-Identifier: GPL-3.0-or-later

//! Auto-crop: the region every frame of a stack covers, and the largest
//! axis-aligned rectangle inscribed in it.
//!
//! Each frame's kernel-inset footprint (inset by [`super::KERNEL_RADIUS`], mapped into reference
//! coordinates through its stored [`super::Transform`]) is intersected against every other frame's
//! by incremental convex-polygon clipping. The largest axis-aligned rectangle inscribed in that
//! intersection is found exactly over the continuous `(y_t, y_b)` search space, then rounded inward
//! once (`ceil`/`floor`) into the `u32` [`crate::tile::Rect`] this module returns -- up to one
//! pixel per side short of the continuous optimum.
//!
//! The inset exists because [`super::warp`] needs source taps up to
//! [`super::KERNEL_RADIUS`] pixels past a footprint-boundary destination
//! pixel; the rim the inset removes is edge-clamped, not black.
//!
//! This module never reads, writes or discards a pixel: the crop it
//! computes is a [`crate::tile::Rect`], stored beside the transforms
//! rather than applied.

use crate::align::{AlignError, KERNEL_RADIUS, StackAlignment, Transform};
use crate::tile::Rect;

// ---------------------------------------------------------------------
// Footprint quads
// ---------------------------------------------------------------------

/// One frame's kernel-inset footprint, mapped into reference coordinates,
/// as a winding-normalized quad (CCW, so the clipper's `cross(b - a, p - a)
/// >= 0` "inside" test has one consistent sign).
///
/// Refuses a non-affine `t` (`AlignError::NotAffine`) before doing anything
/// else, and propagates `AlignError::NonInvertible` from [`Transform::inverse`]
/// and `AlignError::HorizonCrossesRect` from [`Transform::map_point`] --
/// both unreachable for a genuinely affine, invertible `t`, but never
/// `unwrap`ed.
fn footprint_quad(t: &Transform, frame: (u32, u32)) -> Result<[(f64, f64); 4], AlignError> {
    if t.m[2][0] != 0.0 || t.m[2][1] != 0.0 || t.m[2][2] == 0.0 {
        return Err(AlignError::NotAffine { bottom_row: t.m[2] });
    }
    let inv = t.inverse()?;

    let r = KERNEL_RADIUS as f64;
    let (w, h) = (frame.0 as f64, frame.1 as f64);
    // The frame's own covered rectangle, in frame coordinates, inset by the
    // kernel radius in *source* samples -- before the inverse map, so the
    // inset is not itself scaled by the transform.
    let src_corners = [
        (r, r),
        (w - 1.0 - r, r),
        (w - 1.0 - r, h - 1.0 - r),
        (r, h - 1.0 - r),
    ];
    let frame_rect = Rect {
        x: 0,
        y: 0,
        width: frame.0,
        height: frame.1,
    };

    let mut quad = [(0.0, 0.0); 4];
    for (i, &(x, y)) in src_corners.iter().enumerate() {
        quad[i] = inv
            .map_point(x, y)
            .ok_or(AlignError::HorizonCrossesRect { rect: frame_rect })?;
    }
    if signed_area(&quad) < 0.0 {
        quad.reverse();
    }
    Ok(quad)
}

fn signed_area(poly: &[(f64, f64); 4]) -> f64 {
    let mut sum = 0.0;
    for i in 0..4 {
        let (x1, y1) = poly[i];
        let (x2, y2) = poly[(i + 1) % 4];
        sum += x1 * y2 - x2 * y1;
    }
    sum * 0.5
}

// ---------------------------------------------------------------------
// Incremental convex clipping (Sutherland-Hodgman)
// ---------------------------------------------------------------------

/// `(b - a) x (p - a)`: positive when `p` is to the left of the directed
/// edge `a -> b`.
fn cross(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// The point where segment `p1 -> p2` crosses the infinite line through `a
/// -> b`, by the standard line-parameter construction. `None` only when the
/// segment is parallel to the line, which never happens at a call site here
/// (always called with one endpoint inside and one outside the clip edge).
fn segment_intersection(
    p1: (f64, f64),
    p2: (f64, f64),
    a: (f64, f64),
    b: (f64, f64),
) -> Option<(f64, f64)> {
    let d1 = cross(a, b, p1);
    let d2 = cross(a, b, p2);
    let denom = d1 - d2;
    if denom.abs() < 1e-15 {
        return None;
    }
    let t = d1 / denom;
    Some((p1.0 + t * (p2.0 - p1.0), p1.1 + t * (p2.1 - p1.1)))
}

/// Pushes `p` unless it lies within `1e-12` of the last point already in
/// `out` -- keeps the polygon's ordinate list clean of near-duplicate
/// vertices produced by a clip landing almost exactly on an existing corner.
fn push_dedup(out: &mut Vec<(f64, f64)>, p: (f64, f64)) {
    if let Some(&last) = out.last()
        && (last.0 - p.0).hypot(last.1 - p.1) < 1e-12
    {
        return;
    }
    out.push(p);
}

/// Clips `subject` (assumed CCW) against the convex CCW quad `clip`, one
/// edge at a time, writing the result back into `subject`. `scratch` is
/// reused across calls so the polygon is never rebuilt from an owned `Vec`
/// per clip -- the driver folds many frames' quads into one growing
/// `subject`, never re-intersecting from scratch.
fn clip_convex(
    subject: &mut Vec<(f64, f64)>,
    clip: &[(f64, f64); 4],
    scratch: &mut Vec<(f64, f64)>,
) {
    for e in 0..4 {
        if subject.is_empty() {
            return;
        }
        let a = clip[e];
        let b = clip[(e + 1) % 4];
        scratch.clear();
        let n = subject.len();
        for k in 0..n {
            let cur = subject[k];
            let prev = subject[(k + n - 1) % n];
            let cur_inside = cross(a, b, cur) >= 0.0;
            let prev_inside = cross(a, b, prev) >= 0.0;
            if cur_inside {
                if !prev_inside && let Some(pt) = segment_intersection(prev, cur, a, b) {
                    push_dedup(scratch, pt);
                }
                push_dedup(scratch, cur);
            } else if prev_inside && let Some(pt) = segment_intersection(prev, cur, a, b) {
                push_dedup(scratch, pt);
            }
        }
        // A closing near-duplicate (last point re-landing on the first) is
        // the same deduplication, applied across the wrap-around seam.
        if scratch.len() > 1 {
            let first = scratch[0];
            let last = scratch[scratch.len() - 1];
            if (first.0 - last.0).hypot(first.1 - last.1) < 1e-12 {
                scratch.pop();
            }
        }
        std::mem::swap(subject, scratch);
    }
}

// ---------------------------------------------------------------------
// Slab table
// ---------------------------------------------------------------------

/// One horizontal slab `[y_lo, y_hi]` of a convex polygon: within it, both
/// the left and the right boundary are a single affine function of `y`,
/// pinned by their values at the slab's two ordinates.
#[derive(Debug, Clone, Copy)]
struct Slab {
    y_lo: f64,
    y_hi: f64,
    left_lo: f64,
    left_hi: f64,
    right_lo: f64,
    right_hi: f64,
}

impl Slab {
    fn left_at(&self, y: f64) -> f64 {
        if self.y_hi == self.y_lo {
            return self.left_lo;
        }
        let t = (y - self.y_lo) / (self.y_hi - self.y_lo);
        self.left_lo + t * (self.left_hi - self.left_lo)
    }

    fn right_at(&self, y: f64) -> f64 {
        if self.y_hi == self.y_lo {
            return self.right_lo;
        }
        let t = (y - self.y_lo) / (self.y_hi - self.y_lo);
        self.right_lo + t * (self.right_hi - self.right_lo)
    }

    /// `(a, b)` such that `left(y) = a + b * y`.
    fn left_coeffs(&self) -> (f64, f64) {
        if self.y_hi == self.y_lo {
            return (self.left_lo, 0.0);
        }
        let b = (self.left_hi - self.left_lo) / (self.y_hi - self.y_lo);
        (self.left_lo - b * self.y_lo, b)
    }

    /// `(a, b)` such that `right(y) = a + b * y`.
    fn right_coeffs(&self) -> (f64, f64) {
        if self.y_hi == self.y_lo {
            return (self.right_lo, 0.0);
        }
        let b = (self.right_hi - self.right_lo) / (self.y_hi - self.y_lo);
        (self.right_lo - b * self.y_lo, b)
    }
}

/// The polygon's boundary span `(min_x, max_x)` at ordinate `y`: the min and
/// max `x` over every edge that straddles or touches `y`, including both
/// endpoints of a horizontal edge that sits exactly at `y`. `None` when no
/// edge reaches `y` at all (should not happen for `y` within the polygon's
/// own ordinate range).
fn span_at(polygon: &[(f64, f64)], y: f64) -> Option<(f64, f64)> {
    let n = polygon.len();
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut found = false;
    for k in 0..n {
        let (x1, y1) = polygon[k];
        let (x2, y2) = polygon[(k + 1) % n];
        if (y1 - y2).abs() < 1e-12 {
            if (y1 - y).abs() < 1e-9 {
                min_x = min_x.min(x1).min(x2);
                max_x = max_x.max(x1).max(x2);
                found = true;
            }
            continue;
        }
        let (lo, hi) = if y1 < y2 { (y1, y2) } else { (y2, y1) };
        if y >= lo - 1e-9 && y <= hi + 1e-9 {
            let t = ((y - y1) / (y2 - y1)).clamp(0.0, 1.0);
            let x = x1 + t * (x2 - x1);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            found = true;
        }
    }
    found.then_some((min_x, max_x))
}

/// Builds the slab table over `polygon`'s own vertex ordinates. `None` when
/// the polygon has fewer than three vertices, or fewer than two distinct `y`
/// ordinates (it has degenerated to a horizontal sliver or a point) -- costs
/// exactly one [`span_at`] scan per distinct ordinate (`m + 1` for `m`
/// slabs), never one per slab-pair candidate.
fn build_slabs(polygon: &[(f64, f64)]) -> Option<Vec<Slab>> {
    if polygon.len() < 3 {
        return None;
    }
    let mut ys: Vec<f64> = polygon.iter().map(|&(_, y)| y).collect();
    ys.sort_by(f64::total_cmp);
    ys.dedup();
    if ys.len() < 2 {
        return None;
    }
    let spans: Vec<(f64, f64)> = ys
        .iter()
        .map(|&y| span_at(polygon, y))
        .collect::<Option<Vec<_>>>()?;
    Some(
        ys.windows(2)
            .zip(spans.windows(2))
            .map(|(yw, sw)| Slab {
                y_lo: yw[0],
                y_hi: yw[1],
                left_lo: sw[0].0,
                left_hi: sw[1].0,
                right_lo: sw[0].1,
                right_hi: sw[1].1,
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------
// The exact largest inscribed axis-aligned rectangle
// ---------------------------------------------------------------------

/// `a + b * v`, where `v` is `y_t` when the second element is `true`, else
/// `y_b`.
type Term = ((f64, f64), bool);

/// Solves `m * [x, y]^T = rhs` by Cramer's rule. `None` when
/// `|det(m)| <= 1e-9` -- a singular system, the parallel/degenerate case
/// every caller here is written to skip rather than divide through.
fn solve_2x2(m: [[f64; 2]; 2], rhs: [f64; 2]) -> Option<(f64, f64)> {
    let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
    if det.abs() <= 1e-9 {
        return None;
    }
    let x = (rhs[0] * m[1][1] - m[0][1] * rhs[1]) / det;
    let y = (m[0][0] * rhs[1] - rhs[0] * m[1][0]) / det;
    Some((x, y))
}

/// The stationary point of `area(t, b) = (b - t) * (g0 + g_t*t + g_b*b)`,
/// restricted to the line `alpha*t + beta*b == gamma`, found by substituting
/// the line into `area` and solving the resulting 1-D quadratic. `None` when
/// the line is degenerate (`alpha == beta == 0`) or the substituted
/// quadratic has no interior maximizer (its leading coefficient is ~0 --
/// the line runs parallel to `area`'s own level curves there).
fn stationary_on_line(
    g0: f64,
    g_t: f64,
    g_b: f64,
    alpha: f64,
    beta: f64,
    gamma: f64,
) -> Option<(f64, f64)> {
    if beta.abs() > 1e-12 {
        // b = slope * t + intercept
        let slope = -alpha / beta;
        let intercept = gamma / beta;
        let bt_lin = slope - 1.0; // (b - t) = bt_lin*t + intercept
        let g_const = g0 + g_b * intercept;
        let g_lin = g_t + g_b * slope; // (g0 + g_t*t + g_b*b) = g_lin*t + g_const
        let a2 = bt_lin * g_lin;
        let a1 = bt_lin * g_const + intercept * g_lin;
        if a2.abs() <= 1e-12 {
            return None;
        }
        let t = -a1 / (2.0 * a2);
        Some((t, slope * t + intercept))
    } else if alpha.abs() > 1e-12 {
        let t = gamma / alpha;
        // area(t, b) fixed t, quadratic in b: g_b*b^2 + (g0 + g_t*t -
        // t*g_b)*b - t*(g0 + g_t*t)
        if g_b.abs() <= 1e-12 {
            return None;
        }
        let lin = g0 + g_t * t - t * g_b;
        let b = -lin / (2.0 * g_b);
        Some((t, b))
    } else {
        None
    }
}

/// The four `(g0, g_t, g_b)` coefficients of `right - left` as a function of
/// `(y_t, y_b)`, for one branch assignment (`left`, `right`).
fn branch_g(left: Term, right: Term) -> (f64, f64, f64) {
    let ((la, lb), left_is_t) = left;
    let ((ra, rb), right_is_t) = right;
    let g0 = ra - la;
    let g_t = (if right_is_t { rb } else { 0.0 }) - (if left_is_t { lb } else { 0.0 });
    let g_b = (if !right_is_t { rb } else { 0.0 }) - (if !left_is_t { lb } else { 0.0 });
    (g0, g_t, g_b)
}

/// For one branch assignment (`left`, `right`): the interior stationary
/// point of the unconstrained quadratic `area = (y_b - y_t) *
/// (right - left)`, plus its four box-edge stationary points (`y_t` or
/// `y_b` pinned to a slab boundary, optimized over the other).
fn branch_candidates(
    left: Term,
    right: Term,
    box_: (f64, f64, f64, f64),
    mut push: impl FnMut(f64, f64),
) {
    let (g0, g_t, g_b) = branch_g(left, right);
    let (y_lo_i, y_hi_i, y_lo_j, y_hi_j) = box_;

    // Interior: solve the 2x2 system from setting both partials to zero.
    let m = [[-2.0 * g_t, g_t - g_b], [g_t - g_b, 2.0 * g_b]];
    if let Some((t, b)) = solve_2x2(m, [g0, -g0]) {
        push(t, b);
    }

    for &y0 in &[y_lo_i, y_hi_i] {
        if let Some((t, b)) = stationary_on_line(g0, g_t, g_b, 1.0, 0.0, y0) {
            push(t, b);
        }
    }
    for &y0 in &[y_lo_j, y_hi_j] {
        if let Some((t, b)) = stationary_on_line(g0, g_t, g_b, 0.0, 1.0, y0) {
            push(t, b);
        }
    }
}

/// The branch-swap line where slab `i`'s and slab `j`'s `matched`
/// boundary (left when `matched_is_left`, else right) are equal -- the
/// locus where that boundary's dominance flips -- paired against one choice
/// of the *other* boundary. Two calls, one per `other` choice, cover both
/// branch assignments consistent with this line.
fn swap_line_candidates(
    matched_i: (f64, f64),
    matched_j: (f64, f64),
    other: Term,
    matched_is_left: bool,
    mut push: impl FnMut(f64, f64),
) {
    let (a1, b1) = matched_i;
    let (a2, b2) = matched_j;
    let ((oa, ob), other_is_t) = other;

    let (g0, g_t, g_b) = if matched_is_left {
        (
            oa - a1,
            (if other_is_t { ob } else { 0.0 }) - b1,
            if !other_is_t { ob } else { 0.0 },
        )
    } else {
        (
            a1 - oa,
            b1 - (if other_is_t { ob } else { 0.0 }),
            -(if !other_is_t { ob } else { 0.0 }),
        )
    };

    // The line `a1 + b1*t == a2 + b2*b`, i.e. `b1*t - b2*b == a2 - a1`.
    if let Some((t, b)) = stationary_on_line(g0, g_t, g_b, b1, -b2, a2 - a1) {
        push(t, b);
    }
}

/// Scores one candidate `(y_t, y_b)` by the true area formula (clamped into
/// slab `si`'s and `sj`'s own ranges, and into `y_t <= y_b`, before
/// evaluation), keeping `best` when it strictly beats the current best --
/// strict `>`, so the first candidate of a tie wins and the result is
/// deterministic.
fn consider(
    best: &mut Option<(f64, f64, f64, f64, f64)>,
    si: &Slab,
    sj: &Slab,
    y_t: f64,
    y_b: f64,
) {
    if !y_t.is_finite() || !y_b.is_finite() {
        return;
    }
    let y_t = y_t.clamp(si.y_lo, si.y_hi);
    let y_b = y_b.clamp(sj.y_lo, sj.y_hi);
    if y_t > y_b {
        return;
    }
    let x_lo = si.left_at(y_t).max(sj.left_at(y_b));
    let x_hi = si.right_at(y_t).min(sj.right_at(y_b));
    if x_hi <= x_lo {
        return;
    }
    let area = (y_b - y_t) * (x_hi - x_lo);
    let better = match best {
        Some((a, ..)) => area > *a,
        None => true,
    };
    if better {
        *best = Some((area, x_lo, x_hi, y_t, y_b));
    }
}

/// The largest axis-aligned rectangle inscribed in the convex polygon
/// `slabs` describes, as `(x_lo, x_hi, y_lo, y_hi)` in the polygon's own
/// coordinates. `None` when every candidate has non-positive area (the
/// polygon has degenerated to a sliver or a point).
fn largest_rect(slabs: &[Slab]) -> Option<(f64, f64, f64, f64)> {
    let mut best: Option<(f64, f64, f64, f64, f64)> = None;

    for i in 0..slabs.len() {
        for j in i..slabs.len() {
            let si = &slabs[i];
            let sj = &slabs[j];
            let box_ = (si.y_lo, si.y_hi, sj.y_lo, sj.y_hi);

            let left_i: Term = (si.left_coeffs(), true);
            let left_j: Term = (sj.left_coeffs(), false);
            let right_i: Term = (si.right_coeffs(), true);
            let right_j: Term = (sj.right_coeffs(), false);

            // The box corners.
            for &y_t in &[si.y_lo, si.y_hi] {
                for &y_b in &[sj.y_lo, sj.y_hi] {
                    consider(&mut best, si, sj, y_t, y_b);
                }
            }

            // All four branch assignments.
            for &left in &[left_i, left_j] {
                for &right in &[right_i, right_j] {
                    branch_candidates(left, right, box_, |t, b| consider(&mut best, si, sj, t, b));
                }
            }

            // The two branch-swap lines, plus where they meet.
            for &right in &[right_i, right_j] {
                swap_line_candidates(left_i.0, left_j.0, right, true, |t, b| {
                    consider(&mut best, si, sj, t, b)
                });
            }
            for &left in &[left_i, left_j] {
                swap_line_candidates(right_i.0, right_j.0, left, false, |t, b| {
                    consider(&mut best, si, sj, t, b)
                });
            }
            let (la1, lb1) = left_i.0;
            let (la2, lb2) = left_j.0;
            let (ra1, rb1) = right_i.0;
            let (ra2, rb2) = right_j.0;
            if let Some((t, b)) = solve_2x2([[lb1, -lb2], [rb1, -rb2]], [la2 - la1, ra2 - ra1]) {
                consider(&mut best, si, sj, t, b);
            }

            // Each branch-swap line can also cross a box edge before it
            // reaches the box's interior -- the region the swap line
            // bounds is itself cut off there, so that crossing is a
            // vertex of the true piecewise area function too.
            let left_line = (lb1, -lb2, la2 - la1);
            let right_line = (rb1, -rb2, ra2 - ra1);
            for &(alpha, beta, gamma) in &[left_line, right_line] {
                for &t0 in &[si.y_lo, si.y_hi] {
                    if beta.abs() > 1e-12 {
                        consider(&mut best, si, sj, t0, (gamma - alpha * t0) / beta);
                    }
                }
                for &b0 in &[sj.y_lo, sj.y_hi] {
                    if alpha.abs() > 1e-12 {
                        consider(&mut best, si, sj, (gamma - beta * b0) / alpha, b0);
                    }
                }
            }
        }
    }

    best.map(|(_, x_lo, x_hi, y_t, y_b)| (x_lo, x_hi, y_t, y_b))
}

// ---------------------------------------------------------------------
// Rounding, at the boundary, once
// ---------------------------------------------------------------------

/// `Option<Rect>`, rounded strictly inward: `x = ceil(min_x)`, `x_end =
/// floor(max_x)`, `width = x_end - x + 1` (inclusive, matching
/// [`Transform::source_footprint`]'s own sample-centre arithmetic). `None`
/// when rounding empties the range in either axis. No epsilon: an
/// already-integer ordinate (identity, integer translations) rounds to
/// itself exactly.
fn round_rect(x_lo: f64, x_hi: f64, y_lo: f64, y_hi: f64) -> Option<Rect> {
    let x = x_lo.ceil();
    let x_end = x_hi.floor();
    let y = y_lo.ceil();
    let y_end = y_hi.floor();
    if x_end < x || y_end < y {
        return None;
    }
    debug_assert!(
        x >= 0.0 && y >= 0.0,
        "the covered region is a subset of the reference frame's own inset rect: x={x} y={y}"
    );
    Some(Rect {
        x: x as u32,
        y: y as u32,
        width: (x_end - x + 1.0) as u32,
        height: (y_end - y + 1.0) as u32,
    })
}

// ---------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------

/// The crop rect for `transforms` (one per frame, in any order) against a
/// `frame`-sized source frame: the largest axis-aligned rectangle inscribed
/// in the intersection of every frame's kernel-inset footprint, in
/// reference coordinates. `Ok(None)` for an empty `transforms`, a `frame`
/// at or below `2 * KERNEL_RADIUS` on either axis, or footprints whose
/// intersection has no positive area.
pub fn crop_rect(transforms: &[Transform], frame: (u32, u32)) -> Result<Option<Rect>, AlignError> {
    let r = KERNEL_RADIUS;
    if transforms.is_empty() || frame.0 <= 2 * r || frame.1 <= 2 * r {
        return Ok(None);
    }

    let mut subject = footprint_quad(&transforms[0], frame)?.to_vec();
    let mut scratch = Vec::new();
    for t in &transforms[1..] {
        if subject.is_empty() {
            return Ok(None);
        }
        let quad = footprint_quad(t, frame)?;
        clip_convex(&mut subject, &quad, &mut scratch);
    }
    if subject.is_empty() {
        return Ok(None);
    }

    let Some(slabs) = build_slabs(&subject) else {
        return Ok(None);
    };
    let Some((x_lo, x_hi, y_lo, y_hi)) = largest_rect(&slabs) else {
        return Ok(None);
    };
    Ok(round_rect(x_lo, x_hi, y_lo, y_hi))
}

/// [`crop_rect`] driven from a [`StackAlignment`]: seeds the fold with the
/// reference frame's own transform (identity by construction, so the seed
/// is the axis-aligned inset rect and the first clip is trivially exact),
/// then folds in every other frame's transform regardless of its
/// [`super::FrameAlignment`] status -- a diverged frame is not skipped,
/// because under either failure policy its transform is a genuine
/// statement about where that frame's pixels land, and dropping it would
/// admit pixels that frame does not cover.
pub fn auto_crop(
    alignment: &StackAlignment,
    frame: (u32, u32),
) -> Result<Option<Rect>, AlignError> {
    if alignment.frames.is_empty() {
        return Ok(None);
    }
    let reference = alignment.reference;
    let mut ordered = Vec::with_capacity(alignment.frames.len());
    ordered.push(alignment.frames[reference].transform);
    for (i, f) in alignment.frames.iter().enumerate() {
        if i != reference {
            ordered.push(f.transform);
        }
    }
    crop_rect(&ordered, frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic xorshift32 PRNG, seeded with a fixed constant --
    /// never `rand`, so these tests need no dependency and are
    /// bit-for-bit reproducible across runs.
    struct XorShift32(u32);

    impl XorShift32 {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
    }

    fn square_quad(w: f64, h: f64) -> [(f64, f64); 4] {
        [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]
    }

    fn random_quad(rng: &mut XorShift32) -> [(f64, f64); 4] {
        let cx = 40.0 + (rng.next() % 120) as f64;
        let cy = 40.0 + (rng.next() % 120) as f64;
        let hw = 40.0 + (rng.next() % 60) as f64;
        let hh = 40.0 + (rng.next() % 60) as f64;
        let angle = ((rng.next() % 90) as f64).to_radians();
        let (s, c) = angle.sin_cos();
        let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
        let mut out = [(0.0, 0.0); 4];
        for (i, &(x, y)) in corners.iter().enumerate() {
            out[i] = (cx + c * x - s * y, cy + s * x + c * y);
        }
        out
    }

    /// Every corner of `rect` maps, through every one of `transforms`, into
    /// that frame's kernel-inset interior -- the containment invariant
    /// crop guarantees.
    fn assert_inside_all_footprints(rect: Rect, transforms: &[Transform], frame: (u32, u32)) {
        let r = KERNEL_RADIUS as f64;
        let (w, h) = (frame.0 as f64, frame.1 as f64);
        let corners = [
            (rect.x as f64, rect.y as f64),
            ((rect.x + rect.width - 1) as f64, rect.y as f64),
            (rect.x as f64, (rect.y + rect.height - 1) as f64),
            (
                (rect.x + rect.width - 1) as f64,
                (rect.y + rect.height - 1) as f64,
            ),
        ];
        for t in transforms {
            for &(x, y) in &corners {
                let (fx, fy) = t
                    .map_point(x, y)
                    .expect("crop stays inside every footprint");
                assert!(
                    fx >= r - 1e-6 && fx <= w - 1.0 - r + 1e-6,
                    "fx {fx} outside [{r}, {}] for {t:?}",
                    w - 1.0 - r
                );
                assert!(
                    fy >= r - 1e-6 && fy <= h - 1.0 - r + 1e-6,
                    "fy {fy} outside [{r}, {}] for {t:?}",
                    h - 1.0 - r
                );
            }
        }
    }

    #[test]
    fn identity_transforms_give_the_inset_full_frame() {
        let (w, h) = (100u32, 80u32);
        let r = KERNEL_RADIUS;
        let transforms = vec![Transform::identity(); 4];
        let rect = crop_rect(&transforms, (w, h))
            .expect("identity is affine")
            .expect("non-empty");
        assert_eq!(
            rect,
            Rect {
                x: r,
                y: r,
                width: w - 2 * r,
                height: h - 2 * r,
            },
            "every footprint, including an identity one, is inset by KERNEL_RADIUS"
        );
    }

    #[test]
    fn pure_translations_give_the_computed_rect() {
        let (w, h) = (200u32, 150u32);
        let r = KERNEL_RADIUS as f64;
        let cases: [&[(f64, f64)]; 4] = [
            &[(0.0, 0.0), (10.0, 0.0)],
            &[(0.0, 0.0), (0.0, -7.0)],
            &[(0.0, 0.0), (-4.0, 6.0)],
            &[(0.0, 0.0), (5.0, -2.0), (-3.0, 4.0), (2.0, 2.0)],
        ];
        for txs in cases {
            let transforms: Vec<Transform> = txs
                .iter()
                .map(|&(tx, ty)| Transform::translation(tx, ty))
                .collect();
            let rect = crop_rect(&transforms, (w, h))
                .expect("translation is affine")
                .expect("non-empty for these small shifts");

            let min_tx = txs.iter().map(|&(tx, _)| tx).fold(f64::INFINITY, f64::min);
            let max_tx = txs
                .iter()
                .map(|&(tx, _)| tx)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_ty = txs.iter().map(|&(_, ty)| ty).fold(f64::INFINITY, f64::min);
            let max_ty = txs
                .iter()
                .map(|&(_, ty)| ty)
                .fold(f64::NEG_INFINITY, f64::max);

            let x_lo = (r - min_tx).ceil() as u32;
            let x_hi = (w as f64 - 1.0 - r - max_tx).floor() as u32;
            let y_lo = (r - min_ty).ceil() as u32;
            let y_hi = (h as f64 - 1.0 - r - max_ty).floor() as u32;
            let expected = Rect {
                x: x_lo,
                y: y_lo,
                width: x_hi - x_lo + 1,
                height: y_hi - y_lo + 1,
            };
            assert_eq!(rect, expected, "txs = {txs:?}");
        }
    }

    #[test]
    fn pure_scale_gives_the_smaller_footprint() {
        let (w, h) = (200u32, 150u32);
        let r = KERNEL_RADIUS as f64;
        for &s in &[1.5, 0.6] {
            let scale_t = Transform::similarity(s, 0.0, 0.0, 0.0);
            let transforms = vec![Transform::identity(), scale_t];
            let rect = crop_rect(&transforms, (w, h))
                .expect("similarity is affine")
                .expect("non-empty");

            let x_lo = r.max(r / s).ceil();
            let x_hi = (w as f64 - 1.0 - r).min((w as f64 - 1.0 - r) / s).floor();
            let y_lo = r.max(r / s).ceil();
            let y_hi = (h as f64 - 1.0 - r).min((h as f64 - 1.0 - r) / s).floor();
            let expected = Rect {
                x: x_lo as u32,
                y: y_lo as u32,
                width: (x_hi - x_lo + 1.0) as u32,
                height: (y_hi - y_lo + 1.0) as u32,
            };
            assert_eq!(rect, expected, "scale {s}");
        }
    }

    #[test]
    fn pure_rotation_gives_the_inscribed_rect() {
        let (w, h) = (200u32, 200u32);
        let r = KERNEL_RADIUS as f64;
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let theta = 7.0f64.to_radians();
        let to_origin = Transform::translation(-cx, -cy);
        let rot = Transform::similarity(1.0, theta, 0.0, 0.0);
        let back = Transform::translation(cx, cy);
        let rotated = back.compose(&rot).compose(&to_origin);

        let transforms = vec![Transform::identity(), rotated];
        let rect = crop_rect(&transforms, (w, h))
            .expect("similarity is affine")
            .expect("non-empty for a small rotation");
        assert_inside_all_footprints(rect, &transforms, (w, h));

        // `side` is the inset rect's *extent*, not a sample count: its
        // corners are `[r, w - 1 - r]`, so the extent is `w - 1 - 2r`.
        let side = w as f64 - 1.0 - 2.0 * r;
        // The classical bound is a statement about the continuous problem; the search attains it to
        // within 1e-4 here. `Rect::width` is a sample count, one more than the extent the bound
        // speaks of, so `area >= bound` is algebraically `delivered_extent >= lower_bound_side -
        // 1.0` -- exactly the one-pixel-per-side allowance rounding strictly inward is entitled to,
        // and no more. The allowance is nearly spent here: 174 delivered against a 174.9796
        // optimum, so losing one further pixel on either axis (174x175) fails the assertion.
        let lower_bound_side = side / (theta.cos() + theta.sin());
        let lower_bound_area = lower_bound_side * lower_bound_side;
        let area = rect.width as f64 * rect.height as f64;
        assert!(
            area >= lower_bound_area,
            "area {area} is smaller than the inscribed-square lower bound {lower_bound_area}"
        );
    }

    #[test]
    fn disjoint_footprints_are_empty() {
        let (w, h) = (100u32, 100u32);
        let transforms = vec![Transform::identity(), Transform::translation(1000.0, 0.0)];
        assert_eq!(
            crop_rect(&transforms, (w, h)).expect("translation is affine"),
            None
        );

        let tiny = 2 * KERNEL_RADIUS;
        assert_eq!(
            crop_rect(&[Transform::identity()], (tiny, 100)).expect("identity is affine"),
            None,
            "a frame at or below 2*KERNEL_RADIUS wide has no interior to inset"
        );
        assert_eq!(
            crop_rect(&[Transform::identity()], (100, tiny)).expect("identity is affine"),
            None
        );
    }

    #[test]
    fn single_frame_returns_its_own_footprint() {
        let (w, h) = (120u32, 90u32);
        let r = KERNEL_RADIUS;
        let rect = crop_rect(&[Transform::identity()], (w, h))
            .expect("identity is affine")
            .expect("non-empty");
        assert_eq!(
            rect,
            Rect {
                x: r,
                y: r,
                width: w - 2 * r,
                height: h - 2 * r,
            }
        );

        assert_eq!(
            crop_rect(&[], (w, h)).expect("empty slice never touches a transform"),
            None,
            "an empty transform slice is Ok(None), not a panic"
        );
    }

    #[test]
    fn projective_transform_is_refused() {
        let (w, h) = (100u32, 100u32);
        let projective =
            Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.001, 0.0, 1.0]]);
        match crop_rect(&[projective], (w, h)) {
            Err(AlignError::NotAffine { bottom_row }) => {
                assert_eq!(bottom_row, [0.001, 0.0, 1.0]);
            }
            other => panic!("expected NotAffine, got {other:?}"),
        }

        let singular = Transform::affine(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        match crop_rect(&[singular], (w, h)) {
            Err(AlignError::NonInvertible { .. }) => {}
            other => panic!("expected NonInvertible, got {other:?}"),
        }
    }

    #[test]
    fn crop_corners_lie_inside_every_footprint() {
        let (w, h) = (300u32, 250u32);
        let mut rng = XorShift32(0xC0FFEE);
        for _ in 0..200 {
            let n = 2 + (rng.next() % 4) as usize;
            let mut transforms = vec![Transform::identity()];
            for _ in 1..n {
                let t = if rng.next().is_multiple_of(2) {
                    let tx = (rng.next() % 41) as f64 - 20.0;
                    let ty = (rng.next() % 41) as f64 - 20.0;
                    Transform::translation(tx, ty)
                } else {
                    let scale = 0.9 + (rng.next() % 21) as f64 / 100.0;
                    let angle = ((rng.next() % 21) as f64 - 10.0).to_radians();
                    let tx = (rng.next() % 21) as f64 - 10.0;
                    let ty = (rng.next() % 21) as f64 - 10.0;
                    Transform::similarity(scale, angle, tx, ty)
                };
                transforms.push(t);
            }
            if let Some(rect) =
                crop_rect(&transforms, (w, h)).expect("translation/similarity is always affine")
            {
                assert_inside_all_footprints(rect, &transforms, (w, h));
            }
        }
    }

    #[test]
    fn exact_search_is_never_beaten_by_a_dense_grid_search() {
        let mut rng = XorShift32(0x1234_5678);
        let mut trials_run = 0;
        for _ in 0..40 {
            let mut subject = square_quad(200.0, 200.0).to_vec();
            let mut scratch = Vec::new();
            for _ in 0..3 {
                let quad = random_quad(&mut rng);
                clip_convex(&mut subject, &quad, &mut scratch);
                if subject.is_empty() {
                    break;
                }
            }
            if subject.len() < 3 {
                continue;
            }
            let Some(slabs) = build_slabs(&subject) else {
                continue;
            };
            let Some((x_lo, x_hi, y_lo, y_hi)) = largest_rect(&slabs) else {
                continue;
            };
            let exact_area = (x_hi - x_lo) * (y_hi - y_lo);
            trials_run += 1;

            let y_min = subject
                .iter()
                .map(|&(_, y)| y)
                .fold(f64::INFINITY, f64::min);
            let y_max = subject
                .iter()
                .map(|&(_, y)| y)
                .fold(f64::NEG_INFINITY, f64::max);
            let steps = 80;
            let mut grid_best = 0.0f64;
            for i in 0..=steps {
                let yt = y_min + (y_max - y_min) * i as f64 / steps as f64;
                for j in i..=steps {
                    let yb = y_min + (y_max - y_min) * j as f64 / steps as f64;
                    if let (Some((lt, rt)), Some((lb, rb))) =
                        (span_at(&subject, yt), span_at(&subject, yb))
                    {
                        let x_lo_g = lt.max(lb);
                        let x_hi_g = rt.min(rb);
                        if x_hi_g > x_lo_g {
                            grid_best = grid_best.max((yb - yt) * (x_hi_g - x_lo_g));
                        }
                    }
                }
            }

            assert!(
                exact_area >= grid_best - 1e-9,
                "exact area {exact_area} beaten by dense grid area {grid_best} on polygon {subject:?}"
            );
        }
        assert!(
            trials_run > 0,
            "every trial degenerated to an empty polygon"
        );
    }

    #[test]
    fn footprints_are_inset_by_the_kernel_radius() {
        let (w, h) = (200u32, 200u32);
        let r = KERNEL_RADIUS as f64;
        let t = Transform::similarity(2.0, 0.0, 0.0, 0.0);
        let quad = footprint_quad(&t, (w, h)).expect("similarity is affine and invertible");

        let min_x = quad.iter().map(|&(x, _)| x).fold(f64::INFINITY, f64::min);
        let max_x = quad
            .iter()
            .map(|&(x, _)| x)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = quad.iter().map(|&(_, y)| y).fold(f64::INFINITY, f64::min);
        let max_y = quad
            .iter()
            .map(|&(_, y)| y)
            .fold(f64::NEG_INFINITY, f64::max);

        // A 2x scale's inverse is a 0.5x scale, so the inset shrinks the
        // reference-space footprint by r/2, not by r.
        let expected_min = r / 2.0;
        let expected_max = (w as f64 - 1.0 - r) / 2.0;
        assert!((min_x - expected_min).abs() < 1e-9, "min_x {min_x}");
        assert!((max_x - expected_max).abs() < 1e-9, "max_x {max_x}");
        assert!((min_y - expected_min).abs() < 1e-9, "min_y {min_y}");
        assert!((max_y - expected_max).abs() < 1e-9, "max_y {max_y}");
    }

    #[test]
    fn degenerate_slabs_and_ties() {
        // Fewer than three vertices: no polygon at all.
        assert!(build_slabs(&[(0.0, 0.0), (1.0, 1.0)]).is_none());

        // A single distinct y ordinate: a degenerate horizontal sliver.
        let flat = vec![(0.0, 5.0), (10.0, 5.0), (5.0, 5.0)];
        assert!(build_slabs(&flat).is_none());

        // A zero-width polygon (a vertical segment, doubled to stay a valid
        // 4-gon): two distinct ordinates exist, so a slab is built, but no
        // candidate has positive width.
        let segment = vec![(0.0, 0.0), (0.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        let slabs = build_slabs(&segment).expect("two distinct y ordinates");
        assert!(
            largest_rect(&slabs).is_none(),
            "a zero-width polygon has no inscribed rectangle"
        );

        // A hand-built zero-height slab must not panic (division by zero in
        // its affine coefficients is guarded) and must not corrupt the
        // search over a neighbouring, well-formed slab.
        let degenerate_slab = Slab {
            y_lo: 5.0,
            y_hi: 5.0,
            left_lo: 1.0,
            left_hi: 1.0,
            right_lo: 9.0,
            right_hi: 9.0,
        };
        let normal_slab = Slab {
            y_lo: 5.0,
            y_hi: 15.0,
            left_lo: 1.0,
            left_hi: 2.0,
            right_lo: 9.0,
            right_hi: 8.0,
        };
        let mixed = vec![degenerate_slab, normal_slab];
        assert!(largest_rect(&mixed).is_some());

        // An axis-aligned rectangle: several branch assignments hit a
        // parallel (zero-determinant) contact-edge pair -- horizontal and
        // vertical edges have identical slopes across slabs -- and the
        // search must still find the full rectangle rather than panicking
        // or silently dropping area.
        let square = square_quad(50.0, 30.0).to_vec();
        let slabs = build_slabs(&square).expect("a rectangle has slabs");
        let (x_lo, x_hi, y_lo, y_hi) = largest_rect(&slabs).expect("non-empty");
        assert!(
            ((x_hi - x_lo) * (y_hi - y_lo) - 50.0 * 30.0).abs() < 1e-9,
            "expected the full 50x30 rectangle, got area {}",
            (x_hi - x_lo) * (y_hi - y_lo)
        );

        // Ties: strict `>` means repeated searches over identical input are
        // bit-for-bit identical -- the "first candidate wins" guarantee.
        assert_eq!(largest_rect(&slabs), largest_rect(&slabs));
    }
}
