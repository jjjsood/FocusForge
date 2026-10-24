// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli stack`: groups the input frames, resolves a registered
//! `StackAlgorithm` and hands each group to `focusforge_core::pipeline::run`,
//! which seals one project per group. Every refusal fires before a frame is
//! opened where the check allows it (algorithm id, `--set` values), and
//! before a tile is read otherwise.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use focusforge_core::algorithm::StackAlgorithm;
use focusforge_core::align::ReferencePolicy;
use focusforge_core::equalize::{EqualizationReport, EqualizeSettings};
use focusforge_core::gpu::{AlignBackend, TileBackend};
use focusforge_core::io::{FrameReader, Group, Proposal, Rect, open_frame_set, propose};
use focusforge_core::job::{JobControl, StageCache};
use focusforge_core::params::{self, ParamValue, ParamValues, Provenance};
use focusforge_core::project::{self, PROJECT_EXTENSION};
use focusforge_core::registry;
use focusforge_core::tile::{TileScheduler, TileSchedulerOptions};
use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::Serialize;

use crate::align::{AlignOptions, AlignmentEntry, resolve_align_algo};
use crate::groups::{self, GroupArgs};
use crate::job::{self, JobOptions};
use crate::preset::{self, PresetOptions};
use crate::progress::{ProgressMode, ProgressRenderer};
use crate::set::{SetArgs, merge_preset, parse_settings};
use crate::signals;
use crate::status::StatusSink;

/// The `stack --json` document.
#[derive(Serialize)]
pub struct StackReport {
    pub input: String,
    pub frame_count: usize,
    pub frames: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    /// The intensity encoding fusion operates on: `encoded-srgb` for RAW
    /// input, `encoded-unknown-transfer` for TIFF. Code values are used as
    /// delivered, never converted.
    pub working_space: &'static str,
    pub algorithm: String,
    pub display_name: String,
    pub params: Vec<ParamReport>,
    pub tile_size: u32,
    pub levels: u32,
    pub margin: u32,
    pub tiles: usize,
    pub frame_tiles_read: usize,
    pub in_flight_limit: usize,
    /// This group's *configured* host in-flight-tile limit (`in_flight_limit`,
    /// above), not a measured peak: `Job::run` does not expose the
    /// scheduler's per-segment `TileRunReport`. `device_peak_in_flight` is
    /// the measured device-side figure.
    pub peak_in_flight: usize,
    /// The host in-flight-byte budget actually in force for this run.
    pub max_bytes_in_flight: u64,
    /// The bytes the scheduler accounted for at its in-flight limit
    /// (`in_flight_limit` tiles, reader scratch included): the figure
    /// compared with `max_bytes_in_flight`, never with resident memory.
    pub accounted_bytes_in_flight: u64,
    /// Where that budget came from: `Host` (adaptive), `HostFallback` (the
    /// probe degraded), or `Explicit` (`--max-host-memory` lowered it).
    pub budget_provenance: Provenance,
    /// Whether the **host** scheduler's runtime back-off (the process-RSS
    /// pressure rule) shrunk the in-flight limit during this run -- reported
    /// on both paths, since the host scheduler runs on the GPU path too.
    /// Never about VRAM: the device budget has no runtime back-off of its
    /// own to report.
    pub backed_off: bool,
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
    /// The brightness equalization's estimate; `null` when it was off.
    pub equalization: Option<EqualizationReport>,
    /// `"cpu"`, or the selected adapter's [`focusforge_core::gpu::BackendInfo`]
    /// display, so which path ran is machine-readable and not only a stderr
    /// line.
    pub backend: String,
    /// The device in-flight-byte budget actually in force for this run's
    /// GPU path -- `null` on the CPU path.
    pub max_device_bytes_in_flight: Option<u64>,
    /// Where that budget came from: `Host` (the startup probe settled),
    /// `HostFallback` (no probe ran, the stated default was used), or
    /// `Explicit` (`--max-vram` lowered it) -- `null` on the CPU path.
    pub device_budget_provenance: Option<Provenance>,
    /// The device budget's high-water count of tiles admitted at once during
    /// this group (per group, never a running maximum). `0` when no tile
    /// went through the device; `null` on the CPU path.
    pub device_peak_in_flight: Option<usize>,
    /// Why the run fused on the host although a device was available: the
    /// message naming the tile that did not fit the device budget. `null`
    /// when the device ran, or no device was in use. On a fallback `backend`
    /// is `cpu` and the three device fields above are `null`.
    pub gpu_fallback: Option<String>,
    /// `true` only when the alignment came out of the stage cache;
    /// `false` for a computed one, a `--resume` ledger-peeked one, or a
    /// `--no-align` run.
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
    /// reading a stage number as a real cost.
    pub align_source: String,
    /// The anchor arm's worker bound and the peak it actually reached --
    /// `None` unless the alignment was computed this run.
    pub align_workers: Option<usize>,
    pub peak_align_workers: Option<usize>,
    /// The bytes the alignment accounted for (shared reference data plus one
    /// registration per worker) -- `None` unless the alignment was computed
    /// this run.
    pub align_accounted_bytes: Option<u64>,
    /// `"cpu"`, or the adapter's display when at least one alignment level
    /// ran on it -- `null` unless the alignment was computed this run.
    pub align_backend: Option<String>,
}

