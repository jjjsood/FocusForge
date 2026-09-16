// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli stack`: groups the input frames, resolves a registered
//! `StackAlgorithm` and hands each group to `focusforge_core::pipeline::run`,
//! which seals one project per group. Every refusal fires before a frame is
//! opened where the check allows it (algorithm id, `--set` values), and
//! before a tile is read otherwise.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use focusforge_core::algorithm::{AlignStatus, StackAlgorithm};
use focusforge_core::align::{
    AlignDriverOptions, FrameAlignment, ReferencePolicy, StackAlignment, Transform, WarpingReader,
    align_stack_with, auto_crop,
};
use focusforge_core::fuse::{
    BackendFusionConsumer, FusionConsumer, FusionGeometry,
    description::format_composite_description, laplacian::Laplacian,
};
use focusforge_core::io::{Group, Proposal, Rect, TiffSpec, TiffStack, propose};
use focusforge_core::job::{
    AlignIdentity, FrameFingerprint, JOB_RECORD_FORMAT, Job, JobOutcome, JobRecord,
    RecordingConsumer, Resumable, ResumeStore, StackFingerprint, StageCache, StageKey,
    read_alignment,
};
use focusforge_core::params::{self, ParamValue, ParamValues, Provenance, StackContext};
use focusforge_core::project::{self, Metrics, PROJECT_EXTENSION};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileConsumer, TileScheduler, TileSchedulerOptions};
use focusforge_core::timing::{Stage, StageTimer};
use serde::Serialize;

use crate::align::{AlignOptions, AlignmentEntry, resolve_align_algo};
use crate::groups::{self, GroupArgs};
use crate::job::{self, JobOptions};
use crate::preset::{self, PresetOptions};
use crate::progress::{ProgressMode, ProgressRenderer};
use crate::set::{SetArgs, merge_preset, parse_settings};
use crate::signals;
use crate::status::StatusSink;

/// The `stack --json` document; fields are only ever added, never removed
/// or renamed.
#[derive(Serialize)]
pub struct StackReport {
    pub input: String,
    pub frame_count: usize,
    pub frames: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub algorithm: String,
    pub display_name: String,
    pub params: Vec<ParamReport>,
    pub tile_size: u32,
    pub levels: u32,
    pub margin: u32,
    pub tiles: usize,
    pub frame_tiles_read: usize,
    pub in_flight_limit: usize,
    pub peak_in_flight: usize,
    /// Final paths only -- never the `.partial` names a run writes to first.
    pub outputs: Vec<String>,
    pub wall_seconds: f64,
    /// The output extent -- equal to `width`/`height` unless a crop was
    /// applied; `width`/`height` are the **source** frame extent.
    pub output_width: u32,
    pub output_height: u32,
    pub crop: Option<Rect>,
    pub description: Option<String>,
    pub align: Option<StackAlignReport>,
    /// `"cpu"`, or the selected adapter's [`focusforge_core::gpu::BackendInfo`]
    /// display -- additive, so which path ran is machine-readable and not
    /// only a stderr line.
    pub backend: String,
    /// `true` only when the alignment came out of the stage cache;
    /// `false` for a computed one, a `--resume` ledger-peeked one, or a
    /// `--no-align` run. Additive field.
    pub align_cached: bool,
    /// Wall-clock stage breakdown -- `setup`/`align`/`fuse`/`finalize` as
    /// seconds, `warp_write` always `null` (`stack` never enters it). Every
    /// figure is wall clock, never CPU time; the four non-null stages sum
    /// exactly to `wall_seconds`, because the spans are contiguous and
    /// disjoint by construction -- see
    /// [`focusforge_core::timing::StageTimings`]'s own rustdoc for the full
    /// contract, including what a parallel stage's number means.
    pub stages: focusforge_core::timing::StageTimings,
    /// How this run's `align` stage was satisfied: `"computed"` (a real
    /// `align_stack` solve), `"cached"` (the stage cache),
    /// `"resumed"` (a `--resume` ledger peek), or `"skipped"`
    /// (`--no-align`, so `stages.align` is `null`). `align_cached` alone
    /// cannot tell a resumed run from a genuinely fast alignment -- both
    /// spend a few milliseconds inside the stage and neither is a compute
    /// -- so this field, not that one, is what a later run compares before
    /// reading a stage number as a real cost. Additive field.
    pub align_source: String,
    /// The anchor arm's worker bound and the peak it actually reached --
    /// `None` unless `align_source == "computed"`. A `"cached"`, `"resumed"`
    /// or `"skipped"` run never calls `align_stack_with`, so it has no
    /// worker count of its own; reporting one from a previous run's cache
    /// entry would be a fabricated number, exactly the confusion
    /// `align_source` exists to prevent. Additive fields.
    pub align_workers: Option<usize>,
    pub peak_align_workers: Option<usize>,
}

/// `stack --json`'s `align` field: `null` when `--no-align` was given.
#[derive(Serialize)]
pub struct StackAlignReport {
    pub algorithm: String,
    pub display_name: String,
    pub params: Vec<ParamReport>,
    pub reference: usize,
    pub reference_policy: ReferencePolicy,
    pub failure_policy: String,
    pub failure_count: usize,
    pub alignments: Vec<AlignmentEntry>,
}

#[derive(Serialize)]
pub struct ParamReport {
    pub id: String,
    pub value: ParamValue,
    pub provenance: Provenance,
}

/// `stack --dry-run --json`'s document: **not** a [`StackReport`].
///
/// `stack --json`'s standing contract is that a document on stdout means a
/// completed run, so a plan gets its own shape rather than a report with
/// hollow fields: `dry_run: true` leads, `would_write` replaces `outputs`,
/// and `crop`/`output_width`/`output_height`/`wall_seconds` are absent
/// entirely -- an existing consumer of `stack --json` therefore cannot
/// mistake a plan for a run, and a new one cannot read a crop that was never
/// computed.
#[derive(Serialize)]
pub struct DryRunPlan {
    /// Always `true`. A constant field, because the marker is what a
    /// consumer branches on.
    pub dry_run: bool,
    pub input: String,
    pub frame_count: usize,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub algorithm: String,
    pub display_name: String,
    pub params: Vec<ParamReport>,
    /// `null` with `--no-align`; otherwise the alignment algorithm and its
    /// resolved parameters -- resolved, never run.
    pub align: Option<DryRunAlignPlan>,
    pub tile_size: u32,
    pub levels: u32,
    pub margin: u32,
    pub tiles: usize,
    pub in_flight_limit: usize,
    pub backend: String,
    /// The paths this invocation *would* write. None of them exists.
    pub would_write: Vec<String>,
    /// What this plan deliberately does not promise, named rather than
    /// omitted: a plan that silently left `crop` out would read as "no
    /// crop".
    pub not_computed: Vec<String>,
}

/// [`DryRunPlan`]'s `align` field: the alignment half resolved, not run.
#[derive(Serialize)]
pub struct DryRunAlignPlan {
    pub algorithm: String,
    pub display_name: String,
    pub params: Vec<ParamReport>,
}

