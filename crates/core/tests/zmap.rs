// SPDX-License-Identifier: GPL-3.0-or-later

//! Storage-level behaviour of the Z-Map through its on-disk form. Maps here
//! are built by hand -- no reference stack, no fixture except the one
//! 3-channel-refusal test. Files go under `env!("CARGO_TARGET_TMPDIR")`.
//! Unit-level behaviour (`ZMapInfo`, `validate_tile`, `parse_description`)
//! lives in `crates/core/src/zmap/mod.rs`'s own `tests` module instead.

mod common;

use std::num::NonZeroU32;
use std::path::PathBuf;

use focusforge_core::io::{Rect, TiffError, TiffSpec, TiffWriter};
use focusforge_core::tile::TilePlan;
use focusforge_core::zmap::{
    NO_SOURCE, ZMapError, ZMapInfo, ZMapReader, ZMapWriter, format_description,
};

fn scratch_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn mantis_64_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-64")
}

#[test]
fn roundtrip_is_bit_identical() {
    let path = scratch_path("zmap_roundtrip.tif");
    let width = 4;
    let height = 4;
    let frame_count = 4;
    let info = ZMapInfo::new(width, height, frame_count).unwrap();

    // A hand-built map containing NO_SOURCE, written row by row.
    let expected: Vec<u16> = vec![
        0, 1, 2, 3, 3, 2, 1, 0, NO_SOURCE, 0, 1, NO_SOURCE, 2, 3, 0, 1,
    ];
    assert_eq!(expected.len(), (width * height) as usize);

    let mut writer = ZMapWriter::create(&path, info).expect("create must succeed");
    for y in 0..height {
        let row = &expected[(y * width) as usize..((y + 1) * width) as usize];
        writer
            .write_tile(
                Rect {
                    x: 0,
                    y,
                    width,
                    height: 1,
                },
                row,
            )
            .expect("write_tile must succeed");
    }
    writer.finish().expect("finish must succeed");

    let reader = ZMapReader::open(&path).expect("open must succeed");
    assert_eq!(reader.dimensions(), (width, height));
    assert_eq!(reader.frame_count(), frame_count);

    // Read back tile by tile (two 2-row bands), never the whole map at once.
    let mut actual = vec![0u16; (width * height) as usize];
    for (band, chunk) in actual.chunks_mut((width * 2) as usize).enumerate() {
        let rect = Rect {
            x: 0,
            y: band as u32 * 2,
            width,
            height: 2,
        };
        reader
            .read_tile_into(rect, chunk)
            .expect("read_tile_into must succeed");
    }
    assert_eq!(actual, expected, "round trip must be bit-identical");
}

#[test]
fn stepped_read_is_the_step1_read_subsampled() {
    let path = scratch_path("zmap_stepped.tif");
    let width = 23;
    let height = 19;
    let frame_count = 7;
    let info = ZMapInfo::new(width, height, frame_count).unwrap();
    let map: Vec<u16> = (0..width * height)
        .map(|i| {
            if i % 5 == 0 {
                NO_SOURCE
            } else {
                (i % frame_count) as u16
            }
        })
        .collect();
    let mut writer = ZMapWriter::create(&path, info).unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            &map,
        )
        .unwrap();
    writer.finish().unwrap();

    let reader = ZMapReader::open(&path).unwrap();
    let rect = Rect {
        x: 2,
        y: 1,
        width: 21,
        height: 18,
    };
    let mut full = vec![0u16; (rect.width * rect.height) as usize];
    reader.read_tile_into(rect, &mut full).unwrap();
    for step in [2u32, 3, 7] {
        let expected: Vec<u16> = (0..rect.height)
            .step_by(step as usize)
            .flat_map(|r| {
                (0..rect.width)
                    .step_by(step as usize)
                    .map(move |c| (r * rect.width + c) as usize)
            })
            .map(|i| full[i])
            .collect();
        assert!(
            expected.contains(&NO_SOURCE),
            "step {step} must keep NO_SOURCE"
        );
        let mut out = vec![0u16; expected.len()];
        reader
            .read_tile_step_into(rect, NonZeroU32::new(step).unwrap(), &mut out)
            .unwrap();
        assert_eq!(out, expected, "step {step}");
    }
}

#[test]
fn description_is_the_pinned_grammar() {
    let path = scratch_path("zmap_description.tif");
    let frame_count = 61;
    let info = ZMapInfo::new(2, 2, frame_count).unwrap();
    let mut writer = ZMapWriter::create(&path, info).unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            &[0, 1, 2, NO_SOURCE],
        )
        .unwrap();
    writer.finish().unwrap();

    let header = focusforge_core::io::TiffHeader::open(&path).expect("must reopen");
    assert_eq!(
        header.image_description.as_deref(),
        Some("FocusForge zmap v1 frames=61"),
        "description must be exactly the pinned grammar"
    );
    assert_eq!(
        header.image_description.as_deref(),
        Some(format_description(frame_count).as_str()),
        "written description must equal format_description's own output"
    );
}

