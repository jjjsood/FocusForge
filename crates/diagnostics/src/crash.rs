// SPDX-License-Identifier: GPL-3.0-or-later

//! The panic hook and the crash report it writes: one JSON object per
//! panic, holding the panic's own facts and the flight recorder's dump.

use std::backtrace::Backtrace;
use std::fs::File;
use std::io::{self, Write};
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::ring::Ring;

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

/// Where a data directory keeps its crash evidence.
pub fn crash_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("crashes")
}

/// Every readable file in `dir` as a (path, file name) pair, by name. A
/// directory that cannot be read yields nothing.
pub(crate) fn read_dir_sorted(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, String)> = read
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            Some((entry.path(), name))
        })
        .collect();
    files.sort_by(|a, b| a.1.cmp(&b.1));
    files
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
    let seconds = focusforge_core::project::seal::now_unix();
    let location = info.location();
    let report = serde_json::json!({
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

    let stem = report_stem(seconds, binary, std::process::id());
    let (mut file, path) = create_new_numbered(crash_dir, &stem, "json").ok()?;
    file.write_all(&body).ok()?;
    file.flush().ok()?;
    Some(path)
}

/// The number of stems tried before giving up, which bounds the work done
/// inside a panic hook.
const MAX_STEM_ATTEMPTS: u32 = 64;

/// Creates `<dir>/<stem>.<extension>`, or the first free
/// `<stem>-2.<extension>`, `<stem>-3.<extension>`, ... when that name is
/// taken. Two crash events in the same second of the same process share a
/// stem, and neither may overwrite the other.
pub(crate) fn create_new_numbered(
    dir: &Path,
    stem: &str,
    extension: &str,
) -> io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    for attempt in 1..=MAX_STEM_ATTEMPTS {
        let name = if attempt == 1 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem}-{attempt}.{extension}")
        };
        let path = dir.join(name);
        match File::create_new(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{MAX_STEM_ATTEMPTS} crash files already carry the stem {stem}"),
    ))
}

/// `<YYYYMMDD>T<HHMMSS>Z-<binary>-<pid>`: basic ISO 8601, whose lack of a
/// `:` is what makes the name legal on every target filesystem. Full
/// RFC 3339 precision lives in the report body instead.
pub(crate) fn report_stem(unix_seconds: i64, binary: &str, pid: u32) -> String {
    format!("{}-{binary}-{pid}", utc_stamp(unix_seconds))
}

/// `<YYYYMMDD>T<HHMMSS>Z`: the timestamp half of [`report_stem`], and the
/// one colon-free spelling of an instant this crate puts into a file name.
pub(crate) fn utc_stamp(unix_seconds: i64) -> String {
    let (year, month, day) = civil_from_unix(unix_seconds);
    let (hour, minute, second) = time_of_day(unix_seconds);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// The same instant as an RFC 3339 UTC timestamp.
pub fn rfc3339(unix_seconds: i64) -> String {
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
    fn the_timestamp_half_of_a_stem_is_the_stem_without_its_binary_and_pid() {
        assert_eq!(utc_stamp(1789482330), "20260915T142530Z");
        assert!(
            report_stem(1789482330, "focusforge-cli", 4242).starts_with(&utc_stamp(1789482330))
        );
    }

    #[test]
    fn a_second_file_under_one_stem_is_numbered_rather_than_lost() {
        let dir = tempfile::tempdir().unwrap();
        let stem = "20260917T142530Z-focusforge-cli-7";

        let (mut first, first_path) = create_new_numbered(dir.path(), stem, "json").unwrap();
        first.write_all(b"first").unwrap();
        let (mut second, second_path) = create_new_numbered(dir.path(), stem, "json").unwrap();
        second.write_all(b"second").unwrap();
        let (_, third_path) = create_new_numbered(dir.path(), stem, "json").unwrap();

        assert_eq!(
            first_path.file_name().unwrap(),
            "20260917T142530Z-focusforge-cli-7.json"
        );
        assert_eq!(
            second_path.file_name().unwrap(),
            "20260917T142530Z-focusforge-cli-7-2.json"
        );
        assert_eq!(
            third_path.file_name().unwrap(),
            "20260917T142530Z-focusforge-cli-7-3.json"
        );
        assert_eq!(std::fs::read_to_string(&first_path).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&second_path).unwrap(), "second");
    }

    #[test]
    fn a_stem_differing_only_in_pid_stays_unsuffixed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, first) = create_new_numbered(
            dir.path(),
            &report_stem(1789482330, "focusforge-cli", 7),
            "json",
        )
        .unwrap();
        let (_, second) = create_new_numbered(
            dir.path(),
            &report_stem(1789482330, "focusforge-cli", 8),
            "json",
        )
        .unwrap();

        assert_eq!(
            first.file_name().unwrap(),
            "20260915T142530Z-focusforge-cli-7.json"
        );
        assert_eq!(
            second.file_name().unwrap(),
            "20260915T142530Z-focusforge-cli-8.json"
        );
    }

    #[test]
    fn the_minidump_and_its_metadata_share_one_stem() {
        let dir = tempfile::tempdir().unwrap();
        let (_, dump) =
            create_new_numbered(dir.path(), "20260917T142530Z-focusforge-cli-7", "dmp").unwrap();
        let metadata = dump.with_extension("json");

        assert_eq!(dump.file_stem(), metadata.file_stem());
        assert_eq!(dump.extension().unwrap(), "dmp");
    }

    #[test]
    fn the_body_timestamp_keeps_full_rfc_3339_precision() {
        assert_eq!(rfc3339(1789482330), "2026-09-15T14:25:30Z");
        assert_eq!(rfc3339(1709208000), "2024-02-29T12:00:00Z");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }
}
