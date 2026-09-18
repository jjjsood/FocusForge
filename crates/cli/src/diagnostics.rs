// SPDX-License-Identifier: GPL-3.0-or-later

//! `diagnostics`: package the logs, the crash evidence and this machine's
//! own facts into one ZIP the user can attach to a report. The file list is
//! printed before anything is written, and it is the archive's own entry
//! list. Nothing is sent anywhere.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use focusforge_diagnostics::bundle::{self, BundleError, Options, Redaction};

pub(crate) fn run(output: Option<&Path>, include_minidumps: bool, force: bool) -> ExitCode {
    let manifest = bundle::manifest(&Options {
        data_dir: focusforge_diagnostics::data_dir_from_env(),
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
            ExitCode::SUCCESS
        }
        Err(BundleError::OutputExists(path)) => crate::fail(format!(
            "{} exists -- pass --force to overwrite it",
            path.display()
        )),
        Err(error) => crate::fail(error),
    }
}
