// SPDX-License-Identifier: GPL-3.0-or-later

//! What a hit-cap frame actually does, at fixture scale, over
//! `fixtures/mantis-512`. Every path here resolves under `fixtures/` --
//! this file never reads the reference stack.

mod common;

use std::path::PathBuf;
use std::time::Instant;

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignStatus, TileReader,
};
use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{
    DivergenceReason, EccLevelRecord, EccStatus, PyramidalEcc, Transform,
};
use focusforge_core::io::{FrameError, TiffError, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::{Rect, Tile};

const FRAME_3: usize = 3;

fn open_fixture() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512")
}

fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Identical to `PyramidalEcc::solve_region`'s own 512px-level rect
/// (`crates/core/tests/align_algorithm.rs` documents the same equality),
/// which is what makes the truncation-cost corners meaningful.
fn default_region() -> Rect {
    Rect {
        x: 64,
        y: 64,
        width: 384,
        height: 384,
    }
}

/// The maximum, over `region`'s four corners, of the Euclidean distance
/// between where `a` and `b` map that corner -- the solver's own
/// termination measure, reused here as the deconfounded truncation-cost and
/// corner-error metric.
fn corner_error(a: &Transform, b: &Transform, region: Rect) -> f64 {
    let corners = [
        (region.x as f64, region.y as f64),
        ((region.x + region.width - 1) as f64, region.y as f64),
        (region.x as f64, (region.y + region.height - 1) as f64),
        (
            (region.x + region.width - 1) as f64,
            (region.y + region.height - 1) as f64,
        ),
    ];
    corners
        .iter()
        .map(|&(x, y)| {
            let (ax, ay) = a.map_point(x, y).expect("a stays in domain");
            let (bx, by) = b.map_point(x, y).expect("b stays in domain");
            ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
        })
        .fold(0.0f64, f64::max)
}

/// Fully resolved defaults for `PyramidalEcc` against `ctx` -- the same
/// pattern `align_algorithm.rs`'s `resolved_defaults` uses.
fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), ctx)
        .expect("defaults resolve cleanly");
    params::resolved_values(&resolved)
}

/// The shipped defaults, with `mode` and `max-iterations` overridden --
/// every other key (`epsilon-px`, `pyramid-depth = auto`, `reference-policy`,
/// `reference-frame`, the last unused here since the reference is set on
/// `AlignContext` directly) stays exactly as `PyramidalEcc::params` declares
/// it.
fn cfg_for(ctx: &StackContext, mode: &str, max_iterations: i64) -> ParamValues {
    let mut cfg = resolved_defaults(ctx);
    cfg.insert("mode".to_string(), ParamValue::Enum(mode.to_string()));
    cfg.insert(
        "max-iterations".to_string(),
        ParamValue::Int(max_iterations),
    );
    cfg
}

/// A synthetic in-memory `TileReader` over same-sized frames -- mirrors
/// `align_algorithm.rs`'s own `FramesReader` (private per test binary, so
/// reimplemented here rather than shared).
struct FramesReader {
    width: u32,
    height: u32,
    spp: u32,
    frames: Vec<Vec<u16>>,
}

impl TileReader for FramesReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let Some(src) = self.frames.get(frame) else {
            return Err(AlgorithmError::FrameOutOfRange {
                frame,
                frame_count: self.frames.len(),
            });
        };
        let expected = rect.width as usize * rect.height as usize * self.spp as usize;
        if out.len() != expected {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::BufferSize {
                    expected,
                    got: out.len(),
                },
            )));
        }
        let in_bounds = rect
            .x
            .checked_add(rect.width)
            .is_some_and(|xw| xw <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.height);
        if !in_bounds {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::OutOfBounds {
                    path: PathBuf::from("<memory>"),
                    requested: rect,
                    frame: (self.width, self.height),
                },
            )));
        }
        let stride = self.width as usize * self.spp as usize;
        let row_len = rect.width as usize * self.spp as usize;
        let mut off = 0;
        for r in rect.y..(rect.y + rect.height) {
            let row_start = r as usize * stride + rect.x as usize * self.spp as usize;
            out[off..off + row_len].copy_from_slice(&src[row_start..row_start + row_len]);
            off += row_len;
        }
        Ok(())
    }
}

