// SPDX-License-Identifier: GPL-3.0-or-later

//! TIFF frames read upright by their Orientation tag, in every layout the
//! decoded reader accepts. Tagged frames are copies of untagged ones with a
//! rewritten IFD0; the expected pixels come from a per-value formula applied
//! to the untagged read.

mod common;

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use focusforge_core::algorithm::read_tile_step_into;
use focusforge_core::io::{
    FrameError, FrameReader, ReaderKind, Rect, StripCompression, StripSpec, StripWriter, TiffError,
    TiffHeader, frame_paths, open_frame_set,
};
use focusforge_core::tile::HostBudget;

const BIG: u64 = 1 << 30;

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

/// `bytes` with IFD0 rewritten to hold Orientation `value` of `field_type`
/// (3 SHORT or 4 LONG), appended at the end of the file; every out-of-line
/// value offset of the original stays valid.
fn with_orientation(bytes: &[u8], value: u32, field_type: u16) -> Vec<u8> {
    let big = &bytes[..2] == b"MM";
    let p16 = |v: u16| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let p32 = |v: u32| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let at16 = |at: usize| {
        let pair = [bytes[at], bytes[at + 1]];
        if big {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    };
    let ifd = {
        let quad = [bytes[4], bytes[5], bytes[6], bytes[7]];
        (if big {
            u32::from_be_bytes(quad)
        } else {
            u32::from_le_bytes(quad)
        }) as usize
    };
    let count = usize::from(at16(ifd));
    let mut entries: Vec<[u8; 12]> = (0..count)
        .map(|i| {
            bytes[ifd + 2 + i * 12..ifd + 14 + i * 12]
                .try_into()
                .expect("12-byte entry")
        })
        .filter(|entry| at16_of(entry, big) != 274)
        .collect();
    let mut entry = [0_u8; 12];
    entry[0..2].copy_from_slice(&p16(274));
    entry[2..4].copy_from_slice(&p16(field_type));
    entry[4..8].copy_from_slice(&p32(1));
    if field_type == 3 {
        entry[8..10].copy_from_slice(&p16(value as u16));
    } else {
        entry[8..12].copy_from_slice(&p32(value));
    }
    entries.push(entry);
    entries.sort_by_key(|entry| at16_of(entry, big));

    let mut out = bytes.to_vec();
    if out.len() % 2 == 1 {
        out.push(0);
    }
    let new_ifd = out.len() as u32;
    out.extend(p16(entries.len() as u16));
    for entry in &entries {
        out.extend(entry);
    }
    out.extend(p32(0));
    out[4..8].copy_from_slice(&p32(new_ifd));
    out
}

fn at16_of(entry: &[u8; 12], big: bool) -> u16 {
    if big {
        u16::from_be_bytes([entry[0], entry[1]])
    } else {
        u16::from_le_bytes([entry[0], entry[1]])
    }
}

fn strip_frame(
    dir: &Path,
    compression: StripCompression,
    bits: u16,
    spp: u32,
) -> (String, Vec<u8>) {
    let (w, h) = (37_u32, 23_u32);
    let name = format!("strip-{}-{bits}bit-{spp}ch", compression_name(compression));
    let path = dir.join(format!("{name}.tif"));
    let mut writer = StripWriter::create(
        &path,
        StripSpec {
            width: w,
            height: h,
            samples_per_pixel: spp,
            bits_per_sample: bits,
            compression,
            description: None,
            metadata: Default::default(),
        },
    )
    .expect("writer");
    let mut rows = Vec::new();
    for y in 0..h {
        for x in 0..w {
            for c in 0..spp {
                let value = x * 1291 + y * 977 + c * 4001 + x * y * 7;
                if bits == 8 {
                    rows.push((value % 251) as u8);
                } else {
                    rows.extend((value as u16).to_le_bytes());
                }
            }
        }
    }
    writer.write_rows(&rows).expect("rows");
    writer.finish().expect("finish");
    let bytes = std::fs::read(&path).expect("written frame");
    (name, bytes)
}

fn compression_name(compression: StripCompression) -> &'static str {
    match compression {
        StripCompression::None => "none",
        StripCompression::Lzw => "lzw",
        StripCompression::Deflate => "deflate",
    }
}

/// Untagged frames: non-square writer output in every compression, depth and
/// channel count, plus square fixtures with strips, tiles, `MM` and 32946.
fn untagged_frames(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut frames = Vec::new();
    for compression in [
        StripCompression::None,
        StripCompression::Lzw,
        StripCompression::Deflate,
    ] {
        for bits in [8, 16] {
            for spp in [1, 3] {
                frames.push(strip_frame(dir, compression, bits, spp));
            }
        }
    }
    for set in [
        "mantis-64",
        "mantis-64-lzw-p2-tile-mm",
        "mantis-64-old-deflate-p1-strip-mm",
        "mantis-64-deflate-p1-tile-ii",
        "mantis8-64-lzw-p1-tile-mm",
    ] {
        let path = common::fixtures_dir()
            .join(set)
            .join("frames")
            .join("00.tif");
        frames.push((set.to_string(), std::fs::read(path).expect("fixture frame")));
    }
    frames
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("frame file");
    path
}

