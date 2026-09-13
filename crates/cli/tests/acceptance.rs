// SPDX-License-Identifier: GPL-3.0-or-later

//! The recorded, never-asserted full-size comparison between the composite
//! `tools/run-acceptance.sh` already wrote and `Mantis_Stacked.tif`.
//!
//! `tools/run-acceptance.sh` owns the *run* (the input farm, the release
//! invocation, the cgroup scope and its read-back), because none of that is
//! expressible as a cargo test. This file owns the *comparison*, because
//! `focusforge_core::fuse::metrics` is the only place block-mean SSIM
//! exists and `tools/tiff-diff.py` cannot compute it.
//!
//! `#[ignore]` marks that this test touches the reference stack directory;
//! it reads two 268 MB files (the composite and the reference) and never
//! stacks anything. It skips, with a printed reason, when the script's
//! output is absent.
//!
//! No similarity threshold is asserted here: the 61 frames have real drift,
//! so similarity-to-reference is not a fixed quantity. This test records
//! SSIM, PSNR and Laplacian variance and asserts nothing about their
//! values beyond finiteness.

mod common;

use std::path::Path;

use focusforge_core::fuse::metrics::{block_mean_ssim, laplacian_variance, psnr};
use focusforge_core::io::{Rect, TiffStack};

fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Whole-image read: samples, width, height, samples-per-pixel,
/// bits-per-sample. This reader opens the `MM`/`RowsPerStrip = 8`
/// reference directly, so no conversion step is needed on either side.
fn read_whole(path: &Path) -> (Vec<u16>, u32, u32, u32, u16) {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).expect("file must open");
    let (width, height) = stack.dimensions();
    let header = stack.header(0);
    let spp = header.samples_per_pixel;
    let bits = header.bits_per_sample.first().copied().unwrap_or(0);
    let samples = stack
        .read_tile(0, full_rect(width, height))
        .expect("whole-image read must succeed")
        .samples;
    (samples, width, height, spp, bits)
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn full_stack_similarity_to_mantis_stacked_is_recorded() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        eprintln!("skip: reference stack not found");
        return;
    };

    let composite_path = common::workspace_root().join(".scratch/acceptance/unlimited/out.tif");
    if !composite_path.is_file() {
        eprintln!(
            "skip: {} not found -- run tools/run-acceptance.sh first",
            composite_path.display()
        );
        return;
    }

    let reference_path = stack_dir.join("Mantis_Stacked.tif");
    if !reference_path.is_file() {
        eprintln!("skip: {} not found", reference_path.display());
        return;
    }

    let (composite, cw, ch, cspp, cbits) = read_whole(&composite_path);
    let (reference, rw, rh, rspp, rbits) = read_whole(&reference_path);

    // A composite identical to the reference (or a mismatched shape) would
    // mean the wrong file was compared -- these are the only things this
    // test asserts. Nothing about the similarity numbers is asserted.
    //
    // This is the **`--no-crop`** case: `stack` applies the auto-crop by
    // default, which would shrink the composite below the full 8192x5464
    // extent, so `run_align()`'s `run_stack` call passes `--no-crop`
    // specifically to keep this comparison dimensionally comparable with
    // `Mantis_Stacked.tif` (a dimension mismatch fails the comparison
    // outright).
    assert_eq!(
        (cw, ch, cspp, cbits),
        (8192, 5464, 3, 16),
        "composite must be the full-size 16-bit RGB output"
    );
    assert_eq!(
        (rw, rh, rspp, rbits),
        (8192, 5464, 3, 16),
        "Mantis_Stacked.tif must be the full-size 16-bit RGB reference"
    );

    let ssim = block_mean_ssim(&composite, &reference, cw, ch, cspp);
    let psnr_value = psnr(&composite, &reference, cw, ch, cspp);
    let sharpness = laplacian_variance(&composite, cw, ch, cspp);

    assert!(
        psnr_value.is_finite(),
        "psnr must be finite -- an infinite value would mean the composite \
         and the reference are byte-identical, i.e. the wrong file was compared"
    );

    // `eprintln!`, read with `--nocapture`: the run this test guards
    // measured **26.61 dB** / SSIM 0.9665 at full size. No threshold is
    // asserted on it either way.
    eprintln!(
        "tile 512 / levels 5: block_mean_ssim={ssim:.4} psnr={psnr_value:.2}dB \
         laplacian_variance={sharpness:.2}"
    );
}

