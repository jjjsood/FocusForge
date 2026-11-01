// SPDX-License-Identifier: GPL-3.0-or-later

//! Native-crash capture: a segfault, a stack overflow or an abort fires no
//! panic hook, so the binary re-spawns itself as a monitor process and
//! hands that process the crash. The monitor writes a minidump and, beside
//! it under the same stem, a JSON file naming the binary, its version, the
//! crashed pid and the signal.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crash_handler::{CrashContext, CrashEventResult, CrashHandler};
use minidumper::{Client, LoopAction, MinidumpBinary, Server, ServerHandler, SocketName};

use crate::crash::{create_new_numbered, report_stem, rfc3339};

/// Set on a re-spawned process to make it the monitor for the socket its
/// value names. The reserved value [`CAPTURE_OFF`] instead runs the
/// application itself without native-crash capture.
const MONITOR_ENV: &str = "FF_CRASH_MONITOR";

/// Overrides the socket the application and its monitor meet on.
const SOCKET_ENV: &str = "FF_CRASH_SOCKET";

/// The [`MONITOR_ENV`] value that switches native-crash capture off.
const CAPTURE_OFF: &str = "off";

/// The one argument a re-spawned monitor carries. Nothing reads it: both
/// entry points decide from [`MONITOR_ENV`] before any argument parser
/// runs, so the monitor never sees it. It is there for an executable that
/// is not one of them -- a libtest binary, whose argument parser rejects
/// the unknown option and exits instead of running its whole test list
/// again in every generation of a re-spawn chain.
const MONITOR_ARG: &str = "--focusforge-crash-monitor";

/// The monitor ends its loop when no application has identified itself
/// within this period, so a monitor whose parent died before attaching
/// cannot linger.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the application waits for its freshly spawned monitor to bind,
/// and how long it sleeps between connection attempts.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_RETRY: Duration = Duration::from_millis(2);

/// The message kinds the application sends its monitor: the process
/// information the monitor needs to name its files, and the number of the
/// signal that killed the process.
const MESSAGE_PROCESS_INFO: u32 = 1;
const MESSAGE_SIGNAL: u32 = 2;

/// Why native-crash capture could not be set up.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("the monitor process could not be started: {0}")]
    Spawn(io::Error),
    #[error("the monitor process exited before accepting a connection")]
    MonitorGone,
    #[error("the monitor process could not be reached: {0}")]
    Connect(minidumper::Error),
    #[error("the crash handler could not be installed: {0}")]
    Attach(crash_handler::Error),
}

/// Keeps the crash handler installed and the monitor process alive. On drop
/// the handler is detached, which closes the socket and ends the monitor's
/// loop.
pub(crate) struct Guard {
    _handler: CrashHandler,
    _monitor: Child,
}

/// The socket the pair meets on when [`SOCKET_ENV`] does not override it:
/// an abstract Linux socket, which needs no filesystem entry and no
/// cleanup, named for the application's own process.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn default_socket() -> String {
    format!("focusforge-crash-{}", std::process::id())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn default_socket() -> String {
    std::env::temp_dir()
        .join(format!("focusforge-crash-{}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

fn socket_string() -> String {
    std::env::var_os(SOCKET_ENV)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(default_socket)
}

/// A value carrying a path separator names a filesystem socket; on Linux
/// any other value is an abstract socket name.
fn socket_name(value: &str) -> SocketName<'_> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if !value.contains(std::path::MAIN_SEPARATOR) {
        return SocketName::abstract_namespace(value);
    }
    SocketName::Path(Path::new(value))
}

/// The number identifying the crash: the signal on Unix, the structured
/// exception code on Windows.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn crash_signal(cc: &CrashContext) -> u32 {
    cc.siginfo.ssi_signo
}

#[cfg(target_os = "macos")]
fn crash_signal(cc: &CrashContext) -> u32 {
    cc.exception
        .as_ref()
        .map_or(0, |e| mach_exception_signal(e.kind, e.code, e.subcode))
}

