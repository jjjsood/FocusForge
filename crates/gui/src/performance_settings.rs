// SPDX-License-Identifier: GPL-3.0-or-later

//! The `[performance]` table of `settings.toml` -- GPU processing on or off
//! and an optional ceiling on the host-memory and device budgets -- and the
//! pure view the Performance tab shows. A cap is kept as size text, in the
//! syntax of `--max-host-memory` and `--max-vram`
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
    /// The probed device budget before any cap, in bytes.
    Ready(u64),
    /// A run tried the GPU and found no usable adapter.
    NoAdapter,
}

impl DeviceState {
    /// The most a VRAM cap can name; unknown before a probe.
    pub fn maximum(self) -> Option<u64> {
        match self {
            DeviceState::Ready(bytes) => Some(bytes),
            DeviceState::NotProbed | DeviceState::NoAdapter => None,
        }
    }
}

/// The unit of a cap row's number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapUnit {
    Mib,
    Gib,
}

impl CapUnit {
    pub const fn label(self) -> &'static str {
        match self {
            CapUnit::Mib => "MiB",
            CapUnit::Gib => "GiB",
        }
    }

    pub fn from_label(label: &str) -> Option<CapUnit> {
        [CapUnit::Mib, CapUnit::Gib]
            .into_iter()
            .find(|unit| unit.label() == label)
    }

    const fn bytes(self) -> u64 {
        match self {
            CapUnit::Mib => 1 << 20,
            CapUnit::Gib => 1 << 30,
        }
    }
}

/// What a cap row shows beyond its stored cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapNotes {
    /// Size text the user confirmed that is not a cap. The stored cap stays as
    /// it was; the row shows this text in the failure tone.
    pub rejected: Option<String>,
    /// The line under the row naming the maximum a confirmed number exceeded.
    pub limit: Option<String>,
    /// The unit picked while the row had no number to keep it in.
    pub unit: Option<CapUnit>,
}

/// One cap row as the tab draws it: a number, a unit and the line under it.
#[derive(Debug, Clone, PartialEq)]
pub struct CapField {
    /// The number shown; empty while the placeholder shows.
    pub text: String,
    /// Shown while `text` is empty.
    pub placeholder: &'static str,
    pub unit: CapUnit,
    /// The number does not make a cap.
    pub invalid: bool,
    /// False when the row is unavailable: one disabled box.
    pub enabled: bool,
    /// How far the entry is scrolled inside the field, zero or negative.
    pub scroll_offset: f32,
    /// The error line under the row; empty when there is none.
    pub limit: String,
    pub unit_open: bool,
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
    /// The VRAM cap changed after this session's budget settled; empty
    /// otherwise.
    pub note: &'static str,
}

/// The start of every `job::resolve_backend` line.
const BACKEND_PREFIX: &str = "fusion backend: ";
/// Shown in the VRAM number before any run has tried the GPU.
pub const NOT_PROBED: &str = "Not probed yet";
/// The VRAM row without an adapter.
pub const UNAVAILABLE: &str = "Unavailable";
/// Shown while an edited VRAM cap waits for the next launch.
pub const VRAM_PENDING_NOTE: &str =
    "Caps only lower the probed budgets. Changes apply from the next start.";

/// Builds the tab's view. `host_budget` is the uncapped derived host budget in
/// bytes, the default the host row shows and the most it takes;
/// `backend_line` is the last run's `job::resolve_backend` line.
pub fn view(
    settings: &PerformanceSettings,
    host_notes: &CapNotes,
    vram_notes: &CapNotes,
    host_budget: u64,
    device: DeviceState,
    backend_line: Option<&str>,
    vram_pending: bool,
) -> PerformanceView {
    let field = |stored: &Option<Cap>,
                 notes: &CapNotes,
                 default: Option<u64>,
                 placeholder: &'static str| {
        let (text, unit) = if let Some(rejected) = &notes.rejected {
            split_labelled(rejected).unwrap_or_else(|| (rejected.clone(), CapUnit::Gib))
        } else if let Some(cap) = stored {
            split_cap(cap)
        } else if let Some(bytes) = default {
            let (number, unit) = floor_to_unit(bytes);
            (number.to_string(), unit)
        } else {
            (String::new(), notes.unit.unwrap_or(CapUnit::Gib))
        };
        CapField {
            text,
            placeholder,
            unit,
            invalid: notes.rejected.is_some(),
            enabled: true,
            scroll_offset: 0.0,
            limit: notes.limit.clone().unwrap_or_default(),
            unit_open: false,
        }
    };
    let vram_cap = match device {
        DeviceState::NoAdapter => CapField {
            text: String::new(),
            placeholder: UNAVAILABLE,
            unit: CapUnit::Gib,
            invalid: false,
            enabled: false,
            scroll_offset: 0.0,
            limit: String::new(),
            unit_open: false,
        },
        DeviceState::NotProbed => field(&settings.max_vram, vram_notes, None, NOT_PROBED),
        DeviceState::Ready(bytes) => field(&settings.max_vram, vram_notes, Some(bytes), ""),
    };
    PerformanceView {
        gpu: settings.gpu,
        gpu_enabled: device != DeviceState::NoAdapter,
        backend: backend_line
            .map(|line| line.strip_prefix(BACKEND_PREFIX).unwrap_or(line))
            .unwrap_or_default()
            .to_string(),
        host_cap: field(&settings.max_host_memory, host_notes, Some(host_budget), ""),
        vram_cap,
        note: if vram_pending { VRAM_PENDING_NOTE } else { "" },
    }
}

