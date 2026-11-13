// SPDX-License-Identifier: GPL-3.0-or-later

//! Records the GPU per-level ECC step series of the stack named by
//! `FOCUSFORGE_REFERENCE_STACK` into `crates/core/tests/data/`.
//!
//! The data is only valid when recorded with the solver's stall exit disabled:
//! that exit truncates the series the replay judges.

use std::fmt::Write as _;
use std::path::PathBuf;

use focusforge_core::algorithm::AlignAlgorithm;
use focusforge_core::algorithm::AlignContext;
use focusforge_core::align::EccStatus;
use focusforge_core::align::{EccLevelRecord, PyramidalEcc};
use focusforge_core::io::{FrameReader, TiffStack, frame_paths};
use focusforge_core::params::{ParamValue, resolve, resolved_values};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::harness::gpu_backend;

use crate::full_frame_render_identity::core_common;

/// Levels with fewer steps than this cannot reach the stall predicate.
const MIN_LISTED_STEPS: usize = 10;
/// Levels above 0 stop at this multiple of `epsilon-px`.
const COARSE_EPSILON_FACTOR: f64 = 100.0;

fn status_name(status: EccStatus) -> &'static str {
    match status {
        EccStatus::Converged => "converged",
        EccStatus::HitCap => "hit-cap",
        EccStatus::Stalled => "stalled",
        EccStatus::Diverged(_) => "diverged",
    }
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn record_chain_step_series() {
    let Some(stack_dir) = core_common::reference_stack_dir() else {
        return;
    };
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let gpu = gpu_backend().expect("gpu backend");
    let paths: Vec<PathBuf> = frame_paths(&stack_dir)
        .expect("the reference stack directory must enumerate")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .collect();
    let stack = TiffStack::open_files(&paths).expect("the reference stack must open");
    let stack_ctx = stack.stack_context();
    let cfg = resolved_values(
        &resolve(PyramidalEcc.params(), &Default::default(), &stack_ctx)
            .expect("default parameters"),
    );
    let epsilon = cfg
        .get("epsilon-px")
        .and_then(ParamValue::as_f64)
        .expect("epsilon-px is a default parameter");

    let anchor = (stack.len() - 1) / 2;
    let mut solved = 0usize;
    let mut lines = String::new();
    for hop in (0..stack.len()).filter(|&i| i != anchor) {
        let neighbour = if hop > anchor { hop - 1 } else { hop + 1 };
        let ctx = AlignContext::new(stack_ctx, neighbour, &stack).with_align_backend(Some(gpu));
        let mut levels: Vec<EccLevelRecord> = Vec::new();
        PyramidalEcc
            .align_frame_traced(&ctx, hop, &cfg, &mut levels)
            .expect("the hop aligns");
        solved += levels.len();
        for record in levels.iter().filter(|r| r.trace.len() >= MIN_LISTED_STEPS) {
            let level_epsilon = if record.level == 0 {
                epsilon
            } else {
                epsilon * COARSE_EPSILON_FACTOR
            };
            write!(
                lines,
                "{hop} {} {} {level_epsilon}",
                record.level,
                status_name(record.result.status)
            )
            .expect("writing to a String");
            for iteration in &record.trace {
                write!(lines, " {}", iteration.step).expect("writing to a String");
            }
            lines.push('\n');
        }
    }

    let name = stack_dir
        .file_name()
        .and_then(|n| n.to_str())
        .expect("the stack directory has a name")
        .to_lowercase();
    let listed = lines.lines().count();
    let header = format!(
        "# GPU per-level ECC step series, chain hops of {name} ({} frames).\n\
         # {solved} levels solved, {listed} listed (at least {MIN_LISTED_STEPS} steps), \
         default parameters, epsilon {epsilon}. Line: <hop> <level> <status> <epsilon> <step>...\n",
        stack.len()
    );
    let out = core_common::workspace_root()
        .join("crates/core/tests/data")
        .join(format!("gpu-ecc-step-series-{name}.txt"));
    std::fs::write(&out, header + &lines).expect("the data file must be writable");
    eprintln!(
        "{solved} levels solved, {listed} listed, written to {}",
        out.display()
    );
}
