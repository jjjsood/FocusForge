// SPDX-License-Identifier: GPL-3.0-or-later

//! Decode one classic-TIFF strip or tile at a time.

use std::fs;
use std::path::Path;

use flate2::{Decompress, FlushDecompress, Status};
use weezl::{BitOrder, LzwStatus, decode::Decoder};

use crate::io::error::TiffError;
use crate::timing::measurement_scope;

use super::header::TiffHeader;

/// Where a TIFF segment is stored in its image grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentOrigin {
    /// A strip beginning at this image row.
    Strip { y: u32 },
    /// A tile beginning at this image coordinate.
    Tile { x: u32, y: u32 },
}

/// One decoded segment, cropped to pixels inside the image boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedSegment {
    /// Segment location in the image grid.
    pub origin: SegmentOrigin,
    /// Full stored width before edge cropping.
    pub stored_width: u32,
    /// Full stored height before edge cropping.
    pub stored_height: u32,
    /// Width containing image pixels.
    pub valid_width: u32,
    /// Height containing image pixels.
    pub valid_height: u32,
    /// Native-endian chunky samples in row-major order.
    pub samples: Vec<u16>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SegmentInfo {
    pub(crate) origin: SegmentOrigin,
    pub(crate) stored_width: u32,
    pub(crate) stored_height: u32,
    pub(crate) valid_width: u32,
    pub(crate) valid_height: u32,
    offset: u64,
    byte_count: u64,
}

/// Validated view over borrowed TIFF bytes for one-segment decoding.
#[derive(Debug)]
pub struct TiffSegmentDecoder<'a> {
    header: &'a TiffHeader,
    bytes: &'a [u8],
    segments: Vec<SegmentInfo>,
}

impl<'a> TiffSegmentDecoder<'a> {
    /// Validate a parsed TIFF header and its complete file bytes.
    pub fn new(header: &'a TiffHeader, bytes: &'a [u8]) -> Result<Self, TiffError> {
        let segments = segment_layout(header)?;
        Ok(Self {
            header,
            bytes,
            segments,
        })
    }

    /// Number of addressable strips or tiles.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Decode one segment to native-endian samples.
    pub fn decode(&self, index: usize) -> Result<DecodedSegment, TiffError> {
        let info = *self
            .segments
            .get(index)
            .ok_or_else(|| TiffError::SegmentIndex {
                path: self.header.path.clone(),
                index,
                count: self.segments.len(),
            })?;
        decode_segment(self.header, self.bytes, info, index)
    }
}

/// File-backed convenience wrapper around [`TiffSegmentDecoder`].
#[derive(Debug)]
pub struct TiffSegmentReader {
    header: TiffHeader,
    bytes: Box<[u8]>,
    segments: Vec<SegmentInfo>,
}

impl TiffSegmentReader {
    /// Open and validate a classic TIFF file for one-segment decoding.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TiffError> {
        let header = TiffHeader::open(path.as_ref())?;
        let bytes = fs::read(path.as_ref()).map_err(|source| TiffError::Io {
            path: path.as_ref().to_path_buf(),
            source,
        })?;
        let segments = segment_layout(&header)?;
        Ok(Self {
            header,
            bytes: bytes.into_boxed_slice(),
            segments,
        })
    }

    /// Borrow the parsed header.
    pub fn header(&self) -> &TiffHeader {
        &self.header
    }

    /// Number of addressable strips or tiles.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Decode one segment to native-endian samples.
    pub fn decode(&self, index: usize) -> Result<DecodedSegment, TiffError> {
        TiffSegmentDecoder::new(&self.header, &self.bytes)?.decode(index)
    }
}

