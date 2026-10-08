// SPDX-License-Identifier: GPL-3.0-or-later

//! The input table, the loader and the one dispatch point, driven through
//! `AppController` with no window and no fixture images. `Ctrl + Wheel`
//! and `Shift + Wheel` are proven here rather than through a window: slint
//! 1.17.1's testing backend can synthesize a wheel or a drag but neither
//! carries keyboard modifiers (slint-ui/slint#13244).

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use focusforge_gui::input_map::InputTable;
use focusforge_gui::input_map::{
    Action, ActionEvent, Binding, Button, DEFAULT_BINDINGS, Modifiers, PointerKind, Trigger,
};
use focusforge_gui::{AppController, ShellEvent, ShellMode, input_config};

const NONE: Modifiers = Modifiers::NONE;

type EventSink = Arc<dyn Fn(ShellEvent) + Send + Sync>;

fn sink() -> (Arc<Mutex<Vec<ShellEvent>>>, EventSink) {
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    (
        events,
        Arc::new(move |event| sink_events.lock().unwrap().push(event)),
    )
}

// ---------------------------------------------------------------------
// The table itself.
// ---------------------------------------------------------------------

#[test]
fn defaults_match_the_settled_table() {
    // Written out literally rather than derived from the code under test:
    // six displayed rows map to the first eight trigger keys, and the ninth
    // (the paint drag) is a hidden row.
    let expected = [
        (
            Trigger::Wheel(NONE),
            Some(Action::ZoomAtCursor),
            Some(Action::ZoomAtCursor),
        ),
        (
            Trigger::Wheel(Modifiers { ctrl: true, ..NONE }),
            Some(Action::StepSourceFrame),
            Some(Action::StepSourceFrame),
        ),
        (
            Trigger::Wheel(Modifiers {
                shift: true,
                ..NONE
            }),
            Some(Action::PanHorizontal),
            Some(Action::BrushSize),
        ),
        (
            Trigger::Drag {
                button: Button::Middle,
                mods: NONE,
            },
            Some(Action::Pan),
            Some(Action::Pan),
        ),
        (
            Trigger::Drag {
                button: Button::Left,
                mods: Modifiers {
                    space: true,
                    ..NONE
                },
            },
            Some(Action::Pan),
            Some(Action::Pan),
        ),
        (
            Trigger::Key {
                key: 'z',
                mods: Modifiers { ctrl: true, ..NONE },
            },
            None,
            Some(Action::Undo),
        ),
        (
            Trigger::Key {
                key: 'z',
                mods: Modifiers {
                    ctrl: true,
                    shift: true,
                    ..NONE
                },
            },
            None,
            Some(Action::Redo),
        ),
        (
            Trigger::Click {
                button: Button::Left,
                mods: Modifiers { alt: true, ..NONE },
            },
            Some(Action::PickSourceUnderPointer),
            Some(Action::PickSourceUnderPointer),
        ),
        (
            Trigger::Drag {
                button: Button::Left,
                mods: NONE,
            },
            None,
            Some(Action::Paint),
        ),
    ];

    assert_eq!(DEFAULT_BINDINGS.len(), expected.len());
    for (i, (binding, (trigger, stacking, editor))) in
        DEFAULT_BINDINGS.iter().zip(expected.iter()).enumerate()
    {
        assert_eq!(binding.trigger, *trigger, "row {i}: trigger");
        assert_eq!(binding.stacking, *stacking, "row {i}: stacking");
        assert_eq!(binding.editor, *editor, "row {i}: editor");
    }
}

#[test]
fn modes_differ_only_by_declared_overrides() {
    // Only the Shift wheel row, the undo/redo rows and the paint drag
    // differ -- the table's explicit overrides. Every other row's stacking
    // and editor columns must match.
    let overridden = [2usize, 5, 6, 8];
    let table = InputTable::default();
    for (i, binding) in table.bindings.iter().enumerate() {
        if overridden.contains(&i) {
            assert_ne!(
                binding.stacking, binding.editor,
                "row {i} was expected to carry an explicit override"
            );
        } else {
            assert_eq!(
                binding.stacking, binding.editor,
                "row {i} was expected to match across modes"
            );
        }
    }
}

// ---------------------------------------------------------------------
// The config file.
// ---------------------------------------------------------------------

fn row_toml(trigger: &str, stacking: Option<Action>, editor: Option<Action>) -> String {
    let mut s = format!("\n[[input.bindings]]\ntrigger = \"{trigger}\"\n");
    if let Some(a) = stacking {
        s.push_str(&format!("stacking = \"{a}\"\n"));
    }
    if let Some(a) = editor {
        s.push_str(&format!("editor = \"{a}\"\n"));
    }
    s
}

