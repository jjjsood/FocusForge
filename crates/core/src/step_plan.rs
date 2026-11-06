// SPDX-License-Identifier: GPL-3.0-or-later
//! Step calculator: focus step, frame count, duration and disk need from the
//! optics and a depth range. Depth of field is `DOF = 0.55 µm / NA²`; a lens's
//! NA is `1 / (2 · N · (1 + m))` for marked f-stop `N` and magnification `m`.
//! The focus step is `DOF · (1 − overlap)`, rounded to whole nanometres.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::params::{EnumVariant, ParamDesc, ParamKind, ParamValue, ParamValues};

/// Wavelength of green light, in micrometres.
const WAVELENGTH_UM: f64 = 0.55;

fn float(
    id: &str,
    label: &str,
    help: &str,
    (min, max, step): (f64, f64, f64),
    logarithmic: bool,
    default: f64,
) -> ParamDesc {
    ParamDesc {
        id: id.to_string(),
        label: label.to_string(),
        help: help.to_string(),
        kind: ParamKind::Float {
            min,
            max,
            step,
            logarithmic,
        },
        default: ParamValue::Float(default),
        advanced: false,
        auto: None,
        host_default: None,
        stack_max: None,
    }
}

/// The calculator's inputs. Units are part of each label.
pub static PLAN_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        float(
            "magnification",
            "Magnification (x)",
            "Reproduction ratio of the lens; 1 is life size.",
            (0.01, 100.0, 0.1),
            true,
            1.0,
        ),
        ParamDesc {
            id: "optics".to_string(),
            label: "Optics".to_string(),
            help: "A lens is described by its marked f-stop, an objective by its numerical \
                   aperture."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "lens".to_string(),
                        label: "Lens (f-stop)".to_string(),
                    },
                    EnumVariant {
                        id: "objective".to_string(),
                        label: "Objective (NA)".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("lens".to_string()),
            advanced: false,
            auto: None,
            host_default: None,
            stack_max: None,
        },
        float(
            "aperture",
            "f-stop",
            "The f-stop set on the lens. Used with the lens optics only.",
            (0.7, 64.0, 0.1),
            false,
            8.0,
        ),
        float(
            "na",
            "Numerical aperture (NA)",
            "Numerical aperture of a dry objective. Used with the objective optics only.",
            (0.01, 0.95, 0.01),
            false,
            0.25,
        ),
        float(
            "overlap",
            "Overlap (fraction of the depth of field)",
            "Share of one frame's depth of field that the next frame repeats; 0 places \
             frames edge to edge.",
            (0.0, 0.9, 0.05),
            false,
            0.25,
        ),
        float(
            "range_mm",
            "Depth range (mm)",
            "Distance from the nearest to the farthest point to keep sharp; 0 plans one frame.",
            (0.0, 1000.0, 0.1),
            false,
            5.0,
        ),
        float(
            "frame_seconds",
            "Time per frame (s)",
            "Seconds from one frame to the next, including move, settle and exposure.",
            (0.0, 3600.0, 0.1),
            false,
            1.0,
        ),
        float(
            "frame_mb",
            "Size per frame (MB)",
            "Size of one captured frame on disk.",
            (0.0, 2000.0, 1.0),
            false,
            50.0,
        ),
    ]
});

/// The optics input a plan was computed from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Optics {
    Lens { aperture: f64 },
    Objective { na: f64 },
}

/// A focus-stack plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepPlan {
    pub optics: Optics,
    /// Object-side numerical aperture the depth of field was computed from.
    pub na: f64,
    pub dof_um: f64,
    /// Focus step in nanometres, the unit of a linear axis.
    pub step_nm: u64,
    pub frames: u64,
    pub duration_s: f64,
    pub disk_mb: f64,
}

/// Frames needed to cover `range_nm` in steps of `step_nm`, both ends of the
/// range included. `step_nm` must be at least 1.
pub fn frame_count(range_nm: u64, step_nm: u64) -> u64 {
    range_nm.div_ceil(step_nm) + 1
}

/// Computes the plan for values resolved against [`PLAN_PARAMS`].
pub fn plan(values: &ParamValues) -> StepPlan {
    let number = |id: &str| {
        values
            .get(id)
            .and_then(ParamValue::as_f64)
            .expect("resolved against PLAN_PARAMS")
    };
    let (optics, na) = match values
        .get("optics")
        .and_then(ParamValue::as_enum)
        .expect("resolved against PLAN_PARAMS")
    {
        "objective" => {
            let na = number("na");
            (Optics::Objective { na }, na)
        }
        _ => {
            let aperture = number("aperture");
            (
                Optics::Lens { aperture },
                1.0 / (2.0 * aperture * (1.0 + number("magnification"))),
            )
        }
    };
    let dof_um = WAVELENGTH_UM / (na * na);
    let step_nm = (dof_um * (1.0 - number("overlap")) * 1000.0).round() as u64;
    let range_nm = (number("range_mm") * 1_000_000.0).round() as u64;
    let frames = frame_count(range_nm, step_nm);
    StepPlan {
        optics,
        na,
        dof_um,
        step_nm,
        frames,
        duration_s: frames as f64 * number("frame_seconds"),
        disk_mb: frames as f64 * number("frame_mb"),
    }
}
