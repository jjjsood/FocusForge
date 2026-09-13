// SPDX-License-Identifier: GPL-3.0-or-later

//! Reads `.scratch/synth-align/measurement.json` -- the artefact
//! `tools/synth-align.sh` (and, underneath it,
//! `spikes/synthetic-ground-truth/synth_stack.py --warp`) writes -- and
//! asserts, machine-checked, that `focusforge-cli align` recovers a
//! per-frame transform close to a KNOWN ground truth, not merely a
//! transform with a plausible-looking residual.
//!
//! Every test here parses JSON and runs no alignment itself; the artefact is
//! generated once by `tools/synth-align.sh` and every test reads it. An
//! absent artefact skips with a printed reason, unless
//! `FF_REQUIRE_SYNTH_ALIGN=1` is set, where it panics naming the script.

mod common;

use std::path::PathBuf;

use focusforge_core::align::Transform;
use focusforge_core::tile::Rect;

/// The per-frame error budget, calibrated against the measured floor rather
/// than the eye-catching separation between the aligned and unaligned
/// numbers: the in-tree generator (noise added AFTER
/// the warp, Catmull-Rom resampler, rotation and scale about the frame
/// centre) measures a floor of 0.0139px at `|d| = 1` rising to 0.0359px at
/// `|d| = 4` over 24 frames at seeds 3/11/23. `0.04 + 0.03 * |d|` keeps
/// roughly 4-5x headroom everywhere. A legitimate future change (a
/// different interpolation kernel, a changed pyramid depth, a new
/// `epsilon-px` default) crossing this budget is the INTENDED failure mode:
/// the correct response is a deliberate re-tuning that records the new
/// measurement here, never a quiet loosening of the constant.
const BUDGET_BASE_PX: f64 = 0.04; // |d| = 0
const BUDGET_PER_STEP_PX: f64 = 0.03; // per step away from the reference

fn budget_px(d: i64) -> f64 {
    BUDGET_BASE_PX + BUDGET_PER_STEP_PX * d.abs() as f64
}

/// The maximum, over `region`'s four corners, of the Euclidean distance
/// between where `a` and `b` map that corner -- the px-error metric.
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
            let (ax, ay) = a.map_point(x, y).expect("ground truth stays in domain");
            let (bx, by) = b
                .map_point(x, y)
                .expect("recovered transform stays in domain");
            ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
        })
        .fold(0.0f64, f64::max)
}

/// The centred region `PyramidalEcc::solve_region` actually fits over
/// (`REGION_FRACTION = 0.75`, identical to `default_region()`'s rect for a
/// 512px frame). Printed alongside the full-frame number, but only the
/// full-frame number is asserted here, since it is the stricter,
/// user-visible quantity.
fn solve_region_rect(full: Rect) -> Rect {
    let w = (full.width as f64 * 0.75).round() as u32;
    let h = (full.height as f64 * 0.75).round() as u32;
    Rect {
        x: (full.width - w) / 2,
        y: (full.height - h) / 2,
        width: w,
        height: h,
    }
}

fn measurement_path() -> PathBuf {
    common::workspace_root().join(".scratch/synth-align/measurement.json")
}

/// Reads the artefact `tools/synth-align.sh` writes: `None` when absent,
/// a panic instead when `FF_REQUIRE_SYNTH_ALIGN=1` is set.
fn measurement() -> Option<serde_json::Value> {
    let path = measurement_path();
    if !path.is_file() {
        if std::env::var("FF_REQUIRE_SYNTH_ALIGN").as_deref() == Ok("1") {
            panic!(
                "the gate set FF_REQUIRE_SYNTH_ALIGN=1, so tools/synth-align.sh must have \
                 written {}; a skip here is a gate bug, not a pass",
                path.display()
            );
        }
        eprintln!(
            "skip: {} not found -- run tools/synth-align.sh first",
            path.display()
        );
        return None;
    }
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} exists but could not be read: {e}", path.display()));
    Some(
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} exists but is not valid JSON: {e}", path.display())),
    )
}

fn full_rect(m: &serde_json::Value) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: m["generator"]["width"].as_u64().expect("generator.width") as u32,
        height: m["generator"]["height"].as_u64().expect("generator.height") as u32,
    }
}

fn reference_index(m: &serde_json::Value) -> i64 {
    m["generator"]["reference"]
        .as_u64()
        .expect("generator.reference") as i64
}

/// Rebuilds a truth [`Transform`] from `truth[k].matrix`, the same 3x3 a
/// `Transform { m }` literal takes.
fn transform_from_matrix(v: &serde_json::Value) -> Transform {
    let rows = v.as_array().expect("matrix is a 3-row array");
    let mut m = [[0.0f64; 3]; 3];
    for (r, row) in rows.iter().enumerate() {
        let cols = row.as_array().expect("matrix row is a 3-value array");
        for (c, val) in cols.iter().enumerate() {
            m[r][c] = val.as_f64().expect("matrix entry is a number");
        }
    }
    Transform { m }
}

