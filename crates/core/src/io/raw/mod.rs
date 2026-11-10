// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use half::slice::HalfFloatSliceExt;
use rawler::decoders::{RawDecodeParams, WellKnownIFD};
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::imgop::xyz::Illuminant;
use rawler::rawimage::RawImage;
use rawler::rawsource::RawSource;

use super::frame_cache::{CacheFault, CachedFrame, FrameCache, TileFault, check_tile};
use super::grouping::parse_exif_datetime;
use super::{DevelopStore, FrameDesc, FrameError, FrameReader, RawIdentity, cache_refusal_message};
use crate::algorithm::{AlgorithmError, TileReader};
use crate::equalize::UNIT_GAIN;
use crate::tile::budget::DECODED_CACHE_SHARE_DIVISOR;
use crate::tile::{HostBudget, MemoryBound, Rect, Sample, SampleFormat, ScratchDir, TileError};

mod look;
pub use look::LookTransform;
use look::encode_f32;

/// Default bound for one developed RAW frame and its decode reservation.
pub const DEFAULT_RAW_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

// rawler's peak holds an original u16 mosaic (2 B/pixel), a scaled clone
// (4 B/pixel), the demosaiced f32 RGB (12 B/pixel) and its default-cropped
// copy (12 B/pixel). The cropped pixels are then moved, not copied, into the
// retained frame.
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
    /// RAW geometry overflows a byte count.
    #[error("{}: RAW geometry overflows a byte count", path.display())]
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
    /// A developed-frame scratch directory or file could not be created,
    /// written or read.
    #[error("{}: RAW scratch I/O failed: {source}", path.display())]
    Scratch {
        /// Directory or file that failed.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
struct RawFrame {
    metadata: RawMetadata,
    desc: FrameDesc,
}

#[derive(Debug)]
struct LinearFrame {
    width: u32,
    height: u32,
    /// Camera-linear RGB: black and white levels applied, no white balance,
    /// colour matrix, curve or clip.
    samples: Vec<[f32; 3]>,
    /// Each channel's mean over the frame.
    means: [f64; 3],
    /// Grey replicated to three channels, so no colour transform applies.
    monochrome: bool,
}

impl CachedFrame for LinearFrame {
    fn retained_bytes(&self) -> Option<u64> {
        u64::try_from(self.samples.len())
            .ok()?
            .checked_mul(std::mem::size_of::<[f32; 3]>() as u64)
    }
}

/// Bytes one developed pixel takes in a scratch file: three half-precision
/// channels.
const PIXEL_BYTES: usize = std::mem::size_of::<[half::f16; 3]>();

/// Side of the square pixel blocks a developed frame is stored in.
const SCRATCH_BLOCK: usize = 64;

/// Pixels per `write` call when a developed frame goes to scratch.
const WRITE_PIXELS: usize = 1 << 16;

/// A developed frame's bytes: the path of its scratch file, read with
/// positioned reads, or an owned buffer.
#[derive(Debug)]
enum FrameBytes {
    File(PathBuf),
    Memory(Box<[u8]>),
}

/// A developed frame: its `width * height` samples as native-endian
/// half-precision `[f16; 3]`, no header, in `SCRATCH_BLOCK`-square blocks.
/// Blocks follow in row-major order; inside a block the samples are
/// row-major. Blocks at the right and bottom edges are ragged, so a block `bw`
/// wide and `bh` high holds `bw * bh` pixels. The same bytes sit in the
/// scratch file and in the in-memory buffer.
#[derive(Debug)]
struct StoredFrame {
    bytes: FrameBytes,
    width: usize,
    height: usize,
    /// Each channel's mean over the frame.
    means: [f64; 3],
    /// Grey replicated to three channels, so no colour transform applies.
    monochrome: bool,
}

impl StoredFrame {
    /// Calls `visit(row, column, bytes)` for every run of `rect` that lies in
    /// one block row of one block: `row` and `column` locate the run inside
    /// `rect`, `bytes` are its pixels. A file-backed frame is opened once and
    /// each touched block is one positioned read into one reused buffer.
    fn read_runs(
        &self,
        rect: Rect,
        mut visit: impl FnMut(usize, usize, &[u8]),
    ) -> Result<(), RawError> {
        let scratch = |path: &Path| {
            let path = path.to_path_buf();
            move |source| RawError::Scratch { path, source }
        };
        let mut buffer = Vec::new();
        let file = match &self.bytes {
            FrameBytes::File(path) => {
                buffer.resize(read_buffer_bytes(self.width, self.height), 0);
                Some(File::open(path).map_err(scratch(path))?)
            }
            FrameBytes::Memory(_) => None,
        };
        let (x0, y0) = (rect.x as usize, rect.y as usize);
        for (y, rows) in block_segments(y0, rect.height as usize) {
            let block_y = y / SCRATCH_BLOCK * SCRATCH_BLOCK;
            let block_height = SCRATCH_BLOCK.min(self.height - block_y);
            for (x, len) in block_segments(x0, rect.width as usize) {
                let block_x = x / SCRATCH_BLOCK * SCRATCH_BLOCK;
                let block_width = SCRATCH_BLOCK.min(self.width - block_x);
                let first =
                    block_y * self.width + block_x * block_height + (y - block_y) * block_width;
                let span = first * PIXEL_BYTES..(first + rows * block_width) * PIXEL_BYTES;
                let bytes = match (&self.bytes, &file) {
                    (FrameBytes::Memory(memory), _) => &memory[span],
                    (FrameBytes::File(path), Some(file)) => {
                        let bytes = &mut buffer[..span.len()];
                        read_exact_at(file, bytes, span.start as u64).map_err(scratch(path))?;
                        &*bytes
                    }
                    (FrameBytes::File(_), None) => unreachable!("a file-backed frame is open"),
                };
                for row in 0..rows {
                    let start = (row * block_width + x - block_x) * PIXEL_BYTES;
                    visit(
                        y + row - y0,
                        x - x0,
                        &bytes[start..start + len * PIXEL_BYTES],
                    );
                }
            }
        }
        Ok(())
    }
}

/// Bytes of the buffer one read of a `width` x `height` file-backed frame
/// needs: one full block, clamped to the frame.
fn read_buffer_bytes(width: usize, height: usize) -> usize {
    SCRATCH_BLOCK.min(width) * SCRATCH_BLOCK.min(height) * PIXEL_BYTES
}

/// Widens native-endian half-precision `bytes` into `out`, one `f32` per
/// value. `halves` is scratch, reused across calls.
fn decode_pixels(bytes: &[u8], halves: &mut Vec<half::f16>, out: &mut Vec<f32>) {
    halves.clear();
    halves.extend(
        bytes
            .chunks_exact(2)
            .map(|pair| half::f16::from_ne_bytes([pair[0], pair[1]])),
    );
    out.resize(halves.len(), 0.0);
    halves.convert_to_f32_slice(out);
}

/// Fills `buf` from `file` at `offset`; a file that ends first is
/// `UnexpectedEof`.
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Fills `buf` from `file` at `offset`; a file that ends first is
/// `UnexpectedEof`.
#[cfg(not(unix))]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::io::ErrorKind;
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                buf = &mut buf[read..];
                offset += read as u64;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Every frame developed once. The directory that holds the scratch files
/// outlives the frames that name them.
#[derive(Debug)]
struct DevelopedFrames {
    frames: Vec<StoredFrame>,
    /// The scratch directory; `None` when the frames are in memory.
    dir: Option<ScratchDir>,
}

/// Where a frame's developed samples are read from.
enum Developed<'a> {
    Cached(Arc<LinearFrame>),
    Stored(&'a StoredFrame),
}

impl Developed<'_> {
    fn monochrome(&self) -> bool {
        match self {
            Self::Cached(frame) => frame.monochrome,
            Self::Stored(frame) => frame.monochrome,
        }
    }

    fn means(&self) -> [f64; 3] {
        match self {
            Self::Cached(frame) => frame.means,
            Self::Stored(frame) => frame.means,
        }
    }
}

