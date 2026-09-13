// SPDX-License-Identifier: GPL-3.0-or-later

//! The parameter schema: the single source of truth for every algorithm
//! setting in the project.
//!
//! - A parameter that exists as a struct field but not as a [`ParamDesc`] is
//!   invisible to both the CLI and the GUI — every setting an algorithm
//!   exposes must be declared here.
//! - `core` computes every parameter in `f64`. The single cast to `f32`
//!   happens once, at GPU upload, performed identically by the CPU twin, so
//!   both twins read the value through the same boundary. This sentence is
//!   the only place that rule is written down.
//! - [`ParamValue`] is `#[serde(untagged)]`, and the widening rule described
//!   on it is part of the on-disk contract the checkpoint op log inherits:
//!   it is not merely a convenience of this module.
//! - [`ParamValues`] is a [`BTreeMap`] rather than a `HashMap` so its
//!   serialized bytes are reproducible between runs, whatever order the
//!   keys were inserted in.

mod error;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use error::{ParamError, ParamViolation};

/// One value an [`ParamKind::Enum`] descriptor can take: `id` is the
/// `--set mode=<id>` token and the key a schema instance compares against,
/// `label` is what a GUI combo box shows. Decoupled from day one so a label
/// can be reworded, or translated, without a schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumVariant {
    pub id: String,
    pub label: String,
}

/// The shape one parameter's value can take, plus the bounds it must
/// satisfy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamKind {
    Float {
        min: f64,
        max: f64,
        step: f64,
        logarithmic: bool,
    },
    Int {
        min: i64,
        max: i64,
    },
    Bool,
    Enum {
        variants: Vec<EnumVariant>,
    },
}

impl ParamKind {
    fn kind_name(&self) -> &'static str {
        match self {
            ParamKind::Float { .. } => "float",
            ParamKind::Int { .. } => "int",
            ParamKind::Bool => "bool",
            ParamKind::Enum { .. } => "enum",
        }
    }
}

/// The `StackContext` an `auto` hook reads to derive a value from the stack
/// being processed. Declared here because the hook's signature below cannot
/// be written without it, and an algorithm module's `params()` return type
/// depends on it, not the reverse.
///
/// `#[non_exhaustive]`: extended **in place** as further algorithms need
/// fields. It must not move, or this module's own tests break.
///
/// The three fields here are exactly what [`crate::io::TiffStack`] already
/// exposes: `frame_count` from `TiffStack::len()`, `width`/`height` from
/// `TiffStack::dimensions()`. `core::params` does not depend on `core::io`;
/// the caller wires the two together by constructing a `StackContext` from
/// an open stack.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackContext {
    pub frame_count: usize,
    pub width: u32,
    pub height: u32,
}

impl StackContext {
    pub fn new(frame_count: usize, width: u32, height: u32) -> Self {
        StackContext {
            frame_count,
            width,
            height,
        }
    }
}

/// A closure deriving a value for one parameter from the stack being
/// processed. Aliased so the type is written once and so
/// `clippy::type_complexity` has nothing to fire on at the field
/// declaration.
pub type AutoFn = Arc<dyn Fn(&StackContext) -> ParamValue + Send + Sync>;

/// One algorithm setting, described as data rather than as a struct field:
/// its identity, its allowed shape, its default, and an optional hook that
/// derives a value from the stack instead of the plain default.
///
/// Every field is owned. A `ParamDesc` holding `&'static str` cannot
/// implement `Deserialize` at all, and both a round-trip through JSON and
/// `algorithms --json` need the schema to survive as data.
#[derive(Clone, Serialize, Deserialize)]
pub struct ParamDesc {
    pub id: String,
    pub label: String,
    pub help: String,
    pub kind: ParamKind,
    pub default: ParamValue,
    /// Not data, so never serialized: dropped to `None` on deserialize.
    #[serde(skip)]
    pub auto: Option<AutoFn>,
}

