// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use super::{DecodedTiffStack, FrameReader, TiffError, TiffHeader, TiffStack};
use crate::tile::HostBudget;

/// The reader selected for a frame set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderKind {
    /// Direct memory-mapped reads from uncompressed strips.
    Mmap,
    /// On-demand decoded reads for supported non-fast-path TIFFs.
    Decoded,
}

impl ReaderKind {
    /// Stable name used in CLI reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mmap => "TiffStack",
            Self::Decoded => "DecodedTiffStack",
        }
    }
}

/// An opened frame set and the scheduler budget left after input caching.
pub struct FrameSet {
    /// Reader that serves every frame in order.
    pub reader: Box<dyn FrameReader>,
    /// Reader selected from the parsed headers.
    pub kind: ReaderKind,
    /// Budget available to tile scheduling.
    pub scheduler_budget: HostBudget,
}

/// Classify one parsed TIFF header without reading pixel payloads.
pub fn classify_header(header: &TiffHeader) -> Result<ReaderKind, TiffError> {
    if header.require_fast_path().is_ok() {
        return Ok(ReaderKind::Mmap);
    }
    super::tiff::segment::validate_decoded_header(header)?;
    Ok(ReaderKind::Decoded)
}

/// Open `paths` with the reader their headers permit.
pub fn open_frame_set(paths: &[PathBuf], budget: HostBudget) -> Result<FrameSet, TiffError> {
    let mut kind = ReaderKind::Mmap;
    for path in paths {
        if classify_header(&TiffHeader::open(path)?)? == ReaderKind::Decoded {
            kind = ReaderKind::Decoded;
        }
    }

    match kind {
        ReaderKind::Mmap => Ok(FrameSet {
            reader: Box::new(TiffStack::open_files(paths)?),
            kind,
            scheduler_budget: budget,
        }),
        ReaderKind::Decoded => {
            let shares = budget.partition_for_decoded_cache();
            Ok(FrameSet {
                reader: Box::new(DecodedTiffStack::open_files(paths, shares.cache.bytes())?),
                kind,
                scheduler_budget: shares.scheduler,
            })
        }
    }
}
