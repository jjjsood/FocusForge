// SPDX-License-Identifier: GPL-3.0-or-later

//! The Laplacian pyramid: build to `N` levels over a single buffer,
//! collapse back to one. Every rounding, ordering and border-rule choice
//! below is binding for a bit-parity GPU twin.
//!
//! # Working precision and I/O
//!
//! `f32` intermediates, `f32`-stored levels, `u16` in and out. This looks
//! like it collides with `crate::params`'s "every parameter is computed in
//! `f64`, cast to `f32` once at GPU upload" rule -- it does not: that rule
//! governs *parameter scalars*, while sample data here is `f32` end to end
//! so both the CPU path and a GPU twin do identical arithmetic with
//! no extra cast to disagree over.
//!
//! Conversion in: [`Plane::from_u16`], exact length, `u16 as f32` (lossless
//! -- `f32`'s 24-bit mantissa covers every 16-bit value). Conversion out:
//! [`Plane::to_u16_into`], the expression
//!
//! ```text
//! floor(clamp(v, 0.0, 65535.0) + 0.5)
//! ```
//!
//! written as that expression in code and in every description of it,
//! **never** the word "round": Rust's `f32::round` is half-away-from-zero
//! and WGSL's `round` is round-ties-to-even, so a doc comment that says
//! "round" and a WGSL kernel that calls `round()` disagree on exactly the
//! `.5` cases, which is a 1-LSB parity failure by construction against
//! the whole 1-LSB budget. `as u16` on top is a saturating cast that
//! also maps NaN to `0` in Rust, but the clamp above is the rule; the cast
//! is only a second fence.
//!
//! # Kernel and border rule
//!
//! [`reduce`] applies the binomial 5-tap kernel `[1,4,6,4,1]/16` (all five
//! coefficients dyadic, exactly representable in `f32`) as two separable
//! 1D passes, in a **pinned order: horizontal filter-and-decimate first,
//! then vertical**. Every tap samples `2*x + i` for `i` in `-2..=2`,
//! **clamped to edge** (`i.clamp(0, dim - 1)`), strided by
//! `samples_per_pixel`. The five products are summed in increasing `i`
//! order. Both the pass order and the accumulation order are pinned
//! because `f32` addition is not associative -- a different order is a
//! different result at the ulp level, and a WGSL twin must match both
//! to stay inside 1 LSB.
//!
//! [`expand`] takes its **target dimensions as explicit arguments, never
//! inferred**: `ceil` halving both `5` and `6` reduce to `3`, so given only
//! a source size the inverse is ambiguous. [`build`] records every level's
//! `(width, height)` in its own [`Plane`]; [`collapse`] reads them back.
//! This is the single most common off-by-one in a WGSL reimplementation of
//! this file.
//!
//! `expand` is two separable passes in the same pinned order (horizontal
//! to `(width, src.height)`, then vertical to `(width, height)`), with the
//! phase-split kernels the zero-insert-then-convolve-with-`2 * [1,4,6,4,1]
//! / 16` formulation collapses to -- stated here as its own code block
//! because a reimplementer who re-derives these two phases from the
//! reduce kernel alone is exactly where a parity failure comes from:
//!
//! ```text
//! even output index x: 0.125 * S[x/2 - 1] + 0.75 * S[x/2] + 0.125 * S[x/2 + 1]
//! odd  output index x: 0.5   * S[(x - 1)/2] + 0.5  * S[(x + 1)/2]
//! ```
//!
//! Source indices are clamped to edge exactly as in `reduce`. Both phases'
//! weights sum to exactly `1`, so DC is preserved bit-exactly. Clamp-to-edge
//! (not reflect-101) is used identically at every level, in both
//! directions, in both `reduce` and `expand`: it is one `clamp()` in both
//! Rust and WGSL, and it is the natural sampler address mode for a
//! texture-based GPU implementation.
//!
//! # Level sizing and level count
//!
//! `dim_k = ceil(dim / 2^k)`. [`max_levels`] is
//! `floor(log2(min(width, height) / 4))` -- `7` at `512`, `1` at `8`, `0`
//! at `4` and below. [`build`] (via [`build_from_plane`]) refuses
//! `levels > max_levels(width, height)` with
//! [`crate::fuse::PyramidError::LevelsTooDeep`] rather than silently
//! truncating. `levels = 0` is a documented behaviour, not an error: the
//! pyramid has no Laplacian levels, the residual is the input verbatim (as
//! `f32`), and collapsing it reproduces the input bit-identically, because
//! `floor(clamp(v, 0.0, 65535.0) + 0.5)` of an exact integer already in
//! range is that integer. `levels = 1` yields one Laplacian at the input's
//! own dimensions and a residual at `ceil`-halved dimensions.
//!
//! # Reach and the measured tile-overlap margin
//!
//! The *analytic* reach of a build-plus-collapse round trip is
//! `4 * (2^N - 1)` (`2 * (2^N - 1)` for build alone, doubled because
//! collapse retraces the same path outward) -- `254` at `N = 7` for build
//! alone, `508` for build-plus-collapse. [`MEASURED_MARGIN`] below is the
//! **measured** margin, and it is measured against the criterion it is
//! used for rather than against a proxy for it:
//!
//! > two buffers of the same size and the same sample grid that agree
//! > inside a window and differ arbitrarily outside it produce outputs
//! > differing by at most 1 `u16` LSB everywhere inside that window, once
//! > the window is inset by the margin.
//!
//! - operator: `expand^N` composed with `reduce^N` (`flatten` in this
//!   module's tests) with every Laplacian level zeroed before collapsing
//!   -- the deepest path through the pyramid, and therefore the
//!   longest-reach one; every per-level detail path `expand^k(L_k)` is a
//!   strict sub-reach of it.
//! - adversarial input: **all** content at once, not a sampled probe.
//!   Every weight in `flatten` is non-negative, so
//!   `|flatten(a) - flatten(b)| <= flatten(|a - b|) <=
//!   flatten(65535 * outside_window)`, and that right-hand side is a
//!   single run over a buffer holding `65535` outside the window and `0`
//!   inside. The bound is attained, so it is the exact worst case.
//! - threshold: `0.5` LSB of that bound, because
//!   `floor(clamp(v, 0.0, 65535.0) + 0.5)` is monotone and
//!   `floor(v + 1.5) - floor(v + 0.5) == 1`, so an `f32` difference of at
//!   most `1.0` can never become a `u16` difference of `2`. Half of that
//!   leaves a factor of two for ulp-level disagreement between two
//!   independently accumulated pipelines.
//!
//! Two properties of the measurement are load-bearing and easy to lose:
//!
//! 1. **It is taken in the geometry a tile of that margin actually
//!    reads** -- precisely, a square buffer of `4 * margin + 64` px per
//!    side (`tests::leak_plane`) -- not in a comfortable large buffer. The
//!    required margin is not a function of the level count alone: in a
//!    small buffer the tails of two opposite margins overlap in the
//!    middle, and a `512` px tile's coarsest Gaussian at 7 levels is a
//!    `4x4` image. This is why each candidate margin is judged in its own
//!    geometry, ascending until one holds -- a fixed point, not a root --
//!    and it is why the entry at `N = 7` (`512`) is *larger* than the
//!    analytic reach (`508`). The published number therefore carries a
//!    precondition of its own: it is unestablished in any buffer smaller
//!    than the one it was measured in.
//!    `crate::tile::plan::TilePlan::new`'s `min_tile_size` is what
//!    guarantees no plan is ever smaller than that geometry; a direct
//!    caller of this module that windows on a smaller buffer is outside
//!    what the table claims.
//! 2. **It is taken with the window boundary on the decimation grid**, and
//!    the published entries are multiples of `2^N` for that reason. An
//!    off-grid boundary reaches measurably further (at `N = 3`, `21..27`
//!    px depending on the phase; at `N = 7`, `365..433`), because the
//!    boundary's phase relative to the dyadic grid decides how much weight
//!    the coarse samples straddling it carry. `tile::plan` never creates
//!    one: both the margin and the interior stride are multiples of
//!    `2^levels`.
//!
//! | levels (`N`) | published margin | analytic `4*(2^N-1)` | measurement geometry (`4*margin+64`) | smallest tile that plans |
//! |---|---|---|---|---|
//! | 0 | 0 | 0 | n/a (no margin, no pyramid) | 1 |
//! | 1 | 4 | 4 | 80 | 80 |
//! | 2 | 12 | 12 | 112 | 112 |
//! | 3 | 24 | 28 | 160 | 160 |
//! | 4 | 48 | 60 | 256 | 256 |
//! | 5 | 96 | 124 | 448 | 448 (the shipped default, 512, clears this) |
//! | 6 | 192 | 252 | 832 | 832 (512 px is refused here) |
//! | 7 | 512 | 508 | 2112 | 2112 |
//!
//! "Smallest tile that plans" is `crate::tile::plan::TilePlan::new`'s
//! `min_tile_size`, `max(2 * margin + 2^N, 4 * margin + 64)` -- the second
//! term dominates at every level above 0 for this table, which is exactly
//! the measurement-geometry precondition made concrete. The margin itself
//! is below the analytic bound at every level from 3 to 6 and above it at
//! 7, and nowhere near the ~128 px a healthy 512 px tile interior would
//! need at 7 levels. This gap is structural rather than a border-rule
//! artifact: a deep pyramid over a small tile genuinely depends on the
//! whole tile. See `stated_margin_matches_the_measurement`
//! for the measurement itself and `interior_is_margin_independent` for the
//! criterion asserted at exactly the published number, with no slack term.
//!
//! # Memory
//!
//! A 512^2, 3-channel tile as `f32` is 3.1 MB; its full 7-level pyramid is
//! about 4.2 MB. 61 frames' worth resident at once is about 256 MB *per
//! worker thread* against an 8 GB per-worker ceiling -- streaming frames
//! with a running best-per-level (not this module's job) keeps that
//! at two pyramids rather than 61.

