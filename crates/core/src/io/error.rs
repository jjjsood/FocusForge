// SPDX-License-Identifier: GPL-3.0-or-later

//! Errors for the TIFF reader.
//!
//! Two layers, because the CLI's `info` command needs the refusal as
//! *data*, not only as an `Err`: [`Refusal`] names why a file that parsed fine falls
//! outside the reader's fast path, while [`TiffError`] is what a fallible
//! operation actually returns — either a wrapped [`Refusal`] or one of the
//! structural failures (bad magic, truncated IFD, a value past EOF, …).

use std::path::PathBuf;

use crate::tile::{Rect, SampleFormat};

/// Why a file that parsed fine does not qualify for the fast (mmap, no
/// decode) path. The file is not necessarily broken — it is simply outside
/// the format this reader handles — so this is carried as data on a
/// successfully parsed [`crate::io::TiffHeader`] rather than only as an
/// error variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// `Compression` tag value other than 1 (uncompressed).
    #[error("Compression={0}, only uncompressed (1) is supported")]
    Compressed(u16),
    /// A `TileWidth` tag is present: the file is tiled, not strip-based.
    #[error("tiled TIFF (TileWidth present), only strip-based layout is supported")]
    Tiled,
    /// `PlanarConfiguration` other than 1 (chunky).
    #[error("PlanarConfiguration={0}, only chunky (1) is supported")]
    Planar(u16),
    /// `BitsPerSample` other than all-16.
    #[error("BitsPerSample={0:?}, only 16-bit is supported")]
    BitDepth(Box<[u16]>),
    /// `SampleFormat` other than 1 (unsigned integer).
    #[error("SampleFormat={0}, only unsigned integer (1) is supported")]
    SampleFormat(u16),
}

/// Everything that can go wrong opening a stack or reading a tile.
///
/// Every variant that concerns one file carries its `path` so the message
/// names the offending file, not just the offending value.
#[derive(Debug, thiserror::Error)]
pub enum TiffError {
    /// The file parsed but falls outside the fast path; see [`Refusal`].
    #[error("{}: {refusal}", path.display())]
    Unsupported { path: PathBuf, refusal: Refusal },

    /// The file is valid TIFF but outside the decoded segment reader's class.
    #[error("{}: {field}={value} is not supported by the decoded TIFF reader", path.display())]
    UnsupportedDecoded {
        path: PathBuf,
        field: &'static str,
        value: String,
    },