/// A narrow, `crates/gpu`-free handle on a `GpuBackend`'s realized tile
/// concurrency, scoped to exactly one group: [`Self::reset_peak`] before the
/// group's drive, [`Self::peak_in_flight`] read only after it returns. Not a
/// `TileBackend` method, because `crates/core` never learns what VRAM is;
/// not a field on `DeviceBudgetReport`, because that number does not exist
/// until the scheduler has run.
pub trait DevicePeak {
    /// Restarts the high-water interval [`Self::peak_in_flight`] reads back.
    fn reset_peak(&self);
    fn peak_in_flight(&self) -> usize;
}

/// The settled device (VRAM) budget and its provenance. Lives in the CLI
/// because `core` has no concept of VRAM.
#[derive(Clone, Copy)]
pub struct DeviceBudgetReport {
    pub max_device_bytes: u64,
    pub provenance: Provenance,
}

/// `resolve_backend`'s return value: a named struct rather than a growing
/// tuple of `Option`s. `backend` and `device_peak` coerce from the *same*
/// `&'static GpuBackend` binding on the GPU path, so nothing re-enters the
/// `GPU_BACKEND` `OnceLock` and no second probe or ceiling is observed.
pub struct BackendChoice {
    pub backend: Option<&'static dyn TileBackend>,
    pub device_budget: Option<DeviceBudgetReport>,
    pub device_peak: Option<&'static dyn DevicePeak>,
}

impl BackendChoice {
    /// The CPU path: no backend, no device budget, no device peak.
    pub fn cpu() -> Self {
        BackendChoice {
            backend: None,
            device_budget: None,
            device_peak: None,
        }
    }
}

/// `resolve_align_backend`'s return value: the align stage's accelerator and
/// the cell holding the adapter's display once the accelerator opens it, which
/// the reports name only when a level actually ran on it.
pub struct AlignBackendChoice {
    pub backend: Option<&'static dyn AlignBackend>,
    pub adapter: Option<&'static OnceLock<String>>,
}

impl AlignBackendChoice {
    /// The CPU path: no accelerator, no adapter.
    pub fn cpu() -> Self {
        AlignBackendChoice {
            backend: None,
            adapter: None,
        }
    }

    /// The report's `align_backend` value for a run whose accelerator was
    /// used (or not).
    pub fn report_name(&self, used: bool) -> String {
        match (self.adapter.and_then(OnceLock::get), used) {
            (Some(adapter), true) => adapter.clone(),
            _ => "cpu".to_string(),
        }
    }
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

#[derive(Clone, Serialize)]
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
    /// Host bytes available to the tile scheduler after input caching.
    pub max_bytes_in_flight: u64,
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
/// `algo_id: None` is refused here, by name, rather than by clap: `--algorithm`
/// is declared `Option<String>` in `crate::Command::Stack` specifically so
/// its absence reaches this function as `None` instead of aborting the
/// process on clap's own usage error (exit 2, no knowledge of the registry).
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    output: Option<&Path>,
    algo_id: Option<&str>,
    json: bool,
    set: &SetArgs,
    align_opts: &AlignOptions<'_>,
    job_opts: &JobOptions,
    preset_opts: &PresetOptions<'_>,
    group_args: &GroupArgs,
    argv: &[String],
    no_gpu: bool,
    max_host_memory: Option<u64>,
    max_vram: Option<u64>,
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

    // The preset, ahead of everything else -- a malformed file is
    // refused before a frame is opened, and its `algo` key is what
    // algorithm resolution below falls back to.
    let file = match preset::load(preset_opts.preset) {
        Ok(preset) => preset,
        Err(msg) => return refuse(msg),
    };
    if preset_opts.dry_run && job_opts.resume {
        return refuse("--dry-run and --resume are incompatible: a dry run writes nothing, so there is nothing to resume into".to_string());
    }

