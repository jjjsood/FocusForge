// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use focusforge_core::io::{DecodedTiffStack, Rect, TiffError};
use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};

#[test]
fn decoded_reads_count_real_miss_hit_copy_and_decode_error() {
    let fixtures = common::fixtures_dir();
    let source = fixtures
        .join("mantis-64-deflate-p1-strip-ii")
        .join("frames")
        .join("00.tif");
    let reader = DecodedTiffStack::open_files(std::slice::from_ref(&source), 64 * 1024).unwrap();
    let rect = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 11,
    };
    let mut first = vec![0; 64 * 11 * 3];
    let mut second = first.clone();

    enable_trace_accounting();
    let before = timing_snapshot().expect("enabled accounting");
    reader.read_tile_into(0, rect, &mut first).unwrap();
    let after_miss = timing_snapshot().expect("accounting remains enabled");
    let miss_stats = reader.cache_stats();
    reader.read_tile_into(0, rect, &mut second).unwrap();
    let after_hit = timing_snapshot().expect("accounting remains enabled");
    let hit_stats = reader.cache_stats();

    assert_eq!(first, second);
    assert_eq!(after_miss.cache_misses - before.cache_misses, 1);
    assert_eq!(
        after_miss.decoded_bytes - before.decoded_bytes,
        64 * 11 * 3 * 2
    );
    assert_eq!(after_miss.copy_bytes - before.copy_bytes, 64 * 11 * 3 * 2);
    assert_eq!(after_hit.cache_hits - after_miss.cache_hits, 1);
    assert_eq!(after_hit.cache_misses, after_miss.cache_misses);
    assert_eq!(after_hit.decoded_bytes, after_miss.decoded_bytes);
    assert_eq!(
        after_hit.copy_bytes - after_miss.copy_bytes,
        64 * 11 * 3 * 2
    );
    assert_eq!(hit_stats.decodes, miss_stats.decodes);

    let scratch = common::workspace_root().join(".scratch/decoded-timing-accounting");
    std::fs::create_dir_all(&scratch).unwrap();
    let malformed = scratch.join(format!("malformed-segment-{}.tif", std::process::id()));
    let mut bytes = std::fs::read(&source).unwrap();
    bytes.pop();
    std::fs::write(&malformed, bytes).unwrap();
    let malformed_reader =
        DecodedTiffStack::open_files(std::slice::from_ref(&malformed), 64 * 1024).unwrap();
    let before_error = timing_snapshot().expect("accounting remains enabled");
    let mut output = vec![7; 64 * 9 * 3];
    let error = malformed_reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 55,
                width: 64,
                height: 9,
            },
            &mut output,
        )
        .unwrap_err();
    let after_error = timing_snapshot().expect("accounting remains enabled");
    assert!(matches!(error, TiffError::SegmentPastEof { .. }));
    assert_eq!(after_error.decoded_bytes, before_error.decoded_bytes);
    assert_eq!(after_error.copy_bytes, before_error.copy_bytes);
    std::fs::remove_file(malformed).unwrap();
}
