// SPDX-License-Identifier: GPL-3.0-or-later

//! Opt-in brightness equalization: one gain per source, the same on every
//! channel, applied to encoded code values (not a linear-light correction).
//! The gains bring every source's whole-image mean to one shared target and
//! are fixed before alignment. Gains are Q16 (`65536` is 1.0), so every
//! reader applies identical integer arithmetic.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::algorithm::{AlgorithmError, TileReader};
use crate::tile::Rect;

/// Gain 1.0 in Q16 fixed point.
pub const UNIT_GAIN: u32 = 1 << 16;

/// What a run's equalization is asked for: the bound on every gain and the
/// shared target's offset, in stops, from the sources' average brightness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqualizeSettings {
    /// Gains stay within `1/max_gain..=max_gain`.
    pub max_gain: f64,
    /// The target is the sources' mean brightness times `2^target_stops`.
    pub target_stops: f64,
}

/// The recorded outcome of an equalization: the settings it ran with and
/// every frame's applied gain, index == frame index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Equalization {
    pub max_gain: f64,
    #[serde(default)]
    pub target_stops: f64,
    pub gains: Vec<u32>,
}

/// What [`estimate`] found for one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameEstimate {
    /// The frame's mean encoded value over every sample.
    pub mean: f64,
    /// The gain that would reach the target, before the bound.
    pub estimated: f64,
    /// The gain used: `estimated` limited to the bound, or 1.0 for a black
    /// frame.
    pub applied: f64,
    /// `applied` as recorded, in Q16.
    pub gain: u32,
    pub clamped: bool,
    /// The frame's mean is zero, so no gain reaches a nonzero target.
    pub black: bool,
    /// Samples below 65535 that the gain saturates to 65535.
    pub clipped_samples: u64,
    /// The mean after the recorded gain, saturation included.
    pub output_mean: f64,
    /// `output_mean` minus the target, in code values.
    pub residual: f64,
}

impl Equalization {
    /// The recorded form of `estimates`: each frame's Q16 gain.
    pub fn new(settings: EqualizeSettings, estimates: &[FrameEstimate]) -> Self {
        Equalization {
            max_gain: settings.max_gain,
            target_stops: settings.target_stops,
            gains: estimates.iter().map(|e| e.gain).collect(),
        }
    }
}

/// A run's full estimate: the settings, the shared neutral and target
/// brightness and every frame's [`FrameEstimate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqualizationReport {
    pub max_gain: f64,
    pub target_stops: f64,
    /// The mean of the frames' means.
    pub neutral: f64,
    /// `neutral * 2^target_stops`.
    pub target: f64,
    pub frames: Vec<FrameEstimate>,
}

impl EqualizationReport {
    /// The recorded form: the settings and each applied gain in Q16.
    pub fn recorded(&self) -> Equalization {
        Equalization::new(
            EqualizeSettings {
                max_gain: self.max_gain,
                target_stops: self.target_stops,
            },
            &self.frames,
        )
    }

    /// The notices the frames raise with the index of the frame each names,
    /// in frame order.
    pub fn notices(&self) -> impl Iterator<Item = (usize, String)> + '_ {
        self.frames.iter().enumerate().filter_map(|(index, f)| {
            f.notice(index, self.max_gain, self.target)
                .map(|text| (index, text))
        })
    }
}

impl FrameEstimate {
    /// The smallest bound, in hundredths, under which this frame would not
    /// be clamped.
    pub fn covering_bound(&self) -> f64 {
        (self.estimated.max(1.0 / self.estimated) * 100.0).ceil() / 100.0
    }

