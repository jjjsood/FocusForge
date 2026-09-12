// SPDX-License-Identifier: GPL-3.0-or-later

//! Resolves the real 61-frame reference stack's directory from
//! `FOCUSFORGE_REFERENCE_STACK`, the one place every integration test that
//! reads it looks. Included via `#[path]` into the core, cli, gui and spike
//! test crates -- `env!("CARGO_MANIFEST_DIR")` below always names the
//! including crate's own manifest directory, so the relative-path join stays
//! correct in each of them.

use std::path::{Path, PathBuf};

/// The reference stack's directory, from `FOCUSFORGE_REFERENCE_STACK`.
///
/// Unset: prints one `skip:` line naming the variable and returns `None`. A
/// relative value resolves against the workspace root. A value that does not
/// name a directory panics, naming it -- unlike unset, that is a
/// misconfiguration, not the normal no-stack-available case.
#[allow(dead_code)]
pub fn reference_stack_dir() -> Option<PathBuf> {
    let Some(raw) = std::env::var_os("FOCUSFORGE_REFERENCE_STACK") else {
        eprintln!("skip: FOCUSFORGE_REFERENCE_STACK is not set");
        return None;
    };
    let raw = PathBuf::from(raw);
    let dir = if raw.is_relative() {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(&raw)
    } else {
        raw
    };
    if !dir.is_dir() {
        panic!(
            "FOCUSFORGE_REFERENCE_STACK does not name a directory: {}",
            dir.display()
        );
    }
    Some(dir)
}
