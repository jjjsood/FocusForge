// SPDX-License-Identifier: GPL-3.0-or-later

//! JPEG frames: widening, tiling, the decoded-frame cache, refusals and EXIF
//! orientation. Frames are encoded in memory with standard Huffman tables.

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::io::test_frames::{
    ByteOrderSpec, FrameMetadata, ascii_entry, rational_bo, shorts_bo,
    write_frame_with_capture_time, write_frame_with_exif_capture_time,
    write_frame_with_junk_exif_pointer, write_frame_with_metadata,
};
use focusforge_core::io::{
    ByteOrder, FrameError, FrameReader, GroupSignal, JpegError, JpegStack, ReaderKind, Rect,
    SourceKind, SourceMetadata, capture_time, open_frame_set, propose, read_source_metadata,
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
        assert_eq!(
            set.reader.frame(0).orientation,
            value,
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

/// A JPEG at `dir/name` whose Exif payload is `blob`; `None` writes no APP1.
fn jpeg_with_exif(dir: &Path, name: &str, blob: Option<&[u8]>) -> PathBuf {
    let bytes = encode(8, 4, 3, SamplingFactor::F_1_1, blob);
    write(dir, name, &bytes)
}

/// A JPEG at `dir/name` whose Exif payload is the bytes of `tiff`.
fn jpeg_carrying(dir: &Path, name: &str, tiff: &Path) -> PathBuf {
    let blob = std::fs::read(tiff).expect("tiff bytes");
    jpeg_with_exif(dir, name, Some(&blob))
}

fn rich_metadata(order: ByteOrder) -> FrameMetadata {
    let shorts = |v: u16| shorts_bo(order, &[v]);
    let rational = |n, d| rational_bo(order, n, d);
    FrameMetadata {
        byte_order: ByteOrderSpec(order),
        ifd0: vec![
            (271, 2, ascii_entry("TestCam Manufacturing")),
            (272, 2, ascii_entry("Model One")),
            (274, 3, shorts(6)),
            (315, 2, ascii_entry("A. Photographer")),
            (33432, 2, ascii_entry("(c) Someone")),
        ],
        exif: vec![
            (33434, 5, rational(1, 250)),
            (33437, 5, rational(28, 10)),
            (34855, 3, shorts(400)),
            (36867, 2, ascii_entry("2024:03:17 08:00:00")),
            (42036, 2, ascii_entry("Lens 50mm")),
        ],
        gps: vec![(1, 2, ascii_entry("N"))],
        ..FrameMetadata::default()
    }
}

#[test]
fn jpeg_capture_time() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let ifd0 = dir.path().join("ifd0.tif");
    write_frame_with_capture_time(&ifd0, 8, 4, Some("2026:08:29 18:42:41")).expect("tiff");
    let sub_ifd = dir.path().join("sub.tif");
    write_frame_with_exif_capture_time(&sub_ifd, 8, 4, "2026:08:29 18:42:42").expect("tiff");
    let big_endian = dir.path().join("mm.tif");
    let mut meta = FrameMetadata {
        byte_order: ByteOrderSpec(ByteOrder::Big),
        ..FrameMetadata::default()
    };
    meta.ifd0
        .push((36867, 2, ascii_entry("2026:08:29 18:42:43")));
    write_frame_with_metadata(&big_endian, 8, 4, &meta).expect("tiff");
    for (index, tiff) in [ifd0, sub_ifd, big_endian].iter().enumerate() {
        let jpeg = jpeg_carrying(dir.path(), &format!("{index}.jpg"), tiff);
        let time = capture_time(&jpeg);
        assert!(time.is_some(), "case {index}: JPEG is timed");
        assert_eq!(time, capture_time(tiff), "case {index}");
    }
}

#[test]
fn jpeg_frames_group_by_capture_time() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let scratch = tempfile::tempdir().expect("temporary directory");
    let frames = [
        ("a.jpg", "2026:08:29 10:00:00", ByteOrder::Little),
        ("b.jpg", "2026:08:29 10:00:05", ByteOrder::Big),
        ("c.jpg", "2026:08:29 11:00:00", ByteOrder::Little),
        ("d.jpg", "2026:08:29 11:00:05", ByteOrder::Little),
    ];
    for (name, when, order) in frames {
        let tiff = scratch.path().join("t.tif");
        let mut meta = FrameMetadata {
            byte_order: ByteOrderSpec(order),
            ..FrameMetadata::default()
        };
        meta.ifd0.push((36867, 2, ascii_entry(when)));
        write_frame_with_metadata(&tiff, 8, 4, &meta).expect("tiff");
        jpeg_carrying(dir.path(), name, &tiff);
    }
    let proposal = propose(dir.path()).expect("JPEG frames propose");
    assert!(proposal.time_signal_available);
    let grouped: Vec<Vec<String>> = proposal
        .groups
        .iter()
        .map(|group| {
            group
                .frames
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        })
        .collect();
    assert_eq!(grouped, [["a.jpg", "b.jpg"], ["c.jpg", "d.jpg"]]);
    assert!(
        proposal
            .groups
            .iter()
            .all(|g| g.signal == GroupSignal::CaptureTime)
    );
}