/// `reference` is frame 0, `moving` is frame 1 -- the same convention
/// `align_algorithm.rs::two_frame_reader` uses.
fn two_frame_reader(
    width: u32,
    height: u32,
    spp: u32,
    reference: Vec<u16>,
    moving: Vec<u16>,
) -> FramesReader {
    FramesReader {
        width,
        height,
        spp,
        frames: vec![reference, moving],
    }
}

/// Reads frame 3's full tile from the fixture.
fn frame_3_tile() -> (Tile, u32, u32, u32) {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(FRAME_3).samples_per_pixel;
    let rect = full_rect(width, height);
    let tile = stack
        .read_tile(FRAME_3, rect)
        .expect("reading frame 3's tile");
    (tile, width, height, spp)
}

/// `First -> 0`, `Middle -> (frame_count - 1) / 2`, `Last -> frame_count -
/// 1` -- mirrors the private `crate::align::driver::reference_index`
/// formula, reimplemented here because an integration test cannot call a
/// private function.
fn reference_index(setting: &str, frame_count: usize) -> usize {
    match setting {
        "first" => 0,
        "last" => frame_count - 1,
        _ => (frame_count - 1) / 2,
    }
}

/// `Converged` < `HitCap` < `Stalled` < `Diverged`.
fn severity(status: &AlignStatus) -> u8 {
    match status {
        AlignStatus::Converged => 0,
        AlignStatus::HitCap => 1,
        AlignStatus::Stalled => 2,
        other => {
            assert!(matches!(other, AlignStatus::Diverged(_)), "{other:?}");
            3
        }
    }
}

/// One printed row of the distance-from-epsilon and step-trajectory
/// per-level measurements.
fn print_level_row(mode: &str, frame: usize, rec: &EccLevelRecord, epsilon_px: f64) {
    let ratio = rec.result.final_step.map(|s| s / epsilon_px);
    println!(
        "  mode={mode} frame={frame} level={} status={:?} iterations={} final_step={:?} \
         final_step/epsilon={ratio:?} correlation={:.6}",
        rec.level,
        rec.result.status,
        rec.result.iterations,
        rec.result.final_step,
        rec.result.correlation
    );
}

/// **Distance from epsilon**: for every `HitCap` frame of
/// `fixtures/mantis-512` under the shipped defaults, in both modes, the
/// final step measure and its ratio to `epsilon-px` -- per pyramid level,
/// so no statement compares a four-level iteration sum against a per-level
/// threshold.
#[test]
fn hit_cap_frames_report_their_distance_from_epsilon() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);
    let anchor = reference_index("middle", frame_count);
    assert_eq!(anchor, 3, "middle of 8 frames is (8-1)/2 = 3");

    for mode in ["translation", "rotation-scale"] {
        let cfg = cfg_for(&stack_ctx, mode, 50);
        for frame in 0..frame_count {
            if frame == anchor {
                continue;
            }
            let ctx = AlignContext::new(stack_ctx, anchor, &stack);
            let mut levels = Vec::new();
            let outcome = PyramidalEcc
                .align_frame_traced(&ctx, frame, &cfg, &mut levels)
                .expect("fixture frames register (fallback is a driver policy, not a solver one)");
            println!(
                "hit_cap_frames_report_their_distance_from_epsilon: mode={mode} frame={frame} \
                 outcome.status={:?} outcome.iterations={}",
                outcome.status, outcome.iterations
            );
            let mut worst = AlignStatus::Converged;
            for rec in &levels {
                // Levels above 0 stop at 100x the shipped `1e-4`.
                let epsilon_px = if rec.level > 0 { 1e-4 * 100.0 } else { 1e-4 };
                print_level_row(mode, frame, rec, epsilon_px);
                let step = rec.result.final_step;
                match rec.result.status {
                    EccStatus::HitCap | EccStatus::Stalled => assert!(
                        step.expect("a capped level always reports a step") >= epsilon_px,
                        "mode={mode} frame={frame} level={}: capped but final_step < threshold",
                        rec.level
                    ),
                    EccStatus::Converged => assert!(
                        step.expect("Converged always reports a step") < epsilon_px,
                        "mode={mode} frame={frame} level={}: Converged but final_step >= threshold",
                        rec.level
                    ),
                    EccStatus::Diverged(_) => {}
                }
                let level = AlignStatus::from(rec.result.status);
                if severity(&level) > severity(&worst) {
                    worst = level;
                }
            }
            if !matches!(
                outcome.status,
                AlignStatus::Diverged(
                    DivergenceReason::ValidationFailed | DivergenceReason::WeakMatch
                )
            ) {
                assert_eq!(
                    outcome.status, worst,
                    "mode={mode} frame={frame}: the frame reports the worst level status"
                );
            }
        }
    }
}

