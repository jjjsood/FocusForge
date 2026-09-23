// SPDX-License-Identifier: GPL-3.0-or-later

//! The pinned Slint/wgpu stack, asserted rather than assumed.

mod common;

use std::fs;
use std::path::Path;

/// The `version = "X"` line immediately following a `name = "<name>"` line
/// in Cargo.lock's `[[package]]` table format.
fn locked_version(lock: &str, name: &str) -> Option<String> {
    let needle = format!("name = \"{name}\"\n");
    let start = lock.find(&needle)? + needle.len();
    let rest = &lock[start..];
    let version_line = rest.lines().next()?;
    let version = version_line
        .strip_prefix("version = \"")?
        .strip_suffix('"')?;
    Some(version.to_string())
}

#[test]
fn pinned_slint_and_wgpu_versions_resolve() {
    let lock_path = common::workspace_root().join("Cargo.lock");
    let lock = fs::read_to_string(&lock_path).unwrap();

    assert_eq!(
        locked_version(&lock, "slint").as_deref(),
        Some("1.17.1"),
        "slint must resolve to 1.17.1"
    );
    assert_eq!(
        locked_version(&lock, "wgpu").as_deref(),
        Some("29.0.4"),
        "wgpu must resolve to 29.0.4"
    );
}

#[test]
fn slint_feature_set_is_pinned() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).unwrap();

    assert!(
        manifest.contains("default-features = false"),
        "slint must keep default-features off"
    );

    for feature in [
        "\"std\"",
        "\"compat-1-2\"",
        "\"backend-winit\"",
        "\"renderer-femtovg-wgpu\"",
        "\"unstable-wgpu-29\"",
        "\"unstable-winit-030\"",
    ] {
        assert!(
            manifest.contains(feature),
            "{}: missing feature {feature}",
            manifest_path.display()
        );
    }
}

#[test]
fn app_slint_does_not_reexport_primitives() {
    let path = common::ui_dir().join("app.slint");
    let contents = fs::read_to_string(&path).unwrap();
    // Code only -- the header comment is free to name `Primitives` while
    // explaining why the code below never does.
    let code = common::strip_line_comments(&contents);

    assert!(
        !code.contains("Primitives"),
        "{}: must not export or reference Primitives -- it stays unreachable outside theme.slint",
        path.display()
    );
}
