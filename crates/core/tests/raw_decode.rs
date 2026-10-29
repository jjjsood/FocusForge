// SPDX-License-Identifier: GPL-3.0-or-later

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use focusforge_core::algorithm::TileReader;
use focusforge_core::io::Rect;
use focusforge_core::io::{FrameError, RawError, RawStack};
use rawler::decoders::{RawDecodeParams, WellKnownIFD};
use rawler::rawsource::RawSource;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::raw_dng::{DngSpec, write_dng};

#[test]
fn raw_reader_opens_an_empty_path_list_as_a_named_error() {
    let error = RawStack::open_files(&[]).expect_err("an empty RAW set must fail");
    assert!(error.to_string().contains("no RAW frames"));
}

#[test]
fn metadata_does_not_decode_pixels() {
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]);
    spec.capture_time = Some("2026:08:29 18:42:41".to_string());
    let dng = write_dng(&spec, true);
    let source = RawSource::new(dng.path()).expect("header-only source");
    let decoder = rawler::get_decoder(&source).expect("DNG decoder");
    let raw = decoder
        .ifd(WellKnownIFD::Raw)
        .expect("raw IFD query")
        .expect("DNG exposes raw IFD");
    assert_eq!(
        raw.get_entry(256_u16)
            .expect("width tag")
            .value
            .force_u32(0),
        8
    );
    assert_eq!(
        raw.get_entry(258_u16)
            .expect("depth tag")
            .value
            .force_u16(0),
        12
    );
    assert!(
        decoder
            .raw_metadata(&source, &RawDecodeParams::default())
            .is_ok()
    );
    assert!(
        decoder
            .raw_image(&source, &RawDecodeParams::default(), true)
            .is_err()
    );
    let reader = RawStack::open_files(&[PathBuf::from(dng.path())])
        .expect("payload-less DNG keeps metadata available");
    let metadata = reader.metadata(0).expect("metadata");
    assert_eq!(metadata.storage_bits, Some(12));
    assert_eq!(
        metadata.capture_time_original.as_deref(),
        Some("2026:08:29 18:42:41")
    );
    assert_eq!(
        metadata.black_levels.as_deref(),
        Some(&[64.0, 64.0, 64.0, 64.0][..])
    );
    assert_eq!(metadata.white_levels.as_deref(), Some(&[4095][..]));
    let mut output = vec![0; 3];
    assert!(
        reader
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut output,
            )
            .is_err()
    );
}

#[test]
fn metadata_uses_original_capture_time() {
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]);
    spec.capture_time = Some("2026:08:29 18:42:41".to_string());
    spec.serial_number = Some("body-123".to_string());
    let dng = write_dng(&spec, true);

    let reader =
        RawStack::open_files(&[PathBuf::from(dng.path())]).expect("metadata-only DNG must open");
    let metadata = reader.metadata(0).expect("metadata");
    assert_eq!(metadata.serial_number.as_deref(), Some("body-123"));
    assert_eq!(
        metadata
            .capture_time
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
        Some(1_788_028_961),
    );
}

#[test]
fn raw_stack_names_the_first_cfa_disagreement() {
    let first = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        true,
    );
    let second = write_dng(
        &DngSpec::new(12, 64, 4095, [2, 1, 1, 0], vec![64; 64]),
        true,
    );
    let error = RawStack::open_files(&[PathBuf::from(first.path()), PathBuf::from(second.path())])
        .expect_err("different CFA must refuse");
    assert!(matches!(
        &error,
        RawError::MetadataMismatch { property: "CFA", path, .. } if path == second.path()
    ));
    assert!(
        error
            .to_string()
            .contains(&second.path().display().to_string())
    );
}

#[test]
fn unreadable_raw_is_a_named_error() {
    let missing = PathBuf::from("/tmp/focusforge-raw-reader-missing.cr2");
    let error =
        RawStack::open_files(std::slice::from_ref(&missing)).expect_err("missing file must fail");
    assert!(matches!(&error, RawError::Io { path, .. } if path == &missing));
    assert!(
        error
            .to_string()
            .contains("focusforge-raw-reader-missing.cr2")
    );

    let mut malformed = tempfile::Builder::new()
        .suffix(".dng")
        .tempfile()
        .expect("temporary malformed DNG");
    use std::io::Write;
    malformed
        .write_all(b"not a TIFF header")
        .expect("write malformed DNG");
    let malformed_path = PathBuf::from(malformed.path());
    let error = RawStack::open_files(std::slice::from_ref(&malformed_path))
        .expect_err("malformed DNG must fail");
    assert!(matches!(&error, RawError::Decode { path, .. } if path == &malformed_path));
    assert!(error.to_string().contains("RAW decode failed"));

    let mut truncated_header = tempfile::Builder::new()
        .suffix(".dng")
        .tempfile()
        .expect("temporary truncated header");
    truncated_header
        .write_all(b"II\x2a\0\x08\0\0\0\x01")
        .expect("write truncated TIFF header");
    let truncated_path = PathBuf::from(truncated_header.path());
    let error = RawStack::open_files(std::slice::from_ref(&truncated_path))
        .expect_err("truncated header must fail before metadata");
    assert!(matches!(
        error,
        RawError::Decode { .. } | RawError::Panic { .. }
    ));

    let mut missing_width = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]);
    missing_width.omit_width = true;
    let malformed_header = write_dng(&missing_width, true);
    let error = RawStack::open_files(&[PathBuf::from(malformed_header.path())])
        .expect_err("missing mandatory raw header fact is malformed input");
    assert!(
        matches!(&error, RawError::Decode { detail, .. } if detail.contains("ImageWidth")),
        "unexpected malformed-header error: {error:?}"
    );

    let valid = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        true,
    );
    assert!(RawStack::open_files(&[PathBuf::from(valid.path())]).is_ok());
}

#[test]
fn synthetic_dng_metadata_roundtrips() {
    for (bits, cfa) in [
        (12, [0, 1, 1, 2]),
        (12, [1, 0, 2, 1]),
        (14, [1, 2, 0, 1]),
        (14, [2, 1, 1, 0]),
    ] {
        let mut spec = DngSpec::new(bits, 128, (1 << bits) - 1, cfa, vec![128; 64]);
        spec.active_area = Some([1, 1, 7, 7]);
        spec.crop = Some([1, 2, 4, 3]);
        let dng = write_dng(&spec, true);
        let reader =
            RawStack::open_files(&[PathBuf::from(dng.path())]).expect("header-only DNG must open");
        let metadata = reader.metadata(0).expect("DNG metadata");
        let shifted_cfa = [cfa[3], cfa[2], cfa[1], cfa[0]];
        assert_eq!(reader.dimensions(), (4, 3));
        assert_eq!(metadata.storage_bits, Some(bits));
        assert_eq!(metadata.decoder_bits, None);
        assert_eq!(metadata.black_levels.as_deref(), Some(&[128.0; 4][..]));
        assert_eq!(
            metadata.white_levels.as_deref(),
            Some(&[((1 << bits) - 1) as u32][..])
        );
        assert_eq!(metadata.cfa.as_deref(), Some(&shifted_cfa[..]));
        assert_eq!((metadata.crop_x, metadata.crop_y), (2, 3));
        assert_eq!(
            metadata.color_matrix.as_deref(),
            Some(&[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0][..])
        );
    }
}