    /// The notice a black, clamped or clipped frame raises, `None` for a
    /// frame that reached the target. A clip raises one only when it leaves
    /// the mean more than one code value from `target`.
    pub fn notice(&self, frame: usize, max_gain: f64, target: f64) -> Option<String> {
        let reached = format!(
            "mean {:.1} against the target {target:.1} (residual {:+.1})",
            self.output_mean, self.residual
        );
        if self.black {
            Some(format!(
                "frame {frame}: the source is black, so brightness equalization keeps its gain \
                 at 1.0 and the target {target:.1} is unreachable ({reached})"
            ))
        } else if self.clamped {
            Some(format!(
                "frame {frame}: brightness equalization needs a gain of {:.3} but the bound \
                 {max_gain:.2} limits it to {:.3}; a bound of {:.2} would cover it; {reached}",
                self.estimated,
                self.applied,
                self.covering_bound()
            ))
        } else if self.clipped_samples > 0 && self.residual.abs() > 1.0 {
            Some(format!(
                "frame {frame}: brightness equalization clips {} samples at white and misses \
                 the target ({reached})",
                self.clipped_samples
            ))
        } else {
            None
        }
    }
}

/// `ratio` in Q16, rounded half up.
fn to_q16(ratio: f64) -> u32 {
    (ratio * f64::from(UNIT_GAIN) + 0.5).floor() as u32
}

/// One sample times the Q16 `gain`, rounded to nearest and saturated at
/// 65535.
fn gained(sample: u16, gain: u32) -> u16 {
    ((u64::from(sample) * u64::from(gain) + u64::from(UNIT_GAIN / 2)) >> 16).min(65_535) as u16
}

/// Multiplies each sample by the Q16 `gain`.
fn apply_gain(samples: &mut [u16], gain: u32) {
    if gain == UNIT_GAIN {
        return;
    }
    for s in samples {
        *s = gained(*s, gain);
    }
}

/// A [`TileReader`] that multiplies every sample of frame `i` by
/// `gains[i]` (Q16), saturating at 65535. A frame at unit gain, or past the
/// end of `gains`, is read straight through.
pub struct GainReader<'a> {
    inner: &'a dyn TileReader,
    gains: &'a [u32],
}

impl<'a> GainReader<'a> {
    pub fn new(inner: &'a dyn TileReader, gains: &'a [u32]) -> Self {
        GainReader { inner, gains }
    }

    fn gain(&self, frame: usize) -> u32 {
        self.gains.get(frame).copied().unwrap_or(UNIT_GAIN)
    }
}

impl TileReader for GainReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.inner.read_tile_into(frame, rect, out)?;
        apply_gain(out, self.gain(frame));
        Ok(())
    }

    fn read_tile_coverage_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
        let coverage = self.inner.read_tile_coverage_into(frame, rect, out)?;
        apply_gain(out, self.gain(frame));
        Ok(coverage)
    }

    fn read_scratch_bytes(&self, width: u32, height: u32) -> u64 {
        self.inner.read_scratch_bytes(width, height)
    }

    fn read_tile_step_into(
        &self,
        frame: usize,
        rect: Rect,
        step: NonZeroU32,
        samples_per_pixel: u32,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.inner
            .read_tile_step_into(frame, rect, step, samples_per_pixel, out)?;
        apply_gain(out, self.gain(frame));
        Ok(())
    }
}

const BINS: usize = 1 << 16;

/// Samples held by one strip buffer, about.
const STRIP_SAMPLES: usize = 1 << 21;

/// One gain per frame bringing its whole-image mean (every sample of every
/// channel) to `neutral * 2^target_stops`, `neutral` being the mean of the
/// frames' means. The gain is `target / mean` limited to
/// `[1/max_gain, max_gain]`, in Q16; a zero-mean frame keeps unit gain and is
/// flagged black. `cancelled` is polled before every strip read.
pub fn estimate(
    reader: &dyn TileReader,
    frame_count: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    settings: EqualizeSettings,
    cancelled: &dyn Fn() -> bool,
) -> Result<EqualizationReport, AlgorithmError> {
    let row_samples = (frame.0 as usize * samples_per_pixel as usize).max(1);
    let strip_rows = (STRIP_SAMPLES / row_samples).clamp(1, frame.1.max(1) as usize) as u32;
    estimate_in_strips(
        reader,
        frame_count,
        frame,
        samples_per_pixel,
        settings,
        strip_rows,
        cancelled,
    )
}

