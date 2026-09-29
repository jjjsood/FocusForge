// SPDX-License-Identifier: GPL-3.0-or-later

//! The recomposite kernel against its rayon twin, over
//! `fixtures/mantis-512`. The gather is a pure `u16` copy, integer
//! throughout on both sides, so the expected worst |delta| is exactly 0.

use focusforge_core::align::{Transform, WarpingReader};
use focusforge_core::retouch::recomposite::{Sources, recomposite as recomposite_cpu};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{NO_SOURCE, ZMapReader};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::recomposite::recomposite as recomposite_gpu;

use crate::common;

fn golden_zmap_path() -> std::path::PathBuf {
    common::mantis_512_dir()
        .join("golden")
        .join("laplacian-zmap.tif")
}

/// Worst absolute `u16` delta between two equal-length sample buffers, plus
/// the sample index it occurred at.
fn worst_delta(gpu: &[u16], cpu: &[u16]) -> (i32, usize) {
    let mut worst = 0i32;
    let mut worst_index = 0usize;
    for (i, (&g, &c)) in gpu.iter().zip(cpu.iter()).enumerate() {
        let delta = (i32::from(g) - i32::from(c)).abs();
        if delta > worst {
            worst = delta;
            worst_index = i;
        }
    }
    (worst, worst_index)
}

fn run_both(
    indices: &[u16],
    region: Rect,
    sources: &Sources,
    shared: &focusforge_gpu::device::SharedDevice,
) -> (Vec<u16>, Vec<u16>) {
    let spp = sources.samples_per_pixel as usize;
    let mut cpu_out = vec![0u16; indices.len() * spp];
    recomposite_cpu(indices, region, sources, &mut cpu_out).expect("cpu recomposite");
    let gpu_out = recomposite_gpu(shared, indices, region, sources).expect("gpu recomposite");
    (gpu_out, cpu_out)
}

#[test]
fn recomposite_parity_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };

    let stack = focusforge_core::io::TiffStack::open(common::mantis_512_dir())
        .expect("open fixture frames");
    let zmap = ZMapReader::open(&golden_zmap_path()).expect("open golden zmap");
    let (width, height) = zmap.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let sources = Sources {
        reader: &stack,
        frame_count: zmap.frame_count(),
        samples_per_pixel: spp,
        origin: (0, 0),
    };

    for (label, region) in [
        (
            "whole",
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        ),
        (
            "37x23 at (101, 57)",
            Rect {
                x: 101,
                y: 57,
                width: 37,
                height: 23,
            },
        ),
    ] {
        let mut indices = vec![0u16; zmap.info().tile_len(region)];
        zmap.read_tile_into(region, &mut indices)
            .expect("read zmap region");

        let (gpu_out, cpu_out) = run_both(&indices, region, &sources, shared);
        let (worst, worst_index) = worst_delta(&gpu_out, &cpu_out);
        println!("recomposite {label} parity: worst |delta|={worst} at sample index {worst_index}");
        assert_eq!(
            worst, 0,
            "expected bit-exact agreement -- the gather is a pure u16 copy"
        );
    }
}

#[test]
fn recomposite_parity_through_warped_reader_at_non_zero_origin() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };

    let stack = focusforge_core::io::TiffStack::open(common::mantis_512_dir())
        .expect("open fixture frames");
    let zmap = ZMapReader::open(&golden_zmap_path()).expect("open golden zmap");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = zmap.frame_count();

    // Integer shifts, alternating sign -- exact under the warp's
    // Catmull-Rom kernel at a zero fractional part, the same reader the
    // fuse itself builds.
    let transforms: Vec<Transform> = (0..frame_count as usize)
        .map(|i| {
            let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
            Transform::translation(sign * 6.0, sign * -4.0)
        })
        .collect();
    let warping = WarpingReader::new(&stack, (width, height), spp, transforms);

    let origin = (5, 3);
    let region = Rect {
        x: 101,
        y: 57,
        width: 200,
        height: 150,
    };
    let mut indices = vec![0u16; zmap.info().tile_len(region)];
    zmap.read_tile_into(region, &mut indices)
        .expect("read zmap region");

    let sources = Sources {
        reader: &warping,
        frame_count,
        samples_per_pixel: spp,
        origin,
    };
    let (gpu_out, cpu_out) = run_both(&indices, region, &sources, shared);
    let (worst, worst_index) = worst_delta(&gpu_out, &cpu_out);
    println!(
        "recomposite through a warped reader at origin {origin:?}: worst |delta|={worst} at sample index {worst_index}"
    );
    assert_eq!(
        worst, 0,
        "expected bit-exact agreement -- integer shifts are exact under the warp kernel"
    );
}

#[test]
fn recomposite_parity_no_source() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };

    let stack = focusforge_core::io::TiffStack::open(common::mantis_512_dir())
        .expect("open fixture frames");
    let zmap = ZMapReader::open(&golden_zmap_path()).expect("open golden zmap");
    let (width, height) = zmap.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let sources = Sources {
        reader: &stack,
        frame_count: zmap.frame_count(),
        samples_per_pixel: spp,
        origin: (0, 0),
    };

    let region = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut indices = vec![0u16; zmap.info().tile_len(region)];
    zmap.read_tile_into(region, &mut indices)
        .expect("read golden zmap");
    // Every 7th pixel becomes NO_SOURCE.
    for (i, index) in indices.iter_mut().enumerate() {
        if i % 7 == 0 {
            *index = NO_SOURCE;
        }
    }

    let (gpu_out, cpu_out) = run_both(&indices, region, &sources, shared);
    let (worst, worst_index) = worst_delta(&gpu_out, &cpu_out);
    println!("recomposite NO_SOURCE parity: worst |delta|={worst} at sample index {worst_index}");
    assert_eq!(
        worst, 0,
        "expected bit-exact agreement -- the gather is a pure u16 copy"
    );

    for (i, &index) in indices.iter().enumerate() {
        if index == NO_SOURCE {
            for c in 0..spp as usize {
                assert_eq!(gpu_out[i * spp as usize + c], 0, "gpu NO_SOURCE sample {i}");
                assert_eq!(cpu_out[i * spp as usize + c], 0, "cpu NO_SOURCE sample {i}");
            }
        }
    }
}
