// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli` subcommands: `stack` fuses TIFF frames into a project,
//! `align` reports per-frame transforms without fusing; `export` turns a
//! project into an image or Z-Map file, `review` lists results under a
//! directory of projects; `info` reports TIFF metadata, `algorithms` lists
//! registered algorithms and their schemas; `cache` clears the stage
//! cache, `diagnostics` bundles logs and system info into one ZIP.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use focusforge_core::io::{
    ByteOrder, FrameDesc, RAW_EXTENSIONS, RawMetadata, RawStack, TiffError, TiffHeader,
    all_frame_paths, classify_header, has_extension, is_frame_path,
};
use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::Serialize;

mod algorithms;
mod align;
mod bytes;
mod diagnostics;
mod export;
#[cfg(feature = "gpu")]
mod gpu;
mod groups;
mod history;
mod job;
mod preset;
mod progress;
mod retouch;
mod review;
mod set;
mod signals;
mod stack;
mod status;

/// Resolves whether this run takes the GPU path or the CPU one:
/// `#[cfg(feature = "gpu")]` delegates to [`gpu::resolve_backend`]; with the
/// feature off, always [`stack::BackendChoice::cpu()`].
#[cfg(feature = "gpu")]
fn resolve_backend(
    algorithm_id: &str,
    no_gpu: bool,
    max_vram: Option<u64>,
) -> stack::BackendChoice {
    gpu::resolve_backend(algorithm_id, no_gpu, max_vram)
}

#[cfg(not(feature = "gpu"))]
fn resolve_backend(
    _algorithm_id: &str,
    _no_gpu: bool,
    _max_vram: Option<u64>,
) -> stack::BackendChoice {
    stack::BackendChoice::cpu()
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
    /// Increase log detail (repeatable).
    #[arg(short = 'v', action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// Log errors only.
    #[arg(short = 'q', global = true)]
    quiet: bool,
}

