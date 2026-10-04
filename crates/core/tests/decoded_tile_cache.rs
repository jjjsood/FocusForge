// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::Duration;

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::host::{FixedFacts, HostFacts, HostProbe};
use focusforge_core::io::{DecodedTiffStack, FrameReader, Rect, TiffError, TiffStack};
use focusforge_core::params::StackContext;
use focusforge_core::tile::{
    HostBudget, NoopPreprocess, TileConsumer, TileJob, TileScheduler, TileSchedulerOptions,
};
use rayon::ThreadPoolBuilder;

fn compressed_set(name: &str) -> std::path::PathBuf {
    common::fixtures_dir().join(name)
}

#[test]
fn rects_match_the_uncompressed_twin() {
    let fixtures = common::fixtures_dir();
    let mut sets = std::fs::read_dir(&fixtures)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("mantis-64-"))
        })
        .collect::<Vec<_>>();
    sets.sort();
    assert_eq!(sets.len(), 24);
    let names = ["00.tif", "01.tif", "02.tif", "reference.tif"];
    let compressed_paths = |set: &std::path::Path| {
        names
            .iter()
            .map(|name| {
                if *name == "reference.tif" {
                    set.join(name)
                } else {
                    set.join("frames").join(name)
                }
            })
            .collect::<Vec<_>>()
    };
    let reference_paths = names
        .iter()
        .map(|name| {
            if *name == "reference.tif" {
                fixtures.join("mantis-64").join(name)
            } else {
                fixtures.join("mantis-64").join("frames").join(name)
            }
        })
        .collect::<Vec<_>>();
    let mut rects = vec![
        Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        },
        Rect {
            x: 7,
            y: 11,
            width: 29,
            height: 25,
        },
        Rect {
            x: 47,
            y: 47,
            width: 17,
            height: 17,
        },
        Rect {
            x: 63,
            y: 63,
            width: 1,
            height: 1,
        },
    ];
    let mut seed = 0x5EED_u32;
    for _ in 0..16 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let x = seed % 64;
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let y = seed % 64;
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let width = 1 + seed % (64 - x);
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let height = 1 + seed % (64 - y);
        rects.push(Rect {
            x,
            y,
            width,
            height,
        });
    }
    for set in sets {
        let decoded = DecodedTiffStack::open_files(&compressed_paths(&set), 64 * 1024).unwrap();
        let through_trait: &dyn FrameReader = &decoded;
        assert_eq!(through_trait.len(), 4);
        let reference = TiffStack::open_files(&reference_paths).unwrap();
        for frame in 0..4 {
            for &rect in &rects {
                let mut actual = vec![0; (rect.width * rect.height * 3) as usize];
                let mut expected = actual.clone();
                through_trait
                    .read_tile_into(frame, rect, &mut actual)
                    .unwrap();
                reference
                    .read_tile_into(frame, rect, &mut expected)
                    .unwrap();
                assert_eq!(actual, expected, "{} frame {frame} {rect:?}", set.display());
            }
        }
    }
}

#[test]
fn resident_bytes_never_exceed_the_bound() {
    let path = compressed_set("mantis-64-deflate-p1-strip-ii");
    let reader = DecodedTiffStack::open(path, 8_448).unwrap();
    for y in [0, 11, 22, 33, 44, 55] {
        let height: u32 = if y == 55 { 9 } else { 11 };
        let mut samples = vec![0; 64 * height as usize * 3];
        reader
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y,
                    width: 64,
                    height,
                },
                &mut samples,
            )
            .unwrap();
        assert!(reader.cache_stats().resident_bytes <= 8_448);
    }
    let stats = reader.cache_stats();
    assert!(stats.evictions > 0);
    assert!(stats.peak_resident_bytes <= 8_448);
}