#[test]
fn active_area_geometry_matches_development() {
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]);
    spec.active_area = Some([1, 1, 7, 7]);
    let dng = write_dng(&spec, false);
    let reader = RawStack::open_files(&[PathBuf::from(dng.path())]).expect("ActiveArea DNG opens");
    assert_eq!(reader.dimensions(), (6, 6));
    let mut output = vec![0; 6 * 6 * 3];
    reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 6,
                height: 6,
            },
            &mut output,
        )
        .expect("ActiveArea DNG develops at metadata geometry");
}

#[test]
fn develop_is_sixteen_bit_end_to_end() {
    let black = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        false,
    );
    let white = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![4095; 64]),
        false,
    );
    let rect = Rect {
        x: 0,
        y: 0,
        width: 8,
        height: 8,
    };
    for (path, expected) in [(black.path(), 0_u16), (white.path(), u16::MAX)] {
        let reader = RawStack::open_files(&[PathBuf::from(path)]).expect("DNG opens");
        let mut output = vec![0; 8 * 8 * 3];
        reader
            .read_tile_into(0, rect, &mut output)
            .expect("DNG develops");
        assert!(output.iter().all(|sample| *sample == expected));
    }
    let mut ramp = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], Vec::new());
    ramp.width = 32;
    ramp.height = 16;
    ramp.samples = (0..512)
        .map(|index| 64 + (index * (4095 - 64) / 511) as u16)
        .collect();
    let ramp = write_dng(&ramp, false);
    let reader = RawStack::open_files(&[PathBuf::from(ramp.path())]).expect("DNG opens");
    let mut output = vec![0; 32 * 16 * 3];
    reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 16,
            },
            &mut output,
        )
        .expect("DNG develops");
    assert!(
        output
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 256
    );
}

#[test]
fn tiles_partition_the_developed_frame() {
    let dng = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], (64..128).collect()),
        false,
    );
    let reader = RawStack::open_files(&[PathBuf::from(dng.path())]).expect("DNG opens");
    let mut full = vec![0; 8 * 8 * 3];
    reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
            },
            &mut full,
        )
        .expect("full frame");
    let mut assembled = vec![0; full.len()];
    for y in [0, 4] {
        for x in [0, 4] {
            let mut tile = vec![0; 4 * 4 * 3];
            reader
                .read_tile_into(
                    0,
                    Rect {
                        x,
                        y,
                        width: 4,
                        height: 4,
                    },
                    &mut tile,
                )
                .expect("partition tile");
            for row in 0..4 {
                let source = row as usize * 4 * 3;
                let destination = ((y + row) * 8 + x) as usize * 3;
                assembled[destination..destination + 4 * 3]
                    .copy_from_slice(&tile[source..source + 4 * 3]);
            }
        }
    }
    assert_eq!(assembled, full);
    let mut output = vec![0; 3];
    assert!(
        reader
            .read_tile_into(
                0,
                Rect {
                    x: 8,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut output,
            )
            .is_err()
    );
}

#[test]
fn cache_retains_multiple_decoded_frames_within_budget() {
    let root = tempfile::tempdir().expect("temporary directory");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 256 * 256]);
    spec.width = 256;
    spec.height = 256;
    let source = write_dng(&spec, false);
    let first = root.path().join("00.dng");
    let second = root.path().join("01.dng");
    std::fs::copy(source.path(), &first).expect("copy first frame");
    std::fs::copy(source.path(), &second).expect("copy second frame");
    let peak = 256_u64 * 256 * 30 + std::fs::metadata(&first).expect("source size").len();
    let retained = 256_u64 * 256 * 3 * 2;
    let budget = peak + 2 * retained;
    assert!(
        budget < 2 * peak,
        "budget must exclude a second decode peak"
    );
    let reader = RawStack::open_files_with_budget(&[first.clone(), second.clone()], budget)
        .expect("metadata opens");
    for frame in [0, 1] {
        reader
            .read_tile_into(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut [0; 3],
            )
            .expect("develop frame");
    }
    std::fs::remove_file(&first).expect("remove first source");
    std::fs::remove_file(&second).expect("remove second source");
    for frame in [0, 1] {
        reader
            .read_tile_into(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut [0; 3],
            )
            .expect("cached frame remains available");
    }
}

#[test]
fn decoded_frames_stay_within_budget() {
    let dng = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        false,
    );
    let reader = RawStack::open_files_with_budget(&[PathBuf::from(dng.path())], 1)
        .expect("metadata fits without decode");
    let mut output = vec![0; 3];
    assert!(
        reader
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut output,
            )
            .is_err()
    );
    let source_excluded =
        RawStack::open_files_with_budget(&[PathBuf::from(dng.path())], 8 * 8 * 30)
            .expect("metadata fits without decode");
    let error = source_excluded
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        )
        .expect_err("admission includes the mapped source and every peak buffer");
    let expected_peak = 8 * 8 * 30 + std::fs::metadata(dng.path()).expect("DNG size").len();
    assert!(matches!(
        error,
        focusforge_core::algorithm::AlgorithmError::Frame(FrameError::Raw(
            RawError::CacheTooSmall { required, .. }
        )) if required == expected_peak
    ));
    let withheld = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        true,
    );
    let paths = vec![PathBuf::from(withheld.path()); 16];
    let unopened = RawStack::open_files(&paths).expect("opening does not decode payloads");
    assert!(
        unopened
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut output,
            )
            .is_err()
    );
    let full = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        false,
    );
    let one_frame_budget =
        8 * 8 * 30 + std::fs::metadata(full.path()).expect("full DNG size").len();
    let recovered = RawStack::open_files_with_budget(
        &[PathBuf::from(full.path()), PathBuf::from(withheld.path())],
        one_frame_budget,
    )
    .expect("both headers open without decode");
    assert!(
        recovered
            .read_tile_into(
                1,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut output,
            )
            .is_err()
    );
    recovered
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        )
        .expect("failed decode releases the next frame admission");
    let reader = std::sync::Arc::new(
        RawStack::open_files_with_budget(&vec![PathBuf::from(full.path()); 16], one_frame_budget)
            .expect("metadata fits for every frame"),
    );
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let reader = std::sync::Arc::clone(&reader);
            scope.spawn(move || {
                let mut tile = vec![0; 3];
                reader
                    .read_tile_into(
                        0,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 1,
                            height: 1,
                        },
                        &mut tile,
                    )
                    .expect("same-frame readers share one bounded decode");
            });
        }
        for frame in 0..16 {
            let reader = std::sync::Arc::clone(&reader);
            scope.spawn(move || {
                let mut tile = vec![0; 3];
                reader
                    .read_tile_into(
                        frame,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 1,
                            height: 1,
                        },
                        &mut tile,
                    )
                    .expect("serialized decode stays within one-frame budget");
            });
        }
    });

    let mut overflow_spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]);
    overflow_spec.header_dimensions = Some([u32::MAX, u32::MAX]);
    let overflow = write_dng(&overflow_spec, true);
    let reader = RawStack::open_files(&[PathBuf::from(overflow.path())])
        .expect("header geometry opens without decoding");
    let error = reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        )
        .expect_err("overflowed geometry must fail before decode");
    assert!(error.to_string().contains("overflows cache accounting"));
}

