// SPDX-License-Identifier: GPL-3.0-or-later

//! The one fixture-backed test for `crate::fuse::pyramid`: a real
//! `fixtures/mantis-512` frame round-trips bit-identically at every level
//! count `0..=7`. The TIFF reader is used by *this test* only -- the
//! pyramid code itself never sees a file.

mod common;

use focusforge_core::fuse::{build, collapse_into, max_levels};
use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;

#[test]
fn roundtrip_on_a_fixture_frame() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512");
    let (width, height) = stack.dimensions();
    assert_eq!((width, height), (512, 512));
    assert_eq!(max_levels(width, height), 7);

    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let tile = stack.read_tile(0, rect).expect("reading frame 0's tile");

    for levels in 0..=7u32 {
        let pyramid = build(&tile.samples, width, height, tile.samples_per_pixel, levels)
            .unwrap_or_else(|e| panic!("build at levels={levels} failed: {e}"));
        let mut out = vec![0u16; tile.samples.len()];
        collapse_into(&pyramid, &mut out)
            .unwrap_or_else(|e| panic!("collapse_into at levels={levels} failed: {e}"));
        assert_eq!(
            out, tile.samples,
            "round trip must be bit-identical at levels={levels}"
        );
    }
}
