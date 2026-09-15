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
    FrameAlignment, ReferencePolicy, StackAlignment, Transform, WarpingReader, align_stack,
    auto_crop,
};
use focusforge_core::fuse::{
    BackendFusionConsumer, FusionConsumer, FusionGeometry,
    description::format_composite_description, laplacian::Laplacian,
};
use focusforge_core::io::{Rect, TiffStack, frame_paths};
use focusforge_core::job::{
    AlignIdentity, FrameFingerprint, JOB_RECORD_FORMAT, Job, JobOutcome, JobRecord,
    RecordingConsumer, Resumable, ResumeStore, StackFingerprint,
};
use focusforge_core::params::{self, ParamValue, ParamValues, Provenance, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileConsumer, TileScheduler, TileSchedulerOptions};
use serde::Serialize;

use crate::align::{AlignOptions, AlignmentEntry, resolve_align_algo};
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
    output: &Path,
    algo_id: Option<&str>,
    zmap: Option<&Path>,
    json: bool,
    set: &SetArgs,
    align_opts: &AlignOptions<'_>,
    job_opts: &JobOptions,
    preset_opts: &PresetOptions<'_>,
    argv: &[String],
    no_gpu: bool,
) -> ExitCode {
    let start = Instant::now();
    let mut renderer = ProgressRenderer::new(ProgressMode::choose(job_opts.quiet));
    let status = StatusSink::new(preset_opts.status, dir.display().to_string(), "stack");
    let signals = match signals::install(true) {
        Ok(signals) => signals,
        Err(msg) => return status.fail(msg),
    };

    // Step 1a: the preset, ahead of everything else -- a malformed file is
    // refused before a frame is opened, and its `algo` key is what step 2
    // falls back to.
    let file = match preset::load(preset_opts.preset) {
        Ok(preset) => preset,
        Err(msg) => return status.fail(msg),
    };
    if preset_opts.dry_run && job_opts.resume {
        return status.fail("--dry-run and --resume are incompatible: a dry run writes nothing, so there is nothing to resume into");
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
            return status.fail(format!(
                "--algo is required (or an `algo` key in the preset); known algorithms: {}",
                ids.join(", ")
            ));
        }
        Some(id) => match registry::find_stack(id) {
            Ok(algo) => algo,
            Err(err) => return status.fail(err),
        },
    };
    status.set_algorithm(algo.id());

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
            return status.fail(
                violations
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    };

    // Check `--output`'s parent directory, still **before**
    // the input directory is read at all -- a run naming both a missing
    // input and a missing output parent must name the output parent, which
    // `refuses_empty_dir_and_unwritable_output` pins. Group-independent:
    // in a multi-group run `-o` is the directory every group writes into.
    // Never `--zmap`'s parent: that path is deliberately left to
    // `FusionConsumer::create`'s own failure, so the `.partial` cleanup is
    // exercised by a real failure rather than a second copy of this check.
    if let Err(msg) = check_output_parent(output) {
        return status.fail(msg);
    }

    // Step 5: frame discovery and the strict uniformity guard.
    let paths = match frame_paths(dir) {
        Ok(paths) => paths,
        Err(err) => return status.fail(err),
    };
    let stack = match TiffStack::open_files(&paths) {
        Ok(stack) => stack,
        Err(err) => return status.fail(err),
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
        return status.fail(detail.unwrap_or_else(|| "non-uniform frame set".to_string()));
    }

    // Step 6: refuse an output that is also an input frame.
    if let Some(name) = colliding_frame(output, &paths, &frame_names) {
        return status.fail(format!(
            "output path {} is also an input frame ({name})",
            output.display()
        ));
    }
    if let Some(zmap) = zmap
        && let Some(name) = colliding_frame(zmap, &paths, &frame_names)
    {
        return status.fail(format!(
            "--zmap path {} is also an input frame ({name})",
            zmap.display()
        ));
    }

    // Step 7: resolve parameters.
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let stack_ctx = StackContext::new(stack.len(), width, height, spp);
    let resolved = match params::resolve(algo.params(), &explicit, &stack_ctx) {
        Ok(resolved) => resolved,
        Err(err) => return status.fail(err),
    };
    let values = params::resolved_values(&resolved);

    // Bridges the resolved pyramid depth into the scheduler geometry via a
    // trait method.
    let levels = algo.tile_levels(&values);

    // Header-only stack fingerprint -- cheap, needed both to peek a
    // resumable ledger below and to build this run's own `JobRecord`.
    let fingerprint = StackFingerprint {
        frames: (0..stack.len())
            .map(|i| FrameFingerprint::from(stack.header(i)))
            .collect(),
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

    // Step 8.5a: *resolving* the alignment algorithm and its parameters is
    // pure -- registry lookup, `--align-set`/`[align]` validation, `auto`
    // evaluation -- and is split out from *running* `align_stack` below so
    // `--write-preset` and `--dry-run` can report the alignment half without
    // paying for the ECC solve (~240 s on the reference stack).
    let align_prepared = if align_opts.align {
        let align_algo = match resolve_align_algo(align_opts.algo.or(file.align_algo.as_deref())) {
            Ok(a) => a,
            Err(msg) => return status.fail(msg),
        };
        let align_explicit = match parse_settings(align_algo.params(), align_opts.set)
            .and_then(|cli| merge_preset(align_algo.params(), &file.align, cli))
        {
            Ok(v) => v,
            Err(violations) => {
                return status.fail(
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
            Err(err) => return status.fail(err),
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
            return status.fail(msg);
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
            Err(err) => return status.fail(err),
        };
        let geometry = scheduler.plan();
        let mut would_write = vec![output.display().to_string()];
        if let Some(zmap) = zmap {
            would_write.push(zmap.display().to_string());
        }
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
        if json {
            match serde_json::to_string_pretty(&plan) {
                Ok(text) => println!("{text}"),
                Err(err) => return status.fail(format!("failed to serialize JSON: {err}")),
            }
        } else {
            println!("{}", render_plan(&plan));
        }
        return ExitCode::SUCCESS;
    }

    if let Some((align_algo, _, align_values, align_params)) = align_prepared {
        let identity = AlignIdentity {
            algorithm: align_algo.id().to_string(),
            params: align_values.clone(),
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
            alignment_result = Some((align_algo, result, align_values, align_params));
        } else {
            let policy = if align_opts.lax {
                focusforge_core::align::FailurePolicy::IdentityFallback
            } else {
                focusforge_core::align::FailurePolicy::Strict
            };
            // Step 9: `align_stack` has no unit boundary -- a scoped
            // watcher thread prints the cancellation notice the moment a
            // SIGINT is observed, so the terminal never looks hung for the
            // whole alignment's wall time, and once it returns, a still-set
            // cancel exits 130 before anything is created (below).
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
                let result = align_stack(align_algo, stack_ctx, &stack, &align_values, policy);
                done.store(true, Ordering::SeqCst);
                watcher.join().expect("align watcher thread must not panic");
                result
            });
            if signals.cancel.load(Ordering::SeqCst) {
                status.cancelled();
                return ExitCode::from(130);
            }
            let result = match solved {
                Ok(r) => r,
                Err(err) => return status.fail(err),
            };
            if !align_opts.lax && result.failure_count() > 0 {
                let names: Vec<String> = result
                    .failed()
                    .map(|f| format!("{} ({:?})", frame_names[f.frame], f.status))
                    .collect();
                return status.fail(format!(
                    "alignment failed for {} frame(s): {}",
                    names.len(),
                    names.join(", ")
                ));
            }
            alignment_result = Some((align_algo, result, align_values, align_params));
        }
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
        Err(err) => return status.fail(err),
    };
    if align_opts.crop && crop_rect_value.is_none() {
        return status.fail(crate::align::empty_crop_refusal());
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
        Err(err) => return status.fail(err),
    };

    // Step 10: `.partial` discipline -- create both writers on partial
    // paths, run, finish, then rename onto the names the user gave. Any
    // failure from here on removes both partials (best effort) and leaves
    // nothing at either named path.
    let composite_partial = partial_path(output);
    let zmap_partial = zmap.map(partial_path);

    let geometry = FusionGeometry {
        width,
        height,
        samples_per_pixel: spp,
        frame_count: stack.len() as u32,
        crop: applied_crop,
        description: description.clone(),
    };

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
        composite_output: composite_partial.clone(),
        zmap_output: zmap_partial.clone(),
    };
    let resume_store = match ResumeStore::open(Path::new(".scratch")) {
        Ok(store) => store,
        Err(err) => return status.fail(err.to_string()),
    };
    let ledger = if job_opts.resume {
        match resume_store.find(&record) {
            Ok(Resumable::None) => {
                return status
                    .fail("no resumable state for this stack under .scratch/resume".to_string());
            }
            Ok(Resumable::Mismatch(mismatch)) => return status.fail(mismatch.to_string()),
            Ok(Resumable::Ledger(ledger)) => ledger,
            Err(err) => return status.fail(err.to_string()),
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
                    return status.fail(err.to_string());
                }
            }
            Ok(_) => {}
            Err(err) => return status.fail(err.to_string()),
        }
        match resume_store.begin(&record) {
            Ok(ledger) => ledger,
            Err(err) => return status.fail(err.to_string()),
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
        cleanup_partials(&composite_partial, zmap_partial.as_deref());
        let _ = ledger.discard();
        return status.fail(err.to_string());
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
            &composite_partial,
            zmap_partial.as_deref(),
            geometry.clone(),
        ) {
            Ok((sink, surviving)) => (sink, Some(surviving)),
            Err(err) => {
                cleanup_partials(&composite_partial, zmap_partial.as_deref());
                let _ = ledger.discard();
                return status.fail(err);
            }
        }
    } else {
        match FusionConsumer::create(
            algo,
            values.clone(),
            &composite_partial,
            zmap_partial.as_deref(),
            geometry,
        ) {
            Ok(sink) => (sink, None),
            Err(err) => {
                cleanup_partials(&composite_partial, zmap_partial.as_deref());
                let _ = ledger.discard();
                return status.fail(err);
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
        &signals,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            // A genuine error: `Job::run` already called `consumer.abort()`
            // on this exact path, so `consumer.finish()` must never be
            // attempted here.
            cleanup_partials(&composite_partial, zmap_partial.as_deref());
            let _ = ledger.discard();
            return status.fail(err);
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
        status.cancelled();
        return ExitCode::from(130);
    }
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
        cleanup_partials(&composite_partial, zmap_partial.as_deref());
        let _ = ledger.discard();
        return status.fail(err);
    }

    if let Err(err) = std::fs::rename(&composite_partial, output) {
        cleanup_partials(&composite_partial, zmap_partial.as_deref());
        let _ = ledger.discard();
        return status.fail(format!(
            "renaming {} to {}: {err}",
            composite_partial.display(),
            output.display()
        ));
    }
    if let (Some(zmap_partial), Some(zmap)) = (&zmap_partial, zmap)
        && let Err(err) = std::fs::rename(zmap_partial, zmap)
    {
        let _ = std::fs::remove_file(zmap_partial);
        let _ = ledger.discard();
        return status.fail(format!(
            "renaming {} to {}: {err}",
            zmap_partial.display(),
            zmap.display()
        ));
    }
    if let Err(err) = ledger.discard() {
        return status.fail(err.to_string());
    }
    renderer.finish("stack complete");

    let mut outputs = vec![output.display().to_string()];
    if let Some(zmap) = zmap {
        outputs.push(zmap.display().to_string());
    }
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
        wall_seconds: start.elapsed().as_secs_f64(),
        output_width: applied_crop.map(|r| r.width).unwrap_or(width),
        output_height: applied_crop.map(|r| r.height).unwrap_or(height),
        crop: applied_crop,
        description,
        align: align_report,
        backend: backend_id,
    };

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return status.fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!("{}", render(&report));
    }

    ExitCode::SUCCESS
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
        let alignment = std::fs::read_to_string(dir.join("alignment.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        return Some(job::PeekedResume { alignment });
    }
    None
}

