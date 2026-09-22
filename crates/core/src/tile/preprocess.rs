// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-frame-tile preprocessing hook is a no-op. The value is
//! the *call site* in [`super::scheduler::TileScheduler::run`], not the
//! body: a stage such as brightness/exposure normalization becomes a new
//! [`FramePreprocess`] implementation, not a pipeline change.

use crate::algorithm::AlgorithmError;
use crate::tile::Tile;

/// A per-frame-tile preprocessing stage, run before a frame's tile reaches
/// the [`super::consumer::TileConsumer`].
pub trait FramePreprocess: Send + Sync {
    fn preprocess(&self, frame: usize, tile: &mut Tile) -> Result<(), AlgorithmError>;
}

/// Leaves the tile untouched.
pub struct NoopPreprocess;

impl FramePreprocess for NoopPreprocess {
    fn preprocess(&self, _frame: usize, _tile: &mut Tile) -> Result<(), AlgorithmError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::Rect;

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
