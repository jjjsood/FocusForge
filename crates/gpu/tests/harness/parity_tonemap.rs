// SPDX-License-Identifier: GPL-3.0-or-later

//! The display kernel against its rayon twin, over
//! `fixtures/mantis-512/golden`. Expected worst |delta| is exactly 0 --
//! integer throughout on both sides -- so a non-zero delta is a
//! finding to investigate, not tolerance to spend.

use focusforge_core::display::{overlay_cpu, tonemap_cpu, zmap_index_scale};
use focusforge_core::io::TiffStack;
use focusforge_core::project::{Op, Point};
use focusforge_core::retouch::overlay_colors;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{NO_SOURCE, ZMapReader, overlay_table};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::display::{
    DisplayRenderer, DisplayTexture, read_display_texture, render_overlay,
};

use crate::common::{self, worst_delta};

fn golden_dir() -> std::path::PathBuf {
    common::mantis_512_dir().join("golden")
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

    let mut renderer = DisplayRenderer::new(shared.device());
    let target = DisplayTexture::new(shared.device(), width, height);
    renderer.render(
        shared.device(),
        shared.queue(),
        &target,
        &composite_samples,
        spp,
        0,
    );
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
    renderer.render(
        shared.device(),
        shared.queue(),
        &ztarget,
        &indices,
        1,
        index_scale,
    );
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

/// A draw above 65,535 workgroups of 64 invocations (4,194,240 texels), with
/// sides that are not multiples of the 8 x 8 workgroup, so the kernel's
/// bounds guard runs on both axes. The golden 512 x 512 planes are tiled to
/// size, sampled at `(x % 512, y % 512)`.
#[test]
fn tonemap_matches_cpu_above_the_one_dimensional_dispatch_ceiling() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let (width, height) = (2563u32, 2565u32);
    assert!(width * height > 65_535 * 64);

    let dir = golden_dir();
    let composite =
        TiffStack::open_files(&[dir.join("laplacian.tif")]).expect("open golden composite");
    let tile = composite
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 512,
                height: 512,
            },
        )
        .expect("read golden composite");
    let zmap = ZMapReader::open(&dir.join("laplacian-zmap.tif")).expect("open golden zmap");
    let mut zmap_tile = vec![0u16; 512 * 512];
    zmap.read_tile_into(
        Rect {
            x: 0,
            y: 0,
            width: 512,
            height: 512,
        },
        &mut zmap_tile,
    )
    .expect("read golden zmap");
    let index_scale = zmap_index_scale(zmap.frame_count());

    let tiled_pixel = |x: u32, y: u32| ((y % 512) * 512 + (x % 512)) as usize;
    let mut samples = Vec::with_capacity((width * height * 3) as usize);
    let mut indices = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let p = tiled_pixel(x, y);
            samples.extend_from_slice(&tile.samples[p * 3..p * 3 + 3]);
            indices.push(zmap_tile[p]);
        }
    }

    let mut renderer = DisplayRenderer::new(shared.device());
    for (label, src, spp, scale) in [
        ("composite", &samples, 3u32, 0u32),
        ("zmap", &indices, 1, index_scale),
    ] {
        let target = DisplayTexture::new(shared.device(), width, height);
        renderer.render(shared.device(), shared.queue(), &target, src, spp, scale);
        let gpu_bytes = read_display_texture(shared.device(), shared.queue(), &target);
        let mut cpu_bytes = vec![0u8; (width * height * 4) as usize];
        tonemap_cpu(src, width, height, spp, scale, &mut cpu_bytes);

        let (worst, worst_index) = worst_delta(&gpu_bytes, &cpu_bytes);
        println!(
            "tonemap {label} {width}x{height} parity: worst |delta|={worst} at byte index \
             {worst_index}"
        );
        assert_eq!(
            worst, 0,
            "expected bit-exact agreement -- integer-only on both sides"
        );
    }
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

    // A viewport width that is not a multiple of 64 px, so the readback's
    // row padding is exercised.
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

    let mut renderer = DisplayRenderer::new(shared.device());
    let target = DisplayTexture::new(shared.device(), width, height);
    renderer.render(shared.device(), shared.queue(), &target, &samples, 3, 0);
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

