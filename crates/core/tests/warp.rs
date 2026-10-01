// SPDX-License-Identifier: GPL-3.0-or-later

//! The tiled warp exercised against `fixtures/mantis-512` and synthetic
//! `Tile`s. Opened the way `crates/core/tests/pyramid.rs` opens its
//! fixture: `mod common;` plus `TiffStack::open(common::mantis_512_dir())`,
//! never a relative literal.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::align::warp::{warp_from_tile, warp_tile};
use focusforge_core::align::{
    AlignError, KERNEL_RADIUS, SourceFootprint, Transform, WarpingReader,
};
use focusforge_core::io::TiffStack;
use focusforge_core::tile::{Rect, Tile};

fn open_fixture() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512")
}

/// A [`TileReader`] that counts calls, for asserting "no read attempted".
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
fn identity_warp_is_bit_identical() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    assert_eq!((width, height), (512, 512));
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;
    let t = Transform::identity();

    let interior = Rect {
        x: 200,
        y: 200,
        width: 96,
        height: 96,
    };
    let edge_flush = Rect {
        x: width - 96,
        y: height - 96,
        width: 96,
        height: 96,
    };

    for dst in [interior, edge_flush] {
        let warped =
            warp_tile(&stack, 0, frame, spp, dst, &t).expect("identity warp should succeed");
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
fn integer_translation_is_exact() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;
    // Every destination pixel (dx, dy) maps to source (dx + 5, dy - 3): a
    // pure integer shift, no interpolation blur.
    let t = Transform::translation(5.0, -3.0);
    let dst = Rect {
        x: 100,
        y: 100,
        width: 64,
        height: 64,
    };

    let warped = warp_tile(&stack, 0, frame, spp, dst, &t).expect("translation warp");
    assert!(warped.covered.iter().all(|&c| c));

    let source_rect = Rect {
        x: dst.x + 5,
        y: dst.y - 3,
        width: dst.width,
        height: dst.height,
    };
    let expected = stack
        .read_tile(0, source_rect)
        .expect("reading the shifted rect directly");
    assert_eq!(warped.tile.samples, expected.samples);
}

#[test]
fn round_trip_through_inverse() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;

    // A small similarity transform whose footprint on a 96 px interior tile
    // stays well inside the 512 px frame in both directions, so every
    // pixel in both passes is covered.
    let t = Transform::similarity(1.05, 0.05, 3.0, -2.0);
    let t_inv = t.inverse().expect("similarity transform is invertible");
    let dst = Rect {
        x: 200,
        y: 200,
        width: 96,
        height: 96,
    };
    // The forward pass warps a padded rect so the *second* (inverse) warp
    // has enough margin for its own kernel taps -- the reach of the
    // inverse map across a rotated, scaled rect is larger at its corners
    // than `KERNEL_RADIUS` alone, so the padding here is generous rather
    // than the tight footprint margin the reader-driven path computes.
    const ROUND_TRIP_PAD: u32 = 24;
    let padded_dst = Rect {
        x: dst.x - ROUND_TRIP_PAD,
        y: dst.y - ROUND_TRIP_PAD,
        width: dst.width + 2 * ROUND_TRIP_PAD,
        height: dst.height + 2 * ROUND_TRIP_PAD,
    };

    let forward = warp_tile(&stack, 0, frame, spp, padded_dst, &t).expect("forward warp");
    assert!(
        forward.covered.iter().all(|&c| c),
        "forward warp must stay fully covered for this transform and rect"
    );

    let back = warp_from_tile(&forward.tile, frame, dst, &t_inv).expect("inverse warp");
    assert!(
        back.covered.iter().all(|&c| c),
        "inverse warp must stay fully covered for this transform and rect"
    );

    let original = stack.read_tile(0, dst).expect("reading the original rect");

    let mut worst = 0i32;
    let mut deviations: Vec<i32> = Vec::new();
    for (a, b) in original.samples.iter().zip(back.tile.samples.iter()) {
        let d = (*a as i32 - *b as i32).abs();
        worst = worst.max(d);
        deviations.push(d);
    }
    deviations.sort_unstable();
    let p99_index = (deviations.len() * 99 / 100).min(deviations.len() - 1);
    let p99 = deviations[p99_index];

    // Measured 2026-09-13 on fixtures/mantis-512 frame 0, dst = (200, 200,
    // 96, 96), t = similarity(1.05, 0.05, 3.0, -2.0), forward pass padded
    // by 24 px so the inverse pass always has its own kernel margin:
    // worst-case and p99 absolute deviation of a T-then-T^-1 round trip, in
    // u16 LSB, against a fixture whose largest neighbour-to-neighbour
    // gradient is 1117 LSB/px. Pinned as the tolerance rather than a round
    // number.
    const ROUND_TRIP_WORST_CASE_LSB: i32 = 184;
    const ROUND_TRIP_P99_LSB: i32 = 117;
    assert!(
        worst <= ROUND_TRIP_WORST_CASE_LSB,
        "worst-case round-trip deviation {worst} exceeds recorded {ROUND_TRIP_WORST_CASE_LSB}"
    );
    assert!(
        p99 <= ROUND_TRIP_P99_LSB,
        "p99 round-trip deviation {p99} exceeds recorded {ROUND_TRIP_P99_LSB}"
    );
}