/// The `-v`/`-q` count as [`focusforge_diagnostics::Options::verbosity`]:
/// `-1` when `-q` is set (which wins over any `-v`), else the `-v` count.
fn verbosity(cli: &Cli) -> i8 {
    if cli.quiet {
        -1
    } else {
        i8::try_from(cli.verbose).unwrap_or(i8::MAX)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Print TIFF or RAW metadata for a file, or for every frame in a directory.
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
    /// Fuse supported TIFF or RAW frames of a directory into one **project** per
    /// detected stack, through a registered stacking algorithm. A run
    /// prepares projects for review; it does not export images -- run
    /// `focusforge-cli export` for that.
    Stack {
        /// Directory of TIFF or supported RAW frames to fuse (non-recursive, descending into
        /// `<dir>/frames` when that subdirectory exists).
        dir: PathBuf,
        /// Directory to collect this run's projects into. Omit to write
        /// each project beside its own frames.
        #[arg(short = 'o', long = "output", value_name = "DIR")]
        output: Option<PathBuf>,
        /// Registered stacking algorithm id (`focusforge-cli algorithms`
        /// lists them). Required unless a `--preset` supplies `algo`.
        #[arg(long)]
        algo: Option<String>,
        /// Hidden. Declared only so `stack` can refuse it by name and point
        /// at `focusforge-cli export`; a project carries its own Z-Map.
        #[arg(long, hide = true)]
        zmap: Option<PathBuf>,
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        set: set::SetArgs,
        #[command(flatten)]
        group_args: groups::GroupArgs,
        /// Skip alignment: frames are fused as read and the output keeps
        /// the full frame.
        #[arg(long = "no-align", action = clap::ArgAction::SetFalse)]
        align: bool,
        /// Skip the auto-crop: the output keeps the full frame extent even
        /// when alignment is on.
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
        /// A **ceiling only** on the adaptive host-memory budget: it can
        /// lower the budget the host-derived estimate resolved to, and can
        /// never raise it. Nobody has to pass this to get the machine's own
        /// concurrency -- omit it entirely and the budget is derived from
        /// the host. Accepts suffixed sizes (`8GiB`, `512MiB`, `4096`).
        #[arg(
            long = "max-host-memory",
            value_name = "SIZE",
            value_parser = crate::bytes::parse_byte_size
        )]
        max_host_memory: Option<u64>,
        /// A **ceiling only** on the adaptive device (VRAM) budget: it can
        /// lower the budget the startup probe settled on, and can never
        /// raise it. Omit it entirely to get the probed value. Accepts the
        /// same suffixed sizes as `--max-host-memory`. Only present when the
        /// `gpu` feature is compiled in.
        #[cfg(feature = "gpu")]
        #[arg(
            long = "max-vram",
            value_name = "SIZE",
            value_parser = crate::bytes::parse_byte_size
        )]
        max_vram: Option<u64>,
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
        /// Only in builds with GPU support.
        #[cfg(feature = "gpu")]
        #[arg(long = "no-gpu", action = clap::ArgAction::SetFalse)]
        gpu: bool,
        /// Skip the stage cache: recompute the alignment even when a valid
        /// entry exists, and store no new entry.
        #[arg(long = "no-cache", action = clap::ArgAction::SetFalse)]
        cache: bool,
    },
    /// Turn a project into an image file: the image with `-o`, the
    /// Z-Map with `--zmap`, either or both in one invocation. At least one
    /// of the two is required. This is the only command that writes an
    /// image.
    Export {
        /// The `.ffp` project to read.
        project: PathBuf,
        /// Write the image to this path.
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: Option<PathBuf>,
        /// Write the Z-Map to this path.
        #[arg(long, value_name = "FILE")]
        zmap: Option<PathBuf>,
        /// A checkpoint id or unique id prefix. Defaults to the head. Every
        /// image export re-reads and verifies the nearest fused ancestor's
        /// source frames; `--zmap` alone needs none.
        #[arg(long, value_name = "ID")]
        checkpoint: Option<String>,
        /// `--format` and its per-format flags: what `-o`'s image is
        /// encoded as.
        #[command(flatten)]
        format: export::FormatArgs,
        /// Write this source frame "as fused" -- warped by its recorded
        /// transform and offset by the applied crop, at the resolved
        /// checkpoint's own composite width and height. Requires
        /// `--frame-output`.
        #[arg(long, value_name = "INDEX", requires = "frame_output")]
        frame: Option<usize>,
        /// Where to write the frame `--frame` names.
        #[arg(long, value_name = "FILE")]
        frame_output: Option<PathBuf>,
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Apply an op list to a Z-Map and write the edited Z-Map. Every
    /// editing capability is reachable through this command, headless.
    Retouch {
        /// The Z-Map to edit.
        #[arg(long, value_name = "FILE")]
        zmap: PathBuf,
        /// The op list to apply, in the persisted `Op` JSON shape.
        /// Positions in error messages count from 0 in list order.
        #[arg(long, value_name = "FILE")]
        ops: PathBuf,
        /// Where to write the edited Z-Map. Required: no default output
        /// path, and never the same file as `--zmap`.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
        /// Undo this many ops off the end of the list before writing.
        #[arg(long, default_value_t = 0)]
        undo: usize,
        /// Redo this many undone ops. Must not exceed `--undo`.
        #[arg(long, default_value_t = 0)]
        redo: usize,
        /// Directory of source frames to recomposite `--render` from.
        /// Required by `--render`.
        #[arg(long, value_name = "DIR")]
        frames: Option<PathBuf>,
        /// Write the recomposited image at the edited map's undo/redo
        /// position: a per-pixel gather from `--frames`, CPU only.
        #[arg(long, value_name = "FILE", requires = "frames")]
        render: Option<PathBuf>,
        /// Blend the rainbow overlay over the edited map's recomposited
        /// image and write it as a PNG. Requires `--frames`, as `--render`
        /// does.
        #[arg(long, value_name = "FILE", requires = "frames")]
        overlay: Option<PathBuf>,
        /// The overlay's blend opacity: `0.0` leaves the composite
        /// untouched, `1.0` is the palette colour alone.
        #[arg(long, default_value_t = 0.65)]
        opacity: f32,
        /// Highlight this 0-based source index in the overlay -- the
        /// numbering an op's `source_id` uses -- and desaturate every
        /// other source, rather than the undivided wheel.
        #[arg(long, value_name = "INDEX")]
        solo: Option<u16>,
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// List, materialize, apply an op list onto, or delete a project's
    /// checkpoints. Every history operation is reachable through this
    /// command, headless.
    History {
        /// Accept a project whose recorded source frames' content changed
        /// since it was written, printing a warning instead of refusing.
        #[arg(long = "accept-changed-sources", global = true)]
        accept_changed_sources: bool,
        #[command(subcommand)]
        command: history::HistoryCommand,
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
        /// run.
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
    /// non-recursive listing of `*.ffp` files; never groups frames
    /// itself and writes nothing.
    Review {
        /// Directory to review: a run's `-o` output, or a plain directory of
        /// `*.ffp` projects.
        dir: PathBuf,
        /// Emit one wrapped JSON document instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Remove every entry from the stage cache under
    /// `.scratch/stage-cache/`. Never touches `.scratch/resume/` or any
    /// project.
    Cache {
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Package this installation's logs, crash reports, system summary,
    /// graphics adapter record, effective configuration and version into
    /// one ZIP, listing every file before writing it. The home directory
    /// and the user name are redacted out of every text entry, image data
    /// is never included, and nothing is sent anywhere.
    Diagnostics {
        /// Where to write the ZIP. Omit for a dated name in the current
        /// directory.
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: Option<PathBuf>,
        /// Include the minidump binaries. They cannot be redacted.
        #[arg(long = "include-minidumps")]
        include_minidumps: bool,
        /// Overwrite the output path when it already exists.
        #[arg(long)]
        force: bool,
    },
}

/// The subcommand's own name, as the job span carries it in the log.
fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Info { .. } => "info",
        Command::Algorithms { .. } => "algorithms",
        Command::Stack { .. } => "stack",
        Command::Export { .. } => "export",
        Command::Retouch { .. } => "retouch",
        Command::History { .. } => "history",
        Command::Align { .. } => "align",
        Command::Review { .. } => "review",
        Command::Cache { .. } => "cache",
        Command::Diagnostics { .. } => "diagnostics",
    }
}

fn main() -> ExitCode {
    // Ahead of every parser: a re-spawned monitor process carries no
    // arguments of its own and never reaches the application's own work.
    if let Some(code) = focusforge_diagnostics::run_monitor() {
        return ExitCode::from(code as u8);
    }
    let cli = Cli::parse();
    let data_dir = focusforge_diagnostics::data_dir_from_env();
    let _diagnostics = focusforge_diagnostics::init(&focusforge_diagnostics::Options {
        data_dir: data_dir.clone(),
        verbosity: verbosity(&cli),
    });
    // Ahead of the job span, so the notice carries no subcommand of its own.
    diagnostics::announce_crashes(data_dir.as_deref());
    // The earliest point a forced crash is observable: `init` is what
    // installs the hook that reports it.
    focusforge_diagnostics::force_crash("startup");
    // Entered for the whole invocation, so every event a run raises names
    // the command it came from.
    let _job = tracing::debug_span!("job", command = command_name(&cli.command)).entered();
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
            max_host_memory,
            #[cfg(feature = "gpu")]
            max_vram,
            preset,
            write_preset,
            status,
            dry_run,
            #[cfg(feature = "gpu")]
            gpu,
            cache,
        } => {
            #[cfg(feature = "gpu")]
            let no_gpu = !gpu;
            #[cfg(not(feature = "gpu"))]
            let no_gpu = false;
            #[cfg(not(feature = "gpu"))]
            let max_vram: Option<u64> = None;
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
                    cache,
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
                max_host_memory,
                max_vram,
            )
        }
        Command::Export {
            project,
            output,
            zmap,
            checkpoint,
            format,
            frame,
            frame_output,
            json,
        } => export::run(
            &project,
            output.as_deref(),
            zmap.as_deref(),
            checkpoint.as_deref(),
            &format,
            frame,
            frame_output.as_deref(),
            json,
        ),
        Command::Retouch {
            zmap,
            ops,
            out,
            undo,
            redo,
            frames,
            render,
            overlay,
            opacity,
            solo,
            json,
        } => retouch::run(
            &zmap,
            &ops,
            &out,
            undo,
            redo,
            frames.as_deref(),
            render.as_deref(),
            overlay.as_deref(),
            opacity,
            solo,
            json,
        ),
        Command::History {
            command,
            accept_changed_sources,
        } => history::run(command, accept_changed_sources),
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
        Command::Cache { json } => cache_clear(json),
        Command::Diagnostics {
            output,
            include_minidumps,
            force,
        } => diagnostics::run(output.as_deref(), include_minidumps, force),
    }
}

