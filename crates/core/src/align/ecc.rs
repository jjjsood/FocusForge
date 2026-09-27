// SPDX-License-Identifier: GPL-3.0-or-later

//! The single-level inverse-compositional ECC (Enhanced Correlation
//! Coefficient) solver: recovers a [`Transform`] between a template and a
//! moving [`Plane`], for a translation-only and a rotation+scale
//! (similarity) parameterization.
//!
//! ## The objective
//!
//! `I_r` is the template (reference), `I_w` the moving image. `y = φ(x; p)`
//! maps **template** coordinates into moving-image coordinates, so
//! `I_r(x) = I_w(φ(x; p))`. Overbar denotes zero-mean; the objective
//! maximizes `ρ(p) = (ī_r^t ī_w(p)) / (‖ī_r‖ · ‖ī_w(p)‖)`, i.e. **zero-mean
//! normalized cross-correlation** -- invariant to a per-frame multiplicative
//! gain *and* additive bias, unlike plain SSD Lucas-Kanade: a focus sweep
//! changes brightness frame to frame.
//!
//! **`ρ ∈ [-1, 1]`, never `[0, 1]`**: the objective can lock onto a negative
//! correlation, so "diverged" is never defined as though ρ were
//! non-negative.
//!
//! ## The inverse-compositional update
//!
//! With `Ḡ` the **column**-zero-mean design matrix (row `n` is
//! `∇T(x_n)^t · J(x_n)`, `J` the warp Jacobian at the identity warp) and
//! `C = Ḡ^tḠ`:
//!
//! ```text
//! Gt  = Ḡ^t ī_r,           Gw = Ḡ^t ī_w
//! v   = C^-1 Gt
//! num = ‖ī_r‖^2 - Gt^t v,  den = ī_r^t ī_w - Gw^t v
//! λ   = num / den
//! Δp  = C^-1 (λ · Gw - Gt)
//! ```
//!
//! `den <= 0` reports [`DivergenceReason::LemmaOneRegion`]. `Ḡ` is
//! column-centred, not the raw design matrix: with `ī_r`/`ī_w` zero-mean,
//! the centred projector annihilates the constant vector and so acts on
//! the space the data vectors actually live in. Zero-meaning is over the
//! current overlap only, recomputed whenever validity changes -- this is
//! load-bearing, not an optimization.
//!
//! ## Composition -- `W ← W · ΔW^-1`
//!
//! The increment is inverted and composed on the **right** (`Transform::compose`'s "right operand
//! applied first" order): under the template-to-moving-image convention the increment gets
//! conjugated by the current warp, so the opposite order is not equivalent. Measured (template
//! warped by 20° about its centre plus a translation, 3-DOF Euclidean IC): the opposite order
//! converges **linearly** (ratio ≈ 0.37 every step) while this order converges **quadratically**; a
//! pure translation makes the two orders identical, since translations commute.
//!
//! ## The similarity Jacobian
//!
//! With `p = [c, s, tx, ty]` and
//! `ΔW = [[1+Δp0, -Δp1, Δp2], [Δp1, 1+Δp0, Δp3], [0, 0, 1]]`:
//!
//! ```text
//! ∂φ/∂p |_identity = [ x  -y  1  0 ;
//!                      y   x  0  1 ]
//! ```
//!
//! `ΔW^-1` has the closed form (`ΔW = [[a, -b, tx], [b, a, ty], [0, 0, 1]]`):
//!
//! ```text
//! ΔW^-1 = (1 / (a^2 + b^2)) * [ a   b  -(a*tx + b*ty) ;
//!                               -b  a   (b*tx - a*ty) ;
//!                               0   0    a^2 + b^2     ]
//! ```
//!
//! **`x, y` above are centred on `region`'s own centre, not the image
//! origin** (`similarity_jacobian_row`): with uncentred coordinates, the
//! scale and rotation columns' `C = Ḡ^tḠ` diagonal grows as the region's
//! absolute offset squared while the translation columns stay flat, and
//! [`solve_linear`]'s relative pivot threshold eventually rejects the
//! (still perfectly good) translation pivots -- confirmed on the full 16GB
//! reference stack, where `mode=rotation-scale` diverged as
//! `singular-hessian` on 60 of 61 frames. Centring the Jacobian means the
//! recovered `Δp` is an increment about the region's centre, not the image
//! origin, so `similarity_delta` conjugates it back (`T(c) ∘ ΔW ∘ T(-c)`)
//! before composing it into `w`.
//!
//! ## Sampling and validity
//!
//! The solver reuses [`super::warp::catmull_rom_weights`], the crate's one bicubic Catmull-Rom
//! kernel, with its pinned horizontal-then-vertical accumulation order. **Validity is stricter than
//! the warp's own centre rule**: a sample counts only when its whole 4x4 tap window lies inside the
//! moving plane -- a clamped tap would fabricate motion signal. `region` is one rectangle, never a
//! per-pixel mask.

//! Out-of-domain samples are excluded from every accumulation and counted; a `valid_fraction` below
//! [`EccParams::min_valid_fraction`] ends the run as [`DivergenceReason::TooFewValidSamples`].
//!
//! `∇T`, the warp Jacobian, `Ḡ` and `C` are computed once, outside the
//! loop. `ī_r`, `Gt` and `v = C^-1 Gt` are loop-invariant **only when
//! every sample in `region` is valid on every iteration** -- checked at
//! runtime every iteration; when the check fails they are recomputed
//! rather than reusing a stale template mean.
//!
//! ## Termination
//!
//! Three signals: **converged** when the step measure -- the maximum, over
//! `region`'s four corners `x`, of `‖ΔW^-1(x) - x‖` in template pixels --
//! falls below [`EccParams::epsilon_px`]; **hit the cap** at
//! [`EccParams::max_iterations`] with no convergence; **diverged**, for a
//! singular Hessian, the Lemma 1 region, a non-invertible increment, too
//! few valid samples, a non-finite update, or a correlation that falls
//! more than a documented slack below the best ρ seen so far
//! ([`DivergenceReason::CorrelationDecreased`]). The step measure is used
//! rather than the raw `‖Δp‖`: in similarity mode that norm mixes
//! dimensionless `c`, `s` against pixel-valued `tx`, `ty`, so one epsilon
//! would mean two different things per mode, and could not be scaled
//! between pyramid levels the way a pixel corner displacement can. On
//! divergence, the returned transform is the **best-ρ transform seen**,
//! never the last one tried.
//!
//! The shipped default, `max-iterations = 400`, converges all 61
//! reference frames -- none hits the cap, the slowest needing 332
//! iterations -- for `+31 s` on a `182 s` alignment pass (`+17%`).
//!
//! Sample arithmetic is `f32`; every accumulation (means, dot products,
//! `C`, `Ḡ^t ·`) and the normal-equation solve are `f64`. The loop is
//! serial, not rayon: `f64` summation order is what makes a recovered
//! transform reproducible run to run, matching `crate::fuse::pyramid`.

use crate::align::error::AlignError;
use crate::align::transform::Transform;
use crate::align::warp::catmull_rom_weights;
use crate::fuse::pyramid::Plane;
use crate::tile::Rect;
use crate::timing::{TimingCounter, mark_timing_saturated, record_timing};

/// The two degree-of-freedom parameterizations [`solve_level`] supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EccMode {
    /// 2 DOF: `(tx, ty)`.
    Translation,
    /// 4 DOF: uniform scale, rotation and translation, `p = [c, s, tx, ty]`.
    Similarity,
}

