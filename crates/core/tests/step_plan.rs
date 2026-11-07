// SPDX-License-Identifier: GPL-3.0-or-later
//! The step calculator: optics model, integer-nanometre frame count and the
//! parameter schema.

use focusforge_core::host;
use focusforge_core::params::{ParamKind, ParamValue, ParamValues, resolve_in, resolved_values};
use focusforge_core::step_plan::{Optics, PLAN_PARAMS, StepPlan, frame_count, plan};

fn resolved(pairs: &[(&str, ParamValue)]) -> ParamValues {
    let explicit: ParamValues = pairs
        .iter()
        .map(|(id, value)| ((*id).to_string(), value.clone()))
        .collect();
    let resolved = resolve_in(&PLAN_PARAMS, &explicit, None, &host::snapshot())
        .expect("test settings are inside the schema bounds");
    resolved_values(&resolved)
}

fn f(value: f64) -> ParamValue {
    ParamValue::Float(value)
}

fn lens_plan(magnification: f64, aperture: f64, overlap: f64) -> StepPlan {
    plan(&resolved(&[
        ("optics", ParamValue::Enum("lens".into())),
        ("magnification", f(magnification)),
        ("aperture", f(aperture)),
        ("overlap", f(overlap)),
    ]))
}

fn objective_plan(na: f64, overlap: f64) -> StepPlan {
    plan(&resolved(&[
        ("optics", ParamValue::Enum("objective".into())),
        ("na", f(na)),
        ("overlap", f(overlap)),
    ]))
}

#[test]
fn step_plan_lens_reference_point() {
    let p = lens_plan(1.0, 8.0, 0.25);
    assert_eq!(p.optics, Optics::Lens { aperture: 8.0 });
    assert_eq!(p.na, 0.03125);
    assert!((p.dof_um - 563.2).abs() < 0.1, "dof {}", p.dof_um);
    assert!(p.step_nm.abs_diff(422_400) <= 100, "step {}", p.step_nm);
}

#[test]
fn step_plan_lens_follows_the_model_over_a_grid() {
    for m in [0.01, 0.1, 0.5, 1.0, 2.0, 10.0, 100.0] {
        for n in [0.7, 2.8, 8.0, 22.0, 64.0] {
            for overlap in [0.0, 0.25, 0.9] {
                let na = 1.0 / (2.0 * n * (1.0 + m));
                let dof_um = 0.55 / (na * na);
                let p = lens_plan(m, n, overlap);
                assert!((p.na - na).abs() < 1e-12, "na at m={m} N={n}");
                assert!(
                    (p.dof_um - dof_um).abs() <= dof_um * 1e-12,
                    "dof at m={m} N={n}"
                );
                let exact_nm = dof_um * (1.0 - overlap) * 1000.0;
                assert!(
                    (p.step_nm as f64 - exact_nm).abs() <= 0.5 + 1e-9,
                    "step {} vs {exact_nm} at m={m} N={n} overlap={overlap}",
                    p.step_nm
                );
            }
        }
    }
}

#[test]
fn step_plan_lens_ignores_na() {
    let base = lens_plan(1.0, 8.0, 0.25);
    let with_na = plan(&resolved(&[
        ("optics", ParamValue::Enum("lens".into())),
        ("aperture", f(8.0)),
        ("na", f(0.9)),
    ]));
    assert_eq!(base, with_na);
}

#[test]
fn step_plan_objective_reference_points() {
    let a = objective_plan(0.25, 0.25);
    assert_eq!(a.optics, Optics::Objective { na: 0.25 });
    assert!((a.dof_um - 8.80).abs() <= 0.01, "dof {}", a.dof_um);
    assert_eq!(a.step_nm, 6600);
    assert!((objective_plan(0.4, 0.25).dof_um - 3.44).abs() <= 0.01);
}

#[test]
fn step_plan_objective_ignores_aperture_and_magnification() {
    let base = objective_plan(0.25, 0.25);
    let other = plan(&resolved(&[
        ("optics", ParamValue::Enum("objective".into())),
        ("na", f(0.25)),
        ("aperture", f(1.4)),
        ("magnification", f(40.0)),
    ]));
    assert_eq!(base, other);
    for na in [0.01, 0.1, 0.4, 0.95] {
        let p = objective_plan(na, 0.0);
        assert!((p.dof_um - 0.55 / (na * na)).abs() <= p.dof_um * 1e-12);
        assert_eq!(p.na, na);
    }
}

#[test]
fn step_plan_frame_count_is_integer_nanometres() {
    // 0.3 / 0.1 is 2.9999999999999996 in f64.
    assert_eq!(frame_count(300_000, 100_000), 4);
    assert_eq!(frame_count(700_000, 100_000), 8);
    assert_eq!(frame_count(50_000, 100_000), 2);
    assert_eq!(frame_count(0, 100_000), 1);
    assert_eq!(frame_count(100_001, 100_000), 3);
}

