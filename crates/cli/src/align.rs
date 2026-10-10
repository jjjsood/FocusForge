// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli align`: computes and reports the per-frame transforms
//! for a stack -- human text by default, a `--json` document on
//! request, and an optional per-frame warped-TIFF dump for a visual check
//! before any fusion runs. Every number here comes from
//! `focusforge_core::align`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use focusforge_core::algorithm::{AlignAlgorithm, StackAlgorithm, TileReader};
use focusforge_core::align::{
    AlignDriverOptions, FailurePolicy, ReferencePolicy, StackAlignment, Transform, WarpingReader,
    align_stack_with, auto_crop,
};
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::{FrameReader, TiffSpec, TiffWriter, frame_paths, open_frame_set};
use focusforge_core::params;
use focusforge_core::registry;
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{Rect, TileSchedulerOptions};
use focusforge_core::timing::{Stage, StageTimer};
use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::Serialize;

use crate::preset::{self, PresetOptions};
use crate::progress::{ProgressMode, ProgressRenderer};
use crate::set::SetArgs;
use crate::set::{merge_preset, parse_settings};
use crate::stack::ParamReport;
use crate::status::StatusSink;

/// The alignment options `stack` passes down, bundled to keep
/// `stack::run`'s argument count down.
pub struct AlignOptions<'a> {
    pub align: bool,
    pub crop: bool,
    pub lax: bool,
    pub algo: Option<&'a str>,
    pub set: &'a [(String, String)],
}

/// Resolves the align algorithm `id` names, or -- when `id` is `None` --
/// `registry::default_align()`, which keeps `crates/cli/src` free of every
/// algorithm id. Test-only registrations never count toward that default;
/// ambiguity among the rest is refused naming every candidate.
pub(crate) fn resolve_align_algo(id: Option<&str>) -> Result<&'static dyn AlignAlgorithm, String> {
    match id {
        Some(id) => registry::find_align(id),
        None => registry::default_align(),
    }
    .map_err(|e| e.to_string())
}

/// The `align --json` document.
#[derive(Serialize)]
pub struct AlignReport {
    pub input: String,
    pub frame_count: usize,
    pub frames: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub algorithm: String,
    pub display_name: String,
    pub params: Vec<ParamReport>,
    pub reference: usize,
    pub reference_policy: ReferencePolicy,
    /// `"strict"` or `"lax"` -- a plain string, not `FailurePolicy` itself:
    /// only the seven types the schema needs derive `Serialize`, no more.
    pub failure_policy: String,
    pub failure_count: usize,
    pub alignments: Vec<AlignmentEntry>,
    pub crop: Option<Rect>,
    pub warped_dir: Option<String>,
    pub wall_seconds: f64,
    /// Wall-clock stage breakdown -- `setup`/`align`/`finalize` as seconds,
    /// `warp_write` a number only when `--write-warped` ran, `fuse`
    /// always `null` (`align` never fuses). Every figure is wall clock,
    /// never CPU time; the non-null stages sum exactly to `wall_seconds` --
    /// see [`focusforge_core::timing::StageTimings`]'s own rustdoc for the
    /// full contract. Fields are only ever added.
    pub stages: focusforge_core::timing::StageTimings,
    /// Always `"computed"`: `align` has no `--resume`, no stage cache and
    /// no `--no-align`. Present so `align --json` and `stack --json` report
    /// their `align` stage the same way.
    pub align_source: String,
    /// The anchor arm's worker bound and the peak it reached: always `Some`
    /// here. `Option` so the key has one type in `align --json` and
    /// `stack --json`.
    pub align_workers: Option<usize>,
    pub peak_align_workers: Option<usize>,
    /// The bytes the alignment accounted for: the shared reference data plus
    /// one registration per worker. An explicit budget below one
    /// registration still runs one worker, so this can exceed the budget.
    pub align_accounted_bytes: u64,
    /// `"cpu"`, or the adapter's display when at least one alignment level
    /// ran on it.
    pub align_backend: String,
}

