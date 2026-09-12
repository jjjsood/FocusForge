// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    /// List a directory's entries (or a single file) with byte sizes.
    Info {
        /// Path to a file or directory.
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Info { path } => info(&path),
    }
}

fn info(path: &std::path::Path) -> ExitCode {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => return fail(format!("cannot read {}: {err}", path.display())),
    };

    if metadata.is_dir() {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(err) => return fail(format!("cannot read directory {}: {err}", path.display())),
        };

        let mut rows: Vec<(String, u64)> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    return fail(format!("cannot read entry in {}: {err}", path.display()));
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let bytes = match entry.metadata() {
                Ok(meta) => meta.len(),
                Err(err) => return fail(format!("cannot stat {name}: {err}")),
            };
            rows.push((name, bytes));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let mut total: u64 = 0;
        for (name, bytes) in &rows {
            println!("{name}\t{bytes}");
            total += bytes;
        }
        println!("total\t{total}");
    } else {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        println!("{name}\t{}", metadata.len());
    }

    ExitCode::SUCCESS
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}
