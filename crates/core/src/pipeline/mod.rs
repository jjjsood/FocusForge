// SPDX-License-Identifier: GPL-3.0-or-later

//! The align -> fuse -> seal spine: opens a stack, resolves its two
//! algorithm schemas, aligns it, tiles and fuses it under
//! [`Job`](crate::job::Job), and seals the result into a project or holds
//! it for a caller to seal later.
//! `focusforge-cli` and the GUI are its only two callers, so a stack run
//! means exactly one thing regardless of which front end started it.
//!
//! Presentation reaches this module only as a
//! [`StageObserver`](crate::pipeline::StageObserver) trait object: this
//! module never prints, never depends on a terminal or a window, and the
//! poll thread it owns calls the observer, never a `println!`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::algorithm::{AlgorithmError, AlignAlgorithm, StackAlgorithm, TileReader};
use crate::align::{
    AlignDriverOptions, FailurePolicy, FrameAlignment, ReferencePolicy, Registration,
    StackAlignment, Transform, WarpingReader, align_stack_with, auto_crop,
};
use crate::equalize::{self, EqualizationReport, EqualizeSettings, GainReader};
use crate::fuse::description::format_composite_description;
use crate::fuse::{BackendFusionConsumer, FusionConsumer, FusionGeometry};
use crate::gpu::{AlignBackend, TileBackend};
use crate::io::{
    COMPOSITE_FILENAME, FrameDesc, FrameReader, ReaderKind, Rect, TiffStack, ZMAP_FILENAME,
    open_frame_set,
};
use crate::job::{
    AlignIdentity, ControlState, FrameFingerprint, Job, JobControl, JobOutcome, JobProgress,
    JobRecord, JobStage, RecordingConsumer, Resumable, ResumeStore, StackFingerprint, StageCache,
    StageKey, read_alignment,
};
use crate::params::{self, ParamValue, ParamValues, Provenance, ResolvedParams};
use crate::project::{
    self, AlignRecord, AppendRequest, HeldCheckpoint, Metrics, SealRequest, SealedProject,
};
use crate::tile::{NoopPreprocess, TileConsumer, TileError, TileScheduler, TileSchedulerOptions};
use crate::timing::{Stage, StageTimer, StageTimings, equalize_scope};

/// An alignment request: which algorithm, its explicit parameters, and
/// whether a frame that fails to converge is refused (`Strict`) or falls
/// back to the identity transform (`IdentityFallback`).
pub struct AlignRequest {
    pub algorithm: &'static dyn AlignAlgorithm,
    pub explicit: ParamValues,
    pub lax: bool,
}

/// Everything one align-fuse-seal run needs, owned so it can cross a thread
/// boundary. `frames` is the source list in frame order; `explicit` carries
/// only the caller's own overrides, resolved once inside [`run`] against the
/// [`StackContext`](crate::params::StackContext) built from `frames` itself -- never against a context
/// the caller computed earlier, which could have drifted from what actually
/// gets opened.
pub struct FuseRequest {
    pub frames: Vec<PathBuf>,
    pub output: FuseOutput,
    pub scratch_root: PathBuf,
    /// Names this run's staging directory
    /// (`<scratch_root>/projects/<staging_key>`); the caller picks a key
    /// stable across a resume of the same run and unique across concurrent
    /// ones.
    pub staging_key: String,
    pub algorithm: &'static dyn StackAlgorithm,
    pub explicit: ParamValues,
    pub align: Option<AlignRequest>,
    pub crop: bool,
    /// The brightness equalization to run before alignment; `None` leaves the
    /// frames as delivered.
    pub equalize: Option<EqualizeSettings>,
    pub tile_size: Option<u32>,
    pub max_host_memory: Option<u64>,
    pub backend: Option<&'static dyn TileBackend>,
    /// An accelerator the align stage may offer its large ECC solves to.
    pub align_backend: Option<&'static dyn AlignBackend>,
    pub resume: bool,
    pub cache: bool,
    /// How often the poll thread this module owns calls
    /// [`StageObserver::tick`].
    pub poll: Duration,
}

/// Where a completed run's result goes.
pub enum FuseOutput {
    /// Seal a fresh project at this path, replacing any file there.
    Seal(PathBuf),
    /// Write onto the existing project at this path as a new checkpoint.
    /// Existence at a path is deliberately not the trigger: `focusforge-cli
    /// stack` over a folder that already holds a project keeps overwriting
    /// it rather than silently growing a chain.
    Append(PathBuf),
    /// Write no project: move the composite and Z-Map into this directory
    /// and report them as a [`HeldCheckpoint`] a caller seals later.
    Hold(PathBuf),
}

/// What a completed run left behind, matching its [`FuseOutput`].
pub enum FuseProduct {
    Sealed(SealedProject),
    Held(HeldCheckpoint),
}

/// How [`run`] ended: either the whole job completed and sealed, or a
/// cancel landed -- always at a fuse-unit boundary, never mid-tile.
pub enum FuseOutcome {
    Completed(Box<FuseReport>),
    Cancelled {
        units_done: usize,
        unit_total: usize,
    },
}

/// The alignment half of [`FuseReport`], present only when the request
/// carried an [`AlignRequest`].
pub struct FuseAlignReport {
    pub algorithm_id: String,
    pub display_name: String,
    pub params: ResolvedParams,
    pub failure_policy: FailurePolicy,
    pub alignment: StackAlignment,
}

