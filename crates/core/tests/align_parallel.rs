// SPDX-License-Identifier: GPL-3.0-or-later

//! `align_stack_with` run in parallel -- anchor registrations and chain hops --
//! checked against the same arm run serially and against an uncached per-frame loop over the same
//! input, with the serial run's per-frame read counts bounded through a
//! counting reader -- the behavioural counterpart of `align_parallel_memory.rs`'s
//! memory test.
//!
//! Apart from `output_is_identical_across_worker_counts_and_budgets`, the
//! fixture-driven tests lower `max-iterations` from the shipped 400 to 40: the
//! ordering property needs no more, and some frames then diverge. Those tests
//! assert no literal worker count -- `available_parallelism` is the host's --
//! only that the parallel arm engaged (`worker_limit > 1`) and matches the
//! serial arm exactly. The determinism test runs at the shipped defaults and
//! compares each anchor `worker_limit` with the host's parallelism.

mod common;

use common::align::{
    CountingReader, ESCALATION_READS, FramesReader, PATCH_COUNT, XorShift32, anchor_defaults,
    frame_3_tile, lowered_max_iterations, open_fixture, open_stack_context, reference_reads,
    resolved_defaults, stack_context, streamed_band_reads, uncached_outcomes,
};

use focusforge_core::algorithm::{AlignOutcome, TileReader};
use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, FrameAlignment, PyramidalEcc, Transform, align_stack_with,
    bytes_per_chain_hop, bytes_per_frame_build, bytes_per_registration, bytes_shared_reference,
};
use focusforge_core::params::{ParamValue, StackContext};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
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

/// A chain run at the default budget solves its hops on more than one worker and
/// returns the frames of the one-worker run.
#[test]
fn chain_hops_engage_the_pool_at_the_default_budget() {
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

    assert_eq!(one_byte.worker_limit, 1);
    assert_eq!(one_byte.peak_workers, 1);
    assert!(
        default_budget.worker_limit > 1,
        "the default budget must solve the 2 hops of a 3-frame chain in parallel: {}",
        default_budget.worker_limit
    );
    assert_eq!(
        one_byte.alignment.frames, default_budget.alignment.frames,
        "chain mode's result must not depend on the worker count"
    );
}

