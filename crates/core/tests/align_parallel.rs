// SPDX-License-Identifier: GPL-3.0-or-later

//! `align_stack_with`'s anchor arm run in parallel, checked against the same
//! arm run serially over the same input -- the behavioural counterpart of
//! `align_parallel_memory.rs`'s memory test.
//!
//! Every fixture-driven test here overrides `max-iterations` down to 40 from
//! the shipped 400: an 8-frame fixture align at the shipped default costs
//! seconds this ordering property does not need, and a lowered cap makes some
//! frames diverge, which the failure-policy test wants anyway. No test asserts
//! a literal worker count from a fixture run -- `available_parallelism` is the
//! host's -- only that the parallel arm actually engaged (`worker_limit > 1`)
//! and that its result matches the serial arm's exactly.

mod common;

use common::align::{
    FramesReader, XorShift32, frame_3_tile, full_rect, lowered_max_iterations, open_fixture,
};

use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, PyramidalEcc, Transform, align_stack_with,
};
use focusforge_core::params::{ParamValue, StackContext};
use focusforge_core::tile::HostBudget;

#[test]
fn parallel_anchor_matches_serial_transforms() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let cfg = lowered_max_iterations(&stack_ctx);

    let serial = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions {
            budget: HostBudget::explicit(1),
            ..Default::default()
        },
    )
    .expect("serial run completes");
    assert_eq!(serial.worker_limit, 1);
    assert_eq!(serial.peak_workers, 1);

    let parallel = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("parallel run completes");
    assert!(
        parallel.worker_limit > 1,
        "the default budget must exercise the parallel arm on this 8-frame fixture: {}",
        parallel.worker_limit
    );

    assert_eq!(serial.alignment.reference, parallel.alignment.reference);
    assert_eq!(serial.alignment.policy, parallel.alignment.policy);
    assert_eq!(
        serial.alignment.frames, parallel.alignment.frames,
        "a parallel anchored run must recover transforms byte-identical to the serial one -- \
         every FrameAlignment field, including status/iterations/residual/fallback"
    );
}

#[test]
fn parallel_anchor_is_reproducible() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let cfg = lowered_max_iterations(&stack_ctx);

    let first = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("first parallel run completes");
    let second = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("second parallel run completes");

    assert!(first.worker_limit > 1, "must exercise the parallel arm");
    assert_eq!(
        first.alignment.frames, second.alignment.frames,
        "two parallel runs over the same input must yield identical transforms -- rules out a \
         scheduling-order dependence the serial comparison alone could miss"
    );
}

#[test]
fn chain_mode_stays_serial() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let mut cfg = lowered_max_iterations(&stack_ctx);
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("chain".to_string()),
    );

    let one_byte = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions {
            budget: HostBudget::explicit(1),
            ..Default::default()
        },
    )
    .expect("chain run completes under a 1-byte budget");
    let default_budget = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("chain run completes under the default budget");

    for run in [&one_byte, &default_budget] {
        assert_eq!(
            run.worker_limit, 1,
            "chain mode is serial by construction, at every budget"
        );
        assert_eq!(run.peak_workers, 1);
    }
    assert_eq!(
        one_byte.alignment.frames, default_budget.alignment.frames,
        "chain mode's result must not depend on the (unused) worker budget"
    );
}

#[test]
fn parallel_anchor_failure_policy_is_unchanged() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);

    let shift = Transform::translation(8.0, 3.0);
    let warped = warp_from_tile(&tile, (width, height), rect, &shift)
        .expect("small translation stays well inside the frame");

    let mut rng = XorShift32(0xC0FFEE);
    let noise: Vec<u16> = (0..tile.samples.len())
        .map(|_| (rng.next() % 65536) as u16)
        .collect();

    let stack_ctx = StackContext::new(3, width, height, spp);
    let mut cfg = lowered_max_iterations(&stack_ctx);
    cfg.insert(
        "reference-frame".to_string(),
        ParamValue::Enum("first".to_string()),
    );

    let make_reader = || FramesReader {
        width,
        height,
        spp,
        frames: vec![
            tile.samples.clone(),
            warped.tile.samples.clone(),
            noise.clone(),
        ],
    };

    for policy in [FailurePolicy::Strict, FailurePolicy::IdentityFallback] {
        let serial = align_stack_with(
            &PyramidalEcc,
            stack_ctx,
            &make_reader(),
            &cfg,
            policy,
            &AlignDriverOptions {
                budget: HostBudget::explicit(1),
                ..Default::default()
            },
        )
        .expect("serial run completes even with a failing frame");
        let parallel = align_stack_with(
            &PyramidalEcc,
            stack_ctx,
            &make_reader(),
            &cfg,
            policy,
            &AlignDriverOptions::default(),
        )
        .expect("parallel run completes even with a failing frame");

        assert_eq!(serial.worker_limit, 1);
        assert!(
            parallel.worker_limit > 1,
            "2 registrations and the default budget must still exercise the parallel arm: {}",
            parallel.worker_limit
        );

        assert_eq!(
            serial.alignment.failure_count(),
            parallel.alignment.failure_count(),
            "policy {policy:?}"
        );
        assert_eq!(
            serial.alignment.frames, parallel.alignment.frames,
            "policy {policy:?}: strict must keep the same best-rho transform and lax must \
             substitute the same identity fallback under both arms"
        );
    }
}