#[test]
fn user_file_overrides_a_default() {
    // A row matches a default **by trigger identity**: every row below
    // keeps its default trigger spelling and overrides only the action it
    // names, which is what a file row can unambiguously do against this
    // table's fixed nine-trigger layout. (Renaming *which* physical
    // gesture performs an action -- e.g. moving `Pan` from `middle-drag`
    // onto `right-drag` -- is refused instead: the schema has no field
    // identifying which default row an unrecognised trigger is meant to
    // replace.)
    let dir = tempfile::tempdir().unwrap();
    let defaults = InputTable::default();

    let mut toml = String::from("[input]\n");
    for (i, binding) in defaults.bindings.iter().enumerate() {
        if i == 7 {
            // Left absent on purpose -- must keep its default.
            continue;
        }
        let (stacking, editor) = if i == 3 {
            // Override the `middle-drag` row's action assignment -- still
            // its default trigger, a different action.
            (Some(Action::PickSourceUnderPointer), Some(Action::Pan))
        } else {
            (binding.stacking, binding.editor)
        };
        toml.push_str(&row_toml(&binding.trigger.to_string(), stacking, editor));
    }
    fs::write(dir.path().join("settings.toml"), &toml).unwrap();

    let loaded = input_config::load(dir.path());
    assert!(
        loaded.problems.is_empty(),
        "unexpected problems: {:?}",
        loaded
            .problems
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );

    assert_eq!(
        loaded.table.action(
            &Trigger::Drag {
                button: Button::Middle,
                mods: NONE
            },
            ShellMode::Stacking
        ),
        Some(Action::PickSourceUnderPointer),
        "the overridden row's stacking action must take effect"
    );
    assert_eq!(
        loaded
            .table
            .action(&Trigger::Wheel(NONE), ShellMode::Stacking),
        Some(Action::ZoomAtCursor),
        "an untouched row must keep its default"
    );
    assert_eq!(
        loaded.table.action(
            &Trigger::Click {
                button: Button::Left,
                mods: Modifiers { alt: true, ..NONE }
            },
            ShellMode::Stacking
        ),
        Some(Action::PickSourceUnderPointer),
        "the absent row must keep its default"
    );
    match loaded.provenance {
        input_config::Provenance::File { overridden, .. } => assert_eq!(overridden, 1),
        other => panic!("expected Provenance::File, got {other:?}"),
    }
}

#[test]
fn partial_out_of_order_single_row_config_leaves_other_rows_at_default() {
    // A config file that is not a full in-order transcript of the
    // defaults must not shift or delete unrelated rows. A one-row file
    // naming a trigger that is *not* the table's first row is exactly the
    // case an `enumerate()`-indexed merge would get wrong -- it would
    // clobber `bindings[0]` (the wheel row) regardless of which trigger
    // the lone row actually named.
    let dir = tempfile::tempdir().unwrap();
    let toml = "[input]\n\n[[input.bindings]]\ntrigger = \"shift+wheel\"\nstacking = \"pan\"\neditor = \"brush-size\"\n";
    fs::write(dir.path().join("settings.toml"), toml).unwrap();

    let loaded = input_config::load(dir.path());
    assert!(
        loaded.problems.is_empty(),
        "unexpected problems: {:?}",
        loaded
            .problems
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );

    let defaults = InputTable::default();

    // The touched row: `stacking` changed, `editor` unchanged.
    assert_eq!(
        loaded.table.action(
            &Trigger::Wheel(Modifiers {
                shift: true,
                ..NONE
            }),
            ShellMode::Stacking
        ),
        Some(Action::Pan),
        "the overridden row must take effect"
    );
    assert_eq!(
        loaded.table.action(
            &Trigger::Wheel(Modifiers {
                shift: true,
                ..NONE
            }),
            ShellMode::Editor
        ),
        Some(Action::BrushSize)
    );

    // Every other default trigger, including `wheel` itself, must resolve
    // exactly as the defaults do: the file's one custom row names
    // `shift+wheel`, leaving plain `wheel` still bound to its default action.
    for binding in defaults.bindings.iter() {
        if binding.trigger
            == (Trigger::Wheel(Modifiers {
                shift: true,
                ..NONE
            }))
        {
            continue;
        }
        assert_eq!(
            loaded.table.action(&binding.trigger, ShellMode::Stacking),
            binding.stacking,
            "trigger {:?} (stacking) must be untouched",
            binding.trigger
        );
        assert_eq!(
            loaded.table.action(&binding.trigger, ShellMode::Editor),
            binding.editor,
            "trigger {:?} (editor) must be untouched",
            binding.trigger
        );
    }

    match loaded.provenance {
        input_config::Provenance::File { overridden, .. } => assert_eq!(overridden, 1),
        other => panic!("expected Provenance::File, got {other:?}"),
    }
}

#[test]
fn missing_config_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let loaded = input_config::load(dir.path());
    assert!(loaded.problems.is_empty());
    assert_eq!(loaded.table, InputTable::default());
    assert_eq!(loaded.provenance, input_config::Provenance::Defaults);
}

