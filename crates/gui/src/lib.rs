// SPDX-License-Identifier: GPL-3.0-or-later

//! Generated Rust bindings for `ui/app.slint`, the crate's real entry
//! point. `build.rs` compiles `ui/app.slint` as the **main** file, so only
//! what it exports is visible here: `AppWindow` itself, plus `Theme`,
//! `TypeRole` and `ThemeProbe`, which `ui/app.slint` re-exports from
//! `ui/theme.slint` so the crate's Slint integration tests keep compiling --
//! see `ui/app.slint`'s own header comment.

// Machine-generated code only; the workspace's `unsafe_code = "deny"` lint
// stays in force for everything hand-written in this crate.
mod generated {
    #![allow(unsafe_code)]
    slint::include_modules!();
}
pub use generated::*;

/// Plain-Rust shell state and the callback-forwarding seam -- no
/// `use slint::` anywhere in it. Reachable from `cargo test` without a
/// window, the same way
/// `tests/theme.rs` reaches `Theme` and `ThemeProbe` above.
mod controller;
pub use controller::*;

/// Canvas state as plain Rust -- no `use slint::` and no `use wgpu::`
/// anywhere in it. `pub` so `tests/canvas.rs` reaches
/// its pure types and functions directly, the same way `tests/shell.rs`
/// reaches `AppController` above.
pub mod canvas;

/// A freehand stroke in progress, as plain Rust -- no `use slint::`
/// anywhere in it. `pub` so `tests/controller.rs` reaches its types
/// directly, the same way `canvas` does above.
pub mod brush;

/// The entry path: classifying a picker result, a drop, or `--open`'s path
/// into a folder, a set of dropped frames, or a saved project, and turning
/// that into per-tab project state. No `use slint::`
/// anywhere in it -- the one module in the crate naming the core algorithm
/// crate's grouping and project modules. `pub` so `tests/entry.rs` and
/// `src/main.rs`/`src/picker.rs`/`src/bridge.rs` reach its types directly.
pub mod entry;

/// The native folder/file picker -- blocking, never `AsyncFileDialog`.
/// `pub` so `src/bridge.rs` can call it.
pub mod picker;

/// The input table as data -- no `use slint::`, no `use wgpu::`. `pub` so
/// `tests/input_map.rs` reaches its types directly.
pub mod input_map;

/// The input table's file format: load/write the `[input]` table of
/// `settings.toml`, no `use slint::`. `pub` so `src/main.rs` and
/// `tests/input_map.rs` reach it directly.
pub mod input_config;

/// The one settings file every table saves through: `settings.toml`,
/// shared by `recent` and `input_config`. No `use slint::`. `pub` because
/// `settings::WriteError` appears in `recent::save`'s return type and in
/// `input_config::ConfigError::CouldNotWrite`.
pub mod settings;

/// The viewport as pure arithmetic: no `use slint::`, no `use wgpu::`, and no
/// dependency on the core algorithm crate. `pub` so `tests/viewport.rs` and
/// `src/bridge.rs`/`src/main.rs` reach its types directly.
pub mod viewport;

/// The diagnostics panel's view types, no `use slint::` anywhere in it.
/// `pub` so `tests/controller.rs` and `src/bridge.rs` reach its types
/// directly, the same way `input_map` and `viewport` do above.
pub mod diagnostics;

/// The `Settings` dock panel model, generated from `ParamDesc` alone -- no
/// `use slint::` anywhere in it. `pub` so `tests/settings_panel.rs` reaches
/// its types directly, the same way `tests/tab_scroll.rs` reaches
/// `crate::controller`'s mirrored geometry.
pub mod settings_panel;

/// The wgpu half of the canvas: texture creation and
/// `slint::Image::try_from`, the only module in the crate naming `wgpu` or
/// `slint::wgpu_29`. `pub` so `src/main.rs` can drive it
/// after `bridge::install`.
pub mod gpu_canvas;

/// The Slint side of the seam, and the only module in the crate that names
/// `winit`. `pub` so `src/main.rs` can install it on an `AppWindow`.
pub mod bridge;

/// Writes a `Window::take_snapshot()` buffer to a PNG file, for
/// `--screenshot`.
pub mod screenshot;

/// The window's own fuse worker, running a stack on a background thread
/// through the same driver `focusforge-cli` uses. `pub` so `tests/job.rs`
/// reaches its types directly.
pub mod job;

/// The session's retained notifications, toast queue and read/routing rules.
/// No `use slint::` anywhere in it. `pub` so the integration tests reach its
/// types directly.
pub mod notifications;

/// The boundary to the operating system's notification service.
pub mod native_notify;

/// The `Images` dock panel model, and the frame-set-change warnings' copy --
/// no `use slint::` anywhere in it. `pub` so `tests/images.rs` reaches its
/// types directly.
pub mod images;

/// The projects overlay's own vocabulary -- a failure card's fixed copy per
/// kind and the gallery's grid arithmetic. No `use slint::` anywhere in it.
/// `pub` so `tests/projects_overlay.rs` and `src/bridge.rs` reach its types
/// directly.
pub mod overlay;

pub mod export_settings;

pub mod performance_settings;

pub mod appearance_settings;
pub mod source_previews;

/// The `Recent` list's own file format, mirroring `crate::input_config`. No
/// `use slint::` anywhere in it. `pub` so `src/main.rs` and
/// `tests/projects_overlay.rs` reach it directly.
pub mod recent;

/// The checkpoint dock panel's own view model -- day-grouped rows built from
/// a project's saved history. No `use slint::` anywhere in it. `pub` so
/// `tests/checkpoint_panel.rs` reaches its types directly.
pub mod checkpoints;
