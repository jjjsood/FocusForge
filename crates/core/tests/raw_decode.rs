// SPDX-License-Identifier: GPL-3.0-or-later

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::equalize::{EqualizeSettings, UNIT_GAIN};
use focusforge_core::host::HostFacts;
use focusforge_core::io::Rect;
use focusforge_core::io::{
    DevelopStore, FrameError, FrameReader, RawError, RawStack, open_frame_set,
};
use focusforge_core::job::{JobControl, JobStage};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::pipeline::{
    FreeSpaceProbe, FuseOutcome, FuseOutput, FuseProduct, FuseRequest, PipelineError, RunStage,
    StageObserver, VolumeShortfall, VolumeSpace,
};
use focusforge_core::tile::{HOST_FIXED_BYTES, HostBudget, MemoryBound};
use rawler::decoders::{RawDecodeParams, WellKnownIFD};
use rawler::rawsource::RawSource;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::common::target_tmp;
use crate::raw_dng::{DngSpec, copy_padded, write_dng};

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
        assert_eq!(FrameReader::frame(&reader, 0).orientation, 1);
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
    assert!(error.to_string().contains("overflows a byte count"));
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

/// Prints how far scratch reads of whole corpus frames fall from cached reads.
#[test]
#[ignore = "measurement: prints the scratch-versus-cached read error of corpus frames; needs the FOCUSFORGE_RAW_SAMPLES corpus"]
fn raw_scratch_error_on_corpus_frames() {
    let Some(samples) = std::env::var_os("FOCUSFORGE_RAW_SAMPLES") else {
        eprintln!("skip: FOCUSFORGE_RAW_SAMPLES is not set");
        return;
    };
    for name in ["panasonic-dmc-lx7.rw2", "fujifilm-x-s10.raf"] {
        let path = PathBuf::from(&samples).join(name);
        if !path.is_file() {
            eprintln!("skip: {} is missing", path.display());
            continue;
        }
        let cached = RawStack::open_files(std::slice::from_ref(&path)).expect("sample opens");
        let developed = RawStack::open_files(std::slice::from_ref(&path)).expect("sample opens");
        let root = target_tmp();
        developed
            .develop_once(
                root.path(),
                DevelopStore::ScratchFile,
                HostBudget::explicit(u64::MAX),
                1,
                &|| false,
                &AtomicUsize::new(0),
            )
            .expect("the pass develops the frame");
        let (width, height) = cached.dimensions();
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let count = width as usize * height as usize * 3;

        let (mut want16, mut got16) = (vec![0u16; count], vec![0u16; count]);
        cached
            .read_tile_into(0, rect, &mut want16)
            .expect("cached u16 read");
        developed
            .read_tile_into(0, rect, &mut got16)
            .expect("scratch u16 read");
        let differing16 = want16.iter().zip(&got16).filter(|(w, g)| w != g).count();
        let worst16 = want16.iter().zip(&got16).map(|(w, g)| w.abs_diff(*g)).max();
        let over = |limit: u16| {
            want16
                .iter()
                .zip(&got16)
                .filter(|(w, g)| w.abs_diff(**g) > limit)
                .count()
        };

        let (mut want, mut got) = (vec![0f32; count], vec![0f32; count]);
        cached
            .read_tile_f32_into(0, rect, &mut want)
            .expect("cached f32 read");
        developed
            .read_tile_f32_into(0, rect, &mut got)
            .expect("scratch f32 read");
        let differing = want.iter().zip(&got).filter(|(w, g)| w != g).count();
        let worst = want
            .iter()
            .zip(&got)
            .map(|(w, g)| (w - g).abs())
            .fold(0.0f32, f32::max);
        let worst_relative = want
            .iter()
            .zip(&got)
            .map(|(w, g)| (w - g).abs() / (w.abs() / 65_535.0).max(1.0))
            .fold(0.0f32, f32::max);
        let (low, high) = got.iter().fold((f32::MAX, f32::MIN), |(low, high), v| {
            (low.min(*v), high.max(*v))
        });
        eprintln!(
            "{name}: {width}x{height} u16 max diff {worst16:?} LSB, {differing16} differ; \
             u16 diff over 8/16/32/64 LSB: {}/{}/{}/{}; \
             f32 max diff {worst} (relative bound {worst_relative}) LSB, {differing} differ; \
             mapped f32 min {low} max {high}",
            over(8),
            over(16),
            over(32),
            over(64),
        );
        assert!(
            got.iter().all(|value| value.is_finite()),
            "{name}: a scratch read is not finite"
        );
    }
}

#[test]
#[ignore = "needs the FOCUSFORGE_RAW_SAMPLES corpus"]
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
#[ignore = "needs the FOCUSFORGE_RAW_SAMPLES corpus"]
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
#[ignore = "needs the FOCUSFORGE_RAW_LOCAL_SAMPLES corpus"]
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
#[ignore = "needs the FOCUSFORGE_RAW_SAMPLES corpus"]
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
#[ignore = "needs the FOCUSFORGE_RAW_SAMPLES corpus"]
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
    fused_rect(reader, 2, label, None, PROBE).2
}

/// The scheduler's initial in-flight limit, its band width and the samples of
/// `rect` in the weighted-average fusion of the `frame_count` square frames
/// `reader` serves, with `facts` as the host the scheduler probes when given.
fn fused_rect(
    reader: &dyn TileReader,
    frame_count: usize,
    label: &str,
    facts: Option<HostFacts>,
    rect: Rect,
) -> (usize, Option<usize>, Vec<u16>) {
    use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
    use focusforge_core::host::FixedFacts;
    use focusforge_core::io::TiffStack;
    use focusforge_core::params::{self, ParamValues, StackContext};
    use focusforge_core::tile::{
        HostSources, MonotonicClock, NoopPreprocess, TileScheduler, TileSchedulerOptions,
    };

    let (width, height) = (SIDE, SIDE);
    let context = StackContext::new(frame_count, width, height, 3);
    let algo = focusforge_core::registry::find_stack("weighted-average")
        .expect("weighted-average resolves");
    let resolved = params::resolve(algo.params(), &ParamValues::default(), &context)
        .expect("parameters resolve");
    let values = params::resolved_values(&resolved);

    let temp = target_tmp();
    let out_dir = temp.path();
    let composite = out_dir.join(format!("{label}.tif"));
    let mut options = TileSchedulerOptions::explicit_budget(out_dir.join(label), 1 << 30);
    options.apply_support(algo.tile_support(&values));
    options.tile_size = 80;
    if let Some(facts) = facts {
        options.host = HostSources {
            probe: Arc::new(FixedFacts(facts)),
            clock: Arc::new(MonotonicClock::new()),
        };
    }
    let scheduler = TileScheduler::new(reader, context, 3, &options).expect("scheduler builds");
    let in_flight_limit = scheduler.in_flight_limit();
    let band_columns = scheduler.band_columns();
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

    let samples = TiffStack::open_files(&[composite])
        .expect("composite reopens")
        .read_tile(0, rect)
        .expect("composite read")
        .samples;
    (in_flight_limit, band_columns, samples)
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

/// A gradient frame in the tinted camera, its pixels moving with `shift`.
fn gradient_dng(width: u32, height: u32, shift: u32) -> tempfile::NamedTempFile {
    let samples = (0..width * height)
        .map(|i| ((((i % width) + (i / width)) * 8 + shift) % 4000) as u16)
        .collect();
    write_dng(&tinted_spec(samples, width, height, [0, 1, 1, 2]), false)
}

fn copies_of(dng: &tempfile::NamedTempFile, count: usize, dir: &Path) -> Vec<PathBuf> {
    (0..count)
        .map(|index| {
            let path = dir.join(format!("{index:02}.dng"));
            std::fs::copy(dng.path(), &path).expect("copy the DNG frame");
            path
        })
        .collect()
}

struct NoopObserver;
impl StageObserver for NoopObserver {}

fn raw_request(
    frames: Vec<PathBuf>,
    root: &Path,
    equalize: Option<EqualizeSettings>,
    max_host_memory: Option<u64>,
) -> FuseRequest {
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(3));
    FuseRequest {
        frames,
        output: FuseOutput::Seal(root.join("project.ffp")),
        scratch_root: root.join("scratch"),
        staging_key: "raw-develop".to_string(),
        algorithm: focusforge_core::registry::find_stack("laplacian")
            .expect("laplacian is registered"),
        explicit,
        align: None,
        crop: false,
        equalize,
        tile_size: Some(160),
        max_host_memory,
        backend: None,
        align_backend: None,
        resume: false,
        cache: false,
        poll: std::time::Duration::from_millis(5),
        free_space: None,
    }
}