#[test]
fn first_run_writes_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("settings.toml");
    // An `[recent]` table already in the file must survive the write.
    fs::write(
        &target,
        "[recent]\n[[recent.entries]]\nname = \"a\"\nfolder = \"/a\"\nopened_at = 1\n",
    )
    .unwrap();

    let written = input_config::write_defaults(dir.path()).unwrap();
    assert_eq!(written, target);
    assert!(written.exists());
    let contents = fs::read_to_string(&written).unwrap();
    assert!(contents.contains("[input]"));
    assert!(!contents.contains("version"), "{contents}");
    assert!(contents.contains("trigger = \"wheel\""));
    assert!(
        contents.contains("# FocusForge input map."),
        "the header comment block must be written: {contents}"
    );
    assert!(
        contents.contains("[recent]") && contents.contains("name = \"a\""),
        "an existing [recent] table must survive: {contents}"
    );
}

#[test]
fn written_defaults_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    input_config::write_defaults(dir.path()).unwrap();

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
    assert_eq!(loaded.table, InputTable::default());
    match loaded.provenance {
        input_config::Provenance::File { path, overridden } => {
            assert_eq!(overridden, 0);
            assert_eq!(path, dir.path().join("settings.toml"));
        }
        other => panic!("expected Provenance::File, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn unwritable_config_dir_is_not_fatal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("focusforge");
    fs::create_dir_all(&sub).unwrap();
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();

    let result = input_config::write_defaults(&sub);
    assert!(
        result.is_err(),
        "an unwritable config dir must not silently succeed"
    );

    // Restore so the tempdir's own drop-cleanup can remove it.
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();

    // Not fatal: loading from the same directory still yields the
    // defaults, never a panic.
    let loaded = input_config::load(&sub);
    assert_eq!(loaded.table, InputTable::default());
}

/// Writes `toml` (the bare top-level shape: `[[bindings]]` rows) as
/// `settings.toml`'s `[input]` table, loads it, and asserts the load fell
/// back to the defaults with exactly one problem -- the common shape every
/// case below shares. Returns that problem alongside the path it was written
/// to, so a caller can still check a path- or spelling-specific field.
fn expect_one_problem(toml: &str) -> (PathBuf, input_config::ConfigError) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let body = format!(
        "[input]\n{}",
        toml.replace("[[bindings]]", "[[input.bindings]]")
    );
    fs::write(&path, body).unwrap();
    let loaded = input_config::load(dir.path());
    assert_eq!(loaded.table, InputTable::default());
    assert_eq!(
        loaded.problems.len(),
        1,
        "expected exactly one problem: {:?}",
        loaded
            .problems
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    let mut problems = loaded.problems;
    (path, problems.remove(0))
}

#[test]
fn bad_config_is_refused_by_name() {
    // Malformed TOML syntax.
    {
        let (path, problem) = expect_one_problem("[[bindings]\n");
        assert!(matches!(problem, input_config::ConfigError::Parse { .. }));
        assert!(problem.to_string().contains(&path.display().to_string()));
    }

    // Unknown action name.
    {
        let (path, problem) =
            expect_one_problem("[[bindings]]\ntrigger = \"wheel\"\nstacking = \"levitate\"\n");
        match problem {
            input_config::ConfigError::UnknownAction {
                spelling, path: p, ..
            } => {
                assert_eq!(spelling, "levitate");
                assert_eq!(p, path);
            }
            other => panic!("expected UnknownAction, got {other}"),
        }
    }

    // Unknown trigger spelling.
    {
        let (_, problem) =
            expect_one_problem("[[bindings]]\ntrigger = \"banana-drag\"\nstacking = \"pan\"\n");
        match problem {
            input_config::ConfigError::UnknownTrigger { spelling, .. } => {
                assert_eq!(spelling, "banana-drag");
            }
            other => panic!("expected UnknownTrigger, got {other}"),
        }
    }

    // A trigger that parses but names none of the table's nine default
    // triggers -- the table's rows are display-indexed, so a trigger it has
    // no row for cannot be shown.
    {
        let (_, problem) =
            expect_one_problem("[[bindings]]\ntrigger = \"right-click\"\nstacking = \"pan\"\n");
        match problem {
            input_config::ConfigError::UnknownTrigger { spelling, .. } => {
                assert_eq!(spelling, "right-click");
            }
            other => panic!("expected UnknownTrigger, got {other}"),
        }
    }

    // An `alt+wheel` row is a trigger the table has no row for: it refuses
    // the whole table, the valid `shift+wheel` row before it included.
    {
        let (_, problem) = expect_one_problem(
            "[[bindings]]\ntrigger = \"shift+wheel\"\nstacking = \"pan\"\n\n\
             [[bindings]]\ntrigger = \"alt+wheel\"\nstacking = \"pan\"\n",
        );
        match problem {
            input_config::ConfigError::UnknownTrigger { spelling, line, .. } => {
                assert_eq!(spelling, "alt+wheel");
                assert_eq!(line, 7, "the line of the trigger key in the whole file");
            }
            other => panic!("expected UnknownTrigger, got {other}"),
        }
    }

    // Duplicate trigger within one mode.
    {
        let (_, problem) = expect_one_problem(
            "[[bindings]]\ntrigger = \"wheel\"\nstacking = \"zoom-at-cursor\"\n\n\
             [[bindings]]\ntrigger = \"wheel\"\nstacking = \"pan\"\n",
        );
        assert!(matches!(
            problem,
            input_config::ConfigError::DuplicateTrigger { .. }
        ));
    }
}

// ---------------------------------------------------------------------
// The dispatch point.
// ---------------------------------------------------------------------

#[test]
fn drag_emits_start_update_end() {
    let mut controller = AppController::new();
    let (events, cb) = sink();
    controller.set_sink(cb);

    controller.pointer(PointerKind::Down, Button::Middle, 10.0, 10.0, NONE);
    controller.pointer_moved(15.0, 12.0);
    controller.pointer_moved(20.0, 8.0);
    controller.pointer(PointerKind::Up, Button::Middle, 20.0, 8.0, NONE);

    let got = events.lock().unwrap().clone();
    assert_eq!(got.len(), 4);
    assert!(matches!(
        &got[0],
        ShellEvent::Input(ActionEvent::Start { action: Action::Pan, x, y })
            if (*x - 10.0).abs() < 1e-6 && (*y - 10.0).abs() < 1e-6
    ));
    assert!(matches!(
        &got[1],
        ShellEvent::Input(ActionEvent::Update { action: Action::Pan, dx, dy })
            if (*dx - 5.0).abs() < 1e-6 && (*dy - 2.0).abs() < 1e-6
    ));
    assert!(matches!(
        &got[2],
        ShellEvent::Input(ActionEvent::Update { action: Action::Pan, dx, dy })
            if (*dx - 5.0).abs() < 1e-6 && (*dy + 4.0).abs() < 1e-6
    ));
    assert!(matches!(
        &got[3],
        ShellEvent::Input(ActionEvent::End {
            action: Action::Pan
        })
    ));
}

#[test]
fn drag_survives_the_leading_move_kind_pointer_event() {
    // Drives the exact sequence of Slint-shaped callback invocations the
    // real window's `pointer-event`/`moved` handlers produce: `pointer-event`
    // fires with `PointerEventKind::Move` on the *first* mouse movement of a
    // drag, before `moved` (`i-slint-core` 1.17.1's `TouchArea`
    // implementation). Folding that `Move` into `PointerKind::Up` would end
    // the drag after zero `ActionUpdate`s. `drag_emits_start_update_end`
    // above never catches this: it drives `pointer_moved` directly and
    // never interleaves the `pointer-event(Move)` callback at all.
    let mut controller = AppController::new();
    let (events, cb) = sink();
    controller.set_sink(cb);

    controller.pointer(PointerKind::Down, Button::Middle, 10.0, 10.0, NONE);
    // The real window's leading `pointer-event(Move)` -- must be a no-op.
    controller.pointer(PointerKind::Move, Button::Middle, 12.0, 11.0, NONE);
    controller.pointer_moved(15.0, 12.0);
    controller.pointer_moved(20.0, 8.0);
    controller.pointer(PointerKind::Up, Button::Middle, 20.0, 8.0, NONE);

    let got = events.lock().unwrap().clone();
    assert_eq!(
        got.len(),
        4,
        "the interleaved Move must emit nothing: {got:?}"
    );
    assert!(matches!(
        &got[0],
        ShellEvent::Input(ActionEvent::Start { action: Action::Pan, x, y })
            if (*x - 10.0).abs() < 1e-6 && (*y - 10.0).abs() < 1e-6
    ));
    assert!(matches!(
        &got[1],
        ShellEvent::Input(ActionEvent::Update { action: Action::Pan, dx, dy })
            if (*dx - 5.0).abs() < 1e-6 && (*dy - 2.0).abs() < 1e-6
    ));
    assert!(matches!(
        &got[2],
        ShellEvent::Input(ActionEvent::Update { action: Action::Pan, dx, dy })
            if (*dx - 5.0).abs() < 1e-6 && (*dy + 4.0).abs() < 1e-6
    ));
    assert!(matches!(
        &got[3],
        ShellEvent::Input(ActionEvent::End {
            action: Action::Pan
        })
    ));
}

#[test]
fn cancelled_grab_ends_the_drag() {
    let mut controller = AppController::new();
    let (events, cb) = sink();
    controller.set_sink(cb);

    controller.pointer(PointerKind::Down, Button::Middle, 0.0, 0.0, NONE);
    // A cancelled grab is forwarded as `PointerKind::Up` -- it must not
    // leave the drag live.
    controller.pointer(PointerKind::Up, Button::Middle, 0.0, 0.0, NONE);
    controller.pointer_moved(50.0, 50.0);

    let got = events.lock().unwrap().clone();
    assert_eq!(got.len(), 2, "the moved-after-cancel must emit nothing");
    assert!(matches!(
        &got[1],
        ShellEvent::Input(ActionEvent::End {
            action: Action::Pan
        })
    ));
}

#[test]
fn space_drag_is_a_table_row() {
    let mut controller = AppController::new();
    let (events, cb) = sink();
    controller.set_sink(cb);

    controller.key(true, " ", NONE);
    assert!(controller.space_held());

    let space = Modifiers {
        space: true,
        ..NONE
    };
    controller.pointer(PointerKind::Down, Button::Left, 0.0, 0.0, space);
    controller.pointer(PointerKind::Up, Button::Left, 0.0, 0.0, space);
    let got = events.lock().unwrap().clone();
    assert!(matches!(
        &got[0],
        ShellEvent::Input(ActionEvent::Start {
            action: Action::Pan,
            ..
        })
    ));

    // Releasing space stops it resolving.
    controller.key(false, " ", NONE);
    assert!(!controller.space_held());
    events.lock().unwrap().clear();
    controller.pointer(PointerKind::Down, Button::Left, 0.0, 0.0, NONE);
    assert!(
        events.lock().unwrap().is_empty(),
        "a plain left-drag must not resolve"
    );
}

#[test]
fn focus_loss_clears_space_held() {
    let mut controller = AppController::new();
    controller.key(true, " ", NONE);
    assert!(controller.space_held());
    controller.clear_input_modifiers();
    assert!(!controller.space_held());
}

#[test]
fn horizontal_wheel_resolves_as_shift_wheel() {
    let mut controller = AppController::new();
    let (events, cb) = sink();
    controller.set_sink(cb);

    controller.wheel(12.0, 3.0, 0.0, 0.0, NONE); // delta_x dominant, unmodified
    {
        let got = events.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert!(matches!(
            &got[0],
            ShellEvent::Input(ActionEvent::Fired { action: Action::PanHorizontal, steps })
                if (*steps - 12.0).abs() < 1e-6
        ));
    }
    events.lock().unwrap().clear();

    controller.wheel(
        0.0,
        12.0,
        0.0,
        0.0,
        Modifiers {
            shift: true,
            ..NONE
        },
    );
    {
        let got = events.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert!(matches!(
            &got[0],
            ShellEvent::Input(ActionEvent::Fired { action: Action::PanHorizontal, steps })
                if (*steps - 12.0).abs() < 1e-6
        ));
    }
    events.lock().unwrap().clear();

    // delta_y-dominant is unaffected.
    controller.wheel(1.0, 10.0, 0.0, 0.0, NONE);
    let got = events.lock().unwrap();
    assert!(matches!(
        &got[0],
        ShellEvent::Input(ActionEvent::Fired {
            action: Action::ZoomAtCursor,
            ..
        })
    ));
}

#[test]
fn key_rows_resolve_with_their_modifiers() {
    let mut editor = AppController::new_with_mode_for_test(ShellMode::Editor);
    let (events, cb) = sink();
    editor.set_sink(cb);

    editor.key(true, "z", NONE);
    assert!(
        events.lock().unwrap().is_empty(),
        "plain z must not be a table row"
    );

    editor.key(true, "z", Modifiers { ctrl: true, ..NONE });
    {
        let got = events.lock().unwrap();
        assert!(matches!(
            &got[0],
            ShellEvent::Input(ActionEvent::Fired {
                action: Action::Undo,
                ..
            })
        ));
    }
    events.lock().unwrap().clear();

    editor.key(
        true,
        "z",
        Modifiers {
            ctrl: true,
            shift: true,
            ..NONE
        },
    );
    let got = events.lock().unwrap();
    assert!(matches!(
        &got[0],
        ShellEvent::Input(ActionEvent::Fired {
            action: Action::Redo,
            ..
        })
    ));
    drop(got);

    // Stacking mode: an explicit `—` for both -- neither resolves.
    let mut stacking = AppController::new();
    let (events2, cb2) = sink();
    stacking.set_sink(cb2);
    stacking.key(true, "z", Modifiers { ctrl: true, ..NONE });
    stacking.key(
        true,
        "z",
        Modifiers {
            ctrl: true,
            shift: true,
            ..NONE
        },
    );
    assert!(events2.lock().unwrap().is_empty());
}

#[test]
fn undo_and_redo_are_blocked_under_the_settings_modal() {
    let mut editor = AppController::new_with_mode_for_test(ShellMode::Editor);
    let (events, cb) = sink();
    editor.set_sink(cb);

    editor.open_settings();
    editor.key(true, "z", Modifiers { ctrl: true, ..NONE });
    editor.key(
        true,
        "z",
        Modifiers {
            ctrl: true,
            shift: true,
            ..NONE
        },
    );
    assert!(
        events.lock().unwrap().is_empty(),
        "the default undo/redo chords must raise no event under Settings"
    );

    // A custom binding of undo is blocked too, whatever key it is bound to.
    let mut table = InputTable::default();
    table.bindings.push(Binding {
        trigger: Trigger::Key {
            key: 'y',
            mods: Modifiers { ctrl: true, ..NONE },
        },
        stacking: None,
        editor: Some(Action::Undo),
    });
    editor.install_input_table(table);
    editor.key(true, "y", Modifiers { ctrl: true, ..NONE });
    assert!(events.lock().unwrap().is_empty());

    editor.close_settings();
    editor.key(true, "z", Modifiers { ctrl: true, ..NONE });
    {
        let got = events.lock().unwrap();
        assert!(matches!(
            &got[0],
            ShellEvent::Input(ActionEvent::Fired {
                action: Action::Undo,
                ..
            })
        ));
    }
    events.lock().unwrap().clear();

    editor.key(
        true,
        "z",
        Modifiers {
            ctrl: true,
            shift: true,
            ..NONE
        },
    );
    let got = events.lock().unwrap();
    assert!(matches!(
        &got[0],
        ShellEvent::Input(ActionEvent::Fired {
            action: Action::Redo,
            ..
        })
    ));
}

#[test]
fn alt_is_only_the_pipette() {
    let alt = Modifiers { alt: true, ..NONE };
    let alt_wheel = Trigger::Wheel(alt);
    let alt_click = Trigger::Click {
        button: Button::Left,
        mods: alt,
    };

    let table = InputTable::default();
    for mode in [ShellMode::Stacking, ShellMode::Editor] {
        assert_eq!(table.action(&alt_wheel, mode), None, "{mode:?}");
        assert_eq!(
            table.action(&alt_click, mode),
            Some(Action::PickSourceUnderPointer),
            "{mode:?}"
        );
    }
    assert!(DEFAULT_BINDINGS.iter().all(|b| b.trigger != alt_wheel));

    let rows = table.display_rows();
    assert_eq!(rows.len(), 6);
    assert!(
        rows.iter()
            .all(|row| !row.input.to_lowercase().contains("alt + wheel")
                && !row.input.to_lowercase().contains("alt+wheel")),
        "no row names Alt + Wheel: {rows:?}"
    );
    assert!("overlay-opacity".parse::<Action>().is_err());

    for mode in [ShellMode::Stacking, ShellMode::Editor] {
        let mut controller = match mode {
            ShellMode::Stacking => AppController::new(),
            ShellMode::Editor => AppController::new_with_mode_for_test(mode),
        };
        let (events, cb) = sink();
        controller.set_sink(cb);
        controller.wheel(0.0, 60.0, 0.0, 0.0, alt);
        assert!(
            events.lock().unwrap().is_empty(),
            "{mode:?}: Alt + Wheel raises no event"
        );
        controller.pointer(PointerKind::Down, Button::Left, 0.0, 0.0, alt);
        controller.pointer(PointerKind::Up, Button::Left, 0.0, 0.0, alt);
        assert_eq!(
            fired(&events),
            vec![Action::PickSourceUnderPointer],
            "{mode:?}: Alt + click is the pipette"
        );
    }
}

/// The brush's `(size, soft)` as the editor toolbar reads them.
fn brush(controller: &AppController) -> (f32, f32) {
    let view = controller.brush_toolbar_view();
    (view.size, view.soft)
}

/// The action every `ShellEvent::Input` in `events` carried, draining it.
fn fired(events: &Mutex<Vec<ShellEvent>>) -> Vec<Action> {
    events
        .lock()
        .unwrap()
        .drain(..)
        .filter_map(|event| match event {
            ShellEvent::Input(input) => Some(input.action()),
            _ => None,
        })
        .collect()
}

#[test]
fn edit_mode_overrides_are_table_driven() {
    let ctrl = Modifiers { ctrl: true, ..NONE };
    let shift = Modifiers {
        shift: true,
        ..NONE
    };
    // The editor's `Shift + Wheel` default is the brush size, and no default
    // wheel moves Soft.
    let mut editor = AppController::new_with_mode_for_test(ShellMode::Editor);
    let (events, cb) = sink();
    editor.set_sink(cb);
    let (_, soft) = brush(&editor);
    editor.wheel(0.0, 60.0, 0.0, 0.0, shift);
    editor.wheel(0.0, 60.0, 0.0, 0.0, ctrl);
    assert_eq!(brush(&editor).1, soft, "size and source leave Soft");
    assert_eq!(
        fired(&events),
        vec![Action::BrushSize, Action::StepSourceFrame]
    );

    let mut view = AppController::new();
    let (events, cb) = sink();
    view.set_sink(cb);
    let before = brush(&view);
    view.wheel(0.0, 60.0, 0.0, 0.0, shift);
    view.wheel(0.0, 60.0, 0.0, 0.0, ctrl);
    assert_eq!(brush(&view), before, "no view-mode wheel touches the brush");
    assert_eq!(
        fired(&events),
        vec![Action::PanHorizontal, Action::StepSourceFrame]
    );

    // A user file that rebinds `shift+wheel` in the editor column changes
    // what the wheel does there, and nothing else: `brush-softness` stays
    // reachable by rebinding, one detent moving Soft by 5.
    let dir = tempfile::tempdir().unwrap();
    let toml = format!(
        "[input]\n{}",
        row_toml(
            &Trigger::Wheel(shift).to_string(),
            Some(Action::PanHorizontal),
            Some(Action::BrushSoftness),
        )
    );
    fs::write(dir.path().join("settings.toml"), toml).unwrap();
    let loaded = input_config::load(dir.path());
    assert!(loaded.problems.is_empty());
    let mut rebound = AppController::new_with_mode_for_test(ShellMode::Editor);
    let (events, cb) = sink();
    rebound.set_sink(cb);
    rebound.install_input_table(loaded.table);
    let (size, soft) = brush(&rebound);
    rebound.wheel(0.0, 60.0, 0.0, 0.0, shift);
    assert_eq!(
        brush(&rebound),
        (size, soft + 5.0),
        "one detent moves Soft by 5"
    );
    rebound.wheel(0.0, 30.0, 0.0, 0.0, shift);
    assert_eq!(brush(&rebound).1, soft + 5.0, "half a detent does nothing");
    rebound.wheel(0.0, 30.0, 0.0, 0.0, shift);
    assert_eq!(
        brush(&rebound).1,
        soft + 10.0,
        "the second half completes it"
    );
    assert_eq!(fired(&events), vec![Action::BrushSoftness; 3]);
}

#[test]
fn paint_drag_is_a_hidden_table_row() {
    let left_drag = Trigger::Drag {
        button: Button::Left,
        mods: NONE,
    };
    let table = InputTable::default();
    assert_eq!(
        table.action(&left_drag, ShellMode::Editor),
        Some(Action::Paint)
    );
    assert_eq!(table.action(&left_drag, ShellMode::Stacking), None);

    let rows = table.display_rows();
    assert_eq!(rows.len(), 6, "the settings table has six rows");
    assert!(
        rows.iter()
            .all(|row| !format!("{} {} {}", row.input, row.stacking, row.editor).contains("paint")),
        "the paint drag is not shown: {rows:?}"
    );

    let mut view = AppController::new();
    let (events, cb) = sink();
    view.set_sink(cb);
    view.pointer(PointerKind::Down, Button::Left, 5.0, 5.0, NONE);
    view.pointer_moved(9.0, 7.0);
    view.pointer(PointerKind::Up, Button::Left, 9.0, 7.0, NONE);
    assert!(
        events.lock().unwrap().is_empty(),
        "a view-mode left drag does nothing"
    );

    let mut editor = AppController::new_with_mode_for_test(ShellMode::Editor);
    let (events, cb) = sink();
    editor.set_sink(cb);
    editor.pointer(PointerKind::Down, Button::Left, 5.0, 5.0, NONE);
    editor.pointer_moved(9.0, 7.0);
    editor.pointer(PointerKind::Up, Button::Left, 9.0, 7.0, NONE);
    let got = events.lock().unwrap().clone();
    assert!(
        matches!(
            got.as_slice(),
            [
                ShellEvent::Input(ActionEvent::Start {
                    action: Action::Paint,
                    ..
                }),
                ShellEvent::Input(ActionEvent::Update {
                    action: Action::Paint,
                    ..
                }),
                ShellEvent::Input(ActionEvent::End {
                    action: Action::Paint
                }),
            ]
        ),
        "{got:?}"
    );
}

// ---------------------------------------------------------------------
// One table, one dispatch point.
// ---------------------------------------------------------------------

/// Files whose own widget owns a raw gesture callback rather than routing
/// through the canvas's rebindable zoom/pan/drag gesture table: the canvas
/// and detached window, every scrollable panel and drag-based control (the slider and
/// the scrollbar), each forwarding its callback body in one line to
/// `AppController` and interpreting nothing in `.slint`, plus every modal
/// scrim and the Projects overlay's background, whose `scroll-event`
/// swallows the wheel with a bare `accept` and forwards nothing.
const RAW_GESTURE_CALLBACK_EXCEPTIONS: &[&str] = &[
    "canvas.slint",
    "comparison_window.slint",
    "ff_slider.slint",
    "ff_scrollbar.slint",
    "settings_panel.slint",
    "images_panel.slint",
    "checkpoint_panel.slint",
    "ff_image_row.slint",
    "gallery.slint",
    "ff_manifest_list.slint",
    "ff_modal_panel.slint",
    "projects_overlay.slint",
];

#[test]
fn no_handler_outside_the_dispatch_point() {
    let mut violations = Vec::new();
    for path in common::find_slint_files(&common::ui_dir()) {
        let is_exempt = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| RAW_GESTURE_CALLBACK_EXCEPTIONS.contains(&name));
        if is_exempt {
            continue;
        }
        let contents = fs::read_to_string(&path).unwrap();
        for needle in ["scroll-event(", "pointer-event(", "moved =>"] {
            if contents.contains(needle) {
                violations.push(format!("{}: {needle}", path.display()));
            }
        }
    }
    assert!(violations.is_empty(), "{violations:?}");

    for path in common::find_rust_files(&common::src_dir()) {
        if path.file_name().and_then(|n| n.to_str()) == Some("controller.rs") {
            continue;
        }
        let contents = fs::read_to_string(&path).unwrap();
        let code = common::strip_line_comments(&contents);
        assert!(
            !code.contains(".action("),
            "{}: maps a gesture to an Action outside the dispatch point",
            path.display()
        );
    }
}

