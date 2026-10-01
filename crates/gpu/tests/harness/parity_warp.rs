// SPDX-License-Identifier: GPL-3.0-or-later

//! Parity between `crates/gpu`'s WGSL bicubic warp kernel and its
//! `crates/core` rayon twin, over `fixtures/mantis-512` and synthetic
//! geometries, each taken from an existing CPU test (`crates/core/tests/warp.rs`,
//! `crates/core/src/align/transform.rs`) so the CPU side of every comparison
//! is already known-good, never invented here.

use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{AlignError, Transform};
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::{Rect, Tile};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::harness::gpu_backend;
use focusforge_gpu::warp::{WarpPipelines, warp_from_tile_gpu};

use crate::common::{open_stack, worst_delta};

/// A [`TileReader`] that counts calls, for asserting "no read attempted" --
/// the same shape `crates/core/tests/warp.rs`'s own `CountingReader` uses.
struct CountingReader<'a> {
    inner: &'a TiffStack,
    calls: AtomicUsize,
}

impl<'a> TileReader for CountingReader<'a> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .read_tile_into(frame, rect, out)
            .map_err(AlgorithmError::from)
    }
}

#[test]
fn identity_transform_is_exact() {
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;
    let t = Transform::identity();
    let backend = gpu_backend().expect("gpu backend");

    for dst in [
        Rect {
            x: 200,
            y: 200,
            width: 96,
            height: 96,
        },
        Rect {
            x: width - 96,
            y: height - 96,
            width: 96,
            height: 96,
        },
    ] {
        let warped = backend
            .warp_tile(&stack, 0, frame, spp, dst, &t)
            .expect("identity warp should succeed");
        assert!(
            warped.covered.iter().all(|&c| c),
            "every pixel must be covered for {dst:?}"
        );
        let expected = stack
            .read_tile(0, dst)
            .expect("reading the same rect directly");
        assert_eq!(
            warped.tile.samples, expected.samples,
            "identity warp must be bit-identical for {dst:?}"
        );
    }
}

#[test]
fn warped_tile_matches_cpu_within_one_lsb() {
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;
    let t = Transform::similarity(1.02, 0.03, 1.5, -1.0);
    let dst = Rect {
        x: 128,
        y: 128,
        width: 200,
        height: 200,
    };

    let cpu =
        focusforge_core::align::warp::warp_tile(&stack, 0, frame, spp, dst, &t).expect("cpu warp");
    let backend = gpu_backend().expect("gpu backend");
    let gpu = backend
        .warp_tile(&stack, 0, frame, spp, dst, &t)
        .expect("gpu warp");

    assert_eq!(
        gpu.covered, cpu.covered,
        "coverage must match the CPU twin exactly"
    );
    let (worst, worst_index) = worst_delta(&gpu.tile.samples, &cpu.tile.samples);
    println!("warp parity: worst |delta|={worst} at sample index {worst_index}");
    assert!(
        worst <= 1,
        "warp parity exceeded 1 LSB: worst={worst} at index {worst_index}"
    );
}

#[test]
fn source_rect_crossing_frame_edge_matches_cpu() {
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;
    let t = Transform::translation(50.0, 0.0);
    let dst = Rect {
        x: 400,
        y: 200,
        width: 96,
        height: 32,
    };

    let cpu =
        focusforge_core::align::warp::warp_tile(&stack, 0, frame, spp, dst, &t).expect("cpu warp");
    let backend = gpu_backend().expect("gpu backend");
    let gpu = backend
        .warp_tile(&stack, 0, frame, spp, dst, &t)
        .expect("gpu warp");

    assert_eq!(
        gpu.covered, cpu.covered,
        "coverage must match the CPU twin exactly for an edge-crossing tile"
    );
    assert_eq!(
        gpu.tile.samples, cpu.tile.samples,
        "an integer translation must be bit-identical to the CPU twin"
    );
}