#[test]
fn tile_whose_source_crosses_the_frame_edge() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;

    // Half the destination tile maps past the right edge of the frame:
    // dst x in 400..496 maps to source x in 450..546, and the frame is only
    // 512 px wide (valid source x is 0..=511).
    let t = Transform::translation(50.0, 0.0);
    let dst = Rect {
        x: 400,
        y: 200,
        width: 96,
        height: 32,
    };
    let warped = warp_tile(&stack, 0, frame, spp, dst, &t).expect("no panic warping past the edge");

    for dy in 0..dst.height {
        for dx in 0..dst.width {
            let idx = (dy * dst.width + dx) as usize;
            let source_x = dst.x + dx + 50;
            if source_x > width - 1 {
                assert!(!warped.covered[idx], "dx={dx} dy={dy} should be uncovered");
                for c in 0..spp as usize {
                    assert_eq!(warped.tile.samples[idx * spp as usize + c], 0);
                }
            } else {
                assert!(warped.covered[idx], "dx={dx} dy={dy} should be covered");
            }
        }
    }

    // A destination rect that pulls entirely from outside the frame: no
    // read attempted, all pixels uncovered.
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
    let warped_outside = warp_tile(&counting, 0, frame, spp, far_outside, &t_far)
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
fn tiled_warp_matches_single_piece_warp() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let frame = (width, height);
    let spp = stack.header(0).samples_per_pixel;
    let t = Transform::similarity(1.02, 0.03, 1.5, -1.0);

    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let single = warp_tile(&stack, 0, frame, spp, whole, &t).expect("single-piece warp");

    let mut tiled_samples = vec![0u16; single.tile.samples.len()];
    let mut tiled_covered = vec![false; single.covered.len()];

    const TILE: u32 = 96;
    let mut y = 0u32;
    while y < height {
        let th = TILE.min(height - y);
        let mut x = 0u32;
        while x < width {
            let tw = TILE.min(width - x);
            let dst = Rect {
                x,
                y,
                width: tw,
                height: th,
            };
            let warped = warp_tile(&stack, 0, frame, spp, dst, &t).expect("per-tile warp");
            for dy in 0..th {
                for dx in 0..tw {
                    let local_idx = (dy * tw + dx) as usize;
                    let global_x = x + dx;
                    let global_y = y + dy;
                    let global_pixel = (global_y * width + global_x) as usize;
                    tiled_covered[global_pixel] = warped.covered[local_idx];
                    for c in 0..spp as usize {
                        tiled_samples[global_pixel * spp as usize + c] =
                            warped.tile.samples[local_idx * spp as usize + c];
                    }
                }
            }
            x += tw;
        }
        y += th;
    }

    assert_eq!(
        tiled_covered, single.covered,
        "coverage must match, no seams"
    );
    assert_eq!(
        tiled_samples, single.tile.samples,
        "samples must be bit-identical, no seams"
    );
}

