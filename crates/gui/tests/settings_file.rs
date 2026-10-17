// SPDX-License-Identifier: GPL-3.0-or-later

//! `settings.toml`'s own mechanics: per-table defaults, per-table isolation
//! on a broken table, saving one table without disturbing another, an
//! unparseable file left untouched and an unwritable directory not being
//! fatal.

use std::fs;

use focusforge_gui::input_config::{self, Provenance};
use focusforge_gui::input_map::InputTable;
use focusforge_gui::recent::{self, RecentEntry};

#[test]
fn appearance_presentation_loads_defaults_and_round_trips_saved_choice() {
    use focusforge_gui::appearance_settings::{self, AppearanceSettings, SourcePresentation};

    let absent = tempfile::tempdir().unwrap();
    assert_eq!(
        appearance_settings::load(absent.path()),
        AppearanceSettings::default(),
        "an absent settings file keeps the default presentation"
    );

    for source in [
        "[recent]\nkeep = \"yes\"\n",
        "[appearance]\npresentation = 3\n",
        "[appearance]\npresentation = \"gallery\"\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("settings.toml"), source).unwrap();
        assert_eq!(
            appearance_settings::load(dir.path()),
            AppearanceSettings::default(),
            "{source:?} must fall back without accepting an invalid appearance choice"
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(
        &path,
        "[recent]\nkeep = \"yes\"\n\n[appearance]\npresentation = \"list\"\n",
    )
    .unwrap();
    assert_eq!(
        appearance_settings::load(dir.path()),
        AppearanceSettings {
            presentation: SourcePresentation::List,
        }
    );
    appearance_settings::save(
        dir.path(),
        AppearanceSettings {
            presentation: SourcePresentation::Large,
        },
    )
    .unwrap();
    let saved = fs::read_to_string(path).unwrap();
    assert!(saved.contains("[recent]\nkeep = \"yes\""));
    assert!(saved.contains("[appearance]\npresentation = \"large\""));
    assert_eq!(
        appearance_settings::load(dir.path()),
        AppearanceSettings {
            presentation: SourcePresentation::Large,
        },
        "the saved appearance choice survives a fresh load"
    );
}

#[test]
fn absent_tables_read_as_defaults() {
    // Neither table, no file at all.
    let dir = tempfile::tempdir().unwrap();
    assert!(recent::load(dir.path()).is_empty());
    let loaded = input_config::load(dir.path());
    assert!(loaded.problems.is_empty());
    assert_eq!(loaded.table, InputTable::default());
    assert_eq!(loaded.provenance, Provenance::Defaults);

    // `[input]` present, `[recent]` absent.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("settings.toml"), "[input]\n").unwrap();
    assert!(recent::load(dir.path()).is_empty());
    let loaded = input_config::load(dir.path());
    assert!(loaded.problems.is_empty());
    assert_eq!(loaded.table, InputTable::default());

    // `[recent]` present, `[input]` absent.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("settings.toml"),
        "[[recent.entries]]\nname = \"a\"\nfolder = \"/a\"\npath = \"/a.ffp\"\nopened_at = 1\n",
    )
    .unwrap();
    let entries = recent::load(dir.path());
    assert_eq!(entries.len(), 1);
    let loaded = input_config::load(dir.path());
    assert!(loaded.problems.is_empty());
    assert_eq!(loaded.table, InputTable::default());
    assert_eq!(loaded.provenance, Provenance::Defaults);
}

#[test]
fn broken_table_resets_only_itself() {
    // A malformed `[recent]` (wrong type) after a valid `[input]`: `[input]`
    // still loads, `[recent]` reads as empty.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("settings.toml"),
        "[input]\n\n[[input.bindings]]\ntrigger = \"wheel\"\nstacking = \"pan\"\n\n\
         [recent]\nentries = 3\n",
    )
    .unwrap();
    assert!(recent::load(dir.path()).is_empty());
    let loaded = input_config::load(dir.path());
    assert!(
        loaded.problems.is_empty(),
        "{:?}",
        loaded
            .problems
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        loaded.table.bindings[0].stacking,
        Some(focusforge_gui::input_map::Action::Pan)
    );

    // A refused `[input]` placed after a valid `[recent]`: `[recent]` still
    // loads intact, and each of `[input]`'s three refusal kinds names
    // `settings.toml` and the line the refusing text is actually on in the
    // whole file -- never the table-relative line it would be if `[input]`
    // were the first table.
    let settings_path_for = |dir: &std::path::Path| dir.join("settings.toml");

    // Unknown trigger: "banana-drag" is on line 11.
    {
        let dir = tempfile::tempdir().unwrap();
        let src = "[recent]\n[[recent.entries]]\nname = \"a\"\nfolder = \"/a\"\npath = \"/a.ffp\"\nopened_at = 1\n\n\
                   [input]\n\n[[input.bindings]]\ntrigger = \"banana-drag\"\nstacking = \"pan\"\n";
        fs::write(dir.path().join("settings.toml"), src).unwrap();
        let entries = recent::load(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a");
        let loaded = input_config::load(dir.path());
        assert_eq!(loaded.table, InputTable::default());
        assert_eq!(loaded.problems.len(), 1);
        match &loaded.problems[0] {
            input_config::ConfigError::UnknownTrigger {
                path,
                spelling,
                line,
            } => {
                assert_eq!(path, &settings_path_for(dir.path()));
                assert_eq!(spelling, "banana-drag");
                assert_eq!(*line, 11, "must report the line within the whole file");
            }
            other => panic!("expected UnknownTrigger, got {other}"),
        }
    }

    // Duplicate trigger: two `[[input.bindings]]` rows naming "wheel"; the
    // first row's span starts on line 10, the duplicate's on line 14.
    {
        let dir = tempfile::tempdir().unwrap();
        let src = "[recent]\n[[recent.entries]]\nname = \"a\"\nfolder = \"/a\"\npath = \"/a.ffp\"\nopened_at = 1\n\n\
                   [input]\n\n[[input.bindings]]\ntrigger = \"wheel\"\nstacking = \"zoom-at-cursor\"\n\n\
                   [[input.bindings]]\ntrigger = \"wheel\"\nstacking = \"pan\"\n";
        fs::write(dir.path().join("settings.toml"), src).unwrap();
        let entries = recent::load(dir.path());
        assert_eq!(entries.len(), 1);
        let loaded = input_config::load(dir.path());
        assert_eq!(loaded.problems.len(), 1);
        match &loaded.problems[0] {
            input_config::ConfigError::DuplicateTrigger {
                path,
                line,
                first_line,
                ..
            } => {
                assert_eq!(path, &settings_path_for(dir.path()));
                assert_eq!(
                    *first_line, 10,
                    "must report the line within the whole file"
                );
                assert_eq!(*line, 14, "must report the line within the whole file");
            }
            other => panic!("expected DuplicateTrigger, got {other}"),
        }
    }
}

#[test]
fn saving_recent_keeps_hand_edited_input() {
    for input_first in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let input_block = "[input]\n# a hand edit\n\n[[input.bindings]]\ntrigger = \"wheel\"\nstacking = \"pan\"\n";
        let export_block = "[export]\nformat = \"tiff\"\n";
        let recent_block = "[recent]\n[[recent.entries]]\nname = \"old\"\nfolder = \"/old\"\npath = \"/old\"\nopened_at = 1\n";
        let src = if input_first {
            format!("{input_block}\n{export_block}\n{recent_block}")
        } else {
            format!("{recent_block}\n{export_block}\n{input_block}")
        };
        fs::write(dir.path().join("settings.toml"), &src).unwrap();

        recent::save(
            dir.path(),
            &[
                RecentEntry {
                    name: "one".into(),
                    folder: "/one".into(),
                    path: "/one.ffp".into(),
                    opened_at: 10,
                },
                RecentEntry {
                    name: "two".into(),
                    folder: "/two".into(),
                    path: "/two.ffp".into(),
                    opened_at: 20,
                },
            ],
        )
        .unwrap();

        let after = fs::read_to_string(dir.path().join("settings.toml")).unwrap();
        assert!(
            after.contains("# a hand edit"),
            "input_first={input_first}: {after}"
        );
        assert!(
            after.contains("trigger = \"wheel\""),
            "input_first={input_first}: {after}"
        );
        assert!(
            after.contains("format = \"tiff\""),
            "input_first={input_first}: {after}"
        );

        let loaded = input_config::load(dir.path());
        assert!(loaded.problems.is_empty());
        assert_eq!(
            loaded.table.bindings[0].stacking,
            Some(focusforge_gui::input_map::Action::Pan)
        );

        let entries = recent::load(dir.path());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "one");
        assert_eq!(entries[1].name, "two");
    }
}

