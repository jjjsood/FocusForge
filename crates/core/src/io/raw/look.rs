// SPDX-License-Identifier: GPL-3.0-or-later

//! The look space RAW frames are fused in: one fixed, invertible map from
//! camera-linear RGB to display-referred RGB, taken from the reference
//! frame. Nothing is clipped, so values above white and below black survive
//! the round trip.

use rawler::imgop::chromatic_adaption::adapt_bradford;
use rawler::imgop::matrix::transform_1d;
use rawler::imgop::xyz::{Illuminant, SRGB_TO_XYZ_D65};
use rawler::rawimage::RawImage;

use crate::tile::Sample;

type Matrix = [[f64; 3]; 3];

const IDENTITY: Matrix = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Camera-linear to look-space transform: the as-shot white balance
/// (normalized to green), the camera's matrix to linear sRGB (D65), then the
/// sRGB transfer curve extended above 1 and mirrored below 0.
#[derive(Debug, Clone, PartialEq)]
pub struct LookTransform {
    balance: [f64; 3],
    to_srgb: Matrix,
    from_srgb: Matrix,
    /// `to_srgb` with the white balance folded in, for [`Self::forward_f32`].
    balanced_to_srgb: [[f32; 3]; 3],
}

impl LookTransform {
    /// The transform for a camera with as-shot multipliers `wb_coeffs` (RGBE
    /// order, NaN when the file carries none) and `xyz_to_camera`, its D65
    /// XYZ-to-camera matrix. Missing multipliers and a singular matrix leave
    /// the white balance and the colour matrix at identity.
    pub fn new(wb_coeffs: [f32; 4], xyz_to_camera: [[f32; 3]; 3]) -> Self {
        let usable = wb_coeffs[..3].iter().all(|c| c.is_finite() && *c > 0.0);
        let balance = if usable {
            let green = f64::from(wb_coeffs[1]);
            [
                f64::from(wb_coeffs[0]) / green,
                1.0,
                f64::from(wb_coeffs[2]) / green,
            ]
        } else {
            [1.0; 3]
        };
        let xyz_to_camera = xyz_to_camera.map(|row| row.map(f64::from));
        let srgb_to_xyz = SRGB_TO_XYZ_D65.map(|row| row.map(f64::from));
        let srgb_to_camera = normalize_rows(multiply(&xyz_to_camera, &srgb_to_xyz));
        let (to_srgb, from_srgb) = match invert(&srgb_to_camera) {
            Some(inverse) => (inverse, srgb_to_camera),
            None => (IDENTITY, IDENTITY),
        };
        let balanced_to_srgb =
            std::array::from_fn(|i| std::array::from_fn(|j| (to_srgb[i][j] * balance[j]) as f32));
        Self {
            balance,
            to_srgb,
            from_srgb,
            balanced_to_srgb,
        }
    }

    /// The transform for the camera that took `image`. The matrix is the D65
    /// one when the file has it, else the nearest illuminant's, Bradford-adapted
    /// to D65; a file with no 3x3 matrix keeps the identity.
    pub fn from_image(image: &RawImage) -> Self {
        const PREFERENCE: [Illuminant; 9] = [
            Illuminant::D65,
            Illuminant::A,
            Illuminant::B,
            Illuminant::C,
            Illuminant::D50,
            Illuminant::D55,
            Illuminant::D75,
            Illuminant::Daylight,
            Illuminant::Flash,
        ];
        let matrix = image
            .color_matrix_find_first(PREFERENCE)
            .and_then(|(illuminant, flat)| {
                let matrix = transform_1d::<3, 3>(&flat)?;
                Some(if illuminant == Illuminant::D65 {
                    matrix
                } else {
                    adapt_bradford(&illuminant, &Illuminant::D65, &matrix)
                })
            })
            .unwrap_or_else(|| IDENTITY.map(|row| row.map(|value| value as f32)));
        Self::new(image.wb_coeffs, matrix)
    }

    /// Camera-linear `rgb` to look space.
    pub fn forward(&self, rgb: [f64; 3]) -> [f64; 3] {
        let balanced = [
            rgb[0] * self.balance[0],
            rgb[1] * self.balance[1],
            rgb[2] * self.balance[2],
        ];
        apply(&self.to_srgb, balanced).map(encode)
    }