use crate::fuse::PyramidError;

/// The binomial 5-tap Gaussian kernel `[1,4,6,4,1]/16`, applied identically
/// on both axes of [`reduce`]. Every coefficient is dyadic and therefore
/// exactly representable in `f32`.
pub const KERNEL: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];

/// The measured tile-overlap margin (pixels) for `levels = 0..=7`.
///
/// Each entry is the smallest multiple of `2^levels` at which the
/// criterion in this module's "Reach and the measured tile-overlap margin"
/// section holds: two same-size, same-grid buffers agreeing inside a
/// window, differing arbitrarily outside it, produce outputs differing by
/// at most **1 `u16` LSB** everywhere in that window inset by this margin.
/// The number is measured in the geometry a tile of that margin actually
/// reads -- precisely, a square buffer of `4 * margin + 64` px per side --
/// and for a window boundary on the decimation grid.
/// `crate::tile::plan::TilePlan::new`'s `min_tile_size` is what guarantees
/// that geometry is the only one this number is ever claimed for: no plan
/// is accepted below it. See `stated_margin_matches_the_measurement` for the
/// measurement and `interior_is_margin_independent` for the criterion,
/// asserted at exactly these values with no slack.
pub const MEASURED_MARGIN: [u32; 8] = [0, 4, 12, 24, 48, 96, 192, 512];

/// Clamp a possibly out-of-range signed sample coordinate to `0..dim`,
/// i.e. clamp-to-edge on one axis.
fn clamp_index(v: i64, dim: u32) -> u32 {
    v.clamp(0, dim as i64 - 1) as u32
}

