// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture- and reference-stack-backed reader tests. Unit-level, in-memory
//! header parsing lives in `crates/core/src/io/tiff/header.rs`'s own
//! `mod tests` instead -- see that file for the refusal and structural-error
//! coverage.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::TileReader;
use focusforge_core::io::{Rect, Refusal, TiffError, TiffHeader, TiffStack};

fn mantis_512_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-512")
}

fn mantis_64_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-64")
}

fn mantis_64_frame(name: &str) -> PathBuf {
    mantis_64_dir().join(name)
}

/// Copy `source` (a little-endian, classic-TIFF fixture frame) to `dest`,
/// patching its `Compression` tag (259) from 1 to 5 in place. Built under
/// `env!("CARGO_TARGET_TMPDIR")` at each call site -- never under
/// `fixtures/`, which `tools/check-fixtures.sh` polices against exactly this
/// kind of hand-edit.
fn write_compressed_copy(source: &Path, dest: &Path) {
    let mut bytes = std::fs::read(source).expect("read fixture frame");
    assert_eq!(
        &bytes[0..2],
        b"II",
        "fixture frame must be little-endian for this byte-level patch"
    );
    let ifd_offset = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let n_entries =
        u16::from_le_bytes(bytes[ifd_offset..ifd_offset + 2].try_into().unwrap()) as usize;
    let mut patched = false;
    for i in 0..n_entries {
        let entry_off = ifd_offset + 2 + i * 12;
        let tag = u16::from_le_bytes(bytes[entry_off..entry_off + 2].try_into().unwrap());
        if tag == 259 {
            // Compression: SHORT, count 1 -- always inline at entry+8..+10.
            bytes[entry_off + 8..entry_off + 10].copy_from_slice(&5u16.to_le_bytes());
            patched = true;
            break;
        }
    }
    assert!(
        patched,
        "Compression tag (259) not found in {}",
        source.display()
    );
    std::fs::write(dest, bytes).expect("write compressed copy");
}

/// Shell out to `tools/lib/rawtiff.py`, an independent Python reader, for
/// `rect` on `image_path`, returning the samples it read as `u16`. `None` --
/// a skip, not a failure -- when `python3` is not available.
fn golden_rect(image_path: &std::path::Path, rect: Rect, tag: &str) -> Option<Vec<u16>> {
    let script = common::workspace_root()
        .join("tools")
        .join("lib")
        .join("rawtiff.py");
    let out_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("golden-{tag}.bin"));
    let spec = format!("{},{},{},{}", rect.x, rect.y, rect.width, rect.height);

    let output = match std::process::Command::new("python3")
        .arg(&script)
        .arg("--rect")
        .arg(&spec)
        .arg("--out")
        .arg(&out_path)
        .arg(image_path)
        .output()
    {
        Ok(output) => output,
        Err(_) => {
            eprintln!("skip: python3 not on PATH, skipping golden comparison");
            return None;
        }
    };
    assert!(
        output.status.success(),
        "rawtiff.py --rect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let bytes = std::fs::read(&out_path).expect("golden output file must be readable");
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

#[test]
fn opens_fixture_stack() {
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    assert_eq!(stack.len(), 8);
    assert_eq!(stack.dimensions(), (512, 512));
}

#[test]
fn skips_non_tiff_and_refuses_empty_dir() {
    let tmp =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("skips_non_tiff_and_refuses_empty_dir");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    std::fs::copy(
        mantis_64_dir().join("frames").join("00.tif"),
        tmp.join("00.tif"),
    )
    .unwrap();
    std::fs::copy(
        mantis_64_dir().join("frames").join("01.tif"),
        tmp.join("01.tif"),
    )
    .unwrap();
    std::fs::write(tmp.join("README.md"), b"not a frame").unwrap();

    let stack = TiffStack::open(&tmp).expect("directory with 2 tiffs and a README must open");
    assert_eq!(stack.len(), 2, "README.md must not become a frame");

    let empty = tmp.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    match TiffStack::open(&empty) {
        Err(TiffError::NoFrames { .. }) => {}
        other => panic!("expected NoFrames for an empty directory, got {other:?}"),
    }
}

#[test]
fn tile_matches_rawtiff_rect() {
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    let rect = Rect {
        x: 100,
        y: 150,
        width: 48,
        height: 48,
    };
    let tile = stack.read_tile(0, rect).expect("tile read must succeed");

    let frame_path = mantis_512_dir().join("frames").join("00.tif");
    let Some(golden) = golden_rect(&frame_path, rect, "tile_matches_rawtiff_rect") else {
        return;
    };
    assert_eq!(tile.samples, golden);
}

#[test]
fn mm_and_ii_references_agree() {
    let paths = vec![
        mantis_64_frame("reference.tif"),
        mantis_64_frame("reference-MM-rps8.tif"),
    ];
    let stack = TiffStack::open_files(&paths).expect("mixed II/MM references must open together");

    let rect = Rect {
        x: 5,
        y: 3,
        width: 48,
        height: 48,
    };
    let ii_tile = stack.read_tile(0, rect).expect("II reference tile");
    let mm_tile = stack.read_tile(1, rect).expect("MM reference tile");
    assert_eq!(
        ii_tile.samples, mm_tile.samples,
        "reference.tif and reference-MM-rps8.tif must be the same pixels"
    );
}

#[test]
fn tile_across_strip_boundary() {
    // reference-MM-rps8.tif is 64x64, RowsPerStrip=8: a 48px tile starting
    // at y=4 spans strips 0 through 6 (rows 4..52 over 8-row strips).
    let paths = vec![
        mantis_64_frame("reference.tif"),
        mantis_64_frame("reference-MM-rps8.tif"),
    ];
    let stack = TiffStack::open_files(&paths).expect("mixed II/MM references must open together");

    let rect = Rect {
        x: 0,
        y: 4,
        width: 48,
        height: 48,
    };
    let ii_tile = stack.read_tile(0, rect).expect("II reference tile");
    let mm_tile = stack
        .read_tile(1, rect)
        .expect("MM reference tile spanning strip boundaries");
    assert_eq!(ii_tile.samples, mm_tile.samples);
}

#[test]
fn tile_at_frame_edge() {
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    let rect = Rect {
        x: 464,
        y: 464,
        width: 48,
        height: 48,
    };
    let tile = stack
        .read_tile(0, rect)
        .expect("edge tile must succeed, not be clipped away");
    assert_eq!(tile.width(), 48);
    assert_eq!(tile.height(), 48);
    assert_eq!(tile.samples.len(), 48 * 48 * 3);

    let frame_path = mantis_512_dir().join("frames").join("00.tif");
    let Some(golden) = golden_rect(&frame_path, rect, "tile_at_frame_edge") else {
        return;
    };
    assert_eq!(tile.samples, golden);
}

#[test]
fn tile_out_of_bounds_is_an_error() {
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");

    let past_right = Rect {
        x: 500,
        y: 0,
        width: 48,
        height: 48,
    };
    match stack.read_tile(0, past_right) {
        Err(TiffError::OutOfBounds { .. }) => {}
        other => panic!("expected OutOfBounds past the right edge, got {other:?}"),
    }

    let zero_sized = Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 10,
    };
    match stack.read_tile(0, zero_sized) {
        Err(TiffError::OutOfBounds { .. }) => {}
        other => panic!("expected OutOfBounds for a zero-sized rect, got {other:?}"),
    }
}

