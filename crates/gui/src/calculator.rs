// SPDX-License-Identifier: GPL-3.0-or-later

//! The step calculator's state: the entered values, the plan computed from
//! the last valid ones, and the views the overlay shows.

use std::collections::BTreeMap;

use focusforge_core::host;
use focusforge_core::params::{
    self, ParamDesc, ParamKind, ParamValue, ParamValues, ParamViolation,
};
use focusforge_core::step_plan::{Optics, PLAN_PARAMS, StepPlan, plan};

use crate::settings_panel::{field_row, violation_text};

/// Focus planes the illustration draws, however many frames the plan needs.
const MAX_PLANES: u64 = 30;

/// Entered fields in layout order, per optics; `None` is a slot the optics
/// does not use.
const LENS_SLOTS: [Option<&str>; 5] = [
    Some("frame_seconds"),
    Some("magnification"),
    Some("aperture"),
    Some("overlap"),
    Some("range_mm"),
];
const OBJECTIVE_SLOTS: [Option<&str>; 5] = [
    Some("frame_seconds"),
    None,
    Some("na"),
    Some("overlap"),
    Some("range_mm"),
];

/// One entered field as the overlay shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldView {
    pub label: String,
    /// The text as typed, or the formatted value while none was typed.
    pub text: String,
    /// The violation line under the field; empty while the entry is valid.
    pub error: String,
}

/// Everything the overlay shows for the current entries.
#[derive(Debug, Clone, PartialEq)]
pub struct CalculatorView {
    pub optics_labels: Vec<String>,
    pub optics_selected: String,
    pub objective: bool,
    /// The five field slots; `None` hides a slot.
    pub fields: Vec<Option<FieldView>>,
    /// SUMMARY ENTERED then CALCULATED, as (caption, value).
    pub entered: Vec<(String, String)>,
    pub calculated: Vec<(String, String)>,
    pub step: String,
    pub depth_of_field: String,
    pub frames: String,
    pub duration: String,
    pub planes: usize,
    pub overlap: f32,
}

pub struct Calculator {
    explicit: ParamValues,
    typed: BTreeMap<String, String>,
    errors: BTreeMap<String, ParamViolation>,
    values: ParamValues,
    plan: StepPlan,
}

impl Default for Calculator {
    fn default() -> Self {
        Self::new()
    }
}

fn desc(id: &str) -> &'static ParamDesc {
    PLAN_PARAMS
        .iter()
        .find(|d| d.id == id)
        .expect("the calculator reads ids declared in PLAN_PARAMS")
}

fn resolve(explicit: &ParamValues) -> ParamValues {
    let resolved = params::resolve_in(&PLAN_PARAMS, explicit, None, &host::snapshot())
        .expect("entries parsed against PLAN_PARAMS resolve");
    params::resolved_values(&resolved)
}

/// A number with at most two decimals and no trailing zeros, as
/// `focusforge-cli step-plan` prints it.
fn trimmed(value: f64) -> String {
    ((value * 100.0).round() / 100.0).to_string()
}

/// The unit a summary value carries after its number.
fn unit(id: &str) -> &'static str {
    match id {
        "magnification" => "\u{00D7}",
        "range_mm" => " mm",
        "frame_seconds" => " s",
        _ => "",
    }
}

impl Calculator {
    pub fn new() -> Self {
        let explicit = ParamValues::new();
        let values = resolve(&explicit);
        let plan = plan(&values);
        Self {
            explicit,
            typed: BTreeMap::new(),
            errors: BTreeMap::new(),
            values,
            plan,
        }
    }

    /// The plan over the last valid entries.
    pub fn plan(&self) -> &StepPlan {
        &self.plan
    }

    fn objective(&self) -> bool {
        self.values.get("optics").and_then(ParamValue::as_enum) == Some("objective")
    }

