// SPDX-License-Identifier: GPL-3.0-or-later

//! The two sample types a stack's pixels travel as: `u16`, and `f32` on the
//! 16-bit scale (`65535.0` is white) with no range limit.

use std::borrow::Cow;

use crate::algorithm::{AlgorithmError, TileReader};
use crate::tile::{FramePreprocess, Rect, Tile};

/// What a reader serves natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    U16,
    F32,
}

impl SampleFormat {
    /// Bytes per sample.
    pub fn bytes(self) -> u64 {
        match self {
            SampleFormat::U16 => 2,
            SampleFormat::F32 => 4,
        }
    }
}

/// A sample type the CPU pipeline runs over. `narrow` is the one place an
/// `f32` result becomes a stored sample: `u16` clamps to `[0, 65535]` and
/// rounds half up, `f32` keeps the value.
pub trait Sample: Copy + Default + Send + Sync + 'static {
    const FORMAT: SampleFormat;

    fn widen(self) -> f32;

    fn narrow(value: f32) -> Self;

    /// [`TileReader::read_tile_into`] in this sample type.
    fn read_tile(
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        out: &mut [Self],
    ) -> Result<(), AlgorithmError>;

    /// [`TileReader::read_tile_coverage_into`] in this sample type.
    fn read_tile_coverage(
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        out: &mut [Self],
    ) -> Result<Option<Vec<bool>>, AlgorithmError>;

    /// Runs `preprocess` over `samples` of `rect` and hands the buffer back.
    fn preprocess(
        preprocess: &dyn FramePreprocess,
        frame: usize,
        rect: Rect,
        samples_per_pixel: u32,
        samples: Vec<Self>,
    ) -> Result<Vec<Self>, AlgorithmError>;

    /// `samples` of a `u16` tile in this sample type.
    fn from_u16(samples: &[u16]) -> Cow<'_, [Self]>;
}

impl Sample for u16 {
    const FORMAT: SampleFormat = SampleFormat::U16;

    fn widen(self) -> f32 {
        f32::from(self)
    }

    fn narrow(value: f32) -> Self {
        (value.clamp(0.0, 65535.0) + 0.5).floor() as u16
    }

    fn read_tile(
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        out: &mut [Self],
    ) -> Result<(), AlgorithmError> {
        reader.read_tile_into(frame, rect, out)
    }

    fn read_tile_coverage(
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        out: &mut [Self],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
        reader.read_tile_coverage_into(frame, rect, out)
    }

    fn preprocess(
        preprocess: &dyn FramePreprocess,
        frame: usize,
        rect: Rect,
        samples_per_pixel: u32,
        samples: Vec<Self>,
    ) -> Result<Vec<Self>, AlgorithmError> {
        let mut tile = Tile {
            rect,
            samples_per_pixel,
            samples,
        };
        preprocess.preprocess(frame, &mut tile)?;
        Ok(tile.samples)
    }

    fn from_u16(samples: &[u16]) -> Cow<'_, [Self]> {
        Cow::Borrowed(samples)
    }
}

impl Sample for f32 {
    const FORMAT: SampleFormat = SampleFormat::F32;

    fn widen(self) -> f32 {
        self
    }

    fn narrow(value: f32) -> Self {
        value
    }

    fn read_tile(
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        out: &mut [Self],
    ) -> Result<(), AlgorithmError> {
        reader.read_tile_f32_into(frame, rect, out)
    }

    fn read_tile_coverage(
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        out: &mut [Self],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
        reader.read_tile_coverage_f32_into(frame, rect, out)
    }

    fn preprocess(
        preprocess: &dyn FramePreprocess,
        frame: usize,
        rect: Rect,
        samples_per_pixel: u32,
        mut samples: Vec<Self>,
    ) -> Result<Vec<Self>, AlgorithmError> {
        preprocess.preprocess_f32(frame, rect, samples_per_pixel, &mut samples)?;
        Ok(samples)
    }

    fn from_u16(samples: &[u16]) -> Cow<'_, [Self]> {
        Cow::Owned(samples.iter().map(|&s| f32::from(s)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u16_narrowing_clamps_and_rounds_half_up() {
        let cases = [
            (-3.0, 0),
            (0.49, 0),
            (0.5, 1),
            (1.5, 2),
            (65_534.5, 65_535),
            (70_000.0, 65_535),
        ];
        for (value, expected) in cases {
            assert_eq!(u16::narrow(value), expected, "{value}");
        }
    }

    #[test]
    fn u16_narrow_clips_every_f32() {
        let cases = [
            (f32::NAN, 0),
            (f32::NEG_INFINITY, 0),
            (f32::INFINITY, 65_535),
            (-0.0, 0),
            (f32::MAX, 65_535),
            (f32::MIN, 0),
            (0.5, 1),
            (65_534.5, 65_535),
            (65_535.0, 65_535),
            (65_535.5, 65_535),
        ];
        for (value, expected) in cases {
            assert_eq!(u16::narrow(value), expected, "{value}");
        }

        let sweep: Vec<f32> = (-40..=40)
            .map(|i| -2.0 + i as f32 * 0.05)
            .chain((0..=2_000).map(|i| i as f32 * 32.75))
            .chain((-40..=40).map(|i| 65_535.0 + i as f32 * 0.05))
            .collect();
        for pair in sweep.windows(2) {
            assert!(
                u16::narrow(pair[0]) <= u16::narrow(pair[1]),
                "{} then {}",
                pair[0],
                pair[1]
            );
        }
        for &value in &sweep {
            let expected = (f64::from(value).clamp(0.0, 65_535.0) + 0.5).floor() as u16;
            assert_eq!(u16::narrow(value), expected, "{value}");
        }
    }

    #[test]
    fn f32_narrowing_keeps_the_value() {
        for value in [-3.5_f32, 0.25, 65_535.0, 98_304.0] {
            assert_eq!(f32::narrow(value), value);
        }
    }

    #[test]
    fn formats_know_their_width() {
        assert_eq!(SampleFormat::U16.bytes(), 2);
        assert_eq!(SampleFormat::F32.bytes(), 4);
        assert_eq!(<u16 as Sample>::FORMAT, SampleFormat::U16);
        assert_eq!(<f32 as Sample>::FORMAT, SampleFormat::F32);
    }
}
