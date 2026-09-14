// SPDX-License-Identifier: GPL-3.0-or-later

//! The projective transform: a 3x3 matrix, composition, inversion, forward
//! point mapping and the source-footprint query every tiled warp starts
//! from.
//!
//! Three conventions are pinned here because every alignment, warp and
//! fusion caller in this crate inherits them:
//!
//! 1. **Pixel centres are integers: sample `k` sits at coordinate `k`.** No
//!    half-pixel offset anywhere in this module or in [`super::warp`]. This
//!    matches the only resampling convention already in the tree
//!    (`crate::fuse::pyramid` taps `2*x + i` with no offset).
//! 2. **A stored `Transform` maps reference-to-frame coordinates**: given a
//!    coordinate in the stack's reference frame, [`Transform::map_point`]
//!    returns where that point lands in *this* transform's own frame. This
//!    is the direction `AlignAlgorithm::align_frame` already implies (one
//!    call per frame, against `AlignContext::reference()`), the direction
//!    an inverse-compositional ECC solver wants (its warp carries template
//!    coordinates into the moving image), and the direction that leaves no
//!    matrix inversion in the warp's inner loop.
//! 3. **The warp is destination-driven**: [`super::warp`] iterates
//!    destination pixels and computes each one's source coordinate with the
//!    stored matrix applied *forward* (convention 2), which is what makes
//!    the output hole-free. [`Transform::inverse`] exists for callers that
//!    need it, never for the hot loop.

use serde::{Deserialize, Serialize};

use crate::align::error::AlignError;
use crate::tile::Rect;

/// A single 3x3 projective (homogeneous) transform, general enough to
/// represent translation, similarity, affine and perspective motion.
///
/// `m` is row-major: `m[row][col]`. Translation-only and rotation+scale
/// modes are degree-of-freedom restrictions a *solver* applies when
/// producing a `Transform`, not separate types.
///
/// `m` is public because four sites across this crate build a `Transform {
/// m }` literal directly (`crate::algorithm`, `crate::test_algorithms`); no
/// invariant can be enforced at construction, so every invariant this module
/// needs (invertibility, a finite `w`) is checked where it is used instead.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub m: [[f64; 3]; 3],
}

/// The interpolation kernel's tap radius, in pixels, on every side of a
/// sample. Bicubic Catmull-Rom needs the two nearest integer samples on
/// each side of a fractional coordinate, i.e. `floor(s) - 1 ..= floor(s) +
/// 2`; `2` is a superset of that reach and is what
/// [`Transform::source_footprint`] grows a destination rect's bounding box
/// by.
pub const KERNEL_RADIUS: u32 = 2;

/// The determinant threshold below which a matrix is treated as singular.
///
/// `1e-9` against typical entries near unity (a rotation-scale block has
/// magnitude `scale^2`, and a homography's bottom row is usually near
/// `[0, 0, 1]`): comfortably below any legitimate near-degenerate
/// similarity transform (an extreme but valid scale), while still catching
/// a truly singular matrix (zero scale, a repeated row) well before the
/// division in [`Transform::inverse`] would produce a huge or `NaN` result.
const DET_EPS: f64 = 1e-9;

/// The minimum admissible (signed) homogeneous `w` in [`Transform::map_point`].
///
/// Below this, the mapped coordinate would exceed any representable frame
/// extent (a numerator of order `1e4` -- comfortably larger than any frame
/// this project reads -- divided by `1e-6` already exceeds `1e10` px), so
/// the guard's only real job is to keep a `NaN` or an infinity from ever
/// reaching the sampler; the exact threshold does not need to be tight.
const MIN_W: f64 = 1e-6;

