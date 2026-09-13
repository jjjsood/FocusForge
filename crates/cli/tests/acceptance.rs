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