/// The POSIX signal number a Mach exception stands for. A bad access or
/// stack-guard hit is a segmentation fault; an abort arrives as a software
/// exception whose subcode is the signal itself. Numbered as
/// [`signal_name`] expects.
#[cfg(any(target_os = "macos", test))]
fn mach_exception_signal(kind: u32, code: u64, subcode: Option<u64>) -> u32 {
    const EXC_BAD_ACCESS: u32 = 1;
    const EXC_BAD_INSTRUCTION: u32 = 2;
    const EXC_ARITHMETIC: u32 = 3;
    const EXC_SOFTWARE: u32 = 5;
    const EXC_BREAKPOINT: u32 = 6;
    const KERN_INVALID_ADDRESS: u64 = 1;
    const KERN_PROTECTION_FAILURE: u64 = 2;
    const EXC_SOFT_SIGNAL: u64 = 0x10003;

    match kind {
        EXC_BAD_ACCESS if matches!(code, KERN_INVALID_ADDRESS | KERN_PROTECTION_FAILURE) => 11,
        EXC_BAD_ACCESS => 7,
        EXC_BAD_INSTRUCTION => 4,
        EXC_ARITHMETIC => 8,
        EXC_BREAKPOINT => 5,
        EXC_SOFTWARE if code == EXC_SOFT_SIGNAL => subcode.map_or(0, |s| s as u32),
        _ => 0,
    }
}

#[cfg(target_os = "windows")]
fn crash_signal(cc: &CrashContext) -> u32 {
    cc.exception_code as u32
}

/// The name of the signal the crash handler caught, as the metadata's
/// human-readable `reason`.
fn signal_name(signal: u32) -> String {
    match signal {
        4 => "SIGILL".to_string(),
        5 => "SIGTRAP".to_string(),
        6 => "SIGABRT".to_string(),
        7 => "SIGBUS".to_string(),
        8 => "SIGFPE".to_string(),
        11 => "SIGSEGV".to_string(),
        other => format!("signal {other}"),
    }
}

/// Runs this process as its parent's crash monitor when `FF_CRASH_MONITOR`
/// marks it as one, returning the exit code to leave with; `None` means
/// this process is an ordinary application run. Call as the first statement
/// of `main`, before any argument parser sees the argument a re-spawned
/// monitor carries.
pub fn run_monitor() -> Option<i32> {
    let socket = std::env::var_os(MONITOR_ENV)?
        .to_string_lossy()
        .into_owned();
    if socket == CAPTURE_OFF {
        return None;
    }
    Some(i32::from(serve(&socket).is_err()))
}

/// The monitor's whole life: bind the socket, serve one application until
/// it crashes or disconnects. No subscriber, no panic hook and no thread
/// pool are installed here -- a monitor that shares the application's log
/// file would interleave with it, and a monitor that crashes has nothing to
/// report.
fn serve(socket: &str) -> Result<(), minidumper::Error> {
    let mut server = Server::with_name(socket_name(socket))?;
    let handler = Monitor::default();
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let attached = handler.attached.clone();
    let watchdog = shutdown.clone();
    std::thread::spawn(move || {
        std::thread::sleep(IDLE_TIMEOUT);
        if !attached.load(Ordering::SeqCst) {
            watchdog.store(true, Ordering::SeqCst);
        }
    });
    // `stale_timeout` stays `None`: it drops a client that has sent no
    // message within the period, and an application can work silently for
    // minutes. The application's disconnect is what ends the loop instead.
    server.run(Box::new(handler), &shutdown, None)
}

/// What the application tells its monitor about itself, once, right after
/// connecting -- the monitor cannot read any of it out of a process that is
/// already dead.
struct ProcessInfo {
    crash_dir: PathBuf,
    binary: String,
    version: String,
    pid: u32,
}

#[derive(Default)]
struct Monitor {
    /// Set once a client has sent [`MESSAGE_PROCESS_INFO`]: only that client
    /// is the application. A connection that closes before identifying
    /// itself is a failed attempt -- on macOS a client connects the socket
    /// before it looks up the monitor's Mach port, and gives up on the
    /// socket when that port is not registered yet -- and the application
    /// is still to come.
    attached: std::sync::Arc<AtomicBool>,
    process: Mutex<Option<ProcessInfo>>,
    signal: AtomicU32,
    crashed_at: AtomicI64,
}

