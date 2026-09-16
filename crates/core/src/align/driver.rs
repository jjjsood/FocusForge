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
//! keys with a default when absent -- the same pattern
//! `StackAlgorithm::tile_levels`'s default body already uses for `"levels"`
//! -- so an algorithm that declares neither still runs, anchored on the
//! middle frame. `Middle` resolves to `(frame_count - 1) / 2`.
//!
//! **Anchor** builds one [`AlignContext`] against the reference, calling `align_frame` for every
//! other frame across a driver-owned rayon pool from [`AlignDriverOptions::max_bytes_in_flight`],
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
//! composing onto the prefix -- the frame inherits the same accumulated
//! transform its predecessor had, which is not neighbour inheritance
//! (rejected outright): neighbour inheritance would attribute a transform
//! the solver never found, while this is the solver's own hop, set to "no
//! motion" and then composed exactly like every other hop.

use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, AlignStatus, TileReader};
use crate::align::Transform;
use crate::params::{ParamValue, ParamValues, StackContext};
use crate::tile::plan::DEFAULT_TILE_SIZE;

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

/// The shipped default alignment worker budget: 2 GiB.
///
/// Ships as a **share**, not an arithmetic ceiling:
/// [`AlignDriverOptions::max_bytes_in_flight`] is the field a caller with a
/// measured multi-worker budget raises, not a new constant here.
pub const DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT: usize = 2 * (1 << 30);

/// The alignment driver's own memory budget -- the model is
/// `crate::tile::TileSchedulerOptions`, minus its `scratch_base`: every
/// field here is a pure number, so (unlike that type) this one carries a
/// `Default`.
#[derive(Debug, Clone)]
pub struct AlignDriverOptions {
    /// How many bytes of concurrent [`bytes_per_registration`] this driver
    /// may hold live in the anchor arm. See `workers_for_budget` (private to
    /// this module) for the arithmetic this bounds.
    pub max_bytes_in_flight: usize,
}

impl Default for AlignDriverOptions {
    fn default() -> Self {
        AlignDriverOptions {
            max_bytes_in_flight: DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT,
        }
    }
}

/// The bytes the anchor arm's **shared** reference pyramid holds live, once
/// per run rather than once per worker -- a per-run constant instead of a
/// term inside every registration's own figure. The sibling of
/// [`bytes_per_registration`], which is what remains per worker once this
/// term is pulled out.
///
/// ```text
/// plane                  = width * height * 4    // f32 luma
/// bytes_shared_reference  = plane * 4 / 3         // level 0 + sum of levels 1..N, ratio 1/4 each
/// ```
///
/// The `4/3` is the infinite geometric sum `1 + 1/4 + 1/16 + ...`, so the
/// figure is an upper bound independent of `pyramid-depth`. At 8192x5464x3
/// this evaluates to ~238.7 MB; at the 512x512x3 fixture, ~1.4 MB.
/// `bytes_shared_reference(stack) + bytes_per_registration(stack,
/// DEFAULT_TILE_SIZE)` equals the 622 MB figure
/// `the_split_sums_to_622_mb_at_the_reference_scale` checks, so the two
/// functions can never silently drift into describing different physics.
pub fn bytes_shared_reference(stack: StackContext) -> u64 {
    let plane = stack.width as u64 * stack.height as u64 * 4;
    plane * 4 / 3
}

