// SPDX-License-Identifier: GPL-3.0-or-later

//! 16-bit uncompressed TIFF writer: `II`, `RowsPerStrip = 1`, the fixture's
//! 12-entry tag set, mirroring
//! `tools/lib/rawtiff.py` entry for entry. It never
//! buffers a whole frame: header and both strip-offset tables are computed
//! and written once `new` returns (every offset is computable up front,
//! since `RowsPerStrip = 1`), and every later [`write_tile`](TiffWriter::write_tile)
//! call is a seek plus one row-sized write, reusing a single row buffer.
//! [`finish`](TiffWriter::finish) refuses a frame that is not fully covered
//! rather than silently accepting a hole or a double write.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use weezl::BitOrder;
use weezl::encode::Encoder as LzwEncoder;

use super::super::error::TiffError;
use super::super::metadata::{MetadataTags, TagEntry};
use super::header::{ByteOrder, TAG_EXIF_IFD, TAG_GPS_IFD, TAG_STRIP_OFFSETS, TiffHeader};
use crate::tile::Rect;

const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_IMAGE_DESCRIPTION: u16 = 270;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_PLANAR_CONFIG: u16 = 284;
const TAG_PREDICTOR: u16 = 317;
const TAG_SAMPLE_FORMAT: u16 = 339;

/// A strip's on-disk encoding. `StripWriter`'s own compression tag values,
/// matching what `segment.rs`'s decoder accepts (1, 5, 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripCompression {
    None,
    Lzw,
    Deflate,
}

impl StripCompression {
    fn tag(self) -> u16 {
        match self {
            StripCompression::None => 1,
            StripCompression::Lzw => 5,
            StripCompression::Deflate => 8,
        }
    }

    /// Horizontal differencing (predictor 2) is applied for every
    /// compressed strip and skipped for uncompressed ones, where it would
    /// only cost time.
    fn predictor(self) -> u16 {
        match self {
            StripCompression::None => 1,
            StripCompression::Lzw | StripCompression::Deflate => 2,
        }
    }
}

/// What a [`StripWriter`] will produce.
pub struct StripSpec {
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    /// 8 or 16.
    pub bits_per_sample: u16,
    pub compression: StripCompression,
    /// `None` -> [`TiffSpec::default_description`].
    pub description: Option<String>,
    /// Extra IFD0 entries plus EXIF and GPS sub-IFDs, merged into the IFD
    /// [`StripWriter::finish`] writes. Empty by default: an export with
    /// nothing to carry costs this writer nothing beyond the field itself.
    pub metadata: MetadataTags,
}

/// Bytes per strip that keeps peak memory bounded regardless of image
/// height: about 1 MiB uncompressed.
const STRIP_BUDGET_BYTES: u64 = 1 << 20;

/// Sequential strip TIFF writer: `II`, one strip per up-to-`rows_per_strip`
/// rows, in row order. A compressed or 8-bit strip has no fixed size, so
/// [`Self::finish`] writes the IFD only once every strip is written, and
/// patches its offset into the header's `IfdOffset` word. Callers hand this
/// writer raw bytes already in the target bit depth; it never converts.
pub struct StripWriter<W: Write + Seek> {
    sink: W,
    path: PathBuf,
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    bits_per_sample: u16,
    compression: StripCompression,
    description: String,
    rows_per_strip: u32,
    row_bytes: usize,
    next_row: u32,
    strip_offsets: Vec<u32>,
    strip_byte_counts: Vec<u32>,
    /// Raw (pre-predictor) bytes of the strip in progress.
    strip_buf: Vec<u8>,
    cursor: u64,
    metadata: MetadataTags,
}

impl StripWriter<File> {
    /// Create `path`, write its header, and return a writer ready for
    /// [`Self::write_rows`].
    pub fn create(path: &Path, spec: StripSpec) -> Result<Self, TiffError> {
        let file = File::create(path).map_err(|source| TiffError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut writer = Self::new(file, spec)?;
        writer.path = path.to_path_buf();
        Ok(writer)
    }
}

impl<W: Write + Seek> StripWriter<W> {
    /// Write the 8-byte classic-TIFF header (`II`, 42, a placeholder IFD
    /// offset) and nothing else -- every strip offset is unknown until the
    /// last strip has been written.
    pub fn new(mut sink: W, spec: StripSpec) -> Result<Self, TiffError> {
        let StripSpec {
            width,
            height,
            samples_per_pixel: spp,
            bits_per_sample,
            compression,
            description,
            metadata,
        } = spec;
        let path = PathBuf::new();
        let description = description.unwrap_or_else(default_description);

        let mut header = [0u8; 8];
        header[0..2].copy_from_slice(b"II");
        header[2..4].copy_from_slice(&42u16.to_le_bytes());
        // IfdOffset patched by `finish`.
        sink.write_all(&header).map_err(|source| TiffError::Io {
            path: path.clone(),
            source,
        })?;

        let row_bytes = width as usize * spp as usize * (bits_per_sample as usize / 8);
        let rows_per_strip = ((STRIP_BUDGET_BYTES as usize / row_bytes.max(1)) as u32)
            .max(1)
            .min(height.max(1));

        Ok(StripWriter {
            sink,
            path,
            width,
            height,
            samples_per_pixel: spp,
            bits_per_sample,
            compression,
            description,
            rows_per_strip,
            row_bytes,
            next_row: 0,
            strip_offsets: Vec::new(),
            strip_byte_counts: Vec::new(),
            strip_buf: Vec::new(),
            cursor: 8,
            metadata,
        })
    }

    /// Append `bytes` (a whole number of rows -- `bytes.len()` must be a
    /// multiple of the row size) as the next rows of the image, in order.
    /// Rows are buffered until a full strip (or the image's last, shorter
    /// one) is ready, then compressed and written.
    pub fn write_rows(&mut self, bytes: &[u8]) -> Result<(), TiffError> {
        if self.row_bytes == 0 || !bytes.len().is_multiple_of(self.row_bytes) {
            return Err(TiffError::BufferSize {
                expected: self.row_bytes,
                got: bytes.len(),
            });
        }
        let n_rows = (bytes.len() / self.row_bytes) as u32;
        if self.next_row + n_rows > self.height {
            return Err(TiffError::OutOfBounds {
                path: self.path.clone(),
                requested: Rect {
                    x: 0,
                    y: self.next_row,
                    width: self.width,
                    height: n_rows,
                },
                frame: (self.width, self.height),
            });
        }

        let mut offset = 0usize;
        while offset < bytes.len() {
            let rows_left_in_strip =
                self.rows_per_strip - (self.next_row % self.rows_per_strip.max(1));
            let rows_available = ((bytes.len() - offset) / self.row_bytes) as u32;
            let take_rows = rows_left_in_strip.min(rows_available).max(1);
            let take_bytes = (take_rows as usize * self.row_bytes).min(bytes.len() - offset);
            self.strip_buf
                .extend_from_slice(&bytes[offset..offset + take_bytes]);
            offset += take_bytes;
            self.next_row += take_bytes as u32 / self.row_bytes as u32;

            let strip_full = self.next_row.is_multiple_of(self.rows_per_strip.max(1));
            let image_done = self.next_row == self.height;
            if strip_full || image_done {
                self.flush_strip()?;
            }
        }
        Ok(())
    }