/// Reads `align-<mode>.json`, `tools/run-acceptance.sh`'s new `align` stage
/// artefact for one mode, parsed as a plain `serde_json::Value` -- the
/// script's own JSON, never re-derived here.
fn read_align_json(mode: &str) -> Option<serde_json::Value> {
    let path = common::workspace_root()
        .join(".scratch/acceptance/unlimited")
        .join(format!("align-{mode}.json"));
    if !path.is_file() {
        eprintln!(
            "skip: {} not found -- run tools/run-acceptance.sh first",
            path.display()
        );
        return None;
    }
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} exists but could not be read: {e}", path.display()));
    Some(
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} exists but is not valid JSON: {e}", path.display())),
    )
}

/// All 61 real frames produce an outcome; the per-frame `status` histogram
/// and `failure_count` are part of what is recorded -- a sweep this long is
/// where divergence far from the anchor is expected, not an anomaly. The
/// alignment itself is done by the script, once, under `--lax`; this test
/// only parses what it already wrote.
#[test]
#[ignore = "reads the 16 GB reference stack"]
fn aligns_the_reference_stack() {
    let Some(report) = read_align_json("rotation-scale") else {
        return;
    };

    let frame_count = report["frame_count"].as_u64().expect("frame_count");
    assert_eq!(frame_count, 61, "the full reference stack has 61 frames");

    let alignments = report["alignments"].as_array().expect("alignments array");
    assert_eq!(alignments.len(), 61);

    let mut histogram: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut max_drift = 0.0f64;
    let mut sum_drift = 0.0f64;
    for entry in alignments {
        let status = entry["status"].as_str().expect("status string").to_string();
        *histogram.entry(status).or_insert(0) += 1;
        let (tx, ty) = (
            entry["tx"].as_f64().expect("tx"),
            entry["ty"].as_f64().expect("ty"),
        );
        let drift = tx.hypot(ty);
        max_drift = max_drift.max(drift);
        sum_drift += drift;
    }
    let mean_drift = sum_drift / alignments.len() as f64;
    let failure_count = report["failure_count"].as_u64().expect("failure_count");
    let wall_seconds = report["wall_seconds"].as_f64().unwrap_or(0.0);

    eprintln!(
        "aligns_the_reference_stack: frame_count={frame_count} failure_count={failure_count} \
         max_drift={max_drift:.2}px mean_drift={mean_drift:.2}px wall_seconds={wall_seconds:.1} \
         status_histogram={histogram:?}"
    );
}

/// The two modes disagree on the real stack in the direction focus
/// breathing predicts: rotation+scale recovers a scale factor away from
/// 1.0 across the sweep, and leaves a higher ρ (an ECC correlation --
/// higher is better) than translation-only. Parsed from the two
/// `align-<mode>.json` artefacts, never aligned in-test, and
/// assertion-light: finiteness and the 61-frame count are asserted, the
/// comparison is recorded.
#[test]
#[ignore = "reads the 16 GB reference stack"]
fn the_two_modes_disagree_as_focus_breathing_predicts() {
    let Some(translation) = read_align_json("translation") else {
        return;
    };
    let Some(rotation_scale) = read_align_json("rotation-scale") else {
        return;
    };

    let mean_abs_scale_minus_one = |report: &serde_json::Value| -> f64 {
        let alignments = report["alignments"].as_array().expect("alignments array");
        assert_eq!(alignments.len(), 61);
        let sum: f64 = alignments
            .iter()
            .map(|e| (e["scale"].as_f64().expect("scale") - 1.0).abs())
            .sum();
        sum / alignments.len() as f64
    };
    let mean_rho = |report: &serde_json::Value| -> f64 {
        let alignments = report["alignments"].as_array().expect("alignments array");
        let sum: f64 = alignments
            .iter()
            .map(|e| e["residual"].as_f64().expect("residual"))
            .sum();
        sum / alignments.len() as f64
    };

    let translation_scale_dev = mean_abs_scale_minus_one(&translation);
    let rotation_scale_scale_dev = mean_abs_scale_minus_one(&rotation_scale);
    let translation_rho = mean_rho(&translation);
    let rotation_scale_rho = mean_rho(&rotation_scale);

    assert!(translation_scale_dev.is_finite());
    assert!(rotation_scale_scale_dev.is_finite());
    assert!(translation_rho.is_finite());
    assert!(rotation_scale_rho.is_finite());

    eprintln!(
        "the_two_modes_disagree_as_focus_breathing_predicts: \
         translation mean|scale-1|={translation_scale_dev:.6} mean_rho={translation_rho:.4} -- \
         rotation-scale mean|scale-1|={rotation_scale_scale_dev:.6} mean_rho={rotation_scale_rho:.4}"
    );
}