impl Transform {
    /// The multiplicative identity: maps every point to itself.
    pub const fn identity() -> Self {
        Transform {
            m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// A pure translation (2 degrees of freedom): `(x, y) -> (x + tx, y + ty)`.
    pub const fn translation(tx: f64, ty: f64) -> Self {
        Transform {
            m: [[1.0, 0.0, tx], [0.0, 1.0, ty], [0.0, 0.0, 1.0]],
        }
    }

    /// A uniform-scale rotation plus translation (4 degrees of freedom):
    /// rotate by `radians`, scale uniformly by `scale`, then translate by
    /// `(tx, ty)`.
    pub fn similarity(scale: f64, radians: f64, tx: f64, ty: f64) -> Self {
        let (s, c) = radians.sin_cos();
        Transform {
            m: [
                [scale * c, -scale * s, tx],
                [scale * s, scale * c, ty],
                [0.0, 0.0, 1.0],
            ],
        }
    }

    /// A general affine transform (6 degrees of freedom):
    /// `x' = a*x + b*y + e`, `y' = c*x + d*y + f`.
    pub const fn affine(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Transform {
            m: [[a, b, e], [c, d, f], [0.0, 0.0, 1.0]],
        }
    }

    /// The general projective case (8 degrees of freedom up to scale): the
    /// matrix `m` verbatim, with no restriction on its bottom row.
    pub const fn projective(m: [[f64; 3]; 3]) -> Self {
        Transform { m }
    }

    /// Decomposes a similarity matrix (as [`Transform::similarity`] and
    /// [`Transform::translation`] both produce) into `(scale, radians, tx,
    /// ty)` -- the inverse of [`Transform::similarity`]:
    /// `scale = hypot(m[0][0], m[1][0])`, `radians = atan2(m[1][0],
    /// m[0][0])`, `tx = m[0][2]`, `ty = m[1][2]`.
    ///
    /// This is a decomposition, not a general affine factorization: it
    /// reads the exact entries a similarity matrix has and says nothing
    /// about a matrix with independent shear or non-uniform scale (an
    /// [`Transform::affine`] or [`Transform::projective`] built by hand).
    /// It is exact for `EccMode::Translation` and `EccMode::Similarity`
    /// outputs, both of which produce similarity matrices.
    pub fn similarity_parts(&self) -> (f64, f64, f64, f64) {
        let scale = self.m[0][0].hypot(self.m[1][0]);
        let radians = self.m[1][0].atan2(self.m[0][0]);
        let tx = self.m[0][2];
        let ty = self.m[1][2];
        (scale, radians, tx, ty)
    }

    /// Composes `self` with `other` as the matrix product `self · other`:
    /// applying the result to a point is equivalent to applying `other`
    /// first, then `self`. This order is a pinned convention --
    /// [`super::warp`] and its footprint query inherit it, and flipping it
    /// silently would be a wrong-by-180-degrees-of-translation bug nothing
    /// else catches.
    #[allow(clippy::needless_range_loop)]
    pub fn compose(&self, other: &Transform) -> Transform {
        let mut m = [[0.0; 3]; 3];
        for row in 0..3 {
            for col in 0..3 {
                let mut sum = 0.0;
                for k in 0..3 {
                    sum += self.m[row][k] * other.m[k][col];
                }
                m[row][col] = sum;
            }
        }
        Transform { m }
    }

    /// The matrix inverse, via the cofactor/adjugate formula over `f64`.
    ///
    /// A determinant with `.abs() <= DET_EPS` is
    /// [`AlignError::NonInvertible`] rather than a `NaN`-filled matrix.
    pub fn inverse(&self) -> Result<Transform, AlignError> {
        let m = self.m;
        let (a, b, c) = (m[0][0], m[0][1], m[0][2]);
        let (d, e, f) = (m[1][0], m[1][1], m[1][2]);
        let (g, h, i) = (m[2][0], m[2][1], m[2][2]);

        let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
        if det.abs() <= DET_EPS {
            return Err(AlignError::NonInvertible { det });
        }
        let inv_det = 1.0 / det;

        Ok(Transform {
            m: [
                [
                    (e * i - f * h) * inv_det,
                    (c * h - b * i) * inv_det,
                    (b * f - c * e) * inv_det,
                ],
                [
                    (f * g - d * i) * inv_det,
                    (a * i - c * g) * inv_det,
                    (c * d - a * f) * inv_det,
                ],
                [
                    (d * h - e * g) * inv_det,
                    (b * g - a * h) * inv_det,
                    (a * e - b * d) * inv_det,
                ],
            ],
        })
    }

    /// The sign of the positive-`w` half-space, chosen from the matrix
    /// alone (never from a tile or a pixel): a projective matrix and its
    /// negation represent the same map, so this has to be a property of
    /// `self.m` for two different tilings of the same destination region to
    /// never disagree about which side of the horizon a point is on.
    fn w_reference_sign(&self) -> f64 {
        if self.m[2][2] < 0.0 { -1.0 } else { 1.0 }
    }

    /// The forward point map, with the perspective divide, entirely in
    /// `f64`. Returns `None` when the homogeneous `w` is non-finite or falls
    /// below `1e-6` (this module's minimum admissible signed `w`) on the
    /// reference side of the horizon -- checked **before** the divide,
    /// never as a comparison of results after it, because a WGSL twin has
    /// no division trap.
    pub fn map_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let m = self.m;
        let w = m[2][0] * x + m[2][1] * y + m[2][2];
        if !w.is_finite() || w * self.w_reference_sign() < MIN_W {
            return None;
        }
        let ux = m[0][0] * x + m[0][1] * y + m[0][2];
        let uy = m[1][0] * x + m[1][1] * y + m[1][2];
        Some((ux / w, uy / w))
    }

    /// The axis-aligned source rectangle that covers every sample a warp of
    /// destination rect `dst` needs, grown by [`KERNEL_RADIUS`] and clamped
    /// to a `frame`-sized source frame (`(width, height)`).
    ///
    /// A projective map sends lines to lines, so the image of `dst` is the
    /// quadrilateral of its four mapped corners, and the bounding box of
    /// those corners bounds every interior sample -- corners suffice, no
    /// per-pixel scan. The corners are `dst`'s *sample centres* per
    /// convention 1 in this module's doc: `(x, y) ..= (x + w - 1, y + h -
    /// 1)`, not the outer edges. Any corner [`Transform::map_point`]
    /// rejects is [`AlignError::HorizonCrossesRect`]: the bounding box is
    /// genuinely unbounded there, and refusing is cheaper and more honest
    /// than over-approximating to the whole frame.
    pub fn source_footprint(
        &self,
        dst: Rect,
        frame: (u32, u32),
    ) -> Result<SourceFootprint, AlignError> {
        let corners = [
            (dst.x as f64, dst.y as f64),
            ((dst.x + dst.width - 1) as f64, dst.y as f64),
            (dst.x as f64, (dst.y + dst.height - 1) as f64),
            (
                (dst.x + dst.width - 1) as f64,
                (dst.y + dst.height - 1) as f64,
            ),
        ];

        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for &(cx, cy) in &corners {
            let (sx, sy) = self
                .map_point(cx, cy)
                .ok_or(AlignError::HorizonCrossesRect { rect: dst })?;
            min_x = min_x.min(sx);
            max_x = max_x.max(sx);
            min_y = min_y.min(sy);
            max_y = max_y.max(sy);
        }

        let (x_lo, x_hi, x_clamped) = grow_and_clamp(min_x, max_x, frame.0);
        let (y_lo, y_hi, y_clamped) = grow_and_clamp(min_y, max_y, frame.1);

        if x_lo > x_hi || y_lo > y_hi {
            return Ok(SourceFootprint::Outside);
        }

        Ok(SourceFootprint::Covers {
            read: Rect {
                x: x_lo as u32,
                y: y_lo as u32,
                width: (x_hi - x_lo + 1) as u32,
                height: (y_hi - y_lo + 1) as u32,
            },
            clamped: x_clamped || y_clamped,
        })
    }
}

/// Grows `[min, max]` by [`KERNEL_RADIUS`] on both sides, in `i64`, then
/// clamps to `0..=dim - 1`. Returns `(lo, hi, clamped)`, `clamped` true when
/// the unclamped range wanted pixels outside `0..dim`. `lo > hi` on return
/// means the clamp emptied the interval -- every requested sample lies
/// outside the frame.
fn grow_and_clamp(min: f64, max: f64, dim: u32) -> (i64, i64, bool) {
    let lo = min.floor() as i64 - KERNEL_RADIUS as i64;
    let hi = max.floor() as i64 + KERNEL_RADIUS as i64;
    let dim_max = dim as i64 - 1;
    let clamped_lo = lo.max(0);
    let clamped_hi = hi.min(dim_max);
    (clamped_lo, clamped_hi, lo < 0 || hi > dim_max)
}

/// The result of [`Transform::source_footprint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFootprint {
    /// Non-empty, already clamped to the source frame. `clamped` is true
    /// when the unclamped footprint wanted pixels the frame does not have.
    Covers { read: Rect, clamped: bool },
    /// Every sample this destination rect needs lies outside the source
    /// frame: nothing to read, every destination pixel uncovered.
    Outside,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_multiplicative_identity() {
        let t = Transform::translation(3.0, -2.0);
        assert_eq!(t.compose(&Transform::identity()), t);
        assert_eq!(Transform::identity().compose(&t), t);
    }

