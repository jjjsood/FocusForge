// SPDX-License-Identifier: GPL-3.0-or-later

//! A test-only alignment algorithm, registered through the production path.
//! See `crate::test_algorithms` for why this lives behind the
//! `test-algorithms` feature rather than `#[cfg(test)]`.

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, AlignAlgorithm, AlignContext};
use crate::align::Transform;
use crate::params::{ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::AlignRegistration;

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![ParamDesc {
        id: "max-shift".to_string(),
        label: "Maximum shift".to_string(),
        help: "dummy per-frame translation bound, in pixels".to_string(),
        kind: ParamKind::Int { min: 0, max: 256 },
        default: ParamValue::Int(16),
        auto: None,
    }]
});

/// A unit-struct alignment algorithm: identity for the reference frame, a
/// translation for every other frame -- the same shape as
/// `crates/core/src/algorithm/mod.rs`'s own test-only `DummyAlign`.
pub struct DummyAlign;

impl AlignAlgorithm for DummyAlign {
    fn id(&self) -> &str {
        "dummy-align"
    }

    fn display_name(&self) -> &str {
        "Dummy Align (test-only)"
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        cfg: &ParamValues,
    ) -> Result<Transform, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        if frame == ctx.reference() {
            Ok(Transform::identity())
        } else {
            let mut m = Transform::identity().m;
            m[0][2] = frame as f64 - ctx.reference() as f64;
            Ok(Transform { m })
        }
    }
}

inventory::submit! { AlignRegistration::new(&DummyAlign) }
