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
//!
//! The input dispatch methods below (`wheel`, `pointer`, `pointer_moved`,
//! `key`, ...) are the **one** point in the crate that maps a gesture to an
//! `Action`. They read `crate::input_map`'s types only -- no `use slint::`
//! here either.

use crate::input_config::Provenance;
use crate::input_map::{
    Action, ActionEvent, Button, InputRowView, InputTable, Modifiers, PointerKind, Trigger,
};
use crate::viewport::{ImageSize, PaneDraw, PaneRect, Viewport};

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
/// `PartialEq` only, not `Eq`, since one variant carries `f32`.
#[derive(Debug, Clone, PartialEq)]
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
    /// Raised by the one input dispatch point (`wheel`, `pointer`,
    /// `pointer_moved`, `key`) every time a trigger resolves to an
    /// `Action`.
    Input(ActionEvent),
}

/// One live drag: which action it started and the last position seen, so
/// `pointer_moved` can difference successive positions into deltas
/// (slint-ui/slint#13189 -- `PointerEvent` carries no delta of its own).
#[derive(Debug, Clone, Copy, PartialEq)]
struct DragState {
    action: Action,
    last: (f32, f32),
}

/// The one dispatch point's own state: the active table, where it came
/// from, the controller-tracked `space` pseudo-modifier, any live drag,
/// every action fired so far with no implementation, and the two pieces of
/// state zoom/pan needs: the last known cursor position (`wheel` and
/// `pointer` both record it, so a drag and a wheel share one notion of
/// where the cursor is) and the wheel's own sub-detent accumulator.
#[derive(Debug, Clone, Default)]
struct InputState {
    table: InputTable,
    provenance: Provenance,
    space_held: bool,
    drag: Option<DragState>,
    unimplemented: Vec<Action>,
    last_pointer: (f32, f32),
    /// Accumulated wheel delta not yet spent into a ladder step -- one stop
    /// per accumulated 60 px. Reset on a sign change so reversing direction
    /// never burns a leftover.
    wheel_accum: f32,
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
    input: InputState,
    /// The loaded project's composite/Z-Map size, set once by
    /// `AppController::set_project_image` (`crate::canvas::open_project`).
    /// `None` until a project is open.
    image: Option<ImageSize>,
    /// Both panes' current geometry, pushed by `set_pane_geometry`
    /// (`ui/shell/canvas.slint`'s five `out` properties, read once by
    /// `src/main.rs` at startup and again on every `canvas-resized`).
    panes: Option<(PaneRect, PaneRect)>,
    /// `None` until the first `set_pane_geometry` call after a project
    /// opens fits it -- `open_project` cannot fit immediately, since it is
    /// never handed a pane rect.
    viewport: Option<Viewport>,
    /// The settings modal's visibility -- the only consumer of
    /// `--settings-section`. No `ShellEvent` variant needed:
    /// nothing else in the crate observes it, so `bridge.rs` reads the
    /// return value back directly rather than round-tripping the sink.
    settings_open: bool,
}

