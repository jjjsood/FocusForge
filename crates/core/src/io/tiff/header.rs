// SPDX-License-Identifier: GPL-3.0-or-later

//! Classic-TIFF header parser, mirroring `tools/lib/rawtiff.py` tag for tag.
//! Reads through a private
//! [`ByteSource`] trait so `TiffStack::open` can parse a header out of a
//! plain `File` — a few tens of KiB at most — while a unit test parses a
//! `Vec<u8>` it built in memory, with the exact same code either way.
//!
//! A header is *not* a fixed-size prefix read: a reference-stack frame's
//! strip tables are 5464 entries (~33 KiB) at an arbitrary offset, so the
//! parser seeks to wherever the IFD and its out-of-line values actually live
//! rather than reading the first N bytes and hoping.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::super::error::{Refusal, TiffError};
use super::super::metadata::{SourceKind, SourceMetadata, TagEntry};
use crate::io::FrameDesc;

// --- tags ---------------------------------------------------------------

const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_IMAGE_DESCRIPTION: u16 = 270;
pub(crate) const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_PLANAR_CONFIG: u16 = 284;
const TAG_PREDICTOR: u16 = 317;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_SAMPLE_FORMAT: u16 = 339;
/// EXIF sub-IFD pointer: a `LONG` offset to a second IFD.
pub(crate) const TAG_EXIF_IFD: u16 = 34665;
/// EXIF `DateTimeOriginal` -- **capture** time, the tag grouping reads.
/// Never `DateTime` (306), which on the reference data is darktable's
/// *export* time, thirteen days after the shot.
const TAG_DATE_TIME_ORIGINAL: u16 = 36867;
/// GPS sub-IFD pointer: a `LONG` offset to a second IFD.
pub(crate) const TAG_GPS_IFD: u16 = 34853;
pub(crate) const TAG_ICC_PROFILE: u16 = 34675;
pub(crate) const TAG_XMP: u16 = 700;
pub(crate) const TAG_IPTC: u16 = 33723;
/// EXIF's Interoperability IFD pointer, dropped: it names another sub-IFD
/// this reader never follows.
const TAG_INTEROP_IFD: u16 = 40965;
/// The MakerNote, opaque and camera-specific; dropped rather than carried
/// into an export it was never written to describe.
const TAG_MAKER_NOTE: u16 = 37500;

/// IFD0 tags an export carries: identity (Make, Model, Artist, Copyright),
/// orientation and resolution, and the two tags the export rewrite later
/// overwrites (Software, DateTime). Excludes `DocumentName` (269), which
/// commonly holds the exporting tool's absolute file path, and every
/// strip/tile/compression/pointer tag.
const METADATA_IFD0_WHITELIST: &[u16] = &[271, 272, 274, 282, 283, 296, 305, 306, 315, 33432];

/// TIFF field type -> byte size of one value.
fn field_type_size(field_type: u16) -> Option<u32> {
    match field_type {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// Which end the multi-byte integers in a TIFF file are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    /// The two-letter TIFF tag for this byte order.
    pub fn tag(self) -> &'static str {
        match self {
            ByteOrder::Little => "II",
            ByteOrder::Big => "MM",
        }
    }

    /// Decode one `u16` stored in this byte order.
    #[inline(always)]
    pub(crate) fn u16_from(self, bytes: [u8; 2]) -> u16 {
        match self {
            ByteOrder::Little => u16::from_le_bytes(bytes),
            ByteOrder::Big => u16::from_be_bytes(bytes),
        }
    }

    fn u16_at(self, bytes: &[u8]) -> Option<u16> {
        let arr: [u8; 2] = bytes.get(0..2)?.try_into().ok()?;
        Some(self.u16_from(arr))
    }

    fn u32_at(self, bytes: &[u8]) -> Option<u32> {
        let arr: [u8; 4] = bytes.get(0..4)?.try_into().ok()?;
        Some(match self {
            ByteOrder::Little => u32::from_le_bytes(arr),
            ByteOrder::Big => u32::from_be_bytes(arr),
        })
    }

    /// Read `count` values of `field_type` out of `bytes`, widened to `u64`.
    /// Used for every integer tag: `SHORT` and `LONG` both occur in practice
    /// (the reference-stack frames use `SHORT` for
    /// `StripOffsets`/`StripByteCounts`, the fixtures use `LONG`), so this is
    /// generic over both rather than assuming one.
    fn ints_at(self, field_type: u16, count: u32, bytes: &[u8]) -> Option<Vec<u64>> {
        let size = field_type_size(field_type)? as usize;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let chunk = bytes.get(i * size..i * size + size)?;
            let value = match field_type {
                1 | 6 | 7 => u64::from(*chunk.first()?),
                3 | 8 => u64::from(self.u16_at(chunk)?),
                4 | 9 => u64::from(self.u32_at(chunk)?),
                _ => return None,
            };
            out.push(value);
        }
        Some(out)
    }
}

impl From<&TiffHeader> for FrameDesc {
    fn from(header: &TiffHeader) -> Self {
        let layout_signature = vec![
            header.byte_order.tag().to_string(),
            format!("rps={}", header.rows_per_strip),
            format!("compression={}", header.compression),
            format!("{}x{}", header.width, header.height),
            format!("bits={:?}", header.bits_per_sample),
            format!("samples={}", header.samples_per_pixel),
            format!("photometric={}", header.photometric),
            format!("planar={}", header.planar_config),
        ];
        let layout_label = format!("{}/{}", layout_signature[0], layout_signature[1]);
        FrameDesc {
            path: header.path.clone(),
            file_size: header.file_size,
            image_description: header.image_description.clone(),
            width: header.width,
            height: header.height,
            samples_per_pixel: header.samples_per_pixel,
            strip_offsets: header.strip_offsets.clone(),
            strip_byte_counts: header.strip_byte_counts.clone(),
            raw_identity: None,
            layout_label,
            layout_signature,
        }
    }
}

/// One IFD entry, unresolved: its value is either the four inline payload
/// bytes or an offset to them.
struct RawEntry {
    tag: u16,
    field_type: u16,
    count: u32,
    payload: [u8; 4],
}