#[test]
fn sigma_pin_rejects_a_replaced_fixture() {
    use std::io::Write;

    let mut replacement = tempfile::Builder::new()
        .suffix(".x3f")
        .tempfile()
        .expect("temporary Sigma replacement");
    replacement
        .write_all(b"replacement is not the pinned Sigma frame")
        .expect("write replacement");
    assert!(
        std::panic::catch_unwind(|| {
            assert_pinned_file(
                replacement.path(),
                "44508f7df4191aacaecb6461e1841dae002e5b71a4f95a00aa3f52d1c6d6bc5f",
            );
        })
        .is_err()
    );
}

#[test]
#[ignore]
fn canon_dummy_metadata_has_no_pixel_buffer() {
    let Some(samples) = std::env::var_os("FOCUSFORGE_RAW_SAMPLES") else {
        eprintln!("skip: FOCUSFORGE_RAW_SAMPLES is not set");
        return;
    };
    let path = PathBuf::from(samples).join("canon-eos-20d.cr2");
    assert!(
        path.is_file(),
        "configured Canon sample is missing: {}",
        path.display()
    );
    let source = RawSource::new(&path).expect("Canon source");
    let decoder = rawler::get_decoder(&source).expect("Canon decoder");
    let image = decoder
        .raw_image(&source, &RawDecodeParams::default(), true)
        .expect("Canon dummy metadata");
    assert!(matches!(image.data, rawler::RawImageData::Integer(ref data) if data.is_empty()));
    let reader = RawStack::open_files(&[path]).expect("Canon opens without pixel decode");
    let metadata = reader.metadata(0).expect("Canon metadata");
    assert_eq!((metadata.raw_width, metadata.raw_height), (3596, 2360));
    assert_eq!(
        (metadata.output_width, metadata.output_height),
        (3504, 2336)
    );
    assert_eq!(reader.dimensions(), (3504, 2336));
}

#[test]
#[ignore]
fn canon_develops_after_lazy_open() {
    let Some(samples) = std::env::var_os("FOCUSFORGE_RAW_SAMPLES") else {
        eprintln!("skip: FOCUSFORGE_RAW_SAMPLES is not set");
        return;
    };
    let path = PathBuf::from(samples).join("canon-eos-20d.cr2");
    assert!(
        path.is_file(),
        "configured Canon sample is missing: {}",
        path.display()
    );
    let reader = RawStack::open_files(&[path]).expect("Canon opens lazily");
    let mut output = vec![0; 3];
    reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        )
        .expect("Canon develops to RGB");
}

#[test]
#[ignore]
fn local_canon_cr3_smoke() {
    let Some(samples) = std::env::var_os("FOCUSFORGE_RAW_LOCAL_SAMPLES") else {
        eprintln!("skip: FOCUSFORGE_RAW_LOCAL_SAMPLES is not set");
        return;
    };
    for file in [
        "canon-r5-mark-ii/canon-eos-r5-mark-ii_2026-09-26_125359.cr3",
        "canon-r7/canon-eos-r7_2025-01-10_220452.cr3",
    ] {
        let path = PathBuf::from(&samples).join(file);
        assert!(
            path.is_file(),
            "configured local RAW sample is missing: {}",
            path.display()
        );
        let reader = RawStack::open_files(std::slice::from_ref(&path)).expect("CR3 metadata opens");
        let metadata = reader.metadata(0).expect("CR3 metadata");
        eprintln!(
            "{file}: make={} model={} raw={}x{} output={}x{} bits={:?}",
            metadata.make,
            metadata.model,
            metadata.raw_width,
            metadata.raw_height,
            metadata.output_width,
            metadata.output_height,
            metadata.decoder_bits
        );
        let mut output = vec![0; 3];
        match reader.read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        ) {
            Ok(()) => eprintln!("{file}: first tile developed"),
            Err(error) => eprintln!("{file}: first tile refused: {error}"),
        }
        let reader =
            RawStack::open_files_with_budget(std::slice::from_ref(&path), 2 * 1024 * 1024 * 1024)
                .expect("CR3 opens with 2GiB cache budget");
        let started = std::time::Instant::now();
        reader
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut output,
            )
            .expect("CR3 first tile develops with 2GiB cache budget");
        eprintln!(
            "{file}: 2GiB first tile developed in {:?}",
            started.elapsed()
        );
    }
}

#[test]
#[ignore]
fn supported_corpus_dummy_metadata_has_no_pixel_buffers() {
    let Some(samples) = std::env::var_os("FOCUSFORGE_RAW_SAMPLES") else {
        eprintln!("skip: FOCUSFORGE_RAW_SAMPLES is not set");
        return;
    };
    let samples = PathBuf::from(samples);
    for file in [
        "canon-eos-20d.cr2",
        "fujifilm-x-s10.raf",
        "panasonic-dmc-lx7.rw2",
        "sony-ilce-7s.arw",
    ] {
        let path = samples.join(file);
        assert!(
            path.is_file(),
            "configured RAW sample is missing: {}",
            path.display()
        );
        let reader = RawStack::open_files(&[path]).expect("dummy metadata opens without pixels");
        assert!(reader.metadata(0).expect("metadata").decoder_bits.is_some());
    }
    let nikon = samples.join("nikon-d1h.nef");
    let error = RawStack::open_files(&[nikon]).expect_err("unsupported Nikon is named");
    assert!(error.to_string().contains("NIKON D1H"));
}

#[test]
#[ignore]
fn corpus_matches_manifest() {
    let (Some(samples), Some(expectations)) = (
        std::env::var_os("FOCUSFORGE_RAW_SAMPLES"),
        std::env::var_os("FOCUSFORGE_RAW_EXPECTATIONS"),
    ) else {
        eprintln!("skip: FOCUSFORGE_RAW_SAMPLES and FOCUSFORGE_RAW_EXPECTATIONS must be set");
        return;
    };
    let samples = PathBuf::from(samples);
    let expectations = PathBuf::from(expectations);
    assert!(
        samples.is_dir(),
        "configured RAW samples are missing: {}",
        samples.display()
    );
    assert!(
        expectations.is_file(),
        "configured RAW expectations are missing: {}",
        expectations.display()
    );
    let pins_path = expectations
        .parent()
        .expect("expectations parent")
        .join("pins.json");
    let expectations: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&expectations).expect("read RAW expectations"))
            .expect("parse RAW expectations");
    let pins: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pins_path).expect("read RAW pins"))
            .expect("parse RAW pins");
    assert_corpus_pins(&samples, &pins);
    let sigma = samples.join("sigma-dp1.x3f");
    assert!(
        sigma.is_file(),
        "configured refusal sample is missing: {}",
        sigma.display()
    );
    let sigma_error = RawStack::open_files(std::slice::from_ref(&sigma))
        .expect_err("configured Sigma refusal is named");
    assert!(sigma_error.to_string().contains("sigma-dp1.x3f"));
    for (name, file) in [
        ("canon-eos-20d", "canon-eos-20d.cr2"),
        ("fujifilm-x-s10", "fujifilm-x-s10.raf"),
        ("panasonic-dmc-lx7", "panasonic-dmc-lx7.rw2"),
        ("sony-ilce-7s", "sony-ilce-7s.arw"),
    ] {
        let path = samples.join(file);
        let reader = RawStack::open_files(std::slice::from_ref(&path)).expect("lazy RAW metadata");
        let metadata = reader.metadata(0).expect("RAW metadata");
        let expected = &expectations["frames"][name];
        compare_dimensions(name, metadata.raw_width, metadata.raw_height, expected);
        compare_cfa(name, metadata.cfa.as_deref(), &expected_cfa(expected));
        report_native_fields(name, metadata, expected);
        assert_real_comparator_controls(name, metadata, expected);
        let mut tile = vec![0; 3];
        reader
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                &mut tile,
            )
            .expect("configured RAW develops");
    }
    for (name, file) in [
        (
            "blackmagic-micro-cinema-camera",
            "blackmagic-micro-cinema-camera.dng",
        ),
        ("nikon-d1h", "nikon-d1h.nef"),
    ] {
        let path = samples.join(file);
        assert!(
            path.is_file(),
            "configured refusal sample is missing: {}",
            path.display()
        );
        let result = RawStack::open_files(std::slice::from_ref(&path)).and_then(|reader| {
            let mut tile = vec![0; 3];
            reader
                .read_tile_into(
                    0,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    },
                    &mut tile,
                )
                .map_err(|error| focusforge_core::io::RawError::Decode {
                    path: path.clone(),
                    detail: error.to_string(),
                })
        });
        let error = result.expect_err("configured refusal is named");
        assert!(error.to_string().contains(file));
        if name == "nikon-d1h" {
            assert!(
                matches!(
                    &error,
                    RawError::Unsupported { make, model, .. } if make == "NIKON CORPORATION" && model == "NIKON D1H"
                ),
                "unexpected Nikon refusal: {error:?}"
            );
        }
        assert!(expectations["frames"][name].is_object());
    }
}

