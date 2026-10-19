// SPDX-License-Identifier: GPL-3.0-or-later

//! Accounting regressions on the bounded global timing state. Its counters
//! are process-wide and every test here asserts exact deltas, so the tests
//! take one lock and run one at a time.

use std::sync::{Mutex, MutexGuard, PoisonError};

mod common;

use focusforge_core::algorithm::{AlignAlgorithm, AlignContext};
use focusforge_core::align::{
    DivergenceReason, EccMode, EccParams, EccResult, EccStatus, PyramidalEcc, Transform,
    solve_level,
};
use focusforge_core::fuse::Plane;
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::Rect;
use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Allocation operations and bytes, and copy operations, one call recorded.
#[derive(Debug, PartialEq)]
struct Recorded {
    allocation_operations: u64,
    allocation_bytes: u64,
    copy_operations: u64,
}

fn record<T>(call: impl FnOnce() -> T) -> (T, Recorded) {
    enable_trace_accounting();
    let before = timing_snapshot().expect("enabled accounting");
    let value = call();
    let after = timing_snapshot().expect("enabled accounting");
    let recorded = Recorded {
        allocation_operations: after.allocation_operations - before.allocation_operations,
        allocation_bytes: after.allocation_bytes - before.allocation_bytes,
        copy_operations: after.copy_operations - before.copy_operations,
    };
    (value, recorded)
}

#[test]
fn singular_solve_counts_each_precompute_vector() {
    let _serial = serial();
    let samples = vec![1.0_f32; 8 * 8];
    let template = Plane::from_f32(&samples, 8, 8, 1).expect("valid template");
    let moving = Plane::from_f32(&samples, 8, 8, 1).expect("valid moving plane");

    let (result, recorded) = record(|| {
        solve_level(
            &template,
            Rect {
                x: 1,
                y: 1,
                width: 6,
                height: 6,
            },
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &EccParams {
                max_iterations: 1,
                epsilon_px: 0.01,
                min_valid_fraction: 0.5,
            },
        )
        .expect("valid solver inputs")
    });

    assert_eq!(
        result.status,
        EccStatus::Diverged(DivergenceReason::SingularHessian)
    );
    assert_eq!(
        recorded.allocation_operations, 2,
        "a singular template allocates only its values and its centred Jacobian"
    );
    assert_eq!(recorded.copy_operations, 0);
}

fn sinusoid(x: f64, y: f64) -> f32 {
    (2000.0
        + 700.0 * (x * 0.11 + 0.3).sin()
        + 500.0 * (y * 0.17 + 1.1).sin()
        + 400.0 * ((x + y) * 0.05).sin()
        + 300.0 * ((x - 2.0 * y) * 0.07).cos()) as f32
}

const WIDTH: u32 = 128;
const HEIGHT: u32 = 48;

/// `moving` is the sinusoid; `template` is the same surface read `(dx, dy)`
/// further along, so the transform that registers them is `translation(dx, dy)`.
fn sinusoid_pair(dx: f64, dy: f64) -> (Plane, Plane) {
    let plane = |shift_x: f64, shift_y: f64| {
        let data: Vec<f32> = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
            .map(|(x, y)| sinusoid(f64::from(x) + shift_x, f64::from(y) + shift_y))
            .collect();
        Plane::from_f32(&data, WIDTH, HEIGHT, 1).expect("plane matches its size")
    };
    (plane(dx, dy), plane(0.0, 0.0))
}

/// One solve capped at `max_iterations` that never converges early.
fn capped_solve(
    template: &Plane,
    moving: &Plane,
    region: Rect,
    initial: &Transform,
    max_iterations: u32,
) -> (EccResult, Recorded) {
    let (result, recorded) = record(|| {
        solve_level(
            template,
            region,
            moving,
            initial,
            EccMode::Translation,
            &EccParams {
                max_iterations,
                epsilon_px: 1e-12,
                min_valid_fraction: 0.5,
            },
        )
        .expect("valid solver inputs")
    });
    assert_eq!(result.status, EccStatus::HitCap, "{result:?}");
    assert_eq!(result.iterations, max_iterations);
    (result, recorded)
}

