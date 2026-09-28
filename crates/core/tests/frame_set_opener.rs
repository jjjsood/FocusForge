// SPDX-License-Identifier: GPL-3.0-or-later

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use focusforge_core::io::{FrameError, ReaderKind, TiffError, frame_paths, open_frame_set};
use focusforge_core::tile::HostBudget;
use raw_dng::{DngSpec, write_dng};

#[test]
fn classification_picks_the_mmap_path_when_it_qualifies() {
    let fixtures = common::fixtures_dir();
    let mmap = frame_paths(&fixtures.join("mantis-64")).unwrap();
    let decoded = frame_paths(&fixtures.join("mantis-64-deflate-p1-strip-ii")).unwrap();

    assert_eq!(
        open_frame_set(&mmap, HostBudget::explicit(65_536))
            .unwrap()
            .kind,
        ReaderKind::Mmap
    );
    assert_eq!(
        open_frame_set(&decoded, HostBudget::explicit(65_536))
            .unwrap()
            .kind,
        ReaderKind::Decoded
    );
}

#[test]
fn classification_does_not_decode_a_corrupt_compressed_payload() {
    let fixtures = common::fixtures_dir();
    let source = fixtures.join("mantis-64-deflate-p1-strip-ii/frames/00.tif");
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "corrupt-compressed-opener-{}.tif",
        std::process::id()
    ));
    let mut bytes = std::fs::read(source).unwrap();
    *bytes.last_mut().unwrap() ^= 0xff;
    std::fs::write(&path, bytes).unwrap();

    let opened = open_frame_set(std::slice::from_ref(&path), HostBudget::explicit(65_536)).unwrap();
    assert_eq!(opened.kind, ReaderKind::Decoded);
    assert_eq!(opened.reader.frame(0).path, path);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn budget_partition_is_applied_once() {
    let paths = frame_paths(&common::fixtures_dir().join("mantis-64-deflate-p1-strip-ii")).unwrap();
    let opened = open_frame_set(&paths, HostBudget::explicit(65_536)).unwrap();
    assert_eq!(opened.scheduler_budget.bytes(), 49_152);
    match open_frame_set(&paths, HostBudget::explicit(1)) {
        Err(FrameError::Tiff(TiffError::DecodedCacheTooSmall {
            path,
            bound,
            required,
        })) => {
            assert_eq!(bound, 0);
            assert!(required > bound);
            assert!(path.ends_with("00.tif"));
        }
        Ok(_) => panic!("undersized decoded cache must refuse"),
        Err(error) => panic!("expected decoded-cache error, got {error}"),
    }
}

#[test]
fn incompatible_frames_are_rejected() {
    assert!(matches!(
        open_frame_set(&[], HostBudget::explicit(65_536)),
        Err(FrameError::Tiff(TiffError::NoFrames { .. }))
    ));
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("frame-set-opener-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let malformed = root.join("malformed.tif");
    std::fs::write(&malformed, b"not a TIFF").unwrap();
    assert!(matches!(
        open_frame_set(
            std::slice::from_ref(&malformed),
            HostBudget::explicit(65_536)
        ),
        Err(FrameError::Tiff(TiffError::NotATiff { .. }))
    ));

    let fixtures = common::fixtures_dir();
    let source = fixtures.join("mantis-64-lzw-p1-strip-ii/frames/00.tif");
    let incompatible = root.join("incompatible.tif");
    let mut bytes = std::fs::read(&source).unwrap();
    patch_short_tag(&mut bytes, 277, 1);
    std::fs::write(&incompatible, bytes).unwrap();
    match open_frame_set(
        &[source, incompatible.clone()],
        HostBudget::explicit(65_536),
    ) {
        Ok(_) => panic!("sample-incompatible frames must refuse"),
        Err(error) => assert!(
            error
                .to_string()
                .contains(&incompatible.display().to_string())
        ),
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mixed_sets_decode_and_unsupported_headers_refuse() {
    let fixtures = common::fixtures_dir();
    let mixed = vec![
        fixtures.join("mantis-64/frames/00.tif"),
        fixtures.join("mantis-64-lzw-p1-strip-ii/frames/01.tif"),
        fixtures.join("mantis-64-deflate-p2-tile-mm/frames/02.tif"),
    ];
    let opened = open_frame_set(&mixed, HostBudget::explicit(65_536)).unwrap();
    assert_eq!(opened.kind, ReaderKind::Decoded);
    for (index, path) in mixed.iter().enumerate() {
        assert_eq!(&opened.reader.frame(index).path, path);
    }
    let mismatch = fixtures.join("mantis-512/frames/00.tif");
    assert!(matches!(
        open_frame_set(&[mixed[0].clone(), mismatch], HostBudget::explicit(65_536)),
        Err(FrameError::Tiff(TiffError::FrameMismatch { .. }))
    ));

    let source = fixtures.join("mantis-64-lzw-p1-strip-ii/frames/00.tif");
    let mut bytes = std::fs::read(&source).unwrap();
    patch_short_tag(&mut bytes, 259, 7);
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("unsupported-opener-{}.tif", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    let error = match open_frame_set(std::slice::from_ref(&path), HostBudget::explicit(65_536)) {
        Ok(_) => panic!("unsupported codec must refuse"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("Compression=7"), "{error}");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn raw_sets_open_lazily_and_mixed_formats_name_the_differing_frame() {
    let root = tempfile::tempdir().expect("temporary directory");
    let source = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        true,
    );
    let raw = root.path().join("raw.dng");
    std::fs::copy(source.path(), &raw).expect("copy RAW source");
    let opened = open_frame_set(std::slice::from_ref(&raw), HostBudget::explicit(65_536))
        .expect("metadata-only RAW must open");
    assert_eq!(opened.kind, ReaderKind::Raw);
    assert_eq!(opened.scheduler_budget.bytes(), 49_152);
    assert_eq!(opened.reader.frame(0).path, raw);

    let tiff = common::fixtures_dir().join("mantis-64/frames/00.tif");
    let error = match open_frame_set(&[raw.clone(), tiff], HostBudget::explicit(65_536)) {
        Ok(_) => panic!("RAW and TIFF must not mix"),
        Err(error) => error,
    };
    assert!(matches!(
        &error,
        FrameError::MixedFormats { path, found: "TIFF", .. } if path.ends_with("00.tif")
    ));
    assert!(error.to_string().contains("raw.dng"));
}

fn patch_short_tag(bytes: &mut [u8], wanted_tag: u16, value: u16) {
    assert_eq!(&bytes[..2], b"II");
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
    for index in 0..entries {
        let offset = ifd + 2 + index * 12;
        if u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) != wanted_tag {
            continue;
        }
        assert_eq!(
            u16::from_le_bytes(bytes[offset + 2..offset + 4].try_into().unwrap()),
            3
        );
        let count = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let data = if count * 2 <= 4 {
            offset + 8
        } else {
            u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize
        };
        bytes[data..data + 2].copy_from_slice(&value.to_le_bytes());
        return;
    }
    panic!("missing TIFF tag {wanted_tag}");
}
