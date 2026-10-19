// SPDX-License-Identifier: GPL-3.0-or-later

//! The `[performance]` table of `settings.toml` -- GPU processing on or off
//! and an optional ceiling on the host-memory and device budgets -- and the
//! pure view the Performance tab shows. A cap is kept as the text the user
//! typed, in the size syntax of `--max-host-memory` and `--max-vram`
//! ([`focusforge_core::bytes::parse_byte_size`]); it can only lower a budget.
//! No `use slint::` anywhere in this file.

use std::num::NonZeroU64;
use std::path::Path;

use focusforge_core::bytes::parse_byte_size;
use serde::Serialize;

use crate::settings::{self, WriteError};

/// A budget ceiling: the text as typed and the byte count it parses to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cap {
    pub text: String,
    pub bytes: NonZeroU64,
}

/// What the Performance tab edits. `None` for a cap means no cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerformanceSettings {
    pub gpu: bool,
    pub max_host_memory: Option<Cap>,
    pub max_vram: Option<Cap>,
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        PerformanceSettings {
            gpu: true,
            max_host_memory: None,
            max_vram: None,
        }
    }
}

impl PerformanceSettings {
    pub fn max_host_memory_bytes(&self) -> Option<u64> {
        self.max_host_memory.as_ref().map(|cap| cap.bytes.get())
    }

    pub fn max_vram_bytes(&self) -> Option<u64> {
        self.max_vram.as_ref().map(|cap| cap.bytes.get())
    }
}

/// Parses a cap field: empty (or blank) is no cap, anything else must be a
/// size [`parse_byte_size`] accepts and at least one byte.
pub fn parse_cap(text: &str) -> Result<Option<Cap>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let bytes = parse_byte_size(text)?;
    let bytes = NonZeroU64::new(bytes)
        .ok_or_else(|| format!("{text:?} is zero; a cap needs at least one byte"))?;
    Ok(Some(Cap {
        text: text.to_string(),
        bytes,
    }))
}

/// Loads `[performance]`. A file or table that is absent, unreadable or not
/// parsing reads as the defaults; an entry that is the wrong type or not a
/// valid cap falls back alone. Every fallback is logged.
pub fn load(config_dir: &Path) -> PerformanceSettings {
    let (settings, problems) = read_settings(config_dir);
    for problem in problems {
        tracing::warn!("{problem}");
    }
    settings
}

/// [`load`] without the logging: the settings and one line per fallback.
fn read_settings(config_dir: &Path) -> (PerformanceSettings, Vec<String>) {
    let defaults = PerformanceSettings::default();
    let (path, read) = settings::read(config_dir);
    let whole_file_problem = |what: &dyn std::fmt::Display| {
        (
            PerformanceSettings::default(),
            vec![format!(
                "{}: {what}, using the default performance settings",
                path.display()
            )],
        )
    };
    let src = match read {
        Ok(Some(src)) => src,
        Ok(None) => return (defaults, vec![]),
        Err(e) => return whole_file_problem(&format!("unreadable ({e})")),
    };
    let whole = match src.parse::<toml::Table>() {
        Ok(whole) => whole,
        Err(e) => return whole_file_problem(&format!("not valid TOML ({e})")),
    };
    let table = match whole.get("performance") {
        None => return (defaults, vec![]),
        Some(toml::Value::Table(table)) => table,
        Some(_) => return whole_file_problem(&"[performance] is not a table"),
    };

    let mut problems = Vec::new();
    let gpu = match table.get("gpu") {
        None => defaults.gpu,
        Some(toml::Value::Boolean(on)) => *on,
        Some(other) => {
            problems.push(format!(
                "[performance] gpu must be true or false, got {other}"
            ));
            defaults.gpu
        }
    };
    let mut cap_entry = |key: &str| match table.get(key) {
        None => None,
        Some(toml::Value::String(text)) => parse_cap(text).unwrap_or_else(|e| {
            problems.push(format!("[performance] {key}: {e}"));
            None
        }),
        Some(other) => {
            problems.push(format!(
                "[performance] {key} must be a size string, got {other}"
            ));
            None
        }
    };
    let max_host_memory = cap_entry("max-host-memory");
    let max_vram = cap_entry("max-vram");
    (
        PerformanceSettings {
            gpu,
            max_host_memory,
            max_vram,
        },
        problems,
    )
}

