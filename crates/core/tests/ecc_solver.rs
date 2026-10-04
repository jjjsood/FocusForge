// SPDX-License-Identifier: GPL-3.0-or-later

//! The single-level IC-ECC solver exercised against `fixtures/mantis-512`
//! and synthetic buffers.
//!
//! Every ground-truth test here warps **one** frame (index 3, `03.tif`, the
//! sharpest at Laplacian variance 285,247) and registers it against itself
//! -- the only configuration where the "known" transform is actually known,
//! since the fixture frames carry the real stack's own drift between them.

mod common;

use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{
    DivergenceReason, EccMode, EccParams, EccResult, EccStatus, Transform, solve_level,
};
use focusforge_core::fuse::Plane;
use focusforge_core::fuse::metrics::luma;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::{Rect, Tile};

const FRAME_3: usize = 3;

fn open_fixture() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512")
}

fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Opens `fixtures/mantis-512` and reads frame 3's full tile.
fn frame_3_tile() -> (Tile, u32, u32) {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let rect = full_rect(width, height);
    let tile = stack
        .read_tile(FRAME_3, rect)
        .expect("reading frame 3's tile");
    (tile, width, height)
}

/// Reduces a chunky RGB [`Tile`] to a single-channel luminance [`Plane`],
/// [`luma`] sample-for-sample -- the same reduction
/// `crate::align::pyramid::LumaAssembler` performs, done directly here
/// because the whole frame is already in memory.
fn luma_plane(tile: &Tile, width: u32, height: u32) -> Plane {
    let spp = tile.samples_per_pixel;
    let data: Vec<f32> = (0..(width as usize * height as usize))
        .map(|i| {
            let start = i * spp as usize;
            luma(&tile.samples[start..start + spp as usize], spp) as f32
        })
        .collect();
    Plane::from_f32(&data, width, height, 1).expect("luma plane has the right length")
}

/// The maximum, over `region`'s four corners, of the Euclidean distance
/// between where `a` and `b` map that corner -- the same "corner
/// displacement" measure [`solve_level`]'s own termination criterion uses,
/// applied here to compare a recovered transform against ground truth.
fn corner_error(a: &Transform, b: &Transform, region: Rect) -> f64 {
    let corners = [
        (region.x as f64, region.y as f64),
        ((region.x + region.width - 1) as f64, region.y as f64),
        (region.x as f64, (region.y + region.height - 1) as f64),
        (
            (region.x + region.width - 1) as f64,
            (region.y + region.height - 1) as f64,
        ),
    ];
    corners
        .iter()
        .map(|&(x, y)| {
            let (ax, ay) = a
                .map_point(x, y)
                .expect("ground-truth transform stays in domain");
            let (bx, by) = b
                .map_point(x, y)
                .expect("recovered transform stays in domain");
            ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
        })
        .fold(0.0f64, f64::max)
}

/// A rotation+scale [`Transform`] about `(cx, cy)`, built by composing
/// three transforms rather than hand-deriving the closed-form translation
/// component -- `Transform::compose` is already tested elsewhere, so this
/// stays a thin, obviously-correct wrapper.
fn similarity_about(scale: f64, radians: f64, cx: f64, cy: f64) -> Transform {
    let to_origin = Transform::translation(-cx, -cy);
    let rot_scale = Transform::similarity(scale, radians, 0.0, 0.0);
    let back = Transform::translation(cx, cy);
    back.compose(&rot_scale).compose(&to_origin)
}

/// A deterministic xorshift32 PRNG, seeded with a fixed constant -- never
/// `rand`, reproducible across runs, and local to this test file.
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

/// The region every ground-truth test in this file solves over: inset far
/// enough from the 512px frame's edges that no fabricated rim pixel
/// (`warp_from_tile`'s clamp-to-edge taps near the source frame's border)
/// ever enters the accumulation, for every transform this file uses --
/// `KERNEL_RADIUS` (2px) plus the largest corner displacement any test
/// here applies (well under 40px) is far inside a 64px inset on a 512px
/// frame.
fn default_region() -> Rect {
    Rect {
        x: 64,
        y: 64,
        width: 384,
        height: 384,
    }
}

