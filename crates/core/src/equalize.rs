// SPDX-License-Identifier: GPL-3.0-or-later

//! Opt-in brightness equalization: one gain per frame, the same on every
//! channel, applied to encoded code values (not a linear-light correction).
//! Gains are Q16 (`65536` is 1.0), so every reader applies identical integer
//! arithmetic.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::algorithm::{AlgorithmError, TileReader};
use crate::tile::Rect;

/// Gain 1.0 in Q16 fixed point.
pub const UNIT_GAIN: u32 = 1 << 16;

/// A frame with fewer usable samples than this keeps unit gain.
pub const MIN_SAMPLES: usize = 1000;

/// The recorded outcome of an equalization: the bound it ran with and every
/// frame's applied gain, index == frame index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Equalization {
    pub max_gain: f64,
    pub gains: Vec<u32>,
}

/// What [`estimate`] found for one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameEstimate {
    /// The gain that would match the reference, before the bound.
    pub estimated: f64,
    /// The gain used: `estimated` limited to the bound, or 1.0 when thin.
    pub applied: f64,
    pub clamped: bool,
    pub thin: bool,
    /// Samples the median was taken over.
    pub samples: usize,
}

impl Equalization {
    /// The recorded form of `estimates`: each applied gain in Q16.
    pub fn new(max_gain: f64, estimates: &[FrameEstimate]) -> Self {
        Equalization {
            max_gain,
            gains: estimates
                .iter()
                .map(|e| (e.applied * f64::from(UNIT_GAIN)).round() as u32)
                .collect(),
        }
    }
}

/// A run's full estimate: the bound, the reference frame and every frame's
/// [`FrameEstimate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqualizationReport {
    pub max_gain: f64,
    pub reference: usize,
    pub frames: Vec<FrameEstimate>,
}

impl EqualizationReport {
    /// The recorded form: the bound and each applied gain in Q16.
    pub fn recorded(&self) -> Equalization {
        Equalization::new(self.max_gain, &self.frames)
    }
}

impl FrameEstimate {
    /// The smallest bound, in hundredths, under which this frame would not
    /// be clamped.
    pub fn covering_bound(&self) -> f64 {
        (self.estimated.max(1.0 / self.estimated) * 100.0).ceil() / 100.0
    }

    /// The notice a clamped or thin frame raises, `None` for a frame that
    /// equalized cleanly.
    pub fn notice(&self, frame: usize, max_gain: f64) -> Option<String> {
        if self.clamped {
            Some(format!(
                "frame {frame}: brightness equalization needs a gain of {:.3} but the bound \
                 {max_gain:.2} limits it to {:.3}; a bound of {:.2} would cover it",
                self.estimated,
                self.applied,
                self.covering_bound()
            ))
        } else if self.thin {
            Some(format!(
                "frame {frame}: only {} usable mid-tone samples, so brightness equalization \
                 leaves its gain at 1.0",
                self.samples
            ))
        } else {
            None
        }
    }
}