impl Monitor {
    /// Writes the metadata beside `dump`, under the same stem.
    fn write_metadata(&self, dump: &Path) -> io::Result<()> {
        let guard = self.process.lock().unwrap_or_else(|e| e.into_inner());
        let Some(info) = guard.as_ref() else {
            return Ok(());
        };
        let signal = self.signal.load(Ordering::SeqCst);
        let metadata = serde_json::json!({
            "timestamp": rfc3339(self.crashed_at.load(Ordering::SeqCst)),
            "binary": info.binary,
            "version": info.version,
            "pid": info.pid,
            "signal": signal,
            "reason": signal_name(signal),
            "minidump": dump.file_name().map(|n| n.to_string_lossy()),
        });
        std::fs::write(
            dump.with_extension("json"),
            serde_json::to_vec_pretty(&metadata)?,
        )
    }
}

impl ServerHandler for Monitor {
    fn create_minidump_file(&self) -> Result<(File, PathBuf), io::Error> {
        let guard = self.process.lock().unwrap_or_else(|e| e.into_inner());
        let info = guard.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the crashed process sent no process information",
            )
        })?;
        let seconds = focusforge_core::project::seal::now_unix();
        self.crashed_at.store(seconds, Ordering::SeqCst);
        let stem = report_stem(seconds, &info.binary, info.pid);
        create_new_numbered(&info.crash_dir, &stem, "dmp")
    }

    fn on_minidump_created(&self, result: Result<MinidumpBinary, minidumper::Error>) -> LoopAction {
        if let Ok(dump) = result {
            let _ = self.write_metadata(&dump.path);
        }
        LoopAction::Exit
    }

    fn on_message(&self, kind: u32, buffer: Vec<u8>) {
        match kind {
            MESSAGE_PROCESS_INFO => {
                self.attached.store(true, Ordering::SeqCst);
                if let Ok(info) = serde_json::from_slice::<serde_json::Value>(&buffer) {
                    *self.process.lock().unwrap_or_else(|e| e.into_inner()) = Some(ProcessInfo {
                        crash_dir: PathBuf::from(info["crash_dir"].as_str().unwrap_or_default()),
                        binary: info["binary"].as_str().unwrap_or_default().to_string(),
                        version: info["version"].as_str().unwrap_or_default().to_string(),
                        pid: info["pid"].as_u64().unwrap_or_default() as u32,
                    });
                }
            }
            MESSAGE_SIGNAL => {
                if let Ok(bytes) = <[u8; 4]>::try_from(buffer.as_slice()) {
                    self.signal
                        .store(u32::from_le_bytes(bytes), Ordering::SeqCst);
                }
            }
            _ => {}
        }
    }

    fn on_client_disconnected(&self, _num_clients: usize) -> LoopAction {
        if self.attached.load(Ordering::SeqCst) {
            LoopAction::Exit
        } else {
            LoopAction::Continue
        }
    }
}

/// The callback the crash handler runs in the compromised process.
#[allow(unsafe_code)]
fn crash_event(client: Client) -> Box<dyn crash_handler::CrashEvent> {
    // SAFETY: `make_crash_event` requires the callback to be safe to run
    // from a signal handler on a crashed process. This one allocates
    // nothing and takes no lock: it writes a stack-sized buffer to a socket
    // that was already open when the handler was attached, and blocks until
    // the monitor has written the dump.
    unsafe {
        crash_handler::make_crash_event(move |cc: &CrashContext| {
            let _ = client.send_message(MESSAGE_SIGNAL, crash_signal(cc).to_le_bytes());
            let _ = client.ping();
            CrashEventResult::Handled(client.request_dump(cc).is_ok())
        })
    }
}

/// Connects to the monitor, giving up as soon as it is clear the monitor
/// will never answer -- either because it has already exited, or because
/// [`CONNECT_TIMEOUT`] passed.
fn connect(socket: &str, monitor: &mut Child) -> Result<Client, Error> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match Client::with_name(socket_name(socket)) {
            Ok(client) => return Ok(client),
            Err(e) => {
                if matches!(monitor.try_wait(), Ok(Some(_))) {
                    return Err(Error::MonitorGone);
                }
                if Instant::now() >= deadline {
                    return Err(Error::Connect(e));
                }
                std::thread::sleep(CONNECT_RETRY);
            }
        }
    }
}