/// One `f32` pyramid level (or the base image before any pyramid math):
/// `width * height * samples_per_pixel` samples, **interleaved** --
/// `data[(y * width + x) * samples_per_pixel + c]`. This matches
/// `crate::tile::Tile::samples`'s layout with no transpose at either end.
#[derive(Debug, Clone, PartialEq)]
pub struct Plane {
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    data: Vec<f32>,
}

impl Plane {
    /// Builds a [`Plane`] from `u16` samples, refusing a zero extent
    /// ([`PyramidError::EmptyExtent`]), `samples_per_pixel == 0`
    /// ([`PyramidError::SamplesPerPixelZero`]) and a slice whose length is
    /// not exactly `width * height * samples_per_pixel`
    /// ([`PyramidError::BufferSize`], never clamped).
    pub fn from_u16(
        samples: &[u16],
        width: u32,
        height: u32,
        samples_per_pixel: u32,
    ) -> Result<Self, PyramidError> {
        if width == 0 || height == 0 {
            return Err(PyramidError::EmptyExtent { width, height });
        }
        if samples_per_pixel == 0 {
            return Err(PyramidError::SamplesPerPixelZero);
        }
        let expected = width as usize * height as usize * samples_per_pixel as usize;
        if samples.len() != expected {
            return Err(PyramidError::BufferSize {
                expected,
                got: samples.len(),
            });
        }
        let data = samples.iter().map(|&s| f32::from(s)).collect();
        Ok(Plane {
            width,
            height,
            samples_per_pixel,
            data,
        })
    }

    /// Writes this plane's samples into `out` as `u16`, each sample
    /// converted with `floor(clamp(v, 0.0, 65535.0) + 0.5)`. Refuses a
    /// length mismatch ([`PyramidError::BufferSize`]) rather than
    /// truncating or panicking.
    pub fn to_u16_into(&self, out: &mut [u16]) -> Result<(), PyramidError> {
        if out.len() != self.data.len() {
            return Err(PyramidError::BufferSize {
                expected: self.data.len(),
                got: out.len(),
            });
        }
        for (o, &v) in out.iter_mut().zip(self.data.iter()) {
            *o = (v.clamp(0.0, 65535.0) + 0.5).floor() as u16;
        }
        Ok(())
    }

