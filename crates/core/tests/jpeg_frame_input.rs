// SPDX-License-Identifier: GPL-3.0-or-later

//! JPEG frames: widening, tiling, the decoded-frame cache, refusals and EXIF
//! orientation. Frames are encoded in memory with standard Huffman tables.

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::io::{
    FrameError, FrameReader, JpegError, JpegStack, ReaderKind, Rect, open_frame_set,
};
use focusforge_core::tile::HostBudget;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

const BIG: u64 = 1 << 30;

fn pixels(width: u32, height: u32, components: usize) -> Vec<u8> {
    (0..width as usize * height as usize * components)
        .map(|i| {
            let pixel = (i / components) as u32;
            let (x, y) = (pixel % width, pixel / width);
            ((x * 5 + y * 9 + (i % components) as u32 * 40 + x * y) % 251) as u8
        })
        .collect()
}

fn encode_with(
    width: u32,
    height: u32,
    color: ColorType,
    data: &[u8],
    sampling: SamplingFactor,
    exif: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, 90);
    encoder.set_sampling_factor(sampling);
    if let Some(exif) = exif {
        encoder.add_exif_metadata(exif).expect("EXIF segment fits");
    }
    encoder
        .encode(data, width as u16, height as u16, color)
        .expect("frame encodes");
    out
}

fn encode(
    width: u32,
    height: u32,
    components: usize,
    sampling: SamplingFactor,
    exif: Option<&[u8]>,
) -> Vec<u8> {
    let color = if components == 1 {
        ColorType::Luma
    } else {
        ColorType::Rgb
    };
    let data = pixels(width, height, components);
    encode_with(width, height, color, &data, sampling, exif)
}

/// The decoder's own 8-bit output for `bytes`.
fn reference(bytes: &[u8], components: usize) -> Vec<u8> {
    let colorspace = if components == 1 {
        ColorSpace::Luma
    } else {
        ColorSpace::RGB
    };
    JpegDecoder::new_with_options(
        ZCursor::new(bytes),
        DecoderOptions::default().jpeg_set_out_colorspace(colorspace),
    )
    .decode()
    .expect("reference decode")
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("frame file");
    path
}

fn full(reader: &dyn FrameReader, frame: usize) -> Vec<u16> {
    let (width, height) = reader.dimensions();
    let spp = reader.frame(frame).samples_per_pixel as usize;
    let mut out = vec![0; width as usize * height as usize * spp];
    reader
        .read_tile_into(
            frame,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            &mut out,
        )
        .expect("whole frame reads");
    out
}

fn widened(samples: &[u8]) -> Vec<u16> {
    samples.iter().map(|&v| u16::from(v) * 257).collect()
}

fn tile_error(reader: &JpegStack, frame: usize) -> String {
    let mut out = [0; 3];
    reader
        .read_tile_into(
            frame,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut out[..reader.frame(frame).samples_per_pixel as usize],
        )
        .expect_err("frame is refused")
        .to_string()
}

#[test]
fn jpeg_samples_are_widened() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for (components, sampling) in [
        (1, SamplingFactor::F_1_1),
        (3, SamplingFactor::F_1_1),
        (3, SamplingFactor::F_2_2),
    ] {
        let bytes = encode(37, 23, components, sampling, None);
        let path = write(
            dir.path(),
            &format!("c{components}-{sampling:?}.jpg"),
            &bytes,
        );
        let set = open_frame_set(&[path], HostBudget::explicit(BIG)).expect("JPEG set opens");
        assert_eq!(set.kind, ReaderKind::Jpeg);
        assert_eq!(set.reader.dimensions(), (37, 23));
        assert_eq!(set.reader.frame(0).samples_per_pixel, components as u32);
        assert_eq!(
            full(set.reader.as_ref(), 0),
            widened(&reference(&bytes, components)),
            "{components} components, {sampling:?}"
        );
    }
}