    #[test]
    fn compose_applies_the_right_operand_first() {
        // Translate then scale: scaling a point translated by (1, 0) by 2
        // moves the origin to (2, 0), not (1, 0) -- proving `other` is
        // applied before `self`.
        let scale = Transform::similarity(2.0, 0.0, 0.0, 0.0);
        let shift = Transform::translation(1.0, 0.0);
        let composed = scale.compose(&shift);
        let point = [0.0, 0.0, 1.0];
        let mapped = apply(&composed, point);
        assert_eq!(mapped, [2.0, 0.0, 1.0]);
    }

    #[test]
    fn composition_is_associative_within_tolerance() {
        let a = Transform::translation(1.0, 2.0);
        let b = Transform::similarity(1.5, 0.3, 0.0, 0.0);
        let c = Transform::translation(-4.0, 0.5);

        let left = a.compose(&b).compose(&c);
        let right = a.compose(&b.compose(&c));

        for row in 0..3 {
            for col in 0..3 {
                assert!(
                    (left.m[row][col] - right.m[row][col]).abs() < 1e-12,
                    "row {row} col {col}: {} vs {}",
                    left.m[row][col],
                    right.m[row][col]
                );
            }
        }
    }

    #[allow(clippy::needless_range_loop)]
    fn apply(t: &Transform, p: [f64; 3]) -> [f64; 3] {
        let mut out = [0.0; 3];
        for row in 0..3 {
            out[row] = t.m[row][0] * p[0] + t.m[row][1] * p[1] + t.m[row][2] * p[2];
        }
        out
    }

