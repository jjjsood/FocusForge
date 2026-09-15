// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli`: `info`, the command that makes a claim about a TIFF
//! file or directory checkable -- byte order, strip layout and whether the
//! mmap fast path applies, as human text or as `--json`; and `algorithms`,
//! which lists every registered algorithm or prints one named algorithm's
//! full parameter schema, validating `--set key=value` against it. `info`
//! reads headers only: never `TiffStack`, never a tile, never a pixel.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use focusforge_core::io::{ByteOrder, TiffError, TiffHeader, frame_paths};
use serde::Serialize;

mod algorithms;
mod align;
mod export;
#[cfg(feature = "gpu")]
mod gpu;
mod groups;
mod job;
mod preset;
mod progress;
mod review;
mod set;
mod signals;
mod stack;
mod status;

/// Resolves whether this run takes the GPU path or the CPU one:
/// `#[cfg(feature = "gpu")]` delegates to [`gpu::resolve_backend`]; with the
/// feature off, always `None` -- `stack`/`align` behave exactly as phases 3
/// and 4 left them, with no GPU probing at all. A crate-root `fn`, not
/// `pub(crate)`: `stack` and `align` are descendant modules of this one, so
/// a private item here is already visible to them (the same visibility
/// `crate::fail` already relies on).
#[cfg(feature = "gpu")]
fn resolve_backend(no_gpu: bool) -> Option<&'static dyn focusforge_core::gpu::TileBackend> {
    gpu::resolve_backend(no_gpu)
}

#[cfg(not(feature = "gpu"))]
fn resolve_backend(_no_gpu: bool) -> Option<&'static dyn focusforge_core::gpu::TileBackend> {
    None
}

