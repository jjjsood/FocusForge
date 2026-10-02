// SPDX-License-Identifier: GPL-3.0-or-later

//! The window's own fuse worker: owns a background thread over
//! `focusforge_core::pipeline::run`, never `focusforge_core::job::Job`
//! directly -- a separate wake source from any other caller (a button, not
//! a signal), the same segment/resume contract.
//!
//! The one module in this crate besides `entry.rs`/`settings_panel.rs`/the
//! canvas files allowed to name `focusforge_core`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use focusforge_core::algorithm::{AlignAlgorithm, StackAlgorithm};
use focusforge_core::align::WarpingReader;
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::{
    COMPOSITE_FILENAME, FrameError, FrameReader, ZMAP_FILENAME, open_frame_set,
};
pub use focusforge_core::io::{JPEG_EXTENSIONS, PNG_EXTENSIONS, TIFF_EXTENSIONS};
use focusforge_core::params::ParamValues;
use focusforge_core::pipeline::{
    self, AlignRequest, FuseOutput, FuseProduct, FuseRequest, RunStage, StageObserver,
};
pub use focusforge_core::project::export::{
    ExportFormat, ExportOptions, JpegSubsampling, MetadataOptions, PngBits, PngCompression,
    params_for,
};
use focusforge_core::project::{self, Project, ProjectError};
use focusforge_core::registry::{self, LookupError};
use focusforge_core::retouch::{RetouchError, Sources};
pub use focusforge_core::tile::{HostBudget, Rect};
use focusforge_core::zmap::{ZMapError, ZMapInfo, ZMapReader, ZMapWriter};

/// Re-exported so `crate::controller` -- which names no algorithm crate --
/// can hold and drive these through the `job::` path alone.
pub use focusforge_core::align::{Registration, Transform};
pub use focusforge_core::job::{ControlState, JobControl, JobProgress, JobStage};
pub use focusforge_core::pipeline::{FuseOutcome, PipelineError};
pub use focusforge_core::project::Op;
pub use focusforge_core::project::{
    HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, ParentRef, kept_entries, now_unix,
};
pub use focusforge_core::retouch::{EditSession, footprint};
pub use focusforge_core::zmap::{NO_SOURCE, source_color};

/// Which operation a failure belongs to, classified from the typed
/// [`PipelineError`] variant rather than from [`JobStage`] or the message
/// text: the control still reads [`JobStage::Align`] well past the align
/// stage itself (crop, staging, the resume ledger and fusion-consumer
/// creation all run under it too), and a raw error string is never parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedOperation {
    /// The frames could not be opened, or the run's settings were rejected,
    /// before any align or fuse work started.
    Setup,
    Alignment,
    Fusion,
    Save,
}

impl FailedOperation {
    /// The failure toast's own short summary -- never the full error text.
    pub fn summary(self) -> &'static str {
        match self {
            FailedOperation::Setup => "Setup failed",
            FailedOperation::Alignment => "Alignment failed",
            FailedOperation::Fusion => "Fusion failed",
            FailedOperation::Save => "Save failed",
        }
    }
}

impl From<&PipelineError> for FailedOperation {
    fn from(err: &PipelineError) -> Self {
        match err {
            PipelineError::Input(_) | PipelineError::Params(_) => FailedOperation::Setup,
            PipelineError::Align(_) => FailedOperation::Alignment,
            _ => FailedOperation::Fusion,
        }
    }
}

/// One failure's durable record: the complete text a toast never shows in
/// full, plus the context to identify which project and (for a save) which
/// target it belongs to. Appended, never edited or removed, so a later
/// failure, a retry or a tab switch never drops what an earlier one
/// recorded.
#[derive(Debug, Clone)]
pub struct Notification {
    pub operation: FailedOperation,
    pub message: String,
    pub project_title: Option<String>,
    pub project_path: Option<PathBuf>,
    /// The save target, `None` for a run failure.
    pub target: Option<PathBuf>,
}

/// The shell's own view of a fuse run -- the surface's six reachable
/// states.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RunState {
    #[default]
    Idle,
    Running,
    Paused,
    Cancelling,
    Failed(String),
    Fused {
        at: String,
    },
    /// Waiting for the live run to settle -- `position` counts the live run
    /// as 1, so the first waiting tab is `2`.
    Queued {
        position: usize,
    },
}

/// Owned data for one fuse run, assembled under the controller lock and
/// moved onto the worker thread -- algorithm ids rather than resolved
/// trait objects, since a `ProjectTab` stores only ids and explicit values.
#[derive(Clone)]
pub struct JobRequest {
    pub frames: Vec<PathBuf>,
    /// Where the finished composite and Z-Map are kept; the run writes no
    /// project.
    pub hold_dir: PathBuf,
    pub scratch_root: PathBuf,
    pub staging_key: String,
    pub fuse_id: String,
    pub fuse_values: ParamValues,
    pub align_id: Option<String>,
    pub align_values: ParamValues,
    pub align_lax: bool,
    pub crop: bool,
    /// Whether this run may use the headless GPU backend at all -- resolved
    /// against the real device only inside [`spawn`], on the worker thread,
    /// never here.
    pub gpu: bool,
}

/// How often the pipeline's poll thread ticks progress -- the window's
/// own frame clock.
pub const POLL: Duration = Duration::from_millis(100);

/// The host budget the frame pane and the edit session each open their
/// readers against.
pub fn host_budget() -> HostBudget {
    HostBudget::host_derived(&focusforge_core::host::snapshot())
}

/// The registered algorithms a [`JobRequest`]'s ids name, resolved before
/// any thread starts.
pub struct Algorithms {
    fuse: &'static dyn StackAlgorithm,
    align: Option<&'static dyn AlignAlgorithm>,
}