/// Read the entry table of the IFD at `offset`. Extracted so the EXIF
/// sub-IFD behind tag 34665 is read by exactly the same code as IFD0 --
/// a second, hand-rolled walk for the fallback is how the two drift apart.
fn read_ifd<S: ByteSource>(
    source: &S,
    byte_order: ByteOrder,
    offset: u64,
    path: &Path,
) -> Result<Vec<RawEntry>, TiffError> {
    let count_bytes = source
        .read_at(offset, 2)
        .map_err(|_| TiffError::TruncatedIfd {
            path: path.to_path_buf(),
            entries: 0,
        })?;
    let n_entries = byte_order.u16_at(&count_bytes).unwrap_or(0);

    let ifd_bytes = source
        .read_at(offset + 2, 12 * usize::from(n_entries))
        .map_err(|_| TiffError::TruncatedIfd {
            path: path.to_path_buf(),
            entries: n_entries,
        })?;

    let mut entries: Vec<RawEntry> = Vec::with_capacity(usize::from(n_entries));
    for i in 0..usize::from(n_entries) {
        let base = i * 12;
        let chunk = &ifd_bytes[base..base + 12];
        let tag = byte_order.u16_at(&chunk[0..2]).unwrap_or(0);
        let field_type = byte_order.u16_at(&chunk[2..4]).unwrap_or(0);
        let count = byte_order.u32_at(&chunk[4..8]).unwrap_or(0);
        let mut payload = [0u8; 4];
        payload.copy_from_slice(&chunk[8..12]);
        entries.push(RawEntry {
            tag,
            field_type,
            count,
            payload,
        });
    }
    Ok(entries)
}

/// One open header's source, byte order, size and path, bundled so entry
/// resolution and sub-IFD walks don't repeat them as separate arguments at
/// every call: `read_tiff_metadata` resolves entries from IFD0 and from the
/// EXIF/GPS sub-IFDs alike.
struct IfdContext<'a, S: ByteSource> {
    source: &'a S,
    byte_order: ByteOrder,
    file_size: u64,
    path: &'a Path,
}

impl<S: ByteSource> IfdContext<'_, S> {
    /// Resolve one entry's raw bytes: inline when total size <= 4 bytes,
    /// otherwise seek to its offset.
    fn resolve_bytes(&self, entry: &RawEntry) -> Result<Vec<u8>, TiffError> {
        let Some(size) = field_type_size(entry.field_type) else {
            return Ok(Vec::new());
        };
        let total = u64::from(size) * u64::from(entry.count);
        if total <= 4 {
            Ok(entry.payload[..total as usize].to_vec())
        } else {
            let offset = u64::from(self.byte_order.u32_at(&entry.payload).unwrap_or(0));
            let past_eof = offset
                .checked_add(total)
                .is_none_or(|end| end > self.file_size);
            if past_eof {
                return Err(TiffError::ValuePastEof {
                    path: self.path.to_path_buf(),
                    tag: entry.tag,
                });
            }
            let data = self.source.read_at(offset, total as usize).map_err(|_| {
                TiffError::ValuePastEof {
                    path: self.path.to_path_buf(),
                    tag: entry.tag,
                }
            })?;
            Ok(data.into_owned())
        }
    }

    /// Read one sub-IFD at `offset`, keeping only the entries `keep`
    /// accepts and resolving each to a little-endian [`TagEntry`]. A
    /// failure at any point -- the sub-IFD itself unreadable, or one
    /// entry's value past EOF -- becomes a message in `warnings` and that
    /// much of the metadata is simply absent; `read_tiff_metadata` never
    /// fails outright over a damaged sub-IFD.
    fn read_tag_list(
        &self,
        offset: u64,
        label: &str,
        keep: impl Fn(u16) -> bool,
        warnings: &mut Vec<String>,
    ) -> Vec<TagEntry> {
        let entries = match read_ifd(self.source, self.byte_order, offset, self.path) {
            Ok(entries) => entries,
            Err(err) => {
                warnings.push(format!("{label}: {err}"));
                return Vec::new();
            }
        };
        entries
            .iter()
            .filter(|entry| keep(entry.tag))
            .filter_map(|entry| match self.resolve_bytes(entry) {
                Ok(bytes) => Some(to_tag_entry(self.byte_order, entry, bytes)),
                Err(err) => {
                    warnings.push(format!("{label} tag {}: {err}", entry.tag));
                    None
                }
            })
            .collect()
    }
}

/// Byte-swap a resolved value to little-endian by TIFF field-type width
/// (`SHORT` 2, `LONG`/`RATIONAL` half 4, `DOUBLE` 8); `BYTE`/`ASCII`/
/// `UNDEFINED` never move. Never run this on the ICC/XMP/IPTC blobs, which
/// stay opaque bytes even when mis-declared as `LONG`.
fn swap_to_little_endian(byte_order: ByteOrder, field_type: u16, bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if byte_order == ByteOrder::Little {
        return out;
    }
    match field_type {
        3 | 8 => out.chunks_exact_mut(2).for_each(|c| c.reverse()),
        4 | 5 | 9 | 10 | 11 => out.chunks_exact_mut(4).for_each(|c| c.reverse()),
        12 => out.chunks_exact_mut(8).for_each(|c| c.reverse()),
        _ => {}
    }
    out
}

fn to_tag_entry(byte_order: ByteOrder, entry: &RawEntry, bytes: Vec<u8>) -> TagEntry {
    TagEntry {
        tag: entry.tag,
        field_type: entry.field_type,
        count: entry.count,
        value: swap_to_little_endian(byte_order, entry.field_type, &bytes),
    }
}