    /// [`Self::forward`] in `f32`, within 1e-6 of it scaled by the larger of 1
    /// and the input's largest magnitude: the per-pixel form for reads.
    #[inline]
    pub fn forward_f32(&self, rgb: [f32; 3]) -> [f32; 3] {
        self.balanced_to_srgb.map(|row| {
            let linear = row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2];
            encode_f32(linear)
        })
    }

    /// [`Self::forward_f32`] before the transfer curve: white balance and
    /// colour matrix in linear sRGB. Same operations, same order.
    #[inline]
    pub(super) fn linear_f32(&self, rgb: [f32; 3]) -> [f32; 3] {
        self.balanced_to_srgb
            .map(|row| row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2])
    }

    /// Look-space `rgb` back to camera-linear: the exact inverse of
    /// [`Self::forward`].
    pub fn inverse(&self, rgb: [f64; 3]) -> [f64; 3] {
        let camera = apply(&self.from_srgb, rgb.map(decode));
        [
            camera[0] / self.balance[0],
            camera[1] / self.balance[1],
            camera[2] / self.balance[2],
        ]
    }
}

/// The look space on the 16-bit scale a RAW document stores.
const WHITE: f64 = 65535.0;

/// A sensor channel counts as clipped from this fraction of the saturation
/// point up. A camera's practical clip can sit below its declared white level
/// (0.99365 of it on a 6960x4640 linear DNG), and demosaiced borders sit a
/// little under the clip, so the band is 2 %; a saturated sample returns from
/// the `f32` store within about 1e-5 relative.
const DETECTION_FRACTION: f64 = 1.0 - 0.02;

/// Narrows look-space samples of a RAW document to `u16`, clipping pixels
/// whose sensor saturated in camera space so they come out neutral instead of
/// tinted by the per-channel clamp.
#[derive(Debug, Clone, PartialEq)]
pub struct HighlightClip {
    look: LookTransform,
    saturation: f64,
}

impl HighlightClip {
    /// A clip for documents in `look` whose sensor saturates at `saturation`
    /// camera-linear (1.0 unless the frames were scaled by an equalization
    /// gain).
    pub fn new(look: LookTransform, saturation: f64) -> Self {
        Self { look, saturation }
    }

    /// Narrows one look-space triple on the 16-bit scale. A channel within the
    /// detection tolerance of the saturation point counts as clipped at it, and
    /// the balanced channels clamp to the least-amplified one's saturation
    /// point, so a fully saturated pixel comes out neutral. Every other pixel,
    /// and any triple with a non-finite value, narrows as `u16::narrow` does.
    pub fn narrow(&self, rgb: [f32; 3]) -> [u16; 3] {
        let plain = rgb.map(<u16 as Sample>::narrow);
        if !rgb.iter().all(|value| value.is_finite()) {
            return plain;
        }
        let camera = self.look.inverse(rgb.map(|value| f64::from(value) / WHITE));
        let threshold = self.saturation * DETECTION_FRACTION;
        if camera.iter().all(|channel| *channel < threshold) {
            return plain;
        }
        let balance = self.look.balance;
        let level = self.saturation * balance.iter().copied().fold(f64::INFINITY, f64::min);
        let clipped = std::array::from_fn(|c| {
            if camera[c] >= threshold {
                level / balance[c]
            } else {
                camera[c].min(level / balance[c])
            }
        });
        self.look
            .forward(clipped)
            .map(|channel| <u16 as Sample>::narrow((channel * WHITE) as f32))
    }

    /// [`Self::narrow`] over an interleaved RGB band; `out` has `samples`'
    /// length.
    pub fn narrow_band(&self, samples: &[f32], out: &mut [u16]) {
        assert_eq!(samples.len(), out.len(), "one output sample per input");
        for (rgb, narrowed) in samples.chunks_exact(3).zip(out.chunks_exact_mut(3)) {
            narrowed.copy_from_slice(&self.narrow([rgb[0], rgb[1], rgb[2]]));
        }
    }
}

/// Linear value where the sRGB line `12.92 x` meets the power segment, so the
/// curve is continuous to rounding.
const KNEE: f64 = 0.003_130_668_442_500_537;
/// The encoded value at [`KNEE`].
const ENCODED_KNEE: f64 = 12.92 * KNEE;

/// The sRGB transfer curve, odd-symmetric and defined for every real `x`.
fn encode(x: f64) -> f64 {
    let magnitude = x.abs();
    let encoded = if magnitude <= KNEE {
        12.92 * magnitude
    } else {
        1.055 * magnitude.powf(1.0 / 2.4) - 0.055
    };
    encoded.copysign(x)
}