#[test]
fn unparseable_file_is_never_overwritten() {
    use focusforge_gui::appearance_settings::{self, AppearanceSettings, SourcePresentation};

    for original in ["not valid toml {{{", "[inp"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(&path, original).unwrap();

        assert!(recent::load(dir.path()).is_empty());
        let loaded = input_config::load(dir.path());
        assert_eq!(loaded.table, InputTable::default());
        assert_eq!(loaded.problems.len(), 1, "{original:?}");
        match &loaded.problems[0] {
            input_config::ConfigError::Parse { path: p, .. } => assert_eq!(p, &path),
            other => panic!("expected Parse, got {other}"),
        }

        let result = recent::save(dir.path(), &[]);
        assert!(result.is_err(), "{original:?}");
        assert!(
            appearance_settings::save(
                dir.path(),
                AppearanceSettings {
                    presentation: SourcePresentation::Large,
                },
            )
            .is_err()
        );

        let after = fs::read(&path).unwrap();
        assert_eq!(
            after,
            original.as_bytes(),
            "the unparseable file must be untouched"
        );
    }
}

#[test]
fn non_utf8_file_is_never_overwritten() {
    use focusforge_gui::appearance_settings::{self, AppearanceSettings, SourcePresentation};

    // A `settings.toml` that exists but cannot even be read as UTF-8 must
    // never be treated as "absent": that would silently wipe its `[input]`
    // table on the very next `[recent]` save.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let original: &[u8] = b"[input]\n# \xff\xfe garbage\n";
    fs::write(&path, original).unwrap();

    assert!(recent::load(dir.path()).is_empty());
    let loaded = input_config::load(dir.path());
    assert_eq!(loaded.table, InputTable::default());
    assert_eq!(loaded.problems.len(), 1);
    match &loaded.problems[0] {
        input_config::ConfigError::CouldNotRead { path: p, .. } => assert_eq!(p, &path),
        other => panic!("expected CouldNotRead, got {other}"),
    }

    let result = recent::save(dir.path(), &[]);
    assert!(
        result.is_err(),
        "a save must refuse, not silently wipe the file"
    );
    assert!(
        appearance_settings::save(
            dir.path(),
            AppearanceSettings {
                presentation: SourcePresentation::Large,
            },
        )
        .is_err()
    );

    let after = fs::read(&path).unwrap();
    assert_eq!(after, original, "the unreadable file must be untouched");
}

