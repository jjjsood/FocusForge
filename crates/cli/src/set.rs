// SPDX-License-Identifier: GPL-3.0-or-later

//! The reusable `--set key=value` argument: parsing `key=value` text into
//! pairs, and validating those pairs against a resolved algorithm's schema.
//! This is the only place `--set` text becomes [`ParamValue`]s.
//!
//! [`SetArgs`] is `#[command(flatten)]`ed into `algorithms`, `stack` and
//! `align`.

use clap::Args;
use focusforge_core::params::{ParamDesc, ParamValue, ParamValues, ParamViolation};

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

/// Parses and validates `pairs` against `schema`, returning a complete set
/// of settings or every violation found — every pair is visited before
/// returning, which is what makes several bad settings one report.
///
/// [`ParamViolation::UnknownKey`] is the one violation this function
/// constructs directly: [`ParamValue::parse`] takes a descriptor, and an
/// unknown key has none. Every other rejection is
/// [`ParamValue::parse`]'s own, folded in via
/// [`focusforge_core::params::ParamError::into_violations`].
///
/// A repeated key is last-wins: `pairs` is folded into a [`ParamValues`] (a
/// `BTreeMap`), so a later pair overwrites an earlier one for the same key —
/// deliberate, and matching clap's own convention for a repeated
/// single-value option.
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
                auto: None,
            },
            ParamDesc {
                id: "i".to_string(),
                label: "I".to_string(),
                help: "an int".to_string(),
                kind: ParamKind::Int { min: 0, max: 10 },
                default: ParamValue::Int(1),
                auto: None,
            },
            ParamDesc {
                id: "b".to_string(),
                label: "B".to_string(),
                help: "a bool".to_string(),
                kind: ParamKind::Bool,
                default: ParamValue::Bool(false),
                auto: None,
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
                auto: None,
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
}