pub(crate) fn segment_layout(header: &TiffHeader) -> Result<Vec<SegmentInfo>, TiffError> {
    validate_class(header)?;
    let tiled = header.tile_width.is_some();
    let (offsets, counts, geometry) = if let Some(width) = header.tile_width {
        let height = header
            .tile_length
            .ok_or_else(|| layout_error(header, "missing TileLength"))?;
        if width == 0 || height == 0 {
            return Err(layout_error(header, "tile dimensions must be nonzero"));
        }
        let columns = header.width.div_ceil(width);
        let rows = header.height.div_ceil(height);
        let expected = usize::try_from(
            u64::from(columns)
                .checked_mul(u64::from(rows))
                .ok_or_else(|| layout_error(header, "tile count overflows"))?,
        )
        .map_err(|_| layout_error(header, "tile count overflows usize"))?;
        (
            &header.tile_offsets,
            &header.tile_byte_counts,
            (width, height, columns, expected),
        )
    } else {
        if header.rows_per_strip == 0 {
            return Err(layout_error(header, "RowsPerStrip must be nonzero"));
        }
        let rows = header.height.div_ceil(header.rows_per_strip);
        let expected = usize::try_from(u64::from(rows))
            .map_err(|_| layout_error(header, "strip count overflows usize"))?;
        (
            &header.strip_offsets,
            &header.strip_byte_counts,
            (header.width, header.rows_per_strip, 1, expected),
        )
    };
    if offsets.len() != geometry.3 || counts.len() != geometry.3 {
        return Err(layout_error(
            header,
            "offset and byte-count tables must match geometry",
        ));
    }

    let mut segments = Vec::with_capacity(geometry.3);
    for segment_index in 0..geometry.3 {
        let index = u32::try_from(segment_index)
            .map_err(|_| layout_error(header, "segment index overflows u32"))?;
        let (x, y) = if tiled {
            let col = index % geometry.2;
            let row = index / geometry.2;
            (
                col.checked_mul(geometry.0)
                    .ok_or_else(|| layout_error(header, "tile x coordinate overflows"))?,
                row.checked_mul(geometry.1)
                    .ok_or_else(|| layout_error(header, "tile y coordinate overflows"))?,
            )
        } else {
            (
                0,
                index
                    .checked_mul(geometry.1)
                    .ok_or_else(|| layout_error(header, "strip y coordinate overflows"))?,
            )
        };
        let valid_width = header.width.saturating_sub(x).min(geometry.0);
        let valid_height = header.height.saturating_sub(y).min(geometry.1);
        if valid_width == 0 || valid_height == 0 {
            return Err(layout_error(header, "segment lies outside image bounds"));
        }
        segments.push(SegmentInfo {
            origin: if tiled {
                SegmentOrigin::Tile { x, y }
            } else {
                SegmentOrigin::Strip { y }
            },
            stored_width: geometry.0,
            stored_height: if tiled {
                geometry.1
            } else {
                geometry.1.min(header.height.saturating_sub(y))
            },
            valid_width,
            valid_height,
            offset: offsets[segment_index],
            byte_count: counts[segment_index],
        });
    }
    Ok(segments)
}

pub(crate) fn validate_decoded_header(header: &TiffHeader) -> Result<(), TiffError> {
    segment_layout(header).map(|_| ())
}

/// Decode one pre-validated segment from borrowed file bytes.
pub(crate) fn decode_segment(
    header: &TiffHeader,
    bytes: &[u8],
    info: SegmentInfo,
    index: usize,
) -> Result<DecodedSegment, TiffError> {
    let _scope = measurement_scope("tiff_segment_decode");
    let start = usize::try_from(info.offset).map_err(|_| TiffError::SegmentPastEof {
        path: header.path.clone(),
        index,
    })?;
    let count = usize::try_from(info.byte_count).map_err(|_| TiffError::SegmentPastEof {
        path: header.path.clone(),
        index,
    })?;
    let end = start
        .checked_add(count)
        .ok_or_else(|| TiffError::SegmentPastEof {
            path: header.path.clone(),
            index,
        })?;
    let stored = bytes
        .get(start..end)
        .ok_or_else(|| TiffError::SegmentPastEof {
            path: header.path.clone(),
            index,
        })?;
    let expected = sample_bytes(header, info.stored_width, info.stored_height)?;
    let raw = decode_bytes(header, stored, expected, index)?;
    let samples = samples_and_crop(header, &raw, info)?;
    Ok(DecodedSegment {
        origin: info.origin,
        stored_width: info.stored_width,
        stored_height: info.stored_height,
        valid_width: info.valid_width,
        valid_height: info.valid_height,
        samples,
    })
}

fn validate_class(header: &TiffHeader) -> Result<(), TiffError> {
    let unsupported = |field, value: String| TiffError::UnsupportedDecoded {
        path: header.path.clone(),
        field,
        value,
    };
    if !matches!(header.compression, 1 | 5 | 8 | 32946) {
        return Err(unsupported("Compression", header.compression.to_string()));
    }
    if !matches!(header.predictor, 1 | 2) {
        return Err(unsupported("Predictor", header.predictor.to_string()));
    }
    if header.planar_config != 1 {
        return Err(unsupported(
            "PlanarConfiguration",
            header.planar_config.to_string(),
        ));
    }
    if header.bits_per_sample.iter().any(|&value| value != 16) {
        return Err(unsupported(
            "BitsPerSample",
            format!("{:?}", header.bits_per_sample),
        ));
    }
    if header.sample_format.iter().any(|&value| value != 1) {
        return Err(unsupported(
            "SampleFormat",
            format!("{:?}", header.sample_format),
        ));
    }
    if header.width == 0 || header.height == 0 || header.samples_per_pixel == 0 {
        return Err(layout_error(
            header,
            "image dimensions and samples per pixel must be nonzero",
        ));
    }
    if header.bits_per_sample.len() != header.samples_per_pixel as usize
        || header.sample_format.len() != header.samples_per_pixel as usize
    {
        return Err(layout_error(
            header,
            "sample vectors must match SamplesPerPixel",
        ));
    }
    Ok(())
}