    /// Segment tables or geometry cannot describe image samples safely.
    #[error("{}: invalid segment layout: {detail}", path.display())]
    SegmentLayout { path: PathBuf, detail: &'static str },

    /// The requested segment does not exist.
    #[error("{}: segment {index} is outside the segment table ({count} entries)", path.display())]
    SegmentIndex {
        path: PathBuf,
        index: usize,
        count: usize,
    },

    /// A segment's stored bytes run past the provided file bytes.
    #[error("{}: segment {index} runs past end of file", path.display())]
    SegmentPastEof { path: PathBuf, index: usize },

    /// A segment's compressed stream is incomplete or malformed.
    #[error("{}: segment {index} has an invalid compressed stream", path.display())]
    SegmentStream { path: PathBuf, index: usize },

    /// A decoded segment has a byte length different from its stored geometry.
    #[error("{}: segment {index} decoded {got} bytes, expected {expected}", path.display())]
    SegmentLength {
        path: PathBuf,
        index: usize,
        got: usize,
        expected: usize,
    },

    /// The decoded-segment cache cannot retain even one segment.
    #[error("{}: decoded cache bound {bound} bytes is below required segment size {required} bytes", path.display())]
    DecodedCacheTooSmall {
        path: PathBuf,
        bound: u64,
        required: u64,
    },

    /// The requested frame does not exist in this opened set.
    #[error("frame {index} is outside the opened frame set ({count} frames)")]
    FrameIndex { index: usize, count: usize },

    /// Magic 43: BigTIFF, which this reader never supports.
    #[error("{}: BigTIFF (magic 43) is not supported, only classic TIFF (magic 42)", path.display())]
    BigTiff { path: PathBuf },

    /// The first two bytes are neither `II` nor `MM`.
    #[error("{}: not a TIFF (byte-order mark {mark:?})", path.display())]
    NotATiff { path: PathBuf, mark: [u8; 2] },

    /// The magic number is present but neither 42 nor 43.
    #[error("{}: bad TIFF magic {magic} (expected 42)", path.display())]
    BadMagic { path: PathBuf, magic: u16 },

    /// The IFD claims more entries than the file has bytes for.
    #[error("{}: truncated IFD ({entries} entries)", path.display())]
    TruncatedIfd { path: PathBuf, entries: u16 },

    /// An out-of-line IFD value's offset + length runs past EOF.
    #[error("{}: IFD value for tag {tag} runs past end of file", path.display())]
    ValuePastEof { path: PathBuf, tag: u16 },

    /// A required tag is absent.
    #[error("{}: missing required tag {tag}", path.display())]
    MissingTag { path: PathBuf, tag: u16 },

    /// `StripOffsets`/`StripByteCounts` length disagrees with the count
    /// implied by `RowsPerStrip`.
    #[error(
        "{}: {found} strip table entries but {expected} strips implied by RowsPerStrip",
        path.display()
    )]
    StripTableMismatch {
        path: PathBuf,
        found: usize,
        expected: usize,
    },

    /// A strip's `StripByteCounts` entry is smaller than the row it is
    /// asked to serve.
    #[error("{}: strip {strip} holds {have} bytes, row needs {need}", path.display())]
    StripTooShort {
        path: PathBuf,
        strip: usize,
        have: u64,
        need: u64,
    },

    /// The requested rectangle is not fully inside the frame, or is
    /// zero-sized. No clamping — callers that want clamped rects clamp at
    /// their own call site.
    #[error("{}: rect {requested:?} is outside frame {frame:?}", path.display())]
    OutOfBounds {
        path: PathBuf,
        requested: Rect,
        frame: (u32, u32),
    },

    /// A frame disagrees with frame 0's dimensions, bit depth, samples per
    /// pixel, photometric or planar configuration.
    #[error("{}: does not match first frame {}: {detail}", path.display(), first.display())]
    FrameMismatch {
        path: PathBuf,
        first: PathBuf,
        detail: String,
    },

    /// The directory (or its `frames/` subdirectory) has zero TIFF entries.
    #[error("{}: no TIFF frames found", path.display())]
    NoFrames { path: PathBuf },

    /// An I/O error while reading a header or a tile.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `read_tile_into`'s output buffer is the wrong length for the rect.
    #[error("buffer size mismatch: expected {expected} u16 samples, got {got}")]
    BufferSize { expected: usize, got: usize },

    /// `TiffWriter::finish` was called before every pixel of the frame was
    /// written. Names the first row whose merged coverage is not exactly
    /// `[(0, width)]`, so a caller can tell a hole from a double write.
    #[error(
        "{}: row {row} is covered {covered:?}, expected exactly one interval [(0, {width})]",
        path.display()
    )]
    IncompleteFrame {
        path: PathBuf,
        row: u32,
        covered: Vec<(u32, u32)>,
        width: u32,
    },

    /// A `TiffWriter` was handed samples of the other type than its file
    /// holds (`write_tile` on a float writer, `write_tile_f32` on a 16-bit one).
    #[error("{}: this writer holds {expected:?} samples, got {got:?}", path.display())]
    SampleType {
        path: PathBuf,
        expected: SampleFormat,
        got: SampleFormat,
    },

    /// `TiffWriter::reopen` refuses a file that is not the exact layout this
    /// writer would itself have produced -- byte order, compression,
    /// `RowsPerStrip`, dimensions, description, or an arithmetic strip-offset
    /// table. In the style of [`Self::StripTableMismatch`]: named field,
    /// both values.
    #[error("{}: reopen refused, {field} disagrees: expected {expected}, found {found}", path.display())]
    ReopenMismatch {
        path: PathBuf,
        field: &'static str,
        expected: String,
        found: String,
    },
}