/// [`encode`] in `f32`.
#[inline]
pub(super) fn encode_f32(x: f32) -> f32 {
    let magnitude = x.abs();
    let encoded = if magnitude <= KNEE as f32 {
        12.92 * magnitude
    } else {
        1.055 * magnitude.powf(1.0 / 2.4) - 0.055
    };
    encoded.copysign(x)
}

fn decode(y: f64) -> f64 {
    let magnitude = y.abs();
    let linear = if magnitude <= ENCODED_KNEE {
        magnitude / 12.92
    } else {
        ((magnitude + 0.055) / 1.055).powf(2.4)
    };
    linear.copysign(y)
}

fn apply(m: &Matrix, v: [f64; 3]) -> [f64; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

/// Each row scaled to sum to 1, so a camera-neutral triple maps to itself.
fn normalize_rows(m: Matrix) -> Matrix {
    m.map(|row| {
        let sum: f64 = row.iter().sum();
        if sum == 0.0 {
            row
        } else {
            row.map(|v| v / sum)
        }
    })
}

fn invert(m: &Matrix) -> Option<Matrix> {
    let cofactor = |r: usize, c: usize| {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (c1, c2) = ((c + 1) % 3, (c + 2) % 3);
        m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1]
    };
    let determinant: f64 = (0..3).map(|c| m[0][c] * cofactor(0, c)).sum();
    if !determinant.is_finite() || determinant.abs() < 1e-12 {
        return None;
    }
    Some(std::array::from_fn(|i| {
        std::array::from_fn(|j| cofactor(j, i) / determinant)
    }))
}

#[cfg(test)]
mod look_transform {
    use super::*;

    const NAN: f32 = f32::NAN;
    const XYZ_TO_CAMERA: [[f32; 3]; 3] = [
        [0.72, -0.21, -0.08],
        [-0.31, 1.12, 0.12],
        [-0.02, 0.19, 0.55],
    ];

    fn camera() -> LookTransform {
        LookTransform::new([2.1, 1.0, 1.6, NAN], XYZ_TO_CAMERA)
    }