/// Resolves `request`'s algorithm ids, or the message a refused run shows.
/// An empty id reports why the registry named no default, when it refused.
pub fn resolve_request(request: &JobRequest) -> Result<Algorithms, String> {
    fn lookup<T>(
        id: &str,
        find: fn(&str) -> Result<T, LookupError>,
        default: fn() -> Result<T, LookupError>,
    ) -> Result<T, String> {
        find(id).map_err(|err| match default() {
            Err(refusal) if id.is_empty() => refusal.to_string(),
            _ => err.to_string(),
        })
    }
    Ok(Algorithms {
        fuse: lookup(
            &request.fuse_id,
            registry::find_stack,
            registry::default_stack,
        )?,
        align: match &request.align_id {
            Some(id) => Some(lookup(id, registry::find_align, registry::default_align)?),
            None => None,
        },
    })
}

/// Resolves whether a run of `fuse_id` may use the headless GPU backend, and
/// a one-line account of the choice either way. `gpu = false` reports the
/// policy without probing an adapter; a device already lost earlier this
/// session falls back to the host rather than reusing it.
pub fn resolve_backend(fuse_id: &str, gpu: bool) -> (Option<&'static dyn TileBackend>, String) {
    if !gpu {
        return (
            None,
            "fusion backend: cpu — GPU processing is off".to_string(),
        );
    }
    let (backend, message) = focusforge_gpu::harness::backend_for(fuse_id, false, None);
    match backend {
        Some(backend) if backend.device_lost() => (
            None,
            "fusion backend: cpu — the GPU device was lost earlier in this session".to_string(),
        ),
        Some(backend) => (
            Some(backend as &'static dyn TileBackend),
            format!("fusion backend: gpu — {message}"),
        ),
        None if message.is_empty() => (
            None,
            format!("fusion backend: cpu — {fuse_id} has no GPU implementation"),
        ),
        None => (None, format!("fusion backend: cpu — {message}")),
    }
}

/// Moves `request` onto a worker thread and runs it to completion through
/// `focusforge_core::pipeline::run` with `algorithms` (from
/// [`resolve_request`]), reporting through `observer`. Resolves the
/// processing backend first thing on this thread ([`resolve_backend`]), so
/// the first adapter probe never runs on the UI thread. Returns the join
/// handle; the caller is expected to read progress through `observer` rather
/// than by joining eagerly.
pub fn spawn(
    request: JobRequest,
    algorithms: Algorithms,
    control: Arc<JobControl>,
    observer: Arc<GuiObserver>,
) -> std::thread::JoinHandle<Result<FuseOutcome, PipelineError>> {
    std::thread::spawn(move || {
        let (backend, backend_line) = resolve_backend(&request.fuse_id, request.gpu);
        tracing::info!("{backend_line}");
        observer.set_backend_line(backend_line);
        let align = algorithms.align.map(|algorithm| AlignRequest {
            algorithm,
            explicit: request.align_values,
            lax: request.align_lax,
        });
        let fuse_request = FuseRequest {
            frames: request.frames,
            output: FuseOutput::Hold(request.hold_dir),
            scratch_root: request.scratch_root,
            staging_key: request.staging_key,
            algorithm: algorithms.fuse,
            explicit: request.fuse_values,
            align,
            crop: request.crop,
            tile_size: None,
            max_host_memory: None,
            backend,
            resume: false,
            cache: true,
            poll: POLL,
        };
        pipeline::run(&fuse_request, observer.as_ref(), control, None)
    })
}

/// The GUI's [`StageObserver`]: no terminal, no signal -- `paused` parks on
/// a condvar until [`GuiObserver::resume`] or the control itself is
/// cancelled, and every tick is stashed for `AppController::job_view` to
/// read on the window's own poll timer rather than pushed anywhere.
pub struct GuiObserver {
    last_progress: Mutex<Option<JobProgress>>,
    pause_gate: Mutex<bool>,
    condvar: Condvar,
    /// [`resolve_backend`]'s own account of this run's processing backend,
    /// set once from the worker thread before the pipeline starts.
    backend_line: Mutex<Option<String>>,
}

impl GuiObserver {
    pub fn new() -> Self {
        GuiObserver {
            last_progress: Mutex::new(None),
            pause_gate: Mutex::new(false),
            condvar: Condvar::new(),
            backend_line: Mutex::new(None),
        }
    }

    /// The most recent [`JobProgress`] snapshot `tick` recorded, or `None`
    /// before the first one.
    pub fn last_progress(&self) -> Option<JobProgress> {
        *self
            .last_progress
            .lock()
            .expect("no thread panicked while holding the progress lock")
    }

    /// [`resolve_backend`]'s line for this run, once [`spawn`] has resolved
    /// it -- `None` before that point.
    pub fn backend_line(&self) -> Option<String> {
        self.backend_line
            .lock()
            .expect("no thread panicked while holding the backend-line lock")
            .clone()
    }

    fn set_backend_line(&self, line: String) {
        *self
            .backend_line
            .lock()
            .expect("no thread panicked while holding the backend-line lock") = Some(line);
    }

    /// Wakes a parked [`StageObserver::paused`] call -- what the toolbar's
    /// resume button calls after clearing the control.
    pub fn resume(&self) {
        let mut gate = self
            .pause_gate
            .lock()
            .expect("no thread panicked while holding the pause gate");
        *gate = true;
        self.condvar.notify_all();
    }
}

impl Default for GuiObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl StageObserver for GuiObserver {
    fn stage(&self, _stage: RunStage) {}

    fn tick(&self, progress: &JobProgress) {
        *self
            .last_progress
            .lock()
            .expect("no thread panicked while holding the progress lock") = Some(*progress);
    }

    fn paused(&self, control: &JobControl) {
        let mut gate = self
            .pause_gate
            .lock()
            .expect("no thread panicked while holding the pause gate");
        *gate = false;
        while !*gate {
            let (guard, timeout) = self
                .condvar
                .wait_timeout(gate, POLL)
                .expect("no thread panicked while holding the pause gate");
            gate = guard;
            if timeout.timed_out() && control.state() != ControlState::PauseRequested {
                // A resume can clear the control and set the gate between
                // the driver's state read and this call's own gate reset
                // above -- a lost wake-up, caught within one more `POLL`
                // rather than parking on a gate nothing will ever set.
                return;
            }
        }
    }
}

/// The run's own phases, in order. A phase declares its name and whether it
/// counts frames ([`Phase::frames`]) -- the one place either is decided, so a
/// phase added later needs only an arm here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Aligning,
    Fusing,
    Writing,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Aligning => "Aligning",
            Phase::Fusing => "Fusing",
            Phase::Writing => "Writing",
        }
    }

    /// `Some((done, total))` while `self` works per frame and `progress`
    /// carries a non-zero total for it, `None` otherwise -- a cached or
    /// resumed alignment included, since its total is `0`. `Aligning` is the
    /// only phase with a frame total.
    pub fn frames(self, progress: &JobProgress) -> Option<(usize, usize)> {
        match self {
            Phase::Aligning if progress.align_total > 0 => {
                Some((progress.align_done, progress.align_total))
            }
            _ => None,
        }
    }
}