/// Read one TIFF-source frame's descriptive metadata: the IFD0 whitelist,
/// the EXIF and GPS sub-IFDs, and the ICC/XMP/IPTC blocks as opaque bytes.
/// IFD0's own `DateTimeOriginal` (36867) moves into the EXIF list rather
/// than `ifd0`. Has no `Err` case: a damaged block becomes a warning, never
/// a reason to fail the whole read.
pub(crate) fn read_tiff_metadata<S: ByteSource>(
    source: &S,
    byte_order: ByteOrder,
    ifd0_offset: u64,
    path: &Path,
) -> SourceMetadata {
    let ctx = IfdContext {
        source,
        byte_order,
        file_size: source.len(),
        path,
    };
    let mut warnings = Vec::new();

    let ifd0_entries = match read_ifd(source, byte_order, ifd0_offset, path) {
        Ok(entries) => entries,
        Err(err) => {
            warnings.push(format!("IFD0: {err}"));
            Vec::new()
        }
    };

    let mut ifd0 = Vec::new();
    let mut exif = Vec::new();
    let mut exif_ifd_offset = None;
    let mut gps_ifd_offset = None;
    let mut icc = None;
    let mut xmp = None;
    let mut iptc = None;

    let pointer_offset = |entry: &RawEntry| {
        byte_order
            .ints_at(entry.field_type, entry.count, &entry.payload)
            .and_then(|v| v.first().copied())
    };

    for entry in &ifd0_entries {
        match entry.tag {
            TAG_EXIF_IFD => exif_ifd_offset = pointer_offset(entry),
            TAG_GPS_IFD => gps_ifd_offset = pointer_offset(entry),
            TAG_ICC_PROFILE => match ctx.resolve_bytes(entry) {
                Ok(bytes) => icc = Some(bytes),
                Err(err) => warnings.push(format!("ICC profile: {err}")),
            },
            TAG_XMP => match ctx.resolve_bytes(entry) {
                Ok(bytes) => xmp = Some(bytes),
                Err(err) => warnings.push(format!("XMP packet: {err}")),
            },
            TAG_IPTC => match ctx.resolve_bytes(entry) {
                Ok(bytes) => iptc = Some(bytes),
                Err(err) => warnings.push(format!("IPTC block: {err}")),
            },
            TAG_DATE_TIME_ORIGINAL => match ctx.resolve_bytes(entry) {
                Ok(bytes) => exif.push(to_tag_entry(byte_order, entry, bytes)),
                Err(err) => warnings.push(format!("DateTimeOriginal: {err}")),
            },
            tag if METADATA_IFD0_WHITELIST.contains(&tag) => match ctx.resolve_bytes(entry) {
                Ok(bytes) => ifd0.push(to_tag_entry(byte_order, entry, bytes)),
                Err(err) => warnings.push(format!("IFD0 tag {tag}: {err}")),
            },
            _ => {}
        }
    }

    if let Some(offset) = exif_ifd_offset {
        exif.extend(ctx.read_tag_list(
            offset,
            "EXIF sub-IFD",
            |tag| tag != TAG_MAKER_NOTE && tag != TAG_INTEROP_IFD,
            &mut warnings,
        ));
    }

    let gps = match gps_ifd_offset {
        Some(offset) => ctx.read_tag_list(offset, "GPS sub-IFD", |_| true, &mut warnings),
        None => Vec::new(),
    };

    SourceMetadata {
        ifd0,
        exif,
        gps,
        icc,
        xmp,
        iptc,
        kind: SourceKind::Tiff,
        warnings,
    }
}

/// A byte-addressable source a header can be parsed out of, without caring
/// whether the bytes come from a `File` (the `open` path) or an in-memory
/// slice (unit tests, and the mapping once it exists).
pub(crate) trait ByteSource {
    fn len(&self) -> u64;
    fn read_at(&self, off: u64, n: usize) -> io::Result<Cow<'_, [u8]>>;
}

impl ByteSource for &[u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, off: u64, n: usize) -> io::Result<Cow<'_, [u8]>> {
        let off =
            usize::try_from(off).map_err(|_| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        let end = off
            .checked_add(n)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        self.get(off..end)
            .map(Cow::Borrowed)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
    }
}

impl ByteSource for File {
    fn len(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }

    fn read_at(&self, off: u64, n: usize) -> io::Result<Cow<'_, [u8]>> {
        // `Read`/`Seek` are implemented for `&File` too (the OS file offset
        // lives in the descriptor, not in any Rust-side mutable state), so
        // no `&mut File` is needed on `self` here.
        let mut file_ref: &File = self;
        file_ref.seek(SeekFrom::Start(off))?;
        let mut buf = vec![0u8; n];
        file_ref.read_exact(&mut buf)?;
        Ok(Cow::Owned(buf))
    }
}

/// Everything the first IFD says, plus the fast-path verdict.
///
/// `TiffHeader::parse` fails with `Err` only for *structural* problems
/// (bad magic, BigTIFF, a truncated IFD, a value offset past EOF, a missing
/// required tag). A file that parses but sits outside the fast path
/// (compressed, tiled, planar, 8-bit, non-integer samples) still returns
/// `Ok`, with `refusal` set — so the CLI's `info` command can report *why*
/// the fast path does not apply as data on a header it already has, not
/// only as an error path.
#[derive(Debug, Clone)]
pub struct TiffHeader {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub byte_order: ByteOrder,
    /// IFD0's file offset, for a metadata read over the same bytes this
    /// header's own parse already located.
    pub ifd_offset: u64,
    pub bits_per_sample: Box<[u16]>,
    pub samples_per_pixel: u32,
    pub compression: u16,
    pub photometric: u16,
    pub planar_config: u16,
    pub predictor: u16,
    pub rows_per_strip: u32,
    pub tile_width: Option<u32>,
    pub tile_length: Option<u32>,
    pub sample_format: Box<[u16]>,
    pub image_description: Option<String>,
    /// EXIF `DateTimeOriginal` (36867), verbatim ASCII, from IFD0 or --
    /// when IFD0 lacks it -- the EXIF sub-IFD behind tag 34665. `None` when
    /// absent or unreadable, including when the sub-IFD pointer is junk: a
    /// frame that is otherwise a perfectly good stack member must not
    /// become unreadable because its EXIF pointer is garbage.
    ///
    /// Never `DateTime` (306). See `TAG_DATE_TIME_ORIGINAL`.
    pub date_time_original: Option<String>,
    pub file_size: u64,
    pub strip_offsets: Vec<u64>,
    pub strip_byte_counts: Vec<u64>,
    pub tile_offsets: Vec<u64>,
    pub tile_byte_counts: Vec<u64>,
    pub refusal: Option<Refusal>,
}