fn assert_pinned_file(path: &std::path::Path, expected: &str) {
    let digest = Sha256::digest(std::fs::read(path).expect("read pinned RAW fixture"));
    assert_eq!(
        format!("{digest:x}"),
        expected,
        "{} SHA-256 differs",
        path.display()
    );
}

fn assert_corpus_pins(samples: &std::path::Path, pins: &serde_json::Value) {
    for pin in pins["samples"].as_array().expect("pinned RAW samples") {
        let id = pin["id"].as_str().expect("pin id");
        let extension = pin["ext"]
            .as_str()
            .expect("pin extension")
            .to_ascii_lowercase();
        let path = samples.join(format!("{id}{extension}"));
        assert!(
            path.is_file(),
            "configured pinned sample is missing: {}",
            path.display()
        );
        assert_pinned_file(&path, pin["sha256"].as_str().expect("pin SHA-256"));
    }
}

fn compare_dimensions(name: &str, width: u32, height: u32, expected: &serde_json::Value) {
    assert_eq!(
        u64::from(width),
        expected["raw_width"]["value"]
            .as_u64()
            .expect("expected raw width"),
        "{name}: raw width"
    );
    assert_eq!(
        u64::from(height),
        expected["raw_height"]["value"]
            .as_u64()
            .expect("expected raw height"),
        "{name}: raw height"
    );
}

fn expected_cfa(expected: &serde_json::Value) -> Vec<u8> {
    expected["cfa_pattern"]["value"]["channels"]
        .as_array()
        .expect("expected CFA channels")
        .iter()
        .map(|channel| match channel.as_str().expect("CFA channel") {
            "R" => 0,
            "G" => 1,
            "B" => 2,
            other => panic!("unsupported manifest CFA channel: {other}"),
        })
        .collect()
}

fn compare_cfa(name: &str, actual: Option<&[u8]>, expected: &[u8]) {
    assert_eq!(actual, Some(expected), "{name}: CFA");
}

fn report_native_fields(
    name: &str,
    metadata: &focusforge_core::io::RawMetadata,
    expected: &serde_json::Value,
) {
    let storage_bits = expected["bits_per_sample"]["value"]
        .as_u64()
        .expect("storage bits");
    if metadata.decoder_bits.map(|bits| bits as u64) != Some(storage_bits) {
        eprintln!(
            "{name}: decoder_bits={:?} (decoder unpack domain), oracle_storage_bits={storage_bits} (LibRaw unpack storage); distinct quantities",
            metadata.decoder_bits
        );
    }
    let black = &expected["black_level"]["value"];
    let pattern_offsets = black["pattern_offsets"]
        .as_array()
        .expect("black pattern offsets");
    let pattern_rows = black["pattern_rows"].as_u64().expect("black pattern rows");
    let pattern_cols = black["pattern_cols"]
        .as_u64()
        .expect("black pattern columns");
    assert_eq!(
        pattern_offsets.len(),
        (pattern_rows * pattern_cols) as usize,
        "{name}: every black pattern offset is accounted for"
    );
    if pattern_offsets.len() > 4 {
        eprintln!(
            "{name}: oracle black pattern has {} offsets across {}x{} raw coordinates: {:?}",
            pattern_offsets.len(),
            pattern_rows,
            pattern_cols,
            pattern_offsets
        );
    }
    compare_black(
        name,
        metadata.black_levels.as_deref(),
        &expected_black_levels(black),
    );
    let white = &expected["white_level"]["value"];
    let linear = white["linear_max"].as_array().expect("linear max");
    let expected_white = linear
        .iter()
        .filter_map(|value| value.as_u64())
        .find(|value| *value > 0)
        .unwrap_or_else(|| white["maximum"].as_u64().expect("white maximum"));
    compare_white(
        name,
        metadata.white_levels.as_deref(),
        expected_white as u32,
        white,
    );
    let crop = &expected["crop_margins"]["value"];
    let oracle_crop = (
        crop["left"].as_u64(),
        crop["top"].as_u64(),
        crop["output_width"].as_u64(),
        crop["output_height"].as_u64(),
    );
    let decoder_crop = (
        u64::from(metadata.crop_x),
        u64::from(metadata.crop_y),
        u64::from(metadata.output_width),
        u64::from(metadata.output_height),
    );
    if decoder_crop
        != (
            oracle_crop.0.unwrap(),
            oracle_crop.1.unwrap(),
            oracle_crop.2.unwrap(),
            oracle_crop.3.unwrap(),
        )
    {
        eprintln!(
            "{name}: decoder_crop={decoder_crop:?} (decoder default-crop raw coordinates), oracle_crop={oracle_crop:?} (LibRaw visible bounds); distinct crop policies"
        );
    }
    let oracle_matrix: Vec<f32> = expected["color_matrix"]["value"]
        .as_array()
        .expect("matrix rows")
        .iter()
        .flat_map(|row| {
            row.as_array()
                .expect("matrix row")
                .iter()
                .map(|value| value.as_f64().expect("matrix value") as f32)
        })
        .collect();
    if let Some(matrix) = &metadata.color_matrix {
        let delta = matrix_delta(matrix, &oracle_matrix);
        assert!(
            delta <= 0.0002,
            "{name}: matrix delta {delta} exceeds 0.0002"
        );
        eprintln!(
            "{name}: matrix max_abs_delta={delta:.8} (decoder camera-table XYZ-to-camera vs LibRaw table XYZ-to-camera; no file-matrix equality claim)"
        );
    }
}

