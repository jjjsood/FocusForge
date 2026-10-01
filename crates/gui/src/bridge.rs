// SPDX-License-Identifier: GPL-3.0-or-later

//! The Slint side of the seam, and the only file in this crate that names
//! `winit`. Owns the `AppController`, installs its observer sink, wires
//! every `AppWindow` callback as a one-line forward, and performs each
//! `ShellEvent` against the window.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::ComponentHandle;
use slint::Model as _;
use slint::winit_030::{CustomApplicationHandler, EventResult, WinitWindowAccessor, winit};
use winit::event_loop::ActiveEventLoop;
use winit::window::WindowId;

use crate::canvas::{
    CanvasFeed, EditedView, OverlayDraw, Pane, PaneJob, PaneSource, Preparer, RepaintGate,
    truncate_pane_label,
};
use crate::controller::{
    AppController, ChromeKey, DockTab, GroupingQuestion, JobView, LocateTarget, OpenAs,
    OpenOutcome, SettingsSection, ShellEvent, ShellMode,
};
use crate::diagnostics::{
    DiagnosticsStatus, InputBatch, InputSurface, finish_input_batches, input_trace_incomplete,
    record_coalesced_input, record_destroyed_window, record_input, record_shared_input,
    take_published_input,
};
use crate::diagnostics::{
    TimingCounter, measurement_scope, measurement_scope_with_fields, record_timing,
};
use crate::generated::{
    AppWindow, CheckpointEntryView as GeneratedCheckpointEntry, FFManifestRow,
    ImageRowView as GeneratedImageRow, ProjectCardView as GeneratedProjectCard, ProjectTabView,
    RecentRowView as GeneratedRecentRow, SectionHeadView, SettingsRow as GeneratedSettingsRow,
};
use crate::gpu_canvas::CanvasRenderer;
use crate::input_map::{Button, Modifiers, PointerKind};
use crate::job;
use crate::overlay::Thumbnail;
use crate::settings_panel::{HeadingState, PanelModel, RowKind};
use crate::viewport::{PaneDraw, PaneRect};

/// `slint::platform::PointerEventButton` -> `input_map::Button` -- the only
/// place in the crate that names the Slint type.
fn button(b: slint::platform::PointerEventButton) -> Button {
    match b {
        slint::platform::PointerEventButton::Left => Button::Left,
        slint::platform::PointerEventButton::Middle => Button::Middle,
        slint::platform::PointerEventButton::Right => Button::Right,
        _ => Button::Other,
    }
}

fn modifiers(controller: &AppController, ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers {
        ctrl,
        shift,
        alt,
        space: controller.space_held(),
    }
}

/// `install`'s return: the controller handle so `src/main.rs` can read the
/// same state the window shows, and the GPU-owned renderer cell -- an
/// `Rc<RefCell<..>>`, not held by the `Send + Sync` sink,
/// because it holds an `Rc` and the GPU state `perform` (the sink's own
/// path) cannot touch.
pub struct Installed {
    pub controller: Arc<Mutex<AppController>>,
    /// The live run's own poll timer, recreated by every `Fuse stack`/`Retry`
    /// press (`start_job_timer`) and dropped -- which deactivates it --
    /// the moment a new one replaces it.
    pub job_timer: Rc<RefCell<Option<slint::Timer>>>,
    /// The failure toast's auto-dismiss timer, recreated every time the
    /// toast opens and dropped -- deactivating it -- the moment a new
    /// failure replaces it.
    pub toast_timer: Rc<RefCell<Option<slint::Timer>>>,
}

/// The single open renderer, and which tab's planes it holds -- `None`
/// before anything has loaded. Built once in `src/main.rs`, before any
/// window exists, and shared with the drop route as well as `install`'s own
/// callbacks, so every path that can load a project shares the one cell.
/// The renderer outlives a plane clear: an unfused tab still has one, for
/// its selected frame alone.
#[derive(Default)]
pub struct Canvas {
    renderer: Option<CanvasRenderer>,
    /// Reads the panes' samples off the event loop; set by [`install`].
    preparer: Option<Preparer>,
    tab: Option<usize>,
    /// The left `FFPane`'s refusal lines, kept until a different frame opens
    /// or the same one succeeds; empty while the frame shows.
    frame_refusal: Vec<String>,
    /// The frame that last failed to open; not retried until the selection
    /// moves off it.
    failed_frame: Option<PathBuf>,
}

impl Canvas {
    /// Which tab's planes the renderer currently holds.
    pub fn tab(&self) -> Option<usize> {
        self.tab
    }

    /// Why the last [`Self::set_frame`] could not open its frame, one entry
    /// per line; empty when it opened.
    pub fn frame_refusal(&self) -> &[String] {
        &self.frame_refusal
    }

    /// Drops the planes and forgets the owning tab; the frame half is left
    /// alone.
    fn clear(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.clear_planes();
        }
        self.tab = None;
    }

    /// The open renderer, creating one first when this tab has never had
    /// anything to draw. Its source readers remain available before Slint
    /// finishes creating the surface-compatible device.
    fn renderer_mut(&mut self) -> &mut CanvasRenderer {
        if self.renderer.is_none() {
            self.renderer = Some(CanvasRenderer::new());
        }
        self.renderer.as_mut().expect("renderer was inserted above")
    }

    /// Attaches Slint's device and queue after rendering setup.
    pub fn attach_device(
        &mut self,
        device: slint::wgpu_29::wgpu::Device,
        queue: slint::wgpu_29::wgpu::Queue,
    ) {
        self.renderer_mut().attach_device(device, queue);
    }

    /// Releases only canvas resources owned by Slint's device.
    pub fn detach_device(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.detach_device();
        }
    }

    /// Opens `composite`/`zmap` as `tab`'s own planes, logging failures rather
    /// than panicking: a plane the pipeline itself just wrote
    /// failing to reopen is an environment problem (a full disk, a
    /// permissions change), not a bug worth crashing the window over. `tab`
    /// is recorded even when the open fails, so an unreadable plane file is
    /// not retried on every gesture. Reuses the renderer's own frame half
    /// rather than replacing it, since it may already hold the tab's
    /// selected frame.
    fn load(&mut self, tab: usize, composite: &Path, zmap: &Path) {
        self.tab = Some(tab);
        let renderer = self.renderer_mut();
        if let Err(err) = renderer.set_planes(composite, zmap) {
            tracing::error!("failed to open the project's planes: {err}");
        }
    }

    /// Opens `path` as the left pane's source, creating the renderer first
    /// when this tab has never had planes to draw. A no-op when `path` and
    /// `view` are both already open, or `path` is the frame that last
    /// failed -- so a new fuse over the same selected path still reloads
    /// through its own, changed view. `None` drops whatever frame was open.
    /// A failed open names the frame as missing or changed
    /// ([`job::frame_refusal_lines`]).
    pub fn set_frame(&mut self, path: Option<&Path>, view: Option<crate::canvas::FrameView>) {
        if path.is_some() && self.failed_frame.as_deref() == path {
            return;
        }
        let renderer = self.renderer_mut();
        let Some(path) = path else {
            renderer.clear_frame();
            self.frame_refusal.clear();
            self.failed_frame = None;
            return;
        };
        if renderer.frame_path() == Some(path) && renderer.frame_view() == view {
            return;
        }
        match renderer.set_frame(path, view, job::host_budget()) {
            Ok(()) => {
                self.frame_refusal.clear();
                self.failed_frame = None;
            }
            Err(err) => {
                eprintln!("failed to open the frame: {err}");
                self.frame_refusal = job::frame_refusal_lines(path);
                self.failed_frame = Some(path.to_path_buf());
            }
        }
    }

    /// Hands the worker one request for every pane whose texture does not
    /// yet show its draw; a request still pending is replaced by it.
    fn request_reads(
        &mut self,
        frame: Option<&PaneDraw>,
        plane: Option<&PaneDraw>,
        generation: Option<u64>,
        overlay: Option<&OverlayDraw>,
    ) {
        let (Some(renderer), Some(preparer)) = (self.renderer.as_mut(), self.preparer.as_ref())
        else {
            return;
        };
        let jobs: Vec<PaneJob> = frame
            .and_then(|draw| renderer.request_frame(draw))
            .into_iter()
            .chain(plane.and_then(|draw| renderer.request_planes(draw, generation, overlay)))
            .collect();
        if !jobs.is_empty() {
            preparer.submit(jobs);
        }
    }

    /// Renders whatever the worker completed into the panes' textures, the
    /// live `edited` session applied over the sealed reads; `false` when
    /// nothing had completed.
    fn publish_prepared(&mut self, edited: Option<&EditedView>) -> bool {
        let Some(done) = self.preparer.as_ref().and_then(Preparer::take) else {
            return false;
        };
        if let Some(renderer) = self.renderer.as_mut() {
            for prepared in &done {
                let stamp = prepared.job.stamp;
                let _scope = measurement_scope_with_fields(
                    "gui_canvas_publish",
                    stamp,
                    stamp,
                    1,
                    0,
                    false,
                    0,
                );
                renderer.publish_edited(
                    &prepared.job,
                    &prepared.samples,
                    &prepared.indices,
                    edited,
                );
            }
        }
        if let Some(preparer) = self.preparer.as_ref() {
            preparer.recycle(done);
        }
        true
    }
}

thread_local! {
    /// What the worker's wake runs on the event loop: publish, then show.
    static PREPARED_HANDLER: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Publishes the worker's completed reads and shows every pane where the
/// controller's current view puts it.
fn publish_completed(
    app: &AppWindow,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
) {
    let (frame_draw, plane_draw) = {
        let controller = controller.lock().unwrap();
        if !canvas
            .borrow_mut()
            .publish_prepared(controller.edited_view().as_ref())
        {
            return;
        }
        (controller.frame_draw(), controller.plane_draw())
    };
    show_panes(app, &canvas.borrow(), frame_draw, plane_draw);
    // The textures were rendered into in place; Slint sees no property change.
    app.window().request_redraw();
}

/// Sets each pane's source and placement from the texture it already holds,
/// scaled and offset to `frame_draw`/`plane_draw`; a pane holding nothing is
/// cleared rather than left showing the previous tab.
fn show_panes(
    app: &AppWindow,
    canvas: &Canvas,
    frame_draw: Option<PaneDraw>,
    plane_draw: Option<PaneDraw>,
) {
    let renderer = canvas.renderer.as_ref();
    let frame = frame_draw.and_then(|draw| {
        let placed = renderer?.placement(Pane::Frame, &draw)?;
        Some((draw, placed))
    });
    match frame {
        Some((draw, (image, dst))) => {
            app.set_frame_source(image);
            app.set_frame_image_x(dst.0);
            app.set_frame_image_y(dst.1);
            app.set_frame_image_w(dst.2);
            app.set_frame_image_h(dst.3);
            app.set_pixelated(draw.pixelated);
        }
        None => {
            app.set_frame_source(slint::Image::default());
            app.set_frame_image_x(0.0);
            app.set_frame_image_y(0.0);
            app.set_frame_image_w(0.0);
            app.set_frame_image_h(0.0);
        }
    }

    let planes = plane_draw.and_then(|draw| {
        let renderer = renderer?;
        let plane = renderer.plane();
        let placed = renderer.placement(plane.into(), &draw)?;
        Some((draw, plane, placed))
    });
    match planes {
        Some((draw, plane, (image, dst))) => {
            match plane {
                PaneSource::Composite => {
                    app.set_composite_source(image);
                    app.set_zmap_source(slint::Image::default());
                }
                PaneSource::ZMap => {
                    app.set_zmap_source(image);
                    app.set_composite_source(slint::Image::default());
                }
            }
            app.set_image_x(dst.0);
            app.set_image_y(dst.1);
            app.set_image_w(dst.2);
            app.set_image_h(dst.3);
            app.set_pixelated(draw.pixelated);
        }
        None => {
            app.set_composite_source(slint::Image::default());
            app.set_zmap_source(slint::Image::default());
            app.set_image_x(0.0);
            app.set_image_y(0.0);
            app.set_image_w(0.0);
            app.set_image_h(0.0);
        }
    }
}

/// A fresh model for `lines`, or `None` when `current` already holds them:
/// a pane's message lines are replaced only on a change, since a fresh model
/// re-mounts every line.
fn changed_lines(
    current: slint::ModelRc<slint::SharedString>,
    lines: &[String],
) -> Option<slint::ModelRc<slint::SharedString>> {
    if current
        .iter()
        .map(|line| line.to_string())
        .eq(lines.iter().cloned())
    {
        return None;
    }
    let lines: Vec<slint::SharedString> = lines.iter().map(|line| line.as_str().into()).collect();
    Some(slint::ModelRc::new(slint::VecModel::from(lines)))
}

/// Reconciles the open renderer cell with the active tab
/// ([`AppController::canvas_feed`]) and the selected frame, then places both
/// panes from the textures they hold, scaled and offset to the new view, plus
/// the frame label, `zoom-readout` and `project-loaded`. Exact samples are
/// read on the worker and shown by publish_completed; nothing here reads a
/// source. A pane holding nothing is cleared, not left on the previous tab.
pub fn repaint(
    app: &AppWindow,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
) {
    controller.lock().unwrap().flush_live_stroke();
    let _repaint = measurement_scope("gui_repaint");
    let _publication = take_published_input(InputSurface::Canvas).map(|batch| {
        measurement_scope_with_fields(
            "gui_canvas_publication",
            batch.first,
            batch.last,
            batch.count,
            batch.coalesced,
            batch.incomplete,
            batch.publication,
        )
    });
    let (feed, render_started, message_lines) = {
        let loaded = canvas.borrow().tab();
        let mut controller = controller.lock().unwrap();
        controller.fit_if_unfitted();
        let feed = controller.canvas_feed(loaded);
        (
            feed,
            controller.take_render_started(),
            controller.composite_message_lines(),
        )
    };
    if render_started {
        poll_checkpoint_render(app.as_weak(), controller.clone(), canvas.clone());
    }
    if let Some(lines) = changed_lines(app.get_composite_message_lines(), &message_lines) {
        app.set_composite_message_lines(lines);
    }
    match feed {
        CanvasFeed::Keep => {}
        CanvasFeed::Clear => canvas.borrow_mut().clear(),
        CanvasFeed::Load {
            tab,
            composite,
            zmap,
        } => canvas.borrow_mut().load(tab, &composite, &zmap),
    }

    let (
        frame_path,
        frame_view,
        frame_label,
        frame_draw,
        plane_draw,
        readout,
        project_loaded,
        generation,
        overlay,
    ) = {
        let controller = controller.lock().unwrap();
        (
            controller.selected_frame_path(),
            controller.selected_frame_view(),
            controller.frame_pane_label(),
            controller.frame_draw(),
            controller.plane_draw(),
            controller.zoom_readout(),
            controller.active_project().is_some(),
            controller.edited_view().map(|view| view.tiles.generation()),
            controller.overlay_draw(),
        )
    };
    canvas
        .borrow_mut()
        .set_frame(frame_path.as_deref(), frame_view);

    app.set_zoom_readout(readout.into());
    app.set_project_loaded(project_loaded);
    app.set_frame_label(truncate_pane_label(&frame_label).into());
    if let Some(lines) = changed_lines(
        app.get_frame_message_lines(),
        canvas.borrow().frame_refusal(),
    ) {
        app.set_frame_message_lines(lines);
    }
    push_brush_cursor(app, &controller.lock().unwrap());

    canvas.borrow_mut().request_reads(
        frame_draw.as_ref(),
        plane_draw.as_ref(),
        generation,
        overlay.as_ref(),
    );
    show_panes(app, &canvas.borrow(), frame_draw, plane_draw);
}

/// Emits the final pending input batch before diagnostics closes its trace.
pub fn finish_input_trace() {
    for batch in finish_input_batches().into_iter().flatten() {
        let _scope = measurement_scope_with_fields(
            "gui_publication_pending",
            batch.first,
            batch.last,
            batch.count,
            batch.coalesced,
            batch.incomplete,
            batch.publication,
        );
    }
}

/// Polls [`AppController::settle_checkpoint_render`] every [`job::POLL`]
/// until the background render settles, then repaints -- the render's
/// worker cannot reach the canvas.
fn poll_checkpoint_render(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
) {
    slint::Timer::single_shot(job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        if controller.lock().unwrap().settle_checkpoint_render() {
            repaint(&app, &controller, &canvas);
        } else {
            poll_checkpoint_render(app_weak, controller, canvas);
        }
    });
}

