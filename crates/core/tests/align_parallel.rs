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

use std::path::PathBuf;

use focusforge_core::algorithm::{AlgorithmError, AlignAlgorithm, TileReader};
use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, PyramidalEcc, Transform, align_stack_with,
};
use focusforge_core::io::{FrameError, TiffError, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::{HostBudget, Rect, Tile};

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

fn frame_3_tile() -> (Tile, u32, u32, u32) {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(FRAME_3).samples_per_pixel;
    let rect = full_rect(width, height);
    let tile = stack
        .read_tile(FRAME_3, rect)
        .expect("reading frame 3's tile");
    (tile, width, height, spp)
}

/// Fully resolved defaults for `PyramidalEcc` against `ctx`, `max-iterations`
/// then lowered to 40 by every caller here (see the module docs), under the
/// anchor policy these tests exercise.
fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), ctx)
        .expect("defaults resolve cleanly");
    let mut cfg = params::resolved_values(&resolved);
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );
    cfg
}

fn lowered_max_iterations(ctx: &StackContext) -> ParamValues {
    let mut cfg = resolved_defaults(ctx);
    cfg.insert("max-iterations".to_string(), ParamValue::Int(40));
    cfg
}

/// A deterministic xorshift32 PRNG, seeded with a fixed constant, local to
/// this test file.
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

/// A synthetic in-memory [`TileReader`] over an arbitrary number of
/// same-sized frames.
struct FramesReader {
    width: u32,
    height: u32,
    spp: u32,
    frames: Vec<Vec<u16>>,
}

impl TileReader for FramesReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let Some(src) = self.frames.get(frame) else {
            return Err(AlgorithmError::FrameOutOfRange {
                frame,
                frame_count: self.frames.len(),
            });
        };
        let expected = rect.width as usize * rect.height as usize * self.spp as usize;
        if out.len() != expected {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::BufferSize {
                    expected,
                    got: out.len(),
                },
            )));
        }
        let in_bounds = rect
            .x
            .checked_add(rect.width)
            .is_some_and(|xw| xw <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.height);
        if !in_bounds {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::OutOfBounds {
                    path: PathBuf::from("<memory>"),
                    requested: rect,
                    frame: (self.width, self.height),
                },
            )));
        }
        let stride = self.width as usize * self.spp as usize;
        let row_len = rect.width as usize * self.spp as usize;
        let mut off = 0;
        for r in rect.y..(rect.y + rect.height) {
            let row_start = r as usize * stride + rect.x as usize * self.spp as usize;
            out[off..off + row_len].copy_from_slice(&src[row_start..row_start + row_len]);
            off += row_len;
        }
        Ok(())
    }
}

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