impl fmt::Debug for ParamDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParamDesc")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("help", &self.help)
            .field("kind", &self.kind)
            .field("default", &self.default)
            .field("auto", &self.auto.as_ref().map(|_| "..."))
            .finish()
    }
}

impl PartialEq for ParamDesc {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.label == other.label
            && self.help == other.help
            && self.kind == other.kind
            && self.default == other.default
    }
}

/// One concrete value for one parameter.
///
/// `#[serde(untagged)]`: serde resolves an untagged enum by buffering the
/// content and trying variants in **declaration order**
/// (serde-rs/serde#2661), which is why `Int` is declared before `Float` —
/// that ordering is what makes a bare `2` deserialize as `Int(2)` and a
/// bare `2.0` deserialize as `Float(2.0)`. Do not reorder these variants:
/// the test `untagged_number_widening_is_pinned` pins this behaviour against
/// the locked serde/serde_json versions, and reordering silently changes
/// which JSON numbers become which variant.
///
/// The widening rule this enables is part of the on-disk contract: a JSON
/// integer is accepted where a descriptor declares `Float` (widened to
/// `f64`), but a JSON float is never accepted where a descriptor declares
/// `Int`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Enum(String),
}

impl ParamValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ParamValue::Int(i) => Some(*i as f64),
            ParamValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ParamValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ParamValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_enum(&self) -> Option<&str> {
        match self {
            ParamValue::Enum(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            ParamValue::Int(_) => "int",
            ParamValue::Float(_) => "float",
            ParamValue::Bool(_) => "bool",
            ParamValue::Enum(_) => "enum",
        }
    }

    /// Parses `text` against `desc`'s declared kind, returning a value that
    /// already satisfies the descriptor completely — kind, finiteness,
    /// range and enum membership — so a caller gets one rendered error per
    /// mistake rather than a parse error followed by a validation error.
    pub fn parse(desc: &ParamDesc, text: &str) -> Result<ParamValue, ParamError> {
        let value = match &desc.kind {
            ParamKind::Bool => match text.to_ascii_lowercase().as_str() {
                "true" => ParamValue::Bool(true),
                "false" => ParamValue::Bool(false),
                _ => {
                    return Err(ParamError::new(vec![ParamViolation::Unparsable {
                        id: desc.id.clone(),
                        kind: "bool",
                        text: text.to_string(),
                    }]));
                }
            },
            ParamKind::Int { .. } => match text.parse::<i64>() {
                Ok(i) => ParamValue::Int(i),
                Err(_) => {
                    return Err(ParamError::new(vec![ParamViolation::Unparsable {
                        id: desc.id.clone(),
                        kind: "int",
                        text: text.to_string(),
                    }]));
                }
            },
            ParamKind::Float { .. } => match text.parse::<f64>() {
                Ok(f) => ParamValue::Float(f),
                Err(_) => {
                    return Err(ParamError::new(vec![ParamViolation::Unparsable {
                        id: desc.id.clone(),
                        kind: "float",
                        text: text.to_string(),
                    }]));
                }
            },
            ParamKind::Enum { .. } => ParamValue::Enum(text.to_string()),
        };
        if let Err(violation) = check(desc, &value) {
            return Err(ParamError::new(vec![violation]));
        }
        Ok(value)
    }
}

/// A complete set of concrete values for a schema, keyed by parameter id.
/// A [`BTreeMap`] rather than a `HashMap` so its serialized bytes are
/// reproducible between runs regardless of insertion order — needed for
/// content-addressed checkpoint blobs and `--json` golden output.
pub type ParamValues = BTreeMap<String, ParamValue>;

/// Where a resolved value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    Explicit,
    Auto,
    Default,
}

/// One resolved parameter value plus its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub value: ParamValue,
    pub provenance: Provenance,
}

/// A schema fully resolved against a partial set of explicit values and a
/// [`StackContext`], keyed by parameter id.
pub type ResolvedParams = BTreeMap<String, Resolved>;