/// Polls [`AppController::settle_export`] every [`job::POLL`] until the
/// background export write settles, then pushes the modal's own state --
/// [`poll_checkpoint_render`]'s twin for [`job::export_position`].
fn poll_export(app_weak: slint::Weak<AppWindow>, controller: Arc<Mutex<AppController>>) {
    slint::Timer::single_shot(job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        if controller.lock().unwrap().settle_export() {
            push_export_dialog(&app, &controller.lock().unwrap());
        } else {
            poll_export(app_weak, controller);
        }
    });
}

fn terminal_input_scope(kind: &'static str, batch: InputBatch) {
    let _scope = measurement_scope_with_fields(
        kind,
        batch.first,
        batch.last,
        batch.count,
        batch.coalesced,
        batch.incomplete,
        batch.publication,
    );
}

fn direct_publication_scope(surface: InputSurface) {
    if let Some(batch) = take_published_input(surface) {
        terminal_input_scope("gui_direct_publication", batch);
    }
}

fn model_publication_scope(event: &ShellEvent) -> Option<crate::diagnostics::MeasurementScope> {
    let surface = match event {
        ShellEvent::WindowMaximizeToggled(_)
        | ShellEvent::ModeChanged(_)
        | ShellEvent::DockTabSelected(_)
        | ShellEvent::DiagnosticsWriteFinished(_)
        | ShellEvent::DiagnosticsScanFinished { .. }
        | ShellEvent::JobChanged
        | ShellEvent::LoadStarted
        | ShellEvent::LoadReady
        | ShellEvent::ImportReady
        | ShellEvent::CheckpointsChanged => InputSurface::Other,
        ShellEvent::ProjectsChanged
        | ShellEvent::TabScrollChanged
        | ShellEvent::QuestionRaised(_)
        | ShellEvent::EntryRefused(_)
        | ShellEvent::MismatchRaised
        | ShellEvent::LocateReady
        | ShellEvent::OverlayChanged
        | ShellEvent::GalleryScrolled => InputSurface::Gallery,
        ShellEvent::SettingsChanged | ShellEvent::SettingsScrolled => InputSurface::Settings,
        ShellEvent::ImagesChanged | ShellEvent::ImagesScrolled => InputSurface::Images,
        ShellEvent::EditorChanged => InputSurface::Other,
        ShellEvent::WindowDragRequested
        | ShellEvent::WindowMinimizeRequested
        | ShellEvent::WindowCloseRequested
        | ShellEvent::CheckpointExportRequested
        | ShellEvent::PaneUndockRequested(_)
        | ShellEvent::Input(_)
        | ShellEvent::LoadProgress { .. } => return None,
    };
    take_published_input(surface).map(|batch| {
        measurement_scope_with_fields(
            "gui_model_publication",
            batch.first,
            batch.last,
            batch.count,
            batch.coalesced,
            batch.incomplete,
            batch.publication,
        )
    })
}

fn record_direct_input() {
    let _scope = measurement_scope("gui_input_origin_unavailable");
}

fn record_settings_input() {
    record_surface_input(InputSurface::Settings);
}

fn record_images_input() {
    record_surface_input(InputSurface::Images);
}

fn record_gallery_input() {
    record_surface_input(InputSurface::Gallery);
}

fn record_surface_input(surface: InputSurface) {
    let sequence = record_input(surface);
    let _scope = measurement_scope_with_fields(
        "gui_input_callback",
        sequence,
        sequence,
        1,
        0,
        input_trace_incomplete(),
        0,
    );
}

fn record_shared_surface_input(primary: InputSurface, secondary: InputSurface) {
    let sequence = record_shared_input(primary, secondary);
    let _scope = measurement_scope_with_fields(
        "gui_input_callback",
        sequence,
        sequence,
        1,
        0,
        input_trace_incomplete(),
        0,
    );
}

fn record_native_surface_input(surface: InputSurface) {
    let sequence = record_input(surface);
    let _scope = measurement_scope_with_fields(
        "gui_native_callback",
        sequence,
        sequence,
        1,
        0,
        input_trace_incomplete(),
        0,
    );
}

fn record_layout_callback() {
    let _scope = measurement_scope("gui_layout_callback");
}

/// Locks `controller`, runs `mutate` against it, then repaints if the
/// window is still alive.
fn dispatch(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    mutate: impl FnOnce(&mut AppController),
) {
    let sequence = record_input(InputSurface::Canvas);
    let _input = measurement_scope_with_fields(
        "gui_input",
        sequence,
        sequence,
        1,
        0,
        input_trace_incomplete(),
        0,
    );
    mutate(&mut controller.lock().unwrap());
    if let Some(app) = app_weak.upgrade() {
        repaint(&app, controller, canvas);
    } else {
        for batch in record_destroyed_window().into_iter().flatten() {
            terminal_input_scope("gui_input_window_destroyed", batch);
        }
    }
}

fn dispatch_gallery(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    mutate: impl FnOnce(&mut AppController),
) {
    dispatch_shared(InputSurface::Gallery, controller, canvas, app_weak, mutate);
}

/// [`dispatch`] for a callback whose input belongs to `surface` as well as
/// the canvas it repaints.
fn dispatch_shared(
    surface: InputSurface,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    mutate: impl FnOnce(&mut AppController),
) {
    record_shared_surface_input(surface, InputSurface::Canvas);
    mutate(&mut controller.lock().unwrap());
    if let Some(app) = app_weak.upgrade() {
        repaint(&app, controller, canvas);
    } else {
        for batch in record_destroyed_window().into_iter().flatten() {
            terminal_input_scope("gui_input_window_destroyed", batch);
        }
    }
}

/// [`dispatch`] for pointer events: `mutate` runs at once, the repaint is
/// deferred to one zero-delay timer, so every pointer event delivered before
/// it fires shares one repaint and a drag costs one per event-loop turn.
fn dispatch_coalesced(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    gate: &Rc<RepaintGate>,
    mutate: impl FnOnce(&mut AppController),
) {
    let sequence = record_input(InputSurface::Canvas);
    let _input = measurement_scope_with_fields(
        "gui_input",
        sequence,
        sequence,
        1,
        0,
        input_trace_incomplete(),
        0,
    );
    mutate(&mut controller.lock().unwrap());
    if !gate.request() {
        record_coalesced_input(InputSurface::Canvas);
        return;
    }
    let (controller, canvas, app_weak, gate) = (
        controller.clone(),
        canvas.clone(),
        app_weak.clone(),
        gate.clone(),
    );
    slint::Timer::single_shot(Duration::ZERO, move || {
        if gate.take()
            && let Some(app) = app_weak.upgrade()
        {
            repaint(&app, &controller, &canvas);
        } else {
            for batch in record_destroyed_window().into_iter().flatten() {
                terminal_input_scope("gui_input_window_destroyed", batch);
            }
        }
    });
}

/// Reads `FFCanvas`'s five geometry `out` properties (mirrored at the root)
/// and pushes them into the controller as the two panes' current rects.
/// `pub` so `src/main.rs`'s startup push reads the same properties through
/// the same function rather than re-listing them.
pub fn read_pane_geometry(app: &AppWindow) -> (PaneRect, PaneRect) {
    let y = app.get_canvas_pane_left_y();
    let width = app.get_canvas_pane_left_width();
    let height = app.get_canvas_pane_left_height();
    (
        PaneRect {
            x: app.get_canvas_pane_left_x(),
            y,
            width,
            height,
        },
        PaneRect {
            x: app.get_canvas_pane_right_x(),
            y,
            width,
            height,
        },
    )
}

/// Locks `controller`, runs `mutate` against it, then pushes the resulting
/// manifest scroll offset into the window if it's still alive -- the tail
/// shared by every manifest-scrollbar callback below.
fn dispatch_manifest_scroll(
    controller: &Arc<Mutex<AppController>>,
    app_weak: &slint::Weak<AppWindow>,
    mutate: impl FnOnce(&mut AppController),
) {
    let offset = {
        let mut controller = controller.lock().unwrap();
        mutate(&mut controller);
        controller.manifest_scroll_offset()
    };
    if let Some(app) = app_weak.upgrade() {
        app.set_diagnostics_manifest_scroll_offset(offset);
        direct_publication_scope(InputSurface::Other);
    }
}

/// Rebuilds the diagnostics panel's whole view from the controller and
/// pushes it into the window -- the panel carries no state of its own, so
/// every change that could affect it re-runs this rather than patching one
/// property.
pub fn push_diagnostics_view(app: &AppWindow, controller: &Arc<Mutex<AppController>>) {
    let view = controller.lock().unwrap().diagnostics_view();
    let rows: Vec<FFManifestRow> = view
        .entries
        .iter()
        .map(|entry| FFManifestRow {
            name: entry.name.clone().into(),
            value: entry.value.clone().into(),
            not_redacted: entry.not_redacted,
        })
        .collect();
    app.set_diagnostics_rows(Rc::new(slint::VecModel::from(rows)).into());
    app.set_diagnostics_note(view.note.unwrap_or_default().into());
    app.set_diagnostics_include_minidumps(view.include_minidumps);
    app.set_diagnostics_destination(view.destination.into());
    let status = match view.status {
        DiagnosticsStatus::Idle => "idle",
        DiagnosticsStatus::ExistingFile => "existing-file",
        DiagnosticsStatus::Writing => "writing",
        DiagnosticsStatus::Written => "written",
        DiagnosticsStatus::Failed => "failed",
    };
    app.set_diagnostics_status(status.into());
    app.set_diagnostics_message(view.message.into());
    app.set_diagnostics_primary_label(view.primary_label.into());
    app.set_diagnostics_busy(view.busy);
    app.set_diagnostics_loading(view.loading);
    app.set_diagnostics_manifest_scroll_offset(view.manifest_scroll_offset);
    app.set_diagnostics_manifest_content_height(view.manifest_content_height);
    app.set_diagnostics_destination_scroll_offset(view.destination_scroll_offset);
}

/// Pushes the crash notice's own two properties into the window -- mirrors
/// [`push_diagnostics_view`] for the toast.
pub fn push_crash_notice(app: &AppWindow, controller: &Arc<Mutex<AppController>>) {
    let controller = controller.lock().unwrap();
    app.set_crash_notice_visible(controller.crash_notice_visible());
    app.set_crash_notice_title(controller.crash_notice_text().into());
}