#[test]
fn raw_stack_develops_each_frame_once() {
    let dng = gradient_dng(256, 256, 0);
    for equalize in [
        None,
        Some(EqualizeSettings {
            max_gain: 4.0,
            target_stops: 0.0,
        }),
    ] {
        let root = target_tmp();
        let frames = copies_of(&dng, 8, root.path());
        // Room for two developed frames and a decode, far below the stack.
        let request = raw_request(frames, root.path(), equalize, Some(12 * 1024 * 1024));
        let outcome =
            focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
                .expect("the RAW run completes");
        let FuseOutcome::Completed(report) = outcome else {
            panic!("expected a completed run");
        };
        assert_eq!(
            report.develop_counts,
            Some(vec![1; 8]),
            "equalize: {equalize:?}"
        );
    }
}

/// Largest read difference, in 16-bit output LSB, a frame read through the
/// scratch file may show against the same read through the in-memory cache:
/// half-precision storage reaches 86 LSB on the Panasonic and 42 on the
/// Fujifilm bench frame, a few samples in a million above 32.
const SCRATCH_TOLERANCE: f32 = 96.0;

/// Whether a scratch read `got` is within [`SCRATCH_TOLERANCE`] of the cached
/// read `want`; the bound grows relative to `want` above white.
fn within_scratch_bound(want: f32, got: f32) -> bool {
    (got - want).abs() <= SCRATCH_TOLERANCE * (want.abs() / 65_535.0).max(1.0)
}

fn assert_scratch_f32_reads(want: &[f32], got: &[f32], what: &str) {
    assert_eq!(want.len(), got.len(), "{what}");
    for (index, (want, got)) in want.iter().zip(got).enumerate() {
        assert!(
            within_scratch_bound(*want, *got),
            "{what}: sample {index} read {got} against {want}"
        );
    }
}

fn assert_scratch_u16_reads(want: &[u16], got: &[u16], what: &str) {
    assert_eq!(want.len(), got.len(), "{what}");
    for (index, (want, got)) in want.iter().zip(got).enumerate() {
        assert!(
            f32::from(want.abs_diff(*got)) <= SCRATCH_TOLERANCE,
            "{what}: sample {index} read {got} against {want}"
        );
    }
}

#[test]
fn raw_develop_pass_reads_match_cached_reads() {
    for side in [(40u32, 30u32), (150, 70)] {
        let files: Vec<_> = (0..3)
            .map(|i| gradient_dng(side.0, side.1, i * 700))
            .collect();
        let paths: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(f.path())).collect();
        let cached = RawStack::open_files(&paths).expect("DNG stack opens");
        let developed = RawStack::open_files(&paths).expect("DNG stack opens");
        let root = target_tmp();
        developed
            .develop_once(
                root.path(),
                DevelopStore::ScratchFile,
                HostBudget::explicit(u64::MAX),
                2,
                &|| false,
                &AtomicUsize::new(0),
            )
            .expect("the pass develops every frame");
        assert_eq!(developed.develop_counts(), Some(vec![1; 3]));
        let rects = [
            Rect {
                x: 0,
                y: 0,
                width: side.0,
                height: side.1,
            },
            Rect {
                x: side.0 - 5,
                y: side.1 - 3,
                width: 5,
                height: 3,
            },
            Rect {
                x: side.0 - 1,
                y: side.1 - 1,
                width: 1,
                height: 1,
            },
        ];
        // Rects across 64 px block boundaries, on and off the 32 px grid.
        let straddling = [
            Rect {
                x: 60,
                y: 60,
                width: 8,
                height: 8,
            },
            Rect {
                x: 33,
                y: 1,
                width: 100,
                height: 69,
            },
            Rect {
                x: 63,
                y: 0,
                width: 2,
                height: 70,
            },
        ];
        let rects: Vec<Rect> = rects
            .into_iter()
            .chain(
                straddling
                    .into_iter()
                    .filter(|r| r.x + r.width <= side.0 && r.y + r.height <= side.1),
            )
            .collect();
        for frame in 0..3 {
            for &rect in &rects {
                let samples = rect.width as usize * rect.height as usize * 3;
                for gain in [UNIT_GAIN, UNIT_GAIN * 3 / 2, UNIT_GAIN / 3] {
                    let (mut want, mut got) = (vec![0u16; samples], vec![0u16; samples]);
                    cached
                        .read_tile_gained_into(frame, rect, gain, &mut want)
                        .expect("cached read");
                    developed
                        .read_tile_gained_into(frame, rect, gain, &mut got)
                        .expect("mapped read");
                    assert_scratch_u16_reads(
                        &want,
                        &got,
                        &format!("u16 frame {frame} {rect:?} gain {gain}"),
                    );

                    let (mut want, mut got) = (vec![0f32; samples], vec![0f32; samples]);
                    cached
                        .read_tile_gained_f32_into(frame, rect, gain, &mut want)
                        .expect("cached read");
                    developed
                        .read_tile_gained_f32_into(frame, rect, gain, &mut got)
                        .expect("mapped read");
                    assert_scratch_f32_reads(
                        &want,
                        &got,
                        &format!("f32 frame {frame} {rect:?} gain {gain}"),
                    );
                }
                let (mut want, mut got) = (vec![0u16; samples], vec![0u16; samples]);
                cached
                    .read_tile_into(frame, rect, &mut want)
                    .expect("cached read");
                developed
                    .read_tile_into(frame, rect, &mut got)
                    .expect("mapped read");
                assert_scratch_u16_reads(&want, &got, &format!("unit gain frame {frame} {rect:?}"));
                let (mut want, mut got) = (vec![0f32; samples], vec![0f32; samples]);
                cached
                    .read_tile_f32_into(frame, rect, &mut want)
                    .expect("cached read");
                developed
                    .read_tile_f32_into(frame, rect, &mut got)
                    .expect("mapped read");
                assert_scratch_f32_reads(
                    &want,
                    &got,
                    &format!("unit gain f32 frame {frame} {rect:?}"),
                );
            }
            assert_eq!(
                cached.linear_means(frame).expect("cached means"),
                developed.linear_means(frame).expect("mapped means"),
                "means of frame {frame}"
            );
        }
        assert_eq!(developed.develop_counts(), Some(vec![1; 3]));
    }
}