/// A RAW frame set whose metadata is available before pixel development.
#[derive(Debug)]
pub struct RawStack {
    frames: Vec<RawFrame>,
    cache: FrameCache<LinearFrame>,
    /// What limited the cache capacity.
    cache_bound: MemoryBound,
    /// Develops started per frame, counting failed ones.
    develops: Vec<AtomicU64>,
    look: OnceLock<LookTransform>,
    /// Set by [`FrameReader::develop_once`]; reads are served from it.
    scratch: OnceLock<DevelopedFrames>,
}

impl RawStack {
    /// Opens one or more RAW files without reading DNG pixel payloads.
    pub fn open_files(paths: &[PathBuf]) -> Result<Self, RawError> {
        Self::open_files_with_budget(paths, DEFAULT_RAW_CACHE_BYTES)
    }

    /// Opens one or more RAW files with a fixed byte limit on developed-frame
    /// retention.
    pub fn open_files_with_budget(paths: &[PathBuf], cache_bytes: u64) -> Result<Self, RawError> {
        Ok(Self::from_frames(
            open_frames(paths)?,
            cache_bytes,
            MemoryBound::Limit { divisor: 1 },
        ))
    }

    /// Opens one or more RAW files within `budget`. The developed-frame cache
    /// takes a quarter of it, but never less than the largest frame's decode
    /// reservation; the second value is the budget left for tile scheduling.
    /// A budget below that reservation is refused.
    pub(crate) fn open_files_for_budget(
        paths: &[PathBuf],
        budget: HostBudget,
    ) -> Result<(Self, HostBudget), RawError> {
        let frames = open_frames(paths)?;
        let (largest, required) = largest_reservation(&frames)?;
        if budget.bytes() < required {
            return Err(RawError::CacheTooSmall {
                path: largest.metadata.path.clone(),
                bound: budget.bytes(),
                required,
                bounded_by: budget.memory_bound(),
            });
        }
        let cache_bytes = (budget.bytes() / DECODED_CACHE_SHARE_DIVISOR).max(required);
        Ok((
            Self::from_frames(frames, cache_bytes, budget.memory_bound()),
            budget.reduced_by(cache_bytes),
        ))
    }

    fn from_frames(frames: Vec<RawFrame>, cache_bytes: u64, cache_bound: MemoryBound) -> Self {
        Self {
            develops: frames.iter().map(|_| AtomicU64::new(0)).collect(),
            frames,
            cache: FrameCache::new(cache_bytes),
            cache_bound,
            look: OnceLock::new(),
            scratch: OnceLock::new(),
        }
    }

    /// Frame 0's developed output dimensions after the decoder crop.
    pub fn dimensions(&self) -> (u32, u32) {
        let frame = &self.frames[0].desc;
        (frame.width, frame.height)
    }

    /// Bytes one developed frame occupies in its scratch file.
    pub fn scratch_frame_bytes(&self) -> Result<u64, RawError> {
        self.scratch_total(1)
    }

    fn scratch_total(&self, frames: usize) -> Result<u64, RawError> {
        let (width, height) = self.dimensions();
        scratch_bytes(frames, width, height).ok_or_else(|| RawError::BudgetOverflow {
            path: self.frames[0].metadata.path.clone(),
        })
    }

    /// Metadata for frame `index`.
    pub fn metadata(&self, index: usize) -> Option<&RawMetadata> {
        self.frames.get(index).map(|frame| &frame.metadata)
    }

    /// Description for frame `index`.
    pub fn frame(&self, index: usize) -> Option<&FrameDesc> {
        self.frames.get(index).map(|frame| &frame.desc)
    }

    /// The look transform of frame 0, shared by every frame of the set.
    fn look(&self) -> Result<&LookTransform, RawError> {
        if let Some(look) = self.look.get() {
            return Ok(look);
        }
        let look = reference_look(&self.frames[0].metadata.path)?;
        Ok(self.look.get_or_init(|| look))
    }

    /// Frame `index`, developed once and cached; developments are serialised.
    fn decoded(&self, index: usize) -> Result<Arc<LinearFrame>, RawError> {
        let frame = self.frames.get(index).ok_or(RawError::FrameIndex {
            index,
            count: self.frames.len(),
        })?;
        let path = &frame.metadata.path;
        let required = reservation_bytes(frame)?;
        self.cache
            .get_or_decode(index, required, || self.develop_frame(index))
            .map_err(|fault| match fault {
                CacheFault::Decode(error) => error,
                CacheFault::TooSmall { bound, required } => RawError::CacheTooSmall {
                    path: path.clone(),
                    bound,
                    required,
                    bounded_by: self.cache_bound,
                },
                CacheFault::Overflow => RawError::BudgetOverflow { path: path.clone() },
            })
    }