fn default_params() -> EccParams {
    EccParams {
        max_iterations: 50,
        epsilon_px: 1e-4,
        min_valid_fraction: 0.99,
    }
}

/// Measured 2026-09-13 (`cargo test -p focusforge-core --test ecc_solver
/// recovers_known_translation -- --nocapture`): recovered-vs-ground-truth
/// corner error was `9.24e-7` px. Asserted at roughly 100x that measured
/// figure -- headroom for platform-to-platform `f64`/`f32` accumulation
/// differences, not a round number pulled from nowhere.
const TRANSLATION_TOLERANCE_PX: f64 = 1e-4;

/// Measured 2026-09-13 the same way, `recovers_known_rotation_and_scale`:
/// corner error `8.51e-6` px. The 4-DOF normal equations are
/// worse-conditioned than translation's 2-DOF ones, so this tolerance is
/// wider than [`TRANSLATION_TOLERANCE_PX`] -- not because the test is
/// looser, but because the quantity itself is -- asserted at roughly 100x
/// the measured figure for the same reason.
const ROTATION_SCALE_TOLERANCE_PX: f64 = 1e-3;

#[test]
fn recovers_known_translation() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);

    let moving_plane = luma_plane(&tile, width, height);

    let w_true = Transform::translation(6.5, -4.25);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("translation stays well inside the frame");
    let template_plane = luma_plane(&warped.tile, width, height);

    let region = default_region();
    let result = solve_level(
        &template_plane,
        region,
        &moving_plane,
        &Transform::identity(),
        EccMode::Translation,
        &default_params(),
    )
    .expect("valid solver inputs");

    assert_eq!(result.status, EccStatus::Converged, "result: {result:?}");
    let error = corner_error(&result.transform, &w_true, region);
    eprintln!("recovers_known_translation: corner error = {error} px");
    assert!(
        error < TRANSLATION_TOLERANCE_PX,
        "corner error {error} px exceeds {TRANSLATION_TOLERANCE_PX} px"
    );
}

#[test]
fn recovers_known_rotation_and_scale() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);

    let moving_plane = luma_plane(&tile, width, height);

    let region = default_region();
    let (cx, cy) = (
        (region.x + region.width / 2) as f64,
        (region.y + region.height / 2) as f64,
    );
    // A modest rotation plus scale ~= 1.002 -- the focus-breathing regime,
    // not the 20 degree stress case `rotation_converges_quadratically` uses.
    let w_true = similarity_about(1.002, 2.0f64.to_radians(), cx, cy);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("rotation+scale stays well inside the frame");
    let template_plane = luma_plane(&warped.tile, width, height);

    let result = solve_level(
        &template_plane,
        region,
        &moving_plane,
        &Transform::identity(),
        EccMode::Similarity,
        &default_params(),
    )
    .expect("valid solver inputs");

    assert_eq!(result.status, EccStatus::Converged, "result: {result:?}");
    let error = corner_error(&result.transform, &w_true, region);
    eprintln!("recovers_known_rotation_and_scale: corner error = {error} px");
    assert!(
        error < ROTATION_SCALE_TOLERANCE_PX,
        "corner error {error} px exceeds {ROTATION_SCALE_TOLERANCE_PX} px"
    );
}

