// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rawler::decoders::{RawDecodeParams, WellKnownIFD};
use rawler::imgop::develop::{Intermediate, RawDevelop};
use rawler::rawsource::RawSource;

use super::grouping::parse_exif_datetime;
use super::{FrameDesc, FrameReader, RawIdentity};
use crate::algorithm::{AlgorithmError, TileReader};
use crate::tile::Rect;

/// Default bound for one developed RAW frame and its decode reservation.
pub const DEFAULT_RAW_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

// rawler's peak holds an original u16 mosaic (2 B/pixel), a scaled clone
// (4 B/pixel), an f32 RGB intermediate (12 B/pixel), and the new f32 RGB
// output while it is copied to retained u16 RGB (12 B/pixel).
const DECODE_PEAK_BYTES_PER_PIXEL: u64 = 30;

/// Decoder facts available before pixels are developed.
#[derive(Debug, Clone, PartialEq)]
pub struct RawMetadata {
    /// Source file.
    pub path: PathBuf,
    /// Camera make reported by the decoder.
    pub make: String,
    /// Camera model reported by the decoder.
    pub model: String,
    /// Camera body serial when the source exposes one.
    pub serial_number: Option<String>,
    /// Original EXIF capture timestamp before it is converted for reporting.
    pub capture_time_original: Option<String>,
    /// Capture time when the decoder can parse one.
    pub capture_time: Option<SystemTime>,
    /// Sensor width reported by the file header.
    pub raw_width: u32,
    /// Sensor height reported by the file header.
    pub raw_height: u32,
    /// Developed output width after the decoder's default crop.
    pub output_width: u32,
    /// Developed output height after the decoder's default crop.
    pub output_height: u32,
    /// X origin of the decoder's default crop in raw sensor coordinates.
    pub crop_x: u32,
    /// Y origin of the decoder's default crop in raw sensor coordinates.
    pub crop_y: u32,
    /// Storage depth when the header exposes it.
    pub storage_bits: Option<u16>,
    /// Decoder-native depth when it differs from storage depth.
    pub decoder_bits: Option<usize>,
    /// Decoder-native black levels in raw code values; DNG values come from its header, while other formats may use a camera table.
    pub black_levels: Option<Vec<f32>>,
    /// Decoder-native white levels in raw code values; DNG values come from its header, while other formats may use a camera table.
    pub white_levels: Option<Vec<u32>>,
    /// CFA channel values in raw sensor order.
    pub cfa: Option<Vec<u8>>,
    /// CFA repeat width.
    pub cfa_width: Option<u16>,
    /// CFA repeat height.
    pub cfa_height: Option<u16>,
    /// Decoder-native XYZ-to-camera row-major matrix; DNG exposes its file matrix and other formats may expose a camera-table matrix.
    pub color_matrix: Option<Vec<f32>>,
    /// The decoder's typed EXIF, the one field set every RAW format exposes
    /// it through (only the DNG decoder surfaces a raw EXIF IFD directly).
    pub exif: rawler::exif::Exif,
}