fn scratch_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("scratch directory lists") {
            let path = entry.expect("scratch entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn raw_scratch_file_size_matches_encoding() {
    // One block, and ragged on both axes across several blocks.
    for (width, height) in [(48u32, 20u32), (130, 70)] {
        let dng = gradient_dng(width, height, 0);
        let root = target_tmp();
        let paths = copies_of(&dng, 8, root.path());
        let stack = RawStack::open_files(&paths).expect("DNG stack opens");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&scratch).expect("scratch root");
        stack
            .develop_once(
                &scratch,
                DevelopStore::ScratchFile,
                HostBudget::explicit(u64::MAX),
                2,
                &|| false,
                &AtomicUsize::new(0),
            )
            .expect("the pass develops every frame");
        let frame_bytes = stack.scratch_frame_bytes().expect("frame bytes fit");
        assert_eq!(frame_bytes, u64::from(width * height * 6));
        let files = scratch_files(&scratch);
        assert_eq!(files.len(), 8, "{files:?}");
        for file in files {
            let length = std::fs::metadata(&file).expect("scratch file").len();
            assert_eq!(length, frame_bytes, "{}", file.display());
        }
    }
}

#[test]
fn raw_scratch_keeps_values_above_white() {
    let files = [textured_dng(2600), textured_dng(1500)];
    let cached = open_pair(&files);
    let developed = open_pair(&files);
    let root = target_tmp();
    developed
        .develop_once(
            root.path(),
            DevelopStore::ScratchFile,
            HostBudget::explicit(u64::MAX),
            2,
            &|| false,
            &AtomicUsize::new(0),
        )
        .expect("the pass develops every frame");
    let rect = Rect {
        x: 0,
        y: 0,
        width: SIDE,
        height: SIDE,
    };
    let samples = SIDE as usize * SIDE as usize * 3;
    let (mut want, mut got) = (vec![0f32; samples], vec![0f32; samples]);
    cached
        .read_tile_f32_into(0, rect, &mut want)
        .expect("cached read");
    developed
        .read_tile_f32_into(0, rect, &mut got)
        .expect("mapped read");
    assert!(got.iter().any(|value| *value > 65_535.0 + 100.0));
    assert_scratch_f32_reads(&want, &got, "highlight frame");
}

#[test]
fn cache_refusal_message_names_the_file_in_mb_at_both_sites() {
    let dng = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        false,
    );
    let one = 8 * 8 * 30 + std::fs::metadata(dng.path()).expect("DNG size").len();
    let path = PathBuf::from(dng.path());
    let need = format!("needs {} MB", one.div_ceil(1_000_000));
    let check = |text: String, wanted: &str| {
        assert!(text.contains(&dng.path().display().to_string()), "{text}");
        assert!(text.contains(&need), "{text}");
        assert!(text.contains(wanted), "{text}");
        assert!(text.contains("convert the frames to TIFF"), "{text}");
        assert!(!text.contains("bytes"), "{text}");
        assert!(!text.contains("cache bound"), "{text}");
        assert!(!text.contains(&one.to_string()), "{text}");
    };

    let reader = RawStack::open_files_with_budget(std::slice::from_ref(&path), 1)
        .expect("metadata fits without decode");
    let error = reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut [0; 3],
        )
        .expect_err("a one-byte cache refuses");
    check(error.to_string(), "memory limit allows 0 MB");

    let root = target_tmp();
    let stack = RawStack::open_files(std::slice::from_ref(&path)).expect("DNG stack opens");
    let develop = |budget: HostBudget| {
        stack
            .develop_once(
                root.path(),
                DevelopStore::ScratchFile,
                budget,
                1,
                &|| false,
                &AtomicUsize::new(0),
            )
            .expect_err("a budget below one reservation refuses")
            .to_string()
    };
    check(
        develop(HostBudget::explicit(one - 1)),
        "Raise the memory limit to at least",
    );
    let facts = HostFacts::new(3000, 1000, None, None, 1);
    let text = develop(HostBudget::host_derived(&facts));
    check(text.clone(), "not enough free RAM");
    assert!(!text.contains("memory limit"), "{text}");
}

#[test]
fn capped_budget_opens_at_the_largest_reservation_and_refuses_one_byte_below() {
    let source = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        false,
    );
    let root = target_tmp();
    let sets: [Vec<PathBuf>; 2] = [
        vec![PathBuf::from(source.path())],
        (0..3)
            .map(|index| root.path().join(format!("{index:02}.dng")))
            .collect(),
    ];
    copy_padded(source.path(), &sets[1][0], 0);
    copy_padded(source.path(), &sets[1][1], 4_000);
    copy_padded(source.path(), &sets[1][2], 0);
    let rect = Rect {
        x: 0,
        y: 0,
        width: 8,
        height: 8,
    };
    for paths in &sets {
        let (largest_path, largest) = paths
            .iter()
            .map(|path| (path, std::fs::metadata(path).expect("DNG size").len()))
            .max_by_key(|(_, len)| *len)
            .expect("frames");
        let r = 8 * 8 * 30 + largest;
        let opened = open_frame_set(paths, HostBudget::explicit(r)).expect("R opens");
        for frame in 0..paths.len() {
            opened
                .reader
                .read_tile_into(frame, rect, &mut [0; 8 * 8 * 3])
                .unwrap_or_else(|error| panic!("frame {frame} at R: {error}"));
        }

        let refused = |budget: HostBudget| match open_frame_set(paths, budget) {
            Err(FrameError::Raw(error)) => error,
            Ok(_) => panic!("a budget below R must refuse at open"),
            Err(error) => panic!("expected a RAW refusal, got {error}"),
        };
        let error = refused(HostBudget::explicit(r - 1));
        let text = error.to_string();
        let RawError::CacheTooSmall {
            path,
            bound,
            required,
            bounded_by,
        } = error
        else {
            panic!("expected CacheTooSmall, got {text}");
        };
        assert_eq!((bound, required), (r - 1, r), "{text}");
        assert_eq!(bounded_by, MemoryBound::Limit { divisor: 1 });
        assert_eq!(&path, largest_path);
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(
            text.contains(&format!("needs {} MB", r.div_ceil(1_000_000))),
            "{text}"
        );
        assert!(!text.contains("bytes"), "{text}");

        let facts = HostFacts::new(HOST_FIXED_BYTES + r, HOST_FIXED_BYTES + r, None, None, 1);
        let host = HostBudget::host_derived(&facts);
        assert!(host.bytes() < r);
        let text = refused(host).to_string();
        assert!(text.contains("not enough free RAM"), "{text}");
    }
}