#[test]
fn read_tile_into_checks_buffer_size() {
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    let rect = Rect {
        x: 0,
        y: 0,
        width: 48,
        height: 48,
    };
    let mut too_small = vec![0u16; 10];
    match stack.read_tile_into(0, rect, &mut too_small) {
        Err(TiffError::BufferSize { expected, got }) => {
            assert_eq!(expected, 48 * 48 * 3);
            assert_eq!(got, 10);
        }
        other => panic!("expected BufferSize, got {other:?}"),
    }
}

#[test]
fn reads_big_endian() {
    let stack = TiffStack::open_files(&[mantis_64_frame("reference-MM-rps8.tif")])
        .expect("MM reference must open");
    let rect = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 64,
    };
    let tile = stack
        .read_tile(0, rect)
        .expect("full-frame MM tile must read");
    assert_eq!(tile.samples.len(), 64 * 64 * 3);
}

#[test]
fn reads_any_rows_per_strip() {
    // rps=1 (the real stack's frames' own layout).
    let stack_rps1 = TiffStack::open_files(&[mantis_64_frame("reference.tif")]).unwrap();
    assert_eq!(stack_rps1.header(0).rows_per_strip, 1);
    let rect = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 64,
    };
    let tile_rps1 = stack_rps1.read_tile(0, rect).unwrap();

    // rps=8 (Mantis_Stacked.tif's own layout).
    let stack_rps8 = TiffStack::open_files(&[mantis_64_frame("reference-MM-rps8.tif")]).unwrap();
    assert_eq!(stack_rps8.header(0).rows_per_strip, 8);
    let tile_rps8 = stack_rps8.read_tile(0, rect).unwrap();

    assert_eq!(tile_rps1.samples, tile_rps8.samples);
}

#[test]
fn refuses_mismatched_frames() {
    let paths = vec![
        mantis_512_dir().join("frames").join("00.tif"),
        mantis_64_dir().join("frames").join("00.tif"),
    ];
    match TiffStack::open_files(&paths) {
        Err(TiffError::FrameMismatch { path, .. }) => {
            assert_eq!(path, mantis_64_dir().join("frames").join("00.tif"));
        }
        other => panic!("expected FrameMismatch, got {other:?}"),
    }
}