/// FocusForge command-line interface.
#[derive(Parser)]
#[command(
    name = "focusforge-cli",
    version,
    about = "FocusForge command-line interface"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print TIFF metadata for a file, or for every TIFF in a directory.
    Info {
        /// Path to a file or directory.
        path: PathBuf,
        /// Emit the frozen JSON schema instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// List every registered algorithm, or print one named algorithm's full
    /// parameter schema.
    Algorithms {
        /// Algorithm id. Omit to list every registered algorithm.
        id: Option<String>,
        /// Emit the frozen JSON schema instead of human-readable text.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        set: set::SetArgs,
    },
    /// Fuse every TIFF frame of a directory into one **project** per
    /// detected stack, through a registered stacking algorithm. A run
    /// prepares projects for review; it does not export images -- run
    /// `focusforge-cli export` for that. `--algo` is not marked required in
    /// clap's sense even though it is required in practice -- see
    /// `stack::run`'s doc comment for why.
    Stack {
        /// Directory of TIFF frames to fuse (non-recursive, descending into
        /// `<dir>/frames` when that subdirectory exists).
        dir: PathBuf,
        /// Directory to collect this run's projects into. Omit to write
        /// each project beside its own frames.
        #[arg(short = 'o', long = "output", value_name = "DIR")]
        output: Option<PathBuf>,
        /// Registered stacking algorithm id. Required, but never a clap
        /// `required` field.
        #[arg(long)]
        algo: Option<String>,
        /// Gone: a project carries its own Z-Map. Still **declared**, and
        /// hidden, purely so `stack::run` can refuse it by name and point
        /// at `focusforge-cli export` -- removing it outright would leave
        /// clap printing "unexpected argument '--zmap'", which tells a user
        /// nothing about where their Z-Map went.
        #[arg(long, hide = true)]
        zmap: Option<PathBuf>,
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        set: set::SetArgs,
        #[command(flatten)]
        group_args: groups::GroupArgs,
        /// Skip alignment: the tile reader stays the raw stack and the
        /// output geometry stays full-frame. Defaults to `true`
        /// (alignment on); `--no-align` is what turns it off (clap
        /// `ArgAction::SetFalse`, no `default_value` -- clap-rs/clap#4370).
        #[arg(long = "no-align", action = clap::ArgAction::SetFalse)]
        align: bool,
        /// Skip the auto-crop: the output stays at the full frame extent
        /// even when alignment is on. Defaults to `true` (crop applied);
        /// `--no-crop` is what turns it off.
        #[arg(long = "no-crop", action = clap::ArgAction::SetFalse)]
        crop: bool,
        /// Soften alignment failures to an identity fallback instead of
        /// refusing the run. Meaningless with `--no-align`.
        #[arg(long)]
        lax: bool,
        /// Alignment algorithm id. Optional: defaults to the sole
        /// registered alignment algorithm, refusing (and listing the
        /// choices) when more than one is registered.
        #[arg(long = "align-algo")]
        align_algo: Option<String>,
        #[command(flatten)]
        align_set: set::AlignSetArgs,
        /// Resume a previously cancelled run of this same stack, found by
        /// stack fingerprint under `.scratch/resume/` -- no argument.
        /// Refuses at exit 1 when no matching scratch state exists or it
        /// disagrees with this invocation's inputs.
        #[arg(long)]
        resume: bool,
        /// Print no progress at all.
        #[arg(long)]
        quiet: bool,
        /// Override the scheduler's outer tile size, in pixels. Omit to use
        /// the scheduler's own default.
        #[arg(long = "tile-size", value_name = "PX")]
        tile_size: Option<u32>,
        /// Override the scheduler's in-flight memory budget, in bytes. Omit
        /// to use the scheduler's own default.
        #[arg(long = "max-bytes-in-flight", value_name = "BYTES")]
        max_bytes_in_flight: Option<usize>,
        /// Read parameter values from a TOML preset: the `algo` and
        /// `align-algo` keys plus the `[stack]` and `[align]` value
        /// tables. A flag always beats the file -- `--set`/`--align-set`
        /// override the tables, `--algo`/`--align-algo` override the keys.
        #[arg(long, value_name = "PATH")]
        preset: Option<PathBuf>,
        /// Write the preset that reproduces this run to this path: the
        /// algorithm ids plus the values you actually chose. Values left
        /// to an `auto` closure stay live and are never frozen into the
        /// file.
        #[arg(long = "write-preset", value_name = "PATH")]
        write_preset: Option<PathBuf>,
        /// Append one NDJSON status record for this stack to this path:
        /// identity, outcome, outputs and the error, if any. Every
        /// terminal path writes exactly one record.
        #[arg(long, value_name = "PATH")]
        status: Option<PathBuf>,
        /// Resolve everything, print what would happen -- parameters with
        /// their provenance, tile geometry, output paths -- and exit 0
        /// without computing or writing anything. Stops *before*
        /// alignment, so it reports no crop rect and no output extent.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Force the CPU path even when a usable GPU adapter is available.
        /// Only present when the `gpu` feature is compiled in -- with it
        /// off, this flag does not exist at all, not even as a no-op.
        /// Defaults to `true` (the GPU path is used automatically);
        /// `--no-gpu` is what turns it off (clap `ArgAction::SetFalse`, no
        /// `default_value` -- clap-rs/clap#4370).
        #[cfg(feature = "gpu")]
        #[arg(long = "no-gpu", action = clap::ArgAction::SetFalse)]
        gpu: bool,
    },
    /// Turn a project into an image file: the composite with `-o`, the
    /// Z-Map with `--zmap`, either or both in one invocation. At least one
    /// of the two is required. This is the only thing in the workspace that
    /// writes a TIFF from a stack result.
    Export {
        /// The `.ffproj` project to read.
        project: PathBuf,
        /// Write the composite to this path.
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: Option<PathBuf>,
        /// Write the Z-Map to this path.
        #[arg(long, value_name = "FILE")]
        zmap: Option<PathBuf>,
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Compute and report the per-frame transforms for a stack: readable
    /// text by default, a frozen `--json` document on request, and an
    /// optional per-frame warped-TIFF dump for a visual check.
    Align {
        /// Directory of TIFF frames to align (non-recursive, descending
        /// into `<dir>/frames` when that subdirectory exists).
        dir: PathBuf,
        /// Registered alignment algorithm id. Optional: defaults to the
        /// sole registered alignment algorithm.
        #[arg(long)]
        algo: Option<String>,
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
        /// Soften a failed frame to an identity fallback and exit 0,
        /// reporting the failure count on stderr, instead of refusing the
        /// run. Strict (the default) is not a flag -- `--lax` has no
        /// opposite to be confused with, unlike a `--no-strict` that would
        /// imply a nonexistent `--strict`.
        #[arg(long)]
        lax: bool,
        /// Write one warped TIFF per frame into this directory, at source
        /// dimensions, uncropped.
        #[arg(long = "write-warped", value_name = "DIR")]
        write_warped: Option<PathBuf>,
        #[command(flatten)]
        set: set::SetArgs,
        /// Print no progress at all. `align` has no fuse-tile units to
        /// count: this suppresses only the one stage line `align`
        /// otherwise prints.
        #[arg(long)]
        quiet: bool,
        /// Read parameter values from a TOML preset. `align` uses only
        /// `align-algo` and the `[align]` table; a non-empty `[stack]`
        /// table or `algo` key is unused here and says so on stderr, so
        /// one preset file serves both subcommands.
        #[arg(long, value_name = "PATH")]
        preset: Option<PathBuf>,
        /// Write the preset that reproduces this run to this path:
        /// `align-algo` plus the `[align]` values you actually chose.
        #[arg(long = "write-preset", value_name = "PATH")]
        write_preset: Option<PathBuf>,
        /// Append one NDJSON status record for this run to this path.
        #[arg(long, value_name = "PATH")]
        status: Option<PathBuf>,
        /// Force the CPU path even when a usable GPU adapter is available.
        /// Only present when the `gpu` feature is compiled in. Meaningful
        /// only with `--write-warped`: plain `align` has nothing on the
        /// GPU-shaped path (its ECC solver is CPU-only).
        #[cfg(feature = "gpu")]
        #[arg(long = "no-gpu", action = clap::ArgAction::SetFalse)]
        gpu: bool,
    },
    /// Report one record per result found under a directory of projects, or
    /// a batch run's output directory: identity, outcome, sharpness ratio
    /// and Z-Map spread, with a failed stack a visible record rather than a
    /// missing file. Reads a run manifest when one is present, otherwise a
    /// non-recursive listing of `*.ffproj` files; never groups frames
    /// itself and writes nothing.
    Review {
        /// Directory to review: a run's `-o` output, or a plain directory of
        /// `*.ffproj` projects.
        dir: PathBuf,
        /// Emit one wrapped JSON document instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Info { path, json } => info(&path, json),
        Command::Algorithms { id, json, set } => algorithms::run(id.as_deref(), json, &set),
        Command::Stack {
            dir,
            output,
            algo,
            zmap,
            json,
            set,
            group_args,
            align,
            crop,
            lax,
            align_algo,
            align_set,
            resume,
            quiet,
            tile_size,
            max_bytes_in_flight,
            preset,
            write_preset,
            status,
            dry_run,
            #[cfg(feature = "gpu")]
            gpu,
        } => {
            #[cfg(feature = "gpu")]
            let no_gpu = !gpu;
            #[cfg(not(feature = "gpu"))]
            let no_gpu = false;
            stack::run(
                &dir,
                output.as_deref(),
                algo.as_deref(),
                zmap.as_deref(),
                json,
                &set,
                &align::AlignOptions {
                    align,
                    crop,
                    lax,
                    algo: align_algo.as_deref(),
                    set: &align_set.align_set,
                },
                &job::JobOptions {
                    resume,
                    quiet,
                    tile_size,
                    max_bytes_in_flight,
                },
                &preset::PresetOptions {
                    preset: preset.as_deref(),
                    write_preset: write_preset.as_deref(),
                    status: status.as_deref(),
                    dry_run,
                },
                &group_args,
                &std::env::args().collect::<Vec<_>>(),
                no_gpu,
            )
        }
        Command::Export {
            project,
            output,
            zmap,
            json,
        } => export::run(&project, output.as_deref(), zmap.as_deref(), json),
        Command::Align {
            dir,
            algo,
            json,
            lax,
            write_warped,
            set,
            quiet,
            preset,
            write_preset,
            status,
            #[cfg(feature = "gpu")]
            gpu,
        } => {
            #[cfg(feature = "gpu")]
            let no_gpu = !gpu;
            #[cfg(not(feature = "gpu"))]
            let no_gpu = false;
            align::run(
                &dir,
                algo.as_deref(),
                json,
                lax,
                write_warped.as_deref(),
                &set,
                quiet,
                &preset::PresetOptions {
                    preset: preset.as_deref(),
                    write_preset: write_preset.as_deref(),
                    status: status.as_deref(),
                    dry_run: false,
                },
                no_gpu,
            )
        }
        Command::Review { dir, json } => review::run(&dir, json),
    }
}

/// One directory member's metadata, or the reason it has none.
#[derive(Serialize)]
struct FileReport {
    name: String,
    path: String,
    width: Option<u32>,
    height: Option<u32>,
    bits_per_sample: Option<Vec<u16>>,
    samples_per_pixel: Option<u32>,
    byte_order: Option<String>,
    compression: Option<u16>,
    photometric: Option<u16>,
    planar_config: Option<u16>,
    rows_per_strip: Option<u32>,
    strip_count: Option<usize>,
    file_size: Option<u64>,
    description: Option<String>,
    mmap_fast_path: Option<bool>,
    refusal: Option<String>,
    error: Option<String>,
}

/// The frozen `info --json` schema. One shape for both a file and a
/// directory: a single-file invocation is `is_directory: false`,
/// `frame_count: 1`, a one-element `files`.
#[derive(Serialize)]
struct InfoReport {
    path: String,
    is_directory: bool,
    frame_count: usize,
    total_bytes: u64,
    unreadable_count: usize,
    uniform_layout: bool,
    uniform_detail: Option<String>,
    files: Vec<FileReport>,
}

fn byte_order_str(order: ByteOrder) -> &'static str {
    match order {
        ByteOrder::Little => "II",
        ByteOrder::Big => "MM",
    }
}

