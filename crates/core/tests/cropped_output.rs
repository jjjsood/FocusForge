// SPDX-License-Identifier: GPL-3.0-or-later

//! `FusionConsumer` declaring both outputs at the crop rect's dimensions
//! (`crates/core/src/fuse/consumer.rs`), tested against the measured
//! `fixtures/mantis-512` crop rect rather than a synthetic one, so a real
//! tile partition is what proves `finish()`'s full-coverage check.

mod common;

use std::num::NonZeroU32;

use focusforge_core::algorithm::{AlgorithmError, TileReader, read_tile_step_into};
use focusforge_core::align::{Registration, Transform, WarpingReader, crop_rect};
use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};

/// The fixture rect measured over `fixtures/mantis-512`:
/// `Rect { x: 31, y: 40, width: 422, height: 424 }`.
fn measured_crop() -> Rect {
    Rect {
        x: 31,
        y: 40,
        width: 422,
        height: 424,
    }
}

#[test]
fn cropped_run_declares_both_outputs_at_the_crop() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);

    let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(5));
    let resolved =
        params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);

    let scratch_dir = common::workspace_root()
        .join(".scratch")
        .join("cropped_output");
    std::fs::create_dir_all(&scratch_dir).expect("scratch dir must be creatable");
    let composite_path = scratch_dir.join("composite.tif");
    let zmap_path = scratch_dir.join("zmap.tif");

    let scheduler_scratch =
        std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cropped_output_scheduler");
    let mut options = TileSchedulerOptions::explicit_budget(scheduler_scratch, 1 << 30);
    options.tile_size = 448;
    options.levels = 5;
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");

    let crop = measured_crop();
    let consumer = FusionConsumer::create(
        algo,
        values,
        &composite_path,
        Some(&zmap_path),
        FusionGeometry {
            width,
            height,
            samples_per_pixel: spp,
            frame_count: frame_count as u32,
            crop: Some(crop),
            description: Some(format!(
                "FocusForge test align=pyramidal-ecc anchor=3 mode=rotation-scale crop={},{},{},{} cropped=true",
                crop.x, crop.y, crop.width, crop.height
            )),
        },
    )
    .expect("consumer must create");

    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("run must succeed");
    consumer
        .finish()
        .expect("finish must succeed: full coverage after clipping");

    let composite_header =
        focusforge_core::io::TiffHeader::open(&composite_path).expect("composite must be readable");
    assert_eq!(
        (composite_header.width, composite_header.height),
        (crop.width, crop.height),
        "composite must be declared at the crop rect's dimensions"
    );
    assert!(
        composite_header
            .image_description
            .as_deref()
            .is_some_and(|d| d.contains("crop=31,40,422,424")),
        "composite ImageDescription must carry the crop rect: {:?}",
        composite_header.image_description
    );

    let zmap_header =
        focusforge_core::io::TiffHeader::open(&zmap_path).expect("zmap must be readable");
    assert_eq!(
        (zmap_header.width, zmap_header.height),
        (crop.width, crop.height),
        "Z-Map must be declared at the crop rect's dimensions too"
    );
}

// ---------------------------------------------------------------------
// "Frame as fused": a `Registration`'s transforms plus crop, composed
// through `WarpingReader` and `read_tile_step_into` -- what every
// consumer (this test, `focusforge-cli export`) does.
// ---------------------------------------------------------------------

/// A synthetic in-memory reader: `read_tile_into` returns
/// `frame*10_000 + y*100 + x` per sample, one sample per pixel, so every
/// gathered sample names its own frame and source coordinate.
struct PatternedFrames;

impl TileReader for PatternedFrames {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        for row in 0..rect.height {
            for col in 0..rect.width {
                let x = rect.x + col;
                let y = rect.y + row;
                out[(row * rect.width + col) as usize] =
                    (frame as u32 * 10_000 + y * 100 + x) as u16;
            }
        }
        Ok(())
    }
}

/// The frame-as-fused view of frame `i` at composite `rect`: `reader`
/// (a `WarpingReader` over a `Registration`'s transforms) read at
/// `rect + origin`.
fn read_fused_tile(
    reader: &WarpingReader<'_>,
    origin: (u32, u32),
    frame: usize,
    composite_rect: Rect,
    step: NonZeroU32,
    out: &mut [u16],
) {
    let frame_rect = Rect {
        x: composite_rect.x + origin.0,
        y: composite_rect.y + origin.1,
        width: composite_rect.width,
        height: composite_rect.height,
    };
    read_tile_step_into(reader, frame, frame_rect, step, 1, out).expect("read fused tile");
}