#[test]
fn raw_develops_overlap() {
    let dng = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        false,
    );
    let one = 8 * 8 * 30 + std::fs::metadata(dng.path()).expect("DNG size").len();
    let paths = vec![PathBuf::from(dng.path()); 4];
    let root = target_tmp();
    let developed = AtomicUsize::new(0);
    let pass = |budget: u64, workers: usize| {
        let stack = RawStack::open_files(&paths).expect("DNG stack opens");
        developed.store(0, Ordering::SeqCst);
        let result = stack.develop_once(
            root.path(),
            DevelopStore::ScratchFile,
            HostBudget::explicit(budget),
            workers,
            &|| false,
            &developed,
        );
        (stack, result)
    };

    let (stack, result) = pass(4 * one, 4);
    assert_eq!(result.expect("four reservations fit"), 4);
    assert_eq!(stack.develop_counts(), Some(vec![1; 4]));
    assert_eq!(developed.load(Ordering::SeqCst), 4);
    let (stack, result) = pass(2 * one, 4);
    assert_eq!(result.expect("exactly two reservations fit two"), 2);
    assert_eq!(stack.develop_counts(), Some(vec![1; 4]));
    assert_eq!(developed.load(Ordering::SeqCst), 4);
    let (stack, result) = pass(2 * one - 1, 4);
    assert_eq!(result.expect("one byte under two reservations fits one"), 1);
    assert_eq!(stack.develop_counts(), Some(vec![1; 4]));
    assert_eq!(developed.load(Ordering::SeqCst), 4);
    let (stack, result) = pass(one + one / 2, 4);
    assert_eq!(result.expect("one and a half reservations fit one"), 1);
    assert_eq!(stack.develop_counts(), Some(vec![1; 4]));
    assert_eq!(developed.load(Ordering::SeqCst), 4);
    let (stack, result) = pass(100 * one, 8);
    assert_eq!(result.expect("workers beyond the frames idle"), 4);
    assert_eq!(stack.develop_counts(), Some(vec![1; 4]));
    assert_eq!(developed.load(Ordering::SeqCst), 4);

    let (stack, result) = pass(one - 1, 4);
    let error = result.expect_err("a budget below one reservation refuses");
    assert!(
        matches!(
            error,
            AlgorithmError::Frame(FrameError::Raw(RawError::CacheTooSmall { required, .. }))
                if required == one
        ),
        "{error}"
    );
    assert_eq!(stack.develop_counts(), Some(vec![0; 4]));
    assert_eq!(developed.load(Ordering::SeqCst), 0);
}

/// `root` holds no file or directory.
fn assert_scratch_empty(root: &Path) {
    let left: Vec<_> = match std::fs::read_dir(root) {
        Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
        Err(_) => Vec::new(),
    };
    assert!(left.is_empty(), "scratch left behind: {left:?}");
}

#[test]
fn raw_develop_failure_leaves_no_scratch() {
    let good = gradient_dng(256, 256, 0);
    let withheld = write_dng(
        &tinted_spec(vec![64; 256 * 256], 256, 256, [0, 1, 1, 2]),
        true,
    );
    let root = target_tmp();
    let frames = vec![
        PathBuf::from(good.path()),
        PathBuf::from(withheld.path()),
        PathBuf::from(good.path()),
    ];
    let request = raw_request(frames, root.path(), None, None);
    let Err(error) =
        focusforge_core::pipeline::run(&request, &NoopObserver, JobControl::new(), None)
    else {
        panic!("a frame without pixels must end the run");
    };
    assert!(
        error
            .to_string()
            .contains(&withheld.path().display().to_string()),
        "{error}"
    );
    assert_scratch_empty(&request.scratch_root);

    let request = raw_request(copies_of(&good, 3, root.path()), root.path(), None, None);
    let control = JobControl::new();
    control.cancel();
    let outcome = focusforge_core::pipeline::run(&request, &NoopObserver, control, None)
        .expect("a pre-cancelled run is not an error");
    assert!(matches!(outcome, FuseOutcome::Cancelled { .. }));
    assert_scratch_empty(&request.scratch_root);
}

/// Records the stage announcements and, on [`RunStage::Develop`], what the
/// control reports at that moment; cancels the run there when asked.
struct DevelopWatch {
    control: Arc<JobControl>,
    cancel_on_develop: bool,
    seen: Mutex<Vec<&'static str>>,
    at_develop: Mutex<Option<(JobStage, usize, usize)>>,
}

impl DevelopWatch {
    fn new(control: &Arc<JobControl>, cancel_on_develop: bool) -> Self {
        DevelopWatch {
            control: Arc::clone(control),
            cancel_on_develop,
            seen: Mutex::default(),
            at_develop: Mutex::default(),
        }
    }
}

impl StageObserver for DevelopWatch {
    fn stage(&self, stage: RunStage) {
        let name = match stage {
            RunStage::Develop { frames } => {
                let p = self.control.progress();
                *self.at_develop.lock().expect("no panic while logging") =
                    Some((p.stage, p.develop_total, frames));
                if self.cancel_on_develop {
                    self.control.cancel();
                }
                "develop"
            }
            RunStage::Prepare { .. } => "prepare",
            RunStage::Align { .. } => "align",
            RunStage::Fuse => "fuse",
            _ => "other",
        };
        self.seen.lock().expect("no panic while logging").push(name);
    }
}

#[test]
fn raw_run_reports_the_develop_stage() {
    let dng = gradient_dng(64, 64, 0);
    for count in [3, 5] {
        let root = target_tmp();
        let request = raw_request(
            copies_of(&dng, count, root.path()),
            root.path(),
            Some(EqualizeSettings {
                max_gain: 4.0,
                target_stops: 0.0,
            }),
            None,
        );
        let control = JobControl::new();
        let watch = DevelopWatch::new(&control, false);
        let outcome = focusforge_core::pipeline::run(&request, &watch, Arc::clone(&control), None)
            .expect("the RAW run completes");
        let FuseOutcome::Completed(report) = outcome else {
            panic!("expected a completed run");
        };
        let seen = watch.seen.lock().expect("no panic while logging");
        assert_eq!(
            seen[..2],
            ["develop", "prepare"],
            "{count} frames: {seen:?}"
        );
        assert_eq!(
            *watch.at_develop.lock().expect("no panic while logging"),
            Some((JobStage::Develop, count, count)),
            "the control is on the develop stage when it is announced"
        );
        let progress = control.progress();
        assert_eq!(
            (progress.develop_done, progress.develop_total),
            (count, count)
        );
        assert!(report.stages.develop.is_some(), "{:?}", report.stages);
    }

    let root = target_tmp();
    let request = raw_request(copies_of(&dng, 4, root.path()), root.path(), None, None);
    let control = JobControl::new();
    let watch = DevelopWatch::new(&control, true);
    let outcome = focusforge_core::pipeline::run(&request, &watch, Arc::clone(&control), None)
        .expect("a cancelled run is not an error");
    assert!(matches!(outcome, FuseOutcome::Cancelled { .. }));
    assert_eq!(
        watch.seen.lock().expect("no panic while logging")[..],
        ["develop"],
        "nothing follows a cancel raised on the develop announcement"
    );
    assert_scratch_empty(&request.scratch_root);
}