/// Multiplies each sample by the Q16 `gain`, rounding to nearest and
/// saturating at 65535.
fn apply_gain(samples: &mut [u16], gain: u32) {
    if gain == UNIT_GAIN {
        return;
    }
    for s in samples {
        *s =
            ((u64::from(*s) * u64::from(gain) + u64::from(UNIT_GAIN / 2)) >> 16).min(65_535) as u16;
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

/// The reference's mid-tone luma band, in code values.
const BAND: (f64, f64) = (0.05 * 65_535.0, 0.95 * 65_535.0);

/// Samples read per frame, about; the sampling step is sized to it.
const TARGET_SAMPLES: f64 = 262_144.0;

/// One gain per frame matching `reference`'s mid-tone brightness, read
/// through `reader` (normally the warped one, so coverage is the fuse's):
/// the median reference-over-frame luma ratio (channel mean) on sampled
/// pixels both cover, reference luma in the band, limited to
/// `[1/max_gain, max_gain]`. Under [`MIN_SAMPLES`] pixels: unit, flagged thin.
pub fn estimate(
    reader: &dyn TileReader,
    frame_count: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    reference: usize,
    max_gain: f64,
) -> Result<Vec<FrameEstimate>, AlgorithmError> {
    let (width, height) = frame;
    let spp = samples_per_pixel as usize;
    let step = ((f64::from(width) * f64::from(height) / TARGET_SAMPLES)
        .sqrt()
        .ceil() as usize)
        .max(1);
    let rows: Vec<u32> = (0..height).step_by(step).collect();
    let columns = (width as usize).div_ceil(step);
    let mut line = vec![0u16; width as usize * spp];
    let read_line = |frame_index: usize,
                     y: u32,
                     line: &mut [u16]|
     -> Result<Option<Vec<bool>>, AlgorithmError> {
        let rect = Rect {
            x: 0,
            y,
            width,
            height: 1,
        };
        reader.read_tile_coverage_into(frame_index, rect, line)
    };
    let luma = |line: &[u16], x: usize| {
        line[x * spp..(x + 1) * spp]
            .iter()
            .map(|&s| f64::from(s))
            .sum::<f64>()
            / spp as f64
    };

    // Reference luma on the sampled grid; 0.0 marks a sample that is
    // uncovered or outside the band.
    let mut reference_luma = Vec::with_capacity(rows.len() * columns);
    for &y in &rows {
        let coverage = read_line(reference, y, &mut line)?;
        for x in (0..width as usize).step_by(step) {
            let l = luma(&line, x);
            let usable = coverage.as_ref().is_none_or(|c| c[x]) && (BAND.0..=BAND.1).contains(&l);
            reference_luma.push(if usable { l } else { 0.0 });
        }
    }

    let mut ratios = Vec::new();
    (0..frame_count)
        .map(|f| {
            ratios.clear();
            for (r, &y) in rows.iter().enumerate() {
                let coverage = read_line(f, y, &mut line)?;
                for (c, x) in (0..width as usize).step_by(step).enumerate() {
                    let wanted = reference_luma[r * columns + c];
                    let l = luma(&line, x);
                    if wanted > 0.0 && l > 0.0 && coverage.as_ref().is_none_or(|m| m[x]) {
                        ratios.push(wanted / l);
                    }
                }
            }
            Ok(summarize(&mut ratios, max_gain))
        })
        .collect()
}

/// The median of `ratios` as a [`FrameEstimate`].
fn summarize(ratios: &mut [f64], max_gain: f64) -> FrameEstimate {
    let samples = ratios.len();
    if samples < MIN_SAMPLES {
        return FrameEstimate {
            estimated: 1.0,
            applied: 1.0,
            clamped: false,
            thin: true,
            samples,
        };
    }
    ratios.sort_unstable_by(f64::total_cmp);
    let estimated = if samples % 2 == 1 {
        ratios[samples / 2]
    } else {
        (ratios[samples / 2 - 1] + ratios[samples / 2]) / 2.0
    };
    let applied = estimated.clamp(1.0 / max_gain, max_gain);
    FrameEstimate {
        estimated,
        applied,
        clamped: applied != estimated,
        thin: false,
        samples,
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

    fn stack_of(reference_samples: Vec<u16>, others: &[f64], width: u32, spp: u32) -> Stack {
        let mut frames = vec![reference_samples.clone()];
        frames.extend(others.iter().map(|&f| scaled(&reference_samples, f)));
        Stack::new(frames, width, spp)
    }

    const W: u32 = 640;
    const H: u32 = 480;
    const FACTORS: [f64; 4] = [0.85, 0.92, 1.06, 1.18];

    #[test]
    fn gains_recover_the_inverse_factor_and_equalize_within_two_lsb() {
        for spp in [1, 3] {
            let ref_samples = reference(W, H, spp);
            let stack = stack_of(ref_samples.clone(), &FACTORS, W, spp);
            let found = estimate(&stack, 5, (W, H), spp, 0, 1.25).unwrap();
            assert_eq!(found[0].applied, 1.0, "the reference is exactly unit");
            let eq = Equalization::new(1.25, &found);
            assert_eq!(eq.gains[0], UNIT_GAIN);
            let gained = GainReader::new(&stack, &eq.gains);
            for (k, f) in FACTORS.iter().enumerate() {
                let est = &found[k + 1];
                assert!(
                    (est.estimated - 1.0 / f).abs() < 5e-4,
                    "spp {spp} f {f}: {est:?}"
                );
                assert!(!est.clamped && !est.thin);
                let mut out = vec![0u16; ref_samples.len()];
                let full = Rect {
                    x: 0,
                    y: 0,
                    width: W,
                    height: H,
                };
                gained.read_tile_into(k + 1, full, &mut out).unwrap();
                let worst = out
                    .iter()
                    .zip(&ref_samples)
                    .map(|(&a, &b)| a.abs_diff(b))
                    .max()
                    .unwrap();
                assert!(worst <= 2, "spp {spp} f {f}: worst {worst} LSB");
            }
        }
    }

    #[test]
    fn one_gain_on_every_channel_keeps_the_frames_own_channel_ratios() {
        let ref_samples = reference(W, H, 3);
        let mut tinted = scaled(&ref_samples, 0.8);
        for px in tinted.chunks_mut(3) {
            px[0] = (f64::from(px[0]) * 0.9 / 0.8).round() as u16;
            px[2] = (f64::from(px[2]) * 1.1 / 0.8).round() as u16;
        }
        let stack = Stack::new(vec![ref_samples, tinted.clone()], W, 3);
        let found = estimate(&stack, 2, (W, H), 3, 0, 2.0).unwrap();
        let eq = Equalization::new(2.0, &found);
        let gained = GainReader::new(&stack, &eq.gains);
        let mut out = vec![0u16; tinted.len()];
        let full = Rect {
            x: 0,
            y: 0,
            width: W,
            height: H,
        };
        gained.read_tile_into(1, full, &mut out).unwrap();
        for (a, b) in out.chunks(3).zip(tinted.chunks(3)).step_by(97) {
            let (ra, rb) = (
                f64::from(a[0]) / f64::from(a[1]),
                f64::from(b[0]) / f64::from(b[1]),
            );
            assert!((ra - rb).abs() < 1e-3, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn a_frame_beyond_the_bound_is_clamped_both_ways_and_flagged() {
        let ref_samples = reference(W, H, 1);
        let stack = stack_of(ref_samples, &[0.8, 1.25, 1.05], W, 1);
        let found = estimate(&stack, 4, (W, H), 1, 0, 1.1).unwrap();
        assert!(found[1].clamped && (found[1].applied - 1.1).abs() < 1e-12);
        assert!((found[1].estimated - 1.25).abs() < 5e-4);
        assert!(found[2].clamped && (found[2].applied - 1.0 / 1.1).abs() < 1e-12);
        assert!(!found[3].clamped && (found[3].applied - found[3].estimated).abs() < 1e-12);
        let eq = Equalization::new(1.1, &found);
        assert_eq!(eq.gains[1], (1.1f64 * 65_536.0).round() as u32);
    }

    #[test]
    fn the_notice_names_the_frame_the_needed_gain_and_a_covering_bound() {
        let ref_samples = reference(W, H, 1);
        let stack = stack_of(ref_samples, &[0.8], W, 1);
        let found = estimate(&stack, 2, (W, H), 1, 0, 1.1).unwrap();
        let cover = found[1].covering_bound();
        assert!(cover >= found[1].estimated && cover < found[1].estimated + 0.0101);
        let text = found[1].notice(1, 1.1).expect("clamped frames notify");
        assert!(text.contains("frame 1"), "{text}");
        assert!(
            text.contains(&format!("{:.3}", found[1].estimated)),
            "{text}"
        );
        assert!(
            text.contains("1.10") && text.contains(&format!("{cover:.2}")),
            "{text}"
        );
        assert!(found[0].notice(0, 1.1).is_none());
    }

    #[test]
    fn a_frame_with_few_usable_samples_keeps_unit_gain_and_is_flagged_thin() {
        let ref_samples = reference(30, 30, 1);
        let stack = stack_of(ref_samples, &[0.8], 30, 1);
        let found = estimate(&stack, 2, (30, 30), 1, 0, 1.25).unwrap();
        assert!(found[1].thin && !found[1].clamped);
        assert_eq!((found[1].estimated, found[1].applied), (1.0, 1.0));
        assert!(found[1].samples < MIN_SAMPLES);
        assert!(
            found[1]
                .notice(1, 1.25)
                .expect("thin frames notify")
                .contains("frame 1")
        );
    }

    #[test]
    fn samples_outside_the_reference_mid_tone_band_do_not_count() {
        let mut ref_samples = reference(W, H, 1);
        for s in ref_samples.iter_mut() {
            *s = if *s > 30_000 { 64_500 } else { 1_000 };
        }
        let stack = stack_of(ref_samples, &[0.9], W, 1);
        let found = estimate(&stack, 2, (W, H), 1, 0, 1.25).unwrap();
        assert!(found[1].thin && found[1].samples == 0, "{:?}", found[1]);
    }

    #[test]
    fn uncovered_regions_of_the_frame_or_the_reference_are_ignored() {
        let ref_samples = reference(W, H, 1);
        let honest = stack_of(ref_samples.clone(), &[0.8], W, 1);
        let truth = estimate(&honest, 2, (W, H), 1, 0, 1.25).unwrap()[1].estimated;

        let mut stack = stack_of(ref_samples, &[0.8], W, 1);
        let mut frame_mask = vec![true; (W * H) as usize];
        for y in 0..H {
            for x in 0..W * 6 / 10 {
                let px = (y * W + x) as usize;
                stack.frames[1][px] = 65_000;
                frame_mask[px] = false;
            }
        }
        stack.masks[1] = Some(frame_mask);
        let mut ref_mask = vec![true; (W * H) as usize];
        let top = (W * H / 5) as usize;
        stack.frames[0][..top].fill(10);
        ref_mask[..top].fill(false);
        stack.masks[0] = Some(ref_mask);

        let found = estimate(&stack, 2, (W, H), 1, 0, 1.25).unwrap();
        assert!(
            (found[1].estimated - truth).abs() < 5e-4,
            "{found:?} vs {truth}"
        );
        assert!(!found[1].thin);
    }

    #[test]
    fn gain_reader_at_unit_is_a_pass_through_and_forwards_scratch_and_coverage() {
        let mut stack = stack_of(reference(64, 32, 1), &[0.9], 64, 1);
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
    fn equalization_round_trips_through_json_and_registration_defaults_to_none() {
        let eq = Equalization {
            max_gain: 1.37,
            gains: vec![UNIT_GAIN, 52_429, 81_920, 1, 131_072],
        };
        let back: Equalization =
            serde_json::from_str(&serde_json::to_string(&eq).unwrap()).unwrap();
        assert_eq!(back, eq);

        let plain = Registration::default();
        assert_eq!(plain.gain(3), UNIT_GAIN);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("equalization"), "{json}");
        let old: Registration = serde_json::from_str(&json).unwrap();
        assert!(old.equalization.is_none());

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
