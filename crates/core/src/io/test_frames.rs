// SPDX-License-Identifier: GPL-3.0-or-later

//! Test-only TIFF frames carrying a capture-time tag.
//!
//! No committed fixture frame has one (`fixtures/mantis-512/frames/00.tif`
//! carries twelve tags, none of them 306 or 36867) and [`TiffWriter`]
//! deliberately cannot write one -- its IFD is a fixed twelve-entry set.
//! Without this module every capture-time test would assert against data
//! it has no way to construct, so grouping's whole time signal would be
//! untested.
//!
//! Behind the `test-algorithms` feature rather than `#[cfg(test)]`, per
//! [`crate::test_algorithms`]: `#[cfg(test)]` code never reaches the rlib,
//! so `crates/cli`'s own tests -- which drive the built binary over a
//! directory tree they must first *build* -- could not reach it.
//!
//! The frames are small but genuinely valid: uncompressed 16-bit RGB, one
//! strip, `II`, a real pixel payload, so [`TiffStack::open_files`] opens
//! them and a CLI test can *stack* such a tree rather than only group it.
//!
//! [`TiffWriter`]: crate::io::TiffWriter
//! [`TiffStack::open_files`]: crate::io::TiffStack::open_files

use std::path::Path;

use super::error::TiffError;
use super::tiff::header::{ByteOrder, TAG_GPS_IFD};

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
/// data actually uses: all 61 reference-stack frames carry it in IFD0.
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
    for (tag, _, data) in entries.iter_mut() {
        match *tag {
            TAG_STRIP_OFFSETS => *data = longs(&[pixel_start as u32]),
            TAG_EXIF_IFD if exif_value.is_some() => *data = longs(&[exif_ifd_offset as u32]),
            _ => {}
        }
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

// --- arbitrary metadata (`write_frame_with_metadata`) ---------------------
//
// A hand-rolled serializer, deliberately independent of the production IFD
// writer (`crate::io::tiff::writer::ifd0_with_sub_ifds`): a test asserting
// against the production reader must not also exercise the production
// writer on both sides of the same round trip.

pub fn shorts_bo(order: ByteOrder, values: &[u16]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|v| match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        })
        .collect()
}

pub fn longs_bo(order: ByteOrder, values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|v| match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        })
        .collect()
}

/// One `(numerator, denominator)` pair, the TIFF `RATIONAL`/`SRATIONAL`
/// shape.
pub fn rational_bo(order: ByteOrder, num: u32, den: u32) -> Vec<u8> {
    longs_bo(order, &[num, den])
}

/// NUL-terminated ASCII -- byte-order independent, like every
/// single-byte-per-element TIFF type (`ASCII`, `BYTE`, `UNDEFINED`).
pub fn ascii_entry(text: &str) -> Vec<u8> {
    ascii(text)
}

/// One arbitrary IFD entry: a tag, its TIFF field type, and the value's
/// bytes already encoded in the frame's own [`ByteOrder`].
pub type RawEntry = (u16, u16, Vec<u8>);

/// What [`write_frame_with_metadata`] writes beyond the eleven base image
/// tags every synthetic frame here carries.
#[derive(Default)]
pub struct FrameMetadata {
    pub byte_order: ByteOrderSpec,
    /// Extra IFD0 entries: identity tags, `ICC_PROFILE` (34675), `XMP`
    /// (700), `IPTC` (33723), a sentinel `MakerNote` (37500) or
    /// `DocumentName` (269).
    pub ifd0: Vec<RawEntry>,
    /// EXIF sub-IFD (34665) entries. Empty means no 34665 pointer at all,
    /// matching [`ExifPlan::None`].
    pub exif: Vec<RawEntry>,
    /// GPS sub-IFD (34853) entries. Empty means no 34853 pointer at all.
    pub gps: Vec<RawEntry>,
    /// Writes a second, minimal IFD after IFD0 and points IFD0's next-IFD
    /// offset at it -- the "IFD1 thumbnail" shape a reader must drop.
    pub thumbnail: bool,
}

/// [`ByteOrder`] with a `Default` (`Little`), so [`FrameMetadata`] can
/// derive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteOrderSpec(pub ByteOrder);

impl Default for ByteOrderSpec {
    fn default() -> Self {
        ByteOrderSpec(ByteOrder::Little)
    }
}

