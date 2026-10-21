// SPDX-License-Identifier: GPL-3.0-or-later

//! [`align_stack`]: the whole-stack driver an [`AlignAlgorithm`] cannot be,
//! because its per-frame trait carries no state between calls. A **free
//! function**, never a trait method -- adding it to the trait would grow the
//! trait one method per driver concern.
//!
//! # Reference policy
//!
//! `reference-policy` (`anchor` or `chain`) and `reference-frame` (`first`,
//! `middle` or `last`) are read out of the resolved `cfg` as well-known
//! keys with a default when absent, so an algorithm that declares neither
//! still runs, anchored on the middle frame. `Middle` resolves to
//! `(frame_count - 1) / 2`.
//!
//! **Anchor** builds one [`AlignContext`] against the reference, calling `align_frame` for every
//! other frame across a driver-owned rayon pool sized from [`AlignDriverOptions::budget`],
//! built and dropped here so it never overlaps the tile scheduler's pool. **Chain** walks outward
//! both ways, rebuilding an `AlignContext` per hop and composing `T_i = T_hop.compose(&T_previous)`
//! -- hop **after** prefix, since [`Transform::compose`] applies its right operand first -- serial
//! by construction, each hop needing the previous hop's result.
//!
//! ## The reference pyramid is cached once per run
//!
//! `align_stack_with` calls [`AlignAlgorithm::prepare_run`] once, before either sub-arm; the result
//! lives in **this function's own stack frame** as an `Arc`, never as state on the `&'static`
//! algorithm, so overlapping runs hold separate `Arc`s. It reaches every `align_frame` call through
//! [`AlignContext::prepared`], by `&`, safe under this module's rayon fan-out because every worker
//! holds the same shared, already-finished, never-mutated reference. Chain mode never calls
//! `prepare_run`: its reference changes every hop, so nothing stays stable to cache.
//!
//! # Failure policy
//!
//! A frame whose outcome is [`AlignStatus::Diverged`] is a failure. **Both**
//! [`FailurePolicy`] branches produce the same complete report -- only the
//! caller's exit policy differs. Under [`FailurePolicy::Strict`] a failed
//! frame keeps the best-ρ transform the solver returned; under
//! [`FailurePolicy::IdentityFallback`] that frame's transform is replaced
//! by [`Transform::identity`] and `fallback` is set. In chain mode an
//! identity fallback substitutes identity for **that hop only** before
//! composing onto the prefix, rather than inheriting the neighbour's
//! transform outright.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, AlignStatus, TileReader};
use crate::align::Transform;
use crate::align::ecc_algorithm::{
    retained_template_bytes, seed_scratch_bytes, solve_scratch_bytes,
};
use crate::gpu::{AlignBackend, EccMomentSession, EccSessionRequest};
use crate::job::{ControlState, JobControl};
use crate::params::{ParamValue, ParamValues, StackContext};
use crate::tile::control::ADMISSION_RECHECK;
use crate::tile::plan::DEFAULT_TILE_SIZE;
use crate::tile::{HostBudget, TileControl};
use crate::timing::{TimingCounter, measurement_scope, observe_timing_max, record_timing};

/// Whether every frame registers against one anchor frame, or chains
/// hop-by-hop through its neighbours toward the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReferencePolicy {
    Anchor,
    Chain,
}

/// Which frame [`align_stack`] treats as the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReferenceFrame {
    First,
    Middle,
    Last,
}

/// What [`align_stack`] does with a frame whose outcome is
/// [`AlignStatus::Diverged`]. See the module docs' "Failure policy".
///
/// Serialized as `"strict"` / `"identity-fallback"`; part of the
/// alignment cache key [`crate::job::record::AlignIdentity`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailurePolicy {
    Strict,
    IdentityFallback,
}

/// One frame's place in a [`StackAlignment`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameAlignment {
    pub frame: usize,
    pub transform: Transform,
    /// `#[serde(flatten)]`ed: [`AlignStatus`] is itself adjacently tagged
    /// (`{"status":..,"reason":..}`), and flattening it here is what makes
    /// `status`/`reason` siblings of `frame`/`transform`/... on this
    /// struct, rather than a nested `"status": {"status":..}` object one
    /// level down.
    #[serde(flatten)]
    pub status: AlignStatus,
    pub iterations: u32,
    pub residual: f64,
    /// `true` when [`FailurePolicy::IdentityFallback`] substituted identity
    /// for this frame's own (diverged) hop.
    pub fallback: bool,
}

/// The result of one [`align_stack`] run: every frame's outcome, plus which
/// frame was the reference and under which policy.
///
/// Serializable so a resumed run can reuse a recorded alignment instead of
/// calling [`align_stack`] again. `PartialEq` is additive, for the
/// round-trip test comparing a deserialized value against the one that
/// produced it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StackAlignment {
    pub reference: usize,
    pub policy: ReferencePolicy,
    /// One entry per frame, index == frame index.
    pub frames: Vec<FrameAlignment>,
}

impl StackAlignment {
    /// How many frames diverged.
    pub fn failure_count(&self) -> usize {
        self.failed().count()
    }

    /// Every frame whose status is [`AlignStatus::Diverged`].
    pub fn failed(&self) -> impl Iterator<Item = &FrameAlignment> {
        self.frames
            .iter()
            .filter(|f| matches!(f.status, AlignStatus::Diverged(_)))
    }
}

/// The library default alignment budget: 2 GiB, used by
/// [`AlignDriverOptions::default`]. The pipeline and the CLI pass the host
/// budget they resolved instead.
pub const DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT: usize = 2 * (1 << 30);

/// The alignment driver's own options -- the model is
/// `crate::tile::TileSchedulerOptions`, minus its `scratch_base`. Unlike that
/// type it carries a `Default`: an explicit
/// [`DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT`] budget and no control hooks.
pub struct AlignDriverOptions<'a> {
    /// The host memory this driver may account: the shared reference data
    /// once, plus [`bytes_per_registration`] per anchor worker. A
    /// host-derived budget below the shared data plus one registration
    /// refuses the run ([`AlgorithmError::HostMemoryTooSmall`]); any other
    /// budget floors to one worker. See `workers_for_budget` (private to this
    /// module) for the arithmetic.
    pub budget: HostBudget,
    /// Bumped once per frame whose registration has completed, reference
    /// frame included, in completion order -- `None` when the caller has
    /// nowhere to report it.
    pub frames_done: Option<Arc<AtomicUsize>>,
    /// The run's control, read at the top of every frame's registration:
    /// once it reads `CancelRequested`, no further registration starts, the
    /// ones in flight finish, and the run returns
    /// [`AlgorithmError::Cancelled`]. Once it reads `PauseRequested` **and**
    /// [`Self::on_pause`] is set, the same boundary calls the hook and
    /// re-checks after it returns, holding again while the state is still
    /// `PauseRequested`. With no hook a pause is not honoured at all: the
    /// run registers straight through it.
    pub control: Option<Arc<JobControl>>,
    /// Called once per pause, at the same frame boundary as `control`,
    /// **on the thread that called [`align_stack_with`]** -- never from a
    /// pool worker.
    pub on_pause: Option<&'a (dyn Fn(&JobControl) + Sync)>,
    /// An accelerator the registration may offer its large solves to; `None`
    /// solves everything on the CPU.
    pub align_backend: Option<&'a dyn AlignBackend>,
}

impl Default for AlignDriverOptions<'_> {
    fn default() -> Self {
        AlignDriverOptions {
            budget: HostBudget::explicit(DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT as u64),
            frames_done: None,
            control: None,
            on_pause: None,
            align_backend: None,
        }
    }
}