/// Which [`Phase`] `progress` is on: `Aligning` while the engine reports that
/// stage, `Writing` once every unit of the fuse stage is done (the seal, which
/// the engine reports as no stage of its own -- `JobStage` has only
/// `Align`/`Fuse`), `Fusing` otherwise -- including no progress at all yet.
pub fn progress_phase(progress: Option<&JobProgress>) -> Phase {
    match progress {
        Some(p) if p.stage == JobStage::Align => Phase::Aligning,
        Some(p) if p.unit_total > 0 && p.units_done >= p.unit_total => Phase::Writing,
        _ => Phase::Fusing,
    }
}

/// The stage word the bar shows for `state`/`progress` -- `Aligning`,
/// `Fusing`, `Writing`, `Paused`, `Cancelling`, `Failed` or a completed run's
/// own `fused HH:MM`. `progress` picks between `Aligning`/`Fusing`/`Writing`
/// while running, via `progress_phase`.
pub fn stage_word(state: &RunState, progress: Option<&JobProgress>) -> String {
    match state {
        RunState::Paused => "Paused".to_string(),
        RunState::Cancelling => "Cancelling".to_string(),
        RunState::Failed(_) => "Failed".to_string(),
        RunState::Idle => String::new(),
        RunState::Fused { at } => format!("fused {at}"),
        RunState::Queued { position } => format!("Queued ({position})"),
        RunState::Running => progress_phase(progress).name().to_string(),
    }
}

/// The toolbar's own run-state vocabulary for `state`: a completed run reads
/// as idle again, since its own transport is a fresh `Fuse stack`.
pub fn toolbar_run_state(state: &RunState) -> &'static str {
    match state {
        RunState::Idle | RunState::Fused { .. } => "idle",
        RunState::Running => "running",
        RunState::Paused => "paused",
        RunState::Cancelling => "cancelling",
        RunState::Failed(_) => "failed",
        RunState::Queued { .. } => "queued",
    }
}

/// `FFProgressBar`'s own tone vocabulary (`"running" | "held" | "failed"`)
/// for `state`.
pub fn bar_tone(state: &RunState) -> &'static str {
    match state {
        RunState::Running => "running",
        RunState::Failed(_) => "failed",
        _ => "held",
    }
}

/// Whether a worker thread is still doing something for `state` -- what
/// gates the settings panel's whole-panel disable and the bridge's own poll
/// timer.
pub fn is_live(state: &RunState) -> bool {
    matches!(
        state,
        RunState::Running | RunState::Paused | RunState::Cancelling
    )
}

/// The phase this run is on, derived through the same `progress_phase`
/// classification [`stage_word`] uses -- never fabricated from a state the
/// engine did not report.
pub fn current_phase(state: &RunState, progress: Option<&JobProgress>) -> Option<Phase> {
    match state {
        RunState::Idle => None,
        RunState::Paused
        | RunState::Cancelling
        | RunState::Failed(_)
        | RunState::Fused { .. }
        | RunState::Queued { .. } => None,
        RunState::Running => Some(progress_phase(progress)),
    }
}

/// The bottom bar's step-text slot: `"<done> of <total>"` while the current
/// phase works per frame and `progress` carries a total for it
/// ([`Phase::frames`]), empty for a name-only phase, outside a running state,
/// or with no progress to derive a phase from.
pub fn progress_text(state: &RunState, progress: Option<&JobProgress>) -> String {
    let (Some(phase), Some(p)) = (current_phase(state, progress), progress) else {
        return String::new();
    };
    phase
        .frames(p)
        .map(|(done, total)| format!("{done} of {total}"))
        .unwrap_or_default()
}

/// The run's phase line for a card or overlay meta text: the lower-cased
/// phase name, plus `, <k> of <N>` when [`progress_text`] has a frame count
/// to show. `None` outside `Running` -- a name-only phase still returns
/// `Some`, so it never falls back to some other line.
pub fn phase_line(state: &RunState, progress: Option<&JobProgress>) -> Option<String> {
    if !matches!(state, RunState::Running) {
        return None;
    }
    let name = progress_phase(progress).name().to_lowercase();
    let text = progress_text(state, progress);
    Some(if text.is_empty() {
        name
    } else {
        format!("{name}, {text}")
    })
}

