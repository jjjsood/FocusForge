// SPDX-License-Identifier: GPL-3.0-or-later

//! The two measures `focusforge-cli review` reports, over `fixtures/mantis-512`'s
//! `golden/` outputs: the composite sharpness ratio (`fuse::metrics::laplacian_variance`,
//! no second definition) and the Z-Map spread (`zmap::metrics`), plus the
//! streaming-equals-whole-buffer equivalence the band-carry logic depends on.

mod common;

use focusforge_core::fuse::metrics::laplacian_variance;
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::zmap::NO_SOURCE;
use focusforge_core::zmap::metrics::{SpreadScan, all_eight_identical_fraction};

fn read_whole(path: &std::path::Path) -> (Vec<u16>, u32, u32, u32) {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).expect("must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let samples = stack
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .expect("must read whole plane")
        .samples;
    (samples, width, height, spp)
}

/// The calibration check: every other floor in this file is meaningless
/// unless this passes.
#[test]
fn sharpness_ratio_matches_the_golden_composite() {
    let golden = common::mantis_512_dir()
        .join("golden")
        .join("laplacian.tif");
    let (composite, cw, ch, cspp) = read_whole(&golden);
    let composite_lapvar = laplacian_variance(&composite, cw, ch, cspp);

    let frames_dir = common::mantis_512_dir().join("frames");
    let mut entries: Vec<_> = std::fs::read_dir(&frames_dir)
        .expect("frames dir must exist")
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 8, "mantis-512 has eight source frames");

    let mut max_input_lapvar = 0.0f64;
    for path in &entries {
        let (samples, w, h, spp) = read_whole(path);
        let lv = laplacian_variance(&samples, w, h, spp);
        max_input_lapvar = max_input_lapvar.max(lv);
    }
    let ratio = composite_lapvar / max_input_lapvar;

    println!(
        "sharpness_ratio_matches_the_golden_composite: composite={composite_lapvar:.1} \
         max_input={max_input_lapvar:.1} ratio={ratio:.4}"
    );
    // Measured directly against the committed golden file (`golden_pin_matches_byte_for_byte`
    // in `laplacian_fusion.rs` proves it is byte-identical to a fresh unaligned 448/5 run, which
    // equals the single-tile run): composite 347107.5, sharpest input 285247.1, ratio 1.2169.
    assert!(
        (composite_lapvar - 347_107.5).abs() < 1.0,
        "composite lapvar must reproduce 347107.5: got {composite_lapvar:.1}"
    );
    assert!(
        (max_input_lapvar - 285_247.1).abs() < 1.0,
        "sharpest input lapvar must reproduce 285247.1: got {max_input_lapvar:.1}"
    );
    assert!(
        (ratio - 1.2169).abs() < 0.0005,
        "ratio must reproduce 1.2169: got {ratio:.4}"
    );
}

#[test]
fn zmap_spread_matches_the_golden_zmap() {
    let golden = common::mantis_512_dir()
        .join("golden")
        .join("laplacian-zmap.tif");
    let (indices, width, height, spp) = read_whole(&golden);
    assert_eq!(spp, 1, "a Z-Map is one index per pixel");

    let mut scan = SpreadScan::new(width, height);
    scan.push_band(
        &indices,
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
    );
    let spread = scan.finish();

    println!(
        "zmap_spread_matches_the_golden_zmap: distinct_sources={} coherence={:.4} \
         (random baseline 0.125, floor 0.55)",
        spread.distinct_sources, spread.coherence
    );
    assert_eq!(spread.distinct_sources, 8);
    assert!(
        spread.coherence >= 0.55,
        "all-8-identical coherence must be >= 0.55: got {:.4}",
        spread.coherence
    );
    assert!(
        (spread.coherence - 0.6376).abs() < 0.001,
        "coherence must reproduce 0.6376: got {:.4}",
        spread.coherence
    );

    // Agrees with the whole-buffer measure on the same data -- one
    // definition, not two.
    let whole_buffer = all_eight_identical_fraction(&indices, width, height);
    assert_eq!(spread.coherence, whole_buffer);
}

/// The band-carry logic must produce the identical bit pattern regardless of
/// how the plane is chunked -- 1-row, 3-row, 7-row and single-band inputs
/// all fed through [`SpreadScan`].
#[test]
fn streaming_spread_equals_the_whole_buffer_measure() {
    let golden = common::mantis_512_dir()
        .join("golden")
        .join("laplacian-zmap.tif");
    let (indices, width, height, _spp) = read_whole(&golden);
    let expected = all_eight_identical_fraction(&indices, width, height);

    for band_height in [1u32, 3, 7, height] {
        let mut scan = SpreadScan::new(width, height);
        let mut y = 0u32;
        while y < height {
            let rows = band_height.min(height - y);
            let start = (y * width) as usize;
            let end = ((y + rows) * width) as usize;
            scan.push_band(
                &indices[start..end],
                Rect {
                    x: 0,
                    y,
                    width,
                    height: rows,
                },
            );
            y += rows;
        }
        let spread = scan.finish();
        assert_eq!(
            spread.coherence, expected,
            "band height {band_height} must match the whole-buffer measure bit for bit"
        );
    }
}

#[test]
fn no_source_is_not_a_distinct_source() {
    let (width, height) = (16u32, 16u32);
    let indices = vec![NO_SOURCE; (width * height) as usize];
    let mut scan = SpreadScan::new(width, height);
    scan.push_band(
        &indices,
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
    );
    let spread = scan.finish();
    assert_eq!(spread.distinct_sources, 0);
    assert_eq!(spread.coherence, 1.0);
}