/// `info`'s two report-don't-refuse exceptions: a single named file that
/// fails to parse structurally, and a directory with zero TIFFs. Both exit
/// 1 with `error: <TiffError>`; every other case -- a member of a listing,
/// a policy refusal like an unsupported compression -- stays exit 0,
/// refusal or failure carried as data instead.
fn info(path: &Path, json: bool) -> ExitCode {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => return fail(format!("cannot read {}: {err}", path.display())),
    };

    let is_directory = metadata.is_dir();
    let paths: Vec<PathBuf> = if is_directory {
        match frame_paths(path) {
            Ok(paths) => paths,
            Err(err) => return fail(err),
        }
    } else {
        if let Err(err) = TiffHeader::open(path) {
            return fail(err);
        }
        vec![path.to_path_buf()]
    };

    let report = build_report(path, is_directory, &paths);

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        print_human(&report);
    }

    ExitCode::SUCCESS
}

/// Header-only probe of every path in `paths`, never `TiffStack` and never
/// a tile: this is what lets an unreadable or policy-refused member end up
/// as one report line instead of failing the whole command.
fn build_report(display_path: &Path, is_directory: bool, paths: &[PathBuf]) -> InfoReport {
    struct Parsed {
        path: PathBuf,
        name: String,
        header: Result<TiffHeader, TiffError>,
    }

    let parsed: Vec<Parsed> = paths
        .iter()
        .map(|p| Parsed {
            path: p.clone(),
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            header: TiffHeader::open(p),
        })
        .collect();

    let total_bytes: u64 = parsed
        .iter()
        .map(|p| std::fs::metadata(&p.path).map(|m| m.len()).unwrap_or(0))
        .sum();
    let unreadable_count = parsed.iter().filter(|p| p.header.is_err()).count();

    let ok_headers: Vec<(&str, &TiffHeader)> = parsed
        .iter()
        .filter_map(|p| p.header.as_ref().ok().map(|h| (p.name.as_str(), h)))
        .collect();
    let (uniform_layout, uniform_detail) = uniform_check(&ok_headers);

    let files = parsed
        .iter()
        .map(|p| match &p.header {
            Ok(h) => FileReport {
                name: p.name.clone(),
                path: p.path.display().to_string(),
                width: Some(h.width),
                height: Some(h.height),
                bits_per_sample: Some(h.bits_per_sample.to_vec()),
                samples_per_pixel: Some(h.samples_per_pixel),
                byte_order: Some(byte_order_str(h.byte_order).to_string()),
                compression: Some(h.compression),
                photometric: Some(h.photometric),
                planar_config: Some(h.planar_config),
                rows_per_strip: Some(h.rows_per_strip),
                strip_count: Some(h.strip_count()),
                file_size: Some(h.file_size),
                description: h.image_description.clone(),
                mmap_fast_path: Some(h.refusal.is_none()),
                refusal: h.refusal.as_ref().map(|r| r.to_string()),
                error: None,
            },
            Err(e) => FileReport {
                name: p.name.clone(),
                path: p.path.display().to_string(),
                width: None,
                height: None,
                bits_per_sample: None,
                samples_per_pixel: None,
                byte_order: None,
                compression: None,
                photometric: None,
                planar_config: None,
                rows_per_strip: None,
                strip_count: None,
                file_size: None,
                description: None,
                mmap_fast_path: None,
                refusal: None,
                error: Some(e.to_string()),
            },
        })
        .collect();

    InfoReport {
        path: display_path.display().to_string(),
        is_directory,
        frame_count: paths.len(),
        total_bytes,
        unreadable_count,
        uniform_layout,
        uniform_detail,
        files,
    }
}