/// Scans for unreported crash evidence and pushes the notice, then --
/// only when it is showing -- arms its auto-dismiss timeout. The timer is
/// armed before `run()`, the same precedent `src/main.rs`'s own
/// `--exit-after` timer sets.
pub fn install_crash_notice(controller: &Arc<Mutex<AppController>>, app: &AppWindow) {
    controller.lock().unwrap().scan_crash_notice();
    push_crash_notice(app, controller);
    if !controller.lock().unwrap().crash_notice_visible() {
        return;
    }
    let timer_controller = controller.clone();
    let app_weak = app.as_weak();
    slint::Timer::single_shot(Duration::from_secs(10), move || {
        timer_controller.lock().unwrap().crash_notice_timed_out();
        if let Some(app) = app_weak.upgrade() {
            push_crash_notice(&app, &timer_controller);
        }
    });
}

/// Pushes the controller's project-tab state (`project_tabs`,
/// `tab_scroll_x`, the two scroll-enabled flags, `show_empty_state`,
/// `project_loaded`, `plus_selected`) into
/// `app` -- the one place `ShellEvent::ProjectsChanged` is performed. `pub`
/// so a test can call it directly without a real event loop (`install`'s
/// sink needs one; this function does not).
pub fn push_projects(app: &AppWindow, controller: &Arc<Mutex<AppController>>) {
    let controller = controller.lock().unwrap();
    push_tab_rows(app, &controller);
    push_tab_scroll(app, &controller);
    app.set_show_empty_state(controller.active_project().is_none());
    // The grouping question closes itself here rather than in its own
    // answer handler -- answering opens a project, which raises this same
    // `ProjectsChanged` push, so the dialog only needs to track whether a
    // question is still pending.
    app.set_question_open(controller.pending_question().is_some());
    match controller
        .pending_close()
        .or_else(|| controller.pending_quit_copy())
        .or_else(|| controller.pending_restore_copy())
        .or_else(|| controller.pending_fuse_copy())
    {
        Some(copy) => {
            app.set_close_guard_title(copy.title.into());
            app.set_close_guard_context(copy.context.into());
            app.set_close_guard_body(copy.body.into());
            app.set_close_guard_info(copy.info.into());
            app.set_close_guard_secondary_label(copy.secondary_label.into());
            app.set_close_guard_confirm_label(copy.confirm_label.into());
            app.set_close_guard_open(true);
        }
        None => app.set_close_guard_open(false),
    }
    match controller.pending_checkpoint_delete_copy() {
        Some((context, body)) => {
            app.set_checkpoint_delete_context(context.into());
            app.set_checkpoint_delete_body(body.into());
            app.set_checkpoint_delete_open(true);
        }
        None => app.set_checkpoint_delete_open(false),
    }
    // The dock's own gate for the tab row, the settings panel and the
    // "No project open." note (`ui/shell/dock.slint`) -- otherwise stuck at
    // whatever a canvas gesture (`repaint`, above) last set it to, since a
    // tab switch or close never fires one of its own.
    app.set_project_loaded(controller.active_project().is_some());
    app.set_composite_ready(controller.composite_ready());
    push_overlay_controls(app, &controller);
    app.set_checkpoint_line(controller.checkpoint_panel_line().into());
    app.set_plus_selected(controller.plus_selected());
    app.set_skipped_notice(controller.notice().unwrap_or_default().into());
    // The active tab may have changed with no `JobChanged`/poll event of its
    // own in between -- push its own run onto the bar right here rather than
    // leaving the words the previously active tab last wrote.
    push_job(app, &controller.job_view());
    push_settings_panel(app, &controller);
    push_images_panel(app, &controller);
    push_checkpoint_panel(app, &controller);
    push_projects_overlay(app, &controller);
    push_divergence(app, &controller);
}

/// Pushes the tab strip's own rows -- title, unsaved marker, selection and
/// each tab's own run mark -- so a mark appears or clears the moment that
/// tab's run state changes, not only when the project list itself does.
/// Also pushes the toolbar's own preview of what a `Fuse stack` press on the
/// active tab would do right now, read off that tab's own row.
fn push_tab_rows(app: &AppWindow, controller: &AppController) {
    let rows = controller.project_tabs();
    let active = controller.active_project();
    let would_queue = active
        .and_then(|i| rows.get(i))
        .is_some_and(|row| row.would_queue);
    app.set_toolbar_would_queue(would_queue);
    let tabs: Vec<ProjectTabView> = rows
        .into_iter()
        .map(|t| ProjectTabView {
            title: t.title.into(),
            unsaved: t.unsaved,
            selected: t.selected,
            run_mark_icon: t.run_mark_icon.into(),
        })
        .collect();
    app.set_project_tabs(std::rc::Rc::new(slint::VecModel::from(tabs)).into());
}

/// Pushes the tab strip's scroll offset, overflow layout and two arrow
/// flags -- the one place `ShellEvent::TabScrollChanged` is performed.
fn push_tab_scroll(app: &AppWindow, controller: &AppController) {
    app.set_tab_scroll_x(controller.tab_scroll_x());
    app.set_tabs_overflow(controller.tabs_overflow());
    app.set_can_scroll_left(controller.can_scroll_left());
    app.set_can_scroll_right(controller.can_scroll_right());
}

/// How long the failure toast stays up before closing itself.
const TOAST_AUTO_DISMISS: Duration = Duration::from_secs(6);

/// Opens the failure toast with `message` and arms its auto-dismiss timer,
/// replacing any timer already running -- dropping the old one deactivates
/// it, so a second failure while the first toast is still up restarts the
/// count rather than closing early.
fn open_job_toast(
    app: &AppWindow,
    message: String,
    toast_timer: &Rc<RefCell<Option<slint::Timer>>>,
) {
    app.set_job_toast_message(message.into());
    app.set_job_toast_open(true);
    let timer = slint::Timer::default();
    let app_weak = app.as_weak();
    timer.start(
        slint::TimerMode::SingleShot,
        TOAST_AUTO_DISMISS,
        move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_job_toast_open(false);
            }
        },
    );
    *toast_timer.borrow_mut() = Some(timer);
}

/// Pushes the run's current view onto the toolbar/bottom-bar properties --
/// the one place `ShellEvent::JobChanged` and the poll timer both write.
fn push_job(app: &AppWindow, view: &JobView) {
    app.set_toolbar_run_state(view.toolbar_run_state.as_str().into());
    app.set_stage_word(view.stage_word.as_str().into());
    app.set_step_text(view.step_text.as_str().into());
    app.set_bar_fraction(view.fraction);
    app.set_bar_tone(view.bar_tone.as_str().into());
    app.set_eta_text(view.eta_text.as_str().into());
    app.set_fuse_enabled(view.fuse_enabled);
}

/// Installs a repeating timer, on the window's own clock ([`job::POLL`]),
/// that polls the live run's progress onto the toolbar/bottom-bar
/// properties, opens the failure toast the moment a run fails, and stops
/// itself once the run settles -- what `Fuse stack`/`Retry` starts, and the
/// only place [`AppController::settle_job`] is called from. A completed run
/// pushes the projects and repaints regardless of which tab it ran against;
/// `repaint`'s own reconcile decides whether its planes belong on screen.
fn start_job_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    job_timer: Rc<RefCell<Option<slint::Timer>>>,
    toast_timer: Rc<RefCell<Option<slint::Timer>>>,
) {
    let timer = slint::Timer::default();
    let timer_slot = job_timer.clone();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let _poll = measurement_scope("gui_job_poll");
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        record_timing(TimingCounter::JobPolls, 1);
        let settled = controller.lock().unwrap().settle_job();
        if settled.is_some() {
            push_projects(&app, &controller);
            repaint(&app, &controller, &canvas);
        } else if let Some(notification) = controller.lock().unwrap().take_settled_failure() {
            open_job_toast(
                &app,
                notification.operation.summary().to_string(),
                &toast_timer,
            );
        }
        let view = controller.lock().unwrap().job_view();
        let live = controller.lock().unwrap().work_pending();
        push_job(&app, &view);
        push_projects_overlay(&app, &controller.lock().unwrap());
        push_divergence(&app, &controller.lock().unwrap());
        if !live {
            // The run just settled -- the moment the images/settings panels'
            // own whole-panel disable must lift again.
            let controller = controller.lock().unwrap();
            push_settings_panel(&app, &controller);
            push_images_panel(&app, &controller);
        }
        if !live && let Some(t) = timer_slot.borrow().as_ref() {
            t.stop();
        }
    });
    *job_timer.borrow_mut() = Some(timer);
}

/// Starts the job poll timer when an answer to a dialog left a run live or
/// queued -- the fuse question's answers start one from a dialog callback.
fn poll_if_work_pending(
    app_weak: &slint::Weak<AppWindow>,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    job_timer: &Rc<RefCell<Option<slint::Timer>>>,
    toast_timer: &Rc<RefCell<Option<slint::Timer>>>,
) {
    if controller.lock().unwrap().work_pending() {
        start_job_timer(
            app_weak.clone(),
            controller.clone(),
            canvas.clone(),
            job_timer.clone(),
            toast_timer.clone(),
        );
    }
}

/// Polls [`AppController::load_in_flight`], opening each `queued` path once
/// the one before it settles, then repaints -- the sink that settles a load
/// cannot reach the canvas. Replaces (and so stops) any timer in `slot`.
pub fn start_load_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    slot: &Rc<RefCell<Option<slint::Timer>>>,
    queued: Vec<PathBuf>,
) {
    let mut queued = queued.into_iter();
    let timer_slot = slot.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        if controller.lock().unwrap().load_in_flight() {
            return;
        }
        if let Some(next) = queued.next() {
            controller
                .lock()
                .unwrap()
                .open_paths(std::slice::from_ref(&next));
            return;
        }
        repaint(&app, &controller, &canvas);
        if let Some(t) = timer_slot.borrow().as_ref() {
            t.stop();
        }
    });
    *slot.borrow_mut() = Some(timer);
}

/// `RowKind`'s own string reading -- the settings panel's `.slint` side
/// branches on this, never on a `ParamKind` or an algorithm id.
fn row_kind_str(kind: RowKind) -> &'static str {
    match kind {
        RowKind::SectionHeading => "heading",
        RowKind::Picker => "picker",
        RowKind::LinearFloat => "linear-float",
        RowKind::LogFloat => "log-float",
        RowKind::Int => "int",
        RowKind::Bool => "bool",
        RowKind::Enum => "enum",
        RowKind::Error => "error",
        RowKind::ResetAll => "reset-all",
    }
}

/// `HeadingState`'s own string reading, matching `FFSectionHeading`'s
/// `provenance` property.
fn heading_state_str(state: HeadingState) -> &'static str {
    match state {
        HeadingState::Default => "default",
        HeadingState::Explicit => "explicit",
        HeadingState::Pending => "pending",
    }
}

/// Brings the live `VecModel` behind `existing` to `rows` in place: trims or
/// extends its tail and calls `set_row_data` only where a row differs, so an
/// unchanged row keeps its element tree -- a fresh model would rebuild every
/// row's tree and drop a `TouchArea` mid-gesture. Any other model is replaced
/// through `install`. Returns how many rows were notified.
fn sync_rows<T: Clone + PartialEq + 'static>(
    existing: slint::ModelRc<T>,
    rows: Vec<T>,
    install: impl FnOnce(slint::ModelRc<T>),
) -> usize {
    let Some(model) = existing.as_any().downcast_ref::<slint::VecModel<T>>() else {
        let count = rows.len();
        install(std::rc::Rc::new(slint::VecModel::from(rows)).into());
        return count;
    };
    let mut updates = 0;
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
        updates += 1;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if i >= model.row_count() {
            model.push(row);
            updates += 1;
        } else if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
            updates += 1;
        }
    }
    updates
}

/// `live` when it already lists exactly `strings`, a fresh model otherwise:
/// `ModelRc` equality is model identity, so only a kept model lets its row
/// compare equal.
fn retained_strings(
    live: Option<slint::ModelRc<slint::SharedString>>,
    strings: &[String],
) -> slint::ModelRc<slint::SharedString> {
    match live {
        Some(model)
            if model.row_count() == strings.len()
                && model.iter().zip(strings).all(|(a, b)| a.as_str() == b) =>
        {
            model
        }
        _ => std::rc::Rc::new(slint::VecModel::from(
            strings
                .iter()
                .map(slint::SharedString::from)
                .collect::<Vec<_>>(),
        ))
        .into(),
    }
}

/// Pushes the settings panel's rows through `sync_rows`, reusing each live
/// row's choice models while their strings are unchanged. The one place
/// `ShellEvent::SettingsChanged` is performed; also called from
/// `push_projects` so a tab switch or close rebuilds the panel for the
/// newly active tab. `pub` so a test can push a real, unmodified row set
/// and then edit one row's own text without going through a live event.
pub fn push_settings_panel(app: &AppWindow, controller: &AppController) {
    let _scope = measurement_scope("gui_settings_rows");
    let panel = controller.settings_panel();
    let existing = app.get_settings_rows();
    let rows: Vec<GeneratedSettingsRow> = panel
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let live = existing.row_data(i);
            GeneratedSettingsRow {
                kind: row_kind_str(row.kind).into(),
                section: row.section.clone().into(),
                id: row.id.clone().into(),
                label: row.label.clone().into(),
                value_text: row.value_text.clone().into(),
                fraction: row.fraction,
                choices: retained_strings(live.as_ref().map(|r| r.choices.clone()), &row.choices),
                choice_ids: retained_strings(live.map(|r| r.choice_ids), &row.choice_ids),
                selected: row.selected.clone().into(),
                menu_open: row.menu_open,
                heading_state: heading_state_str(row.heading_state).into(),
                top: row.top,
            }
        })
        .collect();
    let updates = sync_rows(existing, rows, |model| app.set_settings_rows(model));
    record_timing(TimingCounter::RowUpdates, updates as u64);
    app.set_settings_content_height(panel.content_height);
    push_settings_scroll(app, &panel);
    app.set_settings_enabled(!controller.job_running());
}