#[test]
fn warping_reader_identity_matches_a_direct_read() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();

    // All-identity transforms, one per frame.
    let transforms = vec![Transform::identity(); frame_count];
    let reader = WarpingReader::new(&stack, (width, height), spp, transforms);

    // A tile straddling the frame edge, matching this file's own
    // `edge_flush` convention above.
    let dst = Rect {
        x: width - 96,
        y: height - 96,
        width: 96,
        height: 96,
    };
    let mut got = vec![0u16; dst.width as usize * dst.height as usize * spp as usize];
    reader
        .read_tile_into(0, dst, &mut got)
        .expect("identity WarpingReader read");

    let direct = stack.read_tile(0, dst).expect("direct read");
    assert_eq!(
        got, direct.samples,
        "an identity WarpingReader must be bit-identical to a direct read"
    );
}

#[test]
fn warping_reader_clamps_at_the_frame_edge() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();

    // A large translation whose destination rect's footprint reaches past
    // the frame edge: `source_footprint` clamps rather than refusing, and
    // the read must succeed with the out-of-source part zero, never
    // `TiffError::OutOfBounds`.
    let mut transforms = vec![Transform::identity(); frame_count];
    transforms[0] = Transform::translation(500.0, 0.0);
    let reader = WarpingReader::new(&stack, (width, height), spp, transforms);

    let dst = Rect {
        x: 0,
        y: 200,
        width: 64,
        height: 32,
    };
    let mut got = vec![0u16; dst.width as usize * dst.height as usize * spp as usize];
    reader
        .read_tile_into(0, dst, &mut got)
        .expect("a clamped-footprint read must succeed, not error");

    // Some destination pixels map past the right edge (source x = dst.x +
    // 500 >= width), so their samples are zero -- proving the clamp
    // happened and no `AlgorithmError::Frame(FrameError::Tiff(TiffError::OutOfBounds))` was
    // raised for the whole tile.
    assert!(
        got.contains(&0),
        "at least one sample must be the zero fill from the clamped, uncovered region"
    );
}

#[test]
fn coverage_is_none_for_identity_and_for_an_unclamped_translation() {
    let stack = open_fixture();
    let spp = stack.header(0).samples_per_pixel;
    let mut transforms = vec![Transform::identity(); stack.len()];
    transforms[1] = Transform::translation(0.25, 0.0);
    let reader = WarpingReader::new(&stack, (512, 512), spp, transforms);

    let rect = Rect {
        x: 100,
        y: 100,
        width: 64,
        height: 8,
    };
    let mut out = vec![0u16; 64 * 8 * spp as usize];
    for frame in [0, 1] {
        assert_eq!(
            reader
                .read_tile_coverage_into(frame, rect, &mut out)
                .unwrap(),
            None,
            "frame {frame}"
        );
    }
}

#[test]
fn coverage_names_exactly_the_uncovered_columns_and_leaves_samples_untouched() {
    let stack = open_fixture();
    let spp = stack.header(0).samples_per_pixel as usize;
    let mut transforms = vec![Transform::identity(); stack.len()];
    transforms[2] = Transform::translation(5.5, 0.0);
    let reader = WarpingReader::new(&stack, (512, 512), spp as u32, transforms);

    // A destination pixel is covered while its source x = x + 5.5 stays within
    // 0..=511, so the last six columns of the frame (x >= 506) are uncovered.
    for width in [64u32, 37] {
        let rect = Rect {
            x: 512 - width,
            y: 100,
            width,
            height: 8,
        };
        let len = width as usize * 8 * spp;
        let mut via_coverage = vec![0u16; len];
        let covered = reader
            .read_tile_coverage_into(2, rect, &mut via_coverage)
            .unwrap()
            .expect("the right edge is uncovered");
        assert_eq!(covered.len(), width as usize * 8);
        for (i, &c) in covered.iter().enumerate() {
            let column = i % width as usize;
            assert_eq!(c, column < width as usize - 6, "width {width} pixel {i}");
        }

        let mut via_read = vec![0u16; len];
        reader.read_tile_into(2, rect, &mut via_read).unwrap();
        assert_eq!(via_coverage, via_read, "width {width}");
    }
}