/// Cancels the run from a tick that reports every frame developed, and
/// records the stage announcements that follow.
struct CancelWhenDeveloped {
    control: Arc<JobControl>,
    seen: Mutex<Vec<&'static str>>,
}

impl StageObserver for CancelWhenDeveloped {
    fn stage(&self, stage: RunStage) {
        let name = match stage {
            RunStage::Develop { .. } => "develop",
            RunStage::Prepare { .. } => "prepare",
            _ => "other",
        };
        self.seen.lock().expect("no panic while logging").push(name);
    }

    fn tick(&self, progress: &focusforge_core::job::JobProgress) {
        if progress.stage == JobStage::Develop && progress.develop_done == progress.develop_total {
            self.control.cancel();
        }
    }
}

#[test]
fn a_cancel_landing_as_the_develop_pass_ends_stops_the_run_in_that_stage() {
    let dng = gradient_dng(64, 64, 0);
    let root = target_tmp();
    let request = raw_request(
        copies_of(&dng, 4, root.path()),
        root.path(),
        Some(EqualizeSettings {
            max_gain: 4.0,
            target_stops: 0.0,
        }),
        None,
    );
    let control = JobControl::new();
    let watch = CancelWhenDeveloped {
        control: Arc::clone(&control),
        seen: Mutex::default(),
    };
    let outcome = focusforge_core::pipeline::run(&request, &watch, Arc::clone(&control), None)
        .expect("a cancelled run is not an error");
    assert!(matches!(outcome, FuseOutcome::Cancelled { .. }));
    assert_eq!(
        watch.seen.lock().expect("no panic while logging")[..],
        ["develop"],
        "the preparation pass is never announced after the cancel"
    );
    assert_eq!(control.progress().stage, JobStage::Develop);
    assert_scratch_empty(&request.scratch_root);
}

#[test]
fn raw_scratch_unwritable_is_a_named_error() {
    let dng = gradient_dng(8, 8, 0);
    let paths = vec![PathBuf::from(dng.path()); 2];
    let stack = RawStack::open_files(&paths).expect("DNG stack opens");
    let blocker = tempfile::NamedTempFile::new_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("a regular file at the scratch root");
    let error = stack
        .develop_once(
            blocker.path(),
            DevelopStore::ScratchFile,
            HostBudget::explicit(u64::MAX),
            2,
            &|| false,
            &AtomicUsize::new(0),
        )
        .expect_err("a scratch root that is a file cannot hold a directory");
    assert!(
        error
            .to_string()
            .contains(&blocker.path().display().to_string()),
        "{error}"
    );
    assert_eq!(stack.develop_counts(), Some(vec![0; 2]));
}

/// Every path is on the volume `name` with `free` bytes free.
fn volume(name: &str, free: u64) -> std::io::Result<VolumeSpace> {
    Ok(VolumeSpace {
        volume: name.into(),
        free,
    })
}

/// A probe answering through `answer` that records every path it is asked about.
fn recording_probe(
    answer: impl Fn(&Path) -> std::io::Result<VolumeSpace> + Send + Sync + 'static,
) -> (FreeSpaceProbe, Arc<Mutex<Vec<PathBuf>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    let probe: FreeSpaceProbe = Arc::new(move |path: &Path| {
        log.lock().expect("probe log").push(path.to_path_buf());
        answer(path)
    });
    (probe, asked)
}

fn run_raw(request: &FuseRequest) -> Result<FuseOutcome, PipelineError> {
    focusforge_core::pipeline::run(request, &NoopObserver, JobControl::new(), None)
}

/// The one shortfall `request` is refused with when its single volume has
/// `free` bytes.
fn single_shortfall(request: &mut FuseRequest, free: u64) -> VolumeShortfall {
    request.free_space = Some(recording_probe(move |_| volume("only", free)).0);
    let Err(PipelineError::DiskSpace(mut shortfalls)) = run_raw(request) else {
        panic!("a run that does not fit must be refused");
    };
    assert_eq!(shortfalls.len(), 1, "{shortfalls:?}");
    shortfalls.remove(0)
}

/// Holds the result beside the scratch root.
fn hold_request(frames: Vec<PathBuf>, root: &Path) -> FuseRequest {
    let mut request = raw_request(frames, root, None, None);
    request.output = FuseOutput::Hold(root.join("held"));
    request
}

/// The summed length of the composite and Z-Map a completed hold run staged.
fn held_len(request: &FuseRequest) -> u64 {
    let FuseOutcome::Completed(report) = run_raw(request).expect("the RAW run completes") else {
        panic!("expected a completed run");
    };
    let FuseProduct::Held(held) = report.product else {
        panic!("a hold request reports a held checkpoint");
    };
    [&held.composite, &held.zmap]
        .iter()
        .map(|file| std::fs::metadata(file).expect("held plane").len())
        .sum()
}

#[test]
fn scratch_space_needed_matches_written_scratch() {
    for (width, height, count) in [(33, 17, 1), (62, 26, 5)] {
        let dng = gradient_dng(width, height, 0);
        let root = target_tmp();
        let paths = copies_of(&dng, count, root.path());
        let stack = RawStack::open_files(&paths).expect("DNG stack opens");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&scratch).expect("scratch root");
        let needed = stack.develop_scratch_bytes().expect("the figure fits");
        stack
            .develop_once(
                &scratch,
                DevelopStore::ScratchFile,
                HostBudget::explicit(u64::MAX),
                2,
                &|| false,
                &AtomicUsize::new(0),
            )
            .expect("the pass develops every frame");
        let (width, height) = stack.dimensions();
        assert_eq!(
            needed,
            count as u64 * u64::from(width) * u64::from(height) * 6
        );
        let files = scratch_files(&scratch);
        assert_eq!(files.len(), count, "{files:?}");
        let written: u64 = files
            .iter()
            .map(|file| std::fs::metadata(file).expect("scratch file").len())
            .sum();
        assert_eq!(needed, written, "{width}x{height} x {count}");
    }
}

#[test]
fn raw_run_space_staged_bytes_cover_written_staging() {
    let dng = gradient_dng(64, 48, 0);
    let develop = 4 * 64 * 48 * 6;
    let root = target_tmp();
    let frames = copies_of(&dng, 4, root.path());
    let mut capped = hold_request(frames.clone(), root.path());
    capped.max_host_memory = Some(on_disk_budget(&dng));
    let needed = single_shortfall(&mut capped, 0).needed;
    let written = held_len(&hold_request(frames, root.path()));
    let staged = needed - develop;
    assert!(
        staged >= written && staged - written < 256 * 1024,
        "needed {needed}, developed {develop}, staged {written}"
    );
}