/// Base description: byte order and `RowsPerStrip`, the two fields the
/// frozen sentence always names.
pub(crate) fn describe_layout(h: &TiffHeader) -> String {
    format!("{}/rps={}", byte_order_str(h.byte_order), h.rows_per_strip)
}

/// `describe_layout(h)` plus a suffix for every field that differs from
/// `other` and is not already covered by the base string (byte order and
/// `RowsPerStrip` are; the rest of `uniform_check`'s nine-field signature is
/// not). Without this, two members that disagree only on, say,
/// `Compression` render an identical-looking base string on both sides of
/// "is X, others X".
pub(crate) fn describe_layout_diff(h: &TiffHeader, other: &TiffHeader) -> String {
    let mut s = describe_layout(h);
    if h.compression != other.compression {
        s.push_str(&format!("/compression={}", h.compression));
    }
    if h.width != other.width || h.height != other.height {
        s.push_str(&format!("/{}x{}", h.width, h.height));
    }
    if h.bits_per_sample != other.bits_per_sample {
        s.push_str(&format!("/bits={:?}", h.bits_per_sample));
    }
    if h.samples_per_pixel != other.samples_per_pixel {
        s.push_str(&format!("/samples={}", h.samples_per_pixel));
    }
    if h.photometric != other.photometric {
        s.push_str(&format!("/photometric={}", h.photometric));
    }
    if h.planar_config != other.planar_config {
        s.push_str(&format!("/planar={}", h.planar_config));
    }
    s
}