#[test]
fn rejects_file_without_a_parseable_description() {
    let path = scratch_path("zmap_no_description.tif");
    // Plain TiffWriter, spp = 1, description left at its default
    // ("FocusForge <version>") -- the trap the pinned-grammar prefix check
    // exists for: a bare "starts with FocusForge" test would pass here.
    let mut writer = TiffWriter::create(
        &path,
        TiffSpec {
            width: 2,
            height: 2,
            samples_per_pixel: 1,
            description: None,
        },
    )
    .unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            &[0, 0, 0, 0],
        )
        .unwrap();
    writer.finish().unwrap();

    match ZMapReader::open(&path) {
        Err(ZMapError::NotAZMap { description, .. }) => {
            assert!(
                description.starts_with("FocusForge "),
                "description: {description:?}"
            );
        }
        other => panic!("expected NotAZMap, got {other:?}"),
    }
}

#[test]
fn rejects_a_three_channel_file() {
    let path = mantis_64_dir().join("frames").join("00.tif");
    match ZMapReader::open(&path) {
        Err(ZMapError::NotSingleChannel {
            samples_per_pixel, ..
        }) => {
            assert_eq!(samples_per_pixel, 3);
        }
        other => panic!("expected NotSingleChannel, got {other:?}"),
    }
}

#[test]
fn rejects_file_with_out_of_range_index() {
    let path = scratch_path("zmap_bad_index.tif");
    // Built with a raw TiffWriter because ZMapWriter refuses this file.
    let width = 4;
    let height = 4;
    let mut writer = TiffWriter::create(
        &path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: 1,
            description: Some("FocusForge zmap v1 frames=4".to_string()),
        },
    )
    .unwrap();
    for y in 0..height {
        let row = if y == 2 {
            vec![0u16, 1, 97, 2]
        } else {
            vec![0u16, 1, 2, 3]
        };
        writer
            .write_tile(
                Rect {
                    x: 0,
                    y,
                    width,
                    height: 1,
                },
                &row,
            )
            .unwrap();
    }
    writer.finish().unwrap();

    let reader = ZMapReader::open(&path).expect("a bad index does not block opening");

    let mut buf = vec![0u16; (width * height) as usize];
    match reader.read_tile_into(
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        &mut buf,
    ) {
        Err(ZMapError::IndexOutOfRange {
            index: 97,
            frame_count: 4,
        }) => {}
        other => panic!("expected IndexOutOfRange from read_tile_into, got {other:?}"),
    }

    match reader.validate_all(1) {
        Err(ZMapError::IndexOutOfRange {
            index: 97,
            frame_count: 4,
        }) => {}
        other => panic!("expected IndexOutOfRange from validate_all, got {other:?}"),
    }
}

#[test]
fn tiled_write_covers_the_extent() {
    let path = scratch_path("zmap_tiled_write.tif");
    let width = 50;
    let height = 37;
    let frame_count = 5;
    // levels = 0 -> margin_for_levels(0) == 0, so interior == read and the
    // plan's tiles partition the extent exactly once, ragged at the edges.
    let plan = TilePlan::new(width, height, 20, 0).expect("plan must build");

    let info = ZMapInfo::new(width, height, frame_count).unwrap();
    let mut writer = ZMapWriter::create(&path, info).unwrap();
    for tile in plan.tiles() {
        let interior = tile.interior();
        assert_eq!(interior, tile.read(), "levels = 0 must give margin 0");
        let value = (tile.index() as u16) % frame_count as u16;
        let indices = vec![value; (interior.width * interior.height) as usize];
        writer.write_tile(interior, &indices).unwrap();
    }
    // finish() would refuse an incomplete frame -- succeeding here proves
    // the plan's tiles covered the extent exactly once.
    writer
        .finish()
        .expect("plan must cover the extent exactly once");

    let reader = ZMapReader::open(&path).unwrap();
    for tile in plan.tiles() {
        let interior = tile.interior();
        let expected_value = (tile.index() as u16) % frame_count as u16;
        let mut buf = vec![0u16; (interior.width * interior.height) as usize];
        reader.read_tile_into(interior, &mut buf).unwrap();
        assert!(
            buf.iter().all(|&v| v == expected_value),
            "tile {} mismatched",
            tile.index()
        );
    }
}

#[test]
fn incomplete_write_is_an_error() {
    let path = scratch_path("zmap_incomplete.tif");
    let width = 4;
    let height = 4;
    let info = ZMapInfo::new(width, height, 2).unwrap();
    let mut writer = ZMapWriter::create(&path, info).unwrap();
    // Write every row but the last.
    for y in 0..3u32 {
        writer
            .write_tile(
                Rect {
                    x: 0,
                    y,
                    width,
                    height: 1,
                },
                &[0u16; 4],
            )
            .unwrap();
    }
    match writer.finish() {
        Err(ZMapError::Tiff(TiffError::IncompleteFrame { row, .. })) => assert_eq!(row, 3),
        other => panic!("expected IncompleteFrame, got {other:?}"),
    }
}