/// The frozen `cache --json` schema.
#[derive(Serialize)]
struct CacheClearReport {
    format: u32,
    removed: usize,
}

fn cache_clear(json: bool) -> ExitCode {
    let cache = match focusforge_core::job::StageCache::open(Path::new(".scratch")) {
        Ok(cache) => cache,
        Err(err) => return fail(err.to_string()),
    };
    let removed = match cache.clear() {
        Ok(removed) => removed,
        Err(err) => return fail(err.to_string()),
    };
    if json {
        let report = CacheClearReport { format: 1, removed };
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!(
            "removed {removed} stage cache entr{}",
            if removed == 1 { "y" } else { "ies" }
        );
    }
    ExitCode::SUCCESS
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
    reader: Option<String>,
    refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw: Option<RawRecord>,
    error: Option<String>,
}

/// RAW metadata reported without developing image pixels.
#[derive(Serialize)]
struct RawRecord {
    make: String,
    model: String,
    raw_width: u32,
    raw_height: u32,
    output_width: u32,
    output_height: u32,
    crop_x: u32,
    crop_y: u32,
    storage_bits: Option<u16>,
    decoder_bits: Option<usize>,
    black_levels: Option<Vec<f32>>,
    white_levels: Option<Vec<u32>>,
    cfa: Option<Vec<u8>>,
    cfa_width: Option<u16>,
    cfa_height: Option<u16>,
    color_matrix: Option<Vec<f32>>,
    capture_time_epoch_seconds: Option<u64>,
    decoder: &'static str,
}

