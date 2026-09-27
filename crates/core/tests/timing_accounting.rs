// SPDX-License-Identifier: GPL-3.0-or-later

//! Accounting regressions that need a process without sibling tests sharing
//! the bounded global timing state.

use focusforge_core::align::{
    DivergenceReason, EccMode, EccParams, EccStatus, Transform, solve_level,
};
use focusforge_core::fuse::Plane;
use focusforge_core::tile::Rect;
use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};

#[test]
fn singular_solve_counts_each_precompute_vector() {
    let samples = vec![1.0_f32; 8 * 8];
    let template = Plane::from_f32(&samples, 8, 8, 1).expect("valid template");
    let moving = Plane::from_f32(&samples, 8, 8, 1).expect("valid moving plane");

    enable_trace_accounting();
    let before = timing_snapshot().expect("enabled accounting");
    let result = solve_level(
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
    .expect("valid solver inputs");
    let after = timing_snapshot().expect("enabled accounting");

    assert_eq!(
        result.status,
        EccStatus::Diverged(DivergenceReason::SingularHessian)
    );
    assert_eq!(
        after
            .allocation_operations
            .checked_sub(before.allocation_operations),
        Some(4),
        "three precompute vectors and the centred Jacobian each allocate"
    );
}