#[test]
fn step_plan_frame_count_duration_and_disk() {
    let p = plan(&resolved(&[
        ("optics", ParamValue::Enum("lens".into())),
        ("magnification", f(1.0)),
        ("aperture", f(8.0)),
        ("overlap", f(0.25)),
        ("range_mm", f(5.0)),
        ("frame_seconds", f(3.0)),
        ("frame_mb", f(50.0)),
    ]));
    assert_eq!(p.frames, 13);
    assert_eq!(p.duration_s, 39.0);
    assert_eq!(p.disk_mb, 650.0);

    let objective = plan(&resolved(&[
        ("optics", ParamValue::Enum("objective".into())),
        ("na", f(0.25)),
        ("overlap", f(0.25)),
        ("range_mm", f(1.0)),
    ]));
    assert_eq!(objective.frames, 153);

    let one = plan(&resolved(&[("range_mm", f(0.0))]));
    assert_eq!(one.frames, 1);
}

#[test]
fn step_plan_schema_declares_bounded_inputs_with_valid_defaults() {
    let ids: Vec<&str> = PLAN_PARAMS.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "magnification",
            "optics",
            "aperture",
            "na",
            "overlap",
            "range_mm",
            "frame_seconds",
            "frame_mb"
        ]
    );
    for desc in PLAN_PARAMS.iter() {
        assert!(
            !desc.label.is_empty() && !desc.help.is_empty(),
            "{}",
            desc.id
        );
    }
    let float_bounds = |id: &str| match PLAN_PARAMS.iter().find(|d| d.id == id).map(|d| &d.kind) {
        Some(ParamKind::Float { min, max, .. }) => (*min, *max),
        other => panic!("{id} is not a float: {other:?}"),
    };
    assert!(float_bounds("overlap").1 < 1.0);
    assert!(float_bounds("aperture").0 > 0.0);
    assert!(float_bounds("na").0 > 0.0);

    // Defaults resolve to a valid plan.
    let defaults = plan(&resolved(&[]));
    assert!(defaults.step_nm > 0 && defaults.frames >= 1);

    // The smallest step inside the bounds is at least 1 nm.
    let (_, na_max) = float_bounds("na");
    let (_, overlap_max) = float_bounds("overlap");
    let (n_min, _) = float_bounds("aperture");
    let (m_min, _) = float_bounds("magnification");
    assert!(objective_plan(na_max, overlap_max).step_nm >= 1);
    assert!(lens_plan(m_min, n_min, overlap_max).step_nm >= 1);
}

#[test]
fn step_plan_schema_matches_the_published_inputs() {
    // (id, label, min, max, step, logarithmic, default)
    let floats: [(&str, &str, f64, f64, f64, bool, f64); 7] = [
        (
            "magnification",
            "Magnification (x)",
            0.01,
            100.0,
            0.1,
            true,
            1.0,
        ),
        ("aperture", "f-stop", 0.7, 64.0, 0.1, false, 8.0),
        (
            "na",
            "Numerical aperture (NA)",
            0.01,
            0.95,
            0.01,
            false,
            0.25,
        ),
        (
            "overlap",
            "Overlap (fraction of the depth of field)",
            0.0,
            0.9,
            0.05,
            false,
            0.25,
        ),
        ("range_mm", "Depth range (mm)", 0.0, 1000.0, 0.1, false, 5.0),
        (
            "frame_seconds",
            "Time per frame (s)",
            0.0,
            3600.0,
            0.1,
            false,
            1.0,
        ),
        (
            "frame_mb",
            "Size per frame (MB)",
            0.0,
            2000.0,
            1.0,
            false,
            50.0,
        ),
    ];
    for (id, label, min, max, step, logarithmic, default) in floats {
        let desc = PLAN_PARAMS
            .iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("`{id}` is missing"));
        assert_eq!(desc.label, label, "{id} label");
        assert_eq!(
            desc.kind,
            ParamKind::Float {
                min,
                max,
                step,
                logarithmic
            },
            "{id} bounds"
        );
        assert_eq!(desc.default, ParamValue::Float(default), "{id} default");
    }

    let optics = PLAN_PARAMS
        .iter()
        .find(|d| d.id == "optics")
        .expect("optics is declared");
    assert_eq!(optics.label, "Optics");
    assert_eq!(optics.default, ParamValue::Enum("lens".into()));
    let ParamKind::Enum { variants } = &optics.kind else {
        panic!("optics is not an enum: {:?}", optics.kind);
    };
    let variants: Vec<(&str, &str)> = variants
        .iter()
        .map(|v| (v.id.as_str(), v.label.as_str()))
        .collect();
    assert_eq!(
        variants,
        [("lens", "Lens (f-stop)"), ("objective", "Objective (NA)")]
    );
}