#[test]
fn out_of_footprint_matches_cpu() {
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;
    let backend = gpu_backend().expect("gpu backend");

    // The edge-crossing tile again: the CPU twin's uncovered value (0) must
    // match exactly on every pixel the CPU twin itself reports uncovered.
    let t = Transform::translation(50.0, 0.0);
    let dst = Rect {
        x: 400,
        y: 200,
        width: 96,
        height: 32,
    };
    let cpu =
        focusforge_core::align::warp::warp_tile(&stack, 0, frame, spp, dst, &t).expect("cpu warp");
    let gpu = backend
        .warp_tile(&stack, 0, frame, spp, dst, &t)
        .expect("gpu warp");
    for (i, &covered) in cpu.covered.iter().enumerate() {
        if covered {
            continue;
        }
        assert!(
            !gpu.covered[i],
            "pixel {i} is uncovered on the CPU side and must be uncovered on the GPU side too"
        );
        for c in 0..spp as usize {
            assert_eq!(
                gpu.tile.samples[i * spp as usize + c],
                cpu.tile.samples[i * spp as usize + c],
                "the out-of-footprint value at pixel {i} channel {c} must match the CPU twin"
            );
        }
    }

    // A destination rect that pulls entirely from outside the frame: no read
    // attempted, all pixels uncovered -- mirrors the CPU twin's own
    // out-of-frame test.
    let counting = CountingReader {
        inner: &stack,
        calls: AtomicUsize::new(0),
    };
    let far_outside = Rect {
        x: 0,
        y: 0,
        width: 32,
        height: 32,
    };
    let t_far = Transform::translation(600.0, 0.0);
    let warped_outside = backend
        .warp_tile(&counting, 0, frame, spp, far_outside, &t_far)
        .expect("entirely-outside warp must not error");
    assert!(warped_outside.covered.iter().all(|&c| !c));
    assert!(warped_outside.tile.samples.iter().all(|&s| s == 0));
    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        0,
        "an entirely-outside destination rect must not read anything"
    );
}

#[test]
fn projective_transform_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    // w = 1 + 0.001*x, not exactly 1 anywhere in either destination rect
    // below, so the divide is genuinely exercised.
    let t = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.001, 0.0, 1.0]]);

    let width = 64u32;
    let height = 64u32;
    let mut samples = vec![0u16; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            samples[(y * width + x) as usize] = (x as u16).wrapping_mul(200) ^ (y as u16);
        }
    }
    let src = Tile {
        rect: Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        samples_per_pixel: 1,
        samples,
    };
    let pipelines = WarpPipelines::new();

    for dst in [
        Rect {
            x: 20,
            y: 20,
            width: 4,
            height: 4,
        },
        Rect {
            x: 8,
            y: 8,
            width: 40,
            height: 40,
        },
    ] {
        let cpu = warp_from_tile(&src, (width, height), dst, &t).expect("cpu projective warp");
        let gpu = warp_from_tile_gpu(shared, &pipelines, &src, (width, height), dst, &t)
            .expect("gpu projective warp");

        assert_eq!(
            gpu.covered, cpu.covered,
            "coverage must match the CPU twin exactly for {dst:?}"
        );
        let (worst, worst_index) = worst_delta(&gpu.tile.samples, &cpu.tile.samples);
        println!("projective parity ({dst:?}): worst |delta|={worst} at index {worst_index}");
        assert!(
            worst <= 1,
            "projective parity exceeded 1 LSB at {dst:?}: worst={worst} at index {worst_index}"
        );
    }
}