/// Failures while opening or reading a RAW frame set.
#[derive(Debug, thiserror::Error)]
pub enum RawError {
    /// No paths were supplied for the frame set.
    #[error("no RAW frames were supplied")]
    NoFrames,
    /// The source file could not be opened.
    #[error("{}: RAW I/O failed: {source}", path.display())]
    Io {
        /// File that could not be opened.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// A decoder cannot provide lazy metadata for this file.
    #[error("{}: unsupported RAW reader for {make} {model}: {detail}", path.display())]
    Unsupported {
        /// File that was refused.
        path: PathBuf,
        /// Decoder make, when available.
        make: String,
        /// Decoder model, when available.
        model: String,
        /// Refusal detail.
        detail: String,
    },
    /// A RAW file could not be parsed.
    #[error("{}: RAW decode failed: {detail}", path.display())]
    Decode {
        /// File that failed.
        path: PathBuf,
        /// Decoder error text.
        detail: String,
    },
    /// A decoder panicked while parsing a file.
    #[error("{}: RAW decoder panicked", path.display())]
    Panic {
        /// File that triggered the panic.
        path: PathBuf,
    },
    /// A frame does not share the first frame's dimensions.
    #[error("{}: RAW dimensions {found:?} differ from first frame {first:?}", path.display())]
    FrameMismatch {
        /// File that differs.
        path: PathBuf,
        /// First-frame dimensions.
        first: (u32, u32),
        /// This frame's dimensions.
        found: (u32, u32),
    },
    /// A frame disagrees with frame 0 on a RAW-only layout property.
    #[error("{}: RAW {property} {found} differs from first frame {first}", path.display())]
    MetadataMismatch {
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
    #[error("RAW frame {index} is outside the opened set ({count} frames)")]
    FrameIndex {
        /// Requested frame index.
        index: usize,
        /// Number of opened frames.
        count: usize,
    },
    /// A tile rectangle is outside the developed frame.
    #[error("{}: tile {requested:?} is outside developed frame {frame:?}", path.display())]
    OutOfBounds {
        /// Source file.
        path: PathBuf,
        /// Requested rectangle.
        requested: Rect,
        /// Developed frame dimensions.
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
    /// The cache cannot hold one developed frame and its decode reservation.
    #[error("{}: RAW cache bound {bound} bytes is below required {required} bytes", path.display())]
    CacheTooSmall {
        /// Source file.
        path: PathBuf,
        /// Configured cache capacity.
        bound: u64,
        /// Conservative required reservation.
        required: u64,
    },
    /// RAW geometry overflows a byte count.
    #[error("{}: RAW geometry overflows cache accounting", path.display())]
    BudgetOverflow {
        /// Source file.
        path: PathBuf,
    },
    /// Developed geometry differs from the metadata-only description.
    #[error("{}: developed RAW geometry {developed:?} differs from metadata {metadata:?}", path.display())]
    GeometryChanged {
        /// Source file.
        path: PathBuf,
        /// Geometry reported before development.
        metadata: (u32, u32),
        /// Geometry produced by development.
        developed: (u32, u32),
    },
}

#[derive(Debug)]
struct RawFrame {
    metadata: RawMetadata,
    desc: FrameDesc,
}

#[derive(Debug)]
struct DecodedFrame {
    width: u32,
    height: u32,
    samples: Vec<u16>,
}

#[derive(Debug, Default)]
struct Cache {
    frames: Vec<(usize, Arc<DecodedFrame>, u64)>,
    bytes: u64,
}

fn retained_bytes(frame: &DecodedFrame, path: &Path) -> Result<u64, RawError> {
    u64::try_from(frame.samples.len())
        .ok()
        .and_then(|samples| samples.checked_mul(u64::try_from(std::mem::size_of::<u16>()).ok()?))
        .ok_or_else(|| RawError::BudgetOverflow {
            path: path.to_path_buf(),
        })
}

/// A RAW frame set whose metadata is available before pixel development.
#[derive(Debug)]
pub struct RawStack {
    frames: Vec<RawFrame>,
    cache: Mutex<Cache>,
    decode_lock: Mutex<()>,
    cache_bytes: u64,
}

impl RawStack {
    /// Opens one or more RAW files without reading DNG pixel payloads.
    pub fn open_files(paths: &[PathBuf]) -> Result<Self, RawError> {
        Self::open_files_with_budget(paths, DEFAULT_RAW_CACHE_BYTES)
    }

    /// Opens one or more RAW files with a bound on developed-frame retention.
    pub fn open_files_with_budget(paths: &[PathBuf], cache_bytes: u64) -> Result<Self, RawError> {
        let Some(first_path) = paths.first() else {
            return Err(RawError::NoFrames);
        };
        let first = open_frame(first_path)?;
        let dimensions = (first.desc.width, first.desc.height);
        let mut frames = Vec::with_capacity(paths.len());
        frames.push(first);
        for path in &paths[1..] {
            let frame = open_frame(path)?;
            let found = (frame.desc.width, frame.desc.height);
            if found != dimensions {
                return Err(RawError::FrameMismatch {
                    path: path.clone(),
                    first: dimensions,
                    found,
                });
            }
            let first_metadata = &frames[0].metadata;
            if (frame.metadata.make.as_str(), frame.metadata.model.as_str())
                != (first_metadata.make.as_str(), first_metadata.model.as_str())
            {
                return Err(RawError::MetadataMismatch {
                    path: path.clone(),
                    property: "camera",
                    first: format!("{}/{}", first_metadata.make, first_metadata.model),
                    found: format!("{}/{}", frame.metadata.make, frame.metadata.model),
                });
            }
            if (
                &frame.metadata.cfa,
                frame.metadata.cfa_width,
                frame.metadata.cfa_height,
            ) != (
                &first_metadata.cfa,
                first_metadata.cfa_width,
                first_metadata.cfa_height,
            ) {
                return Err(RawError::MetadataMismatch {
                    path: path.clone(),
                    property: "CFA",
                    first: format!(
                        "{:?}/{}x{}",
                        first_metadata.cfa,
                        first_metadata.cfa_width.unwrap_or(0),
                        first_metadata.cfa_height.unwrap_or(0)
                    ),
                    found: format!(
                        "{:?}/{}x{}",
                        frame.metadata.cfa,
                        frame.metadata.cfa_width.unwrap_or(0),
                        frame.metadata.cfa_height.unwrap_or(0)
                    ),
                });
            }
            frames.push(frame);
        }
        Ok(Self {
            frames,
            cache: Mutex::new(Cache::default()),
            decode_lock: Mutex::new(()),
            cache_bytes,
        })
    }

    /// Frame 0's developed output dimensions after the decoder crop.
    pub fn dimensions(&self) -> (u32, u32) {
        let frame = &self.frames[0].desc;
        (frame.width, frame.height)
    }

    /// Metadata for frame `index`.
    pub fn metadata(&self, index: usize) -> Option<&RawMetadata> {
        self.frames.get(index).map(|frame| &frame.metadata)
    }

    /// Description for frame `index`.
    pub fn frame(&self, index: usize) -> Option<&FrameDesc> {
        self.frames.get(index).map(|frame| &frame.desc)
    }

    fn decoded(&self, index: usize) -> Result<Arc<DecodedFrame>, RawError> {
        if let Some((_, frame, _)) = self
            .cache
            .lock()
            .expect("RAW cache mutex is not poisoned")
            .frames
            .iter()
            .find(|(cached, _, _)| *cached == index)
        {
            return Ok(Arc::clone(frame));
        }
        let frame = self.frames.get(index).ok_or(RawError::FrameIndex {
            index,
            count: self.frames.len(),
        })?;
        let required = reservation_bytes(frame)?;
        if required > self.cache_bytes {
            return Err(RawError::CacheTooSmall {
                path: frame.metadata.path.clone(),
                bound: self.cache_bytes,
                required,
            });
        }
        let mut cache = self.cache.lock().expect("RAW cache mutex is not poisoned");
        while cache.bytes.saturating_add(required) > self.cache_bytes {
            let (_, _, evicted) = cache.frames.remove(0);
            cache.bytes -= evicted;
        }
        drop(cache);
        let decoded = Arc::new(develop(&frame.metadata.path)?);
        if (decoded.width, decoded.height) != (frame.desc.width, frame.desc.height) {
            return Err(RawError::GeometryChanged {
                path: frame.metadata.path.clone(),
                metadata: (frame.desc.width, frame.desc.height),
                developed: (decoded.width, decoded.height),
            });
        }
        let retained = retained_bytes(&decoded, &frame.metadata.path)?;
        let mut cache = self.cache.lock().expect("RAW cache mutex is not poisoned");
        while cache.bytes.saturating_add(retained) > self.cache_bytes {
            let (_, _, evicted) = cache.frames.remove(0);
            cache.bytes -= evicted;
        }
        cache.frames.push((index, Arc::clone(&decoded), retained));
        cache.bytes += retained;
        Ok(decoded)
    }
}

impl FrameReader for RawStack {
    fn len(&self) -> usize {
        self.frames.len()
    }

    fn dimensions(&self) -> (u32, u32) {
        RawStack::dimensions(self)
    }

    fn frame(&self, index: usize) -> &FrameDesc {
        &self.frames[index].desc
    }
}

impl TileReader for RawStack {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let _decode = self
            .decode_lock
            .lock()
            .expect("RAW decode mutex is not poisoned");
        let decoded = self.decoded(frame).map_err(crate::io::FrameError::from)?;
        let path = self.frames[frame].metadata.path.clone();
        let in_bounds = rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|right| right <= decoded.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|bottom| bottom <= decoded.height);
        if !in_bounds {
            return Err(crate::io::FrameError::from(RawError::OutOfBounds {
                path,
                requested: rect,
                frame: (decoded.width, decoded.height),
            })
            .into());
        }
        let expected = rect.width as usize * rect.height as usize * 3;
        if out.len() != expected {
            return Err(crate::io::FrameError::from(RawError::BufferSize {
                path,
                expected,
                got: out.len(),
            })
            .into());
        }
        let stride = decoded.width as usize * 3;
        let row_len = rect.width as usize * 3;
        for (row, source_y) in (rect.y..rect.y + rect.height).enumerate() {
            let source = source_y as usize * stride + rect.x as usize * 3;
            let destination = row * row_len;
            out[destination..destination + row_len]
                .copy_from_slice(&decoded.samples[source..source + row_len]);
        }
        Ok(())
    }
}

fn reservation_bytes(frame: &RawFrame) -> Result<u64, RawError> {
    u64::from(frame.metadata.raw_width)
        .checked_mul(u64::from(frame.metadata.raw_height))
        .and_then(|pixels| pixels.checked_mul(DECODE_PEAK_BYTES_PER_PIXEL))
        .and_then(|decode_and_develop| decode_and_develop.checked_add(frame.desc.file_size))
        .ok_or_else(|| RawError::BudgetOverflow {
            path: frame.metadata.path.clone(),
        })
}

fn develop(path: &Path) -> Result<DecodedFrame, RawError> {
    let source = RawSource::new(path).map_err(|source| RawError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoder = seam(path, || rawler::get_decoder(&source))?;
    let image = seam(path, || {
        decoder.raw_image(&source, &RawDecodeParams::default(), false)
    })?;
    let intermediate = seam(path, || RawDevelop::default().develop_intermediate(&image))?;
    match intermediate {
        Intermediate::ThreeColor(pixels) => Ok(DecodedFrame {
            width: pixels.width as u32,
            height: pixels.height as u32,
            samples: pixels
                .into_flatten()
                .into_iter()
                .map(float_to_u16)
                .collect(),
        }),
        Intermediate::Monochrome(pixels) => Ok(DecodedFrame {
            width: pixels.width as u32,
            height: pixels.height as u32,
            samples: pixels
                .data
                .into_iter()
                .flat_map(|sample| [float_to_u16(sample); 3])
                .collect(),
        }),
        Intermediate::FourColor(_) => Err(RawError::Unsupported {
            path: path.to_path_buf(),
            make: String::new(),
            model: String::new(),
            detail: "development did not produce RGB".to_string(),
        }),
    }
}

fn float_to_u16(sample: f32) -> u16 {
    (sample.clamp(0.0, 1.0) * f32::from(u16::MAX) + 0.5) as u16
}

fn open_frame(path: &Path) -> Result<RawFrame, RawError> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dng"))
    {
        return open_dng(path);
    }
    open_dummy(path)
}

fn open_dng(path: &Path) -> Result<RawFrame, RawError> {
    panic_boundary(path, || open_dng_inner(path))
}

fn open_dng_inner(path: &Path) -> Result<RawFrame, RawError> {
    let source = RawSource::new(path).map_err(|source| RawError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoder = seam(path, || rawler::get_decoder(&source))?;
    let params = RawDecodeParams::default();
    let raw =
        seam(path, || decoder.ifd(WellKnownIFD::Raw))?.ok_or_else(|| RawError::Unsupported {
            path: path.to_path_buf(),
            make: String::new(),
            model: String::new(),
            detail: "decoder exposes no raw header".to_string(),
        })?;
    let width = raw
        .get_entry(256_u16)
        .map(|entry| entry.value.force_u32(0))
        .ok_or_else(|| missing_mandatory_tag(path, "ImageWidth"))?;
    let height = raw
        .get_entry(257_u16)
        .map(|entry| entry.value.force_u32(0))
        .ok_or_else(|| missing_mandatory_tag(path, "ImageLength"))?;
    let general = seam(path, || decoder.raw_metadata(&source, &params))?;
    let capture_time_original = general.exif.date_time_original.clone();
    let capture_time = capture_time_original
        .as_deref()
        .and_then(parse_exif_datetime)
        .and_then(|time| u64::try_from(time.epoch_seconds).ok())
        .and_then(|seconds| {
            std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds))
        });
    let active_area = values_u32(raw.get_entry(50829_u16));
    let (active_x, active_y, active_width, active_height) = match active_area.as_deref() {
        None => (0, 0, width, height),
        Some([top, left, bottom, right]) if bottom >= top && right >= left => {
            (*left, *top, right - left, bottom - top)
        }
        _ => {
            return Err(RawError::Decode {
                path: path.to_path_buf(),
                detail: "invalid ActiveArea".to_string(),
            });
        }
    };
    if active_x.checked_add(active_width) > Some(width)
        || active_y.checked_add(active_height) > Some(height)
    {
        return Err(RawError::Decode {
            path: path.to_path_buf(),
            detail: "ActiveArea is outside image bounds".to_string(),
        });
    }
    let default_crop_x = raw
        .get_entry(50719_u16)
        .map(|entry| entry.value.force_u32(0))
        .unwrap_or(0);
    let default_crop_y = raw
        .get_entry(50719_u16)
        .map(|entry| entry.value.force_u32(1))
        .unwrap_or(0);
    let output_width = raw
        .get_entry(50720_u16)
        .map(|entry| entry.value.force_u32(0))
        .unwrap_or(active_width);
    let output_height = raw
        .get_entry(50720_u16)
        .map(|entry| entry.value.force_u32(1))
        .unwrap_or(active_height);
    let crop_x = active_x
        .checked_add(default_crop_x)
        .ok_or_else(|| RawError::Decode {
            path: path.to_path_buf(),
            detail: "DefaultCropOrigin overflows".to_string(),
        })?;
    let crop_y = active_y
        .checked_add(default_crop_y)
        .ok_or_else(|| RawError::Decode {
            path: path.to_path_buf(),
            detail: "DefaultCropOrigin overflows".to_string(),
        })?;
    if crop_x.checked_add(output_width) > Some(active_x + active_width)
        || crop_y.checked_add(output_height) > Some(active_y + active_height)
    {
        return Err(RawError::Decode {
            path: path.to_path_buf(),
            detail: "DefaultCrop is outside ActiveArea".to_string(),
        });
    }
    let metadata = RawMetadata {
        path: path.to_path_buf(),
        make: general.make,
        model: general.model,
        serial_number: general.exif.serial_number.clone(),
        capture_time_original,
        capture_time,
        raw_width: width,
        raw_height: height,
        output_width,
        output_height,
        crop_x,
        crop_y,
        storage_bits: raw.get_entry(258_u16).map(|entry| entry.value.force_u16(0)),
        decoder_bits: None,
        black_levels: values_f32(raw.get_entry(50714_u16)),
        white_levels: values_u32(raw.get_entry(50717_u16)),
        cfa: raw.get_entry(33422_u16).map(|entry| {
            shift_cfa(
                entry.value.get_data(),
                raw.get_entry(33421_u16)
                    .map(|dimensions| dimensions.value.force_u16(0))
                    .unwrap_or(0),
                raw.get_entry(33421_u16)
                    .map(|dimensions| dimensions.value.force_u16(1))
                    .unwrap_or(0),
                active_x,
                active_y,
            )
        }),
        cfa_width: raw
            .get_entry(33421_u16)
            .map(|entry| entry.value.force_u16(0)),
        cfa_height: raw
            .get_entry(33421_u16)
            .map(|entry| entry.value.force_u16(1)),
        color_matrix: values_f32(raw.get_entry(50721_u16)),
        exif: general.exif,
    };
    let desc = raw_desc(&metadata);
    Ok(RawFrame { metadata, desc })
}