/// The transient peak bytes one anchor-mode registration holds live **on top
/// of** the one shared reference pyramid ([`bytes_shared_reference`]) -- the
/// sibling of `crate::tile::scheduler::bytes_per_tile_set`, `pub` because two
/// callers outside this module need it: the memory test
/// (`crates/core/tests/align_parallel_memory.rs`) sizes its budgets from it,
/// and a caller choosing [`AlignDriverOptions::max_bytes_in_flight`]
/// deliberately is choosing a multiple of it.
///
/// ```text
/// plane      = width * height * 4                      // f32 luma
/// assembler  = plane                                    // LumaAssembler::data
/// copy       = plane                                    // Plane::from_f32 copies
/// band       = width * band_rows * spp * 2
/// bytes_per_registration = assembler + copy + band
/// ```
///
/// `band_rows` is `DEFAULT_TILE_SIZE`, the band height
/// `PyramidalEcc::build_pyramid` streams with. The peak is **transient, not
/// steady-state**: it is the instant inside the frame-side `build_pyramid`
/// call when the shared reference pyramid, the assembler's `Vec<f32>`,
/// `Plane::from_f32`'s copy of it and one band buffer are all live at once.
/// Exactly one reference pyramid exists per run, not one per concurrent
/// registration -- its term lives in [`bytes_shared_reference`], a
/// per-**run** constant, not here. At 8192x5464x3 this evaluates to
/// ~383.3 MB (179.0 + 179.0 + 25.2); at the 512x512x3 fixture, ~3.67 MB.
/// + 179.0 + 25.2); at the 512x512x3 fixture, ~3.67 MB.
pub fn bytes_per_registration(stack: StackContext, band_rows: u32) -> u64 {
    let plane = stack.width as u64 * stack.height as u64 * 4;
    let assembler = plane;
    let copy = plane;
    let band = stack.width as u64 * band_rows as u64 * stack.samples_per_pixel as u64 * 2;
    assembler + copy + band
}

