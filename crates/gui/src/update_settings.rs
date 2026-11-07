// SPDX-License-Identifier: GPL-3.0-or-later

//! The `[updates]` table in `settings.toml`, and the release source behind the
//! hidden update-notice preview.

use std::path::Path;

use serde::Serialize;

use crate::job::ReleaseSource;
use crate::settings::{self, WriteError};

/// Answers every request with a final release of one fixed version, for
/// showing the update notice without asking the release server. `slow` never
/// answers and `failing` always refuses.
pub struct PreviewRelease(Answer);

enum Answer {
    Release(String),
    Slow,
    Failing,
}

impl PreviewRelease {
    pub fn new(version: &str) -> Self {
        Self(Answer::Release(version.trim_start_matches('v').to_owned()))
    }

    /// Parks the requesting thread for good, so a check stays in flight.
    pub fn slow() -> Self {
        Self(Answer::Slow)
    }

    pub fn failing() -> Self {
        Self(Answer::Failing)
    }
}

impl ReleaseSource for PreviewRelease {
    fn fetch(&self) -> Result<String, String> {
        let version = match &self.0 {
            Answer::Release(version) => version,
            Answer::Slow => loop {
                std::thread::park();
            },
            Answer::Failing => return Err("connection refused".to_owned()),
        };
        let tag = format!("v{version}");
        let page = format!("https://github.com/jjjsood/FocusForge/releases/tag/{tag}");
        Ok(format!(
            r#"{{"tag_name":"{tag}","html_url":"{page}","prerelease":false,"draft":false}}"#
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSettings {
    /// Whether the window looks for a newer release once at start.
    pub check_on_start: bool,
    /// The release version the user chose not to be told about.
    pub skipped_version: Option<String>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_on_start: true,
            skipped_version: None,
        }
    }
}

/// Every key falls back to its default on its own, so a file that lacks or
/// mangles one keeps the other.
pub fn load(config_dir: &Path) -> UpdateSettings {
    let defaults = UpdateSettings::default();
    let (_, read) = settings::read(config_dir);
    let Ok(Some(src)) = read else {
        return defaults;
    };
    let Ok(table) = src.parse::<toml::Table>() else {
        return defaults;
    };
    let Some(table) = table.get("updates").and_then(toml::Value::as_table) else {
        return defaults;
    };
    UpdateSettings {
        check_on_start: table
            .get("check_on_start")
            .and_then(toml::Value::as_bool)
            .unwrap_or(defaults.check_on_start),
        skipped_version: table
            .get("skipped_version")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
    }
}

pub fn save(config_dir: &Path, settings_value: &UpdateSettings) -> Result<(), WriteError> {
    #[derive(Serialize)]
    struct File<'a> {
        check_on_start: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped_version: Option<&'a str>,
    }
    settings::write_table(
        config_dir,
        "updates",
        settings::to_table(&File {
            check_on_start: settings_value.check_on_start,
            skipped_version: settings_value.skipped_version.as_deref(),
        }),
    )
    .map(|_| ())
}