/// `<path>.partial`: appends the suffix to the path's own file name, in the
/// same directory as `path`, so the eventual rename is same-filesystem and
/// atomic by construction.
fn partial_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    path.with_file_name(name)
}

/// Best-effort cleanup of both partial files. Never fails the caller: a
/// missing partial (the common case, when nothing was created yet) is not
/// itself an error.
fn cleanup_partials(composite_partial: &Path, zmap_partial: Option<&Path>) {
    let _ = std::fs::remove_file(composite_partial);
    if let Some(zmap_partial) = zmap_partial {
        let _ = std::fs::remove_file(zmap_partial);
    }
}

/// Refuses when `output`'s parent does not exist or is not a directory.
/// Never creates it. Without this check, `TiffWriter::create` surfaces a
/// bare `ENOENT` from inside the run, after tiles have been read.
fn check_output_parent(output: &Path) -> Result<(), String> {
    let parent = output.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = match parent {
        Some(parent) => parent,
        None => return Ok(()),
    };
    match std::fs::metadata(parent) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "output directory {} is not a directory",
            parent.display()
        )),
        Err(_) => Err(format!(
            "output directory {} does not exist",
            parent.display()
        )),
    }
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
    lines.join("\n")
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
            outputs: vec!["out.tif".to_string(), "out-zmap.tif".to_string()],
            wall_seconds: 1.5,
            output_width: 512,
            output_height: 512,
            crop: None,
            description: None,
            align: None,
            backend: "cpu".to_string(),
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
            "wrote: out.tif",
            "wrote: out-zmap.tif",
            "wall: 1.500s",
            "backend: cpu",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    #[test]
    fn partial_path_appends_the_suffix_to_the_file_name() {
        assert_eq!(
            partial_path(Path::new("/tmp/out.tif")),
            Path::new("/tmp/out.tif.partial")
        );
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
}
