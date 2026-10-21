// SPDX-License-Identifier: GPL-3.0-or-later

//! The feature-seed escalation over the checked-in 512 px fixture. A process
//! of its own, so the global seed counters see this run only.

mod common;

use focusforge_core::algorithm::{AlignAlgorithm, AlignStatus};
use focusforge_core::align::{FailurePolicy, PyramidalEcc, align_stack};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};

/// Frames 00, 01, 06 and 07 diverge from an identity start and are the only
/// ones to run the detector; frames 02 to 05 converge and never do. None of
/// the four diverged frames has features that pass the inlier and coverage
/// gates, so each keeps its identity-start outcome.
#[test]
fn only_the_diverged_fixture_frames_run_the_detector() {
    enable_trace_accounting();
    let stack = TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512");
    let (width, height) = stack.dimensions();
    let ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), &ctx)
        .expect("defaults resolve cleanly");
    let mut cfg = params::resolved_values(&resolved);
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );

    let result = align_stack(
        &PyramidalEcc,
        ctx,
        &stack,
        &cfg,
        FailurePolicy::IdentityFallback,
    )
    .expect("the run completes even with diverging frames");

    let diverged = result
        .frames
        .iter()
        .filter(|f| matches!(f.status, AlignStatus::Diverged(_)))
        .count();
    let snapshot = timing_snapshot().expect("accounting is enabled");
    assert_eq!(diverged, 4);
    assert_eq!(snapshot.feature_seed_attempts, 4, "{snapshot:?}");
    assert_eq!(snapshot.feature_seeds_accepted, 0, "{snapshot:?}");
}
