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

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::super::error::TiffError;
use super::header::TAG_STRIP_OFFSETS;
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
const TAG_SAMPLE_FORMAT: u16 = 339;

/// What frame a [`TiffWriter`] will produce.
pub struct TiffSpec {
    pub width: u32,
    pub height: u32,
    /// 1 for a Z-Map plane, 3 for RGB.
    pub samples_per_pixel: u32,
    /// `None` -> `"FocusForge <CARGO_PKG_VERSION>"`.
    pub description: Option<String>,
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

        let description =
            description.unwrap_or_else(|| format!("FocusForge {}", env!("CARGO_PKG_VERSION")));
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

    /// `Ok(())` only when every row's merged coverage is exactly
    /// `[(0, width)]`; otherwise `IncompleteFrame` naming the first row that
    /// is short -- a per-row covered-sample *counter* was rejected (Strategy
    /// step 5) because it is green for a double-written region that
    /// compensates a gap elsewhere. Ends with `flush()`; a `File` sink is
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
}