fn estimate_in_strips(
    reader: &dyn TileReader,
    frame_count: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    settings: EqualizeSettings,
    strip_rows: u32,
    cancelled: &dyn Fn() -> bool,
) -> Result<EqualizationReport, AlgorithmError> {
    let (width, height) = frame;
    let row_samples = width as usize * samples_per_pixel as usize;
    let mut strip = vec![0u16; row_samples * strip_rows as usize];
    // The gain needs every frame's mean first, and the output statistics
    // need the gain, so each frame's histogram is kept for that second step.
    let mut histograms: Vec<Box<[u64]>> = Vec::with_capacity(frame_count);
    for index in 0..frame_count {
        let mut histogram = vec![0u64; BINS].into_boxed_slice();
        let mut y = 0;
        while y < height {
            if cancelled() {
                return Err(AlgorithmError::Cancelled);
            }
            let rows = strip_rows.min(height - y);
            let samples = &mut strip[..row_samples * rows as usize];
            let rect = Rect {
                x: 0,
                y,
                width,
                height: rows,
            };
            reader.read_tile_into(index, rect, samples)?;
            for &s in samples.iter() {
                histogram[usize::from(s)] += 1;
            }
            y += rows;
        }
        histograms.push(histogram);
    }

    // Every frame holds the same number of samples, so the mean of the
    // frames' means is the total sum over the total count, rounded once.
    let totals: Vec<(u64, u64)> = histograms
        .iter()
        .map(|h| histogram_totals(h, |v| v))
        .collect();
    let means: Vec<f64> = totals
        .iter()
        .map(|&(sum, count)| mean_of(sum, count))
        .collect();
    let neutral = mean_of(
        totals.iter().map(|t| t.0).sum(),
        totals.iter().map(|t| t.1).sum(),
    );
    let target = neutral * settings.target_stops.exp2();
    let frames = histograms
        .iter()
        .zip(&means)
        .map(|(histogram, &mean)| frame_estimate(histogram, mean, target, settings.max_gain))
        .collect();
    Ok(EqualizationReport {
        max_gain: settings.max_gain,
        target_stops: settings.target_stops,
        neutral,
        target,
        frames,
    })
}

/// The sum and count of `map(value)` over every sample `histogram` counts.
fn histogram_totals(histogram: &[u64], map: impl Fn(u16) -> u16) -> (u64, u64) {
    let (mut sum, mut count) = (0u64, 0u64);
    for (value, &n) in histogram.iter().enumerate() {
        sum += n * u64::from(map(value as u16));
        count += n;
    }
    (sum, count)
}

fn mean_of(sum: u64, count: u64) -> f64 {
    sum as f64 / count.max(1) as f64
}

