// SPDX-License-Identifier: GPL-3.0-or-later

//! Zero-decode TIFF reading.
//!
//! `Compression = 1` means every row's bytes sit at a computable file
//! offset, so a tile can be sliced straight out of an `mmap` with no
//! decompression at all, and the OS page cache becomes the tile cache. This
//! is why [`TiffStack::open`] and [`TiffStack::open_files`] parse headers
//! only, and why [`TiffStack::read_tile`]/[`TiffStack::read_tile_into`] are
//! the *only* way pixels come out of a frame.
//!
//! Never add a convenience that assembles a whole frame from tiles. Tiling
//! is not a fallback here — the reference stack (61 frames at ~33 GB as
//! `f32`, before pyramid levels) does not fit in memory, so a "just load the
//! frame" method is not a shortcut, it is a way to run out of RAM.

mod error;
mod frame;
mod frame_set;
mod grouping;
mod metadata;
mod paths;
pub mod raw;
#[cfg(feature = "test-algorithms")]
pub mod test_frames;
pub(crate) mod tiff;

pub use crate::tile::{Rect, Tile};
pub use error::{Refusal, TiffError};
pub use frame::{FrameDesc, FrameError, FrameReader, RawIdentity};
pub use frame_set::{FrameSet, ReaderKind, classify_header, open_frame_set};
pub use grouping::{
    CaptureTime, GROUP_GAP, Group, GroupSignal, Proposal, capture_time, one_group, propose,
    propose_with_progress,
};
pub(crate) use metadata::{
    IccOrigin, MetadataPlan, XMP_APP1_HEADER, exif_blob, icc_tag_entry, iptc_tag_entry,
    photoshop_iptc_segment, resolve_plan, xmp_tag_entry,
};
pub use metadata::{MetadataTags, SourceKind, SourceMetadata, TagEntry, read_source_metadata};
pub use paths::{
    JPEG_EXTENSIONS, PNG_EXTENSIONS, RAW_EXTENSIONS, TIFF_EXTENSIONS, all_frame_paths,
    frame_paths_for_extensions, has_extension,
};
pub use raw::{DEFAULT_RAW_CACHE_BYTES, RawError, RawMetadata, RawStack};
pub use tiff::decoded::{CacheStats, DecodedTiffStack};
pub use tiff::header::{ByteOrder, TiffHeader};
pub use tiff::segment::{DecodedSegment, SegmentOrigin, TiffSegmentDecoder, TiffSegmentReader};
pub use tiff::stack::{COMPOSITE_FILENAME, TiffStack, ZMAP_FILENAME, frame_paths, is_frame_path};
pub use tiff::writer::{StripCompression, StripSpec, StripWriter, TiffSpec, TiffWriter};
