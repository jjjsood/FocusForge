// SPDX-License-Identifier: GPL-3.0-or-later

//! The window's own fuse worker: owns a background thread over
//! `focusforge_core::pipeline::run`, never `focusforge_core::job::Job`
//! directly -- a separate wake source from any other caller (a button, not
//! a signal), the same segment/resume contract.
//!
//! The one module in this crate besides `entry.rs`/`settings_panel.rs`/the
//! canvas files allowed to name `focusforge_core`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use focusforge_core::algorithm::{AlignAlgorithm, AlignStatus, StackAlgorithm};
use focusforge_core::align::WarpingReader;
use focusforge_core::equalize::GainReader;
use focusforge_core::gpu::{AlignBackend, TileBackend};
use focusforge_core::io::{
    COMPOSITE_FILENAME, FrameError, FrameReader, TiffStack, ZMAP_FILENAME, open_frame_set,
};
pub use focusforge_core::io::{JPEG_EXTENSIONS, PNG_EXTENSIONS, TIFF_EXTENSIONS};
use focusforge_core::job::StageCache;
use focusforge_core::params::ParamValues;
use focusforge_core::pipeline::{
    self, AlignRequest, FuseOutput, FuseProduct, FuseReport, FuseRequest, RunStage, StageObserver,
};
pub use focusforge_core::project::PendingState;
pub use focusforge_core::project::export::{
    ExportFormat, ExportOptions, JpegSubsampling, MetadataOptions, PngBits, PngCompression,
    params_for,
};
use focusforge_core::project::{self, Project, ProjectError};
use focusforge_core::registry::{self, LookupError};
use focusforge_core::retouch::{RetouchError, Sources, blend};
pub use focusforge_core::tile::{HostBudget, Rect};
use focusforge_core::zmap::{ZMapError, ZMapInfo, ZMapReader};

/// Re-exported so `crate::controller` -- which names no algorithm crate --
/// can hold and drive these through the `job::` path alone.
pub use focusforge_core::align::{Registration, Transform};
pub use focusforge_core::equalize::UNIT_GAIN;
pub use focusforge_core::job::{ControlState, JobControl, JobProgress, JobStage};
pub use focusforge_core::pipeline::{FuseOutcome, PipelineError};
pub use focusforge_core::project::Op;
pub use focusforge_core::project::{
    HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, ParentRef, kept_entries, now_unix,
};
pub use focusforge_core::retouch::{footprint, top_source};
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
        /// Frames the run's alignment could not register and fused as read.
        unaligned: usize,
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
    /// The gain bound brightness equalization runs with, `None` when off.
    pub equalize: Option<f64>,
    /// Whether this run may use the headless GPU backend at all -- resolved
    /// against the real device only inside [`spawn`], on the worker thread,
    /// never here.
    pub gpu: bool,
}

/// How often the pipeline's poll thread ticks progress -- the window's
/// own frame clock.
pub const POLL: Duration = Duration::from_millis(100);

/// How long the canvas must see no action before a run's interactive
/// admission cap may lift.
pub const INTERACTION_QUIET: Duration = Duration::from_millis(250);

/// The most tiles or registrations a run may hold in flight while the
/// canvas is being used: a quarter of the host's threads, at least one.
pub fn interactive_cap() -> usize {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    (threads / 4).max(1)
}

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
/// the first adapter probe never runs on the UI thread, and opens the stage
/// cache under the run's scratch root so an unchanged alignment is reused
/// (a cache that cannot open is logged and the run goes uncached). Returns the join
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
        let align_backend = resolve_align_backend(&request, &algorithms, &observer);
        let stage_cache = match StageCache::open(&request.scratch_root) {
            Ok(cache) => Some(cache),
            Err(err) => {
                tracing::warn!("alignment cache unavailable, running uncached: {err}");
                None
            }
        };
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
            equalize: request.equalize,
            tile_size: None,
            max_host_memory: None,
            backend,
            align_backend,
            resume: false,
            cache: true,
            poll: POLL,
        };
        pipeline::run(
            &fuse_request,
            observer.as_ref(),
            control,
            stage_cache.as_ref(),
        )
    })
}