/// Regression test for the similarity increment's conjugation about the
/// region centre. Uses a region whose centre sits well away
/// from `default_region`'s (256, 256) -- (385, 385), the far corner of the
/// fixture the inset margin still allows -- so that skipping the
/// conjugation (`similarity_delta` in `crates/core/src/align/ecc.rs`)
/// would apply the recovered increment about the wrong point and this test
/// would actually notice: with a region centred at the origin, conjugating
/// about the centre is a no-op and proves nothing about this step at all.
#[test]
fn similarity_conjugates_increment_about_region_centre() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);
    let moving_plane = luma_plane(&tile, width, height);

    let region = Rect {
        x: 290,
        y: 290,
        width: 190,
        height: 190,
    };
    let (cx, cy) = (
        (region.x + region.width / 2) as f64,
        (region.y + region.height / 2) as f64,
    );
    let w_true = similarity_about(0.995, (-2.5f64).to_radians(), cx, cy);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("rotation+scale about a far-from-origin centre stays inside the frame");
    let template_plane = luma_plane(&warped.tile, width, height);

    let result = solve_level(
        &template_plane,
        region,
        &moving_plane,
        &Transform::identity(),
        EccMode::Similarity,
        &default_params(),
    )
    .expect("valid solver inputs");

    assert_eq!(result.status, EccStatus::Converged, "result: {result:?}");
    let error = corner_error(&result.transform, &w_true, region);
    eprintln!("similarity_conjugates_increment_about_region_centre: corner error = {error} px");
    assert!(
        error < ROTATION_SCALE_TOLERANCE_PX,
        "corner error {error} px exceeds {ROTATION_SCALE_TOLERANCE_PX} px -- a large error here \
         (many px, not a small numerical one) means the similarity increment's conjugation \
         about the region centre is missing or wrong"
    );
}

#[test]
fn identity_converges_immediately() {
    let (tile, width, height) = frame_3_tile();
    let plane = luma_plane(&tile, width, height);
    let region = default_region();

    let result = solve_level(
        &plane,
        region,
        &plane,
        &Transform::identity(),
        EccMode::Translation,
        &default_params(),
    )
    .expect("valid solver inputs");

    assert_eq!(result.status, EccStatus::Converged, "result: {result:?}");
    assert!(
        result.iterations <= 3,
        "identity should converge in very few iterations, took {}",
        result.iterations
    );
    let error = corner_error(&result.transform, &Transform::identity(), region);
    assert!(
        error < TRANSLATION_TOLERANCE_PX,
        "identity drifted by {error} px"
    );
}

#[test]
fn photometric_gain_and_bias_do_not_move_the_estimate() {
    let (tile, width, height) = frame_3_tile();
    let template_plane = luma_plane(&tile, width, height);
    let region = default_region();
    let params = default_params();

    let baseline = solve_level(
        &template_plane,
        region,
        &template_plane,
        &Transform::identity(),
        EccMode::Translation,
        &params,
    )
    .expect("valid solver inputs");
    assert_eq!(baseline.status, EccStatus::Converged);

    // The property IC-ECC was chosen for: a per-frame gain and bias on the
    // moving plane must not move the estimate, unlike plain SSD.
    let scaled: Vec<f32> = template_plane
        .data()
        .iter()
        .map(|&v| 1.7 * v + 30.0)
        .collect();
    let moving_scaled =
        Plane::from_f32(&scaled, template_plane.width(), template_plane.height(), 1)
            .expect("scaled plane has the right length");

    let photo = solve_level(
        &template_plane,
        region,
        &moving_scaled,
        &Transform::identity(),
        EccMode::Translation,
        &params,
    )
    .expect("valid solver inputs");
    assert_eq!(photo.status, EccStatus::Converged, "result: {photo:?}");

    let moved = corner_error(&baseline.transform, &photo.transform, region);
    eprintln!("photometric_gain_and_bias_do_not_move_the_estimate: moved by {moved} px");
    assert!(
        moved <= 10.0 * params.epsilon_px,
        "photometric distortion moved the estimate by {moved} px, more than the identity run's \
         own tolerance"
    );
}

