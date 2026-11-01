// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a test
//! binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};

#[path = "reference_stack.rs"]
mod reference_stack;
#[allow(unused_imports)]
pub use reference_stack::reference_stack_dir;

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
    use std::time::Duration;

    use focusforge_core::job::JobControl;
    use focusforge_core::params::ParamValues;
    use focusforge_core::pipeline::{
        AlignRequest, FuseOutcome, FuseOutput, FuseProduct, FuseRequest, StageObserver,
    };
    use focusforge_core::{project, registry};

    struct NoopObserver;
    impl StageObserver for NoopObserver {}

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
        tile_size: None,
        max_host_memory: None,
        backend: None,
        align_backend: None,
        resume: false,
        cache: true,
        poll: Duration::from_millis(50),
    };
    let outcome = focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
        .expect("the fusion must complete");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("the fusion was cancelled");
    };
    let FuseProduct::Held(held) = report.product else {
        panic!("a hold request must report a held checkpoint");
    };
    held
}