/// Spawns this executable again as a monitor process and attaches a crash
/// handler that hands it every native crash. `Ok(None)` means capture is
/// switched off by [`MONITOR_ENV`], which is not a failure.
pub(crate) fn attach(
    crash_dir: &Path,
    binary: &str,
    version: &str,
) -> Result<Option<Guard>, Error> {
    if std::env::var_os(MONITOR_ENV).is_some_and(|value| value == CAPTURE_OFF) {
        return Ok(None);
    }

    let socket = socket_string();
    let executable = std::env::current_exe().map_err(Error::Spawn)?;
    let mut monitor = Command::new(executable)
        .arg(MONITOR_ARG)
        .env(MONITOR_ENV, &socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::Spawn)?;

    let attached = connect(&socket, &mut monitor).and_then(|client| {
        let info = serde_json::json!({
            "crash_dir": crash_dir.to_string_lossy(),
            "binary": binary,
            "version": version,
            "pid": std::process::id(),
        });
        client
            .send_message(MESSAGE_PROCESS_INFO, info.to_string())
            .map_err(Error::Connect)?;
        CrashHandler::attach(crash_event(client)).map_err(Error::Attach)
    });

    match attached {
        Ok(handler) => {
            // Yama's default `ptrace_scope` refuses a child process access
            // to its own parent, and without that access every dump comes
            // out empty.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            handler.set_ptracer(Some(monitor.id()));
            Ok(Some(Guard {
                _handler: handler,
                _monitor: monitor,
            }))
        }
        Err(e) => {
            let _ = monitor.kill();
            let _ = monitor.wait();
            Err(e)
        }
    }
}

/// Crashes this process on purpose, so the capture path can be exercised on
/// a real binary.
#[allow(unsafe_code)]
pub(crate) fn raise(kind: &str) {
    // SAFETY: every one of these raisers ends the process by the crash it
    // names; that is the whole point of calling them.
    unsafe {
        match kind {
            "segv" => sadness_generator::raise_segfault(),
            "stack-overflow" => sadness_generator::raise_stack_overflow(),
            _ => sadness_generator::raise_abort(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KERN_INVALID_ADDRESS: u64 = 1;
    const KERN_PROTECTION_FAILURE: u64 = 2;
    const EXC_SOFT_SIGNAL: u64 = 0x10003;

    #[test]
    fn a_bad_access_is_a_segfault() {
        for code in [KERN_INVALID_ADDRESS, KERN_PROTECTION_FAILURE] {
            let signal = mach_exception_signal(1, code, Some(0x42));
            assert_eq!(signal_name(signal), "SIGSEGV", "kern_return {code}");
        }
        assert_eq!(signal_name(mach_exception_signal(1, 10, None)), "SIGBUS");
    }

    #[test]
    fn an_abort_arrives_as_a_software_exception_carrying_the_signal() {
        let signal = mach_exception_signal(5, EXC_SOFT_SIGNAL, Some(6));
        assert_eq!(signal_name(signal), "SIGABRT");
    }

    /// A connection attempt that reaches the socket and then fails -- on
    /// macOS, one made before the monitor has registered its Mach port --
    /// closes without identifying itself. The monitor must keep waiting for
    /// its application instead of taking that close for the application's.
    #[test]
    fn a_connection_that_never_identifies_itself_does_not_end_the_monitor() {
        let socket = format!("{}-unidentified", default_socket());
        let server = {
            let socket = socket.clone();
            std::thread::spawn(move || serve(&socket))
        };
        let connect = || {
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            loop {
                match Client::with_name(socket_name(&socket)) {
                    Ok(client) => return client,
                    Err(e) if Instant::now() >= deadline => panic!("no monitor to reach: {e}"),
                    Err(_) => std::thread::sleep(CONNECT_RETRY),
                }
            }
        };

        drop(connect());
        let application = connect();
        application
            .send_message(MESSAGE_PROCESS_INFO, "{}")
            .expect("process information reaches the monitor");
        application
            .ping()
            .expect("the monitor still serves after an unidentified connection closed");

        drop(application);
        server
            .join()
            .expect("the monitor thread does not panic")
            .expect("the application's disconnect ends the loop cleanly");
    }

    #[test]
    fn the_other_hardware_exceptions_name_their_signals() {
        assert_eq!(signal_name(mach_exception_signal(2, 1, None)), "SIGILL");
        assert_eq!(signal_name(mach_exception_signal(3, 1, None)), "SIGFPE");
        assert_eq!(signal_name(mach_exception_signal(6, 1, None)), "SIGTRAP");
    }
}