    /// Convenience over [`Self::to_u16_into`] that allocates its own
    /// output buffer.
    pub fn to_u16(&self) -> Vec<u16> {
        let mut out = vec![0u16; self.data.len()];
        self.to_u16_into(&mut out)
            .expect("out.len() == self.data.len() by construction");
        out
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn samples_per_pixel(&self) -> u32 {
        self.samples_per_pixel
    }

    pub fn data(&self) -> &[f32] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [f32] {
        &mut self.data
    }

    /// Builds a [`Plane`] directly from `f32` samples, with the identical
    /// three refusals [`Self::from_u16`] applies, in the identical order:
    /// [`PyramidError::EmptyExtent`], [`PyramidError::SamplesPerPixelZero`],
    /// [`PyramidError::BufferSize`]. No conversion, no clamp, no rounding --
    /// the input already is the storage type. This is the constructor a
    /// luminance plane needs: the mean of three `u16` samples is not
    /// integral, so routing it through [`Self::from_u16`] would be a silent
    /// precision claim.
    pub fn from_f32(
        samples: &[f32],
        width: u32,
        height: u32,
        samples_per_pixel: u32,
    ) -> Result<Self, PyramidError> {
        Self::from_f32_vec(samples.to_vec(), width, height, samples_per_pixel)
    }

    /// [`Self::from_f32`] taking ownership of the buffer: the plane's storage
    /// is `samples` itself, never a copy. The refusals and their order are
    /// the same.
    pub fn from_f32_vec(
        samples: Vec<f32>,
        width: u32,
        height: u32,
        samples_per_pixel: u32,
    ) -> Result<Self, PyramidError> {
        if width == 0 || height == 0 {
            return Err(PyramidError::EmptyExtent { width, height });
        }
        if samples_per_pixel == 0 {
            return Err(PyramidError::SamplesPerPixelZero);
        }
        let expected = width as usize * height as usize * samples_per_pixel as usize;
        if samples.len() != expected {
            return Err(PyramidError::BufferSize {
                expected,
                got: samples.len(),
            });
        }
        Ok(Plane {
            width,
            height,
            samples_per_pixel,
            data: samples,
        })
    }
}

/// Builds the Gaussian pyramid of `base`: `levels + 1` planes, index `k` is
/// `G_k` (`k` [`reduce`] passes applied to `base`), index `0` is `base`
/// itself, unchanged. This is the only `reduce` chain in the crate: a
/// caller needing the same chain (alignment, for instance) reuses it
/// rather than writing a second.
///
/// `levels > max_levels(base.width(), base.height())` is
/// [`PyramidError::LevelsTooDeep`], never a silently truncated pyramid.
pub fn gaussian_levels(base: Plane, levels: u32) -> Result<Vec<Plane>, PyramidError> {
    let max = max_levels(base.width, base.height);
    if levels > max {
        return Err(PyramidError::LevelsTooDeep {
            requested: levels,
            max,
            width: base.width,
            height: base.height,
        });
    }

    let mut gaussians = Vec::with_capacity(levels as usize + 1);
    gaussians.push(base);
    for _ in 0..levels {
        let next = reduce(gaussians.last().expect("gaussians is never empty"));
        gaussians.push(next);
    }
    Ok(gaussians)
}

/// Filter-and-decimate one row-major `f32` buffer along its horizontal
/// axis with [`KERNEL`], halving the width by `ceil`. The first of
/// [`reduce`]'s two pinned passes.
fn reduce_horizontal(data: &[f32], width: u32, height: u32, spp: u32) -> (Vec<f32>, u32) {
    let out_width = width.div_ceil(2);
    let mut out = vec![0.0f32; out_width as usize * height as usize * spp as usize];
    for y in 0..height {
        for x in 0..out_width {
            for c in 0..spp {
                let mut acc = 0.0f32;
                for (k, &coef) in KERNEL.iter().enumerate() {
                    let offset = k as i64 - 2;
                    let src_x = clamp_index(2 * x as i64 + offset, width);
                    let idx = ((y * width + src_x) * spp + c) as usize;
                    acc += coef * data[idx];
                }
                let out_idx = ((y * out_width + x) * spp + c) as usize;
                out[out_idx] = acc;
            }
        }
    }
    (out, out_width)
}

/// Filter-and-decimate one row-major `f32` buffer along its vertical axis
/// with [`KERNEL`], halving the height by `ceil`. The second of
/// [`reduce`]'s two pinned passes.
fn reduce_vertical(data: &[f32], width: u32, height: u32, spp: u32) -> (Vec<f32>, u32) {
    let out_height = height.div_ceil(2);
    let mut out = vec![0.0f32; width as usize * out_height as usize * spp as usize];
    for y in 0..out_height {
        for x in 0..width {
            for c in 0..spp {
                let mut acc = 0.0f32;
                for (k, &coef) in KERNEL.iter().enumerate() {
                    let offset = k as i64 - 2;
                    let src_y = clamp_index(2 * y as i64 + offset, height);
                    let idx = ((src_y * width + x) * spp + c) as usize;
                    acc += coef * data[idx];
                }
                let out_idx = ((y * width + x) * spp + c) as usize;
                out[out_idx] = acc;
            }
        }
    }
    (out, out_height)
}

/// Gaussian-blurs and decimates `src` by two, `(ceil(w/2), ceil(h/2))`,
/// same `samples_per_pixel`. See the module docs for the pinned pass
/// order, kernel, border rule and accumulation order.
pub fn reduce(src: &Plane) -> Plane {
    let spp = src.samples_per_pixel;
    let (h_data, out_width) = reduce_horizontal(&src.data, src.width, src.height, spp);
    let (v_data, out_height) = reduce_vertical(&h_data, out_width, src.height, spp);
    Plane {
        width: out_width,
        height: out_height,
        samples_per_pixel: spp,
        data: v_data,
    }
}

/// One output sample of a 1D expand along an axis of length `src_len`,
/// reading source samples through `get`. See the module docs for the two
/// phase kernels this implements.
fn expand_1d(src_len: u32, out_index: u32, get: impl Fn(u32) -> f32) -> f32 {
    let x = out_index as i64;
    if x % 2 == 0 {
        let c = x / 2;
        let lo = get(clamp_index(c - 1, src_len));
        let mid = get(clamp_index(c, src_len));
        let hi = get(clamp_index(c + 1, src_len));
        0.125 * lo + 0.75 * mid + 0.125 * hi
    } else {
        let lo = get(clamp_index((x - 1) / 2, src_len));
        let hi = get(clamp_index((x + 1) / 2, src_len));
        0.5 * lo + 0.5 * hi
    }
}

/// Upsamples `data` (row-major, `src_width x height`) to `out_width x
/// height` along the horizontal axis. The first of [`expand`]'s two
/// pinned passes.
fn expand_horizontal(
    data: &[f32],
    src_width: u32,
    height: u32,
    spp: u32,
    out_width: u32,
) -> Vec<f32> {
    let mut out = vec![0.0f32; out_width as usize * height as usize * spp as usize];
    for y in 0..height {
        for x in 0..out_width {
            for c in 0..spp {
                let value = expand_1d(src_width, x, |sx| {
                    data[((y * src_width + sx) * spp + c) as usize]
                });
                out[((y * out_width + x) * spp + c) as usize] = value;
            }
        }
    }
    out
}

/// Upsamples `data` (row-major, `width x src_height`) to `width x
/// out_height` along the vertical axis. The second of [`expand`]'s two
/// pinned passes.
fn expand_vertical(
    data: &[f32],
    width: u32,
    src_height: u32,
    spp: u32,
    out_height: u32,
) -> Vec<f32> {
    let mut out = vec![0.0f32; width as usize * out_height as usize * spp as usize];
    for y in 0..out_height {
        for x in 0..width {
            for c in 0..spp {
                let value = expand_1d(src_height, y, |sy| {
                    data[((sy * width + x) * spp + c) as usize]
                });
                out[((y * width + x) * spp + c) as usize] = value;
            }
        }
    }
    out
}

/// Upsamples `src` to exactly `(width, height)` -- **target dimensions are
/// explicit, never inferred**; see the module docs for why. Two separable
/// passes in the pinned order: horizontal to `(width, src.height())`, then
/// vertical to `(width, height)`.
pub fn expand(src: &Plane, width: u32, height: u32) -> Plane {
    let spp = src.samples_per_pixel;
    let h_data = expand_horizontal(&src.data, src.width, src.height, spp, width);
    let v_data = expand_vertical(&h_data, width, src.height, spp, height);
    Plane {
        width,
        height,
        samples_per_pixel: spp,
        data: v_data,
    }
}

/// The maximum pyramid depth a `width x height` buffer supports:
/// `floor(log2(min(width, height) / 4))`. `const fn`, like
/// `crate::tile::plan::margin_for_levels` -- `u32::leading_zeros` is
/// `const`, so integer division plus a leading-zeros count stands in for
/// a floating `log2`.
pub const fn max_levels(width: u32, height: u32) -> u32 {
    let min = if width < height { width } else { height };
    let q = min / 4;
    if q == 0 { 0 } else { 31 - q.leading_zeros() }
}

/// A Laplacian pyramid built from one base [`Plane`]: `levels[k]` is
/// `L_k = G_k - expand(G_{k+1}, G_k.width, G_k.height)`, and `residual` is
/// the coarsest Gaussian level `G_N`.
#[derive(Debug, Clone)]
pub struct Pyramid {
    levels: Vec<Plane>,
    residual: Plane,
}

impl Pyramid {
    pub fn levels(&self) -> &[Plane] {
        &self.levels
    }