fn expected_black_levels(black: &serde_json::Value) -> Vec<f32> {
    let base = black["base"].as_f64().expect("black base") as f32;
    let pattern = black["pattern_offsets"]
        .as_array()
        .expect("black pattern offsets");
    if pattern.is_empty() {
        black["channel_offsets"]
            .as_array()
            .expect("black offsets")
            .iter()
            .map(|offset| base + offset.as_f64().expect("black offset") as f32)
            .collect()
    } else {
        pattern
            .iter()
            .map(|offset| base + offset.as_f64().expect("black pattern offset") as f32)
            .collect()
    }
}

fn compare_black(name: &str, actual: Option<&[f32]>, expected: &[f32]) {
    if name == "fujifilm-x-s10" {
        assert_eq!(actual, Some(expected), "{name}: 6x6 black offsets");
    } else if actual != Some(expected) {
        eprintln!(
            "{name}: decoder_black={actual:?} (decoder raw-code domain), oracle_effective_black={expected:?} (LibRaw raw-code domain); decoder-native level semantics"
        );
    }
}

fn assert_real_comparator_controls(
    name: &str,
    metadata: &focusforge_core::io::RawMetadata,
    expected: &serde_json::Value,
) {
    let mut wrong_dimensions = expected.clone();
    wrong_dimensions["raw_width"]["value"] = serde_json::json!(metadata.raw_width + 1);
    assert_rejected(|| {
        compare_dimensions(
            name,
            metadata.raw_width,
            metadata.raw_height,
            &wrong_dimensions,
        );
    });
    let mut wrong_cfa = expected_cfa(expected);
    wrong_cfa[0] = (wrong_cfa[0] + 1) % 3;
    assert_rejected(|| compare_cfa(name, metadata.cfa.as_deref(), &wrong_cfa));
    if name == "fujifilm-x-s10" {
        let mut wrong_black = expected_black_levels(&expected["black_level"]["value"]);
        wrong_black[0] = 1000.0;
        assert_rejected(|| compare_black(name, metadata.black_levels.as_deref(), &wrong_black));
    }
    if name == "sony-ilce-7s" {
        let white = &expected["white_level"]["value"];
        assert_rejected(|| compare_white(name, metadata.white_levels.as_deref(), 1, white));
    }
    if let Some(matrix) = &metadata.color_matrix {
        let mut wrong_matrix = matrix.clone();
        wrong_matrix[0] += 0.001;
        assert_rejected(|| {
            assert!(
                matrix_delta(matrix, &wrong_matrix) <= 0.0002,
                "{name}: matrix similarity"
            );
        });
    }
}

fn compare_white(name: &str, actual: Option<&[u32]>, expected: u32, oracle: &serde_json::Value) {
    let actual = actual.and_then(|levels| levels.first()).copied();
    if name == "sony-ilce-7s" {
        assert_eq!(
            actual,
            Some(expected),
            "{name}: decoder raw-code white must equal LibRaw selected {}",
            oracle["selected"].as_str().expect("white selection")
        );
    } else if actual != Some(expected) {
        eprintln!(
            "{name}: decoder_white={actual:?} (decoder raw-code domain), oracle_selected_white={expected} (LibRaw {}); distinct selection semantics",
            oracle["selected"].as_str().expect("white selection")
        );
    }
}

fn assert_rejected(check: impl FnOnce() + std::panic::UnwindSafe) {
    assert!(std::panic::catch_unwind(check).is_err());
}

#[test]
fn corpus_comparator_negative_controls_differ() {
    let expected = serde_json::json!({
        "raw_width": { "value": 640 },
        "raw_height": { "value": 480 },
        "cfa_pattern": { "value": { "channels": ["R", "G", "G", "B"] } }
    });
    let mut wrong_dimensions = expected.clone();
    wrong_dimensions["raw_width"]["value"] = serde_json::json!(641);
    assert_rejected(|| compare_dimensions("control", 640, 480, &wrong_dimensions));
    assert_rejected(|| compare_cfa("control", Some(&[0, 1, 1, 2]), &[1, 0, 2, 1]));
    assert_rejected(|| compare_black("fujifilm-x-s10", Some(&[1022.0; 36]), &[1000.0; 36]));
    let sony_white = serde_json::json!({ "selected": "linear_max" });
    assert_rejected(|| compare_white("sony-ilce-7s", Some(&[15360]), 15361, &sony_white));
    assert!(
        matrix_delta(&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0]) > 0.0002,
        "wrong matrix must exceed table similarity bound"
    );
}

fn matrix_delta(left: &[f32], right: &[f32]) -> f32 {
    let right = if left.len() == 9 && right.len() == 12 && right[9..] == [0.0, 0.0, 0.0] {
        &right[..9]
    } else {
        right
    };
    assert_eq!(left.len(), right.len(), "matrix shapes must match");
    left.iter()
        .zip(right)
        .map(|(left, right)| (left - right).abs())
        .fold(0.0, f32::max)
}

/// A frame decode runs the decoder's own parallel loops while it holds the
/// decode lock. A second read reaching the pool at any point of that decode
/// must not end up running on a worker the decode waits for.
#[test]
fn a_pool_read_arriving_during_a_decode_finishes() {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    const SIDE: u32 = 512;
    let mut spec = DngSpec::new(
        12,
        64,
        4095,
        [0, 1, 1, 2],
        vec![512; (SIDE * SIDE) as usize],
    );
    spec.width = SIDE;
    spec.height = SIDE;
    let dng = write_dng(&spec, false);
    let rect = Rect {
        x: 0,
        y: 0,
        width: SIDE,
        height: 1,
    };
    for arrival_ms in (0..120).step_by(2) {
        let reader = Arc::new(
            RawStack::open_files(&[PathBuf::from(dng.path()), PathBuf::from(dng.path())])
                .expect("DNG pair opens"),
        );
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .expect("test pool"),
        );
        let (done, finished) = mpsc::channel();
        let read = move |reader: Arc<RawStack>, frame: usize, done: mpsc::Sender<()>| {
            reader
                .read_tile_into(frame, rect, &mut vec![0; SIDE as usize * 3])
                .expect("frame develops");
            let _ = done.send(());
        };
        {
            let (reader, pool, done) = (Arc::clone(&reader), Arc::clone(&pool), done.clone());
            std::thread::spawn(move || pool.install(|| read(reader, 0, done)));
        }
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(arrival_ms));
            pool.spawn(move || read(reader, 1, done));
        });
        for _ in 0..2 {
            finished
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| {
                    panic!("a read arriving {arrival_ms} ms into a decode never finishes")
                });
        }
    }
}

/// As-shot multipliers `[2.0, 1.0, 1.6]`.
const AS_SHOT_NEUTRAL: [f32; 3] = [0.5, 1.0, 0.625];
const XYZ_TO_CAMERA: [f32; 9] = [0.72, -0.21, -0.08, -0.31, 1.12, 0.12, -0.02, 0.19, 0.55];

fn tinted_spec(samples: Vec<u16>, width: u32, height: u32, cfa: [u8; 4]) -> DngSpec {
    let mut spec = DngSpec::new(12, 64, 4095, cfa, samples);
    spec.width = width;
    spec.height = height;
    spec.as_shot_neutral = Some(AS_SHOT_NEUTRAL);
    spec.color_matrix = Some(XYZ_TO_CAMERA);
    spec
}

