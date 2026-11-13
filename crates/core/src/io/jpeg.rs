// SPDX-License-Identifier: GPL-3.0-or-later

//! 8-bit JPEG frames (greyscale or 3-component) widened to the 16-bit sample
//! range. A JPEG has no random access, so a whole frame is decoded once into
//! a bounded cache and tiles are cut from it, with the EXIF Orientation
//! applied per tile.

use std::fs::File;
use std::io::{BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};

use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use super::frame_cache::{CacheFault, DecodedFrame, FrameCache, TileFault, check_tile};
use super::orientation::Orientation;
use super::tiff::header::exif_orientation;
use super::tiff::segment::widen_8bit;
use super::{FrameDesc, FrameError, FrameReader, cache_refusal_message};
use crate::algorithm::{AlgorithmError, TileReader};
use crate::tile::{MemoryBound, Rect};

/// Default bound for decoded JPEG frames and one decode reservation.
const DEFAULT_JPEG_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// Failures while opening or reading a JPEG frame set.
#[derive(Debug, thiserror::Error)]
pub enum JpegError {
    /// No paths were supplied for the frame set.
    #[error("no JPEG frames were supplied")]
    NoFrames,
    /// The source file could not be read.
    #[error("{}: JPEG I/O failed: {source}", path.display())]
    Io {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a well-formed JPEG header.
    #[error("{}: not a readable JPEG: {detail}", path.display())]
    Malformed {
        /// File that was refused.
        path: PathBuf,
        /// What is wrong with it.
        detail: &'static str,
    },
    /// The file uses a JPEG feature this reader does not decode.
    #[error("{}: unsupported JPEG {field} {found}", path.display())]
    Unsupported {
        /// File that was refused.
        path: PathBuf,
        /// Header field that holds the unsupported value.
        field: &'static str,
        /// The value found.
        found: String,
    },
    /// The decoder rejected the file.
    #[error("{}: JPEG decode failed: {detail}", path.display())]
    Decode {
        /// File that failed.
        path: PathBuf,
        /// Decoder error text.
        detail: String,
    },
    /// A frame does not share the first frame's geometry or components.
    #[error("{}: JPEG {property} {found} differs from first frame {first}", path.display())]
    FrameMismatch {
        /// File that differs.
        path: PathBuf,
        /// Property that differs.
        property: &'static str,
        /// First frame value.
        first: String,
        /// This frame value.
        found: String,
    },
    /// A requested frame index is outside the set.
    #[error("JPEG frame {index} is outside the opened set ({count} frames)")]
    FrameIndex {
        /// Requested frame index.
        index: usize,
        /// Number of opened frames.
        count: usize,
    },
    /// A tile rectangle is outside the frame.
    #[error("{}: tile {requested:?} is outside frame {frame:?}", path.display())]
    OutOfBounds {
        /// Source file.
        path: PathBuf,
        /// Requested rectangle.
        requested: Rect,
        /// Upright frame dimensions.
        frame: (u32, u32),
    },
    /// A tile output buffer has the wrong length.
    #[error("{}: tile buffer has {got} samples, expected {expected}", path.display())]
    BufferSize {
        /// Source file.
        path: PathBuf,
        /// Expected sample count.
        expected: usize,
        /// Actual sample count.
        got: usize,
    },
    /// The cache cannot hold one decoded frame and its decode reservation.
    #[error("{}", cache_refusal_message(path, *bound, *required, *bounded_by, true))]
    CacheTooSmall {
        /// Source file.
        path: PathBuf,
        /// Configured cache capacity.
        bound: u64,
        /// Conservative required reservation.
        required: u64,
        /// What limited `bound`.
        bounded_by: MemoryBound,
    },
    /// JPEG geometry overflows a byte count.
    #[error("{}: JPEG geometry overflows cache accounting", path.display())]
    BudgetOverflow {
        /// Source file.
        path: PathBuf,
    },
}

#[derive(Debug)]
struct JpegFrame {
    desc: FrameDesc,
    /// Width and height as stored, before the orientation turns the frame.
    stored: (u32, u32),
    orientation: Orientation,
    /// Peak bytes of one decode: the file, the 8-bit samples and the widened
    /// frame.
    required: u64,
}

/// A JPEG frame set decoded on demand into a bounded cache.
#[derive(Debug)]
pub struct JpegStack {
    frames: Vec<JpegFrame>,
    cache: FrameCache,
    /// What limited the cache capacity.
    cache_bound: MemoryBound,
}

impl JpegStack {
    /// Opens one or more JPEG files, reading their headers only.
    pub fn open_files(paths: &[PathBuf]) -> Result<Self, JpegError> {
        Self::open_files_with_budget(paths, DEFAULT_JPEG_CACHE_BYTES)
    }