/// The names `code` declares as components, exported or not.
fn declared_components(code: &str) -> Vec<&str> {
    code.lines()
        .map(str::trim_start)
        .map(|line| line.strip_prefix("export ").unwrap_or(line))
        .filter_map(|line| line.strip_prefix("component "))
        .filter_map(|rest| rest.split_whitespace().next())
        .collect()
}

/// Whether `code` writes a number straight before `px` or `%`.
fn has_unit_literal(code: &str) -> bool {
    let bytes = code.as_bytes();
    (1..bytes.len()).any(|i| {
        bytes[i - 1].is_ascii_digit() && (bytes[i] == b'%' || bytes[i..].starts_with(b"px"))
    })
}

/// The Input map tab is layout over shared components: one component, no
/// styling or size of its own, the table row declared once under
/// `components/`. Neither the tab nor the row can edit a binding.
#[test]
fn input_map_tab_uses_only_shared_settings_components() {
    let ui = common::ui_dir();
    let read = |path: &PathBuf| common::strip_line_comments(&fs::read_to_string(path).unwrap());
    let row_path = ui.join("components/ff_input_table_row.slint");
    let tab = read(&ui.join("shell/settings_modal/input_map_tab.slint"));
    let row = read(&row_path);

    assert_eq!(declared_components(&tab), ["FFInputMapTab"]);
    assert_eq!(
        tab.matches("struct ").count(),
        1,
        "the tab declares only the `InputRow` struct"
    );
    for import in tab.lines().filter(|line| line.starts_with("import ")) {
        assert!(
            import.contains("\"../../components/")
                || import.contains("\"../../theme.slint\"")
                || import.contains("\"../../geometry.slint\""),
            "the tab imports only shared components, the theme and the geometry: {import}"
        );
    }
    for shared in ["FFSettingsSection {", "FFInputTableRow {"] {
        assert!(tab.contains(shared), "the tab does not draw {shared}");
    }
    for needle in [
        "font-size",
        "font-weight",
        "color",
        "background",
        "border-",
        "width",
    ] {
        assert!(!tab.contains(needle), "the tab sets {needle}");
    }
    assert!(!has_unit_literal(&tab), "the tab writes a px or % literal");

    let declaring: Vec<PathBuf> = common::find_slint_files(&ui)
        .into_iter()
        .filter(|path| declared_components(&read(path)).contains(&"FFInputTableRow"))
        .collect();
    assert_eq!(declaring, [row_path]);

    for (file, code) in [
        ("input_map_tab.slint", &tab),
        ("ff_input_table_row.slint", &row),
    ] {
        for needle in [
            "TouchArea",
            "FocusScope",
            "TextInput",
            "LineEdit",
            "callback",
            "=>",
        ] {
            assert!(!code.contains(needle), "{file} is not read-only: {needle}");
        }
    }
}

