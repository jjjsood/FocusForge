// SPDX-License-Identifier: GPL-3.0-or-later

//! The input table's file format: TOML at `<config_dir>/input.toml`. Reads
//! and writes that one file -- the fourth file in the crate with a
//! legitimate reason to touch storage
//! (`crates/gui/tests/shell.rs`'s `checkpoint_panel_touches_no_storage`
//! exclusion list). No `use slint::` anywhere in this file
//! (`input_modules_are_slint_free`).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Spanned;

use crate::input_map::{Action, Binding, DEFAULT_BINDINGS, InputTable, Trigger};

/// The only `version` this build accepts. A file naming a higher version is
/// refused by name; a file naming a lower one is unreachable today and
/// refused the same way.
pub const SUPPORTED_VERSION: u32 = 1;

/// Where the active table came from, shown as the settings modal's one
/// section-level provenance line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Provenance {
    #[default]
    Defaults,
    File {
        path: PathBuf,
        overridden: usize,
    },
}

/// Every way loading or writing the config file can go wrong, each variant
/// naming the path and (where relevant) the line -- never a bare panic or a
/// stringly-typed error, following `CanvasError`'s idiom.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: could not parse as TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "{path}:{line}: unsupported version {found} -- this build supports version {supported}"
    )]
    UnsupportedVersion {
        path: PathBuf,
        line: usize,
        found: u32,
        supported: u32,
    },
    #[error("{path}:{line}: unknown trigger {spelling:?}")]
    UnknownTrigger {
        path: PathBuf,
        line: usize,
        spelling: String,
    },
    #[error("{path}:{line}: unknown action {spelling:?}")]
    UnknownAction {
        path: PathBuf,
        line: usize,
        spelling: String,
    },
    #[error(
        "{path}:{line}: trigger {trigger:?} duplicates the one already read at line {first_line}"
    )]
    DuplicateTrigger {
        path: PathBuf,
        line: usize,
        first_line: usize,
        trigger: String,
    },
    #[error("{path}: could not save the defaults: {source}")]
    CouldNotWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    version: Spanned<u32>,
    #[serde(default)]
    bindings: Vec<Spanned<ConfigBinding>>,
}

#[derive(Debug, Deserialize)]
struct ConfigBinding {
    trigger: Spanned<String>,
    #[serde(default)]
    stacking: Option<Spanned<String>>,
    #[serde(default)]
    editor: Option<Spanned<String>>,
}

/// 1-indexed line of the byte offset `byte` within `src`.
fn line_of(src: &str, byte: usize) -> usize {
    src[..byte].matches('\n').count() + 1
}

/// The result of [`load`]. Never `Err` -- the application must start.
#[derive(Debug)]
pub struct Loaded {
    pub table: InputTable,
    pub provenance: Provenance,
    pub problems: Vec<ConfigError>,
}

fn defaults_with(problems: Vec<ConfigError>) -> Loaded {
    Loaded {
        table: InputTable::default(),
        provenance: Provenance::Defaults,
        problems,
    }
}