/// `estimating` while `eta` is `None` and no unit has finished yet,
/// `ETA M:SS` while running with a real estimate, `ETA —` once paused,
/// cancelling or failed -- never a rendered zero.
pub fn eta_text(state: &RunState, progress: Option<&JobProgress>) -> String {
    match state {
        RunState::Paused | RunState::Cancelling | RunState::Failed(_) => "ETA —".to_string(),
        RunState::Idle | RunState::Fused { .. } | RunState::Queued { .. } => String::new(),
        RunState::Running => match progress.and_then(|p| p.eta) {
            Some(eta) => {
                let secs = eta.as_secs();
                format!("ETA {}:{:02}", secs / 60, secs % 60)
            }
            None => "estimating".to_string(),
        },
    }
}

/// The bar's fill fraction: during a computed align stage,
/// `align_done / align_total`, restarting from zero once the fuse stage
/// begins; otherwise `units_done / unit_total` of the fuse job. `0.0` before
/// the first unit, during a cached align stage, or with no run at all.
pub fn bar_fraction(progress: Option<&JobProgress>) -> f32 {
    match progress {
        Some(p) if p.stage == JobStage::Align && p.align_total > 0 => {
            p.align_done as f32 / p.align_total as f32
        }
        Some(p) if p.unit_total > 0 => p.units_done as f32 / p.unit_total as f32,
        _ => 0.0,
    }
}