/// Pushes the settings panel's scroll offset and sticky heading alone -- the
/// one place `ShellEvent::SettingsScrolled` is performed. Rows sit in content
/// space, so a scroll moves no row.
pub fn push_settings_scroll(app: &AppWindow, panel: &PanelModel) {
    app.set_settings_scroll_y(panel.scroll_y);
    let sticky = panel.sticky.map(|i| &panel.rows[i]);
    app.set_settings_sticky(SectionHeadView {
        visible: sticky.is_some(),
        section: sticky
            .map(|r| r.section.as_str())
            .unwrap_or_default()
            .into(),
        label: sticky.map(|r| r.label.as_str()).unwrap_or_default().into(),
        provenance: sticky
            .map(|r| heading_state_str(r.heading_state))
            .unwrap_or("default")
            .into(),
        push: panel.sticky_push,
    });
}

/// Pushes the `Images` panel's whole state -- [`push_settings_panel`]'s
/// twin.
pub fn push_images_panel(app: &AppWindow, controller: &AppController) {
    let _scope = measurement_scope("gui_image_rows");
    app.set_editor_enabled(controller.editor_enabled());
    let panel = controller.images_panel();
    let rows: Vec<GeneratedImageRow> = panel
        .rows
        .iter()
        .map(|row| GeneratedImageRow {
            chip: slint::Color::from_rgb_u8(row.chip[0], row.chip[1], row.chip[2]),
            name: row.name.clone().into(),
            time_text: row.trailing_text.clone().into(),
            excluded: row.excluded,
            unresolved: row.unresolved,
            selected: row.selected,
        })
        .collect();
    let updates = sync_rows(app.get_images_rows(), rows, |model| {
        app.set_images_rows(model)
    });
    record_timing(TimingCounter::RowUpdates, updates as u64);
    app.set_images_header_text(panel.header_text.into());
    app.set_images_list_viewport_y(-controller.images_scroll_y());
    let sort = match controller.frame_sort() {
        Some(crate::images::FrameSort::Name) => "name",
        Some(crate::images::FrameSort::CaptureTime) => "capture-time",
        None => "",
    };
    app.set_images_sort(sort.into());
    app.set_images_sort_menu_open(controller.images_sort_menu_open());
    app.set_images_dragging_row(controller.dragging_row().map_or(-1, |i| i as i32));
    app.set_images_drag_offset(controller.drag_offset());
    app.set_images_drag_target_row(controller.drag_target_row().map_or(-1, |i| i as i32));
    app.set_images_enabled(!controller.job_running());

    match controller.pending_frame_change() {
        Some(copy) => {
            let secondary = copy.secondary_label();
            app.set_frame_warning_open(true);
            app.set_frame_warning_title(copy.title.into());
            app.set_frame_warning_body(copy.body.into());
            app.set_frame_warning_info(copy.info.into());
            app.set_frame_warning_confirm_label(copy.confirm_label.into());
            app.set_frame_warning_secondary_label(secondary.into());
        }
        None => app.set_frame_warning_open(false),
    }
}

/// Pushes the `Checkpoints` panel's whole state -- [`push_images_panel`]'s
/// twin.
pub fn push_checkpoint_panel(app: &AppWindow, controller: &AppController) {
    app.set_checkpoint_export_enabled(controller.export_enabled());
    let panel = controller.checkpoints_panel();
    let rows: Vec<GeneratedCheckpointEntry> = panel
        .entries
        .iter()
        .map(|entry| GeneratedCheckpointEntry {
            kind: if entry.separator_label.is_some() {
                "separator".into()
            } else {
                "row".into()
            },
            label: entry.separator_label.clone().unwrap_or_default().into(),
            algorithm: entry.algorithm.clone().into(),
            time_text: entry.time_text.clone().into(),
            top: entry.top,
            selected: entry.selected,
            deletable: panel.row_count > 1,
            // No next separator sticks solid once reached; a value far
            // past any real content height so the "pushed out" comparison
            // never trips.
            next_separator_top: entry.next_separator_top.unwrap_or(1.0e7),
        })
        .collect();
    let existing = app.get_checkpoint_rows();
    if existing.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            existing.set_row_data(i, row);
        }
    } else {
        app.set_checkpoint_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    }
    app.set_checkpoint_content_height(panel.content_height);
    app.set_checkpoint_scroll_y(panel.scroll_y);
}

/// Pushes the editor toolbar's own state -- `AppController::brush_toolbar_view`
/// -- the one place `ShellEvent::EditorChanged` is performed. Numeric
/// readouts are formatted here, never in `.slint`.
pub fn push_editor_toolbar(app: &AppWindow, controller: &AppController) {
    let view = controller.brush_toolbar_view();
    app.set_brush_size_text(format!("{:.0}", view.size).into());
    app.set_brush_size_fraction(view.size_fraction);
    app.set_brush_size_ceiling(view.size_ceiling as i32);
    app.set_brush_soft_text(format!("{:.0}%", view.soft).into());
    app.set_brush_soft_fraction(view.soft_fraction);
    app.set_brush_picking(view.picking);
    app.set_can_undo(view.can_undo);
    app.set_can_redo(view.can_redo);
    app.set_can_save_checkpoint(view.can_save_checkpoint);
    push_overlay_controls(app, controller);
}

/// Pushes the bottom bar's overlay controls: state, opacity readout and
/// whether the active tab has a fuse to draw over.
fn push_overlay_controls(app: &AppWindow, controller: &AppController) {
    let overlay = controller.overlay_view();
    app.set_overlay_available(controller.overlay_available());
    app.set_overlay_on(overlay.on);
    app.set_overlay_fraction(overlay.opacity);
    app.set_overlay_text(format!("{:.0}%", overlay.opacity * 100.0).into());
}

/// Pushes the brush cursor overlay's own view -- every repaint, since its
/// extent also depends on the active tab's current zoom, which
/// [`push_editor_toolbar`]'s own `EditorChanged` trigger does not cover.
fn push_brush_cursor(app: &AppWindow, controller: &AppController) {
    let Some(view) = controller.brush_cursor_view() else {
        app.set_cursor_extent(0.0);
        return;
    };
    app.set_cursor_extent(view.extent);
    app.set_cursor_ring_commands(view.ring_commands.into());
    app.set_cursor_core_commands(view.core_commands.into());
    app.set_cursor_picking(view.picking);
    app.set_cursor_pick_color(slint::Color::from_rgb_u8(
        view.pick_color[0],
        view.pick_color[1],
        view.pick_color[2],
    ));
    app.set_cursor_mirror_dx(view.mirror_dx);
}

/// Opens the Keep/Discard dialog while the active tab holds a parked run,
/// naming its unaligned frames; closes it otherwise.
fn push_divergence(app: &AppWindow, controller: &AppController) {
    match controller.pending_divergence() {
        Some(names) => {
            app.set_divergence_body(job::divergence_body(names).into());
            app.set_divergence_open(true);
        }
        None => app.set_divergence_open(false),
    }
}

/// A converted card picture per thumbnail, keyed by the thumbnail's `Arc`:
/// a thumbnail changes only by being replaced, and the held `Weak` keeps its
/// address from being reused while it is cached.
type PictureCache = Vec<(std::sync::Weak<Thumbnail>, slint::Image)>;

thread_local! {
    /// The pictures of the cards the open overlay last showed.
    static PICTURES: RefCell<PictureCache> = const { RefCell::new(Vec::new()) };
}

/// A card picture as a Slint image -- the one place
/// `crate::overlay::Thumbnail` crosses into `slint::Image`: the `cached`
/// handle when this same thumbnail was converted before, a fresh conversion
/// otherwise. A blank 1x1 image with no picture yet -- `FFProjectCard` only
/// draws it when `has-picture` is set, so its content never shows.
fn thumbnail_image(thumbnail: Option<&Arc<Thumbnail>>, cached: &PictureCache) -> slint::Image {
    let Some(thumbnail) = thumbnail else {
        return slint::Image::default();
    };
    if let Some((_, image)) = cached
        .iter()
        .find(|(weak, _)| std::ptr::eq(weak.as_ptr(), Arc::as_ptr(thumbnail)))
    {
        return image.clone();
    }
    let _scope = measurement_scope("gui_thumbnail");
    let mut buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(thumbnail.width, thumbnail.height);
    buffer.make_mut_bytes().copy_from_slice(&thumbnail.rgba);
    record_timing(TimingCounter::ThumbnailBytes, thumbnail.rgba.len() as u64);
    slint::Image::from_rgba8(buffer)
}

/// Pushes the projects overlay's whole state -- the one place
/// `ShellEvent::OverlayChanged` is performed. A closed overlay publishes
/// only that it is closed; opening it pushes again. Cards and Recent rows
/// go through `sync_rows`, and each picture keeps its handle until its
/// thumbnail is replaced or its card leaves.
pub fn push_projects_overlay(app: &AppWindow, controller: &AppController) {
    let _scope = measurement_scope("gui_projects_overlay");
    let open = controller.projects_overlay_open();
    app.set_projects_overlay_open(open);
    if !open {
        return;
    }
    app.set_projects_save_enabled(controller.save_enabled());
    app.set_gallery_heading(controller.gallery_heading().into());
    app.set_gallery_count_text(controller.gallery_count_text().into());
    let cached = PICTURES.take();
    let mut pictures = PictureCache::new();
    let cards: Vec<GeneratedProjectCard> = controller
        .project_cards()
        .into_iter()
        .map(|c| {
            let picture = thumbnail_image(c.picture.as_ref(), &cached);
            if let Some(thumbnail) = &c.picture {
                pictures.push((Arc::downgrade(thumbnail), picture.clone()));
            }
            GeneratedProjectCard {
                kind: c.kind.into(),
                opened: c.opened,
                current: c.current,
                unsaved: c.unsaved,
                fusing: c.fusing,
                fusing_fraction: c.fusing_fraction,
                project_name: c.project_name.into(),
                meta_text: c.meta_text.into(),
                headline: c.headline.into(),
                stage: c.stage.into(),
                reason: c.reason.into(),
                action_label: c.action_label.into(),
                has_picture: c.picture.is_some(),
                picture,
                x: c.x,
                y: c.y,
            }
        })
        .collect();
    PICTURES.set(pictures);
    let updates = sync_rows(app.get_project_cards(), cards, |model| {
        app.set_project_cards(model)
    });
    record_timing(TimingCounter::RowUpdates, updates as u64);
    app.set_gallery_content_height(controller.gallery_content_height_px());
    app.set_gallery_scroll_y(controller.gallery_scroll_y());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let recent: Vec<GeneratedRecentRow> = controller
        .recent_rows(now)
        .into_iter()
        .map(|r| GeneratedRecentRow {
            name: r.name.into(),
            folder: r.folder.into(),
            date_text: r.date_text.into(),
        })
        .collect();
    let updates = sync_rows(app.get_recent_rows(), recent, |model| {
        app.set_recent_rows(model)
    });
    record_timing(TimingCounter::RowUpdates, updates as u64);
}

/// Pushes a raised [`GroupingQuestion`] into `app`'s dialog properties.
fn push_question(app: &AppWindow, question: &GroupingQuestion) {
    app.set_question_open(true);
    app.set_question_frame_count(question.frame_count as i32);
    app.set_question_project_count(question.project_count as i32);
}

/// Winit-level drop observer, installed via
/// `BackendSelector::with_winit_custom_application_handler` -- the only
/// route to a file-manager drop. Buffers every `WindowEvent::DroppedFile`
/// and flushes the buffer into one
/// `open_paths` call in `about_to_wait`: one gesture is one open, whether it
/// delivered one folder path or three file paths.
pub struct DropHandler {
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>>,
    pending: Vec<PathBuf>,
    load_timer: Rc<RefCell<Option<slint::Timer>>>,
    /// Shared with `install`'s own failure toast -- one slot, so a blocked
    /// drop's toast and a blocked add's toast never close each other's
    /// auto-dismiss timer early.
    toast_timer: Rc<RefCell<Option<slint::Timer>>>,
}

impl DropHandler {
    /// `controller` and `canvas` are cloned into this handler before any
    /// window exists (`src/main.rs`), and `app_handle` is filled in once
    /// `AppWindow::new` runs -- the handler is installed on the
    /// `BackendSelector` chain before that, so it cannot hold an
    /// `AppWindow` of its own at construction time.
    pub fn new(
        controller: Arc<Mutex<AppController>>,
        canvas: Rc<RefCell<Canvas>>,
        app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>>,
        toast_timer: Rc<RefCell<Option<slint::Timer>>>,
    ) -> Self {
        DropHandler {
            controller,
            canvas,
            app_handle,
            pending: Vec::new(),
            load_timer: Rc::new(RefCell::new(None)),
            toast_timer,
        }
    }
}

impl CustomApplicationHandler for DropHandler {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _winit_window: Option<&winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &winit::event::WindowEvent,
    ) -> EventResult {
        if let winit::event::WindowEvent::DroppedFile(path) = event {
            record_gallery_input();
            self.pending.push(path.clone());
        }
        EventResult::Propagate
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) -> EventResult {
        if !self.pending.is_empty() {
            let paths = std::mem::take(&mut self.pending);
            let result = self.controller.lock().unwrap().drop_paths(&paths);
            match result {
                Ok(_) => {
                    // The same settle-then-repaint tail every other open
                    // path ends in, so a dropped project's own planes
                    // replace whatever the canvas held before, with no
                    // further gesture needed.
                    if let Some(app_weak) = self.app_handle.borrow().clone() {
                        start_load_timer(
                            app_weak,
                            self.controller.clone(),
                            self.canvas.clone(),
                            &self.load_timer,
                            Vec::new(),
                        );
                    }
                }
                Err(refused) => {
                    if let Some(app) = self.app_handle.borrow().clone().and_then(|w| w.upgrade()) {
                        open_job_toast(&app, refused.to_string(), &self.toast_timer);
                    }
                }
            }
        }
        EventResult::Propagate
    }
}

