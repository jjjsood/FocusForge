// SPDX-License-Identifier: GPL-3.0-or-later

//! `step-plan`: the focus step, frame count, duration and disk need of a
//! focus stack, computed by [`focusforge_core::step_plan`].

use std::process::ExitCode;

use focusforge_core::step_plan::{Optics, PLAN_PARAMS, StepPlan, plan};

use crate::set::{SetArgs, resolve_settings};

pub(crate) fn run(set: &SetArgs, json: bool) -> ExitCode {
    let values = match resolve_settings(&PLAN_PARAMS, set) {
        Ok(values) => values,
        Err(message) => return crate::fail(message),
    };
    let plan = plan(&values);
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
