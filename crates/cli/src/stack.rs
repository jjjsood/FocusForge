// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli stack`: groups the input frames, resolves a registered
//! `StackAlgorithm` and hands each group to `focusforge_core::pipeline::run`,
//! which seals one project per group. Every refusal fires before a frame is
//! opened where the check allows it (algorithm id, `--set` values), and
//! before a tile is read otherwise.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
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
use focusforge_core::params::{self, ParamValue, ParamValues, Provenance, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileConsumer, TileScheduler, TileSchedulerOptions};
use serde::Serialize;

use crate::align::{AlignOptions, AlignmentEntry, resolve_align_algo};
use crate::set::{SetArgs, parse_settings};

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
    /// The host in-flight-byte budget actually in force for this run.
    pub max_bytes_in_flight: u64,
    /// Where that budget came from: `Host` (adaptive), `HostFallback` (the
    /// probe degraded), or `Explicit` (`--max-host-memory` lowered it).
    pub budget_provenance: Provenance,
    /// Whether the runtime back-off shrunk the in-flight limit during this
    /// run.
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
    no_gpu: bool,
    max_host_memory: Option<u64>,
) -> ExitCode {
    let start = Instant::now();

    // Step 2: resolve the algorithm first, before anything else -- an
    // unknown or missing id is refused before the input directory (which
    // may not even exist) is ever touched.
    let algo = match algo_id {
        None => {
            // Stacking algorithms only: this flag cannot accept an
            // alignment id, so offering one as a candidate would send the
            // reader to an error instead of a fix.
            let ids: Vec<&str> = registry::stack_algorithms()
                .iter()
                .map(|a| a.id())
                .collect();
            return crate::fail(format!(
                "--algo is required; known algorithms: {}",
                ids.join(", ")
            ));
        }
        Some(id) => match registry::find_stack(id) {
            Ok(algo) => algo,
            Err(err) => return crate::fail(err),
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

    // Step 3: parse `--set` against that schema. An unknown key or an
    // out-of-range value is refused here, range-checked already by
    // `ParamValue::parse`, with the input directory still unread.
    let explicit = match parse_settings(algo.params(), &set.set) {
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

    // Check `--output`'s parent directory, still **before**
    // the input directory is read at all -- a run naming both a missing
    // input and a missing output parent must name the output parent, which
    // `refuses_empty_dir_and_unwritable_output` pins. Group-independent:
    // in a multi-group run `-o` is the directory every group writes into.
    // Never `--zmap`'s parent: that path is deliberately left to
    // `FusionConsumer::create`'s own failure, so the `.partial` cleanup is
    // exercised by a real failure rather than a second copy of this check.
    if let Err(msg) = check_output_parent(output) {
        return crate::fail(msg);
    }

    // Step 5: frame discovery and the strict uniformity guard.
    let paths = match frame_paths(dir) {
        Ok(paths) => paths,
        Err(err) => return crate::fail(err),
    };
    let stack = match TiffStack::open_files(&paths) {
        Ok(stack) => stack,
        Err(err) => return crate::fail(err),
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
        return crate::fail(detail.unwrap_or_else(|| "non-uniform frame set".to_string()));
    }

    // Step 6: refuse an output that is also an input frame.
    if let Some(name) = colliding_frame(output, &paths, &frame_names) {
        return crate::fail(format!(
            "output path {} is also an input frame ({name})",
            output.display()
        ));
    }
    if let Some(zmap) = zmap
        && let Some(name) = colliding_frame(zmap, &paths, &frame_names)
    {
        return crate::fail(format!(
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
        Err(err) => return crate::fail(err),
    };
    let values = params::resolved_values(&resolved);

    // Bridges the resolved pyramid depth into the scheduler geometry via a
    // trait method.
    let levels = algo.tile_levels(&values);

    // Alignment, between the stack algorithm's own parameters and the
    // scheduler build. Computed *before* any output file exists: a
    // strict-mode failure refuses here.
    let alignment_result: Option<(
        &'static dyn focusforge_core::algorithm::AlignAlgorithm,
        StackAlignment,
        ParamValues,
        Vec<ParamReport>,
    )> = if align_opts.align {
        let align_algo = match resolve_align_algo(align_opts.algo) {
            Ok(a) => a,
            Err(msg) => return crate::fail(msg),
        };
        let align_explicit = match parse_settings(align_algo.params(), align_opts.set) {
            Ok(v) => v,
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
        let align_resolved = match params::resolve(align_algo.params(), &align_explicit, &stack_ctx)
        {
            Ok(r) => r,
            Err(err) => return crate::fail(err),
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
        let policy = if align_opts.lax {
            focusforge_core::align::FailurePolicy::IdentityFallback
        } else {
            focusforge_core::align::FailurePolicy::Strict
        };
        let result = match align_stack(align_algo, stack_ctx, &stack, &align_values, policy) {
            Ok(r) => r,
            Err(err) => return crate::fail(err),
        };
        if !align_opts.lax && result.failure_count() > 0 {
            let names: Vec<String> = result
                .failed()
                .map(|f| format!("{} ({:?})", frame_names[f.frame], f.status))
                .collect();
            return crate::fail(format!(
                "alignment failed for {} frame(s): {}",
                names.len(),
                names.join(", ")
            ));
        }
        Some((align_algo, result, align_values, align_params))
    } else {
        None
    };

    // The alignment used to compute the crop and to drive the tile reader:
    // the real result when aligned, or an all-identity one otherwise --
    // `--no-align` with the crop left on still trims the kernel-radius
    // margin (the two flags are independent), and going through the same
    // `auto_crop` call in both cases is what guarantees the rect matches
    // what `align --json` reports for the same stack.
    let alignment_for_crop: StackAlignment = match &alignment_result {
        Some((_, result, _, _)) => result.clone(),
        None => identity_alignment_for_crop(stack.len()),
    };
    let crop_rect_value = match auto_crop(&alignment_for_crop, (width, height)) {
        Ok(c) => c,
        Err(err) => return crate::fail(err),
    };
    if align_opts.crop && crop_rect_value.is_none() {
        return crate::fail(crate::align::empty_crop_refusal());
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
    let mut options = TileSchedulerOptions::new(".scratch");
    options.levels = levels;
    options.budget = options.budget.capped_to(max_host_memory);
    let reader: &dyn focusforge_core::algorithm::TileReader = match &backend {
        Some(_) => &stack,
        None => &warping_reader,
    };
    let scheduler = match TileScheduler::new(reader, stack_ctx, spp, &options) {
        Ok(scheduler) => scheduler,
        Err(err) => return crate::fail(err),
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

    let sink = match FusionConsumer::create(
        algo,
        values.clone(),
        &composite_partial,
        zmap_partial.as_deref(),
        geometry,
    ) {
        Ok(sink) => sink,
        Err(err) => {
            cleanup_partials(&composite_partial, zmap_partial.as_deref());
            return crate::fail(err);
        }
    };

    let backend_consumer = backend.map(|b| {
        BackendFusionConsumer::new(b, &stack, (width, height), transforms, spp, levels, &sink)
    });
    let consumer: &dyn TileConsumer = match &backend_consumer {
        Some(bc) => bc,
        None => &sink,
    };

    let run_report = match scheduler.run(&NoopPreprocess, consumer) {
        Ok(report) => report,
        Err(err) => {
            cleanup_partials(&composite_partial, zmap_partial.as_deref());
            return crate::fail(err);
        }
    };

    if let Err(err) = sink.finish() {
        cleanup_partials(&composite_partial, zmap_partial.as_deref());
        return crate::fail(err);
    }

    if let Err(err) = std::fs::rename(&composite_partial, output) {
        cleanup_partials(&composite_partial, zmap_partial.as_deref());
        return crate::fail(format!(
            "renaming {} to {}: {err}",
            composite_partial.display(),
            output.display()
        ));
    }
    if let (Some(zmap_partial), Some(zmap)) = (&zmap_partial, zmap)
        && let Err(err) = std::fs::rename(zmap_partial, zmap)
    {
        let _ = std::fs::remove_file(zmap_partial);
        return crate::fail(format!(
            "renaming {} to {}: {err}",
            zmap_partial.display(),
            zmap.display()
        ));
    }

    let mut outputs = vec![output.display().to_string()];
    if let Some(zmap) = zmap {
        outputs.push(zmap.display().to_string());
    }

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
        tiles: run_report.tiles,
        frame_tiles_read: run_report.frame_tiles_read,
        in_flight_limit: run_report.in_flight_limit,
        peak_in_flight: run_report.peak_in_flight,
        max_bytes_in_flight: run_report.max_bytes_in_flight,
        budget_provenance: run_report.budget_provenance,
        backed_off: run_report.final_in_flight_limit < run_report.in_flight_limit,
        outputs,
        wall_seconds: start.elapsed().as_secs_f64(),
        output_width: applied_crop.map(|r| r.width).unwrap_or(width),
        output_height: applied_crop.map(|r| r.height).unwrap_or(height),
        crop: applied_crop,
        description,
        align: align_report,
        backend: backend
            .map(|b| b.info().to_string())
            .unwrap_or_else(|| "cpu".to_string()),
    };

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
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
    for p in &report.params {
        lines.push(format!(
            "{} = {} ({:?})",
            p.id,
            serde_json::to_string(&p.value).unwrap_or_else(|_| "?".to_string()),
            p.provenance,
        ));
    }
    lines.push(format!(
        "tile_size={} levels={} margin={} tiles={} in_flight_limit={}",
        report.tile_size, report.levels, report.margin, report.tiles, report.in_flight_limit,
    ));
    lines.push(format!(
        "max_bytes_in_flight={} budget={:?} backed_off={}",
        report.max_bytes_in_flight, report.budget_provenance, report.backed_off,
    ));
    lines.push(format!("backend: {}", report.backend));
    for output in &report.outputs {
        lines.push(format!("wrote: {output}"));
    }
    lines.push(format!("wall: {:.3}s", report.wall_seconds));
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
            max_bytes_in_flight: 1 << 30,
            budget_provenance: Provenance::Host,
            backed_off: false,
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
            "max_bytes_in_flight=1073741824",
            "budget=Host",
            "backed_off=false",
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