/// Runs the subcommand.
///
/// `algo_id: None` is refused here, by name, rather than by clap: `--algo`
/// is declared `Option<String>` in `crate::Command::Stack` specifically so
/// its absence reaches this function as `None` instead of aborting the
/// process on clap's own usage error (exit 2, no knowledge of the registry).
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    output: Option<&Path>,
    algo_id: Option<&str>,
    zmap: Option<&Path>,
    json: bool,
    set: &SetArgs,
    align_opts: &AlignOptions<'_>,
    job_opts: &JobOptions,
    preset_opts: &PresetOptions<'_>,
    group_args: &GroupArgs,
    argv: &[String],
    no_gpu: bool,
) -> ExitCode {
    // Every refusal in this outer driver is group-independent, so it fires
    // exactly once, before the first group runs, and writes exactly one
    // status record -- a `StatusSink` built for that one refusal, which its
    // own `Drop` assertion is then satisfied by.
    let refuse = |message: String| -> ExitCode {
        StatusSink::new(preset_opts.status, dir.display().to_string(), "stack").fail(message)
    };

    // `signals::install` registers its SIGINT/SIGUSR1 handlers **per call**
    // and its two-stage Ctrl-C mechanism depends on that registration
    // order, so it is installed once here and passed down: a per-group call
    // would stack N handler pairs and break the second-Ctrl-C kill.
    let signals = match signals::install(true) {
        Ok(signals) => signals,
        Err(msg) => return refuse(msg),
    };

    // Step 1a: the preset, ahead of everything else -- a malformed file is
    // refused before a frame is opened, and its `algo` key is what step 2
    // falls back to.
    let file = match preset::load(preset_opts.preset) {
        Ok(preset) => preset,
        Err(msg) => return refuse(msg),
    };
    if preset_opts.dry_run && job_opts.resume {
        return refuse("--dry-run and --resume are incompatible: a dry run writes nothing, so there is nothing to resume into".to_string());
    }

    // Step 2: resolve the algorithm first, before anything else -- an
    // unknown or missing id is refused before the input directory (which
    // may not even exist) is ever touched. `--algo` beats the preset's own
    // `algo` key: one rule, flag beats file.
    let algo = match algo_id.or(file.algo.as_deref()) {
        None => {
            // Stacking algorithms only: this flag cannot accept an
            // alignment id, so offering one as a candidate would send the
            // reader to an error instead of a fix.
            let ids: Vec<&str> = registry::stack_algorithms()
                .iter()
                .map(|a| a.id())
                .collect();
            return refuse(format!(
                "--algo is required (or an `algo` key in the preset); known algorithms: {}",
                ids.join(", ")
            ));
        }
        Some(id) => match registry::find_stack(id) {
            Ok(algo) => algo,
            Err(err) => return refuse(err.to_string()),
        },
    };

    // `Laplacian` (`crates/core/src/gpu.rs`'s own doc comment: "the WGSL
    // twin of ... `Laplacian::fuse`"). Every other registered algorithm
    // (`weighted-average`, `depth-map`, and any test-only one) has no GPU
    // implementation and must stay on the CPU path regardless of whether a
    // usable adapter exists -- resolving (and reporting) a backend at all is
    // therefore gated on the selected algorithm's own id, compared against
    // `Laplacian`'s own `id()` rather than a bare string literal repeated
    // here.
    let backend = if algo.id() == Laplacian.id() {
        crate::resolve_backend(no_gpu)
    } else {
        None
    };

    // Step 3: parse `--set` against that schema and overlay it on the
    // preset's `[stack]` table. An unknown key or an out-of-range value is
    // refused here -- from either source, with the same `ParamViolation`
    // rendering -- with the input directory still unread.
    let explicit = match parse_settings(algo.params(), &set.set)
        .and_then(|cli| merge_preset(algo.params(), &file.stack, cli))
    {
        Ok(values) => values,
        Err(violations) => {
            return refuse(
                violations
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    };

    // The zmap-flag and `-o` refusals fire **before** the input directory is
    // read at all -- a run naming both a missing input and a bad `-o` must name
    // the `-o`, which `refuses_empty_dir_and_unwritable_output` pins. Both are
    // group-independent, so they fire exactly once, before the first group.
    if let Some(zmap) = zmap {
        return refuse(zmap_flag_refusal(zmap));
    }
    if let Err(msg) = check_output_dir(output) {
        return refuse(msg);
    }

    // Step 3b: the grouping, before any work starts. `propose` reads
    // directory entries and header tags only -- no tile, no pixel -- and
    // `--groups` replaces its verdict wholesale.
    let walked = match propose(dir) {
        Ok(proposal) => proposal,
        Err(err) => return refuse(err.to_string()),
    };
    let proposal = match group_args.groups.as_deref() {
        None => walked,
        Some(path) => match groups::load(path, dir, &walked) {
            Ok(proposal) => proposal,
            Err(msg) => return refuse(msg),
        },
    };

    // Printed **even under `--quiet`**: `--quiet` governs progress, not
    // decisions, and no flag suppresses the proposal. Hence a direct
    // `eprintln!` rather than `ProgressRenderer`,
    // whose `Quiet` mode is a no-op by construction.
    eprintln!(
        "{}",
        groups::render_proposal(&proposal, group_args.groups.as_deref())
    );
    if let Some(path) = group_args.write_groups.as_deref()
        && let Err(msg) = groups::write(&proposal, path)
    {
        return refuse(msg);
    }

    let multi = proposal.groups.len() > 1;
    if let Err(msg) = check_project_collisions(output, &proposal) {
        return refuse(msg);
    }

    // Step 9: `--only` subset selection. Identities are computed against
    // the **full** proposal, before any filtering -- a subset selection
    // must never change what a group is called, or a re-run could not be
    // matched back to the original record.
    let identities: Vec<String> = proposal
        .groups
        .iter()
        .map(|g| group_identity(multi, dir, g))
        .collect();
    let selected: Vec<bool> = if group_args.only.is_empty() {
        vec![true; proposal.groups.len()]
    } else {
        for requested in &group_args.only {
            let matches = proposal
                .groups
                .iter()
                .enumerate()
                .any(|(i, g)| &identities[i] == requested || &g.name == requested);
            if !matches {
                return refuse(only_no_match_refusal(requested, &proposal, &identities));
            }
        }
        proposal
            .groups
            .iter()
            .enumerate()
            .map(|(i, g)| {
                group_args
                    .only
                    .iter()
                    .any(|only| &identities[i] == only || &g.name == only)
            })
            .collect()
    };
    for (i, sel) in selected.iter().enumerate() {
        if !sel {
            eprintln!("skipped (not named by --only): {}", identities[i]);
        }
    }

    let setup = RunSetup {
        algo,
        backend,
        explicit,
        file,
        align_opts,
        job_opts,
        preset_opts,
        argv,
        signals: &signals,
        input: dir,
    };

    let mut results: Vec<GroupResult> = Vec::new();
    let mut manifest = Vec::new();
    let mut failed = false;
    let mut cancelled = false;
    for (i, group) in proposal.groups.iter().enumerate() {
        if !selected[i] {
            continue;
        }
        let group_output = project_path(output, group);
        let identity = identities[i].clone();
        let result = run_group(&setup, group, &group_output, identity.clone());
        manifest.push(manifest_entry(group, identity, &result));
        match &result {
            GroupResult::Failed { .. } => failed = true,
            // A SIGINT stops the whole run, not one group.
            GroupResult::Cancelled { .. } => cancelled = true,
            _ => {}
        }
        results.push(result);
        if cancelled {
            break;
        }
    }

    // A `--only` subset re-run merges into whatever manifest already sits
    // at this run's path rather than overwriting it: without this,
    // `review` (which prefers a manifest over a directory glob whenever one
    // exists) would silently shrink to just the re-run subset after every
    // partial re-run, defeating the triage this supports.
    if let Err(msg) = write_manifest(output, dir, manifest) {
        eprintln!("warning: {msg}");
    }

    report_results(&results, json);

    if cancelled {
        return ExitCode::from(130);
    }
    if failed {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Everything one group's run needs that cannot differ between groups:
/// resolved once by [`run`], borrowed by every [`run_group`] call.
struct RunSetup<'a> {
    algo: &'static dyn StackAlgorithm,
    backend: Option<&'static dyn focusforge_core::gpu::TileBackend>,
    explicit: ParamValues,
    file: preset::Preset,
    align_opts: &'a AlignOptions<'a>,
    job_opts: &'a JobOptions,
    preset_opts: &'a PresetOptions<'a>,
    argv: &'a [String],
    signals: &'a signals::Signals,
    /// The directory the run was pointed at, as the user wrote it: what
    /// `StackReport::input` reports, for every group.
    input: &'a Path,
}

/// What one group's run ended as. The outer driver owns every print of a
/// document, so a single-group run emits exactly today's `StackReport` and
/// a multi-group run emits one wrapping document instead of N concatenated
/// ones.
enum GroupResult {
    Completed(Box<StackReport>),
    DryRun(Box<DryRunPlan>),
    /// Already reported on stderr and recorded by the group's own status
    /// sink; carried so the manifest has an outcome to list.
    Failed {
        error: String,
        wall_seconds: f64,
    },
    Cancelled {
        wall_seconds: f64,
    },
}

/// Records the refusal on this group's status sink, prints it exactly as
/// [`crate::fail`] does, and hands the outer driver an outcome to list.
fn failed(status: &StatusSink, start: Instant, message: impl std::fmt::Display) -> GroupResult {
    let message = message.to_string();
    let _ = status.fail(message.clone());
    GroupResult::Failed {
        error: message,
        wall_seconds: start.elapsed().as_secs_f64(),
    }
}

fn cancelled(status: &StatusSink, start: Instant) -> GroupResult {
    status.cancelled();
    GroupResult::Cancelled {
        wall_seconds: start.elapsed().as_secs_f64(),
    }
}

/// Where one group's project lands: inside the `-o` directory when one was
/// given, otherwise **beside the frames** -- the directory the group's
/// frames were enumerated from (`Group::dir`, the directory holding
/// `frames/`, never inside it).
///
/// The stem is the **group name**, already unique across the proposal,
/// already prints in the proposal and already writes into a `--groups`
/// file. The directory basename would collide for two time-split groups in
/// one directory, which is a normal case rather than an error.
fn project_path(out_dir: Option<&Path>, group: &Group) -> PathBuf {
    out_dir
        .unwrap_or(group.dir.as_path())
        .join(format!("{}.{}", group.name, PROJECT_EXTENSION))
}

/// The `--zmap` refusal. Loud means the message names the replacement: a
/// removed flag is refused *quietly and unhelpfully*, by clap, as
/// "unexpected argument '--zmap'".
fn zmap_flag_refusal(zmap: &Path) -> String {
    format!(
        "a project carries its own Z-Map, so `--zmap {}` is gone from `stack`. \
         Run `focusforge-cli export <project> --zmap {}` after the run.",
        zmap.display(),
        zmap.display()
    )
}

/// Two groups must never resolve to one project path. The naming rule
/// makes this unreachable inside a single directory; it becomes reachable
/// the moment `-o` collapses several directories onto one, so it is
/// asserted rather than trusted -- a collision would silently overwrite one
/// group's project with another's.
fn check_project_collisions(output: Option<&Path>, proposal: &Proposal) -> Result<(), String> {
    let mut paths: Vec<PathBuf> = proposal
        .groups
        .iter()
        .map(|g| project_path(output, g))
        .collect();
    paths.sort();
    let unique = {
        let mut copy = paths.clone();
        copy.dedup();
        copy.len()
    };
    if unique != paths.len() {
        let named: Vec<&str> = proposal.groups.iter().map(|g| g.name.as_str()).collect();
        return Err(format!(
            "two of the {} proposed groups ({}) resolve to the same project path -- refusing \
             rather than overwriting one group's project with another's",
            named.len(),
            named.join(", ")
        ));
    }
    Ok(())
}

/// One group's identity: the input directory as the user wrote it for a
/// single-group run, the group's name otherwise. `--only` and the run loop
/// both use it.
fn group_identity(multi: bool, dir: &Path, group: &Group) -> String {
    if multi {
        group.name.clone()
    } else {
        dir.display().to_string()
    }
}

/// The refusal when a `--only` value matches no proposed group: names every
/// group's identity (and its plain name, where the two differ), before any
/// frame is opened.
fn only_no_match_refusal(requested: &str, proposal: &Proposal, identities: &[String]) -> String {
    let known: Vec<String> = proposal
        .groups
        .iter()
        .zip(identities)
        .map(|(g, identity)| {
            if &g.name == identity {
                identity.clone()
            } else {
                format!("{identity} (name: {})", g.name)
            }
        })
        .collect();
    format!(
        "--only {requested:?} matches no proposed group; known identities: {}",
        known.join(", ")
    )
}

fn manifest_entry(group: &Group, identity: String, result: &GroupResult) -> groups::ManifestEntry {
    let (outcome, outputs, wall_seconds, error) = match result {
        GroupResult::Completed(report) => (
            "completed",
            report.outputs.clone(),
            report.wall_seconds,
            None,
        ),
        GroupResult::DryRun(plan) => ("dry-run", plan.would_write.clone(), 0.0, None),
        GroupResult::Failed {
            error,
            wall_seconds,
        } => ("failed", Vec::new(), *wall_seconds, Some(error.clone())),
        GroupResult::Cancelled { wall_seconds } => ("cancelled", Vec::new(), *wall_seconds, None),
    };
    groups::ManifestEntry {
        name: group.name.clone(),
        identity,
        frame_count: group.frames.len(),
        outcome: outcome.to_string(),
        outputs,
        wall_seconds,
        error,
    }
}

/// The run-level manifest, written for every run, single-group included.
/// With `-o <dir>` it goes to `<dir>/manifest.json`; without `-o`, projects
/// may land in several directories, so it goes to the input root as
/// `<root-basename>-run.json` (a name that cannot collide with a project).
/// A write failure warns rather than failing the run: a missing manifest
/// must never read as a missing failure.
fn write_manifest(
    out_dir: Option<&Path>,
    root: &Path,
    entries: Vec<groups::ManifestEntry>,
) -> Result<(), String> {
    let path = manifest_path(out_dir, root);
    let root_str = root.display().to_string();
    let merged = merge_manifest_entries(&path, &root_str, entries);
    let manifest = groups::RunManifest {
        format: groups::MANIFEST_FORMAT,
        root: root_str,
        groups: merged,
    };
    let text = serde_json::to_string_pretty(&manifest)
        .map_err(|err| format!("{}: serializing the manifest: {err}", path.display()))?;
    std::fs::write(&path, text).map_err(|err| format!("{}: {err}", path.display()))
}

/// Merges freshly-(re-)run `entries` into whatever manifest already sits at
/// `path`: an existing entry whose identity matches one just (re-)run is
/// replaced, every other existing entry survives untouched, and a manifest
/// whose `root` disagrees with this run's is treated as absent -- a fresh
/// write. A full run (no `--only`) attempts every proposed group, so this
/// degenerates to a full overwrite in that case; a `--only` subset re-run is
/// where the merge is load-bearing.
fn merge_manifest_entries(
    path: &Path,
    root: &str,
    entries: Vec<groups::ManifestEntry>,
) -> Vec<groups::ManifestEntry> {
    let existing = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<groups::RunManifest>(&text).ok())
        .filter(|m| m.format == groups::MANIFEST_FORMAT && m.root == root);

    let Some(existing) = existing else {
        return entries;
    };

    let fresh_identities: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.identity()).collect();
    let mut merged: Vec<groups::ManifestEntry> = existing
        .groups
        .into_iter()
        .filter(|e| !fresh_identities.contains(e.identity()))
        .collect();
    merged.extend(entries);
    merged
}

/// Where [`write_manifest`] puts its document. Split out so the naming rule
/// is unit-testable without a filesystem, and `pub(crate)` so
/// `crate::review`'s beside-the-frames scan mirrors this exact rule instead
/// of re-deriving it by hand.
pub(crate) fn manifest_path(out_dir: Option<&Path>, root: &Path) -> PathBuf {
    match out_dir {
        Some(dir) => dir.join("manifest.json"),
        None => {
            let base = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "run".to_string());
            root.join(format!("{base}-run.json"))
        }
    }
}