/// Whether `(x, y)` falls within `pane`'s rect -- [`AppController::anchor`]'s
/// only helper.
fn pane_contains(pane: PaneRect, x: f32, y: f32) -> bool {
    x >= pane.x && x <= pane.x + pane.width && y >= pane.y && y <= pane.y + pane.height
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

    /// Test-only: a controller already in `mode`, bypassing
    /// [`Self::set_mode`]'s guard against [`ShellMode::Editor`]. No
    /// Slint callback reaches this constructor -- only `set_mode` is wired
    /// in `src/bridge.rs` -- so the editor half stays unreachable from a
    /// running window while its dispatch logic stays testable from
    /// `cargo test` "by construction".
    pub fn new_with_mode_for_test(mode: ShellMode) -> Self {
        let mut controller = Self::new();
        controller.state.mode = mode;
        controller
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

    /// Installs the input table `input_config::load` resolved -- called
    /// once at startup, before any gesture is dispatched.
    pub fn install_input_table(&mut self, table: InputTable, provenance: Provenance) {
        self.state.input.table = table;
        self.state.input.provenance = provenance;
    }

    /// Records `event`'s action as unimplemented (by name, never silently)
    /// when there is no implementation for it; an implemented one is
    /// applied instead ([`Self::apply_action`]). Either way
    /// [`ShellEvent::Input`] is raised -- nothing is both dropped and
    /// unrecorded, and nothing stops being emitted.
    fn emit_action(&mut self, event: ActionEvent) {
        let action = event.action();
        if action.implemented() {
            self.apply_action(&event);
        } else if !self.state.input.unimplemented.contains(&action) {
            self.state.input.unimplemented.push(action);
        }
        self.emit(ShellEvent::Input(event));
    }

    /// The viewport half of dispatch: `ZoomAtCursor`
    /// spends the wheel's accumulated delta into ladder steps about the
    /// cursor; `PanHorizontal` and the drag-driven `Pan` both read straight
    /// into [`crate::viewport::Viewport::pan_by`].
    fn apply_action(&mut self, event: &ActionEvent) {
        match *event {
            ActionEvent::Fired {
                action: Action::ZoomAtCursor,
                steps,
            } => self.zoom_wheel(steps),
            ActionEvent::Fired {
                action: Action::PanHorizontal,
                steps,
            } => self.pan_viewport(steps, 0.0),
            ActionEvent::Update {
                action: Action::Pan,
                dx,
                dy,
            } => self.pan_viewport(dx, dy),
            _ => {}
        }
    }

    /// Spends `steps` into whole 60 px ladder units, anchored at the last
    /// known cursor position -- one stop per accumulated 60 px. The
    /// accumulator resets on a sign change, so reversing direction never
    /// burns a leftover.
    fn zoom_wheel(&mut self, steps: f32) {
        const DETENT: f32 = 60.0;
        let Some(image) = self.state.image else {
            return;
        };
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let anchor = self.anchor();

        let accum = &mut self.state.input.wheel_accum;
        if *accum != 0.0 && accum.signum() != steps.signum() {
            *accum = 0.0;
        }
        *accum += steps;

        while self.state.input.wheel_accum.abs() >= DETENT {
            let dir = if self.state.input.wheel_accum > 0.0 {
                1
            } else {
                -1
            };
            self.state.input.wheel_accum -= DETENT * dir as f32;
            if let Some(viewport) = self.state.viewport.as_mut() {
                viewport.zoom_steps(dir, anchor, image, left);
            }
        }
    }

    /// A screen-pixel pan on whichever pane the cursor is over -- both
    /// panes share the one viewport, so only the size (identical for both)
    /// matters here.
    fn pan_viewport(&mut self, dx: f32, dy: f32) {
        let Some(image) = self.state.image else {
            return;
        };
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let Some(viewport) = self.state.viewport.as_mut() else {
            return;
        };
        viewport.pan_by(dx, dy, image, left);
    }

    /// The last known cursor position, mapped into pane-local coordinates:
    /// subtract the rect of whichever pane contains it, and when it is over
    /// the gutter, the divider or outside both, clamp it into the nearer
    /// pane's rect. Both panes carry the same viewport, so which pane the
    /// cursor is in does not change the result, only where inside it.
    fn anchor(&self) -> (f32, f32) {
        let (x, y) = self.state.input.last_pointer;
        let Some((left, right)) = self.state.panes else {
            return (x, y);
        };
        let pane = if pane_contains(left, x, y) {
            left
        } else if pane_contains(right, x, y) {
            right
        } else {
            let mid = (left.x + left.width + right.x) / 2.0;
            if x < mid { left } else { right }
        };
        (
            x.clamp(pane.x, pane.x + pane.width) - pane.x,
            y.clamp(pane.y, pane.y + pane.height) - pane.y,
        )
    }

    /// Installs both panes' current geometry, pushed by `ui/shell/canvas.slint`'s
    /// five `out` properties (`src/bridge.rs`'s `canvas-resized` handler and
    /// `src/main.rs`'s startup push). Fits the viewport on the first call
    /// after a project opens (`open_project` cannot, having no pane rect of
    /// its own); every later call re-`resize`s it, so fit stays sticky.
    pub fn set_pane_geometry(&mut self, left: PaneRect, right: PaneRect) {
        self.state.panes = Some((left, right));
        let Some(image) = self.state.image else {
            return;
        };
        match self.state.viewport.as_mut() {
            Some(viewport) => viewport.resize(image, left),
            None => self.state.viewport = Some(crate::viewport::fit(image, left)),
        }
    }

    /// One ladder step in, about the pane's centre -- a button has no
    /// cursor.
    pub fn zoom_in(&mut self) {
        self.zoom_button(1);
    }

    /// One ladder step out, about the pane's centre.
    pub fn zoom_out(&mut self) {
        self.zoom_button(-1);
    }

    fn zoom_button(&mut self, dir: i32) {
        let Some(image) = self.state.image else {
            return;
        };
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let Some(viewport) = self.state.viewport.as_mut() else {
            return;
        };
        let centre = (left.width / 2.0, left.height / 2.0);
        viewport.zoom_steps(dir, centre, image, left);
    }

    /// `Fit`: shows the whole image, centred, in whichever pane geometry is
    /// current.
    pub fn fit(&mut self) {
        let Some(image) = self.state.image else {
            return;
        };
        let Some((left, _)) = self.state.panes else {
            return;
        };
        self.state.viewport = Some(crate::viewport::fit(image, left));
    }

    /// The zoom cluster's readout string -- empty before a project is open.
    pub fn zoom_readout(&self) -> String {
        self.state
            .viewport
            .as_ref()
            .map(Viewport::readout)
            .unwrap_or_default()
    }

    /// Whether the viewport is at the ladder's minimum -- fit-to-window with
    /// one step below it.
    pub fn at_min_zoom(&self) -> bool {
        let (Some(image), Some((left, _)), Some(viewport)) = (
            self.state.image,
            self.state.panes,
            self.state.viewport.as_ref(),
        ) else {
            return false;
        };
        viewport.at_min(image, left)
    }

    /// Whether the viewport is at the ladder's maximum -- 500 %, when fit
    /// does not already exceed it.
    pub fn at_max_zoom(&self) -> bool {
        let (Some(image), Some((left, _)), Some(viewport)) = (
            self.state.image,
            self.state.panes,
            self.state.viewport.as_ref(),
        ) else {
            return false;
        };
        viewport.at_max(image, left)
    }

    /// The current viewport's visible source rect and its pane-local
    /// projection, for `crate::gpu_canvas::CanvasRenderer::redraw` and
    /// `ui/shell/canvas.slint`'s `FFPane` placement properties. `None`
    /// before a project is open or before the first pane geometry arrives.
    pub fn pane_draw(&self) -> Option<PaneDraw> {
        let image = self.state.image?;
        let (left, _) = self.state.panes?;
        self.state.viewport.as_ref()?.draw(image, left)
    }

    /// Sets the loaded project's image size and clears any prior viewport --
    /// `crate::canvas::open_project`'s own seam into `ShellState`, kept here
    /// so it can reach the private field. The next `set_pane_geometry` call
    /// fits a fresh viewport for the new image.
    pub(crate) fn set_project_image(&mut self, image: ImageSize) {
        self.state.image = Some(image);
        self.state.viewport = None;
    }

    /// The one wheel entry point. Axis normalisation first: an unmodified
    /// `delta_x`-dominant event resolves to the same row as
    /// `shift + delta_y`, because platforms commonly deliver a shifted
    /// wheel as a horizontal delta with the modifier consumed.
    /// `x`/`y` are the wheel's own cursor position -- `TouchArea` writes
    /// `mouse-x`/`mouse-y` before `scroll-event` is invoked.
    pub fn wheel(&mut self, delta_x: f32, delta_y: f32, x: f32, y: f32, mods: Modifiers) {
        self.state.input.last_pointer = (x, y);
        let (trigger, steps) = if delta_x.abs() > delta_y.abs() && !mods.shift {
            (Trigger::Wheel(mods.with_shift()), delta_x)
        } else {
            (Trigger::Wheel(mods), delta_y)
        };
        if let Some(action) = self.state.input.table.action(&trigger, self.state.mode) {
            self.emit_action(ActionEvent::Fired { action, steps });
        }
    }

    /// The one pointer-down/up entry point (also the Cancel path -- a
    /// cancelled grab is forwarded here as `PointerKind::Up` so a live drag
    /// never gets stuck on, step 10). `Down` starts a drag on a
    /// `Trigger::Drag` hit or fires once on a `Trigger::Click` hit; `Up`
    /// ends any live drag.
    pub fn pointer(&mut self, kind: PointerKind, button: Button, x: f32, y: f32, mods: Modifiers) {
        self.state.input.last_pointer = (x, y);
        match kind {
            PointerKind::Down => {
                let drag_trigger = Trigger::Drag { button, mods };
                if let Some(action) = self
                    .state
                    .input
                    .table
                    .action(&drag_trigger, self.state.mode)
                {
                    self.state.input.drag = Some(DragState {
                        action,
                        last: (x, y),
                    });
                    self.emit_action(ActionEvent::Start { action, x, y });
                    return;
                }
                let click_trigger = Trigger::Click { button, mods };
                if let Some(action) = self
                    .state
                    .input
                    .table
                    .action(&click_trigger, self.state.mode)
                {
                    self.emit_action(ActionEvent::Fired { action, steps: 0.0 });
                }
            }
            PointerKind::Up => {
                if let Some(drag) = self.state.input.drag.take() {
                    self.emit_action(ActionEvent::End {
                        action: drag.action,
                    });
                }
            }
            // Slint's `pointer-event` fires with `PointerEventKind::Move` on
            // every mouse movement over the `TouchArea`, *before* `moved` --
            // including the first movement of a drag. It carries no motion
            // of its own; `pointer_moved` is the real motion source, so a
            // live drag must not end here.
            PointerKind::Move => {}
        }
    }

    /// Meaningful only while a drag is live. Deltas are differences
    /// between successive positions -- `PointerEvent` carries no movement
    /// delta of its own (slint-ui/slint#13189).
    pub fn pointer_moved(&mut self, x: f32, y: f32) {
        let Some(drag) = &mut self.state.input.drag else {
            return;
        };
        let (last_x, last_y) = drag.last;
        let action = drag.action;
        drag.last = (x, y);
        self.emit_action(ActionEvent::Update {
            action,
            dx: x - last_x,
            dy: y - last_y,
        });
    }

    /// The one entry point for both key callbacks. `" "` sets/clears
    /// `space_held` (idempotent, absorbing a held key's auto-repeat) and
    /// resolves no row of its own; any other press looks up
    /// `Trigger::Key { key, mods }` -- a release resolves nothing.
    pub fn key(&mut self, pressed: bool, text: &str, mods: Modifiers) {
        if text == " " {
            self.set_space_held(pressed);
            return;
        }
        if !pressed {
            return;
        }
        let Some(key) = text.chars().next() else {
            return;
        };
        let trigger = Trigger::Key { key, mods };
        if let Some(action) = self.state.input.table.action(&trigger, self.state.mode) {
            self.emit_action(ActionEvent::Fired { action, steps: 0.0 });
        }
    }

    pub fn set_space_held(&mut self, held: bool) {
        self.state.input.space_held = held;
    }

    pub fn space_held(&self) -> bool {
        self.state.input.space_held
    }

    /// Called on focus loss so a Space held across an alt-tab does not
    /// latch (slint-ui/slint#6263).
    pub fn clear_input_modifiers(&mut self) {
        self.state.input.space_held = false;
    }

    pub fn input_rows(&self) -> Vec<InputRowView> {
        self.state.input.table.display_rows()
    }

    /// The settings modal's one section-level provenance line.
    pub fn input_provenance_line(&self) -> String {
        match &self.state.input.provenance {
            Provenance::Defaults => {
                "Loaded from the shipped defaults (0 rows overridden)".to_string()
            }
            Provenance::File { path, overridden } => {
                let noun = if *overridden == 1 { "row" } else { "rows" };
                format!(
                    "Loaded from {} ({overridden} {noun} overridden)",
                    path.display()
                )
            }
        }
    }

    pub fn unimplemented_actions(&self) -> &[Action] {
        &self.state.input.unimplemented
    }

    pub fn settings_open(&self) -> bool {
        self.state.settings_open
    }

    pub fn open_settings(&mut self) -> bool {
        self.state.settings_open = true;
        true
    }

    pub fn close_settings(&mut self) -> bool {
        self.state.settings_open = false;
        false
    }
}