/// A failure preparing a tab's plane cache directory, or one of the two
/// planes `focusforge_core` materializes into it.
#[derive(Debug, thiserror::Error)]
pub enum PlaneCacheError {
    #[error("failed to prepare the plane cache: {0}")]
    Cache(#[from] std::io::Error),
    #[error(transparent)]
    Project(#[from] ProjectError),
}

/// Where a project's materialized composite/Z-Map TIFFs live for the
/// canvas, under `scratch_root`: keyed by a short hash of its own `.ffp`
/// path, since two tabs can share a project name.
pub fn plane_cache_dir(scratch_root: &Path, project_path: &Path) -> PathBuf {
    scratch_root.join("planes").join(format!(
        "{:016x}",
        crate::controller::path_digest(project_path)
    ))
}

/// Whether `path` already exists with a modification time at or after
/// `ffp_modified` -- what lets a reopened, already-materialized project skip
/// re-decompressing its planes.
fn is_fresh(path: &Path, ffp_modified: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| mtime >= ffp_modified)
        .unwrap_or(false)
}

/// The directory holding checkpoint `id`'s planes in the project's plane cache.
pub fn checkpoint_plane_dir(scratch_root: &Path, project_path: &Path, id: &str) -> PathBuf {
    plane_cache_dir(scratch_root, project_path).join(id)
}

/// Checkpoint `id`'s own composite and Z-Map paths in the project's plane
/// cache: one directory per checkpoint, so showing another row never
/// overwrites the planes already on screen.
fn checkpoint_plane_paths(
    scratch_root: &Path,
    project_path: &Path,
    id: &str,
) -> (PathBuf, PathBuf) {
    let dir = checkpoint_plane_dir(scratch_root, project_path, id);
    (dir.join(COMPOSITE_FILENAME), dir.join(ZMAP_FILENAME))
}

/// What a saved checkpoint needs before the canvas can show it.
pub enum CheckpointPlanes {
    /// Its composite and Z-Map, on disk.
    Ready(PathBuf, PathBuf),
    /// Not cached yet: [`render_checkpoint_planes`] must run first, off the
    /// event-loop thread, since it decompresses planes and an op-bearing
    /// checkpoint re-hashes every source.
    NeedsRender,
}

/// A fused checkpoint's plane cache pair when both files are already newer
/// than the `.ffp`; any other checkpoint, or a stale or missing pair,
/// answers [`CheckpointPlanes::NeedsRender`]. Reads metadata only, never a
/// plane.
pub fn cached_checkpoint_planes(
    project: &Project,
    scratch_root: &Path,
    id: &str,
) -> Result<CheckpointPlanes, PlaneCacheError> {
    if !project.history().get(id)?.ops.is_empty() {
        return Ok(CheckpointPlanes::NeedsRender);
    }
    let (composite, zmap) = checkpoint_plane_paths(scratch_root, project.path(), id);
    let ffp_modified = std::fs::metadata(project.path())?.modified()?;
    if is_fresh(&composite, ffp_modified) && is_fresh(&zmap, ffp_modified) {
        Ok(CheckpointPlanes::Ready(composite, zmap))
    } else {
        Ok(CheckpointPlanes::NeedsRender)
    }
}

/// Renders checkpoint `id` of the project at `project_path` into its plane
/// cache pair, over a project opened on the calling thread. An op-bearing
/// checkpoint re-hashes its sources on every call and refuses on any
/// missing or changed frame.
pub fn render_checkpoint_planes(
    project_path: &Path,
    scratch_root: &Path,
    id: &str,
) -> Result<(PathBuf, PathBuf), PlaneCacheError> {
    let mut project = Project::open(project_path)?;
    let (composite, zmap) = checkpoint_plane_paths(scratch_root, project_path, id);
    write_checkpoint_planes(&mut project, id, &composite, &zmap)?;
    Ok((composite, zmap))
}

fn write_checkpoint_planes(
    project: &mut Project,
    id: &str,
    composite: &Path,
    zmap: &Path,
) -> Result<(), PlaneCacheError> {
    if let Some(dir) = composite.parent() {
        std::fs::create_dir_all(dir)?;
    }
    project::render_checkpoint(project, id, composite)?;
    let checkpoint = project.history().get(id)?.clone();
    project::materialize_zmap(project, &checkpoint, zmap)?;
    Ok(())
}

/// One `"missing: <path>"` / `"changed: <path> (found at <path>)"` detail
/// line cut down to its state and the frame's file name, which a pane-wide
/// line would otherwise elide.
fn frame_line(line: &str) -> String {
    let Some((state, rest)) = line.split_once(": ") else {
        return line.to_string();
    };
    let path = rest.split(" (found at ").next().unwrap_or(rest);
    let name = Path::new(path)
        .file_name()
        .map_or(path.into(), |name| name.to_string_lossy());
    format!("{state}: {name}")
}

/// The right pane's text for a checkpoint it cannot show, one entry per
/// line: a heading, then each missing or changed frame. Never names the
/// checkpoint.
pub fn render_refusal_lines(err: &PlaneCacheError) -> Vec<String> {
    match err {
        PlaneCacheError::Project(ProjectError::RenderSourcesUnavailable { detail, .. }) => {
            std::iter::once(
                "Cannot show this checkpoint — these frames are missing or changed:".to_string(),
            )
            .chain(detail.lines().map(frame_line))
            .collect()
        }
        _ => {
            eprintln!("failed to render a checkpoint: {err}");
            vec!["Cannot show this checkpoint.".to_string()]
        }
    }
}

/// A checkpoint render worker's result as the controller keeps it: the
/// planes, or the right pane's refusal lines. A panicked worker refuses with
/// the generic line rather than leaving its row pending.
pub fn render_outcome(
    result: std::thread::Result<Result<(PathBuf, PathBuf), PlaneCacheError>>,
) -> Result<(PathBuf, PathBuf), Vec<String>> {
    match result {
        Ok(rendered) => rendered.map_err(|err| render_refusal_lines(&err)),
        Err(_) => {
            eprintln!("a checkpoint render panicked");
            Err(vec!["Cannot show this checkpoint.".to_string()])
        }
    }
}

/// What the export modal writes: the tab's current position, resolved on
/// the calling thread so the worker needs nothing but this and a
/// destination.
pub enum ExportSource {
    /// A saved checkpoint: reopens `project_path` and calls
    /// [`project::export_checkpoint`].
    Saved { project_path: PathBuf, id: String },
    /// A held row's own state -- not yet in any `.ffp`.
    Held(HeldCheckpoint),
    /// The active tab's edit session applied over its current row: a clone
    /// of `EditSession::indices()`, the map's own shape, and the row's own
    /// sources/registration/reference/description (`ProjectTab::fused()`
    /// for the first two).
    Live {
        width: u32,
        height: u32,
        indices: Vec<u16>,
        sources: Vec<PathBuf>,
        registration: Registration,
        reference: usize,
        description: String,
    },
}

/// [`export_position`]'s own error: a project/render failure, or writing a
/// live export's temp Z-Map.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error(transparent)]
    ZMap(#[from] ZMapError),
}

/// A process-wide counter, so two live exports started in the same process
/// never share a temp Z-Map path.
static NEXT_EXPORT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Writes `source`'s image to `path` as the default TIFF (16-bit
/// uncompressed, EXIF and ICC on) -- the export modal's own worker body,
/// run off the UI thread. A held row's or a live edit session's export
/// changes nothing about the project: no checkpoint is written, no `.ffp`
/// touched, and a live export's temp Z-Map is removed whether the write
/// finishes or fails.
pub fn export_position(
    source: ExportSource,
    path: &Path,
    options: &project::export::ExportOptions,
    metadata: &project::export::MetadataOptions,
) -> Result<(), ExportError> {
    match source {
        ExportSource::Saved { project_path, id } => {
            let mut project = Project::open(&project_path)?;
            project::export_checkpoint(&mut project, &id, path, options, metadata)?;
        }
        ExportSource::Held(held) => {
            let zmap_source = project::ZmapSource {
                reference: held.align.as_ref().map_or(0, |a| a.reference),
                sources: held.sources,
                registration: held.registration,
                description: held.composite_description,
            };
            project::export_zmap(&held.zmap, &zmap_source, path, options, metadata)?;
        }
        ExportSource::Live {
            width,
            height,
            indices,
            sources,
            registration,
            reference,
            description,
        } => {
            let temp = std::env::temp_dir().join(format!(
                "focusforge-export-{}-{}.tif",
                std::process::id(),
                NEXT_EXPORT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let write = (|| -> Result<(), ExportError> {
                write_zmap(&temp, width, height, sources.len() as u32, &indices)?;
                let zmap_source = project::ZmapSource {
                    sources,
                    registration,
                    reference,
                    description,
                };
                project::export_zmap(&temp, &zmap_source, path, options, metadata)?;
                Ok(())
            })();
            let _ = std::fs::remove_file(&temp);
            write?;
        }
    }
    Ok(())
}

/// Writes `indices`, a whole `width` x `height` map over `frame_count`
/// sources, as the Z-Map file at `path`.
pub fn write_zmap(
    path: &Path,
    width: u32,
    height: u32,
    frame_count: u32,
    indices: &[u16],
) -> Result<(), ZMapError> {
    let info = ZMapInfo::new(width, height, frame_count)?;
    let mut writer = ZMapWriter::create(path, info)?;
    writer.write_tile(
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        indices,
    )?;
    writer.finish()?;
    Ok(())
}

/// An export worker's result as the controller keeps it: `Ok` once written,
/// or the modal's own refusal lines -- [`render_outcome`]'s counterpart. A
/// panicked worker refuses with the generic line rather than leaving the
/// modal writing forever.
pub fn export_outcome(
    result: std::thread::Result<Result<(), ExportError>>,
) -> Result<(), ExportRefusal> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(ExportRefusal {
            lines: export_refusal_lines(&err),
            locate: matches!(
                err,
                ExportError::Project(
                    ProjectError::RenderSourcesUnavailable { .. }
                        | ProjectError::ExportSourcesUnavailable { .. }
                )
            ),
        }),
        Err(_) => {
            eprintln!("an export panicked");
            Err(ExportRefusal {
                lines: vec!["Cannot export this position.".to_string()],
                locate: false,
            })
        }
    }
}

/// A refused export as the modal shows it: its lines, and whether
/// `Locate frames…` can help (only a missing or changed source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRefusal {
    pub lines: Vec<String>,
    pub locate: bool,
}

