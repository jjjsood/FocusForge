// SPDX-License-Identifier: GPL-3.0-or-later

//! The display kernel against its rayon twin, over
//! `fixtures/mantis-512/golden`. Expected worst |delta| is exactly 0 --
//! integer throughout on both sides -- so a non-zero delta is a
//! finding to investigate, not tolerance to spend.

use focusforge_core::display::{tonemap_cpu, zmap_index_scale};
use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapReader;
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::display::{DisplayTexture, read_display_texture, render};

use crate::common;

fn golden_dir() -> std::path::PathBuf {
    common::mantis_512_dir().join("golden")
}

/// Worst absolute `u8` delta between two equal-length RGBA8 byte buffers,
/// plus the byte index it occurred at.
fn worst_delta(gpu: &[u8], cpu: &[u8]) -> (i32, usize) {
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

#[test]
fn tonemap_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };

    let dir = golden_dir();

    // The composite path: `fixtures/mantis-512/golden/laplacian.tif`,
    // 512x512x3.
    let composite =
        TiffStack::open_files(&[dir.join("laplacian.tif")]).expect("open golden composite");
    let (width, height) = composite.dimensions();
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let spp = composite.header(0).samples_per_pixel;
    let mut composite_samples = vec![0u16; (width * height * spp) as usize];
    composite
        .read_tile_into(0, rect, &mut composite_samples)
        .expect("read golden composite");

    let target = DisplayTexture::new(shared.device(), width, height);
    render(shared, &target, &composite_samples, spp, 0);
    let gpu_bytes = read_display_texture(shared.device(), shared.queue(), &target);

    let mut cpu_bytes = vec![0u8; (width * height * 4) as usize];
    tonemap_cpu(&composite_samples, width, height, spp, 0, &mut cpu_bytes);

    let (worst, worst_index) = worst_delta(&gpu_bytes, &cpu_bytes);
    println!("tonemap composite parity: worst |delta|={worst} at byte index {worst_index}");
    assert_eq!(
        worst, 0,
        "expected bit-exact agreement -- integer-only on both sides"
    );

    // The Z-Map path: `fixtures/mantis-512/golden/laplacian-zmap.tif`,
    // 512x512x1, frames=8 -> index_scale=9362.
    let zmap = ZMapReader::open(&dir.join("laplacian-zmap.tif")).expect("open golden zmap");
    let (zwidth, zheight) = zmap.dimensions();
    let zrect = Rect {
        x: 0,
        y: 0,
        width: zwidth,
        height: zheight,
    };
    let mut indices = vec![0u16; (zwidth * zheight) as usize];
    zmap.read_tile_into(zrect, &mut indices)
        .expect("read golden zmap");
    let index_scale = zmap_index_scale(zmap.frame_count());
    assert_eq!(index_scale, 9362, "frames=8 -> 65535 / 7");

    let ztarget = DisplayTexture::new(shared.device(), zwidth, zheight);
    render(shared, &ztarget, &indices, 1, index_scale);
    let zgpu_bytes = read_display_texture(shared.device(), shared.queue(), &ztarget);

    let mut zcpu_bytes = vec![0u8; (zwidth * zheight * 4) as usize];
    tonemap_cpu(&indices, zwidth, zheight, 1, index_scale, &mut zcpu_bytes);

    let (zworst, zworst_index) = worst_delta(&zgpu_bytes, &zcpu_bytes);
    println!("tonemap zmap parity: worst |delta|={zworst} at byte index {zworst_index}");
    assert_eq!(
        zworst, 0,
        "expected bit-exact agreement -- integer-only on both sides"
    );
}

#[test]
fn tonemap_matches_cpu_at_the_extremes() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };

    // Full black, full white, and every 0xNN00/0xNNFF pair straddling an
    // 8-bit step.
    let mut values: Vec<u16> = vec![0, 65535];
    for hi in 0u16..=255 {
        values.push(hi * 256);
        values.push(hi * 256 + 255);
    }

    // A viewport width that is not a multiple of 64 -- what exercises
    // `crates/gpu/src/display/readback.rs`'s row padding.
    let width = 77u32;
    let pixel_count = values.len();
    let height = (pixel_count as u32).div_ceil(width);
    let total_pixels = (width * height) as usize;

    let mut samples = vec![0u16; total_pixels * 3];
    for (i, chunk) in samples.chunks_exact_mut(3).enumerate() {
        let v = values[i % values.len()];
        chunk[0] = v;
        chunk[1] = v;
        chunk[2] = v;
    }

    let target = DisplayTexture::new(shared.device(), width, height);
    render(shared, &target, &samples, 3, 0);
    let gpu_bytes = read_display_texture(shared.device(), shared.queue(), &target);

    let mut cpu_bytes = vec![0u8; total_pixels * 4];
    tonemap_cpu(&samples, width, height, 3, 0, &mut cpu_bytes);

    let (worst, worst_index) = worst_delta(&gpu_bytes, &cpu_bytes);
    println!("tonemap extremes parity: worst |delta|={worst} at byte index {worst_index}");
    assert_eq!(
        worst, 0,
        "expected bit-exact agreement at the extremes -- integer-only on both sides"
    );

    assert_eq!(cpu_bytes[0], 0, "0 must reduce to 0 (full black)");
    let white_pixel_byte = 4; // values[1] == 65535, the second pixel
    assert_eq!(
        cpu_bytes[white_pixel_byte], 255,
        "65535 must reduce to 255 (full white)"
    );
}