/// One frame's row in [`AlignReport::alignments`]: `#[serde(flatten)]`ed
/// from [`focusforge_core::align::FrameAlignment`] (`frame`, `transform`,
/// `status`, `iterations`, `residual`, `fallback`), plus the frame's file
/// name and the four `similarity_parts` conveniences.
#[derive(Serialize)]
pub struct AlignmentEntry {
    pub name: String,
    #[serde(flatten)]
    pub base: focusforge_core::align::FrameAlignment,
    pub scale: f64,
    pub rotation_deg: f64,
    pub tx: f64,
    pub ty: f64,
}

pub(crate) fn alignment_entries(
    alignment: &StackAlignment,
    frame_names: &[String],
) -> Vec<AlignmentEntry> {
    alignment
        .frames
        .iter()
        .map(|f| {
            let (scale, radians, tx, ty) = f.transform.similarity_parts();
            AlignmentEntry {
                name: frame_names[f.frame].clone(),
                base: *f,
                scale,
                rotation_deg: radians.to_degrees(),
                tx,
                ty,
            }
        })
        .collect()
}

/// Runs the subcommand.
///
/// `algo_id: None` resolves through [`resolve_align_algo`], never a
/// hardcoded id. `--strict` selects [`FailurePolicy::Strict`]; its absence
/// selects [`FailurePolicy::IdentityFallback`], the default.
///
/// `no_progress` suppresses the one stage line this subcommand prints.
/// `align` installs **no** SIGINT handler at all: it has no
/// fuse-tile units, so a handler here would have nothing to act on and would
/// only make the process look hung for up to the alignment's own wall time
/// after a first Ctrl-C. The default disposition (immediate kill) is left
/// untouched.
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    algo_id: Option<&str>,
    json: bool,
    lax: bool,
    write_warped: Option<&Path>,
    set: &SetArgs,
    max_host_memory: Option<u64>,
    no_progress: bool,
    quiet: bool,
    preset_opts: &PresetOptions<'_>,
    no_gpu: bool,
    max_vram: Option<u64>,
) -> ExitCode {
    let start = Instant::now();
    // One monotonic timer shares `start` with `wall_seconds` below.
    let mut timer = StageTimer::from_start(start);
    timer.enter(Stage::Setup);
    let mut renderer = ProgressRenderer::new(ProgressMode::choose(no_progress));
    let sink = StatusSink::new(preset_opts.status, dir.display().to_string(), "align");

    // One preset file serves both subcommands: `align` reads `align-algo`
    // and `[align]` only. The unused half is not silently dropped -- a
    // `[stack]`/`[align]` typo would otherwise be a quiet no-op.
    let file = match preset::load(preset_opts.preset) {
        Ok(file) => file,
        Err(msg) => return sink.fail(msg),
    };
    if !file.stack.is_empty() {
        eprintln!("preset: [stack] table not used by align");
    }
    if file.algo.is_some() {
        eprintln!("preset: algo key not used by align (align reads align-algo)");
    }

    let algo = match resolve_align_algo(algo_id.or(file.align_algo.as_deref())) {
        Ok(algo) => algo,
        Err(msg) => return sink.fail(msg),
    };
    sink.set_algorithm(algo.id());

    let explicit = match parse_settings(algo.params(), &set.set)
        .and_then(|cli| merge_preset(algo.params(), &file.align, cli))
    {
        Ok(values) => values,
        Err(violations) => {
            return sink.fail(
                violations
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    };

    if let Some(path) = preset_opts.write_preset {
        let written = preset::Preset {
            algo: None,
            align_algo: Some(algo.id().to_string()),
            stack: Default::default(),
            align: explicit.clone(),
        };
        if let Err(msg) = preset::write(&written, path) {
            return sink.fail(msg);
        }
    }

    let paths = match frame_paths(dir) {
        Ok(paths) => paths,
        Err(err) => return sink.fail(err),
    };
    let budget = TileSchedulerOptions::new(".scratch")
        .budget
        .capped_to(max_host_memory);
    let opened = match open_frame_set(&paths, budget) {
        Ok(opened) => opened,
        Err(err) => return sink.fail(err),
    };
    let frame_reader: &dyn FrameReader = &*opened.reader;
    let frame_names: Vec<String> = paths
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    let stack_ctx = frame_reader.stack_context();
    let (width, height) = (stack_ctx.width, stack_ctx.height);
    let spp = stack_ctx.samples_per_pixel;
    let resolved = match params::resolve(algo.params(), &explicit, &stack_ctx) {
        Ok(resolved) => resolved,
        Err(err) => return sink.fail(err),
    };
    let values = params::resolved_values(&resolved);

    let policy = if lax {
        FailurePolicy::IdentityFallback
    } else {
        FailurePolicy::Strict
    };
    let align_choice = crate::resolve_align_backend(algo.id(), no_gpu, max_vram, quiet);
    timer.mark(Stage::Setup);
    timer.enter(Stage::Align);
    renderer.stage(&format!("align {} frames", frame_reader.len()));
    let run = match align_stack_with(
        algo,
        stack_ctx,
        frame_reader,
        &values,
        policy,
        &AlignDriverOptions {
            budget: opened.scheduler_budget,
            align_backend: align_choice.backend,
            ..Default::default()
        },
    ) {
        Ok(run) => run,
        Err(err) => return sink.fail(err),
    };
    if let Some(reason) = &run.rerun_reason {
        eprintln!("{reason}; the align stage reran on the CPU");
    }
    let alignment = run.alignment;
    let crop = match auto_crop(&alignment, (width, height)) {
        Ok(crop) => crop,
        Err(err) => return sink.fail(err),
    };
    timer.mark(Stage::Align);

    if let Some(write_warped) = write_warped {
        timer.enter(Stage::WarpWrite);
        // The fuse backend is resolved only for `--write-warped`: the
        // alignment's own accelerator was chosen above.
        let backend = crate::resolve_backend(
            focusforge_core::fuse::laplacian::Laplacian.id(),
            no_gpu,
            max_vram,
            quiet,
        )
        .backend;
        if let Err(msg) = write_warped_frames(
            write_warped,
            frame_reader,
            &paths,
            &frame_names,
            &alignment,
            width,
            height,
            spp,
            backend,
        ) {
            return sink.fail(msg);
        }
        // Marked only inside this arm: no `--write-warped`, no mark, so the
        // field stays `null` -- "not requested" is never confused with
        // "instant".
        timer.mark(Stage::WarpWrite);
    }

    timer.mark(Stage::Finalize);
    let wall_seconds = timer.total_seconds();
    let stages = timer.finish();

    let report = AlignReport {
        input: dir.display().to_string(),
        frame_count: frame_reader.len(),
        frames: frame_names.clone(),
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
        reference: alignment.reference,
        reference_policy: alignment.policy,
        failure_policy: if lax { "lax" } else { "strict" }.to_string(),
        failure_count: alignment.failure_count(),
        alignments: alignment_entries(&alignment, &frame_names),
        crop,
        warped_dir: write_warped.map(|p| p.display().to_string()),
        wall_seconds,
        stages,
        // `align` has no `--resume`, no stage cache and no `--no-align`:
        // every run reaches this line by computing.
        align_source: "computed".to_string(),
        align_workers: Some(run.worker_limit),
        peak_align_workers: Some(run.peak_workers),
        align_accounted_bytes: run.accounted_bytes,
        align_backend: align_choice.report_name(run.backend_used),
    };

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return sink.fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!("{}", render(&report));
    }

    if !lax && alignment.failure_count() > 0 {
        let names: Vec<String> = alignment
            .failed()
            .map(|f| format!("{} ({:?})", frame_names[f.frame], f.status))
            .collect();
        return sink.fail(format!(
            "alignment failed for {} frame(s): {}",
            names.len(),
            names.join(", ")
        ));
    }
    if lax && alignment.failure_count() > 0 {
        let warning = format!(
            "warning: {} frame(s) failed to register and fell back to identity",
            alignment.failure_count()
        );
        eprintln!("{warning}");
        tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "{warning}");
    }

    // `outputs` carries the `--write-warped` directory when one was given,
    // and is empty otherwise: a plain `align` computes numbers, not files.
    sink.completed(
        &write_warped
            .map(|p| vec![p.display().to_string()])
            .unwrap_or_default(),
    );
    ExitCode::SUCCESS
}