/// The accelerator a run's align stage may use: the lazy GPU backend when GPU
/// processing is on and the aligner has a GPU path, unless its device was
/// lost earlier this session. The adapter comes up only when a level is
/// large enough to offload; the observer reads its name back for the run's
/// alignment summary.
fn resolve_align_backend(
    request: &JobRequest,
    algorithms: &Algorithms,
    observer: &GuiObserver,
) -> Option<&'static dyn AlignBackend> {
    let algorithm = algorithms.align.filter(|_| request.gpu)?;
    let (lazy, _) =
        focusforge_gpu::harness::align_backend_for(algorithm.id(), false, None, |line| {
            tracing::info!("{line}")
        });
    let lazy = lazy?;
    if lazy.lost() {
        tracing::info!("alignment backend: cpu — the GPU device was lost earlier in this session");
        return None;
    }
    observer.set_align_adapter(lazy.adapter());
    Some(lazy)
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
    /// Every [`StageObserver::notice`] the run raised, in order.
    notices: Mutex<Vec<String>>,
    /// Where the align backend names the adapter once a level opens it; unset
    /// for a run with no accelerator on offer.
    align_adapter: OnceLock<&'static OnceLock<String>>,
    /// Why the align stage reran on the CPU, when it did.
    rerun_reason: Mutex<Option<String>>,
}

impl GuiObserver {
    pub fn new() -> Self {
        GuiObserver {
            last_progress: Mutex::new(None),
            pause_gate: Mutex::new(false),
            condvar: Condvar::new(),
            backend_line: Mutex::new(None),
            notices: Mutex::new(Vec::new()),
            align_adapter: OnceLock::new(),
            rerun_reason: Mutex::new(None),
        }
    }

    /// The notices the pipeline raised so far, in order.
    pub fn notices(&self) -> Vec<String> {
        self.notices
            .lock()
            .expect("no thread panicked while holding the notices lock")
            .clone()
    }

    /// Why the align stage reran on the CPU after its accelerator failed;
    /// `None` when it did not.
    pub fn rerun_reason(&self) -> Option<String> {
        self.rerun_reason
            .lock()
            .expect("no thread panicked while holding the rerun lock")
            .clone()
    }

    fn set_align_adapter(&self, adapter: &'static OnceLock<String>) {
        let _ = self.align_adapter.set(adapter);
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

    fn notice(&self, message: &str) {
        tracing::warn!("{message}");
        // The CPU rerun reaches the user through the alignment toast, so it
        // stays out of the run's own notice list.
        if let Some(reason) = message.strip_suffix(pipeline::ALIGN_RERUN_NOTICE) {
            *self
                .rerun_reason
                .lock()
                .expect("no thread panicked while holding the rerun lock") =
                Some(reason.to_string());
            return;
        }
        self.notices
            .lock()
            .expect("no thread panicked while holding the notices lock")
            .push(message.to_string());
    }

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
        RunState::Fused { at, .. } => format!("fused {at}"),
        RunState::Queued { position } => format!("Queued ({position})"),
        RunState::Running => progress_phase(progress).name().to_string(),
    }
}

/// The notice a kept run with unregistered frames raises, `None` for none:
/// `"4 frames unaligned"`, `"1 frame unaligned"`.
pub fn unaligned_notice(unaligned: usize) -> Option<String> {
    let frames = match unaligned {
        0 => return None,
        1 => "frame",
        _ => "frames",
    };
    Some(format!("{unaligned} {frames} unaligned"))
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
    /// A held row's own state -- not yet in any `.ffp`: exports its
    /// composite through [`project::export_held`], with no project; the
    /// reference frame is read only for its metadata.
    Held(HeldCheckpoint),
    /// The active tab's strokes not yet saved, over its current row's
    /// composite: exported through [`project::export_pending`], with no
    /// project.
    Pending(project::PendingState),
}

/// [`export_position`]'s own error: a project or render failure.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Project(#[from] ProjectError),
}

/// Writes `source`'s image to `path` as the default TIFF (16-bit
/// uncompressed, EXIF and ICC on) -- the export modal's own worker body,
/// run off the UI thread. A held row's or a pending state's export changes
/// nothing about the project: no checkpoint is written and no `.ffp`
/// touched.
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
            project::export_held(&held, path, options, metadata)?;
        }
        ExportSource::Pending(state) => {
            project::export_pending(&state, path, options, metadata)?;
        }
    }
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
/// changed frame (a saved checkpoint's own recorded sources, or a live
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