/// Opens `path`, if the picker returned one, and repaints once the load
/// settles -- the rail's `New project`/`Open project…` actions and their
/// `Ctrl+N`/`Ctrl+O` chrome-key twins share this one path. The projects
/// overlay closes itself once the load settles onto a real tab
/// (`AppController::accept`), the same as a successful open.
fn open_from_picker(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    load_timer: &Rc<RefCell<Option<slint::Timer>>>,
    path: Option<PathBuf>,
) {
    let Some(path) = path else { return };
    controller.lock().unwrap().open_paths(&[path]);
    start_load_timer(
        app_weak.clone(),
        controller.clone(),
        canvas.clone(),
        load_timer,
        Vec::new(),
    );
}

/// Opens the `Recent` row at `index` through [`AppController::open_recent`]
/// and repaints once it settles -- covers a fresh load, an in-place tab
/// switch and a no-op alike, the same polling tail [`open_from_picker`]
/// uses.
fn open_recent(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    load_timer: &Rc<RefCell<Option<slint::Timer>>>,
    index: usize,
) {
    controller.lock().unwrap().open_recent(index);
    start_load_timer(
        app_weak.clone(),
        controller.clone(),
        canvas.clone(),
        load_timer,
        Vec::new(),
    );
}

/// Runs one of the controller's saves -- `Save project`, its `Ctrl+S` twin
/// and the close question's `Save and close`. A failed save closes the
/// projects overlay before opening the failure toast.
fn save_project(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    toast_timer: &Rc<RefCell<Option<slint::Timer>>>,
    save: impl FnOnce(&mut AppController) -> bool,
) {
    let failure = {
        let mut controller = controller.lock().unwrap();
        if save(&mut controller) {
            None
        } else {
            controller.take_settled_failure()
        }
    };
    let Some(app) = app_weak.upgrade() else {
        return;
    };
    if let Some(notification) = failure {
        controller.lock().unwrap().close_projects_overlay();
        open_job_toast(
            &app,
            notification.operation.summary().to_string(),
            toast_timer,
        );
    }
    push_projects(&app, controller);
    direct_publication_scope(InputSurface::Gallery);
    repaint(&app, controller, canvas);
}

