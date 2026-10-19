// SPDX-License-Identifier: GPL-3.0-or-later

//! The display reduction's CPU twin: no tone curve, ordered dither, and the
//! Z-Map index-scale uniform plus sentinel (`crates/core/src/display/mod.rs`).

use focusforge_core::display::{
    NO_SOURCE_DISPLAY, dither_threshold, reduce16to8, tonemap_cpu, zmap_index_scale,
};
use focusforge_core::zmap::NO_SOURCE;

#[test]
fn tonemap_cpu_twin_is_monotonic() {
    let (x, y) = (2, 1);
    let mut prev = reduce16to8(0, x, y);
    assert_eq!(prev, 0, "0 must map to 0");
    for v in 1..=u16::MAX {
        let out = reduce16to8(v, x, y);
        assert!(
            out >= prev,
            "reduce16to8 must never decrease: v={v} out={out} prev={prev}"
        );
        prev = out;
    }
    assert_eq!(reduce16to8(u16::MAX, x, y), 255, "65535 must map to 255");
}

#[test]
fn display_path_applies_no_curve() {
    // A linear ramp of 16-bit input must come back as a linear ramp of
    // 8-bit output, within the dither's own amplitude of +-1 -- never a
    // curve.
    for v in (0..=u16::MAX).step_by(257) {
        let expected = i32::from(v >> 8);
        for (x, y) in [(0u32, 0u32), (1, 2), (3, 3)] {
            let out = i32::from(reduce16to8(v, x, y));
            assert!(
                (out - expected).abs() <= 1,
                "v={v} x={x} y={y}: out={out} expected~{expected}"
            );
        }
    }
}

#[test]
fn dither_is_deterministic() {
    // Ordered, not random: the same input at the same pixel produces the
    // same output on every run -- a still image does not shimmer.
    for v in [0u16, 1, 12345, 40000, 65535] {
        for (x, y) in [(0u32, 0u32), (5, 7), (100, 200)] {
            assert_eq!(reduce16to8(v, x, y), reduce16to8(v, x, y));
            assert_eq!(dither_threshold(x, y), dither_threshold(x, y));
        }
    }
}

#[test]
fn zmap_index_scale_and_sentinel() {
    assert_eq!(
        zmap_index_scale(8),
        9362,
        "65535 / 7 under integer division"
    );
    assert_eq!(
        zmap_index_scale(1),
        0,
        "a single-frame map divides by nothing"
    );
    assert_eq!(zmap_index_scale(2), 65535);

    let width = 2;
    let height = 1;
    let src = [NO_SOURCE, 3u16];
    let mut out = [0u8; 8];
    tonemap_cpu(&src, width, height, 1, zmap_index_scale(8), &mut out);

    assert_eq!(
        &out[0..4],
        &[NO_SOURCE_DISPLAY, NO_SOURCE_DISPLAY, NO_SOURCE_DISPLAY, 255],
        "NO_SOURCE must map to NO_SOURCE_DISPLAY, not a wrapped index"
    );

    let expected_level = ((3u32 * zmap_index_scale(8)).min(65535)) as u16;
    let expected = reduce16to8(expected_level, 1, 0);
    assert_eq!(&out[4..8], &[expected, expected, expected, 255]);
}
