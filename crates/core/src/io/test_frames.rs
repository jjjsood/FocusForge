// SPDX-License-Identifier: GPL-3.0-or-later

//! Test-only TIFF frames carrying a capture-time tag.
//!
//! No committed fixture frame has one (`fixtures/mantis-512/frames/00.tif`
//! carries twelve tags, none of them 306 or 36867) and [`TiffWriter`]
//! deliberately cannot write one -- its IFD is a fixed twelve-entry set
//! (`crates/core/src/io/tiff/writer.rs`). Without this module every
//! capture-time test would assert against data it has no way to construct,
//! so grouping's whole time signal would be untested.
//!
//! Behind the `test-algorithms` feature rather than `#[cfg(test)]` for the
//! reason [`crate::test_algorithms`] states at length: `#[cfg(test)]` code
//! never reaches the rlib, so `crates/cli`'s own tests -- which drive the
//! built binary over a directory tree they must first *build* -- could not
//! reach it.
//!
//! The frames are small but genuinely valid: uncompressed 16-bit RGB, one
//! strip, `II`, a real pixel payload, so [`TiffStack::open_files`] opens
//! them and a CLI test can *stack* such a tree rather than only group it.
//!
//! [`TiffWriter`]: crate::io::TiffWriter
//! [`TiffStack::open_files`]: crate::io::TiffStack::open_files

use std::path::Path;

use super::error::TiffError;

const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_PLANAR_CONFIG: u16 = 284;
const TAG_SAMPLE_FORMAT: u16 = 339;
const TAG_EXIF_IFD: u16 = 34665;
const TAG_DATE_TIME_ORIGINAL: u16 = 36867;

/// What, if anything, the frame's EXIF sub-IFD pointer (34665) holds.
enum ExifPlan<'a> {
    /// No 34665 entry at all.
    None,
    /// A real sub-IFD holding `DateTimeOriginal`.
    DateTime(&'a str),
    /// A 34665 entry pointing far past the end of the file: the case a
    /// reader must survive, since a frame with a junk EXIF pointer is still
    /// a perfectly good stack member.
    Junk,
}

/// Write a valid 16-bit RGB frame at `path`, carrying `DateTimeOriginal`
/// (36867) in **IFD0** when `when` is `Some` -- the placement the reference
/// data actually uses: verified on 2026-09-15, all 61 reference frames
/// carry it in IFD0.
///
/// `when` is written verbatim, so a test can pass a malformed string
/// (`"not-a-date"`) and exercise the unparseable path.
pub fn write_frame_with_capture_time(
    path: &Path,
    width: u32,
    height: u32,
    when: Option<&str>,
) -> Result<(), TiffError> {
    write_frame(path, width, height, when, ExifPlan::None)
}

/// Write a frame whose `DateTimeOriginal` lives **only** behind the EXIF
/// sub-IFD pointer (34665), with IFD0 carrying no 36867 of its own: the
/// fallback path, for exporters that do not duplicate the tag into IFD0.
pub fn write_frame_with_exif_capture_time(
    path: &Path,
    width: u32,
    height: u32,
    when: &str,
) -> Result<(), TiffError> {
    write_frame(path, width, height, None, ExifPlan::DateTime(when))
}

/// Write a frame with no 36867 anywhere and a 34665 pointer aimed past the
/// end of the file. Reading it must yield no capture time and **no error**.
pub fn write_frame_with_junk_exif_pointer(
    path: &Path,
    width: u32,
    height: u32,
) -> Result<(), TiffError> {
    write_frame(path, width, height, None, ExifPlan::Junk)
}

/// Write a frame carrying `DateTime` (306) and nothing else time-shaped --
/// darktable's *export* time on the reference data, which grouping must
/// never consult.
pub fn write_frame_with_only_tag_306(
    path: &Path,
    width: u32,
    height: u32,
    when: &str,
) -> Result<(), TiffError> {
    let mut entries = base_entries(width, height);
    entries.push((306, 2, ascii(when)));
    finish(path, width, height, entries)
}

fn write_frame(
    path: &Path,
    width: u32,
    height: u32,
    ifd0_when: Option<&str>,
    exif: ExifPlan<'_>,
) -> Result<(), TiffError> {
    let mut entries = base_entries(width, height);
    if let Some(when) = ifd0_when {
        entries.push((TAG_DATE_TIME_ORIGINAL, 2, ascii(when)));
    }
    match exif {
        ExifPlan::None => finish(path, width, height, entries),
        ExifPlan::Junk => {
            entries.push((TAG_EXIF_IFD, 4, longs(&[0xFFFF_0000])));
            finish(path, width, height, entries)
        }
        ExifPlan::DateTime(when) => {
            entries.push((TAG_EXIF_IFD, 4, longs(&[0])));
            finish_with_exif(path, width, height, entries, when)
        }
    }
}

/// The eleven tags every frame here carries, in tag order.
fn base_entries(width: u32, height: u32) -> Vec<(u16, u16, Vec<u8>)> {
    let spp: u32 = 3;
    let row_bytes = width * spp * 2;
    vec![
        (TAG_IMAGE_WIDTH, 4, longs(&[width])),
        (TAG_IMAGE_LENGTH, 4, longs(&[height])),
        (TAG_BITS_PER_SAMPLE, 3, shorts(&[16, 16, 16])),
        (TAG_COMPRESSION, 3, shorts(&[1])),
        (TAG_PHOTOMETRIC, 3, shorts(&[2])),
        (TAG_STRIP_OFFSETS, 4, longs(&[0])),
        (TAG_SAMPLES_PER_PIXEL, 3, shorts(&[spp as u16])),
        (TAG_ROWS_PER_STRIP, 4, longs(&[height])),
        (TAG_STRIP_BYTE_COUNTS, 4, longs(&[row_bytes * height])),
        (TAG_PLANAR_CONFIG, 3, shorts(&[1])),
        (TAG_SAMPLE_FORMAT, 3, shorts(&[1, 1, 1])),
    ]
}

