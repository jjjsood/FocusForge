// SPDX-License-Identifier: GPL-3.0-or-later

//! `install`: wires SIGINT (cancel, two-stage) and, on Unix, SIGUSR1 (pause
//! toggle) into a pair of plain `Arc<AtomicBool>` flags a caller polls --
//! `core` never links a signal crate, so this module is the entire seam
//! between a real terminal and the run's poll loop.
//!
//! # The double-Ctrl-C pattern
//!
//! `flag::register_conditional_default(SIGINT, Arc::clone(&cancel))` is
//! registered **first**, then `flag::register(SIGINT, Arc::clone(&cancel))`
//! second -- signal-hook's own documented order for this pattern.
//! On the *first* SIGINT, handlers run in registration order: the
//! conditional-default check finds `cancel` still `false` (does nothing),
//! then the plain `register` handler sets it `true`. On the *second*
//! SIGINT, the conditional-default check now finds `cancel` already `true`
//! and calls `low_level::emulate_default_handler`, which unregisters and
//! re-raises SIGINT -- the process dies by signal, entirely inside the
//! signal-handling machinery, before any Rust code in this crate's `main`
//! thread ever wakes up to see it. The poll loop must therefore only ever
//! **read** `cancel`, never reset it -- resetting it would disarm the
//! second-Ctrl-C kill.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use signal_hook::consts::signal::SIGINT;
use signal_hook::flag;

/// The flags a caller polls. `pause` is armed only `#[cfg(unix)]`
/// (signal-hook has no SIGUSR1 on Windows); elsewhere the field exists but
/// nothing sets it.
pub struct Signals {
    pub cancel: Arc<AtomicBool>,
    pub pause: Arc<AtomicBool>,
}

/// Installs the SIGINT double-Ctrl-C pair and, when `pause` is `true` and
/// the target is Unix, a SIGUSR1 toggle. Every call here is a **safe**
/// `signal_hook::flag::*` function -- the workspace denies `unsafe_code`,
/// which is exactly why `low_level::register` (unsafe) is never used
/// directly.
pub fn install(pause: bool) -> Result<Signals, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    flag::register_conditional_default(SIGINT, Arc::clone(&cancel))
        .map_err(|err| format!("installing the SIGINT handler: {err}"))?;
    flag::register(SIGINT, Arc::clone(&cancel))
        .map_err(|err| format!("installing the SIGINT handler: {err}"))?;

    let pause_flag = Arc::new(AtomicBool::new(false));
    install_pause(pause, &pause_flag)?;

    Ok(Signals {
        cancel,
        pause: pause_flag,
    })
}

#[cfg(unix)]
fn install_pause(pause: bool, flag_handle: &Arc<AtomicBool>) -> Result<(), String> {
    if !pause {
        return Ok(());
    }
    signal_hook::flag::register(
        signal_hook::consts::signal::SIGUSR1,
        Arc::clone(flag_handle),
    )
    .map_err(|err| format!("installing the SIGUSR1 handler: {err}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn install_pause(_pause: bool, _flag_handle: &Arc<AtomicBool>) -> Result<(), String> {
    // No SIGUSR1 on this platform: the flag simply never gets set.
    Ok(())
}
