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
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::{COMPOSITE_FILENAME, ZMAP_FILENAME};
use focusforge_core::params::ParamValues;
use focusforge_core::pipeline::{
    self, AlignRequest, FuseOutput, FuseProduct, FuseRequest, RunStage, StageObserver,
};
use focusforge_core::project::{self, Plane, Project, ProjectError};
use focusforge_core::registry::{self, LookupError};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapError, ZMapReader};

/// Re-exported so `crate::controller` -- which names no algorithm crate --
/// can hold and drive these through the `job::` path alone.
pub use focusforge_core::job::{ControlState, JobControl, JobProgress, JobStage};
pub use focusforge_core::pipeline::{FuseOutcome, PipelineError};
pub use focusforge_core::project::HeldCheckpoint;
pub use focusforge_core::retouch::EditSession;

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
            if timeout.timed_out() && control.state() == ControlState::CancelRequested {
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

/// `unix_secs` as `"HH:MM"` UTC -- no date crate in this workspace, and a
/// checkpoint's own creation time never needs more than the clock face.
pub fn format_utc_hhmm(unix_secs: i64) -> String {
    let secs_of_day = unix_secs.rem_euclid(86_400);
    format!("{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60)
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

/// Materializes `project`'s composite and Z-Map into its own plane cache
/// directory ([`plane_cache_dir`]), skipping either file already newer than
/// the project's own `.ffp` -- a reopened, previously-materialized project
/// costs nothing.
pub fn materialize_planes(
    project: &mut Project,
    scratch_root: &Path,
) -> Result<(PathBuf, PathBuf), PlaneCacheError> {
    let dir = plane_cache_dir(scratch_root, project.path());
    std::fs::create_dir_all(&dir)?;
    let composite = dir.join(COMPOSITE_FILENAME);
    let zmap = dir.join(ZMAP_FILENAME);
    let ffp_modified = std::fs::metadata(project.path())?.modified()?;
    if !is_fresh(&composite, ffp_modified) {
        project::materialize_plane(project, Plane::Composite, &composite)?;
    }
    if !is_fresh(&zmap, ffp_modified) {
        project::materialize_plane(project, Plane::ZMap, &zmap)?;
    }
    Ok((composite, zmap))
}

/// Opens `zmap` and builds an [`EditSession`] over its whole map -- the same
/// open-then-read sequence the `retouch` CLI subcommand uses, but over every
/// pixel rather than an op list's footprint.
pub fn open_edit_session(zmap: &Path) -> Result<EditSession, ZMapError> {
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
    Ok(EditSession::new(info, tile, indices)
        .expect("a whole-map tile always fits its own map and its own tile_len"))
}

/// Writes `held`, in order, as one checkpoint chain at `target` -- onto the
/// project already there when `onto_existing` -- and opens the result. On
/// failure `target` is left exactly as it was.
pub fn save_held(
    target: &Path,
    held: &[HeldCheckpoint],
    onto_existing: bool,
) -> Result<Project, ProjectError> {
    project::seal_held(target, held, onto_existing)?;
    Project::open(target)
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
                    at: format_utc_hhmm(held.created_at),
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