#[cfg(unix)]
#[test]
fn unwritable_config_dir_is_not_fatal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("focusforge");
    fs::create_dir_all(&sub).unwrap();
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();

    assert!(recent::load(&sub).is_empty());
    let loaded = input_config::load(&sub);
    assert_eq!(loaded.table, InputTable::default());

    let result = recent::save(
        &sub,
        &[RecentEntry {
            name: "a".into(),
            folder: "/a".into(),
            path: "/a".into(),
            opened_at: 1,
        }],
    );
    assert!(result.is_err());

    fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();

    // Not fatal: a controller whose config_dir points there still lists the
    // entry just recorded, in memory for the session.
    let mut controller = focusforge_gui::AppController::new();
    controller.install_recent(recent::load(&sub), sub.clone());
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
    controller.record_recent_for_test("b".into(), "/b".into(), "/b".into(), 2);
    assert_eq!(controller.recent_rows(0).len(), 1);
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();

    // A regular file where the directory should be: `create_dir_all` fails
    // outright, before any read or write is attempted.
    let dir = tempfile::tempdir().unwrap();
    let file_in_place_of_dir = dir.path().join("not_a_dir");
    fs::write(&file_in_place_of_dir, b"not a directory").unwrap();

    assert!(recent::load(&file_in_place_of_dir).is_empty());
    let loaded = input_config::load(&file_in_place_of_dir);
    assert_eq!(loaded.table, InputTable::default());

    let result = recent::save(
        &file_in_place_of_dir,
        &[RecentEntry {
            name: "a".into(),
            folder: "/a".into(),
            path: "/a".into(),
            opened_at: 1,
        }],
    );
    assert!(result.is_err());
}

#[test]
fn old_recent_and_input_files_are_not_read() {
    let old_recent = "[[entries]]\nname = \"old\"\nfolder = \"/old\"\nopened_at = 5\n";
    let old_input = "[[bindings]]\ntrigger = \"wheel\"\nstacking = \"pan\"\n";
    let export_only = "[export]\nformat = \"tiff\"\n";

    for settings in [None, Some(export_only)] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("recent.toml"), old_recent).unwrap();
        fs::write(dir.path().join("input.toml"), old_input).unwrap();
        if let Some(text) = settings {
            fs::write(dir.path().join("settings.toml"), text).unwrap();
        }

        assert!(recent::load(dir.path()).is_empty());
        let loaded = input_config::load(dir.path());
        assert!(loaded.problems.is_empty());
        assert_eq!(loaded.table, InputTable::default());
        assert_eq!(loaded.provenance, Provenance::Defaults);

        assert_eq!(
            fs::read_to_string(dir.path().join("recent.toml")).unwrap(),
            old_recent
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("input.toml")).unwrap(),
            old_input
        );
        match settings {
            None => assert!(!dir.path().join("settings.toml").exists()),
            Some(text) => assert_eq!(
                fs::read_to_string(dir.path().join("settings.toml")).unwrap(),
                text
            ),
        }
    }
}

#[test]
fn recent_row_without_path_reads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let src = "[input]\n\n[[input.bindings]]\ntrigger = \"wheel\"\nstacking = \"pan\"\n\n\
               [[recent.entries]]\nname = \"a\"\nfolder = \"/a\"\npath = \"/a\"\nopened_at = 1\n\n\
               [[recent.entries]]\nname = \"b\"\nfolder = \"/b\"\nopened_at = 2\n";
    fs::write(&path, src).unwrap();

    assert!(recent::load(dir.path()).is_empty());
    let loaded = input_config::load(dir.path());
    assert!(loaded.problems.is_empty());
    assert_eq!(
        loaded.table.bindings[0].stacking,
        Some(focusforge_gui::input_map::Action::Pan)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), src);
}