/// Write a 16-bit RGB frame at `path` carrying arbitrary IFD0/EXIF/GPS
/// entries per `meta`, for the export-metadata tests: a real EXIF IFD, an
/// ICC profile, XMP, IPTC, a sentinel MakerNote, or a big-endian byte order.
pub fn write_frame_with_metadata(
    path: &Path,
    width: u32,
    height: u32,
    meta: &FrameMetadata,
) -> Result<(), TiffError> {
    let order = meta.byte_order.0;
    let mut ifd0 = base_entries_bo(order, width, height);
    ifd0.extend(meta.ifd0.iter().cloned());
    serialize_with_sub_ifds(
        path,
        order,
        width,
        height,
        ifd0,
        &meta.exif,
        &meta.gps,
        meta.thumbnail,
    )
}

/// [`base_entries`], re-encoded in `order` rather than hardcoded to
/// little-endian.
fn base_entries_bo(order: ByteOrder, width: u32, height: u32) -> Vec<RawEntry> {
    let spp: u32 = 3;
    let row_bytes = width * spp * 2;
    vec![
        (TAG_IMAGE_WIDTH, 4, longs_bo(order, &[width])),
        (TAG_IMAGE_LENGTH, 4, longs_bo(order, &[height])),
        (TAG_BITS_PER_SAMPLE, 3, shorts_bo(order, &[16, 16, 16])),
        (TAG_COMPRESSION, 3, shorts_bo(order, &[1])),
        (TAG_PHOTOMETRIC, 3, shorts_bo(order, &[2])),
        (TAG_STRIP_OFFSETS, 4, longs_bo(order, &[0])),
        (TAG_SAMPLES_PER_PIXEL, 3, shorts_bo(order, &[spp as u16])),
        (TAG_ROWS_PER_STRIP, 4, longs_bo(order, &[height])),
        (
            TAG_STRIP_BYTE_COUNTS,
            4,
            longs_bo(order, &[row_bytes * height]),
        ),
        (TAG_PLANAR_CONFIG, 3, shorts_bo(order, &[1])),
        (TAG_SAMPLE_FORMAT, 3, shorts_bo(order, &[1, 1, 1])),
    ]
}

/// One IFD's worth of entries, laid out at `at`: `(count, 12 bytes/entry,
/// next-ifd u32)`, in tag order, with every value under 5 bytes inline and
/// every larger one appended after the entry table -- `cursor` carries the
/// running write position across every IFD this frame writes, the same
/// single-pass layout [`serialize`] uses for one IFD.
fn layout_ifd(mut entries: Vec<RawEntry>, cursor: &mut u64) -> (u64, Vec<RawEntry>, Vec<u64>) {
    entries.sort_by_key(|(tag, ..)| *tag);
    let at = *cursor;
    let n = entries.len() as u64;
    *cursor += 2 + 12 * n + 4;
    *cursor += *cursor % 2;
    let mut offsets = vec![0u64; entries.len()];
    for (i, (_, _, data)) in entries.iter().enumerate() {
        if data.len() > 4 {
            offsets[i] = *cursor;
            *cursor += data.len() as u64;
            *cursor += *cursor % 2;
        }
    }
    (at, entries, offsets)
}

/// Bytes per element for a TIFF field type, matching the production
/// reader's own `field_type_size`: `RATIONAL`/`SRATIONAL` are 8 (a 4-byte
/// numerator and denominator), `SHORT`/`SSHORT` 2, `BYTE`/`ASCII`/
/// `UNDEFINED` 1, everything else (`LONG`, `SLONG`, `FLOAT`) 4.
fn field_element_size(field_type: u16) -> u32 {
    match field_type {
        1 | 2 | 7 => 1,
        3 | 8 => 2,
        5 | 10 => 8,
        _ => 4,
    }
}

fn write_ifd(
    buf: &mut [u8],
    order: ByteOrder,
    at: u64,
    entries: &[RawEntry],
    offsets: &[u64],
    next_ifd: u64,
) {
    let put16 = |buf: &mut [u8], pos: usize, v: u16| {
        buf[pos..pos + 2].copy_from_slice(&match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        });
    };
    let put32 = |buf: &mut [u8], pos: usize, v: u32| {
        buf[pos..pos + 4].copy_from_slice(&match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        });
    };
    let mut pos = at as usize;
    put16(buf, pos, entries.len() as u16);
    pos += 2;
    for (i, (tag, field_type, data)) in entries.iter().enumerate() {
        let count = data.len() as u32 / field_element_size(*field_type);
        put16(buf, pos, *tag);
        put16(buf, pos + 2, *field_type);
        put32(buf, pos + 4, count);
        if data.len() <= 4 {
            buf[pos + 8..pos + 8 + data.len()].copy_from_slice(data);
        } else {
            let off = offsets[i] as usize;
            put32(buf, pos + 8, off as u32);
            buf[off..off + data.len()].copy_from_slice(data);
        }
        pos += 12;
    }
    put32(buf, pos, next_ifd as u32);
}