#[test]
fn raw_run_space_includes_developed_scratch() {
    let dng = gradient_dng(64, 48, 0);
    let develop = 4 * 64 * 48 * 6;
    let root = target_tmp();
    let frames = copies_of(&dng, 4, root.path());
    let staged = held_len(&hold_request(frames.clone(), root.path()));
    let probe_root = target_tmp();
    let probe_frames = copies_of(&dng, 4, probe_root.path());
    let mut probe = hold_request(probe_frames, probe_root.path());
    probe.max_host_memory = Some(on_disk_budget(&dng));
    let needed = single_shortfall(&mut probe, 0).needed;
    // The sealed result adds its temporary compressed planes to the scratch
    // volume and the project itself to the output volume.
    for free in [develop - 1, 0] {
        let root = target_tmp();
        let frames = copies_of(&dng, 4, root.path());
        let mut request = raw_request(frames, root.path(), None, Some(on_disk_budget(&dng)));
        let shortfall = single_shortfall(&mut request, free);
        assert_eq!(shortfall.free, free);
        assert!(
            shortfall.needed >= develop + 3 * staged,
            "needed {} for {develop} developed and {staged} staged",
            shortfall.needed
        );
        assert!(shortfall.needed > needed);
        assert_scratch_empty(&root.path().join("scratch"));
        let text = PipelineError::DiskSpace(vec![shortfall.clone()]).to_string();
        for want in [
            shortfall.path.display().to_string(),
            format!("{} bytes", shortfall.needed),
            format!("{free} bytes"),
            "0.0 GB".to_string(),
        ] {
            assert!(text.contains(&want), "{want:?} missing from {text}");
        }
    }
}

#[test]
fn raw_run_proceeds_when_run_space_suffices() {
    let dng = gradient_dng(64, 48, 0);
    let held = |free: Option<u64>| {
        let root = target_tmp();
        let frames = copies_of(&dng, 4, root.path());
        let mut request = hold_request(frames, root.path());
        request.free_space = free.map(|free| recording_probe(move |_| volume("only", free)).0);
        let FuseOutcome::Completed(report) = run_raw(&request).expect("the RAW run completes")
        else {
            panic!("expected a completed run");
        };
        assert_eq!(report.develop_counts, Some(vec![1; 4]));
        let FuseProduct::Held(held) = report.product else {
            panic!("a hold request reports a held checkpoint");
        };
        std::fs::read(&held.composite).expect("composite is readable")
    };
    let root = target_tmp();
    let frames = copies_of(&dng, 4, root.path());
    let needed = single_shortfall(&mut hold_request(frames, root.path()), 0).needed;
    assert_eq!(held(Some(needed)), held(None));
}

#[test]
fn scratch_space_query_uses_nearest_existing_ancestor() {
    let dng = gradient_dng(64, 48, 0);
    let root = target_tmp();
    let frames = copies_of(&dng, 2, root.path());
    let mut request = raw_request(frames, root.path(), None, None);
    request.scratch_root = PathBuf::from("scratch-space-ancestor-test/deeper");
    let (probe, asked) = recording_probe(|_| volume("only", 0));
    request.free_space = Some(probe);
    assert!(matches!(
        run_raw(&request),
        Err(PipelineError::DiskSpace(_))
    ));
    let cwd = std::env::current_dir().expect("working directory");
    let asked = asked.lock().expect("probe log");
    assert!(asked.contains(&cwd), "{asked:?}");
    assert!(!cwd.join("scratch-space-ancestor-test").exists());
}

#[test]
fn scratch_space_query_failure_refuses_the_run() {
    let dng = gradient_dng(64, 48, 0);
    let root = target_tmp();
    let frames = copies_of(&dng, 2, root.path());
    let mut request = raw_request(frames, root.path(), None, None);
    request.free_space = Some(Arc::new(|_: &Path| {
        Err(std::io::Error::other("volume query refused"))
    }));
    let Err(error) = run_raw(&request) else {
        panic!("an unanswered query must refuse the run");
    };
    assert!(
        matches!(error, PipelineError::DiskSpaceQuery { .. }),
        "{error:?}"
    );
    let text = error.to_string();
    assert!(text.contains("volume query refused"), "{text}");
    assert!(text.contains(&root.path().display().to_string()), "{text}");
    assert_scratch_empty(&root.path().join("scratch"));
}

#[test]
fn page_cache_working_set_is_the_block_span_before_and_after_develop() {
    // 130 x 70: three blocks of 64 across, two down, both edges ragged.
    let dng = gradient_dng(130, 70, 0);
    let root = target_tmp();
    let paths = copies_of(&dng, 2, root.path());
    let stack = RawStack::open_files(&paths).expect("DNG stack opens");
    // (rect width, rect height, bytes): a rect can touch one block more than
    // it covers, whatever its origin, and the frame clamps the span.
    let cases = [
        (1, 1, 64 * 64 * 6),
        (1, 65, 64 * 70 * 6),
        (32, 32, 128 * 70 * 6),
        (64, 64, 128 * 70 * 6),
        (130, 70, 130 * 70 * 6),
        (512, 512, 130 * 70 * 6),
    ];
    let check = |stack: &RawStack, when: &str| {
        for (width, height, bytes) in cases {
            assert_eq!(
                stack.read_page_cache_bytes(width, height),
                bytes,
                "{width}x{height} {when}"
            );
        }
    };
    check(&stack, "before develop");
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&scratch).expect("scratch root");
    stack
        .develop_once(
            &scratch,
            DevelopStore::ScratchFile,
            HostBudget::explicit(u64::MAX),
            2,
            &|| false,
            &AtomicUsize::new(0),
        )
        .expect("the pass develops every frame");
    check(&stack, "after develop");
}

#[test]
fn page_cache_bands_output_identical() {
    let files = [
        gradient_dng(SIDE, SIDE, 0),
        gradient_dng(SIDE, SIDE, 700),
        gradient_dng(SIDE, SIDE, 1500),
    ];
    let stack = open_pair(&files);
    let root = target_tmp();
    stack
        .develop_once(
            root.path(),
            DevelopStore::ScratchFile,
            HostBudget::explicit(u64::MAX),
            2,
            &|| false,
            &AtomicUsize::new(0),
        )
        .expect("the pass develops every frame");
    let whole = Rect {
        x: 0,
        y: 0,
        width: SIDE,
        height: SIDE,
    };
    let host =
        |cgroup_limit| HostFacts::new(64_000_000_000, 50_000_000_000, cgroup_limit, Some(0), 16);
    // Fallback facts give no bands: plain plan order.
    let (plan_limit, plan_bands, in_plan_order) = fused_rect(
        &stack,
        3,
        "bands-plan-order",
        Some(HostFacts::fallback()),
        whole,
    );
    assert_eq!(plan_bands, None);
    // A limit above the fixed part by less than one working set gives
    // one-column bands.
    let tight = host(Some(HOST_FIXED_BYTES + 1_000));
    let (banded_limit, banded_bands, in_bands) =
        fused_rect(&stack, 3, "bands-one-column", Some(tight), whole);
    assert_eq!(banded_bands, Some(1));
    assert_eq!(banded_limit, plan_limit, "bands leave the in-flight limit");
    assert_eq!(in_bands.len(), (SIDE * SIDE * 3) as usize);
    assert!(
        in_plan_order
            .iter()
            .any(|&sample| sample != in_plan_order[0]),
        "the composite must carry detail for the comparison to mean anything"
    );
    assert!(
        in_bands == in_plan_order,
        "the band order changed the composite"
    );
}

/// Develops `stack` into `store` under `root` with room for any pass.
fn develop_into(stack: &RawStack, root: &Path, store: DevelopStore) {
    stack
        .develop_once(
            root,
            store,
            HostBudget::explicit(u64::MAX),
            2,
            &|| false,
            &AtomicUsize::new(0),
        )
        .expect("the pass develops every frame");
}