fn rect_of(reader: &dyn FrameReader) -> Rect {
    let (width, height) = reader.dimensions();
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn read(reader: &dyn FrameReader, rect: Rect) -> Vec<u16> {
    let spp = reader.frame(0).samples_per_pixel as usize;
    let mut out = vec![0; rect.width as usize * rect.height as usize * spp];
    reader
        .read_tile_into(0, rect, &mut out)
        .expect("tile reads");
    out
}

/// `stored` (row-major, `spp` interleaved) displayed under `value`.
fn expected_upright(stored: &[u16], value: u16, w: u32, h: u32, spp: usize) -> Vec<u16> {
    let (uw, _) = if value >= 5 { (h, w) } else { (w, h) };
    let mut out = vec![0; stored.len()];
    for y in 0..h {
        for x in 0..w {
            let (ux, uy) = upright_position(value, x, y, w, h);
            let to = (uy * uw + ux) as usize * spp;
            let from = (y * w + x) as usize * spp;
            out[to..to + spp].copy_from_slice(&stored[from..from + spp]);
        }
    }
    out
}

#[test]
fn orientation_is_applied() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for (name, bytes) in untagged_frames(dir.path()) {
        let plain = write(dir.path(), &format!("{name}-plain.tif"), &bytes);
        let source = open_frame_set(&[plain], HostBudget::explicit(BIG)).expect("untagged opens");
        let (w, h) = source.reader.dimensions();
        let spp = source.reader.frame(0).samples_per_pixel as usize;
        let stored = read(source.reader.as_ref(), rect_of(source.reader.as_ref()));
        for value in 1..=8_u16 {
            let tagged = write(
                dir.path(),
                &format!("{name}-{value}.tif"),
                &with_orientation(&bytes, u32::from(value), 3),
            );
            let set = open_frame_set(&[tagged], HostBudget::explicit(BIG)).expect("opens");
            let (uw, uh) = if value >= 5 { (h, w) } else { (w, h) };
            assert_eq!(set.reader.dimensions(), (uw, uh), "{name}, value {value}");
            let desc = set.reader.frame(0);
            assert_eq!((desc.width, desc.height), (uw, uh), "{name}, value {value}");
            assert_eq!(desc.orientation, value, "{name}, value {value}");
            assert_eq!(
                read(set.reader.as_ref(), rect_of(set.reader.as_ref())),
                expected_upright(&stored, value, w, h, spp),
                "{name}, value {value}"
            );
        }
    }
}

#[test]
fn oriented_tiles_match_upright_frame() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let step = NonZeroU32::new(3).expect("nonzero");
    for (name, bytes) in untagged_frames(dir.path()) {
        let plain = write(dir.path(), &format!("{name}-plain.tif"), &bytes);
        let source = open_frame_set(&[plain], HostBudget::explicit(BIG)).expect("untagged opens");
        let (w, h) = source.reader.dimensions();
        let stored = read(source.reader.as_ref(), rect_of(source.reader.as_ref()));
        for value in 1..=8_u16 {
            let tagged = write(
                dir.path(),
                &format!("{name}-{value}.tif"),
                &with_orientation(&bytes, u32::from(value), 3),
            );
            let set = open_frame_set(&[tagged], HostBudget::explicit(BIG)).expect("opens");
            let reader = set.reader.as_ref();
            let (uw, uh) = reader.dimensions();
            let spp = reader.frame(0).samples_per_pixel as usize;
            let whole = expected_upright(&stored, value, w, h, spp);
            let region = |rect: Rect| -> Vec<u16> {
                (rect.y..rect.y + rect.height)
                    .flat_map(|row| {
                        let start = (row * uw + rect.x) as usize * spp;
                        whole[start..start + rect.width as usize * spp].to_vec()
                    })
                    .collect()
            };
            for y in (0..uh).step_by(13) {
                for x in (0..uw).step_by(17) {
                    let rect = Rect {
                        x,
                        y,
                        width: 17.min(uw - x),
                        height: 13.min(uh - y),
                    };
                    assert_eq!(
                        read(reader, rect),
                        region(rect),
                        "{name}, value {value}, tile {x},{y}"
                    );
                }
            }
            let rect = Rect {
                x: 1,
                y: 2,
                width: uw - 2,
                height: uh - 3,
            };
            let (out_w, out_h) = (rect.width.div_ceil(3), rect.height.div_ceil(3));
            let mut strided = vec![0; (out_w * out_h) as usize * spp];
            read_tile_step_into(reader, 0, rect, step, spp as u32, &mut strided)
                .expect("strided read");
            let mut expected = Vec::new();
            for j in 0..out_h {
                for i in 0..out_w {
                    let at = ((rect.y + j * 3) * uw + rect.x + i * 3) as usize * spp;
                    expected.extend_from_slice(&whole[at..at + spp]);
                }
            }
            assert_eq!(strided, expected, "{name}, value {value}, step 3");
        }
    }
}