    // Resolve the algorithm first, before anything else -- an
    // unknown or missing id is refused before the input directory (which
    // may not even exist) is ever touched. `--algorithm` beats the preset's own
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
                "--algorithm is required (or an `algo` key in the preset); known algorithms: {}",
                ids.join(", ")
            ));
        }
        Some(id) => match registry::find_stack(id) {
            Ok(algo) => algo,
            Err(err) => return refuse(err.to_string()),
        },
    };

    // This crate's GPU backend is a WGSL port of exactly one algorithm, so
    // every other id stays on the CPU path -- `crate::resolve_backend`
    // (`focusforge_gpu::harness::backend_for`) holds that gate itself.
    let choice = crate::resolve_backend(algo.id(), no_gpu, max_vram, job_opts.quiet);
    let backend = choice.backend;
    let device_budget_report = choice.device_budget;
    let device_peak = choice.device_peak;

    // Parse `--set` against the algorithm's schema chained with the project
    // parameters and overlay it on the preset's `[stack]` table. An unknown
    // key or an out-of-range value is refused here -- from either source,
    // with the same `ParamViolation` rendering -- with the input directory
    // still unread. The project keys are split off before resolution: they
    // belong to the run, not to the algorithm's own schema.
    let project_schema = project::params_for(algo);
    let schema: Vec<_> = algo
        .params()
        .iter()
        .chain(project_schema.iter())
        .cloned()
        .collect();
    let (project_values, explicit) = match parse_settings(&schema, &set.set)
        .and_then(|cli| merge_preset(&schema, &file.stack, cli))
    {
        Ok(values) => values
            .into_iter()
            .partition::<ParamValues, _>(|(id, _)| project_schema.iter().any(|d| &d.id == id)),
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

    // The `align` project value (`--set align=false`, a preset's `[stack]`
    // table) switches alignment off like `--no-align`.
    let aligns = align_opts.align
        && project_values
            .get("align")
            .and_then(ParamValue::as_bool)
            .unwrap_or(true);
    let align_opts = &AlignOptions {
        align: aligns,
        ..*align_opts
    };

    // The align accelerator is resolved only for a run that aligns. An
    // unknown align algorithm is refused later, per group, with the usual
    // message; here it only means no accelerator.
    let align_choice = if align_opts.align {
        match resolve_align_algo(align_opts.algo.or(file.align_algo.as_deref())) {
            Ok(align_algo) => {
                crate::resolve_align_backend(align_algo.id(), no_gpu, max_vram, job_opts.quiet)
            }
            Err(_) => AlignBackendChoice::cpu(),
        }
    } else {
        AlignBackendChoice::cpu()
    };

    // The `-o` refusal fires before the input directory is read, so a run
    // naming both a missing input and a bad output names the output first.
    if let Err(msg) = check_output_dir(output) {
        return refuse(msg);
    }

    // The grouping, before any work starts. `propose` reads
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

    if !job_opts.quiet {
        eprintln!(
            "{}",
            groups::render_proposal(&proposal, group_args.groups.as_deref())
        );
    }
    if let Some(path) = group_args.write_groups.as_deref()
        && let Err(msg) = groups::write(&proposal, path)
    {
        return refuse(msg);
    }

    let multi = proposal.groups.len() > 1;
    if let Err(msg) = check_project_collisions(output, &proposal) {
        return refuse(msg);
    }

    // `--only` subset selection. Identities are computed against
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
        if !sel && !job_opts.quiet {
            eprintln!("skipped (not named by --only): {}", identities[i]);
        }
    }

    let setup = RunSetup {
        algo,
        backend,
        device_budget_report,
        device_peak,
        align_choice,
        explicit,
        equalize: project::equalization_settings(&project_schema, &project_values),
        project_values,
        file,
        align_opts,
        job_opts,
        preset_opts,
        max_host_memory,
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
        let group_start = Instant::now();
        // A panic ends this group alone: the rest of the batch still runs,
        // and the group is recorded exactly as any other refusal is. The
        // unwinding run's own status sink never emitted, so the verdict
        // needs a fresh one.
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_group(&setup, group, &group_output, identity.clone())
        })) {
            Ok(result) => result,
            Err(payload) => {
                let status = StatusSink::new(preset_opts.status, identity.clone(), "stack");
                let result = self::failed(
                    &status,
                    group_start,
                    focusforge_diagnostics::panic_payload(&*payload),
                );
                if let Some(report) = focusforge_diagnostics::last_crash_report() {
                    eprintln!("crash report: {}", report.display());
                    tracing::error!(
                        target: MIRRORED_OUTPUT_TARGET,
                        "crash report: {}",
                        report.display()
                    );
                }
                result
            }
        };
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
        tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "warning: {msg}");
    }

    for result in &results {
        let GroupResult::Completed(report) = result else {
            continue;
        };
        let Some(align) = &report.align else {
            continue;
        };
        if align.failure_count > 0 {
            let warning = format!(
                "warning: {} frame(s) failed to register and fell back to identity",
                align.failure_count
            );
            eprintln!("{warning}");
            tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "{warning}");
        }
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
    /// The device (VRAM) budget settled on when `backend` was resolved --
    /// `None` on the CPU path, or when the GPU probe itself never ran.
    device_budget_report: Option<DeviceBudgetReport>,
    /// The same `&'static GpuBackend` as `backend`, coerced to its narrow
    /// `DevicePeak` handle -- `None` on the CPU path, mirroring
    /// `device_budget_report`.
    device_peak: Option<&'static dyn DevicePeak>,
    /// The align stage's accelerator, resolved once beside `backend`.
    align_choice: AlignBackendChoice,
    explicit: ParamValues,
    /// The explicitly set project parameters (`PROJECT_PARAMS`).
    project_values: ParamValues,
    /// What brightness equalization runs with, `None` when off: resolved
    /// against the fusion method's own project defaults.
    equalize: Option<EqualizeSettings>,
    file: preset::Preset,
    align_opts: &'a AlignOptions<'a>,
    job_opts: &'a JobOptions,
    preset_opts: &'a PresetOptions<'a>,
    /// A **ceiling only** on the adaptive host-memory budget
    /// (`--max-host-memory`) -- applied to every group's own
    /// `TileSchedulerOptions`.
    max_host_memory: Option<u64>,
    argv: &'a [String],
    signals: &'a signals::Signals,
    /// The directory the run was pointed at, as the user wrote it: what
    /// `StackReport::input` reports, for every group.
    input: &'a Path,
}