/// Bytes one decode reserves for the stack's frames: raw pixels at 30 bytes
/// each plus the file.
fn decode_reservation(stack: &RawStack) -> u64 {
    let raw = stack.metadata(0).expect("frame 0 has metadata");
    u64::from(raw.raw_width) * u64::from(raw.raw_height) * 30
        + stack.frame(0).expect("frame 0").file_size
}

/// A host memory ceiling that holds one decode of `dng` but not the developed
/// frames beside it, so a run with it develops to scratch files.
fn on_disk_budget(dng: &tempfile::NamedTempFile) -> u64 {
    decode_reservation(&RawStack::open_files(&[PathBuf::from(dng.path())]).expect("DNG opens")) + 1
}

#[test]
fn raw_in_ram_reads_match_scratch_reads() {
    // One ragged block, and ragged on both axes across several blocks.
    for side in [(40u32, 30u32), (150, 70)] {
        let files: Vec<_> = (0..3)
            .map(|i| gradient_dng(side.0, side.1, i * 700))
            .collect();
        let paths: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(f.path())).collect();
        let on_disk = RawStack::open_files(&paths).expect("DNG stack opens");
        let in_ram = RawStack::open_files(&paths).expect("DNG stack opens");
        let disk_root = target_tmp();
        let ram_root = target_tmp();
        develop_into(&on_disk, disk_root.path(), DevelopStore::ScratchFile);
        develop_into(&in_ram, ram_root.path(), DevelopStore::Memory);
        assert_scratch_empty(ram_root.path());
        assert_eq!(scratch_files(disk_root.path()).len(), 3);
        assert_eq!(in_ram.develop_counts(), Some(vec![1; 3]));

        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        let rects: Vec<Rect> = [
            // The whole frame, the far corner, and a single pixel.
            rect(0, 0, side.0, side.1),
            rect(side.0 - 5, side.1 - 3, 5, 3),
            rect(side.0 - 1, side.1 - 1, 1, 1),
            // Across block edges, on and off the 32 px grid.
            rect(60, 60, 8, 8),
            rect(33, 1, 100, 69),
            rect(63, 0, 2, 70),
            rect(64, 64, 6, 6),
        ]
        .into_iter()
        .filter(|r| r.x + r.width <= side.0 && r.y + r.height <= side.1)
        .collect();
        for frame in 0..3 {
            for &rect in &rects {
                let samples = rect.width as usize * rect.height as usize * 3;
                for gain in [UNIT_GAIN, UNIT_GAIN * 3 / 2, UNIT_GAIN / 3] {
                    let (mut want, mut got) = (vec![0u16; samples], vec![0u16; samples]);
                    on_disk
                        .read_tile_gained_into(frame, rect, gain, &mut want)
                        .expect("scratch read");
                    in_ram
                        .read_tile_gained_into(frame, rect, gain, &mut got)
                        .expect("in-memory read");
                    assert_eq!(want, got, "u16 frame {frame} {rect:?} gain {gain}");

                    let (mut want, mut got) = (vec![0f32; samples], vec![0f32; samples]);
                    on_disk
                        .read_tile_gained_f32_into(frame, rect, gain, &mut want)
                        .expect("scratch read");
                    in_ram
                        .read_tile_gained_f32_into(frame, rect, gain, &mut got)
                        .expect("in-memory read");
                    assert_eq!(want, got, "f32 frame {frame} {rect:?} gain {gain}");
                }
                let (mut want, mut got) = (vec![0u16; samples], vec![0u16; samples]);
                on_disk
                    .read_tile_into(frame, rect, &mut want)
                    .expect("scratch read");
                in_ram
                    .read_tile_into(frame, rect, &mut got)
                    .expect("in-memory read");
                assert_eq!(want, got, "unit gain u16 frame {frame} {rect:?}");
                let (mut want, mut got) = (vec![0f32; samples], vec![0f32; samples]);
                on_disk
                    .read_tile_f32_into(frame, rect, &mut want)
                    .expect("scratch read");
                in_ram
                    .read_tile_f32_into(frame, rect, &mut got)
                    .expect("in-memory read");
                assert_eq!(want, got, "unit gain f32 frame {frame} {rect:?}");
            }
            assert_eq!(
                on_disk.linear_means(frame).expect("scratch means"),
                in_ram.linear_means(frame).expect("in-memory means"),
                "means of frame {frame}"
            );
        }
        assert_eq!(in_ram.develop_counts(), Some(vec![1; 3]));
    }
}

#[test]
fn raw_store_choice_flips_at_the_in_ram_requirement() {
    let dng = gradient_dng(130, 70, 0);
    let root = target_tmp();
    let paths = copies_of(&dng, 4, root.path());
    let stack = RawStack::open_files(&paths).expect("DNG stack opens");
    let frames = stack.develop_scratch_bytes().expect("the figure fits");
    assert_eq!(frames, 4 * 130 * 70 * 6);
    let reservation = decode_reservation(&stack);
    let workers = 2;
    let choose = |budget: u64, share: u64| {
        stack
            .develop_store(HostBudget::explicit(budget), workers, share)
            .expect("the choice is made")
    };
    // The share below the two reservations, at them, and above them.
    for share in [0, 2 * reservation, 10 * reservation] {
        let required = frames + (workers as u64 * reservation).max(share);
        assert_eq!(
            choose(required - 1, share),
            Some(DevelopStore::ScratchFile),
            "share {share}, one byte short"
        );
        assert_eq!(
            choose(required, share),
            Some(DevelopStore::Memory),
            "share {share}, exactly enough"
        );
    }

    // Room for both decodes but not for the frames: the pass writes the
    // frames to scratch and keeps nothing in memory.
    let budget = 2 * reservation;
    assert_eq!(choose(budget, 0), Some(DevelopStore::ScratchFile));
    let on_disk = RawStack::open_files(&paths).expect("DNG stack opens");
    let disk_root = target_tmp();
    on_disk
        .develop_once(
            disk_root.path(),
            DevelopStore::ScratchFile,
            HostBudget::explicit(budget),
            workers,
            &|| false,
            &AtomicUsize::new(0),
        )
        .expect("the pass develops every frame");
    let written: u64 = scratch_files(disk_root.path())
        .iter()
        .map(|file| std::fs::metadata(file).expect("scratch file").len())
        .sum();
    assert_eq!(written, frames);
    assert!(on_disk.read_page_cache_bytes(64, 64) > 0);

    let in_ram = RawStack::open_files(&paths).expect("DNG stack opens");
    let ram_root = target_tmp();
    develop_into(&in_ram, ram_root.path(), DevelopStore::Memory);
    assert_scratch_empty(ram_root.path());
    assert_eq!(in_ram.read_page_cache_bytes(64, 64), 0);
}