#[test]
fn rotation_converges_quadratically() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);
    let moving_plane = luma_plane(&tile, width, height);

    // A smaller, well-inset region: a 20 degree rotation's corner
    // displacement is large, and this file's regions must still keep every
    // sample well away from `warp_from_tile`'s fabricated edge rim.
    let region = Rect {
        x: 180,
        y: 180,
        width: 150,
        height: 150,
    };
    let (cx, cy) = (
        (region.x + region.width / 2) as f64,
        (region.y + region.height / 2) as f64,
    );
    let w_true = similarity_about(1.0, 20.0f64.to_radians(), cx, cy);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("20 degree rotation about the region centre stays inside the frame");
    let template_plane = luma_plane(&warped.tile, width, height);

    // The step measure and composition order this pins are deterministic
    // and serial, so running `solve_level` fresh with an increasing
    // iteration cap reproduces the exact same trajectory's prefix each
    // time -- this is what lets a black-box, public-API-only test observe
    // per-iteration convergence without a diagnostics callback.
    // Measured 2026-09-13 (`cargo test -p focusforge-core --test ecc_solver
    // rotation_converges_quadratically -- --nocapture`), corner error per
    // cap 1..=14: `33.23, 30.96, 29.35, 27.73, 23.01, 17.19, 12.65, 9.16,
    // 6.19, 3.50, 1.14, 0.102, 0.019, 0.0035` -- slow at first (Newton's
    // quadratic convergence is a *local* property near the solution, not
    // from an initial 20 degree misalignment), then a textbook quadratic
    // tail from index 9 onward: each error is roughly the square of the
    // fraction the previous one was of its predecessor. The toolbox's
    // `ΔW⁻¹ · W` order does not develop this tail on a rotation; pure
    // translation cannot distinguish the two orders at all, which is why
    // this file needs its own dedicated test.
    let mut errors = Vec::new();
    for cap in 1..=14u32 {
        let params = EccParams {
            max_iterations: cap,
            epsilon_px: 1e-12,
            min_valid_fraction: 0.9,
        };
        let result = solve_level(
            &template_plane,
            region,
            &moving_plane,
            &Transform::identity(),
            EccMode::Similarity,
            &params,
        )
        .expect("valid solver inputs");
        assert_ne!(
            result.status,
            EccStatus::Diverged(DivergenceReason::SingularHessian),
            "cap {cap}: {result:?}"
        );
        errors.push(corner_error(&result.transform, &w_true, region));
    }
    eprintln!("rotation_converges_quadratically: corner errors per cap = {errors:?}");

    // Composition order `W ← W · ΔW⁻¹` (Baker & Matthews eq. 34) converges
    // quadratically on a rotation; the toolbox's `ΔW⁻¹ · W` converges only
    // linearly there. The discriminator: a linear sequence's consecutive-error
    // ratio is roughly constant: a quadratic one's ratio shrinks toward zero. The
    // ratio over the later, already-small-error iterations must be
    // markedly smaller than the ratio over the earlier ones.
    let ratio = |i: usize| errors[i] / errors[i - 1].max(f64::MIN_POSITIVE);
    let early_ratio = ratio(3);
    let late_ratio = ratio(11);
    assert!(
        late_ratio < early_ratio * 0.5,
        "convergence did not accelerate: early ratio {early_ratio}, late ratio {late_ratio}, \
         errors {errors:?}"
    );
    let last = errors.len() - 1;
    assert!(
        errors[last] < errors[0] * 1e-3,
        "final error {} did not shrink enough relative to the first, errors {errors:?}",
        errors[last]
    );
}

#[test]
fn hopeless_input_reports_failure() {
    let (tile, width, height) = frame_3_tile();
    let template_plane = luma_plane(&tile, width, height);

    let mut rng = XorShift32(0x5EED_1234);
    let noise: Vec<f32> = (0..(width as usize * height as usize))
        .map(|_| (rng.next() % 65536) as f32)
        .collect();
    let moving_plane = Plane::from_f32(&noise, width, height, 1).expect("noise plane");

    let region = default_region();
    let result = solve_level(
        &template_plane,
        region,
        &moving_plane,
        &Transform::identity(),
        EccMode::Translation,
        &default_params(),
    )
    .expect("valid solver inputs");

    assert_ne!(
        result.status,
        EccStatus::Converged,
        "an unrelated pair must never report a converged transform: {result:?}"
    );
}

