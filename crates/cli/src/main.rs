// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli` subcommands: `stack` fuses TIFF frames into a project,
//! `align` reports per-frame transforms without fusing; `export` turns a
//! project into an image or Z-Map file, `review` lists results under a
//! directory of projects; `info` reports TIFF metadata, `algorithms` lists
//! registered algorithms and their schemas; `cache` clears the stage
//! cache, `diagnostics` bundles logs and system info into one ZIP.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use focusforge_core::io::{
    ByteOrder, FrameDesc, RAW_EXTENSIONS, RawMetadata, RawStack, TiffError, TiffHeader,
    all_frame_paths, classify_header, has_extension, is_frame_path,
};
use focusforge_core::params::ParamValues;
use focusforge_core::project::Project;
use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::Serialize;

/// Set once a write to stdout finds its reader gone.
#[cfg(not(test))]
static STDOUT_CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Shadows std's `println!` in every module below. Once stdout's reader has
/// gone away, this and every later call drops its output, so the command
/// runs on to its own exit status; any other write error panics as std's
/// does.
#[cfg(not(test))]
macro_rules! println {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        use std::sync::atomic::Ordering::Relaxed;
        if !$crate::STDOUT_CLOSED.load(Relaxed) {
            if let Err(err) = writeln!(std::io::stdout().lock(), $($arg)*) {
                if err.kind() != std::io::ErrorKind::BrokenPipe {
                    panic!("failed printing to stdout: {err}");
                }
                $crate::STDOUT_CLOSED.store(true, Relaxed);
            }
        }
    }};
}

mod algorithms;
mod align;
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
fn resolve_backend(
    algorithm_id: &str,
    no_gpu: bool,
    max_vram: Option<u64>,
    quiet: bool,
) -> stack::BackendChoice {
    #[cfg(feature = "gpu")]
    {
        gpu::resolve_backend(algorithm_id, no_gpu, max_vram, quiet)
    }
    #[cfg(not(feature = "gpu"))]
    {
        let _ = (algorithm_id, no_gpu, max_vram, quiet);
        stack::BackendChoice::cpu()
    }
}