/// What one group's run ended as. The outer driver owns every print of a
/// document, so a single-group run emits a `StackReport` and a multi-group
/// run emits one wrapping document instead of N concatenated ones.
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
        .filter(|m| m.root == root);

    let Some(existing) = existing else {
        return entries;
    };

    let fresh_identities: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.identity.as_str()).collect();
    let mut merged: Vec<groups::ManifestEntry> = existing
        .groups
        .into_iter()
        .filter(|e| !fresh_identities.contains(e.identity.as_str()))
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

/// Prints whatever the run produced: one group, a `StackReport`; several,
/// one wrapping document (a document on stdout means a completed run).
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
        let wrapped = serde_json::json!({ "groups": documents });
        match serde_json::to_string_pretty(&wrapped) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("error: failed to serialize JSON: {err}");
                tracing::error!(
                    target: MIRRORED_OUTPUT_TARGET,
                    "error: failed to serialize JSON: {err}"
                );
            }
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
            Err(err) => {
                eprintln!("error: failed to serialize JSON: {err}");
                tracing::error!(
                    target: MIRRORED_OUTPUT_TARGET,
                    "error: failed to serialize JSON: {err}"
                );
            }
        }
    } else {
        println!("{}", human(value));
    }
}

/// One group's stack, from the checks before the run to the composite. The
/// frames arrive as a slice instead of being enumerated here.
///
/// Everything downstream receives this path slice in order, so a one-group
/// proposal is equivalent to opening `frame_paths(dir)` directly.
///
/// The per-group state lives here on purpose: one `StatusSink` (whose `Drop`
/// assertion is per instance, so one record per group), one
/// `ProgressRenderer`, and one `Instant` -- so `wall_seconds` stays per
/// stack rather than per run.
fn run_group(setup: &RunSetup<'_>, group: &Group, output: &Path, identity: String) -> GroupResult {
    let RunSetup {
        algo,
        backend,
        device_budget_report,
        device_peak,
        align_choice,
        explicit,
        project_values,
        equalize,
        file,
        align_opts,
        job_opts,
        preset_opts,
        max_host_memory,
        argv,
        signals,
        input: dir,
    } = setup;
    let (algo, backend, device_budget_report, device_peak, explicit, file) = (
        *algo,
        *backend,
        *device_budget_report,
        *device_peak,
        explicit.clone(),
        file,
    );
    let (align_opts, job_opts, preset_opts, max_host_memory, argv, signals) = (
        *align_opts,
        *job_opts,
        *preset_opts,
        *max_host_memory,
        *argv,
        *signals,
    );
    let dir: &Path = dir;

    let start = Instant::now();
    // Entered for this group alone, so an event raised anywhere below names
    // the stack it belongs to.
    let _stack = tracing::debug_span!("stack", identity = %identity).entered();
    // Inside the span, so a forced crash carries this run's own context.
    focusforge_diagnostics::force_crash("stack");
    let renderer = ProgressRenderer::new(ProgressMode::choose(job_opts.no_progress));
    let status = StatusSink::new(preset_opts.status, identity, "stack");
    status.set_algorithm(algo.id());

    // `--output`'s parent is the outer driver's: it is group-independent,
    // and it must fire before the input directory is read at all, which
    // grouping does.
    //
    // This group's frames, already enumerated by the proposal, and the
    // lenient uniformity check (dimensions, bits per sample, samples per
    // pixel, photometric and planar configuration; byte order and
    // RowsPerStrip are not compared), are opened here too (not only inside
    // the driver) because `--write-preset` and `--dry-run` must report
    // without ever entering `focusforge_core::pipeline::run`.
    let paths = group.frames.clone();
    let preflight_budget = TileSchedulerOptions::new(".scratch")
        .budget
        .capped_to(max_host_memory);
    let opened = match open_frame_set(&paths, preflight_budget) {
        Ok(opened) => opened,
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
    let frame_reader: &dyn FrameReader = &*opened.reader;

    // Refuse an output that is also an input frame.
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

    // Resolved here for the report/plan only; the real run re-resolves
    // inside the driver against the frames it actually opens.
    let stack_ctx = frame_reader.stack_context();
    let (width, height) = (stack_ctx.width, stack_ctx.height);
    let spp = stack_ctx.samples_per_pixel;
    let resolved = match params::resolve(algo.params(), &explicit, &stack_ctx) {
        Ok(resolved) => resolved,
        Err(err) => return failed(&status, start, err),
    };

    // Resolving the alignment algorithm and its parameters is pure --
    // registry lookup, `--align-set`/`[align]` validation, `auto`
    // evaluation -- and is split out from running alignment (inside the
    // driver) so `--write-preset` and `--dry-run` can report the alignment
    // half without paying for the ECC solve.
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
        let align_params: Vec<ParamReport> = align_resolved
            .into_iter()
            .map(|(id, r)| ParamReport {
                id,
                value: r.value,
                provenance: r.provenance,
            })
            .collect();
        Some((align_algo, align_explicit, align_params))
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
            stack: explicit
                .iter()
                .chain(project_values.iter())
                .map(|(id, value)| (id.clone(), value.clone()))
                .collect(),
            align: align_prepared
                .as_ref()
                .map(|(_, e, _)| e.clone())
                .unwrap_or_default(),
        };
        if let Err(msg) = preset::write(&written, path) {
            return failed(&status, start, msg);
        }
    }

    // `--dry-run`: resolve, report, exit 0 -- never enters the driver, so
    // alignment (~240 s on the reference stack) never runs and nothing is
    // created anywhere. `TileScheduler::new` is pure: it builds a `TilePlan`
    // and divides the byte budget, touching no file.
    if preset_opts.dry_run {
        let support = algo.tile_support(&params::resolved_values(&resolved));
        let mut options = TileSchedulerOptions::new(".scratch");
        options.apply_support(support);
        if let Some(tile_size) = job_opts.tile_size {
            options.tile_size = tile_size;
        }
        options.budget = opened.scheduler_budget;
        let scheduler = match TileScheduler::new(frame_reader, stack_ctx, spp, &options) {
            Ok(scheduler) => scheduler,
            Err(err) => return failed(&status, start, err),
        };
        let geometry = scheduler.plan();
        // One project, and nothing else: a run writes no image at all.
        let would_write = vec![output.display().to_string()];
        let plan = DryRunPlan {
            dry_run: true,
            input: dir.display().to_string(),
            frame_count: frame_reader.len(),
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
                .map(|(align_algo, _, params)| DryRunAlignPlan {
                    algorithm: align_algo.id().to_string(),
                    display_name: align_algo.display_name().to_string(),
                    params: params.clone(),
                }),
            tile_size: geometry.tile_size(),
            levels: geometry.levels(),
            margin: geometry.margin(),
            tiles: geometry.tiles().len(),
            in_flight_limit: scheduler.in_flight_limit(),
            max_bytes_in_flight: opened.scheduler_budget.bytes(),
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

    // The real run: hand the whole align -> fuse -> seal spine to
    // `focusforge_core::pipeline::run`, exactly the call `crates/gui`'s own
    // worker makes -- one driver, two front ends.
    let align_request = align_prepared.map(|(align_algo, align_explicit, _)| {
        focusforge_core::pipeline::AlignRequest {
            algorithm: align_algo,
            explicit: align_explicit,
            lax: align_opts.lax,
        }
    });
    // The key is the group name plus a short digest of the project path it
    // will seal into. It must be stable across runs or `--resume` cannot
    // find its ledger (`ResumeStore::find` skips a candidate whose
    // `composite_output` does not exist) -- the same invocation always
    // produces the same key. The digest is what keeps two *different* runs
    // that happen to propose the same group name apart: with the name
    // alone, two runs collecting into two different `-o` directories would
    // fight over one staging directory, which is not a collision the user
    // asked for. With it, two runs collide exactly when they already share
    // an output path.
    let staging_key = format!(
        "{}-{}",
        group.name,
        project::short_hash(&output.display().to_string())
    );
    let request = focusforge_core::pipeline::FuseRequest {
        frames: paths.clone(),
        output: focusforge_core::pipeline::FuseOutput::Seal(output.to_path_buf()),
        scratch_root: PathBuf::from(".scratch"),
        staging_key,
        algorithm: algo,
        explicit,
        align: align_request,
        crop: align_opts.crop
            && project_values
                .get("auto-crop")
                .and_then(ParamValue::as_bool)
                .unwrap_or(true),
        equalize: *equalize,
        tile_size: job_opts.tile_size,
        max_host_memory,
        backend,
        align_backend: align_choice.backend,
        resume: job_opts.resume,
        cache: job_opts.cache,
        poll: job::POLL,
    };

    // Resolved once: the report's `backend` field, and the ledger's own
    // identity inside the driver, are the same fact.
    let backend_id = backend
        .map(|b| b.info().to_string())
        .unwrap_or_else(|| "cpu".to_string());

    let control = JobControl::new();
    let stage_cache = match StageCache::open(Path::new(".scratch")) {
        Ok(cache) => cache,
        Err(err) => return failed(&status, start, err.to_string()),
    };
    let observer = CliObserver {
        renderer: Mutex::new(renderer),
        signals,
        control: Arc::clone(&control),
    };

    // Reset here, not earlier: the only `VramBudget::acquire` call site in
    // the tree is inside `GpuBackend::warp_fuse_tile`, reached only through
    // the driver's own `BackendFusionConsumer` below -- so this is the last
    // point before any permit for this group can be taken, making the read
    // at the end of the group a genuine per-group high-water mark.
    if let Some(peak) = device_peak {
        peak.reset_peak();
    }

    let outcome = focusforge_core::pipeline::run(
        &request,
        &observer,
        Arc::clone(&control),
        Some(&stage_cache),
    );

    match outcome {
        Ok(focusforge_core::pipeline::FuseOutcome::Cancelled {
            units_done,
            unit_total,
        }) => {
            // The `.partial` file and the ledger deliberately survive a
            // clean cancel. Nothing on stdout: the `--json` report means a
            // completed run, the same way every failure path prints none.
            let notice = format!(
                "cancelled -- {units_done} of {unit_total} unit(s) done; resume with: {}",
                job::resume_recipe(argv)
            );
            eprintln!("{notice}");
            tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "{notice}");
            cancelled(&status, start)
        }
        Ok(focusforge_core::pipeline::FuseOutcome::Completed(report)) => {
            let outputs = vec![sealed_path(&report)];
            status.completed(&outputs);
            let stack_report = build_stack_report(
                dir,
                algo,
                backend_id,
                device_budget_report,
                device_peak,
                align_opts,
                align_choice,
                *report,
            );
            GroupResult::Completed(Box::new(stack_report))
        }
        Err(err) => failed(&status, start, err.to_string()),
    }
}