/// What can go wrong opening a tab's editor state: its Z-Map, the composite
/// it edits over or the source frames a stroke blends from.
#[derive(Debug, thiserror::Error)]
pub enum EditSessionError {
    #[error(transparent)]
    ZMap(#[from] ZMapError),
    #[error(transparent)]
    Composite(#[from] focusforge_core::TiffError),
    #[error(transparent)]
    Frames(#[from] FrameError),
}

/// What a recomposite starts from before its ops are blended over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreImage {
    /// The base composite alone.
    Base,
    /// The base composite under the resident samples as they were before the
    /// drag in progress began.
    Snapshot,
    /// The base composite under the resident samples as they are.
    Current,
}

/// One region of the edited image to render: where, what it starts from, the
/// ops blended over that in order, and whether it is the last job of a drag,
/// after which the pre-drag samples are dropped.
#[derive(Debug, Clone)]
pub struct Recomposite {
    pub region: Rect,
    pub pre: PreImage,
    pub ops: Vec<Op>,
    pub ends_drag: bool,
}

/// What a recomposite worker renders: each region, in dispatch order, with
/// its samples or the reason it failed.
type Rendered = Vec<(Rect, Result<Vec<u16>, String>)>;

/// What a worker parks: its regions, the pre-drag tiles it holds once the
/// batch is done (`None` when it panicked), and whether the batch ended the
/// drag.
struct Parked {
    rendered: Rendered,
    snapshot: Option<crate::canvas::ResidentTiles>,
    ends_drag: bool,
}

/// Wakes the UI thread once a recomposite worker has parked its result.
pub type RecompositeWake = Arc<dyn Fn() + Send + Sync>;

/// One session's recomposite queue: jobs waiting for a worker, whether a
/// worker holds a batch, and where it parks the result.
#[derive(Default)]
struct Recomposites {
    queued: Vec<Recomposite>,
    in_flight: bool,
    parked: Arc<Mutex<Option<Parked>>>,
}

/// One tab's live editor state: the strokes not yet saved with their undo
/// and redo stacks, the composite and the frame set the edited image is
/// blended from and the registration warping each frame read, the resident
/// display cache the renderer overlays, the in-progress stroke, if any, and
/// the regions still being rendered. The queue lives here, so a state that
/// is dropped takes its queued results with it.
pub struct EditState {
    /// The ops over `chain` not yet saved, oldest first.
    pending: Vec<Op>,
    /// The ops undone from `pending`, the most recent last.
    undone: Vec<Op>,
    /// The stored Z-Map of the row being edited, read-only.
    zmap: ZMapReader,
    pub sources: Arc<dyn FrameReader>,
    pub registration: Registration,
    /// The composite the edits are blended over.
    pub base: Arc<TiffStack>,
    /// The file `base` reads.
    pub(crate) composite: PathBuf,
    /// The ops already blended into `base`, oldest first.
    base_ops: Vec<Op>,
    /// The ops the row stacks over `base` before the pending ones, oldest
    /// first.
    chain: Vec<Op>,
    pub tiles: crate::canvas::ResidentTiles,
    /// The resident samples as they were when the drag in progress began, a
    /// tile at a time; empty between drags.
    snapshot: crate::canvas::ResidentTiles,
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
    let gained = GainReader::new(reader, registration.gains());
    let warped = WarpingReader::new(&gained, frame, samples_per_pixel, transforms);
    f(&Sources {
        reader: &warped,
        frame_count: reader.len() as u32,
        samples_per_pixel,
        origin: registration.origin(),
    })
}

/// What rendering a region reads, apart from the resident samples.
struct RenderInput {
    base: Arc<TiffStack>,
    sources: Arc<dyn FrameReader>,
    registration: Registration,
    info: ZMapInfo,
    samples_per_pixel: usize,
}

/// Renders `job`: the pre-image's samples over its region with its ops
/// blended in order from their frames, the same step a saved checkpoint's
/// render takes. The result is installed in `local`; a job that ends the
/// drag then empties `snapshot`, whether it rendered or not.
fn render_region(
    input: &RenderInput,
    job: &Recomposite,
    local: &mut crate::canvas::ResidentTiles,
    snapshot: &mut crate::canvas::ResidentTiles,
) -> Result<Vec<u16>, String> {
    let rendered = blend_region(input, job, local, snapshot);
    if job.ends_drag {
        snapshot.clear();
    }
    rendered
}

fn blend_region(
    input: &RenderInput,
    job: &Recomposite,
    local: &mut crate::canvas::ResidentTiles,
    snapshot: &mut crate::canvas::ResidentTiles,
) -> Result<Vec<u16>, String> {
    let region = job.region;
    let spp = input.samples_per_pixel;
    let mut out = vec![0u16; region.width as usize * region.height as usize * spp];
    input
        .base
        .read_tile_into(0, region, &mut out)
        .map_err(|error| error.to_string())?;
    match job.pre {
        PreImage::Base => {}
        PreImage::Current => local.overlay_into(region, &mut out),
        PreImage::Snapshot => {
            snapshot.capture(local, region);
            snapshot.overlay_into(region, &mut out);
        }
    }
    let mut placed = Vec::new();
    for op in &job.ops {
        let Op::BrushStroke { source_id, .. } = op else {
            return Err(RetouchError::Unsupported.to_string());
        };
        let footprint = blend::footprint(op, input.info).map_err(|error| error.to_string())?;
        if let Some(footprint) = footprint {
            placed.push(blend::PlacedStroke {
                op,
                frame: usize::from(*source_id),
                footprint,
            });
        }
    }
    with_registered_sources(&*input.sources, &input.registration, |sources| {
        blend::apply_strokes(
            input.info,
            region,
            &mut out,
            &placed,
            sources.reader,
            sources.origin,
            spp,
        )
    })
    .map_err(|error| error.to_string())?;
    local.install(region, &out);
    Ok(out)
}

impl EditState {
    /// The stored Z-Map's shape.
    pub fn info(&self) -> ZMapInfo {
        self.zmap.info()
    }

    /// The stored Z-Map's index at pixel (`x`, `y`); `None` outside the map or
    /// when the read fails.
    pub fn stored_source(&self, x: u32, y: u32) -> Option<u16> {
        let info = self.info();
        if x >= info.width() || y >= info.height() {
            return None;
        }
        let mut index = [0u16; 1];
        let pixel = Rect {
            x,
            y,
            width: 1,
            height: 1,
        };
        match self.zmap.read_tile_into(pixel, &mut index) {
            Ok(()) => Some(index[0]),
            Err(error) => {
                tracing::warn!(%error, x, y, "stored Z-Map read failed");
                None
            }
        }
    }

    /// The pending ops over the row's chain, oldest first; undone ops are not
    /// included.
    pub fn ops(&self) -> impl Iterator<Item = &Op> {
        self.pending.iter()
    }

    /// How many ops are pending.
    pub fn op_count(&self) -> usize {
        self.pending.len()
    }

    /// Whether a pending op can be undone.
    pub fn can_undo(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether an undone op can be redone.
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Validates `op` against the stored map, then pends it and discards the
    /// redo stack. Returns the region it changes and renders nothing; on
    /// `Err` nothing changed.
    pub fn append(&mut self, op: Op) -> Result<Option<Rect>, RetouchError> {
        let region = footprint(&op, self.info())?;
        self.pending.push(op);
        self.undone.clear();
        Ok(region)
    }

    /// Steps back one pending op, returning its region; `None` with nothing
    /// to undo.
    pub fn undo(&mut self) -> Option<Option<Rect>> {
        let op = self.pending.pop()?;
        let region =
            footprint(&op, self.info()).expect("a pending op passed footprint when appended");
        self.undone.push(op);
        Some(region)
    }

    /// Pends the most recently undone op again, returning its region; `None`
    /// with nothing to redo.
    pub fn redo(&mut self) -> Option<Option<Rect>> {
        let op = self.undone.pop()?;
        let region =
            footprint(&op, self.info()).expect("an undone op passed footprint when appended");
        self.pending.push(op);
        Some(region)
    }

    fn render_input(&self) -> RenderInput {
        RenderInput {
            base: Arc::clone(&self.base),
            sources: Arc::clone(&self.sources),
            registration: self.registration.clone(),
            info: self.info(),
            samples_per_pixel: self.sources.frame(0).samples_per_pixel as usize,
        }
    }

    /// Every op the edited image shows over `base`: the row's chain, then the
    /// pending ops, oldest first.
    pub fn stack_ops(&self) -> Vec<Op> {
        self.chain.iter().chain(&self.pending).cloned().collect()
    }

    /// Every op the edited image shows, including those already in `base`:
    /// the base ops, then [`Self::stack_ops`].
    pub fn drawn_ops(&self) -> Vec<Op> {
        self.base_ops
            .iter()
            .cloned()
            .chain(self.stack_ops())
            .collect()
    }

    /// Makes the pending ops part of the chain and forgets the redo stack, so
    /// nothing is left to undo or redo.
    pub fn rebase(&mut self) {
        self.chain.append(&mut self.pending);
        self.undone.clear();
    }

    /// Renders `job` on the calling thread straight into the resident tiles.
    /// The open-time replay uses it; a live edit goes through
    /// [`Self::queue_recomposite`].
    pub fn recomposite(&mut self, job: &Recomposite) -> Result<(), String> {
        let input = self.render_input();
        render_region(&input, job, &mut self.tiles, &mut self.snapshot).map(|_| ())
    }

    fn enqueue(&mut self, job: Recomposite) {
        self.recomposites.queued.push(job);
        self.tiles.touch();
    }

    /// Queues `job` for a worker and bumps the tiles' generation, so the
    /// repaint picks the region up. Jobs queued while a worker holds a batch
    /// go out together when it is installed, rendered in queue order; `wake`
    /// runs on the worker once a batch is parked.
    pub fn queue_recomposite(&mut self, job: Recomposite, wake: &RecompositeWake) {
        self.enqueue(job);
        if !self.recomposites.in_flight {
            self.dispatch_recomposites(wake);
        }
    }

    /// Ends a drag: queues every resident tile `touched` meets, each
    /// re-rendered from its pre-drag samples with `ops` blended over them;
    /// the last job drops the pre-drag samples.
    pub fn end_drag(&mut self, touched: &[Rect], ops: Vec<Op>, wake: &RecompositeWake) {
        let tiles = self.tiles.tile_rects(touched);
        let last = tiles.len().saturating_sub(1);
        for (at, region) in tiles.into_iter().enumerate() {
            self.enqueue(Recomposite {
                region,
                pre: PreImage::Snapshot,
                ops: ops.clone(),
                ends_drag: at == last,
            });
        }
        if !self.recomposites.in_flight {
            self.dispatch_recomposites(wake);
        }
    }

    /// Whether no stroke is live and no recomposite is queued or in flight,
    /// so the resident tiles hold every edit.
    pub fn is_settled(&self) -> bool {
        self.stroke.is_none() && self.recomposites.queued.is_empty() && !self.recomposites.in_flight
    }

    /// Installs the batch a worker has parked, in the order its jobs were
    /// queued, then dispatches whatever queued meanwhile. A region whose
    /// render failed is logged and leaves its tiles as they were. Returns
    /// `true` once nothing is in flight.
    pub fn settle_recomposite(&mut self, wake: &RecompositeWake) -> bool {
        let parked = self
            .recomposites
            .parked
            .lock()
            .expect("no worker panicked while parking its recomposites")
            .take();
        if let Some(parked) = parked {
            self.recomposites.in_flight = false;
            match parked.snapshot {
                Some(snapshot) => self.snapshot = snapshot,
                None if parked.ends_drag => self.snapshot.clear(),
                None => {}
            }
            for (region, result) in parked.rendered {
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

    /// Hands every queued job to one worker thread, with the resident tiles
    /// they meet as they are now, and marks the batch in flight.
    fn dispatch_recomposites(&mut self, wake: &RecompositeWake) {
        if self.recomposites.queued.is_empty() {
            return;
        }
        let jobs: Vec<Recomposite> = self.recomposites.queued.drain(..).collect();
        let regions: Vec<Rect> = jobs.iter().map(|job| job.region).collect();
        let (mut local, mut snapshot) = (self.tiles.subset(&regions), self.snapshot.clone());
        let ends_drag = jobs.iter().any(|job| job.ends_drag);
        let input = self.render_input();
        self.recomposites.in_flight = true;
        let parked = Arc::clone(&self.recomposites.parked);
        let wake = Arc::clone(wake);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                jobs.iter()
                    .map(|job| {
                        let samples = render_region(&input, job, &mut local, &mut snapshot);
                        (job.region, samples)
                    })
                    .collect::<Rendered>()
            }));
            let parked_result = match result {
                Ok(rendered) => Parked {
                    rendered,
                    snapshot: Some(snapshot),
                    ends_drag,
                },
                Err(_) => Parked {
                    rendered: jobs
                        .iter()
                        .map(|job| (job.region, Err("the recomposite worker panicked".into())))
                        .collect(),
                    snapshot: None,
                    ends_drag,
                },
            };
            *parked
                .lock()
                .expect("no worker panicked while parking its recomposites") = Some(parked_result);
            wake();
        });
    }
}

/// Builds an [`EditState`] over the whole map: `composite` is the image edits
/// blend over, the resident cache is gridded at `tile_size` px, `base_ops`
/// are the ops `composite` already shows (oldest first), and `chain` (a row's
/// ops, oldest first) is replayed over it and kept as the state's chain. An
/// op that cannot be applied ends the replay; a region that cannot be
/// rendered is skipped.
// The inputs are independent; none of them groups naturally with another.
#[allow(clippy::too_many_arguments)]
pub fn open_edit_session(
    zmap: &Path,
    composite: &Path,
    frames: &[PathBuf],
    registration: Registration,
    tile_size: u32,
    budget: HostBudget,
    base_ops: Vec<Op>,
    chain: Vec<Op>,
) -> Result<EditState, EditSessionError> {
    let reader = ZMapReader::open(zmap)?;
    let info = reader.info();
    // Each plane is mapped on its first read; the mapping outlives the file,
    // which a save removes together with the row's other held files.
    let pixel = Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    };
    reader.read_tile_into(pixel, &mut [0u16; 1])?;
    let base = Arc::new(TiffStack::open_files(&[composite.to_path_buf()])?);
    let mut first = vec![0u16; base.header(0).samples_per_pixel as usize];
    base.read_tile_into(0, pixel, &mut first)?;
    let sources: Arc<dyn FrameReader> = Arc::from(open_frame_set(frames, budget)?.reader);
    let samples_per_pixel = sources.frame(0).samples_per_pixel;
    let tiles = crate::canvas::ResidentTiles::new(
        tile_size,
        info.width(),
        info.height(),
        samples_per_pixel,
    );
    let mut edit = EditState {
        pending: Vec::new(),
        undone: Vec::new(),
        zmap: reader,
        sources,
        registration,
        base,
        composite: composite.to_path_buf(),
        base_ops,
        chain: Vec::new(),
        snapshot: tiles.subset(&[]),
        tiles,
        stroke: None,
        recomposites: Recomposites::default(),
    };
    for op in chain {
        match footprint(&op, info) {
            Ok(region) => {
                edit.chain.push(op);
                if let Some(region) = region {
                    let job = Recomposite {
                        region,
                        pre: PreImage::Base,
                        ops: edit.chain.clone(),
                        ends_drag: false,
                    };
                    if let Err(error) = edit.recomposite(&job) {
                        tracing::error!(%error, "restored stroke recomposite failed");
                    }
                }
            }
            Err(err) => {
                tracing::error!(error = %err, "restored stroke could not be applied");
                break;
            }
        }
    }
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
    /// What this run's alignment did, `Some` for a completed run.
    pub alignment: Option<AlignSummary>,
    pub operation: Option<FailedOperation>,
}