    /// One full-frame develop of frame `index`, counted. `index` is in range.
    fn develop_frame(&self, index: usize) -> Result<LinearFrame, RawError> {
        let frame = &self.frames[index];
        let path = &frame.metadata.path;
        self.develops[index].fetch_add(1, Ordering::Relaxed);
        // The decoder runs its own rayon loops. On a pool worker, waiting for
        // them could pick up another reader's job, which blocks on the decode
        // lock this call holds; a plain thread waits on the global pool
        // without taking any work.
        let developed = std::thread::scope(|scope| {
            scope
                .spawn(|| develop(path))
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })?;
        if (developed.width, developed.height) != (frame.desc.width, frame.desc.height) {
            return Err(RawError::GeometryChanged {
                path: path.clone(),
                metadata: (frame.desc.width, frame.desc.height),
                developed: (developed.width, developed.height),
            });
        }
        Ok(developed)
    }

    /// Frame `index`'s developed samples: its stored frame when the pass has
    /// run, else the cache.
    fn developed(&self, index: usize) -> Result<Developed<'_>, RawError> {
        match self.scratch.get() {
            Some(scratch) => {
                scratch
                    .frames
                    .get(index)
                    .map(Developed::Stored)
                    .ok_or(RawError::FrameIndex {
                        index,
                        count: self.frames.len(),
                    })
            }
            None => self.decoded(index).map(Developed::Cached),
        }
    }

    /// Develops frame `index` into `<dir>/<index>.f16`, or into an owned
    /// buffer when `dir` is `None`.
    fn develop_to_store(&self, index: usize, dir: Option<&Path>) -> Result<StoredFrame, RawError> {
        let developed = self.develop_frame(index)?;
        let (width, height) = {
            let desc = &self.frames[index].desc;
            (desc.width as usize, desc.height as usize)
        };
        let bytes = match dir {
            Some(dir) => {
                let path = dir.join(format!("{index}.f16"));
                let fault = |source| RawError::Scratch {
                    path: path.clone(),
                    source,
                };
                let mut file = File::create(&path).map_err(fault)?;
                write_samples(&mut file, &developed.samples, width).map_err(fault)?;
                drop(file);
                FrameBytes::File(path)
            }
            None => {
                let mut buffer = Vec::with_capacity(width * height * PIXEL_BYTES);
                write_samples(&mut buffer, &developed.samples, width)
                    .expect("writing to a vector cannot fail");
                FrameBytes::Memory(buffer.into_boxed_slice())
            }
        };
        Ok(StoredFrame {
            bytes,
            width,
            height,
            means: developed.means,
            monochrome: developed.monochrome,
        })
    }
}

/// Writes `samples`, `width` pixels per row, to `out` as native-endian
/// `f16`s, each rounded to nearest, in `SCRATCH_BLOCK`-square blocks: block
/// rows top to bottom, blocks left to right, each block's rows in order.
/// Blocks at the right and bottom edges are ragged, not padded.
fn write_samples(out: &mut impl Write, samples: &[[f32; 3]], width: usize) -> std::io::Result<()> {
    let mut chunk = Vec::with_capacity(WRITE_PIXELS * PIXEL_BYTES);
    for band in samples.chunks(width * SCRATCH_BLOCK) {
        for first in (0..width).step_by(SCRATCH_BLOCK) {
            let last = (first + SCRATCH_BLOCK).min(width);
            for row in band.chunks_exact(width) {
                chunk.extend(
                    row[first..last]
                        .iter()
                        .flatten()
                        .flat_map(|value| half::f16::from_f32(*value).to_ne_bytes()),
                );
                if chunk.len() >= WRITE_PIXELS * PIXEL_BYTES {
                    out.write_all(&chunk)?;
                    chunk.clear();
                }
            }
        }
    }
    out.write_all(&chunk)
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

    fn develop_scratch_bytes(&self) -> Result<u64, AlgorithmError> {
        Ok(self
            .scratch_total(self.frames.len())
            .map_err(FrameError::from)?)
    }

    fn develop_store(
        &self,
        budget: HostBudget,
        workers: usize,
        tile_share: u64,
    ) -> Result<Option<DevelopStore>, AlgorithmError> {
        let (_, required) = largest_reservation(&self.frames).map_err(FrameError::from)?;
        let threads = develop_threads(workers, budget.bytes(), required, self.frames.len());
        let frames = self
            .scratch_total(self.frames.len())
            .map_err(FrameError::from)?;
        let peak = (threads as u64).saturating_mul(required).max(tile_share);
        Ok(Some(
            if frames
                .checked_add(peak)
                .is_some_and(|needed| needed <= budget.bytes())
            {
                DevelopStore::Memory
            } else {
                DevelopStore::ScratchFile
            },
        ))
    }

    fn develop_counts(&self) -> Option<Vec<u64>> {
        Some(
            self.develops
                .iter()
                .map(|count| count.load(Ordering::Relaxed))
                .collect(),
        )
    }

    /// Develops every frame once, in parallel, into `store`: scratch files in
    /// a directory under `scratch_root` that lives as long as this reader, or
    /// buffers owned by it. At most `workers` develops overlap,
    /// and no more than `budget` holds decode reservations for. `developed`
    /// gains one as each frame lands in its store.
    fn develop_once(
        &self,
        scratch_root: &Path,
        store: DevelopStore,
        budget: HostBudget,
        workers: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
        developed: &AtomicUsize,
    ) -> Result<usize, AlgorithmError> {
        if self.scratch.get().is_some() {
            return Ok(0);
        }
        let (frame, required) = largest_reservation(&self.frames).map_err(FrameError::from)?;
        let budget_bytes = budget.bytes();
        if budget_bytes < required {
            return Err(FrameError::from(RawError::CacheTooSmall {
                path: frame.metadata.path.clone(),
                bound: budget_bytes,
                required,
                bounded_by: budget.memory_bound(),
            })
            .into());
        }
        let threads = develop_threads(workers, budget_bytes, required, self.frames.len());

        let dir = match store {
            DevelopStore::Memory => None,
            DevelopStore::ScratchFile => Some(
                ScratchDir::create_in(scratch_root)
                    .map_err(|error| match error {
                        TileError::Scratch { path, source } => RawError::Scratch { path, source },
                        other => RawError::Scratch {
                            path: scratch_root.to_path_buf(),
                            source: std::io::Error::other(other),
                        },
                    })
                    .map_err(FrameError::from)?,
            ),
        };
        let slots: Vec<OnceLock<StoredFrame>> =
            self.frames.iter().map(|_| OnceLock::new()).collect();
        let next = AtomicUsize::new(0);
        let failure = Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= slots.len() {
                            break;
                        }
                        let stored = if cancelled() {
                            Err(AlgorithmError::Cancelled)
                        } else {
                            self.develop_to_store(index, dir.as_ref().map(ScratchDir::path))
                                .map_err(|error| AlgorithmError::from(FrameError::from(error)))
                        };
                        match stored {
                            Ok(frame) => {
                                slots[index]
                                    .set(frame)
                                    .expect("each frame is taken by one worker");
                                developed.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(error) => {
                                next.store(slots.len(), Ordering::Relaxed);
                                failure
                                    .lock()
                                    .expect("no worker panics holding the failure slot")
                                    .get_or_insert(error);
                                break;
                            }
                        }
                    }
                });
            }
        });
        if let Some(error) = failure.into_inner().expect("no worker panicked") {
            return Err(error);
        }
        let frames = slots
            .into_iter()
            .map(|slot| slot.into_inner().expect("every frame developed"))
            .collect();
        self.scratch.get_or_init(|| DevelopedFrames { frames, dir });
        Ok(threads)
    }
}