/// `n` with a comma between every group of three digits.
fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One `"<name> — missing"` / `"<name> — changed"` line -- [`frame_line`]'s
/// counterpart for the export modal, which shows no `state: ` prefix.
fn export_frame_line(line: &str) -> String {
    let Some((state, rest)) = line.split_once(": ") else {
        return line.to_string();
    };
    let path = rest.split(" (found at ").next().unwrap_or(rest);
    let name = Path::new(path)
        .file_name()
        .map_or(path.into(), |name| name.to_string_lossy());
    format!("{name} — {state}")
}

/// The export modal's own refusal lines: one per source for a missing or
/// changed frame (a saved checkpoint's own recorded sources, or a held/live
/// export's plain paths, which can only ever be `missing`), naming the
/// underlying cause for anything else -- an unwritable destination above
/// all.
pub fn export_refusal_lines(err: &ExportError) -> Vec<String> {
    match err {
        ExportError::Project(ProjectError::RenderSourcesUnavailable { detail, .. })
        | ExportError::Project(ProjectError::ExportSourcesUnavailable { detail, .. }) => {
            detail.lines().map(export_frame_line).collect()
        }
        ExportError::Project(ProjectError::ExportWriteFailed { path, source }) => {
            vec![format!("Can't write {}: {source}", path.display())]
        }
        ExportError::Project(ProjectError::ExportTooLarge {
            format,
            width,
            height,
            limit,
            ..
        }) => vec![format!(
            "{} can't be bigger than {} pixels on a side. This image is {} × {}.",
            format.to_uppercase(),
            thousands(*limit),
            thousands(*width),
            thousands(*height)
        )],
        ExportError::Project(ProjectError::ExportUnsupportedSamples { .. }) => vec![
            "PNG and JPEG can only save grey or colour (RGB) images; this one has extra channels."
                .to_string(),
        ],
        ExportError::Project(ProjectError::ExportOptions { source }) => {
            vec![format!("Some export settings aren't valid: {source}.")]
        }
        _ => {
            eprintln!("failed to export a checkpoint: {err}");
            vec!["Cannot export this position.".to_string()]
        }
    }
}

/// The left pane's text for a frame it cannot open, in
/// [`render_refusal_lines`]' form: a heading, then `missing: <file name>`
/// when no file is at `path`, `changed: <file name>` when the file there no
/// longer reads as a frame.
pub fn frame_refusal_lines(path: &Path) -> Vec<String> {
    let state = if path.is_file() { "changed" } else { "missing" };
    vec![
        "Cannot show this frame:".to_string(),
        frame_line(&format!("{state}: {}", path.display())),
    ]
}