    /// Opens one or more JPEG files with a fixed byte limit on decoded-frame
    /// retention.
    pub fn open_files_with_budget(paths: &[PathBuf], cache_bytes: u64) -> Result<Self, JpegError> {
        Self::open_files_bounded(paths, cache_bytes, MemoryBound::Limit { divisor: 1 })
    }

    /// Opens one or more JPEG files; `cache_bound` says what limited
    /// `cache_bytes`, for the refusal text.
    pub(crate) fn open_files_bounded(
        paths: &[PathBuf],
        cache_bytes: u64,
        cache_bound: MemoryBound,
    ) -> Result<Self, JpegError> {
        if paths.is_empty() {
            return Err(JpegError::NoFrames);
        }
        let mut frames: Vec<JpegFrame> = Vec::with_capacity(paths.len());
        for path in paths {
            let frame = open_frame(path)?;
            if let Some(first) = frames.first() {
                let (first_desc, desc) = (&first.desc, &frame.desc);
                let mismatch = |property, first: String, found: String| {
                    Err(JpegError::FrameMismatch {
                        path: path.clone(),
                        property,
                        first,
                        found,
                    })
                };
                if (desc.width, desc.height) != (first_desc.width, first_desc.height) {
                    return mismatch(
                        "dimensions",
                        format!("{}x{}", first_desc.width, first_desc.height),
                        format!("{}x{}", desc.width, desc.height),
                    );
                }
                if desc.samples_per_pixel != first_desc.samples_per_pixel {
                    return mismatch(
                        "components",
                        first_desc.samples_per_pixel.to_string(),
                        desc.samples_per_pixel.to_string(),
                    );
                }
            }
            frames.push(frame);
        }
        Ok(Self {
            frames,
            cache: FrameCache::new(cache_bytes),
            cache_bound,
        })
    }
}

impl FrameReader for JpegStack {
    fn len(&self) -> usize {
        self.frames.len()
    }

    fn dimensions(&self) -> (u32, u32) {
        let desc = &self.frames[0].desc;
        (desc.width, desc.height)
    }

    fn frame(&self, index: usize) -> &FrameDesc {
        &self.frames[index].desc
    }
}

impl TileReader for JpegStack {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        Ok(self.read_tile(frame, rect, out).map_err(FrameError::from)?)
    }

    fn read_scratch_bytes(&self, width: u32, height: u32) -> u64 {
        let oriented = self
            .frames
            .iter()
            .any(|frame| frame.orientation != Orientation::Upright);
        let samples_per_pixel = u64::from(self.frames[0].desc.samples_per_pixel);
        if oriented {
            u64::from(width) * u64::from(height) * samples_per_pixel * 2
        } else {
            0
        }
    }
}

