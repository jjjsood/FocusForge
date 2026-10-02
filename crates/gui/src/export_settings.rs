// SPDX-License-Identifier: GPL-3.0-or-later

//! The export modal's remembered choice: `[export]` in
//! `<config_dir>/settings.toml`. Every option, default and range comes from
//! core's descriptors; this file only keeps and restores the values. No
//! `use slint::` anywhere in this file, mirroring `crate::recent`.

use std::path::Path;

use crate::job::{ExportFormat, ExportOptions, MetadataOptions, params_for};
use serde::{Deserialize, Serialize};

use crate::settings::{self, WriteError};
use crate::settings_panel::{ParamValues, SettingsRow, field_row, validate};

/// The formats the modal offers, in segment order.
pub const FORMATS: [ExportFormat; 3] = [ExportFormat::Tiff, ExportFormat::Png, ExportFormat::Jpeg];

/// The selected format, each format's last settings and the one `Metadata`
/// switch.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportChoice {
    pub format: ExportFormat,
    pub tiff: ExportOptions,
    pub png: ExportOptions,
    pub jpeg: ExportOptions,
    pub metadata: bool,
}

fn options_from(format: ExportFormat, stored: &ParamValues) -> ExportOptions {
    let schema = params_for(format);
    let mut values = ParamValues::new();
    for (id, value) in stored {
        let one = ParamValues::from([(id.clone(), value.clone())]);
        if validate(schema, &one).is_ok() {
            values.insert(id.clone(), value.clone());
        }
    }
    ExportOptions::from_values(format, &values).expect("every kept entry was validated")
}

impl Default for ExportChoice {
    fn default() -> Self {
        let none = ParamValues::new();
        ExportChoice {
            format: ExportOptions::default().format(),
            tiff: options_from(ExportFormat::Tiff, &none),
            png: options_from(ExportFormat::Png, &none),
            jpeg: options_from(ExportFormat::Jpeg, &none),
            metadata: true,
        }
    }
}

impl ExportChoice {
    /// The selected format's options.
    pub fn options(&self) -> ExportOptions {
        self.options_of(self.format).clone()
    }

    fn options_of(&self, format: ExportFormat) -> &ExportOptions {
        match format {
            ExportFormat::Tiff => &self.tiff,
            ExportFormat::Png => &self.png,
            ExportFormat::Jpeg => &self.jpeg,
        }
    }

    /// Replaces the slot of `options`' own format.
    pub fn set_options(&mut self, options: ExportOptions) {
        match options.format() {
            ExportFormat::Tiff => self.tiff = options,
            ExportFormat::Png => self.png = options,
            ExportFormat::Jpeg => self.jpeg = options,
        }
    }

    /// `Metadata` on carries EXIF and the ICC profile; off carries nothing.
    pub fn metadata_options(&self) -> MetadataOptions {
        if self.metadata {
            MetadataOptions::default()
        } else {
            MetadataOptions {
                exif: false,
                icc: false,
                xmp: false,
                iptc: false,
            }
        }
    }

    /// What the modal shows for this choice, `open_menu` naming the one
    /// descriptor id whose list is open.
    pub fn view(&self, open_menu: Option<&str>) -> ExportView {
        let values = self.options().values();
        let rows = params_for(self.format)
            .iter()
            .map(|desc| {
                let value = values
                    .get(&desc.id)
                    .expect("values() holds every descriptor of its format");
                field_row("export", desc, value, open_menu == Some(desc.id.as_str()))
            })
            .collect();
        let label = |f: ExportFormat| f.id().to_uppercase();
        ExportView {
            segments: FORMATS.iter().map(|f| label(*f)).collect(),
            selected: label(self.format),
            heading: format!("{} SETTINGS", label(self.format)),
            rows,
            slider_group: self.format == ExportFormat::Jpeg,
            metadata: self.metadata,
        }
    }
}

