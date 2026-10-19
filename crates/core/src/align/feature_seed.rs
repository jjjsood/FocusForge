// SPDX-License-Identifier: GPL-3.0-or-later

//! Feature-based similarity seeds: AKAZE keypoints on one coarse pyramid
//! level, brute-force Hamming matching, a 2-point RANSAC similarity fit and
//! the inlier and spatial-coverage gates a seed must pass.

use crate::align::Transform;
use crate::fuse::pyramid::Plane;

/// The longest side, in pixels, of the level the detector runs on.
pub(crate) const SEED_PROXY_LONG_SIDE: u32 = 512;

/// Most keypoints kept per plane, strongest response first.
pub(crate) const MAX_FEATURES: usize = 2000;

/// Detector threshold per unit of the plane's luma variance (`u16` scale
/// normalised to `[0, 1]`), capped at the detector's own default.
const THRESHOLD_PER_VARIANCE: f64 = 0.03;

/// The detector's default response threshold, for full-contrast scenes.
const DEFAULT_THRESHOLD: f64 = 0.001;

/// The detector's scale space needs this much on the short side.
const MIN_EXTENT_PX: u32 = 40;

/// A match counts when its Hamming distance is under this share of the
/// second-best candidate's.
const RATIO: f64 = 0.8;

/// Fixed number of 2-point similarity hypotheses drawn.
const RANSAC_DRAWS: usize = 2000;

/// A match is an inlier within this many pixels of the model.
const INLIER_PX: f64 = 3.0;

/// Hypothesis pairs closer than this fix scale and rotation too loosely.
const MIN_PAIR_SEPARATION_PX: f64 = 8.0;

/// Inliers a fit needs before it may seed a solve.
const MIN_INLIERS: usize = 12;

/// Cells of the 4x4 grid over the reference level that must hold an inlier.
const MIN_OCCUPIED_CELLS: usize = 6;

/// The share of the matches the fit must explain: a fit that explains a
/// handful of many matches is a coincidence among near-repeats.
const MIN_INLIER_SHARE: f64 = 0.3;

/// Fewest occupied cells at which a fit may still seed when it explains
/// [`MIN_CLUSTERED_SHARE`] of its matches: a defocused frame keeps texture in
/// only a few cells, and a single cell fixes scale and rotation too loosely.
const MIN_CLUSTERED_CELLS: usize = 2;

/// The share of the matches a fit in [`MIN_CLUSTERED_CELLS`] to
/// [`MIN_OCCUPIED_CELLS`] - 1 cells must explain. Correct clustered seeds on a
/// 64-frame defocus series explained 0.767 to 0.918 of their matches.
const MIN_CLUSTERED_SHARE: f64 = 0.75;

/// Why a plane cannot be searched for features.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FeatureSeedError {
    /// The detector's scale space needs at least 40 px on the short side.
    #[error("a {width}x{height} plane is below the {MIN_EXTENT_PX} px the feature detector needs")]
    TooSmall { width: u32, height: u32 },
    /// The detector reads one luma sample per pixel.
    #[error(
        "the feature detector reads a single-channel plane, got {samples_per_pixel} samples per pixel"
    )]
    NotLuma { samples_per_pixel: u32 },
}

fn gray_image(plane: &Plane) -> akaze::image::GrayFloatImage {
    let mut gray =
        akaze::image::GrayFloatImage::new(plane.width() as usize, plane.height() as usize);
    for (dst, src) in gray.iter_mut().zip(plane.data()) {
        *dst = src / 65535.0;
    }
    gray
}

/// The response threshold for `plane`: the detector default on a
/// full-contrast scene, lower in proportion to the luma variance on a flat
/// one; `None` for a plane with no variance at all.
pub fn detector_threshold(plane: &Plane) -> Option<f64> {
    let n = plane.data().len() as f64;
    let mean = plane.data().iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let variance = plane
        .data()
        .iter()
        .map(|&v| (f64::from(v) - mean).powi(2))
        .sum::<f64>()
        / n
        / (65535.0 * 65535.0);
    (variance > 0.0).then(|| DEFAULT_THRESHOLD.min(THRESHOLD_PER_VARIANCE * variance))
}

/// Keypoints and binary descriptors of one plane.
pub struct Features {
    points: Vec<[f64; 2]>,
    descriptors: Vec<[u64; 8]>,
}

