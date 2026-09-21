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

use focusforge_core::gpu::TileBackend;
use focusforge_core::io::{COMPOSITE_FILENAME, ZMAP_FILENAME};
use focusforge_core::params::ParamValues;
use focusforge_core::pipeline::{self, AlignRequest, FuseRequest, RunStage, StageObserver};
use focusforge_core::project::{self, Plane, Project, ProjectError};
use focusforge_core::registry;

/// Re-exported so `crate::controller` -- which names no algorithm crate --
/// can hold and drive these through the `job::` path alone.
pub use focusforge_core::job::{ControlState, JobControl, JobProgress, JobStage};
pub use focusforge_core::pipeline::{FuseOutcome, PipelineError};

/// The shell's own view of a fuse run -- the surface's five reachable
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
}

/// Owned data for one fuse run, assembled under the controller lock and
/// moved onto the worker thread -- algorithm ids rather than resolved
/// trait objects, since a `ProjectTab` stores only ids and explicit values.
#[derive(Clone)]
pub struct JobRequest {
    pub frames: Vec<PathBuf>,
    pub project_path: PathBuf,
    pub scratch_root: PathBuf,
    pub staging_key: String,
    pub fuse_id: String,
    pub fuse_values: ParamValues,
    pub align_id: Option<String>,
    pub align_values: ParamValues,
    pub align_lax: bool,
    pub crop: bool,
    pub backend: Option<&'static dyn TileBackend>,
    /// Write onto the tab's already-sealed project as a new checkpoint
    /// instead of sealing a fresh container -- set for a re-fuse of an
    /// already-opened tab, clear for a first fuse.
    pub append: bool,
}

/// How often the pipeline's poll thread ticks progress -- the window's own
/// frame clock, not inherited from any spike measurement.
pub const POLL: Duration = Duration::from_millis(100);