fn tinted_look() -> focusforge_core::io::raw::LookTransform {
    let matrix = [
        [XYZ_TO_CAMERA[0], XYZ_TO_CAMERA[1], XYZ_TO_CAMERA[2]],
        [XYZ_TO_CAMERA[3], XYZ_TO_CAMERA[4], XYZ_TO_CAMERA[5]],
        [XYZ_TO_CAMERA[6], XYZ_TO_CAMERA[7], XYZ_TO_CAMERA[8]],
    ];
    focusforge_core::io::raw::LookTransform::new([2.0, 1.0, 1.6, f32::NAN], matrix)
}

fn quantize(value: f64) -> u16 {
    (value.clamp(0.0, 1.0) * f64::from(u16::MAX) + 0.5) as u16
}

fn read_whole(reader: &RawStack) -> Vec<u16> {
    let (width, height) = reader.dimensions();
    let mut out = vec![0; width as usize * height as usize * 3];
    reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            &mut out,
        )
        .expect("frame develops");
    out
}

/// A flat field's camera-linear value: its sample scaled between the black
/// and white levels of [`tinted_spec`].
fn flat_field(level: u16) -> (Vec<u16>, f64) {
    (
        vec![level; 64],
        f64::from(level - 64) / f64::from(4095 - 64),
    )
}

#[test]
fn camera_space_flat_field_is_the_look_of_the_level_scaled_sample() {
    let (samples, linear) = flat_field(900);
    let dng = write_dng(&tinted_spec(samples, 8, 8, [0, 1, 1, 2]), false);
    let reader = RawStack::open_files(&[PathBuf::from(dng.path())]).expect("DNG opens");
    let expected = tinted_look().forward([linear; 3]).map(quantize);
    for pixel in read_whole(&reader).chunks_exact(3) {
        for channel in 0..3 {
            assert!(
                pixel[channel].abs_diff(expected[channel]) <= 1,
                "{pixel:?} against {expected:?}"
            );
        }
    }
}

#[test]
fn camera_space_one_channel_over_white_leaves_the_others_alone() {
    let (samples, linear) = flat_field(2600);
    let dng = write_dng(&tinted_spec(samples, 8, 8, [0, 1, 1, 2]), false);
    let reader = RawStack::open_files(&[PathBuf::from(dng.path())]).expect("DNG opens");
    let look = tinted_look().forward([linear; 3]);
    assert!(look.iter().any(|v| *v > 1.0) && look.iter().any(|v| (0.1..0.9).contains(v)));
    let expected = look.map(quantize);
    for pixel in read_whole(&reader).chunks_exact(3) {
        for channel in 0..3 {
            assert!(
                pixel[channel].abs_diff(expected[channel]) <= 1,
                "{pixel:?} against {expected:?}"
            );
        }
    }
}

fn rawler_default_read(path: &std::path::Path) -> (usize, usize, Vec<u16>) {
    use rawler::imgop::develop::{Intermediate, RawDevelop};
    let source = RawSource::new(path).expect("source opens");
    let decoder = rawler::get_decoder(&source).expect("decoder");
    let image = decoder
        .raw_image(&source, &RawDecodeParams::default(), false)
        .expect("raw image");
    let Intermediate::ThreeColor(pixels) = RawDevelop::default()
        .develop_intermediate(&image)
        .expect("develops")
    else {
        panic!("three-colour development expected");
    };
    let (width, height) = (pixels.width, pixels.height);
    let samples = pixels
        .into_flatten()
        .into_iter()
        .map(|v| quantize(f64::from(v)))
        .collect();
    (width, height, samples)
}

fn assert_matches_rawler_where_unclipped(path: &std::path::Path) {
    let (width, height, rawler) = rawler_default_read(path);
    let reader = RawStack::open_files(&[path.to_path_buf()]).expect("RAW opens");
    assert_eq!(reader.dimensions(), (width as u32, height as u32));
    let ours = read_whole(&reader);
    let (mut compared, mut worst) = (0_usize, 0_u16);
    for (a, b) in ours.chunks_exact(3).zip(rawler.chunks_exact(3)) {
        if a.iter().any(|v| *v == 0 || *v == u16::MAX) {
            continue;
        }
        compared += 1;
        for channel in 0..3 {
            worst = worst.max(a[channel].abs_diff(b[channel]));
        }
    }
    assert!(
        compared * 2 > width * height,
        "only {compared} pixels compared"
    );
    assert!(worst <= 2, "{} differs by {worst} LSB", path.display());
}

#[test]
fn camera_space_matches_rawler_on_tinted_synthetic_dngs() {
    for cfa in [[0, 1, 1, 2], [1, 0, 2, 1], [2, 1, 1, 0]] {
        let samples = (0..512).map(|i| 64 + (i * 1400 / 511) as u16).collect();
        let dng = write_dng(&tinted_spec(samples, 32, 16, cfa), false);
        assert_matches_rawler_where_unclipped(dng.path());
    }
}

/// Two flat frames one stop apart in camera-linear light, equalized to the
/// shared target: the camera-linear value both must reach.
fn equalized_pair() -> (Vec<tempfile::NamedTempFile>, f64) {
    let (dark, dark_linear) = flat_field(64 + 400);
    let (bright, bright_linear) = flat_field(64 + 800);
    assert!((bright_linear / dark_linear - 2.0).abs() < 1e-12);
    let files = [dark, bright]
        .into_iter()
        .map(|samples| write_dng(&tinted_spec(samples, 8, 8, [0, 1, 1, 2]), false))
        .collect();
    (files, (dark_linear + bright_linear) / 2.0)
}

fn equalization_of(reader: &RawStack) -> focusforge_core::equalize::EqualizationReport {
    use std::sync::atomic::AtomicUsize;
    focusforge_core::equalize::estimate(
        reader,
        2,
        reader.dimensions(),
        3,
        focusforge_core::equalize::EqualizeSettings {
            max_gain: 4.0,
            target_stops: 0.0,
        },
        1,
        &AtomicUsize::new(0),
        &|| false,
    )
    .expect("equalization estimates")
}

fn open_pair(files: &[tempfile::NamedTempFile]) -> RawStack {
    let paths: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(f.path())).collect();
    RawStack::open_files(&paths).expect("DNG pair opens")
}

#[test]
fn equalize_raw_estimates_on_camera_linear_means() {
    let (files, target) = equalized_pair();
    let report = equalization_of(&open_pair(&files));
    let (dark, bright) = (&report.frames[0], &report.frames[1]);
    let scale = f64::from(u16::MAX);
    assert!(
        (dark.mean - 400.0 / 4031.0 * scale).abs() < 0.1,
        "{}",
        dark.mean
    );
    assert!(
        (bright.mean - 800.0 / 4031.0 * scale).abs() < 0.1,
        "{}",
        bright.mean
    );
    assert!((report.target - target * scale).abs() < 0.1);
    assert!((dark.applied / bright.applied - 2.0).abs() < 1e-4);
}