#[test]
fn whole_frame_sweeps_respect_one_and_two_segment_capacities() {
    let fixtures = common::fixtures_dir();
    let compressed = fixtures
        .join("mantis-64-deflate-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let oracle = fixtures.join("mantis-64").join("frames").join("00.tif");
    let expected = TiffStack::open_files(&[oracle])
        .unwrap()
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
        )
        .unwrap();
    for capacity in [4_224, 8_448] {
        let reader =
            DecodedTiffStack::open_files(std::slice::from_ref(&compressed), capacity).unwrap();
        let actual = reader
            .read_tile(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
            )
            .unwrap();
        assert_eq!(actual.samples, expected.samples);
        let stats = reader.cache_stats();
        assert!(stats.resident_bytes <= capacity);
        assert!(stats.peak_resident_bytes <= capacity);
        assert!(stats.evictions > 0);
    }
}

#[test]
fn eviction_then_reread_is_identical() {
    let path = compressed_set("mantis-64-lzw-p1-tile-ii");
    let reader = DecodedTiffStack::open(path, 13_824).unwrap();
    let rect = Rect {
        x: 0,
        y: 0,
        width: 48,
        height: 48,
    };
    let mut first = vec![0; 48 * 48 * 3];
    reader.read_tile_into(0, rect, &mut first).unwrap();
    let mut other = vec![0; 16 * 48 * 3];
    reader
        .read_tile_into(
            0,
            Rect {
                x: 48,
                y: 0,
                width: 16,
                height: 48,
            },
            &mut other,
        )
        .unwrap();
    let before = reader.cache_stats().decodes;
    let mut reread = vec![0; 48 * 48 * 3];
    reader.read_tile_into(0, rect, &mut reread).unwrap();
    assert_eq!(first, reread);
    assert_eq!(reader.cache_stats().decodes, before + 1);
}

#[test]
fn concurrent_misses_decode_once() {
    let reader = Arc::new(
        DecodedTiffStack::open(compressed_set("mantis-64-lzw-p1-tile-ii"), 64 * 1024).unwrap(),
    );
    let start = Arc::new(Barrier::new(8));
    let samples = Arc::new(Mutex::new(Vec::new()));
    let pool = ThreadPoolBuilder::new().num_threads(8).build().unwrap();
    pool.scope(|scope| {
        for _ in 0..8 {
            let reader = Arc::clone(&reader);
            let start = Arc::clone(&start);
            let samples = Arc::clone(&samples);
            scope.spawn(move |_| {
                let mut output = vec![0; 48 * 48 * 3];
                start.wait();
                reader
                    .read_tile_into(
                        0,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 48,
                            height: 48,
                        },
                        &mut output,
                    )
                    .unwrap();
                samples.lock().unwrap().push(output);
            });
        }
    });
    let samples = samples.lock().unwrap();
    assert!(samples.windows(2).all(|pair| pair[0] == pair[1]));
    let stats = reader.cache_stats();
    assert_eq!(stats.decodes, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 7);
}

#[test]
fn concurrent_multi_segment_reads_stay_bounded() {
    let reader = Arc::new(
        DecodedTiffStack::open(compressed_set("mantis-64-lzw-p1-tile-ii"), 13_824).unwrap(),
    );
    let start = Arc::new(Barrier::new(2));
    let pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
    pool.scope(|scope| {
        for rect in [
            Rect {
                x: 0,
                y: 0,
                width: 48,
                height: 48,
            },
            Rect {
                x: 48,
                y: 0,
                width: 16,
                height: 48,
            },
        ] {
            let reader = Arc::clone(&reader);
            let start = Arc::clone(&start);
            scope.spawn(move |_| {
                let mut samples = vec![0; (rect.width * rect.height * 3) as usize];
                start.wait();
                reader.read_tile_into(0, rect, &mut samples).unwrap();
            });
        }
    });
    let stats = reader.cache_stats();
    assert_eq!(stats.decodes, 2);
    assert!(stats.peak_resident_bytes <= 13_824);
}