/// The step-trajectory classification:
///
/// | verdict | condition |
/// | --- | --- |
/// | `Stalled` | `max(w) / min(w) <= 1.05` |
/// | `Oscillating` | `sign(w_{i+1} - w_i)` changes >= 3 times |
/// | `Shrinking` | every `r_i = w_{i+1}/w_i < 1` and `w_W / w_1 <= 0.8` |
/// | `Mixed` | none of the above |
///
/// Evaluated in that order, so a sequence that is both flat and jittery
/// classifies `Stalled` (magnitude over sign).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trajectory {
    Shrinking,
    Stalled,
    Oscillating,
    Mixed,
}

fn classify_trajectory(window: &[f64]) -> Trajectory {
    assert!(window.len() >= 2, "need at least two steps to classify");
    let max = window.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min = window.iter().cloned().fold(f64::INFINITY, f64::min);
    if min > 0.0 && max / min <= 1.05 {
        return Trajectory::Stalled;
    }
    let diffs: Vec<f64> = window.windows(2).map(|w| w[1] - w[0]).collect();
    let mut sign_changes = 0u32;
    for pair in diffs.windows(2) {
        if pair[0].signum() != 0.0
            && pair[1].signum() != 0.0
            && pair[0].signum() != pair[1].signum()
        {
            sign_changes += 1;
        }
    }
    if sign_changes >= 3 {
        return Trajectory::Oscillating;
    }
    let ratios: Vec<f64> = window.windows(2).map(|w| w[1] / w[0]).collect();
    let all_shrinking = ratios.iter().all(|&r| r < 1.0);
    let total_ratio = window[window.len() - 1] / window[0];
    if all_shrinking && total_ratio <= 0.8 {
        return Trajectory::Shrinking;
    }
    Trajectory::Mixed
}

/// **Step trajectory**: classify the step trajectory of every `HitCap`
/// level over its last `min(10, iterations)` recorded steps.
#[test]
fn hit_cap_step_trajectory_is_classified() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);
    let anchor = reference_index("middle", frame_count);
    let epsilon_px = 1e-4;

    for mode in ["translation", "rotation-scale"] {
        let cfg = cfg_for(&stack_ctx, mode, 50);
        for frame in 0..frame_count {
            if frame == anchor {
                continue;
            }
            let ctx = AlignContext::new(stack_ctx, anchor, &stack);
            let mut levels = Vec::new();
            let _ = PyramidalEcc
                .align_frame_traced(&ctx, frame, &cfg, &mut levels)
                .expect("fixture frames register");
            for rec in &levels {
                if !matches!(rec.result.status, EccStatus::HitCap | EccStatus::Stalled) {
                    continue;
                }
                let steps: Vec<f64> = rec.trace.iter().map(|it| it.step).collect();
                let w = steps.len().min(10);
                let window = &steps[steps.len() - w..];
                let verdict = classify_trajectory(window);
                print!(
                    "hit_cap_step_trajectory_is_classified: mode={mode} frame={frame} \
                     level={} window={window:?} verdict={verdict:?}",
                    rec.level
                );
                if verdict == Trajectory::Shrinking {
                    let ratios: Vec<f64> = window.windows(2).map(|w| w[1] / w[0]).collect();
                    let log_sum: f64 = ratios.iter().map(|r| r.ln()).sum();
                    let r_bar = (log_sum / ratios.len() as f64).exp();
                    let w_last = *window.last().unwrap();
                    let extra = (epsilon_px / w_last).ln() / r_bar.ln();
                    let predicted_extra_iterations = extra.ceil();
                    println!(
                        " r_bar={r_bar:.6} predicted_additional_iterations={predicted_extra_iterations}"
                    );
                } else {
                    println!();
                }
            }
        }
    }
}