/// Writes one warped TIFF per frame into `dir`, at **source** dimensions,
/// uncropped -- the crop is reported, not applied, on this path, so the
/// uncovered rim stays visible for the manual check.
///
/// With no backend, each frame streams in full-width bands of
/// [`DEFAULT_TILE_SIZE`] rows through a [`WarpingReader`]. With a backend,
/// bands are also cut into [`DEFAULT_TILE_SIZE`]-wide chunks: a full-width
/// band at the reference stack's 8192 px needs `8192 * 512 * 24 = 100.7 MB`,
/// close to the `max_storage_buffer_binding_size = 128 MiB` floor this crate
/// runs at; chunking keeps it at 6.3 MB.
#[allow(clippy::too_many_arguments)]
fn write_warped_frames(
    dir: &Path,
    stack: &dyn TileReader,
    paths: &[PathBuf],
    frame_names: &[String],
    alignment: &StackAlignment,
    width: u32,
    height: u32,
    spp: u32,
    backend: Option<&dyn TileBackend>,
) -> Result<(), String> {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "--write-warped target {} is not a directory",
                dir.display()
            ));
        }
        Err(_) => {
            return Err(format!(
                "--write-warped target {} does not exist",
                dir.display()
            ));
        }
    }
    for name in frame_names {
        let candidate = dir.join(name);
        if let Some(collide) = crate::stack::colliding_frame(&candidate, paths, frame_names) {
            return Err(format!(
                "--write-warped target {} would overwrite input frame {collide}",
                dir.display()
            ));
        }
    }

    let transforms: Vec<_> = alignment.frames.iter().map(|f| f.transform).collect();

    match backend {
        None => {
            let reader = WarpingReader::new(stack, (width, height), spp, transforms);
            for (i, name) in frame_names.iter().enumerate() {
                let out_path = dir.join(name);
                let mut writer = TiffWriter::create(
                    &out_path,
                    TiffSpec {
                        width,
                        height,
                        samples_per_pixel: spp,
                        description: None,
                    },
                )
                .map_err(|e| e.to_string())?;

                let mut y = 0u32;
                while y < height {
                    let band_height = DEFAULT_TILE_SIZE.min(height - y);
                    let rect = Rect {
                        x: 0,
                        y,
                        width,
                        height: band_height,
                    };
                    let mut band = vec![0u16; width as usize * band_height as usize * spp as usize];
                    reader
                        .read_tile_into(i, rect, &mut band)
                        .map_err(|e| e.to_string())?;
                    writer.write_tile(rect, &band).map_err(|e| e.to_string())?;
                    y += band_height;
                }
                writer.finish().map_err(|e| e.to_string())?;
            }
        }
        Some(backend) => {
            for (i, name) in frame_names.iter().enumerate() {
                let out_path = dir.join(name);
                let mut writer = TiffWriter::create(
                    &out_path,
                    TiffSpec {
                        width,
                        height,
                        samples_per_pixel: spp,
                        description: None,
                    },
                )
                .map_err(|e| e.to_string())?;

                let t = &transforms[i];
                let mut y = 0u32;
                while y < height {
                    let band_height = DEFAULT_TILE_SIZE.min(height - y);
                    let mut x = 0u32;
                    while x < width {
                        let chunk_width = DEFAULT_TILE_SIZE.min(width - x);
                        let rect = Rect {
                            x,
                            y,
                            width: chunk_width,
                            height: band_height,
                        };
                        let samples = if *t == Transform::identity() {
                            let mut buf =
                                vec![
                                    0u16;
                                    chunk_width as usize * band_height as usize * spp as usize
                                ];
                            stack
                                .read_tile_into(i, rect, &mut buf)
                                .map_err(|e| e.to_string())?;
                            buf
                        } else {
                            backend
                                .warp_tile(stack, i, (width, height), spp, rect, t)
                                .map_err(|e| e.to_string())?
                                .tile
                                .samples
                        };
                        writer
                            .write_tile(rect, &samples)
                            .map_err(|e| e.to_string())?;
                        x += chunk_width;
                    }
                    y += band_height;
                }
                writer.finish().map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

fn status_text(status: &focusforge_core::algorithm::AlignStatus) -> String {
    match status {
        focusforge_core::algorithm::AlignStatus::Converged => "converged".to_string(),
        focusforge_core::algorithm::AlignStatus::HitCap => "hit-cap".to_string(),
        focusforge_core::algorithm::AlignStatus::Stalled => "stalled".to_string(),
        focusforge_core::algorithm::AlignStatus::Diverged(reason) => {
            format!("diverged({reason:?})")
        }
        // `AlignStatus` is `#[non_exhaustive]`: a future variant renders as
        // its own `Debug` text rather than failing to compile here.
        other => format!("{other:?}"),
    }
}

/// Human rendering of one [`AlignReport`]: one row per frame, unit-testable
/// without capturing stdout. `rho` is labelled in a header line so it is
/// never mistaken for a pixel distance.
pub(crate) fn render(report: &AlignReport) -> String {
    let mut lines = vec![
        format!("{} ({} frames)", report.input, report.frame_count),
        format!(
            "{} - {} (reference={} policy={:?} failure_policy={} failures={})",
            report.algorithm,
            report.display_name,
            report.reference,
            report.reference_policy,
            report.failure_policy,
            report.failure_count,
        ),
        "rho = ECC correlation in [-1, 1], not a pixel distance".to_string(),
    ];
    for entry in &report.alignments {
        let mut line = format!(
            "{:9} tx={:+.2} ty={:+.2} scale={:.6} rot={:+.4}deg  {} iters={} rho={:.4}",
            entry.name,
            entry.tx,
            entry.ty,
            entry.scale,
            entry.rotation_deg,
            status_text(&entry.base.status),
            entry.base.iterations,
            entry.base.residual,
        );
        if entry.base.frame == report.reference {
            line.push_str(" (reference)");
        }
        if entry.base.fallback {
            line.push_str(" [fallback]");
        }
        lines.push(line);
    }
    match report.crop {
        Some(rect) => lines.push(format!(
            "auto-crop: {},{} {}x{}",
            rect.x, rect.y, rect.width, rect.height
        )),
        None => lines.push("auto-crop: empty (frame footprints do not intersect)".to_string()),
    }
    // The total rides the `stages:` line as `(wall …)`, and `workers=`/`peak=`
    // ride it too: that line already varies run to run, so every other stdout
    // line stays deterministic.
    lines.push(format!(
        "stages: {} {} {} {} workers={} peak={} (wall {:.3}s)",
        crate::stack::stage_fact("setup", report.stages.setup),
        crate::stack::stage_fact("align", report.stages.align),
        crate::stack::stage_fact("warp_write", report.stages.warp_write),
        crate::stack::stage_fact("finalize", report.stages.finalize),
        crate::stack::worker_fact(report.align_workers),
        crate::stack::worker_fact(report.peak_align_workers),
        report.wall_seconds,
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_frame_renders_stalled() {
        use focusforge_core::algorithm::AlignStatus;
        assert_eq!(status_text(&AlignStatus::Stalled), "stalled");
    }

    #[test]
    fn resolve_align_algo_without_an_id_skips_test_only_registrations() {
        let algo = resolve_align_algo(None).expect("one real alignment algorithm is registered");
        assert!(!algo.test_only(), "{} is test-only", algo.id());
        assert_eq!(algo.id(), registry::default_align().unwrap().id());
    }
}
