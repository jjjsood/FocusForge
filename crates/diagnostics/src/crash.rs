// SPDX-License-Identifier: GPL-3.0-or-later

//! The panic hook and the crash report it writes: one JSON object per
//! panic, holding the panic's own facts and the flight recorder's dump.

use std::backtrace::Backtrace;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ring::Ring;

/// The crash report's schema version. Additive-only: a later field must not
/// break a reader written against this one.
const CRASH_REPORT_FORMAT: u32 = 1;

/// The path of the last report this process wrote, so a caught panic can
/// point at it.
static LAST_REPORT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The path of the most recent crash report this process wrote.
pub fn last_report() -> Option<PathBuf> {
    LAST_REPORT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Installs a panic hook that writes a crash report into `crash_dir` and
/// then runs whatever hook was installed before, so the default panic text
/// still reaches stderr and a test harness keeps its own behaviour.
///
/// The hook itself never panics and never blocks: every I/O error is
/// swallowed, and the ring is read without waiting for its lock.
pub(crate) fn install_hook(crash_dir: PathBuf, ring: Ring, binary: String) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = Backtrace::force_capture().to_string();
        if let Some(path) = write_report(&crash_dir, &binary, info, &backtrace, ring.dump()) {
            *LAST_REPORT.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
        }
        previous(info);
    }));
}

/// Writes one report, returning its path, or `None` when anything about
/// writing it failed -- a panic that cannot be recorded must still reach
/// the rest of the hook chain.
fn write_report(
    crash_dir: &Path,
    binary: &str,
    info: &PanicHookInfo<'_>,
    backtrace: &str,
    ring: Vec<String>,
) -> Option<PathBuf> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let location = info.location();
    let report = serde_json::json!({
        "format": CRASH_REPORT_FORMAT,
        "timestamp": rfc3339(seconds),
        "payload": crate::panic_payload(info.payload()),
        "file": location.map(|l| l.file()),
        "line": location.map(|l| l.line()),
        "column": location.map(|l| l.column()),
        "thread": std::thread::current().name().unwrap_or("unnamed"),
        "backtrace": backtrace,
        "ring": ring,
    });
    let body = serde_json::to_vec_pretty(&report).ok()?;

    std::fs::create_dir_all(crash_dir).ok()?;
    let path = crash_dir.join(format!(
        "{}.json",
        report_stem(seconds, binary, std::process::id())
    ));
    let mut file = std::fs::File::create_new(&path).ok()?;
    file.write_all(&body).ok()?;
    file.flush().ok()?;
    Some(path)
}

/// `<YYYYMMDD>T<HHMMSS>Z-<binary>-<pid>`: basic ISO 8601, whose lack of a
/// `:` is what makes the name legal on every target filesystem. Full
/// RFC 3339 precision lives in the report body instead.
fn report_stem(unix_seconds: i64, binary: &str, pid: u32) -> String {
    let (year, month, day) = civil_from_unix(unix_seconds);
    let (hour, minute, second) = time_of_day(unix_seconds);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z-{binary}-{pid}")
}

/// The same instant as an RFC 3339 UTC timestamp.
fn rfc3339(unix_seconds: i64) -> String {
    let (year, month, day) = civil_from_unix(unix_seconds);
    let (hour, minute, second) = time_of_day(unix_seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn time_of_day(unix_seconds: i64) -> (i64, i64, i64) {
    let secs_of_day = unix_seconds.rem_euclid(86_400);
    (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    )
}

/// The proleptic Gregorian date of a Unix timestamp, by era arithmetic:
/// a 400-year era holds exactly 146_097 days, which makes the leap rule
/// exact for every year without a calendar dependency.
fn civil_from_unix(unix_seconds: i64) -> (i64, i64, i64) {
    let days = unix_seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // March-based month index, so a leap day is the last day of the year.
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_is_colon_free_basic_iso_8601() {
        assert_eq!(
            report_stem(1789482330, "focusforge-cli", 4242),
            "20260915T142530Z-focusforge-cli-4242"
        );
        assert_eq!(
            report_stem(1709208000, "focusforge-gui", 7),
            "20240229T120000Z-focusforge-gui-7"
        );
        assert!(!report_stem(0, "focusforge-cli", 1).contains(':'));
    }

    #[test]
    fn the_body_timestamp_keeps_full_rfc_3339_precision() {
        assert_eq!(rfc3339(1789482330), "2026-09-15T14:25:30Z");
        assert_eq!(rfc3339(1709208000), "2024-02-29T12:00:00Z");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }
}