#[test]
fn rayon_multi_segment_one_capacity_completes_with_oracle() {
    let fixtures = common::fixtures_dir();
    let compressed = fixtures
        .join("mantis-64-lzw-p1-tile-ii")
        .join("frames")
        .join("00.tif");
    let reader = Arc::new(DecodedTiffStack::open_files(&[compressed], 13_824).unwrap());
    let oracle =
        TiffStack::open_files(&[fixtures.join("mantis-64").join("frames").join("00.tif")]).unwrap();
    let rects = [
        Rect {
            x: 0,
            y: 0,
            width: 48,
            height: 48,
        },
        Rect {
            x: 48,
            y: 0,
            width: 16,
            height: 48,
        },
    ];
    let expected = rects
        .iter()
        .map(|&rect| oracle.read_tile(0, rect).unwrap().samples)
        .collect::<Vec<_>>();
    let start = Arc::new(Barrier::new(2));
    let (done_tx, done_rx) = mpsc::channel();
    let runner = std::thread::spawn({
        let reader = Arc::clone(&reader);
        let start = Arc::clone(&start);
        move || {
            let pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
            pool.scope(|scope| {
                for (rect, expected) in rects.into_iter().zip(expected) {
                    let reader = Arc::clone(&reader);
                    let start = Arc::clone(&start);
                    scope.spawn(move |_| {
                        start.wait();
                        let actual = reader.read_tile(0, rect).unwrap();
                        assert_eq!(actual.samples, expected);
                    });
                }
            });
            done_tx.send(()).unwrap();
        }
    });
    done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    runner.join().unwrap();
    let stats = reader.cache_stats();
    assert_eq!(stats.decodes, 2);
    assert!(stats.peak_resident_bytes <= 13_824);
}

#[test]
fn bound_below_one_segment_is_refused() {
    let path = compressed_set("mantis-64-deflate-p1-tile-ii");
    let error = DecodedTiffStack::open(path, 13_823).unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, TiffError::DecodedCacheTooSmall { .. }));
    assert!(message.contains("13823"));
    assert!(message.contains("13824"));
    assert!(DecodedTiffStack::open(compressed_set("mantis-64-deflate-p1-tile-ii"), 13_824).is_ok());
}