/// The iteration's stopping controls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EccParams {
    /// The iteration cap. `0` is [`AlignError::InvalidParams`].
    pub max_iterations: u32,
    /// The convergence threshold, in template pixels, on the per-iteration
    /// step measure (see the module docs' "Termination"). Must be finite
    /// and positive.
    pub epsilon_px: f64,
    /// The minimum fraction of `region`'s pixels that must sample validly
    /// from the moving plane every iteration, or the run diverges as
    /// [`DivergenceReason::TooFewValidSamples`]. Must be finite and in
    /// `(0.0, 1.0]`.
    pub min_valid_fraction: f64,
}

/// Why a registration failed to converge. A data outcome
/// ([`EccStatus::Diverged`]), never an [`AlignError`] -- the caller did
/// nothing wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DivergenceReason {
    /// `C = Ḡ^tḠ`'s pivot test failed in the linear solve -- a flat,
    /// textureless template, most often.
    SingularHessian,
    /// The paper's Lemma 1 region: `den <= 0` would *decrease* the
    /// correlation.
    LemmaOneRegion,
    /// The incremental warp `ΔW` was not invertible.
    NonInvertibleIncrement,
    /// ρ fell more than this module's correlation slack below the best ρ
    /// seen so far.
    CorrelationDecreased,
    /// `valid_fraction < EccParams::min_valid_fraction`.
    TooFewValidSamples,
    /// `Δp`, the step measure, or ρ was non-finite.
    NonFiniteUpdate,
}

/// How a [`solve_level`] run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EccStatus {
    /// The step measure fell below [`EccParams::epsilon_px`].
    Converged,
    /// [`EccParams::max_iterations`] was reached with no convergence.
    HitCap,
    /// Registration failed; see [`DivergenceReason`].
    Diverged(DivergenceReason),
}

/// The outcome of one [`solve_level`] call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EccResult {
    /// Reference-to-frame, template as reference: a caller warps the
    /// moving image onto the template with this matrix directly, no
    /// inversion. On [`EccStatus::Diverged`] this is the **best-ρ
    /// transform seen**, not the last one tried.
    pub transform: Transform,
    pub status: EccStatus,
    pub iterations: u32,
    /// ρ ∈ `[-1, 1]`, paired with `transform` (the ρ that transform itself
    /// achieved, or the best-seen ρ on divergence) -- never an RMS.
    pub correlation: f64,
    pub valid_samples: usize,
    pub valid_fraction: f64,
    /// The step measure of the iteration that ended the run -- exactly the
    /// value compared against [`EccParams::epsilon_px`]. `None` when no
    /// iteration ever reached that comparison (the pre-loop singular return,
    /// or a divergence on the very first iteration).
    pub final_step: Option<f64>,
}

/// The relative pivot threshold [`solve_linear`] treats as singular: a
/// pivot below `PIVOT_EPS` times the input matrix's largest absolute entry.
/// `C`'s scale is `O(N · gradient^2)`, which varies by orders of magnitude
/// across region sizes and image content, so an absolute threshold would be
/// meaningless; this one is relative to the matrix actually being solved.
const PIVOT_EPS: f64 = 1e-9;

/// How far ρ may fall below the best value seen before a run is reported as
/// [`DivergenceReason::CorrelationDecreased`]. **Ours, not sourced** -- the
/// paper has no divergence test at all -- and deliberately a monotonicity
/// slack, not a ρ threshold: a run is expected to end near ρ ≈ 1 for a
/// well-conditioned registration and near ρ ≈ 0 for an unrelated pair, so no
/// single ρ value distinguishes "still converging" from "diverging".
const CORRELATION_SLACK: f64 = 1e-3;

/// Solves the `K x K` linear system `a * x = b` in `f64` by Gaussian
/// elimination with partial pivoting. `None` when a pivot's magnitude falls
/// below [`PIVOT_EPS`] times `a`'s largest absolute entry -- the
/// degenerate/near-singular case, reported by [`solve_level`] as
/// [`DivergenceReason::SingularHessian`] rather than propagated as a `NaN`.
#[allow(clippy::needless_range_loop)]
fn solve_linear<const K: usize>(mut a: [[f64; K]; K], mut b: [f64; K]) -> Option<[f64; K]> {
    let scale = a
        .iter()
        .flat_map(|row| row.iter())
        .fold(0.0f64, |acc, &v| acc.max(v.abs()));
    if scale == 0.0 {
        return None;
    }
    let threshold = PIVOT_EPS * scale;

    for col in 0..K {
        let mut pivot_row = col;
        let mut pivot_val = a[col][col].abs();
        for row in (col + 1)..K {
            if a[row][col].abs() > pivot_val {
                pivot_row = row;
                pivot_val = a[row][col].abs();
            }
        }
        if pivot_val < threshold {
            return None;
        }
        if pivot_row != col {
            a.swap(col, pivot_row);
            b.swap(col, pivot_row);
        }
        for row in (col + 1)..K {
            let factor = a[row][col] / a[col][col];
            if factor == 0.0 {
                continue;
            }
            for k in col..K {
                a[row][k] -= factor * a[col][k];
            }
            b[row] -= factor * b[col];
        }
    }

    let mut x = [0.0f64; K];
    for row in (0..K).rev() {
        let mut sum = b[row];
        for k in (row + 1)..K {
            sum -= a[row][k] * x[k];
        }
        x[row] = sum / a[row][row];
    }
    Some(x)
}

/// Whether every sample in `region` was valid this iteration -- the
/// loop-invariant hoist's precondition, checked every iteration and never
/// assumed.
fn has_full_overlap(valid_samples: usize, region_pixel_count: usize) -> bool {
    valid_samples == region_pixel_count
}

fn dot<const K: usize>(a: &[f64; K], b: &[f64; K]) -> f64 {
    a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum()
}

/// Samples `moving` at template-space coordinates `(x, y)` through `t`:
/// bicubic Catmull-Rom, reusing [`catmull_rom_weights`] and its pinned
/// horizontal-then-vertical accumulation order. `None` when `t.map_point`
/// fails or the mapped point's whole 4x4 tap window does not lie entirely
/// inside `moving` -- see the module docs' "Sampling and out-of-domain
/// handling".
fn sample_plane(moving: &Plane, t: &Transform, x: f64, y: f64) -> Option<f32> {
    let (sx, sy) = t.map_point(x, y)?;
    let width = moving.width() as i64;
    let height = moving.height() as i64;
    let ix = sx.floor() as i64;
    let iy = sy.floor() as i64;
    if ix - 1 < 0 || ix + 2 > width - 1 || iy - 1 < 0 || iy + 2 > height - 1 {
        return None;
    }

    let tx = (sx - ix as f64) as f32;
    let ty = (sy - iy as f64) as f32;
    let wx = catmull_rom_weights(tx);
    let wy = catmull_rom_weights(ty);

    let data = moving.data();
    let stride = width;
    let sample = |col: i64, row: i64| -> f32 { data[(row * stride + col) as usize] };

    let cols = [ix - 1, ix, ix + 1, ix + 2];
    let rows = [iy - 1, iy, iy + 1, iy + 2];
    let mut row_results = [0f32; 4];
    for (j, &row) in rows.iter().enumerate() {
        row_results[j] = wx[0] * sample(cols[0], row)
            + wx[1] * sample(cols[1], row)
            + wx[2] * sample(cols[2], row)
            + wx[3] * sample(cols[3], row);
    }
    Some(
        wy[0] * row_results[0]
            + wy[1] * row_results[1]
            + wy[2] * row_results[2]
            + wy[3] * row_results[3],
    )
}

/// Central-difference gradient `(T[x+1] - T[x-1]) / 2` on each axis, at
/// template pixel `(x, y)`. Caller must guarantee `1 <= x <= width - 2` and
/// `1 <= y <= height - 2` (the valid-region-only discipline
/// `crate::fuse::metrics::laplacian_variance` already uses).
fn central_diff_gradient(template: &Plane, x: u32, y: u32) -> (f32, f32) {
    let w = template.width() as usize;
    let data = template.data();
    let idx = |xx: u32, yy: u32| yy as usize * w + xx as usize;
    let gx = (data[idx(x + 1, y)] - data[idx(x - 1, y)]) / 2.0;
    let gy = (data[idx(x, y + 1)] - data[idx(x, y - 1)]) / 2.0;
    (gx, gy)
}