/// Everything a completed run produced, returned as data so every front end
/// builds its own report from one source.
pub struct FuseReport {
    pub product: FuseProduct,
    pub frame_count: usize,
    pub frames: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    /// The intensity encoding fusion operates on, as delivered by the reader
    /// (see [`working_space`]); no conversion happens.
    pub working_space: &'static str,
    pub stack_params: ResolvedParams,
    pub tile_size: u32,
    pub levels: u32,
    pub margin: u32,
    pub tiles: usize,
    pub frame_tiles_read: usize,
    pub in_flight_limit: usize,
    pub max_bytes_in_flight: u64,
    /// The bytes the scheduler accounted for at its in-flight limit:
    /// `in_flight_limit` tiles, each counted with the method's working bytes
    /// and reader scratch.
    pub accounted_bytes_in_flight: u64,
    pub budget_provenance: Provenance,
    pub backed_off: bool,
    /// Why the run fused on the host although the request carried a backend:
    /// the backend's own account of the tile that did not fit its device.
    /// `None` when the backend ran, or the request carried none.
    pub gpu_fallback: Option<String>,
    pub output_width: u32,
    pub output_height: u32,
    pub crop: Option<Rect>,
    pub description: Option<String>,
    pub align: Option<FuseAlignReport>,
    /// The brightness equalization's estimate; `None` when it was off.
    pub equalization: Option<EqualizationReport>,
    pub align_cached: bool,
    pub align_source: String,
    pub align_workers: Option<usize>,
    pub peak_align_workers: Option<usize>,
    /// The bytes the align stage accounted for (shared reference data plus
    /// one registration per worker); `None` when no alignment was computed.
    pub align_accounted_bytes: Option<u64>,
    /// Whether an accelerator ran at least one level of the alignment this
    /// run computed; `false` for a cached, resumed or skipped alignment and
    /// for a stage that reran on the CPU.
    pub align_backend_used: bool,
    pub stages: StageTimings,
    pub wall_seconds: f64,
    pub metrics: Metrics,
}

/// The intensity encoding of the code values a reader delivers: RAW frames
/// are developed to sRGB-encoded values; TIFF frames are 16-bit unsigned
/// samples whose transfer function is not read from the file.
pub fn working_space(kind: ReaderKind) -> &'static str {
    match kind {
        ReaderKind::Raw => "encoded-srgb",
        ReaderKind::Mmap | ReaderKind::Decoded => "encoded-unknown-transfer",
    }
}

/// What one run's align stage carries the moment it starts, reported once
/// through [`StageObserver::stage`] before any frame is registered. A
/// computed alignment's own per-frame completion count follows through every
/// [`StageObserver::tick`] afterwards, on [`JobProgress`]'s `align_done`/
/// `align_total`.
pub enum RunStage {
    /// Brightness preparation of `frames` sources, before alignment.
    Prepare {
        frames: usize,
    },
    Align {
        frames: usize,
        source: AlignSource,
    },
    Fuse,
    Seal,
    Finished,
}

/// How the align stage is about to be satisfied -- named so a front end can
/// render "reused from the stage cache" without inspecting a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignSource {
    Computed,
    Cached,
    Resumed,
}

/// A front end's presentational hooks into [`run`]. Every method has a
/// no-op default, which is what a headless caller (a test, or the GUI's own
/// `--test job` path) runs on. `Sync`: the poll thread this module owns
/// calls these methods from a thread other than the caller's.
pub trait StageObserver: Sync {
    fn stage(&self, _stage: RunStage) {}
    fn tick(&self, _progress: &JobProgress) {}
    /// Called once a fuse segment reports [`JobOutcome::Paused`], or once
    /// per pause held at an align-stage frame boundary; blocks until the
    /// front end resumes (`control.clear()`) or cancels (`control.cancel()`)
    /// -- `focusforge-cli`'s own signal wait and the GUI's condvar park are
    /// both implementations of this one method. Always called from the
    /// thread that called [`run`], never concurrently with itself.
    fn paused(&self, _control: &JobControl) {}
    /// A human-readable warning raised mid-run that a front end may surface
    /// however it sees fit -- `focusforge-cli`'s own implementation mirrors
    /// it into the diagnostics log; `core` itself never depends on that
    /// crate and never decides how (or whether) a message is displayed.
    fn notice(&self, _message: &str) {}
    /// A [`Self::notice`] about one source frame, `frame` being its position
    /// in the request's frame list. Defaults to the plain notice, so an
    /// observer that ignores the index prints the same text.
    fn frame_notice(&self, _frame: usize, message: &str) {
        self.notice(message);
    }
}

/// The tail of the [`StageObserver::notice`] raised when the align stage
/// reran on the CPU after its accelerator failed; the text before it is the
/// reason.
pub const ALIGN_RERUN_NOTICE: &str = "; the align stage reran on the CPU";