impl From<&RawMetadata> for RawRecord {
    fn from(metadata: &RawMetadata) -> Self {
        Self {
            make: metadata.make.clone(),
            model: metadata.model.clone(),
            raw_width: metadata.raw_width,
            raw_height: metadata.raw_height,
            output_width: metadata.output_width,
            output_height: metadata.output_height,
            crop_x: metadata.crop_x,
            crop_y: metadata.crop_y,
            storage_bits: metadata.storage_bits,
            decoder_bits: metadata.decoder_bits,
            black_levels: metadata.black_levels.clone(),
            white_levels: metadata.white_levels.clone(),
            cfa: metadata.cfa.clone(),
            cfa_width: metadata.cfa_width,
            cfa_height: metadata.cfa_height,
            color_matrix: metadata.color_matrix.clone(),
            capture_time_epoch_seconds: metadata
                .capture_time
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs()),
            decoder: "rawler",
        }
    }
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
/// fails to parse structurally, and a directory with zero frames. Both exit
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
        match info_frame_paths(path) {
            Ok(paths) => paths,
            Err(err) => return fail(err),
        }
    } else {
        if is_raw_path(path) {
            if let Err(err) = RawStack::open_files(&[path.to_path_buf()]) {
                return fail(err);
            }
        } else if let Err(err) = TiffHeader::open(path) {
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

fn is_raw_path(path: &Path) -> bool {
    has_extension(path, RAW_EXTENSIONS)
}

fn info_frame_paths(dir: &Path) -> Result<Vec<PathBuf>, TiffError> {
    let paths = all_frame_paths(dir)?;
    let paths: Vec<_> = paths
        .into_iter()
        .filter(|path| is_raw_path(path) || is_frame_path(path))
        .collect();
    if paths.is_empty() {
        return Err(TiffError::NoFrames {
            path: dir.to_path_buf(),
        });
    }
    Ok(paths)
}

/// Header-only probe of every path in `paths`, never `TiffStack` and never
/// a tile: this is what lets an unreadable or policy-refused member end up
/// as one report line instead of failing the whole command.
fn build_report(display_path: &Path, is_directory: bool, paths: &[PathBuf]) -> InfoReport {
    enum Probe {
        Tiff(TiffHeader),
        Raw(RawMetadata, Box<FrameDesc>),
    }

    impl Probe {
        fn desc(&self) -> FrameDesc {
            match self {
                Self::Tiff(header) => FrameDesc::from(header),
                Self::Raw(_, desc) => (**desc).clone(),
            }
        }
    }

    struct Parsed {
        path: PathBuf,
        name: String,
        probe: Result<Probe, String>,
    }

    let parsed: Vec<Parsed> = paths
        .iter()
        .map(|p| Parsed {
            path: p.clone(),
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(p.display().to_string()),
            probe: if is_raw_path(p) {
                RawStack::open_files(std::slice::from_ref(p))
                    .and_then(|stack| {
                        let metadata = stack.metadata(0).cloned().ok_or(
                            focusforge_core::io::RawError::FrameIndex { index: 0, count: 0 },
                        )?;
                        let desc = stack.frame(0).cloned().ok_or(
                            focusforge_core::io::RawError::FrameIndex { index: 0, count: 0 },
                        )?;
                        Ok(Probe::Raw(metadata, Box::new(desc)))
                    })
                    .map_err(|error| error.to_string())
            } else {
                TiffHeader::open(p)
                    .map(Probe::Tiff)
                    .map_err(|error| error.to_string())
            },
        })
        .collect();

    let total_bytes: u64 = parsed
        .iter()
        .map(|p| std::fs::metadata(&p.path).map(|m| m.len()).unwrap_or(0))
        .sum();
    let unreadable_count = parsed.iter().filter(|p| p.probe.is_err()).count();

    let named_descs: Vec<(&str, FrameDesc)> = parsed
        .iter()
        .filter_map(|p| {
            p.probe
                .as_ref()
                .ok()
                .map(|probe| (p.name.as_str(), probe.desc()))
        })
        .collect();
    let named_descs: Vec<(&str, &FrameDesc)> = named_descs
        .iter()
        .map(|(name, desc)| (*name, desc))
        .collect();
    let (uniform_layout, uniform_detail) = focusforge_core::pipeline::uniform_check(&named_descs);

    let files = parsed
        .iter()
        .map(|p| match &p.probe {
            Ok(Probe::Tiff(h)) => FileReport {
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
                reader: classify_header(h).ok().map(|kind| kind.name().to_string()),
                refusal: h.refusal.as_ref().map(|r| r.to_string()),
                raw: None,
                error: None,
            },
            Ok(Probe::Raw(metadata, _)) => FileReport {
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
                file_size: std::fs::metadata(&p.path)
                    .ok()
                    .map(|metadata| metadata.len()),
                description: None,
                mmap_fast_path: None,
                reader: Some("RawStack".to_string()),
                refusal: None,
                raw: Some(RawRecord::from(metadata)),
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
                reader: None,
                refusal: None,
                raw: None,
                error: Some(e.clone()),
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
        if let Some(raw) = &file.raw {
            println!(
                "  RAW {} {} raw={}x{} output={}x{} crop={},{} storage_bits={:?} decoder_bits={:?}",
                raw.make,
                raw.model,
                raw.raw_width,
                raw.raw_height,
                raw.output_width,
                raw.output_height,
                raw.crop_x,
                raw.crop_y,
                raw.storage_bits,
                raw.decoder_bits,
            );
            println!(
                "  black={:?} white={:?}",
                raw.black_levels, raw.white_levels
            );
            println!(
                "  cfa={:?} {}x{} matrix={:?}",
                raw.cfa,
                raw.cfa_width.unwrap_or(0),
                raw.cfa_height.unwrap_or(0),
                raw.color_matrix
            );
            if let Some(capture_time) = raw.capture_time_epoch_seconds {
                println!("  capture_time_epoch_seconds: {capture_time}");
            }
            println!("  decoder: {}", raw.decoder);
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
        if let Some(reader) = &file.reader {
            println!("  reader: {reader}");
        }
    }

    if report.is_directory {
        println!();
        let raw_count = report
            .files
            .iter()
            .filter(|file| is_raw_path(Path::new(&file.path)))
            .count();
        if raw_count == 0 {
            println!(
                "{} TIFFs, {} bytes total",
                report.frame_count, report.total_bytes
            );
        } else {
            println!(
                "{} frames ({} TIFF, {} RAW), {} bytes total",
                report.frame_count,
                report.frame_count - raw_count,
                raw_count,
                report.total_bytes
            );
        }
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
    tracing::error!(target: MIRRORED_OUTPUT_TARGET, "error: {message}");
    ExitCode::FAILURE
}

fn fail_with_code(message: impl std::fmt::Display, code: u8) -> ExitCode {
    eprintln!("error: {message}");
    tracing::error!(target: MIRRORED_OUTPUT_TARGET, "error: {message}");
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(label: &str, signature: &[&str]) -> FrameDesc {
        FrameDesc {
            path: PathBuf::from(label),
            file_size: 0,
            image_description: None,
            width: 1,
            height: 1,
            samples_per_pixel: 1,
            strip_offsets: Vec::new(),
            strip_byte_counts: Vec::new(),
            raw_identity: None,
            layout_label: signature[0..2].join("/"),
            layout_signature: signature.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn neutral_layout_tie_uses_the_last_group_and_lists_suffixes() {
        let a = desc("a", &["II", "rps=1", "compression=1", "4x4"]);
        let b = desc("b", &["MM", "rps=8", "compression=5", "8x4"]);
        let (_, detail) = focusforge_core::pipeline::uniform_check(&[("a", &a), ("b", &b)]);
        assert_eq!(
            detail.as_deref(),
            Some("a is II/rps=1/compression=1/4x4, others MM/rps=8")
        );
    }

    /// `--no-crop`'s clap default and `PROJECT_PARAMS`'s `auto-crop`
    /// descriptor default are one source of truth. This test fails the
    /// moment either one moves without the other.
    #[test]
    fn no_crop_flag_reads_the_project_schema() {
        let cli = Cli::try_parse_from(["focusforge-cli", "stack", "dir", "--algo", "x"])
            .expect("a minimal `stack` invocation must parse");
        let Command::Stack { crop, .. } = cli.command else {
            panic!("expected a Command::Stack");
        };
        let expected = focusforge_core::project::PROJECT_PARAMS
            .iter()
            .find(|d| d.id == "auto-crop")
            .expect("PROJECT_PARAMS must declare an auto-crop descriptor")
            .default
            .as_bool()
            .expect("auto-crop's default must be a bool");
        assert_eq!(crop, expected);
    }

    /// clap's own consistency check over the whole parser: every subcommand
    /// and flag it declares actually builds into a valid `Command`.
    #[test]
    fn parser_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// Every export `ParamDesc` id is a `--<id>` flag on `export` whose help
    /// text names its default and every choice or numeric range -- the
    /// same one-source-of-truth check `no_crop_flag_reads_the_project_schema`
    /// runs for `auto-crop`.
    #[test]
    fn export_flags_read_the_export_schema() {
        use clap::CommandFactory;
        use focusforge_core::params::ParamKind;
        use focusforge_core::project::export::{JPEG_PARAMS, PNG_PARAMS, TIFF_PARAMS};

        let export_cmd = Cli::command()
            .find_subcommand("export")
            .expect("export subcommand must exist")
            .clone();
        for schema in [&*TIFF_PARAMS, &*PNG_PARAMS, &*JPEG_PARAMS] {
            for desc in schema.iter() {
                let arg = export_cmd
                    .get_arguments()
                    .find(|a| a.get_long() == Some(desc.id.as_str()))
                    .unwrap_or_else(|| panic!("no --{} flag on export", desc.id));
                let help = arg
                    .get_help()
                    .unwrap_or_else(|| panic!("--{} needs help text", desc.id))
                    .to_string();
                match &desc.kind {
                    ParamKind::Enum { variants } => {
                        for variant in variants {
                            assert!(
                                help.contains(&variant.id),
                                "--{}'s help is missing choice {:?}: {help:?}",
                                desc.id,
                                variant.id
                            );
                        }
                        let default = desc.default.as_enum().expect("enum default");
                        assert!(
                            help.contains(default),
                            "--{}'s help is missing its default {:?}: {help:?}",
                            desc.id,
                            default
                        );
                    }
                    ParamKind::Int { min, max } => {
                        for bound in [min, max] {
                            assert!(
                                help.contains(&bound.to_string()),
                                "--{}'s help is missing bound {bound}: {help:?}",
                                desc.id
                            );
                        }
                        let default = desc.default.as_i64().expect("int default");
                        assert!(
                            help.contains(&default.to_string()),
                            "--{}'s help is missing its default {default}: {help:?}",
                            desc.id
                        );
                    }
                    other => panic!("unexpected export param kind for {}: {other:?}", desc.id),
                }
            }
        }
    }
}