/// Prints whatever the run produced: for **one** group exactly today's
/// document, for many a single wrapping one. `stack --json`'s standing
/// contract is that *a* document on stdout means a completed run, so N
/// concatenated documents is not an option, and hollowing out the frozen
/// `StackReport` is not either (`DryRunPlan` is the precedent).
fn report_results(results: &[GroupResult], json: bool) {
    if results.len() == 1 {
        match &results[0] {
            GroupResult::Completed(report) => print_document(report.as_ref(), json, render),
            GroupResult::DryRun(plan) => print_document(plan.as_ref(), json, render_plan),
            GroupResult::Failed { .. } | GroupResult::Cancelled { .. } => {}
        }
        return;
    }
    if json {
        let documents: Vec<serde_json::Value> = results
            .iter()
            .filter_map(|result| match result {
                GroupResult::Completed(report) => serde_json::to_value(report.as_ref()).ok(),
                GroupResult::DryRun(plan) => serde_json::to_value(plan.as_ref()).ok(),
                _ => None,
            })
            .collect();
        let wrapped = serde_json::json!({ "format": 1, "groups": documents });
        match serde_json::to_string_pretty(&wrapped) {
            Ok(text) => println!("{text}"),
            Err(err) => eprintln!("error: failed to serialize JSON: {err}"),
        }
        return;
    }
    let mut first = true;
    for result in results {
        let text = match result {
            GroupResult::Completed(report) => render(report),
            GroupResult::DryRun(plan) => render_plan(plan),
            _ => continue,
        };
        if !first {
            println!();
        }
        first = false;
        println!("{text}");
    }
}

fn print_document<T: Serialize>(value: &T, json: bool, human: fn(&T) -> String) {
    if json {
        match serde_json::to_string_pretty(value) {
            Ok(text) => println!("{text}"),
            Err(err) => eprintln!("error: failed to serialize JSON: {err}"),
        }
    } else {
        println!("{}", human(value));
    }
}