/// Every early exit from the spine, typed by the stage that raised it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PipelineError {
    #[error("{0}")]
    Input(String),
    #[error("{0}")]
    Params(String),
    #[error(transparent)]
    Algorithm(#[from] AlgorithmError),
    #[error("{0}")]
    Align(String),
    #[error(transparent)]
    Tile(#[from] TileError),
    #[error(transparent)]
    StageCache(#[from] crate::job::StageCacheError),
    #[error(transparent)]
    Resume(#[from] crate::job::ResumeError),
    #[error("{0}")]
    ResumeMismatch(String),
    #[error(transparent)]
    Project(#[from] project::ProjectError),
    #[error("{0}")]
    Io(String),
}

/// Runs one stack from `request.frames` to the result `request.output`
/// names: opens the stack and checks uniformity, resolves
/// both schemas, aligns (ledger peek, then `stage_cache`, then a genuine
/// solve), crops, builds the reader and the tile scheduler, stages under
/// `<scratch_root>/projects/<staging_key>`, opens the resume ledger, runs
/// [`Job`] to completion over `control` and seals or holds. `control` is built by
/// the caller **before** this call, so [`JobControl::set_stage`] covers both
/// the align and the fuse stage with one object.
pub fn run(
    request: &FuseRequest,
    observer: &dyn StageObserver,
    control: Arc<JobControl>,
    stage_cache: Option<&StageCache>,
) -> Result<FuseOutcome, PipelineError> {
    let start = Instant::now();
    let mut timer = StageTimer::from_start(start);
    timer.enter(Stage::Setup);

    let mut options = TileSchedulerOptions::new(&request.scratch_root);
    options.budget = options.budget.capped_to(request.max_host_memory);
    let opened = open_frame_set(&request.frames, options.budget)
        .map_err(|err| PipelineError::Input(err.to_string()))?;
    // Hashes the sources one file at a time while the fusion reads them; the
    // seal joins it, so the hashes are in the memo by the time it needs them.
    // A failed hash is left for the seal to hit and report.
    let source_hashes = {
        let frames = request.frames.clone();
        std::thread::spawn(move || {
            for frame in &frames {
                let _ = project::source_hash(frame, None);
            }
        })
    };
    options.budget = opened.scheduler_budget;
    let frame_reader: &dyn FrameReader = &*opened.reader;
    let frame_names: Vec<String> = request
        .frames
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    let stack_ctx = frame_reader.stack_context();
    let (width, height) = (stack_ctx.width, stack_ctx.height);
    let spp = stack_ctx.samples_per_pixel;
    let resolved = params::resolve(request.algorithm.params(), &request.explicit, &stack_ctx)
        .map_err(|err| PipelineError::Params(err.to_string()))?;
    let values = params::resolved_values(&resolved);
    let support = request.algorithm.tile_support(&values);

    let fingerprint = StackFingerprint {
        frames: (0..frame_reader.len())
            .map(|i| FrameFingerprint::from(frame_reader.frame(i)))
            .collect(),
    };

    options.apply_support(support);
    options.working_bytes_per_pixel = request.algorithm.working_bytes_per_pixel(&values, spp);
    if let Some(tile_size) = request.tile_size {
        options.tile_size = tile_size;
    }
    if let Some(backend) = request.backend {
        options.tile_size = options.tile_size.min(backend.max_tile_size(spp));
    }

    let equalization = match request.equalize {
        Some(settings) => {
            control.set_stage(JobStage::Prepare);
            observer.stage(RunStage::Prepare {
                frames: frame_reader.len(),
            });
            let estimated = {
                let _prepare = equalize_scope();
                equalize::estimate(
                    frame_reader,
                    frame_reader.len(),
                    (width, height),
                    spp,
                    settings,
                    &|| control.state() == ControlState::CancelRequested,
                )
            };
            let report = match estimated {
                Err(AlgorithmError::Cancelled) => {
                    return Ok(FuseOutcome::Cancelled {
                        units_done: 0,
                        unit_total: 0,
                    });
                }
                other => other?,
            };
            for (frame, notice) in report.notices() {
                observer.frame_notice(frame, &notice);
            }
            Some(report)
        }
        None => None,
    };
    let recorded = equalization.as_ref().map(EqualizationReport::recorded);
    let prepared_gains: &[u32] = recorded.as_ref().map_or(&[], |e| e.gains.as_slice());
    // Every reader of a frame beneath this point -- alignment, host fuse,
    // device upload and the device-loss fallback -- sees the gained samples.
    let gain_reader = GainReader::new(frame_reader, prepared_gains);

    timer.mark(Stage::Setup);

    let mut alignment_result: Option<(
        &'static dyn AlignAlgorithm,
        ResolvedParams,
        FailurePolicy,
        StackAlignment,
    )> = None;
    let mut used_peeked_alignment = false;
    let mut align_cached = false;
    let mut align_source = "skipped".to_string();
    let mut align_workers: Option<usize> = None;
    let mut peak_align_workers: Option<usize> = None;
    let mut align_accounted_bytes: Option<u64> = None;
    let mut align_backend_used = false;

    if let Some(align_req) = &request.align {
        timer.enter(Stage::Align);
        // The control exists before this stage starts (`Job::with_control`'s
        // whole point), so a poller reading `control.progress()` right now
        // sees `stage == Align`.
        control.set_stage(JobStage::Align);

        let align_resolved = params::resolve(
            align_req.algorithm.params(),
            &align_req.explicit,
            &stack_ctx,
        )
        .map_err(|err| PipelineError::Params(err.to_string()))?;
        let align_values = params::resolved_values(&align_resolved);
        let policy = if align_req.lax {
            FailurePolicy::IdentityFallback
        } else {
            FailurePolicy::Strict
        };
        let identity = AlignIdentity::new(
            align_req.algorithm.id(),
            align_values.clone(),
            policy,
            prepared_gains.to_vec(),
        );

        let peeked = if request.resume {
            peek_resumable_alignment(
                &request.scratch_root,
                &fingerprint,
                options.tile_size,
                options.levels,
                request.algorithm.id(),
                &values,
                Some(&identity),
            )
        } else {
            None
        };

        if let Some(alignment) = peeked.flatten() {
            used_peeked_alignment = true;
            align_source = "resumed".to_string();
            alignment_result = Some((align_req.algorithm, align_resolved, policy, alignment));
        } else {
            let stage_key = StageKey::align(&fingerprint, &identity);
            // The sources' content hashes, one frame at a time through the
            // memo the background hash thread fills; a frame that cannot be
            // hashed is a miss, and the seal reports the failure itself.
            let frame_hash = |i: usize| {
                project::source_hash_change_checked(&request.frames[i])
                    .ok()
                    .map(|(_, hash)| hash)
            };
            let cache_hit = if request.cache {
                stage_cache.and_then(|cache| cache.get_alignment(&stage_key, frame_hash))
            } else {
                None
            };

            if let Some(alignment) = cache_hit {
                align_cached = true;
                align_source = "cached".to_string();
                observer.stage(RunStage::Align {
                    frames: frame_reader.len(),
                    source: AlignSource::Cached,
                });
                alignment_result = Some((align_req.algorithm, align_resolved, policy, alignment));
            } else {
                observer.stage(RunStage::Align {
                    frames: frame_reader.len(),
                    source: AlignSource::Computed,
                });
                let frames_done = control.begin_align(frame_reader.len());
                let done = AtomicBool::new(false);
                let poll = request.poll;
                // Span context does not cross onto the watcher thread by
                // itself: the stage span current here is captured and
                // re-entered there, so every event it raises names the
                // stage that drives it.
                let parent = tracing::Span::current();
                let solved = std::thread::scope(|scope| {
                    let watcher = scope.spawn(|| {
                        let _parent = parent.enter();
                        // Pause and cancel both land at `align_stack_with`'s
                        // next frame boundary, after the registrations in
                        // flight finish. While a hold is in effect this
                        // thread stops ticking, so `observer.paused` (run on
                        // the caller's thread, below) is the only reader of
                        // a front end's resume signal during the hold.
                        let mut warned = false;
                        while !done.load(Ordering::SeqCst) {
                            std::thread::sleep(poll);
                            if control.state() == ControlState::PauseRequested {
                                continue;
                            }
                            observer.tick(&control.progress());
                            if !warned && control.state() == ControlState::CancelRequested {
                                observer.notice(
                                    "cancel requested -- stopping alignment once the frames \
                                     in flight finish (Ctrl-C again to abort now)",
                                );
                                warned = true;
                            }
                        }
                    });
                    let result = align_stack_with(
                        align_req.algorithm,
                        stack_ctx,
                        &gain_reader,
                        &align_values,
                        policy,
                        &AlignDriverOptions {
                            budget: options.budget,
                            frames_done: Some(frames_done),
                            control: Some(Arc::clone(&control)),
                            on_pause: Some(&|control| observer.paused(control)),
                            align_backend: request.align_backend,
                        },
                    );
                    done.store(true, Ordering::SeqCst);
                    watcher.join().expect("align watcher thread must not panic");
                    result
                });
                // The driver reports a cancel it read at a frame boundary;
                // one requested after the last registration started lands
                // here, before the alignment reaches the stage cache.
                if matches!(solved, Err(AlgorithmError::Cancelled))
                    || control.state() == ControlState::CancelRequested
                {
                    return Ok(FuseOutcome::Cancelled {
                        units_done: 0,
                        unit_total: 0,
                    });
                }
                let solved = solved.map_err(|err| PipelineError::Align(err.to_string()))?;
                align_workers = Some(solved.worker_limit);
                peak_align_workers = Some(solved.peak_workers);
                align_accounted_bytes = Some(solved.accounted_bytes);
                align_backend_used = solved.backend_used;
                if let Some(reason) = &solved.rerun_reason {
                    observer.notice(&format!("{reason}{ALIGN_RERUN_NOTICE}"));
                }
                let alignment = solved.alignment;
                if !align_req.lax && alignment.failure_count() > 0 {
                    return Err(PipelineError::Align(describe_align_failure(
                        &alignment,
                        &frame_names,
                    )));
                }
                // A cache write failure costs a missed optimisation next
                // run, never this one -- it must not turn a completed
                // alignment into a failed run.
                if request.cache
                    && let Some(cache) = stage_cache
                    && let Some(content) = (0..request.frames.len())
                        .map(frame_hash)
                        .collect::<Option<Vec<_>>>()
                    && let Err(err) =
                        cache.put_alignment(&stage_key, &request.frames, &content, &alignment)
                {
                    observer.notice(&format!("warning: {err}"));
                }
                align_source = "computed".to_string();
                alignment_result = Some((align_req.algorithm, align_resolved, policy, alignment));
            }
        }
        timer.mark(Stage::Align);
    }
    timer.enter(Stage::Fuse);

    let alignment_for_crop: StackAlignment = match &alignment_result {
        Some((_, _, _, alignment)) => alignment.clone(),
        None => identity_alignment_for_crop(frame_reader.len()),
    };
    let crop_rect_value = auto_crop(&alignment_for_crop, (width, height))
        .map_err(|err| PipelineError::Align(err.to_string()))?;
    if request.crop && crop_rect_value.is_none() {
        return Err(PipelineError::Align(
            "the aligned frames' overlap leaves no interior once the kernel margin is trimmed; \
             turn off auto-crop to keep the full frame extent"
                .to_string(),
        ));
    }
    let applied_crop = if request.crop { crop_rect_value } else { None };

    let description =
        alignment_result
            .as_ref()
            .map(|(align_algo, align_resolved, _, alignment)| {
                let align_values = params::resolved_values(align_resolved);
                let mode = align_values
                    .get("mode")
                    .and_then(ParamValue::as_enum)
                    .unwrap_or("n/a");
                format_composite_description(
                    align_algo.id(),
                    alignment.reference,
                    mode,
                    crop_rect_value.map(|r| (r, request.crop)),
                )
            });

    let transforms: Vec<Transform> = alignment_for_crop
        .frames
        .iter()
        .map(|f| f.transform)
        .collect();
    let registration = Registration {
        transforms: transforms.clone(),
        crop: applied_crop,
        equalization: recorded.clone(),
    };
    let warping_reader = WarpingReader::new(&gain_reader, (width, height), spp, transforms.clone());

    // The scheduler's own per-tile rect: the largest read rect it plans.
    let gpu_fallback = request.backend.and_then(|b| {
        b.tile_budget_shortfall(
            options.tile_size.min(width),
            options.tile_size.min(height),
            spp,
            support.decimation_levels,
            &transforms,
        )
    });
    if let Some(message) = &gpu_fallback {
        observer.notice(message);
    }
    let backend = request.backend.filter(|_| gpu_fallback.is_none());

    let reader: &dyn TileReader = match &backend {
        Some(_) => &gain_reader,
        None => &warping_reader,
    };
    let scheduler = TileScheduler::new(reader, stack_ctx, spp, &options)?;

    let staging = request
        .scratch_root
        .join("projects")
        .join(&request.staging_key);
    std::fs::create_dir_all(&staging)
        .map_err(|err| PipelineError::Io(format!("creating {}: {err}", staging.display())))?;
    let staged_composite = staging.join(COMPOSITE_FILENAME);
    let staged_zmap = staging.join(ZMAP_FILENAME);

    let geometry = FusionGeometry {
        width,
        height,
        samples_per_pixel: spp,
        frame_count: frame_reader.len() as u32,
        crop: applied_crop,
        description: description.clone(),
    };
    let composite_description = description
        .clone()
        .unwrap_or_else(crate::io::TiffSpec::default_description);
    let backend_id = backend
        .map(|b| b.info().to_string())
        .unwrap_or_else(|| "cpu".to_string());

    let record = JobRecord {
        fingerprint,
        width,
        height,
        samples_per_pixel: spp,
        frame_count: frame_reader.len(),
        tile_size: scheduler.plan().tile_size(),
        levels: scheduler.plan().levels(),
        stack_algorithm: request.algorithm.id().to_string(),
        stack_params: values.clone(),
        backend: backend_id.clone(),
        align: alignment_result
            .as_ref()
            .map(|(align_algo, align_resolved, policy, _)| {
                AlignIdentity::new(
                    align_algo.id(),
                    params::resolved_values(align_resolved),
                    *policy,
                    prepared_gains.to_vec(),
                )
            }),
        crop: applied_crop,
        equalization: recorded.clone(),
        composite_output: staged_composite.clone(),
        zmap_output: Some(staged_zmap.clone()),
    };
    let resume_store = ResumeStore::open(&request.scratch_root)?;
    let ledger = if request.resume {
        match resume_store.find(&record)? {
            Resumable::None => {
                return Err(PipelineError::ResumeMismatch(format!(
                    "no resumable state for this stack under {}",
                    request.scratch_root.join("resume").display()
                )));
            }
            Resumable::Mismatch(mismatch) => {
                return Err(PipelineError::ResumeMismatch(mismatch.to_string()));
            }
            Resumable::Ledger(ledger) => ledger,
        }
    } else {
        if let Resumable::Ledger(stale) = resume_store.find(&record)? {
            stale
                .discard()
                .map_err(|err| PipelineError::Io(err.to_string()))?;
        }
        resume_store.begin(&record)?
    };

    if !used_peeked_alignment
        && let Some((_, _, _, alignment)) = &alignment_result
        && let Err(err) = ledger.record_alignment(alignment)
    {
        cleanup_staging(&staging);
        let _ = ledger.discard();
        return Err(PipelineError::Io(err.to_string()));
    }

    let is_resuming = !ledger.completed().is_empty();
    let (sink, surviving): (FusionConsumer, Option<Vec<usize>>) = if is_resuming {
        match FusionConsumer::resume(
            scheduler.plan(),
            ledger.completed(),
            request.algorithm,
            values.clone(),
            &staged_composite,
            Some(&staged_zmap),
            geometry.clone(),
        ) {
            Ok((sink, surviving)) => (sink, Some(surviving)),
            Err(err) => {
                cleanup_staging(&staging);
                let _ = ledger.discard();
                return Err(err.into());
            }
        }
    } else {
        match FusionConsumer::create(
            request.algorithm,
            values.clone(),
            &staged_composite,
            Some(&staged_zmap),
            geometry,
        ) {
            Ok(sink) => (sink, None),
            Err(err) => {
                cleanup_staging(&staging);
                let _ = ledger.discard();
                return Err(err.into());
            }
        }
    };

    control.set_stage(JobStage::Fuse);
    // `Job::with_control` is what credits `control` with the plan's real
    // unit/pixel totals (and any already-completed units on a resume);
    // constructing it before `observer.stage` fires means the very first
    // progress snapshot a front end reads already carries the true total,
    // never a transient `0/0`.
    let mut job = Job::with_control(
        &scheduler,
        surviving.as_deref().unwrap_or(&[]),
        Arc::clone(&control),
    );
    observer.stage(RunStage::Fuse);

    let backend_consumer = backend.map(|b| {
        BackendFusionConsumer::new(
            b,
            &gain_reader,
            (width, height),
            transforms,
            spp,
            support.decimation_levels,
            &sink,
        )
    });
    let consumer: &dyn TileConsumer = match &backend_consumer {
        Some(bc) => bc,
        None => &sink,
    };
    let recording = RecordingConsumer {
        inner: consumer,
        ledger: &ledger,
    };

    let outcome = match drive_job(&mut job, &control, &recording, observer, request.poll) {
        Ok(outcome) => outcome,
        Err(err) => {
            cleanup_staging(&staging);
            let _ = ledger.discard();
            return Err(err.into());
        }
    };

    if outcome == JobOutcome::Cancelled {
        let progress = control.progress();
        return Ok(FuseOutcome::Cancelled {
            units_done: progress.units_done,
            unit_total: progress.unit_total,
        });
    }
    timer.mark(Stage::Fuse);
    timer.enter(Stage::Finalize);
    debug_assert_eq!(
        outcome,
        JobOutcome::Completed,
        "drive_job never returns Paused"
    );

    let final_progress = control.progress();
    let in_flight_limit = scheduler.in_flight_limit();
    let tiles = final_progress.unit_total;
    let frame_tiles_read = match &backend_consumer {
        Some(consumer) => consumer.fallback_tiles() * frame_reader.len(),
        None => tiles * frame_reader.len(),
    };

    if let Err(err) = sink.finish() {
        cleanup_staging(&staging);
        let _ = ledger.discard();
        return Err(err.into());
    }

    observer.stage(RunStage::Seal);
    let _ = source_hashes.join();
    let metrics = measure_metrics(&staged_composite, frame_reader, &gain_reader);
    let align_record =
        alignment_result
            .as_ref()
            .map(|(align_algo, align_resolved, _, alignment)| AlignRecord {
                algorithm: align_algo.id().to_string(),
                params: params::resolved_values(align_resolved),
                reference: alignment.reference,
            });
    let product = match &request.output {
        FuseOutput::Append(project_path) => project::append(AppendRequest {
            project_path,
            staged_composite: &staged_composite,
            staged_zmap: &staged_zmap,
            sources: &request.frames,
            algorithm: request.algorithm.id(),
            params: &values,
            align: align_record,
            registration: registration.clone(),
            composite_description,
            label: None,
            metrics,
        })
        .map(FuseProduct::Sealed)
        .map_err(PipelineError::from),
        FuseOutput::Seal(project_path) => project::seal(SealRequest {
            project_path,
            staged_composite: &staged_composite,
            staged_zmap: &staged_zmap,
            sources: &request.frames,
            algorithm: request.algorithm.id(),
            params: &values,
            align: align_record,
            registration: registration.clone(),
            composite_description,
            label: None,
            metrics,
        })
        .map(FuseProduct::Sealed)
        .map_err(PipelineError::from),
        FuseOutput::Hold(dir) => hold(
            dir,
            &staged_composite,
            &staged_zmap,
            HeldCheckpoint {
                composite: dir.join(COMPOSITE_FILENAME),
                zmap: dir.join(ZMAP_FILENAME),
                width: applied_crop.map(|r| r.width).unwrap_or(width),
                height: applied_crop.map(|r| r.height).unwrap_or(height),
                sources: request.frames.clone(),
                algorithm: request.algorithm.id().to_string(),
                params: values.clone(),
                align: align_record,
                registration: registration.clone(),
                composite_description,
                metrics,
                created_at: project::seal::now_unix(),
                compressed_composite: None,
            },
        )
        .map(FuseProduct::Held),
    };
    let product = match product {
        Ok(product) => product,
        Err(err) => {
            cleanup_staging(&staging);
            let _ = ledger.discard();
            return Err(err);
        }
    };
    cleanup_staging(&staging);
    ledger
        .discard()
        .map_err(|err| PipelineError::Io(err.to_string()))?;
    observer.stage(RunStage::Finished);

    timer.mark(Stage::Finalize);
    let wall_seconds = timer.total_seconds();
    let stages = timer.finish();
    let plan = scheduler.plan();

    let align_report =
        alignment_result.map(
            |(align_algo, align_resolved, policy, alignment)| FuseAlignReport {
                algorithm_id: align_algo.id().to_string(),
                display_name: align_algo.display_name().to_string(),
                params: align_resolved,
                failure_policy: policy,
                alignment,
            },
        );

    Ok(FuseOutcome::Completed(Box::new(FuseReport {
        product,
        frame_count: frame_reader.len(),
        frames: frame_names,
        width,
        height,
        samples_per_pixel: spp,
        working_space: working_space(opened.kind),
        stack_params: resolved,
        tile_size: plan.tile_size(),
        levels: plan.levels(),
        margin: plan.margin(),
        tiles,
        frame_tiles_read,
        in_flight_limit,
        max_bytes_in_flight: scheduler.budget().bytes(),
        accounted_bytes_in_flight: in_flight_limit as u64 * scheduler.bytes_per_tile(),
        budget_provenance: scheduler.budget().provenance(),
        backed_off: scheduler.final_in_flight_limit() < in_flight_limit,
        gpu_fallback,
        output_width: applied_crop.map(|r| r.width).unwrap_or(width),
        output_height: applied_crop.map(|r| r.height).unwrap_or(height),
        crop: applied_crop,
        description,
        align: align_report,
        equalization,
        align_cached,
        align_source,
        align_workers,
        peak_align_workers,
        align_accounted_bytes,
        align_backend_used,
        stages,
        wall_seconds,
        metrics,
    })))
}

/// Moves the staged planes into `dir`, where `held` already names them.
fn hold(
    dir: &Path,
    staged_composite: &Path,
    staged_zmap: &Path,
    held: HeldCheckpoint,
) -> Result<HeldCheckpoint, PipelineError> {
    let io =
        |path: &Path, err: std::io::Error| PipelineError::Io(format!("{}: {err}", path.display()));
    std::fs::create_dir_all(dir).map_err(|err| io(dir, err))?;
    std::fs::rename(staged_composite, &held.composite).map_err(|err| io(&held.composite, err))?;
    std::fs::rename(staged_zmap, &held.zmap).map_err(|err| io(&held.zmap, err))?;
    Ok(held)
}

/// Runs `job` to completion, one segment at a time: [`StageObserver::tick`]
/// every [`FuseRequest::poll`] while a segment executes, and
/// [`StageObserver::paused`] (blocking until it returns) whenever a segment
/// reports [`JobOutcome::Paused`] -- the one segment/resume contract every
/// caller drives against, whatever wakes it.
fn drive_job(
    job: &mut Job<'_>,
    control: &Arc<JobControl>,
    consumer: &dyn TileConsumer,
    observer: &dyn StageObserver,
    poll: Duration,
) -> Result<JobOutcome, TileError> {
    loop {
        let outcome = run_segment(job, control, consumer, poll, observer)?;
        match outcome {
            JobOutcome::Completed | JobOutcome::Cancelled => return Ok(outcome),
            JobOutcome::Paused => {
                if control.state() == ControlState::CancelRequested {
                    // A cancel raced the pause within the same segment.
                    continue;
                }
                observer.paused(control);
            }
        }
    }
}

/// Runs exactly one [`Job::run`] segment on a scoped thread, polling
/// `control.progress()` onto `observer.tick` from this thread every `poll`.
fn run_segment(
    job: &mut Job<'_>,
    control: &Arc<JobControl>,
    consumer: &dyn TileConsumer,
    poll: Duration,
    observer: &dyn StageObserver,
) -> Result<JobOutcome, TileError> {
    // Span context does not cross onto the segment thread by itself: the
    // stage span current here is captured and re-entered there, so every
    // event the scheduler raises names the stage that drives it.
    let parent = tracing::Span::current();
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let _parent = parent.enter();
            job.run(&NoopPreprocess, consumer)
        });
        loop {
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(poll);
            observer.tick(&control.progress());
        }
        handle.join().expect("job segment thread must not panic")
    })
}

