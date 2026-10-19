// SPDX-License-Identifier: GPL-3.0-or-later

//! The preset file: a reusable bundle of parameter values, written once and
//! referenced by `--preset <path>` instead of retyped as `--set` flags.
//!
//! # Format
//!
//! TOML, two scalar keys and two value tables:
//!
//! ```toml
//! algo = "laplacian"
//! align-algo = "ecc"
//!
//! [stack]
//! levels = 5
//!
//! [align]
//! max-iterations = 40
//! ```
//!
//! Values are **typed TOML**, never quoted text: `levels = 5` is an integer.
//! A quoted number (`levels = "5"`) is *not* caught by the `toml` parser
//! itself -- [`focusforge_core::params::ParamValue`] is `#[serde(untagged)]`
//! with a string variant, so any TOML string deserializes -- it surfaces one
//! layer later as a schema `TypeMismatch`. A value no `ParamValue` variant
//! can hold at all (an array, for instance) is the case the `toml` parser
//! itself refuses, before the schema ever runs.
//!
//! # Precedence
//!
//! One rule, in both directions: **a flag beats the file.** An inline
//! `--set key=value` overrides the `[stack]` table for the same key,
//! `--align-set` overrides `[align]`, and `--algorithm`/`--align-algorithm` override
//! the file's `algo`/`align-algo`. A key the file sets and no flag mentions
//! resolves with [`focusforge_core::params::Provenance::Explicit`]: a preset
//! is the user setting the value, in a file instead of on the command line.
//!
//! `align` reads the same file, using only `align-algo` and `[align]`; the
//! `[stack]` half is unused there and says so on stderr rather than
//! disappearing silently.

use std::path::Path;

use focusforge_core::params::ParamValues;
use serde::{Deserialize, Serialize};

/// One preset file.
///
/// Field order is load-bearing, not cosmetic: a TOML serializer emits a
/// struct's fields in declaration order, and a bare key written *after* a
/// table header belongs to that table. Declaring the two scalars before the
/// two tables is what keeps `--write-preset`'s output re-readable.
/// `toml::ser::Error` is an opaque struct in 1.1.6, not an enum with a
/// nameable `ValueAfterTable` variant, so field order is the guarantee and
/// `preset_round_trips_with_scalars_outside_both_tables` is the check.
///
/// `deny_unknown_fields`: a mistyped table name (`[stacking]`) is a refusal,
/// never a silently ignored section.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Preset {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align_algo: Option<String>,
    #[serde(default, skip_serializing_if = "ParamValues::is_empty")]
    pub stack: ParamValues,
    #[serde(default, skip_serializing_if = "ParamValues::is_empty")]
    pub align: ParamValues,
}

/// The four preset/status/dry-run flags, bundled to keep `stack::run` and
/// `align::run`'s argument count down.
pub struct PresetOptions<'a> {
    pub preset: Option<&'a Path>,
    pub write_preset: Option<&'a Path>,
    pub status: Option<&'a Path>,
    /// Always `false` on `align`, which declares no `--dry-run`: its whole
    /// cost *is* the alignment a dry run stops before.
    pub dry_run: bool,
}

/// Reads and parses the preset at `path`, or `Preset::default()` when no
/// `--preset` was given. Read and TOML errors render as `"<path>: <detail>"`
/// (see the module docs for which type errors surface here).
pub fn load(path: Option<&Path>) -> Result<Preset, String> {
    let Some(path) = path else {
        return Ok(Preset::default());
    };
    read_toml(path)
}

/// Serializes `preset` to `path`, creating or truncating it.
pub fn write(preset: &Preset, path: &Path) -> Result<(), String> {
    write_toml(path, preset, "the preset")
}

/// Reads and parses the TOML file at `path`; both failures render as
/// `"<path>: <detail>"`.
pub(crate) fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