/// One group's stack, from the checks before the run to the composite. The
/// frames arrive as a slice instead of being enumerated here.
///
/// Everything downstream already took a path slice
/// (`TiffStack::open_files`), so a one-group proposal hands this function
/// precisely `frame_paths(dir)`'s own vector, in its own order, which is
/// what makes the single-stack equivalence true by construction rather than
/// by a special case.
///
/// The per-group state lives here on purpose: one `StatusSink` (whose `Drop`
/// assertion is per instance, so one record per group), one
/// `ProgressRenderer`, and one `Instant` -- so `wall_seconds` stays per
/// stack rather than per run.
fn run_group(setup: &RunSetup<'_>, group: &Group, output: &Path, identity: String) -> GroupResult {
    let RunSetup {
        algo,
        backend,
        explicit,
        file,
        align_opts,
        job_opts,
        preset_opts,
        argv,
        signals,
        input: dir,
    } = setup;
    let (algo, backend, explicit, file) = (*algo, *backend, explicit.clone(), file);
    let (align_opts, job_opts, preset_opts, argv, signals) =
        (*align_opts, *job_opts, *preset_opts, *argv, *signals);
    let dir: &Path = dir;

    let start = Instant::now();
    // `cancelled` already use for `wall_seconds`, so a completed run's
    // stage breakdown and its own `wall_seconds` share one clock exactly.
    let mut timer = StageTimer::from_start(start);
    let mut renderer = ProgressRenderer::new(ProgressMode::choose(job_opts.quiet));
    let status = StatusSink::new(preset_opts.status, identity, "stack");
    status.set_algorithm(algo.id());

    // Step 4 (`--output`'s parent) is the outer driver's now: it is
    // group-independent, and it must fire before the input directory is
    // read at all, which grouping does.
    //
    // Step 5: this group's frames, already enumerated by the proposal, and
    // the strict uniformity guard.
    let paths = group.frames.clone();
    let stack = match TiffStack::open_files(&paths) {
        Ok(stack) => stack,
        Err(err) => return failed(&status, start, err),
    };
    let frame_names: Vec<String> = paths
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    let header_refs: Vec<(&str, &focusforge_core::io::TiffHeader)> = frame_names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), stack.header(i)))
        .collect();
    let (uniform, detail) = crate::uniform_check(&header_refs);
    if !uniform {
        return failed(
            &status,
            start,
            detail.unwrap_or_else(|| "non-uniform frame set".to_string()),
        );
    }

    // Step 6: refuse an output that is also an input frame.
    if let Some(name) = colliding_frame(output, &paths, &frame_names) {
        return failed(
            &status,
            start,
            format!(
                "output path {} is also an input frame ({name})",
                output.display()
            ),
        );
    }

    // Step 7: resolve parameters.
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let stack_ctx = StackContext::new(stack.len(), width, height, spp);
    let resolved = match params::resolve(algo.params(), &explicit, &stack_ctx) {
        Ok(resolved) => resolved,
        Err(err) => return failed(&status, start, err),
    };
    let values = params::resolved_values(&resolved);

    // Bridges the resolved pyramid depth into the scheduler geometry via a
    // trait method.
    let levels = algo.tile_levels(&values);

    // Header-only stack fingerprint -- cheap, needed both to peek a
    // resumable ledger below and to build this run's own `JobRecord`, and
    // as the stage cache key: one fingerprint, one definition, two
    // consumers.
    let fingerprint = StackFingerprint {
        frames: (0..stack.len())
            .map(|i| FrameFingerprint::from(stack.header(i)))
            .collect(),
    };
    let stage_cache = match StageCache::open(Path::new(".scratch")) {
        Ok(cache) => cache,
        Err(err) => return failed(&status, start, err.to_string()),
    };

    // Step 6: scheduler tuning flags, before alignment -- they reach the
    // scheduler regardless of whether alignment runs at all.
    let mut options = TileSchedulerOptions::new(".scratch");
    options.levels = levels;
    if let Some(tile_size) = job_opts.tile_size {
        options.tile_size = tile_size;
    }
    if let Some(max_bytes_in_flight) = job_opts.max_bytes_in_flight {
        options.max_bytes_in_flight = max_bytes_in_flight;
    }

    // Alignment, between the stack algorithm's own parameters and the
    // scheduler build. Computed *before* any output file exists: a
    // strict-mode failure refuses here.
    //
    // `used_peeked_alignment`: when `--resume` finds a matching,
    // already-recorded alignment under `.scratch/resume/`
    // (`peek_resumable_crop_and_alignment`), `align_stack` is skipped
    // entirely -- both its ~240s reference-scale cost and the stage line
    // below -- and the ledger's own recorded `StackAlignment` is reused
    // verbatim, so a resumed run's coordinates can never drift from the
    // tiles the interrupted run already wrote. `crop_rect_value` below is
    // *always* recomputed via `auto_crop`, peeked or not: it is cheap, pure
    // geometry over whichever `StackAlignment` is in hand, so it never
    // needs peeking of its own, and computing it consistently the same way
    // every time is what keeps a resumed run's composite/Z-Map header
    // byte-identical to the interrupted run's.
    let mut align_identity: Option<AlignIdentity> = None;
    let mut alignment_result: Option<(
        &'static dyn focusforge_core::algorithm::AlignAlgorithm,
        StackAlignment,
        ParamValues,
        Vec<ParamReport>,
    )> = None;
    let mut used_peeked_alignment = false;
    // `true` only when the alignment came from the stage cache --
    // never for a computed one, a ledger-peeked one, or a `--no-align` run.
    let mut align_cached = false;
    // `align_cached` alone cannot tell a `--resume` reuse from a genuinely
    // fast compute (both are `false`), so this field names all four ways
    // the align stage can be satisfied. Overwritten below the moment any
    // non-`--no-align` path is taken; stays `"skipped"` only when
    // `align_prepared` is `None`.
    let mut align_source = "skipped".to_string();
    // `Some` only in the "computed" arm below -- a cached, resumed or
    // skipped alignment never earns a worker count.
    let mut align_workers: Option<usize> = None;
    let mut peak_align_workers: Option<usize> = None;

    // Step 8.5a: *resolving* the alignment algorithm and its parameters is
    // pure -- registry lookup, `--align-set`/`[align]` validation, `auto`
    // evaluation -- and is split out from *running* `align_stack` below so
    // `--write-preset` and `--dry-run` can report the alignment half without
    // paying for the ECC solve (~240 s on the reference stack).
    let align_prepared = if align_opts.align {
        let align_algo = match resolve_align_algo(align_opts.algo.or(file.align_algo.as_deref())) {
            Ok(a) => a,
            Err(msg) => return failed(&status, start, msg),
        };
        let align_explicit = match parse_settings(align_algo.params(), align_opts.set)
            .and_then(|cli| merge_preset(align_algo.params(), &file.align, cli))
        {
            Ok(v) => v,
            Err(violations) => {
                return failed(
                    &status,
                    start,
                    violations
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        };
        let align_resolved = match params::resolve(align_algo.params(), &align_explicit, &stack_ctx)
        {
            Ok(r) => r,
            Err(err) => return failed(&status, start, err),
        };
        let align_values = params::resolved_values(&align_resolved);
        let align_params: Vec<ParamReport> = align_resolved
            .into_iter()
            .map(|(id, r)| ParamReport {
                id,
                value: r.value,
                provenance: r.provenance,
            })
            .collect();
        Some((align_algo, align_explicit, align_values, align_params))
    } else {
        None
    };

    // `--write-preset`, after resolution and independent of whether the run
    // proceeds -- it is meaningful under `--dry-run`, the "same as last
    // time, new folders" path. **Explicit values only**:
    // `explicit` and `align_explicit` are exactly the keys that resolve as
    // `Provenance::Explicit`, so a value left to an `auto` closure stays
    // live and is recomputed fresh on the next, differently-sized stack.
    if let Some(path) = preset_opts.write_preset {
        let written = preset::Preset {
            algo: Some(algo.id().to_string()),
            align_algo: align_prepared.as_ref().map(|(a, ..)| a.id().to_string()),
            stack: explicit.clone(),
            align: align_prepared
                .as_ref()
                .map(|(_, e, ..)| e.clone())
                .unwrap_or_default(),
        };
        if let Err(msg) = preset::write(&written, path) {
            return failed(&status, start, msg);
        }
    }

    // `--dry-run`: resolve, report, exit 0 -- inserted **before** the
    // alignment arm below, so `align_stack` (~240 s on the reference stack)
    // never runs, and before the `.partial` files, the `ResumeStore` and the
    // ledger further down, so nothing is created anywhere.
    // `TileScheduler::new` is pure: it builds a `TilePlan` and divides the
    // byte budget, touching no file (`crates/core/src/tile/scheduler.rs`).
    if preset_opts.dry_run {
        let scheduler = match TileScheduler::new(&stack, stack_ctx, spp, &options) {
            Ok(scheduler) => scheduler,
            Err(err) => return failed(&status, start, err),
        };
        let geometry = scheduler.plan();
        // One project, and nothing else: a run writes no image at all.
        let would_write = vec![output.display().to_string()];
        let plan = DryRunPlan {
            dry_run: true,
            input: dir.display().to_string(),
            frame_count: stack.len(),
            width,
            height,
            samples_per_pixel: spp,
            algorithm: algo.id().to_string(),
            display_name: algo.display_name().to_string(),
            params: resolved
                .into_iter()
                .map(|(id, r)| ParamReport {
                    id,
                    value: r.value,
                    provenance: r.provenance,
                })
                .collect(),
            align: align_prepared
                .as_ref()
                .map(|(align_algo, _, _, params)| DryRunAlignPlan {
                    algorithm: align_algo.id().to_string(),
                    display_name: align_algo.display_name().to_string(),
                    params: params
                        .iter()
                        .map(|p| ParamReport {
                            id: p.id.clone(),
                            value: p.value.clone(),
                            provenance: p.provenance,
                        })
                        .collect(),
                }),
            tile_size: geometry.tile_size(),
            levels: geometry.levels(),
            margin: geometry.margin(),
            tiles: geometry.tiles().len(),
            in_flight_limit: scheduler.in_flight_limit(),
            backend: backend
                .map(|b| b.info().to_string())
                .unwrap_or_else(|| "cpu".to_string()),
            would_write,
            not_computed: vec![
                "crop".to_string(),
                "output_width".to_string(),
                "output_height".to_string(),
            ],
        };
        status.dry_run(&plan.would_write);
        return GroupResult::DryRun(Box::new(plan));
    }

    timer.mark(Stage::Setup);
    if let Some((align_algo, _, align_values, align_params)) = align_prepared {
        // Step 5: resolved once, fed both to the identity below and to
        // `align_stack` (never re-derived in the miss arm).
        let policy = if align_opts.lax {
            focusforge_core::align::FailurePolicy::IdentityFallback
        } else {
            focusforge_core::align::FailurePolicy::Strict
        };
        let identity = AlignIdentity {
            algorithm: align_algo.id().to_string(),
            params: align_values.clone(),
            failure_policy: policy,
        };
        align_identity = Some(identity.clone());

        let peeked = if job_opts.resume {
            peek_resumable_crop_and_alignment(
                Path::new(".scratch"),
                &fingerprint,
                options.tile_size,
                options.levels,
                algo.id(),
                &values,
                Some(&identity),
            )
        } else {
            None
        };

        if let Some(peeked) = peeked.filter(|p| p.alignment.is_some()) {
            let result = peeked.alignment.expect("filtered to Some above");
            used_peeked_alignment = true;
            align_source = "resumed".to_string();
            alignment_result = Some((align_algo, result, align_values, align_params));
        } else {
            // Step 6: the stage cache lookup. Consulted only when the
            // ledger peek above returned nothing (`--resume` with no
            // matching ledger, or no `--resume` at all) -- ordering is
            // fixed: a peeked alignment is the value the already-written
            // tiles were produced against and always wins.
            let stage_key = StageKey::align(&fingerprint, &identity);
            let cache_hit = if job_opts.cache {
                stage_cache.get_alignment(&stage_key)
            } else {
                None
            };

            if let Some(result) = cache_hit {
                align_cached = true;
                align_source = "cached".to_string();
                renderer.stage(&format!(
                    "align {} frames (reused from the stage cache)",
                    stack.len()
                ));
                alignment_result = Some((align_algo, result, align_values, align_params));
            } else {
                // Step 9 (unnumbered in the align stage): `align_stack` has
                // no unit boundary -- a scoped watcher thread prints the
                // cancellation notice the moment a SIGINT is observed, so
                // the terminal never looks hung for the whole alignment's
                // wall time, and once it returns, a still-set cancel exits
                // 130 before anything is created (below).
                renderer.stage(&format!("align {} frames (no unit progress)", stack.len()));
                let done = AtomicBool::new(false);
                let solved = std::thread::scope(|scope| {
                    let watcher = scope.spawn(|| {
                        let mut warned = false;
                        while !done.load(Ordering::SeqCst) {
                            std::thread::sleep(job::POLL);
                            if !warned && signals.cancel.load(Ordering::SeqCst) {
                                eprintln!(
                                    "cancel requested -- alignment has no unit boundary; \
                                     stopping at the fuse stage (Ctrl-C again to abort now)"
                                );
                                warned = true;
                            }
                        }
                    });
                    let result = align_stack_with(
                        align_algo,
                        stack_ctx,
                        &stack,
                        &align_values,
                        policy,
                        &AlignDriverOptions::default(),
                    );
                    done.store(true, Ordering::SeqCst);
                    watcher.join().expect("align watcher thread must not panic");
                    result
                });
                if signals.cancel.load(Ordering::SeqCst) {
                    return cancelled(&status, start);
                }
                let run = match solved {
                    Ok(r) => r,
                    Err(err) => return failed(&status, start, err),
                };
                align_workers = Some(run.worker_limit);
                peak_align_workers = Some(run.peak_workers);
                let result = run.alignment;
                if !align_opts.lax && result.failure_count() > 0 {
                    let names: Vec<String> = result
                        .failed()
                        .map(|f| format!("{} ({:?})", frame_names[f.frame], f.status))
                        .collect();
                    return failed(
                        &status,
                        start,
                        format!(
                            "alignment failed for {} frame(s): {}",
                            names.len(),
                            names.join(", ")
                        ),
                    );
                }
                // A run about to refuse (above) caches nothing. A `put`
                // failure warns on stderr and does not fail the run -- the
                // posture `write_manifest`'s own caller already takes.
                if job_opts.cache
                    && let Err(err) = stage_cache.put_alignment(&stage_key, &paths, &result)
                {
                    eprintln!("warning: {err}");
                }
                align_source = "computed".to_string();
                alignment_result = Some((align_algo, result, align_values, align_params));
            }
        }
        // Marked only inside this arm: `--no-align` never enters this
        // `if let` at all, so the stage stays `None` -- "not entered"
        // rather than a near-zero measured span.
        timer.mark(Stage::Align);
    }

    // The alignment used to compute the crop and to drive the tile reader:
    // the real result when aligned, or an all-identity one otherwise --
    // `--no-align` with the crop left on still trims the kernel-radius
    // margin (the two flags are independent), and going through the same
    // `auto_crop` call in both cases (peeked alignment or not: `auto_crop`
    // is cheap, pure geometry, never the expensive part) is what guarantees
    // the rect matches what `align --json` reports for the same stack, and
    // what a resumed run recomputes identically to the interrupted run it
    // continues.
    let alignment_for_crop: StackAlignment = match &alignment_result {
        Some((_, result, _, _)) => result.clone(),
        None => identity_alignment_for_crop(stack.len()),
    };
    let crop_rect_value = match auto_crop(&alignment_for_crop, (width, height)) {
        Ok(c) => c,
        Err(err) => return failed(&status, start, err),
    };
    if align_opts.crop && crop_rect_value.is_none() {
        return failed(&status, start, crate::align::empty_crop_refusal());
    }
    let applied_crop = if align_opts.crop {
        crop_rect_value
    } else {
        None
    };

    let description = alignment_result
        .as_ref()
        .map(|(align_algo, result, values, _params)| {
            let mode = values
                .get("mode")
                .and_then(ParamValue::as_enum)
                .unwrap_or("n/a");
            format_composite_description(
                align_algo.id(),
                result.reference,
                mode,
                crop_rect_value.map(|r| (r, align_opts.crop)),
            )
        });

    let transforms: Vec<Transform> = alignment_for_crop
        .frames
        .iter()
        .map(|f| f.transform)
        .collect();
    let warping_reader = WarpingReader::new(&stack, (width, height), spp, transforms.clone());

    // Builds the scheduler over the warping reader -- bit-identical to
    // `&stack` when every transform is identity (the decorator's own
    // short-circuit), which is what keeps `--no-align --no-crop` byte for
    // byte with the unaligned path. `TileError::NoInterior` fires here, if
    // it fires at all, before any tile is read.
    //
    // With a backend selected, the scheduler is built over `&stack` (the
    // raw reader) instead: the GPU path pulls its own footprint-grown
    // source rects through `BackendFusionConsumer` and must not receive
    // CPU-warped pixels.
    //
    // `options` is built far above, before alignment, so `--tile-size` and
    // `--max-bytes-in-flight` are already in effect for the resume peek --
    // never rebuilt here.
    let reader: &dyn focusforge_core::algorithm::TileReader = match &backend {
        Some(_) => &stack,
        None => &warping_reader,
    };
    let scheduler = match TileScheduler::new(reader, stack_ctx, spp, &options) {
        Ok(scheduler) => scheduler,
        Err(err) => return failed(&status, start, err),
    };

    // Step 10: the staging area. Fusion still writes two TIFFs -- a project
    // cannot be written tile by tile, because a blob's entry name *is* the
    // content hash of the finished plane and zstd has no seekable format --
    // and the project is sealed from them once the job completes.
    //
    // They live under `.scratch/`, never beside the target: `propose`'s walk
    // descends into every subdirectory, so a staging *directory* left beside
    // the frames by an interrupted run would be proposed as an extra group
    // on the next run. Only the sealed container's `.partial` file sits
    // beside the target, and a `.partial` file is invisible to that walk
    // (`frame_paths` filters `*.tif`/`*.tiff`).
    //
    // The key is the group name plus a short digest of the project path it
    // will seal into. It must be stable across runs or `--resume` cannot
    // find its ledger (`ResumeStore::find` skips a candidate whose
    // `composite_output` no longer exists) -- the same invocation always
    // produces the same key. The digest is what keeps two *different* runs
    // that happen to propose the same group name apart: with the name
    // alone, two runs collecting into two different `-o` directories would
    // fight over one staging directory, which is not a collision the user
    // asked for. With it, two runs collide exactly when they already share
    // an output path.
    let staging = Path::new(".scratch/projects").join(format!(
        "{}-{}",
        group.name,
        project::short_hash(&output.display().to_string())
    ));
    if let Err(err) = std::fs::create_dir_all(&staging) {
        return failed(
            &status,
            start,
            format!("creating {}: {err}", staging.display()),
        );
    }
    let staged_composite = staging.join("composite.tif");
    let staged_zmap = staging.join("zmap.tif");

    let geometry = FusionGeometry {
        width,
        height,
        samples_per_pixel: spp,
        frame_count: stack.len() as u32,
        crop: applied_crop,
        description: description.clone(),
    };

    // Resolved **here**, in one place: the exact `ImageDescription` the
    // staged composite carries, so the manifest records it and `export`
    // re-emits a byte-identical file. `None` is `TiffWriter`'s own default,
    // and guessing at that string rather than asking for it is how the two
    // drift apart.
    let composite_description = description
        .clone()
        .unwrap_or_else(TiffSpec::default_description);

    // Resolved once: the ledger's identity and the report's `backend` field
    // are the same fact, and a resume that compared a differently-spelled
    // copy of it would refuse (or accept) for the wrong reason.
    let backend_id = backend
        .map(|b| b.info().to_string())
        .unwrap_or_else(|| "cpu".to_string());

    // Step 7: the ledger, around the `.partial` discipline above.
    // `composite_output` is the `.partial` path, never the final name --
    // `ResumeStore::find` skips a candidate whose `composite_output` no
    // longer exists, and the final name only appears after a successful
    // rename.
    let record = JobRecord {
        format: JOB_RECORD_FORMAT,
        fingerprint,
        width,
        height,
        samples_per_pixel: spp,
        frame_count: stack.len(),
        tile_size: scheduler.plan().tile_size(),
        levels: scheduler.plan().levels(),
        stack_algorithm: algo.id().to_string(),
        stack_params: values.clone(),
        backend: backend_id.clone(),
        align: align_identity,
        crop: applied_crop,
        composite_output: staged_composite.clone(),
        zmap_output: Some(staged_zmap.clone()),
    };
    let resume_store = match ResumeStore::open(Path::new(".scratch")) {
        Ok(store) => store,
        Err(err) => return failed(&status, start, err.to_string()),
    };
    let ledger = if job_opts.resume {
        match resume_store.find(&record) {
            Ok(Resumable::None) => {
                return failed(
                    &status,
                    start,
                    "no resumable state for this stack under .scratch/resume",
                );
            }
            Ok(Resumable::Mismatch(mismatch)) => {
                return failed(&status, start, mismatch.to_string());
            }
            Ok(Resumable::Ledger(ledger)) => ledger,
            Err(err) => return failed(&status, start, err.to_string()),
        }
    } else {
        // A non-resume run may legally disagree with old state -- a
        // `Mismatch` here is not an error. Any matching ledger is
        // discarded first: two cancelled runs over the same stack and
        // output path would otherwise leave two ledgers, and
        // `ResumeStore::find`'s pick among them is unspecified order.
        match resume_store.find(&record) {
            Ok(Resumable::Ledger(stale)) => {
                if let Err(err) = stale.discard() {
                    return failed(&status, start, err.to_string());
                }
            }
            Ok(_) => {}
            Err(err) => return failed(&status, start, err.to_string()),
        }
        match resume_store.begin(&record) {
            Ok(ledger) => ledger,
            Err(err) => return failed(&status, start, err.to_string()),
        }
    };

    // When no alignment was peeked, `align_stack` runs as usual and
    // `ledger.record_alignment(&result)` records it at the align/fuse
    // boundary. By construction, this is the *only* place that boundary can
    // be: the ledger above is always either fresh (nothing recorded yet) or
    // came from `find` after this run's own `record` already matched it in
    // full including `align` identity, and a cancel can only ever happen
    // once a ledger exists (`Job` -- hence any interruptible state -- is
    // built after alignment resolves). A peeked alignment
    // (`used_peeked_alignment`) is already the ledger's own recorded value
    // and needs no re-recording.
    if !used_peeked_alignment
        && let Some((_, result, _, _)) = &alignment_result
        && let Err(err) = ledger.record_alignment(result)
    {
        cleanup_staging(&staging);
        let _ = ledger.discard();
        return failed(&status, start, err.to_string());
    }

    // The resume composition -- `FusionConsumer::resume` when the ledger
    // already recorded completed units, `FusionConsumer::create` otherwise;
    // `Job::resuming`/`Job::new` to match. Both paths wrap the consumer in
    // `RecordingConsumer` before handing it to `job::drive`.
    let is_resuming = !ledger.completed().is_empty();
    let (sink, surviving): (FusionConsumer, Option<Vec<usize>>) = if is_resuming {
        match FusionConsumer::resume(
            scheduler.plan(),
            ledger.completed(),
            algo,
            values.clone(),
            &staged_composite,
            Some(&staged_zmap),
            geometry.clone(),
        ) {
            Ok((sink, surviving)) => (sink, Some(surviving)),
            Err(err) => {
                cleanup_staging(&staging);
                let _ = ledger.discard();
                return failed(&status, start, err);
            }
        }
    } else {
        match FusionConsumer::create(
            algo,
            values.clone(),
            &staged_composite,
            Some(&staged_zmap),
            geometry,
        ) {
            Ok(sink) => (sink, None),
            Err(err) => {
                cleanup_staging(&staging);
                let _ = ledger.discard();
                return failed(&status, start, err);
            }
        }
    };

    let mut job = match &surviving {
        Some(surviving) => Job::resuming(&scheduler, surviving),
        None => Job::new(&scheduler),
    };
    let control = job.control();
    // A fuse-stage entry line, unconditional and immediate (`eta --`, since
    // no unit has finished yet) -- same reasoning as the align stage line:
    // it gives a script or a human watching a redirected log a fact the
    // instant the job starts, and it is what lets a test drive its SIGINT
    // off "the first line observed" rather than a sleep
    // (`crates/cli/tests/job_cli.rs`'s own doc comment).
    renderer.start(&control.progress());
    // The GPU consumer, when one was resolved, sits *between* the ledger and
    // the `FusionConsumer`: `BackendFusionConsumer` supplies its own pixels
    // and writes through `sink`'s `write_output`, while `RecordingConsumer`
    // stays the outermost wrapper so every unit the engine completes is
    // recorded on either path. `RecordingConsumer` forwards
    // `supplies_own_pixels`, so wrapping it does not put the scheduler's
    // host read loop back.
    let backend_consumer = backend.map(|b| {
        BackendFusionConsumer::new(b, &stack, (width, height), transforms, spp, levels, &sink)
    });
    let consumer: &dyn TileConsumer = match &backend_consumer {
        Some(bc) => bc,
        None => &sink,
    };
    let recording = RecordingConsumer {
        inner: consumer,
        ledger: &ledger,
    };
    let outcome = match job::drive(
        &mut job,
        &control,
        &NoopPreprocess,
        &recording,
        &mut renderer,
        signals,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            // A genuine error: `Job::run` already called `consumer.abort()`
            // on this exact path, so `consumer.finish()` must never be
            // attempted here.
            cleanup_staging(&staging);
            let _ = ledger.discard();
            return failed(&status, start, err);
        }
    };

    // Step 10: the three outcomes.
    if outcome == JobOutcome::Cancelled {
        // On cancel, the `.partial` file
        // and the ledger deliberately survive -- no `finish()`, no rename,
        // no `cleanup_partials`. Nothing on stdout: the `--json` report
        // means a completed run, the same way every existing failure path
        // prints no JSON.
        eprintln!(
            "cancelled -- {} of {} unit(s) done; resume with: {}",
            control.progress().units_done,
            control.progress().unit_total,
            job::resume_recipe(argv)
        );
        return cancelled(&status, start);
    }
    // Marked only past the cancel return above: a cancelled run's timer is
    // dropped with `GroupResult::Cancelled`, never turned into
    // `StageTimings`; folding `prepare` into `fuse` means this one mark
    // also closes the scheduler build/staging/ledger-open span above.
    timer.mark(Stage::Fuse);
    debug_assert_eq!(
        outcome,
        JobOutcome::Completed,
        "job::drive never returns Paused"
    );

    let final_progress = control.progress();
    let in_flight_limit = scheduler.in_flight_limit();
    let tiles = final_progress.unit_total;
    // On the CPU path every tile read reads every frame
    // (`crates/core/src/tile/scheduler.rs`'s per-tile loop), so this is the
    // exact count, not an approximation. On the GPU path it is exactly 0:
    // `BackendFusionConsumer` supplies its own pixels, so the scheduler
    // short-circuits its host read loop and never reads a frame-tile through
    // it at all (the same 0 `TileRunReport::frame_tiles_read` reports there).
    let frame_tiles_read = if backend.is_some() {
        0
    } else {
        tiles * stack.len()
    };
    // `peak_in_flight`: `Job::run` does not expose the per-segment
    // `TileRunReport` `TileScheduler::run` returns (the job layer discards
    // it after folding `completed`/`stopped` into `JobOutcome`), so the
    // true observed peak concurrency is not available through this engine
    // without a `core` change. `in_flight_limit` is used as the reported
    // value: an honest upper bound, exact whenever the run never dropped
    // below the scheduler's configured concurrency.
    let peak_in_flight = in_flight_limit;

    if let Err(err) = sink.finish() {
        cleanup_staging(&staging);
        let _ = ledger.discard();
        return failed(&status, start, err);
    }

    // Step 11: seal the two staged planes into one project. This is where
    // the two renames used to be: same seam, same failure posture -- any
    // error cleans the staging directory, discards the ledger and fails the
    // group, leaving neither a project at the named path nor a `.partial`
    // beside it.
    renderer.stage("seal project");
    let metrics = measure_metrics(&staged_composite, &paths);
    let sealed = project::seal(project::SealRequest {
        project_path: output,
        staged_composite: &staged_composite,
        staged_zmap: &staged_zmap,
        sources: &paths,
        algorithm: algo.id(),
        params: &values,
        align: alignment_result
            .as_ref()
            .map(
                |(align_algo, result, align_values, _)| project::AlignRecord {
                    algorithm: align_algo.id().to_string(),
                    params: align_values.clone(),
                    reference: result.reference,
                },
            ),
        composite_description,
        label: None,
        metrics,
    });
    let sealed = match sealed {
        Ok(sealed) => sealed,
        Err(err) => {
            cleanup_staging(&staging);
            let _ = ledger.discard();
            return failed(&status, start, err);
        }
    };
    cleanup_staging(&staging);
    if let Err(err) = ledger.discard() {
        return failed(&status, start, err.to_string());
    }
    // The completion line stays a fixed sentence: it is
    // what `job_cli.rs` watches for, and a measured number belongs in the
    // report and the manifest rather than in a line a test matches on.
    renderer.finish("stack complete");

    let outputs = vec![sealed.path.display().to_string()];
    status.completed(&outputs);

    let align_report = alignment_result.map(|(align_algo, result, _values, params)| {
        let failure_count = result.failure_count();
        let reference = result.reference;
        let reference_policy = result.policy;
        let alignments = crate::align::alignment_entries(&result, &frame_names);
        StackAlignReport {
            algorithm: align_algo.id().to_string(),
            display_name: align_algo.display_name().to_string(),
            params,
            reference,
            reference_policy,
            failure_policy: if align_opts.lax { "lax" } else { "strict" }.to_string(),
            failure_count,
            alignments,
        }
    });

    timer.mark(Stage::Finalize);
    let wall_seconds = timer.total_seconds();
    let stages = timer.finish();

    let plan = scheduler.plan();
    let report = StackReport {
        input: dir.display().to_string(),
        frame_count: stack.len(),
        frames: frame_names,
        width,
        height,
        samples_per_pixel: spp,
        algorithm: algo.id().to_string(),
        display_name: algo.display_name().to_string(),
        params: resolved
            .into_iter()
            .map(|(id, r)| ParamReport {
                id,
                value: r.value,
                provenance: r.provenance,
            })
            .collect(),
        tile_size: plan.tile_size(),
        levels: plan.levels(),
        margin: plan.margin(),
        tiles,
        frame_tiles_read,
        in_flight_limit,
        peak_in_flight,
        outputs,
        wall_seconds,
        output_width: applied_crop.map(|r| r.width).unwrap_or(width),
        output_height: applied_crop.map(|r| r.height).unwrap_or(height),
        crop: applied_crop,
        description,
        align: align_report,
        backend: backend_id,
        align_cached,
        stages,
        align_source,
        align_workers,
        peak_align_workers,
    };

    GroupResult::Completed(Box::new(report))
}

