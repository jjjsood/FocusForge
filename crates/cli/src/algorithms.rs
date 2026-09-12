// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli algorithms`: lists every registered algorithm, or prints
//! one named algorithm's full parameter schema and validates `--set`
//! against it.
//!
//! Resolution here is **explicit → `host_default` → default**: with no image
//! loaded there is no `StackContext`, so a stack `auto` hook is still never
//! evaluated -- but a host default needs no loaded image and *is* evaluated,
//! through `focusforge_core::params::resolve_in(..., None, &host_facts)`. An
//! `auto` hook is reported as present but unevaluated instead of being run.

use std::process::ExitCode;

use focusforge_core::params::{ParamDesc, ParamKind, ParamValue, ParamValues, Provenance};
use focusforge_core::registry::{self, AlgorithmRef};
use serde::Serialize;

use crate::set::{SetArgs, parse_settings};

/// The frozen `algorithms --json` schema: one envelope for the listing and
/// the single-id form (a one-element `algorithms` array). Additive only: a
/// field added to `ParamDesc` appears in `params` without an edit here,
/// because the descriptor is `#[serde(flatten)]`ed.
#[derive(Serialize)]
struct AlgorithmsReport<'a> {
    algorithms: Vec<AlgorithmReport<'a>>,
}

#[derive(Serialize)]
struct AlgorithmReport<'a> {
    id: String,
    display_name: String,
    family: String,
    params: Vec<ParamReport<'a>>,
}

#[derive(Serialize)]
struct ParamReport<'a> {
    #[serde(flatten)]
    desc: &'a ParamDesc,
    has_auto: bool,
    value: ParamValue,
    provenance: Provenance,
}

/// Runs the subcommand. `id` selects one algorithm; omitting it lists every
/// registered algorithm and makes `--set` meaningless, which is why a
/// non-empty `set` with no `id` is rejected before anything else runs.
///
/// This is the explicit runtime check the decision allows over a clap
/// `requires = "id"` rule: clap does not support `#[arg(...)]` attributes on
/// a `#[command(flatten)]` field (clap-rs/clap#3269), and a `requires` rule
/// written *inside* `SetArgs` would make a future `stack` subcommand —
/// which has no `id` argument — abort at startup on clap's own assertion.
pub fn run(id: Option<&str>, json: bool, set: &SetArgs) -> ExitCode {
    if id.is_none() && !set.set.is_empty() {
        return crate::fail("--set requires an algorithm id");
    }

    let entries: Vec<AlgorithmRef> = match id {
        Some(id) => match registry::find(id) {
            Ok(entry) => vec![entry],
            Err(err) => return crate::fail(err),
        },
        None => registry::algorithms(),
    };

    let explicit = match id {
        Some(_) => match parse_settings(entries[0].params(), &set.set) {
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
        },
        None => Default::default(),
    };

    let report = build_report(&entries, &explicit);

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
        }
    } else if id.is_some() {
        println!("{}", render_schema(&report.algorithms[0]));
    } else {
        println!("{}", render_listing(&report.algorithms));
    }

    ExitCode::SUCCESS
}

/// Builds the report for `entries`, resolving each parameter explicit →
/// default only: a value present in `explicit` (keyed by parameter id) wins
/// with [`Provenance::Explicit`], otherwise the descriptor's own default
/// with [`Provenance::Default`]. `explicit` only ever carries settings for
/// a single selected algorithm (`run` rejects `--set` without an id), so it
/// cannot cross-apply to a different algorithm's parameter of the same id.
fn build_report(entries: &[AlgorithmRef], explicit: &ParamValues) -> AlgorithmsReport<'static> {
    let algorithms = entries
        .iter()
        .map(|entry| AlgorithmReport {
            id: entry.id().to_string(),
            display_name: entry.display_name().to_string(),
            family: entry.family().to_string(),
            params: entry
                .params()
                .iter()
                .map(|desc| {
                    let (value, provenance) = match explicit.get(&desc.id) {
                        Some(v) => (v.clone(), Provenance::Explicit),
                        None => (desc.default.clone(), Provenance::Default),
                    };
                    ParamReport {
                        desc,
                        has_auto: desc.auto.is_some(),
                        value,
                        provenance,
                    }
                })
                .collect(),
        })
        .collect();
    AlgorithmsReport { algorithms }
}

/// One line per registered algorithm (id, display name, family), or the
/// literal line `no algorithms registered` when nothing is registered.
/// Takes `&[AlgorithmReport]` rather than printing directly so the empty
/// case is unit-testable without capturing stdout. Which of the two a given
/// process actually shows depends on whether `target/debug/focusforge-cli`
/// was last built by `cargo build --workspace --all-targets` (pulls in
/// `focusforge-core/test-algorithms`) or a plain `cargo build` — not on
/// this function, which is correct either way.
fn render_listing(algorithms: &[AlgorithmReport<'_>]) -> String {
    if algorithms.is_empty() {
        return "no algorithms registered".to_string();
    }
    algorithms
        .iter()
        .map(|a| format!("{} - {} ({})", a.id, a.display_name, a.family))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One block per parameter: kind (range or variants), default, the resolved
/// value/provenance pair, and whether an `auto` hook exists. `has_auto` is
/// reported as `auto: present, not evaluated` or `auto: none` — never as a
/// resolved value, since no `auto` hook is ever invoked here.
fn render_schema(algo: &AlgorithmReport<'_>) -> String {
    let mut lines = vec![format!(
        "{} - {} ({})",
        algo.id, algo.display_name, algo.family
    )];
    for p in &algo.params {
        lines.push(format!(
            "  {}: {} default {} value {} ({:?})",
            p.desc.id,
            describe_kind(&p.desc.kind),
            serde_json::to_string(&p.desc.default).unwrap_or_else(|_| "?".to_string()),
            serde_json::to_string(&p.value).unwrap_or_else(|_| "?".to_string()),
            p.provenance,
        ));
        lines.push(format!(
            "    auto: {}",
            if p.has_auto {
                "present, not evaluated"
            } else {
                "none"
            }
        ));
    }
    lines.join("\n")
}

/// Human rendering of a parameter's kind: its range or variant list. Every
/// number, variant id and label comes out of `kind`; `--json` never calls
/// this, serde renders `ParamKind` itself.
fn describe_kind(kind: &ParamKind) -> String {
    match kind {
        ParamKind::Float {
            min,
            max,
            step,
            logarithmic,
        } => {
            format!(
                "float [{min}, {max}] step {step}{}",
                if *logarithmic { " (log)" } else { "" }
            )
        }
        ParamKind::Int { min, max } => format!("int [{min}, {max}]"),
        ParamKind::Bool => "bool".to_string(),
        ParamKind::Enum { variants } => format!(
            "enum {{{}}}",
            variants
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_listing_renders_the_placeholder_line() {
        assert_eq!(render_listing(&[]), "no algorithms registered");
    }

    #[test]
    fn empty_listing_serializes_to_an_empty_array() {
        let report = AlgorithmsReport { algorithms: vec![] };
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(json, r#"{"algorithms":[]}"#);
    }
}