fn open_dummy(path: &Path) -> Result<RawFrame, RawError> {
    panic_boundary(path, || open_dummy_inner(path))
}

fn open_dummy_inner(path: &Path) -> Result<RawFrame, RawError> {
    let source = RawSource::new(path).map_err(|source| RawError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoder = seam(path, || rawler::get_decoder(&source))?;
    let params = RawDecodeParams::default();
    let general = seam(path, || decoder.raw_metadata(&source, &params))?;
    let image = seam(path, || decoder.raw_image(&source, &params, true))?;
    if !matches!(image.data, rawler::RawImageData::Integer(ref data) if data.is_empty())
        && !matches!(image.data, rawler::RawImageData::Float(ref data) if data.is_empty())
    {
        return Err(RawError::Unsupported {
            path: path.to_path_buf(),
            make: general.make,
            model: general.model,
            detail: "dummy decoder retained pixel samples".to_string(),
        });
    }
    let (cfa, cfa_width, cfa_height) = match &image.photometric {
        rawler::rawimage::RawPhotometricInterpretation::Cfa(config) => (
            Some(config.cfa.flat_pattern()),
            Some(config.cfa.width as u16),
            Some(config.cfa.height as u16),
        ),
        _ => (None, None, None),
    };
    let (crop_x, crop_y, output_width, output_height) = image
        .crop_area
        .or(image.active_area)
        .map(|crop| {
            (
                crop.p.x as u32,
                crop.p.y as u32,
                crop.d.w as u32,
                crop.d.h as u32,
            )
        })
        .unwrap_or((0, 0, image.width as u32, image.height as u32));
    let capture_time_original = general.exif.date_time_original.clone();
    let capture_time = capture_time_original
        .as_deref()
        .and_then(parse_exif_datetime)
        .and_then(|time| u64::try_from(time.epoch_seconds).ok())
        .and_then(|seconds| {
            std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds))
        });
    let metadata = RawMetadata {
        path: path.to_path_buf(),
        make: general.make,
        model: general.model,
        serial_number: general.exif.serial_number.clone(),
        capture_time_original,
        capture_time,
        raw_width: image.width as u32,
        raw_height: image.height as u32,
        output_width,
        output_height,
        crop_x,
        crop_y,
        storage_bits: None,
        decoder_bits: Some(image.bps),
        black_levels: Some(image.blacklevel.as_vec()),
        white_levels: Some(image.whitelevel.0.clone()),
        cfa,
        cfa_width,
        cfa_height,
        color_matrix: image
            .color_matrix_find_first([rawler::imgop::xyz::Illuminant::D65])
            .map(|(_, matrix)| matrix),
        exif: general.exif,
    };
    let desc = raw_desc(&metadata);
    Ok(RawFrame { metadata, desc })
}