fn finish(
    path: &Path,
    width: u32,
    height: u32,
    entries: Vec<(u16, u16, Vec<u8>)>,
) -> Result<(), TiffError> {
    serialize(path, width, height, entries, None)
}

fn finish_with_exif(
    path: &Path,
    width: u32,
    height: u32,
    entries: Vec<(u16, u16, Vec<u8>)>,
    when: &str,
) -> Result<(), TiffError> {
    serialize(path, width, height, entries, Some(when))
}

/// Header -> IFD0 -> out-of-line values -> (optional EXIF sub-IFD and its
/// own value) -> pixels. Every offset is computed in one pass and patched
/// length-preservingly afterwards, the same shape `TiffWriter::new` uses.
fn serialize(
    path: &Path,
    width: u32,
    height: u32,
    mut entries: Vec<(u16, u16, Vec<u8>)>,
    exif_when: Option<&str>,
) -> Result<(), TiffError> {
    entries.sort_by_key(|(tag, ..)| *tag);
    let n = entries.len() as u32;

    let mut cursor: u64 = 8 + 2 + 12 * u64::from(n) + 4;
    cursor += cursor % 2;
    let mut offsets = vec![0u64; entries.len()];
    for (i, (_, _, data)) in entries.iter().enumerate() {
        if data.len() > 4 {
            offsets[i] = cursor;
            cursor += data.len() as u64;
            cursor += cursor % 2;
        }
    }

    // The EXIF sub-IFD, when there is one: one entry (36867), its ASCII
    // value immediately after the block.
    let exif_value = exif_when.map(ascii);
    let exif_ifd_offset = cursor;
    let mut exif_value_offset = 0u64;
    if let Some(value) = &exif_value {
        cursor += 2 + 12 + 4;
        cursor += cursor % 2;
        exif_value_offset = cursor;
        cursor += value.len() as u64;
        cursor += cursor % 2;
    }

    let pixel_start = cursor;
    let row_bytes = u64::from(width) * 3 * 2;
    let pixel_bytes = row_bytes * u64::from(height);

    // Patch StripOffsets and, when present, the 34665 pointer -- both are
    // single LONGs, so the patch cannot move anything.
    for (i, (tag, _, data)) in entries.iter_mut().enumerate() {
        match *tag {
            TAG_STRIP_OFFSETS => *data = longs(&[pixel_start as u32]),
            TAG_EXIF_IFD if exif_value.is_some() => *data = longs(&[exif_ifd_offset as u32]),
            _ => {}
        }
        let _ = i;
    }

    let mut buf = vec![0u8; (pixel_start + pixel_bytes) as usize];
    buf[0..2].copy_from_slice(b"II");
    buf[2..4].copy_from_slice(&42u16.to_le_bytes());
    buf[4..8].copy_from_slice(&8u32.to_le_bytes());
    buf[8..10].copy_from_slice(&(n as u16).to_le_bytes());
    let mut pos = 10usize;
    for (i, (tag, field_type, data)) in entries.iter().enumerate() {
        let count = match *field_type {
            2 => data.len() as u32,
            3 => (data.len() / 2) as u32,
            _ => (data.len() / 4) as u32,
        };
        buf[pos..pos + 2].copy_from_slice(&tag.to_le_bytes());
        buf[pos + 2..pos + 4].copy_from_slice(&field_type.to_le_bytes());
        buf[pos + 4..pos + 8].copy_from_slice(&count.to_le_bytes());
        if data.len() <= 4 {
            buf[pos + 8..pos + 8 + data.len()].copy_from_slice(data);
        } else {
            let off = offsets[i] as usize;
            buf[pos + 8..pos + 12].copy_from_slice(&(off as u32).to_le_bytes());
            buf[off..off + data.len()].copy_from_slice(data);
        }
        pos += 12;
    }
    buf[pos..pos + 4].copy_from_slice(&0u32.to_le_bytes());

    if let Some(value) = &exif_value {
        let base = exif_ifd_offset as usize;
        buf[base..base + 2].copy_from_slice(&1u16.to_le_bytes());
        let e = base + 2;
        buf[e..e + 2].copy_from_slice(&TAG_DATE_TIME_ORIGINAL.to_le_bytes());
        buf[e + 2..e + 4].copy_from_slice(&2u16.to_le_bytes());
        buf[e + 4..e + 8].copy_from_slice(&(value.len() as u32).to_le_bytes());
        buf[e + 8..e + 12].copy_from_slice(&(exif_value_offset as u32).to_le_bytes());
        buf[e + 12..e + 16].copy_from_slice(&0u32.to_le_bytes());
        let v = exif_value_offset as usize;
        buf[v..v + value.len()].copy_from_slice(value);
    }

    // A deterministic, non-constant payload: the frame is a real image, so
    // a test that stacks such a tree gets a real composite out.
    for y in 0..height as usize {
        for x in 0..width as usize {
            for s in 0..3usize {
                let value = ((x * 7 + y * 13 + s * 31) % 4096) as u16;
                let at = pixel_start as usize + y * row_bytes as usize + (x * 3 + s) * 2;
                buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
    }

    std::fs::write(path, &buf).map_err(|source| TiffError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn shorts(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn longs(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// NUL-terminated ASCII, the form every TIFF `field_type = 2` value takes.
fn ascii(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}