#[test]
fn accepts_mixed_byte_order_and_rps() {
    let paths = vec![
        mantis_64_frame("reference.tif"),
        mantis_64_frame("reference-MM-rps8.tif"),
    ];
    let stack = TiffStack::open_files(&paths).expect("byte order and rps must not be compared");
    assert_eq!(stack.len(), 2);
}

#[test]
fn open_files_refuses_a_compressed_frame() {
    let dest = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("open_files_refuses_a_compressed_frame.tif");
    write_compressed_copy(&mantis_64_dir().join("frames").join("00.tif"), &dest);

    match TiffStack::open_files(std::slice::from_ref(&dest)) {
        Err(TiffError::Unsupported {
            path,
            refusal: Refusal::Compressed(5),
        }) => {
            assert_eq!(path, dest);
        }
        other => panic!("expected Unsupported(Compressed(5)) naming {dest:?}, got {other:?}"),
    }
}

#[test]
fn open_dir_refuses_a_compressed_frame() {
    let tmp =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("open_dir_refuses_a_compressed_frame");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let dest = tmp.join("00.tif");
    write_compressed_copy(&mantis_64_dir().join("frames").join("00.tif"), &dest);

    match TiffStack::open(&tmp) {
        Err(TiffError::Unsupported {
            path,
            refusal: Refusal::Compressed(5),
        }) => {
            assert_eq!(path, dest);
        }
        other => panic!("expected Unsupported(Compressed(5)) naming {dest:?}, got {other:?}"),
    }
}

#[test]
fn header_probe_reports_refusal_without_error() {
    let dest = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("header_probe_reports_refusal_without_error.tif");
    write_compressed_copy(&mantis_64_dir().join("frames").join("00.tif"), &dest);

    let header = TiffHeader::open(&dest).expect("a compressed file must still probe Ok");
    assert_eq!(header.refusal, Some(Refusal::Compressed(5)));
    assert_eq!(header.width, 64);
    assert_eq!(header.height, 64);
}

#[test]
fn tiff_stack_is_a_tile_reader() {
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    let rect = Rect {
        x: 100,
        y: 150,
        width: 48,
        height: 48,
    };

    let inherent = stack
        .read_tile(0, rect)
        .expect("inherent read must succeed");

    let mut via_trait = vec![0u16; inherent.samples.len()];
    TileReader::read_tile_into(&stack, 0, rect, &mut via_trait).expect("trait read must succeed");

    assert_eq!(
        inherent.samples, via_trait,
        "&dyn TileReader must read the same bytes as the inherent method"
    );
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn reads_the_reference_stack() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        eprintln!("skip: reference stack not found");
        return;
    };

    let mut frame_paths: Vec<PathBuf> = std::fs::read_dir(&stack_dir)
        .expect("the reference stack must be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("20260829-IMG_") && n.to_ascii_lowercase().ends_with(".tif")
            })
        })
        .collect();
    frame_paths.sort();
    assert_eq!(
        frame_paths.len(),
        61,
        "expected 61 source frames in the reference stack"
    );

    let reference_path = stack_dir.join("Mantis_Stacked.tif");

    let start = std::time::Instant::now();
    let stack = TiffStack::open_files(&frame_paths).expect("all 61 source frames must open");
    assert_eq!(stack.len(), 61);
    assert_eq!(stack.dimensions(), (8192, 5464));

    let rect = Rect {
        x: 0,
        y: 0,
        width: 256,
        height: 256,
    };
    for i in 0..stack.len() {
        let header = stack.header(i);
        assert_eq!(header.samples_per_pixel, 3);
        assert_eq!(header.bits_per_sample.first().copied(), Some(16));
        assert_eq!(header.compression, 1);
        assert_eq!(header.planar_config, 1);
        assert_eq!(header.photometric, 2);
        assert_eq!(header.rows_per_strip, 1);
        assert_eq!(header.strip_count(), 5464);
        assert_eq!(header.row_bytes(), 49152);

        let tile = stack
            .read_tile(i, rect)
            .expect("one 256px tile per frame must read");
        assert_eq!(tile.samples.len(), 256 * 256 * 3);
    }

    let reference_stack =
        TiffStack::open_files(&[reference_path]).expect("Mantis_Stacked.tif must open");
    let reference_header = reference_stack.header(0);
    assert_eq!(reference_stack.dimensions(), (8192, 5464));
    assert_eq!(reference_header.rows_per_strip, 8);
    assert_eq!(reference_header.strip_count(), 683);
    let reference_tile = reference_stack
        .read_tile(0, rect)
        .expect("reference tile must read");
    assert_eq!(reference_tile.samples.len(), 256 * 256 * 3);

    eprintln!(
        "reads_the_reference_stack: opened 61 frames + reference, read 62 tiles in {:?}",
        start.elapsed()
    );
}
