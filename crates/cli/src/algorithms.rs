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
    has_host_default: bool,
    value: ParamValue,
    provenance: Provenance,
}

/// Runs the subcommand. `id` selects one algorithm; omitting it lists every
/// registered algorithm and makes `--set` meaningless, which is why a
/// non-empty `set` with no `id` is rejected before anything else runs.
///
/// Checked at runtime because clap cannot attach `requires = "id"` to a
/// `#[command(flatten)]` field (clap-rs/clap#3269).
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

    let report = match build_report(&entries, &explicit) {
        Ok(report) => report,
        Err(err) => return crate::fail(err),
    };

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

/// Builds the report for `entries`, resolving each parameter through
/// [`focusforge_core::params::resolve_in`] with no `StackContext`: explicit
/// wins, a stack `auto` hook is skipped, a host-derived default is
/// evaluated, otherwise the descriptor's default applies. `explicit` only
/// ever holds settings for the one selected algorithm (`run` rejects `--set`
/// without an id).
fn build_report(
    entries: &[AlgorithmRef],
    explicit: &ParamValues,
) -> Result<AlgorithmsReport<'static>, focusforge_core::params::ParamError> {
    let facts = focusforge_core::host::snapshot();
    let algorithms = entries
        .iter()
        .map(|entry| {
            let resolved =
                focusforge_core::params::resolve_in(entry.params(), explicit, None, &facts)?;
            Ok(AlgorithmReport {
                id: entry.id().to_string(),
                display_name: entry.display_name().to_string(),
                family: entry.family().to_string(),
                params: entry
                    .params()
                    .iter()
                    .map(|desc| {
                        let r = &resolved[&desc.id];
                        ParamReport {
                            desc,
                            has_auto: desc.auto.is_some(),
                            has_host_default: desc.host_default.is_some(),
                            value: r.value.clone(),
                            provenance: r.provenance,
                        }
                    })
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AlgorithmsReport { algorithms })
}

/// One line per registered algorithm (id, display name, family), or the
/// literal line `no algorithms registered` when nothing is registered.
/// Returns a `String` so the empty case is unit-testable without capturing
/// stdout.
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
/// value/provenance pair, its help sentence on its own line, and whether an
/// `auto` hook exists. `has_auto` is reported as `auto: present, not
/// evaluated` or `auto: none` — never as a resolved value, since no `auto`
/// hook is ever invoked here.
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
        lines.push(format!("    {}", p.desc.help));
        lines.push(format!(
            "    auto: {}",
            if p.has_auto {
                "present, not evaluated"
            } else {
                "none"
            }
        ));
        if p.has_host_default {
            lines.push("    host default: present, evaluated".to_string());
        }
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