/// `(ī_r zero-mean vector, Gt, v = C^-1 Gt, num, ‖ī_r‖^2)`, the return shape
/// [`compute_ir_stats`] and the loop's full-overlap cache share.
type IrStats<const K: usize> = (Vec<f64>, [f64; K], [f64; K], f64, f64);

struct TimingBytes {
    counter: TimingCounter,
    bytes: u64,
    operations: u64,
    saturated: bool,
}

impl TimingBytes {
    fn new(counter: TimingCounter) -> Self {
        Self {
            counter,
            bytes: 0,
            operations: 0,
            saturated: false,
        }
    }

    fn add(&mut self, bytes: usize) {
        if bytes != 0 {
            self.bytes = match self.bytes.checked_add(bytes as u64) {
                Some(total) => total,
                None => {
                    self.saturated = true;
                    u64::MAX
                }
            };
            self.operations = match self.operations.checked_add(1) {
                Some(total) => total,
                None => {
                    self.saturated = true;
                    u64::MAX
                }
            };
        }
    }
}

impl Drop for TimingBytes {
    fn drop(&mut self) {
        if self.saturated {
            mark_timing_saturated();
        }
        if self.bytes != 0 {
            record_timing(self.counter, self.bytes);
            let operations = match self.counter {
                TimingCounter::AllocationBytes => TimingCounter::AllocationOperations,
                TimingCounter::CopyBytes => TimingCounter::CopyOperations,
                _ => return,
            };
            record_timing(operations, self.operations);
        }
    }
}

/// Template-only precompute, cached once full overlap is first observed:
/// `Gt`, `v = C^-1 Gt`, `num = ‖ī_r‖^2 - Gt^t v` and the zero-mean template
/// vector itself (needed for ρ's cross term every iteration).
struct CachedTemplate<const K: usize> {
    ir_zero: Vec<f64>,
    gt: [f64; K],
    v: [f64; K],
    num: f64,
    ir_ss: f64,
}

/// Recovers the [`Transform`] mapping `template`'s pixel coordinates into
/// `moving`'s, over `region`, starting from `initial`.
///
/// `template` and `moving` must both be single-channel
/// ([`AlignError::PlaneMismatch`] otherwise; they may differ in size).
/// `region` must be non-empty ([`AlignError::RegionTooSmall`]) and lie
/// entirely inside `template`'s gradient-valid interior (`1..width-1`,
/// `1..height-1`, [`AlignError::RegionOutOfBounds`] otherwise). `params`
/// must be well-formed ([`AlignError::InvalidParams`] otherwise). Every
/// other failure -- the solver simply not being able to register the pair --
/// is [`EccStatus::Diverged`] inside `Ok(EccResult)`, never an `Err`.
pub fn solve_level(
    template: &Plane,
    region: Rect,
    moving: &Plane,
    initial: &Transform,
    mode: EccMode,
    params: &EccParams,
) -> Result<EccResult, AlignError> {
    solve_level_dispatch(template, region, moving, initial, mode, params, None)
}

/// One recorded iteration of [`solve_level_traced`], in that level's own
/// pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EccIteration {
    /// 1-based, so the last entry's `iteration` equals
    /// [`EccResult::iterations`].
    pub iteration: u32,
    /// The step measure this iteration produced -- the quantity compared
    /// against [`EccParams::epsilon_px`].
    pub step: f64,
    /// ρ as of this iteration, before its update was applied.
    pub correlation: f64,
    pub valid_fraction: f64,
}

/// [`solve_level`], plus a per-iteration trace of the step measure and ρ,
/// appended to `trace` -- never cleared, so a caller that wants one trace per
/// call passes a fresh `Vec`. `trace.len() == result.iterations` holds on
/// every exit path, including divergence, because the push below sits at the
/// exact point the step measure is computed, before `iterations_done` is
/// incremented on that same iteration.
pub fn solve_level_traced(
    template: &Plane,
    region: Rect,
    moving: &Plane,
    initial: &Transform,
    mode: EccMode,
    params: &EccParams,
    trace: &mut Vec<EccIteration>,
) -> Result<EccResult, AlignError> {
    solve_level_dispatch(template, region, moving, initial, mode, params, Some(trace))
}

/// The validation and mode dispatch shared by [`solve_level`] and
/// [`solve_level_traced`]. `trace` is `None` on the plain path, so tracing
/// costs the untraced caller no allocation and no branch of real work beyond
/// one `Option` check per iteration inside [`solve_level_generic`].
fn solve_level_dispatch(
    template: &Plane,
    region: Rect,
    moving: &Plane,
    initial: &Transform,
    mode: EccMode,
    params: &EccParams,
    trace: Option<&mut Vec<EccIteration>>,
) -> Result<EccResult, AlignError> {
    if params.max_iterations == 0 {
        return Err(AlignError::InvalidParams {
            detail: "max_iterations must be at least 1".to_string(),
        });
    }
    if !params.epsilon_px.is_finite() || params.epsilon_px <= 0.0 {
        return Err(AlignError::InvalidParams {
            detail: format!(
                "epsilon_px must be finite and positive, got {}",
                params.epsilon_px
            ),
        });
    }
    if !params.min_valid_fraction.is_finite()
        || params.min_valid_fraction <= 0.0
        || params.min_valid_fraction > 1.0
    {
        return Err(AlignError::InvalidParams {
            detail: format!(
                "min_valid_fraction must be finite and in (0.0, 1.0], got {}",
                params.min_valid_fraction
            ),
        });
    }
    if template.samples_per_pixel() != 1 || moving.samples_per_pixel() != 1 {
        return Err(AlignError::PlaneMismatch {
            template_spp: template.samples_per_pixel(),
            moving_spp: moving.samples_per_pixel(),
        });
    }
    if region.width == 0 || region.height == 0 {
        return Err(AlignError::RegionTooSmall { region });
    }
    let valid_x_hi = template.width().saturating_sub(1);
    let valid_y_hi = template.height().saturating_sub(1);
    let region_ok = region.x >= 1
        && region.y >= 1
        && region.x + region.width <= valid_x_hi
        && region.y + region.height <= valid_y_hi;
    if !region_ok {
        return Err(AlignError::RegionOutOfBounds {
            region,
            plane: (template.width(), template.height()),
        });
    }

    match mode {
        EccMode::Translation => solve_level_generic::<2>(
            template,
            region,
            moving,
            initial,
            params,
            |gx, gy, _x, _y| [f64::from(gx), f64::from(gy)],
            |dp: &[f64; 2]| Transform::translation(dp[0], dp[1]),
            trace,
        ),
        EccMode::Similarity => {
            // The Jacobian's `x, y` are centred on `region`'s own centre,
            // not measured from the image origin -- see
            // `similarity_jacobian_row`'s doc comment for why -- and the
            // recovered increment is conjugated back to image coordinates
            // about the same centre in `similarity_delta`. The two must
            // share exactly this `(cx, cy)`, which is why both closures
            // below capture it from the same two `let`s rather than each
            // recomputing it.
            let cx = region.x as f64 + (region.width as f64 - 1.0) / 2.0;
            let cy = region.y as f64 + (region.height as f64 - 1.0) / 2.0;
            solve_level_generic::<4>(
                template,
                region,
                moving,
                initial,
                params,
                move |gx, gy, x, y| similarity_jacobian_row(gx, gy, x, y, cx, cy),
                move |dp: &[f64; 4]| similarity_delta(dp, cx, cy),
                trace,
            )
        }
    }
}