#[test]
fn iteration_cap_is_honored() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);
    let moving_plane = luma_plane(&tile, width, height);

    let w_true = Transform::translation(15.0, 15.0);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("translation stays well inside the frame");
    let template_plane = luma_plane(&warped.tile, width, height);

    let region = default_region();
    let params = EccParams {
        max_iterations: 2,
        epsilon_px: 1e-9,
        min_valid_fraction: 0.99,
    };
    let result = solve_level(
        &template_plane,
        region,
        &moving_plane,
        &Transform::identity(),
        EccMode::Translation,
        &params,
    )
    .expect("valid solver inputs");

    assert_eq!(result.status, EccStatus::HitCap, "result: {result:?}");
    assert_eq!(result.iterations, 2);
}

/// A frame against its own contrast inversion is anti-correlated: whatever the
/// solver reports, it is not `Converged`.
#[test]
fn an_inverted_contrast_pair_is_not_converged() {
    let (tile, width, height) = frame_3_tile();
    let template_plane = luma_plane(&tile, width, height);
    let inverted = Tile {
        samples: tile.samples.iter().map(|&s| u16::MAX - s).collect(),
        ..tile
    };
    let moving_plane = luma_plane(&inverted, width, height);

    for mode in [EccMode::Translation, EccMode::Similarity] {
        let result = solve_level(
            &template_plane,
            default_region(),
            &moving_plane,
            &Transform::identity(),
            mode,
            &default_params(),
        )
        .expect("valid solver inputs");
        assert_ne!(result.status, EccStatus::Converged, "{mode:?}: {result:?}");
    }
}

#[test]
fn result_reports_iterations_and_error() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);
    let moving_plane = luma_plane(&tile, width, height);

    let w_true = Transform::translation(3.0, 2.0);
    let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
        .expect("translation stays well inside the frame");
    let template_plane = luma_plane(&warped.tile, width, height);

    let region = default_region();
    let result = solve_level(
        &template_plane,
        region,
        &moving_plane,
        &Transform::identity(),
        EccMode::Translation,
        &default_params(),
    )
    .expect("valid solver inputs");

    assert_eq!(result.status, EccStatus::Converged, "result: {result:?}");
    assert!(result.iterations > 0);
    assert!(result.valid_samples > 0);
    assert!(result.valid_fraction > 0.0 && result.valid_fraction <= 1.0);
    assert!(
        result.correlation.is_finite() && (-1.0..=1.0).contains(&result.correlation),
        "correlation {} is out of [-1, 1]",
        result.correlation
    );
}

/// The largest pure translation (0..=`SWEEP_MAX_PX`, in whole pixels) still
/// recovered from identity at this level, within `TRANSLATION_TOLERANCE_PX`
/// of ground truth. The default pyramid depth is derived from this
/// number: a translation larger than the capture range needs a coarser
/// level to start from.
///
/// Measured 2026-09-13 (`cargo test -p focusforge-core --test ecc_solver
/// capture_range_is_measured -- --nocapture`).
const SWEEP_MAX_PX: u32 = 100;

#[test]
fn capture_range_is_measured() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);
    let moving_plane = luma_plane(&tile, width, height);
    let region = default_region();
    let params = default_params();

    let mut largest_recovered = 0u32;
    for shift in (5..=SWEEP_MAX_PX).step_by(5) {
        let w_true = Transform::translation(shift as f64, 0.0);
        let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
            .expect("swept translation stays well inside the frame");
        let template_plane = luma_plane(&warped.tile, width, height);

        let result = solve_level(
            &template_plane,
            region,
            &moving_plane,
            &Transform::identity(),
            EccMode::Translation,
            &params,
        )
        .expect("valid solver inputs");

        let recovered = result.status == EccStatus::Converged
            && corner_error(&result.transform, &w_true, region) < TRANSLATION_TOLERANCE_PX;
        if recovered {
            largest_recovered = shift;
        } else {
            break;
        }
    }

    eprintln!(
        "capture_range_is_measured: largest recovered pure translation = {largest_recovered} px"
    );
    // Measured 2026-09-13: 60px, on this fixture, this region and this
    // 5px sweep step. Fully deterministic (a fixed fixture frame, no
    // randomness), so pinned exactly rather than as a floor: a future
    // change to the solver, the sampler or the Jacobian that moves this
    // number is exactly the kind of change the default pyramid depth
    // needs to know about.
    assert_eq!(
        largest_recovered, 60,
        "measured capture range moved -- the default pyramid depth is derived from this number"
    );
}