#[test]
fn full_overlap_allocations_do_not_grow_with_iterations() {
    let _serial = serial();
    let (template, moving) = sinusoid_pair(1.5, 0.75);
    let region = Rect {
        x: 8,
        y: 8,
        width: 96,
        height: 32,
    };

    let (short, few) = capped_solve(&template, &moving, region, &Transform::identity(), 3);
    let (long, many) = capped_solve(&template, &moving, region, &Transform::identity(), 6);

    assert_eq!(short.valid_fraction, 1.0);
    assert_eq!(long.valid_fraction, 1.0);
    assert_eq!(few, many);
    assert_eq!(many.copy_operations, 0, "no cached vector is cloned");
}

#[test]
fn partial_overlap_allocations_do_not_grow_with_iterations() {
    let _serial = serial();
    // The region's right columns read past the moving plane once the
    // transform approaches its answer, and the start is already close enough
    // for every iteration to run on a partial overlap.
    let (template, moving) = sinusoid_pair(16.0, 0.0);
    let region = Rect {
        x: 4,
        y: 4,
        width: 120,
        height: 30,
    };
    let start = Transform::translation(15.0, 0.0);

    let (short, few) = capped_solve(&template, &moving, region, &start, 3);
    let (long, many) = capped_solve(&template, &moving, region, &start, 6);

    assert!(short.valid_fraction < 1.0, "{short:?}");
    assert!(long.valid_fraction < 1.0, "{long:?}");
    assert_eq!(few, many);
    assert_eq!(many.copy_operations, 0);
}

/// Allocation operations of one `align_frame` of frame 0 against reference 1
/// of `fixtures/mantis-64` under `cfg`, with `prepared` attached.
fn align_allocations(
    stack: &TiffStack,
    cfg: &ParamValues,
    prepared: Option<&(dyn std::any::Any + Send + Sync)>,
) -> u64 {
    let (width, height) = stack.dimensions();
    let context = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let ctx = AlignContext::new(context, 1, stack).with_prepared(prepared);
    let (outcome, recorded) = record(|| PyramidalEcc.align_frame(&ctx, 0, cfg));
    outcome.expect("align_frame completes");
    recorded.allocation_operations
}

#[test]
fn a_matching_prepared_reference_spares_the_registration_its_preparation() {
    let _serial = serial();
    let stack = TiffStack::open(common::fixtures_dir().join("mantis-64"))
        .expect("opening fixtures/mantis-64");
    let (width, height) = stack.dimensions();
    let context = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), &context)
        .expect("defaults resolve cleanly");
    let mut cfg = params::resolved_values(&resolved);
    cfg.insert("pyramid-depth".to_string(), ParamValue::Int(1));
    cfg.insert("max-iterations".to_string(), ParamValue::Int(5));

    let prepare = |cfg: &ParamValues| {
        PyramidalEcc
            .prepare_run(&AlignContext::new(context, 1, &stack), cfg)
            .expect("prepare_run completes")
            .expect("pyramidal-ecc prepares its reference")
    };
    let matching = prepare(&cfg);
    let mut translation = cfg.clone();
    translation.insert(
        "mode".to_string(),
        ParamValue::Enum("translation".to_string()),
    );
    let other_mode = prepare(&translation);

    let unprepared = align_allocations(&stack, &cfg, None);
    let reused = align_allocations(&stack, &cfg, Some(&*matching));
    let ignored = align_allocations(&stack, &cfg, Some(&*other_mode));

    assert!(
        reused < unprepared,
        "a matching prepared value must save the reference pyramid and both levels' \
         templates: {reused} vs {unprepared}"
    );
    assert_eq!(
        ignored, unprepared,
        "a prepared value for another mode is not used"
    );
}