/// What one completed run's alignment did: where its transforms came from,
/// which backend solved them, how each frame ended up, and why a whole-stage
/// rerun happened, if one did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlignSummary {
    /// `computed`, `cached`, `resumed` or `skipped`.
    pub source: String,
    /// `cpu`, or the adapter's display when a level of the solve ran on it.
    pub backend: String,
    pub converged: usize,
    pub hit_cap: usize,
    pub stalled: usize,
    /// Frames fused as read because registration failed.
    pub unaligned: usize,
    pub rerun: Option<String>,
}

impl AlignSummary {
    fn of(report: &FuseReport, observer: &GuiObserver) -> Self {
        let adapter = observer.align_adapter.get().and_then(|cell| cell.get());
        let mut summary = AlignSummary {
            source: report.align_source.clone(),
            backend: match adapter {
                Some(adapter) if report.align_backend_used => adapter.clone(),
                _ => "cpu".to_string(),
            },
            converged: 0,
            hit_cap: 0,
            stalled: 0,
            unaligned: 0,
            rerun: observer.rerun_reason(),
        };
        for frame in report.align.iter().flat_map(|a| &a.alignment.frames) {
            match frame.status {
                AlignStatus::Converged => summary.converged += 1,
                AlignStatus::HitCap => summary.hit_cap += 1,
                AlignStatus::Stalled => summary.stalled += 1,
                AlignStatus::Diverged(_) => summary.unaligned += 1,
                _ => {}
            }
        }
        summary
    }