#[test]
fn equalize_raw_brings_a_frame_at_a_known_offset_to_the_reference_exposure() {
    use focusforge_core::equalize::GainReader;
    let (files, target) = equalized_pair();
    let reader = open_pair(&files);
    let gains = equalization_of(&reader).recorded().gains;
    let expected = tinted_look().forward([target; 3]).map(quantize);
    let gained = GainReader::new(&reader, &gains);
    let rect = Rect {
        x: 0,
        y: 0,
        width: 8,
        height: 8,
    };
    let step = std::num::NonZeroU32::new(2).expect("nonzero step");
    for frame in 0..2 {
        let mut whole = vec![0; 8 * 8 * 3];
        gained
            .read_tile_into(frame, rect, &mut whole)
            .expect("gained read");
        let mut covered = vec![0; 8 * 8 * 3];
        let coverage = gained
            .read_tile_coverage_into(frame, rect, &mut covered)
            .expect("gained coverage read");
        let mut stepped = vec![0; 4 * 4 * 3];
        gained
            .read_tile_step_into(frame, rect, step, 3, &mut stepped)
            .expect("gained step read");
        assert!(coverage.is_none());
        for read in [&whole, &covered, &stepped] {
            for pixel in read.chunks_exact(3) {
                for channel in 0..3 {
                    assert!(
                        pixel[channel].abs_diff(expected[channel]) <= 1,
                        "frame {frame}: {pixel:?} against {expected:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn equalize_raw_gains_replay_on_a_later_read() {
    use focusforge_core::equalize::GainReader;
    let (files, _) = equalized_pair();
    let first = open_pair(&files);
    let gains = equalization_of(&first).recorded().gains;
    let rect = Rect {
        x: 1,
        y: 2,
        width: 5,
        height: 4,
    };
    let read = |reader: &RawStack, frame: usize| {
        let mut out = vec![0; 5 * 4 * 3];
        GainReader::new(reader, &gains)
            .read_tile_into(frame, rect, &mut out)
            .expect("gained read");
        out
    };
    let later = open_pair(&files);
    for frame in 0..2 {
        assert_eq!(read(&first, frame), read(&later, frame));
    }
}

fn whole_rect(reader: &RawStack) -> Rect {
    let (width, height) = reader.dimensions();
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// `value` narrowed the way the `u16` read narrows an `f32` one.
fn narrowed(value: f32) -> u16 {
    (value.clamp(0.0, 65_535.0) + 0.5).floor() as u16
}

fn flat_dng(level: u16, width: u32, height: u32) -> (tempfile::NamedTempFile, f64) {
    let linear = f64::from(level - 64) / f64::from(4095 - 64);
    let samples = vec![level; (width * height) as usize];
    (
        write_dng(&tinted_spec(samples, width, height, [0, 1, 1, 2]), false),
        linear,
    )
}

#[test]
fn raw_f32_read_keeps_headroom_and_matches_the_look() {
    use focusforge_core::equalize::UNIT_GAIN;
    let (dng, linear) = flat_dng(2600, 40, 24);
    let reader = RawStack::open_files(&[PathBuf::from(dng.path())]).expect("DNG opens");
    assert_eq!(
        reader.sample_format(),
        focusforge_core::tile::SampleFormat::F32
    );
    let sub = Rect {
        x: 5,
        y: 3,
        width: 17,
        height: 9,
    };
    let scale = f64::from(u16::MAX);
    let look = tinted_look();
    for gain in [UNIT_GAIN, UNIT_GAIN / 2] {
        let ratio = f64::from(gain) / f64::from(UNIT_GAIN);
        let expected = look.forward([linear * ratio; 3]).map(|v| v * scale);
        for rect in [sub, whole_rect(&reader)] {
            let mut wide = vec![0.0f32; rect.width as usize * rect.height as usize * 3];
            reader
                .read_tile_gained_f32_into(0, rect, gain, &mut wide)
                .expect("f32 gained read");
            for pixel in wide.chunks_exact(3) {
                for channel in 0..3 {
                    assert!(
                        (f64::from(pixel[channel]) - expected[channel]).abs() < 0.5,
                        "gain {gain}: {pixel:?} against {expected:?}"
                    );
                }
            }
            let mut narrow = vec![0u16; wide.len()];
            reader
                .read_tile_gained_into(0, rect, gain, &mut narrow)
                .expect("u16 gained read");
            assert!(
                narrow.iter().zip(&wide).all(|(n, w)| *n == narrowed(*w)),
                "the u16 read is the narrowed f32 read"
            );
        }
        let clips = expected.iter().any(|v| *v > scale);
        assert_eq!(clips, gain == UNIT_GAIN, "{expected:?}");
    }
    let mut plain = vec![0.0f32; 3];
    let one = Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    };
    reader.read_tile_f32_into(0, one, &mut plain).expect("read");
    assert!(plain.iter().any(|v| *v > 65_535.0), "{plain:?}");
}

#[test]
fn raw_f32_read_rejects_out_of_frame_rects_and_wrong_lengths() {
    let (dng, _) = flat_dng(900, 40, 24);
    let reader = RawStack::open_files(&[PathBuf::from(dng.path())]).expect("DNG opens");
    for rect in [
        Rect {
            x: 39,
            y: 0,
            width: 2,
            height: 1,
        },
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
        Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1,
        },
    ] {
        let mut out = vec![9.0f32; 6];
        let error = reader
            .read_tile_f32_into(0, rect, &mut out)
            .expect_err("a rect outside the frame is refused");
        assert!(
            error.to_string().contains("outside developed frame"),
            "{error}"
        );
        assert_eq!(out, vec![9.0; 6]);
    }
    let whole = whole_rect(&reader);
    for len in [40 * 24 * 3 - 1, 40 * 24 * 3 + 1] {
        let mut out = vec![7.0f32; len];
        let error = reader
            .read_tile_f32_into(0, whole, &mut out)
            .expect_err("a wrong length is refused");
        assert!(error.to_string().contains(&len.to_string()), "{error}");
        assert!(out.iter().all(|v| *v == 7.0));
    }
}

#[test]
fn equalize_raw_surfaces_a_decode_error() {
    let (good, _) = flat_dng(900, 8, 8);
    let truncated = write_dng(&tinted_spec(vec![900; 64], 8, 8, [0, 1, 1, 2]), true);
    let files = [good, truncated];
    let reader = open_pair(&files);
    let started = std::sync::atomic::AtomicUsize::new(0);
    let error = focusforge_core::equalize::estimate(
        &reader,
        2,
        reader.dimensions(),
        3,
        focusforge_core::equalize::EqualizeSettings {
            max_gain: 4.0,
            target_stops: 0.0,
        },
        1,
        &started,
        &|| false,
    )
    .expect_err("frame 1 cannot be decoded");
    let text = error.to_string();
    assert!(
        text.contains(&truncated_name(reader.metadata(1).expect("metadata"))),
        "{text}"
    );
    assert!(!text.contains("no linear-light mean"), "{text}");
}

fn truncated_name(metadata: &focusforge_core::io::raw::RawMetadata) -> String {
    metadata
        .path
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned()
}

/// Frame side: large enough that the fusion's tile margin leaves an interior.
const SIDE: u32 = 112;

/// The pixel the stacks are compared at: the frame centre, clear of the edge.
const PROBE: Rect = Rect {
    x: SIDE / 2,
    y: SIDE / 2,
    width: 1,
    height: 1,
};

/// Serves only the `u16` read, so the sample format stays the `u16` default
/// and every consumer reads clipped samples.
struct NarrowOnly<'a>(&'a dyn TileReader);

impl TileReader for NarrowOnly<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), focusforge_core::algorithm::AlgorithmError> {
        self.0.read_tile_into(frame, rect, out)
    }
}

/// The pixel `PROBE` of the weighted-average fusion of the two square frames
/// `reader` serves.
fn fused_probe_pixel(reader: &dyn TileReader, label: &str) -> Vec<u16> {
    use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
    use focusforge_core::io::TiffStack;
    use focusforge_core::params::{self, ParamValues, StackContext};
    use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};

    let (width, height, frame_count) = (SIDE, SIDE, 2);
    let context = StackContext::new(frame_count, width, height, 3);
    let algo = focusforge_core::registry::find_stack("weighted-average")
        .expect("weighted-average resolves");
    let resolved = params::resolve(algo.params(), &ParamValues::default(), &context)
        .expect("parameters resolve");
    let values = params::resolved_values(&resolved);

    let out_dir = common::workspace_root()
        .join(".scratch")
        .join("raw-f32-stack");
    std::fs::create_dir_all(&out_dir).expect("output dir");
    let composite = out_dir.join(format!("{label}.tif"));
    let mut options = TileSchedulerOptions::explicit_budget(out_dir.join(label), 1 << 30);
    options.apply_support(algo.tile_support(&values));
    options.tile_size = 80;
    let scheduler = TileScheduler::new(reader, context, 3, &options).expect("scheduler builds");
    let consumer = FusionConsumer::create(
        algo,
        values,
        &composite,
        None,
        FusionGeometry {
            width,
            height,
            samples_per_pixel: 3,
            frame_count: frame_count as u32,
            crop: None,
            description: None,
        },
    )
    .expect("consumer creates");
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("fusion runs");
    consumer.finish().expect("fusion finishes");

    TiffStack::open_files(&[composite])
        .expect("composite reopens")
        .read_tile(0, PROBE)
        .expect("composite read")
        .samples
}