#[test]
fn frame_as_fused() {
    let frame_dims = (64u32, 48u32);
    // Shifts of both signs, and in y as well as x.
    let shifts: [(i64, i64); 3] = [(0, 0), (4, -3), (-5, 6)];
    let transforms: Vec<Transform> = shifts
        .iter()
        .map(|&(dx, dy)| Transform::translation(dx as f64, dy as f64))
        .collect();
    let crop = crop_rect(&transforms, frame_dims)
        .expect("crop_rect must succeed")
        .expect("these shifts must leave a non-empty interior");
    let registration = Registration {
        transforms: transforms.clone(),
        crop: Some(crop),
        equalization: None,
    };
    assert_eq!(
        registration.extent(frame_dims),
        (crop.width, crop.height),
        "the fused view's extent is the crop's size"
    );

    let inner = PatternedFrames;
    let reader = WarpingReader::new(&inner, frame_dims, 1, registration.transforms(shifts.len()));
    let origin = registration.origin();

    // A 37x23 tile grid that does not divide the crop's extent.
    const TILE_W: u32 = 37;
    const TILE_H: u32 = 23;

    for (frame, &(dx, dy)) in shifts.iter().enumerate() {
        let mut y = 0u32;
        while y < crop.height {
            let th = TILE_H.min(crop.height - y);
            let mut x = 0u32;
            while x < crop.width {
                let tw = TILE_W.min(crop.width - x);
                let composite_rect = Rect {
                    x,
                    y,
                    width: tw,
                    height: th,
                };

                // step = 1: exact equality against a direct computation of
                // the shifted, cropped-offset coordinate.
                let mut got = vec![0u16; (tw * th) as usize];
                read_fused_tile(
                    &reader,
                    origin,
                    frame,
                    composite_rect,
                    NonZeroU32::new(1).unwrap(),
                    &mut got,
                );
                for row in 0..th {
                    for col in 0..tw {
                        let cx = (x + col) as i64 + crop.x as i64 + dx;
                        let cy = (y + row) as i64 + crop.y as i64 + dy;
                        let expected = (frame as u32 * 10_000 + cy as u32 * 100 + cx as u32) as u16;
                        assert_eq!(
                            got[(row * tw + col) as usize],
                            expected,
                            "frame {frame} tile ({x},{y}) pixel ({col},{row})"
                        );
                    }
                }

                // step = 3 through read_tile_step_into: every step-th row
                // and column of the same tile.
                let step = NonZeroU32::new(3).unwrap();
                let out_w = tw.div_ceil(3);
                let out_h = th.div_ceil(3);
                let mut got_step = vec![0u16; (out_w * out_h) as usize];
                read_fused_tile(&reader, origin, frame, composite_rect, step, &mut got_step);
                for row in 0..out_h {
                    for col in 0..out_w {
                        let cx = (x + col * 3) as i64 + crop.x as i64 + dx;
                        let cy = (y + row * 3) as i64 + crop.y as i64 + dy;
                        let expected = (frame as u32 * 10_000 + cy as u32 * 100 + cx as u32) as u16;
                        assert_eq!(
                            got_step[(row * out_w + col) as usize],
                            expected,
                            "frame {frame} tile ({x},{y}) step-3 pixel ({col},{row})"
                        );
                    }
                }

                x += tw;
            }
            y += th;
        }
    }
}

#[test]
fn read_tile_step_into_matches_tiffstacks_own() {
    let stack =
        TiffStack::open(common::fixtures_dir().join("mantis-64")).expect("open fixtures/mantis-64");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let rect = Rect {
        x: 3,
        y: 5,
        width: (width - 3).min(40),
        height: (height - 5).min(31),
    };
    let step = NonZeroU32::new(2).unwrap();
    let out_len = (rect.width.div_ceil(2) * rect.height.div_ceil(2) * spp) as usize;

    let mut via_generic = vec![0u16; out_len];
    read_tile_step_into(&stack, 0, rect, step, spp, &mut via_generic)
        .expect("generic step read over a TiffStack");

    let mut via_native = vec![0u16; out_len];
    stack
        .read_tile_step_into(0, rect, step, &mut via_native)
        .expect("TiffStack's own step read");

    assert_eq!(via_generic, via_native);
}