/// Stricter than the reader's `check_uniform`: compares byte order and
/// `RowsPerStrip` too, because this verdict is a pre-check for stacking,
/// not the reader's admission test. Only headers that parsed are compared;
/// a file that failed to parse is counted in `unreadable_count` instead.
/// Ties among equally common layouts favor the most-recently-seen group,
/// which is what makes the two required fixture cases (one outlier among a
/// majority, either direction) name the same outlier a human would point
/// at.
pub(crate) fn uniform_check(headers: &[(&str, &TiffHeader)]) -> (bool, Option<String>) {
    type Signature = (u32, u32, Vec<u16>, u32, ByteOrder, u32, u16, u16, u16);
    fn signature(h: &TiffHeader) -> Signature {
        (
            h.width,
            h.height,
            h.bits_per_sample.to_vec(),
            h.samples_per_pixel,
            h.byte_order,
            h.rows_per_strip,
            h.compression,
            h.planar_config,
            h.photometric,
        )
    }

    if headers.len() < 2 {
        return (true, None);
    }

    let mut groups: Vec<(Signature, usize)> = Vec::new();
    for (_, h) in headers {
        let sig = signature(h);
        if let Some(group) = groups.iter_mut().find(|(gs, _)| *gs == sig) {
            group.1 += 1;
        } else {
            groups.push((sig, 1));
        }
    }

    if groups.len() == 1 {
        return (true, None);
    }

    let mut majority = groups[0].clone();
    for group in groups.iter().skip(1) {
        if group.1 >= majority.1 {
            majority = group.clone();
        }
    }
    let majority_sig = majority.0;

    let outlier = headers
        .iter()
        .find(|(_, h)| signature(h) != majority_sig)
        .expect("groups.len() > 1 implies at least one non-majority header");
    let majority_header = headers
        .iter()
        .find(|(_, h)| signature(h) == majority_sig)
        .expect("majority signature came from one of these headers");

    let detail = format!(
        "{} is {}, others {}",
        outlier.0,
        describe_layout_diff(outlier.1, majority_header.1),
        describe_layout(majority_header.1)
    );
    (false, Some(detail))
}

fn print_human(report: &InfoReport) {
    for (i, file) in report.files.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{}", file.path);
        if let Some(err) = &file.error {
            println!("  error: {err}");
            continue;
        }
        println!(
            "  {}x{} {}ch {}-bit {} compression={} planar={} photometric={}",
            file.width.unwrap_or(0),
            file.height.unwrap_or(0),
            file.samples_per_pixel.unwrap_or(0),
            file.bits_per_sample
                .as_ref()
                .and_then(|b| b.first().copied())
                .unwrap_or(0),
            file.byte_order.as_deref().unwrap_or("??"),
            file.compression.unwrap_or(0),
            file.planar_config.unwrap_or(0),
            file.photometric.unwrap_or(0),
        );
        println!(
            "  rows_per_strip={} strips={} file_size={}",
            file.rows_per_strip.unwrap_or(0),
            file.strip_count.unwrap_or(0),
            file.file_size.unwrap_or(0),
        );
        if let Some(description) = &file.description {
            println!("  description: {description}");
        }
        match &file.refusal {
            None => println!("  mmap fast path: yes"),
            Some(refusal) => println!("  mmap fast path: no — {refusal}"),
        }
    }

    if report.is_directory {
        println!();
        println!(
            "{} TIFFs, {} bytes total",
            report.frame_count, report.total_bytes
        );
        match &report.uniform_detail {
            None => println!("uniform layout: yes"),
            Some(detail) => println!("uniform layout: no — {detail}"),
        }
        if report.unreadable_count > 0 {
            println!("unreadable: {}", report.unreadable_count);
        }
    }
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}