/// One (uncentred-argument, internally coordinate-centred) row of the
/// similarity Jacobian `∂φ/∂p |_identity` from the module docs' "The
/// similarity Jacobian" section: `[gx*x' + gy*y', gy*x' - gx*y', gx, gy]`,
/// where `x' = x - cx`, `y' = y - cy` are `(x, y)` measured from the
/// region's own centre `(cx, cy)`, never from the image origin.
///
/// # Why the coordinates must be centred
///
/// Columns 0-1 (scale, rotation) scale as `|coord| * gradient`; columns 2-3
/// (translation) are bare gradient and scale-invariant. With uncentred
/// `x, y`, `C = Ḡ^tḠ`'s diagonal for columns 0-1 grows as `offset^2` while
/// columns 2-3 stay flat, so [`solve_linear`]'s *relative* pivot threshold
/// (`PIVOT_EPS * max|a|`) rises with the region's absolute position until
/// the translation pivots fall under it:
///
/// ```text
/// offset (0,0)       max|C|=9.4761e8   diag=[9.476e8 9.429e8 2.180e4 2.182e4] solve=Some
/// offset (256,256)   max|C|=6.6518e9   diag=[6.643e9 6.652e9 2.180e4 2.182e4] solve=Some
/// offset (3500,2000) max|C|=3.8689e11  diag=[3.848e11 3.869e11 2.180e4 2.182e4] solve=NONE (singular)
/// ```
///
/// Confirmed on the full 16GB reference stack, where `mode=rotation-scale`
/// diverged as `singular-hessian` on 60 of 61 frames. Centring `x, y` on
/// the region's own centre removes the offset from the columns entirely.
///
/// Once the Jacobian is centred this way, the recovered `Δp` describes an
/// increment about `(cx, cy)`, not about the image origin, so
/// [`similarity_delta`] must conjugate it back -- skipping that pairs a
/// centred Jacobian with an origin-relative increment, converging
/// confidently to a wrong transform without reporting a singularity.
fn similarity_jacobian_row(gx: f32, gy: f32, x: f64, y: f64, cx: f64, cy: f64) -> [f64; 4] {
    let (tx, ty) = (f64::from(gx), f64::from(gy));
    let (xc, yc) = (x - cx, y - cy);
    [tx * xc + ty * yc, ty * xc - tx * yc, tx, ty]
}

/// Builds the incremental similarity warp from a solved `Δp`, conjugated
/// about `(cx, cy)`: `T(c) ∘ ΔW ∘ T(-c)`, i.e. `Transform::translation(cx,
/// cy).compose(&delta).compose(&Transform::translation(-cx, -cy))`, where
/// `delta = Transform::affine(1+Δp0, -Δp1, Δp1, 1+Δp0, Δp2, Δp3)`.
/// Required because [`similarity_jacobian_row`] centres its `x, y` on
/// `(cx, cy)`, so the recovered `Δp` must be conjugated back before
/// applying it to image coordinates.
fn similarity_delta(dp: &[f64; 4], cx: f64, cy: f64) -> Transform {
    let delta = Transform::affine(1.0 + dp[0], -dp[1], dp[1], 1.0 + dp[0], dp[2], dp[3]);
    Transform::translation(cx, cy)
        .compose(&delta)
        .compose(&Transform::translation(-cx, -cy))
}