#[test]
fn canvas_ground_receives_forward_focus() {
    // Without `forward-focus`, nothing in the shell ever takes keyboard
    // focus, so `key-pressed`/`key-released` never fire and every key-chord
    // row (`ctrl+z`, `ctrl+shift+z`, `space+left-drag`) is dead code in the
    // real window. `forward-focus` is the compiler-recognised `Window`
    // property for setting initial focus (`i-slint-compiler` 1.17.1's
    // `forward-focus` builtin), not a second handler -- this is a
    // structural check, not a behavioural one: the actual delivery of a
    // real keystroke can only be confirmed by a human in front of the
    // running window.
    let path = common::ui_dir().join("app.slint");
    let contents = fs::read_to_string(&path).unwrap();
    let code = common::strip_line_comments(&contents);
    assert!(
        code.contains("forward-focus: canvas-ground;"),
        "AppWindow must forward initial focus to canvas-ground"
    );
}

#[test]
fn input_modules_are_slint_free() {
    for file in ["input_map.rs", "input_config.rs", "settings.rs"] {
        let path = common::src_dir().join(file);
        let contents = fs::read_to_string(&path).unwrap();
        let code = common::strip_line_comments(&contents);

        assert!(
            !code.contains("use slint"),
            "{}: imports slint",
            path.display()
        );

        let tokens: Vec<&str> = code
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .collect();
        for needle in ["Shared", "Model", "Image", "Brush", "Color", "Weak"] {
            assert!(
                !tokens.contains(&needle),
                "{}: names Slint type {needle:?}",
                path.display()
            );
        }
    }
}