#[test]
fn jpeg_tiles_match_whole_frame() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let bytes = encode(37, 23, 3, SamplingFactor::F_2_2, None);
    let path = write(dir.path(), "00.jpg", &bytes);
    let reader =
        JpegStack::open_files_with_budget(std::slice::from_ref(&path), BIG).expect("opens");
    let whole = full(&reader, 0);
    for y in (0..23).step_by(11) {
        for x in (0..37).step_by(16) {
            let rect = Rect {
                x,
                y,
                width: 16.min(37 - x),
                height: 11.min(23 - y),
            };
            let mut tile = vec![0; (rect.width * rect.height * 3) as usize];
            reader
                .read_tile_into(0, rect, &mut tile)
                .expect("tile reads");
            let expected: Vec<u16> = (rect.y..rect.y + rect.height)
                .flat_map(|row| {
                    let start = ((row * 37 + rect.x) * 3) as usize;
                    whole[start..start + (rect.width * 3) as usize].to_vec()
                })
                .collect();
            assert_eq!(tile, expected, "tile at {x},{y}");
        }
    }
    let outside = Rect {
        x: 30,
        y: 0,
        width: 16,
        height: 1,
    };
    assert!(matches!(
        reader.read_tile_into(0, outside, &mut [0; 48]),
        Err(AlgorithmError::Frame(FrameError::Jpeg(JpegError::OutOfBounds { path: named, .. })))
            if named == path
    ));
    let inside = Rect {
        x: 0,
        y: 0,
        width: 2,
        height: 2,
    };
    assert!(matches!(
        reader.read_tile_into(0, inside, &mut [0; 11]),
        Err(AlgorithmError::Frame(FrameError::Jpeg(
            JpegError::BufferSize {
                expected: 12,
                got: 11,
                ..
            }
        )))
    ));
}

#[test]
fn jpeg_cache_stays_within_its_bound() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let first = write(
        dir.path(),
        "00.jpg",
        &encode(37, 23, 3, SamplingFactor::F_1_1, None),
    );
    let second = write(
        dir.path(),
        "01.jpg",
        &encode(37, 23, 3, SamplingFactor::F_1_1, None),
    );
    let reservation = |path: &Path| std::fs::metadata(path).expect("size").len() + 3 * 37 * 23 * 3;
    let one_frame = reservation(&first).max(reservation(&second));
    let reader = JpegStack::open_files_with_budget(&[first.clone(), second.clone()], one_frame)
        .expect("opens");
    let before = full(&reader, 0);
    full(&reader, 1);
    // Frame 0 was evicted, so reading it decodes the file again.
    let replacement = encode(37, 23, 3, SamplingFactor::F_1_1, None);
    let changed: Vec<u8> = pixels(37, 23, 3).iter().map(|v| 255 - v).collect();
    let replaced = encode_with(
        37,
        23,
        ColorType::Rgb,
        &changed,
        SamplingFactor::F_1_1,
        None,
    );
    assert_ne!(replacement, replaced);
    std::fs::write(&first, &replaced).expect("replace frame 0");
    let after = full(&reader, 0);
    assert_ne!(before, after);
    assert_eq!(after, widened(&reference(&replaced, 3)));
    // Frame 1 was evicted by the third decode, so it is read from disk again.
    std::fs::remove_file(&second).expect("remove frame 1");
    assert!(
        reader
            .read_tile_into(
                1,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                },
                &mut [0; 3]
            )
            .is_err()
    );

    let required = reservation(&first);
    let small = JpegStack::open_files_with_budget(std::slice::from_ref(&first), required - 1)
        .expect("metadata fits without decode");
    let message = tile_error(&small, 0);
    assert!(message.contains(&first.display().to_string()), "{message}");
    assert!(message.contains(&(required - 1).to_string()), "{message}");
    assert!(message.contains(&required.to_string()), "{message}");
}

fn patch_sof(bytes: &mut [u8], marker: Option<u8>, precision: Option<u8>) {
    let mut at = 2;
    while bytes[at] == 0xff {
        if bytes[at + 1] == 0xc0 {
            if let Some(marker) = marker {
                bytes[at + 1] = marker;
            }
            if let Some(precision) = precision {
                bytes[at + 4] = precision;
            }
            return;
        }
        at += 2 + usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
    }
    panic!("no SOF0 segment");
}

#[test]
fn jpeg_refusals_name_the_value_found() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let cmyk = pixels(16, 16, 4);
    let rgb = encode(16, 16, 3, SamplingFactor::F_1_1, None);
    let mut twelve_bit = rgb.clone();
    patch_sof(&mut twelve_bit, None, Some(12));
    let mut lossless = rgb.clone();
    patch_sof(&mut lossless, Some(0xc3), None);
    let whole = encode(64, 64, 3, SamplingFactor::F_1_1, None);
    let truncated_scan = whole[..whole.len() * 3 / 5].to_vec();
    let cases: Vec<(&str, Vec<u8>, Option<&str>)> = vec![
        (
            "cmyk",
            encode_with(16, 16, ColorType::Cmyk, &cmyk, SamplingFactor::F_1_1, None),
            Some("components 4"),
        ),
        (
            "ycck",
            encode_with(16, 16, ColorType::Ycck, &cmyk, SamplingFactor::F_1_1, None),
            Some("components 4"),
        ),
        ("twelve", twelve_bit, Some("sample precision 12")),
        ("lossless", lossless, Some("coding process SOF3")),
        ("text", b"not a jpeg at all".to_vec(), None),
        ("truncated", truncated_scan, None),
    ];
    for (name, bytes, expected) in cases {
        let path = write(dir.path(), &format!("{name}.jpg"), &bytes);
        let message = match JpegStack::open_files_with_budget(std::slice::from_ref(&path), BIG) {
            Err(error) => error.to_string(),
            Ok(reader) => tile_error(&reader, 0),
        };
        assert!(
            message.contains(&path.display().to_string()),
            "{name}: {message}"
        );
        if let Some(expected) = expected {
            assert!(message.contains(expected), "{name}: {message}");
        }
    }
}

