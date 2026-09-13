// SPDX-License-Identifier: GPL-3.0-or-later

//! The four similarity/sharpness measurements this crate pins, all in
//! `f64`, all over interleaved `&[u16]` plus `(width, height,
//! samples_per_pixel)` -- the same shape `crate::fuse::pyramid::build`
//! takes, so a synthetic test buffer, a fixture frame and a fused tile are
//! all callable with no conversion.
//!
//! **Definitions, pinned because they decide the calibration floors tests
//! are stated against** (`crates/core/tests/laplacian_fusion.rs`'s
//! `metrics_reproduce_the_measured_baselines`):
//!
//! - [`luma`]: the arithmetic mean of a pixel's samples, in `f64` -- for
//!   this crate's RGB fixtures, the mean of R, G and B. Never a perceptual
//!   weighting.
//! - [`laplacian_variance`]: the kernel `[[0,1,0],[1,-4,1],[0,1,0]]` applied
//!   to the luma plane over its **valid region only** (`1..width-1`,
//!   `1..height-1`, no border rule -- a value is never computed at an
//!   edge), then the **population** variance (divide by `n`, not `n - 1`)
//!   of those valid-region values. This is the definition that reproduces
//!   the fixture's measured 3.24x exactly; a sample-variance or a
//!   border-extended kernel does not.
//! - [`psnr`]: `10 * log10(65535^2 / mse)`, MSE over **every sample**
//!   (never just luma). Two identical buffers report `f64::INFINITY`
//!   rather than dividing by zero.
//! - [`block_mean_ssim`]: non-overlapping 8x8 blocks over luma,
//!   `C1 = (0.01 * 65535)^2`, `C2 = (0.03 * 65535)^2`, the arithmetic mean
//!   of the per-block SSIM over **complete blocks only** -- a
//!   `width`/`height` not a multiple of 8 drops the remainder rather than
//!   padding it.

/// One pixel's luma: the arithmetic mean of its `spp` samples, in `f64`.
/// `pixel` must be at least `spp` samples long; only the first `spp` are
/// read, so a caller may pass a full-row slice starting at the pixel.
pub fn luma(pixel: &[u16], spp: u32) -> f64 {
    let sum: f64 = pixel.iter().take(spp as usize).map(|&s| f64::from(s)).sum();
    sum / f64::from(spp)
}

/// Builds the `width * height` luma plane (`f64`) of an interleaved
/// `width * height * spp` `u16` buffer.
fn luma_plane(samples: &[u16], width: u32, height: u32, spp: u32) -> Vec<f64> {
    let spp = spp as usize;
    (0..(width as usize * height as usize))
        .map(|i| luma(&samples[i * spp..(i + 1) * spp], spp as u32))
        .collect()
}

/// The population variance (divide by `n`, never `n - 1`) of `values`.
/// `0.0` on an empty slice -- there is no detail to measure over zero
/// pixels, which is the right answer for [`laplacian_variance`] on a buffer
/// too small to have any valid-region pixel.
fn population_variance(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n
}

/// The 16-bit Laplacian variance of an interleaved `u16` buffer: the
/// population variance of `[[0,1,0],[1,-4,1],[0,1,0]]` applied to the luma
/// plane, over the valid region (`1..width-1`, `1..height-1`) only. `0.0`
/// when `width < 3 || height < 3` -- there is no valid-region pixel to
/// measure.
///
/// **This measure counts a per-pixel Z-Map winner flip as detail.** Two
/// adjacent composite pixels drawing their coefficients from two different
/// source frames -- frames whose photon-noise grain is uncorrelated -- raise
/// this variance exactly as real edge structure does: the kernel cannot
/// distinguish "two frames disagree about the same real edge" from "two
/// frames' independent grain happened to land on either side of a selection
/// boundary". So part of a speckled composite's Laplacian variance *is the
/// speckle measuring itself*, and de-speckling the Z-Map (a wider selection
/// window, a regularization pass, or any rule that removes spurious winner
/// flips) necessarily removes that part along with them -- a drop here is
/// not automatically a loss of real sharpness.
///
/// **A drop at a Z-Map de-speckling change is expected; a drop at some later, unrelated change
/// is not, and the two must not be confused** -- the independent signal is block-mean SSIM and
/// PSNR against a reference, which move up (never down) at a genuine de-speckling change. This
/// is the project's only automated sharpness number and cannot separate "speckle" from "detail"
/// itself; that needs the similarity metrics beside it, or a human review of the composite.
pub fn laplacian_variance(samples: &[u16], width: u32, height: u32, spp: u32) -> f64 {
    if width < 3 || height < 3 {
        return 0.0;
    }
    let luma = luma_plane(samples, width, height, spp);
    let idx = |x: u32, y: u32| (y as usize * width as usize) + x as usize;
    let mut values = Vec::with_capacity((width as usize - 2) * (height as usize - 2));
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let center = luma[idx(x, y)];
            let up = luma[idx(x, y - 1)];
            let down = luma[idx(x, y + 1)];
            let left = luma[idx(x - 1, y)];
            let right = luma[idx(x + 1, y)];
            values.push(up + down + left + right - 4.0 * center);
        }
    }
    population_variance(&values)
}

