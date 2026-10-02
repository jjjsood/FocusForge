// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::path::PathBuf;

use focusforge_core::io::{
    ByteOrder, Rect, SegmentOrigin, TiffError, TiffHeader, TiffSegmentDecoder, TiffSegmentReader,
    TiffStack,
};

#[test]
fn every_fixture_segment_decodes_exactly() {
    let fixtures = common::fixtures_dir();
    let mut sets = std::fs::read_dir(&fixtures)
        .expect("fixture root")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("mantis-64-"))
        })
        .collect::<Vec<_>>();
    sets.sort();
    assert_eq!(
        sets.len(),
        24,
        "fixture matrix must cover every codec/layout combination"
    );

    for set in sets {
        for name in ["00.tif", "01.tif", "02.tif", "reference.tif"] {
            let compressed = if name == "reference.tif" {
                set.join(name)
            } else {
                set.join("frames").join(name)
            };
            let oracle = if name == "reference.tif" {
                fixtures.join("mantis-64").join(name)
            } else {
                fixtures.join("mantis-64").join("frames").join(name)
            };
            let reader = TiffSegmentReader::open(&compressed).expect("compressed fixture header");
            let stack = TiffStack::open_files(&[oracle]).expect("uncompressed oracle");
            assert!(reader.segment_count() > 0);
            for index in 0..reader.segment_count() {
                let segment = reader.decode(index).expect("fixture segment");
                let (x, y) = match segment.origin {
                    SegmentOrigin::Strip { y } => (0, y),
                    SegmentOrigin::Tile { x, y } => (x, y),
                };
                let expected = stack
                    .read_tile(
                        0,
                        Rect {
                            x,
                            y,
                            width: segment.valid_width,
                            height: segment.valid_height,
                        },
                    )
                    .expect("oracle samples");
                assert_eq!(
                    segment.samples,
                    expected.samples,
                    "{} segment {index}",
                    compressed.display()
                );
            }
        }
    }
}

#[test]
fn partial_segments_have_true_geometry() {
    let path = common::fixtures_dir()
        .join("mantis-64-deflate-p1-tile-ii")
        .join("frames")
        .join("00.tif");
    let reader = TiffSegmentReader::open(path).expect("tile fixture");
    let edge = reader
        .decode(reader.segment_count() - 1)
        .expect("edge tile");
    assert_eq!(edge.origin, SegmentOrigin::Tile { x: 48, y: 48 });
    assert_eq!((edge.stored_width, edge.stored_height), (48, 48));
    assert_eq!((edge.valid_width, edge.valid_height), (16, 16));
    assert_eq!(edge.samples.len(), 16 * 16 * 3);
}

