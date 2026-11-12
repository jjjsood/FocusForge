// SPDX-License-Identifier: GPL-3.0-or-later

//! Generated Rust bindings for `ThemeSheet`, the design-token specimen sheet.
//! The library is empty unless the `generate` feature is on; its `Theme` and
//! `StageTile` are distinct types from the main window's.

// Machine-generated code only; the workspace's `unsafe_code = "deny"` lint
// stays in force for everything hand-written in this crate.
#[cfg(feature = "generate")]
mod generated {
    #![allow(unsafe_code)]
    slint::include_modules!();
}
#[cfg(feature = "generate")]
pub use generated::*;