impl JpegStack {
    fn read_tile(&self, index: usize, rect: Rect, out: &mut [u16]) -> Result<(), JpegError> {
        let frame = self.frames.get(index).ok_or(JpegError::FrameIndex {
            index,
            count: self.frames.len(),
        })?;
        let path = &frame.desc.path;
        let spp = frame.desc.samples_per_pixel as usize;
        let tile_fault = |fault| tile_error(path, fault);
        check_tile(rect, (frame.desc.width, frame.desc.height), spp, out.len())
            .map_err(tile_fault)?;
        let decoded = self
            .cache
            .get_or_decode(index, frame.required, || decode(frame))
            .map_err(|fault| match fault {
                CacheFault::Decode(error) => error,
                CacheFault::TooSmall { bound, required } => JpegError::CacheTooSmall {
                    path: path.clone(),
                    bound,
                    required,
                    bounded_by: self.cache_bound,
                },
                CacheFault::Overflow => JpegError::BudgetOverflow { path: path.clone() },
            })?;
        if frame.orientation == Orientation::Upright {
            return decoded.copy_tile(rect, out).map_err(tile_fault);
        }
        let (stored_w, stored_h) = frame.stored;
        let source = frame.orientation.stored_rect(rect, stored_w, stored_h);
        let mut stored = vec![0; source.width as usize * source.height as usize * spp];
        decoded.copy_tile(source, &mut stored).map_err(tile_fault)?;
        frame
            .orientation
            .fill_upright(&stored, rect, frame.stored, spp, out);
        Ok(())
    }
}

fn tile_error(path: &Path, fault: TileFault) -> JpegError {
    match fault {
        TileFault::OutOfBounds { requested, frame } => JpegError::OutOfBounds {
            path: path.to_path_buf(),
            requested,
            frame,
        },
        TileFault::BufferSize { expected, got } => JpegError::BufferSize {
            path: path.to_path_buf(),
            expected,
            got,
        },
    }
}

/// The header facts the reader needs, read without decoding the scan.
struct Header {
    width: u32,
    height: u32,
    components: u8,
    orientation: Orientation,
    /// The first APP1 `Exif` segment's TIFF-structured payload.
    exif: Option<Vec<u8>>,
    file_size: u64,
}

fn open_frame(path: &Path) -> Result<JpegFrame, JpegError> {
    let header = read_header(path)?;
    let components = u64::from(header.components);
    let stored_samples = u64::from(header.width) * u64::from(header.height) * components;
    let required = stored_samples
        .checked_mul(3)
        .and_then(|decode_and_widen| decode_and_widen.checked_add(header.file_size))
        .ok_or_else(|| JpegError::BudgetOverflow {
            path: path.to_path_buf(),
        })?;
    let (width, height) = header.orientation.upright_dims(header.width, header.height);
    let layout_signature = vec![
        "JPEG".to_string(),
        format!("{width}x{height}"),
        "bits=8".to_string(),
        format!("samples={}", header.components),
    ];
    Ok(JpegFrame {
        desc: FrameDesc {
            path: path.to_path_buf(),
            file_size: header.file_size,
            image_description: None,
            orientation: header.orientation.exif(),
            width,
            height,
            samples_per_pixel: u32::from(header.components),
            strip_offsets: Vec::new(),
            strip_byte_counts: Vec::new(),
            raw_identity: None,
            layout_label: "JPEG".to_string(),
            layout_signature,
        },
        stored: (header.width, header.height),
        orientation: header.orientation,
        required,
    })
}