/// Header -> IFD0 (with 34665/34853 pointers patched in when `exif`/`gps`
/// are non-empty) -> EXIF sub-IFD -> GPS sub-IFD -> an optional dummy IFD1
/// -> pixels.
#[allow(clippy::too_many_arguments)] // one shape input apiece; a struct wrapper would only rename this list
fn serialize_with_sub_ifds(
    path: &Path,
    order: ByteOrder,
    width: u32,
    height: u32,
    mut ifd0: Vec<RawEntry>,
    exif: &[RawEntry],
    gps: &[RawEntry],
    thumbnail: bool,
) -> Result<(), TiffError> {
    if !exif.is_empty() {
        ifd0.push((TAG_EXIF_IFD, 4, longs_bo(order, &[0])));
    }
    if !gps.is_empty() {
        ifd0.push((TAG_GPS_IFD, 4, longs_bo(order, &[0])));
    }

    let mut cursor: u64 = 8;
    let (ifd0_at, ifd0_entries, ifd0_offsets) = layout_ifd(ifd0, &mut cursor);
    let (exif_at, exif_entries, exif_offsets) = if exif.is_empty() {
        (0, Vec::new(), Vec::new())
    } else {
        layout_ifd(exif.to_vec(), &mut cursor)
    };
    let (gps_at, gps_entries, gps_offsets) = if gps.is_empty() {
        (0, Vec::new(), Vec::new())
    } else {
        layout_ifd(gps.to_vec(), &mut cursor)
    };
    let (thumb_at, thumb_entries, thumb_offsets) = if thumbnail {
        layout_ifd(
            vec![(TAG_IMAGE_DESCRIPTION_THUMB, 2, ascii("thumb"))],
            &mut cursor,
        )
    } else {
        (0, Vec::new(), Vec::new())
    };

    let pixel_start = cursor;
    let row_bytes = u64::from(width) * 3 * 2;
    let pixel_bytes = row_bytes * u64::from(height);

    let mut ifd0_entries = ifd0_entries;
    for (tag, _, data) in ifd0_entries.iter_mut() {
        match *tag {
            TAG_STRIP_OFFSETS => *data = longs_bo(order, &[pixel_start as u32]),
            TAG_EXIF_IFD => *data = longs_bo(order, &[exif_at as u32]),
            TAG_GPS_IFD => *data = longs_bo(order, &[gps_at as u32]),
            _ => {}
        }
    }

    let mut buf = vec![0u8; (pixel_start + pixel_bytes) as usize];
    buf[0..2].copy_from_slice(order.tag().as_bytes());
    let magic: u16 = 42;
    buf[2..4].copy_from_slice(&match order {
        ByteOrder::Little => magic.to_le_bytes(),
        ByteOrder::Big => magic.to_be_bytes(),
    });
    buf[4..8].copy_from_slice(&match order {
        ByteOrder::Little => (ifd0_at as u32).to_le_bytes(),
        ByteOrder::Big => (ifd0_at as u32).to_be_bytes(),
    });

    let ifd0_next = if thumbnail { thumb_at } else { 0 };
    write_ifd(
        &mut buf,
        order,
        ifd0_at,
        &ifd0_entries,
        &ifd0_offsets,
        ifd0_next,
    );
    if !exif_entries.is_empty() {
        write_ifd(&mut buf, order, exif_at, &exif_entries, &exif_offsets, 0);
    }
    if !gps_entries.is_empty() {
        write_ifd(&mut buf, order, gps_at, &gps_entries, &gps_offsets, 0);
    }
    if !thumb_entries.is_empty() {
        write_ifd(&mut buf, order, thumb_at, &thumb_entries, &thumb_offsets, 0);
    }

    for y in 0..height as usize {
        for x in 0..width as usize {
            for s in 0..3usize {
                let value = ((x * 7 + y * 13 + s * 31) % 4096) as u16;
                let at = pixel_start as usize + y * row_bytes as usize + (x * 3 + s) * 2;
                buf[at..at + 2].copy_from_slice(&match order {
                    ByteOrder::Little => value.to_le_bytes(),
                    ByteOrder::Big => value.to_be_bytes(),
                });
            }
        }
    }

    std::fs::write(path, &buf).map_err(|source| TiffError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// A tag no test whitelist carries, used only to give the dummy IFD1
/// [`FrameMetadata::thumbnail`] writes one harmless entry.
const TAG_IMAGE_DESCRIPTION_THUMB: u16 = 270;