/// The shared solve, generic over the parameterization's degree of freedom
/// `K`. `jacobian_row(grad_x, grad_y, x, y)` builds one row of `G` (never
/// *column*-centred here -- that happens below, uniformly for every mode;
/// a mode whose conditioning needs `x, y` themselves centred, as
/// [`EccMode::Similarity`] does, does that internally, see
/// [`similarity_jacobian_row`]); `build_delta(&Δp)` builds `ΔW` from a
/// solved increment (paired with any such internal centring by conjugating
/// the result back, see [`similarity_delta`]). See the module docs for
/// every equation used here.
#[allow(clippy::too_many_arguments)]
fn solve_level_generic<const K: usize>(
    template: &Plane,
    region: Rect,
    moving: &Plane,
    initial: &Transform,
    params: &EccParams,
    jacobian_row: impl Fn(f32, f32, f64, f64) -> [f64; K],
    build_delta: impl Fn(&[f64; K]) -> Transform,
    mut trace: Option<&mut Vec<EccIteration>>,
) -> Result<EccResult, AlignError> {
    let region_pixel_count = region.width as usize * region.height as usize;
    let mut allocated = TimingBytes::new(TimingCounter::AllocationBytes);
    let mut copied = TimingBytes::new(TimingCounter::CopyBytes);

    // ---- template precompute, once, outside the loop -------------------
    let mut positions = Vec::with_capacity(region_pixel_count);
    let mut template_values = Vec::with_capacity(region_pixel_count);
    let mut g_rows: Vec<[f64; K]> = Vec::with_capacity(region_pixel_count);
    allocated.add(positions.capacity() * std::mem::size_of::<(f64, f64)>());
    allocated.add(template_values.capacity() * std::mem::size_of::<f32>());
    allocated.add(g_rows.capacity() * std::mem::size_of::<[f64; K]>());
    for dy in 0..region.height {
        for dx in 0..region.width {
            let x = region.x + dx;
            let y = region.y + dy;
            let (gx, gy) = central_diff_gradient(template, x, y);
            let (xf, yf) = (x as f64, y as f64);
            positions.push((xf, yf));
            template_values
                .push(template.data()[y as usize * template.width() as usize + x as usize]);
            g_rows.push(jacobian_row(gx, gy, xf, yf));
        }
    }

    let mut col_mean = [0.0f64; K];
    for row in &g_rows {
        for k in 0..K {
            col_mean[k] += row[k];
        }
    }
    for m in &mut col_mean {
        *m /= region_pixel_count as f64;
    }
    let g_centered: Vec<[f64; K]> = g_rows
        .iter()
        .map(|row| {
            let mut r = [0.0f64; K];
            for k in 0..K {
                r[k] = row[k] - col_mean[k];
            }
            r
        })
        .collect();
    let centered_bytes = g_centered.capacity() * std::mem::size_of::<[f64; K]>();
    allocated.add(centered_bytes);

    let mut c = [[0.0f64; K]; K];
    for row in &g_centered {
        for i in 0..K {
            for j in 0..K {
                c[i][j] += row[i] * row[j];
            }
        }
    }

    // A flat, textureless template makes `C` the zero matrix: the pivot
    // test fires here, before ρ is ever formed.
    if solve_linear(c, [0.0f64; K]).is_none() {
        return Ok(EccResult {
            transform: *initial,
            status: EccStatus::Diverged(DivergenceReason::SingularHessian),
            iterations: 0,
            correlation: -1.0,
            valid_samples: 0,
            valid_fraction: 0.0,
            final_step: None,
        });
    }

    // ---- iteration state -------------------------------------------------
    let mut w = *initial;
    let mut best_transform = *initial;
    let mut best_correlation = -1.0f64;
    let mut iterations_done = 0u32;
    let mut cached: Option<CachedTemplate<K>> = None;
    // The step measure of the last iteration that reached the termination
    // comparison -- `None` until then, so a divergence before any iteration
    // completes reports no step. Declared here, with the other
    // iteration-state locals, and not after `macro_rules! diverge` below:
    // the macro's body resolves locals at its *definition* site, so
    // `last_step` must already be in scope where `diverge!` is defined.
    let mut last_step: Option<f64> = None;

    macro_rules! diverge {
        ($reason:expr, $valid_samples:expr, $valid_fraction:expr) => {
            return Ok(EccResult {
                transform: best_transform,
                status: EccStatus::Diverged($reason),
                iterations: iterations_done,
                correlation: best_correlation,
                valid_samples: $valid_samples,
                valid_fraction: $valid_fraction,
                final_step: last_step,
            });
        };
    }

    for _ in 0..params.max_iterations {
        // ---- sample the moving plane at W(x) for every x in region -----
        let mut iw_value: Vec<Option<f32>> = Vec::with_capacity(region_pixel_count);
        let sample_bytes = iw_value.capacity() * std::mem::size_of::<Option<f32>>();
        allocated.add(sample_bytes);
        let mut valid_count = 0usize;
        for &(x, y) in &positions {
            let s = sample_plane(moving, &w, x, y);
            if s.is_some() {
                valid_count += 1;
            }
            iw_value.push(s);
        }
        let valid_fraction = valid_count as f64 / region_pixel_count as f64;

        if valid_fraction < params.min_valid_fraction {
            diverge!(
                DivergenceReason::TooFewValidSamples,
                valid_count,
                valid_fraction
            );
        }

        let full = has_full_overlap(valid_count, region_pixel_count);

        // ---- ī_r, Gt, v, num: cached under guaranteed full overlap ------
        let (ir_zero, gt, v, num, ir_ss) = if full {
            if let Some(c) = &cached {
                let cloned = c.ir_zero.clone();
                allocated.add(cloned.capacity() * std::mem::size_of::<f64>());
                copied.add(cloned.len() * std::mem::size_of::<f64>());
                (cloned, c.gt, c.v, c.num, c.ir_ss)
            } else {
                let computed = compute_ir_stats(&template_values, &g_centered, &iw_value, c);
                let Some((ir_zero, gt, v, num, ir_ss)) = computed else {
                    diverge!(
                        DivergenceReason::SingularHessian,
                        valid_count,
                        valid_fraction
                    );
                };
                allocated.add(ir_zero.capacity() * std::mem::size_of::<f64>());
                let cached_ir_zero = ir_zero.clone();
                allocated.add(cached_ir_zero.capacity() * std::mem::size_of::<f64>());
                copied.add(cached_ir_zero.len() * std::mem::size_of::<f64>());
                cached = Some(CachedTemplate {
                    ir_zero: cached_ir_zero,
                    gt,
                    v,
                    num,
                    ir_ss,
                });
                (ir_zero, gt, v, num, ir_ss)
            }
        } else {
            let computed = compute_ir_stats(&template_values, &g_centered, &iw_value, c);
            let Some(result) = computed else {
                diverge!(
                    DivergenceReason::SingularHessian,
                    valid_count,
                    valid_fraction
                );
            };
            allocated.add(result.0.capacity() * std::mem::size_of::<f64>());
            result
        };

        // ---- ī_w, Gw, cross ---------------------------------------------
        let mut iw_sum = 0.0f64;
        for s in iw_value.iter().flatten() {
            iw_sum += f64::from(*s);
        }
        let iw_mean = iw_sum / valid_count as f64;

        let mut iw_zero = vec![0.0f64; region_pixel_count];
        let zero_bytes = iw_zero.capacity() * std::mem::size_of::<f64>();
        allocated.add(zero_bytes);
        for (n, s) in iw_value.iter().enumerate() {
            if let Some(v) = s {
                iw_zero[n] = f64::from(*v) - iw_mean;
            }
        }

        let mut gw = [0.0f64; K];
        let mut iw_ss = 0.0f64;
        let mut cross = 0.0f64;
        for n in 0..region_pixel_count {
            if iw_value[n].is_none() {
                continue;
            }
            for k in 0..K {
                gw[k] += g_centered[n][k] * iw_zero[n];
            }
            iw_ss += iw_zero[n] * iw_zero[n];
            cross += ir_zero[n] * iw_zero[n];
        }

        let rho = cross / (ir_ss.sqrt() * iw_ss.sqrt());
        if !rho.is_finite() {
            diverge!(
                DivergenceReason::NonFiniteUpdate,
                valid_count,
                valid_fraction
            );
        }

        // ---- best-ρ tracking, before this iteration's update ------------
        if iterations_done == 0 {
            best_correlation = rho;
            best_transform = w;
        } else if rho < best_correlation - CORRELATION_SLACK {
            diverge!(
                DivergenceReason::CorrelationDecreased,
                valid_count,
                valid_fraction
            );
        } else if rho > best_correlation {
            best_correlation = rho;
            best_transform = w;
        }

        let den = cross - dot(&gw, &v);
        if den <= 0.0 {
            diverge!(
                DivergenceReason::LemmaOneRegion,
                valid_count,
                valid_fraction
            );
        }
        let lambda = num / den;

        let mut rhs = [0.0f64; K];
        for k in 0..K {
            rhs[k] = lambda * gw[k] - gt[k];
        }
        let Some(delta_p) = solve_linear(c, rhs) else {
            diverge!(
                DivergenceReason::SingularHessian,
                valid_count,
                valid_fraction
            );
        };
        if delta_p.iter().any(|v| !v.is_finite()) {
            diverge!(
                DivergenceReason::NonFiniteUpdate,
                valid_count,
                valid_fraction
            );
        }

        let delta_w = build_delta(&delta_p);
        let delta_w_inv = match delta_w.inverse() {
            Ok(m) => m,
            Err(_) => {
                diverge!(
                    DivergenceReason::NonInvertibleIncrement,
                    valid_count,
                    valid_fraction
                );
            }
        };
        let new_w = w.compose(&delta_w_inv);

        // ---- termination: corner displacement of ΔW^-1 over region -----
        let corners = [
            (region.x as f64, region.y as f64),
            ((region.x + region.width - 1) as f64, region.y as f64),
            (region.x as f64, (region.y + region.height - 1) as f64),
            (
                (region.x + region.width - 1) as f64,
                (region.y + region.height - 1) as f64,
            ),
        ];
        let mut step = 0.0f64;
        for &(cx, cy) in &corners {
            let Some((mx, my)) = delta_w_inv.map_point(cx, cy) else {
                diverge!(
                    DivergenceReason::NonFiniteUpdate,
                    valid_count,
                    valid_fraction
                );
            };
            let d = ((mx - cx).powi(2) + (my - cy).powi(2)).sqrt();
            if d > step {
                step = d;
            }
        }
        last_step = Some(step);
        if let Some(t) = trace.as_deref_mut() {
            t.push(EccIteration {
                iteration: iterations_done + 1,
                step,
                correlation: rho,
                valid_fraction,
            });
        }

        w = new_w;
        iterations_done += 1;

        if step < params.epsilon_px {
            return Ok(EccResult {
                transform: w,
                status: EccStatus::Converged,
                iterations: iterations_done,
                correlation: rho,
                valid_samples: valid_count,
                valid_fraction,
                final_step: Some(step),
            });
        }

        if iterations_done == params.max_iterations {
            return Ok(EccResult {
                transform: w,
                status: EccStatus::HitCap,
                iterations: iterations_done,
                correlation: rho,
                valid_samples: valid_count,
                valid_fraction,
                final_step: Some(step),
            });
        }
    }

    unreachable!("the loop above always returns by max_iterations")
}