/// What a confirmed number and unit store, and the line to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FittedCap {
    /// `None`: no cap, the row returns to its default.
    pub cap: Option<Cap>,
    /// Names the maximum when the number was above it.
    pub limit: Option<String>,
}

/// Turns a confirmed `number` in `unit` into the cap to store. An empty
/// number, or a size at or above `max` (`None` when it is not known), is no
/// cap; a size above `max` also yields the line naming it. A number that is
/// zero or too large for a byte count is refused.
pub fn fit_cap(number: &str, unit: CapUnit, max: Option<u64>) -> Result<FittedCap, String> {
    let number = number.trim();
    if number.is_empty() {
        return Ok(FittedCap {
            cap: None,
            limit: None,
        });
    }
    let cap = parse_cap(&format!("{number}{}", unit.label()))?
        .expect("a non-empty size text parses to a cap");
    match max {
        Some(max) if cap.bytes.get() >= max => Ok(FittedCap {
            cap: None,
            limit: (cap.bytes.get() > max).then(|| format!("Max {} possible.", format_bytes(max))),
        }),
        _ => Ok(FittedCap {
            cap: Some(cap),
            limit: None,
        }),
    }
}

/// `text` as digits followed by a MiB or GiB suffix, in the number and unit a
/// row shows.
fn split_labelled(text: &str) -> Option<(String, CapUnit)> {
    let lower = text.trim().to_ascii_lowercase();
    [CapUnit::Mib, CapUnit::Gib].into_iter().find_map(|unit| {
        let number = lower
            .strip_suffix(&unit.label().to_ascii_lowercase())?
            .trim_end();
        let digits = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
        digits.then(|| (number.to_string(), unit))
    })
}

/// A stored cap as a row's number and unit: its own MiB or GiB suffix, else
/// GiB when it is a whole number of them, else MiB floored.
fn split_cap(cap: &Cap) -> (String, CapUnit) {
    split_labelled(&cap.text).unwrap_or_else(|| {
        let bytes = cap.bytes.get();
        let unit = if bytes.is_multiple_of(CapUnit::Gib.bytes()) {
            CapUnit::Gib
        } else {
            CapUnit::Mib
        };
        ((bytes / unit.bytes()).to_string(), unit)
    })
}