/// Resolves whether the align stage takes the GPU path or the CPU one, as
/// [`resolve_backend`] does for fusion: `#[cfg(feature = "gpu")]` delegates to
/// [`gpu::resolve_align_backend`]; with the feature off, always
/// [`stack::AlignBackendChoice::cpu()`].
fn resolve_align_backend(
    algorithm_id: &str,
    no_gpu: bool,
    max_vram: Option<u64>,
    quiet: bool,
) -> stack::AlignBackendChoice {
    #[cfg(feature = "gpu")]
    {
        gpu::resolve_align_backend(algorithm_id, no_gpu, max_vram, quiet)
    }
    #[cfg(not(feature = "gpu"))]
    {
        let _ = (algorithm_id, no_gpu, max_vram, quiet);
        stack::AlignBackendChoice::cpu()
    }
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
    #[arg(short = 'q', long, global = true)]
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
    /// Print project, TIFF or RAW metadata for a file, or for every frame in a directory.
    Info {
        /// Path to a file or directory.
        path: PathBuf,
        /// Emit the JSON document instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// List every registered algorithm, or print one named algorithm's full
    /// parameter schema.
    Algorithms {
        /// Algorithm id. Omit to list every registered algorithm.
        id: Option<String>,
        /// Emit the JSON document instead of human-readable text.
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
        #[arg(long = "algorithm")]
        algo: Option<String>,
        /// Emit the JSON report instead of human-readable text.
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
        /// Refuse the run when an alignment fails. Meaningless with
        /// `--no-align`.
        #[arg(long)]
        strict: bool,
        /// Alignment algorithm id. Optional: defaults to the sole
        /// registered alignment algorithm, refusing (and listing the
        /// choices) when more than one is registered.
        #[arg(long = "align-algorithm")]
        align_algo: Option<String>,
        #[command(flatten)]
        align_set: set::AlignSetArgs,
        /// Resume a previously cancelled run of this same stack, found by
        /// stack fingerprint under `.scratch/resume/` -- no argument.
        /// Refuses at exit 1 when no matching scratch state exists or it
        /// disagrees with this invocation's inputs.
        #[arg(long)]
        resume: bool,
        /// Print no stage progress.
        #[arg(long)]
        no_progress: bool,
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
            value_parser = focusforge_core::bytes::parse_byte_size
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
            value_parser = focusforge_core::bytes::parse_byte_size
        )]
        max_vram: Option<u64>,
        /// Read parameter values from a TOML preset: the `algo` and
        /// `align-algo` keys plus the `[stack]` and `[align]` value
        /// tables. A flag always beats the file -- `--set`/`--align-set`
        /// override the tables, `--algorithm`/`--align-algorithm` override the keys.
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
        #[arg(long = "cpu", action = clap::ArgAction::SetFalse)]
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
        /// A checkpoint id or unique id prefix. Defaults to the head. An
        /// unedited image export needs no source frames; an edited one
        /// re-reads and verifies the nearest fused ancestor's. `--zmap`
        /// alone needs none.
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
        /// Emit the JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Append an op list to a project as new states, one per op, each the
    /// previous one's child: on the current head, or on `--from` as a
    /// branch. `export --checkpoint` writes any of them.
    Retouch {
        /// The project to append to.
        project: PathBuf,
        /// The op list: a JSON array of ops in the persisted `Op` shape.
        /// A brush stroke blends frame `source_id` over the image along
        /// its path: fully within `radius * (1 - softness)`, fading
        /// linearly to nothing at `radius`. Every op is checked before
        /// anything is written; positions in error messages count from 0 in
        /// list order.
        #[arg(long, value_name = "FILE")]
        ops: PathBuf,
        /// The state to append under -- any unique prefix of its id.
        /// Defaults to the project's current head.
        #[arg(long)]
        from: Option<String>,
        /// Name the last appended state.
        #[arg(long)]
        name: Option<String>,
        /// Emit the JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// List, delete or compact a project's checkpoints. Every history
    /// operation is reachable through this command, headless.
    History {
        #[command(subcommand)]
        command: history::HistoryCommand,
    },
    /// Compute and report the per-frame transforms for a stack: readable
    /// text by default, a `--json` document on request, and an
    /// optional per-frame warped-TIFF dump for a visual check.
    Align {
        /// Directory of TIFF frames to align (non-recursive, descending
        /// into `<dir>/frames` when that subdirectory exists).
        dir: PathBuf,
        /// Registered alignment algorithm id. Optional: defaults to the
        /// sole registered alignment algorithm.
        #[arg(long = "algorithm")]
        algo: Option<String>,
        /// Emit the JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
        /// Refuse the run when an alignment fails.
        #[arg(long)]
        strict: bool,
        /// Write one warped TIFF per frame into this directory, at source
        /// dimensions, uncropped.
        #[arg(long = "write-warped", value_name = "DIR")]
        write_warped: Option<PathBuf>,
        #[command(flatten)]
        set: set::SetArgs,
        /// A **ceiling only** on the adaptive host-memory budget the
        /// alignment workers are sized from: it can lower the budget the
        /// host-derived estimate resolved to, and can never raise it. Omit
        /// it entirely and the budget is derived from the host. Accepts
        /// suffixed sizes (`8GiB`, `512MiB`, `4096`).
        #[arg(
            long = "max-host-memory",
            value_name = "SIZE",
            value_parser = focusforge_core::bytes::parse_byte_size
        )]
        max_host_memory: Option<u64>,
        /// A **ceiling only** on the adaptive device (VRAM) budget, for the
        /// alignment's GPU levels and the `--write-warped` fuse backend: it
        /// can lower the budget the startup probe settled on, and can never
        /// raise it. Accepts the same suffixed sizes as `--max-host-memory`.
        /// Only present when the `gpu` feature is compiled in.
        #[cfg(feature = "gpu")]
        #[arg(
            long = "max-vram",
            value_name = "SIZE",
            value_parser = focusforge_core::bytes::parse_byte_size
        )]
        max_vram: Option<u64>,
        /// Print no progress at all. `align` has no fuse-tile units to
        /// count: this suppresses only the one stage line `align`
        /// otherwise prints.
        #[arg(long)]
        no_progress: bool,
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
        /// Force the CPU path even when a usable GPU adapter is available:
        /// the alignment's large levels and the `--write-warped` fuse
        /// backend both stay on the CPU. Only present when the `gpu` feature
        /// is compiled in.
        #[cfg(feature = "gpu")]
        #[arg(long = "cpu", action = clap::ArgAction::SetFalse)]
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
    #[command(subcommand_required = true)]
    Cache {
        /// Emit the JSON report instead of human-readable text.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: CacheCommand,
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

#[derive(Subcommand)]
enum CacheCommand {
    /// Remove every entry from the stage cache.
    Clear,
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

const ROOT_CATEGORIES: [(&str, &[&str]); 3] = [
    ("Processing", &["stack", "align", "retouch", "export"]),
    ("Inspection", &["info", "review", "algorithms"]),
    ("Maintenance", &["history", "cache", "diagnostics"]),
];

fn categorized_root_help_template(
    command: &clap::Command,
    categories: &[(&str, &[&str])],
) -> Result<String, String> {
    let visible: Vec<_> = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .collect();
    let mut grouped = String::new();
    let mut seen = std::collections::BTreeSet::new();
    for (heading, names) in categories {
        grouped.push_str(heading);
        grouped.push_str(":\n");
        for &name in *names {
            let subcommand = visible
                .iter()
                .find(|subcommand| subcommand.get_name() == name)
                .ok_or_else(|| format!("root help category names missing command `{name}`"))?;
            if !seen.insert(name) {
                return Err(format!("root help category names `{name}` more than once"));
            }
            let about = subcommand
                .get_about()
                .ok_or_else(|| format!("root command `{name}` needs an about"))?;
            grouped.push_str(&format!("  {name:12} {about}\n"));
        }
        grouped.push('\n');
    }
    for subcommand in visible {
        if !seen.contains(subcommand.get_name()) {
            return Err(format!(
                "root help category has no entry for visible command `{}`",
                subcommand.get_name()
            ));
        }
    }
    Ok(format!(
        "{{before-help}}{{name}} {{version}}\n{{about}}\n\n{{usage-heading}} {{usage}}\n\n{grouped}Examples:\n  focusforge-cli stack frames/ --algorithm laplacian\n  focusforge-cli history list project.ffp\n\n{{options}}{{after-help}}"
    ))
}

fn root_help_template(command: &clap::Command) -> Result<String, String> {
    categorized_root_help_template(command, &ROOT_CATEGORIES)
}

fn parse_cli() -> Cli {
    let command = Cli::command();
    let template =
        root_help_template(&command).expect("every visible root command must be categorized");
    let matches = command.help_template(template).get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

fn main() -> ExitCode {
    // Ahead of every parser: a re-spawned monitor process carries no
    // arguments of its own and never reaches the application's own work.
    if let Some(code) = focusforge_diagnostics::run_monitor() {
        return ExitCode::from(code as u8);
    }
    let cli = parse_cli();
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
    let quiet = cli.quiet;
    match cli.command {
        Command::Info { path, json } => info(&path, json),
        Command::Algorithms { id, json, set } => algorithms::run(id.as_deref(), json, &set),
        Command::Stack {
            dir,
            output,
            algo,
            json,
            set,
            group_args,
            align,
            crop,
            strict,
            align_algo,
            align_set,
            resume,
            no_progress,
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
                json,
                &set,
                &align::AlignOptions {
                    align,
                    crop,
                    lax: !strict,
                    algo: align_algo.as_deref(),
                    set: &align_set.align_set,
                },
                &job::JobOptions {
                    resume,
                    quiet,
                    no_progress,
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
            project,
            ops,
            from,
            name,
            json,
        } => retouch::run(&project, &ops, from.as_deref(), name.as_deref(), json),
        Command::History { command } => history::run(command),
        Command::Align {
            dir,
            algo,
            json,
            strict,
            write_warped,
            set,
            max_host_memory,
            #[cfg(feature = "gpu")]
            max_vram,
            no_progress,
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
            #[cfg(not(feature = "gpu"))]
            let max_vram: Option<u64> = None;
            align::run(
                &dir,
                algo.as_deref(),
                json,
                !strict,
                write_warped.as_deref(),
                &set,
                max_host_memory,
                no_progress,
                quiet,
                &preset::PresetOptions {
                    preset: preset.as_deref(),
                    write_preset: write_preset.as_deref(),
                    status: status.as_deref(),
                    dry_run: false,
                },
                no_gpu,
                max_vram,
            )
        }
        Command::Review { dir, json } => review::run(&dir, json),
        Command::Cache {
            json,
            command: CacheCommand::Clear,
        } => cache_clear(json),
        Command::Diagnostics {
            output,
            include_minidumps,
            force,
        } => diagnostics::run(output.as_deref(), include_minidumps, force),
    }
}

/// The `cache --json` report.
#[derive(Serialize)]
struct CacheClearReport {
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
        let report = CacheClearReport { removed };
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

/// The `info --json` document. One shape for both a file and a
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

/// The `info --json` document for a saved project.
#[derive(Serialize)]
struct ProjectInfoReport {
    path: String,
    bytes: u64,
    producer: String,
    created_at: i64,
    checkpoint_id: String,
    checkpoint_label: Option<String>,
    algorithm: String,
    params: ParamValues,
    width: u32,
    height: u32,
    channels: u32,
    source_count: usize,
    visible_checkpoint_count: usize,
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

    if metadata.is_file() && has_extension(path, &["ffp"]) {
        return info_project(path, metadata.len(), json);
    }

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

fn info_project(path: &Path, bytes: u64, json: bool) -> ExitCode {
    let project = match Project::open(path) {
        Ok(project) => project,
        Err(err) => {
            return fail_with_code(
                format!("cannot inspect {}: {err}", path.display()),
                history::exit_code_for(&err),
            );
        }
    };
    let checkpoint = project.checkpoint();
    let fused = match project.fuse_state(&checkpoint.id) {
        Ok(fused) => fused,
        Err(err) => {
            return fail_with_code(
                format!("cannot inspect {}: {err}", path.display()),
                history::exit_code_for(&err),
            );
        }
    };
    let report = ProjectInfoReport {
        path: path.display().to_string(),
        bytes,
        producer: project.manifest().producer.clone(),
        created_at: project.manifest().created_at,
        checkpoint_id: checkpoint.id.clone(),
        checkpoint_label: checkpoint.label.clone(),
        algorithm: checkpoint.algorithm.clone(),
        params: checkpoint.params.clone(),
        width: fused.composite.width,
        height: fused.composite.height,
        channels: fused.composite.samples_per_pixel,
        source_count: project.manifest().sources.len(),
        visible_checkpoint_count: project.history().visible().len(),
    };

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        let params = match serde_json::to_string(&report.params) {
            Ok(params) => params,
            Err(err) => return fail(format!("failed to serialize parameters: {err}")),
        };
        print_project_human(&report, &params);
    }
    ExitCode::SUCCESS
}

fn print_project_human(report: &ProjectInfoReport, params: &str) {
    println!("{}", report.path);
    println!("  bytes: {}", report.bytes);
    println!("  producer: {}", report.producer);
    println!("  created_at: {}", report.created_at);
    println!("  checkpoint: {}", report.checkpoint_id);
    println!(
        "  checkpoint label: {}",
        report.checkpoint_label.as_deref().unwrap_or("none")
    );
    println!("  algorithm: {}", report.algorithm);
    println!("  params: {params}");
    println!("  width: {}", report.width);
    println!("  height: {}", report.height);
    println!("  channels: {}", report.channels);
    println!("  sources: {}", report.source_count);
    println!("  visible checkpoints: {}", report.visible_checkpoint_count);
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
        Tiff(Box<TiffHeader>),
        Raw(Box<RawMetadata>, Box<FrameDesc>),
    }

    impl Probe {
        fn desc(&self) -> FrameDesc {
            match self {
                Self::Tiff(header) => FrameDesc::from(header.as_ref()),
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
                        Ok(Probe::Raw(Box::new(metadata), Box::new(desc)))
                    })
                    .map_err(|error| error.to_string())
            } else {
                TiffHeader::open(p)
                    .map(|header| Probe::Tiff(Box::new(header)))
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
                raw: Some(RawRecord::from(metadata.as_ref())),
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

    fn project_default(id: &str) -> bool {
        focusforge_core::project::PROJECT_PARAMS
            .iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("PROJECT_PARAMS must declare a `{id}` descriptor"))
            .default
            .as_bool()
            .unwrap_or_else(|| panic!("`{id}`'s default must be a bool"))
    }

    /// `--no-crop`'s clap default and `PROJECT_PARAMS`'s `auto-crop`
    /// descriptor default are one source of truth. This test fails the
    /// moment either one moves without the other.
    #[test]
    fn no_crop_flag_reads_the_project_schema() {
        let cli = Cli::try_parse_from(["focusforge-cli", "stack", "dir", "--algorithm", "x"])
            .expect("a minimal `stack` invocation must parse");
        let Command::Stack { crop, .. } = cli.command else {
            panic!("expected a Command::Stack");
        };
        assert_eq!(crop, project_default("auto-crop"));
    }

    /// `--no-align` and the `align` descriptor are one source of truth.
    #[test]
    fn no_align_flag_reads_the_project_schema() {
        let cli = Cli::try_parse_from(["focusforge-cli", "stack", "dir", "--algorithm", "x"])
            .expect("a minimal `stack` invocation must parse");
        let Command::Stack { align, .. } = cli.command else {
            panic!("expected a Command::Stack");
        };
        assert_eq!(align, project_default("align"));
    }

    /// clap's own consistency check over the whole parser: every subcommand
    /// and flag it declares actually builds into a valid `Command`.
    #[test]
    fn parser_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn root_help_categories_cover_unique_metadata_commands() {
        use clap::CommandFactory;

        let command = Cli::command();
        let help = root_help_template(&command).expect("root categories cover every command");
        for subcommand in command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
        {
            let about = subcommand.get_about().expect("visible command needs about");
            assert!(
                help.contains(&format!("  {:12} {about}", subcommand.get_name())),
                "{} must use its clap about",
                subcommand.get_name()
            );
        }

        let duplicate = [("Duplicate", &["stack", "stack"][..])];
        assert!(
            categorized_root_help_template(&command, &duplicate)
                .expect_err("duplicate category entries must fail")
                .contains("more than once")
        );

        let missing = [("Missing", &["stack"][..])];
        assert!(
            categorized_root_help_template(&command, &missing)
                .expect_err("incomplete categories must fail")
                .contains("no entry")
        );

        let unmapped = command
            .clone()
            .subcommand(clap::Command::new("unmapped").about("Unmapped test"));
        assert!(
            root_help_template(&unmapped)
                .expect_err("new visible command must fail category coverage")
                .contains("unmapped")
        );

        let with_hidden = command.clone().subcommand(
            clap::Command::new("hidden-test")
                .about("Hidden test command")
                .hide(true),
        );
        let hidden_help = root_help_template(&with_hidden)
            .expect("hidden command must not require a product category");
        assert!(!hidden_help.contains("hidden-test"));
        assert!(!hidden_help.contains("Hidden test command"));
        assert!(
            !command
                .get_subcommands()
                .any(|subcommand| subcommand.get_name() == "help"),
            "generated help must not be a product category entry"
        );
        let generated_help = Cli::command()
            .try_get_matches_from(["focusforge-cli", "help", "history", "list"])
            .expect_err("generated help must remain parseable");
        assert_eq!(generated_help.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    fn cli_reference_at(path: &std::path::Path) -> Result<String, String> {
        std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))
    }

    fn cli_reference() -> String {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/cli.md");
        cli_reference_at(&path).unwrap_or_else(|error| panic!("{error}"))
    }

    fn table_cells(line: &str) -> Vec<String> {
        let mut cells = vec![String::new()];
        let mut escaped = false;
        for character in line.chars() {
            if escaped {
                cells
                    .last_mut()
                    .expect("a table row has one cell")
                    .push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '|' {
                cells.push(String::new());
            } else {
                cells
                    .last_mut()
                    .expect("a table row has one cell")
                    .push(character);
            }
        }
        cells
    }

    fn table_rows(section: &str, header: &str) -> Vec<String> {
        let Some(table) = section.split_once(header).map(|(_, table)| table) else {
            return Vec::new();
        };
        table
            .lines()
            .skip(1)
            .take_while(|line| line.starts_with('|'))
            .filter(|line| !line.contains("---"))
            .filter_map(|line| table_cells(line).into_iter().nth(1))
            .map(|cell| cell.trim().trim_matches('`').to_string())
            .collect()
    }

    fn section<'a>(reference: &'a str, heading: &str) -> Option<&'a str> {
        let start = reference.find(heading)? + heading.len();
        let level = heading
            .chars()
            .take_while(|character| *character == '#')
            .count();
        let end = reference[start..]
            .match_indices('\n')
            .find_map(|(offset, _)| {
                let line = &reference[start + offset + 1..];
                let next_level = line
                    .chars()
                    .take_while(|character| *character == '#')
                    .count();
                (next_level > 0 && next_level <= level).then_some(start + offset)
            })
            .unwrap_or(reference.len());
        Some(&reference[start..end])
    }

    fn first_code_block(section: &str) -> Option<&str> {
        let (_, after_open) = section.split_once("```\n")?;
        after_open.split_once("\n```").map(|(block, _)| block)
    }

    fn documented_tokens(rows: &[String]) -> std::collections::BTreeSet<String> {
        rows.iter()
            .flat_map(|row| row.split_whitespace())
            .map(|token| token.trim_end_matches(',').to_string())
            .filter(|token| token.starts_with('-'))
            .collect()
    }

    fn documented_command(row: &str) -> Option<String> {
        let name = row.split_whitespace().next()?;
        if let Some(name) = name.strip_prefix('[') {
            return Some(name.split_once(']')?.0.trim_matches('`').to_string());
        }
        Some(name.trim_matches('`').to_string())
    }

    fn command_path(command: &clap::Command, parent: &str) -> String {
        if parent.is_empty() {
            command.get_name().to_string()
        } else {
            format!("{parent} {}", command.get_name())
        }
    }

    fn check_cli_reference(command: &clap::Command, reference: &str) -> Vec<String> {
        fn check(
            command: &clap::Command,
            parent: &str,
            reference: &str,
            inherited: &std::collections::BTreeSet<String>,
            errors: &mut Vec<String>,
        ) {
            let path = command_path(command, parent);
            let depth = path.split_whitespace().count() - 1;
            let (synopsis, documented, commands) = if depth == 0 {
                let root_end = reference.find("\n## ").unwrap_or(reference.len());
                let root = &reference[..root_end];
                let globals = section(reference, "## Global options").unwrap_or("");
                (
                    first_code_block(root),
                    documented_tokens(&table_rows(globals, "| flag | meaning |")),
                    table_rows(root, "| command | does |"),
                )
            } else {
                let documented_path = path.strip_prefix("focusforge-cli ").unwrap_or(&path);
                let heading = format!("{} `{documented_path}`", "#".repeat(depth + 1));
                let body = match section(reference, &heading) {
                    Some(body) => body,
                    None => {
                        errors.push(format!("missing section `{path}`"));
                        return;
                    }
                };
                (
                    first_code_block(body),
                    documented_tokens(&table_rows(body, "| flag | meaning |")),
                    table_rows(body, "| subcommand | does |"),
                )
            };

            let expected_usage = command
                .clone()
                .render_usage()
                .to_string()
                .trim_start_matches("Usage: ")
                .to_string();
            if synopsis != Some(expected_usage.as_str()) {
                errors.push(format!("{path}: synopsis differs from `{expected_usage}`"));
            }

            let visible_subcommands: std::collections::BTreeSet<_> = command
                .get_subcommands()
                .filter(|child| !child.is_hide_set() && child.get_name() != "help")
                .map(|child| child.get_name().to_string())
                .collect();
            let hidden_subcommands: std::collections::BTreeSet<_> = command
                .get_subcommands()
                .filter(|child| child.is_hide_set())
                .map(|child| child.get_name().to_string())
                .collect();
            let documented_subcommands: std::collections::BTreeSet<_> = commands
                .iter()
                .filter_map(|row| documented_command(row))
                .collect();
            for name in visible_subcommands.difference(&documented_subcommands) {
                errors.push(format!("{path}: undocumented command `{name}`"));
            }
            for name in documented_subcommands.difference(&visible_subcommands) {
                if hidden_subcommands.contains(name) {
                    continue;
                }
                errors.push(format!("{path}: documented command `{name}` is absent"));
            }

            let visible_arguments: Vec<_> = command
                .get_arguments()
                .filter(|arg| !arg.is_hide_set())
                .filter(|arg| !matches!(arg.get_id().as_str(), "help" | "version"))
                .collect();
            let expected: std::collections::BTreeSet<_> = visible_arguments
                .iter()
                .flat_map(|arg| {
                    [
                        arg.get_short().map(|short| format!("-{short}")),
                        arg.get_long().map(|long| format!("--{long}")),
                    ]
                })
                .flatten()
                .collect();
            let hidden: std::collections::BTreeSet<_> = command
                .get_arguments()
                .filter(|arg| arg.is_hide_set())
                .flat_map(|arg| {
                    [
                        arg.get_short().map(|short| format!("-{short}")),
                        arg.get_long().map(|long| format!("--{long}")),
                    ]
                })
                .flatten()
                .collect();
            for name in expected.difference(&documented) {
                if inherited.contains(name) {
                    continue;
                }
                errors.push(format!("{path}: undocumented argument `{name}`"));
            }
            let permitted_without_gpu = ["--max-vram", "--cpu"];
            for name in documented.difference(&expected) {
                if inherited.contains(name)
                    || hidden.contains(name)
                    || matches!(name.as_str(), "--help" | "-h" | "--version" | "-V")
                {
                    continue;
                }
                if !cfg!(feature = "gpu")
                    && matches!(
                        path.as_str(),
                        "focusforge-cli stack" | "focusforge-cli align"
                    )
                    && permitted_without_gpu.contains(&name.as_str())
                {
                    continue;
                }
                errors.push(format!("{path}: documented argument `{name}` is absent"));
            }

            for child in command
                .get_subcommands()
                .filter(|child| !child.is_hide_set() && child.get_name() != "help")
            {
                let mut child_inherited = inherited.clone();
                child_inherited.extend(
                    visible_arguments
                        .iter()
                        .filter(|arg| arg.is_global_set())
                        .flat_map(|arg| {
                            [
                                arg.get_short().map(|short| format!("-{short}")),
                                arg.get_long().map(|long| format!("--{long}")),
                            ]
                        })
                        .flatten(),
                );
                check(child, &path, reference, &child_inherited, errors);
            }
        }

        fn paths(
            command: &clap::Command,
            parent: &str,
            known: &mut std::collections::BTreeSet<String>,
        ) {
            let path = command_path(command, parent);
            known.insert(path.clone());
            for child in command
                .get_subcommands()
                .filter(|child| child.get_name() != "help")
            {
                paths(child, &path, known);
            }
        }

        let mut errors = Vec::new();
        check(
            command,
            "",
            reference,
            &std::collections::BTreeSet::new(),
            &mut errors,
        );
        let mut known = std::collections::BTreeSet::new();
        paths(command, "", &mut known);
        for line in reference.lines() {
            let Some((hashes, title)) = line.split_once(' ') else {
                continue;
            };
            if hashes.len() < 2 || !hashes.chars().all(|character| character == '#') {
                continue;
            }
            let Some(title) = title
                .strip_prefix('`')
                .and_then(|title| title.strip_suffix('`'))
            else {
                continue;
            };
            if title.starts_with('-') {
                continue;
            }
            let path = format!("focusforge-cli {title}");
            if !known.contains(&path) {
                errors.push(format!("documented section `{title}` is absent"));
            }
        }
        errors
    }

    #[test]
    fn cli_reference_matches_parser() {
        let mut command = Cli::command();
        command.build();
        let errors = check_cli_reference(&command, &cli_reference());
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    #[test]
    fn cli_reference_check_reports_drift() {
        let mut command = Cli::command();
        command.build();
        let reference = cli_reference();
        assert_eq!(
            table_rows(
                "| flag | meaning |\n|---|---|\n| `--format <on\\|off>` | value |",
                "| flag | meaning |"
            ),
            ["--format <on|off>"]
        );
        let drifted = reference
            .replacen("| `--strict` |", "| `--strict-missing` |", 1)
            .replacen(
                "| `--algorithm <ALGO>` |",
                "| `--algorithm <ALGO>` |\n| `--frobnicate` | invented |",
                2,
            )
            .replacen("### `history list`", "### `history entries`", 1)
            .replacen("-o, --output", "-x, --output", 1)
            .replacen(
                "focusforge-cli stack [OPTIONS] <DIR>",
                "focusforge-cli stack [OPTIONS] <PATH>",
                1,
            );
        let errors = check_cli_reference(&command, &drifted).join("\n");
        for expected in [
            "focusforge-cli stack: undocumented argument `--strict`",
            "focusforge-cli align: documented argument `--frobnicate` is absent",
            "missing section `focusforge-cli history list`",
            "focusforge-cli stack: undocumented argument `-o`",
            "focusforge-cli stack: documented argument `-x` is absent",
            "focusforge-cli stack: synopsis differs",
        ] {
            assert!(
                errors.contains(expected),
                "missing {expected:?} in {errors}"
            );
        }

        let hidden = command.clone().arg(
            clap::Arg::new("hidden-reference-check")
                .long("hidden-reference-check")
                .hide(true),
        );
        let hidden_reference = reference.replacen(
            "| `-v` |",
            "| `--hidden-reference-check` | hidden |\n| `-v` |",
            1,
        );
        assert!(check_cli_reference(&hidden, &hidden_reference).is_empty());

        let hidden_subcommand = command.clone().subcommand(
            clap::Command::new("hidden-reference-command")
                .about("Hidden")
                .hide(true),
        );
        assert!(check_cli_reference(&hidden_subcommand, &reference).is_empty());
        let hidden_command_reference = reference.replacen(
            "| [`info`](#info) |",
            "| `hidden-reference-command` | hidden |\n| [`info`](#info) |",
            1,
        ) + "\n## `hidden-reference-command`\n\n```\nignored\n```\n";
        assert!(check_cli_reference(&hidden_subcommand, &hidden_command_reference).is_empty());

        let hidden_stack = command.clone().mut_subcommand("stack", |stack| {
            stack.arg(
                clap::Arg::new("hidden-stack-reference-check")
                    .long("hidden-stack-reference-check")
                    .hide(true),
            )
        });
        let hidden_stack_reference = reference.replacen(
            "| `--algorithm <ALGO>` | registered stacking algorithm id; required unless a `--preset` supplies `algo` (see `algorithms` below for the list) |",
            "| `--hidden-stack-reference-check` | hidden |\n| `--algorithm <ALGO>` | registered stacking algorithm id; required unless a `--preset` supplies `algo` (see `algorithms` below for the list) |",
            1,
        );
        assert!(check_cli_reference(&hidden_stack, &reference).is_empty());
        assert!(check_cli_reference(&hidden_stack, &hidden_stack_reference).is_empty());

        let missing_root_verbosity = reference.replacen(
            "| `-v` | increase log detail; repeat it for more detail |\n",
            "",
            1,
        );
        assert!(
            check_cli_reference(&command, &missing_root_verbosity)
                .join("\n")
                .contains("focusforge-cli: undocumented argument `-v`")
        );
        let missing_root_quiet = reference.replacen("| `-q, --quiet` | log errors only |\n", "", 1);
        let quiet_errors = check_cli_reference(&command, &missing_root_quiet).join("\n");
        assert!(quiet_errors.contains("focusforge-cli: undocumented argument `-q`"));
        assert!(quiet_errors.contains("focusforge-cli: undocumented argument `--quiet`"));

        let missing_cache_json = reference.replacen(
            "| `--json` | emit the JSON report instead of human-readable text |",
            "",
            1,
        );
        assert!(
            check_cli_reference(&command, &missing_cache_json)
                .join("\n")
                .contains("focusforge-cli cache: undocumented argument `--json`")
        );
        let without_inherited_cache_json = reference.replacen(
            "### `cache clear`\n\n```\nfocusforge-cli cache clear [OPTIONS]\n```\n\n| flag | meaning |\n|---|---|\n| `--json` | emit the JSON report instead of human-readable text |",
            "### `cache clear`\n\n```\nfocusforge-cli cache clear [OPTIONS]\n```\n\n| flag | meaning |\n|---|---|",
            1,
        );
        assert!(check_cli_reference(&command, &without_inherited_cache_json).is_empty());

        let generated_reference =
            reference.replacen("| `-v` |", "| `--help` | generated |\n| `-v` |", 1);
        assert!(check_cli_reference(&command, &generated_reference).is_empty());

        let without_inherited = reference.replacen(
            "| `-q, --quiet` | log errors only; results and required warnings remain visible |\n",
            "",
            1,
        );
        assert!(check_cli_reference(&command, &without_inherited).is_empty());

        let missing_local_override = reference.replacen(
            "| `--accept-changed-sources` | accept changed source frames and print their warning instead of refusing |\n",
            "",
            1,
        );
        assert!(
            check_cli_reference(&command, &missing_local_override)
                .join("\n")
                .contains(
                    "focusforge-cli history list: undocumented argument `--accept-changed-sources`"
                )
        );
        let parent_override = reference.replacen(
            "| flag | meaning |\n|---|---|\n\n### `history list`",
            "| flag | meaning |\n|---|---|\n| `--accept-changed-sources` | misplaced |\n\n### `history list`",
            1,
        );
        assert!(
            check_cli_reference(&command, &parent_override)
                .join("\n")
                .contains("focusforge-cli history: documented argument `--accept-changed-sources` is absent")
        );
        let compact_override = reference.replacen(
            "### `history compact`\n\n```\nfocusforge-cli history compact [OPTIONS] <PROJECT>\n```\n\n| flag | meaning |\n|---|---|",
            "### `history compact`\n\n```\nfocusforge-cli history compact [OPTIONS] <PROJECT>\n```\n\n| flag | meaning |\n|---|---|\n| `--accept-changed-sources` | misplaced |",
            1,
        );
        assert!(
            check_cli_reference(&command, &compact_override)
                .join("\n")
                .contains("focusforge-cli history compact: documented argument `--accept-changed-sources` is absent")
        );

        let invented_section = reference.replacen("## `review`", "## `frob`", 1);
        let invented_errors = check_cli_reference(&command, &invented_section).join("\n");
        assert!(invented_errors.contains("missing section `focusforge-cli review`"));
        assert!(invented_errors.contains("documented section `frob` is absent"));

        let drifted_child = reference.replacen(
            "| `list <PROJECT> [--json] [--accept-changed-sources]` |",
            "| `entries <PROJECT> [--json] [--accept-changed-sources]` |",
            1,
        );
        let child_errors = check_cli_reference(&command, &drifted_child).join("\n");
        assert!(child_errors.contains("focusforge-cli history: undocumented command `list`"));
        assert!(
            child_errors.contains("focusforge-cli history: documented command `entries` is absent")
        );

        let prose = reference.replacen("Every algorithm", "Every --frobnicate algorithm", 1);
        assert!(check_cli_reference(&command, &prose).is_empty());

        let visible = command
            .clone()
            .subcommand(clap::Command::new("reference-probe").about("Probe"));
        assert!(
            check_cli_reference(&visible, &reference)
                .join("\n")
                .contains("focusforge-cli: undocumented command `reference-probe`")
        );

        #[cfg(not(feature = "gpu"))]
        {
            let non_gpu =
                reference.replacen("| `--cpu` |", "| `--not-gpu` | invented |\n| `--cpu` |", 1);
            assert!(
                check_cli_reference(&command, &non_gpu)
                    .join("\n")
                    .contains("focusforge-cli stack: documented argument `--not-gpu` is absent")
            );
            let wrong_owner = reference.replacen(
                "| `--json` | emit the JSON document instead of human-readable text |",
                "| `--json` | emit the JSON document instead of human-readable text |\n| `--cpu` | invented |",
                1,
            );
            assert!(
                check_cli_reference(&command, &wrong_owner)
                    .join("\n")
                    .contains("focusforge-cli info: documented argument `--cpu` is absent")
            );
        }

        #[cfg(feature = "gpu")]
        {
            let missing_gpu = reference
                .replacen("| `--max-vram <SIZE>` |", "| `--vram-missing <SIZE>` |", 2)
                .replacen("| `--cpu` |", "| `--cpu-missing` |", 2);
            let gpu_errors = check_cli_reference(&command, &missing_gpu).join("\n");
            for expected in [
                "focusforge-cli stack: undocumented argument `--max-vram`",
                "focusforge-cli stack: undocumented argument `--cpu`",
                "focusforge-cli align: undocumented argument `--max-vram`",
                "focusforge-cli align: undocumented argument `--cpu`",
            ] {
                assert!(
                    gpu_errors.contains(expected),
                    "missing {expected:?} in {gpu_errors}"
                );
            }
        }
    }

    #[test]
    fn cli_reference_load_error_names_the_absolute_path() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("missing-cli-reference.md");
        let error = cli_reference_at(&path).expect_err("missing reference must fail");
        assert!(error.contains(&path.display().to_string()));
    }

    const CLI_ASSET_REGENERATION: &str = "FOCUSFORGE_REGENERATE_CLI_ASSETS=1 cargo test -p focusforge-cli --bin focusforge-cli tests::regenerate_cli_assets -- --exact";

    #[cfg(feature = "gpu")]
    fn cli_assets_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets")
    }

    fn visible_commands(
        command: &clap::Command,
        names: &mut Vec<String>,
        pages: &mut std::collections::BTreeMap<String, Vec<u8>>,
        version: &'static str,
    ) {
        let name = names.join("-");
        let mut page = command
            .clone()
            .display_name(name.clone())
            .version(version)
            .disable_help_subcommand(true);
        page.build();
        let man = clap_mangen::Man::new(page);
        let mut bytes = Vec::new();
        man.render(&mut bytes)
            .expect("a Vec accepts generated manual output");
        pages.insert(man.get_filename(), bytes);
        for child in command
            .get_subcommands()
            .filter(|child| !child.is_hide_set() && child.get_name() != "help")
        {
            names.push(child.get_name().to_owned());
            visible_commands(child, names, pages, version);
            names.pop();
        }
    }

    // The generated Bash completion is for Unix shells; a Windows runner's `bash` is not one.
    #[cfg(unix)]
    fn bash_callback(script: &std::path::Path, context: &str, invocation: &str) -> Vec<String> {
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "source \"$1\"\n{context}\nCOMPREPLY=()\n{invocation}\nprintf '%s\\n' \"${{COMPREPLY[@]}}\""
            ))
            .arg("bash")
            .arg(script)
            .output()
            .expect("bash is available for completion generation tests");
        assert!(
            output.status.success(),
            "Bash callback failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Bash completion output is UTF-8")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn generated_cli_bash_callback_completes_root_nested_commands_and_options() {
        let temporary = tempfile::tempdir().expect("temporary completion directory");
        let script = temporary.path().join("focusforge-cli.bash");
        std::fs::write(
            &script,
            generated_cli_assets()
                .remove(std::path::Path::new("completions/focusforge-cli.bash"))
                .expect("Bash completion is generated"),
        )
        .expect("write Bash completion");

        let root = bash_callback(
            &script,
            "COMP_WORDS=(focusforge-cli '')\nCOMP_CWORD=1\nCOMP_LINE='focusforge-cli '\nCOMP_POINT=${#COMP_LINE}",
            "_focusforge__cli focusforge-cli ''",
        );
        for expected in ["info", "history", "cache", "stack"] {
            assert!(
                root.contains(&expected.to_owned()),
                "root lacks {expected}: {root:?}"
            );
        }

        let history = bash_callback(
            &script,
            "COMP_WORDS=(focusforge-cli history '')\nCOMP_CWORD=2\nCOMP_LINE='focusforge-cli history '\nCOMP_POINT=${#COMP_LINE}",
            "_focusforge__cli focusforge-cli '' history",
        );
        for expected in ["list", "delete", "compact"] {
            assert!(
                history.contains(&expected.to_owned()),
                "history lacks {expected}: {history:?}"
            );
        }

        let cache = bash_callback(
            &script,
            "COMP_WORDS=(focusforge-cli cache '')\nCOMP_CWORD=2\nCOMP_LINE='focusforge-cli cache '\nCOMP_POINT=${#COMP_LINE}",
            "_focusforge__cli focusforge-cli '' cache",
        );
        assert!(
            cache.contains(&"clear".to_owned()),
            "cache lacks clear: {cache:?}"
        );

        let options = bash_callback(
            &script,
            "COMP_WORDS=(focusforge-cli history list --)\nCOMP_CWORD=3\nCOMP_LINE='focusforge-cli history list --'\nCOMP_POINT=${#COMP_LINE}",
            "_focusforge__cli focusforge-cli -- list",
        );
        for expected in ["--json", "--accept-changed-sources"] {
            assert!(
                options.contains(&expected.to_owned()),
                "history list lacks {expected}: {options:?}"
            );
        }
    }

    fn generated_cli_assets_for(
        mut command: clap::Command,
        version: &'static str,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        use clap_complete::{
            aot,
            aot::{Bash, Fish, PowerShell, Zsh},
        };

        command.build();
        let mut manuals = std::collections::BTreeMap::new();
        visible_commands(
            &command,
            &mut vec![command.get_name().to_owned()],
            &mut manuals,
            version,
        );
        let mut assets = manuals
            .into_iter()
            .map(|(name, bytes)| (std::path::PathBuf::from("man").join(name), bytes))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut bash = command.clone();
        let mut bash_bytes = Vec::new();
        aot::generate(Bash, &mut bash, "focusforge-cli", &mut bash_bytes);
        let bash = String::from_utf8(bash_bytes)
            .expect("clap Bash completion is UTF-8")
            .replace("focusforge__subcmd__cli", "focusforge__cli")
            .into_bytes();
        assets.insert(
            std::path::PathBuf::from("completions").join("focusforge-cli.bash"),
            bash,
        );
        macro_rules! completion {
            ($shell:expr, $name:literal) => {{
                let mut command = command.clone();
                let mut bytes = Vec::new();
                aot::generate($shell, &mut command, "focusforge-cli", &mut bytes);
                assets.insert(std::path::PathBuf::from("completions").join($name), bytes);
            }};
        }
        completion!(Zsh, "_focusforge-cli");
        completion!(Fish, "focusforge-cli.fish");
        completion!(PowerShell, "_focusforge-cli.ps1");
        assets
    }

    fn generated_cli_assets() -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        use clap::CommandFactory;

        generated_cli_assets_for(Cli::command(), env!("CARGO_PKG_VERSION"))
    }

    fn regeneration_error(path: &std::path::Path, error: std::io::Error) -> String {
        format!(
            "cannot read {}: {error}; run {CLI_ASSET_REGENERATION}",
            path.display()
        )
    }

    fn compare_cli_assets(root: &std::path::Path) -> Result<(), Vec<String>> {
        let generated = generated_cli_assets();
        let mut actual = std::collections::BTreeMap::new();
        for directory in ["man", "completions"] {
            let path = root.join(directory);
            let entries =
                std::fs::read_dir(&path).map_err(|error| vec![regeneration_error(&path, error)])?;
            for entry in entries {
                let entry = entry.map_err(|error| vec![regeneration_error(&path, error)])?;
                let file_type = entry
                    .file_type()
                    .map_err(|error| vec![regeneration_error(&entry.path(), error)])?;
                if file_type.is_file() {
                    let relative = std::path::PathBuf::from(directory).join(entry.file_name());
                    actual.insert(
                        relative.clone(),
                        std::fs::read(entry.path())
                            .map_err(|error| vec![regeneration_error(&entry.path(), error)])?,
                    );
                }
            }
        }
        let mut errors = Vec::new();
        let paths = generated
            .keys()
            .chain(actual.keys())
            .collect::<std::collections::BTreeSet<_>>();
        for path in paths {
            // Reported with `/` on every platform, as the paths are written in the repository.
            let shown = path.to_string_lossy().replace('\\', "/");
            match (generated.get(path), actual.get(path)) {
                (Some(expected), Some(found)) if expected != found => {
                    errors.push(format!("{shown} differs; run {CLI_ASSET_REGENERATION}"))
                }
                (Some(_), None) => {
                    errors.push(format!("{shown} is missing; run {CLI_ASSET_REGENERATION}"))
                }
                (None, Some(_)) => {
                    errors.push(format!("{shown} is orphaned; run {CLI_ASSET_REGENERATION}"))
                }
                _ => {}
            }
        }
        errors.sort();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    #[cfg(feature = "gpu")]
    fn write_cli_assets(
        root: &std::path::Path,
        generated: &std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) -> Result<(), String> {
        for name in ["man", "completions"] {
            let directory = root.join(name);
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
            for entry in std::fs::read_dir(&directory)
                .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
            {
                let entry = entry
                    .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
                let relative = std::path::PathBuf::from(name).join(entry.file_name());
                if entry
                    .file_type()
                    .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?
                    .is_file()
                    && !generated.contains_key(&relative)
                {
                    std::fs::remove_file(entry.path()).map_err(|error| {
                        format!("cannot remove {}: {error}", entry.path().display())
                    })?;
                }
            }
        }
        for (path, bytes) in generated {
            std::fs::write(root.join(path), bytes)
                .map_err(|error| format!("cannot write {}: {error}", root.join(path).display()))?;
        }
        Ok(())
    }

    #[cfg(feature = "gpu")]
    fn regeneration_requested(value: Option<&std::ffi::OsStr>) -> bool {
        value == Some(std::ffi::OsStr::new("1"))
    }

    fn copy_generated_cli_assets(root: &std::path::Path) {
        for (path, bytes) in generated_cli_assets() {
            let destination = root.join(path);
            std::fs::create_dir_all(destination.parent().expect("asset has a parent"))
                .expect("asset parent");
            std::fs::write(destination, bytes).expect("asset copy");
        }
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn generated_cli_files_match_the_parser() {
        let first = generated_cli_assets();
        assert_eq!(first, generated_cli_assets());
        assert_eq!(
            first.keys().filter(|path| path.starts_with("man")).count(),
            15
        );
        let manuals = first
            .keys()
            .filter(|path| path.starts_with("man"))
            .map(|path| path.as_path())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            manuals,
            std::collections::BTreeSet::from([
                std::path::Path::new("man/focusforge-cli.1"),
                std::path::Path::new("man/focusforge-cli-info.1"),
                std::path::Path::new("man/focusforge-cli-algorithms.1"),
                std::path::Path::new("man/focusforge-cli-stack.1"),
                std::path::Path::new("man/focusforge-cli-export.1"),
                std::path::Path::new("man/focusforge-cli-retouch.1"),
                std::path::Path::new("man/focusforge-cli-history.1"),
                std::path::Path::new("man/focusforge-cli-align.1"),
                std::path::Path::new("man/focusforge-cli-review.1"),
                std::path::Path::new("man/focusforge-cli-cache.1"),
                std::path::Path::new("man/focusforge-cli-diagnostics.1"),
                std::path::Path::new("man/focusforge-cli-history-list.1"),
                std::path::Path::new("man/focusforge-cli-history-delete.1"),
                std::path::Path::new("man/focusforge-cli-history-compact.1"),
                std::path::Path::new("man/focusforge-cli-cache-clear.1"),
            ])
        );
        assert!(
            first
                .values()
                .all(|bytes| !String::from_utf8_lossy(bytes).contains("--lax"))
        );
        let nested = String::from_utf8_lossy(
            first
                .get(std::path::Path::new("man/focusforge-cli-cache-clear.1"))
                .expect("nested manual is generated"),
        );
        assert!(nested.contains("\\fB\\-q\\fR"));
        assert!(nested.contains("\\fB\\-\\-quiet\\fR"));
        let history_nested = String::from_utf8_lossy(
            first
                .get(std::path::Path::new("man/focusforge-cli-history-list.1"))
                .expect("nested history manual is generated"),
        );
        assert!(history_nested.contains("\\fB\\-q\\fR"));
        assert!(history_nested.contains(env!("CARGO_PKG_VERSION")));
        for completion in [
            "completions/focusforge-cli.bash",
            "completions/_focusforge-cli",
            "completions/focusforge-cli.fish",
            "completions/_focusforge-cli.ps1",
        ] {
            let text = String::from_utf8_lossy(
                first
                    .get(std::path::Path::new(completion))
                    .expect("completion is generated"),
            );
            assert!(text.contains("--zmap") || text.contains("-l zmap"));
            assert!(text.contains("--quiet") || text.contains("-l quiet"));
            assert!(text.contains("--cpu") || text.contains("-l cpu"));
            assert!(text.contains("--max-vram") || text.contains("-l max-vram"));
        }
        compare_cli_assets(&cli_assets_root())
            .unwrap_or_else(|errors| panic!("{}", errors.join("\n")));
    }

    #[test]
    fn generated_cli_files_report_staleness() {
        let temporary = tempfile::tempdir().expect("temporary assets directory");
        copy_generated_cli_assets(temporary.path());
        let changed = temporary.path().join("man/focusforge-cli.1");
        std::fs::write(&changed, b"changed").expect("changed manual");
        let missing = temporary.path().join("man/focusforge-cli-info.1");
        std::fs::remove_file(&missing).expect("remove manual");
        let changed_completion = temporary.path().join("completions/focusforge-cli.bash");
        std::fs::write(&changed_completion, b"changed").expect("changed completion");
        let missing_completion = temporary.path().join("completions/focusforge-cli.fish");
        std::fs::remove_file(&missing_completion).expect("remove completion");
        std::fs::write(temporary.path().join("man/orphan.1"), b"orphan").expect("orphan manual");
        std::fs::write(temporary.path().join("completions/orphan"), b"orphan")
            .expect("orphan completion");
        let errors = compare_cli_assets(temporary.path())
            .expect_err("mutations must be reported")
            .join("\n");
        assert!(errors.contains("man/focusforge-cli.1 differs"));
        assert!(errors.contains("man/focusforge-cli-info.1 is missing"));
        assert!(errors.contains("man/orphan.1 is orphaned"));
        assert!(errors.contains("completions/focusforge-cli.bash differs"));
        assert!(errors.contains("completions/focusforge-cli.fish is missing"));
        assert!(errors.contains("completions/orphan is orphaned"));
        assert!(errors.contains(CLI_ASSET_REGENERATION));
    }

    #[test]
    fn generated_cli_asset_variations_cover_visible_hidden_and_versioned_commands() {
        use clap::CommandFactory;

        let mut command = Cli::command();
        command = command
            .subcommand(clap::Command::new("visible-test"))
            .subcommand(clap::Command::new("hidden-test").hide(true))
            .arg(
                clap::Arg::new("hidden-test-argument")
                    .long("hidden-test-argument")
                    .hide(true),
            );
        let assets = generated_cli_assets_for(command, "9.9.9");
        let manuals = assets
            .keys()
            .filter(|path| path.starts_with("man"))
            .collect::<Vec<_>>();
        assert_eq!(manuals.len(), 16);
        assert!(assets.contains_key(std::path::Path::new("man/focusforge-cli-visible-test.1")));
        assert!(!assets.contains_key(std::path::Path::new("man/focusforge-cli-hidden-test.1")));
        assert!(manuals.iter().all(|path| {
            let bytes = &assets[*path];
            let text = String::from_utf8_lossy(bytes);
            !text.contains("hidden-test-argument") && !text.contains("hidden-test")
        }));
        assert!(
            manuals
                .iter()
                .all(|path| String::from_utf8_lossy(&assets[*path]).contains("9.9.9"))
        );
    }

    #[test]
    fn generated_cli_files_report_filesystem_failures_without_writing() {
        let temporary = tempfile::tempdir().expect("temporary assets directory");
        copy_generated_cli_assets(temporary.path());
        let manual_directory = temporary.path().join("man");
        std::fs::remove_dir_all(&manual_directory).expect("remove manual directory");
        std::fs::write(&manual_directory, b"not a directory").expect("replace manual directory");
        let error = compare_cli_assets(temporary.path())
            .expect_err("non-directory must fail")
            .join("\n");
        assert!(error.contains(&manual_directory.display().to_string()));
        assert!(error.contains(CLI_ASSET_REGENERATION));
    }

    #[test]
    fn generated_cli_comparison_is_read_only() {
        let temporary = tempfile::tempdir().expect("temporary assets directory");
        copy_generated_cli_assets(temporary.path());
        let before = std::fs::read(temporary.path().join("man/focusforge-cli.1")).expect("manual");
        compare_cli_assets(temporary.path()).expect("comparison succeeds");
        assert_eq!(
            before,
            std::fs::read(temporary.path().join("man/focusforge-cli.1")).expect("manual")
        );
    }

    #[test]
    fn generated_cli_cpu_generation_is_deterministic_and_does_not_use_default_goldens() {
        let first = generated_cli_assets();
        assert_eq!(first, generated_cli_assets());
        assert_eq!(
            first.keys().filter(|path| path.starts_with("man")).count(),
            15
        );
        assert!(first.contains_key(std::path::Path::new("completions/_focusforge-cli.ps1")));
        #[cfg(not(feature = "gpu"))]
        assert!(first.values().all(|bytes| {
            let text = String::from_utf8_lossy(bytes);
            !text.contains("--cpu") && !text.contains("--max-vram")
        }));
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn cli_asset_writer_requires_exact_opt_in_and_is_idempotent() {
        assert!(!regeneration_requested(None));
        assert!(!regeneration_requested(Some(std::ffi::OsStr::new(""))));
        assert!(!regeneration_requested(Some(std::ffi::OsStr::new("0"))));
        assert!(regeneration_requested(Some(std::ffi::OsStr::new("1"))));

        let temporary = tempfile::tempdir().expect("temporary assets directory");
        let generated = generated_cli_assets();
        std::fs::create_dir_all(temporary.path().join("man/subdirectory"))
            .expect("owned directory");
        std::fs::write(temporary.path().join("man/orphan.1"), b"orphan").expect("manual orphan");
        std::fs::create_dir_all(temporary.path().join("completions"))
            .expect("completion directory");
        std::fs::write(temporary.path().join("completions/orphan"), b"orphan")
            .expect("completion orphan");
        std::fs::write(temporary.path().join("outside"), b"outside").expect("outside file");
        write_cli_assets(temporary.path(), &generated).expect("first writer pass");
        let first = generated
            .keys()
            .map(|path| {
                (
                    path.clone(),
                    std::fs::read(temporary.path().join(path)).expect("written asset"),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert!(!temporary.path().join("man/orphan.1").exists());
        assert!(!temporary.path().join("completions/orphan").exists());
        assert!(temporary.path().join("man/subdirectory").is_dir());
        assert_eq!(
            std::fs::read(temporary.path().join("outside")).expect("outside file"),
            b"outside"
        );
        write_cli_assets(temporary.path(), &generated).expect("second writer pass");
        let second = generated
            .keys()
            .map(|path| {
                (
                    path.clone(),
                    std::fs::read(temporary.path().join(path)).expect("written asset"),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(first, second);
        compare_cli_assets(temporary.path()).expect("writer output matches parser");
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn regenerate_cli_assets() {
        if !regeneration_requested(std::env::var_os("FOCUSFORGE_REGENERATE_CLI_ASSETS").as_deref())
        {
            return;
        }
        write_cli_assets(&cli_assets_root(), &generated_cli_assets())
            .expect("write generated CLI assets");
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