/// Serializes `value` to `path`, creating or truncating it. A serialization
/// failure reads `"<path>: serializing <what>: <detail>"`.
pub(crate) fn write_toml<T: Serialize>(path: &Path, value: &T, what: &str) -> Result<(), String> {
    let text = toml::to_string_pretty(value)
        .map_err(|err| format!("{}: serializing {what}: {err}", path.display()))?;
    std::fs::write(path, text).map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use focusforge_core::params::ParamValue;

    /// A directory of its own for one test, removed when the returned guard
    /// drops. A guard from `tempfile`, not `CARGO_TARGET_TMPDIR`: the latter
    /// is set only when cargo builds an integration test or a benchmark,
    /// never for a unit test compiled into the binary itself.
    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::with_prefix("focusforge-preset-").unwrap()
    }

    fn write_temp(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn load_with_no_path_is_an_empty_preset() {
        assert_eq!(load(None).expect("no path must be Ok"), Preset::default());
    }

    #[test]
    fn load_reads_both_tables_and_both_scalars() {
        let dir = temp_dir();
        let path = write_temp(
            dir.path(),
            "preset_load_reads_both_tables.toml",
            "algo = \"a\"\nalign-algo = \"b\"\n\n[stack]\nlevels = 5\n\n[align]\nrho = 0.25\n",
        );
        let preset = load(Some(&path)).expect("a well-formed preset must load");
        assert_eq!(preset.algo.as_deref(), Some("a"));
        assert_eq!(preset.align_algo.as_deref(), Some("b"));
        assert_eq!(preset.stack["levels"], ParamValue::Int(5));
        assert_eq!(preset.align["rho"], ParamValue::Float(0.25));
    }

    #[test]
    fn load_names_the_path_and_the_detail_on_a_parse_error() {
        let dir = temp_dir();
        let path = write_temp(
            dir.path(),
            "preset_load_parse_error.toml",
            "[stack\nlevels = 5\n",
        );
        let err = load(Some(&path)).expect_err("a malformed preset must refuse");
        assert!(
            err.contains("preset_load_parse_error.toml"),
            "refusal must name the path: {err}"
        );
    }

    #[test]
    fn load_refuses_an_unknown_table() {
        let dir = temp_dir();
        let path = write_temp(
            dir.path(),
            "preset_load_unknown_table.toml",
            "[stacking]\nlevels = 5\n",
        );
        let err = load(Some(&path)).expect_err("an unknown table must refuse");
        assert!(err.contains("stacking"), "refusal must name the key: {err}");
    }

    #[test]
    fn load_refuses_a_missing_file() {
        let dir = temp_dir();
        let path = dir.path().join("preset_absent.toml");
        let err = load(Some(&path)).expect_err("a missing preset must refuse");
        assert!(err.contains("preset_absent.toml"), "{err}");
    }

    /// The TOML twin of `untagged_number_widening_is_pinned`
    /// (`crates/core/src/params/mod.rs`), which pins the same widening
    /// against `serde_json` only. `ParamValue` is `#[serde(untagged)]` and
    /// untagged resolution is **declaration-order dependent**
    /// (serde-rs/serde#2661), so every self-describing format that carries a
    /// `ParamValue` needs its own pin: without this one, reordering
    /// `ParamValue`'s variants would break the preset format while the JSON
    /// test stayed green.
    #[test]
    fn toml_scalars_map_onto_the_declared_param_value_variants() {
        let preset: Preset =
            toml::from_str("[stack]\nlevels = 5\nthreshold = 0.25\nmode = \"fast\"\nflag = true\n")
                .expect("every TOML scalar must deserialize");
        assert_eq!(preset.stack["levels"], ParamValue::Int(5));
        assert_eq!(preset.stack["threshold"], ParamValue::Float(0.25));
        assert_eq!(preset.stack["mode"], ParamValue::Enum("fast".to_string()));
        assert_eq!(preset.stack["flag"], ParamValue::Bool(true));
    }

    /// A TOML integer is accepted where a `Float` is meant and stays `Int`
    /// until `params::check` widens it -- the same on-disk contract the JSON
    /// pin states, and the reason a preset's `threshold = 1` is not a type
    /// error.
    #[test]
    fn a_toml_integer_stays_int_and_is_widened_downstream() {
        let preset: Preset =
            toml::from_str("[stack]\nthreshold = 1\n").expect("an integer must deserialize");
        assert_eq!(preset.stack["threshold"], ParamValue::Int(1));
    }

    #[test]
    fn preset_round_trips_with_scalars_outside_both_tables() {
        let mut stack = ParamValues::new();
        stack.insert("levels".to_string(), ParamValue::Int(4));
        let mut align = ParamValues::new();
        align.insert("rho".to_string(), ParamValue::Float(0.5));
        let preset = Preset {
            algo: Some("a".to_string()),
            align_algo: Some("b".to_string()),
            stack,
            align,
        };

        let text = toml::to_string_pretty(&preset).expect("a preset must serialize");
        let algo_line = text.find("algo =").expect("algo must be written");
        let first_table = text.find('[').expect("a table header must be written");
        assert!(
            algo_line < first_table,
            "the scalar keys must precede both table headers, else they land inside one:\n{text}"
        );

        let back: Preset = toml::from_str(&text).expect("a written preset must re-read");
        assert_eq!(back, preset);
    }
}