fn raw_desc(metadata: &RawMetadata) -> FrameDesc {
    let format = metadata
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_else(|| "raw".to_string());
    FrameDesc {
        path: metadata.path.clone(),
        file_size: std::fs::metadata(&metadata.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0),
        image_description: None,
        width: metadata.output_width,
        height: metadata.output_height,
        samples_per_pixel: 3,
        strip_offsets: Vec::new(),
        strip_byte_counts: Vec::new(),
        raw_identity: Some(RawIdentity {
            make: metadata.make.clone(),
            model: metadata.model.clone(),
            serial_number: metadata.serial_number.clone(),
            capture_time_original: metadata.capture_time_original.clone(),
            raw_width: metadata.raw_width,
            raw_height: metadata.raw_height,
            black_levels: metadata.black_levels.clone(),
            white_levels: metadata.white_levels.clone(),
            cfa: metadata.cfa.clone(),
            cfa_width: metadata.cfa_width,
            cfa_height: metadata.cfa_height,
        }),
        layout_label: format!(
            "raw/{format}/output={}x{}",
            metadata.output_width, metadata.output_height
        ),
        layout_signature: vec![
            format!("raw/{format}"),
            format!(
                "output={}x{}",
                metadata.output_width, metadata.output_height
            ),
            format!("camera={}/{}", metadata.make, metadata.model),
            format!(
                "cfa={:?}/{}x{}",
                metadata.cfa,
                metadata.cfa_width.unwrap_or(0),
                metadata.cfa_height.unwrap_or(0)
            ),
        ],
    }
}

