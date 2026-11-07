// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-frame-tile preprocessing hook is a no-op. The value is
//! the *call site* in [`super::scheduler::TileScheduler::run`], not the
//! body: a stage such as brightness/exposure normalization becomes a new
//! [`FramePreprocess`] implementation, not a pipeline change.

use crate::algorithm::AlgorithmError;
use crate::tile::{Rect, Tile};

/// A per-frame-tile preprocessing stage, run before a frame's tile reaches
/// the [`super::consumer::TileConsumer`].
pub trait FramePreprocess: Send + Sync {
    fn preprocess(&self, frame: usize, tile: &mut Tile) -> Result<(), AlgorithmError>;

    /// [`Self::preprocess`] over `f32` samples of `rect`, 16-bit scale. A
    /// stage that has no `f32` form refuses the tile rather than passing it
    /// through unprocessed.
    fn preprocess_f32(
        &self,
        _frame: usize,
        _rect: Rect,
        _samples_per_pixel: u32,
        _samples: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        Err(AlgorithmError::Failed {
            algorithm: "preprocess".to_string(),
            detail: "this stage does not process f32 samples".to_string(),
        })
    }
}

/// Leaves the tile untouched.
pub struct NoopPreprocess;

impl FramePreprocess for NoopPreprocess {
    fn preprocess(&self, _frame: usize, _tile: &mut Tile) -> Result<(), AlgorithmError> {
        Ok(())
    }

    fn preprocess_f32(
        &self,
        _frame: usize,
        _rect: Rect,
        _samples_per_pixel: u32,
        _samples: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_preprocess_leaves_the_tile_untouched() {
        let mut tile = Tile {
            rect: Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            samples_per_pixel: 1,
            samples: vec![1, 2, 3, 4],
        };
        let before = tile.clone();
        NoopPreprocess.preprocess(0, &mut tile).unwrap();
        assert_eq!(tile, before);
    }
}