/// Names the lowest-indexed diverged frame and its divergence reason in
/// plain words. States the total number of diverged frames only when more
/// than one diverged.
fn describe_align_failure(alignment: &StackAlignment, frame_names: &[String]) -> String {
    use crate::algorithm::AlignStatus;

    let mut failed = alignment.failed();
    let first = failed
        .next()
        .expect("caller checks failure_count() > 0 before building this message");
    let AlignStatus::Diverged(reason) = first.status else {
        unreachable!("StackAlignment::failed only yields Diverged entries")
    };
    let name = frame_names
        .get(first.frame)
        .map(String::as_str)
        .unwrap_or("<unnamed frame>");
    let phrase = divergence_phrase(reason);
    match failed.count() {
        0 => format!("{name} failed to align: {phrase}"),
        more => format!(
            "{name} failed to align: {phrase} ({} frames total)",
            more + 1
        ),
    }
}

/// Short, user-facing wording for each way a registration can diverge.
fn divergence_phrase(reason: crate::align::DivergenceReason) -> &'static str {
    use crate::align::DivergenceReason;

    match reason {
        DivergenceReason::SingularHessian => "the frame has too little texture to register",
        DivergenceReason::LemmaOneRegion => "no further step would improve the match",
        DivergenceReason::NonInvertibleIncrement => "the computed correction could not be inverted",
        DivergenceReason::CorrelationDecreased => "the match grew worse instead of better",
        DivergenceReason::TooFewValidSamples => "too few pixels overlapped to measure a match",
        DivergenceReason::NonFiniteUpdate => "the computation produced an invalid result",
        DivergenceReason::ValidationFailed => {
            "the alignment does not hold away from the centre of the frame"
        }
        DivergenceReason::WeakMatch => "the best match found is too weak to be the right one",
    }
}