fn frame_estimate(histogram: &[u64], mean: f64, target: f64, max_gain: f64) -> FrameEstimate {
    if mean == 0.0 {
        return FrameEstimate {
            mean,
            estimated: 1.0,
            applied: 1.0,
            gain: UNIT_GAIN,
            clamped: false,
            black: true,
            clipped_samples: 0,
            output_mean: 0.0,
            residual: -target,
        };
    }
    let estimated = target / mean;
    let applied = estimated.clamp(1.0 / max_gain, max_gain);
    let gain = to_q16(applied);
    let clipped_samples = histogram[..BINS - 1]
        .iter()
        .enumerate()
        .filter(|&(value, _)| gained(value as u16, gain) == u16::MAX)
        .map(|(_, &n)| n)
        .sum();
    let (sum, count) = histogram_totals(histogram, |v| gained(v, gain));
    let output_mean = mean_of(sum, count);
    FrameEstimate {
        mean,
        estimated,
        applied,
        gain,
        clamped: applied != estimated,
        black: false,
        clipped_samples,
        output_mean,
        residual: output_mean - target,
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::align::Registration;

    /// Frames held in memory, optionally with a per-frame coverage mask that
    /// leaves the stored (non-zero) values in place, like a reader would not.
    struct Stack {
        frames: Vec<Vec<u16>>,
        masks: Vec<Option<Vec<bool>>>,
        width: u32,
        spp: u32,
    }

    impl Stack {
        fn new(frames: Vec<Vec<u16>>, width: u32, spp: u32) -> Self {
            let masks = vec![None; frames.len()];
            Stack {
                frames,
                masks,
                width,
                spp,
            }
        }
    }

    impl TileReader for Stack {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            self.read_tile_coverage_into(frame, rect, out).map(drop)
        }

        fn read_tile_coverage_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<Option<Vec<bool>>, AlgorithmError> {
            let spp = self.spp as usize;
            let mut at = 0;
            let mut mask = Vec::new();
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    let px = (y * self.width + x) as usize;
                    out[at..at + spp].copy_from_slice(&self.frames[frame][px * spp..][..spp]);
                    at += spp;
                    mask.push(self.masks[frame].as_ref().is_none_or(|m| m[px]));
                }
            }
            Ok(self.masks[frame].is_some().then_some(mask))
        }

        fn read_scratch_bytes(&self, width: u32, height: u32) -> u64 {
            u64::from(width) * u64::from(height) + 7
        }
    }

    /// A deterministic mid-tone texture in 15000..45000, tinted per channel
    /// when `spp` is 3.
    fn reference(width: u32, height: u32, spp: u32) -> Vec<u16> {
        let mut state = 0x2545_f491_u32;
        let mut samples = Vec::new();
        for _ in 0..width * height {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let base = 15_000.0 + f64::from(state >> 16) / 65_535.0 * 30_000.0;
            for c in 0..spp {
                let tint = [1.0, 0.8, 0.6][c as usize];
                samples.push((base * if spp == 3 { tint } else { 1.0 }).round() as u16);
            }
        }
        samples
    }

    fn scaled(samples: &[u16], f: f64) -> Vec<u16> {
        samples
            .iter()
            .map(|&s| (f64::from(s) * f).round().min(65_535.0) as u16)
            .collect()
    }

    /// Frames of `factors` times one texture, rotated per frame so the content
    /// differs between sources.
    fn shifted_stack(width: u32, height: u32, spp: u32, factors: &[f64]) -> Stack {
        let base = reference(width, height, spp);
        let frames = factors
            .iter()
            .enumerate()
            .map(|(i, &f)| {
                let mut samples = scaled(&base, f);
                samples.rotate_left(i * 11 * spp as usize);
                samples
            })
            .collect();
        Stack::new(frames, width, spp)
    }

    fn settings(max_gain: f64, target_stops: f64) -> EqualizeSettings {
        EqualizeSettings {
            max_gain,
            target_stops,
        }
    }

    fn never() -> bool {
        false
    }

    fn mean_of(samples: &[u16]) -> f64 {
        samples.iter().map(|&s| f64::from(s)).sum::<f64>() / samples.len() as f64
    }

    fn read_whole(reader: &dyn TileReader, frame: usize, w: u32, h: u32, spp: u32) -> Vec<u16> {
        let mut out = vec![0u16; (w * h * spp) as usize];
        let rect = Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        reader.read_tile_into(frame, rect, &mut out).unwrap();
        out
    }

    #[test]
    fn mean_target_converges_within_one_code_value() {
        for spp in [1, 3] {
            let (w, h) = (37, 23);
            let stack = shifted_stack(w, h, spp, &[0.8, 1.0, 1.1]);
            // 5 rows per strip does not divide the 23-row height.
            let report =
                estimate_in_strips(&stack, 3, (w, h), spp, settings(1.5, 0.0), 5, &never).unwrap();
            let gains = report.recorded().gains;
            let gained = GainReader::new(&stack, &gains);
            for (i, f) in report.frames.iter().enumerate() {
                assert!(!f.clamped && !f.black, "spp {spp} frame {i}: {f:?}");
                assert!(
                    (f.output_mean - report.target).abs() <= 1.0,
                    "spp {spp} frame {i}: {f:?} target {}",
                    report.target
                );
                let read = mean_of(&read_whole(&gained, i, w, h, spp));
                assert!(
                    (read - f.output_mean).abs() < 1e-9,
                    "the histogram's output mean is the reader's"
                );
            }
        }
    }

    #[test]
    fn target_stops_scale_the_neutral_mean() {
        let (w, h) = (37, 23);
        let stack = shifted_stack(w, h, 3, &[0.9, 1.0, 1.1]);
        let neutral = (0..3)
            .map(|i| mean_of(&read_whole(&stack, i, w, h, 3)))
            .sum::<f64>()
            / 3.0;
        for stops in [-0.5, 0.0, 0.5] {
            let report = estimate(&stack, 3, (w, h), 3, settings(2.0, stops), &never).unwrap();
            assert!((report.neutral - neutral).abs() < 1e-9);
            assert!((report.target - neutral * 2f64.powf(stops)).abs() < 1e-9);
            assert_eq!((report.max_gain, report.target_stops), (2.0, stops));
            for f in &report.frames {
                assert!(
                    (f.output_mean - report.target).abs() <= 1.0,
                    "{stops}: {f:?}"
                );
            }
        }
    }

    #[test]
    fn q16_rounds_half_up() {
        assert_eq!(to_q16(1.0), UNIT_GAIN);
        assert_eq!(to_q16(1.5 / 65_536.0), 2);
        assert_eq!(to_q16(2.5 / 65_536.0), 3);
        assert_eq!(to_q16(2.49 / 65_536.0), 2);
    }

    #[test]
    fn zero_mean_source_keeps_unit_gain_and_reports_unreachable_target() {
        let (w, h) = (16, 8);
        let mut stack = shifted_stack(w, h, 1, &[1.0, 1.0]);
        stack.frames[0] = vec![0; (w * h) as usize];
        let report = estimate(&stack, 2, (w, h), 1, settings(2.0, 0.0), &never).unwrap();
        let black = &report.frames[0];
        assert!(black.black && !black.clamped);
        assert_eq!((black.gain, black.applied), (UNIT_GAIN, 1.0));
        assert!((black.residual + report.target).abs() < 1e-9);
        assert!(report.target > 0.0, "the black source still weighs in");
        let text = black.notice(0, 2.0, report.target).expect("black notifies");
        assert!(
            text.contains("frame 0") && text.contains("unreachable"),
            "{text}"
        );
        assert!(report.frames[1].notice(1, 2.0, report.target).is_none());
    }

    #[test]
    fn gain_bound_clamps_and_reports_covering_bound_and_residual() {
        let (w, h) = (37, 23);
        let stack = shifted_stack(w, h, 1, &[1.0, 1.3]);
        let report = estimate(&stack, 2, (w, h), 1, settings(1.1, 0.0), &never).unwrap();
        for (i, f) in report.frames.iter().enumerate() {
            assert!(f.clamped, "frame {i}: {f:?}");
            assert!(f.applied == 1.1 || (f.applied - 1.0 / 1.1).abs() < 1e-12);
            assert!((f.residual - (f.output_mean - report.target)).abs() < 1e-9);
            assert!(f.residual.abs() > 1.0);
            let cover = f.covering_bound();
            assert!(cover >= f.estimated.max(1.0 / f.estimated));
            let text = f.notice(i, 1.1, report.target).expect("clamped notifies");
            assert!(text.contains(&format!("frame {i}")), "{text}");
            assert!(text.contains(&format!("{:.3}", f.estimated)), "{text}");
            assert!(
                text.contains("1.10") && text.contains(&format!("{cover:.2}")),
                "{text}"
            );
            assert!(text.contains(&format!("{:+.1}", f.residual)), "{text}");
        }
    }

    #[test]
    fn saturating_gain_counts_newly_clipped_samples() {
        // Frame 1 gains about 1.2: its 65535 samples are already saturated
        // and do not count, its 62000 samples clip.
        let (w, h) = (8, 4);
        let quad = |q: [u16; 4]| -> Vec<u16> { (0..w * h).map(|i| q[(i % 4) as usize]).collect() };
        let stack = Stack::new(
            vec![
                quad([65_535, 40_000, 65_535, 40_000]),
                quad([65_535, 62_000, 10_000, 10_000]),
            ],
            w,
            1,
        );
        let report = estimate(&stack, 2, (w, h), 1, settings(2.0, 0.0), &never).unwrap();
        let f = &report.frames[1];
        assert!(f.applied > 1.0);
        assert_eq!(f.clipped_samples, u64::from(w * h) / 4);
        let gains = report.recorded().gains;
        let gained = read_whole(&GainReader::new(&stack, &gains), 1, w, h, 1);
        let truth = gained
            .iter()
            .zip(&stack.frames[1])
            .filter(|&(&g, &s)| g == 65_535 && s < 65_535)
            .count() as u64;
        assert_eq!(f.clipped_samples, truth);
        assert!((mean_of(&gained) - f.output_mean).abs() < 1e-9);
        assert!(f.residual < -1.0);
        let text = f
            .notice(1, 2.0, report.target)
            .expect("a clip that misses the target notifies");
        assert!(text.contains("clip") && text.contains("frame 1"), "{text}");
        assert_eq!(report.frames[0].clipped_samples, 0);
    }

    #[test]
    fn a_clip_within_one_code_value_of_the_target_raises_no_notice() {
        let quiet = FrameEstimate {
            mean: 100.0,
            estimated: 1.2,
            applied: 1.2,
            gain: 78_643,
            clamped: false,
            black: false,
            clipped_samples: 3,
            output_mean: 120.0,
            residual: -0.9,
        };
        assert!(quiet.notice(2, 1.25, 120.9).is_none());
        let missed = FrameEstimate {
            residual: -1.5,
            ..quiet.clone()
        };
        assert!(missed.notice(2, 1.25, 121.5).is_some());
        let unclipped = FrameEstimate {
            clipped_samples: 0,
            residual: -1.5,
            ..quiet
        };
        assert!(unclipped.notice(2, 1.25, 121.5).is_none());
    }

    #[test]
    fn cancel_between_strips_returns_cancelled() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (w, h) = (37, 23);
        let stack = shifted_stack(w, h, 1, &[1.0, 1.0]);
        let polls = AtomicUsize::new(0);
        let cancel_on_third = || polls.fetch_add(1, Ordering::SeqCst) == 2;
        let result = estimate_in_strips(
            &stack,
            2,
            (w, h),
            1,
            settings(1.25, 0.0),
            5,
            &cancel_on_third,
        );
        assert!(matches!(result, Err(AlgorithmError::Cancelled)));
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn one_gain_on_every_channel_keeps_the_frames_own_channel_ratios() {
        let (w, h) = (64, 32);
        let ref_samples = reference(w, h, 3);
        let mut tinted = scaled(&ref_samples, 0.8);
        for px in tinted.chunks_mut(3) {
            px[0] = (f64::from(px[0]) * 0.9 / 0.8).round() as u16;
            px[2] = (f64::from(px[2]) * 1.1 / 0.8).round() as u16;
        }
        let stack = Stack::new(vec![ref_samples, tinted.clone()], w, 3);
        let report = estimate(&stack, 2, (w, h), 3, settings(2.0, 0.0), &never).unwrap();
        let gains = report.recorded().gains;
        let gained = read_whole(&GainReader::new(&stack, &gains), 1, w, h, 3);
        for (a, b) in gained.chunks(3).zip(tinted.chunks(3)).step_by(7) {
            let (ra, rb) = (
                f64::from(a[0]) / f64::from(a[1]),
                f64::from(b[0]) / f64::from(b[1]),
            );
            assert!((ra - rb).abs() < 1e-3, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn gain_reader_at_unit_is_a_pass_through_and_forwards_scratch_and_coverage() {
        let mut stack = shifted_stack(64, 32, 1, &[1.0, 0.9]);
        stack.masks[1] = Some(vec![true; 64 * 32]);
        let gains = [UNIT_GAIN, UNIT_GAIN];
        let reader = GainReader::new(&stack, &gains);
        let rect = Rect {
            x: 3,
            y: 2,
            width: 20,
            height: 5,
        };
        let (mut a, mut b) = (vec![0u16; 100], vec![0u16; 100]);
        let cov = reader.read_tile_coverage_into(1, rect, &mut a).unwrap();
        let want = stack.read_tile_coverage_into(1, rect, &mut b).unwrap();
        assert_eq!((a, cov), (b, want));
        assert_eq!(
            reader.read_scratch_bytes(8, 8),
            stack.read_scratch_bytes(8, 8)
        );
        let short = GainReader::new(&stack, &gains[..1]);
        let mut c = vec![0u16; 100];
        short.read_tile_into(1, rect, &mut c).unwrap();
        assert_eq!(c, {
            let mut d = vec![0u16; 100];
            stack.read_tile_into(1, rect, &mut d).unwrap();
            d
        });
    }

    #[test]
    fn gain_reader_applies_the_integer_formula_to_a_full_range_ramp_and_saturates() {
        let ramp: Vec<u16> = (0..=65_535u32).map(|v| v as u16).collect();
        let stack = Stack::new(vec![ramp.clone()], 256, 1);
        let gain = 81_920; // 1.25
        let gains = [gain];
        let reader = GainReader::new(&stack, &gains);
        let mut out = vec![0u16; ramp.len()];
        let rect = Rect {
            x: 0,
            y: 0,
            width: 256,
            height: 256,
        };
        reader.read_tile_into(0, rect, &mut out).unwrap();
        for (i, (&got, &s)) in out.iter().zip(&ramp).enumerate() {
            let want = ((u64::from(s) * u64::from(gain) + 32_768) >> 16).min(65_535) as u16;
            assert_eq!(got, want, "sample {i}");
        }
        assert_eq!(
            out[65_535], 65_535,
            "a near-white sample saturates, never wraps"
        );
        assert!(out[60_000] == 65_535 && out[52_428] == 65_535);
    }

    #[test]
    fn gain_reader_applies_the_gain_on_stepped_reads() {
        let stack = Stack::new(vec![vec![40_000u16; 64 * 64]], 64, 1);
        let gains = [32_768];
        let reader = GainReader::new(&stack, &gains);
        let rect = Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        };
        let mut out = vec![0u16; 16 * 16];
        reader
            .read_tile_step_into(0, rect, NonZeroU32::new(4).unwrap(), 1, &mut out)
            .unwrap();
        assert!(out.iter().all(|&s| s == 20_000));
    }

    #[test]
    fn equalization_round_trips_through_json_and_an_absent_one_stays_none() {
        let eq = Equalization {
            max_gain: 1.37,
            target_stops: -0.3,
            gains: vec![UNIT_GAIN, 52_429, 81_920, 1, 131_072],
        };
        let back: Equalization =
            serde_json::from_str(&serde_json::to_string(&eq).unwrap()).unwrap();
        assert_eq!(back, eq);

        let without: Equalization =
            serde_json::from_str(r#"{"max_gain":1.25,"gains":[65536]}"#).unwrap();
        assert_eq!(
            without.target_stops, 0.0,
            "a record without a target reads 0.0"
        );

        let plain = Registration::default();
        assert_eq!(plain.gain(3), UNIT_GAIN);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("equalization"), "{json}");
        let back_plain: Registration = serde_json::from_str(&json).unwrap();
        assert!(back_plain.equalization.is_none());

        let with = Registration {
            equalization: Some(eq.clone()),
            ..Registration::default()
        };
        assert_eq!(with.gain(1), 52_429);
        assert_eq!(with.gain(9), UNIT_GAIN);
        let back: Registration =
            serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert_eq!(back, with);
    }
}
