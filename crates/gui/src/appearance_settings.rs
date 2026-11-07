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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppearanceSettings {
    pub presentation: SourcePresentation,
    /// Whether a run shows where each frame is sharp while it fuses.
    pub sharpness_heatmap: bool,
    /// Whether transitions are instant.
    pub reduce_motion: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            presentation: SourcePresentation::default(),
            sharpness_heatmap: true,
            reduce_motion: false,
        }
    }
}

/// Every key falls back to its default on its own, so a file that lacks or
/// mangles one keeps the others.
pub fn load(config_dir: &Path) -> AppearanceSettings {
    let defaults = AppearanceSettings::default();
    let (_, read) = settings::read(config_dir);
    let Ok(Some(src)) = read else {
        return defaults;
    };
    let Ok(table) = src.parse::<toml::Table>() else {
        return defaults;
    };
    let Some(table) = table.get("appearance").and_then(toml::Value::as_table) else {
        return defaults;
    };
    AppearanceSettings {
        presentation: table
            .get("presentation")
            .and_then(toml::Value::as_str)
            .and_then(SourcePresentation::from_slug)
            .unwrap_or(defaults.presentation),
        sharpness_heatmap: table
            .get("sharpness_heatmap")
            .and_then(toml::Value::as_bool)
            .unwrap_or(defaults.sharpness_heatmap),
        reduce_motion: table
            .get("reduce_motion")
            .and_then(toml::Value::as_bool)
            .unwrap_or(defaults.reduce_motion),
    }
}

pub fn save(config_dir: &Path, settings_value: AppearanceSettings) -> Result<(), WriteError> {
    #[derive(Serialize)]
    struct File<'a> {
        presentation: &'a str,
        sharpness_heatmap: bool,
        reduce_motion: bool,
    }
    settings::write_table(
        config_dir,
        "appearance",
        settings::to_table(&File {
            presentation: settings_value.presentation.slug(),
            sharpness_heatmap: settings_value.sharpness_heatmap,
            reduce_motion: settings_value.reduce_motion,
        }),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_from(source: &str) -> AppearanceSettings {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(settings::FILE_NAME), source).unwrap();
        load(dir.path())
    }

    #[test]
    fn sharpness_heatmap_setting_defaults_on_and_round_trips() {
        let absent = tempfile::tempdir().unwrap();
        assert!(load(absent.path()).sharpness_heatmap);
        assert!(load_from("[appearance]\npresentation = \"list\"\n").sharpness_heatmap);
        assert!(load_from("[appearance]\nsharpness_heatmap = \"off\"\n").sharpness_heatmap);

        let off = load_from("[appearance]\nsharpness_heatmap = false\n");
        assert!(!off.sharpness_heatmap);
        assert_eq!(off.presentation, SourcePresentation::default());

        let bad_presentation =
            load_from("[appearance]\npresentation = \"gallery\"\nsharpness_heatmap = false\n");
        assert!(
            !bad_presentation.sharpness_heatmap,
            "an unrelated presentation value keeps the heatmap key"
        );

        let bad_heatmap =
            load_from("[appearance]\npresentation = \"large\"\nsharpness_heatmap = 3\n");
        assert_eq!(bad_heatmap.presentation, SourcePresentation::Large);
        assert!(
            bad_heatmap.sharpness_heatmap,
            "a bad heatmap value falls back on its own"
        );

        let dir = tempfile::tempdir().unwrap();
        let saved = AppearanceSettings {
            presentation: SourcePresentation::List,
            sharpness_heatmap: false,
            reduce_motion: false,
        };
        save(dir.path(), saved).unwrap();
        assert_eq!(load(dir.path()), saved);
    }

    #[test]
    fn reduce_motion_setting_defaults_and_round_trips() {
        let absent = tempfile::tempdir().unwrap();
        assert!(!load(absent.path()).reduce_motion);
        assert!(!load_from("[appearance]\npresentation = \"list\"\n").reduce_motion);
        assert!(load_from("[appearance]\nreduce_motion = true\n").reduce_motion);
        assert!(!load_from("[appearance]\nreduce_motion = false\n").reduce_motion);

        let malformed = load_from(
            "[appearance]\npresentation = \"large\"\nsharpness_heatmap = false\nreduce_motion = \"yes\"\n",
        );
        assert!(!malformed.reduce_motion, "a bad value falls back alone");
        assert_eq!(malformed.presentation, SourcePresentation::Large);
        assert!(!malformed.sharpness_heatmap, "the heatmap key is kept");

        let bad_presentation =
            load_from("[appearance]\npresentation = \"gallery\"\nreduce_motion = true\n");
        assert!(bad_presentation.reduce_motion);

        let dir = tempfile::tempdir().unwrap();
        let saved = AppearanceSettings {
            presentation: SourcePresentation::Large,
            sharpness_heatmap: false,
            reduce_motion: true,
        };
        save(dir.path(), saved).unwrap();
        assert_eq!(load(dir.path()), saved);
    }
}