/// Translates [`focusforge_core::pipeline::StageObserver`] into terminal
/// rendering and signal handling: `stage`/`tick` render through the shared
/// [`ProgressRenderer`] (called from the driver's poll thread, hence `Sync`);
/// `tick` folds a pending SIGINT/SIGUSR1 into the control; `paused` blocks
/// until a resume or a cancel.
struct CliObserver<'a> {
    renderer: Mutex<ProgressRenderer>,
    signals: &'a signals::Signals,
    control: Arc<JobControl>,
}

impl focusforge_core::pipeline::StageObserver for CliObserver<'_> {
    fn stage(&self, stage: focusforge_core::pipeline::RunStage) {
        use focusforge_core::pipeline::{AlignSource, RunStage};
        let mut renderer = self
            .renderer
            .lock()
            .expect("no thread panicked while rendering");
        match stage {
            RunStage::Prepare { frames } => {
                renderer.stage(&format!("prepare {frames} frames (brightness)"));
                renderer.start(&self.control.progress());
            }
            // A peeked (resumed) alignment prints nothing -- it cost no
            // wall time and is not a stage a viewer is waiting through.
            RunStage::Align {
                source: AlignSource::Resumed,
                ..
            } => {}
            RunStage::Align {
                frames,
                source: AlignSource::Cached,
            } => renderer.stage(&format!(
                "align {frames} frames (reused from the stage cache)"
            )),
            RunStage::Align {
                frames,
                source: AlignSource::Computed,
            } => renderer.stage(&format!("align {frames} frames")),
            // A fuse-stage entry line, unconditional and immediate (`eta
            // --`, since no unit has finished yet): it gives a script or a
            // human watching a redirected log a fact the instant the job
            // starts.
            RunStage::Fuse => renderer.start(&self.control.progress()),
            RunStage::Seal => renderer.stage("seal project"),
            // The completion line's exact wording is what `job_cli.rs`
            // watches for.
            RunStage::Finished => renderer.finish("stack complete"),
        }
    }

    fn tick(&self, progress: &focusforge_core::job::JobProgress) {
        // `cancel` is only ever *read* here -- see `crate::signals`'
        // module docs for why resetting it would disarm the second-Ctrl-C
        // kill.
        if self.signals.cancel.load(Ordering::SeqCst) {
            self.control.cancel();
        }
        if self.signals.pause.swap(false, Ordering::SeqCst) {
            match self.control.state() {
                focusforge_core::job::ControlState::PauseRequested => self.control.clear(),
                _ => self.control.pause(),
            }
        }
        // Only the stage that is counting renders: a tick outside fuse,
        // outside a counted prepare stage, and with a cached or absent
        // alignment (`align_total == 0`) carries a stale or zeroed total.
        use focusforge_core::job::JobStage;
        let preparing = progress.stage == JobStage::Prepare && progress.prepare_total > 0;
        if progress.stage == JobStage::Fuse || preparing || progress.align_total > 0 {
            self.renderer
                .lock()
                .expect("no thread panicked while rendering")
                .tick(progress);
        }
    }

    fn paused(&self, control: &focusforge_core::job::JobControl) {
        self.renderer
            .lock()
            .expect("no thread panicked while rendering")
            .stage("paused -- send SIGUSR1 to resume (Ctrl-C to cancel)");
        loop {
            std::thread::sleep(job::POLL);
            if self.signals.cancel.load(Ordering::SeqCst) {
                control.cancel();
                return;
            }
            if self.signals.pause.swap(false, Ordering::SeqCst) {
                control.clear();
                return;
            }
        }
    }

    fn notice(&self, message: &str) {
        eprintln!("{message}");
        tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "{message}");
    }
}