#[test]
fn source_tile_too_small_is_an_error() {
    // A source tile exactly the size of the destination rect has no margin
    // for the kernel's taps, even under an identity transform.
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
    match warp_from_tile(&src, (100, 100), dst, &t) {
        Err(AlignError::SourceTileTooSmall { have, .. }) => assert_eq!(have, src.rect),
        other => panic!("expected SourceTileTooSmall, got {other:?}"),
    }
}

#[test]
fn projective_transform_warps_with_the_perspective_divide() {
    // A mild perspective transform: w = 1 + 0.001*x, which is not exactly 1
    // anywhere in the destination rect below, so the divide is genuinely
    // exercised rather than being a no-op.
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
    let dst = Rect {
        x: 20,
        y: 20,
        width: 4,
        height: 4,
    };

    // The divide changes where a point lands: without it the numerator
    // alone would put x = 20 at x' = 20, but w = 1.02 at x = 20, so the
    // true mapped coordinate is meaningfully different.
    let (mapped_x, _mapped_y) = t.map_point(20.0, 20.0).unwrap();
    let numerator_only_x = 1.0 * 20.0 + 0.0 * 20.0 + 0.0;
    assert!(
        (mapped_x - numerator_only_x).abs() > 1e-3,
        "the perspective divide must change the mapped coordinate"
    );

    let warped = warp_from_tile(&src, (width, height), dst, &t).expect("projective warp");
    assert!(warped.covered.iter().all(|&c| c));
    // Self-consistency: the warped tile's own coverage/centre rule agrees
    // with `map_point`'s divide for every destination pixel in range.
    for dy in 0..dst.height {
        for dx in 0..dst.width {
            let ddx = (dst.x + dx) as f64;
            let ddy = (dst.y + dy) as f64;
            let (sx, sy) = t.map_point(ddx, ddy).unwrap();
            assert!(sx >= 0.0 && sx <= (width - 1) as f64);
            assert!(sy >= 0.0 && sy <= (height - 1) as f64);
        }
    }
}

#[test]
fn coordinate_precision_worst_case_is_measured() {
    // The gradient regime named in `crates/core/src/align/warp.rs`'s module
    // doc: a hard horizontal edge at frame coordinates near 8192 px, where
    // `ulp(8192.0f32) ~= 9.8e-4` px against a neighbour-to-neighbour delta
    // of 1117 LSB (measured on `fixtures/mantis-512/frames/00.tif`) is where
    // float32 rounding in coordinate arithmetic becomes visible at the
    // 16-bit LSB -- the 512 px fixture never reaches it.
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

    // Destination pixel at frame x = 8192, offset by a fractional
    // translation of 0.00045 px -- searched (not guessed) for the
    // fractional offset in this exact geometry (1117 LSB/px ramp, tile
    // origin at 8176) that maximizes the f64-vs-f32-coordinate deviation;
    // see this test's own arithmetic below, which is the search's
    // reference implementation. The coordinate that reaches
    // `Transform::map_point` is `8192.00045`, cast to `f32` for the sample
    // fraction exactly once in the production path.
    let dx = 8192u32;
    let dst = Rect {
        x: dx,
        y: 0,
        width: 1,
        height: 1,
    };
    let t = Transform::translation(0.00045, 0.0);

    let production = warp_from_tile(&src, frame, dst, &t).expect("production warp");
    let production_value = production.tile.samples[0];

    // Simulate an f32-coordinate pipeline: cast the source coordinate to
    // f32 *before* taking the fractional part, instead of forming it in
    // f64 and casting once.
    let sx_f64: f64 = dx as f64 + 0.00045;
    let sx_f32 = sx_f64 as f32;
    let ix_f64 = sx_f64.floor() as i64;
    let ix_f32 = sx_f32.floor() as i64;
    assert_eq!(
        ix_f64, ix_f32,
        "no integer-boundary crossing in this regime"
    );
    let tx_f32_pipeline = sx_f32 - ix_f32 as f32;

    fn crw(t: f32) -> [f32; 4] {
        [
            t * (t * (-0.5 * t + 1.0) - 0.5),
            t * t * (1.5 * t - 2.5) + 1.0,
            t * (t * (-1.5 * t + 2.0) + 0.5),
            t * t * (0.5 * t - 0.5),
        ]
    }
    let w = crw(tx_f32_pipeline);
    let base = (ix_f64 - src.rect.x as i64 - 1) as usize;
    let taps: [f32; 4] = [
        f32::from(src.samples[base]),
        f32::from(src.samples[base + 1]),
        f32::from(src.samples[base + 2]),
        f32::from(src.samples[base + 3]),
    ];
    let f32_pipeline_value_raw = w[0] * taps[0] + w[1] * taps[1] + w[2] * taps[2] + w[3] * taps[3];
    let f32_pipeline_value = (f32_pipeline_value_raw.clamp(0.0, 65535.0) + 0.5).floor() as u16;

    let deviation = (production_value as i32 - f32_pipeline_value as i32).abs();

    // Measured 2026-09-13: worst-case f64-vs-f32-coordinate deviation found
    // by an exhaustive 200,000-point sweep of the fractional offset in this
    // geometry (linear 1117 LSB/px ramp, tile origin 8176, destination x =
    // 8192). At the 512 px fixture's own scale the same construction
    // measures 0 -- float32 coordinate rounding is already marginal at
    // 8192 px, and this pins the worst case concretely.
    const COORDINATE_PRECISION_WORST_CASE_LSB: i32 = 1;
    assert_eq!(
        deviation, COORDINATE_PRECISION_WORST_CASE_LSB,
        "measured deviation {deviation} no longer matches the recorded worst case \
         {COORDINATE_PRECISION_WORST_CASE_LSB} -- re-measure and update the constant"
    );
}