/// Checks one concrete value against one descriptor, applying the widening
/// rule: an `Int` value is accepted where the descriptor declares `Float`
/// (widened to `f64`, then range-checked); a `Float` value is never
/// accepted where the descriptor declares `Int`, in either direction.
fn check(desc: &ParamDesc, value: &ParamValue) -> Result<(), ParamViolation> {
    match (&desc.kind, value) {
        (ParamKind::Float { min, max, .. }, ParamValue::Float(f)) => {
            if !f.is_finite() {
                return Err(ParamViolation::NotFinite {
                    id: desc.id.clone(),
                    value: *f,
                });
            }
            if *f < *min || *f > *max {
                return Err(ParamViolation::OutOfRange {
                    id: desc.id.clone(),
                    value: *f,
                    min: *min,
                    max: *max,
                });
            }
            Ok(())
        }
        (ParamKind::Float { min, max, .. }, ParamValue::Int(i)) => {
            let f = *i as f64;
            if f < *min || f > *max {
                return Err(ParamViolation::OutOfRange {
                    id: desc.id.clone(),
                    value: f,
                    min: *min,
                    max: *max,
                });
            }
            Ok(())
        }
        (ParamKind::Int { min, max }, ParamValue::Int(i)) => {
            if i < min || i > max {
                return Err(ParamViolation::OutOfRange {
                    id: desc.id.clone(),
                    value: *i as f64,
                    min: *min as f64,
                    max: *max as f64,
                });
            }
            Ok(())
        }
        (ParamKind::Bool, ParamValue::Bool(_)) => Ok(()),
        (ParamKind::Enum { variants }, ParamValue::Enum(s)) => {
            if variants.iter().any(|v| &v.id == s) {
                Ok(())
            } else {
                Err(ParamViolation::UnknownVariant {
                    id: desc.id.clone(),
                    value: s.clone(),
                    allowed: variants.iter().map(|v| v.id.clone()).collect(),
                })
            }
        }
        (kind, value) => Err(ParamViolation::TypeMismatch {
            id: desc.id.clone(),
            expected: kind.kind_name(),
            found: value.kind_name(),
        }),
    }
}