/// The pure arithmetic behind the anchor arm's worker count: mirrors
/// `crate::tile::scheduler::in_flight_for_budget` clamp for clamp -- floored
/// at 1 (never 0, so a budget smaller than one registration still makes
/// progress), capped at `available_parallelism` -- plus one clamp the
/// scheduler has no equivalent for: capped at `registrations`, the number of
/// non-reference frames there are to register.
///
/// `shared_bytes` (the run's one shared reference pyramid,
/// [`bytes_shared_reference`]) is subtracted from `max_bytes_in_flight`
/// **once**, before dividing by the per-worker `bytes_per_registration` --
/// `saturating_sub`, so a budget smaller than the shared pyramid floors to
/// one worker instead of underflowing.
/// additive.
fn workers_for_budget(
    shared_bytes: u64,
    bytes_per_registration: u64,
    max_bytes_in_flight: usize,
    available_parallelism: usize,
    registrations: usize,
) -> usize {
    let available_parallelism = available_parallelism.max(1);
    let registrations = registrations.max(1);
    let remaining = (max_bytes_in_flight as u64).saturating_sub(shared_bytes);
    let raw = if bytes_per_registration == 0 {
        available_parallelism as u64
    } else {
        (remaining / bytes_per_registration).max(1)
    };
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
) -> Result<(), AlgorithmError> {
    let mut previous = reference;
    let mut prefix = Transform::identity();
    for i in indices {
        let ctx = AlignContext::new(stack, previous, reader);
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
/// `options.max_bytes_in_flight` and [`bytes_per_registration`], built and
/// dropped inside this call -- it never coexists with the tile scheduler's
/// own pool, which (on the `stack` path) is not built until after this call
/// returns. When the budget floors to one worker, this runs today's serial
/// loop verbatim rather than building a pool at all, which is also what
/// keeps the byte-identity tests' reference side alive.
///
/// Chain mode is **serial by construction** -- see the module docs -- and
/// always reports `worker_limit: 1, peak_workers: 1`.
pub fn align_stack_with(
    algorithm: &dyn AlignAlgorithm,
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    failure: FailurePolicy,
    options: &AlignDriverOptions,
) -> Result<AlignRun, AlgorithmError> {
    let policy = reference_policy_from_cfg(cfg);
    let reference = reference_index(reference_frame_from_cfg(cfg), stack.frame_count);

    let mut frames: Vec<Option<FrameAlignment>> = vec![None; stack.frame_count];
    frames[reference] = Some(identity_alignment(reference));

    let (worker_limit, peak_workers) = match policy {
        ReferencePolicy::Anchor => {
            let registrations = stack.frame_count.saturating_sub(1);
            let shared_bytes = bytes_shared_reference(stack);
            let bytes = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
            let available_parallelism = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            let workers = workers_for_budget(
                shared_bytes,
                bytes,
                options.max_bytes_in_flight,
                available_parallelism,
                registrations,
            );

            // One `AlignContext` per run, built once before the branch --
            // `prepare_run` is called exactly here, at most once per run,
            // only in the anchor arm, so both sub-arms below share the same
            // prepared reference pyramid from this single line. The `Arc`
            // binding lives in this stack frame and is dropped when this
            // function returns: that is the whole lifetime story for "the
            // cache is scoped to one run", see the module docs.
            let ctx = AlignContext::new(stack, reference, reader);
            let prepared = algorithm.prepare_run(&ctx, cfg)?;
            let ctx = ctx.with_prepared(prepared.as_deref());

            if workers <= 1 {
                // Today's loop, verbatim: no pool is built at all, and this
                // is the byte-identity tests' reference side.
                for (i, slot) in frames.iter_mut().enumerate() {
                    if i == reference {
                        continue;
                    }
                    let outcome = algorithm.align_frame(&ctx, i, cfg)?;
                    *slot = Some(anchored_alignment(i, outcome, failure));
                }
                (1, 1)
            } else {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .thread_name(|i| format!("focusforge-align-{i}"))
                    .build()
                    .map_err(|source| AlgorithmError::Failed {
                        algorithm: "align_stack".to_string(),
                        detail: format!("alignment thread pool: {source}"),
                    })?;

                let live = AtomicUsize::new(0);
                let peak = AtomicUsize::new(0);

                // One `Result` per slot, `collect()`ed into a `Vec` that
                // `IndexedParallelIterator` restores to source order
                // regardless of completion order (rayon 1.12.0). **Not**
                // `try_for_each_init`: rayon 1.12.0 does not specify which
                // of several concurrent errors it returns
                // (`crate::tile::scheduler`'s own `RunStop` doc records the
                // same fact), so a post-loop scan in ascending index order
                // is what makes the returned error exactly the one the
                // serial loop's `?` would have hit first. The cost is a
                // failing run finishes every other frame before returning,
                // instead of short-circuiting -- `align_frame` errors are
                // malformed-params (refused before this loop) and I/O
                // failures, not the common case; divergence is an `Ok`
                // outcome, never an `Err`.
                let outcomes: Vec<Option<Result<FrameAlignment, AlgorithmError>>> =
                    pool.install(|| {
                        (0..stack.frame_count)
                            .into_par_iter()
                            .map(|i| {
                                if i == reference {
                                    return None;
                                }
                                let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
                                peak.fetch_max(cur, Ordering::SeqCst);
                                let result = algorithm
                                    .align_frame(&ctx, i, cfg)
                                    .map(|outcome| anchored_alignment(i, outcome, failure));
                                live.fetch_sub(1, Ordering::SeqCst);
                                Some(result)
                            })
                            .collect()
                    });

                for (i, outcome) in outcomes.into_iter().enumerate() {
                    if let Some(result) = outcome {
                        frames[i] = Some(result?);
                    }
                }

                (workers, peak.load(Ordering::SeqCst))
            }
        }
        ReferencePolicy::Chain => {
            run_chain_leg(
                algorithm,
                stack,
                reader,
                cfg,
                failure,
                &mut frames,
                (reference + 1)..stack.frame_count,
                reference,
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
            )?;
            (1, 1)
        }
    };

    let frames = frames
        .into_iter()
        .enumerate()
        .map(|(i, f)| f.unwrap_or_else(|| panic!("frame {i} was never visited by align_stack")))
        .collect();

    Ok(AlignRun {
        alignment: StackAlignment {
            reference,
            policy,
            frames,
        },
        worker_limit,
        peak_workers,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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
        // `shared_bytes = 0` is what proves the new parameter additive: the
        // expected value is unchanged.
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
        let workers = workers_for_budget(0, 1, usize::MAX, 4, 60);
        assert_eq!(workers, 4);
    }

    #[test]
    fn worker_budget_caps_at_registrations() {
        // The scheduler has no equivalent of this clamp: a tile plan always
        // outnumbers its workers, an 8-frame fixture on a 32-core host does
        // not.
        let workers = workers_for_budget(0, 1, usize::MAX, 32, 7);
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
        let budget = 250_000usize;
        let workers = workers_for_budget(0, bytes, budget, 1_000, 1_000);
        assert!(workers as u64 <= budget as u64 / bytes);
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
    fn the_split_sums_to_622_mb_at_the_reference_scale() {
        // `bytes_shared_reference(stack) + bytes_per_registration(stack,
        // DEFAULT_TILE_SIZE)` must equal the 622 MB total
        // `crate::align::pyramid`'s module docs quote.
        let stack = StackContext::new(61, 8192, 5464, 3);
        let sum = bytes_shared_reference(stack) + bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        let ratio = sum as f64 / 622_000_000f64;
        assert!(
            (ratio - 1.0).abs() < 0.01,
            "bytes_shared_reference + bytes_per_registration = {sum} is not within 1% of 622 MB"
        );
    }

    #[test]
    fn worker_budget_at_the_reference_scale() {
        // Pinned with an explicit `available_parallelism` and registration
        // count, so this assertion never depends on the host: the default
        // budget yields 4 workers at the reference scale.
        let stack = StackContext::new(61, 8192, 5464, 3);
        let shared = bytes_shared_reference(stack);
        let bytes = bytes_per_registration(stack, DEFAULT_TILE_SIZE);
        let workers = workers_for_budget(shared, bytes, DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT, 32, 60);
        assert_eq!(workers, 4, "the decision's own 4-workers figure");
    }

    #[test]
    fn bytes_per_registration_matches_the_pyramid() {
        let width = 512u32;
        let height = 512u32;
        let spp = 3u32;
        let depth = crate::align::max_levels(width, height).min(4);
        let base = crate::fuse::pyramid::Plane::from_f32(
            &vec![0.0f32; (width * height) as usize],
            width,
            height,
            1,
        )
        .unwrap();
        let pyramid = crate::align::AlignPyramid::from_luma(base, depth).unwrap();
        let pyramid_bytes: u64 = (0..pyramid.level_count())
            .map(|k| pyramid.level(k).data().len() as u64 * 4)
            .sum();
        let plane = width as u64 * height as u64 * 4;
        // The assembler's own buffer plus `Plane::from_f32`'s copy of it --
        // the frame-side figure `bytes_per_registration` owns.
        let assembler_and_copy = plane * 2;
        let band = width as u64 * DEFAULT_TILE_SIZE as u64 * spp as u64 * 2;
        let floor = assembler_and_copy + band;

        let stack = StackContext::new(8, width, height, spp);
        let bpr = bytes_per_registration(stack, DEFAULT_TILE_SIZE);

        assert!(
            bpr >= floor,
            "budget {bpr} must be >= the real assembler+copy+band figure {floor}"
        );
        let ratio = bpr as f64 / floor as f64;
        assert!(
            ratio <= 1.05,
            "budget {bpr} must be within 5% of the real figure {floor} (ratio {ratio})"
        );

        // `bytes_shared_reference` is the term that left: it must still be
        // an upper bound on the real, once-per-run pyramid's own size.
        let shared = bytes_shared_reference(stack);
        assert!(
            shared >= pyramid_bytes,
            "bytes_shared_reference {shared} must be >= the real pyramid figure {pyramid_bytes}"
        );
        let shared_ratio = shared as f64 / pyramid_bytes as f64;
        assert!(
            shared_ratio <= 1.05,
            "bytes_shared_reference {shared} must be within 5% of the real pyramid figure \
             {pyramid_bytes} (ratio {shared_ratio})"
        );
    }
}