/// The modal's settings section, ready to push to the window.
pub struct ExportView {
    pub segments: Vec<String>,
    pub selected: String,
    pub heading: String,
    /// One row per descriptor of the selected format, in descriptor order.
    pub rows: Vec<SettingsRow>,
    /// The group is a slider row then a segmented row rather than menus.
    pub slider_group: bool,
    pub metadata: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ExportFile {
    #[serde(default)]
    format: String,
    #[serde(default = "on")]
    metadata: bool,
    #[serde(default)]
    tiff: ParamValues,
    #[serde(default)]
    png: ParamValues,
    #[serde(default)]
    jpeg: ParamValues,
}

fn on() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
struct SettingsWrapper {
    #[serde(default)]
    export: Option<ExportFile>,
}

/// Loads `[export]`. Any failure -- the file or table absent, unreadable or
/// not parsing -- reads as the defaults; an unknown format or an entry that
/// fails its descriptor falls back alone.
pub fn load(config_dir: &Path) -> ExportChoice {
    let (_, read_result) = settings::read(config_dir);
    let Ok(Some(src)) = read_result else {
        return ExportChoice::default();
    };
    let Ok(SettingsWrapper {
        export: Some(file), ..
    }) = toml::from_str::<SettingsWrapper>(&src)
    else {
        return ExportChoice::default();
    };
    let fallback = ExportChoice::default();
    ExportChoice {
        format: FORMATS
            .into_iter()
            .find(|f| f.id() == file.format)
            .unwrap_or(fallback.format),
        tiff: options_from(ExportFormat::Tiff, &file.tiff),
        png: options_from(ExportFormat::Png, &file.png),
        jpeg: options_from(ExportFormat::Jpeg, &file.jpeg),
        metadata: file.metadata,
    }
}

/// Writes `choice` as `settings.toml`'s `[export]` table; every other table
/// is left as it was.
pub fn save(config_dir: &Path, choice: &ExportChoice) -> Result<(), WriteError> {
    let table = settings::to_table(&ExportFile {
        format: choice.format.id().to_string(),
        metadata: choice.metadata,
        tiff: choice.tiff.values(),
        png: choice.png.values(),
        jpeg: choice.jpeg.values(),
    });
    settings::write_table(config_dir, "export", table).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JpegSubsampling, PngBits, PngCompression};
    use std::fs;

    fn write_settings(dir: &Path, text: &str) {
        fs::write(dir.join(settings::FILE_NAME), text).unwrap();
    }

    fn jpeg_choice() -> ExportChoice {
        ExportChoice {
            format: ExportFormat::Jpeg,
            jpeg: ExportOptions::Jpeg {
                quality: 90,
                subsampling: JpegSubsampling::S444,
            },
            png: ExportOptions::Png {
                bits: PngBits::Eight,
                compression: PngCompression::Best,
            },
            metadata: false,
            ..ExportChoice::default()
        }
    }

    #[test]
    fn default_is_built_from_core() {
        let choice = ExportChoice::default();
        assert_eq!(choice.options(), ExportOptions::default());
        assert!(choice.metadata);
        assert_eq!(choice.metadata_options(), MetadataOptions::default());
        assert_eq!(
            choice.jpeg,
            ExportOptions::from_values(ExportFormat::Jpeg, &ParamValues::new()).unwrap()
        );
        let off = ExportChoice {
            metadata: false,
            ..choice
        };
        let none = MetadataOptions {
            exif: false,
            icc: false,
            xmp: false,
            iptc: false,
        };
        assert_eq!(off.metadata_options(), none);
    }

    #[test]
    fn missing_file_and_missing_table_load_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), ExportChoice::default());
        write_settings(dir.path(), "[recent]\nentries = []\n");
        assert_eq!(load(dir.path()), ExportChoice::default());
    }

    #[test]
    fn unparseable_file_loads_defaults_and_stays_byte_unchanged_after_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let broken = "[export\nformat = ";
        write_settings(dir.path(), broken);
        assert_eq!(load(dir.path()), ExportChoice::default());
        assert!(matches!(
            save(dir.path(), &jpeg_choice()),
            Err(WriteError::Parse { .. })
        ));
        assert_eq!(settings::read(dir.path()).1.unwrap().unwrap(), broken);
    }

    #[test]
    fn unknown_format_falls_back_to_the_default_format() {
        let dir = tempfile::tempdir().unwrap();
        write_settings(
            dir.path(),
            "[export]\nformat = \"webp\"\n[export.png]\npng-bits = \"8\"\n",
        );
        let choice = load(dir.path());
        assert_eq!(choice.format, ExportOptions::default().format());
        assert_eq!(
            choice.png,
            ExportOptions::Png {
                bits: PngBits::Eight,
                compression: PngCompression::Default,
            }
        );
    }

    #[test]
    fn an_invalid_entry_falls_back_alone() {
        for bad in ["0", "101"] {
            let dir = tempfile::tempdir().unwrap();
            write_settings(
                dir.path(),
                &format!(
                    "[export]\nformat = \"jpeg\"\n[export.jpeg]\njpeg-quality = {bad}\njpeg-subsampling = \"444\"\n"
                ),
            );
            let choice = load(dir.path());
            assert_eq!(choice.format, ExportFormat::Jpeg);
            assert_eq!(
                choice.jpeg,
                ExportOptions::Jpeg {
                    quality: 95,
                    subsampling: JpegSubsampling::S444,
                },
                "quality {bad} drops to its default, the valid chroma value stays"
            );
        }
    }

    #[test]
    fn save_round_trips_and_leaves_other_tables_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let original = "# hand note\n[recent]\nentries = []\n\n[input]\nzoom = 1\n";
        write_settings(dir.path(), original);
        let choice = jpeg_choice();
        save(dir.path(), &choice).unwrap();
        assert_eq!(load(dir.path()), choice);
        let text = settings::read(dir.path()).1.unwrap().unwrap();
        assert!(text.starts_with(original), "{text}");
    }
}