/// Wires every callback in `app` to `controller`, and returns [`Installed`]
/// so `main.rs` can read the same state the window shows. `controller` and
/// `canvas` are both parameters, not created here: `main.rs` builds both
/// first and clones them into the winit-level [`DropHandler`], which must
/// be installed on the `BackendSelector` chain before any window exists.
/// The sink wraps `slint::invoke_from_event_loop` so a worker thread's own
/// push and a callback's own event take the same path.
pub fn install(
    app: &AppWindow,
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    toast_timer: Rc<RefCell<Option<slint::Timer>>>,
) -> Installed {
    let job_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
    let load_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));

    {
        let app_weak = app.as_weak();
        let controller_for_sink = controller.clone();
        controller.lock().unwrap().set_sink(Arc::new(move |event| {
            let app_weak = app_weak.clone();
            let controller = controller_for_sink.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                perform(&app, &controller, event);
            });
        }));
    }

    {
        let app_weak = app.as_weak();
        let controller = controller.clone();
        let canvas_for_handler = canvas.clone();
        PREPARED_HANDLER.with(|slot| {
            *slot.borrow_mut() = Some(Rc::new(move || {
                if let Some(app) = app_weak.upgrade() {
                    publish_completed(&app, &controller, &canvas_for_handler);
                }
            }));
        });
        // Errors are ignored like the sink's: no event loop, nobody to wake.
        canvas.borrow_mut().preparer = Some(Preparer::new(|| {
            let _ = slint::invoke_from_event_loop(|| {
                if let Some(handler) = PREPARED_HANDLER.with(|slot| slot.borrow().clone()) {
                    handler();
                }
            });
        }));
    }

    app.on_window_drag_requested({
        let controller = controller.clone();
        move || {
            record_direct_input();
            controller.lock().unwrap().tab_strip_pressed()
        }
    });
    app.on_window_minimize_requested({
        let controller = controller.clone();
        move || {
            record_direct_input();
            controller.lock().unwrap().window_minimize()
        }
    });
    app.on_window_maximize_toggled({
        let controller = controller.clone();
        move || {
            record_surface_input(InputSurface::Other);
            controller.lock().unwrap().window_maximize_toggle()
        }
    });
    app.on_window_close_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch_gallery(&controller, &canvas, &app_weak, |controller| {
                controller.window_close();
            });
        }
    });
    // A native `WindowEvent::CloseRequested` (Alt+F4, taskbar close) reaches
    // Slint's own hook, not the app-drawn x above; unregistered, it hides
    // the window with no guard at all.
    app.window().on_close_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            record_native_surface_input(InputSurface::Gallery);
            let keep_shown = {
                let mut controller = controller.lock().unwrap();
                controller.window_close();
                controller.pending_close().is_some()
                    || controller.pending_quit_copy().is_some()
                    || controller.pending_restore_copy().is_some()
                    || controller.pending_fuse_copy().is_some()
                    || controller.pending_checkpoint_delete_copy().is_some()
            };
            if let Some(app) = app_weak.upgrade() {
                repaint(&app, &controller, &canvas);
            }
            if keep_shown {
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        }
    });
    app.on_mode_selected({
        let controller = controller.clone();
        move |mode| {
            record_surface_input(InputSurface::Other);
            let mode = if mode.as_str() == "editor" {
                ShellMode::Editor
            } else {
                ShellMode::Stacking
            };
            controller.lock().unwrap().set_mode(mode);
        }
    });
    app.on_dock_tab_selected({
        let controller = controller.clone();
        move |tab| {
            record_surface_input(InputSurface::Other);
            let tab = if tab.as_str() == "settings" {
                DockTab::Settings
            } else {
                DockTab::Images
            };
            controller.lock().unwrap().select_dock_tab(tab);
        }
    });
    app.on_checkpoint_export_requested({
        let controller = controller.clone();
        move || {
            record_direct_input();
            controller.lock().unwrap().checkpoint_export_requested()
        }
    });
    app.on_export_confirmed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            // The refused state's confirm button reads "Locate frames…" and
            // needs a folder, not a save destination -- the same
            // locate-by-hash flow the missing-sources notice already runs,
            // re-attempting the export once it settles
            // (`AppController::settle_locate`).
            if controller.lock().unwrap().export_stage() == "refused" {
                if let Some(folder) = crate::picker::pick_entry() {
                    controller.lock().unwrap().export_locate_frames(folder);
                }
                return;
            }
            let started = {
                let mut controller = controller.lock().unwrap();
                controller.export_confirmed(crate::picker::pick_save_image);
                controller.take_export_started()
            };
            if let Some(app) = app_weak.upgrade() {
                push_export_dialog(&app, &controller.lock().unwrap());
            }
            if started {
                poll_export(app_weak.clone(), controller.clone());
            }
        }
    });
    app.on_export_cancelled({
        let controller = controller.clone();
        move || controller.lock().unwrap().cancel_export()
    });
    // The editor toolbar: each callback is a single forward into the
    // controller method already driving the matching input-map row.
    app.on_brush_size_fraction_changed({
        let controller = controller.clone();
        move |fraction| controller.lock().unwrap().set_brush_size_fraction(fraction)
    });
    app.on_brush_softness_fraction_changed({
        let controller = controller.clone();
        move |fraction| {
            controller
                .lock()
                .unwrap()
                .set_brush_softness_fraction(fraction)
        }
    });
    // The bottom bar's overlay controls: both change what the `Composite`
    // pane's next repaint blends in, so both go through `dispatch`.
    app.on_overlay_toggled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |on| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.set_overlay_on(on);
            });
        }
    });
    app.on_overlay_fraction_changed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |fraction| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.set_overlay_opacity_fraction(fraction);
            });
        }
    });
    app.on_pipette_toggled({
        let controller = controller.clone();
        move || controller.lock().unwrap().toggle_pipette()
    });
    app.on_undo_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::undo)
    });
    app.on_save_checkpoint_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.save_checkpoint();
            });
        }
    });
    app.on_redo_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::redo)
    });
    // The run's own transport. `fuse-requested`/`retry-requested` start the
    // poll timer that feeds the rest; `pause-or-resume-requested` and
    // `cancel-requested` need nothing extra -- the timer already running for
    // a live job keeps polling through a pause or a cancel.
    app.on_fuse_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        let app_weak = app.as_weak();
        move || {
            record_surface_input(InputSurface::Other);
            let started = controller.lock().unwrap().start_fuse();
            if started {
                start_job_timer(
                    app_weak.clone(),
                    controller.clone(),
                    canvas.clone(),
                    job_timer.clone(),
                    toast_timer.clone(),
                );
            }
        }
    });
    app.on_pause_or_resume_requested({
        let controller = controller.clone();
        move || {
            record_surface_input(InputSurface::Other);
            controller.lock().unwrap().pause_or_resume()
        }
    });
    app.on_cancel_requested({
        let controller = controller.clone();
        move || {
            record_surface_input(InputSurface::Other);
            controller.lock().unwrap().cancel_fuse()
        }
    });
    app.on_retry_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        let app_weak = app.as_weak();
        move || {
            record_surface_input(InputSurface::Other);
            let started = controller.lock().unwrap().retry_fuse();
            if started {
                start_job_timer(
                    app_weak.clone(),
                    controller.clone(),
                    canvas.clone(),
                    job_timer.clone(),
                    toast_timer.clone(),
                );
            }
        }
    });
    app.on_composite_undock_requested({
        let controller = controller.clone();
        move || {
            record_direct_input();
            controller
                .lock()
                .unwrap()
                .undock_requested(crate::canvas::PaneSource::Composite)
        }
    });
    app.on_zmap_undock_requested({
        let controller = controller.clone();
        move || {
            record_direct_input();
            controller
                .lock()
                .unwrap()
                .undock_requested(crate::canvas::PaneSource::ZMap)
        }
    });
    let pointer_repaint = Rc::new(RepaintGate::default());
    app.on_canvas_scrolled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let gate = pointer_repaint.clone();
        move |delta_x, delta_y, x, y, ctrl, shift, alt| {
            dispatch_coalesced(&controller, &canvas, &app_weak, &gate, |controller| {
                let mods = modifiers(controller, ctrl, shift, alt);
                controller.wheel(delta_x, delta_y, x, y, mods);
            });
        }
    });
    app.on_canvas_pointer({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let gate = pointer_repaint.clone();
        move |btn, kind, x, y, ctrl, shift, alt| {
            dispatch_coalesced(&controller, &canvas, &app_weak, &gate, |controller| {
                let mods = modifiers(controller, ctrl, shift, alt);
                // `kind` is `ui/shell/canvas.slint`'s fold of `PointerEventKind`:
                // 0 = Down, 1 = Up or Cancel, 2 = Move. `Move` fires on every
                // mouse movement over the area, before `moved`, and carries no
                // motion of its own -- forwarded as `PointerKind::Move` rather
                // than folded into `Up`, which would end a live drag on the
                // first movement.
                let kind = match kind {
                    0 => PointerKind::Down,
                    2 => PointerKind::Move,
                    _ => PointerKind::Up,
                };
                controller.pointer(kind, button(btn), x, y, mods);
            });
        }
    });
    app.on_canvas_dragged({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |x, y| {
            dispatch_coalesced(
                &controller,
                &canvas,
                &app_weak,
                &pointer_repaint,
                |controller| {
                    controller.pointer_moved(x, y);
                },
            );
        }
    });
    app.on_canvas_resized({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            record_layout_callback();
            if let Some(app) = app_weak.upgrade() {
                let (left, right) = read_pane_geometry(&app);
                controller.lock().unwrap().set_pane_geometry(left, right);
                repaint(&app, &controller, &canvas);
            }
        }
    });
    app.on_zoom_in_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::zoom_in)
    });
    app.on_zoom_out_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::zoom_out)
    });
    app.on_zoom_fit_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::fit)
    });
    // The five chrome keys are consulted before the rebindable table on
    // every key press, whether or not the projects overlay is open, so none
    // of them can be shadowed by a user's `settings.toml` `[input]` table.
    app.on_canvas_key_pressed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        let toast_timer = toast_timer.clone();
        move |text, ctrl, shift, alt| {
            let mods = {
                let controller = controller.lock().unwrap();
                modifiers(&controller, ctrl, shift, alt)
            };
            let chrome = controller.lock().unwrap().chrome_key(text.as_str(), mods);
            match chrome {
                ChromeKey::None => dispatch(&controller, &canvas, &app_weak, |controller| {
                    controller.key(true, text.as_str(), mods);
                }),
                ChromeKey::Consumed => record_gallery_input(),
                ChromeKey::NewProject => {
                    record_surface_input(InputSurface::Other);
                    open_from_picker(
                        &controller,
                        &canvas,
                        &app_weak,
                        &load_timer,
                        crate::picker::pick_entry(),
                    )
                }
                ChromeKey::OpenProject => {
                    record_surface_input(InputSurface::Other);
                    open_from_picker(
                        &controller,
                        &canvas,
                        &app_weak,
                        &load_timer,
                        crate::picker::pick_project(),
                    )
                }
                ChromeKey::SaveProject => {
                    record_gallery_input();
                    save_project(&controller, &canvas, &app_weak, &toast_timer, |c| {
                        c.save_active_project(crate::picker::pick_save_project)
                    })
                }
            }
        }
    });
    app.on_canvas_key_released({
        let controller = controller.clone();
        move |text, ctrl, shift, alt| {
            record_direct_input();
            let mut controller = controller.lock().unwrap();
            let mods = modifiers(&controller, ctrl, shift, alt);
            controller.key(false, text.as_str(), mods);
        }
    });
    app.on_canvas_focus_changed({
        let controller = controller.clone();
        move || {
            record_direct_input();
            controller.lock().unwrap().clear_input_modifiers()
        }
    });
    app.on_settings_open_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            record_settings_input();
            let open = controller.lock().unwrap().open_settings();
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_open(open);
                direct_publication_scope(InputSurface::Settings);
                repaint(&app, &controller, &canvas);
            }
        }
    });
    app.on_settings_close_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            record_settings_input();
            let open = controller.lock().unwrap().close_settings();
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_open(open);
                direct_publication_scope(InputSurface::Settings);
                repaint(&app, &controller, &canvas);
            }
        }
    });
    app.on_settings_section_selected({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |section| {
            record_settings_input();
            let parsed = match section.as_str() {
                "general" => SettingsSection::General,
                "performance" => SettingsSection::Performance,
                "storage" => SettingsSection::Storage,
                "diagnostics" => SettingsSection::Diagnostics,
                "about" => SettingsSection::About,
                _ => SettingsSection::InputMap,
            };
            controller.lock().unwrap().select_settings_section(parsed);
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_section(section);
                direct_publication_scope(InputSurface::Settings);
            }
        }
    });
    app.on_diagnostics_minidumps_toggled({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |include| {
            record_surface_input(InputSurface::Other);
            // Detached: the rescan's own outcome reaches the window through
            // the sink, the same way `on_diagnostics_primary_clicked`'s
            // write does below.
            let _ = controller.lock().unwrap().set_include_minidumps(include);
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
                direct_publication_scope(InputSurface::Other);
            }
        }
    });
    app.on_diagnostics_destination_edited({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |text| {
            record_surface_input(InputSurface::Other);
            controller.lock().unwrap().set_destination(text.as_str());
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
                direct_publication_scope(InputSurface::Other);
            }
        }
    });
    app.on_diagnostics_destination_cursor_moved({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |cursor_x, viewport_width| {
            record_surface_input(InputSurface::Other);
            controller
                .lock()
                .unwrap()
                .diagnostics_destination_cursor_moved(cursor_x, viewport_width);
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
                direct_publication_scope(InputSurface::Other);
            }
        }
    });
    app.on_diagnostics_manifest_scrollbar_pointer({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |kind, y| {
            // `kind` is `ff_manifest_list.slint`'s fold of `PointerEventKind`,
            // the same convention `canvas-pointer` uses: 0 = Down, 1 = Up or
            // Cancel, 2 = Move.
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            record_surface_input(InputSurface::Other);
            dispatch_manifest_scroll(&controller, &app_weak, |c| {
                c.manifest_scrollbar_pointer(kind, y)
            });
        }
    });
    app.on_diagnostics_manifest_scrollbar_dragged({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |y| {
            record_surface_input(InputSurface::Other);
            dispatch_manifest_scroll(&controller, &app_weak, |c| c.manifest_scrollbar_dragged(y));
        }
    });
    app.on_diagnostics_manifest_viewport_height_changed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |height| {
            record_layout_callback();
            dispatch_manifest_scroll(&controller, &app_weak, |c| {
                c.set_manifest_viewport_height(height)
            });
        }
    });
    app.on_diagnostics_manifest_scrolled({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |delta| {
            record_surface_input(InputSurface::Other);
            dispatch_manifest_scroll(&controller, &app_weak, |c| c.manifest_scrolled(delta));
        }
    });
    app.on_diagnostics_primary_clicked({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            record_surface_input(InputSurface::Other);
            // Detached: the write's own outcome reaches the window through
            // the sink, not through this handle. Every click starts a write
            // -- a finished bundle re-derives a fresh timestamped
            // destination first, so exporting again needs no extra click.
            let _ = controller.lock().unwrap().start_bundle_write();
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
                direct_publication_scope(InputSurface::Other);
            }
        }
    });

    app.on_crash_notice_dismissed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            record_surface_input(InputSurface::Other);
            controller.lock().unwrap().dismiss_crash_notice();
            if let Some(app) = app_weak.upgrade() {
                push_crash_notice(&app, &controller);
                direct_publication_scope(InputSurface::Other);
            }
        }
    });
    app.on_crash_notice_review_requested({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            record_surface_input(InputSurface::Other);
            let open = controller.lock().unwrap().review_crashes();
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_open(open);
                app.set_settings_section("diagnostics".into());
                push_crash_notice(&app, &controller);
                push_diagnostics_view(&app, &controller);
                direct_publication_scope(InputSurface::Other);
            }
        }
    });

    // The entry path: `open_paths`'s own sink pushes every property a load
    // raises (`ShellEvent::LoadReady` and friends); the load timer's repaint
    // feeds the canvas for whichever tab the open landed on.
    app.on_open_folder_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        move || {
            record_surface_input(InputSurface::Other);
            open_from_picker(
                &controller,
                &canvas,
                &app_weak,
                &load_timer,
                crate::picker::pick_entry(),
            )
        }
    });
    app.on_plus_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::select_plus)
    });
    app.on_tab_selected({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.select_project(i as usize);
            })
        }
    });
    app.on_tab_closed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.request_close(i as usize);
            })
        }
    });
    app.on_tab_scroll_left({
        let controller = controller.clone();
        move || {
            record_gallery_input();
            controller.lock().unwrap().scroll_tabs(-1)
        }
    });
    app.on_tab_scroll_right({
        let controller = controller.clone();
        move || {
            record_gallery_input();
            controller.lock().unwrap().scroll_tabs(1)
        }
    });
    app.on_tab_region_width_changed({
        let controller = controller.clone();
        move |width| {
            record_layout_callback();
            controller.lock().unwrap().set_tab_region_width(width)
        }
    });
    // A `changed` handler never fires for the initial value, so the
    // viewport's first measurement is read here once.
    controller
        .lock()
        .unwrap()
        .set_tab_region_width(app.get_tab_region_width());
    app.on_question_open_as_one({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        move || {
            record_gallery_input();
            let _ = controller
                .lock()
                .unwrap()
                .answer_question(OpenAs::OneProject);
            // The answer's decode runs on a worker, so nothing else pushes
            // `question_open` before it settles -- closed here instead.
            // When a load is already in flight, the question stays pending
            // and the next `push_projects` reopens the dialog.
            if let Some(app) = app_weak.upgrade() {
                app.set_question_open(false);
                direct_publication_scope(InputSurface::Gallery);
                repaint(&app, &controller, &canvas);
            }
            start_load_timer(
                app_weak.clone(),
                controller.clone(),
                canvas.clone(),
                &load_timer,
                Vec::new(),
            );
        }
    });
    app.on_question_open_as_many({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        move || {
            record_gallery_input();
            let _ = controller
                .lock()
                .unwrap()
                .answer_question(OpenAs::NProjects);
            if let Some(app) = app_weak.upgrade() {
                app.set_question_open(false);
                direct_publication_scope(InputSurface::Gallery);
                repaint(&app, &controller, &canvas);
            }
            start_load_timer(
                app_weak.clone(),
                controller.clone(),
                canvas.clone(),
                &load_timer,
                Vec::new(),
            );
        }
    });
    app.on_question_cancelled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            record_gallery_input();
            controller.lock().unwrap().cancel_question();
            if let Some(app) = app_weak.upgrade() {
                app.set_question_open(false);
                direct_publication_scope(InputSurface::Gallery);
                repaint(&app, &controller, &canvas);
            }
        }
    });
    app.on_refusal_pick_another_folder({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        move || {
            record_gallery_input();
            // Only the folder-empty refusal offers a folder picker; every
            // other refusal's button just closes, read before
            // `dismiss_refusal` would clear it.
            let is_folder_empty = controller.lock().unwrap().refusal_title().is_none();
            controller.lock().unwrap().dismiss_refusal();
            if let Some(app) = app_weak.upgrade() {
                app.set_refusal_open(false);
                direct_publication_scope(InputSurface::Gallery);
                repaint(&app, &controller, &canvas);
            }
            if is_folder_empty {
                open_from_picker(
                    &controller,
                    &canvas,
                    &app_weak,
                    &load_timer,
                    crate::picker::pick_entry(),
                )
            }
        }
    });
    app.on_mismatch_open_anyway({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().open_anyway();
            if let Some(app) = app_weak.upgrade() {
                push_projects(&app, &controller);
                push_project_dialogs(&app, &controller.lock().unwrap());
            }
        }
    });
    app.on_mismatch_locate_frames({
        let controller = controller.clone();
        move || {
            if let Some(folder) = crate::picker::pick_entry() {
                controller
                    .lock()
                    .unwrap()
                    .locate_frames(LocateTarget::Mismatch, folder);
            }
        }
    });
    app.on_mismatch_cancelled({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().cancel_mismatch();
            if let Some(app) = app_weak.upgrade() {
                push_project_dialogs(&app, &controller.lock().unwrap());
            }
        }
    });
    app.on_sources_missing_locate_frames({
        let controller = controller.clone();
        move || {
            if let Some(folder) = crate::picker::pick_entry() {
                controller.lock().unwrap().locate_sources_notice(folder);
            }
        }
    });
    app.on_sources_missing_closed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().close_sources_notice();
            if let Some(app) = app_weak.upgrade() {
                push_project_dialogs(&app, &controller.lock().unwrap());
            }
        }
    });
    app.on_skipped_notice_dismissed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            record_gallery_input();
            controller.lock().unwrap().dismiss_notice();
            if let Some(app) = app_weak.upgrade() {
                app.set_skipped_notice("".into());
                direct_publication_scope(InputSurface::Gallery);
            }
        }
    });
    app.on_job_toast_dismissed({
        let app_weak = app.as_weak();
        move || {
            record_surface_input(InputSurface::Other);
            if let Some(app) = app_weak.upgrade() {
                app.set_job_toast_open(false);
                direct_publication_scope(InputSurface::Other);
            }
        }
    });

    // The settings panel: every callback is a one-line forward,
    // `AppController` alone decides what a row's own kind, section and id
    // mean.
    app.on_settings_param_changed({
        let controller = controller.clone();
        move |section, id, fraction| {
            record_settings_input();
            controller
                .lock()
                .unwrap()
                .set_param_fraction(section.as_str(), id.as_str(), fraction);
        }
    });
    app.on_settings_param_text_edited({
        let controller = controller.clone();
        move |section, id, text| {
            record_settings_input();
            controller
                .lock()
                .unwrap()
                .set_param_text(section.as_str(), id.as_str(), text.as_str());
        }
    });
    app.on_settings_algorithm_selected({
        let controller = controller.clone();
        move |section, id| {
            record_settings_input();
            let mut controller = controller.lock().unwrap();
            if section.as_str() == "align" {
                controller.set_align_algorithm(id.as_str());
            } else {
                controller.set_fuse_algorithm(id.as_str());
            }
        }
    });
    app.on_settings_reset_all({
        let controller = controller.clone();
        move || {
            record_settings_input();
            controller.lock().unwrap().reset_all_sections()
        }
    });
    app.on_settings_menu_toggled({
        let controller = controller.clone();
        move |section, id| {
            record_settings_input();
            controller
                .lock()
                .unwrap()
                .toggle_param_menu_for(section.as_str(), id.as_str());
        }
    });
    app.on_settings_scrolled({
        let controller = controller.clone();
        move |delta| {
            record_settings_input();
            controller.lock().unwrap().scroll_settings_panel(delta)
        }
    });
    app.on_settings_scrollbar_pointer({
        let controller = controller.clone();
        move |kind, y| {
            record_settings_input();
            // `ui/components/ff_scrollbar.slint`'s own fold of
            // `PointerEventKind`, the same convention as `canvas.slint`'s:
            // 0 = Down, 1 = Up or Cancel, 2 = Move.
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller.lock().unwrap().scrollbar_pointer(kind, y);
        }
    });
    app.on_settings_scrollbar_dragged({
        let controller = controller.clone();
        move |y| {
            record_settings_input();
            controller.lock().unwrap().scrollbar_dragged(y)
        }
    });
    app.on_settings_viewport_height_changed({
        let controller = controller.clone();
        move |height| {
            record_layout_callback();
            controller
                .lock()
                .unwrap()
                .set_settings_viewport_height(height);
        }
    });

    // The `Images` panel's own scroll -- `FFScrollbar`'s twin wiring above.
    // The list scrolls itself; the controller mirrors the offset. A move
    // the scrollbar path already caused is not counted as a second input.
    app.on_images_viewport_scrolled({
        let controller = controller.clone();
        move |offset| {
            if controller.lock().unwrap().images_viewport_scrolled(offset) {
                record_images_input();
                direct_publication_scope(InputSurface::Images);
            }
        }
    });
    // A plain wheel over the `Images` list steps the selected/brush-source
    // frame -- the same effect `Ctrl + Wheel` over the canvas has, so this
    // also needs a repaint.
    app.on_images_frame_wheel({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |delta| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.images_list_wheel(delta);
            });
        }
    });
    app.on_images_scrollbar_pointer({
        let controller = controller.clone();
        move |kind, y| {
            record_images_input();
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller.lock().unwrap().images_scrollbar_pointer(kind, y);
        }
    });
    app.on_images_scrollbar_dragged({
        let controller = controller.clone();
        move |y| {
            record_images_input();
            controller.lock().unwrap().images_scrollbar_dragged(y)
        }
    });
    app.on_images_viewport_height_changed({
        let controller = controller.clone();
        move |height| {
            record_layout_callback();
            controller
                .lock()
                .unwrap()
                .set_images_viewport_height(height)
        }
    });
    // The `Checkpoints` panel's own scroll and selection -- `FFScrollbar`'s
    // twin wiring above.
    app.on_checkpoint_scrolled({
        let controller = controller.clone();
        move |delta| controller.lock().unwrap().scroll_checkpoints_panel(delta)
    });
    app.on_checkpoint_scrollbar_pointer({
        let controller = controller.clone();
        move |kind, y| {
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller
                .lock()
                .unwrap()
                .checkpoints_scrollbar_pointer(kind, y);
        }
    });
    app.on_checkpoint_scrollbar_dragged({
        let controller = controller.clone();
        move |y| controller.lock().unwrap().checkpoints_scrollbar_dragged(y)
    });
    app.on_checkpoint_viewport_height_changed({
        let controller = controller.clone();
        move |height| {
            controller
                .lock()
                .unwrap()
                .set_checkpoints_viewport_height(height)
        }
    });
    controller
        .lock()
        .unwrap()
        .set_checkpoints_viewport_height(app.get_checkpoint_viewport_height());
    app.on_checkpoint_selected({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |c| {
                c.select_checkpoint_row(i as usize)
            })
        }
    });

    app.on_checkpoint_delete_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |c| {
                c.request_checkpoint_delete(i as usize)
            })
        }
    });
    app.on_checkpoint_delete_cancelled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch_gallery(&controller, &canvas, &app_weak, |c| {
                c.cancel_checkpoint_delete()
            })
        }
    });
    app.on_checkpoint_delete_confirmed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch_gallery(&controller, &canvas, &app_weak, |c| {
                c.confirm_checkpoint_delete()
            })
        }
    });

    app.on_images_add_frames_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let toast_timer = toast_timer.clone();
        move || {
            record_images_input();
            if let Some(paths) = crate::picker::pick_frames() {
                let mut refused = None;
                dispatch(&controller, &canvas, &app_weak, |controller| {
                    refused = controller.add_frames(&paths).err();
                });
                if let Some(refused) = refused
                    && let Some(app) = app_weak.upgrade()
                {
                    open_job_toast(&app, refused.to_string(), &toast_timer);
                }
            }
        }
    });
    app.on_images_sort_menu_toggled({
        let controller = controller.clone();
        move || {
            record_images_input();
            controller.lock().unwrap().toggle_images_sort_menu()
        }
    });
    app.on_images_sort_selected({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |id| {
            record_images_input();
            let sort = match id.as_str() {
                "capture-time" => crate::images::FrameSort::CaptureTime,
                _ => crate::images::FrameSort::Name,
            };
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.set_frame_sort(sort);
            });
        }
    });
    app.on_images_frame_excluded_toggled({
        let controller = controller.clone();
        move |i| {
            record_images_input();
            controller.lock().unwrap().toggle_frame_excluded(i as usize)
        }
    });
    app.on_images_frame_selected({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.select_frame(i as usize);
            });
        }
    });
    app.on_images_frame_remove_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.remove_frame(i as usize);
            });
        }
    });
    app.on_frame_warning_cancelled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch_shared(
                InputSurface::Images,
                &controller,
                &canvas,
                &app_weak,
                |controller| {
                    controller.cancel_frame_change();
                },
            );
        }
    });
    app.on_frame_warning_confirmed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        move || {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.confirm_frame_change();
            });
            poll_if_work_pending(&app_weak, &controller, &canvas, &job_timer, &toast_timer);
        }
    });
    app.on_frame_warning_secondary_pressed({
        let controller = controller.clone();
        move || {
            let mut controller = controller.lock().unwrap();
            let copy = controller.pending_frame_change();
            if copy.as_ref().is_some_and(|c| c.keep_existing_button) {
                record_images_input();
                controller.keep_existing();
            } else if copy.is_some_and(|c| c.exclude_instead_button) {
                record_images_input();
                controller.exclude_instead();
            } else {
                record_gallery_input();
                controller.new_project_instead();
            }
        }
    });
    app.on_divergence_discarded({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().discard_fused();
            if let Some(app) = app_weak.upgrade() {
                push_projects(&app, &controller);
                repaint(&app, &controller, &canvas);
            }
        }
    });
    app.on_divergence_kept({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().keep_fused();
            if let Some(app) = app_weak.upgrade() {
                push_projects(&app, &controller);
                repaint(&app, &controller, &canvas);
            }
        }
    });
    app.on_close_guard_cancelled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch_gallery(&controller, &canvas, &app_weak, |controller| {
                controller.close_guard_cancelled();
            });
        }
    });
    app.on_close_guard_secondary_pressed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        move || {
            record_gallery_input();
            save_project(&controller, &canvas, &app_weak, &toast_timer, |c| {
                c.close_guard_secondary_pressed(crate::picker::pick_save_project)
            });
            poll_if_work_pending(&app_weak, &controller, &canvas, &job_timer, &toast_timer);
        }
    });
    app.on_close_guard_confirmed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        move || {
            dispatch_gallery(
                &controller,
                &canvas,
                &app_weak,
                AppController::close_guard_confirmed,
            );
            poll_if_work_pending(&app_weak, &controller, &canvas, &job_timer, &toast_timer);
        }
    });
    // The projects overlay: the tab strip's own toggle, the gallery's
    // scroll -- `FFScrollbar`'s twin wiring above -- and its cards' own
    // click-through.
    app.on_projects_overlay_toggled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch_gallery(&controller, &canvas, &app_weak, |controller| {
                controller.toggle_projects_overlay();
            });
        }
    });
    // The rail's own actions: New/Open reuse the picker any other open path
    // uses; Close closes whichever tab is active, the same as `Ctrl+W` would.
    app.on_projects_new_project_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        move || {
            record_gallery_input();
            open_from_picker(
                &controller,
                &canvas,
                &app_weak,
                &load_timer,
                crate::picker::pick_entry(),
            )
        }
    });
    app.on_projects_open_project_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        move || {
            record_gallery_input();
            open_from_picker(
                &controller,
                &canvas,
                &app_weak,
                &load_timer,
                crate::picker::pick_project(),
            )
        }
    });
    app.on_projects_save_project_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let toast_timer = toast_timer.clone();
        move || {
            record_gallery_input();
            save_project(&controller, &canvas, &app_weak, &toast_timer, |c| {
                c.save_active_project(crate::picker::pick_save_project)
            })
        }
    });
    app.on_projects_close_project_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch_gallery(&controller, &canvas, &app_weak, |controller| {
                if let Some(i) = controller.active_project() {
                    controller.request_close(i);
                }
            });
        }
    });
    app.on_projects_card_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch_gallery(&controller, &canvas, &app_weak, |controller| {
                controller.select_gallery_card(i as usize);
            })
        }
    });
    app.on_projects_recent_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        let load_timer = load_timer.clone();
        move |i| {
            record_gallery_input();
            open_recent(&controller, &canvas, &app_weak, &load_timer, i as usize);
        }
    });
    app.on_projects_card_close_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch_gallery(&controller, &canvas, &app_weak, |controller| {
                controller.request_close(i as usize);
            })
        }
    });
    // `Run again` re-runs that one card's own kept request off the worker
    // thread the same way `fuse-requested`/`retry-requested` do; `Locate
    // frames…` needs the folder picker, which only this event-loop-thread
    // callback can call.
    app.on_projects_card_action_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        let app_weak = app.as_weak();
        move |i| {
            let i = i as usize;
            let kind = controller.lock().unwrap().gallery_failure_kind(i);
            match kind {
                Some("sources-missing") => {
                    record_gallery_input();
                    let tab_id = controller.lock().unwrap().tab_id_at(i);
                    if let (Some(tab_id), Some(folder)) = (tab_id, crate::picker::pick_entry()) {
                        controller
                            .lock()
                            .unwrap()
                            .locate_frames(LocateTarget::Tab(tab_id), folder);
                    }
                }
                Some("failed") | Some("cancelled") => {
                    record_surface_input(InputSurface::Other);
                    let started = controller.lock().unwrap().run_gallery_action(i);
                    if started {
                        start_job_timer(
                            app_weak.clone(),
                            controller.clone(),
                            canvas.clone(),
                            job_timer.clone(),
                            toast_timer.clone(),
                        );
                    }
                }
                _ => record_direct_input(),
            }
        }
    });
    app.on_gallery_scrolled({
        let controller = controller.clone();
        move |delta| {
            record_gallery_input();
            controller.lock().unwrap().scroll_gallery(delta)
        }
    });
    app.on_gallery_scrollbar_pointer({
        let controller = controller.clone();
        move |kind, y| {
            record_gallery_input();
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller
                .lock()
                .unwrap()
                .gallery_scrollbar_pointer(kind, y);
        }
    });
    app.on_gallery_scrollbar_dragged({
        let controller = controller.clone();
        move |y| {
            record_gallery_input();
            controller.lock().unwrap().gallery_scrollbar_dragged(y)
        }
    });
    app.on_gallery_viewport_height_changed({
        let controller = controller.clone();
        move |height| {
            record_layout_callback();
            controller
                .lock()
                .unwrap()
                .set_gallery_viewport_height(height)
        }
    });
    app.on_gallery_width_changed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |width| {
            record_layout_callback();
            controller.lock().unwrap().set_gallery_width(width);
            if let Some(app) = app_weak.upgrade() {
                push_projects_overlay(&app, &controller.lock().unwrap());
            }
        }
    });
    // A `changed` handler never fires for the initial value -- the
    // gallery's first measurement is read here once, the same reason
    // `tab_region_width` is (above).
    controller
        .lock()
        .unwrap()
        .set_gallery_width(app.get_gallery_width());

    app.on_images_frame_grip_pointer({
        let controller = controller.clone();
        move |i, kind, y| {
            record_images_input();
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller
                .lock()
                .unwrap()
                .frame_drag_pointer(i as usize, kind, y);
        }
    });

    Installed {
        controller,
        job_timer,
        toast_timer,
    }
}

