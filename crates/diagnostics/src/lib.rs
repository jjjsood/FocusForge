// SPDX-License-Identifier: GPL-3.0-or-later

//! One shared `tracing` subscriber for `focusforge-cli` and
//! `focusforge-gui`: a human-readable stderr sink and a JSON Lines file
//! sink, both fed by `tracing` events and, through the `tracing-log`
//! bridge, by `log`-crate records (`wgpu`, `naga`).

pub mod bundle;
mod crash;
pub mod crash_notice;
mod native;
mod ring;
mod rotating;
#[cfg(feature = "trace")]
mod trace;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

pub use crash::{crash_dir, rfc3339};
pub use native::run_monitor;
use ring::Ring;
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

/// The target for an event that repeats, for the log file alone, a line the
/// program has already printed on stderr itself. Excluded from the stderr
/// sink at every verbosity, so such a line is never printed twice.
pub const MIRRORED_OUTPUT_TARGET: &str = "focusforge::mirrored_output";

/// The live log file's rotation threshold and retention count: an ~48 MiB
/// ceiling (one 8 MiB live file plus five rotated ones).
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const KEEP_ROTATED: u32 = 5;

/// `<FOCUSFORGE_DATA_DIR>/focusforge` when that variable is set, else the
/// platform's own per-user data directory for `focusforge`.
pub fn data_dir_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FOCUSFORGE_DATA_DIR") {
        return Some(PathBuf::from(dir).join("focusforge"));
    }
    directories::ProjectDirs::from("", "", "focusforge").map(|d| d.data_dir().to_path_buf())
}

/// `<FOCUSFORGE_CONFIG_DIR>/focusforge` when that variable is set, else the
/// platform's own per-user configuration directory for `focusforge` -- the
/// directory the graphical shell keeps its input map in.
pub fn config_dir_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FOCUSFORGE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("focusforge"));
    }
    directories::ProjectDirs::from("", "", "focusforge").map(|d| d.config_dir().to_path_buf())
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
    /// Kept for the run's whole length: dropping it detaches the crash
    /// handler and lets the monitor process exit.
    _native: Option<native::Guard>,
    /// Kept until the process returns from `main`: dropping it flushes the
    /// Chrome trace and closes its JSON array. A process that exits without
    /// running this destructor leaves an unterminated file.
    #[cfg(feature = "trace")]
    _chrome: Option<tracing_chrome::FlushGuard>,
    #[cfg(feature = "trace")]
    trace_accounting: bool,
}

impl Diagnostics {
    /// The JSON Lines log file's path, or `None` when the file sink could
    /// not be opened.
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }
}

