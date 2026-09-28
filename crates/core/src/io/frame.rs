// SPDX-License-Identifier: GPL-3.0-or-later

//! Format-neutral facts and tile access for an opened set of frames.

use std::path::PathBuf;

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
    pub layout_label: String,
    pub layout_signature: Vec<String>,
}

/// Everything that can go wrong reading a tile through a [`FrameReader`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
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