/// The bytes an anchor run holds live once, however many workers there are:
/// the reference's coarse pyramid, the reference templates the solver retains
/// for every level, the off-centre patch templates and the reference's feature
/// set, detected at most once per run.
///
/// ```text
/// plane                   = width * height * 4   // f32 luma
/// coarse                  = plane / 3            // levels 1..N, ratio 1/4 each
/// bytes_shared_reference  = coarse + retained_template_bytes(width, height)
/// ```
///
/// The `1/3` is the infinite geometric sum `1/4 + 1/16 + ...`, so the
/// pyramid figure is an upper bound independent of pyramid depth; level 0 is
/// never resident. At 8192x5464x3 the coarse pyramid is ~59.7 MB, the level
/// templates ~161.4 MB, the patch templates ~33.0 MB and the feature set
/// 0.14 MB, ~254.3 MB together; at the 512x512x3 fixture ~15.3 MB. The
/// templates are priced in rotation-scale mode; a translation run retains
/// less.
pub fn bytes_shared_reference(stack: StackContext) -> u64 {
    let plane = stack.width as u64 * stack.height as u64 * 4;
    plane / 3 + retained_template_bytes(stack.width, stack.height)
}

/// The bytes one anchor-mode registration holds live **on top of** the
/// shared term ([`bytes_shared_reference`]) -- the sibling of
/// the scheduler's per-tile accounting, `pub` because the memory test sizes
/// its budgets from it.
///
/// ```text
/// plane      = width * height * 4                      // f32 luma
/// coarse     = plane / 3                               // the frame's own levels 1..N
/// rows       = band_rows + 5                           // input rows of one band, with its halo
/// stream     = width * rows * (2 * spp + 7)            // u16 band, f32 luma band, reduce passes
/// solve      = solve_scratch_bytes(width, height, spp) // per-solve vectors and windows
/// seed       = seed_scratch_bytes(width, height, spp)  // detector scale space, frame features
/// bytes_per_registration = coarse + max(stream, solve, seed)
/// ```
///
/// `band_rows` is `DEFAULT_TILE_SIZE`, the input band height
/// `PyramidalEcc::build_pyramid` streams with. The streaming buffers are
/// dropped before the solve allocates its own, so the two never coexist, and
/// the escalation runs after the solve's buffers are gone. At 8192x5464x3 this
/// is ~114.7 MB (the detector searches level 4, 512x342, for 33.8 MB); at the
/// 512x512x3 fixture ~53.4 MB, nearly all of it the detector's scale space on
/// the whole frame.
pub fn bytes_per_registration(stack: StackContext, band_rows: u32) -> u64 {
    let plane = stack.width as u64 * stack.height as u64 * 4;
    let rows = band_rows as u64 + 5;
    let stream = stack.width as u64 * rows * (2 * stack.samples_per_pixel as u64 + 7);
    plane / 3
        + stream
            .max(solve_scratch_bytes(
                stack.width,
                stack.height,
                stack.samples_per_pixel,
            ))
            .max(seed_scratch_bytes(
                stack.width,
                stack.height,
                stack.samples_per_pixel,
            ))
}

/// The pure arithmetic behind the anchor arm's worker count: mirrors
/// `crate::tile::scheduler::in_flight_for_budget` clamp for clamp -- floored
/// at 1 (never 0, so a budget smaller than one registration still makes
/// progress), capped at `available_parallelism` -- plus one clamp the
/// scheduler has no equivalent for: capped at `registrations`, the number of
/// non-reference frames there are to register.
///
/// `shared_bytes` (the run's one shared reference data, the pyramid plus the
/// retained templates, [`bytes_shared_reference`]) is subtracted from
/// `budget_bytes` **once**, before dividing by the per-worker
/// `bytes_per_registration` -- `saturating_sub`, so a budget smaller than the
/// shared data floors to one worker instead of underflowing.
fn workers_for_budget(
    shared_bytes: u64,
    bytes_per_registration: u64,
    budget_bytes: u64,
    available_parallelism: usize,
    registrations: usize,
) -> usize {
    let available_parallelism = available_parallelism.max(1);
    let registrations = registrations.max(1);
    let remaining = budget_bytes.saturating_sub(shared_bytes);
    let raw = remaining
        .checked_div(bytes_per_registration)
        .map(|quotient| quotient.max(1))
        .unwrap_or(available_parallelism as u64);
    raw.min(available_parallelism as u64)
        .min(registrations as u64) as usize
}

/// The result of one [`align_stack_with`] run: the [`StackAlignment`] plus how many workers the
/// anchor arm was allowed and how many it reached; both are `1` for chain mode and for an anchor
/// run whose budget floors to one worker. Not stored on [`StackAlignment`] itself: a worker count
/// is a property of *this run*, not of the alignment, and `StackAlignment` is round-tripped
/// verbatim by the resume ledger and the stage cache as an exhaustive struct literal -- a cached or
/// resumed alignment would otherwise carry a previous run's worker count as if it were this run's.
#[derive(Debug, Clone)]
pub struct AlignRun {
    pub alignment: StackAlignment,
    pub worker_limit: usize,
    pub peak_workers: usize,
    /// The bytes this run accounted for: the shared reference data plus
    /// `worker_limit` registrations. An explicit budget below one
    /// registration still runs one worker, so this can exceed the budget.
    pub accounted_bytes: u64,
    /// Whether the accelerator opened at least one session and the result is
    /// its run's, not a CPU rerun's.
    pub backend_used: bool,
    /// Why the whole stage reran on the CPU after the accelerator was lost;
    /// `None` when it did not.
    pub rerun_reason: Option<String>,
}

fn reference_policy_from_cfg(cfg: &ParamValues) -> ReferencePolicy {
    match cfg.get("reference-policy").and_then(ParamValue::as_enum) {
        Some("chain") => ReferencePolicy::Chain,
        _ => ReferencePolicy::Anchor,
    }
}

fn reference_frame_from_cfg(cfg: &ParamValues) -> ReferenceFrame {
    match cfg.get("reference-frame").and_then(ParamValue::as_enum) {
        Some("first") => ReferenceFrame::First,
        Some("last") => ReferenceFrame::Last,
        _ => ReferenceFrame::Middle,
    }
}

/// `First -> 0`, `Last -> frame_count - 1`, `Middle -> (frame_count - 1) /
/// 2` -- the lower middle on an even count, pinned rather than arbitrary
/// (on the eight-frame fixture this selects frame 3, the sharpest frame).
fn reference_index(frame: ReferenceFrame, frame_count: usize) -> usize {
    match frame {
        ReferenceFrame::First => 0,
        ReferenceFrame::Middle => (frame_count - 1) / 2,
        ReferenceFrame::Last => frame_count - 1,
    }
}

/// Bumps `counter`, if there is one -- the one place every registration
/// (anchor, pooled or chain hop) credits [`AlignDriverOptions::frames_done`].
fn credit_frame_done(counter: Option<&AtomicUsize>) {
    if let Some(counter) = counter {
        counter.fetch_add(1, Ordering::SeqCst);
    }
}

/// The frame-boundary check every registration site in every arm makes
/// before it starts: [`AlgorithmError::Cancelled`] once `options.control`
/// reads `CancelRequested`; a hold, calling `options.on_pause` and
/// re-checking, for as long as it reads `PauseRequested` and a hook is set.
/// A `control` with no hook set does not hold -- see
/// [`AlignDriverOptions::on_pause`].
fn wait_at_boundary(options: &AlignDriverOptions<'_>) -> Result<(), AlgorithmError> {
    let Some(control) = options.control.as_deref() else {
        return Ok(());
    };
    loop {
        match control.state() {
            ControlState::CancelRequested => return Err(AlgorithmError::Cancelled),
            ControlState::PauseRequested => match options.on_pause {
                Some(hook) => hook(control),
                None => return Ok(()),
            },
            ControlState::Running => return Ok(()),
        }
    }
}