#[test]
fn jpeg_without_exif_is_untimed_and_unwarned() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let bare = jpeg_with_exif(dir.path(), "bare.jpg", None);
    let mut foreign = Vec::new();
    {
        let mut encoder = Encoder::new(&mut foreign, 90);
        encoder
            .add_app_segment(1, b"http://ns.adobe.com/xap/1.0/\0<x/>".to_vec())
            .expect("APP1 fits");
        encoder
            .encode(&pixels(8, 4, 3), 8, 4, ColorType::Rgb)
            .expect("frame encodes");
    }
    let foreign = write(dir.path(), "foreign.jpg", &foreign);
    for path in [&bare, &foreign] {
        assert_eq!(capture_time(path), None, "{}", path.display());
        assert_eq!(read_source_metadata(path), SourceMetadata::default());
    }
    let proposal = propose(dir.path()).expect("untimed JPEG frames propose");
    assert!(!proposal.time_signal_available);
}

#[test]
fn jpeg_damaged_exif_is_untimed_and_warned() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let scratch = tempfile::tempdir().expect("temporary directory");
    let read = |name: &str| std::fs::read(scratch.path().join(name)).expect("tiff bytes");

    write_frame_with_capture_time(
        &scratch.path().join("dated.tif"),
        8,
        4,
        Some("2026:08:29 18:42:41"),
    )
    .expect("tiff");
    let cut_in_ifd0 = read("dated.tif")[..24].to_vec();

    write_frame_with_junk_exif_pointer(&scratch.path().join("junk.tif"), 8, 4).expect("tiff");
    let junk_pointer = read("junk.tif");

    let mut meta = FrameMetadata::default();
    meta.ifd0.push((271, 2, ascii_entry(&"M".repeat(40))));
    write_frame_with_metadata(&scratch.path().join("long.tif"), 8, 4, &meta).expect("tiff");
    let long = read("long.tif");
    let ifd0_offset = u32::from_le_bytes(long[4..8].try_into().unwrap()) as usize;
    let entries = 11 + 1;
    let value_past_end = long[..ifd0_offset + 2 + 12 * entries + 4].to_vec();

    let no_mark = b"XX\x2a\0\x08\0\0\0\0\0\0\0\0\0".to_vec();

    for (index, blob) in [cut_in_ifd0, junk_pointer, value_past_end, no_mark]
        .iter()
        .enumerate()
    {
        let jpeg = jpeg_with_exif(dir.path(), &format!("{index}.jpg"), Some(blob));
        assert_eq!(capture_time(&jpeg), None, "case {index}");
        let metadata = read_source_metadata(&jpeg);
        let shown = jpeg.display().to_string();
        assert!(
            metadata.warnings.iter().any(|w| w.contains(&shown)),
            "case {index}: warnings {:?}",
            metadata.warnings
        );
    }
    propose(dir.path()).expect("damaged EXIF never fails grouping");

    let bad_date = dir.path().join("bad-date");
    std::fs::create_dir(&bad_date).unwrap();
    let tiff = scratch.path().join("bad.tif");
    write_frame_with_capture_time(&tiff, 8, 4, Some("not-a-date")).expect("tiff");
    let jpeg = jpeg_carrying(&bad_date, "bad.jpg", &tiff);
    assert_eq!(capture_time(&jpeg), None);
    let metadata = read_source_metadata(&jpeg);
    assert!(metadata.warnings.is_empty(), "{:?}", metadata.warnings);
    assert!(metadata.exif.iter().any(|e| e.tag == 36867));
}

#[test]
fn jpeg_source_metadata_matches_tiff_reader() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for (name, order) in [("ii", ByteOrder::Little), ("mm", ByteOrder::Big)] {
        let tiff = dir.path().join(format!("{name}.tif"));
        write_frame_with_metadata(&tiff, 8, 4, &rich_metadata(order)).expect("tiff");
        let jpeg = jpeg_carrying(dir.path(), &format!("{name}.jpg"), &tiff);
        let from_tiff = read_source_metadata(&tiff);
        let from_jpeg = read_source_metadata(&jpeg);
        assert!(
            !from_tiff.ifd0.is_empty() && !from_tiff.exif.is_empty() && !from_tiff.gps.is_empty()
        );
        assert_eq!(from_jpeg.ifd0, from_tiff.ifd0, "{name}");
        assert_eq!(from_jpeg.exif, from_tiff.exif, "{name}");
        assert_eq!(from_jpeg.gps, from_tiff.gps, "{name}");
        assert_eq!(from_jpeg.kind, SourceKind::Tiff);
        assert!(from_jpeg.icc.is_none() && from_jpeg.xmp.is_none() && from_jpeg.iptc.is_none());
        assert!(from_jpeg.warnings.is_empty(), "{:?}", from_jpeg.warnings);
    }
}