/// Writes `settings` as `settings.toml`'s `[performance]` table, only the
/// caps that are set; every other table is left as it was.
pub fn save(config_dir: &Path, settings: &PerformanceSettings) -> Result<(), WriteError> {
    #[derive(Serialize)]
    struct File<'a> {
        gpu: bool,
        #[serde(rename = "max-host-memory", skip_serializing_if = "Option::is_none")]
        max_host_memory: Option<&'a str>,
        #[serde(rename = "max-vram", skip_serializing_if = "Option::is_none")]
        max_vram: Option<&'a str>,
    }
    fn text(cap: &Option<Cap>) -> Option<&str> {
        cap.as_ref().map(|cap| cap.text.as_str())
    }
    let table = settings::to_table(&File {
        gpu: settings.gpu,
        max_host_memory: text(&settings.max_host_memory),
        max_vram: text(&settings.max_vram),
    });
    settings::write_table(config_dir, "performance", table).map(|_| ())
}

/// The device budget as far as this session has learned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// No run has tried the GPU yet.
    NotProbed,
    /// The probed (and capped) device budget, in bytes.
    Ready(u64),
    /// A run tried the GPU and found no usable adapter.
    NoAdapter,
}

/// Cap field text the user typed that did not parse. The stored cap stays as
/// it was; the field shows this text in the failure tone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RejectedCaps {
    pub host_memory: Option<String>,
    pub vram: Option<String>,
}

/// One cap field as the tab draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct CapField {
    pub text: String,
    /// Shown while `text` is empty.
    pub placeholder: &'static str,
    /// The text does not parse as a cap.
    pub invalid: bool,
    pub enabled: bool,
    /// How far the entry is scrolled inside the field, zero or negative.
    pub scroll_offset: f32,
}

/// Everything the Performance tab shows, as plain strings and flags.
#[derive(Debug, Clone, PartialEq)]
pub struct PerformanceView {
    pub gpu: bool,
    /// False once a run found no adapter: the switch then shows the stored
    /// value, disabled.
    pub gpu_enabled: bool,
    /// The last run's backend account without its `fusion backend: ` prefix;
    /// empty before any run.
    pub backend: String,
    pub host_cap: CapField,
    pub vram_cap: CapField,
    pub host_budget: String,
    pub vram_budget: String,
    /// `vram_budget` is a figure rather than "not probed yet" or
    /// "unavailable".
    pub vram_budget_known: bool,
    /// The VRAM cap changed after this session's budget settled; empty
    /// otherwise.
    pub note: &'static str,
}

/// The start of every `job::resolve_backend` line.
const BACKEND_PREFIX: &str = "fusion backend: ";
/// Shown while a cap field is empty.
pub const NO_CAP: &str = "No cap";
/// The device budget before any run has tried the GPU.
pub const NOT_PROBED: &str = "Not probed yet";
/// The device budget and the VRAM cap field without an adapter.
pub const UNAVAILABLE: &str = "Unavailable";
/// Shown while an edited VRAM cap waits for the next launch.
pub const VRAM_PENDING_NOTE: &str =
    "Caps only lower the probed budgets. Changes apply from the next start.";