/// A frame of 2x2-cell checkerboard texture: cells alternate between `level`
/// and `level + 200`. Frames of different `level` carry identical detail, so
/// the sharpness weights of a pair are equal.
fn textured_dng(level: u16) -> tempfile::NamedTempFile {
    let samples = (0..SIDE * SIDE)
        .map(|i| {
            let (x, y) = (i % SIDE, i / SIDE);
            level + if (x / 2 + y / 2) % 2 == 0 { 0 } else { 200 }
        })
        .collect();
    write_dng(&tinted_spec(samples, SIDE, SIDE, [0, 1, 1, 2]), false)
}

/// The probe pixel of each frame at `levels` as the reader serves it, and the
/// weighted-average fusions of the frames through gain and warp readers: once
/// natively `f32`, once through [`NarrowOnly`].
#[derive(Debug)]
struct PairFused {
    frames: [[f32; 3]; 2],
    wide: Vec<u16>,
    narrow: Vec<u16>,
}

fn raw_pair_fused(levels: [u16; 2], label: &str) -> PairFused {
    use focusforge_core::align::{Transform, WarpingReader};
    use focusforge_core::equalize::{GainReader, UNIT_GAIN};

    let files: Vec<_> = levels.iter().map(|level| textured_dng(*level)).collect();
    let raw = open_pair(&files);
    let mut frames = [[0.0f32; 3]; 2];
    for (frame, pixel) in frames.iter_mut().enumerate() {
        raw.read_tile_f32_into(frame, PROBE, pixel)
            .expect("f32 read");
    }
    let gained = GainReader::new(&raw, &[UNIT_GAIN; 2]);
    let warping = WarpingReader::new(&gained, (SIDE, SIDE), 3, vec![Transform::identity(); 2]);
    PairFused {
        frames,
        wide: fused_probe_pixel(&warping, &format!("{label}-f32")),
        narrow: fused_probe_pixel(&NarrowOnly(&warping), &format!("{label}-u16")),
    }
}

impl PairFused {
    /// The second frame's weight at the probe, read off the green channel of
    /// `fused`: the sharpness weights are per pixel, the same for every
    /// channel, and green stays below white in every frame, so clipping
    /// cannot move it.
    fn share(&self, fused: &[u16]) -> f32 {
        let [a, b] = self.frames.map(|pixel| pixel[1]);
        (f32::from(fused[1]) - a) / (b - a)
    }

    /// The blend of the frames' probe pixels at `share`, narrowed once, with
    /// each frame clipped at white first when `clip` is set.
    fn blend(&self, share: f32, clip: bool) -> [u16; 3] {
        let [a, b] = self
            .frames
            .map(|pixel| pixel.map(|v| if clip { v.min(65_535.0) } else { v }));
        std::array::from_fn(|c| narrowed(a[c] + share * (b[c] - a[c])))
    }

    /// Per channel, how far the unclipped second frame exceeds white.
    fn headroom(&self) -> [f32; 3] {
        self.frames[1].map(|v| (v - 65_535.0).max(0.0))
    }

    /// `fused` equals the blend of the frames at its own share, to 3 LSB.
    fn assert_blend(&self, fused: &[u16], clip: bool, what: &str) {
        let share = self.share(fused);
        let expected = self.blend(share, clip);
        assert!(
            (0..3).all(|c| fused[c].abs_diff(expected[c]) <= 3),
            "{what}: {fused:?} against {expected:?} at share {share}"
        );
    }
}

// Both frames carry the same texture, so the `f32` stack mixes them at the
// probe and blends the unclipped samples, clipping the result once. The
// `NarrowOnly` stack clips each frame at the read first: the second frame's
// headroom is gone from the blend and from its sharpness weight.
#[test]
fn raw_f32_stack_fuses_unclipped_highlights_and_clips_once() {
    let fused = raw_pair_fused([1500, 2600], "highlights");
    assert!(
        fused.frames.iter().all(|pixel| pixel[1] < 65_535.0),
        "{fused:?}"
    );
    assert!(fused.headroom().iter().any(|v| *v > 100.0), "{fused:?}");
    fused.assert_blend(&fused.wide, false, "f32 stack");
    fused.assert_blend(&fused.narrow, true, "clipped stack");
    // The blend is a real mix, and clipping the frames first would move it.
    let share = fused.share(&fused.wide);
    assert!((0.1..0.9).contains(&share), "share {share}");
    let (unclipped, clipped) = (fused.blend(share, false), fused.blend(share, true));
    assert!(
        (0..3).any(|c| unclipped[c].abs_diff(clipped[c]) > 100),
        "{unclipped:?} against {clipped:?}"
    );
    assert!(
        (0..3).any(|c| fused.wide[c].abs_diff(fused.narrow[c]) > 100),
        "{fused:?}"
    );
}

#[test]
fn raw_f32_stack_agrees_with_the_u16_stack_below_white() {
    let fused = raw_pair_fused([900, 1200], "below-white");
    assert_eq!(fused.headroom(), [0.0; 3], "{fused:?}");
    assert!((0.1..0.9).contains(&fused.share(&fused.wide)), "{fused:?}");
    fused.assert_blend(&fused.wide, false, "f32 stack");
    fused.assert_blend(&fused.narrow, false, "u16 stack");
    assert!(
        (0..3).all(|c| fused.wide[c].abs_diff(fused.narrow[c]) <= 1),
        "{fused:?}"
    );
}