/// What can go wrong opening a tab's editor state: its Z-Map or the source
/// frames a stroke recomposites against.
#[derive(Debug, thiserror::Error)]
pub enum EditSessionError {
    #[error(transparent)]
    ZMap(#[from] ZMapError),
    #[error(transparent)]
    Frames(#[from] FrameError),
}

/// What a recomposite worker parks: each region it gathered, in dispatch
/// order, with its samples or the reason its read failed.
type Gathered = Vec<(Rect, Result<Vec<u16>, String>)>;

/// Wakes the UI thread once a recomposite worker has parked its result.
pub type RecompositeWake = Arc<dyn Fn() + Send + Sync>;

/// One session's recomposite queue: regions waiting for a worker, whether a
/// worker holds a batch, and where it parks the result.
#[derive(Default)]
struct Recomposites {
    queued: Vec<Rect>,
    in_flight: bool,
    parked: Arc<Mutex<Option<Gathered>>>,
}

/// One tab's live editor state: its undo/redo session, the frame set a
/// stroke recomposites against and the registration warping each of its
/// reads, the resident display cache the renderer overlays, the
/// in-progress stroke, if any, and the regions whose pixels are still being
/// gathered. The queue lives here, so a session that is dropped takes its
/// pending results with it.
pub struct EditState {
    pub session: EditSession,
    pub sources: Arc<dyn FrameReader>,
    pub registration: Registration,
    pub tiles: crate::canvas::ResidentTiles,
    pub stroke: Option<crate::brush::LiveStroke>,
    recomposites: Recomposites,
}

/// A resident tile's default edge -- edits are small, so a fixed 256 px grid
/// keeps memory proportional to the edited area.
pub const RESIDENT_TILE_SIZE: u32 = 256;

/// Runs `f` over `reader`'s frames each read through its recorded transform,
/// at the registration's own crop origin -- the same view the left pane
/// draws, so a stroke transfers exactly what it showed.
fn with_registered_sources<R>(
    reader: &dyn FrameReader,
    registration: &Registration,
    f: impl FnOnce(&Sources) -> R,
) -> R {
    let frame = reader.dimensions();
    let samples_per_pixel = reader.frame(0).samples_per_pixel;
    let transforms = registration.transforms(reader.len());
    let warped = WarpingReader::new(reader, frame, samples_per_pixel, transforms);
    f(&Sources {
        reader: &warped,
        frame_count: reader.len() as u32,
        samples_per_pixel,
        origin: registration.origin(),
    })
}

impl EditState {
    /// Recomposites `region` into this tab's resident display tiles on the
    /// calling thread, from its own edited indices and the frame-as-fused
    /// view. The session-open replay uses it; a live edit goes through
    /// [`Self::queue_recomposite`].
    pub fn recomposite(&mut self, region: Rect) -> Result<(), RetouchError> {
        let (tiles, indices) = (&mut self.tiles, self.session.indices());
        with_registered_sources(&*self.sources, &self.registration, |sources| {
            tiles.recomposite(region, indices, sources)
        })
    }

    /// Queues `region`, whose edited indices already changed, for a worker
    /// to gather, and bumps the tiles' generation so the overlay shows the
    /// new indices at once. Regions queued while a worker holds a batch go
    /// out together when it is installed; `wake` runs on the worker once a
    /// batch is parked.
    pub fn queue_recomposite(&mut self, region: Rect, wake: &RecompositeWake) {
        self.recomposites.queued.push(region);
        self.tiles.touch();
        if !self.recomposites.in_flight {
            self.dispatch_recomposites(wake);
        }
    }

    /// Whether no stroke is live and no recomposite is queued or in flight,
    /// so the resident tiles hold every edit the indices do.
    pub fn is_settled(&self) -> bool {
        self.stroke.is_none() && self.recomposites.queued.is_empty() && !self.recomposites.in_flight
    }

    /// Installs the batch a worker has parked, in the order its regions were
    /// queued, then dispatches whatever queued meanwhile. A region whose
    /// read failed is logged and leaves its tiles as they were. Returns
    /// `true` once nothing is in flight.
    pub fn settle_recomposite(&mut self, wake: &RecompositeWake) -> bool {
        let parked = self
            .recomposites
            .parked
            .lock()
            .expect("no worker panicked while parking its recomposites")
            .take();
        if let Some(gathered) = parked {
            self.recomposites.in_flight = false;
            for (region, result) in gathered {
                match result {
                    Ok(samples) => self.tiles.install(region, &samples),
                    Err(error) => {
                        tracing::error!(%error, ?region, "background recomposite failed");
                    }
                }
            }
            self.dispatch_recomposites(wake);
        }
        !self.recomposites.in_flight
    }

    /// Hands every queued region to one worker thread, with the indices as
    /// they are now, and marks the batch in flight.
    fn dispatch_recomposites(&mut self, wake: &RecompositeWake) {
        if self.recomposites.queued.is_empty() {
            return;
        }
        let width = self.session.tile().width;
        let indices = self.session.indices();
        let batch: Vec<(Rect, Vec<u16>)> = self
            .recomposites
            .queued
            .drain(..)
            .map(|region| (region, crate::canvas::gather_rect(indices, width, region)))
            .collect();
        self.recomposites.in_flight = true;
        let sources = Arc::clone(&self.sources);
        let registration = self.registration.clone();
        let parked = Arc::clone(&self.recomposites.parked);
        let wake = Arc::clone(wake);
        std::thread::spawn(move || {
            let gathered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_registered_sources(&*sources, &registration, |sources| {
                    batch
                        .iter()
                        .map(|(region, indices)| {
                            let samples = crate::canvas::gather(*region, indices, sources);
                            (*region, samples.map_err(|error| error.to_string()))
                        })
                        .collect::<Gathered>()
                })
            }))
            .unwrap_or_else(|_| {
                batch
                    .iter()
                    .map(|(region, _)| (*region, Err("the recomposite worker panicked".into())))
                    .collect()
            });
            *parked
                .lock()
                .expect("no worker panicked while parking its recomposites") = Some(gathered);
            wake();
        });
    }
}

/// Opens `zmap` and `frames` and builds an [`EditState`] over the whole
/// map: an [`EditSession`] seeded from every pixel, `frames` opened through
/// [`open_frame_set`] as the frame set a stroke recomposites against
/// (warped by `registration` at recomposite time), and an empty resident
/// display cache gridded at `tile_size` px. `chain` -- the ops an op-bearing
/// row stacks over its fused base, oldest first -- is then applied and
/// recomposited and the session rebased onto it, so it opens with no ops of
/// its own. A chain op that cannot be applied ends the replay; a region that
/// cannot be recomposited is skipped.
pub fn open_edit_session(
    zmap: &Path,
    frames: &[PathBuf],
    registration: Registration,
    tile_size: u32,
    budget: HostBudget,
    chain: Vec<Op>,
) -> Result<EditState, EditSessionError> {
    let reader = ZMapReader::open(zmap)?;
    let info = reader.info();
    let tile = Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    };
    let mut indices = vec![0u16; info.tile_len(tile)];
    reader.read_tile_into(tile, &mut indices)?;
    let session = EditSession::new(info, tile, indices)
        .expect("a whole-map tile always fits its own map and its own tile_len");
    let sources: Arc<dyn FrameReader> = Arc::from(open_frame_set(frames, budget)?.reader);
    let samples_per_pixel = sources.frame(0).samples_per_pixel;
    let tiles = crate::canvas::ResidentTiles::new(
        tile_size,
        info.width(),
        info.height(),
        samples_per_pixel,
    );
    let mut edit = EditState {
        session,
        sources,
        registration,
        tiles,
        stroke: None,
        recomposites: Recomposites::default(),
    };
    for op in chain {
        match edit.session.append(op) {
            Ok(Some(region)) => {
                if let Err(err) = edit.recomposite(region) {
                    tracing::error!(error = %err, "restored stroke recomposite failed");
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::error!(error = %err, "restored stroke could not be applied");
                break;
            }
        }
    }
    edit.session.rebase();
    Ok(edit)
}

/// A written project, opened, with the id written per kept entry.
pub type SavedProject = (Project, Vec<String>);

