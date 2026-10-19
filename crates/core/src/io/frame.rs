// SPDX-License-Identifier: GPL-3.0-or-later

//! Format-neutral facts and tile access for an opened set of frames.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::algorithm::TileReader;
use crate::params::StackContext;

/// One frame's identity for resume comparison, geometry, and layout facts.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameDesc {
    pub path: PathBuf,
    pub file_size: u64,
    pub image_description: Option<String>,
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
    /// RAW and TIFF frames cannot share one development path.
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

    /// Facts used to resolve stack-dependent parameters.
    fn stack_context(&self) -> StackContext {
        let (width, height) = self.dimensions();
        StackContext::new(self.len(), width, height, self.frame(0).samples_per_pixel)
    }
}