/// Peak-signal-to-noise ratio between two same-shaped interleaved `u16`
/// buffers: `10 * log10(65535^2 / mse)`, MSE over every sample (never just
/// luma). `f64::INFINITY` when `mse == 0.0` (identical buffers), reported
/// rather than produced by a division by zero.
pub fn psnr(a: &[u16], b: &[u16], width: u32, height: u32, spp: u32) -> f64 {
    let n = width as usize * height as usize * spp as usize;
    debug_assert_eq!(a.len(), n, "a must be exactly width*height*spp long");
    debug_assert_eq!(b.len(), n, "b must be exactly width*height*spp long");
    let sse: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| {
            let d = f64::from(x) - f64::from(y);
            d * d
        })
        .sum();
    let mse = sse / n as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * ((65535.0_f64 * 65535.0) / mse).log10()
    }
}

const SSIM_C1: f64 = 0.01 * 65535.0 * (0.01 * 65535.0);
const SSIM_C2: f64 = 0.03 * 65535.0 * (0.03 * 65535.0);

/// The arithmetic mean, over non-overlapping 8x8 luma blocks, of the
/// per-block SSIM between two same-shaped interleaved `u16` buffers.
/// Complete blocks only: `width % 8` and `height % 8` remainders are
/// dropped, never padded. `0.0` when neither buffer has a single complete
/// block.
pub fn block_mean_ssim(a: &[u16], b: &[u16], width: u32, height: u32, spp: u32) -> f64 {
    let luma_a = luma_plane(a, width, height, spp);
    let luma_b = luma_plane(b, width, height, spp);
    let blocks_x = width / 8;
    let blocks_y = height / 8;

    let mut total = 0.0f64;
    let mut count = 0usize;
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let mut xs = [0.0f64; 64];
            let mut ys = [0.0f64; 64];
            let mut n = 0usize;
            for dy in 0..8u32 {
                for dx in 0..8u32 {
                    let x = bx * 8 + dx;
                    let y = by * 8 + dy;
                    let idx = y as usize * width as usize + x as usize;
                    xs[n] = luma_a[idx];
                    ys[n] = luma_b[idx];
                    n += 1;
                }
            }
            let mean_x = xs.iter().sum::<f64>() / 64.0;
            let mean_y = ys.iter().sum::<f64>() / 64.0;
            let var_x = xs.iter().map(|v| (v - mean_x).powi(2)).sum::<f64>() / 64.0;
            let var_y = ys.iter().map(|v| (v - mean_y).powi(2)).sum::<f64>() / 64.0;
            let cov = xs
                .iter()
                .zip(ys.iter())
                .map(|(x, y)| (x - mean_x) * (y - mean_y))
                .sum::<f64>()
                / 64.0;
            let numerator = (2.0 * mean_x * mean_y + SSIM_C1) * (2.0 * cov + SSIM_C2);
            let denominator =
                (mean_x * mean_x + mean_y * mean_y + SSIM_C1) * (var_x + var_y + SSIM_C2);
            total += numerator / denominator;
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        total / count as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(width: u32, height: u32, spp: u32, value: u16) -> Vec<u16> {
        vec![value; width as usize * height as usize * spp as usize]
    }

    fn checkerboard(width: u32, height: u32, spp: u32) -> Vec<u16> {
        let mut out = vec![0u16; width as usize * height as usize * spp as usize];
        for y in 0..height {
            for x in 0..width {
                let v = if (x + y) % 2 == 0 { 65535 } else { 0 };
                for c in 0..spp {
                    out[((y * width + x) * spp + c) as usize] = v;
                }
            }
        }
        out
    }

    #[test]
    fn luma_is_the_mean_of_the_channels() {
        assert_eq!(luma(&[0, 0, 0], 3), 0.0);
        assert_eq!(luma(&[65535, 65535, 65535], 3), 65535.0);
        assert_eq!(luma(&[0, 65535, 65535], 3), (65535.0 + 65535.0) / 3.0);
    }

    #[test]
    fn a_constant_image_has_zero_laplacian_variance() {
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let samples = flat(w, h, spp, 12345);
        assert_eq!(laplacian_variance(&samples, w, h, spp), 0.0);
    }

    #[test]
    fn a_buffer_too_small_for_a_valid_region_has_zero_laplacian_variance() {
        assert_eq!(laplacian_variance(&flat(2, 5, 1, 100), 2, 5, 1), 0.0);
        assert_eq!(laplacian_variance(&flat(5, 2, 1, 100), 5, 2, 1), 0.0);
    }

    #[test]
    fn checkerboard_has_nonzero_laplacian_variance() {
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let samples = checkerboard(w, h, spp);
        assert!(laplacian_variance(&samples, w, h, spp) > 0.0);
    }

    #[test]
    fn psnr_of_a_buffer_against_itself_is_infinite() {
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let samples = checkerboard(w, h, spp);
        assert_eq!(psnr(&samples, &samples, w, h, spp), f64::INFINITY);
    }

    #[test]
    fn psnr_is_finite_and_positive_for_differing_buffers() {
        let (w, h, spp) = (16u32, 16u32, 3u32);
        let a = flat(w, h, spp, 40000);
        let b = flat(w, h, spp, 40100);
        let value = psnr(&a, &b, w, h, spp);
        assert!(value.is_finite());
        assert!(value > 0.0);
    }

    /// The metric functions are pinned, not just calibrated against the
    /// fixture -- identical images score SSIM 1.0 and a one-pixel shift
    /// scores strictly lower.
    #[test]
    fn metrics_are_pinned() {
        let (w, h, spp) = (32u32, 32u32, 3u32);
        let a = checkerboard(w, h, spp);
        assert_eq!(block_mean_ssim(&a, &a, w, h, spp), 1.0);

        // A one-pixel horizontal shift: row-major, so shifting by one
        // sample-group moves every pixel one column to the right within
        // its row (dropping the last column, duplicating the first).
        let mut shifted = a.clone();
        for y in 0..h {
            for x in 0..w {
                let src_x = if x == 0 { 0 } else { x - 1 };
                for c in 0..spp {
                    let src = ((y * w + src_x) * spp + c) as usize;
                    let dst = ((y * w + x) * spp + c) as usize;
                    shifted[dst] = a[src];
                }
            }
        }
        let shifted_ssim = block_mean_ssim(&a, &shifted, w, h, spp);
        assert!(
            shifted_ssim < 1.0,
            "a one-pixel shift must score strictly lower than identical: got {shifted_ssim}"
        );
    }

    #[test]
    fn block_mean_ssim_ignores_an_incomplete_trailing_block() {
        // 20x20 has two complete 8x8 blocks per axis (16 px) and a 4 px
        // remainder on each -- the remainder must never panic and never
        // contribute.
        let (w, h, spp) = (20u32, 20u32, 3u32);
        let a = checkerboard(w, h, spp);
        let value = block_mean_ssim(&a, &a, w, h, spp);
        assert_eq!(value, 1.0);
    }

    #[test]
    fn block_mean_ssim_is_zero_with_no_complete_block() {
        let (w, h, spp) = (5u32, 5u32, 3u32);
        let a = flat(w, h, spp, 100);
        assert_eq!(block_mean_ssim(&a, &a, w, h, spp), 0.0);
    }
}