    fn slots(&self) -> &'static [Option<&'static str>; 5] {
        if self.objective() {
            &OBJECTIVE_SLOTS
        } else {
            &LENS_SLOTS
        }
    }

    fn replan(&mut self) {
        self.values = resolve(&self.explicit);
        self.plan = plan(&self.values);
    }

    /// Selects the optics whose variant label is `label`; an unknown label
    /// changes nothing.
    pub fn set_optics(&mut self, label: &str) {
        let optics = desc("optics");
        let ParamKind::Enum { variants } = &optics.kind else {
            unreachable!("optics is an enum parameter");
        };
        let Some(variant) = variants.iter().find(|v| v.label == label) else {
            return;
        };
        let value = ParamValue::parse(optics, &variant.id).expect("a declared variant parses");
        self.explicit.insert("optics".to_string(), value);
        self.replan();
    }

    /// Takes `text` typed into field slot `slot`: a valid entry re-plans, an
    /// invalid one records its violation and keeps the last plan.
    pub fn edit(&mut self, slot: usize, text: &str) {
        let Some(Some(id)) = self.slots().get(slot).copied() else {
            return;
        };
        // A single comma is a decimal point; the echoed text carries the point.
        let text = if text.matches(',').count() == 1 {
            text.replace(',', ".")
        } else {
            text.to_string()
        };
        self.typed.insert(id.to_string(), text.clone());
        match ParamValue::parse(desc(id), &text) {
            Ok(value) => {
                self.errors.remove(id);
                self.explicit.insert(id.to_string(), value);
                self.replan();
            }
            Err(err) => {
                let violation = err
                    .into_violations()
                    .into_iter()
                    .next()
                    .expect("ParamValue::parse returns exactly one violation");
                self.errors.insert(id.to_string(), violation);
            }
        }
    }

    pub fn view(&self) -> CalculatorView {
        let optics = desc("optics");
        let ParamKind::Enum { variants } = &optics.kind else {
            unreachable!("optics is an enum parameter");
        };
        let selected = self
            .values
            .get("optics")
            .and_then(ParamValue::as_enum)
            .unwrap_or_default();
        let optics_selected = variants
            .iter()
            .find(|v| v.id == selected)
            .map(|v| v.label.clone())
            .unwrap_or_default();

        let row = |id: &str| field_row("calculator", desc(id), &self.values[id], false);
        // Floats read as the plain number, not at the slider's step decimals.
        let plain = |id: &str| match &self.values[id] {
            ParamValue::Float(f) => f.to_string(),
            _ => row(id).value_text.to_string(),
        };
        let caption = |label: &str| {
            label
                .split_once(" (")
                .map_or(label, |(head, _)| head)
                .to_string()
        };
        let fields = self
            .slots()
            .iter()
            .map(|slot| {
                slot.map(|id| FieldView {
                    label: desc(id).label.clone(),
                    text: self.typed.get(id).cloned().unwrap_or_else(|| plain(id)),
                    error: self.errors.get(id).map(violation_text).unwrap_or_default(),
                })
            })
            .collect();

        let optics_value = if self.objective() { "NA" } else { "f-stop" };
        let mut entered = vec![(caption(&optics.label), optics_value.to_string())];
        let order = [
            "magnification",
            "aperture",
            "na",
            "overlap",
            "range_mm",
            "frame_seconds",
        ];
        for id in order {
            if self.slots().contains(&Some(id)) {
                let value = format!("{}{}", plain(id), unit(id));
                entered.push((caption(&desc(id).label), value));
            }
        }

        let plan = &self.plan;
        let step = format!("{} \u{00B5}m", trimmed(plan.step_nm as f64 / 1000.0));
        let depth_of_field = format!("{} \u{00B5}m", trimmed(plan.dof_um));
        let frames = plan.frames.to_string();
        let duration = format!("{} s", trimmed(plan.duration_s));
        let mut calculated = Vec::new();
        if matches!(plan.optics, Optics::Lens { .. }) {
            calculated.push((caption(&desc("na").label), format!("{:.3}", plan.na)));
        }
        calculated.push(("Depth of field".to_string(), depth_of_field.clone()));
        calculated.push(("Step".to_string(), step.clone()));
        calculated.push(("Frames".to_string(), frames.clone()));
        calculated.push(("Duration".to_string(), duration.clone()));

        CalculatorView {
            optics_labels: variants.iter().map(|v| v.label.clone()).collect(),
            optics_selected,
            objective: self.objective(),
            fields,
            entered,
            calculated,
            step,
            depth_of_field,
            frames,
            duration,
            planes: plan.frames.min(MAX_PLANES) as usize,
            overlap: self.values["overlap"].as_f64().unwrap_or_default() as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_names_the_aperture_by_the_selected_optics() {
        let mut calculator = Calculator::new();
        let view = calculator.view();
        assert_eq!(
            view.entered[0],
            ("Optics".to_string(), "f-stop".to_string())
        );
        assert_eq!(view.entered[2].0, "f-stop");

        calculator.set_optics("Objective (NA)");
        let view = calculator.view();
        assert_eq!(view.entered[0], ("Optics".to_string(), "NA".to_string()));
        assert_eq!(view.entered[1].0, "Numerical aperture");
    }
}