/// Takes one slot of `live` and returns the new count, waiting while the
/// control's admission cap is full. The wait ends early, taking the slot past
/// the cap, once the control is not `Running`, so a pause or cancel is seen
/// without waiting for the cap to lift; the caller releases the slot on those
/// paths.
fn reserve_live(live: &AtomicUsize, control: Option<&JobControl>) -> usize {
    loop {
        let now = live.load(Ordering::SeqCst);
        let cap = control.map_or(usize::MAX, TileControl::admission_cap);
        if now < cap {
            if live
                .compare_exchange(now, now + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return now + 1;
            }
        } else if control.is_some_and(|c| c.state() != ControlState::Running) {
            return live.fetch_add(1, Ordering::SeqCst) + 1;
        } else {
            std::thread::sleep(ADMISSION_RECHECK);
        }
    }
}

/// The identity [`FrameAlignment`] every reference frame gets, without a
/// solver call.
fn identity_alignment(frame: usize) -> FrameAlignment {
    FrameAlignment {
        frame,
        transform: Transform::identity(),
        status: AlignStatus::Converged,
        iterations: 0,
        residual: 1.0,
        fallback: false,
    }
}

/// Applies [`FailurePolicy`] to one `align_frame` outcome, in the anchor
/// case where the outcome's own transform already is the frame's final
/// transform (no chain composition needed).
fn anchored_alignment(
    frame: usize,
    outcome: crate::algorithm::AlignOutcome,
    failure: FailurePolicy,
) -> FrameAlignment {
    let diverged = matches!(outcome.status, AlignStatus::Diverged(_));
    let fallback = diverged && failure == FailurePolicy::IdentityFallback;
    FrameAlignment {
        frame,
        transform: if fallback {
            Transform::identity()
        } else {
            outcome.transform
        },
        status: outcome.status,
        iterations: outcome.iterations,
        residual: outcome.residual,
        fallback,
    }
}

/// Runs one chain leg (either every frame above the reference, ascending,
/// or every frame below it, descending), writing each hop's
/// [`FrameAlignment`] into `frames[i]`.
#[allow(clippy::too_many_arguments)]
fn run_chain_leg(
    algorithm: &dyn AlignAlgorithm,
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    failure: FailurePolicy,
    frames: &mut [Option<FrameAlignment>],
    indices: impl Iterator<Item = usize>,
    reference: usize,
    options: &AlignDriverOptions<'_>,
) -> Result<(), AlgorithmError> {
    let mut previous = reference;
    let mut prefix = Transform::identity();
    for i in indices {
        wait_at_boundary(options)?;
        let ctx =
            AlignContext::new(stack, previous, reader).with_align_backend(options.align_backend);
        let outcome = algorithm.align_frame(&ctx, i, cfg)?;
        let diverged = matches!(outcome.status, AlignStatus::Diverged(_));
        let fallback = diverged && failure == FailurePolicy::IdentityFallback;
        let hop = if fallback {
            Transform::identity()
        } else {
            outcome.transform
        };
        let composed = hop.compose(&prefix);
        frames[i] = Some(FrameAlignment {
            frame: i,
            transform: composed,
            status: outcome.status,
            iterations: outcome.iterations,
            residual: outcome.residual,
            fallback,
        });
        credit_frame_done(options.frames_done.as_deref());
        prefix = composed;
        previous = i;
    }
    Ok(())
}

/// Registers every frame of `stack` against a reference frame, under
/// `algorithm`, `cfg`'s reference policy and `failure`'s failure policy, at
/// [`AlignDriverOptions::default`]'s worker budget. A thin defaulting
/// wrapper over [`align_stack_with`]. See the module docs.
pub fn align_stack(
    algorithm: &dyn AlignAlgorithm,
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    failure: FailurePolicy,
) -> Result<StackAlignment, AlgorithmError> {
    align_stack_with(
        algorithm,
        stack,
        reader,
        cfg,
        failure,
        &AlignDriverOptions::default(),
    )
    .map(|run| run.alignment)
}

/// Registers every frame of `stack` against a reference frame, under
/// `algorithm`, `cfg`'s reference policy and `failure`'s failure policy, with
/// the anchor arm's worker count bounded by `options`. See the module docs.
///
/// The anchor arm runs across a driver-owned rayon pool sized from
/// `options.budget` and [`bytes_per_registration`], built and
/// dropped inside this call -- it never coexists with the tile scheduler's
/// own pool, which (on the `stack` path) is not built until after this call
/// returns. When the budget floors to one worker, this runs a serial loop
/// rather than building a pool at all, which is also what keeps the
/// byte-identity tests' reference side alive.
///
/// Chain mode is **serial by construction** -- see the module docs -- and
/// always reports `worker_limit: 1, peak_workers: 1`.
///
/// When `options.align_backend` loses its device or runs out of memory
/// mid-run ([`AlgorithmError::BackendLost`]), every result computed so far is
/// discarded and the whole stage reruns on the CPU, so one alignment never
/// mixes backends; [`AlignRun::rerun_reason`] carries the loss.
pub fn align_stack_with(
    algorithm: &dyn AlignAlgorithm,
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    failure: FailurePolicy,
    options: &AlignDriverOptions<'_>,
) -> Result<AlignRun, AlgorithmError> {
    let Some(backend) = options.align_backend else {
        return align_stack_once(algorithm, stack, reader, cfg, failure, options);
    };
    let counting = CountingBackend {
        inner: backend,
        opened: AtomicUsize::new(0),
    };
    match align_stack_once(
        algorithm,
        stack,
        reader,
        cfg,
        failure,
        &rebound(options, Some(&counting)),
    ) {
        Ok(run) => Ok(AlignRun {
            backend_used: counting.opened.load(Ordering::SeqCst) > 0,
            ..run
        }),
        Err(lost @ AlgorithmError::BackendLost { .. }) => {
            let reason = lost.to_string();
            tracing::warn!(%reason, "rerunning the whole align stage on the CPU");
            if let Some(done) = &options.frames_done {
                done.store(0, Ordering::SeqCst);
            }
            align_stack_once(
                algorithm,
                stack,
                reader,
                cfg,
                failure,
                &rebound(options, None),
            )
            .map(|run| AlignRun {
                rerun_reason: Some(reason),
                ..run
            })
        }
        Err(other) => Err(other),
    }
}

/// `options` with its accelerator replaced by `backend`.
fn rebound<'a>(
    options: &AlignDriverOptions<'a>,
    backend: Option<&'a dyn AlignBackend>,
) -> AlignDriverOptions<'a> {
    AlignDriverOptions {
        budget: options.budget,
        frames_done: options.frames_done.clone(),
        control: options.control.clone(),
        on_pause: options.on_pause,
        align_backend: backend,
    }
}

/// Delegates to an accelerator and counts the sessions it opens.
struct CountingBackend<'a> {
    inner: &'a dyn AlignBackend,
    opened: AtomicUsize,
}

impl AlignBackend for CountingBackend<'_> {
    fn open_ecc(
        &self,
        request: &EccSessionRequest<'_>,
    ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
        let session = self.inner.open_ecc(request)?;
        if session.is_some() {
            self.opened.fetch_add(1, Ordering::SeqCst);
        }
        Ok(session)
    }
}