impl RawStack {
    /// Reads `rect` of `frame` with the Q16 `gain` applied to the camera-linear
    /// values, through the look, on the 16-bit scale (1.0 look white is
    /// 65535). `S::narrow` is the only clip.
    fn read_gained<S: Sample>(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [S],
    ) -> Result<(), AlgorithmError> {
        let developed = self.developed(frame).map_err(crate::io::FrameError::from)?;
        let desc = &self.frames[frame].desc;
        let path = &self.frames[frame].metadata.path;
        check_tile(rect, (desc.width, desc.height), 3, out.len())
            .map_err(|fault| match fault {
                TileFault::OutOfBounds { requested, frame } => RawError::OutOfBounds {
                    path: path.clone(),
                    requested,
                    frame,
                },
                TileFault::BufferSize { expected, got } => RawError::BufferSize {
                    path: path.clone(),
                    expected,
                    got,
                },
            })
            .map_err(crate::io::FrameError::from)?;
        let grey_look;
        let look = if developed.monochrome() {
            grey_look = LookTransform::new([f32::NAN; 4], IDENTITY_MATRIX);
            &grey_look
        } else {
            self.look().map_err(crate::io::FrameError::from)?
        };
        let scale = gain as f32 / UNIT_GAIN as f32;
        let stride = desc.width as usize;
        let width = rect.width as usize;
        let row_len = width * 3;
        match &developed {
            Developed::Cached(frame) => {
                let mut linear = Vec::new();
                for (row, source_y) in (rect.y..rect.y + rect.height).enumerate() {
                    let source = source_y as usize * stride + rect.x as usize;
                    let destination = &mut out[row * row_len..(row + 1) * row_len];
                    look_row(
                        frame.samples[source..source + width].as_flattened(),
                        look,
                        scale,
                        &mut linear,
                        destination,
                    );
                }
            }
            Developed::Stored(frame) => {
                look_stored(frame, rect, look, scale, out).map_err(crate::io::FrameError::from)?;
            }
        }
        Ok(())
    }
}

/// [`RawStack::read_gained`] for a stored frame: each run is widened, then
/// looked, through buffers reused across runs.
fn look_stored<S: Sample>(
    frame: &StoredFrame,
    rect: Rect,
    look: &LookTransform,
    scale: f32,
    out: &mut [S],
) -> Result<(), RawError> {
    let row_len = rect.width as usize * 3;
    let (mut halves, mut widened, mut linear) = (Vec::new(), Vec::new(), Vec::new());
    frame.read_runs(rect, |row, column, bytes| {
        let target = &mut out[row * row_len + column * 3..][..bytes.len() / PIXEL_BYTES * 3];
        decode_pixels(bytes, &mut halves, &mut widened);
        look_row(&widened, look, scale, &mut linear, target);
    })
}

/// The runs of `count` pixels from column `x` that each stay inside one
/// scratch block column, as `(first column, length)`.
fn block_segments(x: usize, count: usize) -> impl Iterator<Item = (usize, usize)> {
    let end = x + count;
    std::iter::successors(Some(x), |&start| {
        Some((start / SCRATCH_BLOCK + 1) * SCRATCH_BLOCK)
    })
    .take_while(move |&start| start < end)
    .map(move |start| {
        let stop = ((start / SCRATCH_BLOCK + 1) * SCRATCH_BLOCK).min(end);
        (start, stop - start)
    })
}

/// Applies `scale`, then the look, to `pixels` (three values each),
/// narrowing into `target`. The matrix pass fills `linear`, the encode pass
/// reads it back.
fn look_row<S: Sample>(
    pixels: &[f32],
    look: &LookTransform,
    scale: f32,
    linear: &mut Vec<f32>,
    target: &mut [S],
) {
    linear.resize(pixels.len(), 0.0);
    for (mixed, pixel) in linear.chunks_exact_mut(3).zip(pixels.chunks_exact(3)) {
        let scaled = [pixel[0] * scale, pixel[1] * scale, pixel[2] * scale];
        mixed.copy_from_slice(&look.linear_f32(scaled));
    }
    for (sample, value) in target.iter_mut().zip(linear.iter()) {
        *sample = S::narrow(encode_f32(*value) * WHITE);
    }
}

/// The 16-bit-scale value of look white.
const WHITE: f32 = 65_535.0;