fn values_f32(entry: Option<&rawler::formats::tiff::Entry>) -> Option<Vec<f32>> {
    entry.map(|entry| {
        (0..entry.count())
            .map(|index| entry.value.force_f32(index as usize))
            .collect()
    })
}

fn values_u32(entry: Option<&rawler::formats::tiff::Entry>) -> Option<Vec<u32>> {
    entry.map(|entry| {
        (0..entry.count())
            .map(|index| entry.value.force_u32(index as usize))
            .collect()
    })
}

fn shift_cfa(pattern: &[u8], width: u16, height: u16, x: u32, y: u32) -> Vec<u8> {
    let width = usize::from(width);
    let height = usize::from(height);
    if width == 0 || height == 0 || pattern.len() != width * height {
        return pattern.to_vec();
    }
    (0..height)
        .flat_map(|row| {
            (0..width).map(move |column| {
                pattern[((row + y as usize) % height) * width + (column + x as usize) % width]
            })
        })
        .collect()
}

fn panic_boundary<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T, RawError>,
) -> Result<T, RawError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).unwrap_or_else(|_| {
        Err(RawError::Panic {
            path: path.to_path_buf(),
        })
    })
}

fn missing_mandatory_tag(path: &Path, tag: &str) -> RawError {
    RawError::Decode {
        path: path.to_path_buf(),
        detail: format!("missing {tag}"),
    }
}

fn seam<T>(path: &Path, operation: impl FnOnce() -> rawler::Result<T>) -> Result<T, RawError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(rawler::RawlerError::Unsupported {
            what, model, make, ..
        })) if make.is_empty() && model.is_empty() => Err(RawError::Decode {
            path: path.to_path_buf(),
            detail: what,
        }),
        Ok(Err(rawler::RawlerError::Unsupported {
            what, model, make, ..
        })) => Err(RawError::Unsupported {
            path: path.to_path_buf(),
            make,
            model,
            detail: what,
        }),
        Ok(Err(error)) => Err(RawError::Decode {
            path: path.to_path_buf(),
            detail: error.to_string(),
        }),
        Err(_) => Err(RawError::Panic {
            path: path.to_path_buf(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{RawError, panic_boundary};
    use std::path::Path;

    #[test]
    fn caught_decoder_panic_does_not_poison_the_next_open() {
        let path = Path::new("decoder-panic.raw");
        let error = panic_boundary(path, || -> Result<(), RawError> { panic!("decoder panic") })
            .expect_err("decoder panic is named");
        assert!(matches!(error, RawError::Panic { .. }));
        assert!(panic_boundary(path, || Ok(())).is_ok());
    }
}