#[test]
fn bicubic_overshoot_is_clamped() {
    // Synthetic 0 <-> 65535 step: Catmull-Rom rings past both ends here.
    let samples: Vec<u16> = vec![0, 0, 0, 0, 65535, 65535, 65535, 65535];
    let src = Tile {
        rect: Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 1,
        },
        samples_per_pixel: 1,
        samples,
    };
    let frame = (8u32, 1u32);

    // Undershoot: sampling at x = 2.5 (t = 0.5) puts most negative weight
    // on the step's high side, producing a raw accumulation below 0.
    let low = warp_from_tile(
        &src,
        frame,
        Rect {
            x: 2,
            y: 0,
            width: 1,
            height: 1,
        },
        &Transform::translation(0.5, 0.0),
    )
    .expect("warp near the low side of the step");
    assert_eq!(
        low.tile.samples[0], 0,
        "undershoot must clamp to 0, not wrap"
    );

    // Overshoot: sampling at x = 4.5 (t = 0.5) puts most positive weight
    // beyond the step's high side, producing a raw accumulation above
    // 65535.
    let high = warp_from_tile(
        &src,
        frame,
        Rect {
            x: 4,
            y: 0,
            width: 1,
            height: 1,
        },
        &Transform::translation(0.5, 0.0),
    )
    .expect("warp near the high side of the step");
    assert_eq!(
        high.tile.samples[0], 65535,
        "overshoot must clamp to 65535, not wrap"
    );

    // Ringing on real data: fixture frame 00 spans [9471, 49782] and never
    // reaches either clamp (largest neighbour delta 1117 LSB), so it can
    // only demonstrate ringing -- a warped value outside its own 4-tap
    // window's [min, max] -- not the clamp.
    let stack = open_fixture();
    let spp = stack.header(0).samples_per_pixel as usize;
    let region = Rect {
        x: 480,
        y: 490,
        width: 32,
        height: 16,
    };
    let region_tile = stack
        .read_tile(0, region)
        .expect("reading the ringing region");
    let t = Transform::translation(0.5, 0.0);
    let dst = Rect {
        x: 490,
        y: 496,
        width: 8,
        height: 1,
    };
    let warped = warp_from_tile(&region_tile, stack.dimensions(), dst, &t).expect("ringing warp");

    let mut found_ringing = false;
    for dx in 0..dst.width {
        for c in 0..spp {
            let sx = (dst.x + dx) as f64 + 0.5;
            let ix = sx.floor() as i64;
            let mut lo = u16::MAX;
            let mut hi = 0u16;
            let rel_y = (dst.y - region.y) as usize;
            for tap in (ix - 1)..=(ix + 2) {
                let tap = tap.clamp(0, 511) as u32;
                let rel_x = (tap - region.x) as usize;
                let v = region_tile.samples[(rel_y * region.width as usize + rel_x) * spp + c];
                lo = lo.min(v);
                hi = hi.max(v);
            }
            let idx = dx as usize;
            let out = warped.tile.samples[idx * spp + c];
            if out < lo || out > hi {
                found_ringing = true;
            }
        }
    }
    assert!(
        found_ringing,
        "expected at least one warped sample to ring outside its own 4-tap window"
    );
}

