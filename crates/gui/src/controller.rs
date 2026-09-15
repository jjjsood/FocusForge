// SPDX-License-Identifier: GPL-3.0-or-later

//! Plain-Rust shell state and the callback-forwarding seam. No `use slint::`
//! anywhere in this file: every Slint callback in the crate forwards into
//! `AppController`, whose method signatures name no Slint type, so the
//! window's behaviour is reachable from `cargo test` without a window.
//!
//! The canvas methods below (`pane_label`, `undock_enabled`, `frame_count`,
//! `frame_draw`, `plane_draw`, ...) live here rather than in `crate::canvas`
//! so they can reach `ShellState`'s private fields; `crate::canvas` owns
//! only the plain types and pure functions. The entry path's own state
//! (`GuiProject`, `ProjectTab`, `pending`) is per-project.

/// The stacking/editor mode switch's two positions. Editor is unreachable --
/// `AppController::set_mode` enforces that, not the Slint side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellMode {
    #[default]
    Stacking,
    Editor,
}

/// The dock's tab row (Images / Settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockTab {
    #[default]
    Images,
    Settings,
}

/// The two fields a checkpoint row shows: the fusion algorithm used and when
/// it ran. Left unpopulated until real checkpoint history exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRow {
    pub algorithm: String,
    pub created_at: String,
}

/// Every event the shell can raise, reaching an installed sink through
/// [`AppController::set_sink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    ModeChanged(ShellMode),
    DockTabSelected(DockTab),
    WindowDragRequested,
    WindowMinimizeRequested,
    /// Carries the new maximized state, so the bridge can push it straight
    /// into `AppWindow`'s `window-maximized` property (the glyph binding)
    /// without a second lock on the controller.
    WindowMaximizeToggled(bool),
    WindowCloseRequested,
    CheckpointExportRequested,
    /// Unreachable while `AppController::undock_enabled` stays `true`,
    /// which holds as long as both panes are docked.
    PaneUndockRequested(crate::canvas::PaneSource),
}

/// The shell's state, carried `Send + Sync` so a worker thread inherits the
/// seam rather than re-cutting it.
#[derive(Default)]
struct ShellState {
    mode: ShellMode,
    dock_tab: DockTab,
    checkpoints: Vec<CheckpointRow>,
    maximized: bool,
    canvas: crate::canvas::CanvasState,
}

/// The plain-Rust controller every Slint callback in the crate forwards to.
/// No method signature here names a Slint type.
#[derive(Default)]
pub struct AppController {
    state: ShellState,
    sink: Option<Box<dyn Fn(ShellEvent) + Send + Sync>>,
}

impl AppController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs the observer sink. `src/bridge.rs` is the only caller
    /// outside tests; the controller tests install their own sink directly,
    /// since `i_slint_backend_testing::init_no_event_loop()` disables
    /// `slint::invoke_from_event_loop` and the bridge is never exercised by
    /// them.
    pub fn set_sink(&mut self, sink: Box<dyn Fn(ShellEvent) + Send + Sync>) {
        self.sink = Some(sink);
    }

    fn emit(&self, event: ShellEvent) {
        if let Some(sink) = &self.sink {
            sink(event);
        }
    }

    pub fn mode(&self) -> ShellMode {
        self.state.mode
    }

    /// Returns `false` and emits nothing for [`ShellMode::Editor`]: no editor
    /// is implemented.
    pub fn set_mode(&mut self, mode: ShellMode) -> bool {
        if mode == ShellMode::Editor {
            return false;
        }
        self.state.mode = mode;
        self.emit(ShellEvent::ModeChanged(mode));
        true
    }

    /// Always `false`.
    pub fn editor_enabled(&self) -> bool {
        false
    }

    pub fn dock_tab(&self) -> DockTab {
        self.state.dock_tab
    }

    pub fn select_dock_tab(&mut self, tab: DockTab) {
        self.state.dock_tab = tab;
        self.emit(ShellEvent::DockTabSelected(tab));
    }

    /// Always empty -- the checkpoint panel's frame reads no storage.
    pub fn checkpoints(&self) -> &[CheckpointRow] {
        &self.state.checkpoints
    }

    /// Always `false` -- the checkpoint panel's `Export` stays disabled.
    pub fn export_enabled(&self) -> bool {
        false
    }

    /// The tab strip's pointer-down, forwarded from `window-drag-requested`.
    pub fn tab_strip_pressed(&self) {
        self.emit(ShellEvent::WindowDragRequested);
    }

    pub fn window_minimize(&self) {
        self.emit(ShellEvent::WindowMinimizeRequested);
    }

    /// The window controls' maximize glyph reads this back
    /// (`AppWindow::window-maximized`, wired in `src/bridge.rs`), so it
    /// looks different maximized than not.
    pub fn maximized(&self) -> bool {
        self.state.maximized
    }

    pub fn window_maximize_toggle(&mut self) {
        self.state.maximized = !self.state.maximized;
        self.emit(ShellEvent::WindowMaximizeToggled(self.state.maximized));
    }

    pub fn window_close(&self) {
        self.emit(ShellEvent::WindowCloseRequested);
    }

    /// Never reachable while [`Self::export_enabled`] is always `false`,
    /// but declared so the seam exists to wire without moving anything.
    pub fn checkpoint_export_requested(&self) {
        self.emit(ShellEvent::CheckpointExportRequested);
    }

    /// Read/write access to the canvas's loaded-project state, for
    /// `crate::canvas`'s own `impl AppController` block (`open_project`,
    /// `viewport`, `project_paths`) -- kept out of *this* file so
    /// `checkpoint_panel_touches_no_storage`'s crate-wide scan
    /// (`tests/shell.rs`) stays honest: nothing in `controller.rs` itself
    /// names the core algorithm crate.
    pub(crate) fn canvas(&self) -> &crate::canvas::CanvasState {
        &self.state.canvas
    }

    pub(crate) fn canvas_mut(&mut self) -> &mut crate::canvas::CanvasState {
        &mut self.state.canvas
    }

    /// `"Composite"` / `"Z-Map"` -- the label's geometry is fixed
    /// elsewhere, not its text.
    pub fn pane_label(&self, pane: crate::canvas::PaneSource) -> &'static str {
        match pane {
            crate::canvas::PaneSource::Composite => "Composite",
            crate::canvas::PaneSource::ZMap => "Z-Map",
        }
    }

    /// Always `true` -- both panes are docked together.
    pub fn undock_enabled(&self, _pane: crate::canvas::PaneSource) -> bool {
        true
    }

    /// The active tab's frame count, once a project is open. The
    /// index-scale uniform (`crate::gpu_canvas`'s render path) is derived
    /// from this.
    pub fn frame_count(&self) -> Option<u32> {
        self.state
            .canvas
            .project
            .as_ref()
            .map(crate::project_stub::ProjectStub::frame_count)
    }

    /// Forwards a pane's undock request -- unreachable while
    /// [`Self::undock_enabled`] is always `true` with both panes docked,
    /// but declared so the seam exists.
    pub fn undock_requested(&self, pane: crate::canvas::PaneSource) {
        self.emit(ShellEvent::PaneUndockRequested(pane));
    }
}