/// One [`DisplayRenderer`] reused across a large RGB draw, a small mono draw
/// with an odd sample count, and back to a mid-size RGB draw -- a
/// source-buffer shrink and grow plus a channel-count switch, all through
/// the one retained state. Every draw stays bit-exact against `tonemap_cpu`.
#[test]
fn a_reused_renderer_matches_cpu_after_shrinking_and_switching_channels() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let mut renderer = DisplayRenderer::new(shared.device());

    // (width, height, samples_per_pixel, index_scale) -- 77x13 = 1001
    // samples, odd.
    let draws: [(u32, u32, u32, u32); 3] =
        [(2563, 2565, 3, 0), (77, 13, 1, 9362), (512, 512, 3, 0)];

    for (width, height, spp, scale) in draws {
        let total = (width * height * spp) as usize;
        let samples: Vec<u16> = (0..total).map(|i| (i % 65536) as u16).collect();

        let target = DisplayTexture::new(shared.device(), width, height);
        renderer.render(
            shared.device(),
            shared.queue(),
            &target,
            &samples,
            spp,
            scale,
        );
        let gpu_bytes = read_display_texture(shared.device(), shared.queue(), &target);

        let mut cpu_bytes = vec![0u8; (width * height * 4) as usize];
        tonemap_cpu(&samples, width, height, spp, scale, &mut cpu_bytes);

        let (worst, worst_index) = worst_delta(&gpu_bytes, &cpu_bytes);
        println!(
            "reused renderer {width}x{height} spp={spp}: worst |delta|={worst} at byte index \
             {worst_index}"
        );
        assert_eq!(
            worst, 0,
            "expected bit-exact agreement -- integer-only on both sides"
        );
    }
}

/// The overlay entry point against `overlay_cpu`, on a crop of the golden
/// composite + Z-Map whose sides are not multiples of 8, at every blend
/// opacity the CLI exposes, with solo off and on, with a couple of pixels
/// forced to `NO_SOURCE` and one soft stroke crossing the crop.
#[test]
fn overlay_parity_matches_cpu() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };

    let dir = golden_dir();
    let composite =
        TiffStack::open_files(&[dir.join("laplacian.tif")]).expect("open golden composite");
    let zmap = ZMapReader::open(&dir.join("laplacian-zmap.tif")).expect("open golden zmap");

    let crop = Rect {
        x: 3,
        y: 5,
        width: 37,
        height: 23,
    };
    assert!(!crop.width.is_multiple_of(8) && !crop.height.is_multiple_of(8));

    let mut composite_samples = vec![0u16; (crop.width * crop.height * 3) as usize];
    composite
        .read_tile_into(0, crop, &mut composite_samples)
        .expect("read golden composite crop");
    let mut indices = vec![0u16; (crop.width * crop.height) as usize];
    zmap.read_tile_into(crop, &mut indices)
        .expect("read golden zmap crop");
    // Force a couple of pixels to NO_SOURCE, which the golden map does not
    // itself contain.
    indices[0] = NO_SOURCE;
    indices[5] = NO_SOURCE;

    let sources = zmap.frame_count();
    let stroke = Op::BrushStroke {
        source_id: 2,
        path: vec![Point { x: 0, y: 12 }, Point { x: 30, y: 20 }],
        radius: 9.0,
        softness: 0.6,
    };
    for alpha in [0u8, 166, 255] {
        for solo in [None, Some(3u16)] {
            let table = overlay_table(sources, solo);
            let mut colors = vec![[0u8; 3]; indices.len()];
            overlay_colors(
                std::slice::from_ref(&stroke),
                zmap.info(),
                crop,
                std::num::NonZeroU32::MIN,
                &table,
                &indices,
                &mut colors,
            );

            let target = DisplayTexture::new(shared.device(), crop.width, crop.height);
            render_overlay(
                shared.device(),
                shared.queue(),
                &target,
                &composite_samples,
                &colors,
                alpha,
            );
            let gpu_bytes = read_display_texture(shared.device(), shared.queue(), &target);

            let mut cpu_bytes = vec![0u8; (crop.width * crop.height * 4) as usize];
            overlay_cpu(
                &composite_samples,
                &colors,
                crop.width,
                crop.height,
                alpha,
                &mut cpu_bytes,
            );

            let (worst, worst_index) = worst_delta(&gpu_bytes, &cpu_bytes);
            println!(
                "overlay parity alpha={alpha} solo={solo:?}: worst |delta|={worst} at byte \
                 index {worst_index}"
            );
            assert_eq!(
                worst, 0,
                "expected bit-exact agreement -- integer-only on both sides"
            );
        }
    }
}
