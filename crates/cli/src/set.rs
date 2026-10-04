// SPDX-License-Identifier: GPL-3.0-or-later

//! The reusable `--set key=value` argument: parsing `key=value` text into
//! pairs, and validating those pairs against a resolved algorithm's schema.
//! This is the only place `--set` text becomes [`ParamValue`]s.
//!
//! [`SetArgs`] is `#[command(flatten)]`ed into `algorithms`, `stack` and
//! `align`.

use clap::Args;
use focusforge_core::params::{self, ParamDesc, ParamValue, ParamValues, ParamViolation};

/// A repeatable `--set key=value` argument.
///
/// Declared as a plain `Vec`, never `Option<Vec<_>>` — clap's derive treats
/// the latter specially (clap-rs/clap discussion #5624).
#[derive(Args, Debug, Default)]
pub struct SetArgs {
    /// Set a parameter: `--set key=value`, repeatable. A repeated key is
    /// last-wins.
    #[arg(long = "set", value_name = "KEY=VALUE", value_parser = parse_key_val)]
    pub set: Vec<(String, String)>,
}

/// `--align-set key=value`, [`SetArgs`]'s twin for `stack`'s alignment
/// stage: a second flattened struct, not a shared `--set`, because `mode`
/// is a parameter id both a stacking algorithm's schema (in test builds)
/// and an alignment algorithm's can declare, so one shared flag would be
/// ambiguous about which schema a given `mode=...` pair validates against.
#[derive(Args, Debug, Default)]
pub struct AlignSetArgs {
    /// Set an alignment parameter: `--align-set key=value`, repeatable. A
    /// repeated key is last-wins.
    // clap derives an argument's internal id from the field name; naming
    // this field `set` would collide with `SetArgs::set`'s id once both
    // structs are flattened into the same command.
    #[arg(long = "align-set", value_name = "KEY=VALUE", value_parser = parse_key_val)]
    pub align_set: Vec<(String, String)>,
}

/// Splits `s` at the first `=`, rejecting text with no `=` and rejecting an
/// empty key. Both rejections are clap's own: returning `Err` from a
/// `value_parser` makes clap print its own usage error and exit the process
/// with code **2**, never 1.
fn parse_key_val(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("invalid KEY=VALUE: no `=` found in {s:?}"))?;
    if key.is_empty() {
        return Err(format!("invalid KEY=VALUE: empty key in {s:?}"));
    }
    Ok((key.to_string(), value.to_string()))
}

/// Parses and validates `pairs` against `schema`, returning every setting or
/// every violation (all pairs are visited, so several bad settings make one
/// report). A repeated key is last-wins. [`ParamViolation::UnknownKey`] is
/// built here; every other rejection is [`ParamValue::parse`]'s own.
pub fn parse_settings(
    schema: &[ParamDesc],
    pairs: &[(String, String)],
) -> Result<ParamValues, Vec<ParamViolation>> {
    let mut values = ParamValues::new();
    let mut violations = Vec::new();
    for (key, text) in pairs {
        match schema.iter().find(|d| &d.id == key) {
            Some(desc) => match ParamValue::parse(desc, text) {
                Ok(value) => {
                    values.insert(key.clone(), value);
                }
                Err(err) => violations.extend(err.into_violations()),
            },
            None => violations.push(ParamViolation::UnknownKey {
                key: key.clone(),
                declared: schema.iter().map(|d| d.id.clone()).collect(),
            }),
        }
    }
    if violations.is_empty() {
        Ok(values)
    } else {
        Err(violations)
    }
}

/// Validates a preset's `[stack]` or `[align]` table against `schema` with
/// [`params::validate`] (values arrive typed, never as text) and overlays
/// `cli` on top, key by key: a flag beats the file.
pub fn merge_preset(
    schema: &[ParamDesc],
    preset: &ParamValues,
    cli: ParamValues,
) -> Result<ParamValues, Vec<ParamViolation>> {
    if let Err(err) = params::validate(schema, preset) {
        return Err(err.into_violations());
    }
    let mut values = preset.clone();
    values.extend(cli);
    Ok(values)
}

#[cfg(test)]
mod tests {
    use focusforge_core::params::{EnumVariant, ParamKind};

    use super::*;

