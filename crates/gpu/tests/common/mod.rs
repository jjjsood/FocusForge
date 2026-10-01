// SPDX-License-Identifier: GPL-3.0-or-later

//! Path and parity helpers shared by this crate's integration tests, resolving
//! upward from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a
//! test binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};

use focusforge_core::io::TiffStack;

/// `crates/gpu/../..` -- the workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `crates/gpu/../../fixtures` -- the workspace's committed fixture trees.
pub fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}

/// `fixtures/mantis-512`, the crop this crate's parity tests read frames
/// from through the real `TiffStack` reader.
pub fn mantis_512_dir() -> PathBuf {
    fixtures_dir().join("mantis-512")
}

/// The `mantis-512` fixture opened through the real `TiffStack` reader.
pub fn open_stack() -> TiffStack {
    TiffStack::open(mantis_512_dir()).expect("open mantis-512 fixture")
}

/// Worst absolute delta between two equal-length sample buffers, plus the
/// sample index it occurred at.
pub fn worst_delta<T: Copy + Into<i32>>(gpu: &[T], cpu: &[T]) -> (i32, usize) {
    let mut worst = 0i32;
    let mut worst_index = 0usize;
    for (i, (&g, &c)) in gpu.iter().zip(cpu.iter()).enumerate() {
        let delta = (g.into() - c.into()).abs();
        if delta > worst {
            worst = delta;
            worst_index = i;
        }
    }
    (worst, worst_index)
}
