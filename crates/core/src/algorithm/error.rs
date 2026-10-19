// SPDX-License-Identifier: GPL-3.0-or-later

//! Failures a [`super::StackAlgorithm`] or [`super::AlignAlgorithm`] work
//! method can return.
//!
//! Modelled on `crate::io::error`: every variant names its offending value.
//! `#[error(transparent)]` on the wrapped variants forwards `Display` and
//! `source()` straight through, so a tile failure keeps the source error's
//! own message -- which already names the file.

/// Everything that can go wrong running a [`super::StackAlgorithm::fuse`] or
/// [`super::AlignAlgorithm::align_frame`] work method.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AlgorithmError {
    /// A read through the context's pixel source failed.
    #[error(transparent)]
    Frame(#[from] crate::io::FrameError),

    /// Writing a TIFF output file failed.
    #[error(transparent)]
    Tiff(#[from] crate::io::TiffError),

    /// The values handed to the work method fail the algorithm's own
    /// schema.
    #[error(transparent)]
    Params(#[from] crate::params::ParamError),

    /// A context was asked for a frame index it does not hold.
    #[error("frame {frame} is out of range: the context holds {frame_count} frames")]
    FrameOutOfRange { frame: usize, frame_count: usize },

    /// The algorithm's own work failed for a reason none of the other
    /// variants name.
    #[error("{algorithm}: {detail}")]
    Failed { algorithm: String, detail: String },

    /// A host-derived, uncapped memory budget cannot hold the alignment's
    /// shared reference data plus one registration -- refused by name before
    /// any registration starts. An explicit or `--max-host-memory` budget
    /// still floors to one worker and runs.
    #[error(
        "the host-derived memory budget of {budget_bytes} B cannot hold the alignment's shared \
         reference data and one registration ({required_bytes} B); free memory on this machine \
         or align fewer or smaller frames"
    )]
    HostMemoryTooSmall {
        budget_bytes: u64,
        required_bytes: u64,
    },

    /// An accelerator's device was lost or ran out of memory mid-run; the
    /// work can be redone on the CPU.
    #[error("the accelerator failed: {detail}")]
    BackendLost { detail: String },

    /// The run's control requested a cancel before every frame was
    /// registered; the driver started no registration after reading it.
    #[error("alignment cancelled")]
    Cancelled,
}
