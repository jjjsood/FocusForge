// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use super::{
    DecodedTiffStack, FrameError, FrameReader, RAW_EXTENSIONS, RawStack, TiffError, TiffHeader,
    TiffStack, has_extension,
};
use crate::tile::HostBudget;

/// The reader selected for a frame set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderKind {
    /// Direct memory-mapped reads from uncompressed strips.
    Mmap,
    /// On-demand decoded reads for supported non-fast-path TIFFs.
    Decoded,
    /// On-demand development for RAW frames.
    Raw,
}

impl ReaderKind {
    /// Stable name used in CLI reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mmap => "TiffStack",
            Self::Decoded => "DecodedTiffStack",
            Self::Raw => "RawStack",
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
pub fn open_frame_set(paths: &[PathBuf], budget: HostBudget) -> Result<FrameSet, FrameError> {
    let Some(first_path) = paths.first() else {
        return Err(TiffError::NoFrames {
            path: PathBuf::new(),
        }
        .into());
    };
    let first_is_raw = has_extension(first_path, RAW_EXTENSIONS);
    for path in paths.iter().skip(1) {
        let is_raw = has_extension(path, RAW_EXTENSIONS);
        if is_raw != first_is_raw {
            return Err(FrameError::MixedFormats {
                first_path: first_path.clone(),
                first: if first_is_raw { "RAW" } else { "TIFF" },
                path: path.clone(),
                found: if is_raw { "RAW" } else { "TIFF" },
            });
        }
    }
    if first_is_raw {
        let shares = budget.partition_for_decoded_cache();
        return Ok(FrameSet {
            reader: Box::new(RawStack::open_files_with_budget(
                paths,
                shares.cache.bytes(),
            )?),
            kind: ReaderKind::Raw,
            scheduler_budget: shares.scheduler,
        });
    }
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
        ReaderKind::Raw => unreachable!("RAW paths return before TIFF classification"),
    }
}
