// SPDX-License-Identifier: GPL-3.0-or-later

//! Format-neutral facts and tile access for an opened set of frames.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;

use serde::{Deserialize, Serialize};

use crate::algorithm::{AlgorithmError, TileReader};
use crate::params::StackContext;
use crate::tile::HostBudget;

/// One frame's identity for resume comparison, geometry, and layout facts.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameDesc {
    pub path: PathBuf,
    pub file_size: u64,
    pub image_description: Option<String>,
    /// EXIF Orientation (1-8) the reader applies to serve the frame upright;
    /// 1 when the frame is served as stored.
    pub orientation: u16,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub strip_offsets: Vec<u64>,
    pub strip_byte_counts: Vec<u64>,
    pub raw_identity: Option<RawIdentity>,
    pub layout_label: String,
    pub layout_signature: Vec<String>,
}

/// Header facts that distinguish RAW inputs without developing their pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawIdentity {
    pub make: String,
    pub model: String,
    pub serial_number: Option<String>,
    pub capture_time_original: Option<String>,
    pub raw_width: u32,
    pub raw_height: u32,
    pub black_levels: Option<Vec<f32>>,
    pub white_levels: Option<Vec<u32>>,
    pub cfa: Option<Vec<u8>>,
    pub cfa_width: Option<u16>,
    pub cfa_height: Option<u16>,
}

/// Everything that can go wrong reading a tile through a [`FrameReader`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// RAW, JPEG and TIFF frames cannot share one development path.
    #[error("{} is {found}, but first frame {} is {first}", path.display(), first_path.display())]
    MixedFormats {
        /// First frame path.
        first_path: PathBuf,
        /// First frame format.
        first: &'static str,
        /// Differing frame path.
        path: PathBuf,
        /// Differing frame format.
        found: &'static str,
    },
    /// A read through a TIFF-backed reader failed.
    #[error(transparent)]
    Tiff(#[from] crate::io::TiffError),
    /// A read through a RAW-backed reader failed.
    #[error(transparent)]
    Raw(#[from] crate::io::RawError),
    /// A read through a JPEG-backed reader failed.
    #[error(transparent)]
    Jpeg(#[from] crate::io::JpegError),
}

/// Where [`FrameReader::develop_once`] keeps the developed frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevelopStore {
    /// Anonymous memory, nothing written to disk.
    Memory,
    /// Files under the scratch root.
    ScratchFile,
}

/// An opened set of frames and its tile-shaped pixel source.
pub trait FrameReader: TileReader + Send + Sync {
    /// How many frames this reader holds.
    fn len(&self) -> usize;

    /// `true` when this reader holds no frames.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every frame's shared `(width, height)`.
    fn dimensions(&self) -> (u32, u32);

    /// Frame `index`'s description.
    fn frame(&self, index: usize) -> &FrameDesc;

    /// Full-frame develops per frame since this reader opened, in frame
    /// order; `None` for readers that keep no count.
    fn develop_counts(&self) -> Option<Vec<u64>> {
        None
    }

    /// Develops every frame once into `store`, which later reads are served
    /// from: up to `workers` at a time, within `budget` bytes of decode
    /// reservations. [`DevelopStore::ScratchFile`] writes files under
    /// `scratch_root`, [`DevelopStore::Memory`] nothing there. Returns how many
    /// develop at once, `0` for nothing to develop. `cancelled` is polled
    /// before each frame; a failed or cancelled pass keeps no frame and no file.
    fn develop_once(
        &self,
        _scratch_root: &Path,
        _store: DevelopStore,
        _budget: HostBudget,
        _workers: usize,
        _cancelled: &(dyn Fn() -> bool + Sync),
        _developed: &AtomicUsize,
    ) -> Result<usize, AlgorithmError> {
        Ok(0)
    }

    /// Bytes the developed frames occupy, in either store; `0` for a reader
    /// that develops nothing.
    fn develop_scratch_bytes(&self) -> Result<u64, AlgorithmError> {
        Ok(0)
    }

    /// Where [`FrameReader::develop_once`] should keep the frames: memory
    /// when every frame plus the larger of the pass's decode reservations
    /// (`workers` at a time) and `tile_share` bytes of tile scheduling fits
    /// `budget`, else scratch files. `None` for a reader with nothing to
    /// develop.
    fn develop_store(
        &self,
        _budget: HostBudget,
        _workers: usize,
        _tile_share: u64,
    ) -> Result<Option<DevelopStore>, AlgorithmError> {
        Ok(None)
    }

    /// Facts used to resolve stack-dependent parameters.
    fn stack_context(&self) -> StackContext {
        let (width, height) = self.dimensions();
        StackContext::new(self.len(), width, height, self.frame(0).samples_per_pixel)
    }
}