/// Rebuilds a recovered [`Transform`] from an `align --json` alignment
/// entry's four `similarity_parts` fields (`scale`, `rotation_deg`, `tx`,
/// `ty`) -- the same four fields `Transform::similarity_parts()` reports.
fn transform_from_parts(entry: &serde_json::Value) -> Transform {
    let scale = entry["scale"].as_f64().expect("scale");
    let rotation_deg = entry["rotation_deg"].as_f64().expect("rotation_deg");
    let tx = entry["tx"].as_f64().expect("tx");
    let ty = entry["ty"].as_f64().expect("ty");
    Transform::similarity(scale, rotation_deg.to_radians(), tx, ty)
}

fn alignment_for(report: &serde_json::Value, frame: usize) -> &serde_json::Value {
    report["alignments"]
        .as_array()
        .expect("alignments is an array")
        .iter()
        .find(|e| e["frame"].as_u64() == Some(frame as u64))
        .unwrap_or_else(|| panic!("no alignment entry for frame {frame}"))
}

/// The artefact is one document written once: the truth, its provenance and
/// **both** `align --json` reports verbatim -- so a recorded number can be
/// attributed to a configuration months later and no two halves of it can
/// come from different runs.
#[test]
fn truth_artefact_carries_its_provenance() {
    let Some(m) = measurement() else { return };
    assert_eq!(m["schema"], "focusforge synthetic-alignment-truth v1");

    let g = &m["generator"];
    for field in [
        "script",
        "scene",
        "seed",
        "warp_amplitude",
        "sigma_step",
        "noise_sd",
        "width",
        "height",
        "samples_per_pixel",
        "frame_count",
        "reference",
        "resampler",
        "compose_order",
        "cli_algo",
    ] {
        assert!(
            !g[field].is_null(),
            "generator.{field} missing from the artefact"
        );
    }
    assert_eq!(
        g["compose_order"],
        serde_json::json!(["defocus", "warp", "sensor-noise"]),
        "compose order must be recorded exactly, defocus-then-warp-then-noise"
    );
    println!("generator: {g}");

    let frame_count = g["frame_count"].as_u64().unwrap() as usize;
    let truth = m["truth"].as_array().expect("truth is an array");
    assert_eq!(truth.len(), frame_count, "one truth entry per frame");
    for t in truth {
        for field in [
            "frame",
            "name",
            "matrix",
            "scale",
            "rotation_deg",
            "tx",
            "ty",
        ] {
            assert!(!t[field].is_null(), "truth entry missing {field}");
        }
    }

    for mode in ["rotation-scale", "translation"] {
        let run = &m["runs"][mode];
        assert!(
            !run["reference"].is_null(),
            "runs.{mode} is not a verbatim align --json report (missing `reference`)"
        );
        assert_eq!(
            run["alignments"].as_array().unwrap().len(),
            frame_count,
            "runs.{mode} must report every frame"
        );
    }
}

/// The two independent direction verifications are recorded exactly, so a
/// later edit cannot drop the in-script checks silently.
#[test]
fn direction_checks_are_recorded_and_exact() {
    let Some(m) = measurement() else { return };
    let dc = &m["direction_checks"];
    println!("direction_checks: {dc}");
    assert_eq!(dc["impulse_in"], serde_json::json!([100, 100]));
    assert_eq!(dc["impulse_translation"], serde_json::json!([9, 5]));
    assert_eq!(dc["impulse_out"], serde_json::json!([109, 105]));
    assert_eq!(dc["nonzero_samples"], serde_json::json!(1));
}

/// The claim's positive case: with the aligner working in `rotation-scale`
/// mode, every frame's recovered transform stays inside a budget that
/// scales with `|k - reference|`.
#[test]
fn recovered_transforms_stay_inside_the_per_frame_budget() {
    let Some(m) = measurement() else { return };
    let rect = full_rect(&m);
    let region = solve_region_rect(rect);
    let reference = reference_index(&m);
    let good = &m["runs"]["rotation-scale"];

    println!(
        "{:<6}{:>5}{:>16}{:>16}{:>12}",
        "frame", "d", "full-rect px", "384-rect px", "budget px"
    );
    for t in m["truth"].as_array().unwrap() {
        let frame = t["frame"].as_u64().unwrap() as usize;
        let d = frame as i64 - reference;
        let truth_t = transform_from_matrix(&t["matrix"]);

        let entry = alignment_for(good, frame);
        assert_eq!(
            entry["status"], "converged",
            "frame {frame} did not converge in rotation-scale mode"
        );
        let recovered = transform_from_parts(entry);
        let full = corner_error(&truth_t, &recovered, rect);
        let sub = corner_error(&truth_t, &recovered, region);
        let budget = budget_px(d);
        println!("{frame:<6}{d:>5}{full:>16.4}{sub:>16.4}{budget:>12.4}");
        assert!(
            full <= budget,
            "frame {frame} (d={d}): corner error {full:.4}px exceeds budget {budget:.4}px"
        );
    }
}