/// Chain and anchor runs at the shipped defaults over `fixtures/mantis-512`
/// return the same frames, reference and policy for every requested worker
/// count and every memory budget. The worker-count budgets are the driver's own
/// sum: the shared reference data and `N` registrations for anchor, one frame
/// build and `N` hops (each its own shared data, registration and level-0
/// windows) for chain, so each lands on `N` workers whatever the
/// per-registration figure.
#[test]
fn output_is_identical_across_worker_counts_and_budgets() {
    let stack = open_fixture();
    let stack_ctx = stack_context(&stack);
    let (width, _) = stack.dimensions();
    let per_worker = bytes_per_registration(stack_ctx, DEFAULT_TILE_SIZE)
        + stack.read_scratch_bytes(width, DEFAULT_TILE_SIZE);
    let sized_for = |workers: u64, anchor: bool| {
        HostBudget::explicit(if anchor {
            bytes_shared_reference(stack_ctx) + workers * per_worker
        } else {
            bytes_per_frame_build(stack_ctx)
                + workers
                    * (bytes_per_chain_hop(stack_ctx, DEFAULT_TILE_SIZE)
                        + stack.read_scratch_bytes(width, DEFAULT_TILE_SIZE))
        })
    };
    let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
    let registrations = stack_ctx.frame_count - 1;
    let host_derived = HostBudget::host_derived(&focusforge_core::host::snapshot());

    for (policy, cfg) in [
        ("chain", resolved_defaults(&stack_ctx)),
        ("anchor", anchor_defaults(&stack_ctx)),
    ] {
        let anchor = policy == "anchor";
        let mut cells: Vec<(String, HostBudget, Option<usize>)> = [1usize, 2, 4, 8]
            .iter()
            .map(|&n| {
                let expected = n.min(parallelism).min(registrations);
                (
                    format!("{n} workers"),
                    sized_for(n as u64, anchor),
                    Some(expected),
                )
            })
            .collect();
        cells.push((
            "1-byte budget".to_string(),
            HostBudget::explicit(1),
            Some(1),
        ));
        cells.push((
            "2400 MiB budget".to_string(),
            HostBudget::explicit(2400 * 1024 * 1024),
            None,
        ));
        cells.push(("host-derived budget".to_string(), host_derived, None));

        let mut baseline = None;
        for (cell, budget, expected_workers) in cells {
            let run = align_stack_with(
                &PyramidalEcc,
                stack_ctx,
                &stack,
                &cfg,
                FailurePolicy::Strict,
                &AlignDriverOptions {
                    budget,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|err| panic!("{policy}, {cell}: the run completes: {err}"));
            if let Some(expected) = expected_workers {
                assert_eq!(
                    run.worker_limit, expected,
                    "{policy}, {cell}: worker limit (host parallelism {parallelism}, \
                     {registrations} registrations)"
                );
            }
            let first = baseline.get_or_insert_with(|| run.alignment.clone());
            assert_eq!(
                run.alignment.reference, first.reference,
                "{policy}, {cell}: reference frame against the 1-worker run"
            );
            assert_eq!(
                run.alignment.policy, first.policy,
                "{policy}, {cell}: policy against the 1-worker run"
            );
            assert_eq!(
                run.alignment.frames, first.frames,
                "{policy}, {cell}: every FrameAlignment field against the 1-worker run"
            );
        }
    }
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

/// Five cropped frames around reference 2, the noise frame 3 inside the upper
/// leg so frame 4 composes onto a prefix that went through a failed hop; chain
/// runs give the same frames at one worker and at two, and under
/// `IdentityFallback` a failed hop is identity composed onto its prefix.
#[test]
fn parallel_chain_failure_policy_matches_serial() {
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
    let shifted = |dx: f64, dy: f64| {
        warp_from_tile(
            &frame,
            (full_width, full_height),
            crop,
            &Transform::translation(dx, dy),
        )
        .expect("small translation stays well inside the frame")
        .tile
        .samples
    };
    let mut rng = XorShift32(0xC0FFEE);
    let noise: Vec<u16> = (0..tile.samples.len())
        .map(|_| (rng.next() % 65536) as u16)
        .collect();

    let (width, height) = (CROP_SIZE, CROP_SIZE);
    let stack_ctx = StackContext::new(5, width, height, spp);
    let mut cfg = lowered_max_iterations(&stack_ctx);
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("chain".to_string()),
    );
    let make_reader = || FramesReader {
        width,
        height,
        spp,
        frames: vec![
            shifted(-8.0, 3.0),
            shifted(-4.0, 1.0),
            tile.samples.clone(),
            noise.clone(),
            shifted(8.0, 3.0),
        ],
    };
    let hop = bytes_per_chain_hop(stack_ctx, DEFAULT_TILE_SIZE)
        + make_reader().read_scratch_bytes(width, DEFAULT_TILE_SIZE);

    for policy in [FailurePolicy::Strict, FailurePolicy::IdentityFallback] {
        let run = |budget: u64| {
            align_stack_with(
                &PyramidalEcc,
                stack_ctx,
                &make_reader(),
                &cfg,
                policy,
                &AlignDriverOptions {
                    budget: HostBudget::explicit(budget),
                    ..Default::default()
                },
            )
            .expect("a chain run completes even with a failing hop")
        };
        let serial = run(1);
        let parallel = run(bytes_per_frame_build(stack_ctx) + 2 * hop);
        assert_eq!(serial.worker_limit, 1);
        assert!(
            parallel.worker_limit > 1,
            "two hops of budget must solve in parallel: {}",
            parallel.worker_limit
        );
        assert_eq!(
            serial.alignment.frames, parallel.alignment.frames,
            "policy {policy:?}: every FrameAlignment field at one and at two workers"
        );

        let frames = &parallel.alignment.frames;
        assert!(
            frames[3].fallback == (policy == FailurePolicy::IdentityFallback)
                && matches!(
                    frames[3].status,
                    focusforge_core::algorithm::AlignStatus::Diverged(_)
                ),
            "policy {policy:?}: the noise frame's hop must diverge: {:?}",
            frames[3]
        );
        if policy == FailurePolicy::IdentityFallback {
            // A fallback hop is identity composed onto its predecessor's transform.
            for failed in frames.iter().filter(|f| f.fallback) {
                let predecessor = if failed.frame > 2 {
                    failed.frame - 1
                } else {
                    failed.frame + 1
                };
                assert_eq!(
                    failed.transform, frames[predecessor].transform,
                    "frame {} must compose its identity hop onto frame {predecessor}'s prefix",
                    failed.frame
                );
            }
        }
    }
}