/// Moves `request` onto a worker thread and runs it to completion through
/// `focusforge_core::pipeline::run`, reporting through `observer`. Returns
/// the join handle; the caller is expected to read progress through
/// `observer` rather than by joining eagerly.
pub fn spawn(
    request: JobRequest,
    control: Arc<JobControl>,
    observer: Arc<GuiObserver>,
) -> std::thread::JoinHandle<Result<FuseOutcome, PipelineError>> {
    std::thread::spawn(move || {
        let algorithm = registry::find_stack(&request.fuse_id)
            .expect("the tab's own fuse id is always a registered algorithm");
        let align = request.align_id.map(|id| {
            let algorithm = registry::find_align(&id)
                .expect("the tab's own align id is always a registered algorithm");
            AlignRequest {
                algorithm,
                explicit: request.align_values,
                lax: request.align_lax,
            }
        });
        let fuse_request = FuseRequest {
            frames: request.frames,
            project_path: request.project_path,
            scratch_root: request.scratch_root,
            staging_key: request.staging_key,
            algorithm,
            explicit: request.fuse_values,
            align,
            crop: request.crop,
            tile_size: None,
            max_host_memory: None,
            backend: request.backend,
            resume: false,
            cache: true,
            append: request.append,
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
}

impl GuiObserver {
    pub fn new() -> Self {
        GuiObserver {
            last_progress: Mutex::new(None),
            pause_gate: Mutex::new(false),
            condvar: Condvar::new(),
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

/// The bar's three-step vocabulary, sourced from the run's own phases
/// rather than the engine's two `JobStage` values: `Fuse` is the only step
/// carrying `JobStage::Fuse` units, so no step here is fabricated from
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Align,
    Fuse,
    Write,
}

impl Step {
    pub fn label(self) -> &'static str {
        match self {
            Step::Align => "Align",
            Step::Fuse => "Fuse",
            Step::Write => "Write",
        }
    }

    /// `"step N of 3"`.
    pub fn text(self) -> String {
        let n = match self {
            Step::Align => 1,
            Step::Fuse => 2,
            Step::Write => 3,
        };
        format!("step {n} of 3")
    }
}

/// Which [`Step`] `progress` is on: `Align` while the engine reports that
/// stage, `Write` once every unit of the fuse stage is done (the seal, which
/// the engine reports as no stage of its own -- `JobStage` has only
/// `Align`/`Fuse`), `Fuse` otherwise -- including no progress at all yet.
pub fn progress_step(progress: Option<&JobProgress>) -> Step {
    match progress {
        Some(p) if p.stage == JobStage::Align => Step::Align,
        Some(p) if p.unit_total > 0 && p.units_done >= p.unit_total => Step::Write,
        _ => Step::Fuse,
    }
}

/// The stage word the bar shows for `state`/`progress` -- `Aligning`,
/// `Fusing`, `Writing`, `Paused`, `Cancelling`, `Failed` or a completed run's
/// own `fused HH:MM`. `progress` picks between `Aligning`/`Fusing`/`Writing`
/// while running, via `progress_step`.
pub fn stage_word(state: &RunState, progress: Option<&JobProgress>) -> String {
    match state {
        RunState::Paused => "Paused".to_string(),
        RunState::Cancelling => "Cancelling".to_string(),
        RunState::Failed(_) => "Failed".to_string(),
        RunState::Idle => String::new(),
        RunState::Fused { at } => format!("fused {at}"),
        RunState::Running => match progress_step(progress) {
            Step::Align => "Aligning".to_string(),
            Step::Fuse => "Fusing".to_string(),
            Step::Write => "Writing".to_string(),
        },
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

/// The step this run is on, derived through the same `progress_step`
/// classification [`stage_word`] uses -- never fabricated from a state the
/// engine did not report.
pub fn current_step(state: &RunState, progress: Option<&JobProgress>) -> Option<Step> {
    match state {
        RunState::Idle => None,
        RunState::Paused | RunState::Cancelling | RunState::Failed(_) | RunState::Fused { .. } => {
            None
        }
        RunState::Running => Some(progress_step(progress)),
    }
}

/// `estimating` while `eta` is `None` and no unit has finished yet,
/// `ETA M:SS` while running with a real estimate, `ETA —` once paused,
/// cancelling or failed -- never a rendered zero.
pub fn eta_text(state: &RunState, progress: Option<&JobProgress>) -> String {
    match state {
        RunState::Paused | RunState::Cancelling | RunState::Failed(_) => "ETA —".to_string(),
        RunState::Idle | RunState::Fused { .. } => String::new(),
        RunState::Running => match progress.and_then(|p| p.eta) {
            Some(eta) => {
                let secs = eta.as_secs();
                format!("ETA {}:{:02}", secs / 60, secs % 60)
            }
            None => "estimating".to_string(),
        },
    }
}

/// The bar's fill fraction, `units_done / unit_total` of the whole job --
/// `0.0` before the first unit or with no run at all.
pub fn bar_fraction(progress: Option<&JobProgress>) -> f32 {
    match progress {
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
/// canvas: keyed by a short hash of its own `.ffp` path, since two tabs can
/// share a project name.
pub fn plane_cache_dir(project_path: &Path) -> PathBuf {
    PathBuf::from(".scratch/planes").join(format!(
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
pub fn materialize_planes(project: &mut Project) -> Result<(PathBuf, PathBuf), PlaneCacheError> {
    let dir = plane_cache_dir(project.path());
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

/// Opens a sealed `.ffp` and materializes its planes -- what promotes a tab
/// from `Unsaved` to `Opened` on a completed fuse, and what `--open
/// <saved>.ffp` needs before it can feed the canvas.
pub fn open_and_materialize(path: &Path) -> Result<(Project, PathBuf, PathBuf), PlaneCacheError> {
    let mut project = Project::open(path)?;
    let (composite, zmap) = materialize_planes(&mut project)?;
    Ok((project, composite, zmap))
}

/// One completed worker thread's result, boiled down to the shell's next
/// [`RunState`] and, on a genuine completion, the reopened project plus its
/// two materialized plane paths.
pub struct Settled {
    pub state: RunState,
    pub opened: Option<(Project, PathBuf, PathBuf)>,
}

/// Interprets a joined worker thread's result -- a panic and a pipeline
/// error both become [`RunState::Failed`], a clean cancel returns to
/// [`RunState::Idle`] (the engine already preserved its own resumable state
/// on disk, per its own "reported, never controlled" contract), and a
/// completion re-opens the sealed project and materializes its planes.
pub fn settle(result: std::thread::Result<Result<FuseOutcome, PipelineError>>) -> Settled {
    match result {
        Ok(Ok(FuseOutcome::Completed(report))) => match open_and_materialize(&report.sealed.path) {
            Ok((project, composite, zmap)) => {
                let at = format_utc_hhmm(project.checkpoint().created_at);
                Settled {
                    state: RunState::Fused { at },
                    opened: Some((project, composite, zmap)),
                }
            }
            Err(err) => Settled {
                state: RunState::Failed(err.to_string()),
                opened: None,
            },
        },
        Ok(Ok(FuseOutcome::Cancelled { .. })) => Settled {
            state: RunState::Idle,
            opened: None,
        },
        Ok(Err(err)) => Settled {
            state: RunState::Failed(err.to_string()),
            opened: None,
        },
        Err(_) => Settled {
            state: RunState::Failed("the fuse worker stopped unexpectedly".to_string()),
            opened: None,
        },
    }
}
