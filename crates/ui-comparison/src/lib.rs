// SPDX-License-Identifier: GPL-3.0-or-later

//! Generated Rust bindings for `ComparisonWindow`, the detached comparison
//! window. It is compiled in its own crate so its generated code never shares
//! a compiler process with `AppWindow`'s; its `Theme` and `StageTile` are
//! distinct types from the main window's.

// Machine-generated code only; the workspace's `unsafe_code = "deny"` lint
// stays in force for everything hand-written in this crate.
mod generated {
    #![allow(unsafe_code)]
    slint::include_modules!();
}
pub use generated::*;