impl Features {
    /// Detects AKAZE features on `plane` (luma on the `u16` scale), keeping
    /// the 2000 strongest.
    pub fn detect(plane: &Plane) -> Result<Self, FeatureSeedError> {
        match detector_threshold(plane) {
            Some(threshold) => Self::detect_at(plane, threshold),
            None => Self::detect_at(plane, f64::INFINITY),
        }
    }

    /// [`Features::detect`] with an explicit detector response `threshold`.
    pub fn detect_at(plane: &Plane, threshold: f64) -> Result<Self, FeatureSeedError> {
        if plane.samples_per_pixel() != 1 {
            return Err(FeatureSeedError::NotLuma {
                samples_per_pixel: plane.samples_per_pixel(),
            });
        }
        if plane.width().min(plane.height()) < MIN_EXTENT_PX {
            return Err(FeatureSeedError::TooSmall {
                width: plane.width(),
                height: plane.height(),
            });
        }
        if !threshold.is_finite() {
            return Ok(Features {
                points: Vec::new(),
                descriptors: Vec::new(),
            });
        }
        let detector = akaze::Akaze {
            maximum_features: MAX_FEATURES,
            ..akaze::Akaze::new(threshold)
        };
        let (keypoints, descriptors) = detector.extract_from_gray_float_image(&gray_image(plane));
        Ok(Features {
            points: keypoints
                .iter()
                .map(|k| [f64::from(k.point.0), f64::from(k.point.1)])
                .collect(),
            descriptors: descriptors
                .iter()
                .map(|d| {
                    let mut words = [0u64; 8];
                    for (word, bytes) in words.iter_mut().zip(d.bytes().chunks_exact(8)) {
                        *word = u64::from_le_bytes(bytes.try_into().expect("chunks of 8 bytes"));
                    }
                    words
                })
                .collect(),
        })
    }

    /// The number of keypoints.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether no keypoint was found.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// A similarity fitted to matched keypoints, with the evidence behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeedFit {
    /// Maps reference coordinates to frame coordinates, in the detection level's pixels.
    pub transform: Transform,
    /// Ratio-test matches the fit drew from.
    pub matches: usize,
    /// Matches the fit explains within 3 px.
    pub inliers: usize,
    /// Cells of the 4x4 grid over the reference level holding an inlier.
    pub occupied_cells: usize,
}

impl SeedFit {
    /// Whether the evidence is strong and a large enough share of the matches
    /// to start a solve from: spread over at least 6 of the grid's cells, or
    /// clustered in at least 2 of them with an inlier share of at least 0.75.
    pub fn accepted(&self) -> bool {
        let explains = |share: f64| self.inliers as f64 >= share * self.matches as f64;
        self.inliers >= MIN_INLIERS
            && explains(MIN_INLIER_SHARE)
            && (self.occupied_cells >= MIN_OCCUPIED_CELLS
                || (self.occupied_cells >= MIN_CLUSTERED_CELLS && explains(MIN_CLUSTERED_SHARE)))
    }
}

/// A reference point and the frame point it matched.
type Correspondence = ([f64; 2], [f64; 2]);

/// Frame keypoints whose nearest reference descriptor is clearly closer than
/// the runner-up, paired with that reference keypoint.
fn match_features(reference: &Features, frame: &Features) -> Vec<Correspondence> {
    let mut pairs = Vec::new();
    for (point, descriptor) in frame.points.iter().zip(&frame.descriptors) {
        let (mut best, mut best_at, mut second) = (u32::MAX, 0, u32::MAX);
        for (j, candidate) in reference.descriptors.iter().enumerate() {
            let distance: u32 = descriptor
                .iter()
                .zip(candidate)
                .map(|(a, b)| (a ^ b).count_ones())
                .sum();
            if distance < best {
                (second, best, best_at) = (best, distance, j);
            } else if distance < second {
                second = distance;
            }
        }
        if second != u32::MAX && f64::from(best) < RATIO * f64::from(second) {
            pairs.push((reference.points[best_at], *point));
        }
    }
    pairs
}

/// `q = a p + b` over complex numbers: a similarity, `a = scale * e^(i angle)`.
#[derive(Clone, Copy)]
struct Model {
    a: [f64; 2],
    b: [f64; 2],
}