fn read_header(path: &Path) -> Result<Header, JpegError> {
    let io_error = |source| JpegError::Io {
        path: path.to_path_buf(),
        source,
    };
    let malformed = |detail| JpegError::Malformed {
        path: path.to_path_buf(),
        detail,
    };
    let unsupported = |field, found: String| JpegError::Unsupported {
        path: path.to_path_buf(),
        field,
        found,
    };
    let file = File::open(path).map_err(io_error)?;
    let file_size = file.metadata().map_err(io_error)?.len();
    let mut reader = BufReader::new(file);
    let read = |reader: &mut BufReader<File>, buffer: &mut [u8]| {
        reader.read_exact(buffer).map_err(|source| {
            if source.kind() == ErrorKind::UnexpectedEof {
                malformed("the header ends early")
            } else {
                io_error(source)
            }
        })
    };

    let mut start = [0; 2];
    read(&mut reader, &mut start)?;
    if start != [0xff, 0xd8] {
        return Err(malformed("no start-of-image marker"));
    }
    let mut frame_header = None;
    let mut exif = None;
    loop {
        let mut byte = [0];
        read(&mut reader, &mut byte)?;
        if byte[0] != 0xff {
            return Err(malformed("expected a marker"));
        }
        while byte[0] == 0xff {
            read(&mut reader, &mut byte)?;
        }
        let marker = byte[0];
        match marker {
            0x00 => return Err(malformed("stuffed byte outside scan data")),
            0x01 | 0xd0..=0xd8 => continue,
            0xd9 | 0xda => break,
            _ => {}
        }
        let mut length = [0; 2];
        read(&mut reader, &mut length)?;
        let payload_len = usize::from(u16::from_be_bytes(length))
            .checked_sub(2)
            .ok_or_else(|| malformed("segment length below 2"))?;
        let is_frame_header =
            matches!(marker, 0xc0..=0xcf) && ![0xc4, 0xc8, 0xcc].contains(&marker);
        let is_exif_candidate = marker == 0xe1 && exif.is_none() && payload_len >= 6;
        if !(is_frame_header && frame_header.is_none() || is_exif_candidate) {
            reader.seek_relative(payload_len as i64).map_err(io_error)?;
            continue;
        }
        let mut payload = vec![0; payload_len];
        read(&mut reader, &mut payload)?;
        if is_exif_candidate {
            if let Some(blob) = payload.strip_prefix(b"Exif\0\0") {
                exif = Some(blob.to_vec());
            }
        } else {
            frame_header = Some((marker, payload));
        }
    }
    let (marker, payload) =
        frame_header.ok_or_else(|| malformed("no frame header before the scan"))?;
    if payload.len() < 6 {
        return Err(malformed("frame header is too short"));
    }
    if !matches!(marker, 0xc0..=0xc2) {
        return Err(unsupported(
            "coding process",
            format!("SOF{}", marker - 0xc0),
        ));
    }
    let (precision, components) = (payload[0], payload[5]);
    if precision != 8 {
        return Err(unsupported("sample precision", precision.to_string()));
    }
    if !matches!(components, 1 | 3) {
        return Err(unsupported("components", components.to_string()));
    }
    let height = u32::from(u16::from_be_bytes([payload[1], payload[2]]));
    let width = u32::from(u16::from_be_bytes([payload[3], payload[4]]));
    if width == 0 || height == 0 {
        return Err(unsupported("dimensions", format!("{width}x{height}")));
    }
    let orientation = exif
        .as_deref()
        .and_then(exif_orientation)
        .map_or(Orientation::Upright, Orientation::from_exif);
    Ok(Header {
        width,
        height,
        components,
        orientation,
        exif,
        file_size,
    })
}

/// The TIFF-structured payload of a JPEG's first APP1 `Exif` segment, read
/// from the header without decoding the scan; `None` when the file has none.
pub(crate) fn read_exif(path: &Path) -> Result<Option<Vec<u8>>, JpegError> {
    read_header(path).map(|header| header.exif)
}

fn decode(frame: &JpegFrame) -> Result<DecodedFrame, JpegError> {
    let path = &frame.desc.path;
    let decode_error = |detail: String| JpegError::Decode {
        path: path.clone(),
        detail,
    };
    let bytes = std::fs::read(path).map_err(|source| JpegError::Io {
        path: path.clone(),
        source,
    })?;
    let components = frame.desc.samples_per_pixel as usize;
    let colorspace = if components == 1 {
        ColorSpace::Luma
    } else {
        ColorSpace::RGB
    };
    // Strict mode refuses a truncated scan instead of padding it with grey;
    // the cache bound, not the decoder's default size limit, caps the frame.
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(colorspace)
        .set_strict_mode(true)
        .set_max_width(usize::from(u16::MAX))
        .set_max_height(usize::from(u16::MAX));
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&bytes), options);
    let pixels = decoder
        .decode()
        .map_err(|error| decode_error(error.to_string()))?;
    let (width, height) = frame.stored;
    let info = decoder.info().expect("headers are decoded after decode");
    if (u32::from(info.width), u32::from(info.height)) != (width, height)
        || pixels.len() != width as usize * height as usize * components
    {
        return Err(decode_error(
            "decoded geometry differs from the file header".to_string(),
        ));
    }
    Ok(DecodedFrame {
        width,
        height,
        samples_per_pixel: components,
        samples: pixels.into_iter().map(widen_8bit).collect(),
    })
}