/// One pass over the stack with `options.align_backend` as given.
fn align_stack_once(
    algorithm: &dyn AlignAlgorithm,
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    failure: FailurePolicy,
    options: &AlignDriverOptions<'_>,
) -> Result<AlignRun, AlgorithmError> {
    let policy = reference_policy_from_cfg(cfg);
    let reference = reference_index(reference_frame_from_cfg(cfg), stack.frame_count);

    let mut frames: Vec<Option<FrameAlignment>> = vec![None; stack.frame_count];
    let frame_bytes = frames.capacity() * std::mem::size_of::<Option<FrameAlignment>>();
    if frame_bytes != 0 {
        record_timing(TimingCounter::AllocationBytes, frame_bytes as u64);
        record_timing(TimingCounter::AllocationOperations, 1);
    }
    frames[reference] = Some(identity_alignment(reference));

    let shared_bytes = bytes_shared_reference(stack);
    let mut registration_bytes = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
    let (worker_limit, peak_workers) = match policy {
        ReferencePolicy::Anchor => {
            let registrations = stack.frame_count.saturating_sub(1);
            registration_bytes += reader.read_scratch_bytes(stack.width, DEFAULT_TILE_SIZE);
            let budget_bytes = options.budget.bytes();
            if options.budget.refuses_below_one_tile()
                && budget_bytes < shared_bytes + registration_bytes
            {
                return Err(AlgorithmError::HostMemoryTooSmall {
                    budget_bytes,
                    required_bytes: shared_bytes + registration_bytes,
                });
            }
            let available_parallelism = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            let workers = workers_for_budget(
                shared_bytes,
                registration_bytes,
                budget_bytes,
                available_parallelism,
                registrations,
            );

            // `prepare_run` is called exactly here, at most once per run,
            // only in the anchor arm, so both sub-arms below share the
            // same prepared reference pyramid from this single line. The
            // `Arc` binding lives in this stack frame and drops when this
            // function returns.
            let ctx = AlignContext::new(stack, reference, reader)
                .with_align_backend(options.align_backend);
            let prepared = {
                let _prepare = measurement_scope("align_prepare");
                algorithm.prepare_run(&ctx, cfg)?
            };
            let ctx = ctx.with_prepared(prepared.as_deref());
            credit_frame_done(options.frames_done.as_deref());

            if workers <= 1 {
                // Single worker: serial loop, no pool built. This is the
                // byte-identity tests' reference side.
                for (i, slot) in frames.iter_mut().enumerate() {
                    if i == reference {
                        continue;
                    }
                    wait_at_boundary(options)?;
                    let _solve = measurement_scope("align_solve");
                    let outcome = algorithm.align_frame(&ctx, i, cfg)?;
                    *slot = Some(anchored_alignment(i, outcome, failure));
                    credit_frame_done(options.frames_done.as_deref());
                }
                (1, 1)
            } else {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .thread_name(|i| format!("focusforge-align-{i}"))
                    .panic_handler(crate::pool::log_worker_panic)
                    .build()
                    .map_err(|source| AlgorithmError::Failed {
                        algorithm: "align_stack".to_string(),
                        detail: format!("alignment thread pool: {source}"),
                    })?;

                let live = AtomicUsize::new(0);
                let peak = AtomicUsize::new(0);

                // Span context does not cross onto a pool thread by itself:
                // the caller's span is captured here and each frame span is
                // parented to it explicitly, so an event raised while one
                // frame registers still names the run it belongs to.
                let parent = tracing::Span::current();

                // Every non-reference frame, ascending -- what one
                // `pool.install` round below registers. A pause leaves some
                // entries unregistered; they stay `pending` for the next
                // round on the same pool, so no rayon worker ever blocks
                // waiting out a hold.
                let mut pending: Vec<usize> =
                    (0..stack.frame_count).filter(|&i| i != reference).collect();

                loop {
                    // One `Result` per still-pending frame, `collect()`ed in
                    // `pending`'s own (ascending) order, so the scan below
                    // matches what the serial loop's `?` would have hit
                    // first.
                    let outcomes: Vec<Option<Result<FrameAlignment, AlgorithmError>>> = pool
                        .install(|| {
                            pending
                                .par_iter()
                                .map(|&i| {
                                    let cur = reserve_live(&live, options.control.as_deref());
                                    match options.control.as_deref().map(JobControl::state) {
                                        Some(ControlState::CancelRequested) => {
                                            live.fetch_sub(1, Ordering::SeqCst);
                                            Some(Err(AlgorithmError::Cancelled))
                                        }
                                        Some(ControlState::PauseRequested)
                                            if options.on_pause.is_some() =>
                                        {
                                            live.fetch_sub(1, Ordering::SeqCst);
                                            None
                                        }
                                        _ => {
                                            let frame_span = tracing::debug_span!(
                                                parent: &parent,
                                                "frame",
                                                index = i
                                            );
                                            let _frame = frame_span.enter();
                                            peak.fetch_max(cur, Ordering::SeqCst);
                                            let _solve = measurement_scope("align_solve");
                                            let result = algorithm.align_frame(&ctx, i, cfg).map(
                                                |outcome| anchored_alignment(i, outcome, failure),
                                            );
                                            live.fetch_sub(1, Ordering::SeqCst);
                                            credit_frame_done(options.frames_done.as_deref());
                                            Some(result)
                                        }
                                    }
                                })
                                .collect()
                        });

                    // A device loss reaches concurrent workers in different
                    // shapes; any frame that saw it as one makes the whole
                    // batch rerunnable, whichever frame failed first.
                    let mut outcomes = outcomes;
                    if let Some(lost) = outcomes.iter().position(|outcome| {
                        matches!(outcome, Some(Err(AlgorithmError::BackendLost { .. })))
                    }) && let Some(Err(err)) = outcomes.swap_remove(lost)
                    {
                        return Err(err);
                    }
                    let mut next_pending = Vec::new();
                    for (&i, outcome) in pending.iter().zip(outcomes) {
                        match outcome {
                            Some(Ok(alignment)) => frames[i] = Some(alignment),
                            Some(Err(err)) => return Err(err),
                            None => next_pending.push(i),
                        }
                    }
                    pending = next_pending;
                    if pending.is_empty() {
                        break;
                    }
                    // Every in-flight registration above has already
                    // returned (`install` only comes back once the whole
                    // batch has), so this holds on the caller's thread,
                    // never a pool worker.
                    wait_at_boundary(options)?;
                }

                (workers, peak.load(Ordering::SeqCst))
            }
        }
        ReferencePolicy::Chain => {
            credit_frame_done(options.frames_done.as_deref());
            run_chain_leg(
                algorithm,
                stack,
                reader,
                cfg,
                failure,
                &mut frames,
                (reference + 1)..stack.frame_count,
                reference,
                options,
            )?;
            run_chain_leg(
                algorithm,
                stack,
                reader,
                cfg,
                failure,
                &mut frames,
                (0..reference).rev(),
                reference,
                options,
            )?;
            (1, 1)
        }
    };

    let frames = frames
        .into_iter()
        .enumerate()
        .map(|(i, f)| f.unwrap_or_else(|| panic!("frame {i} was never visited by align_stack")))
        .collect();

    observe_timing_max(TimingCounter::WorkerLimit, worker_limit as u64);
    observe_timing_max(TimingCounter::PeakWorkers, peak_workers as u64);

    Ok(AlignRun {
        alignment: StackAlignment {
            reference,
            policy,
            frames,
        },
        worker_limit,
        peak_workers,
        accounted_bytes: shared_bytes + worker_limit as u64 * registration_bytes,
        backend_used: false,
        rerun_reason: None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::algorithm::AlignOutcome;
    use crate::params::ParamDesc;
    use crate::tile::Rect;

    struct NullReader;

    impl TileReader for NullReader {
        fn read_tile_into(
            &self,
            _frame: usize,
            _rect: Rect,
            _out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            Ok(())
        }
    }

    /// A stub [`AlignAlgorithm`] returning a pre-scripted [`AlignOutcome`]
    /// per queried frame index -- exercises [`align_stack`]'s composition
    /// and failure-policy logic without a real solve.
    struct StubAlign {
        outcomes: HashMap<usize, AlignOutcome>,
    }

    impl AlignAlgorithm for StubAlign {
        fn id(&self) -> &str {
            "stub-align"
        }
        fn display_name(&self) -> &str {
            "Stub Align"
        }
        fn params(&self) -> &[ParamDesc] {
            &[]
        }
        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            Ok(*self
                .outcomes
                .get(&frame)
                .unwrap_or_else(|| panic!("test did not script frame {frame}")))
        }
    }

    fn converged(transform: Transform) -> AlignOutcome {
        AlignOutcome {
            transform,
            status: AlignStatus::Converged,
            iterations: 3,
            residual: 0.95,
        }
    }

    fn diverged(transform: Transform) -> AlignOutcome {
        AlignOutcome {
            transform,
            status: AlignStatus::Diverged(crate::align::DivergenceReason::TooFewValidSamples),
            iterations: 5,
            residual: 0.1,
        }
    }

    fn cfg_with(policy: &str, frame: &str) -> ParamValues {
        let mut cfg = ParamValues::new();
        cfg.insert(
            "reference-policy".to_string(),
            ParamValue::Enum(policy.to_string()),
        );
        cfg.insert(
            "reference-frame".to_string(),
            ParamValue::Enum(frame.to_string()),
        );
        cfg
    }

    #[test]
    fn reference_index_for_each_policy() {
        assert_eq!(reference_index(ReferenceFrame::First, 7), 0);
        assert_eq!(reference_index(ReferenceFrame::First, 8), 0);
        assert_eq!(reference_index(ReferenceFrame::Last, 7), 6);
        assert_eq!(reference_index(ReferenceFrame::Last, 8), 7);
        // Odd count: exact middle. Even count: the lower middle.
        assert_eq!(reference_index(ReferenceFrame::Middle, 7), 3);
        assert_eq!(reference_index(ReferenceFrame::Middle, 8), 3);
    }

    #[test]
    fn chained_hops_compose_to_the_anchor() {
        // Non-commuting transforms (similarity, not pure translation) --
        // pure translations compose the same regardless of argument order,
        // so they cannot catch a flipped composition.
        let hop1 = Transform::similarity(2.0, 0.3, 1.0, -0.5);
        let hop2 = Transform::translation(0.0, 3.0);
        let hop3 = Transform::similarity(0.9, -0.1, 2.0, 0.0);

        let mut outcomes = HashMap::new();
        outcomes.insert(1, converged(hop1));
        outcomes.insert(2, converged(hop2));
        outcomes.insert(3, converged(hop3));
        let algo = StubAlign { outcomes };
        let reader = NullReader;
        let stack = StackContext::new(4, 8, 8, 1);
        let cfg = cfg_with("chain", "first");

        let result = align_stack(&algo, stack, &reader, &cfg, FailurePolicy::Strict)
            .expect("stub algorithm never errors");

        let expected_1 = hop1;
        let expected_2 = hop2.compose(&expected_1);
        let expected_3 = hop3.compose(&expected_2);

        assert_eq!(result.frames[0].transform, Transform::identity());
        assert_eq!(result.frames[1].transform, expected_1);
        assert_eq!(result.frames[2].transform, expected_2);
        assert_eq!(result.frames[3].transform, expected_3);
    }

    #[test]
    fn failure_policy_strict_keeps_the_best_rho_transform() {
        let bad = Transform::translation(99.0, 99.0);
        let mut outcomes = HashMap::new();
        outcomes.insert(1, diverged(bad));
        outcomes.insert(2, converged(Transform::translation(1.0, 0.0)));
        let algo = StubAlign { outcomes };
        let reader = NullReader;
        let stack = StackContext::new(3, 8, 8, 1);
        let cfg = cfg_with("anchor", "first");

        let result = align_stack(&algo, stack, &reader, &cfg, FailurePolicy::Strict).unwrap();

        assert_eq!(result.failure_count(), 1);
        let failed = result.frames[1];
        assert_eq!(
            failed.transform, bad,
            "strict keeps the solver's own best-ρ transform"
        );
        assert!(!failed.fallback);
    }

    #[test]
    fn failure_policy_identity_fallback_substitutes_identity() {
        let bad = Transform::translation(99.0, 99.0);
        let mut outcomes = HashMap::new();
        outcomes.insert(1, diverged(bad));
        outcomes.insert(2, converged(Transform::translation(1.0, 0.0)));
        let algo = StubAlign { outcomes };
        let reader = NullReader;
        let stack = StackContext::new(3, 8, 8, 1);
        let cfg = cfg_with("anchor", "first");

        let result =
            align_stack(&algo, stack, &reader, &cfg, FailurePolicy::IdentityFallback).unwrap();

        assert_eq!(result.failure_count(), 1);
        let failed = result.frames[1];
        assert_eq!(failed.transform, Transform::identity());
        assert!(failed.fallback);
        // The status is preserved, unchanged, under either policy.
        assert!(matches!(failed.status, AlignStatus::Diverged(_)));
    }

    #[test]
    fn worker_budget_arithmetic() {
        // The scheduler's own `budget_arithmetic_at_the_reference_scale`
        // shape: a plain division that floors, not yet clamped by anything.
        // `shared_bytes = 0` is what proves the parameter additive.
        let workers = workers_for_budget(0, 100, 250, 32, 60);
        assert_eq!(workers, 2);
    }

    #[test]
    fn worker_budget_floors_at_one_never_zero() {
        let workers = workers_for_budget(0, 1_000_000, 1, 32, 60);
        assert_eq!(workers, 1);
    }

    #[test]
    fn worker_budget_caps_at_available_parallelism() {
        let workers = workers_for_budget(0, 1, u64::MAX, 4, 60);
        assert_eq!(workers, 4);
    }

    #[test]
    fn worker_budget_caps_at_registrations() {
        // The scheduler has no equivalent of this clamp: a tile plan always
        // outnumbers its workers, an 8-frame fixture on a 32-core host does
        // not.
        let workers = workers_for_budget(0, 1, u64::MAX, 32, 7);
        assert_eq!(workers, 7);
    }

    #[test]
    fn worker_budget_zero_divisor_does_not_panic() {
        let workers = workers_for_budget(0, 0, 100, 8, 60);
        assert_eq!(workers, 8);
    }

    #[test]
    fn worker_budget_never_exceeds_the_raw_ratio() {
        // The bound is never looser than the arithmetic itself allows, even
        // when neither clamp binds.
        let bytes = 1_000u64;
        let budget = 250_000u64;
        let workers = workers_for_budget(0, bytes, budget, 1_000, 1_000);
        assert!(workers as u64 <= budget / bytes);
    }

    #[test]
    fn worker_budget_accounts_for_the_shared_reference() {
        // `workers_for_budget` subtracts `shared_bytes` once, up front, then
        // divides the remainder by the per-worker figure -- never the other
        // way around, and never per-worker.
        let workers = workers_for_budget(100, 100, 300, 32, 60);
        // remaining = 300 - 100 = 200; 200 / 100 = 2.
        assert_eq!(workers, 2);
    }

    #[test]
    fn the_split_sums_to_625_mb_at_the_reference_scale() {
        // Shared: the reference's coarse pyramid, the retained level templates,
        // the patch templates and the feature set. Per registration: the
        // frame's coarse pyramid (59.7 MB) plus the solve scratch and windows
        // (173.1 MB), which outweigh the streaming band (55.1 MB).
        let stack = StackContext::new(61, 8192, 5464, 3);
        let shared = bytes_shared_reference(stack);
        let per_registration = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        assert_eq!(shared, 59_681_450 + 299_706_264 + 33_030_144 + 2000 * 72);
        assert_eq!(
            per_registration,
            59_681_450 + 24 * 2507 * 1672 + 3031 * 2196 * 10 + 772 * 772 * 10
        );
        let ratio = (shared + per_registration) as f64 / 625_000_000f64;
        assert!(
            (ratio - 1.0).abs() < 0.01,
            "shared + per registration = {} is not within 1% of 625 MB",
            shared + per_registration
        );
    }

    #[test]
    fn worker_budget_at_the_reference_scale() {
        // Pinned with an explicit `available_parallelism` and registration
        // count, so this assertion never depends on the host: the library
        // default budget yields 7 workers at the reference scale: the 4x
        // level-0 solve region leaves room for fewer concurrent registrations.
        let stack = StackContext::new(61, 8192, 5464, 3);
        let shared = bytes_shared_reference(stack);
        let bytes = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        let workers = workers_for_budget(
            shared,
            bytes,
            DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT as u64,
            32,
            60,
        );
        assert_eq!(workers, 7);
    }

    #[test]
    fn single_sample_stacks_are_priced_by_templates_and_pyramids() {
        // At 6000x4000x1 the 2508x1672 solve region costs 100.6 MB of scratch,
        // on top of its window (53.3 MB) and a patch window (3.6 MB); the
        // 9 B per pixel stream term is 27.9 MB.
        let stack = StackContext::new(20, 6000, 4000, 1);
        let shared = bytes_shared_reference(stack);
        let per_registration = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        let patches = 8 * 384 * 384 * 28;
        assert_eq!(shared, 32_000_000 + 280_153_808 + patches + 2000 * 72);
        let solve = 24 * 2508 * 1672 + 3032 * 2196 * 8 + 772 * 772 * 6;
        let stream = 6000 * (DEFAULT_TILE_SIZE as u64 + 5) * 9;
        assert!(solve > stream);
        assert_eq!(per_registration, 32_000_000 + solve);
        assert!(shared < 128_000_000 + 280_153_808);
        assert!(per_registration < 128_000_000 + 100_641_024);
        let workers = workers_for_budget(shared, per_registration, 2 << 30, 32, 19);
        assert_eq!(workers, 9);
    }

    #[test]
    fn full_colour_frames_are_priced_by_the_larger_of_streaming_band_and_solve() {
        let stack = StackContext::new(61, 8192, 5464, 3);
        let shared = bytes_shared_reference(stack);
        let per_registration = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        let plane = 8192u64 * 5464 * 4;
        let patches = 8 * 384 * 384 * 28;
        assert_eq!(shared, plane / 3 + 299_706_264 + patches + 2000 * 72);
        let stream = 8192 * (DEFAULT_TILE_SIZE as u64 + 5) * 13;
        let solve = 24 * 2507 * 1672 + 3031 * 2196 * 10 + 772 * 772 * 10;
        assert!(solve > stream);
        assert_eq!(per_registration, plane / 3 + solve);
        assert!(shared < 400_100_000, "shared {shared} B");
        assert!(
            per_registration < 263_900_000,
            "per registration {per_registration} B"
        );
    }

    #[test]
    fn bytes_per_registration_matches_the_pyramid() {
        let width = 512u32;
        let height = 512u32;
        let spp = 3u32;
        let depth = crate::align::max_levels(width, height).min(4);
        let samples = vec![0u16; (width * height * spp) as usize];
        let pyramid = crate::align::AlignPyramid::stream(
            (width, height, spp),
            depth,
            DEFAULT_TILE_SIZE / 2,
            |rect, out| -> Result<(), ()> {
                let row = (width * spp) as usize;
                let start = rect.y as usize * row;
                out.copy_from_slice(&samples[start..start + rect.height as usize * row]);
                Ok(())
            },
        )
        .unwrap_or_else(|_| panic!("streaming the pyramid"));
        let pyramid_bytes: u64 = (1..=depth as usize)
            .map(|k| pyramid.level(k).data().len() as u64 * 4)
            .sum();
        // The solve term: three vectors of 24 bytes per pixel over the
        // 384 x 384 level-0 solve region, the frame window (the whole 512 px
        // frame) and one 260 px patch window at 10 bytes per pixel.
        let solve = 24 * 384 * 384u64 + 512 * 512 * 10 + 260 * 260 * 10;
        let rows = DEFAULT_TILE_SIZE as u64 + 5;
        let stream = width as u64 * rows * (2 * spp as u64 + 7);
        // The escalation's detector runs on the whole frame: 192 bytes of
        // scale space per pixel, the frame read in and 2000 features.
        let seed = 512 * 512 * (192 + 10) + 2000 * 72;
        let floor = pyramid_bytes + stream.max(solve).max(seed);

        let stack = StackContext::new(8, width, height, spp);
        let bpr = bytes_per_registration(stack, DEFAULT_TILE_SIZE);

        assert!(
            bpr >= floor,
            "budget {bpr} must be >= the real frame pyramid + scratch figure {floor}"
        );
        let ratio = bpr as f64 / floor as f64;
        assert!(
            ratio <= 1.05,
            "budget {bpr} must be within 5% of the real figure {floor} (ratio {ratio})"
        );

        let shared = bytes_shared_reference(stack);
        assert!(
            shared >= pyramid_bytes,
            "bytes_shared_reference {shared} must be >= the real pyramid figure {pyramid_bytes}"
        );
    }

    fn anchor_stack() -> StackContext {
        StackContext::new(4, 512, 512, 3)
    }

    #[test]
    fn a_host_derived_budget_below_the_minimum_refuses_before_any_registration() {
        // usable_available = 100 B, so the budget is 40 B.
        let facts = crate::host::HostFacts::new(1_000, 100, None, None, 4);
        let counter = Arc::new(AtomicUsize::new(0));
        let algo = RecordingAlign {
            counter,
            seen: Mutex::new(Vec::new()),
        };
        let options = AlignDriverOptions {
            budget: HostBudget::host_derived(&facts),
            ..Default::default()
        };
        let stack = anchor_stack();
        let error = align_stack_with(
            &algo,
            stack,
            &NullReader,
            &cfg_with("anchor", "middle"),
            FailurePolicy::Strict,
            &options,
        )
        .expect_err("a budget of 40 B cannot hold the shared term");
        let required =
            bytes_shared_reference(stack) + bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        assert!(
            matches!(
                error,
                AlgorithmError::HostMemoryTooSmall { budget_bytes: 40, required_bytes }
                    if required_bytes == required
            ),
            "{error:?}"
        );
        assert!(algo.seen.lock().unwrap().is_empty(), "no registration ran");
    }

    #[test]
    fn an_explicit_budget_below_the_minimum_floors_to_one_worker_and_completes() {
        let algo = ConstAlign;
        let options = AlignDriverOptions {
            budget: HostBudget::explicit(1),
            ..Default::default()
        };
        let run = align_stack_with(
            &algo,
            anchor_stack(),
            &NullReader,
            &cfg_with("anchor", "middle"),
            FailurePolicy::Strict,
            &options,
        )
        .expect("an explicit budget is floored, never refused");
        assert_eq!(run.worker_limit, 1);
    }

    #[test]
    fn a_host_derived_budget_above_the_minimum_runs() {
        // usable_available = 1 GiB, so the budget is about 430 MB.
        let facts = crate::host::HostFacts::new(1 << 31, 1 << 30, None, None, 4);
        let options = AlignDriverOptions {
            budget: HostBudget::host_derived(&facts),
            ..Default::default()
        };
        align_stack_with(
            &ConstAlign,
            anchor_stack(),
            &NullReader,
            &cfg_with("anchor", "middle"),
            FailurePolicy::Strict,
            &options,
        )
        .expect("the budget covers the shared term and one registration");
    }

    #[test]
    fn the_run_reports_the_bytes_it_accounted() {
        let stack = StackContext::new(8, 512, 512, 3);
        let shared = bytes_shared_reference(stack);
        let per_registration = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        let options = AlignDriverOptions {
            budget: HostBudget::explicit(shared + 2 * per_registration),
            ..Default::default()
        };
        for (policy, workers_at_most) in [("anchor", 2), ("chain", 1)] {
            let run = align_stack_with(
                &ConstAlign,
                stack,
                &NullReader,
                &cfg_with(policy, "middle"),
                FailurePolicy::Strict,
                &options,
            )
            .unwrap();
            assert!(run.worker_limit <= workers_at_most, "{policy}");
            assert_eq!(
                run.accounted_bytes,
                shared + run.worker_limit as u64 * per_registration,
                "{policy}"
            );
        }
    }

    /// Records the shared counter's value as seen from inside each
    /// `align_frame` call, and always converges.
    struct RecordingAlign {
        counter: Arc<AtomicUsize>,
        seen: Mutex<Vec<usize>>,
    }

    impl AlignAlgorithm for RecordingAlign {
        fn id(&self) -> &str {
            "recording-align"
        }
        fn display_name(&self) -> &str {
            "Recording Align"
        }
        fn params(&self) -> &[ParamDesc] {
            &[]
        }
        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            _frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            self.seen
                .lock()
                .expect("no panic while recording")
                .push(self.counter.load(Ordering::SeqCst));
            Ok(converged(Transform::identity()))
        }
    }

    /// Converges every frame it is asked about, with no bookkeeping of its
    /// own -- for tests that only care about the driver's own counter.
    struct ConstAlign;

    impl AlignAlgorithm for ConstAlign {
        fn id(&self) -> &str {
            "const-align"
        }
        fn display_name(&self) -> &str {
            "Const Align"
        }
        fn params(&self) -> &[ParamDesc] {
            &[]
        }
        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            _frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            Ok(converged(Transform::identity()))
        }
    }

    /// Converges every frame and counts its calls; the call numbered
    /// `cancel_on` requests a cancel on `control` before it returns.
    struct CancellingAlign {
        control: Arc<JobControl>,
        cancel_on: usize,
        calls: AtomicUsize,
    }

    impl AlignAlgorithm for CancellingAlign {
        fn id(&self) -> &str {
            "cancelling-align"
        }
        fn display_name(&self) -> &str {
            "Cancelling Align"
        }
        fn params(&self) -> &[ParamDesc] {
            &[]
        }
        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            _frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.cancel_on {
                self.control.cancel();
            }
            Ok(converged(Transform::identity()))
        }
    }

    #[test]
    fn cancel_stops_registering_at_a_frame_boundary_in_every_arm() {
        const CANCEL_ON: usize = 2;
        let reader = NullReader;
        let stack = StackContext::new(20, 8, 8, 1);
        let two_workers = bytes_shared_reference(stack) as usize
            + 2 * bytes_per_registration(stack, DEFAULT_TILE_SIZE) as usize;
        // (policy, budget, most calls allowed): the cancelling call, plus
        // the one registration the other pooled worker may hold.
        let arms = [
            ("anchor", 1, CANCEL_ON),
            ("anchor", two_workers, CANCEL_ON + 1),
            ("chain", 1, CANCEL_ON),
        ];
        for (policy, max_bytes_in_flight, bound) in arms {
            let control = JobControl::new();
            let algo = CancellingAlign {
                control: Arc::clone(&control),
                cancel_on: CANCEL_ON,
                calls: AtomicUsize::new(0),
            };
            let options = AlignDriverOptions {
                budget: HostBudget::explicit(max_bytes_in_flight as u64),
                control: Some(control),
                ..Default::default()
            };
            let result = align_stack_with(
                &algo,
                stack,
                &reader,
                &cfg_with(policy, "middle"),
                FailurePolicy::Strict,
                &options,
            );
            assert!(
                matches!(result, Err(AlgorithmError::Cancelled)),
                "{policy} at {max_bytes_in_flight} bytes must report the cancel, got {:?}",
                result.as_ref().err()
            );
            let calls = algo.calls.load(Ordering::SeqCst);
            assert!(
                calls <= bound,
                "{policy} at {max_bytes_in_flight} bytes: {calls} registrations ran, at most {bound} may"
            );
        }
    }

    /// Converges every frame with a transform that is a pure translation by
    /// the frame index -- a skipped, repeated or mis-slotted frame, or a
    /// chain hop composed against the wrong predecessor, changes the result
    /// -- and counts its `align_frame` calls; the call numbered `pause_on`
    /// requests a pause on `control` before it returns.
    struct PausingAlign {
        control: Arc<JobControl>,
        pause_on: usize,
        calls: AtomicUsize,
    }

    impl AlignAlgorithm for PausingAlign {
        fn id(&self) -> &str {
            "pausing-align"
        }
        fn display_name(&self) -> &str {
            "Pausing Align"
        }
        fn params(&self) -> &[ParamDesc] {
            &[]
        }
        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.pause_on {
                self.control.pause();
            }
            Ok(converged(Transform::translation(frame as f64, 0.0)))
        }
    }

    /// Converges every frame after a short solve, counting how often each
    /// frame was registered.
    struct CountingAlign {
        registrations: Mutex<Vec<usize>>,
    }

    impl AlignAlgorithm for CountingAlign {
        fn id(&self) -> &str {
            "counting-align"
        }
        fn display_name(&self) -> &str {
            "Counting Align"
        }
        fn params(&self) -> &[ParamDesc] {
            &[]
        }
        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            std::thread::sleep(Duration::from_millis(3));
            self.registrations.lock().expect("no panic while counting")[frame] += 1;
            Ok(converged(Transform::identity()))
        }
    }

    #[test]
    fn a_cap_bounds_live_registrations_and_skips_no_frame() {
        const FRAMES: usize = 13;
        let stack = StackContext::new(FRAMES, 8, 8, 1);
        let four_workers =
            bytes_shared_reference(stack) + 4 * bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        let algo = CountingAlign {
            registrations: Mutex::new(vec![0; FRAMES]),
        };
        let control = JobControl::new();
        control.set_admission_cap(Some(1));
        let options = AlignDriverOptions {
            budget: HostBudget::explicit(four_workers),
            control: Some(control),
            ..Default::default()
        };
        let run = align_stack_with(
            &algo,
            stack,
            &NullReader,
            &cfg_with("anchor", "middle"),
            FailurePolicy::Strict,
            &options,
        )
        .unwrap();
        assert!(
            run.worker_limit >= 2,
            "the parallel arm needs at least two workers, got {}",
            run.worker_limit
        );
        assert_eq!(run.peak_workers, 1, "a cap of 1 admits one registration");
        let reference = (FRAMES - 1) / 2;
        for (frame, count) in algo.registrations.lock().unwrap().iter().enumerate() {
            let expected = usize::from(frame != reference);
            assert_eq!(*count, expected, "frame {frame} registered {count} times");
        }
    }

    #[test]
    fn pause_holds_at_a_frame_boundary_in_every_arm() {
        const PAUSE_ON: usize = 2;
        let reader = NullReader;
        let stack = StackContext::new(20, 8, 8, 1);
        let two_workers = bytes_shared_reference(stack) as usize
            + 2 * bytes_per_registration(stack, DEFAULT_TILE_SIZE) as usize;
        // The most `align_frame` calls that may have run when the hook is
        // entered: the pausing call itself, plus one in-flight registration
        // per other worker.
        let arms = [
            ("anchor", 1usize, PAUSE_ON),
            ("anchor", two_workers, PAUSE_ON + 1),
            ("chain", 1usize, PAUSE_ON),
        ];
        for (policy, max_bytes_in_flight, bound) in arms {
            let control = JobControl::new();
            // Two admitted registrations leave the pooled arm's bound as is.
            control.set_admission_cap(Some(2));
            let algo = PausingAlign {
                control: Arc::clone(&control),
                pause_on: PAUSE_ON,
                calls: AtomicUsize::new(0),
            };
            let hook_entries = AtomicUsize::new(0);
            let on_pause = |control: &JobControl| {
                hook_entries.fetch_add(1, Ordering::SeqCst);
                let before = algo.calls.load(Ordering::SeqCst);
                assert!(
                    (PAUSE_ON..=bound).contains(&before),
                    "{policy} at {max_bytes_in_flight} bytes: the hook entered after {before} \
                     registrations, it must enter after {PAUSE_ON}..={bound}"
                );
                std::thread::sleep(Duration::from_millis(500));
                assert_eq!(
                    algo.calls.load(Ordering::SeqCst),
                    before,
                    "{policy} at {max_bytes_in_flight} bytes: a registration started during \
                     the hold"
                );
                control.clear();
            };
            let options = AlignDriverOptions {
                budget: HostBudget::explicit(max_bytes_in_flight as u64),
                control: Some(Arc::clone(&control)),
                on_pause: Some(&on_pause),
                ..Default::default()
            };
            align_stack_with(
                &algo,
                stack,
                &reader,
                &cfg_with(policy, "middle"),
                FailurePolicy::Strict,
                &options,
            )
            .unwrap_or_else(|err| {
                panic!("{policy} at {max_bytes_in_flight} bytes must not error: {err}")
            });
            assert_eq!(
                hook_entries.load(Ordering::SeqCst),
                1,
                "{policy} at {max_bytes_in_flight} bytes: the pause hook must be entered exactly \
                 once"
            );
        }
    }

    #[test]
    fn paused_alignment_resumes_to_the_same_result() {
        let reader = NullReader;
        let stack = StackContext::new(20, 8, 8, 1);
        let two_workers = bytes_shared_reference(stack) as usize
            + 2 * bytes_per_registration(stack, DEFAULT_TILE_SIZE) as usize;
        // (policy, budget, pause_on): an interior call and the last
        // registration, in each arm.
        let arms = [
            ("anchor", 1usize, 5usize),
            ("anchor", 1usize, 19usize),
            ("anchor", two_workers, 5usize),
            ("anchor", two_workers, 19usize),
            ("chain", 1usize, 5usize),
            ("chain", 1usize, 19usize),
        ];
        for (policy, max_bytes_in_flight, pause_on) in arms {
            let baseline_algo = PausingAlign {
                control: JobControl::new(),
                pause_on: usize::MAX,
                calls: AtomicUsize::new(0),
            };
            let baseline_options = AlignDriverOptions {
                budget: HostBudget::explicit(max_bytes_in_flight as u64),
                ..Default::default()
            };
            let baseline = align_stack_with(
                &baseline_algo,
                stack,
                &reader,
                &cfg_with(policy, "middle"),
                FailurePolicy::Strict,
                &baseline_options,
            )
            .expect("the unpaused run must not error")
            .alignment;

            let control = JobControl::new();
            let algo = PausingAlign {
                control: Arc::clone(&control),
                pause_on,
                calls: AtomicUsize::new(0),
            };
            let on_pause = |control: &JobControl| control.clear();
            let options = AlignDriverOptions {
                budget: HostBudget::explicit(max_bytes_in_flight as u64),
                control: Some(Arc::clone(&control)),
                on_pause: Some(&on_pause),
                ..Default::default()
            };
            let resumed = align_stack_with(
                &algo,
                stack,
                &reader,
                &cfg_with(policy, "middle"),
                FailurePolicy::Strict,
                &options,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "{policy} at {max_bytes_in_flight} bytes, pause_on {pause_on} must not \
                     error: {err}"
                )
            })
            .alignment;

            assert_eq!(
                resumed, baseline,
                "{policy} at {max_bytes_in_flight} bytes, pause_on {pause_on}: a \
                 paused-then-resumed run must equal the unpaused one"
            );
            assert_eq!(
                algo.calls.load(Ordering::SeqCst),
                stack.frame_count - 1,
                "{policy} at {max_bytes_in_flight} bytes, pause_on {pause_on}: every \
                 non-reference frame must register exactly once"
            );
        }
    }

    #[test]
    fn align_progress_is_visible_mid_stage() {
        // The serial arm (`max_bytes_in_flight: 1`) over a 6-frame stack
        // anchored on the middle frame (index 2): the reference is credited
        // before the loop starts, so the five registrations that follow
        // (frames 0, 1, 3, 4, 5, in that order) each see the counter one
        // higher than the last -- strictly rising, strictly between 0 and
        // the frame count, ending at `frame_count - 1`.
        let counter = Arc::new(AtomicUsize::new(0));
        let algo = RecordingAlign {
            counter: Arc::clone(&counter),
            seen: Mutex::new(Vec::new()),
        };
        let reader = NullReader;
        let stack = StackContext::new(6, 8, 8, 1);
        let cfg = cfg_with("anchor", "middle");
        let options = AlignDriverOptions {
            budget: HostBudget::explicit(1),
            frames_done: Some(Arc::clone(&counter)),
            ..Default::default()
        };

        align_stack_with(&algo, stack, &reader, &cfg, FailurePolicy::Strict, &options)
            .expect("the recording algorithm never errors");

        let seen = algo.seen.lock().expect("no panic while recording").clone();
        assert_eq!(seen, vec![1, 2, 3, 4, 5]);
        assert_eq!(counter.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn align_progress_matches_across_worker_counts() {
        let algo = ConstAlign;
        let reader = NullReader;
        for frame_count in [6usize, 9] {
            let stack = StackContext::new(frame_count, 8, 8, 1);
            for reference in ["middle", "first"] {
                let anchor_cfg = cfg_with("anchor", reference);
                let chain_cfg = cfg_with("chain", reference);

                let serial_counter = Arc::new(AtomicUsize::new(0));
                let serial = AlignDriverOptions {
                    budget: HostBudget::explicit(1),
                    frames_done: Some(Arc::clone(&serial_counter)),
                    ..Default::default()
                };
                align_stack_with(
                    &algo,
                    stack,
                    &reader,
                    &anchor_cfg,
                    FailurePolicy::Strict,
                    &serial,
                )
                .expect("const algorithm never errors");
                assert_eq!(serial_counter.load(Ordering::SeqCst), frame_count);

                let pooled_counter = Arc::new(AtomicUsize::new(0));
                let pooled = AlignDriverOptions {
                    frames_done: Some(Arc::clone(&pooled_counter)),
                    ..Default::default()
                };
                align_stack_with(
                    &algo,
                    stack,
                    &reader,
                    &anchor_cfg,
                    FailurePolicy::Strict,
                    &pooled,
                )
                .expect("const algorithm never errors");
                assert_eq!(pooled_counter.load(Ordering::SeqCst), frame_count);

                let chain_counter = Arc::new(AtomicUsize::new(0));
                let chain = AlignDriverOptions {
                    frames_done: Some(Arc::clone(&chain_counter)),
                    ..Default::default()
                };
                align_stack_with(
                    &algo,
                    stack,
                    &reader,
                    &chain_cfg,
                    FailurePolicy::Strict,
                    &chain,
                )
                .expect("const algorithm never errors");
                assert_eq!(chain_counter.load(Ordering::SeqCst), frame_count);
            }
        }
    }

    /// Fails frame 1 with a plain error and frame 2 with a backend loss while
    /// an accelerator is attached, as two pooled workers do when one device
    /// loss reaches them in different shapes; converges every frame without
    /// one.
    struct LossReachesTwoFramesAlign;

    impl AlignAlgorithm for LossReachesTwoFramesAlign {
        fn id(&self) -> &str {
            "loss-reaches-two-frames"
        }
        fn display_name(&self) -> &str {
            "Loss Reaches Two Frames"
        }
        fn params(&self) -> &[ParamDesc] {
            &[]
        }
        fn align_frame(
            &self,
            ctx: &AlignContext<'_>,
            frame: usize,
            _cfg: &ParamValues,
        ) -> Result<AlignOutcome, AlgorithmError> {
            if ctx.align_backend().is_some() {
                match frame {
                    1 => {
                        return Err(AlgorithmError::Failed {
                            algorithm: "stub".to_string(),
                            detail: "readback failed".to_string(),
                        });
                    }
                    2 => {
                        return Err(AlgorithmError::BackendLost {
                            detail: "device lost".to_string(),
                        });
                    }
                    _ => {}
                }
            }
            Ok(converged(Transform::identity()))
        }
    }

    struct DecliningBackend;

    impl AlignBackend for DecliningBackend {
        fn open_ecc(
            &self,
            _request: &EccSessionRequest<'_>,
        ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
            Ok(None)
        }
    }

    #[test]
    fn a_backend_loss_in_a_pooled_batch_reruns_the_stage_whatever_frame_failed_first() {
        let reader = NullReader;
        let stack = StackContext::new(4, 8, 8, 1);
        let run = align_stack_with(
            &LossReachesTwoFramesAlign,
            stack,
            &reader,
            &cfg_with("anchor", "first"),
            FailurePolicy::Strict,
            &AlignDriverOptions {
                align_backend: Some(&DecliningBackend),
                ..Default::default()
            },
        )
        .expect("a loss anywhere in the batch reruns the stage on the CPU");
        assert!(run.rerun_reason.is_some());
        assert!(!run.backend_used);
    }
}