/// Builds the tab's view. `host_budget` is the derived host budget in bytes;
/// `backend_line` is the last run's `job::resolve_backend` line.
pub fn view(
    settings: &PerformanceSettings,
    rejected: &RejectedCaps,
    host_budget: u64,
    device: DeviceState,
    backend_line: Option<&str>,
    vram_pending: bool,
) -> PerformanceView {
    let field = |stored: &Option<Cap>, rejected: &Option<String>| CapField {
        text: rejected
            .clone()
            .or_else(|| stored.as_ref().map(|cap| cap.text.clone()))
            .unwrap_or_default(),
        placeholder: NO_CAP,
        invalid: rejected.is_some(),
        enabled: true,
        scroll_offset: 0.0,
    };
    let (vram_cap, vram_budget) = match device {
        DeviceState::NotProbed => (
            field(&settings.max_vram, &rejected.vram),
            NOT_PROBED.to_string(),
        ),
        DeviceState::Ready(bytes) => (
            field(&settings.max_vram, &rejected.vram),
            format_bytes(bytes),
        ),
        DeviceState::NoAdapter => (
            CapField {
                text: String::new(),
                placeholder: UNAVAILABLE,
                invalid: false,
                enabled: false,
                scroll_offset: 0.0,
            },
            UNAVAILABLE.to_string(),
        ),
    };
    PerformanceView {
        gpu: settings.gpu,
        gpu_enabled: device != DeviceState::NoAdapter,
        backend: backend_line
            .map(|line| line.strip_prefix(BACKEND_PREFIX).unwrap_or(line))
            .unwrap_or_default()
            .to_string(),
        host_cap: field(&settings.max_host_memory, &rejected.host_memory),
        vram_cap,
        host_budget: format_bytes(host_budget),
        vram_budget,
        vram_budget_known: matches!(device, DeviceState::Ready(_)),
        note: if vram_pending { VRAM_PENDING_NOTE } else { "" },
    }
}

