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

use std::collections::BTreeMap;
use std::process::ExitCode;

use focusforge_core::params::{ParamDesc, ParamKind, ParamValue, ParamValues, Provenance};
use focusforge_core::project::{self, PROJECT_PARAMS};
use focusforge_core::registry::{self, AlgorithmRef};
use serde::Serialize;

use crate::set::{SetArgs, parse_settings};

/// The `algorithms --json` report has one envelope for the listing and the
/// single-id form (a one-element `algorithms` array). `project_params` lists
/// the run-level settings `stack --set` accepts beside an algorithm's own,
/// with the default that holds unless a fusion method declares its own
/// (`project_defaults` on that method's entry).
#[derive(Serialize)]
struct AlgorithmsReport<'a> {
    algorithms: Vec<AlgorithmReport<'a>>,
    project_params: Vec<ParamReport<'a>>,
}

#[derive(Serialize)]
struct AlgorithmReport<'a> {
    id: String,
    display_name: String,
    family: String,
    params: Vec<ParamReport<'a>>,
    /// The default of every project setting in a run fused by this method;
    /// absent for an alignment algorithm, which no run is fused by.
    #[serde(skip_serializing_if = "Option::is_none")]
    project_defaults: Option<BTreeMap<String, ParamValue>>,
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
        println!(
            "{}",
            render_listing(&report.algorithms, &report.project_params)
        );
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
                project_defaults: match entry {
                    AlgorithmRef::Stack(method) => Some(
                        project::params_for(*method)
                            .into_iter()
                            .map(|desc| (desc.id, desc.default))
                            .collect(),
                    ),
                    AlgorithmRef::Align(_) => None,
                },
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
    let project_params = PROJECT_PARAMS
        .iter()
        .map(|desc| ParamReport {
            desc,
            has_auto: false,
            has_host_default: false,
            value: desc.default.clone(),
            provenance: Provenance::Default,
        })
        .collect();
    Ok(AlgorithmsReport {
        algorithms,
        project_params,
    })
}

/// One line per registered algorithm (id, display name, family), then the
/// project settings `stack --set` also accepts, or the literal line `no
/// algorithms registered` when nothing is registered. Returns a `String` so
/// the empty case is unit-testable without capturing stdout.
fn render_listing(algorithms: &[AlgorithmReport<'_>], project: &[ParamReport<'_>]) -> String {
    if algorithms.is_empty() {
        return "no algorithms registered".to_string();
    }
    let mut lines: Vec<String> = algorithms
        .iter()
        .map(|a| format!("{} - {} ({})", a.id, a.display_name, a.family))
        .collect();
    if !project.is_empty() {
        lines.push(String::new());
        lines.push("project settings (stack --set):".to_string());
        lines.extend(
            project
                .iter()
                .map(|p| format!("  {} - {}", p.desc.id, p.desc.label)),
        );
    }
    lines.join("\n")
}

/// One block per parameter: kind (range or variants), default, the resolved
/// value/provenance pair, its help sentence on its own line, and whether an
/// `auto` hook exists. `has_auto` is reported as `auto: present, not
/// evaluated` or `auto: none` — never as a resolved value, since no `auto`
/// hook is ever invoked here. A parameter with an `auto` hook reads `default
/// from the loaded frames` and shows a value only once set explicitly.
fn render_schema(algo: &AlgorithmReport<'_>) -> String {
    let mut lines = vec![format!(
        "{} - {} ({})",
        algo.id, algo.display_name, algo.family
    )];
    for p in &algo.params {
        // An `auto` hook decides from the loaded frames, which this command
        // never loads, so the stored default is not the value in use.
        let default = if p.has_auto {
            "from the loaded frames".to_string()
        } else {
            serde_json::to_string(&p.desc.default).unwrap_or_else(|_| "?".to_string())
        };
        let mut line = format!(
            "  {}: {} default {default}",
            p.desc.id,
            describe_kind(&p.desc.kind),
        );
        if !(p.has_auto && p.provenance == Provenance::Default) {
            line.push_str(&format!(
                " value {} ({:?})",
                serde_json::to_string(&p.value).unwrap_or_else(|_| "?".to_string()),
                p.provenance,
            ));
        }
        lines.push(line);
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
    if let Some(defaults) = &algo.project_defaults {
        lines.push("  project settings default:".to_string());
        lines.extend(defaults.iter().map(|(id, value)| {
            format!(
                "    {id}: {}",
                serde_json::to_string(value).unwrap_or_else(|_| "?".to_string())
            )
        }));
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
        assert_eq!(render_listing(&[], &[]), "no algorithms registered");
    }

    #[test]
    fn empty_listing_serializes_to_an_empty_array() {
        let report = AlgorithmsReport {
            algorithms: vec![],
            project_params: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(json, r#"{"algorithms":[],"project_params":[]}"#);
    }
}
