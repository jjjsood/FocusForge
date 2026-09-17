// SPDX-License-Identifier: GPL-3.0-or-later

//! One shared `tracing` subscriber for `focusforge-cli` and
//! `focusforge-gui`: a human-readable stderr sink and a JSON Lines file
//! sink, both fed by `tracing` events and, through the `tracing-log`
//! bridge, by `log`-crate records (`wgpu`, `naga`).

mod rotating;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

use rotating::RotatingFile;

/// This workspace's own `tracing` targets, filtered independently of every
/// third-party crate.
const WORKSPACE_TARGETS: [&str; 5] = [
    "focusforge_core",
    "focusforge_gpu",
    "focusforge_cli",
    "focusforge_gui",
    "focusforge_diagnostics",
];

/// The live log file's rotation threshold and retention count: an ~48 MiB
/// ceiling (one 8 MiB live file plus five rotated ones).
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const KEEP_ROTATED: u32 = 5;

/// `<FOCUSFORGE_DATA_DIR>/focusforge` when that variable is set, else the
/// platform's own per-user data directory for `focusforge`. The only
/// function in this crate that reads the environment.
pub fn data_dir_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FOCUSFORGE_DATA_DIR") {
        return Some(PathBuf::from(dir).join("focusforge"));
    }
    directories::ProjectDirs::from("", "", "focusforge").map(|d| d.data_dir().to_path_buf())
}

/// Configuration for [`init`].
pub struct Options {
    /// Where the log file's directory is resolved from, typically
    /// [`data_dir_from_env`]'s result. `None` disables the file sink.
    pub data_dir: Option<PathBuf>,
    /// The CLI's `-v` count, or `-1` when `-q` was given. `0` is the
    /// default: WARN and above on stderr.
    pub verbosity: i8,
}

/// The subscriber's resolved state, returned by [`init`].
pub struct Diagnostics {
    log_path: Option<PathBuf>,
}

impl Diagnostics {
    /// The JSON Lines log file's path, or `None` when the file sink could
    /// not be opened.
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }
}

/// Why the file sink did not open.
enum NoFileSink {
    /// No data directory resolved at all.
    NoDataDir,
    /// The directory could not be created, or the file could not be opened.
    Io {
        dir: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for NoFileSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoFileSink::NoDataDir => {
                write!(f, "note: writing no log file: no data directory resolved")
            }
            NoFileSink::Io { dir, source } => {
                write!(f, "note: writing no log file: {}: {source}", dir.display())
            }
        }
    }
}

fn open_file_sink(data_dir: Option<&Path>) -> Result<(RotatingFile, PathBuf), NoFileSink> {
    let data_dir = data_dir.ok_or(NoFileSink::NoDataDir)?;
    let log_dir = data_dir.join("logs");
    let log_path = log_dir.join("focusforge.jsonl");
    RotatingFile::open(&log_path, MAX_LOG_BYTES, KEEP_ROTATED)
        .map(|writer| (writer, log_path))
        .map_err(|source| NoFileSink::Io {
            dir: log_dir,
            source,
        })
}

/// The stderr sink's level for each workspace target, at a given
/// `-v`/`-q` verbosity: ERROR below 0, WARN at 0, INFO at 1, DEBUG at 2,
/// TRACE from 3.
fn stderr_level(verbosity: i8) -> LevelFilter {
    match verbosity {
        i8::MIN..=-1 => LevelFilter::ERROR,
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    }
}

/// The stderr sink's level for every target outside this workspace: `OFF`
/// below verbosity 2, so an ordinary run's stderr carries no `wgpu` or
/// `naga` warning.
fn stderr_default_level(verbosity: i8) -> LevelFilter {
    if verbosity >= 2 {
        stderr_level(verbosity)
    } else {
        LevelFilter::OFF
    }
}

