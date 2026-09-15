// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli cache`: manage the stage cache under
//! `.scratch/stage-cache/`. Today, just `clear`. Removes only the
//! stage cache; resume ledgers under `.scratch/resume/` and every
//! sealed project are untouched.

use std::path::Path;
use std::process::ExitCode;

use clap::Subcommand;
use focusforge_core::job::StageCache;
use serde::Serialize;

#[derive(Subcommand)]
pub enum CacheAction {
    /// Removes every entry from the stage cache under
    /// `.scratch/stage-cache/`. Never touches `.scratch/resume/` or any
    /// project -- a clear must never be able to destroy a cancelled run's
    /// recoverable work.
    Clear {
        /// Emit the frozen JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
}

/// The frozen `cache clear --json` schema.
#[derive(Serialize)]
struct ClearReport {
    format: u32,
    removed: usize,
}

pub fn run(action: CacheAction) -> ExitCode {
    match action {
        CacheAction::Clear { json } => clear(json),
    }
}

fn clear(json: bool) -> ExitCode {
    let cache = match StageCache::open(Path::new(".scratch")) {
        Ok(cache) => cache,
        Err(err) => return crate::fail(err.to_string()),
    };
    let removed = match cache.clear() {
        Ok(removed) => removed,
        Err(err) => return crate::fail(err.to_string()),
    };
    if json {
        let report = ClearReport { format: 1, removed };
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!(
            "removed {removed} stage cache entr{}",
            if removed == 1 { "y" } else { "ies" }
        );
    }
    ExitCode::SUCCESS
}
