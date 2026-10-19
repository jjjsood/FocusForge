// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own binary because it destroys the shared device
//! (`SharedDevice::force_loss`); see `compute_loss.rs`.

#![cfg(feature = "gpu-tests")]

use focusforge_core::project::{Op, Point};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapInfo;
use focusforge_gpu::blend::{BlendError, Blender};
use focusforge_gpu::device::shared_device_or_skip;

/// A blend on a lost device is an `Err` the caller can fall back from, never
/// a panic, and leaves `out` as it was.
#[test]
fn blend_on_a_lost_device_returns_an_error() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = Blender::new(shared.device(), shared.queue());
    let region = Rect {
        x: 0,
        y: 0,
        width: 32,
        height: 32,
    };
    let info = ZMapInfo::new(32, 32, 2).unwrap();
    let op = Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x: 16, y: 16 }],
        radius: 10.0,
        softness: 0.5,
    };
    let base: Vec<u16> = (0..32 * 32).map(|i| i * 3).collect();
    let src = vec![40_000u16; 32 * 32];

    let mut out = base.clone();
    blender
        .apply(&op, info, region, &mut out, region, &src, 1)
        .expect("a live device blends");
    assert_ne!(out, base);

    shared.force_loss();

    let mut out = base.clone();
    let err = blender
        .apply(&op, info, region, &mut out, region, &src, 1)
        .expect_err("a lost device cannot blend");
    assert!(matches!(err, BlendError::Gpu(_)), "{err}");
    assert_eq!(out, base);
}