#[test]
fn vanishing_w_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    // w = 1 - x: column dx=0 (x=0) has w=1 (covered), dx>=1 crosses the
    // horizon. Must go
    // through `warp_from_tile_gpu`, not `warp_tile_gpu`:
    // `source_footprint` rejects the whole rect with `HorizonCrossesRect`
    // if any *corner* is refused, so only the tile-in-hand entry point
    // exercises a per-pixel `w`-vanishing case.
    let t = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 1.0]]);
    let width = 8u32;
    let height = 8u32;
    let mut samples = vec![0u16; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            samples[(y * width + x) as usize] = (x as u16) * 1000 + y as u16;
        }
    }
    let src = Tile {
        rect: Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        samples_per_pixel: 1,
        samples,
    };
    let frame = (100u32, 100u32);
    let dst = Rect {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    };
    let pipelines = WarpPipelines::new();

    let cpu = warp_from_tile(&src, frame, dst, &t).expect("cpu vanishing-w warp");
    let gpu =
        warp_from_tile_gpu(shared, &pipelines, &src, frame, dst, &t).expect("gpu vanishing-w warp");

    assert_eq!(gpu.covered, cpu.covered);
    assert_eq!(gpu.tile.samples, cpu.tile.samples);
}

#[test]
fn source_too_small_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    // Also via `warp_from_tile_gpu`: through the reader-driven path the
    // read rect is already grown by `KERNEL_RADIUS` and clamped to the
    // frame, so the tap window always fits and this error is unreachable
    // by construction there.
    let src = Tile {
        rect: Rect {
            x: 5,
            y: 5,
            width: 1,
            height: 1,
        },
        samples_per_pixel: 1,
        samples: vec![1000],
    };
    let dst = Rect {
        x: 5,
        y: 5,
        width: 1,
        height: 1,
    };
    let t = Transform::identity();
    let frame = (100u32, 100u32);
    let pipelines = WarpPipelines::new();

    let cpu_err = warp_from_tile(&src, frame, dst, &t)
        .expect_err("a source tile with no kernel margin must be refused");
    let gpu_err = warp_from_tile_gpu(shared, &pipelines, &src, frame, dst, &t)
        .expect_err("the GPU twin must refuse the same geometry");

    match (cpu_err, gpu_err) {
        (
            AlignError::SourceTileTooSmall {
                needed: cpu_needed,
                have: cpu_have,
            },
            AlignError::SourceTileTooSmall {
                needed: gpu_needed,
                have: gpu_have,
            },
        ) => {
            assert_eq!(cpu_needed, gpu_needed);
            assert_eq!(cpu_have, gpu_have);
        }
        other => panic!("expected SourceTileTooSmall on both sides, got {other:?}"),
    }
}

#[test]
fn far_origin_coordinate_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    // The gradient regime named in `crates/core/src/align/warp.rs`'s module
    // doc: a hard horizontal
    // edge at frame coordinates near 8192 px, where `ulp(8192.0f32)` against
    // a 1117 LSB/px neighbour delta is already ~1 LSB of coordinate-rounding
    // error -- the regime the 512 px fixture cannot reach.
    const DELTA_PER_PIXEL: i64 = 1117;
    let tile_origin_x = 8176u32; // 8192 - 16
    let width = 32u32;
    let mut samples = vec![0u16; width as usize];
    for (i, s) in samples.iter_mut().enumerate() {
        *s = (i as i64 * DELTA_PER_PIXEL).clamp(0, 65535) as u16;
    }
    let src = Tile {
        rect: Rect {
            x: tile_origin_x,
            y: 0,
            width,
            height: 1,
        },
        samples_per_pixel: 1,
        samples,
    };
    let frame = (tile_origin_x + width + 16, 1);
    let dst = Rect {
        x: 8192,
        y: 0,
        width: 1,
        height: 1,
    };
    let t = Transform::translation(0.00045, 0.0);
    let pipelines = WarpPipelines::new();

    let cpu = warp_from_tile(&src, frame, dst, &t).expect("cpu far-origin warp");
    let gpu =
        warp_from_tile_gpu(shared, &pipelines, &src, frame, dst, &t).expect("gpu far-origin warp");

    let delta = (i32::from(gpu.tile.samples[0]) - i32::from(cpu.tile.samples[0])).abs();
    println!("far-origin coordinate parity: worst |delta|={delta}");
    assert!(
        delta <= 1,
        "far-origin coordinate parity exceeded 1 LSB: delta={delta}"
    );
}