/// A budget as a whole number: GiB from a gibibyte up and MiB below, floored.
fn floor_to_unit(bytes: u64) -> (u64, CapUnit) {
    let unit = if bytes >= CapUnit::Gib.bytes() {
        CapUnit::Gib
    } else {
        CapUnit::Mib
    };
    (bytes / unit.bytes(), unit)
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
    const MIB: u64 = 1 << 20;

    fn view_of(
        settings: &PerformanceSettings,
        notes: &CapNotes,
        host_budget: u64,
        device: DeviceState,
    ) -> PerformanceView {
        view(settings, notes, notes, host_budget, device, None, false)
    }

    fn default_view(device: DeviceState) -> PerformanceView {
        view_of(
            &PerformanceSettings::default(),
            &CapNotes::default(),
            48 * GIB,
            device,
        )
    }

    /// The host row for a stored cap (or none) under a host budget.
    fn host_row(stored: Option<&str>, host_budget: u64) -> CapField {
        let settings = PerformanceSettings {
            max_host_memory: stored.and_then(cap),
            ..PerformanceSettings::default()
        };
        view_of(
            &settings,
            &CapNotes::default(),
            host_budget,
            DeviceState::NotProbed,
        )
        .host_cap
    }

    fn shown(field: &CapField) -> (&str, CapUnit) {
        (field.text.as_str(), field.unit)
    }

    #[test]
    fn cap_row_splits_stored_text_into_number_and_unit() {
        use CapUnit::{Gib, Mib};
        for (stored, expected) in [
            ("8GiB", ("8", Gib)),
            ("512MiB", ("512", Mib)),
            ("1 GiB", ("1", Gib)),
            ("512mib", ("512", Mib)),
            // Another suffix: whole GiB show as GiB, anything else in MiB floored.
            ("2TiB", ("2048", Gib)),
            ("1073741824", ("1", Gib)),
            ("1500MB", ("1430", Mib)),
            ("1048576", ("1", Mib)),
            ("1 MB", ("0", Mib)),
            ("4096", ("0", Mib)),
        ] {
            let field = host_row(Some(stored), 48 * GIB);
            assert_eq!(shown(&field), expected, "{stored:?}");
            assert!(!field.invalid && field.limit.is_empty(), "{stored:?}");
        }
    }

    #[test]
    fn cap_row_keeps_a_stored_cap_above_the_current_budget() {
        let field = host_row(Some("8GiB"), 4 * GIB);
        assert_eq!(shown(&field), ("8", CapUnit::Gib));
        assert!(field.limit.is_empty(), "the line follows a commit only");
    }

    #[test]
    fn cap_row_shows_the_default_budget_without_a_cap() {
        use CapUnit::{Gib, Mib};
        for (budget, expected) in [
            (48 * GIB + GIB * 2 / 5, ("48", Gib)),
            (GIB, ("1", Gib)),
            (GIB - 1, ("1023", Mib)),
            (300 * MIB, ("300", Mib)),
        ] {
            let host = host_row(None, budget);
            assert_eq!(shown(&host), expected, "host {budget}");
            assert!(host.enabled && !host.invalid && host.limit.is_empty());
            let vram = default_view(DeviceState::Ready(budget)).vram_cap;
            assert_eq!(shown(&vram), expected, "vram {budget}");
            assert!(vram.enabled && !vram.invalid && vram.limit.is_empty());
        }
    }

    #[test]
    fn cap_row_without_a_known_budget_shows_the_placeholder_and_the_picked_unit() {
        let field = default_view(DeviceState::NotProbed).vram_cap;
        assert_eq!(shown(&field), ("", CapUnit::Gib));
        assert_eq!(field.placeholder, NOT_PROBED);
        assert!(field.enabled);

        let notes = CapNotes {
            unit: Some(CapUnit::Mib),
            ..CapNotes::default()
        };
        let picked = view_of(
            &PerformanceSettings::default(),
            &notes,
            GIB,
            DeviceState::NotProbed,
        )
        .vram_cap;
        assert_eq!(shown(&picked), ("", CapUnit::Mib));
    }

    #[test]
    fn cap_row_shows_refused_text_in_the_failure_tone_and_the_limit_line() {
        let settings = PerformanceSettings {
            max_host_memory: cap("2GiB"),
            ..PerformanceSettings::default()
        };
        for (refused, expected) in [
            ("0MiB", ("0", CapUnit::Mib)),
            (
                "99999999999999999999GiB",
                ("99999999999999999999", CapUnit::Gib),
            ),
            ("8G", ("8G", CapUnit::Gib)),
        ] {
            let notes = CapNotes {
                rejected: Some(refused.to_string()),
                ..CapNotes::default()
            };
            let field = view_of(&settings, &notes, 48 * GIB, DeviceState::NotProbed).host_cap;
            assert_eq!(shown(&field), expected, "{refused:?}");
            assert!(field.invalid, "{refused:?}");
        }

        let notes = CapNotes {
            limit: Some("Max 48.0 GiB possible.".to_string()),
            ..CapNotes::default()
        };
        let field = view_of(&settings, &notes, 48 * GIB, DeviceState::NotProbed).host_cap;
        assert_eq!(field.limit, "Max 48.0 GiB possible.");
        assert!(!field.invalid);
    }

    #[test]
    fn fit_cap_clamps_above_the_maximum_and_names_it() {
        use CapUnit::{Gib, Mib};
        let fit = |number, unit, max| fit_cap(number, unit, max).unwrap();
        let stores = |text: &str| FittedCap {
            cap: cap(text),
            limit: None,
        };
        let no_cap_with = |line: &str| FittedCap {
            cap: None,
            limit: Some(line.to_string()),
        };
        let ten = Some(10 * GIB);

        assert_eq!(fit("9", Gib, ten), stores("9GiB"));
        assert_eq!(fit("10239", Mib, ten), stores("10239MiB"));
        assert_eq!(fit("12", Gib, ten), no_cap_with("Max 10.0 GiB possible."));
        assert_eq!(
            fit("10241", Mib, ten),
            no_cap_with("Max 10.0 GiB possible.")
        );
        assert_eq!(
            fit("800", Mib, Some(700 * MIB)),
            no_cap_with("Max 700.0 MiB possible.")
        );
        assert_eq!(
            fit("10", Gib, ten),
            FittedCap {
                cap: None,
                limit: None
            },
            "exactly the maximum is no cap and needs no line"
        );
        assert_eq!(fit("12", Gib, None), stores("12GiB"), "no maximum known");
        assert_eq!(
            fit("1000000", Gib, None),
            stores("1000000GiB"),
            "no maximum known"
        );
    }

    #[test]
    fn fit_cap_empty_number_is_no_cap_and_unusable_numbers_are_refused() {
        for blank in ["", "  "] {
            assert_eq!(
                fit_cap(blank, CapUnit::Gib, Some(GIB)),
                Ok(FittedCap {
                    cap: None,
                    limit: None
                })
            );
        }
        let zero = fit_cap("0", CapUnit::Mib, Some(GIB)).unwrap_err();
        assert!(zero.contains("zero"), "{zero}");
        let huge = fit_cap("18446744073709551615", CapUnit::Gib, None).unwrap_err();
        assert!(huge.contains("overflow"), "{huge}");
    }

    #[test]
    fn performance_settings_view_before_any_run() {
        let v = default_view(DeviceState::NotProbed);
        assert!(v.gpu && v.gpu_enabled);
        assert_eq!(v.backend, "");
        assert_eq!(shown(&v.host_cap), ("48", CapUnit::Gib));
        assert_eq!(shown(&v.vram_cap), ("", CapUnit::Gib));
        assert_eq!(v.vram_cap.placeholder, NOT_PROBED);
        for field in [&v.host_cap, &v.vram_cap] {
            assert!(field.enabled && !field.invalid && !field.unit_open);
            assert_eq!(field.limit, "");
        }
        assert_eq!(v.note, "");
    }

    #[test]
    fn performance_settings_view_without_an_adapter() {
        let settings = PerformanceSettings {
            gpu: false,
            max_vram: cap("2GiB"),
            ..PerformanceSettings::default()
        };
        let v = view_of(
            &settings,
            &CapNotes::default(),
            48 * GIB,
            DeviceState::NoAdapter,
        );
        assert!(!v.gpu, "the switch keeps the stored value");
        assert!(!v.gpu_enabled);
        assert_eq!(
            v.vram_cap,
            CapField {
                text: String::new(),
                placeholder: UNAVAILABLE,
                unit: CapUnit::Gib,
                invalid: false,
                enabled: false,
                scroll_offset: 0.0,
                limit: String::new(),
                unit_open: false,
            },
            "one disabled box, the stored cap not shown"
        );
        assert!(v.host_cap.enabled, "the host cap row is unchanged");
    }

    #[test]
    fn performance_settings_view_shows_the_stored_caps() {
        let settings = PerformanceSettings {
            max_host_memory: cap("2GiB"),
            max_vram: cap("512MiB"),
            ..PerformanceSettings::default()
        };
        let v = view_of(
            &settings,
            &CapNotes::default(),
            48 * GIB,
            DeviceState::Ready(8 * GIB),
        );
        assert_eq!(shown(&v.host_cap), ("2", CapUnit::Gib));
        assert_eq!(shown(&v.vram_cap), ("512", CapUnit::Mib));
    }

    #[test]
    fn performance_settings_view_drops_the_backend_prefix() {
        let line = "fusion backend: cpu — GPU processing is off";
        let show = |line| {
            view(
                &PerformanceSettings::default(),
                &CapNotes::default(),
                &CapNotes::default(),
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
            &CapNotes::default(),
            &CapNotes::default(),
            GIB,
            DeviceState::Ready(GIB),
            None,
            true,
        );
        assert_eq!(v.note, VRAM_PENDING_NOTE);
    }

    #[test]
    fn device_state_maximum_is_known_only_after_a_probe() {
        assert_eq!(DeviceState::Ready(GIB).maximum(), Some(GIB));
        assert_eq!(DeviceState::NotProbed.maximum(), None);
        assert_eq!(DeviceState::NoAdapter.maximum(), None);
    }
}