    /// The one line a run logs.
    pub fn line(&self) -> String {
        let rerun = self
            .rerun
            .as_ref()
            .map(|reason| format!(", reran on the cpu: {reason}"))
            .unwrap_or_default();
        format!(
            "alignment {} on {}: {} converged, {} hit cap, {} stalled, {} unaligned{rerun}",
            self.source, self.backend, self.converged, self.hit_cap, self.stalled, self.unaligned
        )
    }
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
pub fn settle(
    result: std::thread::Result<Result<FuseOutcome, PipelineError>>,
    observer: &GuiObserver,
) -> Settled {
    match result {
        Ok(Ok(FuseOutcome::Completed(report))) => {
            let alignment = AlignSummary::of(&report, observer);
            tracing::info!("{}", alignment.line());
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
                    unaligned: alignment.unaligned,
                },
                held: Some(held),
                diverged,
                alignment: Some(alignment),
                operation: None,
            }
        }
        Ok(Ok(FuseOutcome::Cancelled { .. })) => Settled {
            state: RunState::Idle,
            held: None,
            diverged: Vec::new(),
            alignment: None,
            operation: None,
        },
        Ok(Err(err)) => {
            let operation = FailedOperation::from(&err);
            Settled {
                state: RunState::Failed(err.to_string()),
                held: None,
                diverged: Vec::new(),
                alignment: None,
                operation: Some(operation),
            }
        }
        Err(_) => Settled {
            state: RunState::Failed("the fuse worker stopped unexpectedly".to_string()),
            held: None,
            diverged: Vec::new(),
            alignment: None,
            operation: Some(FailedOperation::Fusion),
        },
    }
}