fn header_orientation(path: &Path) -> u16 {
    TiffHeader::open(path).expect("header").orientation
}

#[test]
fn orientation_edge_cases() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let frames = frame_paths(&common::fixtures_dir().join("mantis-64")).expect("fixture frames");
    let bytes = std::fs::read(&frames[0]).expect("fixture frame");
    assert_eq!(header_orientation(&frames[0]), 1);
    let plain = open_frame_set(&[frames[0].clone()], HostBudget::explicit(BIG)).expect("opens");
    assert_eq!(plain.kind, ReaderKind::Mmap);
    let plain_samples = read(plain.reader.as_ref(), rect_of(plain.reader.as_ref()));

    // Tags that do not name an orientation read as stored and keep the mmap reader.
    let ignored: [(&str, Vec<u8>); 4] = [
        ("absent", bytes.clone()),
        ("zero", with_orientation(&bytes, 0, 3)),
        ("nine", with_orientation(&bytes, 9, 3)),
        ("long", with_orientation(&bytes, 6, 4)),
    ];
    for (name, tagged) in &ignored {
        let path = write(dir.path(), &format!("{name}.tif"), tagged);
        assert_eq!(header_orientation(&path), 1, "{name}");
        let set = open_frame_set(&[path], HostBudget::explicit(BIG)).expect("opens");
        assert_eq!(set.kind, ReaderKind::Mmap, "{name}");
        assert_eq!(set.reader.frame(0).orientation, 1, "{name}");
        assert_eq!(
            read(set.reader.as_ref(), rect_of(set.reader.as_ref())),
            plain_samples,
            "{name}"
        );
    }

    // Each frame of a set is normalized by its own tag.
    let values = [1_u16, 3, 6];
    let paths: Vec<PathBuf> = values
        .iter()
        .zip(&frames)
        .map(|(value, frame)| {
            let plain = std::fs::read(frame).expect("fixture frame");
            write(
                dir.path(),
                &format!("set-{value}.tif"),
                &with_orientation(&plain, u32::from(*value), 3),
            )
        })
        .collect();
    let set = open_frame_set(&paths, HostBudget::explicit(BIG)).expect("mixed tags open");
    assert_eq!(set.kind, ReaderKind::Decoded);
    let (uw, uh) = set.reader.dimensions();
    assert_eq!((uw, uh), (plain.reader.dimensions()));
    for (index, value) in values.into_iter().enumerate() {
        let source = open_frame_set(&[frames[index].clone()], HostBudget::explicit(BIG))
            .expect("untagged opens");
        let (w, h) = source.reader.dimensions();
        let spp = source.reader.frame(0).samples_per_pixel as usize;
        let stored = read(source.reader.as_ref(), rect_of(source.reader.as_ref()));
        let mut got = vec![0; stored.len()];
        set.reader
            .read_tile_into(index, rect_of(set.reader.as_ref()), &mut got)
            .expect("frame reads");
        assert_eq!(
            got,
            expected_upright(&stored, value, w, h, spp),
            "frame {index}"
        );
    }

    // Differing upright geometry is refused, naming the second frame.
    let (_, rect_frame) = strip_frame(dir.path(), StripCompression::None, 16, 1);
    let first = write(
        dir.path(),
        "wide-1.tif",
        &with_orientation(&rect_frame, 1, 3),
    );
    let second = write(
        dir.path(),
        "wide-6.tif",
        &with_orientation(&rect_frame, 6, 3),
    );
    match open_frame_set(&[first, second.clone()], HostBudget::explicit(BIG)) {
        Ok(_) => panic!("frames of different upright size must refuse"),
        Err(FrameError::Tiff(TiffError::FrameMismatch { path, detail, .. })) => {
            assert_eq!(path, second);
            assert!(detail.contains("23x37 vs first frame's 37x23"), "{detail}");
        }
        Err(other) => panic!("unexpected error: {other}"),
    }

    // Bounds are the upright frame's: 30 columns fit the stored 37, not 23.
    let set = open_frame_set(&[second], HostBudget::explicit(BIG)).expect("opens");
    let mut out = vec![0; 30 * 5];
    let error = set
        .reader
        .read_tile_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width: 30,
                height: 5,
            },
            &mut out,
        )
        .expect_err("outside the upright frame");
    assert!(error.to_string().contains("(23, 37)"), "{error}");
}
