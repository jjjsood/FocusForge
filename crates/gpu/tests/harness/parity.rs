// SPDX-License-Identifier: GPL-3.0-or-later

//! A per-sample gain-and-clamp kernel against its rayon twin. Expected worst
//! delta is exactly 0 (`floor(x + 0.5)` on both sides, never `round`) -- a
//! non-zero delta is a finding to investigate, not a tolerance to spend
//! (the budget is <= 1 LSB at 16-bit).

use focusforge_core::algorithm::TileReader;
use focusforge_core::gpu::{TileBackend, self_check_cpu};
use focusforge_core::io::TiffStack;
use focusforge_core::tile::{Rect, Tile};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::harness::gpu_backend;

use crate::common;

#[test]
fn harness_kernel_matches_cpu_twin() {
    let Some(_shared) = shared_device_or_skip() else {
        return;
    };
    let backend =
        gpu_backend().expect("gpu backend must be available once shared_device_or_skip found one");

    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let frame = 3;
    let samples_per_pixel = stack.header(frame).samples_per_pixel;
    // 201x137 on frame 03: 201*137*3 = 82,611 samples -- odd, so the odd
    // trailing `u16` is exercised on the parity path too, not only by
    // `transfer::tile_roundtrip_is_bit_identical`.
    let rect = Rect {
        x: 50,
        y: 50,
        width: 201,
        height: 137,
    };
    assert_eq!(
        (rect.width * rect.height * samples_per_pixel) as usize,
        82_611
    );

    let gain = 1.3_f32;

    let gpu_tile = backend
        .self_check(&stack, frame, rect, samples_per_pixel, gain)
        .expect("gpu self_check");

    let mut cpu_input_samples = vec![0u16; (rect.width * rect.height * samples_per_pixel) as usize];
    let reader: &dyn TileReader = &stack;
    reader
        .read_tile_into(frame, rect, &mut cpu_input_samples)
        .expect("read fixture tile for the CPU twin");
    let cpu_input = Tile {
        rect,
        samples_per_pixel,
        samples: cpu_input_samples,
    };
    let cpu_tile = self_check_cpu(&cpu_input, gain);

    assert_eq!(gpu_tile.samples.len(), cpu_tile.samples.len());

    let (worst_delta, worst_index) = common::worst_delta(&gpu_tile.samples, &cpu_tile.samples);

    println!("parity: worst |delta|={worst_delta} at sample index {worst_index}");
    assert!(
        worst_delta <= 1,
        "harness kernel parity exceeded 1 LSB: worst={worst_delta} at index {worst_index}"
    );
}
