// SPDX-License-Identifier: GPL-3.0-or-later

//! The `[editor]` table in `settings.toml`.

use std::path::Path;

use serde::Serialize;

use crate::settings::{self, WriteError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditorSettings {
    /// Whether the brush circle shows the stroke's result before it is
    /// painted.
    pub preview: bool,
}

/// Every key falls back to its default on its own, so a file that lacks or
/// mangles one keeps the others.
pub fn load(config_dir: &Path) -> EditorSettings {
    let defaults = EditorSettings::default();
    let (_, read) = settings::read(config_dir);
    let Ok(Some(src)) = read else {
        return defaults;
    };
    let Ok(table) = src.parse::<toml::Table>() else {
        return defaults;
    };
    let Some(table) = table.get("editor").and_then(toml::Value::as_table) else {
        return defaults;
    };
    EditorSettings {
        preview: table
            .get("preview")
            .and_then(toml::Value::as_bool)
            .unwrap_or(defaults.preview),
    }
}

pub fn save(config_dir: &Path, settings_value: EditorSettings) -> Result<(), WriteError> {
    #[derive(Serialize)]
    struct File {
        preview: bool,
    }
    settings::write_table(
        config_dir,
        "editor",
        settings::to_table(&File {
            preview: settings_value.preview,
        }),
    )
    .map(|_| ())
}
