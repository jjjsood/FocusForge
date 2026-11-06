// SPDX-License-Identifier: GPL-3.0-or-later

//! A test-only alignment algorithm, registered through the production path.
//! See `crate::test_algorithms` for why this lives behind the
//! `test-algorithms` feature rather than `#[cfg(test)]`.

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus};
use crate::align::{DivergenceReason, Transform};
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::AlignRegistration;

/// The `fail-frame` value that fails no frame.
const NO_FAIL_FRAME: i64 = -1;

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "max-shift".to_string(),
            label: "Maximum shift".to_string(),
            help: "dummy per-frame translation bound, in pixels".to_string(),
            kind: ParamKind::Int { min: 0, max: 256 },
            default: ParamValue::Int(16),
            advanced: false,
            auto: None,
            host_default: None,
            stack_max: None,
        },
        ParamDesc {
            id: "fail-frame".to_string(),
            label: "Failing frame".to_string(),
            help: "index of the frame whose registration reports a divergence; -1 fails none, \
                   and the reference frame never fails"
                .to_string(),
            kind: ParamKind::Int {
                min: NO_FAIL_FRAME,
                max: 65535,
            },
            default: ParamValue::Int(NO_FAIL_FRAME),
            advanced: true,
            auto: None,
            host_default: None,
            stack_max: None,
        },
    ]
});

/// A unit-struct alignment algorithm: identity for the reference frame, a
/// translation for every other frame. With `fail-frame` set, that frame
/// instead reports a diverged registration with an identity transform.
pub struct DummyAlign;

impl AlignAlgorithm for DummyAlign {
    fn id(&self) -> &str {
        "dummy-align"
    }

    fn display_name(&self) -> &str {
        "Dummy Align (test-only)"
    }

    fn test_only(&self) -> bool {
        true
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let fail_frame = cfg
            .get("fail-frame")
            .and_then(ParamValue::as_i64)
            .unwrap_or(NO_FAIL_FRAME);
        let fails = frame != ctx.reference() && frame as i64 == fail_frame;
        let transform = if frame == ctx.reference() || fails {
            Transform::identity()
        } else {
            let mut m = Transform::identity().m;
            m[0][2] = frame as f64 - ctx.reference() as f64;
            Transform { m }
        };
        Ok(AlignOutcome {
            transform,
            status: if fails {
                AlignStatus::Diverged(DivergenceReason::ValidationFailed)
            } else {
                AlignStatus::Converged
            },
            iterations: 1,
            residual: 1.0,
        })
    }
}

inventory::submit! { AlignRegistration::new(&DummyAlign) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::TileReader;
    use crate::params::{ParamViolation, StackContext};
    use crate::tile::Rect;

    /// A reader that fails the test when any pixel is requested: the
    /// algorithm under test never reads one.
    struct NoPixels;

    impl TileReader for NoPixels {
        fn read_tile_into(
            &self,
            frame: usize,
            _rect: Rect,
            _out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            panic!("dummy-align read a tile of frame {frame}");
        }
    }

    const REFERENCE: usize = 2;

    fn outcome(frame: usize, fail_frame: Option<i64>) -> Result<AlignOutcome, AlgorithmError> {
        let ctx = AlignContext::new(StackContext::new(5, 4, 4, 3), REFERENCE, &NoPixels);
        let mut cfg = ParamValues::new();
        if let Some(value) = fail_frame {
            cfg.insert("fail-frame".to_string(), ParamValue::Int(value));
        }
        DummyAlign.align_frame(&ctx, frame, &cfg)
    }

    #[test]
    fn fail_frame_defaults_to_no_frame() {
        let fail_frame = PARAMS.iter().find(|d| d.id == "fail-frame").unwrap();
        assert_eq!(fail_frame.default, ParamValue::Int(-1));
        assert!(
            fail_frame.advanced,
            "a test seam stays out of the basic panel"
        );
        for frame in 0..5 {
            let unset = outcome(frame, None).unwrap();
            let explicit = outcome(frame, Some(-1)).unwrap();
            assert_eq!(unset.status, AlignStatus::Converged, "frame {frame}");
            assert_eq!(unset.transform.m[0][2], frame as f64 - REFERENCE as f64);
            assert_eq!(explicit.status, unset.status);
            assert_eq!(explicit.transform, unset.transform);
        }
    }

    #[test]
    fn fail_frame_diverges_only_that_frame() {
        for frame in 0..5 {
            let out = outcome(frame, Some(3)).unwrap();
            if frame == 3 {
                assert_eq!(
                    out.status,
                    AlignStatus::Diverged(DivergenceReason::ValidationFailed)
                );
                assert_eq!(out.transform, Transform::identity());
            } else {
                assert_eq!(out.status, AlignStatus::Converged, "frame {frame}");
            }
        }
    }

    #[test]
    fn fail_frame_on_the_reference_is_a_no_op() {
        let out = outcome(REFERENCE, Some(REFERENCE as i64)).unwrap();
        assert_eq!(out.status, AlignStatus::Converged);
        assert_eq!(out.transform, Transform::identity());
    }

    #[test]
    fn fail_frame_outside_its_range_is_refused() {
        for value in [-2, 65_536] {
            let Err(AlgorithmError::Params(err)) = outcome(1, Some(value)) else {
                panic!("fail-frame={value} must be refused as a parameter error");
            };
            assert!(
                matches!(
                    err.violations(),
                    [ParamViolation::OutOfRange { id, .. }] if id == "fail-frame"
                ),
                "{err:?}"
            );
        }
    }
}