#[test]
fn partial_strip_has_true_geometry() {
    let path = common::fixtures_dir()
        .join("mantis-64-deflate-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let reader = TiffSegmentReader::open(path).expect("strip fixture");
    let last = reader
        .decode(reader.segment_count() - 1)
        .expect("last strip");
    assert_eq!(last.origin, SegmentOrigin::Strip { y: 55 });
    assert_eq!((last.stored_width, last.stored_height), (64, 9));
    assert_eq!((last.valid_width, last.valid_height), (64, 9));
    assert_eq!(last.samples.len(), 64 * 9 * 3);
}

#[test]
fn generated_segments_preserve_samples() {
    let header = test_header(2, 2, 3, 2, ByteOrder::Big, vec![0]);
    let raw = [
        0, 1, 0, 2, 0, 3, 0, 3, 0, 3, 0, 3, // first row, predictor deltas
        0, 10, 0, 20, 0, 30, 0, 3, 0, 3, 0, 3, // second row resets predictor
    ];
    let decoder = TiffSegmentDecoder::new(&header, &raw).expect("valid generated segment");
    let segment = decoder.decode(0).expect("raw segment");
    assert_eq!(
        segment.samples,
        vec![1, 2, 3, 4, 5, 6, 10, 20, 30, 13, 23, 33]
    );
}

#[test]
fn generated_mono_and_rgb_predictor_wrap_in_both_byte_orders() {
    let cases = [
        (
            ByteOrder::Little,
            1,
            vec![u16::MAX, 1],
            vec![255, 255, 2, 0],
        ),
        (
            ByteOrder::Big,
            3,
            vec![u16::MAX, 1, 2, 1, 2, 3],
            vec![255, 255, 0, 1, 0, 2, 0, 2, 0, 1, 0, 1],
        ),
    ];
    for (order, channels, expected, raw) in cases {
        let header = test_header(2, 1, channels, 2, order, vec![0]);
        let decoder = TiffSegmentDecoder::new(&header, &raw).expect("generated layout");
        assert_eq!(
            decoder.decode(0).expect("generated decode").samples,
            expected
        );
    }
}

#[test]
fn generated_partial_tiles_crop_mono_and_rgb_predictor_samples() {
    let cases = [
        (
            ByteOrder::Little,
            1,
            vec![u16::MAX],
            vec![255, 255, 2, 0, 3, 0, 1, 0],
        ),
        (
            ByteOrder::Big,
            3,
            vec![u16::MAX, 1, 2],
            vec![
                255, 255, 0, 1, 0, 2, 0, 2, 0, 1, 0, 1, 0, 4, 0, 5, 0, 6, 0, 3, 0, 3, 0, 3,
            ],
        ),
    ];
    for (order, channels, expected, raw) in cases {
        let mut header = test_header(1, 1, channels, 2, order, vec![]);
        header.tile_width = Some(2);
        header.tile_length = Some(2);
        header.tile_offsets = vec![0];
        header.tile_byte_counts = vec![raw.len() as u64];
        header.strip_byte_counts.clear();
        let decoder = TiffSegmentDecoder::new(&header, &raw).expect("partial tile layout");
        let segment = decoder.decode(0).expect("partial tile decode");
        assert_eq!((segment.stored_width, segment.stored_height), (2, 2));
        assert_eq!((segment.valid_width, segment.valid_height), (1, 1));
        assert_eq!(segment.samples, expected);
    }
}

#[test]
fn invalid_layouts_are_named_errors() {
    let header = test_header(2, 2, 1, 1, ByteOrder::Little, vec![]);
    match TiffSegmentDecoder::new(&header, &[]) {
        Err(TiffError::SegmentLayout { .. }) => {}
        other => panic!("expected named layout error, got {other:?}"),
    }

    let header = test_header(2, 2, 1, 1, ByteOrder::Little, vec![0]);
    let decoder =
        TiffSegmentDecoder::new(&header, &[1, 0, 2, 0, 3, 0, 4, 0]).expect("valid layout");
    match decoder.decode(1) {
        Err(TiffError::SegmentIndex { index: 1, .. }) => {}
        other => panic!("expected named index error, got {other:?}"),
    }
}

#[test]
fn invalid_layout_matrix_and_capacity_overflow_are_errors_without_panics() {
    let mut cases = Vec::new();
    let mut zero_width = test_header(0, 1, 1, 1, ByteOrder::Little, vec![0]);
    zero_width.strip_byte_counts = vec![2];
    cases.push(zero_width);
    let mut bad_samples = test_header(1, 1, 1, 1, ByteOrder::Little, vec![0]);
    bad_samples.bits_per_sample = Box::new([16, 16]);
    cases.push(bad_samples);
    let mut missing_tile_length = test_header(1, 1, 1, 1, ByteOrder::Little, vec![]);
    missing_tile_length.tile_width = Some(1);
    cases.push(missing_tile_length);
    cases.push(test_header(1, 0, 1, 1, ByteOrder::Little, vec![0]));
    cases.push(test_header(1, 1, 0, 1, ByteOrder::Little, vec![0]));
    let mut missing_strip_counts = test_header(1, 1, 1, 1, ByteOrder::Little, vec![0]);
    missing_strip_counts.strip_byte_counts.clear();
    cases.push(missing_strip_counts);
    let mut missing_tile_offsets = test_header(1, 1, 1, 1, ByteOrder::Little, vec![]);
    missing_tile_offsets.tile_width = Some(1);
    missing_tile_offsets.tile_length = Some(1);
    missing_tile_offsets.tile_byte_counts = vec![2];
    cases.push(missing_tile_offsets);
    let mut mismatched_tile_counts = test_header(1, 1, 1, 1, ByteOrder::Little, vec![]);
    mismatched_tile_counts.tile_width = Some(1);
    mismatched_tile_counts.tile_length = Some(1);
    mismatched_tile_counts.tile_offsets = vec![0];
    cases.push(mismatched_tile_counts);
    for header in cases {
        let outcome = std::panic::catch_unwind(|| TiffSegmentDecoder::new(&header, &[]));
        assert!(outcome.is_ok(), "layout validation must not panic");
        assert!(matches!(
            outcome.expect("caught result"),
            Err(TiffError::SegmentLayout { .. })
        ));
    }

    let mut huge = test_header(1, 1, 1, 1, ByteOrder::Little, vec![0]);
    huge.width = u32::MAX;
    huge.height = u32::MAX / 2;
    huge.rows_per_strip = huge.height;
    huge.compression = 5;
    huge.strip_byte_counts = vec![0];
    let decoder =
        TiffSegmentDecoder::new(&huge, &[]).expect("huge dimensions are only decoded lazily");
    let outcome = std::panic::catch_unwind(|| decoder.decode(0));
    assert!(outcome.is_ok(), "overflow path must not panic");
    assert!(matches!(
        outcome.expect("caught result"),
        Err(TiffError::SegmentLayout { .. })
    ));
}

#[test]
fn refusals_name_the_value_found() {
    let fixtures = common::fixtures_dir();
    for (set, tag, value, name) in [
        ("mantis-64-lzw-p1-strip-ii", 259, 7, "Compression=7"),
        ("mantis-64-lzw-p2-strip-ii", 317, 3, "Predictor=3"),
        ("mantis-64-lzw-p1-strip-ii", 258, 8, "BitsPerSample"),
        ("mantis-64-lzw-p1-strip-ii", 284, 2, "PlanarConfiguration=2"),
        ("mantis-64-lzw-p1-strip-ii", 339, 3, "SampleFormat"),
    ] {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("refusal-{tag}.tif"));
        let mut bytes =
            std::fs::read(fixtures.join(set).join("frames").join("00.tif")).expect("fixture bytes");
        patch_short_tag(&mut bytes, tag, value);
        std::fs::write(&path, bytes).expect("patched fixture");
        let error = TiffSegmentReader::open(&path).expect_err("unsupported value must refuse");
        assert!(error.to_string().contains(name), "{error}");
    }
}