/// An all-identity [`StackAlignment`] over `frame_count` frames -- what a
/// request with no [`AlignRequest`] feeds to [`auto_crop`] and the
/// [`WarpingReader`], so a crop still trims the kernel-radius margin with
/// alignment off, and the reader stays bit-identical to a direct read
/// either way (`WarpingReader`'s own identity short-circuit).
fn identity_alignment_for_crop(frame_count: usize) -> StackAlignment {
    use crate::algorithm::AlignStatus;
    StackAlignment {
        reference: 0,
        policy: ReferencePolicy::Anchor,
        frames: (0..frame_count)
            .map(|i| FrameAlignment {
                frame: i,
                transform: Transform::identity(),
                status: AlignStatus::Converged,
                iterations: 0,
                residual: 1.0,
                fallback: false,
            })
            .collect(),
    }
}

/// A resumable candidate's already-recorded [`StackAlignment`], read
/// directly off a matching `<scratch_base>/resume/*/job.json` (and that
/// same directory's `alignment.json`, when present) so a resumed run can
/// build a correct [`JobRecord`] without first running [`align_stack_with`].
/// A `None` return costs only a missed optimisation -- the caller falls
/// back to computing alignment normally -- never a wrong resume, since
/// nothing downstream trusts this peek's verdict on its own.
fn peek_resumable_alignment(
    scratch_base: &Path,
    fingerprint: &StackFingerprint,
    tile_size: u32,
    levels: u32,
    stack_algorithm: &str,
    stack_params: &ParamValues,
    align_identity: Option<&AlignIdentity>,
) -> Option<Option<StackAlignment>> {
    let entries = std::fs::read_dir(scratch_base.join("resume")).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("job.json")) else {
            continue;
        };
        let Ok(candidate) = serde_json::from_str::<JobRecord>(&text) else {
            continue;
        };
        if !candidate.composite_output.exists() {
            continue;
        }
        if &candidate.fingerprint != fingerprint {
            continue;
        }
        if candidate.tile_size != tile_size || candidate.levels != levels {
            continue;
        }
        if candidate.stack_algorithm != stack_algorithm {
            continue;
        }
        if &candidate.stack_params != stack_params {
            continue;
        }
        if candidate.align.as_ref() != align_identity {
            continue;
        }
        let alignment = read_alignment(&dir.join("alignment.json")).ok().flatten();
        return Some(alignment);
    }
    None
}