/// The project a CLI run sealed, displayed.
fn sealed_path(report: &focusforge_core::pipeline::FuseReport) -> String {
    match &report.product {
        focusforge_core::pipeline::FuseProduct::Sealed(sealed) => sealed.path.display().to_string(),
        focusforge_core::pipeline::FuseProduct::Held(_) => {
            unreachable!("a seal request always reports a sealed project")
        }
    }
}

/// Assembles [`StackReport`] from the driver's
/// [`focusforge_core::pipeline::FuseReport`] plus the facts only this crate
/// knows: the input directory as the user wrote it, and the GPU-only device
/// budget/peak (`crates/core` never learns what VRAM is).
#[allow(clippy::too_many_arguments)]
fn build_stack_report(
    dir: &Path,
    algo: &'static dyn StackAlgorithm,
    backend_id: String,
    device_budget_report: Option<DeviceBudgetReport>,
    device_peak: Option<&'static dyn DevicePeak>,
    align_opts: &AlignOptions<'_>,
    align_choice: &AlignBackendChoice,
    report: focusforge_core::pipeline::FuseReport,
) -> StackReport {
    let outputs = vec![sealed_path(&report)];
    // A run that fell back never touched the device: its budget and peak
    // describe a path that did not run.
    let (backend_id, device_budget_report, device_peak) = if report.gpu_fallback.is_some() {
        ("cpu".to_string(), None, None)
    } else {
        (backend_id, device_budget_report, device_peak)
    };
    let align = report.align.map(|a| {
        let failure_count = a.alignment.failure_count();
        let reference = a.alignment.reference;
        let reference_policy = a.alignment.policy;
        let alignments = crate::align::alignment_entries(&a.alignment, &report.frames);
        StackAlignReport {
            algorithm: a.algorithm_id,
            display_name: a.display_name,
            params: a
                .params
                .into_iter()
                .map(|(id, r)| ParamReport {
                    id,
                    value: r.value,
                    provenance: r.provenance,
                })
                .collect(),
            reference,
            reference_policy,
            failure_policy: if align_opts.lax { "lax" } else { "strict" }.to_string(),
            failure_count,
            alignments,
        }
    });
    let align_backend = (report.align_source == "computed")
        .then(|| align_choice.report_name(report.align_backend_used));
    StackReport {
        input: dir.display().to_string(),
        frame_count: report.frame_count,
        frames: report.frames,
        width: report.width,
        height: report.height,
        samples_per_pixel: report.samples_per_pixel,
        working_space: report.working_space,
        algorithm: algo.id().to_string(),
        display_name: algo.display_name().to_string(),
        params: report
            .stack_params
            .into_iter()
            .map(|(id, r)| ParamReport {
                id,
                value: r.value,
                provenance: r.provenance,
            })
            .collect(),
        tile_size: report.tile_size,
        levels: report.levels,
        margin: report.margin,
        tiles: report.tiles,
        frame_tiles_read: report.frame_tiles_read,
        in_flight_limit: report.in_flight_limit,
        // `Job::run` does not expose the per-segment `TileRunReport`
        // `TileScheduler::run` returns, so the true observed peak
        // concurrency is not available through this engine. The configured
        // limit is reported instead: an honest upper bound, exact whenever
        // the run never dropped below the scheduler's configured
        // concurrency.
        peak_in_flight: report.in_flight_limit,
        max_bytes_in_flight: report.max_bytes_in_flight,
        accounted_bytes_in_flight: report.accounted_bytes_in_flight,
        budget_provenance: report.budget_provenance,
        backed_off: report.backed_off,
        outputs,
        wall_seconds: report.wall_seconds,
        output_width: report.output_width,
        output_height: report.output_height,
        crop: report.crop,
        description: report.description,
        align,
        equalization: report.equalization,
        backend: backend_id,
        max_device_bytes_in_flight: device_budget_report.as_ref().map(|r| r.max_device_bytes),
        device_budget_provenance: device_budget_report.as_ref().map(|r| r.provenance),
        device_peak_in_flight: device_peak.map(|p| p.peak_in_flight()),
        gpu_fallback: report.gpu_fallback,
        align_cached: report.align_cached,
        stages: report.stages,
        align_source: report.align_source,
        align_workers: report.align_workers,
        peak_align_workers: report.peak_align_workers,
        align_accounted_bytes: report.align_accounted_bytes,
        align_backend,
    }
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
            " A stacking run writes projects, not images: run \
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
    lines.push(format!(
        "max_bytes_in_flight={} accounted_bytes_in_flight={} budget={:?} backed_off={}",
        report.max_bytes_in_flight,
        report.accounted_bytes_in_flight,
        report.budget_provenance,
        report.backed_off,
    ));
    lines.push(format!("working_space={}", report.working_space));
    lines.push(match &report.equalization {
        None => "equalization=off".to_string(),
        Some(eq) => format!(
            "equalization=on max_gain={} target_stops={} neutral={:.1} target={:.1} clamped={} black={} clipped={} (scales encoded code values, not a linear-light correction)",
            eq.max_gain,
            eq.target_stops,
            eq.neutral,
            eq.target,
            eq.frames.iter().filter(|f| f.clamped).count(),
            eq.frames.iter().filter(|f| f.black).count(),
            eq.frames.iter().filter(|f| f.clipped_samples > 0).count(),
        ),
    });
    lines.push(format!("backend: {}", report.backend));
    if let (
        Some(max_device_bytes_in_flight),
        Some(device_budget_provenance),
        Some(device_peak_in_flight),
    ) = (
        report.max_device_bytes_in_flight,
        report.device_budget_provenance,
        report.device_peak_in_flight,
    ) {
        lines.push(format!(
            "max_device_bytes_in_flight={max_device_bytes_in_flight} device_budget={device_budget_provenance:?} device_peak_in_flight={device_peak_in_flight}"
        ));
    }
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
        worker_fact(report.align_workers),
        worker_fact(report.peak_align_workers),
    ));
    lines.join("\n")
}