/// An EXIF TIFF blob whose IFD0 holds one SHORT entry, `tag` = `value`.
fn exif_blob(big_endian: bool, tag: u16, value: u16) -> Vec<u8> {
    let short = |v: u16| {
        if big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let long = |v: u32| {
        if big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let mut blob = Vec::from(if big_endian { *b"MM" } else { *b"II" });
    blob.extend(short(42));
    blob.extend(long(8));
    blob.extend(short(1));
    blob.extend(short(tag));
    blob.extend(short(3));
    blob.extend(long(1));
    blob.extend(short(value));
    blob.extend([0, 0]);
    blob.extend(long(0));
    blob
}

/// Stored pixel `(x, y)` of a `w` x `h` frame lands here when displayed.
fn upright_position(value: u16, x: u32, y: u32, w: u32, h: u32) -> (u32, u32) {
    match value {
        2 => (w - 1 - x, y),
        3 => (w - 1 - x, h - 1 - y),
        4 => (x, h - 1 - y),
        5 => (y, x),
        6 => (h - 1 - y, x),
        7 => (h - 1 - y, w - 1 - x),
        8 => (y, w - 1 - x),
        _ => (x, y),
    }
}

#[test]
fn jpeg_orientation_is_applied() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (w, h) = (21_u32, 13_u32);
    let plain = encode(w, h, 3, SamplingFactor::F_1_1, None);
    let stored = widened(&reference(&plain, 3));
    // (EXIF payload, orientation the frame must read as)
    let mut cases: Vec<(Option<Vec<u8>>, u16)> = Vec::new();
    for big_endian in [false, true] {
        for value in 1..=8 {
            cases.push((Some(exif_blob(big_endian, 274, value)), value));
        }
        cases.push((Some(exif_blob(big_endian, 274, 0)), 1));
        cases.push((Some(exif_blob(big_endian, 274, 9)), 1));
        cases.push((Some(exif_blob(big_endian, 271, 6)), 1));
    }
    cases.push((None, 1));
    for (index, (exif, value)) in cases.into_iter().enumerate() {
        let bytes = encode(w, h, 3, SamplingFactor::F_1_1, exif.as_deref());
        let path = write(dir.path(), &format!("{index:02}.jpg"), &bytes);
        let set = open_frame_set(&[path], HostBudget::explicit(BIG)).expect("opens");
        let (uw, uh) = if value >= 5 { (h, w) } else { (w, h) };
        assert_eq!(
            set.reader.dimensions(),
            (uw, uh),
            "case {index}, value {value}"
        );
        let mut expected = vec![0_u16; stored.len()];
        for y in 0..h {
            for x in 0..w {
                let (ux, uy) = upright_position(value, x, y, w, h);
                let to = ((uy * uw + ux) * 3) as usize;
                let from = ((y * w + x) * 3) as usize;
                expected[to..to + 3].copy_from_slice(&stored[from..from + 3]);
            }
        }
        assert_eq!(
            full(set.reader.as_ref(), 0),
            expected,
            "case {index}, value {value}"
        );
        for y in (0..uh).step_by(5) {
            for x in (0..uw).step_by(8) {
                let rect = Rect {
                    x,
                    y,
                    width: 8.min(uw - x),
                    height: 5.min(uh - y),
                };
                let mut tile = vec![0; (rect.width * rect.height * 3) as usize];
                set.reader
                    .read_tile_into(0, rect, &mut tile)
                    .expect("tile reads");
                let region: Vec<u16> = (rect.y..rect.y + rect.height)
                    .flat_map(|row| {
                        let start = ((row * uw + rect.x) * 3) as usize;
                        expected[start..start + (rect.width * 3) as usize].to_vec()
                    })
                    .collect();
                assert_eq!(tile, region, "case {index}, value {value}, tile {x},{y}");
            }
        }
    }
}