/// Loads the input table from `<config_dir>/input.toml`. A missing file is
/// the normal case, not an error. Any problem at all -- a parse error, an
/// unsupported version, an unknown trigger or action, a duplicate trigger --
/// refuses the **whole** file: the table falls back to
/// [`InputTable::default`] wholesale, never half-applied.
pub fn load(config_dir: &Path) -> Loaded {
    let path = config_dir.join("input.toml");

    let src = match fs::read_to_string(&path) {
        Ok(src) => src,
        Err(_) => return defaults_with(Vec::new()),
    };

    let parsed: ConfigFile = match toml::from_str(&src) {
        Ok(parsed) => parsed,
        Err(source) => return defaults_with(vec![ConfigError::Parse { path, source }]),
    };

    let version = *parsed.version.get_ref();
    if version > SUPPORTED_VERSION || version < 1 {
        let line = line_of(&src, parsed.version.span().start);
        return defaults_with(vec![ConfigError::UnsupportedVersion {
            path,
            line,
            found: version,
            supported: SUPPORTED_VERSION,
        }]);
    }

    let mut problems = Vec::new();
    let mut bindings = InputTable::default().bindings;
    let mut seen: Vec<(Trigger, usize)> = Vec::new();

    for row in parsed.bindings.iter() {
        let row_line = line_of(&src, row.span().start);
        let inner = row.get_ref();

        let trigger = match inner.trigger.get_ref().parse::<Trigger>() {
            Ok(trigger) => trigger,
            Err(_) => {
                problems.push(ConfigError::UnknownTrigger {
                    path: path.clone(),
                    line: line_of(&src, inner.trigger.span().start),
                    spelling: inner.trigger.get_ref().clone(),
                });
                continue;
            }
        };

        // A row is matched against `DEFAULT_BINDINGS` **by trigger
        // identity**, never by its position within the file: the file is
        // not required to be a full, in-order transcript of the defaults,
        // and a partial or out-of-order file must never shift or delete an
        // unrelated row.
        //
        // A trigger that parses but names none of the table's ten known
        // triggers is refused the same way an unparseable spelling is: this
        // table's rows are display-indexed (`InputTable::display_rows`,
        // `DEFAULT_BINDINGS`'s fixed ten-slot layout), so a trigger this
        // table has no row for cannot be shown in the settings modal, and
        // silently accepting it would show a table that does not match the
        // active bindings. Refusing is final -- "rebind" means reassigning
        // one of the shipped triggers' action, not naming a device none of
        // them already uses.
        let Some(default_index) = DEFAULT_BINDINGS.iter().position(|b| b.trigger == trigger) else {
            problems.push(ConfigError::UnknownTrigger {
                path: path.clone(),
                line: line_of(&src, inner.trigger.span().start),
                spelling: inner.trigger.get_ref().clone(),
            });
            continue;
        };

        let mut resolve_action = |spanned: &Option<Spanned<String>>| -> Result<Option<Action>, ()> {
            match spanned {
                None => Ok(None),
                Some(s) => match s.get_ref().parse::<Action>() {
                    Ok(action) => Ok(Some(action)),
                    Err(_) => {
                        problems.push(ConfigError::UnknownAction {
                            path: path.clone(),
                            line: line_of(&src, s.span().start),
                            spelling: s.get_ref().clone(),
                        });
                        Err(())
                    }
                },
            }
        };

        let Ok(stacking) = resolve_action(&inner.stacking) else {
            continue;
        };
        let Ok(editor) = resolve_action(&inner.editor) else {
            continue;
        };

        if let Some((_, first_line)) = seen.iter().find(|(t, _)| *t == trigger) {
            problems.push(ConfigError::DuplicateTrigger {
                path: path.clone(),
                line: row_line,
                first_line: *first_line,
                trigger: inner.trigger.get_ref().clone(),
            });
            continue;
        }
        seen.push((trigger, row_line));

        bindings[default_index] = Binding {
            trigger,
            stacking,
            editor,
        };
    }

    if !problems.is_empty() {
        return defaults_with(problems);
    }

    let overridden = bindings
        .iter()
        .zip(InputTable::default().bindings.iter())
        .filter(|(a, b)| a != b)
        .count();

    Loaded {
        table: InputTable { bindings },
        provenance: Provenance::File { path, overridden },
        problems: Vec::new(),
    }
}

#[derive(Debug, Serialize)]
struct SerializedConfig {
    version: u32,
    bindings: Vec<SerializedBinding>,
}

#[derive(Debug, Serialize)]
struct SerializedBinding {
    trigger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stacking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<String>,
}

impl SerializedConfig {
    fn defaults() -> Self {
        let table = InputTable::default();
        SerializedConfig {
            version: SUPPORTED_VERSION,
            bindings: table
                .bindings
                .iter()
                .map(|b| SerializedBinding {
                    trigger: b.trigger.to_string(),
                    stacking: b.stacking.map(|a| a.to_string()),
                    editor: b.editor.map(|a| a.to_string()),
                })
                .collect(),
        }
    }
}

const HEADER: &str = "\
# FocusForge input map.
#
# Each `trigger` must be one of the bindings listed below; rebind by
# changing its actions. A trigger this file does not already list is
# refused.
#
# Accepted actions: zoom-at-cursor, step-source-frame, pan-horizontal,
# overlay-opacity, pan, brush-size, brush-softness, undo, redo,
# pick-source-under-pointer, paint.
#
# Omit `stacking` or `editor` to leave that mode with no binding.
";

/// Writes the shipped defaults to `<config_dir>/input.toml`, creating
/// `config_dir` if needed. An I/O failure here is **not** fatal to the
/// caller -- it returns `Err` so the caller can record one
/// [`ConfigError::CouldNotWrite`] and keep running on the in-memory
/// defaults.
pub fn write_defaults(config_dir: &Path) -> Result<PathBuf, ConfigError> {
    fs::create_dir_all(config_dir).map_err(|source| ConfigError::CouldNotWrite {
        path: config_dir.to_path_buf(),
        source,
    })?;
    let path = config_dir.join("input.toml");
    let body = toml::to_string_pretty(&SerializedConfig::defaults())
        .expect("SerializedConfig::defaults always serialises");
    fs::write(&path, format!("{HEADER}{body}")).map_err(|source| ConfigError::CouldNotWrite {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// `directories::ProjectDirs::from("", "", "focusforge").config_dir()` --
/// `$XDG_CONFIG_HOME/focusforge` on Linux, the platform equivalent
/// elsewhere. The only function in the crate that reads the environment;
/// no test calls it: `std::env::set_var` is `unsafe` in edition 2024 and
/// the workspace denies `unsafe_code`.
pub fn default_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "focusforge").map(|d| d.config_dir().to_path_buf())
}