    /// Mutable access to Laplacian level `k`, for per-level selection that
    /// rewrites levels in place before [`collapse`]. Panics on an
    /// out-of-range `k`, like slice indexing.
    pub fn level_mut(&mut self, k: usize) -> &mut Plane {
        &mut self.levels[k]
    }

    pub fn residual(&self) -> &Plane {
        &self.residual
    }

    pub fn residual_mut(&mut self) -> &mut Plane {
        &mut self.residual
    }

    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// The base image's `(width, height)` -- level 0's dimensions if any
    /// Laplacian level exists, else the residual's (the `levels = 0` case,
    /// where the residual *is* the base).
    pub fn dimensions(&self) -> (u32, u32) {
        match self.levels.first() {
            Some(l0) => (l0.width, l0.height),
            None => (self.residual.width, self.residual.height),
        }
    }
}

/// Builds a [`Pyramid`] from an already-converted base [`Plane`]. `build`
/// delegates to this after [`Plane::from_u16`], so a caller that already
/// holds a `Plane` (a `f32` intermediate, or a level being rebuilt) skips
/// the `u16` round trip.
///
/// `levels > max_levels(base.width(), base.height())` is
/// [`PyramidError::LevelsTooDeep`], never a silently truncated pyramid.
pub fn build_from_plane(base: Plane, levels: u32) -> Result<Pyramid, PyramidError> {
    let mut gaussians = gaussian_levels(base, levels)?;

    let mut laplacians = Vec::with_capacity(levels as usize);
    for k in 0..levels as usize {
        let g_k = &gaussians[k];
        let g_k1 = &gaussians[k + 1];
        let expanded = expand(g_k1, g_k.width, g_k.height);
        let data = g_k
            .data
            .iter()
            .zip(expanded.data.iter())
            .map(|(&a, &b)| a - b)
            .collect();
        laplacians.push(Plane {
            width: g_k.width,
            height: g_k.height,
            samples_per_pixel: g_k.samples_per_pixel,
            data,
        });
    }

    let residual = gaussians.pop().expect("gaussians has levels + 1 entries");
    Ok(Pyramid {
        levels: laplacians,
        residual,
    })
}

/// Builds a [`Pyramid`] over `width x height` `u16` samples (interleaved,
/// `samples_per_pixel` channels). Synthetic test buffers and a real
/// `crate::tile::Tile::samples` slice are both callable with no
/// conversion -- this takes `&[u16] + dims + spp` rather than a `&Tile` on
/// purpose.
pub fn build(
    samples: &[u16],
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    levels: u32,
) -> Result<Pyramid, PyramidError> {
    let base = Plane::from_u16(samples, width, height, samples_per_pixel)?;
    build_from_plane(base, levels)
}

/// Collapses a [`Pyramid`] back into one [`Plane`]: `A_N = residual`,
/// `A_k = levels[k] + expand(A_{k+1}, levels[k].width, levels[k].height)`,
/// walking from the coarsest level to the finest.
pub fn collapse(pyramid: &Pyramid) -> Plane {
    let mut current = pyramid.residual.clone();
    for lap in pyramid.levels.iter().rev() {
        let expanded = expand(&current, lap.width, lap.height);
        let data = lap
            .data
            .iter()
            .zip(expanded.data.iter())
            .map(|(&a, &b)| a + b)
            .collect();
        current = Plane {
            width: lap.width,
            height: lap.height,
            samples_per_pixel: lap.samples_per_pixel,
            data,
        };
    }
    current
}

/// [`collapse`], writing the result into `out` as `u16` via
/// [`Plane::to_u16_into`].
pub fn collapse_into(pyramid: &Pyramid, out: &mut [u16]) -> Result<(), PyramidError> {
    collapse(pyramid).to_u16_into(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- signal generators -------------------------------------------

    fn flat(width: u32, height: u32, spp: u32, value: u16) -> Vec<u16> {
        vec![value; width as usize * height as usize * spp as usize]
    }

    fn gradient(width: u32, height: u32, spp: u32) -> Vec<u16> {
        let mut out = vec![0u16; width as usize * height as usize * spp as usize];
        for y in 0..height {
            for x in 0..width {
                for c in 0..spp {
                    let v = ((x * 65535) / width.max(1)) as u16;
                    out[((y * width + x) * spp + c) as usize] = v;
                }
            }
        }
        out
    }

    fn step_edge(width: u32, height: u32, spp: u32) -> Vec<u16> {
        let mut out = vec![0u16; width as usize * height as usize * spp as usize];
        for y in 0..height {
            for x in 0..width {
                let v = if x < width / 2 { 65535 } else { 0 };
                for c in 0..spp {
                    out[((y * width + x) * spp + c) as usize] = v;
                }
            }
        }
        out
    }

    /// A deterministic xorshift32 PRNG, seeded with a fixed constant --
    /// never `rand`, so this test needs no dependency and is bit-for-bit
    /// reproducible across runs.
    struct XorShift32(u32);

    impl XorShift32 {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
    }

    fn pseudo_random(width: u32, height: u32, spp: u32) -> Vec<u16> {
        let mut rng = XorShift32(0x9E3779B9);
        (0..(width as usize * height as usize * spp as usize))
            .map(|_| (rng.next() % 65536) as u16)
            .collect()
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

    fn assert_roundtrip_bit_identical(samples: &[u16], width: u32, height: u32, spp: u32) {
        let levels = max_levels(width, height);
        let pyramid = build(samples, width, height, spp, levels).unwrap();
        let mut out = vec![0u16; samples.len()];
        collapse_into(&pyramid, &mut out).unwrap();
        assert_eq!(
            out, samples,
            "round trip must be bit-identical at width={width} height={height} spp={spp} levels={levels}"
        );
    }

    #[test]
    fn roundtrip_within_tolerance() {
        // 128x128 gives max_levels == 5: enough depth to exercise several
        // reduce/expand passes without a slow test.
        let (width, height) = (128u32, 128u32);
        for &spp in &[1u32, 3u32] {
            assert_roundtrip_bit_identical(&flat(width, height, spp, 12345), width, height, spp);
            assert_roundtrip_bit_identical(&gradient(width, height, spp), width, height, spp);
            assert_roundtrip_bit_identical(&step_edge(width, height, spp), width, height, spp);
            assert_roundtrip_bit_identical(&pseudo_random(width, height, spp), width, height, spp);
        }
    }

    #[test]
    fn roundtrip_on_ragged_sizes() {
        let spp = 3;
        for &(width, height) in &[
            (1u32, 1u32),
            (2, 3),
            (5, 5),
            (17, 9),
            (33, 31),
            (63, 65),
            (100, 7),
        ] {
            assert_roundtrip_bit_identical(&pseudo_random(width, height, spp), width, height, spp);
        }
    }

    #[test]
    fn level_count_limit_is_enforced() {
        assert_eq!(max_levels(512, 512), 7);
        assert_eq!(max_levels(8, 8), 1);
        assert_eq!(max_levels(4, 4), 0);
        assert_eq!(max_levels(3, 100), 0);

        let samples = flat(512, 512, 1, 0);
        let max = max_levels(512, 512);
        match build(&samples, 512, 512, 1, max + 1) {
            Err(PyramidError::LevelsTooDeep {
                requested,
                max: reported_max,
                width,
                height,
            }) => {
                assert_eq!(requested, max + 1);
                assert_eq!(reported_max, max);
                assert_eq!(width, 512);
                assert_eq!(height, 512);
                let rendered = PyramidError::LevelsTooDeep {
                    requested,
                    max: reported_max,
                    width,
                    height,
                }
                .to_string();
                assert!(rendered.contains(&requested.to_string()));
                assert!(rendered.contains(&reported_max.to_string()));
                assert!(rendered.contains("512"));
            }
            other => panic!("expected LevelsTooDeep, got {other:?}"),
        }
    }

    #[test]
    fn degenerate_level_counts() {
        let (width, height, spp) = (16u32, 16u32, 3u32);
        let samples = pseudo_random(width, height, spp);

        // levels = 0: empty level list, residual is the input as f32,
        // collapse reproduces the input bit-identically.
        let pyramid0 = build(&samples, width, height, spp, 0).unwrap();
        assert!(pyramid0.levels().is_empty());
        assert_eq!(pyramid0.residual().width(), width);
        assert_eq!(pyramid0.residual().height(), height);
        let mut out0 = vec![0u16; samples.len()];
        collapse_into(&pyramid0, &mut out0).unwrap();
        assert_eq!(out0, samples);

        // levels = 1: one Laplacian at input dims, residual at
        // ceil-halved dims.
        let pyramid1 = build(&samples, width, height, spp, 1).unwrap();
        assert_eq!(pyramid1.level_count(), 1);
        assert_eq!(pyramid1.levels()[0].width(), width);
        assert_eq!(pyramid1.levels()[0].height(), height);
        assert_eq!(pyramid1.residual().width(), width.div_ceil(2));
        assert_eq!(pyramid1.residual().height(), height.div_ceil(2));
        let mut out1 = vec![0u16; samples.len()];
        collapse_into(&pyramid1, &mut out1).unwrap();
        assert_eq!(out1, samples);
    }

    #[test]
    fn extremes_survive_the_roundtrip() {
        let (width, height, spp) = (32u32, 32u32, 3u32);
        assert_roundtrip_bit_identical(&checkerboard(width, height, spp), width, height, spp);
        assert_roundtrip_bit_identical(&flat(width, height, spp, 0), width, height, spp);
        assert_roundtrip_bit_identical(&flat(width, height, spp, 65535), width, height, spp);

        // A hand-built pyramid whose residual is out of [0, 65535] must
        // clamp, not wrap, on the way out.
        let residual = Plane {
            width: 2,
            height: 2,
            samples_per_pixel: 1,
            data: vec![-5.0, 70000.0, -5.0, 70000.0],
        };
        let pyramid = Pyramid {
            levels: Vec::new(),
            residual,
        };
        let mut out = vec![0u16; 4];
        collapse_into(&pyramid, &mut out).unwrap();
        assert_eq!(out, vec![0, 65535, 0, 65535]);
    }

    // ---- margin measurement --------------------------------------------

    /// Builds to `levels`, zeroes every Laplacian, then collapses: the
    /// deepest path through the pyramid, and therefore the longest-reach
    /// one for measuring margin independence.
    fn flatten(base: Plane, levels: u32) -> Plane {
        let mut pyramid = build_from_plane(base, levels).unwrap();
        for k in 0..pyramid.level_count() {
            for v in pyramid.level_mut(k).data_mut() {
                *v = 0.0;
            }
        }
        collapse(&pyramid)
    }

    /// The `f32` difference at which two independently converted outputs
    /// are still guaranteed to land within 1 `u16` LSB of each other.
    ///
    /// `floor(clamp(v, 0.0, 65535.0) + 0.5)` is monotone and
    /// `floor(v + 1.5) - floor(v + 0.5) == 1` for every `v`, so an `f32`
    /// difference of at most `1.0` can never become a `u16` difference of
    /// `2`. The threshold is `0.5` rather than `1.0` to keep a factor of
    /// two in hand for the ulp-level disagreement between two independently
    /// accumulated `f32` pipelines (an ulp near `65535` is `2^-8` LSB, so
    /// the factor of two is many hundreds of ulps of headroom).
    const MARGIN_THRESHOLD_LSB: f32 = 0.5;

    /// The worst-case leak of the flatten operator into a window, for a
    /// tile whose margin is `margin`: a square buffer of the size such a
    /// tile reads, `65535` everywhere in the `margin`-thick frame two
    /// buffers are allowed to differ in and `0` in the window they agree
    /// in, flattened. Returns the plane, its side and the band thickness.
    ///
    /// The geometry -- `side = (4 * margin + 64).max(4 << levels)` below --
    /// is the tile's own, not a comfortable large one, and that matters:
    /// the reach is **not** a function of the level count alone. In a
    /// buffer only a few `2^levels` wide, the tails of two opposite bands
    /// overlap in the middle -- a 512 px tile's coarsest Gaussian at 7
    /// levels is a 4x4 image -- so the same operator needs a larger margin
    /// in a small buffer than in a large one. Measuring in the large buffer
    /// and publishing that number is how a table comes to need a slack
    /// term bolted on at its point of use. `4 * margin + 64` is therefore
    /// not just this test's buffer size, it is the geometry the published
    /// margin is *for*; `crate::tile::plan::TilePlan::new`'s
    /// `min_tile_size` guarantees no plan is smaller than it.
    fn leak_plane(levels: u32, margin: u32, band: u32) -> (Plane, u32, u32) {
        let side = (4 * margin + 64).max(4 << levels);
        let mut samples = vec![0u16; side as usize * side as usize];
        for y in 0..side {
            for x in 0..side {
                let inside = x >= band && x < side - band && y >= band && y < side - band;
                if !inside {
                    samples[(y * side + x) as usize] = 65535;
                }
            }
        }
        let base = Plane::from_u16(&samples, side, side, 1).unwrap();
        (flatten(base, levels), side, band)
    }

    /// The largest leak anywhere at each Chebyshev distance inside the
    /// band's inner boundary. Corners are covered by construction, and they
    /// are where two sides' tails add -- which is exactly what a
    /// one-dimensional half-plane probe cannot see.
    fn leak_profile(levels: u32, margin: u32, band: u32) -> Vec<f32> {
        let (out, side, band) = leak_plane(levels, margin, band);
        let limit = side / 2 - band;
        let mut leak = vec![0.0f32; limit as usize + 1];
        for y in band..(side - band) {
            for x in band..(side - band) {
                let inset = x.min(y).min(side - 1 - x).min(side - 1 - y);
                let d = (inset - band) as usize;
                if d < leak.len() {
                    let v = out.data()[(y * side + x) as usize].abs();
                    if v > leak[d] {
                        leak[d] = v;
                    }
                }
            }
        }
        leak
    }

    /// Measures the margin directly against the criterion this module's
    /// "Reach and the measured tile-overlap margin" section states, using
    /// the same adversarial worst-case argument. Candidates ascend through
    /// multiples of `2^levels` -- the published margin has to be on the
    /// decimation grid, since that is what puts a tile's read origin on it
    /// -- and the first candidate whose own geometry leaks no more than
    /// [`MARGIN_THRESHOLD_LSB`] into its window wins: a fixed point, not a
    /// root, since each candidate changes the geometry it is judged in.
    fn measure_margin(levels: u32) -> u32 {
        let alignment = 1u32 << levels;
        let analytic = 4 * ((1u32 << levels) - 1);
        if analytic == 0 {
            return 0;
        }
        let mut margin = alignment;
        loop {
            let leak = leak_profile(levels, margin, margin);
            // The criterion is about the window inset by the margin, so
            // the leak that counts is the one at distance `margin` and
            // beyond -- right at the band the leak is, of course, the full
            // scale of the content that differs.
            let worst = leak
                .iter()
                .skip(margin as usize)
                .copied()
                .fold(0.0f32, |acc: f32, v: f32| if v > acc { v } else { acc });
            if worst <= MARGIN_THRESHOLD_LSB {
                return margin;
            }
            assert!(
                margin <= 4 * analytic,
                "the margin search must converge well inside four analytic reaches: \
                 levels={levels}, margin={margin}, worst={worst}"
            );
            margin += alignment;
        }
    }

    #[test]
    fn stated_margin_matches_the_measurement() {
        let started = std::time::Instant::now();
        let mut computed = [0u32; MEASURED_MARGIN.len()];
        for n in 0..=7u32 {
            computed[n as usize] = measure_margin(n);
        }
        // Asserted as a whole table rather than entry by entry: a
        // re-measurement (a changed threshold, a changed kernel) reports
        // every value that moved in one failure message instead of one per
        // run.
        assert_eq!(
            MEASURED_MARGIN, computed,
            "MEASURED_MARGIN must equal the freshly measured margin at every level"
        );
        let _elapsed = started.elapsed();
    }

    #[test]
    fn the_border_rule_is_identical_in_both_directions() {
        // The measurement buffer is symmetric under transposition, so its
        // flatten response must be too: the same kernel, the same
        // clamp-to-edge and the same tap order are applied on whichever
        // axis a given pass calls "x". A future edit clamping one axis
        // differently from the other breaks this first.
        //
        // Not *bit* for bit, and the reason is itself pinned behaviour:
        // both `reduce` and `expand` run the horizontal pass first and the
        // vertical second, and `f32` addition is not associative, so the
        // two axes are not interchangeable at the last ulp even when the
        // rule they apply is identical. The tolerance below is a few ulps
        // of the local magnitude, not a fixed epsilon, so it stays
        // meaningful at both ends of the range.
        let levels = 4u32;
        let m = MEASURED_MARGIN[levels as usize];
        let (out, side, _band) = leak_plane(levels, m, m);
        for y in 0..side {
            for x in 0..side {
                let a = out.data()[(y * side + x) as usize];
                let b = out.data()[(x * side + y) as usize];
                let tolerance = 4.0 * f32::EPSILON * a.abs().max(b.abs()).max(1.0);
                assert!(
                    (a - b).abs() <= tolerance,
                    "transposed samples must agree to a few ulps at ({x},{y}): \
                     {a} vs {b}, tolerance {tolerance}"
                );
            }
        }
    }

    #[test]
    fn an_off_grid_window_boundary_reaches_further() {
        // The published table's precondition, pinned so it cannot rot:
        // every entry is measured for a window boundary that sits on the
        // decimation grid, and a boundary one pixel off that grid leaks
        // *further* into the window -- the coarse samples straddling it
        // carry different weight. `tile::plan` never creates one (both the
        // margin and the interior stride are multiples of `2^levels`),
        // which is what makes the aligned measurement the applicable one
        // rather than an optimistic one.
        let levels = 3u32;
        let alignment = 1u32 << levels;
        let margin = MEASURED_MARGIN[levels as usize];
        // The distance at which the leak from a band starting at `band`
        // finally falls to the threshold: the margin that boundary would
        // need.
        let need = |band: u32| -> u32 {
            leak_profile(levels, margin, band)
                .iter()
                .enumerate()
                .filter(|&(_, &v)| v > MARGIN_THRESHOLD_LSB)
                .map(|(d, _)| d as u32 + 1)
                .max()
                .unwrap_or(0)
        };
        let aligned = need(margin);
        assert!(
            aligned <= margin,
            "the published margin must cover an aligned boundary: needed {aligned}, published {margin}"
        );
        let worst_off_grid = (1..alignment)
            .map(|k| need(margin + k))
            .max()
            .expect("at least one off-grid phase");
        assert!(
            worst_off_grid > aligned,
            "an off-grid boundary must need a larger margin than an aligned one -- if it does \
             not, this table's alignment precondition has lost its reason: \
             aligned={aligned}, worst off-grid={worst_off_grid}"
        );
    }

    #[test]
    fn interior_is_margin_independent() {
        /// A seeded xorshift32 generator, distinct instance per call so
        /// each test buffer is independently reproducible.
        fn seeded_noise(len: usize, seed: u32) -> Vec<u16> {
            let mut rng = XorShift32(seed);
            (0..len).map(|_| (rng.next() % 65536) as u16).collect()
        }

        for &n in &[3u32, 7u32] {
            let margin = MEASURED_MARGIN[n as usize];
            // The band of maximally-different content sits immediately
            // outside the window (no gap), so a pixel *at* the window's
            // own edge is at distance 0 from that band, not distance
            // `margin` -- the published margin's guarantee only applies
            // `margin` px away from a difference. The region this test
            // asserts over is therefore the window inset by a *second*
            // margin, which is why `side` needs room for two margins
            // beyond the asserted core on every side (4 * margin total
            // across each axis). There is **no slack term**: the asserted
            // inset is exactly `2 * margin`, the number at which the
            // <= 1 LSB criterion actually holds.
            let side = (4 * margin + 64).max(4 << n);
            let spp = 3u32;
            let len = side as usize * side as usize * spp as usize;

            // `a`: seeded noise plus a step edge crossing the window
            // boundary -- adversarial content, not the fixture.
            let mut a = seeded_noise(len, 0xC0FFEE ^ n);
            let inset = margin;
            let window = side - 2 * inset;
            let assert_inset = inset + margin;
            let assert_window = side - 2 * assert_inset;
            for y in 0..side {
                for x in 0..side {
                    if x < side / 2 {
                        for c in 0..spp {
                            a[((y * side + x) * spp + c) as usize] = 65535;
                        }
                    }
                }
            }

            // `b`: identical to `a` inside the centred window, maximally
            // different (`65535 - a`) everywhere outside it. Same size,
            // same grid -- never a crop.
            let mut b = a.clone();
            for y in 0..side {
                for x in 0..side {
                    let inside =
                        x >= inset && x < inset + window && y >= inset && y < inset + window;
                    if !inside {
                        for c in 0..spp {
                            let idx = ((y * side + x) * spp + c) as usize;
                            b[idx] = 65535 - a[idx];
                        }
                    }
                }
            }

            for &scale_laplacians in &[false, true] {
                let run = |samples: &[u16]| -> Vec<u16> {
                    let mut pyramid = build(samples, side, side, spp, n).unwrap();
                    if scale_laplacians {
                        for k in 0..pyramid.level_count() {
                            for v in pyramid.level_mut(k).data_mut() {
                                *v *= 0.5;
                            }
                        }
                        let mut out = vec![0u16; len];
                        collapse_into(&pyramid, &mut out).unwrap();
                        out
                    } else {
                        // flatten path (zero every Laplacian).
                        for k in 0..pyramid.level_count() {
                            for v in pyramid.level_mut(k).data_mut() {
                                *v = 0.0;
                            }
                        }
                        let mut out = vec![0u16; len];
                        collapse_into(&pyramid, &mut out).unwrap();
                        out
                    }
                };

                let out_a = run(&a);
                let out_b = run(&b);

                for y in assert_inset..(assert_inset + assert_window) {
                    for x in assert_inset..(assert_inset + assert_window) {
                        for c in 0..spp {
                            let idx = ((y * side + x) * spp + c) as usize;
                            let da = out_a[idx] as i32;
                            let db = out_b[idx] as i32;
                            assert!(
                                (da - db).abs() <= 1,
                                "interior pixel ({x},{y},{c}) differs by {} at levels={n}, scale_laplacians={scale_laplacians}",
                                (da - db).abs()
                            );
                        }
                    }
                }
            }
        }
    }
}