    /// A hand-built schema covering all four `ParamKind`s — the only place
    /// `Bool` is exercised, since no registered algorithm declares one
    /// (`crates/core/src/test_algorithms/`).
    fn four_kind_schema() -> Vec<ParamDesc> {
        vec![
            ParamDesc {
                id: "f".to_string(),
                label: "F".to_string(),
                help: "a float".to_string(),
                kind: ParamKind::Float {
                    min: 0.0,
                    max: 1.0,
                    step: 0.1,
                    logarithmic: false,
                },
                default: ParamValue::Float(0.5),
                advanced: false,
                auto: None,
                host_default: None,
            },
            ParamDesc {
                id: "i".to_string(),
                label: "I".to_string(),
                help: "an int".to_string(),
                kind: ParamKind::Int { min: 0, max: 10 },
                default: ParamValue::Int(1),
                advanced: false,
                auto: None,
                host_default: None,
            },
            ParamDesc {
                id: "b".to_string(),
                label: "B".to_string(),
                help: "a bool".to_string(),
                kind: ParamKind::Bool,
                default: ParamValue::Bool(false),
                advanced: false,
                auto: None,
                host_default: None,
            },
            ParamDesc {
                id: "e".to_string(),
                label: "E".to_string(),
                help: "an enum".to_string(),
                kind: ParamKind::Enum {
                    variants: vec![EnumVariant {
                        id: "x".to_string(),
                        label: "X".to_string(),
                    }],
                },
                default: ParamValue::Enum("x".to_string()),
                advanced: false,
                auto: None,
                host_default: None,
            },
        ]
    }

    #[test]
    fn parse_settings_covers_every_kind() {
        let schema = four_kind_schema();
        let pairs = vec![
            ("f".to_string(), "0.25".to_string()),
            ("i".to_string(), "3".to_string()),
            ("b".to_string(), "true".to_string()),
            ("e".to_string(), "x".to_string()),
        ];
        let values = parse_settings(&schema, &pairs).expect("all four kinds must parse");
        assert_eq!(values["f"], ParamValue::Float(0.25));
        assert_eq!(values["i"], ParamValue::Int(3));
        assert_eq!(values["b"], ParamValue::Bool(true));
        assert_eq!(values["e"], ParamValue::Enum("x".to_string()));
    }

    #[test]
    fn parse_key_val_rejects_empty_key() {
        assert!(parse_key_val("=value").is_err());
    }

    #[test]
    fn merge_preset_keeps_a_preset_only_key() {
        let schema = four_kind_schema();
        let mut preset = ParamValues::new();
        preset.insert("i".to_string(), ParamValue::Int(3));
        let merged =
            merge_preset(&schema, &preset, ParamValues::new()).expect("a valid preset must merge");
        assert_eq!(merged["i"], ParamValue::Int(3));
    }

    #[test]
    fn merge_preset_lets_an_inline_set_win() {
        let schema = four_kind_schema();
        let mut preset = ParamValues::new();
        preset.insert("i".to_string(), ParamValue::Int(3));
        preset.insert("f".to_string(), ParamValue::Float(0.25));
        let cli = parse_settings(&schema, &[("i".to_string(), "7".to_string())])
            .expect("the inline value must parse");
        let merged = merge_preset(&schema, &preset, cli).expect("a valid preset must merge");
        assert_eq!(
            merged["i"],
            ParamValue::Int(7),
            "the flag must beat the file"
        );
        assert_eq!(
            merged["f"],
            ParamValue::Float(0.25),
            "an untouched preset key must survive"
        );
    }

    #[test]
    fn merge_preset_reports_the_same_violation_an_equivalent_set_would() {
        let schema = four_kind_schema();

        let mut out_of_range = ParamValues::new();
        out_of_range.insert("i".to_string(), ParamValue::Int(99));
        let violations = merge_preset(&schema, &out_of_range, ParamValues::new())
            .expect_err("an out-of-range preset value must be refused");
        assert!(
            matches!(violations.as_slice(), [ParamViolation::OutOfRange { id, .. }] if id == "i"),
            "{violations:?}"
        );
        assert_eq!(
            violations,
            parse_settings(&schema, &[("i".to_string(), "99".to_string())])
                .expect_err("the equivalent --set must be refused too"),
        );

        let mut unknown = ParamValues::new();
        unknown.insert("nope".to_string(), ParamValue::Int(1));
        let violations = merge_preset(&schema, &unknown, ParamValues::new())
            .expect_err("an unknown preset key must be refused");
        assert!(
            matches!(violations.as_slice(), [ParamViolation::UnknownKey { key, .. }] if key == "nope"),
            "{violations:?}"
        );

        let mut mistyped = ParamValues::new();
        mistyped.insert("i".to_string(), ParamValue::Bool(true));
        let violations = merge_preset(&schema, &mistyped, ParamValues::new())
            .expect_err("a mistyped preset value must be refused");
        assert!(
            matches!(violations.as_slice(), [ParamViolation::TypeMismatch { id, .. }] if id == "i"),
            "{violations:?}"
        );

        let mut bad_variant = ParamValues::new();
        bad_variant.insert("e".to_string(), ParamValue::Enum("zzz".to_string()));
        let violations = merge_preset(&schema, &bad_variant, ParamValues::new())
            .expect_err("an unknown variant must be refused");
        assert!(
            matches!(violations.as_slice(), [ParamViolation::UnknownVariant { id, .. }] if id == "e"),
            "{violations:?}"
        );
    }
}
