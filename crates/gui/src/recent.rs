// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Recent` list's own table: `[recent]` in `<config_dir>/settings.toml`,
//! capped at [`MAX_ENTRIES`](crate::recent::MAX_ENTRIES), most recent first.
//! No `use slint::` anywhere in this file, mirroring `crate::input_config`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{entry, settings};

/// The most entries the file ever holds.
pub const MAX_ENTRIES: usize = 5;

/// One saved project, as the file and the rail both hold it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub name: String,
    pub folder: PathBuf,
    /// The exact path a click on this row reopens: the project's own `.ffp`.
    pub path: PathBuf,
    /// Unix seconds.
    pub opened_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentFile {
    #[serde(default)]
    entries: Vec<RecentEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct SettingsWrapper {
    #[serde(default)]
    recent: Option<RecentFile>,
}

/// Loads `[recent]` from `settings.toml`, keeping only rows whose `path`
/// has the project extension; whether the file exists is not checked. Any
/// failure -- the table absent, broken (a row without `path` included), or
/// the file unreadable -- reads as an empty list; there is nothing here
/// worth refusing a whole file over, unlike `input_config::load`'s bindings.
pub fn load(config_dir: &Path) -> Vec<RecentEntry> {
    let (_, read_result) = settings::read(config_dir);
    let src = match read_result {
        Ok(text) => text.unwrap_or_default(),
        // The file exists but could not be read (permissions, non-UTF-8
        // bytes, a directory in its place) -- treated the same as broken.
        Err(_) => return Vec::new(),
    };

    match toml::from_str::<SettingsWrapper>(&src) {
        Ok(SettingsWrapper {
            recent: Some(file), ..
        }) => file
            .entries
            .into_iter()
            .filter(|e| entry::has_project_extension(&e.path))
            .collect(),
        _ => Vec::new(),
    }
}

/// Writes `entries` as `settings.toml`'s `[recent]` table, creating
/// `config_dir` if needed. Every other table in the file is left as it was.
pub fn save(config_dir: &Path, entries: &[RecentEntry]) -> Result<PathBuf, settings::WriteError> {
    let table = settings::to_table(&RecentFile {
        entries: entries.to_vec(),
    });
    settings::write_table(config_dir, "recent", table)
}

/// Records an open or a save at the front of `entries`, dropping any existing
/// entry for the same target and capping the result at [`MAX_ENTRIES`].
/// Two entries name the same target when their `path`s match.
pub fn record(
    entries: &mut Vec<RecentEntry>,
    name: String,
    folder: PathBuf,
    path: PathBuf,
    opened_at: i64,
) {
    entries.retain(|e| e.path != path);
    entries.insert(
        0,
        RecentEntry {
            name,
            folder,
            path,
            opened_at,
        },
    );
    entries.truncate(MAX_ENTRIES);
}

/// A civil day count (days since the Unix epoch, UTC).
fn civil_day(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(86_400)
}

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since the Unix epoch -> `(year, month, day)`, proleptic Gregorian --
/// a compact public-domain day-to-calendar algorithm, since this workspace
/// carries no date crate.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// `"today"`, `"yesterday"`, or `"D Mon"` (e.g. `"21 Aug"`) -- both
/// `opened_at` and `now` are Unix seconds, so a test controls "now" without
/// touching the system clock.
pub fn format_date(opened_at: i64, now: i64) -> String {
    match civil_day(now) - civil_day(opened_at) {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        _ => {
            let (_, month, day) = civil_from_days(civil_day(opened_at));
            format!("{day} {}", MONTH_NAMES[(month - 1) as usize])
        }
    }
}
