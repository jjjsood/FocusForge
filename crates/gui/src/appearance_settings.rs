// SPDX-License-Identifier: GPL-3.0-or-later

//! The `[appearance]` table in `settings.toml`.

use std::path::Path;

use serde::Serialize;

use crate::settings::{self, WriteError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourcePresentation {
    List,
    #[default]
    ImageList,
    Large,
}

impl SourcePresentation {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::ImageList => "image-list",
            Self::Large => "large",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "list" => Some(Self::List),
            "image-list" => Some(Self::ImageList),
            "large" => Some(Self::Large),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppearanceSettings {
    pub presentation: SourcePresentation,
}

pub fn load(config_dir: &Path) -> AppearanceSettings {
    let (_, read) = settings::read(config_dir);
    let Ok(Some(src)) = read else {
        return AppearanceSettings::default();
    };
    let Ok(table) = src.parse::<toml::Table>() else {
        return AppearanceSettings::default();
    };
    table
        .get("appearance")
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("presentation"))
        .and_then(toml::Value::as_str)
        .and_then(SourcePresentation::from_slug)
        .map(|presentation| AppearanceSettings { presentation })
        .unwrap_or_default()
}

pub fn save(config_dir: &Path, settings_value: AppearanceSettings) -> Result<(), WriteError> {
    #[derive(Serialize)]
    struct File<'a> {
        presentation: &'a str,
    }
    settings::write_table(
        config_dir,
        "appearance",
        settings::to_table(&File {
            presentation: settings_value.presentation.slug(),
        }),
    )
    .map(|_| ())
}
