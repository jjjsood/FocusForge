// SPDX-License-Identifier: GPL-3.0-or-later

//! Path and parity helpers shared by this crate's integration tests, resolving
//! upward from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a
//! test binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::{AlgorithmError, TileOutput, TileReader};
use focusforge_core::fuse::laplacian::{score_plane, score_radius};
use focusforge_core::fuse::pyramid;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::{Rect, Sample, SampleFormat};

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

/// The `mantis-512` fixture served as `f32` on the 16-bit scale with `offset`
/// added to every sample, so values reach above 65535 (positive `offset`) or
/// below 0 (negative) with the fixture's own gradients.
pub struct MantisF32 {
    pub stack: TiffStack,
    pub offset: f32,
}

impl MantisF32 {
    pub fn open(offset: f32) -> Self {
        Self {
            stack: open_stack(),
            offset,
        }
    }
}

impl TileReader for MantisF32 {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        TileReader::read_tile_into(&self.stack, frame, rect, out)
    }

    fn sample_format(&self) -> SampleFormat {
        SampleFormat::F32
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        let mut scratch = vec![0u16; out.len()];
        TileReader::read_tile_into(&self.stack, frame, rect, &mut scratch)?;
        for (o, &s) in out.iter_mut().zip(&scratch) {
            *o = f32::from(s) + self.offset;
        }
        Ok(())
    }
}

/// `128 * f32::EPSILON`: the near-tie bound -- the separable score is 68
/// `f32` adds per pixel at `radius = 8`, so two independently accumulated
/// pipelines can differ by at most ~68 ulps of the score, rounded up to a
/// power of two.
pub const NEAR_TIE_REL: f32 = 128.0 * f32::EPSILON;

/// The unmasked parity contract: the Z-Map equals the CPU's except at proven
/// near-ties between the two best frames, and every sample outside those
/// pixels is within 1 LSB of the 16-bit scale.
pub fn assert_matches_cpu_outside_near_ties<S: Sample, O: Sample>(
    label: &str,
    cpu: &TileOutput<O>,
    gpu: &TileOutput<O>,
    frames: &[&[S]],
    rect: Rect,
    spp: u32,
    levels: u32,
) {
    // Each frame's level-0 score plane, built once for every differing pixel.
    let radius = score_radius(levels, 0);
    let scores: Vec<Vec<f32>> = frames
        .iter()
        .map(|samples| {
            let pyr = pyramid::build(samples, rect.width, rect.height, spp, levels).expect("build");
            score_plane(&pyr.levels()[0], radius)
        })
        .collect();
    let mut near_ties = std::collections::HashSet::new();
    for (pixel, (&c, &g)) in cpu.zmap.iter().zip(&gpu.zmap).enumerate() {
        if c == g {
            continue;
        }
        let mut at: Vec<f32> = scores.iter().map(|plane| plane[pixel]).collect();
        at.sort_by(|a, b| b.partial_cmp(a).expect("scores are always finite"));
        assert!(
            at[0] - at[1] <= NEAR_TIE_REL * at[0],
            "pixel {pixel} differs (gpu={g}, cpu={c}) but is not a proven near-tie: {at:?}"
        );
        near_ties.insert(pixel);
    }
    println!(
        "{label} z-map parity: {} differing index(es), all proven near-ties",
        near_ties.len()
    );

    // A near-tie picks another source, so its own samples may differ by more
    // than a rounding step; every other sample stays within 1 LSB.
    let worst = cpu
        .tile
        .samples
        .chunks_exact(spp as usize)
        .zip(gpu.tile.samples.chunks_exact(spp as usize))
        .enumerate()
        .filter(|(pixel, _)| !near_ties.contains(pixel))
        .flat_map(|(_, (cpu, gpu))| cpu.iter().zip(gpu))
        .map(|(&c, &g)| (c.widen() - g.widen()).abs())
        .max_by(f32::total_cmp)
        .expect("a non-empty composite");
    println!("{label} composite parity: worst |delta|={worst} outside near-tie pixels");
    assert!(
        worst <= 1.0,
        "composite parity exceeded 1 LSB: worst={worst}"
    );
}