/// **Cap sensitivity**: the cap is measured, not assumed. For every frame
/// that hit-caps under the shipped default, sweep `max-iterations` upward and
/// record the truncation cost -- the same frame's transform at cap `C`
/// against its own transform at the sweep's largest cap, over
/// `default_region()`'s corners. Frame, content, focus distance and anchor
/// are identical on both sides, so they cancel exactly.
#[test]
fn raising_the_cap_is_measured_not_assumed() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);
    let anchor = reference_index("middle", frame_count);
    let region = default_region();

    let overall_start = Instant::now();
    // Caps sized to fit the file's wall-clock ceiling: this sweep measures
    // 93.4 s (the full `[50, 100, 200, 400, 1000]` sweep: 134.5 s).
    for (mode, caps) in [
        ("rotation-scale", vec![50i64, 200, 400, 1000]),
        ("translation", vec![50i64, 1000]),
    ] {
        // Which frames hit-cap under the shipped default, measured here.
        let default_cfg = cfg_for(&stack_ctx, mode, 50);
        let mut hit_cap_frames = Vec::new();
        for frame in 0..frame_count {
            if frame == anchor {
                continue;
            }
            let ctx = AlignContext::new(stack_ctx, anchor, &stack);
            let outcome = PyramidalEcc
                .align_frame(&ctx, frame, &default_cfg)
                .expect("fixture frames register");
            if matches!(outcome.status, AlignStatus::HitCap | AlignStatus::Stalled) {
                hit_cap_frames.push(frame);
            }
        }
        println!(
            "raising_the_cap_is_measured_not_assumed: mode={mode} hit_cap_frames={hit_cap_frames:?}"
        );

        let largest_cap = *caps.last().unwrap();
        for &frame in &hit_cap_frames {
            let largest_cfg = cfg_for(&stack_ctx, mode, largest_cap);
            let ctx = AlignContext::new(stack_ctx, anchor, &stack);
            let mut top_levels = Vec::new();
            let top_outcome = PyramidalEcc
                .align_frame_traced(&ctx, frame, &largest_cfg, &mut top_levels)
                .expect("fixture frames register");

            for &cap in &caps {
                // `cap == largest_cap` is exactly the run already made above
                // to establish `top_outcome` -- reused rather than solved a
                // second time, since the solve is deterministic (see
                // `tracing_does_not_change_the_result`) and wall clock is
                // this test's own scarce budget.
                let (outcome, levels) = if cap == largest_cap {
                    (top_outcome, top_levels.clone())
                } else {
                    let cfg = cfg_for(&stack_ctx, mode, cap);
                    let ctx = AlignContext::new(stack_ctx, anchor, &stack);
                    let mut levels = Vec::new();
                    let outcome = PyramidalEcc
                        .align_frame_traced(&ctx, frame, &cfg, &mut levels)
                        .expect("fixture frames register");
                    (outcome, levels)
                };
                let truncation_cost =
                    corner_error(&outcome.transform, &top_outcome.transform, region);
                let per_level: Vec<(u32, EccStatus, u32)> = levels
                    .iter()
                    .map(|r| (r.level, r.result.status, r.result.iterations))
                    .collect();
                println!(
                    "raising_the_cap_is_measured_not_assumed: mode={mode} frame={frame} cap={cap} \
                     status={:?} total_iterations={} per_level={per_level:?} \
                     truncation_cost_px={truncation_cost}",
                    outcome.status, outcome.iterations
                );
            }
        }
    }
    println!(
        "raising_the_cap_is_measured_not_assumed: wall clock = {:.3}s",
        overall_start.elapsed().as_secs_f64()
    );
}