impl Model {
    fn apply(&self, p: [f64; 2]) -> [f64; 2] {
        [
            self.a[0] * p[0] - self.a[1] * p[1] + self.b[0],
            self.a[1] * p[0] + self.a[0] * p[1] + self.b[1],
        ]
    }

    fn residual(&self, (p, q): &Correspondence) -> f64 {
        let m = self.apply(*p);
        (m[0] - q[0]).hypot(m[1] - q[1])
    }

    /// The similarity taking `p1, p2` to `q1, q2`.
    fn through_two(p1: [f64; 2], p2: [f64; 2], q1: [f64; 2], q2: [f64; 2]) -> Option<Self> {
        let (dp, dq) = (
            [p2[0] - p1[0], p2[1] - p1[1]],
            [q2[0] - q1[0], q2[1] - q1[1]],
        );
        let norm = dp[0] * dp[0] + dp[1] * dp[1];
        if norm < MIN_PAIR_SEPARATION_PX * MIN_PAIR_SEPARATION_PX {
            return None;
        }
        let a = [
            (dq[0] * dp[0] + dq[1] * dp[1]) / norm,
            (dq[1] * dp[0] - dq[0] * dp[1]) / norm,
        ];
        let model = Model { a, b: [0.0; 2] };
        let at = model.apply(p1);
        Some(Model {
            a,
            b: [q1[0] - at[0], q1[1] - at[1]],
        })
    }

    /// The least-squares similarity over `pairs`.
    fn refit(pairs: &[&Correspondence]) -> Option<Self> {
        let n = pairs.len() as f64;
        let mean = |pick: fn(&Correspondence) -> [f64; 2]| {
            let sum = pairs.iter().fold([0.0; 2], |s, p| {
                let v = pick(p);
                [s[0] + v[0], s[1] + v[1]]
            });
            [sum[0] / n, sum[1] / n]
        };
        let (cp, cq) = (mean(|c| c.0), mean(|c| c.1));
        let (mut num, mut den) = ([0.0; 2], 0.0);
        for (p, q) in pairs.iter().map(|c| (c.0, c.1)) {
            let (u, v) = ([p[0] - cp[0], p[1] - cp[1]], [q[0] - cq[0], q[1] - cq[1]]);
            num[0] += u[0] * v[0] + u[1] * v[1];
            num[1] += u[0] * v[1] - u[1] * v[0];
            den += u[0] * u[0] + u[1] * u[1];
        }
        if den <= 0.0 {
            return None;
        }
        let a = [num[0] / den, num[1] / den];
        let centre = Model { a, b: [0.0; 2] }.apply(cp);
        Some(Model {
            a,
            b: [cq[0] - centre[0], cq[1] - centre[1]],
        })
    }
}

/// 2-point RANSAC over `pairs` (a fixed-seed generator, so a pair of inputs
/// always gives the same fit), refitted on its inliers.
fn fit_similarity(pairs: &[Correspondence], (width, height): (u32, u32)) -> Option<SeedFit> {
    if pairs.len() < 2 {
        return None;
    }
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut draw = |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    let inliers_of = |m: &Model| pairs.iter().filter(|c| m.residual(c) <= INLIER_PX).count();

    let mut best: Option<(usize, Model)> = None;
    for _ in 0..RANSAC_DRAWS {
        let (i, j) = (draw(pairs.len()), draw(pairs.len()));
        let Some(model) = Model::through_two(pairs[i].0, pairs[j].0, pairs[i].1, pairs[j].1) else {
            continue;
        };
        let count = inliers_of(&model);
        if best.is_none_or(|(c, _)| count > c) {
            best = Some((count, model));
        }
    }
    let (_, mut model) = best?;
    for _ in 0..2 {
        let inliers: Vec<&Correspondence> = pairs
            .iter()
            .filter(|c| model.residual(c) <= INLIER_PX)
            .collect();
        model = Model::refit(&inliers)?;
    }
    let inliers: Vec<&Correspondence> = pairs
        .iter()
        .filter(|c| model.residual(c) <= INLIER_PX)
        .collect();

    let mut cells = 0u16;
    for (p, _) in &inliers {
        let col = ((p[0] * 4.0 / f64::from(width)) as usize).min(3);
        let row = ((p[1] * 4.0 / f64::from(height)) as usize).min(3);
        cells |= 1 << (row * 4 + col);
    }
    let scale = model.a[0].hypot(model.a[1]);
    (scale > 0.0).then(|| SeedFit {
        transform: Transform::similarity(
            scale,
            model.a[1].atan2(model.a[0]),
            model.b[0],
            model.b[1],
        ),
        matches: pairs.len(),
        inliers: inliers.len(),
        occupied_cells: cells.count_ones() as usize,
    })
}