impl TileReader for RawStack {
    fn sample_format(&self) -> SampleFormat {
        SampleFormat::F32
    }

    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.read_gained(frame, rect, UNIT_GAIN, out)
    }

    fn read_tile_gained_into(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.read_gained(frame, rect, gain, out)
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        self.read_gained(frame, rect, UNIT_GAIN, out)
    }

    fn read_tile_gained_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        self.read_gained(frame, rect, gain, out)
    }

    fn linear_means(&self, frame: usize) -> Result<Option<[f64; 3]>, AlgorithmError> {
        let developed = self.developed(frame).map_err(crate::io::FrameError::from)?;
        Ok(Some(developed.means()))
    }

    /// The buffer one read of a file-backed frame fills block by block; none
    /// when the frames are in memory.
    fn read_scratch_bytes(&self, _width: u32, _height: u32) -> u64 {
        if self
            .scratch
            .get()
            .is_some_and(|frames| frames.dir.is_none())
        {
            return 0;
        }
        let (frame_width, frame_height) = self.dimensions();
        read_buffer_bytes(frame_width as usize, frame_height as usize) as u64
    }

    /// Every scratch block a `width x height` read can touch at any origin,
    /// clamped to the frame; none when the frames are in memory.
    fn read_page_cache_bytes(&self, width: u32, height: u32) -> u64 {
        if self
            .scratch
            .get()
            .is_some_and(|frames| frames.dir.is_none())
        {
            return 0;
        }
        let (frame_width, frame_height) = self.dimensions();
        block_span(width, frame_width) * block_span(height, frame_height) * PIXEL_BYTES as u64
    }
}

/// How many frames develop at once: `workers`, no more than `budget_bytes`
/// holds `required`-byte decode reservations for, no more than `frames`, and
/// at least one.
fn develop_threads(workers: usize, budget_bytes: u64, required: u64, frames: usize) -> usize {
    workers
        .min(usize::try_from(budget_bytes / required.max(1)).unwrap_or(usize::MAX))
        .min(frames)
        .max(1)
}

/// The pixels along one axis of the scratch blocks a read of `n` pixels can
/// touch at any origin, at most `dim`.
fn block_span(n: u32, dim: u32) -> u64 {
    let blocks = (n as usize + SCRATCH_BLOCK - 1).div_ceil(SCRATCH_BLOCK);
    (blocks * SCRATCH_BLOCK).min(dim as usize) as u64
}

/// Bytes `frames` developed frames of `width` x `height` occupy in scratch
/// files; `None` when the total overflows `u64`.
fn scratch_bytes(frames: usize, width: u32, height: u32) -> Option<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(PIXEL_BYTES as u64)?
        .checked_mul(frames as u64)
}

const IDENTITY_MATRIX: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Opens every file's metadata, refusing a set whose frames differ in
/// dimensions, camera or CFA.
fn open_frames(paths: &[PathBuf]) -> Result<Vec<RawFrame>, RawError> {
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
    Ok(frames)
}