/// Best-effort removal of one run's staging directory and both staged
/// planes. Never fails the caller: a missing directory (the common case) is
/// not itself an error. Called on failure and on success, never on cancel --
/// a cancelled run keeps its staged planes and its ledger so a resume can
/// continue.
fn cleanup_staging(staging: &Path) {
    let _ = std::fs::remove_dir_all(staging);
}

/// The maximum side of the sharpness window [`measure_metrics`] reads --
/// bounded memory, one tile read per source frame.
const LAPVAR_WINDOW: u32 = 2048;

/// The composite's and the sharpest source frame's Laplacian variance over
/// one central window, measured identically on both. Every failure is
/// silence rather than a refusal: a metric that could not be measured must
/// not turn a completed run into a failed one. `gained` reads `sources` with
/// the run's equalization gains applied, so the comparison is against the
/// frames the fuse used.
fn measure_metrics(
    composite: &Path,
    sources: &dyn FrameReader,
    gained: &dyn TileReader,
) -> Metrics {
    let Ok(stack) = TiffStack::open_files(std::slice::from_ref(&composite.to_path_buf())) else {
        return Metrics::default();
    };
    let (width, height) = FrameReader::dimensions(&stack);
    let spp = stack.frame(0).samples_per_pixel;
    let side_w = LAPVAR_WINDOW.min(width);
    let side_h = LAPVAR_WINDOW.min(height);
    let window = centered(width, height, side_w, side_h);
    let composite_lapvar = lapvar(&stack, 0, window, spp);

    let mut max_input_lapvar: Option<f64> = None;
    for index in 0..sources.len() {
        let (fw, fh) = sources.dimensions();
        if fw < side_w || fh < side_h {
            continue;
        }
        let fspp = sources.frame(index).samples_per_pixel;
        let rect = centered(fw, fh, side_w, side_h);
        if let Some(value) = lapvar(gained, index, rect, fspp) {
            max_input_lapvar = Some(max_input_lapvar.map_or(value, |m: f64| m.max(value)));
        }
    }

    Metrics {
        composite_lapvar,
        max_input_lapvar,
        lapvar_window: composite_lapvar.map(|_| window),
    }
}

