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

    /// `true` when this stage is known to do nothing -- checkable rather
    /// than assumed, so a consumer that supplies its own pixels (and
    /// therefore never has its per-frame tile run through this hook) can be
    /// refused by name the moment a real preprocess stage exists, instead of
    /// silently skipping it. Default `false`: only [`NoopPreprocess`]
    /// overrides it.
    fn is_noop(&self) -> bool {
        false
    }
}

/// Leaves the tile untouched.
pub struct NoopPreprocess;

impl FramePreprocess for NoopPreprocess {
    fn preprocess(&self, _frame: usize, _tile: &mut Tile) -> Result<(), AlgorithmError> {
        Ok(())
    }

    fn is_noop(&self) -> bool {
        true
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
