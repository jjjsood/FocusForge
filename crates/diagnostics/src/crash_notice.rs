// SPDX-License-Identifier: GPL-3.0-or-later

//! Which crashes the user has not been told about yet. One crash is one
//! file stem in the crash directory: a minidump and its metadata share a
//! stem, a panic report owns its stem alone. A crash counts as reported
//! once a `<stem>.reported` marker sits beside its evidence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::crash::{crash_dir, read_dir_sorted, rfc3339};

/// The marker's extension. It is neither of the two extensions a bundle
/// collects, so a marker is never bundled and never counted as a crash of
/// its own.
const MARKER_EXTENSION: &str = "reported";

/// The file stems of every crash in `data_dir` that carries no reported
/// marker, sorted. No crash file's bytes are read, so a truncated report
/// still counts; a crash directory that cannot be read yields nothing.
pub fn unreported(data_dir: &Path) -> Vec<String> {
    let names: Vec<String> = read_dir_sorted(&crash_dir(data_dir))
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    let marker_suffix = format!(".{MARKER_EXTENSION}");
    let reported: BTreeSet<&str> = names
        .iter()
        .filter_map(|name| name.strip_suffix(&marker_suffix))
        .collect();
    names
        .iter()
        .filter_map(|name| evidence_stem(name))
        .filter(|stem| !reported.contains(stem))
        .map(str::to_string)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// Marks each crash id reported by writing `<id>.reported` into the crash
/// directory, holding the instant of the marking. Marking a crash twice
/// overwrites its marker. Every id is attempted, and the first error is
/// returned once all of them have been.
pub fn mark_reported(data_dir: &Path, crashes: &[String]) -> std::io::Result<()> {
    if crashes.is_empty() {
        return Ok(());
    }
    let dir = crash_dir(data_dir);
    let mut failure = std::fs::create_dir_all(&dir).err();
    let marked_at = rfc3339(focusforge_core::project::seal::now_unix());
    for id in crashes {
        if let Err(e) = std::fs::write(dir.join(format!("{id}.{MARKER_EXTENSION}")), &marked_at) {
            failure.get_or_insert(e);
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The crash ids among `paths`: the stems of those lying directly in
/// `data_dir`'s crash directory and ending in `.json` or `.dmp`, sorted.
/// Every other path, a collected log file included, yields nothing.
pub fn crash_ids(data_dir: &Path, paths: &[PathBuf]) -> Vec<String> {
    let dir = crash_dir(data_dir);
    paths
        .iter()
        .filter(|path| path.parent() == Some(dir.as_path()))
        .filter_map(|path| path.file_name()?.to_str().and_then(evidence_stem))
        .map(str::to_string)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// The stem of a crash evidence file name, or `None` for anything that is
/// neither a report or metadata file nor a minidump.
fn evidence_stem(name: &str) -> Option<&str> {
    name.strip_suffix(".json")
        .or_else(|| name.strip_suffix(".dmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic report, and a minidump with its metadata beside it.
    fn seed(crash_dir: &Path) {
        std::fs::create_dir_all(crash_dir).unwrap();
        std::fs::write(
            crash_dir.join("20260918T010203Z-focusforge-cli-7.json"),
            "{\"format\":1,\"payload\":\"boom\"}",
        )
        .unwrap();
        std::fs::write(
            crash_dir.join("20260918T010204Z-focusforge-cli-8.json"),
            "{\"format\":1,\"minidump\":\"20260918T010204Z-focusforge-cli-8.dmp\"}",
        )
        .unwrap();
        std::fs::write(
            crash_dir.join("20260918T010204Z-focusforge-cli-8.dmp"),
            b"MDMP\0\0\0\0",
        )
        .unwrap();
    }

    #[test]
    fn a_dump_and_its_metadata_are_one_crash() {
        let dir = tempfile::tempdir().unwrap();
        seed(&crate::crash::crash_dir(dir.path()));

        assert_eq!(
            unreported(dir.path()),
            vec![
                "20260918T010203Z-focusforge-cli-7".to_string(),
                "20260918T010204Z-focusforge-cli-8".to_string(),
            ]
        );
    }

    #[test]
    fn a_file_that_is_no_crash_evidence_is_counted_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let crashes = crate::crash::crash_dir(dir.path());
        std::fs::create_dir_all(&crashes).unwrap();
        std::fs::write(crashes.join("frame.tif"), b"II*\0").unwrap();
        std::fs::write(crashes.join("notes.txt"), b"hello").unwrap();

        assert!(unreported(dir.path()).is_empty());
    }

    #[test]
    fn an_empty_report_is_still_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let crashes = crate::crash::crash_dir(dir.path());
        std::fs::create_dir_all(&crashes).unwrap();
        std::fs::write(crashes.join("20260918T010203Z-focusforge-cli-7.json"), b"").unwrap();

        assert_eq!(
            unreported(dir.path()),
            vec!["20260918T010203Z-focusforge-cli-7".to_string()]
        );
    }

    #[test]
    fn a_missing_crash_directory_holds_no_crash() {
        let dir = tempfile::tempdir().unwrap();

        assert!(unreported(dir.path()).is_empty());
        assert!(unreported(&dir.path().join("absent")).is_empty());
    }

    #[test]
    fn marking_drops_a_crash_from_the_list_and_keeps_its_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let crashes = crate::crash::crash_dir(dir.path());
        seed(&crashes);

        mark_reported(
            dir.path(),
            &["20260918T010204Z-focusforge-cli-8".to_string()],
        )
        .unwrap();

        assert_eq!(
            unreported(dir.path()),
            vec!["20260918T010203Z-focusforge-cli-7".to_string()]
        );
        assert!(
            crashes
                .join("20260918T010204Z-focusforge-cli-8.json")
                .is_file()
        );
        assert!(
            crashes
                .join("20260918T010204Z-focusforge-cli-8.dmp")
                .is_file()
        );
    }

    #[test]
    fn marking_the_same_crash_twice_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        seed(&crate::crash::crash_dir(dir.path()));
        let ids = vec!["20260918T010203Z-focusforge-cli-7".to_string()];

        mark_reported(dir.path(), &ids).unwrap();
        mark_reported(dir.path(), &ids).unwrap();

        assert_eq!(
            unreported(dir.path()),
            vec!["20260918T010204Z-focusforge-cli-8".to_string()]
        );
    }

    #[test]
    fn a_marker_records_when_the_crash_was_reported() {
        let dir = tempfile::tempdir().unwrap();
        let crashes = crate::crash::crash_dir(dir.path());
        seed(&crashes);

        mark_reported(
            dir.path(),
            &["20260918T010203Z-focusforge-cli-7".to_string()],
        )
        .unwrap();

        let marker =
            std::fs::read_to_string(crashes.join("20260918T010203Z-focusforge-cli-7.reported"))
                .unwrap();
        assert!(
            marker.len() == 20 && marker.ends_with('Z') && marker.contains('T'),
            "the marker holds the instant it was written, got: {marker}"
        );
    }

    #[test]
    fn a_marker_enters_no_bundle() {
        let dir = tempfile::tempdir().unwrap();
        seed(&crate::crash::crash_dir(dir.path()));
        mark_reported(
            dir.path(),
            &[
                "20260918T010203Z-focusforge-cli-7".to_string(),
                "20260918T010204Z-focusforge-cli-8".to_string(),
            ],
        )
        .unwrap();

        let manifest = crate::bundle::manifest(&crate::bundle::Options {
            data_dir: Some(dir.path().to_path_buf()),
            config_dir: None,
            include_minidumps: true,
        });

        assert!(
            !manifest
                .entries
                .iter()
                .any(|entry| entry.name.ends_with(".reported")),
            "a marker is no crash evidence: {:?}",
            manifest.lines()
        );
    }

    #[test]
    fn only_the_crash_directory_yields_ids() {
        let dir = tempfile::tempdir().unwrap();
        let crashes = crate::crash::crash_dir(dir.path());

        let ids = crash_ids(
            dir.path(),
            &[
                dir.path().join("logs/focusforge.jsonl"),
                dir.path().join("logs/focusforge.1.jsonl"),
                crashes.join("20260918T010203Z-focusforge-cli-7.json"),
                crashes.join("20260918T010204Z-focusforge-cli-8.json"),
                crashes.join("20260918T010204Z-focusforge-cli-8.dmp"),
                crashes.join("frame.tif"),
                crashes.join("deeper/20260918T010205Z-focusforge-cli-9.json"),
            ],
        );

        assert_eq!(
            ids,
            vec![
                "20260918T010203Z-focusforge-cli-7".to_string(),
                "20260918T010204Z-focusforge-cli-8".to_string(),
            ]
        );
    }
}