impl TiffHeader {
    pub fn strip_count(&self) -> usize {
        self.strip_offsets.len()
    }

    pub fn bytes_per_sample(&self) -> u32 {
        u32::from(self.bits_per_sample.first().copied().unwrap_or(16)) / 8
    }

    /// Bytes of one full image row, chunky.
    pub fn row_bytes(&self) -> u64 {
        u64::from(self.width)
            * u64::from(self.samples_per_pixel)
            * u64::from(self.bytes_per_sample())
    }

    /// `Ok(())` when this header is in the fast path, `Err` naming the
    /// refusal otherwise.
    pub fn require_fast_path(&self) -> Result<(), TiffError> {
        match &self.refusal {
            None => Ok(()),
            Some(refusal) => Err(TiffError::Unsupported {
                path: self.path.clone(),
                refusal: refusal.clone(),
            }),
        }
    }

    /// Header-only probe: parse `path`'s first IFD without requiring the
    /// fast path. Structural failures (bad magic, BigTIFF, a truncated IFD,
    /// a value past EOF, a missing required tag) are still `Err`; a file
    /// that parses but sits outside the fast path (compressed, tiled,
    /// planar, 8-bit, non-integer samples) still returns `Ok`, with
    /// `refusal` set. This is what lets the CLI's `info` report *why* the
    /// fast path does not apply — e.g. "Compression=5, only uncompressed
    /// (1) is supported" — for a file that
    /// [`TiffStack::open`](crate::io::TiffStack::open) or
    /// [`open_files`](crate::io::TiffStack::open_files) would refuse outright.
    pub fn open(path: &Path) -> Result<TiffHeader, TiffError> {
        let file = File::open(path).map_err(|source| TiffError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(path, &file)
    }

    /// Parse the first IFD of a classic TIFF out of `source`. `path` is used
    /// only for error messages: `source` need not be a real file (a unit
    /// test hands this a `Vec<u8>` it built).
    pub(crate) fn parse<S: ByteSource>(path: &Path, source: &S) -> Result<TiffHeader, TiffError> {
        let file_size = source.len();

        let head = source.read_at(0, 8).map_err(|source| TiffError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mark = [head[0], head[1]];
        let byte_order = match &mark {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => {
                return Err(TiffError::NotATiff {
                    path: path.to_path_buf(),
                    mark,
                });
            }
        };
        let magic = byte_order.u16_at(&head[2..4]).unwrap_or(0);
        let ifd_offset = u64::from(byte_order.u32_at(&head[4..8]).unwrap_or(0));

        if magic == 43 {
            return Err(TiffError::BigTiff {
                path: path.to_path_buf(),
            });
        }
        if magic != 42 {
            return Err(TiffError::BadMagic {
                path: path.to_path_buf(),
                magic,
            });
        }

        let entries = read_ifd(source, byte_order, ifd_offset, path)?;
        let ctx = IfdContext {
            source,
            byte_order,
            file_size,
            path,
        };

        let find = |tag: u16| entries.iter().find(|e| e.tag == tag);

        let read_ints = |tag: u16| -> Result<Option<Vec<u64>>, TiffError> {
            match find(tag) {
                None => Ok(None),
                Some(entry) => {
                    let bytes = ctx.resolve_bytes(entry)?;
                    Ok(byte_order.ints_at(entry.field_type, entry.count, &bytes))
                }
            }
        };

        let scalar = |tag: u16| -> Result<Option<u64>, TiffError> {
            Ok(read_ints(tag)?.and_then(|v| v.first().copied()))
        };

        let width = scalar(TAG_IMAGE_WIDTH)?.ok_or(TiffError::MissingTag {
            path: path.to_path_buf(),
            tag: TAG_IMAGE_WIDTH,
        })? as u32;
        let height = scalar(TAG_IMAGE_LENGTH)?.ok_or(TiffError::MissingTag {
            path: path.to_path_buf(),
            tag: TAG_IMAGE_LENGTH,
        })? as u32;

        let samples_per_pixel = scalar(TAG_SAMPLES_PER_PIXEL)?.unwrap_or(1) as u32;

        let bits_per_sample: Vec<u16> = match read_ints(TAG_BITS_PER_SAMPLE)? {
            Some(v) => v.into_iter().map(|x| x as u16).collect(),
            None => {
                return Err(TiffError::MissingTag {
                    path: path.to_path_buf(),
                    tag: TAG_BITS_PER_SAMPLE,
                });
            }
        };

        let compression = scalar(TAG_COMPRESSION)?.unwrap_or(1) as u16;
        let planar_config = scalar(TAG_PLANAR_CONFIG)?.unwrap_or(1) as u16;
        let predictor = scalar(TAG_PREDICTOR)?.unwrap_or(1) as u16;
        let photometric = scalar(TAG_PHOTOMETRIC)?.unwrap_or(1) as u16;

        let sample_format: Vec<u16> = match read_ints(TAG_SAMPLE_FORMAT)? {
            Some(v) => v.into_iter().map(|x| x as u16).collect(),
            None => vec![1; samples_per_pixel as usize],
        };

        let tile_width = scalar(TAG_TILE_WIDTH)?.map(|value| value as u32);
        let tile_length = scalar(TAG_TILE_LENGTH)?.map(|value| value as u32);
        let tiled = tile_width.is_some();

        let strip_offsets = read_ints(TAG_STRIP_OFFSETS)?.unwrap_or_default();
        if strip_offsets.is_empty() && !tiled {
            return Err(TiffError::MissingTag {
                path: path.to_path_buf(),
                tag: TAG_STRIP_OFFSETS,
            });
        }
        let strip_byte_counts = read_ints(TAG_STRIP_BYTE_COUNTS)?.unwrap_or_default();
        let tile_offsets = read_ints(TAG_TILE_OFFSETS)?.unwrap_or_default();
        let tile_byte_counts = read_ints(TAG_TILE_BYTE_COUNTS)?.unwrap_or_default();

        let image_description = match find(TAG_IMAGE_DESCRIPTION) {
            None => None,
            Some(entry) => {
                let bytes = ctx.resolve_bytes(entry)?;
                let text = bytes.split(|&b| b == 0).next().unwrap_or(&[]);
                Some(String::from_utf8_lossy(text).into_owned())
            }
        };

        // Capture time: IFD0 first -- where all 61 reference frames carry it
        // -- then the EXIF sub-IFD behind 34665, for exporters that do not
        // duplicate it up. Every failure below yields `None`, never `Err`:
        // a junk sub-IFD pointer must not make an otherwise good stack
        // member unreadable.
        let read_ascii = |entry: &RawEntry| -> Option<String> {
            let bytes = ctx.resolve_bytes(entry).ok()?;
            let text = bytes.split(|&b| b == 0).next().unwrap_or(&[]);
            Some(String::from_utf8_lossy(text).into_owned())
        };
        let date_time_original = match find(TAG_DATE_TIME_ORIGINAL) {
            Some(entry) => read_ascii(entry),
            None => find(TAG_EXIF_IFD)
                .and_then(|entry| {
                    byte_order
                        .ints_at(entry.field_type, entry.count, &entry.payload)
                        .and_then(|v| v.first().copied())
                })
                .and_then(|exif_offset| read_ifd(source, byte_order, exif_offset, path).ok())
                .and_then(|exif_entries| {
                    exif_entries
                        .iter()
                        .find(|e| e.tag == TAG_DATE_TIME_ORIGINAL)
                        .and_then(&read_ascii)
                }),
        };

        // TIFF's default RowsPerStrip is 2**32-1 ("one strip holds the whole
        // image"); clamp so `row / rows_per_strip` stays meaningful.
        let mut rows_per_strip = scalar(TAG_ROWS_PER_STRIP)?.unwrap_or(u64::from(height)) as u32;
        if rows_per_strip == 0 || rows_per_strip > height {
            rows_per_strip = height;
        }

        // Refusal priority: tiled first (the strip tables may not apply at
        // all to a tiled file), then compression, planar layout, bit depth,
        // sample format. At most one refusal is carried.
        let refusal = if tiled {
            Some(Refusal::Tiled)
        } else if compression != 1 {
            Some(Refusal::Compressed(compression))
        } else if planar_config != 1 {
            Some(Refusal::Planar(planar_config))
        } else if bits_per_sample.iter().any(|&b| b != 16) {
            Some(Refusal::BitDepth(
                bits_per_sample.clone().into_boxed_slice(),
            ))
        } else if sample_format.iter().any(|&f| f != 1) {
            Some(Refusal::SampleFormat(sample_format[0]))
        } else {
            None
        };

        if refusal != Some(Refusal::Tiled) {
            let expected_strips = if rows_per_strip == 0 {
                0
            } else {
                u64::from(height).div_ceil(u64::from(rows_per_strip))
            };
            if strip_offsets.len() as u64 != expected_strips {
                return Err(TiffError::StripTableMismatch {
                    path: path.to_path_buf(),
                    found: strip_offsets.len(),
                    expected: expected_strips as usize,
                });
            }
        }

        Ok(TiffHeader {
            path: path.to_path_buf(),
            width,
            height,
            byte_order,
            ifd_offset,
            bits_per_sample: bits_per_sample.into_boxed_slice(),
            samples_per_pixel,
            compression,
            photometric,
            planar_config,
            predictor,
            rows_per_strip,
            tile_width,
            tile_length,
            sample_format: sample_format.into_boxed_slice(),
            image_description,
            date_time_original,
            file_size,
            strip_offsets,
            strip_byte_counts,
            tile_offsets,
            tile_byte_counts,
            refusal,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_bytes(order: ByteOrder, v: u16) -> [u8; 2] {
        match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        }
    }

    fn long_bytes(order: ByteOrder, v: u32) -> [u8; 4] {
        match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        }
    }

    struct TestEntry {
        tag: u16,
        field_type: u16,
        count: u32,
        data: Vec<u8>,
    }

    fn entry_short(tag: u16, order: ByteOrder, vals: &[u16]) -> TestEntry {
        let mut data = Vec::new();
        for &v in vals {
            data.extend_from_slice(&short_bytes(order, v));
        }
        TestEntry {
            tag,
            field_type: 3,
            count: vals.len() as u32,
            data,
        }
    }

    fn entry_long(tag: u16, order: ByteOrder, vals: &[u32]) -> TestEntry {
        let mut data = Vec::new();
        for &v in vals {
            data.extend_from_slice(&long_bytes(order, v));
        }
        TestEntry {
            tag,
            field_type: 4,
            count: vals.len() as u32,
            data,
        }
    }

    /// Serialize a minimal classic-TIFF header (no pixel data) from a list
    /// of entries. Entries need not be tag-sorted; this parser does not
    /// require it.
    fn build(order: ByteOrder, magic: u16, entries: &[TestEntry]) -> Vec<u8> {
        let n = entries.len();
        let ifd_offset: u32 = 8;
        let fixed_end = 8 + 2 + 12 * n as u32 + 4;
        let mut cursor = fixed_end;
        let mut offsets = vec![0u32; n];
        for (i, e) in entries.iter().enumerate() {
            if e.data.len() > 4 {
                offsets[i] = cursor;
                cursor += e.data.len() as u32;
                cursor += cursor % 2;
            }
        }
        let mut buf = vec![0u8; cursor as usize];
        buf[0..2].copy_from_slice(match order {
            ByteOrder::Little => b"II",
            ByteOrder::Big => b"MM",
        });
        buf[2..4].copy_from_slice(&short_bytes(order, magic));
        buf[4..8].copy_from_slice(&long_bytes(order, ifd_offset));
        buf[8..10].copy_from_slice(&short_bytes(order, n as u16));
        let mut pos = 10usize;
        for (i, e) in entries.iter().enumerate() {
            buf[pos..pos + 2].copy_from_slice(&short_bytes(order, e.tag));
            buf[pos + 2..pos + 4].copy_from_slice(&short_bytes(order, e.field_type));
            buf[pos + 4..pos + 8].copy_from_slice(&long_bytes(order, e.count));
            if e.data.len() <= 4 {
                buf[pos + 8..pos + 8 + e.data.len()].copy_from_slice(&e.data);
            } else {
                let off = offsets[i] as usize;
                buf[pos + 8..pos + 12].copy_from_slice(&long_bytes(order, off as u32));
                buf[off..off + e.data.len()].copy_from_slice(&e.data);
            }
            pos += 12;
        }
        buf[pos..pos + 4].copy_from_slice(&long_bytes(order, 0));
        buf
    }

    fn entry_ascii(tag: u16, s: &str) -> TestEntry {
        let mut data = s.as_bytes().to_vec();
        data.push(0);
        TestEntry {
            tag,
            field_type: 2,
            count: data.len() as u32,
            data,
        }
    }

    fn entry_rational(tag: u16, order: ByteOrder, num: u32, den: u32) -> TestEntry {
        let mut data = Vec::new();
        data.extend_from_slice(&long_bytes(order, num));
        data.extend_from_slice(&long_bytes(order, den));
        TestEntry {
            tag,
            field_type: 5,
            count: 1,
            data,
        }
    }

    /// Encode one IFD (entry count, 12-byte entries, next-IFD offset, plus
    /// out-of-line values) as it would sit starting at `base_offset` in a
    /// file, so a caller can append it after other data and patch a pointer
    /// entry elsewhere to `base_offset`.
    fn encode_ifd_at(order: ByteOrder, base_offset: u32, entries: &[TestEntry]) -> Vec<u8> {
        let n = entries.len();
        let fixed_len = 2 + 12 * n as u32 + 4;
        let mut cursor = base_offset + fixed_len;
        let mut offsets = vec![0u32; n];
        for (i, e) in entries.iter().enumerate() {
            if e.data.len() > 4 {
                offsets[i] = cursor;
                cursor += e.data.len() as u32;
                cursor += cursor % 2;
            }
        }
        let mut buf = vec![0u8; (cursor - base_offset) as usize];
        buf[0..2].copy_from_slice(&short_bytes(order, n as u16));
        let mut pos = 2usize;
        for (i, e) in entries.iter().enumerate() {
            buf[pos..pos + 2].copy_from_slice(&short_bytes(order, e.tag));
            buf[pos + 2..pos + 4].copy_from_slice(&short_bytes(order, e.field_type));
            buf[pos + 4..pos + 8].copy_from_slice(&long_bytes(order, e.count));
            if e.data.len() <= 4 {
                buf[pos + 8..pos + 8 + e.data.len()].copy_from_slice(&e.data);
            } else {
                let off = offsets[i];
                buf[pos + 8..pos + 12].copy_from_slice(&long_bytes(order, off));
                let local = (off - base_offset) as usize;
                buf[local..local + e.data.len()].copy_from_slice(&e.data);
            }
            pos += 12;
        }
        buf[pos..pos + 4].copy_from_slice(&long_bytes(order, 0));
        buf
    }

    /// A classic TIFF whose IFD0 carries a pointer entry (already present in
    /// `ifd0`, its 4-byte value still `0`) for each of `sub_ifds`, appended
    /// after IFD0 and patched in once its offset is known -- the shape a
    /// real EXIF/GPS sub-IFD pair has.
    fn build_with_sub_ifds(
        order: ByteOrder,
        ifd0: &[TestEntry],
        sub_ifds: &[(u16, Vec<TestEntry>)],
    ) -> Vec<u8> {
        let mut buf = build(order, 42, ifd0);
        let mut pos = 10usize; // 8-byte file header + 2-byte entry count
        let mut pointer_positions = std::collections::HashMap::new();
        for e in ifd0 {
            pointer_positions.insert(e.tag, pos + 8);
            pos += 12;
        }
        for (pointer_tag, entries) in sub_ifds {
            let offset = buf.len() as u32;
            buf.extend_from_slice(&encode_ifd_at(order, offset, entries));
            let value_pos = *pointer_positions
                .get(pointer_tag)
                .expect("pointer tag must be present in ifd0 entries");
            buf[value_pos..value_pos + 4].copy_from_slice(&long_bytes(order, offset));
        }
        buf
    }

    /// A structurally valid 4x4 RGB 16-bit header, one strip, `order`
    /// endianness. `BitsPerSample` is 3xSHORT (6 bytes), so it is never
    /// inline -- the trap Strategy step 4 calls out.
    fn valid_entries(order: ByteOrder) -> Vec<TestEntry> {
        vec![
            entry_long(TAG_IMAGE_WIDTH, order, &[4]),
            entry_long(TAG_IMAGE_LENGTH, order, &[4]),
            entry_short(TAG_BITS_PER_SAMPLE, order, &[16, 16, 16]),
            entry_short(TAG_COMPRESSION, order, &[1]),
            entry_short(TAG_PHOTOMETRIC, order, &[2]),
            entry_long(TAG_STRIP_OFFSETS, order, &[0]),
            entry_short(TAG_SAMPLES_PER_PIXEL, order, &[3]),
            entry_long(TAG_ROWS_PER_STRIP, order, &[4]),
            entry_long(TAG_STRIP_BYTE_COUNTS, order, &[4 * 3 * 2]),
            entry_short(TAG_PLANAR_CONFIG, order, &[1]),
            entry_short(TAG_SAMPLE_FORMAT, order, &[1, 1, 1]),
        ]
    }

    fn parse(bytes: &[u8]) -> Result<TiffHeader, TiffError> {
        TiffHeader::parse(Path::new("test.tif"), &bytes)
    }

    #[test]
    fn parses_little_endian_header() {
        let bytes = build(ByteOrder::Little, 42, &valid_entries(ByteOrder::Little));
        let header = parse(&bytes).expect("valid header must parse");
        assert_eq!(header.width, 4);
        assert_eq!(header.height, 4);
        assert_eq!(header.byte_order, ByteOrder::Little);
        assert_eq!(&*header.bits_per_sample, &[16, 16, 16]);
        assert_eq!(header.samples_per_pixel, 3);
        assert_eq!(header.rows_per_strip, 4);
        assert_eq!(header.strip_offsets, vec![0]);
        assert_eq!(header.predictor, 1);
        assert_eq!(header.tile_width, None);
        assert_eq!(header.tile_length, None);
        assert!(header.refusal.is_none());
    }

    #[test]
    fn parses_big_endian_header() {
        let bytes = build(ByteOrder::Big, 42, &valid_entries(ByteOrder::Big));
        let header = parse(&bytes).expect("valid header must parse");
        assert_eq!(header.byte_order, ByteOrder::Big);
        assert_eq!(&*header.bits_per_sample, &[16, 16, 16]);
        assert!(header.refusal.is_none());
    }

    #[test]
    fn parses_tile_and_predictor_metadata_without_changing_fast_path_refusal() {
        let mut entries = valid_entries(ByteOrder::Little);
        entries
            .retain(|entry| entry.tag != TAG_STRIP_OFFSETS && entry.tag != TAG_STRIP_BYTE_COUNTS);
        entries.extend([
            entry_short(TAG_PREDICTOR, ByteOrder::Little, &[2]),
            entry_long(TAG_TILE_WIDTH, ByteOrder::Little, &[2]),
            entry_long(TAG_TILE_LENGTH, ByteOrder::Little, &[2]),
            entry_long(TAG_TILE_OFFSETS, ByteOrder::Little, &[0, 24, 48, 72]),
            entry_long(TAG_TILE_BYTE_COUNTS, ByteOrder::Little, &[24, 24, 24, 24]),
        ]);
        let bytes = build(ByteOrder::Little, 42, &entries);
        let header = parse(&bytes).expect("tiled header must still probe");
        assert_eq!(header.predictor, 2);
        assert_eq!(header.tile_width, Some(2));
        assert_eq!(header.tile_length, Some(2));
        assert_eq!(header.tile_offsets, vec![0, 24, 48, 72]);
        assert_eq!(header.tile_byte_counts, vec![24, 24, 24, 24]);
        assert_eq!(header.refusal, Some(Refusal::Tiled));
    }

    #[test]
    fn rows_per_strip_default_and_clamp() {
        // No RowsPerStrip tag at all -> defaults to height.
        let mut entries: Vec<TestEntry> = valid_entries(ByteOrder::Little)
            .into_iter()
            .filter(|e| e.tag != TAG_ROWS_PER_STRIP)
            .collect();
        // A single strip covering the whole 4-row image, since rps defaults
        // to height.
        entries.retain(|e| e.tag != TAG_STRIP_OFFSETS && e.tag != TAG_STRIP_BYTE_COUNTS);
        entries.push(entry_long(TAG_STRIP_OFFSETS, ByteOrder::Little, &[0]));
        entries.push(entry_long(
            TAG_STRIP_BYTE_COUNTS,
            ByteOrder::Little,
            &[4 * 3 * 2 * 4],
        ));
        let bytes = build(ByteOrder::Little, 42, &entries);
        let header = parse(&bytes).expect("valid header must parse");
        assert_eq!(header.rows_per_strip, 4);

        // RowsPerStrip=0 clamps to height too.
        let mut entries = valid_entries(ByteOrder::Little);
        for e in entries.iter_mut() {
            if e.tag == TAG_ROWS_PER_STRIP {
                *e = entry_long(TAG_ROWS_PER_STRIP, ByteOrder::Little, &[0]);
            }
            if e.tag == TAG_STRIP_OFFSETS {
                *e = entry_long(TAG_STRIP_OFFSETS, ByteOrder::Little, &[0]);
            }
            if e.tag == TAG_STRIP_BYTE_COUNTS {
                *e = entry_long(TAG_STRIP_BYTE_COUNTS, ByteOrder::Little, &[4 * 3 * 2 * 4]);
            }
        }
        let bytes = build(ByteOrder::Little, 42, &entries);
        let header = parse(&bytes).expect("valid header must parse");
        assert_eq!(header.rows_per_strip, 4);
    }

    #[test]
    fn strip_count_mismatch_is_named() {
        // RowsPerStrip=1 over 4 rows implies 4 strips; only give one.
        let mut entries: Vec<TestEntry> = valid_entries(ByteOrder::Little)
            .into_iter()
            .filter(|e| e.tag != TAG_ROWS_PER_STRIP)
            .collect();
        entries.push(entry_long(TAG_ROWS_PER_STRIP, ByteOrder::Little, &[1]));
        let bytes = build(ByteOrder::Little, 42, &entries);
        match parse(&bytes) {
            Err(TiffError::StripTableMismatch {
                found, expected, ..
            }) => {
                assert_eq!(found, 1);
                assert_eq!(expected, 4);
            }
            other => panic!("expected StripTableMismatch, got {other:?}"),
        }
    }

    #[test]
    fn refuses_compressed() {
        let mut entries = valid_entries(ByteOrder::Little);
        for e in entries.iter_mut() {
            if e.tag == TAG_COMPRESSION {
                *e = entry_short(TAG_COMPRESSION, ByteOrder::Little, &[5]);
            }
        }
        let bytes = build(ByteOrder::Little, 42, &entries);
        let header = parse(&bytes).expect("a compressed file still parses");
        assert_eq!(header.refusal, Some(Refusal::Compressed(5)));
        let err = header.require_fast_path().unwrap_err();
        assert!(err.to_string().contains('5'), "message: {err}");
        assert!(err.to_string().contains("Compression"), "message: {err}");
    }

    #[test]
    fn refuses_tiled() {
        let mut entries = valid_entries(ByteOrder::Little);
        entries.push(entry_short(TAG_TILE_WIDTH, ByteOrder::Little, &[4]));
        let bytes = build(ByteOrder::Little, 42, &entries);
        let header = parse(&bytes).expect("a tiled file still parses");
        assert_eq!(header.refusal, Some(Refusal::Tiled));
        let err = header.require_fast_path().unwrap_err();
        assert!(err.to_string().contains("tiled"), "message: {err}");
    }

    #[test]
    fn refuses_bigtiff() {
        // Magic 43, minimal 8-byte header.
        let bytes = build(ByteOrder::Little, 43, &[]);
        match parse(&bytes) {
            Err(TiffError::BigTiff { .. }) => {}
            other => panic!("expected BigTiff, got {other:?}"),
        }
    }

    #[test]
    fn refuses_truncated_and_past_eof() {
        // Truncated IFD: n_entries claims more than the buffer holds.
        let entries = valid_entries(ByteOrder::Little);
        let full = build(ByteOrder::Little, 42, &entries);
        let truncated = full[..12].to_vec(); // header + n_entries, no entry bytes
        match parse(&truncated) {
            Err(TiffError::TruncatedIfd { .. }) => {}
            other => panic!("expected TruncatedIfd, got {other:?}"),
        }

        // Value past EOF: cut the file off right after the fixed-size IFD,
        // before BitsPerSample's out-of-line 6 bytes.
        let n = entries.len();
        let fixed_end = 8 + 2 + 12 * n + 4;
        let cut = full[..fixed_end].to_vec();
        match parse(&cut) {
            Err(TiffError::ValuePastEof { tag, .. }) => assert_eq!(tag, TAG_BITS_PER_SAMPLE),
            other => panic!("expected ValuePastEof, got {other:?}"),
        }
    }

    #[test]
    fn refusal_is_data_not_only_error() {
        let mut entries = valid_entries(ByteOrder::Little);
        for e in entries.iter_mut() {
            if e.tag == TAG_COMPRESSION {
                *e = entry_short(TAG_COMPRESSION, ByteOrder::Little, &[5]);
            }
        }
        let bytes = build(ByteOrder::Little, 42, &entries);
        let header = parse(&bytes).expect("Ok, refusal carried as data");
        assert!(header.refusal.is_some());
        assert_eq!(header.width, 4);
        assert_eq!(header.height, 4);
        assert_eq!(header.byte_order, ByteOrder::Little);
    }

    /// Build an IFD0 with a Make tag, a dropped DocumentName, and pointers
    /// to one EXIF entry (a RATIONAL, so endian-swap actually matters) and
    /// one GPS entry, in `order`.
    fn metadata_fixture(order: ByteOrder) -> Vec<u8> {
        let ifd0 = vec![
            entry_ascii(271, "Canon"),
            entry_ascii(269, "/home/user/export.tif"),
            entry_long(TAG_EXIF_IFD, order, &[0]),
            entry_long(TAG_GPS_IFD, order, &[0]),
        ];
        let exif = vec![entry_rational(33434, order, 1, 200)]; // ExposureTime
        let gps = vec![entry_ascii(1, "N")]; // GPSLatitudeRef
        build_with_sub_ifds(order, &ifd0, &[(TAG_EXIF_IFD, exif), (TAG_GPS_IFD, gps)])
    }

    #[test]
    fn metadata_matches_across_byte_order() {
        let little = metadata_fixture(ByteOrder::Little);
        let big = metadata_fixture(ByteOrder::Big);
        let meta_little = read_tiff_metadata(&little.as_slice(), ByteOrder::Little, 8, path());
        let meta_big = read_tiff_metadata(&big.as_slice(), ByteOrder::Big, 8, path());

        assert_eq!(meta_little.ifd0, meta_big.ifd0);
        assert_eq!(meta_little.exif, meta_big.exif);
        assert_eq!(meta_little.gps, meta_big.gps);
        assert!(
            meta_little.warnings.is_empty(),
            "{:?}",
            meta_little.warnings
        );
        assert!(meta_big.warnings.is_empty(), "{:?}", meta_big.warnings);

        // Make (271) survives, DocumentName (269) does not.
        assert!(meta_little.ifd0.iter().any(|e| e.tag == 271));
        assert!(!meta_little.ifd0.iter().any(|e| e.tag == 269));

        // ExposureTime (33434, RATIONAL) decodes to the same little-endian
        // bytes from both a little- and a big-endian source: this is the
        // part that only passes once the big-endian source is swapped.
        let exposure = meta_little
            .exif
            .iter()
            .find(|e| e.tag == 33434)
            .expect("ExposureTime carried");
        let mut expected = Vec::new();
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&200u32.to_le_bytes());
        assert_eq!(exposure.value, expected);

        assert_eq!(meta_little.gps.len(), 1);
        assert_eq!(meta_little.gps[0].tag, 1);
        assert_eq!(meta_little.kind, SourceKind::Tiff);
    }

    #[test]
    fn metadata_warns_instead_of_failing_on_junk_exif_pointer() {
        let ifd0 = vec![entry_long(TAG_EXIF_IFD, ByteOrder::Little, &[999_999_999])];
        let bytes = build(ByteOrder::Little, 42, &ifd0);
        let meta = read_tiff_metadata(&bytes.as_slice(), ByteOrder::Little, 8, path());
        assert!(meta.exif.is_empty());
        assert!(
            meta.warnings.iter().any(|w| w.contains("EXIF")),
            "{:?}",
            meta.warnings
        );
    }

    #[test]
    fn metadata_warns_instead_of_failing_on_icc_past_eof() {
        // Declares 1000 UNDEFINED bytes but the file only has 8 there.
        let icc_entry = TestEntry {
            tag: TAG_ICC_PROFILE,
            field_type: 7,
            count: 1000,
            data: vec![0u8; 8],
        };
        let bytes = build(ByteOrder::Little, 42, &[icc_entry]);
        let meta = read_tiff_metadata(&bytes.as_slice(), ByteOrder::Little, 8, path());
        assert!(meta.icc.is_none());
        assert!(
            meta.warnings.iter().any(|w| w.contains("ICC")),
            "{:?}",
            meta.warnings
        );
    }

    fn path() -> &'static Path {
        Path::new("test.tif")
    }
}