    /// Deterministic triples spread over [-3, 5], plus the exact edges.
    fn triples() -> Vec<[f64; 3]> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64 * 8.0 - 3.0
        };
        let mut all: Vec<[f64; 3]> = (0..20_000).map(|_| [next(), next(), next()]).collect();
        all.extend([
            [0.0; 3],
            [1.0; 3],
            [-1.0; 3],
            [4.0, 0.0, -2.0],
            [1e-9, -1e-9, 3e-3],
        ]);
        all
    }

    #[test]
    fn forward_f32_stays_within_1e6_of_the_f64_forward() {
        let look = camera();
        for rgb in triples() {
            let exact = look.forward(rgb);
            let fast = look.forward_f32(rgb.map(|v| v as f32));
            let scale = rgb.iter().fold(1.0f64, |m, v| m.max(v.abs()));
            for channel in 0..3 {
                let error = (f64::from(fast[channel]) - exact[channel]).abs();
                let limit = 1e-6 * scale;
                assert!(error < limit, "{rgb:?}: {fast:?} against {exact:?}");
            }
        }
    }

    #[test]
    fn round_trips_any_triple_within_1e5_relative() {
        let look = camera();
        for rgb in triples() {
            let back = look.inverse(look.forward(rgb));
            for channel in 0..3 {
                let tolerance = 1e-5 * rgb[channel].abs().max(1e-9);
                assert!(
                    (back[channel] - rgb[channel]).abs() <= tolerance,
                    "{rgb:?} came back as {back:?}"
                );
            }
        }
    }

    #[test]
    fn values_above_one_and_below_zero_keep_their_order_and_sign() {
        let look = LookTransform::new(
            [NAN; 4],
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        );
        let high = look.forward([4.0, 4.0, 4.0]);
        let low = look.forward([-0.5, -0.5, -0.5]);
        assert!(high.iter().all(|v| *v > 1.0));
        assert!(low.iter().all(|v| *v < 0.0));
    }

    #[test]
    fn the_curve_is_continuous_and_strictly_increasing_through_zero_and_one() {
        let mut previous = encode(-4.0);
        for step in 1..=80_000 {
            let x = -4.0 + f64::from(step) * 1e-4;
            let y = encode(x);
            assert!(y > previous, "not increasing at {x}");
            assert!(
                (y - previous).abs() < 2e-3,
                "jump of {} at {x}",
                y - previous
            );
            assert!((decode(y) - x).abs() <= 1e-12 * x.abs().max(1.0));
            previous = y;
        }
        assert_eq!(encode(0.0), 0.0);
        assert!((encode(1.0) - 1.0).abs() < 1e-12);
        for x in [0.3, 1.0, 7.5] {
            assert_eq!(encode(-x), -encode(x));
        }
        assert!((encode(KNEE + 1e-12) - encode(KNEE - 1e-12)).abs() < 1e-10);
    }

    #[test]
    fn the_as_shot_neutral_maps_to_white() {
        let look = camera();
        let neutral = look.inverse([1.0, 1.0, 1.0]);
        let white = look.forward(neutral);
        for channel in white {
            assert!((channel - 1.0).abs() < 1e-9, "{white:?}");
        }
        for (found, expected) in neutral.iter().zip([1.0 / 2.1, 1.0, 1.0 / 1.6]) {
            assert!((found - expected).abs() < 1e-6, "{neutral:?}");
        }
    }

    #[test]
    fn white_balance_is_normalized_to_green() {
        let rgb = [0.2, 0.5, 0.1];
        let scaled = LookTransform::new([4.2, 2.0, 3.2, NAN], XYZ_TO_CAMERA).forward(rgb);
        let normal = camera().forward(rgb);
        for channel in 0..3 {
            assert!((scaled[channel] - normal[channel]).abs() < 1e-12);
        }
    }

    #[test]
    fn missing_or_unusable_multipliers_leave_the_balance_alone() {
        let rgb = [0.2, 0.5, 0.1];
        let unity = LookTransform::new([1.0, 1.0, 1.0, NAN], XYZ_TO_CAMERA).forward(rgb);
        for wb in [
            [NAN; 4],
            [2.0, NAN, 1.5, NAN],
            [0.0, 1.0, 1.0, NAN],
            [-1.0, 1.0, 1.0, NAN],
        ] {
            assert_eq!(LookTransform::new(wb, XYZ_TO_CAMERA).forward(rgb), unity);
        }
    }

    #[test]
    fn a_singular_matrix_falls_back_to_identity() {
        let singular = [[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 0.0, 1.0]];
        let look = LookTransform::new([NAN; 4], singular);
        let rgb = [0.25, 0.5, 0.75];
        assert!(look.forward(rgb).iter().all(|v| v.is_finite()));
        let back = look.inverse(look.forward(rgb));
        for channel in 0..3 {
            assert!((back[channel] - rgb[channel]).abs() < 1e-12);
        }
    }
}

#[cfg(test)]
mod raw_highlight_clip {
    use super::*;

    const XYZ_TO_CAMERA: [[f32; 3]; 3] = [
        [0.72, -0.21, -0.08],
        [-0.31, 1.12, 0.12],
        [-0.02, 0.19, 0.55],
    ];
    /// As-shot multipliers: one with red and blue above green, one with red
    /// below it.
    const MULTIPLIERS: [[f32; 3]; 2] = [[2.1, 1.0, 1.6], [0.6, 1.0, 2.4]];
    const SATURATIONS: [f64; 2] = [1.0, 0.8];

    fn looks() -> Vec<LookTransform> {
        MULTIPLIERS
            .iter()
            .map(|m| LookTransform::new([m[0], m[1], m[2], f32::NAN], XYZ_TO_CAMERA))
            .collect()
    }

    fn camera_of(look: &LookTransform, rgb: [f32; 3]) -> [f64; 3] {
        look.inverse(rgb.map(|v| f64::from(v) / WHITE))
    }

