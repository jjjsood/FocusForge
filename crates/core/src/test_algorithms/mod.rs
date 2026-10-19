// SPDX-License-Identifier: GPL-3.0-or-later

//! Two algorithms that register through the real path -- the same
//! `inventory::submit!` line every real algorithm writes -- feature-gated
//! behind `test-algorithms` rather than `#[cfg(test)]`.
//!
//! `#[cfg(test)]` code never reaches the rlib, so a `#[cfg(test)]`
//! registration in this crate would be invisible to `crates/cli`'s own
//! tests, which is exactly the linkage property `registry` exists to prove
//! (see `crate::registry`'s module docs). Enabling this feature is
//! `crates/cli`'s job, through its own `[dev-dependencies]` entry naming
//! `focusforge-core` with `features = ["test-algorithms"]` -- see
//! `crates/cli/tests/registry_linkage.rs`.
//!
//! One algorithm per file, each file self-contained and ending in its own
//! `inventory::submit!`: adding an algorithm is a new file plus the one
//! `mod` line `lib.rs` already carries for this module, and nothing else --
//! `crate::registry` never names either type in this module, nor either id.

mod dummy_align;
mod dummy_stack;
