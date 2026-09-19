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
//! A crate-level grep for whole-frame reads enforces the rule above.

mod error;
mod frame;
mod grouping;
#[cfg(feature = "test-algorithms")]
pub mod test_frames;
pub(crate) mod tiff;

pub use crate::tile::{Rect, Tile};
pub use error::{Refusal, TiffError};
pub use frame::{FrameDesc, FrameError, FrameReader};
pub use grouping::{CaptureTime, GROUP_GAP, Group, GroupSignal, Proposal, propose};
pub use tiff::header::{ByteOrder, TiffHeader};
pub use tiff::segment::{DecodedSegment, SegmentOrigin, TiffSegmentDecoder, TiffSegmentReader};
pub use tiff::stack::{TiffStack, frame_paths};
pub use tiff::writer::{TiffSpec, TiffWriter};