/// The `log`-crate bridge's cap: below this, a `log` record is never
/// converted into a `tracing` event at all, which is what keeps
/// third-party TRACE chatter out of a release build's file (a compile-time
/// `tracing` cap cannot see records arriving through this bridge) and off
/// the timing budget.
fn bridge_cap(verbosity: i8) -> log::LevelFilter {
    if verbosity >= 2 {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Warn
    }
}

/// The stderr layer's per-target filter: [`WORKSPACE_TARGETS`] follow the
/// `-v`/`-q` verbosity, every other target follows the default level.
fn stderr_targets(verbosity: i8) -> Targets {
    Targets::new()
        .with_default(stderr_default_level(verbosity))
        .with_targets(WORKSPACE_TARGETS.map(|t| (t, stderr_level(verbosity))))
}

/// The file layer's per-target filter: fixed regardless of `-v`/`-q`, so a
/// crash record looks the same whatever flags the run carried.
fn file_targets() -> Targets {
    Targets::new()
        .with_default(LevelFilter::WARN)
        .with_targets(WORKSPACE_TARGETS.map(|t| (t, LevelFilter::DEBUG)))
}

/// Installs the process-wide subscriber and the `tracing-log` bridge, and
/// emits the run's first event. Call once, as the first statement of
/// `main`; a second call panics on the already-installed subscriber. The
/// file sink is opened at `<data dir>/logs/focusforge.jsonl`; when it
/// cannot open, the stderr sink runs alone, [`Diagnostics::log_path`] stays
/// `None` and one note naming the directory goes to stderr.
pub fn init(options: &Options) -> Diagnostics {
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_filter(stderr_targets(options.verbosity));

    let (file_layer, log_path) = match open_file_sink(options.data_dir.as_deref()) {
        Ok((writer, path)) => {
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_writer(writer)
                .with_filter(file_targets());
            (Some(layer), Some(path))
        }
        Err(NoFileSink::NoDataDir) => (None, None),
        Err(err) => {
            eprintln!("{err}");
            (None, None)
        }
    };

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();

    let _ = tracing_log::LogTracer::builder()
        .with_max_level(bridge_cap(options.verbosity))
        .init();

    let binary = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "focusforge".to_string());
    tracing::info!(
        target: "focusforge_diagnostics",
        binary = %binary,
        version = env!("CARGO_PKG_VERSION"),
        "starting"
    );

    Diagnostics { log_path }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn stderr_filter_excludes_third_party_targets_at_default_verbosity() {
        let filter = stderr_targets(0);
        assert!(!filter.would_enable("some_third_party_crate", &Level::ERROR));
        assert!(filter.would_enable("focusforge_core", &Level::WARN));
        assert!(!filter.would_enable("focusforge_core", &Level::INFO));
    }

    #[test]
    fn stderr_filter_verbosity_mapping() {
        assert!(stderr_targets(-1).would_enable("focusforge_cli", &Level::ERROR));
        assert!(!stderr_targets(-1).would_enable("focusforge_cli", &Level::WARN));
        assert!(stderr_targets(1).would_enable("focusforge_cli", &Level::INFO));
        assert!(stderr_targets(2).would_enable("focusforge_cli", &Level::DEBUG));
        assert!(stderr_targets(3).would_enable("focusforge_cli", &Level::TRACE));
    }

    #[test]
    fn stderr_filter_lifts_third_party_default_from_verbosity_two() {
        assert!(!stderr_targets(1).would_enable("some_third_party_crate", &Level::DEBUG));
        assert!(stderr_targets(2).would_enable("some_third_party_crate", &Level::DEBUG));
    }

    #[test]
    fn file_filter_accepts_any_target_at_warn_and_above_independent_of_verbosity() {
        let filter = file_targets();
        assert!(filter.would_enable("some_third_party_crate", &Level::WARN));
        assert!(!filter.would_enable("some_third_party_crate", &Level::INFO));
        assert!(filter.would_enable("focusforge_core", &Level::DEBUG));
        assert!(!filter.would_enable("focusforge_core", &Level::TRACE));
    }
}
