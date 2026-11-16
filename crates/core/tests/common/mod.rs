// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a test
//! binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};

pub mod align;

#[path = "reference_stack.rs"]
mod reference_stack;
#[allow(unused_imports)]
pub use reference_stack::{raw_sample_stack_dir, reference_stack_dir};

/// `crates/core/../..` -- the workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `crates/core/../../fixtures` -- the workspace's committed fixture trees.
pub fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}

/// `fixtures/mantis-512`, shared by every test file driven off that crop.
///
/// `#[allow(dead_code)]`: each integration test file compiles this shared
/// module into its own binary crate, and not every one of this crate's test
/// binaries calls this helper.
#[allow(dead_code)]
pub fn mantis_512_dir() -> PathBuf {
    fixtures_dir().join("mantis-512")
}

/// `path`'s first image: its width, height, samples per pixel and samples,
/// a float file narrowed to `u16`.
#[allow(dead_code)]
pub fn read_tiff(path: &Path) -> (u32, u32, u32, Vec<u16>) {
    use focusforge_core::io::{Rect, TiffStack};

    let stack = TiffStack::open_staged(path).unwrap();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let mut samples = vec![0u16; (width * height * spp) as usize];
    let full = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    stack.read_tile_into(0, full, &mut samples).unwrap();
    (width, height, spp, samples)
}

/// `path`'s first image as `f32` samples, interleaved; panics unless the
/// file stores `f32`.
#[allow(dead_code)]
pub fn read_f32(path: &Path) -> Vec<f32> {
    use focusforge_core::io::{Rect, TiffStack};
    use focusforge_core::tile::SampleFormat;

    let stack = TiffStack::open_staged(path).unwrap();
    assert_eq!(stack.sample_format(), SampleFormat::F32, "{path:?}");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let mut samples = vec![0f32; (width * height * spp) as usize];
    let full = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    stack.read_tile_f32_into(0, full, &mut samples).unwrap();
    samples
}

/// Every field of `report`, each `f64` as `f64::to_bits()`, flattened to
/// words: two reports are bit-identical exactly when these are equal.
#[allow(dead_code)]
pub fn equalization_bits(report: &focusforge_core::equalize::EqualizationReport) -> Vec<u64> {
    let mut words = vec![
        report.max_gain.to_bits(),
        report.target_stops.to_bits(),
        report.neutral.to_bits(),
        report.target.to_bits(),
    ];
    for f in &report.frames {
        words.extend([f.mean, f.estimated, f.applied, f.output_mean, f.residual].map(f64::to_bits));
        words.extend([
            u64::from(f.gain),
            u64::from(f.clamped),
            u64::from(f.black),
            f.clipped_samples,
        ]);
    }
    words
}

/// A fresh temporary directory under `CARGO_TARGET_TMPDIR`, removed on drop.
#[allow(dead_code)]
pub fn target_tmp() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("temporary directory")
}

/// Fuses `frames` in-process with the registered stack algorithm
/// `algorithm_id` at its default parameters (frames aligned, the crop applied
/// when `crop`) and returns the held result: the fusion's own composite and
/// Z-Map TIFFs under `dir`, independent of any project or render.
#[allow(dead_code)]
pub fn fuse_held(
    frames: &[PathBuf],
    algorithm_id: &str,
    crop: bool,
    dir: &Path,
) -> focusforge_core::project::HeldCheckpoint {
    struct NoopObserver;
    impl focusforge_core::pipeline::StageObserver for NoopObserver {}

    fuse_held_observed(frames, algorithm_id, crop, dir, &NoopObserver)
}

/// [`fuse_held`] reporting to `observer`, so a test can read the run's
/// previews.
#[allow(dead_code)]
pub fn fuse_held_observed(
    frames: &[PathBuf],
    algorithm_id: &str,
    crop: bool,
    dir: &Path,
    observer: &dyn focusforge_core::pipeline::StageObserver,
) -> focusforge_core::project::HeldCheckpoint {
    use std::time::Duration;

    use focusforge_core::job::JobControl;
    use focusforge_core::params::ParamValues;
    use focusforge_core::pipeline::{
        AlignRequest, FuseOutcome, FuseOutput, FuseProduct, FuseRequest,
    };
    use focusforge_core::{project, registry};

    let hold = dir.join("held");
    std::fs::create_dir_all(&hold).expect("the held directory must be creatable");
    let algorithm = registry::stack_algorithms()
        .into_iter()
        .find(|a| a.id() == algorithm_id)
        .expect("the algorithm id must be registered");
    let request = FuseRequest {
        frames: frames.to_vec(),
        output: FuseOutput::Hold(hold),
        scratch_root: dir.join("scratch"),
        staging_key: "fuse_held".to_string(),
        algorithm,
        explicit: ParamValues::new(),
        align: Some(AlignRequest {
            algorithm: registry::default_align().expect("a default aligner is registered"),
            explicit: ParamValues::new(),
            // The fixture's frames do not converge; a failed frame keeps the identity.
            lax: true,
        }),
        crop,
        equalize: project::equalization_settings(
            &project::params_for(algorithm),
            &ParamValues::new(),
        ),
        defects: None,
        tile_size: None,
        max_host_memory: None,
        backend: None,
        align_backend: None,
        resume: false,
        cache: true,
        poll: Duration::from_millis(50),
        free_space: None,
    };
    let outcome = focusforge_core::pipeline::run(&request, observer, JobControl::new(), None)
        .expect("the fusion must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("the fusion was cancelled");
    };
    let FuseProduct::Held(held) = report.product else {
        panic!("a hold request must report a held checkpoint");
    };
    held
}
