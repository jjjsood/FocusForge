// SPDX-License-Identifier: GPL-3.0-or-later

//! A test-only stacking algorithm, registered through the same production
//! path real algorithms use, so `crate::registry` is exercised end to end
//! with no dependency on any specific real algorithm. See
//! `crate::test_algorithms` for why this lives behind the
//! `test-algorithms` feature rather than `#[cfg(test)]`.

use std::sync::LazyLock;

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput};
use crate::params::{EnumVariant, ParamDesc, ParamKind, ParamValue, ParamValues};
use crate::registry::StackRegistration;
use crate::tile::{Sample, SampleFormat, Tile};

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
            advanced: false,
            auto: Some(std::sync::Arc::new(|ctx: &crate::params::StackContext| {
                ParamValue::Float(1.0 / ctx.frame_count.max(1) as f64)
            })),
            host_default: None,
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
            advanced: false,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "workers".to_string(),
            label: "Workers".to_string(),
            help: "dummy worker-count parameter whose default is derived from \
                   the host rather than fixed by the descriptor; no shipped \
                   algorithm declares one."
                .to_string(),
            kind: ParamKind::Int { min: 1, max: 1024 },
            default: ParamValue::Int(1),
            advanced: false,
            auto: None,
            host_default: Some(std::sync::Arc::new(|facts: &crate::host::HostFacts| {
                ParamValue::Int(facts.logical_cores() as i64)
            })),
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

    fn test_only(&self) -> bool {
        true
    }

    fn params(&self) -> &[ParamDesc] {
        &PARAMS
    }

    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        crate::params::validate(self.params(), cfg)?;
        let samples_per_pixel = ctx.samples_per_pixel()?;
        let samples = match ctx.sample_format() {
            SampleFormat::U16 => first_frame::<u16>(ctx)?,
            SampleFormat::F32 => first_frame::<f32>(ctx)?,
        };
        let rect = ctx.rect();
        let zmap = vec![0u16; (rect.width * rect.height) as usize];
        Ok(TileOutput {
            tile: Tile {
                rect,
                samples_per_pixel,
                samples,
            },
            zmap,
        })
    }
}

/// Frame 0 of `ctx`, narrowed to `u16` once.
fn first_frame<S: Sample>(ctx: &TileCtx<'_>) -> Result<Vec<u16>, AlgorithmError> {
    let mut first = None;
    ctx.for_each_frame_as::<S>(|f, samples, _| {
        if f == 0 {
            first = Some(samples.iter().map(|s| u16::narrow(s.widen())).collect());
        }
        Ok(())
    })?;
    Ok(first.expect("a context with a frame 0 visits it"))
}

inventory::submit! { StackRegistration::new(&DummyStack) }
