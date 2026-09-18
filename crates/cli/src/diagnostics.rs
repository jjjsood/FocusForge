// SPDX-License-Identifier: GPL-3.0-or-later

//! `diagnostics`: package the logs, the crash evidence and this machine's
//! own facts into one ZIP the user can attach to a report. The file list is
//! printed before anything is written, and it is the archive's own entry
//! list. Nothing is sent anywhere.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use focusforge_diagnostics::bundle::{self, BundleError, Options, Redaction};
use focusforge_diagnostics::crash_notice;

/// Prints one line naming the crashes whose evidence has not been packaged
/// yet, and nothing when there are none. Raised at ERROR, the one level a
/// quiet run keeps.
pub(crate) fn announce_crashes(data_dir: Option<&Path>) {
    let Some(data_dir) = data_dir else {
        return;
    };
    let count = crash_notice::unreported(data_dir).len();
    if count == 0 {
        return;
    }
    let plural = if count == 1 { "" } else { "s" };
    tracing::error!(
        target: "focusforge_cli",
        "{count} earlier run{plural} crashed -- run focusforge-cli diagnostics to package the evidence"
    );
}

pub(crate) fn run(output: Option<&Path>, include_minidumps: bool, force: bool) -> ExitCode {
    let data_dir = focusforge_diagnostics::data_dir_from_env();
    let manifest = bundle::manifest(&Options {
        data_dir: data_dir.clone(),
        config_dir: focusforge_diagnostics::config_dir_from_env(),
        include_minidumps,
    });
    let redaction = Redaction::from_env();

    println!("diagnostics bundle contents:");
    for line in manifest.lines() {
        println!("  {line}");
    }
    if !manifest.has_logs() {
        println!("no log files were found");
    }
    if !manifest.has_crash_evidence() {
        println!("no crash reports were found");
    }
    if include_minidumps {
        println!(
            "warning: a minidump is not redacted -- it holds module paths and stack memory of the crashed process"
        );
    }

    let path = match output {
        Some(path) => path.to_path_buf(),
        None => PathBuf::from(bundle::default_file_name()),
    };
    match bundle::write(&manifest, &path, force, &redaction) {
        Ok(written) => {
            println!("wrote {}", written.path.display());
            if let Some(data_dir) = data_dir.as_deref() {
                let bundled = crash_notice::crash_ids(data_dir, &written.sources);
                // Best effort: the bundle is written either way.
                if let Err(e) = crash_notice::mark_reported(data_dir, &bundled) {
                    tracing::warn!(
                        target: "focusforge_cli",
                        error = %e,
                        "the packaged crashes could not be marked as reported"
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(BundleError::OutputExists(path)) => crate::fail(format!(
            "{} exists -- pass --force to overwrite it",
            path.display()
        )),
        Err(error) => crate::fail(error),
    }
}