/// Writes `entries`, each under its own parent, and applies `deletes` at
/// `target` -- onto the project already there when `onto_existing` -- then
/// opens the result. `keep` stays stored even when deleted. Returns it with
/// the id written per entry [`kept_entries`] keeps. On failure `target` is
/// left exactly as it was.
pub fn save_in_memory(
    target: &Path,
    entries: &[InMemoryCheckpoint],
    deletes: &[ParentRef],
    keep: Option<&ParentRef>,
    onto_existing: bool,
) -> Result<SavedProject, ProjectError> {
    let ids = project::seal_in_memory(target, entries, deletes, keep, onto_existing)?;
    Ok((Project::open(target)?, ids))
}

/// One completed worker thread's result, boiled down to the shell's next
/// [`RunState`] and, on a genuine completion, the checkpoint the run holds
/// and the names of the frames its alignment could not register.
/// `operation` is `Some` exactly when `state` is [`RunState::Failed`].
pub struct Settled {
    pub state: RunState,
    pub held: Option<HeldCheckpoint>,
    pub diverged: Vec<String>,
    pub operation: Option<FailedOperation>,
}

/// The divergence dialog's body, naming the frames alignment could not
/// register inline; past five names the rest are counted, not named.
pub(crate) fn divergence_body(names: &[String]) -> String {
    const NAMED: usize = 5;
    let list = match names {
        [] => String::new(),
        [one] => one.clone(),
        _ if names.len() > NAMED => format!(
            "{} and {} more",
            names[..NAMED].join(", "),
            names.len() - NAMED
        ),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    };
    let (verb, rest) = if names.len() == 1 {
        ("was", "it is")
    } else {
        ("were", "they are")
    };
    format!("{list} could not be aligned and {verb} fused as {rest}.")
}

/// Interprets a joined worker thread's result -- a panic and a pipeline
/// error both become [`RunState::Failed`], a clean cancel returns to
/// [`RunState::Idle`] (the engine already preserved its own resumable state
/// on disk, per its own "reported, never controlled" contract), and a
/// completion hands back the held checkpoint.
pub fn settle(result: std::thread::Result<Result<FuseOutcome, PipelineError>>) -> Settled {
    match result {
        Ok(Ok(FuseOutcome::Completed(report))) => {
            let diverged = report
                .align
                .iter()
                .flat_map(|align| align.alignment.failed())
                .map(|frame| report.frames[frame.frame].clone())
                .collect();
            let FuseProduct::Held(held) = report.product else {
                unreachable!("the window's runs always hold their result");
            };
            Settled {
                state: RunState::Fused {
                    at: crate::checkpoints::local_hhmm(held.created_at),
                },
                held: Some(held),
                diverged,
                operation: None,
            }
        }
        Ok(Ok(FuseOutcome::Cancelled { .. })) => Settled {
            state: RunState::Idle,
            held: None,
            diverged: Vec::new(),
            operation: None,
        },
        Ok(Err(err)) => {
            let operation = FailedOperation::from(&err);
            Settled {
                state: RunState::Failed(err.to_string()),
                held: None,
                diverged: Vec::new(),
                operation: Some(operation),
            }
        }
        Err(_) => Settled {
            state: RunState::Failed("the fuse worker stopped unexpectedly".to_string()),
            held: None,
            diverged: Vec::new(),
            operation: Some(FailedOperation::Fusion),
        },
    }
}

#[cfg(test)]
mod export_refusal_tests {
    use super::*;
    use focusforge_core::params::ParamValue;
    use focusforge_core::project::export::{ExportFormat, ExportOptions};

    fn too_large(width: u32, height: u32) -> ExportError {
        ExportError::Project(ProjectError::ExportTooLarge {
            path: PathBuf::from("out.jpg"),
            format: "jpeg".to_string(),
            width,
            height,
            limit: 65_535,
        })
    }

    #[test]
    fn too_large_names_the_limit_and_the_size() {
        assert_eq!(
            export_refusal_lines(&too_large(70_000, 40_000)),
            vec![
                "JPEG can't be bigger than 65,535 pixels on a side. This image is 70,000 × 40,000."
            ]
        );
        let one_over = export_refusal_lines(&too_large(65_536, 12));
        assert_eq!(
            one_over,
            vec!["JPEG can't be bigger than 65,535 pixels on a side. This image is 65,536 × 12."]
        );
    }

    #[test]
    fn unsupported_samples_name_the_cause() {
        let err = ExportError::Project(ProjectError::ExportUnsupportedSamples {
            path: PathBuf::from("out.png"),
            format: "png".to_string(),
            samples_per_pixel: 4,
        });
        assert_eq!(
            export_refusal_lines(&err),
            vec![
                "PNG and JPEG can only save grey or colour (RGB) images; this one has extra channels."
            ]
        );
    }

    #[test]
    fn invalid_options_name_the_setting() {
        let mut values = ParamValues::new();
        values.insert("jpeg-quality".to_string(), ParamValue::Int(0));
        let err = ExportOptions::from_values(ExportFormat::Jpeg, &values).unwrap_err();
        let lines = export_refusal_lines(&ExportError::Project(err));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("Some export settings aren't valid: ") && lines[0].ends_with('.'),
            "{lines:?}"
        );
        assert!(lines[0].contains("jpeg-quality"), "{lines:?}");
    }

    #[test]
    fn only_a_missing_source_offers_locate() {
        let refusal = export_outcome(Ok(Err(too_large(70_000, 1)))).unwrap_err();
        assert!(!refusal.locate);
        let panicked = export_outcome(Err(Box::new("boom"))).unwrap_err();
        assert!(!panicked.locate);
        assert_eq!(panicked.lines, vec!["Cannot export this position."]);
    }
}