    #[test]
    fn composition_and_inverse_algebra() {
        let t = Transform::similarity(1.7, 0.4, 3.0, -5.0);
        let t_inv = t.inverse().expect("similarity transform is invertible");

        let identity = t.compose(&t_inv);
        for row in 0..3 {
            for col in 0..3 {
                let expected = if row == col { 1.0 } else { 0.0 };
                assert!(
                    (identity.m[row][col] - expected).abs() < 1e-9,
                    "row {row} col {col}: {} vs {expected}",
                    identity.m[row][col]
                );
            }
        }

        let a = Transform::translation(2.0, -1.0);
        let b = Transform::similarity(0.5, 1.1, 4.0, 2.0);
        let composed = a.compose(&b);
        let (px, py) = (3.5, -2.25);
        let (bx, by) = b.map_point(px, py).unwrap();
        let (want_x, want_y) = a.map_point(bx, by).unwrap();
        let (got_x, got_y) = composed.map_point(px, py).unwrap();
        assert!((got_x - want_x).abs() < 1e-9);
        assert!((got_y - want_y).abs() < 1e-9);
    }

    #[test]
    fn mode_constructors_have_their_degrees_of_freedom() {
        let t = Transform::translation(3.0, -5.0);
        assert_eq!(t.m, [[1.0, 0.0, 3.0], [0.0, 1.0, -5.0], [0.0, 0.0, 1.0]]);

        let s = Transform::similarity(2.0, 0.3, 7.0, -1.0);
        assert_eq!(s.m[2], [0.0, 0.0, 1.0]);
        // Uniform-scale rotation: the 2x2 block is `scale * [[c, -s], [s, c]]`.
        assert!((s.m[0][0] - s.m[1][1]).abs() < 1e-12);
        assert!((s.m[0][1] + s.m[1][0]).abs() < 1e-12);

        let aff = Transform::affine(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
        assert_eq!(aff.m, [[1.0, 2.0, 5.0], [3.0, 4.0, 6.0], [0.0, 0.0, 1.0]]);

        let proj = Transform::projective([[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]]);
        assert_eq!(proj.m, [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]]);
    }