/// Pushes the refusal, fingerprint-mismatch and missing-sources dialogs'
/// own state -- every place any of the three can newly show or close.
fn push_project_dialogs(app: &AppWindow, controller: &AppController) {
    match controller.mismatch_copy() {
        Some((_, body, file_name)) => {
            app.set_mismatch_context(file_name.into());
            app.set_mismatch_body(body.into());
            app.set_mismatch_open(true);
        }
        None => app.set_mismatch_open(false),
    }
    match controller.sources_notice() {
        Some((_, body, file_name)) => {
            app.set_sources_missing_context(file_name.into());
            app.set_sources_missing_body(body.into());
            app.set_sources_missing_open(true);
        }
        None => app.set_sources_missing_open(false),
    }
    let folder_empty_refusal = controller.refusal_title().is_none();
    app.set_refusal_title(
        controller
            .refusal_title()
            .unwrap_or("No frames here")
            .into(),
    );
    app.set_refusal_body(
        controller
            .last_refusal()
            .unwrap_or("FocusForge reads 16-bit TIFF (.tif, .tiff). This folder holds none.")
            .into(),
    );
    app.set_refusal_button_label(
        if folder_empty_refusal {
            "Pick another folder…"
        } else {
            "Close"
        }
        .into(),
    );
}

/// Pushes the export modal's own state -- every place it can newly open,
/// change stage or close. `pub` so a test can call it directly, the same
/// reason [`push_projects`] is.
pub fn push_export_dialog(app: &AppWindow, controller: &AppController) {
    app.set_export_open(controller.export_open());
    let (context, what) = controller.export_copy().unwrap_or(("", ""));
    app.set_export_context(context.into());
    app.set_export_what(what.into());
    app.set_export_stage(controller.export_stage().into());
    app.set_export_writing_file_name(controller.export_destination_name().into());
    let lines: Vec<slint::SharedString> = controller
        .export_refusal_lines()
        .iter()
        .map(|line| line.as_str().into())
        .collect();
    app.set_export_refusal_lines(slint::ModelRc::new(slint::VecModel::from(lines)));
}