/// The sharpness window's maximum side. A whole-plane Laplacian variance is
/// not computable here: `laplacian_variance` is whole-buffer and
/// `crates/core/src/io` forbids assembling a whole frame from tiles, and
/// "max over every source frame" at whole-frame size would mean a second
/// full stack read per run. A named central window is what the manifest
/// records instead, honestly, beside the numbers.
const LAPVAR_WINDOW: u32 = 2048;

/// The composite's and the sharpest source frame's Laplacian variance over
/// one **central window**, measured identically on both so the ratio
/// `review` divides stays meaningful.
///
/// Bounded memory (~100 MB at 2048x2048x3) and exactly one tile read per
/// source frame. Every failure is silence rather than a refusal: a metric
/// that could not be measured must not turn a completed stack into a
/// failed one, and `Option` is what says so in the manifest.
fn measure_metrics(composite: &Path, sources: &[PathBuf]) -> Metrics {
    let Ok(stack) = TiffStack::open_files(std::slice::from_ref(&composite.to_path_buf())) else {
        return Metrics::default();
    };
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let side_w = LAPVAR_WINDOW.min(width);
    let side_h = LAPVAR_WINDOW.min(height);
    let window = centered(width, height, side_w, side_h);
    let composite_lapvar = lapvar(&stack, window, spp);

    // Sequentially, one frame at a time: each open maps that frame, and 61
    // concurrent mappings would exceed an 8 GB ceiling.
    let mut max_input_lapvar: Option<f64> = None;
    for path in sources {
        let Ok(frame) = TiffStack::open_files(std::slice::from_ref(path)) else {
            continue;
        };
        let (fw, fh) = frame.dimensions();
        if fw < side_w || fh < side_h {
            continue;
        }
        let fspp = frame.header(0).samples_per_pixel;
        let rect = centered(fw, fh, side_w, side_h);
        if let Some(value) = lapvar(&frame, rect, fspp) {
            max_input_lapvar = Some(max_input_lapvar.map_or(value, |m: f64| m.max(value)));
        }
    }

    Metrics {
        composite_lapvar,
        max_input_lapvar,
        lapvar_window: composite_lapvar.map(|_| window),
    }
}