/// Reports one byte of scratch per pixel of the rect it is asked about.
struct PixelScratchReader;

impl TileReader for PixelScratchReader {
    fn read_tile_into(
        &self,
        _frame: usize,
        _rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        out.fill(0);
        Ok(())
    }

    fn read_scratch_bytes(&self, width: u32, height: u32) -> u64 {
        u64::from(width) * u64::from(height)
    }
}

/// The scratch the warp of a centred `rect` allocates, worked out from the
/// footprint the warp itself reads.
fn expected_warp_scratch(t: &Transform, frame: (u32, u32), spp: u64, rect: (u32, u32)) -> u64 {
    let dst = Rect {
        x: (frame.0 - rect.0) / 2,
        y: (frame.1 - rect.1) / 2,
        width: rect.0,
        height: rect.1,
    };
    let SourceFootprint::Covers { read, .. } = t.source_footprint(dst, frame).unwrap() else {
        panic!("the centred rect must land inside the frame");
    };
    let footprint = u64::from(read.width) * u64::from(read.height);
    let rect_pixels = u64::from(rect.0) * u64::from(rect.1);
    // The warped tile with its coverage, and the footprint's own inner scratch.
    footprint * spp * 2 + rect_pixels * (spp * 2 + 1) + footprint
}

#[test]
fn warp_scratch_of_identity_is_the_inner_scratch() {
    let reader = WarpingReader::new(
        &PixelScratchReader,
        (512, 512),
        3,
        vec![Transform::identity(); 3],
    );
    assert_eq!(reader.read_scratch_bytes(64, 64), 64 * 64);
}

#[test]
fn warp_scratch_of_a_translation_adds_the_kernel_margin() {
    let t = Transform::translation(5.5, 3.25);
    let reader = WarpingReader::new(
        &PixelScratchReader,
        (512, 512),
        3,
        vec![Transform::identity(), t],
    );
    let scratch = reader.read_scratch_bytes(64, 64);
    assert_eq!(scratch, expected_warp_scratch(&t, (512, 512), 3, (64, 64)));
    let margin = u64::from(2 * KERNEL_RADIUS);
    assert!(
        scratch > (64 + margin) * (64 + margin) * 3 * 2,
        "the footprint must exceed the destination rect by the kernel margin: {scratch}"
    );
}

#[test]
fn warp_scratch_of_a_scaling_similarity_exceeds_a_translation() {
    let scale = Transform::similarity(1.5, 0.0, 0.0, 0.0);
    let shift = Transform::translation(5.5, 3.25);
    let scaled = WarpingReader::new(&PixelScratchReader, (512, 512), 3, vec![scale]);
    let shifted = WarpingReader::new(&PixelScratchReader, (512, 512), 3, vec![shift]);
    assert_eq!(
        scaled.read_scratch_bytes(64, 64),
        expected_warp_scratch(&scale, (512, 512), 3, (64, 64))
    );
    assert!(scaled.read_scratch_bytes(64, 64) > shifted.read_scratch_bytes(64, 64));
}

#[test]
fn warp_scratch_footprint_is_clamped_to_the_frame() {
    let scale = Transform::similarity(1.5, 0.0, 0.0, 0.0);
    let reader = WarpingReader::new(&PixelScratchReader, (64, 64), 3, vec![scale]);
    let frame_pixels = 64u64 * 64;
    assert_eq!(
        reader.read_scratch_bytes(64, 64),
        frame_pixels * 3 * 2 + frame_pixels * (3 * 2 + 1) + frame_pixels
    );
}