/// Performs one [`ShellEvent`] against the window. The only function in the
/// crate that names `winit`.
fn perform(app: &AppWindow, controller: &Arc<Mutex<AppController>>, event: ShellEvent) {
    let _publication = model_publication_scope(&event);
    match event {
        ShellEvent::WindowDragRequested => {
            // Both results discarded deliberately: `with_winit_window`
            // returns `None` off the winit backend or an inactive event
            // loop, and a refused compositor move is a no-op, not an error
            // to surface. Never `unwrap` either.
            app.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
        // `minimized`/`maximized` are native-window state, reached through
        // the runtime `Window` API rather than as generated root-property
        // accessors (`i-slint-core` 1.17.1's `Window` runtime API).
        ShellEvent::WindowMinimizeRequested => app.window().set_minimized(true),
        ShellEvent::WindowMaximizeToggled(maximized) => {
            app.window().set_maximized(maximized);
            // `AppWindow::window-maximized` is a plain `in` property, not the
            // builtin (shadowable) `Window.maximized`: native window state
            // needs `app.window()` while chrome state the shell *draws
            // from* needs a custom property. This is
            // what the maximize glyph binding (`ui/shell/tab_strip.slint`)
            // reads to swap `square.svg` for `copy.svg`.
            app.set_window_maximized(maximized);
        }
        ShellEvent::WindowCloseRequested => {
            let _ = app.window().hide();
        }
        ShellEvent::ModeChanged(shell_mode) => {
            let mode = match shell_mode {
                ShellMode::Stacking => "stacking",
                ShellMode::Editor => "editor",
            };
            app.set_mode(mode.into());
            if shell_mode == ShellMode::Editor {
                push_editor_toolbar(app, &controller.lock().unwrap());
            }
            // The legend replaces the ordinary `Images` list only in editor
            // mode -- the list's own shape follows the mode switch.
            push_images_panel(app, &controller.lock().unwrap());
        }
        ShellEvent::DockTabSelected(tab) => {
            let tab = match tab {
                DockTab::Images => "images",
                DockTab::Settings => "settings",
            };
            app.set_dock_tab(tab.into());
        }
        // Every place the export modal opens, changes stage or closes.
        ShellEvent::CheckpointExportRequested => {
            push_export_dialog(app, &controller.lock().unwrap());
        }
        // Never reachable while `undock_enabled()` is always `true` with
        // both panes docked.
        ShellEvent::PaneUndockRequested(_) => {}
        // A no-op here: the viewport
        // mutation itself already happened synchronously inside
        // `AppController::emit_action` before this event was even raised,
        // and the repaint it needs runs on the
        // *callback* path (`repaint`, above), not through the sink --
        // `perform` runs off a `Send + Sync` closure that can hold neither
        // an `Rc` nor the GPU state `CanvasRenderer` needs.
        ShellEvent::Input(_) => {}
        // A write's outcome updates the controller's own state first -- the
        // thread that produced it holds no lock on `self` -- then rebuilds
        // the view the same way every other diagnostics mutation does.
        ShellEvent::DiagnosticsWriteFinished(status) => {
            controller.lock().unwrap().finish_bundle_write(status);
            push_diagnostics_view(app, controller);
        }
        // A scan's outcome updates the controller's stored manifest first,
        // the same way a write's outcome does above, then rebuilds the view
        // -- the one place `diagnostics_view` stops reporting `loading`.
        ShellEvent::DiagnosticsScanFinished { generation, scan } => {
            controller
                .lock()
                .unwrap()
                .finish_manifest_scan(generation, scan);
            push_diagnostics_view(app, controller);
        }
        // The three entry-path events. `EntryRefused`'s
        // message is already rendered by `AppController::refuse`, never
        // formatted here.
        ShellEvent::ProjectsChanged => {
            push_projects(app, controller);
            push_project_dialogs(app, &controller.lock().unwrap());
        }
        ShellEvent::TabScrollChanged => push_tab_scroll(app, &controller.lock().unwrap()),
        ShellEvent::QuestionRaised(question) => push_question(app, &question),
        ShellEvent::MismatchRaised => push_project_dialogs(app, &controller.lock().unwrap()),
        ShellEvent::EntryRefused(_) => {
            app.set_refusal_open(true);
            push_project_dialogs(app, &controller.lock().unwrap());
        }
        ShellEvent::SettingsChanged => push_settings_panel(app, &controller.lock().unwrap()),
        ShellEvent::SettingsScrolled => {
            push_settings_scroll(app, &controller.lock().unwrap().settings_panel());
        }
        ShellEvent::JobChanged => {
            let controller = controller.lock().unwrap();
            push_job(app, &controller.job_view());
            // A tab's own run mark changes at the same moments: it starts,
            // queues, is promoted, or settles.
            push_tab_rows(app, &controller);
            // A run starting or ending is also the moment the images/settings
            // panels' own whole-panel disable must flip -- neither panel
            // otherwise re-reads `job_running()` on its own.
            push_settings_panel(app, &controller);
            push_images_panel(app, &controller);
            push_projects_overlay(app, &controller);
            push_divergence(app, &controller);
        }
        ShellEvent::ImagesChanged => push_images_panel(app, &controller.lock().unwrap()),
        ShellEvent::ImagesScrolled => {
            app.set_images_list_viewport_y(-controller.lock().unwrap().images_scroll_y());
        }
        ShellEvent::CheckpointsChanged => {
            let controller = controller.lock().unwrap();
            push_checkpoint_panel(app, &controller);
            app.set_projects_save_enabled(controller.save_enabled());
        }
        ShellEvent::EditorChanged => {
            let controller = controller.lock().unwrap();
            push_editor_toolbar(app, &controller);
            app.set_projects_save_enabled(controller.save_enabled());
        }
        ShellEvent::OverlayChanged => {
            let controller = controller.lock().unwrap();
            push_projects_overlay(app, &controller);
            push_project_dialogs(app, &controller);
        }
        ShellEvent::GalleryScrolled => {
            app.set_gallery_scroll_y(controller.lock().unwrap().gallery_scroll_y());
        }
        // A background load's own three events (`AppController::open_paths`).
        // The indicator's threshold and fade are the Slint region's own
        // state, driven by `load-active` alone -- it shows no other part of
        // a load's progress, so a tick has nothing to push.
        ShellEvent::LoadStarted => app.set_load_active(true),
        ShellEvent::LoadProgress { .. } => {}
        ShellEvent::LoadReady => {
            let outcome = controller.lock().unwrap().settle_open();
            // Pushed synchronously, in the same step `load-active` clears --
            // `settle_open`'s own sink push (`ProjectsChanged` and friends)
            // only lands a turn later, which would render an empty-state
            // frame in between.
            match outcome {
                Some(OpenOutcome::Opened(_)) => push_projects(app, controller),
                Some(OpenOutcome::Question(question, _)) => {
                    push_question(app, &question);
                    push_brush_cursor(app, &controller.lock().unwrap());
                }
                // A fingerprint mismatch opens no tab: `pending_mismatch`
                // carries what the mismatch dialog needs.
                Some(OpenOutcome::Mismatch(_)) => {
                    let controller = controller.lock().unwrap();
                    push_project_dialogs(app, &controller);
                    push_brush_cursor(app, &controller);
                }
                Some(OpenOutcome::Refused(_)) => {
                    app.set_refusal_open(true);
                    let controller = controller.lock().unwrap();
                    push_project_dialogs(app, &controller);
                    push_brush_cursor(app, &controller);
                }
                None => {}
            }
            app.set_load_active(false);
        }
        // An import scan (`AppController::add_frames`) is ready to settle.
        // Never touches the canvas: an add either stages a question or
        // appends frames, neither of which the sink's own `Send + Sync`
        // path can repaint through anyway.
        ShellEvent::ImportReady => {
            let mut controller = controller.lock().unwrap();
            controller.settle_import();
            push_images_panel(app, &controller);
        }
        // A `Locate frames…` verification (`AppController::locate_frames`)
        // is ready to settle -- relocates the target's frames, or raises
        // the refusal dialog through `settle_locate`'s own
        // `refuse_message`, which raises its own `EntryRefused` event.
        ShellEvent::LocateReady => {
            let started = {
                let mut controller = controller.lock().unwrap();
                controller.settle_locate();
                push_projects_overlay(app, &controller);
                push_project_dialogs(app, &controller);
                // A refused export waiting on this same verification may
                // have just been retried (`AppController::retry_export`).
                push_export_dialog(app, &controller);
                controller.take_export_started()
            };
            if started {
                poll_export(app.as_weak(), controller.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::diagnostics::{enable_trace_accounting, timing_snapshot};

    use super::{
        AppController, DockTab, ShellEvent, finish_input_trace, model_publication_scope,
        record_gallery_input, record_images_input, record_settings_input, record_surface_input,
    };
    use crate::diagnostics::{
        InputSurface, finish_input_batches, record_coalesced_input, record_destroyed_window,
        record_input, take_published_input,
    };

    #[test]
    fn controller_events_publish_matching_batches_and_finish_flushes_pending_input() {
        const CHILD: &str = "FOCUSFORGE_GUI_INPUT_TRACE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(
                std::env::current_exe().expect("the test binary has a path"),
            )
            .args([
                "--exact",
                "bridge::tests::controller_events_publish_matching_batches_and_finish_flushes_pending_input",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .status()
            .expect("the isolated accounting test starts");
            assert!(
                status.success(),
                "the isolated accounting test passes: {status}"
            );
            return;
        }

        assert!(timing_snapshot().is_none());
        assert_eq!(record_input(InputSurface::Settings), 0);
        record_coalesced_input(InputSurface::Settings);
        record_images_input();
        record_settings_input();
        record_images_input();
        record_gallery_input();
        assert_eq!(take_published_input(InputSurface::Settings), None);
        assert_eq!(record_destroyed_window(), [None, None, None, None, None]);
        assert_eq!(finish_input_batches(), [None, None, None, None, None]);

        enable_trace_accounting();
        let _ = finish_input_batches();
        let before = timing_snapshot().expect("trace accounting is enabled");

        assert_ne!(record_input(InputSurface::Canvas), 0);
        record_coalesced_input(InputSurface::Canvas);
        record_coalesced_input(InputSurface::Canvas);
        let burst = take_published_input(InputSurface::Canvas).expect("the first canvas drain");
        assert_eq!((burst.count, burst.coalesced), (1, 2));
        assert_ne!(record_input(InputSurface::Canvas), 0);
        record_coalesced_input(InputSurface::Canvas);
        let pending = finish_input_batches()[0].expect("the terminal canvas drain");
        assert_eq!((pending.count, pending.coalesced), (1, 1));

        let events = Arc::new(Mutex::new(Vec::new()));
        record_images_input();
        assert!(
            model_publication_scope(&ShellEvent::ImagesChanged).is_some(),
            "an Images callback must drain when its Images update is published"
        );
        let mut controller = AppController::new();
        controller.set_sink(Arc::new({
            let events = events.clone();
            move |event| {
                assert!(model_publication_scope(&event).is_some());
                events.lock().unwrap().push(event);
            }
        }));
        record_surface_input(InputSurface::Other);
        controller.select_dock_tab(DockTab::Images);
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &[ShellEvent::DockTabSelected(DockTab::Images)]
        );
        assert_eq!(finish_input_batches(), [None, None, None, None, None]);

        record_settings_input();
        record_images_input();
        record_gallery_input();
        assert!(model_publication_scope(&ShellEvent::SettingsChanged).is_some());
        assert_eq!(
            finish_input_batches(),
            [
                None,
                None,
                Some(crate::diagnostics::InputBatch {
                    first: 6,
                    last: 6,
                    count: 1,
                    coalesced: 0,
                    incomplete: false,
                    publication: 0,
                }),
                Some(crate::diagnostics::InputBatch {
                    first: 7,
                    last: 7,
                    count: 1,
                    coalesced: 0,
                    incomplete: false,
                    publication: 0,
                }),
                None,
            ]
        );

        record_gallery_input();
        finish_input_trace();
        assert_eq!(finish_input_batches(), [None, None, None, None, None]);
        let after = timing_snapshot().expect("trace accounting remains enabled");
        assert_eq!(after.admitted, before.admitted + 10);
    }

    /// Drives the real generated Images callbacks through an installed,
    /// headless window, so a misrouted callback (e.g. to `Other`) fails it.
    #[test]
    fn images_callbacks_invoked_through_the_real_window_record_the_images_surface() {
        const CHILD: &str = "FOCUSFORGE_GUI_IMAGES_CALLBACK_TRACE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(
                std::env::current_exe().expect("the test binary has a path"),
            )
            .args([
                "--exact",
                "bridge::tests::images_callbacks_invoked_through_the_real_window_record_the_images_surface",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .status()
            .expect("the isolated callback-routing test starts");
            assert!(
                status.success(),
                "the isolated callback-routing test passes: {status}"
            );
            return;
        }

        i_slint_backend_testing::init_no_event_loop();
        enable_trace_accounting();

        let app = super::AppWindow::new().unwrap();
        let mut opened = AppController::new();
        let frames = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/mantis-512/frames");
        if let Some(load) = opened.open_paths(&[frames]) {
            load.join().expect("the load worker did not panic");
        }
        opened.settle_open().expect("the fixture load settles");
        opened.set_images_viewport_height(crate::images::ROW_HEIGHT);
        let controller = Arc::new(Mutex::new(opened));
        let canvas = std::rc::Rc::new(std::cell::RefCell::new(super::Canvas::default()));
        let _installed = super::install(
            &app,
            controller.clone(),
            canvas,
            std::rc::Rc::new(std::cell::RefCell::new(None)),
        );

        app.invoke_images_frame_excluded_toggled(0);
        app.invoke_frame_warning_cancelled();

        let batches = finish_input_batches();
        let images = batches[2].expect("both real callbacks must record under the Images surface");
        assert_eq!(images.count, 2);
        assert!(
            batches[0].is_none()
                && batches[1].is_none()
                && batches[3].is_none()
                && batches[4].is_none(),
            "neither callback may be recorded under any other surface: {batches:?}"
        );

        // A list scroll that moved is one Images input, published at once;
        // the offset the controller already holds is no input at all.
        app.invoke_images_viewport_scrolled(crate::images::ROW_HEIGHT);
        app.invoke_images_viewport_scrolled(crate::images::ROW_HEIGHT);
        app.invoke_images_frame_excluded_toggled(0);
        let batches = finish_input_batches();
        let toggle = batches[2].expect("the toggle records under the Images surface");
        assert_eq!(
            (toggle.count, toggle.first),
            (1, images.last + 2),
            "exactly one input between the two toggles: {batches:?}"
        );
        assert!(
            batches[0].is_none()
                && batches[1].is_none()
                && batches[3].is_none()
                && batches[4].is_none(),
            "the scroll records under no other surface: {batches:?}"
        );
    }
}