/// Computes `(ī_r zero-mean vector, Gt, v = C^-1 Gt, num, ‖ī_r‖^2)` over the
/// positions where `iw_value` is `Some` -- "the current overlap", per the
/// module docs. `None` propagates [`DivergenceReason::SingularHessian`]
/// from [`solve_linear`] (should not happen once the pre-loop check on `c`
/// has passed, but never propagated as a panic either way).
fn compute_ir_stats<const K: usize>(
    template_values: &[f32],
    g_centered: &[[f64; K]],
    iw_value: &[Option<f32>],
    c: [[f64; K]; K],
) -> Option<IrStats<K>> {
    let valid_count = iw_value.iter().filter(|v| v.is_some()).count();
    let mut ir_mean = 0.0f64;
    for (n, s) in iw_value.iter().enumerate() {
        if s.is_some() {
            ir_mean += f64::from(template_values[n]);
        }
    }
    ir_mean /= valid_count as f64;

    let mut ir_zero = vec![0.0f64; template_values.len()];
    let mut gt = [0.0f64; K];
    let mut ir_ss = 0.0f64;
    for n in 0..template_values.len() {
        if iw_value[n].is_none() {
            continue;
        }
        let v = f64::from(template_values[n]) - ir_mean;
        ir_zero[n] = v;
        ir_ss += v * v;
        for k in 0..K {
            gt[k] += g_centered[n][k] * v;
        }
    }

    let v = solve_linear(c, gt)?;
    let num = ir_ss - dot(&gt, &v);
    Some((ir_zero, gt, v, num, ir_ss))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_aggregate_saturation_is_retained_for_the_summary() {
        let mut allocations = TimingBytes::new(TimingCounter::AllocationBytes);
        allocations.bytes = u64::MAX - 1;
        allocations.operations = u64::MAX;
        allocations.add(2);

        assert_eq!(allocations.bytes, u64::MAX);
        assert_eq!(allocations.operations, u64::MAX);
        assert!(allocations.saturated);
    }

    fn constant_template(width: u32, height: u32, value: f32) -> Plane {
        Plane::from_f32(
            &vec![value; width as usize * height as usize],
            width,
            height,
            1,
        )
        .unwrap()
    }

    fn default_params() -> EccParams {
        EccParams {
            max_iterations: 30,
            epsilon_px: 1e-4,
            min_valid_fraction: 0.99,
        }
    }

    #[test]
    fn singular_hessian_is_reported() {
        let template = constant_template(20, 20, 1000.0);
        let moving = constant_template(20, 20, 1000.0);
        let region = Rect {
            x: 4,
            y: 4,
            width: 10,
            height: 10,
        };
        let result = solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &default_params(),
        )
        .expect("caller inputs are valid, so this is Ok");
        assert_eq!(
            result.status,
            EccStatus::Diverged(DivergenceReason::SingularHessian)
        );
    }

    #[test]
    fn solve_linear_hand_computed_diagonal_system() {
        // diag(1, 2, 3, 4) x = [2, 6, 12, 20] => x = [2, 3, 4, 5].
        let a = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 2.0, 0.0, 0.0],
            [0.0, 0.0, 3.0, 0.0],
            [0.0, 0.0, 0.0, 4.0],
        ];
        let b = [2.0, 6.0, 12.0, 20.0];
        let x = solve_linear(a, b).expect("diagonal matrix is non-singular");
        for (got, want) in x.iter().zip([2.0, 3.0, 4.0, 5.0]) {
            assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
        }
    }

    #[test]
    fn solve_linear_hand_computed_general_2x2() {
        // [[2, 1], [1, 3]] x = [5, 10] => x = [1, 3].
        let a = [[2.0, 1.0], [1.0, 3.0]];
        let b = [5.0, 10.0];
        let x = solve_linear(a, b).expect("well-conditioned 2x2");
        assert!((x[0] - 1.0).abs() < 1e-9);
        assert!((x[1] - 3.0).abs() < 1e-9);
    }

    #[test]
    fn solve_linear_rank_deficient_is_none() {
        let a = [
            [1.0, 2.0, 3.0, 4.0],
            [2.0, 4.0, 6.0, 8.0], // exact multiple of row 0
            [0.0, 1.0, 0.0, 1.0],
            [1.0, 0.0, 1.0, 0.0],
        ];
        let b = [1.0, 2.0, 3.0, 4.0];
        assert!(solve_linear(a, b).is_none());
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn similarity_inverse_matches_closed_form() {
        let (a, b, tx, ty) = (1.02, -0.08, 3.5, -2.25);
        let delta = Transform::affine(a, -b, b, a, tx, ty);
        let inv = delta.inverse().expect("well-conditioned similarity");

        let denom = a * a + b * b;
        let expected = [
            [a / denom, b / denom, -(a * tx + b * ty) / denom],
            [-b / denom, a / denom, (b * tx - a * ty) / denom],
            [0.0, 0.0, 1.0],
        ];
        for row in 0..3 {
            for col in 0..3 {
                assert!(
                    (inv.m[row][col] - expected[row][col]).abs() < 1e-12,
                    "row {row} col {col}: {} vs {}",
                    inv.m[row][col],
                    expected[row][col]
                );
            }
        }
    }

    #[test]
    fn full_overlap_precondition_is_checked() {
        assert!(has_full_overlap(100, 100));
        assert!(!has_full_overlap(99, 100));
        assert!(!has_full_overlap(0, 100));
    }

    /// [`full_overlap_precondition_is_checked`] only proves the predicate;
    /// [`out_of_domain_samples_are_dropped`] only proves the recompute
    /// branch (recomputing `ī_r`/`Gt`/`v`/`num` over the current, smaller
    /// mask instead of reading the full-overlap cache) is *taken*, not that
    /// its arithmetic is right: reusing the cached full-mask stats once the
    /// mask has shrunk would bias the Gauss-Newton update with
    /// contributions from pixels outside the current overlap.
    ///
    /// This test converges onto a partial overlap (a 16px translation whose
    /// region maps two of 120 columns outside the moving plane's domain),
    /// so the recompute branch produces the returned, checked result.
    const SINUSOID_WIDTH: u32 = 140;
    const SINUSOID_HEIGHT: u32 = 40;

    /// A smooth, non-periodic-within-domain synthetic texture (a sum of a
    /// few incommensurate sinusoids) rather than i.i.d. noise -- pixel noise
    /// gives central differences no spatial coherence to linearize against,
    /// so identity-start Newton steps overshoot and diverge (observed
    /// directly: `Diverged(CorrelationDecreased)` after 2 iterations with
    /// i.i.d. noise here). Real images -- what every other convergence test
    /// in this file and `ecc_solver.rs` uses -- have that coherence; this
    /// reproduces it synthetically. `template` is built by sampling
    /// `moving` through `w_true`; positions `w_true` maps out of `moving`'s
    /// domain have no real correspondence and are filled with `0.0`, never
    /// read by a solve whose region stays where `w_true` keeps it in
    /// domain.
    fn sinusoid_pair(width: u32, height: u32, w_true: &Transform) -> (Plane, Plane) {
        let mut moving_data = vec![0.0f32; width as usize * height as usize];
        for y in 0..height {
            for x in 0..width {
                let (xf, yf) = (f64::from(x), f64::from(y));
                let v = 2000.0
                    + 700.0 * (xf * 0.11 + 0.3).sin()
                    + 500.0 * (yf * 0.17 + 1.1).sin()
                    + 400.0 * ((xf + yf) * 0.05).sin()
                    + 300.0 * ((xf - 2.0 * yf) * 0.07).cos();
                moving_data[(y * width + x) as usize] = v as f32;
            }
        }
        let moving = Plane::from_f32(&moving_data, width, height, 1).unwrap();

        let mut template_data = vec![0.0f32; width as usize * height as usize];
        for y in 0..height {
            for x in 0..width {
                let sampled = sample_plane(&moving, w_true, f64::from(x), f64::from(y));
                template_data[(y * width + x) as usize] = sampled.unwrap_or(0.0);
            }
        }
        let template = Plane::from_f32(&template_data, width, height, 1).unwrap();
        (template, moving)
    }

    #[test]
    fn partial_overlap_recomputes_and_still_recovers() {
        let (width, height) = (SINUSOID_WIDTH, SINUSOID_HEIGHT);

        // The ground-truth transform: its region maps two columns past the
        // moving plane's right edge (see `sinusoid_pair`'s doc comment), so
        // `has_full_overlap` reads false once the solver has nearly
        // converged to it.
        let w_true = Transform::translation(16.0, 0.0);
        let (template, moving) = sinusoid_pair(width, height, &w_true);

        let region = Rect {
            x: 4,
            y: 4,
            width: 120,
            height: 30,
        };
        let params = EccParams {
            max_iterations: 50,
            epsilon_px: 1e-4,
            // Generous headroom against an early Newton step transiently
            // overshooting `w_true` and invalidating more than the two
            // columns the converged answer does -- the property under
            // test is the recompute branch's correctness, not this run's
            // exact overshoot behaviour.
            min_valid_fraction: 0.5,
        };

        let result = solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &params,
        )
        .expect("valid solver inputs");

        assert_eq!(result.status, EccStatus::Converged, "result: {result:?}");
        assert!(
            result.valid_fraction < 1.0,
            "the converged iteration had full overlap -- this test does not \
             exercise the partial-overlap recompute branch, valid_fraction = {}",
            result.valid_fraction
        );

        let corners = [
            (region.x as f64, region.y as f64),
            ((region.x + region.width - 1) as f64, region.y as f64),
            (region.x as f64, (region.y + region.height - 1) as f64),
            (
                (region.x + region.width - 1) as f64,
                (region.y + region.height - 1) as f64,
            ),
        ];
        let error = corners
            .iter()
            .map(|&(x, y)| {
                let (ax, ay) = w_true
                    .map_point(x, y)
                    .expect("ground truth stays in domain");
                let (bx, by) = result
                    .transform
                    .map_point(x, y)
                    .expect("recovered transform stays in domain");
                ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
            })
            .fold(0.0f64, f64::max);
        eprintln!(
            "partial_overlap_recomputes_and_still_recovers: corner error = {error} px, \
             valid_fraction = {}",
            result.valid_fraction
        );
        // Measured: corner error `5.79e-4` px, `valid_fraction` at
        // convergence `0.98333...` (118/120 columns valid, exactly the
        // predicted two-column loss). Tolerance set at roughly 100x that
        // measured figure -- headroom for platform-to-platform `f64`/`f32`
        // accumulation differences.
        assert!(
            error < 5e-2,
            "corner error {error} px exceeds tolerance -- the partial-overlap \
             recompute branch produced a wrong transform"
        );
    }

    /// A small `max_iterations` and a translation the solver has not fully
    /// resolved by then -- deliberately generous overlap (`min_valid_fraction`
    /// at its usual `0.99`, region well inside the plane for this small a
    /// translation) so nothing here can diverge; only convergence-vs-cap is
    /// in play.
    fn hit_cap_setup() -> (Plane, Plane, Rect, Transform) {
        let (width, height) = (SINUSOID_WIDTH, SINUSOID_HEIGHT);
        let w_true = Transform::translation(9.0, 3.0);
        let (template, moving) = sinusoid_pair(width, height, &w_true);
        let region = Rect {
            x: 4,
            y: 4,
            width: 120,
            height: 30,
        };
        (template, moving, region, w_true)
    }

    /// The bracketing check: pins [`EccResult::final_step`] as *the operand
    /// of the convergence test itself*, not merely a number that looks
    /// plausible. Only the compared quantity can make all three runs below
    /// hold simultaneously.
    #[test]
    fn hit_cap_reports_its_final_step() {
        let (template, moving, region, _w_true) = hit_cap_setup();
        let base_params = EccParams {
            max_iterations: 2,
            epsilon_px: 1e-12,
            min_valid_fraction: 0.99,
        };

        // Run A: a cap so tight the solver cannot possibly satisfy such a
        // tiny epsilon by then.
        let a = solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &base_params,
        )
        .expect("valid solver inputs");
        assert_eq!(a.status, EccStatus::HitCap, "run A: {a:?}");
        let s = a.final_step.expect("HitCap always reports a final_step");
        eprintln!("hit_cap_reports_its_final_step: max_iterations=2 final_step={s}");

        // Run B: epsilon just above `s` -- the same inputs must now report
        // Converged, at the same iteration, with the identical transform.
        let params_b = EccParams {
            epsilon_px: s * 1.000_001,
            ..base_params
        };
        let b = solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &params_b,
        )
        .expect("valid solver inputs");
        assert_eq!(b.status, EccStatus::Converged, "run B: {b:?}");
        assert_eq!(b.iterations, a.iterations, "run B iterations: {b:?}");
        assert_eq!(
            b.transform, a.transform,
            "run B transform must match run A's matrix-entry for matrix-entry"
        );

        // Run C: epsilon just below `s` -- must still report HitCap.
        let params_c = EccParams {
            epsilon_px: s * 0.999_999,
            ..base_params
        };
        let c = solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &params_c,
        )
        .expect("valid solver inputs");
        assert_eq!(c.status, EccStatus::HitCap, "run C: {c:?}");
    }

    /// `trace.len() == result.iterations` and `trace.last().step ==
    /// result.final_step` on a converged run, a hit-cap run and a diverged
    /// run; the pre-loop-singular case reports `final_step == None` with an
    /// empty trace.
    #[test]
    fn the_trace_length_matches_the_iteration_count() {
        let (template, moving, region, _w_true) = hit_cap_setup();

        let converged_params = EccParams {
            max_iterations: 50,
            epsilon_px: 1e-4,
            min_valid_fraction: 0.99,
        };
        let mut trace = Vec::new();
        let converged = solve_level_traced(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &converged_params,
            &mut trace,
        )
        .expect("valid solver inputs");
        assert_eq!(converged.status, EccStatus::Converged, "{converged:?}");
        assert_eq!(trace.len() as u32, converged.iterations);
        assert_eq!(trace.last().map(|i| i.step), converged.final_step);
        for (n, entry) in trace.iter().enumerate() {
            assert_eq!(entry.iteration, n as u32 + 1);
        }

        let hit_cap_params = EccParams {
            max_iterations: 2,
            epsilon_px: 1e-12,
            min_valid_fraction: 0.99,
        };
        let mut trace = Vec::new();
        let hit_cap = solve_level_traced(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &hit_cap_params,
            &mut trace,
        )
        .expect("valid solver inputs");
        assert_eq!(hit_cap.status, EccStatus::HitCap, "{hit_cap:?}");
        assert_eq!(trace.len() as u32, hit_cap.iterations);
        assert_eq!(trace.last().map(|i| i.step), hit_cap.final_step);

        // Diverged, past the first iteration: `out_of_domain_samples_are_dropped`'s
        // own setup (a translation large enough to starve the region below
        // `min_valid_fraction`) reproduced here rather than reused, since
        // that test's helper is private to its own function body.
        let diverge_params = EccParams {
            max_iterations: 50,
            epsilon_px: 1e-4,
            min_valid_fraction: 0.999,
        };
        let big_shift = Transform::translation(60.0, 0.0);
        let mut trace = Vec::new();
        let diverged = solve_level_traced(
            &template,
            region,
            &moving,
            &big_shift,
            EccMode::Translation,
            &diverge_params,
            &mut trace,
        )
        .expect("valid solver inputs");
        assert!(
            matches!(diverged.status, EccStatus::Diverged(_)),
            "{diverged:?}"
        );
        assert_eq!(trace.len() as u32, diverged.iterations, "{diverged:?}");
        assert_eq!(trace.last().map(|i| i.step), diverged.final_step);

        // The pre-loop singular case: a flat template makes `C` the zero
        // matrix before any iteration runs.
        let flat = constant_template(20, 20, 1000.0);
        let singular_region = Rect {
            x: 4,
            y: 4,
            width: 10,
            height: 10,
        };
        let mut trace = Vec::new();
        let singular = solve_level_traced(
            &flat,
            singular_region,
            &flat,
            &Transform::identity(),
            EccMode::Translation,
            &converged_params,
            &mut trace,
        )
        .expect("valid solver inputs");
        assert_eq!(
            singular.status,
            EccStatus::Diverged(DivergenceReason::SingularHessian)
        );
        assert_eq!(singular.final_step, None);
        assert!(trace.is_empty());
    }

    /// Instrumenting the solver changes no result: a traced and an untraced
    /// solve of the same inputs return an `EccResult` that is `==`, field
    /// for field, including the transform's matrix entries.
    #[test]
    fn tracing_does_not_change_the_result() {
        let (template, moving, region, _w_true) = hit_cap_setup();
        let params = EccParams {
            max_iterations: 6,
            epsilon_px: 1e-12,
            min_valid_fraction: 0.99,
        };

        let untraced = solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &params,
        )
        .expect("valid solver inputs");

        let mut trace = Vec::new();
        let traced = solve_level_traced(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &params,
            &mut trace,
        )
        .expect("valid solver inputs");

        assert_eq!(untraced, traced);
    }

    /// A deterministic xorshift32 PRNG, seeded with a fixed constant --
    /// never `rand`, mirroring `crate::fuse::pyramid::tests::XorShift32`
    /// (private there, so reimplemented here rather than exposed).
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

    #[test]
    fn out_of_domain_samples_are_dropped() {
        let (width, height) = (40u32, 40u32);
        let mut rng = XorShift32(0xC0FFEE);
        let mut data = vec![0.0f32; width as usize * height as usize];
        for v in &mut data {
            *v = (rng.next() % 4096) as f32;
        }
        let template = Plane::from_f32(&data, width, height, 1).unwrap();
        let moving = template.clone();

        // A translation shifting the region ~40% out of the moving plane's
        // domain, so the tap-window validity rule drops a large fraction
        // of the region's samples.
        let region = Rect {
            x: 4,
            y: 4,
            width: 30,
            height: 30,
        };
        let shift = Transform::translation(12.0, 0.0);
        let result = solve_level(
            &template,
            region,
            &moving,
            &shift,
            EccMode::Translation,
            &EccParams {
                max_iterations: 5,
                epsilon_px: 1e-4,
                min_valid_fraction: 0.99,
            },
        )
        .expect("caller inputs are valid");
        assert_eq!(
            result.status,
            EccStatus::Diverged(DivergenceReason::TooFewValidSamples)
        );
        assert!(result.valid_fraction < 1.0);
        assert!(result.valid_samples < region.width as usize * region.height as usize);
    }

    #[test]
    fn invalid_params_are_rejected() {
        let template = constant_template(20, 20, 500.0);
        let moving = template.clone();
        let region = Rect {
            x: 4,
            y: 4,
            width: 10,
            height: 10,
        };
        let bad = EccParams {
            max_iterations: 0,
            epsilon_px: 1e-4,
            min_valid_fraction: 0.99,
        };
        match solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &bad,
        ) {
            Err(AlignError::InvalidParams { .. }) => {}
            other => panic!("expected InvalidParams, got {other:?}"),
        }
    }

    #[test]
    fn region_outside_valid_interior_is_rejected() {
        let template = constant_template(10, 10, 500.0);
        let moving = template.clone();
        let region = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        match solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &default_params(),
        ) {
            Err(AlignError::RegionOutOfBounds { .. }) => {}
            other => panic!("expected RegionOutOfBounds, got {other:?}"),
        }
    }

    /// The permanent guard for the similarity Jacobian's conditioning
    /// defect: the same normal-matrix construction [`solve_level_generic`]
    /// uses, driven directly (no image sampling, no solver loop -- a pure
    /// linear-algebra check, ~0.04s), at a small region offset and a large
    /// one.
    ///
    /// With **uncentred** `x, y`, `C = Ḡ^tḠ`'s diagonal for the
    /// scale/rotation columns grows as the region's absolute offset
    /// squared while the translation columns stay flat:
    ///
    /// ```text
    /// offset (256,256)   diag=[6.643e9 6.652e9 2.180e4 2.182e4] solve=Some
    /// offset (3500,2000) diag=[3.848e11 3.869e11 2.180e4 2.182e4] solve=NONE (singular)
    /// ```
    ///
    /// [`similarity_jacobian_row`] centres `x, y` on the region's own
    /// centre before this construction runs, which is what makes both
    /// offsets below solve identically.
    #[test]
    fn similarity_normal_matrix_solves_regardless_of_region_offset() {
        // A deterministic, non-degenerate synthetic gradient field --
        // unrelated to any fixture, so this test needs no image data at
        // all. `region` itself never varies; only `offset_x`/`offset_y`
        // (added to every sample's coordinate before building the
        // Jacobian row) does, exactly the one variable the original repro
        // held everything else fixed against.
        let region = Rect {
            x: 0,
            y: 0,
            width: 32,
            height: 32,
        };
        let mut rng = XorShift32(0x5A11_EE5D);
        let gradients: Vec<(f32, f32)> = (0..region.width * region.height)
            .map(|_| {
                let gx = (rng.next() % 2000) as f32 - 1000.0;
                let gy = (rng.next() % 2000) as f32 - 1000.0;
                (gx, gy)
            })
            .collect();

        // Builds `C = Ḡ^tḠ` exactly the way `solve_level_generic` does
        // (column-mean-centred `g_rows`, then the outer-product sum), for
        // a region placed at `(offset_x, offset_y)` in some larger image.
        let normal_matrix = |offset_x: u32, offset_y: u32| -> [[f64; 4]; 4] {
            let cx = offset_x as f64 + (region.width as f64 - 1.0) / 2.0;
            let cy = offset_y as f64 + (region.height as f64 - 1.0) / 2.0;
            let mut rows: Vec<[f64; 4]> = Vec::with_capacity(gradients.len());
            for dy in 0..region.height {
                for dx in 0..region.width {
                    let (gx, gy) = gradients[(dy * region.width + dx) as usize];
                    let x = (offset_x + dx) as f64;
                    let y = (offset_y + dy) as f64;
                    rows.push(similarity_jacobian_row(gx, gy, x, y, cx, cy));
                }
            }
            let mut col_mean = [0.0f64; 4];
            for row in &rows {
                for k in 0..4 {
                    col_mean[k] += row[k];
                }
            }
            for m in &mut col_mean {
                *m /= rows.len() as f64;
            }
            let mut c = [[0.0f64; 4]; 4];
            for row in &rows {
                let centered: [f64; 4] = std::array::from_fn(|k| row[k] - col_mean[k]);
                for i in 0..4 {
                    for j in 0..4 {
                        c[i][j] += centered[i] * centered[j];
                    }
                }
            }
            c
        };

        let c_small_offset = normal_matrix(256, 256);
        let c_large_offset = normal_matrix(3500, 2000);

        assert!(
            solve_linear(c_small_offset, [0.0f64; 4]).is_some(),
            "small-offset (256, 256) region: C = {c_small_offset:?}"
        );
        assert!(
            solve_linear(c_large_offset, [0.0f64; 4]).is_some(),
            "large-offset (3500, 2000) region: C = {c_large_offset:?} -- without centring \
             the Jacobian's x, y on the region's own centre, this offset is singular \
             under PIVOT_EPS even though the underlying alignment problem is perfectly \
             well-posed"
        );
    }

    #[test]
    fn plane_mismatch_is_rejected() {
        let template =
            Plane::from_f32(&vec![1.0f32; 10 * 10 * 3], 10, 10, 3).expect("3-channel plane");
        let moving = constant_template(10, 10, 500.0);
        let region = Rect {
            x: 2,
            y: 2,
            width: 5,
            height: 5,
        };
        match solve_level(
            &template,
            region,
            &moving,
            &Transform::identity(),
            EccMode::Translation,
            &default_params(),
        ) {
            Err(AlignError::PlaneMismatch { .. }) => {}
            other => panic!("expected PlaneMismatch, got {other:?}"),
        }
    }
}