/// The `side_w` x `side_h` rect centred in a `width` x `height` plane.
fn centered(width: u32, height: u32, side_w: u32, side_h: u32) -> Rect {
    Rect {
        x: (width - side_w) / 2,
        y: (height - side_h) / 2,
        width: side_w,
        height: side_h,
    }
}

/// `laplacian_variance` over one window of frame 0 of `stack`, or `None`
/// when that window cannot be read.
fn lapvar(stack: &TiffStack, rect: Rect, spp: u32) -> Option<f64> {
    let mut samples = vec![0u16; rect.width as usize * rect.height as usize * spp as usize];
    stack.read_tile_into(0, rect, &mut samples).ok()?;
    Some(focusforge_core::fuse::metrics::laplacian_variance(
        &samples,
        rect.width,
        rect.height,
        spp,
    ))
}

/// An all-identity [`StackAlignment`] over `frame_count` frames: what
/// `--no-align` feeds to `auto_crop` and the `WarpingReader`, so a crop
/// still trims the kernel-radius margin even with alignment off, and the
/// reader stays bit-identical to a direct read either way
/// (`WarpingReader`'s own identity short-circuit).
fn identity_alignment_for_crop(frame_count: usize) -> StackAlignment {
    StackAlignment {
        reference: 0,
        policy: ReferencePolicy::Anchor,
        frames: (0..frame_count)
            .map(|i| FrameAlignment {
                frame: i,
                transform: Transform::identity(),
                status: AlignStatus::Converged,
                iterations: 0,
                residual: 1.0,
                fallback: false,
            })
            .collect(),
    }
}

