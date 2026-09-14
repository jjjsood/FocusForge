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
//! **Anchor** builds one [`AlignContext`] against the reference frame and
//! calls `align_frame` for every other frame, across a driver-owned rayon
//! pool sized from [`AlignDriverOptions::max_bytes_in_flight`] --
//! alignment owns its own pool, sized from its own budget, the way
//! `crate::tile::scheduler::TileScheduler` owns and sizes its own -- built
//! and dropped inside this call, so it never coexists with the tile
//! scheduler's pool downstream.
//! in both directions, rebuilding an `AlignContext` per hop against the
//! previous frame and composing `T_i = T_hop.compose(&T_previous)` -- hop
//! **after** prefix, since [`Transform::compose`] applies its right operand
//! first. Chaining is **serial by construction**: each hop needs the
//! previous hop's composed result before it can run, so there is no
//! independent unit of work rayon could schedule across -- a "parallel
//! chain" would be a different algorithm, not a faster version of this one.
//! A middle anchor also makes frame 0's transform non-identity, which is why
//! a reference-frame-is-identity test anywhere in this crate is a real
//! assertion rather than an accident.
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

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, AlignStatus, TileReader};
use crate::align::Transform;
use crate::params::{ParamValue, ParamValues, StackContext};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// `algorithm`, `cfg`'s reference policy and `failure`'s failure policy.
/// See the module docs.
pub fn align_stack(
    algorithm: &dyn AlignAlgorithm,
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    failure: FailurePolicy,
) -> Result<StackAlignment, AlgorithmError> {
    let policy = reference_policy_from_cfg(cfg);
    let reference = reference_index(reference_frame_from_cfg(cfg), stack.frame_count);

    let mut frames: Vec<Option<FrameAlignment>> = vec![None; stack.frame_count];
    frames[reference] = Some(identity_alignment(reference));

    match policy {
        ReferencePolicy::Anchor => {
            let ctx = AlignContext::new(stack, reference, reader);
            for (i, slot) in frames.iter_mut().enumerate() {
                if i == reference {
                    continue;
                }
                let outcome = algorithm.align_frame(&ctx, i, cfg)?;
                *slot = Some(anchored_alignment(i, outcome, failure));
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
        }
    }

    let frames = frames
        .into_iter()
        .enumerate()
        .map(|(i, f)| f.unwrap_or_else(|| panic!("frame {i} was never visited by align_stack")))
        .collect();

    Ok(StackAlignment {
        reference,
        policy,
        frames,
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
}