impl Drop for Diagnostics {
    fn drop(&mut self) {
        #[cfg(feature = "trace")]
        if self.trace_accounting {
            focusforge_core::timing::emit_timing_summary();
        }
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

/// Where a data directory keeps its log files.
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

fn open_file_sink(data_dir: Option<&Path>) -> Result<(RotatingFile, PathBuf), NoFileSink> {
    let data_dir = data_dir.ok_or(NoFileSink::NoDataDir)?;
    let log_dir = log_dir(data_dir);
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
fn bridge_cap(verbosity: i8) -> tracing_log::log::LevelFilter {
    if verbosity >= 2 {
        tracing_log::log::LevelFilter::Debug
    } else {
        tracing_log::log::LevelFilter::Warn
    }
}

/// The stderr layer's per-target filter: [`WORKSPACE_TARGETS`] follow the
/// `-v`/`-q` verbosity, [`MIRRORED_OUTPUT_TARGET`] is off whatever the
/// verbosity, every other target follows the default level.
fn stderr_targets(verbosity: i8) -> Targets {
    Targets::new()
        .with_default(stderr_default_level(verbosity))
        .with_targets(WORKSPACE_TARGETS.map(|t| (t, stderr_level(verbosity))))
        .with_target(MIRRORED_OUTPUT_TARGET, LevelFilter::OFF)
}

/// The file layer's per-target filter: fixed regardless of `-v`/`-q`, so a
/// crash record looks the same whatever flags the run carried.
fn file_targets() -> Targets {
    Targets::new()
        .with_default(LevelFilter::WARN)
        .with_targets(WORKSPACE_TARGETS.map(|t| (t, LevelFilter::DEBUG)))
        .with_target(MIRRORED_OUTPUT_TARGET, LevelFilter::DEBUG)
}

/// The flight recorder's per-target filter: DEBUG for every target, so an
/// event a third-party crate raises below the file layer's default still
/// reaches a crash report.
fn ring_targets() -> Targets {
    Targets::new().with_default(LevelFilter::DEBUG)
}

/// This process's own executable name, as both the startup event and a
/// crash report's file name carry it.
fn binary_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "focusforge".to_string())
}

/// A panic payload as text: the `&str` and `String` forms a `panic!` can
/// produce, and a fixed stand-in for anything else.
pub fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// The path of the most recent crash report this process wrote, or `None`
/// when no panic has been reported.
pub fn last_crash_report() -> Option<PathBuf> {
    crash::last_report()
}

/// Configures rayon's global pool: named workers, so a crash report names
/// which pool a panic came from, and a handler for the panics rayon cannot
/// propagate back to a caller (`spawn` and friends), which would otherwise
/// abort the process. A pool another caller already built keeps its own
/// configuration.
fn configure_global_pool() {
    let _ = rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("focusforge-worker-{i}"))
        .panic_handler(|payload| {
            tracing::error!(
                target: "focusforge_diagnostics",
                payload = %panic_payload(&*payload),
                "a worker thread panicked"
            );
        })
        .build_global();
}

/// Installs the process-wide subscriber, the `tracing-log` bridge, the
/// panic hook and the global pool's configuration, and emits the run's
/// first event. Call once, as the first statement of `main`; a second call
/// panics on the already-installed subscriber. The file sink is opened at
/// `<data dir>/logs/focusforge.jsonl` and crash reports go to
/// `<data dir>/crashes/`; when the file cannot open, the stderr sink runs
/// alone and one note naming the directory goes there.
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

    let ring = Ring::new();
    let ring_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(ring.clone())
        .with_filter(ring_targets());

    let subscriber = tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .with(ring_layer);

    #[cfg(feature = "trace")]
    let chrome = trace::chrome();
    #[cfg(feature = "trace")]
    let trace_accounting = chrome.layer.is_some();
    #[cfg(feature = "trace")]
    let subscriber = subscriber.with(chrome.layer).with(trace::tracy());

    subscriber.init();

    #[cfg(feature = "trace")]
    if trace_accounting {
        focusforge_core::timing::enable_trace_accounting();
    }

    // After `.init()`: a warning raised before the subscriber is installed
    // reaches no sink at all.
    #[cfg(feature = "trace")]
    if let Some((path, error)) = &chrome.refused {
        tracing::warn!(
            target: "focusforge_diagnostics",
            path = %path.display(),
            error = %error,
            "writing no trace file"
        );
    }

    let _ = tracing_log::LogTracer::builder()
        .with_max_level(bridge_cap(options.verbosity))
        .init();

    let binary = binary_name();
    let version = env!("CARGO_PKG_VERSION");
    let mut guard = None;
    if let Some(data_dir) = options.data_dir.as_deref() {
        let crash_dir = crash::crash_dir(data_dir);
        crash::install_hook(crash_dir.clone(), ring, binary.clone());
        guard = match native::attach(&crash_dir, &binary, version) {
            Ok(guard) => guard,
            Err(e) => {
                tracing::warn!(
                    target: "focusforge_diagnostics",
                    error = %e,
                    "native crashes will not be captured"
                );
                None
            }
        };
    }
    configure_global_pool();

    tracing::info!(
        target: "focusforge_diagnostics",
        binary = %binary,
        version,
        "starting"
    );

    Diagnostics {
        log_path,
        _native: guard,
        #[cfg(feature = "trace")]
        _chrome: chrome.guard,
        #[cfg(feature = "trace")]
        trace_accounting,
    }
}