/// Matches `frame` against `reference` and fits a similarity over `dims`,
/// the reference level's size. `None` when there are fewer than two matches.
pub fn fit_between(reference: &Features, frame: &Features, dims: (u32, u32)) -> Option<SeedFit> {
    fit_similarity(&match_features(reference, frame), dims)
}

/// [`fit_between`]'s fit, only when it passes the inlier and coverage gates.
pub fn seed_between(reference: &Features, frame: &Features, dims: (u32, u32)) -> Option<SeedFit> {
    fit_between(reference, frame, dims).filter(SeedFit::accepted)
}

/// Synthetic scenes for this module's tests and the solver's.
#[cfg(test)]
pub(crate) mod test_scene {
    use crate::align::Transform;

    pub(crate) struct Rng(pub u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        pub(crate) fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// `t` applied to `p`, `t` a similarity.
    pub(crate) fn apply(t: &Transform, p: [f64; 2]) -> [f64; 2] {
        let (s, r, tx, ty) = t.similarity_parts();
        let (sin, cos) = r.sin_cos();
        [
            s * (cos * p[0] - sin * p[1]) + tx,
            s * (sin * p[0] + cos * p[1]) + ty,
        ]
    }

    /// One Gaussian blob of 3 to 12 px sigma per 750 px on a grey field,
    /// positioned by `t` (a similarity, reference to frame) and scaled with
    /// it, so a warped copy is the same scene seen through `t`. `contrast`
    /// 1.0 is +-20000 on the `u16` scale.
    pub(crate) fn blob_scene(width: u32, height: u32, t: &Transform, contrast: f64) -> Vec<f32> {
        let mut rng = Rng(0xB10B);
        let (scale, _, _, _) = t.similarity_parts();
        let mut data = vec![32768.0f32; (width * height) as usize];
        for _ in 0..width * height / 750 {
            let p = [
                rng.unit() * f64::from(width + 40) - 20.0,
                rng.unit() * f64::from(height + 40) - 20.0,
            ];
            let sigma = (3.0 + rng.unit() * 9.0) * scale;
            let amp = (rng.unit() - 0.5) * 40000.0 * contrast;
            let c = apply(t, p);
            let reach = (3.0 * sigma).ceil() as i64;
            for y in (c[1] as i64 - reach).max(0)..(c[1] as i64 + reach + 1).min(i64::from(height))
            {
                for x in
                    (c[0] as i64 - reach).max(0)..(c[0] as i64 + reach + 1).min(i64::from(width))
                {
                    let d2 = (x as f64 - c[0]).powi(2) + (y as f64 - c[1]).powi(2);
                    data[(y as u32 * width + x as u32) as usize] +=
                        (amp * (-d2 / (2.0 * sigma * sigma)).exp()) as f32;
                }
            }
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use super::test_scene::{Rng, apply, blob_scene};
    use super::*;

    const W: u32 = 1031;
    const H: u32 = 777;

    fn truth() -> Transform {
        Transform::translation(5.5, -3.25).compose(&Transform::similarity(
            1.02,
            3.0f64.to_radians(),
            0.0,
            0.0,
        ))
    }

    /// `n` reference points over the `W x H` frame, `inlier_share` of them
    /// mapped by `t` (within `noise` px), the rest to unrelated frame points.
    fn field(n: usize, inlier_share: f64, noise: f64, t: &Transform) -> Vec<Correspondence> {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        (0..n)
            .map(|i| {
                let p = [rng.unit() * f64::from(W - 1), rng.unit() * f64::from(H - 1)];
                let q = if (i as f64) < inlier_share * n as f64 {
                    let q = apply(t, p);
                    [
                        q[0] + (rng.unit() - 0.5) * noise,
                        q[1] + (rng.unit() - 0.5) * noise,
                    ]
                } else {
                    [rng.unit() * f64::from(W - 1), rng.unit() * f64::from(H - 1)]
                };
                (p, q)
            })
            .collect()
    }

    #[test]
    fn ransac_recovers_a_known_similarity_under_sixty_percent_outliers() {
        let t = truth();
        let pairs = field(300, 0.4, 0.2, &t);
        let fit = fit_similarity(&pairs, (W, H)).expect("a fit");
        assert!(fit.inliers >= 110, "{} inliers of 120", fit.inliers);
        assert!(fit.accepted());
        for p in [
            [0.0, 0.0],
            [1030.0, 0.0],
            [515.0, 388.0],
            [0.0, 776.0],
            [1030.0, 776.0],
        ] {
            let (got, want) = (apply(&fit.transform, p), apply(&t, p));
            assert!(
                (got[0] - want[0]).abs() < 0.1 && (got[1] - want[1]).abs() < 0.1,
                "{p:?}: {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn correspondences_confined_to_one_cell_are_rejected() {
        let t = truth();
        let pairs: Vec<Correspondence> = field(400, 1.0, 0.0, &t)
            .into_iter()
            .filter(|(p, _)| p[0] < 250.0 && p[1] < 190.0)
            .collect();
        assert!(
            pairs.len() >= 20,
            "{} points in the corner cell",
            pairs.len()
        );
        let fit = fit_similarity(&pairs, (W, H)).expect("a fit");
        assert_eq!(fit.occupied_cells, 1);
        assert!(fit.inliers >= 20);
        assert!(!fit.accepted());
    }

    #[test]
    fn too_few_inliers_are_rejected_and_enough_are_accepted() {
        let t = truth();
        let spread = |n: usize| -> Vec<Correspondence> {
            let mut pairs = Vec::new();
            for i in 0..n {
                let cell = i % 16;
                let p = [
                    (cell % 4) as f64 * 250.0 + 40.0 + (i / 16) as f64 * 17.0,
                    (cell / 4) as f64 * 190.0 + 40.0 + (i / 16) as f64 * 11.0,
                ];
                pairs.push((p, apply(&t, p)));
            }
            pairs
        };
        let eleven = fit_similarity(&spread(11), (W, H)).expect("a fit");
        assert_eq!((eleven.inliers, eleven.occupied_cells), (11, 11));
        assert!(!eleven.accepted());
        let twelve = fit_similarity(&spread(12), (W, H)).expect("a fit");
        assert_eq!((twelve.inliers, twelve.occupied_cells), (12, 12));
        assert!(twelve.accepted());
    }

    #[test]
    fn a_fit_explaining_a_small_share_of_the_matches_is_rejected() {
        let t = truth();
        // 45 inliers over the whole frame among 300 matches: enough of them
        // and spread out, but 15 % of the matches.
        let few = fit_similarity(&field(300, 0.15, 0.2, &t), (W, H)).expect("a fit");
        assert!(few.inliers >= 40 && few.occupied_cells >= 6, "{few:?}");
        assert!(!few.accepted(), "{few:?}");
        let many = fit_similarity(&field(300, 0.4, 0.2, &t), (W, H)).expect("a fit");
        assert!(many.accepted(), "{many:?}");
    }

    /// `inliers` exact matches inside the top-left 500x190 px (two grid
    /// cells) and `outliers` matches from there to unrelated frame points.
    fn clustered(inliers: usize, outliers: usize, t: &Transform) -> Vec<Correspondence> {
        let mut pairs: Vec<Correspondence> = field(600, 1.0, 0.0, t)
            .into_iter()
            .filter(|(p, _)| p[0] < 500.0 && p[1] < 190.0)
            .take(inliers)
            .collect();
        assert_eq!(pairs.len(), inliers);
        let mut rng = Rng(0xC1D5);
        for _ in 0..outliers {
            pairs.push((
                [rng.unit() * 500.0, rng.unit() * 190.0],
                [rng.unit() * f64::from(W - 1), rng.unit() * f64::from(H - 1)],
            ));
        }
        pairs
    }

    #[test]
    fn a_clustered_fit_with_a_high_inlier_share_is_accepted() {
        let t = truth();
        let fit = fit_similarity(&clustered(45, 5, &t), (W, H)).expect("a fit");
        assert!((2..6).contains(&fit.occupied_cells), "{fit:?}");
        assert!(fit.inliers >= 45, "{fit:?}");
        assert!(fit.accepted(), "{fit:?}");
    }

    #[test]
    fn a_clustered_fit_with_a_moderate_inlier_share_is_refused() {
        let t = truth();
        // Half the matches: over MIN_INLIER_SHARE, under the clustered share.
        let fit = fit_similarity(&clustered(30, 30, &t), (W, H)).expect("a fit");
        assert!((2..6).contains(&fit.occupied_cells), "{fit:?}");
        assert!(fit.inliers >= 30, "{fit:?}");
        assert!(!fit.accepted(), "{fit:?}");
    }

    #[test]
    fn a_clustered_fit_under_the_inlier_floor_is_refused() {
        let t = truth();
        let fit = fit_similarity(&clustered(11, 0, &t), (W, H)).expect("a fit");
        assert!((2..6).contains(&fit.occupied_cells), "{fit:?}");
        assert_eq!(fit.inliers, 11);
        assert!(!fit.accepted(), "{fit:?}");
    }

    const SW: u32 = 400;
    const SH: u32 = 300;

    fn blob_plane(t: &Transform, contrast: f64) -> Plane {
        Plane::from_f32_vec(blob_scene(SW, SH, t, contrast), SW, SH, 1).unwrap()
    }

    #[test]
    fn a_low_contrast_scene_gets_keypoints_the_default_threshold_misses() {
        let plane = blob_plane(&Transform::identity(), 0.01);
        let default = akaze::Akaze::default().extract_from_gray_float_image(&gray_image(&plane));
        assert_eq!(default.0.len(), 0, "the 0.001 default finds nothing");
        let features = Features::detect(&plane).unwrap();
        assert!(features.len() >= 20, "{} keypoints", features.len());
    }

    #[test]
    fn a_warped_scene_is_seeded_with_its_warp() {
        let warp = Transform::translation(12.3, -7.1).compose(&Transform::similarity(
            1.03,
            4.0f64.to_radians(),
            0.0,
            0.0,
        ));
        let reference = Features::detect(&blob_plane(&Transform::identity(), 1.0)).unwrap();
        let frame = Features::detect(&blob_plane(&warp, 1.0)).unwrap();
        let seed = seed_between(&reference, &frame, (SW, SH)).expect("a seed");
        assert!(seed.accepted() && seed.inliers >= 20, "{seed:?}");
        for p in [
            [0.0, 0.0],
            [399.0, 0.0],
            [200.0, 150.0],
            [0.0, 299.0],
            [399.0, 299.0],
        ] {
            let (got, want) = (apply(&seed.transform, p), apply(&warp, p));
            assert!(
                (got[0] - want[0]).abs() < 0.5 && (got[1] - want[1]).abs() < 0.5,
                "{p:?}: {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn a_periodic_pair_and_a_flat_pair_yield_no_seed() {
        let grating = |shift: f64| {
            let data = (0..SW * SH)
                .map(|i| {
                    let (x, y) = (f64::from(i % SW) - shift, f64::from(i / SW));
                    let tau = std::f64::consts::TAU;
                    (32768.0 + 15000.0 * (tau * x / 20.0).sin() * (tau * y / 20.0).sin()) as f32
                })
                .collect();
            Plane::from_f32_vec(data, SW, SH, 1).unwrap()
        };
        let a = Features::detect(&grating(0.0)).unwrap();
        let b = Features::detect(&grating(7.0)).unwrap();
        assert!(a.len() >= 20 && b.len() >= 20, "{} / {}", a.len(), b.len());
        assert!(seed_between(&a, &b, (SW, SH)).is_none(), "grating");

        let flat = Plane::from_f32_vec(vec![30000.0; (SW * SH) as usize], SW, SH, 1).unwrap();
        let f = Features::detect(&flat).unwrap();
        assert!(f.is_empty());
        assert!(seed_between(&f, &f, (SW, SH)).is_none(), "flat");
    }

    #[test]
    fn a_small_or_multichannel_plane_is_refused() {
        let small = Plane::from_f32_vec(vec![0.0; 30 * 300], 30, 300, 1).unwrap();
        assert_eq!(
            Features::detect(&small).err(),
            Some(FeatureSeedError::TooSmall {
                width: 30,
                height: 300
            })
        );
        let rgb = Plane::from_f32_vec(vec![0.0; 100 * 100 * 3], 100, 100, 3).unwrap();
        assert_eq!(
            Features::detect(&rgb).err(),
            Some(FeatureSeedError::NotLuma {
                samples_per_pixel: 3
            })
        );
    }
}