/// One decimal, GiB from a gibibyte up and MiB below.
fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = (1u64 << 20) as f64;
    const GIB: f64 = (1u64 << 30) as f64;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else {
        format!("{:.1} MiB", bytes / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;
    use std::fs;

    const CAPS: [&str; 4] = ["", "1", "2GiB", "18446744073709551615"];

    fn cap(text: &str) -> Option<Cap> {
        parse_cap(text).unwrap()
    }

    fn write_settings(dir: &Path, text: &str) {
        fs::write(dir.join(settings::FILE_NAME), text).unwrap();
    }

    fn stored(dir: &Path) -> String {
        settings::read(dir).1.unwrap().unwrap()
    }

    #[test]
    fn parse_cap_reads_the_cli_size_syntax() {
        assert_eq!(parse_cap(""), Ok(None));
        assert_eq!(parse_cap("  "), Ok(None));
        let two_gib = parse_cap("2GiB").unwrap().unwrap();
        assert_eq!(two_gib.text, "2GiB");
        assert_eq!(two_gib.bytes.get(), 2 << 30);
        assert_eq!(cap("1 MB").unwrap().bytes.get(), 1_000_000);
        for bad in ["0", "0GiB", "8G", "1.5GiB", "-1", "abc"] {
            assert!(parse_cap(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn performance_settings_default_is_gpu_on_without_caps() {
        let defaults = PerformanceSettings::default();
        assert!(defaults.gpu);
        assert_eq!(defaults.max_host_memory_bytes(), None);
        assert_eq!(defaults.max_vram_bytes(), None);
    }

    #[test]
    fn performance_settings_round_trip_every_combination() {
        for gpu in [true, false] {
            for host in CAPS {
                for vram in CAPS {
                    let dir = tempfile::tempdir().unwrap();
                    let original = PerformanceSettings {
                        gpu,
                        max_host_memory: cap(host),
                        max_vram: cap(vram),
                    };
                    save(dir.path(), &original).unwrap();
                    assert_eq!(load(dir.path()), original, "{gpu} {host:?} {vram:?}");
                }
            }
        }
    }

    #[test]
    fn performance_settings_save_writes_only_the_set_caps() {
        let dir = tempfile::tempdir().unwrap();
        save(
            dir.path(),
            &PerformanceSettings {
                gpu: false,
                max_host_memory: cap("2GiB"),
                max_vram: None,
            },
        )
        .unwrap();
        assert_eq!(
            stored(dir.path()),
            "[performance]\ngpu = false\nmax-host-memory = \"2GiB\"\n"
        );
    }

    #[test]
    fn performance_settings_save_leaves_hand_edited_siblings_byte_identical() {
        let before = "# my notes\n[recent]\nentries = [] # none yet\n\n";
        let input = "[input]\nzoom = 1\n\n";
        let export = "# export by hand\n[export]\nformat = \"png\"\n";
        let changed = PerformanceSettings {
            gpu: false,
            max_host_memory: cap("512MiB"),
            max_vram: cap("1"),
        };

        let dir = tempfile::tempdir().unwrap();
        let original = format!("{before}{input}{export}");
        write_settings(dir.path(), &original);
        save(dir.path(), &changed).unwrap();
        assert!(stored(dir.path()).starts_with(&original));
        assert_eq!(load(dir.path()), changed);

        let dir = tempfile::tempdir().unwrap();
        write_settings(
            dir.path(),
            &format!("{before}[performance]\ngpu = true\n\n{input}{export}"),
        );
        save(dir.path(), &changed).unwrap();
        let text = stored(dir.path());
        for sibling in [before, input, export] {
            assert!(text.contains(sibling), "{sibling:?} changed in {text:?}");
        }
        assert_eq!(load(dir.path()), changed);
    }

    #[test]
    fn performance_settings_save_refuses_an_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let broken = "[performance\ngpu = ";
        write_settings(dir.path(), broken);
        assert!(matches!(
            save(dir.path(), &PerformanceSettings::default()),
            Err(WriteError::Parse { .. })
        ));
        assert_eq!(stored(dir.path()), broken);
    }

    #[test]
    fn performance_settings_missing_file_and_table_load_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_settings(dir.path()),
            (PerformanceSettings::default(), vec![])
        );
        write_settings(dir.path(), "[recent]\nentries = []\n");
        assert_eq!(
            read_settings(dir.path()),
            (PerformanceSettings::default(), vec![])
        );
    }

    /// `[performance]` with `body`, loaded: the settings and the problems.
    fn load_body(body: &str) -> (PerformanceSettings, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        write_settings(dir.path(), &format!("[performance]\n{body}"));
        read_settings(dir.path())
    }

    #[test]
    fn performance_settings_fallback_malformed_table() {
        for text in ["[performance\ngpu = false\n", "performance = 3\n"] {
            let dir = tempfile::tempdir().unwrap();
            write_settings(dir.path(), text);
            let (settings, problems) = read_settings(dir.path());
            assert_eq!(settings, PerformanceSettings::default(), "{text:?}");
            assert_eq!(problems.len(), 1, "{text:?}: {problems:?}");
        }
    }

    #[test]
    fn performance_settings_fallback_wrong_types() {
        let (settings, problems) =
            load_body("gpu = \"yes\"\nmax-vram = 8\nmax-host-memory = \"1GiB\"\n");
        assert_eq!(
            settings,
            PerformanceSettings {
                gpu: true,
                max_host_memory: cap("1GiB"),
                max_vram: None,
            }
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn performance_settings_fallback_zero_cap() {
        for zero in ["0", "0GiB"] {
            let (settings, problems) = load_body(&format!(
                "gpu = false\nmax-vram = \"{zero}\"\nmax-host-memory = \"1GiB\"\n"
            ));
            assert_eq!(
                settings,
                PerformanceSettings {
                    gpu: false,
                    max_host_memory: cap("1GiB"),
                    max_vram: None,
                },
                "{zero}"
            );
            assert_eq!(problems.len(), 1, "{zero}: {problems:?}");
        }
    }

    #[test]
    fn performance_settings_fallback_unparseable_cap() {
        for bad in ["8G", "1.5GiB", "-1"] {
            let (settings, problems) = load_body(&format!(
                "gpu = false\nmax-host-memory = \"{bad}\"\nmax-vram = \"2GiB\"\n"
            ));
            assert_eq!(
                settings,
                PerformanceSettings {
                    gpu: false,
                    max_host_memory: None,
                    max_vram: cap("2GiB"),
                },
                "{bad}"
            );
            assert_eq!(problems.len(), 1, "{bad}: {problems:?}");
        }
    }

    #[test]
    fn performance_settings_fallback_unreadable_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(settings::FILE_NAME)).unwrap();
        let (settings, problems) = read_settings(dir.path());
        assert_eq!(settings, PerformanceSettings::default());
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    const GIB: u64 = 1 << 30;

    fn default_view(device: DeviceState) -> PerformanceView {
        view(
            &PerformanceSettings::default(),
            &RejectedCaps::default(),
            48 * GIB,
            device,
            None,
            false,
        )
    }

    #[test]
    fn performance_settings_view_before_any_run() {
        let v = default_view(DeviceState::NotProbed);
        assert!(v.gpu && v.gpu_enabled);
        assert_eq!(v.backend, "");
        assert_eq!(v.host_budget, "48.0 GiB");
        assert_eq!(v.vram_budget, NOT_PROBED);
        for field in [&v.host_cap, &v.vram_cap] {
            assert_eq!(field.text, "");
            assert_eq!(field.placeholder, NO_CAP);
            assert!(field.enabled && !field.invalid);
        }
        assert_eq!(v.note, "");
    }

    #[test]
    fn performance_settings_view_formats_the_probed_device_budget() {
        let v = default_view(DeviceState::Ready(15 * GIB + GIB * 2 / 5));
        assert_eq!(v.vram_budget, "15.4 GiB");
        assert!(v.vram_cap.enabled);
        assert_eq!(
            default_view(DeviceState::Ready(256 << 20)).vram_budget,
            "256.0 MiB"
        );
    }

    #[test]
    fn performance_settings_view_without_an_adapter() {
        let settings = PerformanceSettings {
            gpu: false,
            max_vram: cap("2GiB"),
            ..PerformanceSettings::default()
        };
        let v = view(
            &settings,
            &RejectedCaps::default(),
            48 * GIB,
            DeviceState::NoAdapter,
            None,
            false,
        );
        assert!(!v.gpu, "the switch keeps the stored value");
        assert!(!v.gpu_enabled);
        assert_eq!(v.vram_budget, UNAVAILABLE);
        assert_eq!(
            v.vram_cap,
            CapField {
                text: String::new(),
                placeholder: UNAVAILABLE,
                invalid: false,
                enabled: false,
                scroll_offset: 0.0,
            }
        );
        assert!(v.host_cap.enabled, "the host cap row is unchanged");
    }

    #[test]
    fn performance_settings_view_shows_stored_caps_and_rejected_text() {
        let settings = PerformanceSettings {
            max_host_memory: cap("2GiB"),
            max_vram: cap("1GiB"),
            ..PerformanceSettings::default()
        };
        let rejected = RejectedCaps {
            host_memory: None,
            vram: Some("8G".to_string()),
        };
        let v = view(
            &settings,
            &rejected,
            GIB,
            DeviceState::NotProbed,
            None,
            false,
        );
        assert_eq!(
            (v.host_cap.text.as_str(), v.host_cap.invalid),
            ("2GiB", false)
        );
        assert_eq!((v.vram_cap.text.as_str(), v.vram_cap.invalid), ("8G", true));
    }

    #[test]
    fn performance_settings_view_drops_the_backend_prefix() {
        let line = "fusion backend: cpu — GPU processing is off";
        let show = |line| {
            view(
                &PerformanceSettings::default(),
                &RejectedCaps::default(),
                GIB,
                DeviceState::NotProbed,
                line,
                false,
            )
            .backend
        };
        assert_eq!(show(Some(line)), "cpu — GPU processing is off");
        assert_eq!(show(Some("no prefix")), "no prefix");
        assert_eq!(show(None), "");
    }

    #[test]
    fn performance_settings_view_notes_a_pending_vram_cap() {
        let v = view(
            &PerformanceSettings::default(),
            &RejectedCaps::default(),
            GIB,
            DeviceState::Ready(GIB),
            None,
            true,
        );
        assert_eq!(v.note, VRAM_PENDING_NOTE);
    }
}