    /// Look-space triples on the 16-bit scale from below black to well above
    /// white, plus the non-finite and out-of-range edges.
    fn triples() -> Vec<[f32; 3]> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 0.3) as f32 * 65535.0
        };
        let mut all: Vec<[f32; 3]> = (0..20_000).map(|_| [next(), next(), next()]).collect();
        all.extend([
            [f32::NAN, 40_000.0, 40_000.0],
            [70_000.0, f32::INFINITY, 100.0],
            [f32::NEG_INFINITY, 65_535.0, 65_535.0],
            [-5.0, -70_000.0, 0.4],
            [90_000.0, 80_000.0, 70_000.0],
            [0.0; 3],
            [65_535.0; 3],
        ]);
        all
    }

    #[test]
    fn a_pixel_below_the_saturation_point_narrows_as_u16_narrow() {
        let mut compared = 0;
        let mut clipped = 0;
        for look in looks() {
            for saturation in SATURATIONS {
                let clip = HighlightClip::new(look.clone(), saturation);
                let threshold = saturation * DETECTION_FRACTION;
                let mut near_threshold = 0;
                for rgb in triples() {
                    let plain = rgb.map(<u16 as Sample>::narrow);
                    let finite = rgb.iter().all(|v| v.is_finite());
                    let camera = camera_of(&look, rgb);
                    let below = camera.iter().all(|c| *c < threshold);
                    if !finite || below {
                        assert_eq!(clip.narrow(rgb), plain, "{rgb:?} at {saturation}");
                        compared += 1;
                        if finite && camera.iter().any(|c| *c >= 0.97 * saturation) {
                            near_threshold += 1;
                        }
                    } else if clip.narrow(rgb) != plain {
                        clipped += 1;
                    }
                }
                assert!(
                    near_threshold > 0,
                    "no compared triple sits just under the threshold at {saturation}"
                );
            }
        }
        assert!(
            compared > 20_000,
            "only {compared} triples stayed unclipped"
        );
        assert!(clipped > 5_000, "only {clipped} triples were clipped");
    }

    #[test]
    fn a_pixel_saturated_in_every_channel_comes_out_neutral() {
        for look in looks() {
            let lowest = look.balance.iter().copied().fold(f64::INFINITY, f64::min);
            for saturation in SATURATIONS {
                let clip = HighlightClip::new(look.clone(), saturation);
                let expected =
                    <u16 as Sample>::narrow((encode(saturation * lowest) * WHITE) as f32);
                for scale in [0.9801, 0.99, 0.99365, 1.0, 1.2] {
                    let triple = look
                        .forward([saturation * scale; 3])
                        .map(|v| (v * WHITE) as f32);
                    let [r, g, b] = clip.narrow(triple);
                    assert!(r.abs_diff(g) <= 1 && g.abs_diff(b) <= 1, "{r} {g} {b}");
                    assert!(g.abs_diff(expected) <= 1, "{g} against {expected}");
                }
            }
        }
    }

    #[test]
    fn a_pixel_saturated_in_one_channel_loses_that_channel_and_raises_none() {
        for look in looks() {
            let lowest = look.balance.iter().copied().fold(f64::INFINITY, f64::min);
            for saturation in SATURATIONS {
                let clip = HighlightClip::new(look.clone(), saturation);
                for (channel, top) in (0..3)
                    .filter(|c| look.balance[*c] > lowest)
                    .flat_map(|c| [0.99, 1.0].map(|top| (c, top)))
                {
                    let mut camera: [f64; 3] =
                        std::array::from_fn(|c| 0.9 * saturation / look.balance[c]);
                    camera[channel] = top * saturation;
                    let triple = look.forward(camera).map(|v| (v * WHITE) as f32);
                    let narrowed = clip.narrow(triple);
                    assert_ne!(narrowed, triple.map(<u16 as Sample>::narrow));
                    // The 16-bit ceiling can only lower the camera values
                    // read back, never hide a raise from below.
                    assert!(narrowed.iter().all(|v| *v > 0), "{narrowed:?}");
                    let after = camera_of(&look, narrowed.map(f32::from));
                    for c in 0..3 {
                        assert!(after[c] <= camera[c] + 2e-3, "{after:?} against {camera:?}");
                    }
                    assert!(after[channel] < camera[channel] - 1e-3 || narrowed.contains(&65535));
                }
            }
        }
    }

    #[test]
    fn a_band_narrows_each_triple_as_narrow_does() {
        let clip = HighlightClip::new(looks().remove(0), 1.0);
        let triples = triples();
        let samples: Vec<f32> = triples.iter().flatten().copied().collect();
        let mut out = vec![0u16; samples.len()];
        clip.narrow_band(&samples, &mut out);
        for (rgb, narrowed) in triples.iter().zip(out.chunks_exact(3)) {
            assert_eq!(narrowed, clip.narrow(*rgb), "{rgb:?}");
        }
    }
}