/// The frame with the largest decode reservation, and that reservation.
fn largest_reservation(frames: &[RawFrame]) -> Result<(&RawFrame, u64), RawError> {
    let mut largest = None;
    for frame in frames {
        let required = reservation_bytes(frame)?;
        if largest.is_none_or(|(_, most)| required > most) {
            largest = Some((frame, required));
        }
    }
    Ok(largest.expect("a RAW stack holds at least one frame"))
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

/// Camera-linear RGB of the file: black and white levels, demosaic and the
/// sensor's default crop, nothing else.
fn develop(path: &Path) -> Result<LinearFrame, RawError> {
    let source = RawSource::new(path).map_err(|source| RawError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoder = seam(path, || rawler::get_decoder(&source))?;
    let image = seam(path, || {
        decoder.raw_image(&source, &RawDecodeParams::default(), false)
    })?;
    let steps = [
        ProcessingStep::Rescale,
        ProcessingStep::Demosaic,
        ProcessingStep::FujiRotate,
        ProcessingStep::CropActiveArea,
        ProcessingStep::CropDefault,
    ];
    let intermediate = seam(path, || {
        RawDevelop::new_with(&steps).develop_intermediate(&image)
    })?;
    let (width, height, samples, monochrome) = match intermediate {
        Intermediate::ThreeColor(pixels) => (pixels.width, pixels.height, pixels.data, false),
        Intermediate::Monochrome(pixels) => (
            pixels.width,
            pixels.height,
            pixels.data.into_iter().map(|value| [value; 3]).collect(),
            true,
        ),
        Intermediate::FourColor(_) => {
            return Err(RawError::Unsupported {
                path: path.to_path_buf(),
                make: String::new(),
                model: String::new(),
                detail: "development did not produce RGB".to_string(),
            });
        }
    };
    let means = channel_means(&samples);
    Ok(LinearFrame {
        width: width as u32,
        height: height as u32,
        samples,
        means,
        monochrome,
    })
}

fn channel_means(samples: &[[f32; 3]]) -> [f64; 3] {
    let mut sums = [0.0_f64; 3];
    for pixel in samples {
        for (sum, value) in sums.iter_mut().zip(pixel) {
            *sum += f64::from(*value);
        }
    }
    sums.map(|sum| sum / samples.len().max(1) as f64)
}

/// The header-only [`RawImage`] of the file at `path`: camera facts and
/// colour data, no pixels.
fn reference_image(path: &Path) -> Result<RawImage, RawError> {
    let source = RawSource::new(path).map_err(|source| RawError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoder = seam(path, || rawler::get_decoder(&source))?;
    seam(path, || {
        decoder.raw_image(&source, &RawDecodeParams::default(), true)
    })
}

/// The look transform of the camera that took the file at `path`, from its
/// metadata alone.
fn reference_look(path: &Path) -> Result<LookTransform, RawError> {
    Ok(LookTransform::from_image(&reference_image(path)?))
}

/// One of a camera's XYZ-to-camera matrices with the DNG light-source code of
/// the illuminant it was measured under.
#[derive(Debug, Clone, PartialEq)]
pub struct DngMatrix {
    /// DNG `CalibrationIlluminant` code.
    pub illuminant: u16,
    /// Row-major XYZ-to-camera matrix.
    pub xyz_to_camera: [f32; 9],
    /// Row-major `CameraCalibration` of the slot this matrix came from, when
    /// the file has one.
    pub calibration: Option<[f32; 9]>,
}

/// What a DNG needs to describe the camera space of a RAW stack's document:
/// the look transform whose inverse leads back to it, and the colour tags of
/// the first frame, the frame the look was taken from.
#[derive(Debug, Clone)]
pub struct DngColour {
    /// Camera-linear to look-space transform of the first frame.
    pub look: LookTransform,
    /// Camera make, cleaned to a short consistent name.
    pub make: String,
    /// Camera model, cleaned to a short consistent name.
    pub model: String,
    /// `make` and `model` joined: the DNG's unique camera model.
    pub unique_model: String,
    /// TIFF Orientation of the frame, `None` when the file carries none.
    pub orientation: Option<u16>,
    /// The camera's matrices with a known colour temperature, at most two,
    /// the lower colour temperature first.
    pub matrices: Vec<DngMatrix>,
    /// As-shot neutral: the reciprocal of the white-balance multipliers,
    /// normalised to green = 1.
    pub as_shot_neutral: [f64; 3],
    /// `AnalogBalance` of the first frame, when the file has one.
    pub analog_balance: Option<[f64; 3]>,
}

/// Correlated colour temperature in kelvin of the illuminants a camera
/// matrix is commonly measured under.
fn colour_temperature(illuminant: Illuminant) -> Option<u32> {
    Some(match illuminant {
        Illuminant::A => 2856,
        Illuminant::Tungsten => 2850,
        Illuminant::B => 4874,
        Illuminant::D50 => 5003,
        Illuminant::Daylight => 5500,
        Illuminant::D55 => 5503,
        Illuminant::D65 => 6504,
        Illuminant::C => 6774,
        Illuminant::D75 => 7504,
        _ => return None,
    })
}

/// A DNG's `CameraCalibration` matrices keyed by the `CalibrationIlluminant`
/// code of their slot, and its `AnalogBalance`. Empty for other formats.
#[derive(Default)]
struct Calibration {
    by_illuminant: Vec<(u16, [f32; 9])>,
    analog_balance: Option<[f64; 3]>,
}

fn calibration(path: &Path) -> Result<Calibration, RawError> {
    let source = RawSource::new(path).map_err(|source| RawError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let decoder = seam(path, || rawler::get_decoder(&source))?;
    let Some(root) = seam(path, || decoder.ifd(WellKnownIFD::Root))? else {
        return Ok(Calibration::default());
    };
    const TAG_CAMERA_CALIBRATION_1: u16 = 50723;
    const TAG_CAMERA_CALIBRATION_2: u16 = 50724;
    const TAG_ANALOG_BALANCE: u16 = 50727;
    const TAG_CALIBRATION_ILLUMINANT_1: u16 = 50778;
    const TAG_CALIBRATION_ILLUMINANT_2: u16 = 50779;
    let slots = [
        (TAG_CALIBRATION_ILLUMINANT_1, TAG_CAMERA_CALIBRATION_1),
        (TAG_CALIBRATION_ILLUMINANT_2, TAG_CAMERA_CALIBRATION_2),
    ];
    let by_illuminant = slots
        .into_iter()
        .filter_map(|(illuminant_tag, calibration_tag)| {
            let illuminant =
                u16::try_from(root.get_entry(illuminant_tag)?.value.force_u32(0)).ok()?;
            let values = values_f32(root.get_entry(calibration_tag))?;
            Some((illuminant, values.try_into().ok()?))
        })
        .collect();
    let analog_balance = values_f32(root.get_entry(TAG_ANALOG_BALANCE))
        .and_then(|values| <[f32; 3]>::try_from(values).ok())
        .map(|values| values.map(f64::from))
        .filter(|values| values.iter().all(|v| v.is_finite() && *v > 0.0));
    Ok(Calibration {
        by_illuminant,
        analog_balance,
    })
}

/// The DNG colour data of the camera that took the file at `path`, read from
/// the same header as the stack's look transform, so look and tags cannot
/// disagree.
///
/// A monochrome sensor, a camera with no 3x3 matrix of known colour
/// temperature and a file without usable as-shot multipliers are refused
/// with [`RawError::Unsupported`].
pub fn dng_colour(path: &Path) -> Result<DngColour, RawError> {
    panic_boundary(path, || {
        let image = reference_image(path)?;
        let refuse = |detail: &str| RawError::Unsupported {
            path: path.to_path_buf(),
            make: image.clean_make.clone(),
            model: image.clean_model.clone(),
            detail: detail.to_string(),
        };
        if image.is_monochrome() {
            return Err(refuse("a monochrome sensor has no camera colour space"));
        }
        let wb = image.wb_coeffs;
        if !wb[..3].iter().all(|c| c.is_finite() && *c > 0.0) {
            return Err(refuse("the file carries no as-shot white balance"));
        }
        let mut matrices: Vec<(u32, DngMatrix)> = image
            .color_matrix
            .iter()
            .filter_map(|(&illuminant, flat)| {
                Some((
                    colour_temperature(illuminant)?,
                    DngMatrix {
                        illuminant: illuminant as u16,
                        xyz_to_camera: flat.as_slice().try_into().ok()?,
                        calibration: None,
                    },
                ))
            })
            .collect();
        matrices.sort_by_key(|(kelvin, _)| *kelvin);
        let Some(coolest) = matrices.first().cloned() else {
            return Err(refuse("the file carries no colour matrix"));
        };
        let mut chosen = vec![coolest.1];
        if matrices.len() > 1 {
            chosen.extend(matrices.pop().map(|(_, matrix)| matrix));
        }
        let calibration = calibration(path)?;
        for matrix in &mut chosen {
            matrix.calibration = calibration
                .by_illuminant
                .iter()
                .find(|(illuminant, _)| *illuminant == matrix.illuminant)
                .map(|(_, calibration)| *calibration);
        }
        Ok(DngColour {
            look: LookTransform::from_image(&image),
            make: image.clean_make.clone(),
            model: image.clean_model.clone(),
            unique_model: format!("{} {}", image.clean_make, image.clean_model),
            orientation: Some(image.orientation.to_u16()).filter(|&o| o != 0),
            matrices: chosen,
            as_shot_neutral: [0, 1, 2].map(|c| f64::from(wb[1]) / f64::from(wb[c])),
            analog_balance: calibration.analog_balance,
        })
    })
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
        orientation: 1,
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
    use super::{
        FrameBytes, IDENTITY_MATRIX, LookTransform, PIXEL_BYTES, RawError, SCRATCH_BLOCK,
        StoredFrame, WHITE, decode_pixels, look_stored, panic_boundary, scratch_bytes,
        write_samples,
    };
    use crate::equalize::UNIT_GAIN;
    use crate::tile::{Rect, Sample};
    use std::path::Path;

    /// A frame whose sample at (x, y) is `[x, y, x + y]`, exact in half precision.
    fn coordinate_samples(width: usize, height: usize) -> Vec<[f32; 3]> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| [x as f32, y as f32, (x + y) as f32]))
            .collect()
    }

    /// The same frame twice: in a scratch file under `dir`, and in memory.
    fn stored_frames(
        dir: &Path,
        width: usize,
        height: usize,
        samples: &[[f32; 3]],
    ) -> (StoredFrame, StoredFrame) {
        let path = dir.join("frame.f16");
        let mut file = std::fs::File::create(&path).expect("scratch file");
        write_samples(&mut file, samples, width).expect("scratch writes");
        let mut memory = Vec::new();
        write_samples(&mut memory, samples, width).expect("vector writes");
        let frame = |bytes| StoredFrame {
            bytes,
            width,
            height,
            means: [0.0; 3],
            monochrome: false,
        };
        (
            frame(FrameBytes::File(path)),
            frame(FrameBytes::Memory(memory.into_boxed_slice())),
        )
    }

    fn read_rect(frame: &StoredFrame, rect: Rect) -> Vec<[f32; 3]> {
        let columns = rect.width as usize;
        let mut pixels = vec![[f32::NAN; 3]; columns * rect.height as usize];
        let (mut halves, mut decoded) = (Vec::new(), Vec::new());
        frame
            .read_runs(rect, |row, column, bytes| {
                decode_pixels(bytes, &mut halves, &mut decoded);
                for (slot, pixel) in pixels[row * columns + column..]
                    .iter_mut()
                    .zip(decoded.chunks_exact(3))
                {
                    *slot = [pixel[0], pixel[1], pixel[2]];
                }
            })
            .expect("the frame reads");
        pixels
    }

    /// Scalar oracle: one `to_f32` per channel.
    fn reference_decode(bytes: &[u8]) -> impl Iterator<Item = [f32; 3]> + '_ {
        bytes.chunks_exact(PIXEL_BYTES).map(|pixel| {
            std::array::from_fn(|channel| {
                let bytes = pixel[channel * 2..(channel + 1) * 2]
                    .try_into()
                    .expect("a channel is two bytes");
                half::f16::from_ne_bytes(bytes).to_f32()
            })
        })
    }

    /// Per-pixel scalar oracle for `look_row`.
    fn reference_look_row<S: Sample>(
        pixels: impl Iterator<Item = [f32; 3]>,
        look: &LookTransform,
        scale: f32,
        target: &mut [S],
    ) {
        for (pixel, target) in pixels.zip(target.chunks_exact_mut(3)) {
            let looked = look.forward_f32(pixel.map(|value| value * scale));
            for (sample, value) in target.iter_mut().zip(looked) {
                *sample = S::narrow(value * WHITE);
            }
        }
    }

    fn reference_stored<S: Sample>(
        frame: &StoredFrame,
        rect: Rect,
        look: &LookTransform,
        scale: f32,
        out: &mut [S],
    ) {
        let row_len = rect.width as usize * 3;
        frame
            .read_runs(rect, |row, column, bytes| {
                let target =
                    &mut out[row * row_len + column * 3..][..bytes.len() / PIXEL_BYTES * 3];
                reference_look_row(reference_decode(bytes), look, scale, target);
            })
            .expect("the frame reads");
    }

    #[test]
    fn f16_widening_matches_half_for_every_pattern() {
        // Two extra values make the count a multiple of three; the slice
        // starts at an odd address.
        let patterns: Vec<u16> = (0..=u16::MAX).chain([0x7e01, 0xfc01]).collect();
        let mut backing = vec![0u8; patterns.len() * 2 + 1];
        for (slot, bits) in backing[1..].chunks_exact_mut(2).zip(&patterns) {
            slot.copy_from_slice(&half::f16::from_bits(*bits).to_ne_bytes());
        }
        let (mut halves, mut widened) = (Vec::new(), Vec::new());
        decode_pixels(&backing[1..], &mut halves, &mut widened);
        assert_eq!(widened.len(), patterns.len());
        for (bits, got) in patterns.iter().zip(&widened) {
            let want = half::f16::from_bits(*bits).to_f32();
            assert_eq!(got.to_bits(), want.to_bits(), "pattern {bits:#06x}");
        }
    }

    /// Frame samples that carry every f16 pattern in every channel (the
    /// strides are odd, so each channel is a permutation of the patterns).
    fn pattern_samples(width: usize, height: usize) -> Vec<[f32; 3]> {
        (0..width * height)
            .map(|pixel| {
                [20_001usize, 30_011, 40_013]
                    .map(|stride| half::f16::from_bits((pixel * stride % 65_536) as u16).to_f32())
            })
            .collect()
    }

    fn assert_same_bits<S: Sample>(got: &[S], want: &[S], message: &str) {
        assert_eq!(got.len(), want.len(), "{message}");
        if let Some(at) = got
            .iter()
            .zip(want)
            .position(|(g, w)| g.widen().to_bits() != w.widen().to_bits())
        {
            panic!(
                "{message}: sample {at} is {} want {}",
                got[at].widen(),
                want[at].widen()
            );
        }
    }

    fn check_look_stored<S: Sample>(
        frame: &StoredFrame,
        rect: Rect,
        look: &LookTransform,
        scale: f32,
        message: &str,
    ) {
        let len = rect.width as usize * rect.height as usize * 3;
        let mut got = vec![S::default(); len];
        look_stored(frame, rect, look, scale, &mut got).expect("the frame reads");
        let mut want = vec![S::default(); len];
        reference_stored(frame, rect, look, scale, &mut want);
        assert_same_bits(&got, &want, message);
    }

    #[test]
    fn raw_look_row_matches_reference() {
        let dir = tempfile::tempdir().expect("temp dir");
        let camera = LookTransform::new(
            [2.1, 1.0, 1.5, f32::NAN],
            [[0.9, -0.2, 0.1], [-0.3, 1.1, 0.2], [0.05, 0.1, 0.8]],
        );
        let grey = LookTransform::new([f32::NAN; 4], IDENTITY_MATRIX);
        let looks = [("camera", &camera), ("grey", &grey)];
        let gains = [0, UNIT_GAIN / 3, UNIT_GAIN, UNIT_GAIN * 3 / 2, u32::MAX];
        for (width, height) in [(300usize, 260usize), (70, 45)] {
            let stores = stored_frames(dir.path(), width, height, &pattern_samples(width, height));
            let (w, h) = (width as u32, height as u32);
            let rects = [
                (0, 0, 64, 40),
                (0, 0, w, h),
                (17, 3, 40, 30),
                (60, 10, 10, 30),
                (3, 60, 20, 10),
                (w - 1, h - 1, 1, 1),
                (w - 33, h - 20, 33, 20),
            ]
            .into_iter()
            .filter(|&(x, y, width, height)| x + width <= w && y + height <= h)
            .collect::<Vec<_>>();
            for (store, frame) in [("file", &stores.0), ("memory", &stores.1)] {
                for (name, look) in looks {
                    for gain in gains {
                        let scale = gain as f32 / UNIT_GAIN as f32;
                        for &(x, y, width, height) in &rects {
                            let rect = Rect {
                                x,
                                y,
                                width,
                                height,
                            };
                            let message = format!(
                                "{w}x{h} {store} {name} gain {gain} rect ({x},{y}) {width}x{height}"
                            );
                            check_look_stored::<u16>(frame, rect, look, scale, &message);
                            check_look_stored::<f32>(frame, rect, look, scale, &message);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn scratch_file_is_block_major() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (width, height) in [(128, 64), (130, 64), (150, 70), (129, 131), (40, 30)] {
            let (on_disk, in_memory) = stored_frames(
                dir.path(),
                width,
                height,
                &coordinate_samples(width, height),
            );
            let (FrameBytes::File(path), FrameBytes::Memory(memory)) =
                (&on_disk.bytes, &in_memory.bytes)
            else {
                panic!("the twins are file-backed and in memory");
            };
            let file = std::fs::read(path).expect("scratch reads back");
            assert_eq!(file.len(), width * height * PIXEL_BYTES, "{width}x{height}");
            // Blocks in row-major block order, rows then columns inside a block.
            let mut order: Vec<(usize, usize)> = (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .collect();
            order.sort_by_key(|&(x, y)| (y / SCRATCH_BLOCK, x / SCRATCH_BLOCK, y, x));
            let expected: Vec<u8> = order
                .iter()
                .flat_map(|&(x, y)| {
                    [x as f32, y as f32, (x + y) as f32]
                        .into_iter()
                        .flat_map(|value| half::f16::from_f32(value).to_ne_bytes())
                })
                .collect();
            assert!(file == expected, "{width}x{height} layout differs");
            assert!(file[..] == memory[..], "{width}x{height} twins differ");
        }
    }

    #[test]
    fn scratch_reads_match_in_memory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let starts = [0usize, 1, 17, 63, 64, 65, 127];
        let lengths = [1usize, 2, 64, 65, 130];
        for (width, height) in [(150, 70), (129, 131), (40, 30)] {
            let (on_disk, in_memory) = stored_frames(
                dir.path(),
                width,
                height,
                &coordinate_samples(width, height),
            );
            let reference = coordinate_samples(width, height);
            let mut rects = vec![(width - 1, height - 1, 1, 1)];
            for &y0 in &starts {
                for &x0 in &starts {
                    for &rows in &lengths {
                        for &columns in &lengths {
                            if x0 + columns <= width && y0 + rows <= height {
                                rects.push((x0, y0, columns, rows));
                            }
                        }
                    }
                }
            }
            for (x0, y0, columns, rows) in rects {
                let rect = Rect {
                    x: x0 as u32,
                    y: y0 as u32,
                    width: columns as u32,
                    height: rows as u32,
                };
                let want: Vec<[f32; 3]> = (y0..y0 + rows)
                    .flat_map(|y| {
                        reference[y * width + x0..y * width + x0 + columns]
                            .iter()
                            .map(|pixel| pixel.map(|v| half::f16::from_f32(v).to_f32()))
                    })
                    .collect();
                let message = format!("{width}x{height} rect at ({x0},{y0}) {columns}x{rows}");
                assert!(read_rect(&on_disk, rect) == want, "file-backed {message}");
                assert!(read_rect(&in_memory, rect) == want, "in-memory {message}");
            }
        }
    }

    #[test]
    fn scratch_space_needed_overflow_is_an_error() {
        let widest = u64::from(u32::MAX);
        assert_eq!(scratch_bytes(1, u32::MAX, u32::MAX), None);
        assert_eq!(scratch_bytes(2, u32::MAX, u32::MAX), None);
        assert_eq!(
            scratch_bytes(1, u32::MAX, 715_827_882),
            Some(widest * 715_827_882 * 6)
        );
        assert_eq!(scratch_bytes(1, u32::MAX, 715_827_883), None);
        let frames = (u64::MAX / 6) as usize;
        assert_eq!(scratch_bytes(frames, 1, 1), Some(frames as u64 * 6));
        assert_eq!(scratch_bytes(frames + 1, 1, 1), None);
    }

    #[test]
    fn caught_decoder_panic_does_not_poison_the_next_open() {
        let path = Path::new("decoder-panic.raw");
        let error = panic_boundary(path, || -> Result<(), RawError> { panic!("decoder panic") })
            .expect_err("decoder panic is named");
        assert!(matches!(error, RawError::Panic { .. }));
        assert!(panic_boundary(path, || Ok(())).is_ok());
    }
}