/// The claim's separation case: the identity (no-alignment) baseline is at
/// least 20x each non-reference frame's budget, so the budget is a
/// statement about the ALIGNER and not about how large the warps are.
#[test]
fn unaligned_baseline_is_outside_the_budget() {
    let Some(m) = measurement() else { return };
    let rect = full_rect(&m);
    let reference = reference_index(&m);
    let identity = Transform::identity();

    println!(
        "{:<6}{:>5}{:>16}{:>16}",
        "frame", "d", "baseline px", "20x budget px"
    );
    for t in m["truth"].as_array().unwrap() {
        let frame = t["frame"].as_u64().unwrap() as usize;
        let d = frame as i64 - reference;
        let truth_t = transform_from_matrix(&t["matrix"]);
        let baseline = corner_error(&truth_t, &identity, rect);
        let floor = 20.0 * budget_px(d);
        println!("{frame:<6}{d:>5}{baseline:>16.4}{floor:>16.4}");
        if d == 0 {
            // The reference frame's truth IS identity -- nothing to separate
            // from, and a comparison against a zero baseline is vacuous.
            continue;
        }
        assert!(
            baseline >= floor,
            "frame {frame} (d={d}): identity baseline {baseline:.4}px is not >= 20x its budget ({floor:.4}px)"
        );
    }
}

/// The claim's whole reason to exist: `mode=translation` against warps
/// containing real rotation and scale puts every non-reference frame
/// outside its budget while every frame still reports `converged` with no
/// fallback, and at least one frame's ECC correlation overlaps the good
/// run's range while landing at least 20x further from the truth than that
/// same frame did in the good run -- demonstrating that the residual cannot
/// substitute for ground truth.
#[test]
fn translation_mode_fails_the_budget_while_reporting_a_confident_residual() {
    let Some(m) = measurement() else { return };
    let rect = full_rect(&m);
    let reference = reference_index(&m);
    let good = &m["runs"]["rotation-scale"];
    let bad = &m["runs"]["translation"];

    let good_min_rho = good["alignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["residual"].as_f64().unwrap())
        .fold(f64::INFINITY, f64::min);

    let mut overlap_found = false;
    println!(
        "{:<6}{:>5}{:>10}{:>16}{:>16}{:>10}",
        "frame", "d", "rho", "good-run px", "bad-run px", "x-further"
    );
    for t in m["truth"].as_array().unwrap() {
        let frame = t["frame"].as_u64().unwrap() as usize;
        let d = frame as i64 - reference;
        let truth_t = transform_from_matrix(&t["matrix"]);

        let bad_entry = alignment_for(bad, frame);
        assert_eq!(
            bad_entry["status"], "converged",
            "frame {frame} did not converge in translation mode -- status alone would already catch this"
        );
        assert_eq!(
            bad_entry["fallback"].as_bool(),
            Some(false),
            "frame {frame} fell back to identity in translation mode"
        );
        let bad_recovered = transform_from_parts(bad_entry);
        let bad_err = corner_error(&truth_t, &bad_recovered, rect);
        let rho = bad_entry["residual"].as_f64().unwrap();

        let good_entry = alignment_for(good, frame);
        let good_recovered = transform_from_parts(good_entry);
        let good_err = corner_error(&truth_t, &good_recovered, rect);
        let times_further = if good_err > 1e-9 {
            bad_err / good_err
        } else {
            f64::INFINITY
        };

        println!("{frame:<6}{d:>5}{rho:>10.4}{good_err:>16.4}{bad_err:>16.4}{times_further:>10.2}");

        if d != 0 {
            let budget = budget_px(d);
            assert!(
                bad_err > budget,
                "frame {frame} (d={d}): translation-mode error {bad_err:.4}px did not exceed its budget {budget:.4}px"
            );
            if rho >= good_min_rho && times_further >= 20.0 {
                overlap_found = true;
            }
        }
    }
    assert!(
        overlap_found,
        "expected at least one translation-mode frame to report rho >= the good run's lowest \
         ({good_min_rho:.4}) while landing >= 20x further from truth than in the good run -- \
         without this, the residual could still be assumed to separate the two runs cleanly"
    );
}

/// The second direction check, independent of the resampler's own
/// arithmetic: the sign of every recovered `tx` (from the good run) agrees
/// with the sign of the corresponding true `tx`, on at least six frames --
/// the count guards against an all-zero truth passing this vacuously.
#[test]
fn recovered_translation_signs_agree_with_the_truth() {
    let Some(m) = measurement() else { return };
    let n = m["direction_checks"]["sign_agreement_frames"]
        .as_u64()
        .expect("direction_checks.sign_agreement_frames");
    println!("sign_agreement_frames = {n}");
    assert!(
        n >= 6,
        "expected at least 6 frames with sign(recovered tx) == sign(true tx), got {n}"
    );
}