    #[test]
    fn similarity_parts() {
        for &(scale, radians, tx, ty) in &[
            (1.0, 0.0, 0.0, 0.0),
            (1.3, 0.5, 11.0, -4.0),
            (0.87, -1.1, -3.0, 6.0),
        ] {
            let t = Transform::similarity(scale, radians, tx, ty);
            let (got_scale, got_radians, got_tx, got_ty) = t.similarity_parts();
            assert!(
                (got_scale - scale).abs() < 1e-9,
                "scale: {got_scale} vs {scale}"
            );
            assert!(
                (got_radians - radians).abs() < 1e-9,
                "radians: {got_radians} vs {radians}"
            );
            assert!((got_tx - tx).abs() < 1e-9, "tx: {got_tx} vs {tx}");
            assert!((got_ty - ty).abs() < 1e-9, "ty: {got_ty} vs {ty}");
        }

        // A pure translation decomposes to unit scale and zero rotation --
        // `Transform::translation` is a restriction of the same similarity
        // family, never a distinct representation.
        let t = Transform::translation(5.0, -2.0);
        let (scale, radians, tx, ty) = t.similarity_parts();
        assert!((scale - 1.0).abs() < 1e-12);
        assert!(radians.abs() < 1e-12);
        assert!((tx - 5.0).abs() < 1e-12);
        assert!((ty - (-2.0)).abs() < 1e-12);
    }

    #[test]
    fn degenerate_transform_is_an_error() {
        let zero_scale = Transform::similarity(0.0, 0.7, 0.0, 0.0);
        match zero_scale.inverse() {
            Err(AlignError::NonInvertible { det }) => assert!(det.abs() < 1e-9),
            other => panic!("expected NonInvertible, got {other:?}"),
        }
    }

    #[test]
    fn vanishing_w_is_not_a_nan() {
        // A projective transform whose w-row makes w == 0 on the unit circle
        // at x == 1 (w = 1 - x): points with x >= 1 sit on or beyond the
        // horizon.
        let t = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 1.0]]);
        assert_eq!(t.map_point(1.0, 0.0), None);
        assert_eq!(t.map_point(2.0, 0.0), None);
        let (x, y) = t.map_point(0.0, 0.0).expect("w == 1 here, well inside");
        assert!(x.is_finite() && y.is_finite());

        let dst = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        match t.source_footprint(dst, (100, 100)) {
            Err(AlignError::HorizonCrossesRect { rect }) => assert_eq!(rect, dst),
            other => panic!("expected HorizonCrossesRect, got {other:?}"),
        }
    }

    #[test]
    fn footprint_contains_every_sample() {
        let t = Transform::similarity(1.3, 0.5, 11.0, -4.0);
        let dst = Rect {
            x: 10,
            y: 10,
            width: 20,
            height: 15,
        };
        let frame = (500, 500);
        let footprint = t.source_footprint(dst, frame).expect("no horizon crossing");
        let read = match footprint {
            SourceFootprint::Covers { read, .. } => read,
            SourceFootprint::Outside => panic!("expected Covers"),
        };

        for dy in 0..dst.height {
            for dx in 0..dst.width {
                let (sx, sy) = t
                    .map_point((dst.x + dx) as f64, (dst.y + dy) as f64)
                    .expect("no horizon crossing for an interior sample");
                assert!(
                    sx >= read.x as f64 && sx <= (read.x + read.width - 1) as f64,
                    "sx {sx} outside read.x range {:?}",
                    read
                );
                assert!(
                    sy >= read.y as f64 && sy <= (read.y + read.height - 1) as f64,
                    "sy {sy} outside read.y range {:?}",
                    read
                );
            }
        }
    }
}