/// Crashes when the `FF_FORCE_CRASH` environment variable names `site`, so
/// the crash path can be exercised on a real binary. At `"startup"`:
/// `panic` and `pool` panic on the calling thread and on a pool worker,
/// `segv`, `stack-overflow` and `abort` raise the native crash they name.
/// At `"stack"`, `stack` panics for a run's first stack alone. Any other
/// value does nothing, as does an unset variable.
pub fn force_crash(site: &str) {
    let Some(kind) = std::env::var_os("FF_FORCE_CRASH") else {
        return;
    };
    match (site, kind.to_string_lossy().as_ref()) {
        ("startup", "panic") => panic!("forced panic: FF_FORCE_CRASH=panic"),
        ("startup", "pool") => {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            rayon::spawn(move || {
                // Dropped as the panic below unwinds, which is what wakes
                // the receiver without a sleep.
                let _tx = tx;
                panic!("forced panic: FF_FORCE_CRASH=pool");
            });
            let _ = rx.recv();
        }
        ("startup", kind @ ("segv" | "stack-overflow" | "abort")) => native::raise(kind),
        ("stack", "stack") => {
            static FIRED: AtomicBool = AtomicBool::new(false);
            if !FIRED.swap(true, Ordering::SeqCst) {
                panic!("forced panic: FF_FORCE_CRASH=stack");
            }
        }
        _ => {}
    }
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
    fn stderr_filter_never_accepts_the_mirrored_output_target() {
        for verbosity in [-1, 0, 1, 2, 3] {
            let filter = stderr_targets(verbosity);
            for level in [Level::ERROR, Level::WARN, Level::INFO, Level::DEBUG] {
                assert!(
                    !filter.would_enable(MIRRORED_OUTPUT_TARGET, &level),
                    "{level} on the mirrored-output target reached stderr at verbosity {verbosity}"
                );
            }
        }
    }

    #[test]
    fn file_filter_accepts_the_mirrored_output_target_down_to_debug() {
        let filter = file_targets();
        assert!(filter.would_enable(MIRRORED_OUTPUT_TARGET, &Level::ERROR));
        assert!(filter.would_enable(MIRRORED_OUTPUT_TARGET, &Level::WARN));
        assert!(filter.would_enable(MIRRORED_OUTPUT_TARGET, &Level::DEBUG));
    }

    #[test]
    fn ring_filter_accepts_a_debug_event_the_file_filter_rejects() {
        let ring = ring_targets();
        let file = file_targets();
        assert!(ring.would_enable("some_third_party_crate", &Level::DEBUG));
        assert!(!file.would_enable("some_third_party_crate", &Level::DEBUG));
        assert!(ring.would_enable("focusforge_core", &Level::DEBUG));
    }

    #[test]
    fn file_filter_accepts_any_target_at_warn_and_above_independent_of_verbosity() {
        let filter = file_targets();
        assert!(filter.would_enable("some_third_party_crate", &Level::WARN));
        assert!(!filter.would_enable("some_third_party_crate", &Level::INFO));
        assert!(filter.would_enable("focusforge_core", &Level::DEBUG));
        assert!(!filter.would_enable("focusforge_core", &Level::TRACE));
    }

    #[test]
    fn the_log_file_opens_inside_log_dir() {
        let data = tempfile::tempdir().unwrap();
        let (_writer, path) = open_file_sink(Some(data.path()))
            .map_err(|e| e.to_string())
            .unwrap();
        assert_eq!(path.parent(), Some(log_dir(data.path()).as_path()));
        assert!(path.exists(), "{} was not created", path.display());
    }

    #[test]
    fn log_dir_and_crash_dir_are_distinct_children_of_the_data_dir() {
        let data = Path::new("/data/focusforge");
        assert_eq!(log_dir(data), data.join("logs"));
        assert_eq!(crash_dir(data), data.join("crashes"));
    }
}
