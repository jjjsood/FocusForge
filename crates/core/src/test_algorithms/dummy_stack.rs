// SPDX-License-Identifier: GPL-3.0-or-later

//! A test-only stacking algorithm, registered through the production path
//! so `crate::registry` is exercised end to end before any real stacking
//! algorithm exists. See `crate::test_algorithms` for why this lives behind
//! the `test-algorithms` feature rather than `#[cfg(test)]`.

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::params::{EnumVariant, ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::StackRegistration;

static PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "threshold".to_string(),
            label: "Threshold".to_string(),
            help: "dummy per-pixel selection threshold".to_string(),
            kind: ParamKind::Float {
                min: 0.0,
                max: 1.0,
                step: 0.01,
                logarithmic: false,
            },
            default: ParamValue::Float(0.5),
            auto: Some(std::sync::Arc::new(|ctx: &crate::params::StackContext| {
                ParamValue::Float(1.0 / ctx.frame_count.max(1) as f64)
            })),
        },
        ParamDesc {
            id: "mode".to_string(),
            label: "Mode".to_string(),
            help: "dummy mode selector".to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "fast".to_string(),
                        label: "Fast".to_string(),
                    },
                    EnumVariant {
                        id: "slow".to_string(),
                        label: "Slow".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("fast".to_string()),
            auto: None,
        },
    ]
});

/// A unit-struct stacking algorithm whose `params()` borrows from a
/// file-local [`LazyLock`], the shape `crates/core/src/algorithm/mod.rs`'s
/// own test-only `DummyStack` uses -- const-constructible as a *value*,
/// while its schema stays owned data.
pub struct DummyStack;

impl StackAlgorithm for DummyStack {
    fn id(&self) -> &str {
        "dummy-stack"
    }

    fn display_name(&self) -> &str {
        "Dummy Stack (test-only)"
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let tile = ctx.frame(0)?.clone();
        let zmap = vec![0u16; (tile.rect.width * tile.rect.height) as usize];
        Ok(TileOutput { tile, zmap })
    }
}

inventory::submit! { StackRegistration::new(&DummyStack) }
