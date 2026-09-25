// SPDX-License-Identifier: GPL-3.0-or-later

//! Drives the real 61-frame reference stack through `pyramidal-ecc`'s
//! shipped defaults under strict alignment.
//!
//! Both `#[ignore]`d tests read `FOCUSFORGE_REFERENCE_STACK` through
//! [`common::reference_stack_dir`] and skip with a neutral message when it
//! is unset or does not name a directory.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use focusforge_core::algorithm::{AlignContext, AlignStatus};
use focusforge_core::align::{
    AlignDriverOptions, AlignRun, EccLevelRecord, FailurePolicy, PyramidalEcc, align_stack_with,
};
use focusforge_core::io::{TiffStack, frame_paths};
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::registry;

/// Serializes this file's two ignored tests: both would otherwise hold the
/// 16 GB stack's pixels live at once, which the machine this runs on cannot
/// afford twice over.
static SERIAL: Mutex<()> = Mutex::new(());

/// `stack_dir`'s frames, `IMG_`-filtered so the checked-in stacked result
/// (present in the same directory, named without that substring) never
/// joins the set.
fn reference_frame_paths(stack_dir: &Path) -> Vec<PathBuf> {
    frame_paths(stack_dir)
        .expect("the reference stack directory must enumerate")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .collect()
}

/// `pyramidal-ecc`'s shipped defaults resolved against `ctx`.
fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let algo = registry::find_align("pyramidal-ecc").expect("pyramidal-ecc is registered");
    let resolved = params::resolve(algo.params(), &ParamValues::new(), ctx)
        .expect("shipped defaults must resolve cleanly");
    params::resolved_values(&resolved)
}

/// Runs `align_stack_with` over the reference stack exactly once per
/// process, cached so the traced per-level pass reuses the accounting
/// pass's own outcome instead of repeating the minutes-long full-stack
/// solve.
fn full_alignment(stack_dir: &Path) -> &'static (AlignRun, Vec<PathBuf>, Duration) {
    static RUN: OnceLock<(AlignRun, Vec<PathBuf>, Duration)> = OnceLock::new();
    RUN.get_or_init(|| {
        let paths = reference_frame_paths(stack_dir);
        let stack = TiffStack::open_files(&paths).expect("reference stack must open");
        let (width, height) = stack.dimensions();
        let spp = stack.header(0).samples_per_pixel;
        let stack_ctx = StackContext::new(stack.len(), width, height, spp);
        let cfg = resolved_defaults(&stack_ctx);
        let algo = registry::find_align("pyramidal-ecc").expect("pyramidal-ecc is registered");
        let start = Instant::now();
        let run = align_stack_with(
            algo,
            stack_ctx,
            &stack,
            &cfg,
            FailurePolicy::Strict,
            &AlignDriverOptions::default(),
        )
        .expect("align_stack_with must not error for a well-formed stack");
        (run, paths, start.elapsed())
    })
}