fn sample_bytes(header: &TiffHeader, width: u32, height: u32) -> Result<usize, TiffError> {
    usize::try_from(
        u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|value| value.checked_mul(u64::from(header.samples_per_pixel)))
            .and_then(|value| value.checked_mul(2))
            .ok_or_else(|| layout_error(header, "segment byte length overflows"))?,
    )
    .map_err(|_| layout_error(header, "segment byte length overflows usize"))
}

fn decode_bytes(
    header: &TiffHeader,
    stored: &[u8],
    expected: usize,
    index: usize,
) -> Result<Vec<u8>, TiffError> {
    if header.compression == 1 {
        if stored.len() != expected {
            return Err(TiffError::SegmentLength {
                path: header.path.clone(),
                index,
                got: stored.len(),
                expected,
            });
        }
        return Ok(stored.to_vec());
    }
    let output_len = expected
        .checked_add(1)
        .ok_or_else(|| layout_error(header, "segment output length overflows"))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(output_len)
        .map_err(|_| layout_error(header, "segment output allocation is too large"))?;
    output.resize(output_len, 0);
    let written = match header.compression {
        5 => {
            let mut decoder = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
            let mut input = stored;
            let mut out_offset = 0;
            loop {
                let result = decoder.decode_bytes(input, &mut output[out_offset..]);
                input = &input[result.consumed_in..];
                out_offset += result.consumed_out;
                if out_offset == output.len() && !matches!(result.status, Ok(LzwStatus::Done)) {
                    return Err(TiffError::SegmentLength {
                        path: header.path.clone(),
                        index,
                        got: out_offset,
                        expected,
                    });
                }
                match result.status {
                    Ok(LzwStatus::Done) => break out_offset,
                    Ok(LzwStatus::Ok) if result.consumed_in != 0 || result.consumed_out != 0 => {}
                    _ => {
                        return Err(TiffError::SegmentStream {
                            path: header.path.clone(),
                            index,
                        });
                    }
                }
            }
        }
        8 | 32946 => {
            let mut decoder = Decompress::new(true);
            let status = decoder
                .decompress(stored, &mut output, FlushDecompress::Finish)
                .map_err(|_| TiffError::SegmentStream {
                    path: header.path.clone(),
                    index,
                })?;
            let written = decoder.total_out() as usize;
            if written == output.len() && status != Status::StreamEnd {
                return Err(TiffError::SegmentLength {
                    path: header.path.clone(),
                    index,
                    got: written,
                    expected,
                });
            }
            if status != Status::StreamEnd {
                return Err(TiffError::SegmentStream {
                    path: header.path.clone(),
                    index,
                });
            }
            written
        }
        _ => unreachable!("validated compression"),
    };
    if written != expected {
        return Err(TiffError::SegmentLength {
            path: header.path.clone(),
            index,
            got: written,
            expected,
        });
    }
    output.truncate(written);
    Ok(output)
}

fn samples_and_crop(
    header: &TiffHeader,
    raw: &[u8],
    info: SegmentInfo,
) -> Result<Vec<u16>, TiffError> {
    let spp = header.samples_per_pixel as usize;
    let stored_width = info.stored_width as usize;
    let mut decoded = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks_exact(2) {
        decoded.push(header.byte_order.u16_from([pair[0], pair[1]]));
    }
    if header.predictor == 2 {
        for row in decoded.chunks_exact_mut(stored_width * spp) {
            for sample in spp..row.len() {
                row[sample] = row[sample].wrapping_add(row[sample - spp]);
            }
        }
    }
    let mut cropped =
        Vec::with_capacity(info.valid_width as usize * info.valid_height as usize * spp);
    for row in decoded
        .chunks_exact(stored_width * spp)
        .take(info.valid_height as usize)
    {
        cropped.extend_from_slice(&row[..info.valid_width as usize * spp]);
    }
    Ok(cropped)
}

fn layout_error(header: &TiffHeader, detail: &'static str) -> TiffError {
    TiffError::SegmentLayout {
        path: header.path.clone(),
        detail,
    }
}