/// Compacts the project at `path` when most of it is dead data; a failure
/// leaves the project as it was and is only logged.
pub fn compact_quietly(path: &Path) {
    if let Err(err) = project::container::compact_if_mostly_dead(path) {
        tracing::warn!(error = %err, path = %path.display(), "the project could not be compacted");
    }
}

/// A thread compressing one held composite into a blob beside it.
pub type Compression = std::thread::JoinHandle<Result<project::blob::StagedBlob, ProjectError>>;

pub fn spawn_compression(composite: PathBuf) -> Compression {
    std::thread::spawn(move || {
        let dir = composite
            .parent()
            .expect("a held composite sits in its own directory");
        project::blob::stage_plane(
            &composite,
            project::Plane::Composite,
            project::blob::ZSTD_LEVEL,
            dir,
        )
    })
}

/// Joins `handles` and gives each finished blob to the fused entry whose
/// composite it was made from. A failed compression, or a blob whose file is
/// gone, leaves its entry for the save to compress itself.
pub fn attach_compressed(entries: &mut [InMemoryCheckpoint], handles: Vec<(PathBuf, Compression)>) {
    for (composite, handle) in handles {
        let blob = match handle.join() {
            Ok(Ok(blob)) if blob.path.is_file() => blob,
            Ok(Ok(_)) => continue,
            Ok(Err(err)) => {
                tracing::warn!(error = %err, composite = %composite.display(), "the composite could not be compressed in the background");
                continue;
            }
            Err(_) => {
                tracing::warn!(composite = %composite.display(), "the composite compression panicked");
                continue;
            }
        };
        for entry in entries.iter_mut() {
            if let InMemoryKind::Fused(held) = &mut entry.kind
                && held.composite == composite
            {
                held.compressed_composite = Some(Box::new(blob.clone()));
            }
        }
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