#[test]
fn raw_in_ram_failure_and_cancel_leave_nothing() {
    let good = gradient_dng(256, 256, 0);
    let withheld = write_dng(
        &tinted_spec(vec![64; 256 * 256], 256, 256, [0, 1, 1, 2]),
        true,
    );
    let frames = vec![
        PathBuf::from(good.path()),
        PathBuf::from(withheld.path()),
        PathBuf::from(good.path()),
    ];
    let pass = |stack: &RawStack, root: &Path, store, cancel_after: usize| {
        let polls = AtomicUsize::new(0);
        stack.develop_once(
            root,
            store,
            HostBudget::explicit(u64::MAX),
            1,
            &|| polls.fetch_add(1, Ordering::SeqCst) >= cancel_after,
            &AtomicUsize::new(0),
        )
    };

    // The pass keeps its frames in memory when nothing goes wrong.
    let control =
        RawStack::open_files(&[frames[0].clone(), frames[2].clone()]).expect("DNG stack opens");
    let root = target_tmp();
    pass(&control, root.path(), DevelopStore::Memory, usize::MAX).expect("the pass completes");
    assert_eq!(control.read_page_cache_bytes(64, 64), 0);

    // A header-only frame ends the pass with the same error in both stores.
    let mut errors = Vec::new();
    for store in [DevelopStore::ScratchFile, DevelopStore::Memory] {
        let stack = RawStack::open_files(&frames).expect("DNG stack opens");
        let root = target_tmp();
        let error = pass(&stack, root.path(), store, usize::MAX)
            .expect_err("a frame without pixels must end the pass");
        assert!(
            error
                .to_string()
                .contains(&withheld.path().display().to_string()),
            "{store:?}: {error}"
        );
        assert_scratch_empty(root.path());
        assert!(stack.read_page_cache_bytes(64, 64) > 0, "{store:?}");
        errors.push(error.to_string());
    }
    assert_eq!(errors[0], errors[1]);

    // A cancel after the second frame starts leaves nothing either, and the
    // reader can run the pass again.
    let stack = RawStack::open_files(&[frames[0].clone(), frames[2].clone(), frames[0].clone()])
        .expect("DNG stack opens");
    let root = target_tmp();
    let cancelled = pass(&stack, root.path(), DevelopStore::Memory, 2);
    assert!(
        matches!(cancelled, Err(AlgorithmError::Cancelled)),
        "{cancelled:?}"
    );
    assert_scratch_empty(root.path());
    assert!(stack.read_page_cache_bytes(64, 64) > 0);
    pass(&stack, root.path(), DevelopStore::Memory, usize::MAX)
        .expect("a failed pass retained nothing, so the next one runs");
    assert_eq!(stack.read_page_cache_bytes(64, 64), 0);
}

#[test]
fn raw_in_ram_run_needs_no_scratch_space() {
    let dng = gradient_dng(64, 48, 0);
    let develop = 4 * 64 * 48 * 6;
    let capped = Some(on_disk_budget(&dng));
    let request = |max_host_memory, free: u64| {
        let root = target_tmp();
        let frames = copies_of(&dng, 4, root.path());
        let mut request = raw_request(frames, root.path(), None, max_host_memory);
        request.free_space = Some(recording_probe(move |_| volume("only", free)).0);
        (request, root)
    };
    let shortfall = |max_host_memory| {
        let (mut request, _root) = request(max_host_memory, 0);
        single_shortfall(&mut request, 0).needed
    };
    let on_disk = shortfall(capped);
    let in_ram = shortfall(None);
    assert_eq!(in_ram + develop, on_disk, "only the frames differ");

    // A volume that covers the rest but not the frames still runs the stack
    // through memory and refuses the stack that needs the frames on disk.
    let (held, _root) = request(None, in_ram);
    let FuseOutcome::Completed(report) = run_raw(&held).expect("the RAW run completes") else {
        panic!("expected a completed run");
    };
    assert_eq!(report.developed_store, Some(DevelopStore::Memory));
    let (mut refused, _root) = request(capped, in_ram);
    let shortfall = single_shortfall(&mut refused, in_ram);
    assert_eq!((shortfall.needed, shortfall.free), (on_disk, in_ram));
}

/// A one-frame `width` x `height` DNG stack developed into `store`; the root
/// and the DNG must outlive the reads.
fn developed_stack(
    width: u32,
    height: u32,
    store: DevelopStore,
) -> (RawStack, tempfile::TempDir, tempfile::NamedTempFile) {
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], Vec::new());
    spec.width = width;
    spec.height = height;
    spec.samples = vec![512; (width * height) as usize];
    let dng = write_dng(&spec, false);
    let stack = RawStack::open_files(&[PathBuf::from(dng.path())]).expect("DNG opens");
    let root = target_tmp();
    stack
        .develop_once(
            root.path(),
            store,
            HostBudget::explicit(u64::MAX),
            1,
            &|| false,
            &AtomicUsize::new(0),
        )
        .expect("the pass develops the frame");
    (stack, root, dng)
}

/// The scratch file of frame 0 under `root`.
fn scratch_file(root: &Path) -> PathBuf {
    std::fs::read_dir(root)
        .expect("scratch root lists")
        .map(|entry| entry.expect("scratch entry").path().join("0.f16"))
        .find(|path| path.is_file())
        .expect("frame 0 has a scratch file")
}

#[test]
fn raw_scratch_short_read_is_a_scratch_error() {
    use std::io::ErrorKind;

    for (damage, kind) in [
        ("truncated", ErrorKind::UnexpectedEof),
        ("emptied", ErrorKind::UnexpectedEof),
        ("removed", ErrorKind::NotFound),
    ] {
        let (stack, root, _dng) = developed_stack(100, 70, DevelopStore::ScratchFile);
        let path = scratch_file(root.path());
        let length = std::fs::metadata(&path).expect("scratch metadata").len();
        match damage {
            "truncated" | "emptied" => std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("scratch opens for writing")
                .set_len(if damage == "emptied" { 0 } else { length / 2 })
                .expect("scratch shrinks"),
            _ => std::fs::remove_file(&path).expect("scratch is removed"),
        }
        let (width, height) = stack.dimensions();
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let count = width as usize * height as usize * 3;
        let results = [
            stack.read_tile_into(0, rect, &mut vec![0u16; count]),
            stack.read_tile_f32_into(0, rect, &mut vec![0f32; count]),
        ];
        for result in results {
            let error = result.expect_err("a damaged scratch file fails the read");
            let AlgorithmError::Frame(FrameError::Raw(RawError::Scratch { path: got, source })) =
                &error
            else {
                panic!("{damage}: expected RawError::Scratch, got {error:?}");
            };
            assert_eq!(got, &path, "{damage}: the error names the scratch file");
            assert_eq!(source.kind(), kind, "{damage}");
        }
    }
}

#[test]
fn raw_read_scratch_bytes_counts_the_read_buffer() {
    // A 64x64 block of 6-byte pixels; a frame smaller than a block clamps it.
    for (width, height, expected) in [(100, 70, 64 * 64 * 6), (40, 30, 40 * 30 * 6)] {
        let (stack, _root, _dng) = developed_stack(width, height, DevelopStore::ScratchFile);
        assert_eq!(
            stack.read_scratch_bytes(width, height),
            expected,
            "{width}x{height} file-backed"
        );
        let (stack, _root, _dng) = developed_stack(width, height, DevelopStore::Memory);
        assert_eq!(
            stack.read_scratch_bytes(width, height),
            0,
            "{width}x{height} in memory"
        );
    }
}
