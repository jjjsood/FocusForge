// SPDX-License-Identifier: GPL-3.0-or-later

//! Generated Rust bindings for `ui/app.slint`, the crate's entry point.
//! `build.rs` compiles it as the **main** file, so only what it exports is
//! visible here: `AppWindow` itself, plus `Theme`, `TypeRole` and
//! `ThemeProbe`, which `ui/app.slint` re-exports from `ui/theme.slint` so
//! the GUI's Slint integration tests can reach them -- see `ui/app.slint`'s
//! own header comment.

// Machine-generated code only; the workspace's `unsafe_code = "deny"` lint
// stays in force for everything hand-written in this crate.
mod generated {
    #![allow(unsafe_code)]
    slint::include_modules!();
}
pub use generated::*;