    fn flush_strip(&mut self) -> Result<(), TiffError> {
        if self.strip_buf.is_empty() {
            return Ok(());
        }
        apply_horizontal_predictor(
            &mut self.strip_buf,
            self.width,
            self.samples_per_pixel,
            self.bits_per_sample,
            self.compression,
        );
        let encoded = encode_strip(&self.strip_buf, self.compression);
        self.strip_offsets.push(self.cursor as u32);
        self.strip_byte_counts.push(encoded.len() as u32);
        self.sink
            .write_all(&encoded)
            .map_err(|source| TiffError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.cursor += encoded.len() as u64;
        self.strip_buf.clear();
        Ok(())
    }

    /// Flush any partial final strip, then write the IFD (with
    /// `ImageDescription` among its tags) and patch its offset into bytes
    /// 4..8 of the header. Refuses an image whose rows were never fully
    /// covered, the same way [`TiffWriter::finish`] does.
    pub fn finish(mut self) -> Result<W, TiffError>
    where
        W: 'static,
    {
        if self.next_row != self.height {
            return Err(TiffError::IncompleteFrame {
                path: self.path.clone(),
                row: self.next_row,
                covered: vec![(0, self.next_row)],
                width: self.width,
            });
        }

        let mut description_bytes = self.description.clone().into_bytes();
        description_bytes.push(0);
        let photometric: u16 = if self.samples_per_pixel >= 3 { 2 } else { 1 };
        let spp = self.samples_per_pixel;
        let n_strips = self.strip_offsets.len() as u32;

        let mut entries: Vec<TagEntry> = vec![
            TagEntry {
                tag: TAG_IMAGE_WIDTH,
                field_type: 4,
                count: 1,
                value: longs(&[self.width]),
            },
            TagEntry {
                tag: TAG_IMAGE_LENGTH,
                field_type: 4,
                count: 1,
                value: longs(&[self.height]),
            },
            TagEntry {
                tag: TAG_BITS_PER_SAMPLE,
                field_type: 3,
                count: spp,
                value: shorts(&vec![self.bits_per_sample; spp as usize]),
            },
            TagEntry {
                tag: TAG_COMPRESSION,
                field_type: 3,
                count: 1,
                value: shorts(&[self.compression.tag()]),
            },
            TagEntry {
                tag: TAG_PHOTOMETRIC,
                field_type: 3,
                count: 1,
                value: shorts(&[photometric]),
            },
            TagEntry {
                tag: TAG_IMAGE_DESCRIPTION,
                field_type: 2,
                count: description_bytes.len() as u32,
                value: description_bytes,
            },
            TagEntry {
                tag: TAG_STRIP_OFFSETS,
                field_type: 4,
                count: n_strips,
                value: longs(&self.strip_offsets),
            },
            TagEntry {
                tag: TAG_SAMPLES_PER_PIXEL,
                field_type: 3,
                count: 1,
                value: shorts(&[spp as u16]),
            },
            TagEntry {
                tag: TAG_ROWS_PER_STRIP,
                field_type: 4,
                count: 1,
                value: longs(&[self.rows_per_strip]),
            },
            TagEntry {
                tag: TAG_STRIP_BYTE_COUNTS,
                field_type: 4,
                count: n_strips,
                value: longs(&self.strip_byte_counts),
            },
            TagEntry {
                tag: TAG_PLANAR_CONFIG,
                field_type: 3,
                count: 1,
                value: shorts(&[1u16]),
            },
            TagEntry {
                tag: TAG_PREDICTOR,
                field_type: 3,
                count: 1,
                value: shorts(&[self.compression.predictor()]),
            },
            TagEntry {
                tag: TAG_SAMPLE_FORMAT,
                field_type: 3,
                count: spp,
                value: shorts(&vec![1u16; spp as usize]),
            },
        ];
        entries.extend(self.metadata.ifd0.iter().cloned());

        // Pad to a word boundary: TIFF 6.0 requires the IFD to start on an
        // even byte offset.
        if !self.cursor.is_multiple_of(2) {
            self.sink
                .write_all(&[0u8])
                .map_err(|source| TiffError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            self.cursor += 1;
        }
        let ifd_offset = self.cursor;
        let buf = ifd0_with_sub_ifds(
            &entries,
            &self.metadata.exif,
            &self.metadata.gps,
            ifd_offset,
            0,
        );

        self.sink.write_all(&buf).map_err(|source| TiffError::Io {
            path: self.path.clone(),
            source,
        })?;

        self.sink
            .seek(SeekFrom::Start(4))
            .map_err(|source| TiffError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.sink
            .write_all(&(ifd_offset as u32).to_le_bytes())
            .map_err(|source| TiffError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.sink.flush().map_err(|source| TiffError::Io {
            path: self.path.clone(),
            source,
        })?;
        if let Some(file) = (&mut self.sink as &mut dyn std::any::Any).downcast_mut::<File>() {
            file.sync_all().map_err(|source| TiffError::Io {
                path: self.path.clone(),
                source,
            })?;
        }
        Ok(self.sink)
    }
}

/// One IFD's entry table plus its out-of-line values, starting at `at`
/// (already word-aligned) and pointing `next` at the following IFD's offset
/// (0 when there is none). An entry whose value fits in 4 bytes is stored
/// inline; a longer one is appended after the entry table, in entry order,
/// each padded to an even length -- the classic-TIFF layout `StripWriter`
/// and [`crate::io::metadata::exif_blob`] both produce.
pub(crate) fn ifd_bytes(entries: &[TagEntry], at: u64, next: u32) -> Vec<u8> {
    debug_assert!(at.is_multiple_of(2));
    let n = entries.len() as u32;
    let mut cursor = at + 2 + 12 * u64::from(n) + 4;
    debug_assert!(cursor.is_multiple_of(2));
    let mut offsets: Vec<u64> = vec![0; entries.len()];
    for (i, entry) in entries.iter().enumerate() {
        if entry.value.len() > 4 {
            offsets[i] = cursor;
            cursor += entry.value.len() as u64;
            cursor += cursor % 2;
        }
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(&(n as u16).to_le_bytes());
    for (i, entry) in entries.iter().enumerate() {
        buf.extend_from_slice(&entry.tag.to_le_bytes());
        buf.extend_from_slice(&entry.field_type.to_le_bytes());
        buf.extend_from_slice(&entry.count.to_le_bytes());
        if entry.value.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..entry.value.len()].copy_from_slice(&entry.value);
            buf.extend_from_slice(&inline);
        } else {
            buf.extend_from_slice(&(offsets[i] as u32).to_le_bytes());
        }
    }
    buf.extend_from_slice(&next.to_le_bytes());
    // Values that did not fit inline follow the entry table, each at its
    // precomputed offset. `at` is word-aligned, so the entry table's own
    // length (2 + 12n + 4, always even) lands `buf` exactly at that first
    // offset with no further padding needed here.
    let mut pos = at + buf.len() as u64;
    for (i, entry) in entries.iter().enumerate() {
        if entry.value.len() > 4 {
            debug_assert_eq!(offsets[i], pos);
            buf.extend_from_slice(&entry.value);
            if entry.value.len() % 2 == 1 {
                buf.push(0);
            }
            pos += entry.value.len() as u64;
            pos += pos % 2;
        }
    }
    buf
}

/// IFD0 (sorted by tag, with a 34665/34853 pointer added for each non-empty
/// sub-IFD) plus its EXIF and GPS sub-IFDs, laid out back to back from `at`
/// through [`ifd_bytes`]. A pointer starts as a placeholder and is patched
/// once its sub-IFD's offset is known -- the offset never depends on the
/// pointer's own value. Shared by [`StripWriter::finish`] (`at` is the
/// file's IFD0 offset) and [`crate::io::metadata::exif_blob`] (`at` is 8).
pub(crate) fn ifd0_with_sub_ifds(
    ifd0_entries: &[TagEntry],
    exif: &[TagEntry],
    gps: &[TagEntry],
    at: u64,
    next: u32,
) -> Vec<u8> {
    let mut ifd0: Vec<TagEntry> = ifd0_entries.to_vec();
    if !exif.is_empty() {
        ifd0.push(TagEntry {
            tag: TAG_EXIF_IFD,
            field_type: 4,
            count: 1,
            value: vec![0u8; 4],
        });
    }
    if !gps.is_empty() {
        ifd0.push(TagEntry {
            tag: TAG_GPS_IFD,
            field_type: 4,
            count: 1,
            value: vec![0u8; 4],
        });
    }
    ifd0.sort_by_key(|entry| entry.tag);

    let mut cursor = at + ifd_bytes(&ifd0, at, next).len() as u64;
    let exif_bytes = if exif.is_empty() {
        Vec::new()
    } else {
        let mut sorted = exif.to_vec();
        sorted.sort_by_key(|entry| entry.tag);
        let bytes = ifd_bytes(&sorted, cursor, 0);
        for entry in &mut ifd0 {
            if entry.tag == TAG_EXIF_IFD {
                entry.value = (cursor as u32).to_le_bytes().to_vec();
            }
        }
        cursor += bytes.len() as u64;
        bytes
    };
    let gps_bytes = if gps.is_empty() {
        Vec::new()
    } else {
        let mut sorted = gps.to_vec();
        sorted.sort_by_key(|entry| entry.tag);
        let bytes = ifd_bytes(&sorted, cursor, 0);
        for entry in &mut ifd0 {
            if entry.tag == TAG_GPS_IFD {
                entry.value = (cursor as u32).to_le_bytes().to_vec();
            }
        }
        bytes
    };

    let mut out = ifd_bytes(&ifd0, at, next);
    out.extend_from_slice(&exif_bytes);
    out.extend_from_slice(&gps_bytes);
    out
}

/// Undoes-in-reverse: `decoded`'s row-major samples become their horizontal
/// difference from the previous sample of the same channel, matching what
/// `segment.rs`'s decoder (predictor 2) adds back. A no-op when
/// `compression` carries no predictor.
fn apply_horizontal_predictor(
    bytes: &mut [u8],
    width: u32,
    spp: u32,
    bits_per_sample: u16,
    compression: StripCompression,
) {
    if compression.predictor() != 2 {
        return;
    }
    let row_samples = width as usize * spp as usize;
    let spp = spp as usize;
    if bits_per_sample == 16 {
        for row in bytes.chunks_exact_mut(row_samples * 2) {
            let mut prev = vec![0u16; spp];
            for i in 0..row_samples {
                let off = i * 2;
                let value = u16::from_le_bytes([row[off], row[off + 1]]);
                let channel = i % spp;
                let diff = value.wrapping_sub(prev[channel]);
                prev[channel] = value;
                row[off..off + 2].copy_from_slice(&diff.to_le_bytes());
            }
        }
    } else {
        for row in bytes.chunks_exact_mut(row_samples) {
            let mut prev = vec![0u8; spp];
            for (i, byte) in row.iter_mut().enumerate() {
                let channel = i % spp;
                let diff = byte.wrapping_sub(prev[channel]);
                prev[channel] = *byte;
                *byte = diff;
            }
        }
    }
}

fn encode_strip(raw: &[u8], compression: StripCompression) -> Vec<u8> {
    match compression {
        StripCompression::None => raw.to_vec(),
        StripCompression::Lzw => LzwEncoder::with_tiff_size_switch(BitOrder::Msb, 8)
            .encode(raw)
            .expect("in-memory LZW encode cannot fail"),
        StripCompression::Deflate => {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(raw)
                .expect("in-memory zlib encode cannot fail");
            encoder.finish().expect("in-memory zlib encode cannot fail")
        }
    }
}

/// What frame a [`TiffWriter`] will produce.
pub struct TiffSpec {
    pub width: u32,
    pub height: u32,
    /// 1 for a Z-Map plane, 3 for RGB.
    pub samples_per_pixel: u32,
    /// `None` -> `"FocusForge <CARGO_PKG_VERSION>"`.
    pub description: Option<String>,
}

impl TiffSpec {
    /// The `ImageDescription` a writer falls back to when
    /// [`TiffSpec::description`] is `None`.
    ///
    /// Public because a caller that must *reproduce* a written file byte for
    /// byte has to know the string rather than guess it: `crates/cli`'s
    /// `stack` resolves the description once, before fusion, and records it
    /// in the project manifest so `export` re-emits the identical tag.
    pub fn default_description() -> String {
        default_description()
    }
}

/// Random-access 16-bit TIFF writer. Never holds a whole frame in memory.
pub struct TiffWriter<W: Write + Seek> {
    sink: W,
    path: PathBuf,
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    pixel_start: u64,
    row_bytes: u64,
    /// One merged, sorted interval list per image row.
    coverage: Vec<Vec<(u32, u32)>>,
    row_buf: Vec<u8>,
}

impl TiffWriter<File> {
    /// Create `path`, write its header and offset tables, and return a
    /// writer ready for [`write_tile`](TiffWriter::write_tile).
    pub fn create(path: &Path, spec: TiffSpec) -> Result<Self, TiffError> {
        let file = File::create(path).map_err(|source| TiffError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut writer = Self::new(file, spec)?;
        writer.path = path.to_path_buf();
        Ok(writer)
    }

    /// Reopens `path` -- an existing file this writer (or an interrupted
    /// prior run of it) already created via [`Self::create`] -- for further
    /// [`write_tile`](TiffWriter::write_tile) calls, refusing anything that
    /// is not the exact layout `create` would itself have produced.
    ///
    /// **Never truncates**: `OpenOptions::read(true).write(true)`, not
    /// `File::create` (`create` would truncate the file this reopen must
    /// extend).
    /// Coverage starts empty, exactly like a freshly created writer -- a
    /// caller reseeds it with `seed_coverage` for whatever rows the
    /// file already durably holds (checked with `holds_rows`) before
    /// writing anything further, so `finish()` accepts a resumed file only
    /// once seeded coverage plus newly written rows again cover every row.
    pub fn reopen(path: &Path, spec: TiffSpec) -> Result<Self, TiffError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| TiffError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let header = TiffHeader::parse(path, &file)?;
        let (pixel_start, row_bytes) = validate_reopen(path, &header, &spec)?;

        Ok(TiffWriter {
            sink: file,
            path: path.to_path_buf(),
            width: header.width,
            height: header.height,
            samples_per_pixel: header.samples_per_pixel,
            pixel_start,
            row_bytes,
            coverage: vec![Vec::new(); header.height as usize],
            row_buf: Vec::new(),
        })
    }

    /// Whether the file on disk currently holds every byte
    /// [`write_tile`](TiffWriter::write_tile) would have written for `rect`,
    /// checked against `rect`'s own last written byte -- `write_tile`
    /// writes its rows in order on one thread, so reaching that offset is
    /// necessary and sufficient. A recorded-complete unit this returns
    /// `false` for is dropped back into the recomputed set.
    pub(crate) fn holds_rows(&self, rect: Rect) -> bool {
        let file_len = self.sink.metadata().map(|m| m.len()).unwrap_or(0);
        let last_row = self.pixel_start + (rect.y + rect.height - 1) as u64 * self.row_bytes;
        let last_row_end =
            last_row + (rect.x + rect.width) as u64 * self.samples_per_pixel as u64 * 2;
        file_len >= last_row_end
    }
}

/// `"FocusForge <CARGO_PKG_VERSION>"` -- the default `ImageDescription`
/// every writer falls back to when [`TiffSpec::description`] is `None`, in
/// one place so [`TiffWriter::new`] and [`validate_reopen`] can never drift
/// apart on the formula.
fn default_description() -> String {
    format!("FocusForge {}", env!("CARGO_PKG_VERSION"))
}

/// `spec`'s effective `ImageDescription`, applying the same default
/// [`TiffWriter::new`] does.
fn spec_description(spec: &TiffSpec) -> String {
    spec.description.clone().unwrap_or_else(default_description)
}

/// Shared by [`TiffWriter::<File>::reopen`]: validates `spec` against an
/// already-parsed header, refusing anything that is not the file `create`
/// would itself have produced -- byte order, compression, `RowsPerStrip`,
/// dimensions, description, and an arithmetic strip-offset table
/// (`pixel_start + s * row_bytes`, taken from `strip_offsets[0]` and
/// `header.row_bytes()` rather than recomputed, so a layout the parse
/// accepted is the layout the writer uses). Returns `(pixel_start,
/// row_bytes)` for the caller to build a [`TiffWriter`] from. Factored out
/// as a free function (not tied to `W`) so a unit test can drive it over a
/// header parsed from a `Cursor<Vec<u8>>`, with no real file.
fn validate_reopen(
    path: &Path,
    header: &TiffHeader,
    spec: &TiffSpec,
) -> Result<(u64, u64), TiffError> {
    macro_rules! check {
        ($field:literal, $expected:expr, $found:expr) => {
            if $expected != $found {
                return Err(TiffError::ReopenMismatch {
                    path: path.to_path_buf(),
                    field: $field,
                    expected: format!("{:?}", $expected),
                    found: format!("{:?}", $found),
                });
            }
        };
    }

    check!("byte_order", ByteOrder::Little, header.byte_order);
    check!("compression", 1u16, header.compression);
    check!("rows_per_strip", 1u32, header.rows_per_strip);
    check!("width", spec.width, header.width);
    check!("height", spec.height, header.height);
    check!(
        "samples_per_pixel",
        spec.samples_per_pixel,
        header.samples_per_pixel
    );

    let expected_description = spec_description(spec);
    check!(
        "image_description",
        Some(expected_description.clone()),
        header.image_description.clone()
    );

    let row_bytes = header.row_bytes();
    let pixel_start = *header
        .strip_offsets
        .first()
        .ok_or_else(|| TiffError::ReopenMismatch {
            path: path.to_path_buf(),
            field: "strip_offsets",
            expected: "at least one strip".to_string(),
            found: "none".to_string(),
        })?;
    for (s, &offset) in header.strip_offsets.iter().enumerate() {
        let expected_offset = pixel_start + s as u64 * row_bytes;
        if offset != expected_offset {
            return Err(TiffError::ReopenMismatch {
                path: path.to_path_buf(),
                field: "strip_offsets",
                expected: expected_offset.to_string(),
                found: offset.to_string(),
            });
        }
    }

    Ok((pixel_start, row_bytes))
}

impl<W: Write + Seek> TiffWriter<W> {
    /// Build the header and both strip-offset tables now: 12 IFD entries
    /// in tag order, header -> IFD -> out-of-line values ->
    /// pixels, every out-of-line value word-aligned. `path` in any error
    /// this writer reports is empty unless the writer was created via
    /// [`TiffWriter::<File>::create`].
    pub fn new(mut sink: W, spec: TiffSpec) -> Result<Self, TiffError> {
        let TiffSpec {
            width,
            height,
            samples_per_pixel: spp,
            description,
        } = spec;
        let path = PathBuf::new();

        let description = description.unwrap_or_else(default_description);
        let mut description_bytes = description.into_bytes();
        description_bytes.push(0);

        let photometric: u16 = if spp >= 3 { 2 } else { 1 };
        let row_bytes = u64::from(width) * u64::from(spp) * 2;
        let n_strips = height; // RowsPerStrip = 1: one strip per row.

        // (tag, field_type, count, data), already in tag order.
        let mut entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (TAG_IMAGE_WIDTH, 4, 1, longs(&[width])),
            (TAG_IMAGE_LENGTH, 4, 1, longs(&[height])),
            (
                TAG_BITS_PER_SAMPLE,
                3,
                spp,
                shorts(&vec![16u16; spp as usize]),
            ),
            (TAG_COMPRESSION, 3, 1, shorts(&[1u16])),
            (TAG_PHOTOMETRIC, 3, 1, shorts(&[photometric])),
            (
                TAG_IMAGE_DESCRIPTION,
                2,
                description_bytes.len() as u32,
                description_bytes,
            ),
            (
                TAG_STRIP_OFFSETS,
                4,
                n_strips,
                vec![0u8; 4 * n_strips as usize],
            ),
            (TAG_SAMPLES_PER_PIXEL, 3, 1, shorts(&[spp as u16])),
            (TAG_ROWS_PER_STRIP, 4, 1, longs(&[1])),
            (
                TAG_STRIP_BYTE_COUNTS,
                4,
                n_strips,
                longs(&vec![row_bytes as u32; n_strips as usize]),
            ),
            (TAG_PLANAR_CONFIG, 3, 1, shorts(&[1u16])),
            (TAG_SAMPLE_FORMAT, 3, spp, shorts(&vec![1u16; spp as usize])),
        ];

        let n = entries.len() as u32;
        let mut cursor: u64 = 8 + 2 + 12 * u64::from(n) + 4;
        cursor += cursor % 2;
        let mut offsets: Vec<u64> = vec![0; entries.len()];
        for (i, (_, _, _, data)) in entries.iter().enumerate() {
            if data.len() > 4 {
                offsets[i] = cursor;
                cursor += data.len() as u64;
                cursor += cursor % 2;
            }
        }
        let pixel_start = cursor;

        // Patch StripOffsets with the now-computable real values. The patch
        // is length-preserving (n_strips LONGs either way), so the layout
        // computed above still holds.
        let strip_offsets_index = entries
            .iter()
            .position(|(tag, ..)| *tag == TAG_STRIP_OFFSETS)
            .expect("StripOffsets entry always present");
        let strip_offsets: Vec<u32> = (0..n_strips)
            .map(|s| (pixel_start + u64::from(s) * row_bytes) as u32)
            .collect();
        entries[strip_offsets_index].3 = longs(&strip_offsets);

        // Serialize header + IFD + out-of-line values into one buffer.
        let mut buf = vec![0u8; pixel_start as usize];
        buf[0..2].copy_from_slice(b"II");
        buf[2..4].copy_from_slice(&42u16.to_le_bytes());
        buf[4..8].copy_from_slice(&8u32.to_le_bytes());
        buf[8..10].copy_from_slice(&(n as u16).to_le_bytes());
        let mut pos = 10usize;
        for (i, (tag, field_type, count, data)) in entries.iter().enumerate() {
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
        buf[pos..pos + 4].copy_from_slice(&0u32.to_le_bytes()); // no next IFD

        sink.write_all(&buf).map_err(|source| TiffError::Io {
            path: path.clone(),
            source,
        })?;

        Ok(TiffWriter {
            sink,
            path,
            width,
            height,
            samples_per_pixel: spp,
            pixel_start,
            row_bytes,
            coverage: vec![Vec::new(); height as usize],
            row_buf: Vec::new(),
        })
    }

    /// Write `samples` (chunky, `rect.width * rect.height * samples_per_pixel`
    /// `u16`s) into `rect` of the frame. A rect not fully inside the frame,
    /// or zero-sized, is `OutOfBounds`; a wrong-length buffer is
    /// `BufferSize`. No whole-frame buffer anywhere: `row_buf` is reused
    /// across calls and across rows of the same call.
    pub fn write_tile(&mut self, rect: Rect, samples: &[u16]) -> Result<(), TiffError> {
        let in_bounds = rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|xw| xw <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.height);
        if !in_bounds {
            return Err(TiffError::OutOfBounds {
                path: self.path.clone(),
                requested: rect,
                frame: (self.width, self.height),
            });
        }

        let spp = self.samples_per_pixel as usize;
        let expected_len = rect.width as usize * rect.height as usize * spp;
        if samples.len() != expected_len {
            return Err(TiffError::BufferSize {
                expected: expected_len,
                got: samples.len(),
            });
        }

        let row_samples = rect.width as usize * spp;
        self.row_buf.resize(row_samples * 2, 0);
        for r in 0..rect.height {
            let row = rect.y + r;
            let src = &samples[r as usize * row_samples..(r as usize + 1) * row_samples];
            for (i, &s) in src.iter().enumerate() {
                self.row_buf[i * 2..i * 2 + 2].copy_from_slice(&s.to_le_bytes());
            }
            let offset = self.pixel_start
                + u64::from(row) * self.row_bytes
                + u64::from(rect.x) * spp as u64 * 2;
            self.sink
                .seek(SeekFrom::Start(offset))
                .map_err(|source| TiffError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            self.sink
                .write_all(&self.row_buf)
                .map_err(|source| TiffError::Io {
                    path: self.path.clone(),
                    source,
                })?;

            insert_interval(
                &mut self.coverage[row as usize],
                rect.x,
                rect.x + rect.width,
            );
        }
        Ok(())
    }

    /// Marks `rect`'s rows as already covered, without writing any bytes --
    /// what a [`TiffWriter::<File>::reopen`]ed writer's coverage tracker is
    /// reseeded with, for whatever rows [`TiffWriter::<File>::holds_rows`]
    /// confirms the file already durably holds, before `finish()` can accept
    /// a resumed file. `pub(crate)`, never `pub`: a public "mark this
    /// covered" would be a way to make `finish()` accept a file with a hole
    /// in it.
    pub(crate) fn seed_coverage(&mut self, rect: Rect) {
        for r in 0..rect.height {
            let row = rect.y + r;
            insert_interval(
                &mut self.coverage[row as usize],
                rect.x,
                rect.x + rect.width,
            );
        }
    }

    /// `Ok(())` only when every row's merged coverage is exactly
    /// `[(0, width)]`; otherwise `IncompleteFrame` naming the first row that
    /// is short. A per-row covered-sample *counter* would not work: it
    /// reads green for a double-written region that compensates a gap
    /// elsewhere. Ends with `flush()`; a `File` sink is
    /// `sync_all()`ed first so a caller that immediately reopens the file
    /// through `TiffStack` sees all of it.
    pub fn finish(mut self) -> Result<(), TiffError>
    where
        W: 'static,
    {
        for (row, intervals) in self.coverage.iter().enumerate() {
            let full = intervals.len() == 1 && intervals[0] == (0, self.width);
            if !full {
                return Err(TiffError::IncompleteFrame {
                    path: self.path.clone(),
                    row: row as u32,
                    covered: intervals.clone(),
                    width: self.width,
                });
            }
        }
        if let Some(file) = (&mut self.sink as &mut dyn std::any::Any).downcast_mut::<File>() {
            file.sync_all().map_err(|source| TiffError::Io {
                path: self.path.clone(),
                source,
            })?;
        }
        self.sink.flush().map_err(|source| TiffError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

fn shorts(vals: &[u16]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn longs(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Insert `[start, end)` into `intervals`, keeping the list sorted and
/// merged. O(n) per call; at 8192x5464 with 512px tiles this is at most a
/// few dozen intervals per row before merging collapses them.
fn insert_interval(intervals: &mut Vec<(u32, u32)>, start: u32, end: u32) {
    intervals.push((start, end));
    intervals.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(intervals.len());
    for &(s, e) in intervals.iter() {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
            continue;
        }
        merged.push((s, e));
    }
    *intervals = merged;
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::io::tiff::header::TiffHeader;

    fn spec(width: u32, height: u32, spp: u32) -> TiffSpec {
        TiffSpec {
            width,
            height,
            samples_per_pixel: spp,
            description: None,
        }
    }

    fn parse_cursor(cursor: &Cursor<Vec<u8>>) -> TiffHeader {
        let bytes: &[u8] = cursor.get_ref();
        TiffHeader::parse(Path::new("test.tif"), &bytes).expect("written bytes must parse")
    }

    #[test]
    fn layout_has_the_twelve_tag_fixture_set_in_order() {
        let writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 3)).unwrap();
        let header = parse_cursor(&writer.sink);
        assert_eq!(header.width, 4);
        assert_eq!(header.height, 4);
        assert_eq!(header.byte_order, super::super::header::ByteOrder::Little);
        assert_eq!(&*header.bits_per_sample, &[16, 16, 16]);
        assert_eq!(header.samples_per_pixel, 3);
        assert_eq!(header.compression, 1);
        assert_eq!(header.rows_per_strip, 1);
        assert_eq!(header.strip_count(), 4);
        assert_eq!(&*header.sample_format, &[1, 1, 1]);
        assert_eq!(header.photometric, 2);
        assert_eq!(header.planar_config, 1);
        assert!(header.refusal.is_none());
    }

    #[test]
    fn spp_one_uses_inline_bits_per_sample_and_sample_format() {
        let writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        let header = parse_cursor(&writer.sink);
        assert_eq!(&*header.bits_per_sample, &[16]);
        assert_eq!(&*header.sample_format, &[1]);
        assert_eq!(header.photometric, 1);
    }

    #[test]
    fn write_tile_rejects_out_of_bounds_rect() {
        let mut writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        let rect = Rect {
            x: 2,
            y: 0,
            width: 4,
            height: 4,
        };
        match writer.write_tile(rect, &[0u16; 16]) {
            Err(TiffError::OutOfBounds { .. }) => {}
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn write_tile_rejects_zero_sized_rect() {
        let mut writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 4,
        };
        match writer.write_tile(rect, &[]) {
            Err(TiffError::OutOfBounds { .. }) => {}
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn write_tile_rejects_wrong_length_buffer() {
        let mut writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        match writer.write_tile(rect, &[0u16; 3]) {
            Err(TiffError::BufferSize {
                expected: 16,
                got: 3,
            }) => {}
            other => panic!("expected BufferSize, got {other:?}"),
        }
    }

    #[test]
    fn finish_refuses_an_incomplete_frame() {
        let mut writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        // Write every row but the last.
        for y in 0..3u32 {
            let rect = Rect {
                x: 0,
                y,
                width: 4,
                height: 1,
            };
            writer.write_tile(rect, &[1u16; 4]).unwrap();
        }
        match writer.finish() {
            Err(TiffError::IncompleteFrame { row, .. }) => assert_eq!(row, 3),
            other => panic!("expected IncompleteFrame naming row 3, got {other:?}"),
        }
    }

    #[test]
    fn tiles_in_reverse_raster_order_still_complete() {
        let mut writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        for y in (0..4u32).rev() {
            let rect = Rect {
                x: 0,
                y,
                width: 4,
                height: 1,
            };
            writer.write_tile(rect, &[7u16; 4]).unwrap();
        }
        writer.finish().expect("fully covered frame must finish");
    }

    #[test]
    fn header_parse_reads_kilobytes_not_the_file() {
        // A 512x512 3ch header: bound the bytes a header parse touches.
        use super::super::header::ByteSource;

        struct CountingSource<'a> {
            bytes: &'a [u8],
            read: std::cell::Cell<u64>,
        }
        impl<'a> ByteSource for CountingSource<'a> {
            fn len(&self) -> u64 {
                self.bytes.len() as u64
            }
            fn read_at(&self, off: u64, n: usize) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
                self.read.set(self.read.get() + n as u64);
                self.bytes
                    .get(off as usize..off as usize + n)
                    .map(std::borrow::Cow::Borrowed)
                    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
            }
        }

        let writer = TiffWriter::new(Cursor::new(Vec::new()), spec(512, 512, 3)).unwrap();
        let bytes: &[u8] = writer.sink.get_ref();
        let source = CountingSource {
            bytes,
            read: std::cell::Cell::new(0),
        };
        let header = TiffHeader::parse(Path::new("test.tif"), &source).expect("must parse");
        assert_eq!(header.strip_count(), 512);
        assert!(
            source.read.get() < 16 * 1024,
            "header parse read {} bytes, expected under 16 KiB",
            source.read.get()
        );
    }

    #[test]
    fn round_trip_via_cursor_matches_written_samples() {
        let width = 4;
        let height = 4;
        let mut writer = TiffWriter::new(Cursor::new(Vec::new()), spec(width, height, 1)).unwrap();
        let mut expected = Vec::new();
        for y in 0..height {
            let row: Vec<u16> = (0..width).map(|x| (y * width + x) as u16).collect();
            expected.extend_from_slice(&row);
            writer
                .write_tile(
                    Rect {
                        x: 0,
                        y,
                        width,
                        height: 1,
                    },
                    &row,
                )
                .unwrap();
        }
        // Capture layout and pixel bytes before `finish` consumes the writer;
        // there is no reopened stack here (no file on disk), so the pixel
        // bytes are decoded straight out of the in-memory sink instead.
        let pixel_start = writer.pixel_start as usize;
        let row_bytes = writer.row_bytes as usize;
        let bytes = writer.sink.get_ref().clone();
        writer.finish().unwrap();

        let mut actual = Vec::with_capacity(expected.len());
        for y in 0..height as usize {
            let row_off = pixel_start + y * row_bytes;
            for x in 0..width as usize {
                let off = row_off + x * 2;
                actual.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
            }
        }
        assert_eq!(
            actual, expected,
            "bytes at each row's computed offset must equal what write_tile wrote"
        );
    }

    // --- reopen ------------------------------------------------------------

    /// The `validate_reopen` logic exercised over a `Cursor<Vec<u8>>`'s
    /// parsed header -- no real file, so the mismatch checks are pinned
    /// without filesystem I/O.
    #[test]
    fn validate_reopen_accepts_the_header_it_would_itself_produce() {
        let writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        let header = parse_cursor(&writer.sink);
        let (pixel_start, row_bytes) =
            validate_reopen(Path::new("test.tif"), &header, &spec(4, 4, 1)).unwrap();
        assert_eq!(pixel_start, writer.pixel_start);
        assert_eq!(row_bytes, writer.row_bytes);
    }

    #[test]
    fn validate_reopen_refuses_a_mismatched_width() {
        let writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        let header = parse_cursor(&writer.sink);
        match validate_reopen(Path::new("test.tif"), &header, &spec(8, 4, 1)) {
            Err(TiffError::ReopenMismatch { field: "width", .. }) => {}
            other => panic!("expected ReopenMismatch on width, got {other:?}"),
        }
    }

    #[test]
    fn validate_reopen_refuses_a_mismatched_description() {
        let writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        let header = parse_cursor(&writer.sink);
        let mut mismatched = spec(4, 4, 1);
        mismatched.description = Some("something else".to_string());
        match validate_reopen(Path::new("test.tif"), &header, &mismatched) {
            Err(TiffError::ReopenMismatch {
                field: "image_description",
                ..
            }) => {}
            other => panic!("expected ReopenMismatch on image_description, got {other:?}"),
        }
    }

    #[test]
    fn validate_reopen_refuses_a_mismatched_rows_per_strip() {
        // A header whose RowsPerStrip is not 1 (e.g. a real stack frame's
        // multi-row strips) must never be accepted for reopen -- this
        // writer's whole layout assumes one strip per row.
        let mut header = parse_cursor(
            &TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1))
                .unwrap()
                .sink,
        );
        header.rows_per_strip = 2;
        match validate_reopen(Path::new("test.tif"), &header, &spec(4, 4, 1)) {
            Err(TiffError::ReopenMismatch {
                field: "rows_per_strip",
                ..
            }) => {}
            other => panic!("expected ReopenMismatch on rows_per_strip, got {other:?}"),
        }
    }

    #[test]
    fn reopen_accepts_the_file_it_wrote_and_seeded_coverage_plus_new_rows_finish() {
        let dir =
            std::env::temp_dir().join(format!("ff-tiff-writer-reopen-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("reopen.tif");
        let _ = std::fs::remove_file(&path);

        // First "segment": write rows 0..2 of a 4x4 frame, then simulate a
        // crash by dropping the writer without calling `finish()`.
        {
            let mut writer = TiffWriter::create(&path, spec(4, 4, 1)).unwrap();
            for y in 0..2u32 {
                writer
                    .write_tile(
                        Rect {
                            x: 0,
                            y,
                            width: 4,
                            height: 1,
                        },
                        &[9u16; 4],
                    )
                    .unwrap();
            }
            // Dropped here, unfinished -- the coverage tracker (in-memory)
            // is lost, but the bytes already reached the OS.
        }

        // Reopen: coverage starts empty again, so it must be reseeded for
        // the rows already on disk before `finish()` can succeed.
        let mut reopened = TiffWriter::<File>::reopen(&path, spec(4, 4, 1)).unwrap();
        assert!(reopened.holds_rows(Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        }));
        assert!(!reopened.holds_rows(Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        }));
        reopened.seed_coverage(Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        });
        for y in 2..4u32 {
            reopened
                .write_tile(
                    Rect {
                        x: 0,
                        y,
                        width: 4,
                        height: 1,
                    },
                    &[9u16; 4],
                )
                .unwrap();
        }
        reopened
            .finish()
            .expect("seeded plus newly written rows must cover the whole frame");

        let bytes = std::fs::read(&path).unwrap();
        // Every pixel byte must equal 9 (as u16 LE) -- the header is proven
        // unchanged by `header_parse_reads_kilobytes_not_the_file` and the
        // other layout tests above; this test only needs the pixel bytes.
        let header = TiffHeader::open(&path).unwrap();
        let pixel_start = header.strip_offsets[0] as usize;
        for i in 0..16usize {
            let off = pixel_start + i * 2;
            let value = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
            assert_eq!(value, 9, "pixel {i} must be 9");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_refuses_a_file_that_never_finished_with_wrong_dimensions() {
        let dir = std::env::temp_dir().join(format!(
            "ff-tiff-writer-reopen-mismatch-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mismatch.tif");
        let _ = std::fs::remove_file(&path);
        TiffWriter::create(&path, spec(4, 4, 1)).unwrap();

        match TiffWriter::<File>::reopen(&path, spec(8, 8, 1)) {
            Err(TiffError::ReopenMismatch { field: "width", .. }) => {}
            Ok(_) => panic!("expected ReopenMismatch on width, got Ok"),
            Err(other) => panic!("expected ReopenMismatch on width, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_refuses_an_incomplete_frame_stays_green_after_reopen_additions() {
        // `reopen`, `seed_coverage` and `holds_rows` are additive and do
        // not change `finish`'s existing refusal.
        let mut writer = TiffWriter::new(Cursor::new(Vec::new()), spec(4, 4, 1)).unwrap();
        for y in 0..3u32 {
            writer
                .write_tile(
                    Rect {
                        x: 0,
                        y,
                        width: 4,
                        height: 1,
                    },
                    &[1u16; 4],
                )
                .unwrap();
        }
        match writer.finish() {
            Err(TiffError::IncompleteFrame { row, .. }) => assert_eq!(row, 3),
            other => panic!("expected IncompleteFrame naming row 3, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod strip_writer_tests {
    use std::io::Cursor;

    use super::*;
    use crate::io::tiff::header::TiffHeader;
    use crate::io::tiff::segment::TiffSegmentDecoder;

    fn strip_spec(
        width: u32,
        height: u32,
        spp: u32,
        bits: u16,
        compression: StripCompression,
    ) -> StripSpec {
        StripSpec {
            width,
            height,
            samples_per_pixel: spp,
            bits_per_sample: bits,
            compression,
            description: None,
            metadata: MetadataTags::default(),
        }
    }

    fn gradient_16(width: u32, height: u32, spp: u32) -> Vec<u16> {
        (0..(width * height * spp) as usize)
            .map(|i| (i * 37 % 65_536) as u16)
            .collect()
    }

    fn bytes_16(samples: &[u16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// Feeds `bytes` to `writer` in row counts of 3, 1, 5 rows repeating --
    /// never a multiple of any `rows_per_strip` this test's small images
    /// pick.
    fn write_in_uneven_chunks<W: Write + Seek>(
        writer: &mut StripWriter<W>,
        bytes: &[u8],
        row_bytes: usize,
    ) {
        let pattern = [3usize, 1, 5];
        let mut offset = 0;
        let mut i = 0;
        while offset < bytes.len() {
            let rows = pattern[i % pattern.len()];
            i += 1;
            let take = (rows * row_bytes).min(bytes.len() - offset);
            writer.write_rows(&bytes[offset..offset + take]).unwrap();
            offset += take;
        }
    }

    fn decode_16(bytes: &[u8]) -> (TiffHeader, Vec<u16>) {
        let header = TiffHeader::parse(Path::new("test.tif"), &bytes).expect("must parse");
        let decoder = TiffSegmentDecoder::new(&header, bytes).expect("segments must validate");
        let mut samples = Vec::new();
        for i in 0..decoder.segment_count() {
            samples.extend(decoder.decode(i).unwrap().samples);
        }
        (header, samples)
    }

    #[test]
    fn sixteen_bit_round_trips_losslessly_for_every_compression() {
        for compression in [
            StripCompression::None,
            StripCompression::Lzw,
            StripCompression::Deflate,
        ] {
            for spp in [1u32, 3] {
                let width = 37;
                let height = 23; // taller than one small strip
                let samples = gradient_16(width, height, spp);
                let bytes = bytes_16(&samples);
                let row_bytes = width as usize * spp as usize * 2;

                let mut writer = StripWriter::new(
                    Cursor::new(Vec::new()),
                    strip_spec(width, height, spp, 16, compression),
                )
                .unwrap();
                write_in_uneven_chunks(&mut writer, &bytes, row_bytes);
                let sink = writer.finish().unwrap();
                let written = sink.into_inner();

                let (header, decoded) = decode_16(&written);
                assert_eq!(header.compression, compression.tag());
                assert_eq!(decoded, samples, "spp={spp} compression={compression:?}");
            }
        }
    }

    #[test]
    fn short_final_strip_is_covered() {
        let width = 10;
        let height = 7; // rows_per_strip picked huge for this tiny image,
        // so the whole image is one short final strip.
        let spp = 1;
        let samples = gradient_16(width, height, spp);
        let bytes = bytes_16(&samples);
        let mut writer = StripWriter::new(
            Cursor::new(Vec::new()),
            strip_spec(width, height, spp, 16, StripCompression::None),
        )
        .unwrap();
        writer.write_rows(&bytes).unwrap();
        let sink = writer.finish().unwrap();
        let (_, decoded) = decode_16(&sink.into_inner());
        assert_eq!(decoded, samples);
    }

    #[test]
    fn multiple_strips_round_trip_losslessly() {
        // Wide and tall enough that the 1 MiB strip budget forces several
        // strips, whose count does not divide the height evenly.
        let width = 2000;
        let height = 150;
        let spp = 3;
        let samples = gradient_16(width, height, spp);
        let bytes = bytes_16(&samples);
        let row_bytes = width as usize * spp as usize * 2;

        for compression in [StripCompression::None, StripCompression::Deflate] {
            let mut writer = StripWriter::new(
                Cursor::new(Vec::new()),
                strip_spec(width, height, spp, 16, compression),
            )
            .unwrap();
            write_in_uneven_chunks(&mut writer, &bytes, row_bytes);
            let sink = writer.finish().unwrap();
            let (header, decoded) = decode_16(&sink.into_inner());
            assert!(
                header.strip_offsets.len() > 1,
                "this image must span more than one strip"
            );
            assert_ne!(
                height % header.rows_per_strip,
                0,
                "the strip count must not divide the height evenly"
            );
            assert_eq!(decoded, samples, "compression={compression:?}");
        }
    }

    #[test]
    fn finish_refuses_an_incomplete_image() {
        let width = 4;
        let height = 4;
        let mut writer = StripWriter::new(
            Cursor::new(Vec::new()),
            strip_spec(width, height, 1, 16, StripCompression::None),
        )
        .unwrap();
        writer
            .write_rows(&bytes_16(&gradient_16(width, 2, 1)))
            .unwrap();
        match writer.finish() {
            Err(TiffError::IncompleteFrame { row, .. }) => assert_eq!(row, 2),
            other => panic!("expected IncompleteFrame naming row 2, got {other:?}"),
        }
    }

    #[test]
    fn eight_bit_tags_are_correct_for_every_compression() {
        for compression in [
            StripCompression::None,
            StripCompression::Lzw,
            StripCompression::Deflate,
        ] {
            let width = 9;
            let height = 11;
            let spp = 3;
            let bytes: Vec<u8> = (0..(width * height * spp) as usize)
                .map(|i| (i * 7 % 256) as u8)
                .collect();
            let row_bytes = width as usize * spp as usize;

            let mut writer = StripWriter::new(
                Cursor::new(Vec::new()),
                strip_spec(width, height, spp, 8, compression),
            )
            .unwrap();
            write_in_uneven_chunks(&mut writer, &bytes, row_bytes);
            let sink = writer.finish().unwrap();
            let written = sink.into_inner();
            let written: &[u8] = &written;

            let header = TiffHeader::parse(Path::new("test.tif"), &written).expect("must parse");
            assert_eq!(&*header.bits_per_sample, &[8, 8, 8]);
            assert_eq!(header.compression, compression.tag());
            assert_eq!(header.predictor, compression.predictor());
            assert_eq!(header.width, width);
            assert_eq!(header.height, height);
            assert_eq!(header.samples_per_pixel, spp);
            let expected_strips = height.div_ceil(header.rows_per_strip);
            assert_eq!(header.strip_offsets.len(), expected_strips as usize);
            assert_eq!(header.strip_byte_counts.len(), expected_strips as usize);
        }
    }

    #[test]
    fn finish_pads_the_ifd_offset_to_a_word_boundary() {
        // A single 8-bit, one-sample, odd-width strip leaves an odd byte
        // count, so the uncompressed cursor after the strip lands odd too.
        for (width, height, spp, bits) in [(9u32, 1u32, 1u32, 8u16), (37, 5, 3, 8)] {
            for compression in [
                StripCompression::None,
                StripCompression::Lzw,
                StripCompression::Deflate,
            ] {
                let row_bytes = width as usize * spp as usize * (bits as usize / 8);
                let bytes: Vec<u8> = (0..(row_bytes * height as usize))
                    .map(|i| (i * 7 % 256) as u8)
                    .collect();
                let mut writer = StripWriter::new(
                    Cursor::new(Vec::new()),
                    strip_spec(width, height, spp, bits, compression),
                )
                .unwrap();
                write_in_uneven_chunks(&mut writer, &bytes, row_bytes);
                let sink = writer.finish().unwrap();
                let written = sink.into_inner();
                let ifd_offset = u32::from_le_bytes(written[4..8].try_into().unwrap());
                assert!(
                    ifd_offset.is_multiple_of(2),
                    "compression={compression:?} width={width}: IFD offset {ifd_offset} is odd"
                );
                // Still parses: padding did not corrupt the strip data.
                let written: &[u8] = &written;
                TiffHeader::parse(Path::new("test.tif"), &written).expect("must parse");
            }
        }
    }

    #[test]
    fn finish_with_non_empty_metadata_reads_back_intact_beside_untouched_pixels() {
        let width = 4;
        let height = 3;
        let spp = 1;
        let samples = gradient_16(width, height, spp);
        let bytes = bytes_16(&samples);

        let metadata = MetadataTags {
            ifd0: vec![TagEntry {
                tag: 305, // Software
                field_type: 2,
                count: 13,
                value: b"FocusForge 1\0".to_vec(),
            }],
            exif: vec![TagEntry {
                tag: 33434, // ExposureTime, RATIONAL -- always out-of-line
                field_type: 5,
                count: 1,
                value: [1u32.to_le_bytes(), 200u32.to_le_bytes()].concat(),
            }],
            gps: vec![TagEntry {
                tag: 1, // GPSLatitudeRef
                field_type: 2,
                count: 2,
                value: b"N\0".to_vec(),
            }],
        };
        let mut spec = strip_spec(width, height, spp, 16, StripCompression::None);
        spec.metadata = metadata.clone();

        let mut writer = StripWriter::new(Cursor::new(Vec::new()), spec).unwrap();
        writer.write_rows(&bytes).unwrap();
        let sink = writer.finish().unwrap();
        let written = sink.into_inner();

        let (header, decoded) = decode_16(&written);
        assert_eq!(decoded, samples, "pixels must survive metadata being added");

        let ifd0_offset = u32::from_le_bytes(written[4..8].try_into().unwrap()) as u64;
        let read = crate::io::tiff::header::read_tiff_metadata(
            &written.as_slice(),
            header.byte_order,
            ifd0_offset,
            Path::new("test.tif"),
        );
        assert!(read.warnings.is_empty(), "warnings: {:?}", read.warnings);
        assert!(read.ifd0.contains(&metadata.ifd0[0]));
        assert!(read.exif.contains(&metadata.exif[0]));
        assert!(read.gps.contains(&metadata.gps[0]));
    }

    #[test]
    fn finish_with_empty_metadata_writes_the_same_ifd_as_before_metadata_existed() {
        // A `StripSpec::metadata` left at its default must cost the IFD nothing.
        let width = 5;
        let height = 4;
        let spp = 1;
        let samples = gradient_16(width, height, spp);
        let bytes = bytes_16(&samples);

        let mut writer = StripWriter::new(
            Cursor::new(Vec::new()),
            strip_spec(width, height, spp, 16, StripCompression::None),
        )
        .unwrap();
        writer.write_rows(&bytes).unwrap();
        let sink = writer.finish().unwrap();
        let written = sink.into_inner();

        let written_slice: &[u8] = &written;
        let header = TiffHeader::parse(Path::new("test.tif"), &written_slice).expect("must parse");
        let ifd0_offset = u32::from_le_bytes(written[4..8].try_into().unwrap()) as u64;
        let n_entries =
            u16::from_le_bytes(written[ifd0_offset as usize..][..2].try_into().unwrap());
        assert_eq!(n_entries, 13, "the fixed 13-tag layout, no extras");
        assert_eq!(header.width, width);
        assert_eq!(header.height, height);
    }
}
