// SPDX-License-Identifier: GPL-3.0-or-later

//! Resolves the real 61-frame reference stack's directory from
//! `FOCUSFORGE_REFERENCE_STACK` and the RAW sample stack's from
//! `FOCUSFORGE_RAW_SAMPLE_STACK`, the one place every integration test that
//! reads either looks. Included via `#[path]` into the core, cli, gui and
//! spike test crates -- `env!("CARGO_MANIFEST_DIR")` below always names the
//! including crate's own manifest directory, so the relative-path join stays
//! correct in each of them.

use std::path::{Path, PathBuf};

/// The directory `variable` names, resolved against the workspace root when
/// relative. Unset: prints one `skip:` line naming the variable and returns
/// `None`.
fn directory_from(variable: &str) -> Option<PathBuf> {
    let Some(raw) = std::env::var_os(variable) else {
        eprintln!("skip: {variable} is not set");
        return None;
    };
    let raw = PathBuf::from(raw);
    Some(if raw.is_relative() {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(&raw)
    } else {
        raw
    })
}

/// The reference stack's directory, from `FOCUSFORGE_REFERENCE_STACK`.
///
/// Unset: prints one `skip:` line naming the variable and returns `None`. A
/// value that does not name a directory panics, naming it -- unlike unset,
/// that is a misconfiguration, not the normal no-stack-available case.
#[allow(dead_code)]
pub fn reference_stack_dir() -> Option<PathBuf> {
    let dir = directory_from("FOCUSFORGE_REFERENCE_STACK")?;
    if !dir.is_dir() {
        panic!(
            "FOCUSFORGE_REFERENCE_STACK does not name a directory: {}",
            dir.display()
        );
    }
    Some(dir)
}

/// The RAW sample stack's directory, from `FOCUSFORGE_RAW_SAMPLE_STACK`.
///
/// Unset, or not a directory (the stack sits on removable media): prints one
/// `skip:` line naming the variable and returns `None`.
#[allow(dead_code)]
pub fn raw_sample_stack_dir() -> Option<PathBuf> {
    let dir = directory_from("FOCUSFORGE_RAW_SAMPLE_STACK")?;
    if !dir.is_dir() {
        eprintln!(
            "skip: FOCUSFORGE_RAW_SAMPLE_STACK does not name a directory: {}",
            dir.display()
        );
        return None;
    }
    Some(dir)
}