/// Validates a complete set of values against a schema, reporting every
/// offending parameter in one pass. A key the schema declares but `values`
/// omits is not a violation — that is what defaults are for.
pub fn validate(schema: &[ParamDesc], values: &ParamValues) -> Result<(), ParamError> {
    let mut violations = Vec::new();
    for (key, value) in values {
        match schema.iter().find(|d| &d.id == key) {
            Some(desc) => {
                if let Err(v) = check(desc, value) {
                    violations.push(v);
                }
            }
            None => violations.push(ParamViolation::UnknownKey {
                key: key.clone(),
                declared: schema.iter().map(|d| d.id.clone()).collect(),
            }),
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(ParamError::new(violations))
    }
}

/// Resolves a schema plus a partial set of explicit values plus a
/// [`StackContext`] into a complete set in which every entry knows where it
/// came from: an explicit value wins, `auto` wins over the plain default,
/// and every resolved value is normalized to the descriptor's declared
/// kind — an `Int` accepted against a `Float` descriptor is stored as
/// `Float`, so no consumer has to know the widening rule existed.
pub fn resolve(
    schema: &[ParamDesc],
    values: &ParamValues,
    ctx: &StackContext,
) -> Result<ResolvedParams, ParamError> {
    let mut violations = Vec::new();
    let mut resolved = BTreeMap::new();
    for desc in schema {
        let (value, provenance) = if let Some(v) = values.get(&desc.id) {
            (v.clone(), Provenance::Explicit)
        } else if let Some(auto) = &desc.auto {
            (auto(ctx), Provenance::Auto)
        } else {
            (desc.default.clone(), Provenance::Default)
        };
        // Checked exactly once per parameter: an `Auto` value's own violation
        // is wrapped so a computed-out-of-range value names the hook, not
        // just the range.
        match check(desc, &value) {
            Ok(()) => {
                let normalized = normalize(desc, value);
                resolved.insert(
                    desc.id.clone(),
                    Resolved {
                        value: normalized,
                        provenance,
                    },
                );
            }
            Err(violation) => violations.push(if provenance == Provenance::Auto {
                ParamViolation::Auto {
                    id: desc.id.clone(),
                    source: Box::new(violation),
                }
            } else {
                violation
            }),
        }
    }
    if violations.is_empty() {
        Ok(resolved)
    } else {
        Err(ParamError::new(violations))
    }
}

/// Bridges [`resolve`]'s output to [`crate::algorithm::StackAlgorithm::fuse`]'s
/// input: drops provenance, keeping only each parameter's value. `fuse`
/// takes a fully resolved [`ParamValues`], and this is the one place that
/// conversion happens, so the CLI, `crate::fuse::consumer::FusionConsumer`
/// and every test agree on what "resolved" means rather than each
/// hand-rolling the same map.
pub fn resolved_values(resolved: &ResolvedParams) -> ParamValues {
    resolved
        .iter()
        .map(|(id, r)| (id.clone(), r.value.clone()))
        .collect()
}

/// Normalizes an accepted value to its descriptor's declared kind — the
/// widening rule made permanent: an `Int` accepted against a `Float`
/// descriptor becomes a `Float`.
fn normalize(desc: &ParamDesc, value: ParamValue) -> ParamValue {
    match (&desc.kind, value) {
        (ParamKind::Float { .. }, ParamValue::Int(i)) => ParamValue::Float(i as f64),
        (_, v) => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float_desc(id: &str, min: f64, max: f64, default: f64) -> ParamDesc {
        ParamDesc {
            id: id.to_string(),
            label: id.to_string(),
            help: format!("{id} help"),
            kind: ParamKind::Float {
                min,
                max,
                step: 0.1,
                logarithmic: false,
            },
            default: ParamValue::Float(default),
            auto: None,
        }
    }

    fn int_desc(id: &str, min: i64, max: i64, default: i64) -> ParamDesc {
        ParamDesc {
            id: id.to_string(),
            label: id.to_string(),
            help: format!("{id} help"),
            kind: ParamKind::Int { min, max },
            default: ParamValue::Int(default),
            auto: None,
        }
    }

    fn bool_desc(id: &str, default: bool) -> ParamDesc {
        ParamDesc {
            id: id.to_string(),
            label: id.to_string(),
            help: format!("{id} help"),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(default),
            auto: None,
        }
    }

    fn enum_desc(id: &str, variants: &[(&str, &str)], default: &str) -> ParamDesc {
        ParamDesc {
            id: id.to_string(),
            label: id.to_string(),
            help: format!("{id} help"),
            kind: ParamKind::Enum {
                variants: variants
                    .iter()
                    .map(|(id, label)| EnumVariant {
                        id: id.to_string(),
                        label: label.to_string(),
                    })
                    .collect(),
            },
            default: ParamValue::Enum(default.to_string()),
            auto: None,
        }
    }

    #[test]
    fn descriptor_kinds_and_defaults() {
        let f = float_desc("threshold", 0.0, 10.0, 2.5);
        assert_eq!(f.default, ParamValue::Float(2.5));
        let i = int_desc("radius", 0, 100, 3);
        assert_eq!(i.default, ParamValue::Int(3));
        let b = bool_desc("enabled", true);
        assert_eq!(b.default, ParamValue::Bool(true));
        let e = enum_desc("mode", &[("fast", "Fast"), ("slow", "Slow")], "fast");
        assert_eq!(e.default, ParamValue::Enum("fast".to_string()));
        match &e.kind {
            ParamKind::Enum { variants } => {
                assert_eq!(variants[0].id, "fast");
                assert_eq!(variants[0].label, "Fast");
            }
            _ => panic!("expected Enum kind"),
        }
    }

    #[test]
    fn rejects_out_of_range_float() {
        let schema = vec![float_desc("threshold", 0.0, 10.0, 2.5)];
        let mut values = ParamValues::new();
        values.insert("threshold".to_string(), ParamValue::Float(20.0));
        let err = validate(&schema, &values).unwrap_err();
        assert_eq!(
            err.violations(),
            &[ParamViolation::OutOfRange {
                id: "threshold".to_string(),
                value: 20.0,
                min: 0.0,
                max: 10.0,
            }]
        );
        assert!(err.to_string().contains("threshold"));
        assert!(err.to_string().contains("20"));
        assert!(err.to_string().contains("0"));
        assert!(err.to_string().contains("10"));
    }

    #[test]
    fn rejects_unknown_enum_variant() {
        let schema = vec![enum_desc(
            "mode",
            &[("fast", "Fast"), ("slow", "Slow")],
            "fast",
        )];
        let mut values = ParamValues::new();
        values.insert("mode".to_string(), ParamValue::Enum("turbo".to_string()));
        let err = validate(&schema, &values).unwrap_err();
        assert_eq!(
            err.violations(),
            &[ParamViolation::UnknownVariant {
                id: "mode".to_string(),
                value: "turbo".to_string(),
                allowed: vec!["fast".to_string(), "slow".to_string()],
            }]
        );
        assert!(err.to_string().contains("fast"));
        assert!(err.to_string().contains("slow"));
    }

    #[test]
    fn rejects_type_mismatch() {
        let schema = vec![
            float_desc("threshold", 0.0, 10.0, 2.5),
            int_desc("radius", 0, 100, 3),
        ];
        let mut values = ParamValues::new();
        values.insert("radius".to_string(), ParamValue::Float(2.0));
        let err = validate(&schema, &values).unwrap_err();
        assert_eq!(
            err.violations(),
            &[ParamViolation::TypeMismatch {
                id: "radius".to_string(),
                expected: "int",
                found: "float",
            }]
        );
    }

    #[test]
    fn rejects_unknown_key() {
        let schema = vec![float_desc("threshold", 0.0, 10.0, 2.5)];
        let mut values = ParamValues::new();
        values.insert("nonexistent".to_string(), ParamValue::Float(1.0));
        let err = validate(&schema, &values).unwrap_err();
        assert_eq!(
            err.violations(),
            &[ParamViolation::UnknownKey {
                key: "nonexistent".to_string(),
                declared: vec!["threshold".to_string()],
            }]
        );
    }

    #[test]
    fn resolves_from_defaults() {
        let schema = vec![
            float_desc("threshold", 0.0, 10.0, 2.5),
            int_desc("radius", 0, 100, 3),
        ];
        let values = ParamValues::new();
        let ctx = StackContext::new(61, 8192, 5464);
        let resolved = resolve(&schema, &values, &ctx).unwrap();
        assert_eq!(resolved["threshold"].value, ParamValue::Float(2.5));
        assert_eq!(resolved["threshold"].provenance, Provenance::Default);
        assert_eq!(resolved["radius"].value, ParamValue::Int(3));
        assert_eq!(resolved["radius"].provenance, Provenance::Default);
    }

    #[test]
    fn explicit_wins_over_auto_wins_over_default() {
        let mut auto_desc = float_desc("threshold", 0.0, 10.0, 2.5);
        auto_desc.auto = Some(Arc::new(|ctx: &StackContext| {
            ParamValue::Float(ctx.frame_count as f64)
        }));
        let schema = vec![auto_desc];
        let ctx = StackContext::new(4, 100, 100);

        // Nothing explicit: resolves to the auto hook's value.
        let values = ParamValues::new();
        let resolved = resolve(&schema, &values, &ctx).unwrap();
        assert_eq!(resolved["threshold"].value, ParamValue::Float(4.0));

        // Explicit value wins over auto.
        let mut values = ParamValues::new();
        values.insert("threshold".to_string(), ParamValue::Float(7.0));
        let resolved = resolve(&schema, &values, &ctx).unwrap();
        assert_eq!(resolved["threshold"].value, ParamValue::Float(7.0));
    }

    #[test]
    fn provenance_is_queryable() {
        let mut auto_desc = float_desc("threshold", 0.0, 10.0, 2.5);
        auto_desc.auto = Some(Arc::new(|ctx: &StackContext| {
            ParamValue::Float(ctx.frame_count as f64)
        }));
        let schema = vec![auto_desc];
        let ctx = StackContext::new(4, 100, 100);

        let mut values = ParamValues::new();
        values.insert("threshold".to_string(), ParamValue::Float(7.0));
        let resolved = resolve(&schema, &values, &ctx).unwrap();
        assert_eq!(resolved["threshold"].provenance, Provenance::Explicit);

        let resolved = resolve(&schema, &ParamValues::new(), &ctx).unwrap();
        assert_eq!(resolved["threshold"].provenance, Provenance::Auto);
    }

    #[test]
    fn rejects_auto_hook_value_outside_range() {
        let mut auto_desc = float_desc("threshold", 0.0, 10.0, 2.5);
        auto_desc.auto = Some(Arc::new(|_ctx: &StackContext| ParamValue::Float(999.0)));
        let schema = vec![auto_desc];
        let ctx = StackContext::new(4, 100, 100);
        let err = resolve(&schema, &ParamValues::new(), &ctx).unwrap_err();
        assert_eq!(err.violations().len(), 1);
        match &err.violations()[0] {
            ParamViolation::Auto { id, source } => {
                assert_eq!(id, "threshold");
                match source.as_ref() {
                    ParamViolation::OutOfRange { value, .. } => assert_eq!(*value, 999.0),
                    other => panic!("expected OutOfRange, got {other:?}"),
                }
            }
            other => panic!("expected Auto, got {other:?}"),
        }
    }

    #[test]
    fn param_values_round_trip_serde() {
        let mut values = ParamValues::new();
        values.insert("threshold".to_string(), ParamValue::Float(2.5));
        values.insert("radius".to_string(), ParamValue::Int(3));
        values.insert("enabled".to_string(), ParamValue::Bool(true));
        values.insert("mode".to_string(), ParamValue::Enum("fast".to_string()));
        let json = serde_json::to_string(&values).unwrap();
        let round_tripped: ParamValues = serde_json::from_str(&json).unwrap();
        assert_eq!(values, round_tripped);
    }

    #[test]
    fn param_values_serialize_in_key_order() {
        let mut values = ParamValues::new();
        values.insert("b".to_string(), ParamValue::Int(2));
        values.insert("a".to_string(), ParamValue::Float(2.5));
        let json = serde_json::to_string(&values).unwrap();
        assert_eq!(json, r#"{"a":2.5,"b":2}"#);
    }

    #[test]
    fn untagged_number_widening_is_pinned() {
        let int_value: ParamValue = serde_json::from_str("2").unwrap();
        assert_eq!(int_value, ParamValue::Int(2));
        let float_value: ParamValue = serde_json::from_str("2.0").unwrap();
        assert_eq!(float_value, ParamValue::Float(2.0));

        let float_desc = float_desc("threshold", 0.0, 10.0, 2.5);
        assert!(check(&float_desc, &ParamValue::Int(2)).is_ok());

        let int_desc = int_desc("radius", 0, 10, 3);
        assert!(matches!(
            check(&int_desc, &ParamValue::Float(2.0)),
            Err(ParamViolation::TypeMismatch { .. })
        ));
    }

    #[test]
    fn rejects_non_finite_float() {
        let schema = vec![float_desc("threshold", 0.0, 10.0, 2.5)];
        let mut values = ParamValues::new();
        values.insert("threshold".to_string(), ParamValue::Float(f64::NAN));
        let err = validate(&schema, &values).unwrap_err();
        assert!(matches!(
            err.violations()[0],
            ParamViolation::NotFinite { .. }
        ));

        let desc = float_desc("threshold", 0.0, 10.0, 2.5);
        let err = ParamValue::parse(&desc, "inf").unwrap_err();
        assert!(matches!(
            err.violations()[0],
            ParamViolation::NotFinite { .. }
        ));
    }

    #[test]
    fn parses_from_descriptor() {
        let f = float_desc("threshold", 0.0, 10.0, 2.5);
        assert_eq!(ParamValue::parse(&f, "2").unwrap(), ParamValue::Float(2.0));

        let i = int_desc("radius", 0, 100, 3);
        assert_eq!(ParamValue::parse(&i, "2").unwrap(), ParamValue::Int(2));
        assert!(ParamValue::parse(&i, "2.0").is_err());

        // A parsed value is already range-checked.
        assert!(ParamValue::parse(&f, "999").is_err());
    }

    #[test]
    fn reports_every_offender() {
        let schema = vec![
            float_desc("threshold", 0.0, 10.0, 2.5),
            int_desc("radius", 0, 100, 3),
        ];
        let mut values = ParamValues::new();
        values.insert("threshold".to_string(), ParamValue::Float(999.0));
        values.insert("radius".to_string(), ParamValue::Float(2.0));
        values.insert("nonexistent".to_string(), ParamValue::Bool(true));
        let err = validate(&schema, &values).unwrap_err();
        assert_eq!(err.violations().len(), 3);
    }

    #[test]
    fn debug_and_partial_eq_ignore_the_auto_hook() {
        let mut a = float_desc("threshold", 0.0, 10.0, 2.5);
        let mut b = a.clone();
        a.auto = Some(Arc::new(|_ctx: &StackContext| ParamValue::Float(1.0)));
        b.auto = Some(Arc::new(|_ctx: &StackContext| ParamValue::Float(2.0)));
        assert_eq!(a, b);
        let debug_a = format!("{a:?}");
        let debug_b = format!("{b:?}");
        assert!(debug_a.contains("threshold"));
        assert!(debug_b.contains("threshold"));
    }

    #[test]
    fn resolved_values_survives_an_auto_value() {
        let mut auto_desc = float_desc("threshold", 0.0, 10.0, 2.5);
        auto_desc.auto = Some(Arc::new(|ctx: &StackContext| {
            ParamValue::Float(ctx.frame_count as f64)
        }));
        let schema = vec![auto_desc, int_desc("radius", 0, 100, 3)];
        let ctx = StackContext::new(4, 100, 100);
        let resolved = resolve(&schema, &ParamValues::new(), &ctx).unwrap();
        assert_eq!(resolved["threshold"].provenance, Provenance::Auto);

        let values = resolved_values(&resolved);
        assert_eq!(values.get("threshold"), Some(&ParamValue::Float(4.0)));
        assert_eq!(values.get("radius"), Some(&ParamValue::Int(3)));
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn param_desc_round_trip_drops_auto() {
        let mut desc = float_desc("threshold", 0.0, 10.0, 2.5);
        desc.auto = Some(Arc::new(|_ctx: &StackContext| ParamValue::Float(1.0)));
        let json = serde_json::to_string(&desc).unwrap();
        let round_tripped: ParamDesc = serde_json::from_str(&json).unwrap();
        assert!(round_tripped.auto.is_none());
        assert_eq!(round_tripped.id, desc.id);
        assert_eq!(round_tripped.label, desc.label);
        assert_eq!(round_tripped.help, desc.help);
        assert_eq!(round_tripped.kind, desc.kind);
        assert_eq!(round_tripped.default, desc.default);
    }
}