/// A resumable candidate's already-recorded `crop` and (if the ledger had
/// already reached the fuse stage) `StackAlignment`, read directly off a
/// matching `.scratch/resume/*/job.json` -- and that same ledger
/// directory's `alignment.json`, when present -- so a `--resume` run can
/// build a correct `JobRecord` for the authoritative `ResumeStore::find`
/// call *without* first running `align_stack` to compute `crop` itself.
/// Every field compared here (`fingerprint`, `tile_size`, `levels`,
/// `stack_algorithm`, `stack_params`, `align` identity) is exactly what
/// `ResumeStore::find` re-verifies afterward -- this function's own mistake
/// costs only a missed optimization (falling back to running `align_stack`
/// normally), never a wrong resume: nothing downstream trusts this peek's
/// verdict on its own.
fn peek_resumable_crop_and_alignment(
    scratch_base: &Path,
    fingerprint: &StackFingerprint,
    tile_size: u32,
    levels: u32,
    stack_algorithm: &str,
    stack_params: &ParamValues,
    align_identity: Option<&AlignIdentity>,
) -> Option<job::PeekedResume> {
    let entries = std::fs::read_dir(scratch_base.join("resume")).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("job.json")) else {
            continue;
        };
        let Ok(candidate) = serde_json::from_str::<JobRecord>(&text) else {
            continue;
        };
        if candidate.format != JOB_RECORD_FORMAT {
            continue;
        }
        if !candidate.composite_output.exists() {
            continue;
        }
        if &candidate.fingerprint != fingerprint {
            continue;
        }
        if candidate.tile_size != tile_size || candidate.levels != levels {
            continue;
        }
        if candidate.stack_algorithm != stack_algorithm {
            continue;
        }
        if &candidate.stack_params != stack_params {
            continue;
        }
        if candidate.align.as_ref() != align_identity {
            continue;
        }
        // `read_alignment` decodes the same bit-exact `transform_bits` shape
        // `alignment.json` is actually written in; any read failure
        // (absent, unparsable) is treated as "no recorded alignment yet"
        // here, same as everywhere else in this function.
        let alignment = read_alignment(&dir.join("alignment.json")).ok().flatten();
        return Some(job::PeekedResume { alignment });
    }
    None
}

/// Best-effort removal of one group's staging directory and both staged
/// planes. Never fails the caller: a missing directory (the common case,
/// when nothing was created yet) is not itself an error.
///
/// Called on **failure** and on success, never on cancel: a cancelled run
/// deliberately keeps its staged planes *and* its ledger so `--resume` can
/// continue.
fn cleanup_staging(staging: &Path) {
    let _ = std::fs::remove_dir_all(staging);
}

/// Refuses an `-o` that is not an existing directory.
///
/// `-o` names the directory a run's projects are **collected into**, not a
/// file path. A path that looks like an image gets an export hint pointing
/// at `focusforge-cli export`; every non-directory path is refused loudly,
/// by name -- never by letting `TiffWriter::create` surface a bare `ENOENT`
/// from inside the run after tiles have been read. Never creates the
/// directory.
fn check_output_dir(output: Option<&Path>) -> Result<(), String> {
    let Some(output) = output else {
        return Ok(());
    };
    if output.is_dir() {
        return Ok(());
    }
    let looks_like_an_image = output
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"));
    let mut message = format!(
        "-o names the directory this run's projects are collected into -- {} is not an \
         existing directory. Point -o at a directory, or omit it to write each project beside \
         its own frames.",
        output.display()
    );
    if looks_like_an_image {
        message.push_str(
            " A stacking run no longer writes an image: run \
             `focusforge-cli export <project> -o <file>` afterwards.",
        );
    }
    Err(message)
}

/// The file name of the first entry of `frame_paths`/`frame_names` whose
/// absolute path equals `candidate`'s, normalized with `std::path::absolute`
/// (lexical only, so it works on `candidate` even though that file does not
/// exist yet -- `canonicalize` requires existence and cannot be used here).
pub(crate) fn colliding_frame(
    candidate: &Path,
    frame_paths: &[PathBuf],
    frame_names: &[String],
) -> Option<String> {
    let candidate_abs = std::path::absolute(candidate).ok()?;
    frame_paths
        .iter()
        .zip(frame_names)
        .find(|(p, _)| std::path::absolute(p).ok().as_ref() == Some(&candidate_abs))
        .map(|(_, name)| name.clone())
}

/// Human rendering of one [`StackReport`]: one fact per line, unit-testable
/// without capturing stdout -- the shape `algorithms::render_listing`/
/// `render_schema` use.
fn render(report: &StackReport) -> String {
    let mut lines = vec![
        format!("{} ({} frames)", report.input, report.frame_count),
        format!(
            "{}x{} {}ch",
            report.width, report.height, report.samples_per_pixel
        ),
        format!("{} - {}", report.algorithm, report.display_name),
    ];
    lines.extend(report.params.iter().map(param_line));
    lines.push(format!(
        "tile_size={} levels={} margin={} tiles={} in_flight_limit={}",
        report.tile_size, report.levels, report.margin, report.tiles, report.in_flight_limit,
    ));
    lines.push(format!("backend: {}", report.backend));
    for output in &report.outputs {
        lines.push(format!("wrote: {output}"));
    }
    lines.push(format!("wall: {:.3}s", report.wall_seconds));
    // `workers=<limit> peak=<peak>` rides the existing `stages:` line, for
    // symmetry with `crate::align::render` -- both `?` unless the align
    // stage was actually computed.
    lines.push(format!(
        "stages: {} {} {} {} workers={} peak={}",
        stage_fact("setup", report.stages.setup),
        stage_fact("align", report.stages.align),
        stage_fact("fuse", report.stages.fuse),
        stage_fact("finalize", report.stages.finalize),
        report
            .align_workers
            .map(|w| w.to_string())
            .unwrap_or_else(|| "?".to_string()),
        report
            .peak_align_workers
            .map(|w| w.to_string())
            .unwrap_or_else(|| "?".to_string()),
    ));
    lines.join("\n")
}

/// One `stages:` fact -- `"{name}={value:.3}s"` for a stage that was
/// entered, `"{name}=not-entered"` for one that was not. Never
/// `"{name}=0.000s"` for a never-entered stage: that would read as work
/// that happened instantly rather than work that never ran. Shared by
/// [`render`] (`stack --json`'s human twin) and
/// [`crate::align::render`] (`align --json`'s).
pub(crate) fn stage_fact(name: &str, value: Option<f64>) -> String {
    match value {
        Some(seconds) => format!("{name}={seconds:.3}s"),
        None => format!("{name}=not-entered"),
    }
}

