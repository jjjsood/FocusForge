// SPDX-License-Identifier: GPL-3.0-or-later

//! `align_stack_with`'s anchor arm run in parallel, checked against the same
//! arm run serially and against an uncached per-frame loop over the same
//! input, with the serial run's per-frame read counts bounded through a
//! counting reader -- the behavioural counterpart of `align_parallel_memory.rs`'s
//! memory test.
//!
//! The fixture-driven tests lower `max-iterations` from the shipped 400 to 40:
//! the ordering property needs no more, and some frames then diverge. No test
//! asserts a literal worker count -- `available_parallelism` is the host's --
//! only that the parallel arm engaged (`worker_limit > 1`) and matches the
//! serial arm exactly.

mod common;

use common::align::{
    CountingReader, ESCALATION_READS, FramesReader, PATCH_COUNT, XorShift32, frame_3_tile,
    lowered_max_iterations, open_fixture, open_stack_context, reference_reads, streamed_band_reads,
    uncached_outcomes,
};

use focusforge_core::algorithm::AlignOutcome;
use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, FrameAlignment, PyramidalEcc, Transform, align_stack_with,
};
use focusforge_core::params::{ParamValue, StackContext};
use focusforge_core::tile::{HostBudget, Rect};

fn assert_frames_match_outcomes(frames: &[FrameAlignment], outcomes: &[AlignOutcome]) {
    assert_eq!(frames.len(), outcomes.len());
    for (frame, outcome) in frames.iter().zip(outcomes) {
        assert_eq!(
            frame.transform, outcome.transform,
            "frame {}: transform must be byte-identical to the uncached loop",
            frame.frame
        );
        assert_eq!(
            frame.status, outcome.status,
            "frame {}: status must match the uncached loop",
            frame.frame
        );
        assert_eq!(
            frame.iterations, outcome.iterations,
            "frame {}: iterations must match the uncached loop",
            frame.frame
        );
        assert_eq!(
            frame.residual, outcome.residual,
            "frame {}: residual must match the uncached loop",
            frame.frame
        );
    }
}

/// One serial run through a counting reader, two parallel runs and an uncached
/// per-frame loop over the same anchored input agree bit for bit; the cached
/// reference frame is read once however many registrations use it.
#[test]
fn anchored_runs_match_serial_parallel_and_uncached() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let cfg = lowered_max_iterations(&stack_ctx);
    let reader = CountingReader::new(&stack, stack_ctx.frame_count);

    let serial = align_stack_with(
        &PyramidalEcc,
        stack_ctx,
        &reader,
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

    let bands = streamed_band_reads(&stack_ctx);
    let reference = serial.alignment.reference;
    // Several fixture frames diverge and escalate, yet the reference gains one
    // read, not one per escalation.
    assert_eq!(
        reader.reads_for(reference),
        reference_reads(&stack_ctx) + ESCALATION_READS,
        "the cached reference frame must be read once (bands, level-0 window, patch \
         windows) plus its one shared escalation read, not once per registration"
    );
    for i in (0..stack_ctx.frame_count).filter(|&i| i != reference) {
        // Bands always; a frame that reaches level 0 adds its window, one
        // that converges there adds at most one window per patch, and one that
        // diverges adds its own whole-frame escalation read.
        let reads = reader.reads_for(i);
        let most = bands + 1 + PATCH_COUNT + ESCALATION_READS;
        assert!(
            (bands..=most).contains(&reads),
            "non-reference frame {i} made {reads} reads, expected {bands} to {most}"
        );
    }

    let parallel_runs: Vec<_> = (0..2)
        .map(|n| {
            align_stack_with(
                &PyramidalEcc,
                stack_ctx,
                &stack,
                &cfg,
                FailurePolicy::Strict,
                &AlignDriverOptions::default(),
            )
            .unwrap_or_else(|err| panic!("parallel run {n} completes: {err}"))
        })
        .collect();
    for parallel in &parallel_runs {
        assert!(
            parallel.worker_limit > 1,
            "the default budget must exercise the parallel arm on this 8-frame fixture: {}",
            parallel.worker_limit
        );
        assert_eq!(serial.alignment.reference, parallel.alignment.reference);
        assert_eq!(serial.alignment.policy, parallel.alignment.policy);
        assert_eq!(
            serial.alignment.frames, parallel.alignment.frames,
            "a parallel anchored run must recover transforms byte-identical to the serial one, \
             and two parallel runs must agree -- every FrameAlignment field, including \
             status/iterations/residual/fallback"
        );
    }

    let uncached = uncached_outcomes(stack_ctx, &stack, reference, &cfg);
    assert_frames_match_outcomes(&serial.alignment.frames, &uncached);
}

#[test]
fn chain_mode_stays_serial() {
    let (stack, stack_ctx) = open_stack_context("mantis-64");
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

/// The frames are a centred 128 px crop of fixture frame 3: frame 1 is the same
/// crop of the frame shifted, frame 2 is noise that cannot register against
/// frame 0.
const CROP_SIZE: u32 = 128;

#[test]
fn parallel_anchor_failure_policy_is_unchanged() {
    let (frame, full_width, full_height, spp) = frame_3_tile();
    let crop = Rect {
        x: (full_width - CROP_SIZE) / 2,
        y: (full_height - CROP_SIZE) / 2,
        width: CROP_SIZE,
        height: CROP_SIZE,
    };
    let tile = open_fixture()
        .read_tile(3, crop)
        .expect("reading frame 3's crop");

    let shift = Transform::translation(8.0, 3.0);
    let warped = warp_from_tile(&frame, (full_width, full_height), crop, &shift)
        .expect("small translation stays well inside the frame");
    let (width, height) = (CROP_SIZE, CROP_SIZE);

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

        for (arm, run) in [("serial", &serial), ("parallel", &parallel)] {
            let alignment = &run.alignment;
            assert_eq!(
                alignment.frames.len(),
                3,
                "{arm}, policy {policy:?}: every frame carries an outcome"
            );
            assert_eq!(
                alignment.failure_count(),
                1,
                "{arm}, policy {policy:?}: only the noise frame must fail: {:?}",
                alignment.frames
            );
            let failed = alignment.failed().next().expect("one failure");
            assert_eq!(failed.frame, 2, "{arm}, policy {policy:?}");
            match policy {
                // Strict keeps the solver's own best-rho transform.
                FailurePolicy::Strict => assert!(!failed.fallback, "{arm}"),
                FailurePolicy::IdentityFallback => {
                    assert!(failed.fallback, "{arm}");
                    assert_eq!(failed.transform, Transform::identity(), "{arm}");
                }
            }
        }

        assert_eq!(
            serial.alignment.frames, parallel.alignment.frames,
            "policy {policy:?}: strict must keep the same best-rho transform and lax must \
             substitute the same identity fallback under both arms"
        );
    }
}
