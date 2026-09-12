// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame-to-frame geometric transforms.
//!
//! [`Transform`] is declared here in its minimum useful form — the matrix,
//! `identity()` and `compose()` — because
//! `algorithm::tests::transform_is_one_representation` requires a
//! translation and a rotation-plus-scale to compose with each other. A later
//! extension adds inversion, forward and inverse point mapping, the per-mode
//! constructors and the footprint query, in place on this same type.

/// A single 3x3 projective (homogeneous) transform, general enough to
/// represent translation, similarity, affine and perspective motion.
///
/// `m` is row-major: `m[row][col]`. Translation-only and rotation+scale
/// modes are degree-of-freedom restrictions a *solver* applies when
/// producing a `Transform`, not separate types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub m: [[f64; 3]; 3],
}

impl Transform {
    /// The multiplicative identity: maps every point to itself.
    pub const fn identity() -> Self {
        Transform {
            m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// Composes `self` with `other` as the matrix product `self · other`:
    /// applying the result to a point is equivalent to applying `other`
    /// first, then `self`. This order is a pinned convention: the warp
    /// and its footprint query inherit it, and flipping it silently
    /// would be a wrong-by-180-degrees-of-translation bug nothing else
    /// catches.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translation(tx: f64, ty: f64) -> Transform {
        Transform {
            m: [[1.0, 0.0, tx], [0.0, 1.0, ty], [0.0, 0.0, 1.0]],
        }
    }

    fn rotation_scale(scale: f64, radians: f64) -> Transform {
        let (s, c) = radians.sin_cos();
        Transform {
            m: [
                [scale * c, -scale * s, 0.0],
                [scale * s, scale * c, 0.0],
                [0.0, 0.0, 1.0],
            ],
        }
    }

    #[test]
    fn identity_is_multiplicative_identity() {
        let t = translation(3.0, -2.0);
        assert_eq!(t.compose(&Transform::identity()), t);
        assert_eq!(Transform::identity().compose(&t), t);
    }

    #[test]
    fn compose_applies_the_right_operand_first() {
        // Translate then scale: scaling a point translated by (1, 0) by 2
        // moves the origin to (2, 0), not (1, 0) -- proving `other` is
        // applied before `self`.
        let scale = rotation_scale(2.0, 0.0);
        let shift = translation(1.0, 0.0);
        let composed = scale.compose(&shift);
        let point = [0.0, 0.0, 1.0];
        let mapped = apply(&composed, point);
        assert_eq!(mapped, [2.0, 0.0, 1.0]);
    }

    #[test]
    fn composition_is_associative_within_tolerance() {
        let a = translation(1.0, 2.0);
        let b = rotation_scale(1.5, 0.3);
        let c = translation(-4.0, 0.5);

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
}