#[test]
#[allow(unsafe_code)]
fn reference_stack_dir_reads_the_environment_variable() {
    // SAFETY: this is the only test in this crate's suite that mutates
    // `FOCUSFORGE_REFERENCE_STACK` (grep confirms no other file writes it);
    // the two `#[ignore]`d tests below only read it through
    // `common::reference_stack_dir`, and `cargo test`'s default filtering
    // never runs an ignored and a non-ignored test in the same invocation.
    let previous = std::env::var_os("FOCUSFORGE_REFERENCE_STACK");

    unsafe { std::env::remove_var("FOCUSFORGE_REFERENCE_STACK") };
    assert_eq!(
        common::reference_stack_dir(),
        None,
        "unset must resolve to None"
    );

    let scratch = tempfile::TempDir::with_prefix("ff-reference-stack-dir-").unwrap();
    unsafe { std::env::set_var("FOCUSFORGE_REFERENCE_STACK", scratch.path()) };
    assert_eq!(
        common::reference_stack_dir().as_deref(),
        Some(scratch.path()),
        "an absolute directory must resolve to exactly that directory"
    );

    let workspace_fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .canonicalize()
        .unwrap();
    unsafe { std::env::set_var("FOCUSFORGE_REFERENCE_STACK", "fixtures") };
    assert_eq!(
        common::reference_stack_dir()
            .unwrap()
            .canonicalize()
            .unwrap(),
        workspace_fixtures,
        "a relative value must resolve against the workspace root"
    );

    let missing = scratch.path().join("does-not-exist");
    unsafe { std::env::set_var("FOCUSFORGE_REFERENCE_STACK", &missing) };
    let message = std::panic::catch_unwind(common::reference_stack_dir)
        .expect_err("a path that does not name a directory must panic")
        .downcast_ref::<String>()
        .cloned()
        .expect("panic payload must be a String");
    assert!(
        message.contains(&*missing.to_string_lossy()),
        "panic message {message:?} must name the offending path {}",
        missing.display()
    );

    match previous {
        Some(v) => unsafe { std::env::set_var("FOCUSFORGE_REFERENCE_STACK", v) },
        None => unsafe { std::env::remove_var("FOCUSFORGE_REFERENCE_STACK") },
    }
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn the_reference_stack_reports_a_per_frame_alignment_outcome() {
    let _guard = SERIAL.lock().unwrap();
    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };

    let (run, paths, elapsed) = full_alignment(&stack_dir);
    let alignment = &run.alignment;
    assert_eq!(
        paths.len(),
        61,
        "IMG_-filtered list must hold exactly the 61 source frames"
    );
    assert_eq!(alignment.frames.len(), 61);
    assert!(alignment.reference < 61);

    let mut converged = 0usize;
    let mut hit_cap = 0usize;
    let mut diverged = 0usize;
    for (i, frame) in alignment.frames.iter().enumerate() {
        assert_eq!(frame.frame, i, "frames must be recorded in frame order");
        let name = paths[i].file_name().unwrap().to_string_lossy();
        match frame.status {
            AlignStatus::Converged => {
                converged += 1;
                println!(
                    "frame={i} name={name} status=converged iterations={} residual={:.6}",
                    frame.iterations, frame.residual
                );
            }
            AlignStatus::HitCap => {
                hit_cap += 1;
                println!(
                    "frame={i} name={name} status=hit-cap iterations={} residual={:.6}",
                    frame.iterations, frame.residual
                );
            }
            AlignStatus::Diverged(reason) => {
                diverged += 1;
                println!(
                    "frame={i} name={name} status=diverged reason={reason:?} \
                     iterations={} residual={:.6}",
                    frame.iterations, frame.residual
                );
            }
            other => panic!("frame={i} name={name}: unrecognised align status {other:?}"),
        }
    }
    println!(
        "the_reference_stack_reports_a_per_frame_alignment_outcome: wall={elapsed:?} \
         converged={converged} hit_cap={hit_cap} diverged={diverged} reference={}",
        alignment.reference
    );
    assert_eq!(
        converged + hit_cap + diverged,
        61,
        "every frame must be accounted for"
    );
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn the_reference_stack_records_each_divergence_level() {
    let _guard = SERIAL.lock().unwrap();
    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };

    let (run, paths, _elapsed) = full_alignment(&stack_dir);
    let alignment = &run.alignment;
    let diverged: Vec<usize> = alignment.failed().map(|f| f.frame).collect();
    println!(
        "the_reference_stack_records_each_divergence_level: {} of 61 diverged",
        diverged.len()
    );
    if diverged.is_empty() {
        return;
    }

    let stack = TiffStack::open_files(paths).expect("reference stack must reopen");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let stack_ctx = StackContext::new(stack.len(), width, height, spp);
    let cfg = resolved_defaults(&stack_ctx);
    let ctx = AlignContext::new(stack_ctx, alignment.reference, &stack);

    for frame in diverged {
        let mut levels: Vec<EccLevelRecord> = Vec::new();
        let outcome = PyramidalEcc
            .align_frame_traced(&ctx, frame, &cfg, &mut levels)
            .expect("a frame the driver already scored must trace the same way");
        let name = paths[frame].file_name().unwrap().to_string_lossy();
        println!(
            "frame={frame} name={name} outcome.status={:?} outcome.iterations={}",
            outcome.status, outcome.iterations
        );
        for rec in &levels {
            println!(
                "  level={} status={:?} iterations={} final_step={:?} correlation={:.6}",
                rec.level,
                rec.result.status,
                rec.result.iterations,
                rec.result.final_step,
                rec.result.correlation
            );
        }
    }
}