#[test]
fn corrupt_segments_are_named_errors() {
    let source = common::fixtures_dir()
        .join("mantis-64-deflate-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("truncated-deflate.tif");
    let mut bytes = std::fs::read(source).expect("fixture bytes");
    bytes.truncate(bytes.len() - 1);
    std::fs::write(&path, bytes).expect("truncated fixture");
    let reader = TiffSegmentReader::open(&path).expect("header survives truncated payload");
    let index = reader.segment_count() - 1;
    let error = reader
        .decode(index)
        .expect_err("truncated segment must fail");
    assert!(matches!(
        error,
        TiffError::SegmentPastEof { .. } | TiffError::SegmentStream { .. }
    ));
    assert!(error.to_string().contains(&path.display().to_string()));
    assert!(error.to_string().contains(&format!("segment {index}")));
}

#[test]
fn codec_corruption_and_length_mismatches_are_named_errors() {
    for set in ["mantis-64-lzw-p1-strip-ii", "mantis-64-deflate-p1-strip-ii"] {
        let path = common::fixtures_dir()
            .join(set)
            .join("frames")
            .join("00.tif");
        let header = TiffHeader::open(&path).expect("fixture header");
        let bytes = std::fs::read(&path).expect("fixture bytes");
        let mut truncated = header.clone();
        truncated.strip_byte_counts[0] -= 1;
        let decoder =
            TiffSegmentDecoder::new(&truncated, &bytes).expect("truncated table stays geometric");
        assert!(matches!(
            decoder.decode(0),
            Err(TiffError::SegmentStream { index: 0, .. })
        ));

        for width in [header.width / 2, header.width * 2] {
            let mut changed = header.clone();
            changed.width = width;
            let decoder = TiffSegmentDecoder::new(&changed, &bytes)
                .expect("changed expected length keeps layout");
            assert!(matches!(
                decoder.decode(0),
                Err(TiffError::SegmentLength { index: 0, .. })
            ));
        }
    }
}

#[test]
fn corrupt_codec_bytes_and_missing_terminators_after_output_are_errors() {
    for set in ["mantis-64-lzw-p1-strip-ii", "mantis-64-deflate-p1-strip-ii"] {
        let path = common::fixtures_dir()
            .join(set)
            .join("frames")
            .join("00.tif");
        let header = TiffHeader::open(&path).expect("fixture header");
        let bytes = std::fs::read(&path).expect("fixture bytes");
        let offset = header.strip_offsets[0] as usize;
        let count = header.strip_byte_counts[0] as usize;

        let mut corrupt = bytes.clone();
        corrupt[offset + count / 2] ^= 0xff;
        let decoder =
            TiffSegmentDecoder::new(&header, &corrupt).expect("corrupt bytes keep layout");
        assert!(matches!(
            decoder.decode(0),
            Err(TiffError::SegmentStream { index: 0, .. }
                | TiffError::SegmentLength { index: 0, .. })
        ));

        let mut missing_terminator = header.clone();
        missing_terminator.strip_byte_counts[0] -= 1;
        let decoder = TiffSegmentDecoder::new(&missing_terminator, &bytes)
            .expect("shortened stream keeps layout");
        assert!(matches!(
            decoder.decode(0),
            Err(TiffError::SegmentStream { index: 0, .. })
        ));
    }
}

fn patch_short_tag(bytes: &mut [u8], wanted_tag: u16, value: u16) {
    assert_eq!(&bytes[0..2], b"II", "test fixture must be little-endian");
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().expect("ifd offset")) as usize;
    let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().expect("entry count")) as usize;
    for index in 0..entries {
        let offset = ifd + 2 + index * 12;
        let tag = u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("tag"));
        if tag == wanted_tag {
            let field_type =
                u16::from_le_bytes(bytes[offset + 2..offset + 4].try_into().expect("type"));
            let count =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().expect("count"));
            assert_eq!(field_type, 3, "patched tag must use SHORT");
            let data = if count * 2 <= 4 {
                offset + 8
            } else {
                u32::from_le_bytes(
                    bytes[offset + 8..offset + 12]
                        .try_into()
                        .expect("value offset"),
                ) as usize
            };
            bytes[data..data + 2].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
    panic!("tag {wanted_tag} must exist in compressed fixture");
}

fn test_header(
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    predictor: u16,
    byte_order: ByteOrder,
    strip_offsets: Vec<u64>,
) -> TiffHeader {
    TiffHeader {
        path: PathBuf::from("generated.tif"),
        width,
        height,
        byte_order,
        ifd_offset: 0,
        bits_per_sample: vec![16; samples_per_pixel as usize].into_boxed_slice(),
        samples_per_pixel,
        compression: 1,
        photometric: 1,
        planar_config: 1,
        predictor,
        rows_per_strip: height,
        tile_width: None,
        tile_length: None,
        sample_format: vec![1; samples_per_pixel as usize].into_boxed_slice(),
        image_description: None,
        date_time_original: None,
        file_size: 0,
        strip_offsets,
        strip_byte_counts: vec![width as u64 * height as u64 * samples_per_pixel as u64 * 2],
        tile_offsets: vec![],
        tile_byte_counts: vec![],
        refusal: None,
    }
}