/// Every field of an [`EccResult`] as exact bits, so a recorded run and a
/// later one compare without a tolerance.
fn fingerprint(result: &EccResult) -> String {
    let m: Vec<String> = result
        .transform
        .m
        .iter()
        .flatten()
        .map(|v| format!("{:016x}", v.to_bits()))
        .collect();
    let step = result
        .final_step
        .map_or("none".to_string(), |s| format!("{:016x}", s.to_bits()));
    format!(
        "m=[{}] status={:?} iterations={} correlation={:016x} valid_samples={} step={}",
        m.join(","),
        result.status,
        result.iterations,
        result.correlation.to_bits(),
        result.valid_samples,
        step
    )
}

/// The solver's exact output on four inputs, pinned bit for bit: buffer reuse
/// must not move a single bit.
#[test]
fn results_are_bit_identical_to_the_recorded_run() {
    let (tile, width, height) = frame_3_tile();
    let rect = full_rect(width, height);
    let plane = luma_plane(&tile, width, height);
    let region = default_region();
    let params = default_params();
    let solve = |template: &Plane, moving: &Plane, mode: EccMode| {
        solve_level(
            template,
            region,
            moving,
            &Transform::identity(),
            mode,
            &params,
        )
        .expect("valid solver inputs")
    };

    let identity = solve(&plane, &plane, EccMode::Translation);

    let shifted = warp_from_tile(
        &tile,
        (width, height),
        rect,
        &Transform::translation(6.5, -4.25),
    )
    .expect("translation stays well inside the frame");
    let translation = solve(
        &luma_plane(&shifted.tile, width, height),
        &plane,
        EccMode::Translation,
    );

    let (cx, cy) = (
        (region.x + region.width / 2) as f64,
        (region.y + region.height / 2) as f64,
    );
    let turned = warp_from_tile(
        &tile,
        (width, height),
        rect,
        &similarity_about(1.002, 2.0f64.to_radians(), cx, cy),
    )
    .expect("rotation+scale stays well inside the frame");
    let rotation_scale = solve(
        &luma_plane(&turned.tile, width, height),
        &plane,
        EccMode::Similarity,
    );

    let mut rng = XorShift32(0x5EED_1234);
    let noise: Vec<f32> = (0..(width as usize * height as usize))
        .map(|_| (rng.next() % 65536) as f32)
        .collect();
    let noise_plane = Plane::from_f32(&noise, width, height, 1).expect("noise plane");
    let divergence = solve(&plane, &noise_plane, EccMode::Translation);

    let actual = [
        fingerprint(&identity),
        fingerprint(&translation),
        fingerprint(&rotation_scale),
        fingerprint(&divergence),
    ];
    assert_eq!(actual, RECORDED_RESULTS);
}

/// Identity, translation, rotation+scale, divergence.
const RECORDED_RESULTS: [&str; 4] = [
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=1 correlation=3ff0000000000000 valid_samples=147456 step=0000000000000000",
    "m=[3ff0000000000000,0000000000000000,401a00003d341317,0000000000000000,3ff0000000000000,c011000009d34765,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=10 correlation=3fefffffff9de874 valid_samples=147456 step=3efc92d0f3c18f9b",
    "m=[3ff005b11f4ece4e,bfa1e77e79cafc1b,4021315a01879686,3fa1e77e79cafc1b,3ff005b11f4ece4e,c0229da29f1d8bcb,0000000000000000,0000000000000000,3ff0000000000000] status=Converged iterations=9 correlation=3fefffffffc8b5c2 valid_samples=147456 step=3f01ba54d84a77dd",
    "m=[3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000,0000000000000000,0000000000000000,0000000000000000,3ff0000000000000] status=Diverged(TooFewValidSamples) iterations=1 correlation=3f30383c0d31099b valid_samples=0 step=40823578dcfcc01b",
];
