// SPDX-License-Identifier: GPL-3.0-or-later

//! Generated Rust bindings for `ui/theme.slint`, re-exported so integration
//! tests under `crates/gui/tests/` can reach the `Theme` global and the
//! `ThemeProbe` component. `build.rs` compiles `ui/theme.slint` as the
//! **main** file, so only what it exports is visible here -- see
//! `ui/theme.slint`'s own header comment.

// Machine-generated code only; the workspace's `unsafe_code = "deny"` lint
// stays in force for everything hand-written in this crate.
mod generated {
    #![allow(unsafe_code)]
    slint::include_modules!();
}
pub use generated::*;