/// One `stages:` fact: `"{name}={value:.3}s"` for an entered stage,
/// `"{name}=not-entered"` otherwise (never `0.000s`, which reads as instant).
pub(crate) fn stage_fact(name: &str, value: Option<f64>) -> String {
    match value {
        Some(seconds) => format!("{name}={seconds:.3}s"),
        None => format!("{name}=not-entered"),
    }
}

/// One `workers=`/`peak=` value on the `stages:` line -- the worker count
/// when the align stage actually ran, `"?"` when it did not (a cached,
/// resumed or skipped alignment reports no worker count of its own). Shared
/// by [`render`] and [`crate::align::render`].
pub(crate) fn worker_fact(value: Option<usize>) -> String {
    match value {
        Some(workers) => workers.to_string(),
        None => "?".to_string(),
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
            working_space: "encoded-unknown-transfer",
            // A fictional id on purpose: no algorithm id appears anywhere in
            // `crates/cli/src`, test data included.
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
            max_bytes_in_flight: 1 << 30,
            accounted_bytes_in_flight: 1 << 29,
            budget_provenance: Provenance::Host,
            backed_off: false,
            outputs: vec!["mantis-512.ffp".to_string()],
            wall_seconds: 1.5,
            output_width: 512,
            output_height: 512,
            crop: None,
            description: None,
            align: None,
            equalization: None,
            backend: "cpu".to_string(),
            max_device_bytes_in_flight: None,
            device_budget_provenance: None,
            device_peak_in_flight: None,
            gpu_fallback: None,
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
            align_accounted_bytes: Some(1 << 28),
            align_backend: Some("cpu".to_string()),
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
            "max_bytes_in_flight=1073741824",
            "accounted_bytes_in_flight=536870912",
            "budget=Host",
            "backed_off=false",
            "working_space=encoded-unknown-transfer",
            "wrote: mantis-512.ffp",
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
    fn render_names_the_device_line_on_the_gpu_path() {
        let mut gpu_report = sample_report();
        gpu_report.backend = "NVIDIA GeForce RTX 5070 Ti".to_string();
        gpu_report.max_device_bytes_in_flight = Some(1 << 28);
        gpu_report.device_budget_provenance = Some(Provenance::Host);
        gpu_report.device_peak_in_flight = Some(2);
        let text = render(&gpu_report);
        for needle in [
            "max_device_bytes_in_flight=268435456",
            "device_budget=Host",
            "device_peak_in_flight=2",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }

        // The negative half: the unmodified CPU `sample_report()` renders no
        // device line at all.
        let cpu_text = render(&sample_report());
        assert!(
            !cpu_text.contains("device_peak_in_flight"),
            "CPU report must render no device_peak_in_flight substring: {cpu_text}"
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
        // without the export hint.
        let plain = check_output_dir(Some(Path::new("/definitely/not/here"))).unwrap_err();
        assert!(!plain.contains("export"), "{plain}");
        assert!(check_output_dir(None).is_ok(), "omitting -o is legal");
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
            skipped: Vec::new(),
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
        let guard = tempfile::TempDir::with_prefix("ff-stack-merge-test-").unwrap();
        let dir = guard.path();
        let path = dir.join("manifest.json");
        let existing = groups::RunManifest {
            root: "root".to_string(),
            groups: vec![
                sample_manifest_entry("a", "a.ffp"),
                sample_manifest_entry("b", "b.ffp"),
            ],
        };
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        let mut fresh_entry = sample_manifest_entry("renamed", "a2.ffp");
        fresh_entry.identity = "a".to_string();
        let fresh = vec![fresh_entry];
        let merged = merge_manifest_entries(&path, "root", fresh);
        assert_eq!(merged.len(), 2);
        let a = merged.iter().find(|e| e.identity == "a").unwrap();
        assert_eq!(a.outputs, vec!["a2.ffp".to_string()]);
        let b = merged.iter().find(|e| e.identity == "b").unwrap();
        assert_eq!(b.outputs, vec!["b.ffp".to_string()]);
    }

    #[test]
    fn merge_manifest_entries_uses_empty_identity_as_an_ordinary_key() {
        let guard = tempfile::TempDir::with_prefix("ff-stack-merge-empty-test-").unwrap();
        let path = guard.path().join("manifest.json");
        let mut existing_entry = sample_manifest_entry("old name", "old.ffp");
        existing_entry.identity = String::new();
        let existing = groups::RunManifest {
            root: "root".to_string(),
            groups: vec![existing_entry],
        };
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        let mut fresh_entry = sample_manifest_entry("new name", "new.ffp");
        fresh_entry.identity = String::new();
        let merged = merge_manifest_entries(&path, "root", vec![fresh_entry]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "new name");
        assert_eq!(merged[0].identity, "");
    }

    #[test]
    fn merge_manifest_entries_treats_a_manifest_missing_identity_as_absent() {
        let guard =
            tempfile::TempDir::with_prefix("ff-stack-merge-missing-identity-test-").unwrap();
        let path = guard.path().join("manifest.json");
        std::fs::write(
            &path,
            r#"{"root":"root","groups":[{"name":"old","frame_count":1,"outcome":"completed","outputs":["old.ffp"],"wall_seconds":1.0,"error":null}]}"#,
        )
        .unwrap();

        let merged = merge_manifest_entries(
            &path,
            "root",
            vec![sample_manifest_entry("fresh", "fresh.ffp")],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].identity, "fresh");
    }

    #[test]
    fn merge_manifest_entries_treats_a_mismatched_root_as_absent() {
        let guard = tempfile::TempDir::with_prefix("ff-stack-merge-mismatch-test-").unwrap();
        let dir = guard.path();
        let path = dir.join("manifest.json");
        let existing = groups::RunManifest {
            root: "other-root".to_string(),
            groups: vec![sample_manifest_entry("a", "a.ffp")],
        };
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        let fresh = vec![sample_manifest_entry("b", "b.ffp")];
        let merged = merge_manifest_entries(&path, "root", fresh);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].identity, "b");
    }
}
