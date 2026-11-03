// SPDX-License-Identifier: GPL-3.0-or-later

//! `step-plan`: the focus step, frame count, duration and disk need of a
//! focus stack, computed by [`focusforge_core::step_plan`].

use std::process::ExitCode;

use focusforge_core::params::{self, ParamDesc, ParamKind, ParamValue};
use focusforge_core::step_plan::{Optics, PLAN_PARAMS, StepPlan, plan};

use crate::set::{SetArgs, parse_settings};

pub(crate) fn run(set: &SetArgs, json: bool) -> ExitCode {
    let explicit = match parse_settings(&PLAN_PARAMS, &set.set) {
        Ok(values) => values,
        Err(violations) => {
            return crate::fail(
                violations
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    };
    let resolved = match params::resolve_in(
        &PLAN_PARAMS,
        &explicit,
        None,
        &focusforge_core::host::snapshot(),
    ) {
        Ok(resolved) => resolved,
        Err(err) => return crate::fail(err),
    };
    let plan = plan(&params::resolved_values(&resolved));
    if json {
        match serde_json::to_string_pretty(&plan) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        print!("{}", render_plan(&plan));
    }
    ExitCode::SUCCESS
}

/// A number with at most two decimals and no trailing zeros.
fn trimmed(value: f64) -> String {
    ((value * 100.0).round() / 100.0).to_string()
}

fn render_plan(plan: &StepPlan) -> String {
    let optics = match &plan.optics {
        Optics::Lens { aperture } => format!("lens, f/{aperture}"),
        Optics::Objective { na } => format!("objective, NA {na}"),
    };
    format!(
        "optics:    {optics}\n\
         NA:        {:.3}\n\
         DOF:       {} µm\n\
         step:      {} µm ({} nm)\n\
         frames:    {}\n\
         duration:  {} s\n\
         disk:      {} MB\n",
        plan.na,
        trimmed(plan.dof_um),
        trimmed(plan.step_nm as f64 / 1000.0),
        plan.step_nm,
        plan.frames,
        trimmed(plan.duration_s),
        trimmed(plan.disk_mb),
    )
}

/// One line per parameter, generated from the schema.
pub(crate) fn parameter_help(schema: &[ParamDesc]) -> String {
    let mut text = String::from("Parameters (--set KEY=VALUE):\n");
    for desc in schema {
        let default = match &desc.default {
            ParamValue::Float(value) => value.to_string(),
            ParamValue::Int(value) => value.to_string(),
            ParamValue::Bool(value) => value.to_string(),
            ParamValue::Enum(value) => value.clone(),
        };
        let range = match &desc.kind {
            ParamKind::Float { min, max, .. } => format!("{min} to {max}"),
            ParamKind::Int { min, max } => format!("{min} to {max}"),
            ParamKind::Bool => "true or false".to_string(),
            ParamKind::Enum { variants } => format!(
                "one of {}",
                variants
                    .iter()
                    .map(|variant| variant.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        text.push_str(&format!(
            "  {:<14} {} (default {default}, {range})\n",
            desc.id, desc.label
        ));
    }
    text
}