#[test]
fn zero_and_later_largest_segment_bounds_are_refused() {
    let fixtures = common::fixtures_dir();
    let strip = fixtures
        .join("mantis-64-deflate-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let tile = fixtures
        .join("mantis-64-deflate-p1-tile-ii")
        .join("frames")
        .join("00.tif");
    let zero = DecodedTiffStack::open_files_with_budget(
        std::slice::from_ref(&strip),
        HostBudget::explicit(0),
    )
    .unwrap_err();
    assert!(matches!(
        zero,
        TiffError::DecodedCacheTooSmall { bound: 0, .. }
    ));
    let later = DecodedTiffStack::open_files(&[strip, tile.clone()], 5_000).unwrap_err();
    assert!(matches!(later, TiffError::DecodedCacheTooSmall { .. }));
    assert!(later.to_string().contains(&tile.display().to_string()));
}

#[test]
fn uniformity_is_lenient_like_tiffstack() {
    let fixtures = common::fixtures_dir();
    let compressed = fixtures
        .join("mantis-64-lzw-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let uncompressed = fixtures.join("mantis-64").join("frames").join("01.tif");
    let mixed =
        DecodedTiffStack::open_files(&[compressed.clone(), uncompressed.clone()], 64 * 1024)
            .unwrap();
    assert_eq!(mixed.frame(0).path, compressed);
    assert_eq!(mixed.frame(1).path, uncompressed);

    let mismatched = fixtures.join("mantis-512").join("frames").join("01.tif");
    let error =
        DecodedTiffStack::open_files(&[compressed, mismatched.clone()], 64 * 1024).unwrap_err();
    assert!(matches!(error, TiffError::FrameMismatch { .. }));
    assert!(
        error
            .to_string()
            .contains(&mismatched.display().to_string())
    );
}

#[test]
fn mixed_codec_layouts_preserve_input_order_and_samples() {
    let fixtures = common::fixtures_dir();
    let inputs = vec![
        fixtures.join("mantis-64").join("frames").join("00.tif"),
        fixtures
            .join("mantis-64-lzw-p1-strip-ii")
            .join("frames")
            .join("01.tif"),
        fixtures
            .join("mantis-64-deflate-p2-tile-mm")
            .join("frames")
            .join("02.tif"),
        fixtures
            .join("mantis-64-old-deflate-p1-tile-ii")
            .join("frames")
            .join("00.tif"),
    ];
    let oracle_paths = [
        fixtures.join("mantis-64").join("frames").join("00.tif"),
        fixtures.join("mantis-64").join("frames").join("01.tif"),
        fixtures.join("mantis-64").join("frames").join("02.tif"),
        fixtures.join("mantis-64").join("frames").join("00.tif"),
    ];
    let reader = DecodedTiffStack::open_files(&inputs, 64 * 1024).unwrap();
    for (index, path) in inputs.iter().enumerate() {
        assert_eq!(&reader.frame(index).path, path);
        let expected = TiffStack::open_files(std::slice::from_ref(&oracle_paths[index]))
            .unwrap()
            .read_tile(
                0,
                Rect {
                    x: 7,
                    y: 11,
                    width: 29,
                    height: 25,
                },
            )
            .unwrap();
        let actual = reader
            .read_tile(
                index,
                Rect {
                    x: 7,
                    y: 11,
                    width: 29,
                    height: 25,
                },
            )
            .unwrap();
        assert_eq!(actual.samples, expected.samples);
    }
}

#[test]
fn uniformity_mismatches_and_unsupported_codecs_are_typed() {
    let fixtures = common::fixtures_dir();
    let source = fixtures
        .join("mantis-64-lzw-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scratch = workspace.join(".scratch/decoded-cache-tests");
    std::fs::create_dir_all(&scratch).unwrap();
    for (tag, value) in [(256, 63_u32), (257, 63), (277, 1), (262, 0), (284, 2)] {
        let path = scratch.join(format!("uniform-mismatch-{tag}.tif"));
        let mut bytes = std::fs::read(&source).unwrap();
        patch_tag_value(&mut bytes, tag, value);
        std::fs::write(&path, bytes).unwrap();
        let error =
            DecodedTiffStack::open_files(&[source.clone(), path.clone()], 64 * 1024).unwrap_err();
        assert!(
            matches!(error, TiffError::FrameMismatch { .. }),
            "tag {tag}: {error}"
        );
        assert!(error.to_string().contains(&path.display().to_string()));
        std::fs::remove_file(path).unwrap();
    }
    let unsupported = scratch.join("unsupported-codec.tif");
    let mut bytes = std::fs::read(&source).unwrap();
    patch_tag_value(&mut bytes, 259, 7);
    std::fs::write(&unsupported, bytes).unwrap();
    let error =
        DecodedTiffStack::open_files(std::slice::from_ref(&unsupported), 64 * 1024).unwrap_err();
    assert!(matches!(error, TiffError::UnsupportedDecoded { .. }));
    assert!(
        error
            .to_string()
            .contains(&unsupported.display().to_string())
    );
    std::fs::remove_file(unsupported).unwrap();
    assert!(matches!(
        DecodedTiffStack::open_files(&[], 64 * 1024),
        Err(TiffError::NoFrames { .. })
    ));
}

#[test]
fn first_differing_bit_depth_is_a_typed_frame_mismatch() {
    let fixtures = common::fixtures_dir();
    let source = fixtures
        .join("mantis-64-lzw-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scratch = workspace.join(".scratch/decoded-cache-tests");
    std::fs::create_dir_all(&scratch).unwrap();
    let mismatch = scratch.join("uniform-mismatch-bits-per-sample.tif");
    let mut bytes = std::fs::read(&source).unwrap();
    patch_tag_value(&mut bytes, 258, 8);
    std::fs::write(&mismatch, bytes).unwrap();
    let error = DecodedTiffStack::open_files(&[source, mismatch.clone()], 64 * 1024).unwrap_err();
    assert!(matches!(error, TiffError::FrameMismatch { .. }));
    assert!(error.to_string().contains(&mismatch.display().to_string()));
    std::fs::remove_file(mismatch).unwrap();
}

#[test]
fn bound_follows_host_budget_and_ceiling() {
    let facts = FixedFacts(HostFacts::new(1_000, 101, Some(500), Some(99), 4));
    let derived = HostBudget::host_derived(&facts.facts());
    let split = derived.partition_for_decoded_cache();
    assert_eq!(
        split.cache.bytes() + split.scheduler.bytes(),
        derived.bytes()
    );
    assert_eq!(split.scheduler.provenance(), derived.provenance());
    assert_eq!(split.scheduler.arms_back_off(), derived.arms_back_off());

    let below = derived.capped_to(Some(derived.bytes() - 1));
    let equal = derived.capped_to(Some(derived.bytes()));
    let above = derived.capped_to(Some(derived.bytes() + 1));
    assert_eq!(below.bytes(), derived.bytes() - 1);
    assert_eq!(equal, derived);
    assert_eq!(above, derived);

    let capped = HostBudget::explicit(101)
        .capped_to(Some(99))
        .partition_for_decoded_cache();
    assert_eq!(capped.cache.bytes(), 24);
    assert_eq!(capped.scheduler.bytes(), 75);
    assert_eq!(capped.cache.bytes() + capped.scheduler.bytes(), 99);
    assert_eq!(
        capped.scheduler.provenance(),
        HostBudget::explicit(1).provenance()
    );
    assert!(!capped.scheduler.arms_back_off());
    let odd = HostBudget::explicit(101).partition_for_decoded_cache();
    assert_eq!((odd.cache.bytes(), odd.scheduler.bytes()), (25, 76));
    let zero = HostBudget::explicit(0).partition_for_decoded_cache();
    assert_eq!((zero.cache.bytes(), zero.scheduler.bytes()), (0, 0));
    let reader = DecodedTiffStack::open_files_with_budget(
        &[compressed_set("mantis-64-deflate-p1-tile-ii")
            .join("frames")
            .join("00.tif")],
        HostBudget::explicit(60_000),
    )
    .unwrap();
    let mut samples = vec![0; 48 * 48 * 3];
    reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 48,
                height: 48,
            },
            &mut samples,
        )
        .unwrap();
    assert!(reader.cache_stats().peak_resident_bytes <= 15_000);
    for facts in [
        FixedFacts(HostFacts::new(1_000, 800, None, None, 4)),
        FixedFacts(HostFacts::fallback()),
        FixedFacts(HostFacts::new(1_000, 101, Some(500), Some(99), 4)),
    ] {
        let host = HostBudget::host_derived(&facts.facts());
        for limit in [host.bytes() - 1, host.bytes(), host.bytes() + 1] {
            let capped = host.capped_to(Some(limit));
            let partition = capped.partition_for_decoded_cache();
            assert_eq!(
                partition.cache.bytes() + partition.scheduler.bytes(),
                capped.bytes()
            );
            assert_eq!(partition.cache.bytes(), capped.bytes() / 4);
            assert_eq!(
                partition.scheduler.bytes(),
                capped.bytes() - capped.bytes() / 4
            );
            assert_eq!(partition.scheduler.provenance(), capped.provenance());
            assert_eq!(partition.scheduler.arms_back_off(), capped.arms_back_off());
        }
    }
}

fn patch_tag_value(bytes: &mut [u8], wanted_tag: u16, value: u32) {
    assert_eq!(&bytes[..2], b"II");
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
    for index in 0..entries {
        let entry = ifd + 2 + index * 12;
        let tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
        if tag != wanted_tag {
            continue;
        }
        let field_type = u16::from_le_bytes(bytes[entry + 2..entry + 4].try_into().unwrap());
        let count = u32::from_le_bytes(bytes[entry + 4..entry + 8].try_into().unwrap());
        let bytes_per_value = match field_type {
            3 => 2,
            4 => 4,
            _ => panic!("unsupported test field type {field_type}"),
        };
        let offset = if count * bytes_per_value <= 4 {
            entry + 8
        } else {
            u32::from_le_bytes(bytes[entry + 8..entry + 12].try_into().unwrap()) as usize
        };
        match field_type {
            3 => bytes[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes()),
            4 => bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes()),
            _ => unreachable!(),
        }
        return;
    }
    panic!("missing TIFF tag {wanted_tag}");
}

#[test]
fn boundary_errors_do_not_mutate_output() {
    let reader =
        DecodedTiffStack::open(compressed_set("mantis-64-lzw-p1-strip-ii"), 64 * 1024).unwrap();
    let mut output = vec![9; 12];
    let error = reader
        .read_tile_into(
            0,
            Rect {
                x: 64,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        )
        .unwrap_err();
    assert!(matches!(error, TiffError::OutOfBounds { .. }));
    assert_eq!(output, vec![9; 12]);
    for rect in [
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
        Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 0,
        },
    ] {
        let error = reader.read_tile_into(0, rect, &mut output).unwrap_err();
        assert!(matches!(error, TiffError::OutOfBounds { .. }));
        assert_eq!(output, vec![9; 12]);
    }
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
        .unwrap_err();
    assert!(matches!(error, TiffError::BufferSize { .. }));
    assert_eq!(output, vec![9; 12]);
    let error = reader
        .read_tile_into(
            3,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        )
        .unwrap_err();
    assert!(matches!(error, TiffError::FrameIndex { .. }));
    assert_eq!(output, vec![9; 12]);
    let error = reader
        .read_tile_into(
            0,
            Rect {
                x: u32::MAX,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut output,
        )
        .unwrap_err();
    assert!(matches!(error, TiffError::OutOfBounds { .. }));
    assert_eq!(output, vec![9; 12]);
}

#[test]
fn boundary_errors_release_waiters() {
    let fixtures = common::fixtures_dir();
    let source = fixtures
        .join("mantis-64-deflate-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scratch = workspace.join(".scratch/decoded-cache-tests");
    std::fs::create_dir_all(&scratch).unwrap();
    let malformed = scratch.join("malformed-last-segment.tif");
    let mut bytes = std::fs::read(&source).unwrap();
    bytes.pop();
    std::fs::write(&malformed, bytes).unwrap();

    let reader =
        Arc::new(DecodedTiffStack::open_files(&[malformed.clone(), source], 4_224).unwrap());
    let start = Arc::new(Barrier::new(2));
    let broken = Arc::clone(&reader);
    let broken_start = Arc::clone(&start);
    let bad = std::thread::spawn(move || {
        let mut out = vec![0; 64 * 9 * 3];
        broken_start.wait();
        broken.read_tile_into(
            0,
            Rect {
                x: 0,
                y: 55,
                width: 64,
                height: 9,
            },
            &mut out,
        )
    });
    let healthy = Arc::clone(&reader);
    let healthy_start = Arc::clone(&start);
    let good = std::thread::spawn(move || {
        let mut out = vec![0; 64 * 11 * 3];
        healthy_start.wait();
        healthy.read_tile_into(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 64,
                height: 11,
            },
            &mut out,
        )
    });
    assert!(matches!(
        bad.join().unwrap(),
        Err(TiffError::SegmentPastEof { .. })
    ));
    assert!(good.join().unwrap().is_ok());
    std::fs::remove_file(malformed).unwrap();
}

/// The largest decoded segment a set holds, read off the refusal an empty
/// cache gives.
fn largest_segment_bytes(paths: &[std::path::PathBuf]) -> u64 {
    match DecodedTiffStack::open_files(paths, 0).unwrap_err() {
        TiffError::DecodedCacheTooSmall { required, .. } => required,
        other => panic!("expected DecodedCacheTooSmall, got {other:?}"),
    }
}

#[test]
fn decode_scratch_is_twice_the_largest_stored_segment_plus_one() {
    let fixtures = common::fixtures_dir();
    let frame = |set: &str, name: &str| fixtures.join(set).join("frames").join(name);
    let sets = [
        vec![frame("mantis-64-deflate-p1-strip-ii", "00.tif")],
        vec![frame("mantis-64-lzw-p1-tile-ii", "00.tif")],
        vec![
            frame("mantis-64", "00.tif"),
            frame("mantis-64-lzw-p1-strip-ii", "01.tif"),
            frame("mantis-64-deflate-p2-tile-mm", "02.tif"),
        ],
    ];
    for paths in sets {
        let largest = largest_segment_bytes(&paths);
        let reader = DecodedTiffStack::open_files(&paths, largest).unwrap();
        assert_eq!(
            TileReader::read_scratch_bytes(&reader, 64, 64),
            2 * largest + 1,
            "{paths:?}"
        );
    }
}

/// One tile's frame-0 samples, keyed by its read rectangle (row, column).
type TileSamples = ((u32, u32), Vec<u16>);

/// Records every tile's frame-0 samples.
struct CollectingConsumer {
    tiles: Mutex<Vec<TileSamples>>,
}

impl TileConsumer for CollectingConsumer {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let rect = job.read_rect();
        let mut samples = Vec::new();
        job.ctx().for_each_frame(|f, tile, _| {
            if f == 0 {
                samples = tile.samples.clone();
            }
            Ok(())
        })?;
        self.tiles.lock().unwrap().push(((rect.y, rect.x), samples));
        Ok(())
    }
}

fn scheduled_tiles(reader: &dyn TileReader, name: &str) -> Vec<TileSamples> {
    let scratch = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let mut options = TileSchedulerOptions::explicit_budget(scratch, 1 << 30);
    options.tile_size = 32;
    options.levels = 0;
    let scheduler =
        TileScheduler::new(reader, StackContext::new(1, 64, 64, 3), 3, &options).unwrap();
    if std::thread::available_parallelism().is_ok_and(|n| n.get() >= 2) {
        assert!(scheduler.in_flight_limit() >= 2);
    }
    let consumer = CollectingConsumer {
        tiles: Mutex::default(),
    };
    scheduler.run(&NoopPreprocess, &consumer).unwrap();
    let mut tiles = consumer.tiles.into_inner().unwrap();
    tiles.sort_by_key(|(key, _)| *key);
    tiles
}

#[test]
fn scheduler_over_a_one_segment_cache_completes_and_matches_the_twin() {
    let fixtures = common::fixtures_dir();
    let compressed = fixtures
        .join("mantis-64-lzw-p1-tile-ii")
        .join("frames")
        .join("00.tif");
    let twin =
        TiffStack::open_files(&[fixtures.join("mantis-64").join("frames").join("00.tif")]).unwrap();
    let reader = DecodedTiffStack::open_files(&[compressed], 13_824).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let actual = scheduled_tiles(&reader, "decoded_one_segment");
        let expected = scheduled_tiles(&twin, "decoded_one_segment_twin");
        done_tx.send((actual, expected)).unwrap();
    });
    let (actual, expected) = done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the scheduler run must not deadlock on a one-segment cache");
    assert_eq!(actual, expected);
}