/// **The confound-free arm**: one fixture frame warped by a known transform
/// at increasing amplitudes, registered against its own unwarped self --
/// zero focus difference by construction. If hit-cap appears here, the cap
/// alone explains it; if it never appears, defocus-driven content mismatch
/// is implicated for the real frames.
#[test]
fn hit_cap_without_a_focus_difference() {
    let (tile, width, height, spp) = frame_3_tile();
    let rect = full_rect(width, height);
    let region = default_region();
    let stack_ctx = StackContext::new(2, width, height, spp);

    let cases: Vec<(&str, Transform)> = vec![
        ("translation 1px", Transform::translation(1.0, 0.0)),
        ("translation 5px", Transform::translation(5.0, 0.0)),
        ("translation 20px", Transform::translation(20.0, 0.0)),
        ("translation 80px", Transform::translation(80.0, 0.0)),
        ("similarity 1.002x/2deg", {
            let (cx, cy) = (
                (region.x + region.width / 2) as f64,
                (region.y + region.height / 2) as f64,
            );
            let to_origin = Transform::translation(-cx, -cy);
            let rot_scale = Transform::similarity(1.002, 2.0f64.to_radians(), 0.0, 0.0);
            let back = Transform::translation(cx, cy);
            back.compose(&rot_scale).compose(&to_origin)
        }),
    ];

    for (label, w_true) in cases {
        let mode = if label.starts_with("translation") {
            "translation"
        } else {
            "rotation-scale"
        };
        let warped = warp_from_tile(&tile, (width, height), rect, &w_true)
            .expect("amplitude stays in domain");
        let reader = two_frame_reader(
            width,
            height,
            spp,
            warped.tile.samples,
            tile.samples.clone(),
        );
        let cfg = cfg_for(&stack_ctx, mode, 50);
        let ctx = AlignContext::new(stack_ctx, 0, &reader);
        let mut levels = Vec::new();
        let outcome = PyramidalEcc
            .align_frame_traced(&ctx, 1, &cfg, &mut levels)
            .expect("same-content pair registers");
        let error = corner_error(&outcome.transform, &w_true, region);
        println!(
            "hit_cap_without_a_focus_difference: case={label} status={:?} corner_error_px={error}",
            outcome.status
        );
        for rec in &levels {
            println!(
                "  level={} status={:?} final_step={:?}",
                rec.level, rec.result.status, rec.result.final_step
            );
        }
    }
}

/// **Distance from anchor**: hit-cap status against `|frame - anchor|`,
/// over the 21 non-anchor pairs the three `reference-frame` settings
/// produce, at the default cap. The test asserts nothing about
/// correlation; it states present / absent / indeterminate.
#[test]
fn hit_cap_against_distance_from_the_anchor() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);

    let mut rows = Vec::new();

    for anchor_setting in ["first", "middle", "last"] {
        let anchor = reference_index(anchor_setting, frame_count);
        for mode in ["translation", "rotation-scale"] {
            let cfg = cfg_for(&stack_ctx, mode, 50);
            for frame in 0..frame_count {
                if frame == anchor {
                    continue;
                }
                let ctx = AlignContext::new(stack_ctx, anchor, &stack);
                let mut levels = Vec::new();
                let outcome = PyramidalEcc
                    .align_frame_traced(&ctx, frame, &cfg, &mut levels)
                    .expect("fixture frames register");
                let distance = frame.abs_diff(anchor);
                let finest = levels.iter().find(|r| r.level == 0);
                println!(
                    "hit_cap_against_distance_from_the_anchor: anchor={anchor_setting}({anchor}) \
                     mode={mode} frame={frame} distance={distance} status={:?} \
                     iterations={} finest_final_step={:?}",
                    outcome.status,
                    outcome.iterations,
                    finest.and_then(|r| r.result.final_step)
                );
                rows.push((distance, outcome.status));
            }
        }
    }

    // Contingency table: distance -> counts of Converged / HitCap / Diverged.
    let max_distance = rows.iter().map(|(d, _)| *d).max().unwrap_or(0);
    for d in 0..=max_distance {
        let (mut converged, mut hit_cap, mut stalled, mut diverged) = (0u32, 0u32, 0u32, 0u32);
        for (dist, status) in &rows {
            if *dist != d {
                continue;
            }
            match status {
                AlignStatus::Converged => converged += 1,
                AlignStatus::HitCap => hit_cap += 1,
                AlignStatus::Stalled => stalled += 1,
                AlignStatus::Diverged(_) => diverged += 1,
                other => panic!("unhandled AlignStatus variant: {other:?}"),
            }
        }
        if converged + hit_cap + stalled + diverged > 0 {
            println!(
                "hit_cap_against_distance_from_the_anchor: |frame-anchor|={d} converged={converged} \
                 hit_cap={hit_cap} stalled={stalled} diverged={diverged}"
            );
        }
    }
}