fn centered(width: u32, height: u32, side_w: u32, side_h: u32) -> Rect {
    Rect {
        x: (width - side_w) / 2,
        y: (height - side_h) / 2,
        width: side_w,
        height: side_h,
    }
}

fn lapvar(reader: &dyn TileReader, frame: usize, rect: Rect, spp: u32) -> Option<f64> {
    let mut samples = vec![0u16; rect.width as usize * rect.height as usize * spp as usize];
    reader.read_tile_into(frame, rect, &mut samples).ok()?;
    Some(crate::fuse::metrics::laplacian_variance(
        &samples,
        rect.width,
        rect.height,
        spp,
    ))
}

/// Compare reader-provided layout fragments and report the outlier against
/// the majority. Ties favor the most-recently-seen group.
pub fn uniform_check(frames: &[(&str, &FrameDesc)]) -> (bool, Option<String>) {
    if frames.len() < 2 {
        return (true, None);
    }

    let mut groups: Vec<(&Vec<String>, usize)> = Vec::new();
    for (_, desc) in frames {
        if let Some(group) = groups
            .iter_mut()
            .find(|(signature, _)| **signature == desc.layout_signature)
        {
            group.1 += 1;
        } else {
            groups.push((&desc.layout_signature, 1));
        }
    }

    if groups.len() == 1 {
        return (true, None);
    }

    let mut majority = groups[0];
    for group in groups.iter().skip(1) {
        if group.1 >= majority.1 {
            majority = *group;
        }
    }
    let majority_sig = majority.0;

    let outlier = frames
        .iter()
        .find(|(_, desc)| &desc.layout_signature != majority_sig)
        .expect("groups.len() > 1 implies at least one non-majority frame");
    let majority_desc = frames
        .iter()
        .find(|(_, desc)| &desc.layout_signature == majority_sig)
        .expect("majority signature came from one of these frames");

    let mut outlier_detail = outlier.1.layout_label.clone();
    for i in 2..outlier.1.layout_signature.len() {
        if outlier.1.layout_signature[i]
            != majority_desc
                .1
                .layout_signature
                .get(i)
                .map(String::as_str)
                .unwrap_or("")
        {
            outlier_detail.push('/');
            outlier_detail.push_str(&outlier.1.layout_signature[i]);
        }
    }

    let detail = format!(
        "{} is {}, others {}",
        outlier.0, outlier_detail, majority_desc.1.layout_label
    );
    (false, Some(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_resumable_alignment_skips_a_ledger_missing_raw_identity() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let composite = scratch.path().join("out.tif");
        std::fs::write(&composite, []).expect("output marker");
        let fingerprint = StackFingerprint {
            frames: vec![FrameFingerprint {
                path: PathBuf::from("frame.tif"),
                file_size: 1,
                image_description: None,
                width: 1,
                height: 1,
                samples_per_pixel: 3,
                strip_offsets: vec![8],
                strip_byte_counts: vec![3],
                raw_identity: None,
            }],
        };
        let record = JobRecord {
            fingerprint: fingerprint.clone(),
            width: 1,
            height: 1,
            samples_per_pixel: 3,
            frame_count: 1,
            tile_size: 64,
            levels: 1,
            stack_algorithm: "test".to_string(),
            stack_params: ParamValues::new(),
            backend: "cpu".to_string(),
            align: None,
            crop: None,
            equalization: None,
            composite_output: composite,
            zmap_output: None,
        };
        let ledger = scratch.path().join("resume").join("candidate");
        std::fs::create_dir_all(&ledger).expect("ledger directory");
        std::fs::write(
            ledger.join("job.json"),
            serde_json::to_vec(&record).expect("record serializes"),
        )
        .expect("job record writes");

        assert_eq!(
            peek_resumable_alignment(
                scratch.path(),
                &fingerprint,
                64,
                1,
                "test",
                &ParamValues::new(),
                None,
            ),
            Some(None)
        );

        let path = ledger.join("job.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("job record reads"))
                .expect("job record is JSON");
        value["fingerprint"]["frames"][0]
            .as_object_mut()
            .expect("frame is an object")
            .remove("raw_identity");
        std::fs::write(path, serde_json::to_vec(&value).expect("record serializes"))
            .expect("job record writes");

        assert_eq!(
            peek_resumable_alignment(
                scratch.path(),
                &fingerprint,
                64,
                1,
                "test",
                &ParamValues::new(),
                None,
            ),
            None
        );
    }

    #[test]
    fn working_space_names_the_transfer_each_reader_delivers() {
        for (kind, expected) in [
            (ReaderKind::Raw, "encoded-srgb"),
            (ReaderKind::Mmap, "encoded-unknown-transfer"),
            (ReaderKind::Decoded, "encoded-unknown-transfer"),
        ] {
            assert_eq!(working_space(kind), expected, "{kind:?}");
        }
    }

    #[test]
    fn validation_failed_has_its_own_phrase() {
        use crate::align::DivergenceReason;

        let reasons = [
            DivergenceReason::SingularHessian,
            DivergenceReason::LemmaOneRegion,
            DivergenceReason::NonInvertibleIncrement,
            DivergenceReason::CorrelationDecreased,
            DivergenceReason::TooFewValidSamples,
            DivergenceReason::NonFiniteUpdate,
            DivergenceReason::ValidationFailed,
            DivergenceReason::WeakMatch,
        ];
        for (i, &a) in reasons.iter().enumerate() {
            for &b in &reasons[i + 1..] {
                assert_ne!(divergence_phrase(a), divergence_phrase(b), "{a:?} {b:?}");
            }
        }
        let phrase = divergence_phrase(DivergenceReason::ValidationFailed);
        assert!(phrase.contains("away from the centre"), "{phrase}");
        let weak = divergence_phrase(DivergenceReason::WeakMatch);
        assert!(weak.contains("too weak"), "{weak}");
    }

    fn desc(label: &str, signature: &[&str]) -> FrameDesc {
        FrameDesc {
            path: label.into(),
            file_size: 0,
            image_description: None,
            width: 1,
            height: 1,
            samples_per_pixel: 1,
            strip_offsets: Vec::new(),
            strip_byte_counts: Vec::new(),
            raw_identity: None,
            layout_label: signature[..2.min(signature.len())].join("/"),
            layout_signature: signature.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn uniform_check_reports_raw_geometry_in_both_majority_directions_and_ties() {
        let raw = desc("raw", &["raw/dng", "output=8x8"]);
        let tiff = desc("tiff", &["II", "rps=1", "compression=1", "8x8"]);
        let raw_other = desc("raw-other", &["raw/dng", "output=4x3"]);
        for (frames, outlier) in [
            (
                vec![("raw", &raw), ("raw-again", &raw), ("tiff", &tiff)],
                "tiff",
            ),
            (
                vec![("tiff", &tiff), ("tiff-again", &tiff), ("raw", &raw)],
                "raw",
            ),
            (vec![("raw", &raw), ("tiff", &tiff)], "raw"),
            (vec![("tiff", &tiff), ("raw", &raw)], "tiff"),
            (vec![("raw", &raw), ("raw-other", &raw_other)], "raw"),
        ] {
            let (uniform, detail) = uniform_check(&frames);
            assert!(!uniform);
            let detail = detail.expect("mixed layouts have a detail");
            assert!(detail.contains(outlier));
            if outlier == "raw" {
                assert!(detail.contains("output="));
            }
        }
    }
}
