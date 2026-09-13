// SPDX-License-Identifier: GPL-3.0-or-later

//! `FusionConsumer` declaring both outputs at the crop rect's dimensions
//! (`crates/core/src/fuse/consumer.rs`), tested against the measured
//! `fixtures/mantis-512` crop rect rather than a synthetic one, so a real
//! tile partition is what proves `finish()`'s full-coverage check.

mod common;

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
    let options = TileSchedulerOptions {
        tile_size: 448,
        levels: 5,
        max_bytes_in_flight: 1 << 30,
        scratch_base: scheduler_scratch,
    };
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