/// One resolved parameter, in the line shape [`render`] uses --
/// `"{id} = {value} ({Provenance:?})"`, so a dry run and a real run print
/// the same sentence for the same fact.
fn param_line(p: &ParamReport) -> String {
    format!(
        "{} = {} ({:?})",
        p.id,
        serde_json::to_string(&p.value).unwrap_or_else(|_| "?".to_string()),
        p.provenance,
    )
}

/// Human rendering of one [`DryRunPlan`], the same one-fact-per-line shape
/// [`render`] uses. The last two lines state what a dry run refuses to
/// promise, rather than leaving it out.
fn render_plan(plan: &DryRunPlan) -> String {
    let mut lines = vec![
        "dry run -- resolved only, nothing computed and nothing written".to_string(),
        format!("{} ({} frames)", plan.input, plan.frame_count),
        format!(
            "{}x{} {}ch",
            plan.width, plan.height, plan.samples_per_pixel
        ),
        format!("{} - {}", plan.algorithm, plan.display_name),
    ];
    lines.extend(plan.params.iter().map(param_line));
    match &plan.align {
        Some(align) => {
            lines.push(format!(
                "align: {} - {}",
                align.algorithm, align.display_name
            ));
            lines.extend(align.params.iter().map(param_line));
        }
        None => lines.push("align: skipped (--no-align)".to_string()),
    }
    lines.push(format!(
        "tile_size={} levels={} margin={} tiles={} in_flight_limit={}",
        plan.tile_size, plan.levels, plan.margin, plan.tiles, plan.in_flight_limit,
    ));
    lines.push(format!("backend: {}", plan.backend));
    for output in &plan.would_write {
        lines.push(format!("would write: {output}"));
    }
    lines.push("crop: not computed -- --dry-run stops before alignment".to_string());
    lines.push("output size: not computed -- it follows from the crop".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> StackReport {
        StackReport {
            input: "fixtures/mantis-512".to_string(),
            frame_count: 8,
            frames: vec!["00.tif".to_string(), "01.tif".to_string()],
            width: 512,
            height: 512,
            samples_per_pixel: 3,
            // A fictional id on purpose. This module is `crates/cli/src`,
            // and the criterion that no algorithm id appears here is
            // enforced by a literal `git grep` -- a gate with a
            // documented exception for test data is not a gate. `render`
            // formats whatever string it is handed, so a real id would
            // buy the test nothing.
            algorithm: "example-stack".to_string(),
            display_name: "Example Stacker".to_string(),
            params: vec![ParamReport {
                id: "levels".to_string(),
                value: ParamValue::Int(5),
                provenance: Provenance::Auto,
            }],
            tile_size: 512,
            levels: 5,
            margin: 96,
            tiles: 4,
            frame_tiles_read: 32,
            in_flight_limit: 5,
            peak_in_flight: 4,
            outputs: vec!["mantis-512.ffproj".to_string()],
            wall_seconds: 1.5,
            output_width: 512,
            output_height: 512,
            crop: None,
            description: None,
            align: None,
            backend: "cpu".to_string(),
            align_cached: false,
            stages: focusforge_core::timing::StageTimings {
                setup: Some(0.012),
                align: Some(1.2),
                fuse: Some(0.25),
                warp_write: None,
                finalize: Some(0.038),
            },
            align_source: "computed".to_string(),
            align_workers: Some(3),
            peak_align_workers: Some(3),
        }
    }

    #[test]
    fn render_names_every_fact_one_per_line() {
        let text = render(&sample_report());
        for needle in [
            "fixtures/mantis-512",
            "8 frames",
            "512x512",
            "3ch",
            "example-stack",
            "Example Stacker",
            "levels = 5 (Auto)",
            "tile_size=512",
            "levels=5",
            "margin=96",
            "tiles=4",
            "in_flight_limit=5",
            "wrote: mantis-512.ffproj",
            "wall: 1.500s",
            "backend: cpu",
            "stages:",
            "setup=0.012s",
            "align=1.200s",
            "fuse=0.250s",
            "finalize=0.038s",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    #[test]
    fn a_stage_never_entered_renders_as_not_entered_never_as_zero() {
        let mut sample = sample_report();
        sample.stages.align = None;
        let text = render(&sample);
        assert!(
            text.contains("align=not-entered"),
            "missing align=not-entered in: {text}"
        );
        assert!(
            !text.contains("align=0.000s"),
            "a never-entered stage must never render as 0.000s: {text}"
        );
    }

    #[test]
    fn the_run_manifest_goes_beside_the_frames_without_o() {
        assert_eq!(
            manifest_path(None, Path::new("/shoot/mantis")),
            Path::new("/shoot/mantis/mantis-run.json")
        );
        assert_eq!(
            manifest_path(Some(Path::new("/out")), Path::new("/shoot/mantis")),
            Path::new("/out/manifest.json")
        );
    }

    #[test]
    fn an_output_file_path_is_refused_by_name() {
        let message = check_output_dir(Some(Path::new("out.tif"))).unwrap_err();
        assert!(message.contains("export"), "{message}");
        assert!(message.contains("out.tif"), "{message}");
        // A non-image path that is not a directory is still refused, just
        // without the migration sentence.
        let plain = check_output_dir(Some(Path::new("/definitely/not/here"))).unwrap_err();
        assert!(!plain.contains("export"), "{plain}");
        assert!(check_output_dir(None).is_ok(), "omitting -o is legal");
    }

    #[test]
    fn the_zmap_refusal_names_the_replacement() {
        let message = zmap_flag_refusal(Path::new("map.tif"));
        assert!(message.contains("focusforge-cli export"), "{message}");
        assert!(message.contains("--zmap map.tif"), "{message}");
    }

    #[test]
    fn colliding_frame_names_the_matching_frame() {
        let frame_paths = vec![PathBuf::from("dir/00.tif"), PathBuf::from("dir/01.tif")];
        let frame_names = vec!["00.tif".to_string(), "01.tif".to_string()];
        let found = colliding_frame(Path::new("dir/01.tif"), &frame_paths, &frame_names);
        assert_eq!(found, Some("01.tif".to_string()));

        let not_found = colliding_frame(Path::new("dir/out.tif"), &frame_paths, &frame_names);
        assert_eq!(not_found, None);
    }

    fn sample_group(name: &str, dir: &str) -> Group {
        Group {
            name: name.to_string(),
            dir: PathBuf::from(dir),
            frames: vec![PathBuf::from(dir).join("00.tif")],
            signal: focusforge_core::io::GroupSignal::Directory,
            first_time: None,
            last_time: None,
        }
    }

    #[test]
    fn group_identity_is_the_input_dir_for_a_single_group_run_and_the_name_for_a_multi_one() {
        let group = sample_group("IMG_01-02", "card-dump");
        assert_eq!(
            group_identity(false, Path::new("card-dump"), &group),
            "card-dump"
        );
        assert_eq!(
            group_identity(true, Path::new("card-dump"), &group),
            "IMG_01-02"
        );
    }

    #[test]
    fn only_no_match_refusal_lists_every_known_identity() {
        let a = sample_group("a", "dir");
        let b = sample_group("b", "dir");
        let proposal = Proposal {
            root: PathBuf::from("dir"),
            groups: vec![a, b],
            gap: std::time::Duration::from_secs(60),
            time_signal_available: false,
        };
        let identities = vec!["a".to_string(), "b".to_string()];
        let message = only_no_match_refusal("c", &proposal, &identities);
        assert!(message.contains("--only \"c\""), "{message}");
        assert!(message.contains('a'), "{message}");
        assert!(message.contains('b'), "{message}");
    }

    fn sample_manifest_entry(name: &str, output: &str) -> groups::ManifestEntry {
        groups::ManifestEntry {
            name: name.to_string(),
            identity: name.to_string(),
            frame_count: 1,
            outcome: "completed".to_string(),
            outputs: vec![output.to_string()],
            wall_seconds: 1.0,
            error: None,
        }
    }

    #[test]
    fn merge_manifest_entries_keeps_unrelated_entries_and_replaces_matching_ones() {
        let dir = std::env::temp_dir().join(format!("ff-stack-merge-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("manifest.json");
        let existing = groups::RunManifest {
            format: groups::MANIFEST_FORMAT,
            root: "root".to_string(),
            groups: vec![
                sample_manifest_entry("a", "a.ffproj"),
                sample_manifest_entry("b", "b.ffproj"),
            ],
        };
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        let fresh = vec![sample_manifest_entry("a", "a2.ffproj")];
        let merged = merge_manifest_entries(&path, "root", fresh);
        assert_eq!(merged.len(), 2);
        let a = merged.iter().find(|e| e.identity() == "a").unwrap();
        assert_eq!(a.outputs, vec!["a2.ffproj".to_string()]);
        let b = merged.iter().find(|e| e.identity() == "b").unwrap();
        assert_eq!(b.outputs, vec!["b.ffproj".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_manifest_entries_treats_a_mismatched_root_as_absent() {
        let dir = std::env::temp_dir().join(format!(
            "ff-stack-merge-mismatch-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("manifest.json");
        let existing = groups::RunManifest {
            format: groups::MANIFEST_FORMAT,
            root: "other-root".to_string(),
            groups: vec![sample_manifest_entry("a", "a.ffproj")],
        };
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        let fresh = vec![sample_manifest_entry("b", "b.ffproj")];
        let merged = merge_manifest_entries(&path, "root", fresh);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].identity(), "b");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
