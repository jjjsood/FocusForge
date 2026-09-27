// SPDX-License-Identifier: GPL-3.0-or-later

//! `AppController` is plain Rust: no Slint backend is initialised anywhere
//! in this file, and none is needed -- the callback-forwarding rule is what
//! these tests pin.

mod common;

use std::fs;
use std::sync::{Arc, Mutex};

use focusforge_core::io::Rect;
use focusforge_core::project::{Op, Point};
use focusforge_core::zmap::ZMapReader;
use focusforge_gui::{AppController, DockTab, ShellEvent, ShellMode};

/// True when `body` (already trimmed) is exactly one statement of the form
/// `root.<ident>(...)`, an optional trailing `accept`/`reject` (`scroll-event`
/// and the key handlers return an `EventResult`), a bare literal assignment
/// (`self.<ident> = <literal>;` or `<child-id>.<ident> = <literal>;`), or one
/// `if <property-path> { <literal-assignment> }` with no `else` -- a small
/// Slint-side gate resetting itself. Nothing else widens: two statements, a
/// computed or forwarded right-hand side, or an `else` branch all still fail.
fn is_single_root_call(body: &str) -> bool {
    let body = body.trim();
    let body = body
        .strip_suffix("accept")
        .or_else(|| body.strip_suffix("reject"))
        .map(str::trim_end)
        .unwrap_or(body);
    if body.starts_with("root.") && body.ends_with(");") && body.matches(';').count() == 1 {
        return true;
    }
    is_bare_literal_assignment(body) || is_conditional_literal_reset(body)
}

/// `self.<ident> = <literal>;` or `<child-id>.<ident> = <literal>;` --
/// exactly one dot, both sides plain identifiers, and `<literal>` is `true`,
/// `false`, a quoted string, or a number with an optional unit suffix (`1`,
/// `0.5`, `1px`) -- never a bare identifier (which would be another
/// property, e.g. `self.x = y;`), a nested path (`self.x.y = 1;`), or
/// anything with a call or an operator in it. See [`is_single_root_call`].
fn is_bare_literal_assignment(body: &str) -> bool {
    let Some((lhs, rhs_with_semi)) = body.split_once('=') else {
        return false;
    };
    let valid_ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    let [qualifier, ident] = lhs.trim().split('.').collect::<Vec<_>>()[..] else {
        return false;
    };
    if !valid_ident(qualifier) || !valid_ident(ident) {
        return false;
    }
    let Some(rhs) = rhs_with_semi.trim().strip_suffix(';') else {
        return false;
    };
    body.matches(';').count() == 1 && is_literal_token(rhs.trim())
}

/// `if <property-path> { <bare-literal-assignment> }`, with no `else` and
/// no other statement -- `<property-path>` is `self.<ident>` or
/// `<child-id>.<ident>`, optionally negated with a leading `!`. The one
/// branch this scan allows: a Slint-side gate resetting itself the moment
/// the property it watches goes false. See [`is_single_root_call`].
fn is_conditional_literal_reset(body: &str) -> bool {
    let Some(rest) = body.strip_prefix("if ") else {
        return false;
    };
    let Some((cond, rest)) = rest.split_once('{') else {
        return false;
    };
    let cond = cond.trim().strip_prefix('!').unwrap_or(cond.trim()).trim();
    let valid_ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    let cond_is_property_path = matches!(cond.split('.').collect::<Vec<_>>()[..], [q, i] if valid_ident(q) && valid_ident(i));
    if !cond_is_property_path {
        return false;
    }
    let Some(inner) = rest.trim().strip_suffix('}') else {
        return false;
    };
    is_bare_literal_assignment(inner.trim())
}

/// `true`, `false`, a `"..."` string, or a number (`1`, `0.5`, `-3`) with an
/// optional trailing unit made only of ASCII letters or `%` (`1px`, `50%`).
fn is_literal_token(token: &str) -> bool {
    if token == "true" || token == "false" {
        return true;
    }
    if let Some(inner) = token.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        return !inner.contains('"');
    }
    let token = token.strip_prefix('-').unwrap_or(token);
    let digits_end = token
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(token.len());
    let (number, unit) = token.split_at(digits_end);
    !number.is_empty()
        && number.chars().filter(|&c| c == '.').count() <= 1
        && number.chars().next().is_some_and(|c| c.is_ascii_digit())
        && (unit.is_empty() || unit.chars().all(|c| c.is_ascii_alphabetic() || c == '%'))
}

#[test]
fn is_single_root_call_allows_one_trailing_accept_or_reject() {
    assert!(is_single_root_call("root.canvas_scrolled(a, b); accept"));
    assert!(is_single_root_call("root.canvas_key_pressed(a); reject"));
    assert!(is_single_root_call("root.foo();"));
    assert!(
        !is_single_root_call("root.foo(); root.bar(); accept"),
        "two statements before the trailing token must still fail"
    );
    assert!(
        !is_single_root_call("let x = 1; root.foo(); accept"),
        "a non-forward statement before the trailing token must still fail"
    );
}

#[test]
fn is_single_root_call_allows_a_bare_self_literal_assignment() {
    assert!(is_single_root_call("self.opacity = 1;"));
    assert!(is_single_root_call("self.on = true;"));
    assert!(is_single_root_call(r#"self.text = "x";"#));
    assert!(is_single_root_call("self.height = 1px;"));
    assert!(is_single_root_call("self.opacity = 0.5;"));
    assert!(
        !is_single_root_call("self.opacity = root.opacity;"),
        "the right side must be a literal, not another property"
    );
    assert!(
        !is_single_root_call("self.opacity = enabled;"),
        "the right side must be a literal, not a bare unqualified property either"
    );
    assert!(
        !is_single_root_call("self.opacity = self.compute();"),
        "the right side must not be a call"
    );
    assert!(
        !is_single_root_call("self.x.y = 1;"),
        "the left side must be exactly one property, not a nested path"
    );
    assert!(
        !is_single_root_call("self.count += 1;"),
        "the left side must be a plain identifier, not a compound assignment"
    );
    assert!(
        !is_single_root_call("self.opacity = 1; self.height = 1;"),
        "two statements must still fail"
    );
}

#[test]
fn is_single_root_call_allows_a_named_childs_bare_literal_assignment() {
    assert!(is_single_root_call("gate.shown = true;"));
    assert!(is_single_root_call("gate.shown = false;"));
    assert!(
        !is_single_root_call("gate.shown = root.load_active;"),
        "the right side must still be a literal, not another property"
    );
    assert!(
        !is_single_root_call("gate.shown.nested = true;"),
        "still exactly one dot, never a nested path"
    );
}

#[test]
fn is_single_root_call_allows_one_conditional_literal_reset() {
    assert!(is_single_root_call(
        "if !root.load_active {\n gate.shown = false;\n}"
    ));
    assert!(is_single_root_call("if self.on { self.on = false; }"));
    assert!(
        !is_single_root_call("if !root.load_active { gate.shown = false; } else { }"),
        "an else branch must still fail"
    );
    assert!(
        !is_single_root_call("if !root.load_active { gate.shown = root.load_active; }"),
        "the inner assignment must still be a bare literal"
    );
    assert!(
        !is_single_root_call("if !root.load_active { gate.shown = false; gate.other = true; }"),
        "still exactly one statement inside"
    );
}

#[test]
fn is_literal_token_accepts_only_true_false_strings_and_numbers() {
    assert!(is_literal_token("true"));
    assert!(is_literal_token("false"));
    assert!(is_literal_token("1"));
    assert!(is_literal_token("-3"));
    assert!(is_literal_token("0.5"));
    assert!(is_literal_token("1px"));
    assert!(is_literal_token("50%"));
    assert!(is_literal_token(r#""text""#));
    assert!(
        !is_literal_token("enabled"),
        "a bare identifier is not a literal"
    );
    assert!(
        !is_literal_token("root"),
        "a bare identifier is not a literal"
    );
    assert!(!is_literal_token("1 + 1"), "an expression is not a literal");
    assert!(
        !is_literal_token("px1"),
        "a unit must trail a number, not lead it"
    );
    assert!(!is_literal_token(""), "empty is not a literal");
}

/// Every `... => { ... }` handler body in `contents`, as
/// `(1-indexed line, trimmed body)`. Handles nested braces so a body that
/// itself contains a struct literal or a block does not truncate early.
fn handler_bodies(contents: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let bytes = contents.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = contents[search_from..].find("=> {") {
        let arrow_pos = search_from + rel;
        let body_start = arrow_pos + "=> {".len();
        let line = contents[..arrow_pos].matches('\n').count() + 1;

        let mut depth = 1i32;
        let mut i = body_start;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        let body_end = i - 1;
        out.push((line, contents[body_start..body_end].trim().to_string()));
        search_from = i;
    }
    out
}

#[test]
fn callbacks_are_one_line_forwards() {
    let files = common::find_slint_files(&common::ui_dir());
    assert!(
        !files.is_empty(),
        "no .slint files found under {}",
        common::ui_dir().display()
    );

    let mut violations = Vec::new();
    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        for (line, body) in handler_bodies(&contents) {
            if !is_single_root_call(&body) {
                violations.push(format!("{}:{line}: {body:?}", path.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "handler body is not a single `root.<callback>(...)` forward:\n{}",
        violations.join("\n")
    );
}

#[test]
fn controller_api_is_slint_free() {
    let path = common::src_dir().join("controller.rs");
    let contents = fs::read_to_string(&path).unwrap();
    // Code only -- doc comments are free to *talk about* the rule (as this
    // file's own header does) without tripping the scan that enforces it.
    let code = common::strip_line_comments(&contents);

    assert!(
        !code.contains("use slint"),
        "{}: imports slint",
        path.display()
    );

    // Whole-identifier match: `DockTab::Images` must not trip the scan for
    // `Image` (`slint::Image`).
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

// ---------------------------------------------------------------------
// Editor mode: entry, refusal, the per-tab edit session, the brush source
// and the fall back to stacking.
// ---------------------------------------------------------------------

const SETTLE: Duration = Duration::from_secs(60);

/// A controller whose ledger, staging and held checkpoints live under a
/// fresh directory named for `name`.
fn controller_with_scratch(name: &str) -> AppController {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

/// Opens the first `count` `fixtures/mantis-64` frames, copied under a fresh
/// directory, as a new active tab with no checkpoint. Returns that directory.
fn open_mantis64_tab(controller: &mut AppController, name: &str, count: usize) -> PathBuf {
    let root = common::tree(&format!("{name}_frames"));
    for i in 0..count {
        fs::copy(
            common::fixture_frame("mantis-64", i),
            root.join(format!("{i:02}.tif")),
        )
        .expect("fixture frame must copy");
    }
    assert!(matches!(
        common::open_and_settle(controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    // Editor entry is under test, not alignment quality; an algorithm that
    // always converges keeps every run clean.
    controller.set_align_algorithm("dummy-align");
    root
}

/// Settles every run, the live one and each one promoted from the queue.
fn settle_all(controller: &mut AppController) {
    while common::wait_for_settle(controller, SETTLE).is_some() {}
}

/// [`open_mantis64_tab`], then fuses it into one held checkpoint.
fn open_fused_tab(controller: &mut AppController, name: &str, count: usize) -> PathBuf {
    let root = open_mantis64_tab(controller, name, count);
    assert!(controller.start_fuse());
    settle_all(controller);
    assert_eq!(controller.checkpoint_count(), Some(1), "{name} must fuse");
    root
}

/// Saves the active, never-saved tab to the path the save dialog proposes.
fn save_to_proposed(controller: &mut AppController) -> PathBuf {
    let mut target = None;
    assert!(controller.save_active_project(|proposed| {
        target = Some(proposed.to_path_buf());
        target.clone()
    }));
    target.expect("a never-saved tab asks where to save")
}

/// Every event the controller emits from here on.
fn record_events(controller: &mut AppController) -> Arc<Mutex<Vec<ShellEvent>>> {
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));
    events
}

/// Drains `events`, keeping only the modes each `ModeChanged` carried.
fn take_mode_changes(events: &Mutex<Vec<ShellEvent>>) -> Vec<ShellMode> {
    events
        .lock()
        .unwrap()
        .drain(..)
        .filter_map(|event| match event {
            ShellEvent::ModeChanged(mode) => Some(mode),
            _ => None,
        })
        .collect()
}

/// The whole Z-Map at `path`, read straight from disk.
fn read_whole_zmap(path: &Path) -> Vec<u16> {
    let reader = ZMapReader::open(path).expect("the tab's Z-Map must open");
    let info = reader.info();
    let whole = Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    };
    let mut indices = vec![0u16; info.tile_len(whole)];
    reader
        .read_tile_into(whole, &mut indices)
        .expect("the tab's Z-Map must read");
    indices
}

fn one_dab(source_id: u16) -> Op {
    Op::BrushStroke {
        source_id,
        path: vec![Point { x: 20, y: 20 }],
        radius: 4.0,
        softness: 0.0,
    }
}

/// `set_mode(Editor)` refused: `false`, nothing emitted, still stacking.
fn assert_editor_refused(controller: &mut AppController, case: &str) {
    let events = record_events(controller);
    assert!(
        !controller.set_mode(ShellMode::Editor),
        "{case}: set_mode(Editor) must return false"
    );
    assert!(
        events.lock().unwrap().is_empty(),
        "{case}: a refused set_mode(Editor) emits nothing"
    );
    assert_eq!(controller.mode(), ShellMode::Stacking, "{case}");
}

#[test]
fn editor_mode_enters_on_a_fused_tab() {
    let mut controller = controller_with_scratch("editor_enters");
    open_fused_tab(&mut controller, "editor_enters_held", 2);
    let events = record_events(&mut controller);
    assert!(controller.editor_enabled(), "a held checkpoint is editable");
    assert!(controller.set_mode(ShellMode::Editor));
    assert_eq!(controller.mode(), ShellMode::Editor);
    assert_eq!(
        *events.lock().unwrap(),
        vec![ShellEvent::ModeChanged(ShellMode::Editor)]
    );

    assert!(controller.set_mode(ShellMode::Stacking));
    let project_path = save_to_proposed(&mut controller);
    let mut reopened = controller_with_scratch("editor_enters_reopened");
    assert!(matches!(
        common::open_and_settle(&mut reopened, &[project_path]),
        OpenOutcome::Opened(_)
    ));
    let events = record_events(&mut reopened);
    assert!(
        reopened.editor_enabled(),
        "a reopened project with a checkpoint is editable"
    );
    assert!(reopened.set_mode(ShellMode::Editor));
    assert_eq!(reopened.mode(), ShellMode::Editor);
    assert_eq!(
        *events.lock().unwrap(),
        vec![ShellEvent::ModeChanged(ShellMode::Editor)]
    );
}

#[test]
fn editor_mode_is_refused_without_fused_planes() {
    let mut controller = controller_with_scratch("editor_refused");
    assert!(!controller.editor_enabled(), "no active tab");
    assert_editor_refused(&mut controller, "no active tab");

    open_mantis64_tab(&mut controller, "editor_refused_unsaved", 3);
    assert!(
        !controller.editor_enabled(),
        "an unsaved tab, no checkpoint"
    );
    assert_editor_refused(&mut controller, "an unsaved tab, no checkpoint");

    // The predicate does no I/O, so planes that fail to materialize surface
    // only in `set_mode` itself.
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    let project_path = save_to_proposed(&mut controller);
    let mut reopened = controller_with_scratch("editor_refused_reopened");
    assert!(matches!(
        common::open_and_settle(&mut reopened, std::slice::from_ref(&project_path)),
        OpenOutcome::Opened(_)
    ));
    fs::remove_file(&project_path).expect("the saved project must delete");
    assert_editor_refused(&mut reopened, "planes that fail to materialize");
}

#[test]
fn editor_mode_requires_the_fused_frame_set() {
    let mut controller = controller_with_scratch("editor_frame_set");
    let root = open_fused_tab(&mut controller, "editor_frame_set", 3);
    assert!(controller.editor_enabled());

    controller.toggle_frame_excluded(1);
    assert!(
        controller.editor_enabled(),
        "an exclude keeps the frame set"
    );
    controller.toggle_frame_excluded(1);

    // A missing frame refuses the re-fuse, so no newer checkpoint lands.
    fs::remove_file(root.join("02.tif")).unwrap();
    controller.move_frame(0, 1);
    assert!(!controller.confirm_frame_change(), "the re-fuse is refused");
    assert!(!controller.editor_enabled(), "after a confirmed Move");
    assert_editor_refused(&mut controller, "after a confirmed Move");

    controller.remove_frame(2);
    assert!(controller.confirm_frame_change(), "the re-fuse starts");
    assert!(!controller.editor_enabled(), "after a confirmed Remove");
    assert_editor_refused(&mut controller, "after a confirmed Remove");
    settle_all(&mut controller);
    assert_eq!(controller.checkpoint_count(), Some(2));
    assert!(
        controller.editor_enabled(),
        "a newer checkpoint over the current frame set is editable again"
    );

    controller.add_frames(&[root.join("never-written.tif")]);
    assert!(!controller.confirm_frame_change(), "the re-fuse is refused");
    assert!(!controller.editor_enabled(), "after a confirmed Add");
    assert_editor_refused(&mut controller, "after a confirmed Add");
}

#[test]
fn editor_mode_refuses_during_the_tabs_own_run() {
    let mut controller = controller_with_scratch("editor_own_run");
    open_fused_tab(&mut controller, "editor_own_run_a", 2);
    open_fused_tab(&mut controller, "editor_own_run_b", 3);

    controller.select_project(0);
    assert!(controller.set_mode(ShellMode::Editor));
    let events = record_events(&mut controller);
    assert!(controller.start_fuse());
    assert!(controller.job_running(), "tab 0's own run is live");
    assert_eq!(
        take_mode_changes(&events),
        vec![ShellMode::Stacking],
        "a run starting on the edited tab falls back once"
    );
    assert!(!controller.editor_enabled(), "while the tab's run is live");
    assert_editor_refused(&mut controller, "while the tab's run is live");

    controller.select_project(1);
    assert!(
        controller.editor_enabled(),
        "another tab's run leaves this one editable"
    );
    assert!(controller.set_mode(ShellMode::Editor));
    let events = record_events(&mut controller);
    assert!(controller.start_fuse(), "enqueued behind tab 0's run");
    assert!(matches!(
        controller.job_view().state,
        job::RunState::Queued { .. }
    ));
    assert_eq!(
        take_mode_changes(&events),
        vec![ShellMode::Stacking],
        "queueing the edited tab falls back once"
    );
    assert!(!controller.editor_enabled(), "while the tab is queued");
    assert_editor_refused(&mut controller, "while the tab is queued");

    settle_all(&mut controller);
    assert!(controller.editor_enabled(), "every run has settled");
    assert!(controller.set_mode(ShellMode::Editor));
}

#[test]
fn edit_session_is_per_tab_and_survives_mode_switches() {
    let mut controller = controller_with_scratch("edit_session_per_tab");
    open_fused_tab(&mut controller, "edit_session_per_tab_a", 2);
    open_fused_tab(&mut controller, "edit_session_per_tab_b", 3);
    assert!(controller.edit_session().is_none(), "none before entry");

    assert!(controller.set_mode(ShellMode::Editor));
    let (_, zmap_b) = controller.materialize_active_tab_planes().unwrap();
    assert_eq!(
        controller.edit_session().unwrap().indices(),
        read_whole_zmap(&zmap_b).as_slice(),
        "tab 1's session is its own whole Z-Map"
    );

    controller.select_project(0);
    assert_eq!(controller.mode(), ShellMode::Editor);
    let (_, zmap_a) = controller.materialize_active_tab_planes().unwrap();
    assert_eq!(
        controller.edit_session().unwrap().indices(),
        read_whole_zmap(&zmap_a).as_slice(),
        "tab 0's session is its own whole Z-Map"
    );

    controller
        .edit_session_mut()
        .unwrap()
        .append(one_dab(1))
        .expect("a dab inside the map applies");
    assert!(controller.set_mode(ShellMode::Stacking));
    assert!(controller.set_mode(ShellMode::Editor));
    assert_eq!(
        controller.edit_session().unwrap().op_count(),
        1,
        "the op survives Stacking and back"
    );

    controller.select_project(1);
    assert_eq!(
        controller.edit_session().unwrap().op_count(),
        0,
        "tab 1's session is untouched by tab 0's op"
    );

    controller.select_project(0);
    save_to_proposed(&mut controller);
    assert_eq!(
        controller.edit_session().map(|session| session.op_count()),
        Some(1),
        "a save keeps the session"
    );
    assert!(controller.editor_enabled(), "a saved tab stays editable");
}

#[test]
fn edit_session_follows_the_newest_planes() {
    let mut controller = controller_with_scratch("edit_session_newest");
    open_fused_tab(&mut controller, "edit_session_newest", 3);
    assert!(controller.set_mode(ShellMode::Editor));
    controller
        .edit_session_mut()
        .unwrap()
        .append(one_dab(2))
        .expect("a dab inside the map applies");

    assert!(controller.set_mode(ShellMode::Stacking));
    controller.toggle_frame_excluded(2);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert_eq!(controller.checkpoint_count(), Some(2));

    assert!(controller.set_mode(ShellMode::Editor));
    let newest = controller.held_checkpoints().last().unwrap().zmap.clone();
    let session = controller.edit_session().unwrap();
    assert_eq!(session.op_count(), 0, "a fresh session, not the old one");
    assert_eq!(session.indices(), read_whole_zmap(&newest).as_slice());
}

#[test]
fn brush_source_is_the_selected_frame() {
    let mut controller = controller_with_scratch("brush_source");
    assert_eq!(controller.brush_source(), None, "no active tab");

    open_mantis64_tab(&mut controller, "brush_source", 3);
    assert_eq!(controller.brush_source(), Some(0));
    controller.select_frame(2);
    assert_eq!(controller.brush_source(), Some(2));

    controller.select_plus();
    assert_eq!(controller.brush_source(), None, "`+` is selected");
}

#[test]
fn editor_mode_falls_back_when_the_tab_cannot_edit() {
    let mut controller = controller_with_scratch("editor_fallback");
    open_fused_tab(&mut controller, "editor_fallback_a", 2);
    open_fused_tab(&mut controller, "editor_fallback_b", 3);
    open_mantis64_tab(&mut controller, "editor_fallback_unsaved", 3);
    let events = record_events(&mut controller);

    controller.select_project(0);
    assert!(controller.set_mode(ShellMode::Editor));
    take_mode_changes(&events);
    controller.select_project(1);
    assert_eq!(controller.mode(), ShellMode::Editor);
    assert!(
        take_mode_changes(&events).is_empty(),
        "two editable tabs switch without a ModeChanged"
    );

    controller.select_project(2);
    assert_eq!(controller.mode(), ShellMode::Stacking);
    assert_eq!(take_mode_changes(&events), vec![ShellMode::Stacking]);

    controller.select_project(1);
    assert!(controller.set_mode(ShellMode::Editor));
    take_mode_changes(&events);
    controller.select_plus();
    assert_eq!(controller.mode(), ShellMode::Stacking);
    assert_eq!(take_mode_changes(&events), vec![ShellMode::Stacking]);

    controller.select_project(1);
    assert!(controller.set_mode(ShellMode::Editor));
    take_mode_changes(&events);
    open_mantis64_tab(&mut controller, "editor_fallback_opened", 2);
    assert_eq!(controller.mode(), ShellMode::Stacking);
    assert_eq!(take_mode_changes(&events), vec![ShellMode::Stacking]);

    // Closing tab 1 makes the unsaved tab 2 its successor.
    controller.select_project(1);
    assert!(controller.set_mode(ShellMode::Editor));
    take_mode_changes(&events);
    controller.close_project(1);
    assert_eq!(controller.mode(), ShellMode::Stacking);
    assert_eq!(take_mode_changes(&events), vec![ShellMode::Stacking]);
}

#[test]
fn controller_events_reach_an_installed_sink() {
    let mut controller = AppController::new();
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    controller.tab_strip_pressed();
    controller.window_minimize();
    controller.window_maximize_toggle();
    controller.select_dock_tab(DockTab::Settings);
    controller.checkpoint_export_requested();
    controller.window_close();

    let got = events.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            ShellEvent::WindowDragRequested,
            ShellEvent::WindowMinimizeRequested,
            ShellEvent::WindowMaximizeToggled(true),
            ShellEvent::DockTabSelected(DockTab::Settings),
            ShellEvent::CheckpointExportRequested,
            ShellEvent::WindowCloseRequested,
        ],
        "events must reach the sink in the order they were raised"
    );
}

#[test]
fn maximize_toggle_flips_controller_state_and_carries_it_on_the_event() {
    let mut controller = AppController::new();
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    assert!(!controller.maximized(), "must start not maximized");

    controller.window_maximize_toggle();
    assert!(controller.maximized(), "first toggle must maximize");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [ShellEvent::WindowMaximizeToggled(true)]
    );

    controller.window_maximize_toggle();
    assert!(!controller.maximized(), "second toggle must restore");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            ShellEvent::WindowMaximizeToggled(true),
            ShellEvent::WindowMaximizeToggled(false),
        ]
    );
}

#[test]
fn controller_state_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AppController>();
}

// --- Background loads: the event sequence, the tick contract, the dropped
// second request, and the thumbnail decode's own thread. ------------------

use focusforge_gui::{OpenAs, OpenOutcome, job};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn load_tree(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp tree must be creatable");
    dir
}

fn mantis_frame(index: usize) -> PathBuf {
    common::workspace_root()
        .join("fixtures")
        .join("mantis-512")
        .join("frames")
        .join(format!("{index:02}.tif"))
}

fn copy_frame(dest_dir: &Path, name: &str, source_index: usize) {
    std::fs::create_dir_all(dest_dir).unwrap();
    std::fs::copy(mantis_frame(source_index), dest_dir.join(name))
        .expect("fixture frame must copy");
}

fn install_load_sink(controller: &mut AppController) -> Arc<Mutex<Vec<ShellEvent>>> {
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));
    events
}

/// A load returns before the worker has done anything, and only
/// [`AppController::settle_open`] applies its result -- true for every entry
/// shape: an unambiguous folder, a saved `.ffp`, and a multi-file drop.
#[test]
fn open_paths_returns_before_the_load_completes_and_lands_only_after_settle() {
    let folder = load_tree("controller_shape_folder");
    copy_frame(&folder, "00.tif", 0);
    copy_frame(&folder, "01.tif", 1);

    let project_dir = load_tree("controller_shape_project");
    let project_path = {
        use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
        use focusforge_core::params::ParamValues;
        use focusforge_core::project::{Metrics, SealRequest, seal};
        use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

        let composite = project_dir.join("composite.tif");
        let zmap = project_dir.join("zmap.tif");
        let full = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let mut writer = TiffWriter::create(
            &composite,
            TiffSpec {
                width: 4,
                height: 4,
                samples_per_pixel: 3,
                description: Some("staged".to_string()),
            },
        )
        .unwrap();
        writer.write_tile(full, &[0u16; 4 * 4 * 3]).unwrap();
        writer.finish().unwrap();
        let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(4, 4, 1).unwrap()).unwrap();
        map.write_tile(full, &[0u16; 4 * 4]).unwrap();
        map.finish().unwrap();
        let project_path = project_dir.join("test.ffp");
        seal(SealRequest {
            project_path: &project_path,
            staged_composite: &composite,
            staged_zmap: &zmap,
            sources: &[],
            algorithm: "laplacian",
            params: &ParamValues::new(),
            align: None,
            composite_description: "staged".to_string(),
            label: None,
            metrics: Metrics::default(),
        })
        .unwrap();
        project_path
    };

    let frames_dir = load_tree("controller_shape_frames");
    copy_frame(&frames_dir, "00.tif", 2);
    copy_frame(&frames_dir, "01.tif", 3);
    let frame_paths = vec![frames_dir.join("00.tif"), frames_dir.join("01.tif")];

    for paths in [vec![folder], vec![project_path], frame_paths] {
        let mut controller = AppController::new();
        let handle = controller
            .open_paths(&paths)
            .expect("classifiable paths must start a load");
        assert!(
            controller.project_tabs().is_empty(),
            "no tab before the worker has even joined"
        );
        assert!(controller.pending_question().is_none());
        assert!(controller.last_refusal().is_none());

        handle.join().expect("the worker must not panic");
        assert!(
            controller.project_tabs().is_empty(),
            "no tab lands until settle_open is called, even after the worker finished"
        );

        let outcome = controller.settle_open();
        assert!(matches!(outcome, Some(OpenOutcome::Opened(_))));
        assert_eq!(controller.project_tabs().len(), 1);
    }
}

/// Every progress tick carries `frames_read` of the tree's real `frame_total`
/// (never the fixture's own), reads climb monotonically to completion, and
/// none follows [`ShellEvent::LoadReady`]. The [`job::POLL`] spacing floor is
/// checked between any ticks beyond the two every load guarantees (the first,
/// unconditional one and the completion one, which always reports regardless
/// of timing).
#[test]
fn load_progress_ticks_report_frames_of_the_real_total_and_never_after_ready() {
    let two_subfolders = load_tree("controller_tick_two_subfolders");
    copy_frame(&two_subfolders.join("subject-a"), "00.tif", 0);
    copy_frame(&two_subfolders.join("subject-a"), "01.tif", 1);
    copy_frame(&two_subfolders.join("subject-a"), "02.tif", 2);
    copy_frame(&two_subfolders.join("subject-b"), "00.tif", 3);

    let mantis_512 = common::workspace_root().join("fixtures").join("mantis-512");

    for (label, root, expected_total) in [
        ("two_subfolder_tree", two_subfolders, 4usize),
        ("mantis_512_fixture", mantis_512, 8usize),
    ] {
        let mut controller = AppController::new();
        let timed_events: Arc<Mutex<Vec<(Instant, ShellEvent)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = timed_events.clone();
        controller.set_sink(Arc::new(move |event| {
            sink_events.lock().unwrap().push((Instant::now(), event))
        }));

        let handle = controller.open_paths(&[root]).expect("a load must start");
        handle.join().unwrap();
        controller.settle_open();

        let got = timed_events.lock().unwrap().clone();
        assert_eq!(
            got.first().map(|(_, e)| e),
            Some(&ShellEvent::LoadStarted),
            "{label}: must start with LoadStarted"
        );
        let ready_pos = got
            .iter()
            .position(|(_, e)| matches!(e, ShellEvent::LoadReady))
            .unwrap_or_else(|| panic!("{label}: LoadReady must be raised"));
        assert!(
            !got[ready_pos + 1..]
                .iter()
                .any(|(_, e)| matches!(e, ShellEvent::LoadProgress { .. })),
            "{label}: no tick after LoadReady"
        );

        let ticks: Vec<(Instant, u32, u32)> = got[..ready_pos]
            .iter()
            .filter_map(|(t, e)| match e {
                ShellEvent::LoadProgress {
                    frames_read,
                    frame_total,
                } => Some((*t, *frames_read, *frame_total)),
                _ => None,
            })
            .collect();
        assert!(
            !ticks.is_empty(),
            "{label}: at least one tick between start and the settle marker"
        );
        for &(_, _, total) in &ticks {
            assert_eq!(
                total as usize, expected_total,
                "{label}: frame_total must equal the tree's own frame count"
            );
        }
        for w in ticks.windows(2) {
            assert!(
                w[1].1 >= w[0].1,
                "{label}: frames_read must never go backwards"
            );
        }
        assert_eq!(
            ticks.last().unwrap().1 as usize,
            expected_total,
            "{label}: the last tick before settle must report full completion"
        );

        // The spacing floor: skip the final pair, since the completion tick
        // always reports immediately once the last frame is read, whatever
        // the elapsed time since the previous one.
        if ticks.len() > 2 {
            for w in ticks[..ticks.len() - 1].windows(2) {
                assert!(
                    w[1].0.duration_since(w[0].0) >= job::POLL,
                    "{label}: two non-terminal ticks arrived less than the poll interval apart"
                );
            }
        }
    }
}

/// All three terminations arrive as exactly one of `ProjectsChanged` /
/// `QuestionRaised` / `EntryRefused`, each preceded by exactly one
/// `LoadStarted` and one `LoadReady`, and each leaves the load no longer
/// in flight.
#[test]
fn every_termination_carries_one_start_one_ready_and_its_own_event() {
    let opened_root = load_tree("controller_term_opened");
    copy_frame(&opened_root, "00.tif", 0);
    copy_frame(&opened_root, "01.tif", 1);

    let question_root = load_tree("controller_term_question");
    copy_frame(&question_root.join("subject-a"), "00.tif", 0);
    copy_frame(&question_root.join("subject-b"), "00.tif", 1);

    let refused_root = load_tree("controller_term_refused");

    for (label, root) in [
        ("opened", opened_root),
        ("question", question_root),
        ("refused", refused_root),
    ] {
        let mut controller = AppController::new();
        let events = install_load_sink(&mut controller);

        let handle = controller.open_paths(&[root]).expect("a load must start");
        handle.join().unwrap();
        assert!(
            controller.load_in_flight(),
            "{label}: still in flight until settle_open is called"
        );
        controller.settle_open();
        assert!(
            !controller.load_in_flight(),
            "{label}: settle_open must clear the in-flight flag"
        );

        let got = events.lock().unwrap().clone();
        assert_eq!(
            got.iter()
                .filter(|e| **e == ShellEvent::LoadStarted)
                .count(),
            1,
            "{label}: exactly one LoadStarted, got {got:?}"
        );
        assert_eq!(
            got.iter()
                .filter(|e| matches!(e, ShellEvent::LoadReady))
                .count(),
            1,
            "{label}: exactly one LoadReady, got {got:?}"
        );
        let terminations = got
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ShellEvent::ProjectsChanged
                        | ShellEvent::QuestionRaised(_)
                        | ShellEvent::EntryRefused(_)
                )
            })
            .count();
        assert_eq!(
            terminations, 1,
            "{label}: exactly one termination event, got {got:?}"
        );
    }
}

/// A second open request while one is in flight starts no second worker and
/// is dropped outright: the call reports that nothing started, exactly one
/// `LoadStarted` is seen, and the first load's own outcome is what settles.
#[test]
fn a_second_open_while_one_is_in_flight_is_dropped() {
    let first = load_tree("controller_dropped_first");
    copy_frame(&first, "00.tif", 0);
    let second = load_tree("controller_dropped_second");
    copy_frame(&second, "00.tif", 1);

    let mut controller = AppController::new();
    let events = install_load_sink(&mut controller);

    let handle = controller
        .open_paths(&[first])
        .expect("the first load must start");
    let dropped = controller.open_paths(&[second]);
    assert!(
        dropped.is_none(),
        "a second open while one is in flight must start nothing"
    );

    handle.join().unwrap();
    let outcome = controller.settle_open();
    assert!(
        matches!(outcome, Some(OpenOutcome::Opened(_))),
        "the settled outcome must be the first load's own"
    );

    let starts = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| **e == ShellEvent::LoadStarted)
        .count();
    assert_eq!(starts, 1, "exactly one LoadStarted must ever be seen");
}

/// A freshly opened tab's thumbnail is decoded on the worker, alongside the
/// header walk -- `settle_open`, called here on the caller's own thread,
/// only assembles a tab from bytes already decoded, so its own duration
/// stays far below a decode measurable on a large-enough frame, while the
/// worker's join time reflects the real decode cost.
#[test]
fn thumbnail_decode_runs_on_the_worker_not_on_settle() {
    use focusforge_core::io::{Rect, TiffSpec, TiffWriter};

    let root = load_tree("controller_thumbnail_worker");
    let size = 3000u32;
    let mut samples = vec![0u16; (size * size * 3) as usize];
    for (i, s) in samples.iter_mut().enumerate() {
        *s = (i % 65536) as u16;
    }
    let mut writer = TiffWriter::create(
        &root.join("00.tif"),
        TiffSpec {
            width: size,
            height: size,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: size,
                height: size,
            },
            &samples,
        )
        .unwrap();
    writer.finish().unwrap();

    let mut controller = AppController::new();
    let worker_start = Instant::now();
    let handle = controller.open_paths(&[root]).expect("a load must start");
    handle.join().unwrap();
    let worker_elapsed = worker_start.elapsed();

    let settle_start = Instant::now();
    let outcome = controller.settle_open();
    let settle_elapsed = settle_start.elapsed();

    assert!(matches!(outcome, Some(OpenOutcome::Opened(_))));
    assert!(
        settle_elapsed < Duration::from_millis(50),
        "settle_open must only assemble an already-decoded thumbnail: took {settle_elapsed:?}"
    );
    assert!(
        worker_elapsed >= settle_elapsed,
        "the worker's own duration must reflect the decode, not settle_open's: \
         worker {worker_elapsed:?}, settle {settle_elapsed:?}"
    );
}

/// The same proof, for the path that opens a tab from an answered grouping
/// question rather than directly from `open_paths` -- two subject
/// directories large enough that a decode on `settle_open`'s own thread
/// would be measurable there instead.
#[test]
fn answer_question_thumbnail_decode_runs_on_the_worker_not_on_settle() {
    use focusforge_core::io::{Rect, TiffSpec, TiffWriter};

    let root = load_tree("controller_question_thumbnail_worker");
    let size = 3000u32;
    let mut samples = vec![0u16; (size * size * 3) as usize];
    for (i, s) in samples.iter_mut().enumerate() {
        *s = (i % 65536) as u16;
    }
    for subject in ["subject-a", "subject-b"] {
        let dir = root.join(subject);
        std::fs::create_dir_all(&dir).unwrap();
        let mut writer = TiffWriter::create(
            &dir.join("00.tif"),
            TiffSpec {
                width: size,
                height: size,
                samples_per_pixel: 3,
                description: None,
            },
        )
        .unwrap();
        writer
            .write_tile(
                Rect {
                    x: 0,
                    y: 0,
                    width: size,
                    height: size,
                },
                &samples,
            )
            .unwrap();
        writer.finish().unwrap();
    }

    let mut controller = AppController::new();
    let handle = controller.open_paths(&[root]).expect("a load must start");
    handle.join().unwrap();
    assert!(matches!(
        controller.settle_open(),
        Some(OpenOutcome::Question(..))
    ));

    let worker_start = Instant::now();
    let handle = controller
        .answer_question(OpenAs::NProjects)
        .expect("answering the question must start a load");
    handle.join().unwrap();
    let worker_elapsed = worker_start.elapsed();

    let settle_start = Instant::now();
    let outcome = controller.settle_open();
    let settle_elapsed = settle_start.elapsed();

    assert!(matches!(outcome, Some(OpenOutcome::Opened(_))));
    assert!(
        settle_elapsed < Duration::from_millis(50),
        "settle_open must only assemble an already-decoded thumbnail: took {settle_elapsed:?}"
    );
    assert!(
        worker_elapsed >= settle_elapsed,
        "the worker's own duration must reflect the decode, not settle_open's: \
         worker {worker_elapsed:?}, settle {settle_elapsed:?}"
    );
}

/// A worker's own failed thumbnail decode must not be retried in
/// `settle_open`. The frame is truncated to just its header/IFD while the
/// worker runs, so its decode fails; only after `handle.join()` is it
/// repaired to a complete file, which a retry would decode successfully.
#[test]
fn a_failed_worker_thumbnail_decode_is_not_retried_on_settle() {
    use focusforge_core::io::{Rect, TiffSpec, TiffWriter};

    let root = load_tree("controller_thumbnail_failure");
    let size = 64u32;
    let frame = root.join("00.tif");
    let mut writer = TiffWriter::create(
        &frame,
        TiffSpec {
            width: size,
            height: size,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .unwrap();
    let header_len = std::fs::metadata(&frame).unwrap().len() as usize;

    let samples = vec![0u16; (size * size * 3) as usize];
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: size,
                height: size,
            },
            &samples,
        )
        .unwrap();
    writer.finish().unwrap();

    let complete_bytes = std::fs::read(&frame).unwrap();
    std::fs::write(&frame, &complete_bytes[..header_len]).unwrap();

    let mut controller = AppController::new();
    let handle = controller.open_paths(&[root]).expect("a load must start");
    handle.join().unwrap();

    // Repaired only now, after the worker's own decode already ran and
    // failed against the truncated file -- a synchronous retry inside
    // `settle_open` would find these bytes and succeed.
    std::fs::write(&frame, &complete_bytes).unwrap();

    let settle_start = Instant::now();
    let outcome = controller.settle_open();
    let settle_elapsed = settle_start.elapsed();

    assert!(
        matches!(outcome, Some(OpenOutcome::Opened(_))),
        "a header-valid frame with unreadable pixels still opens"
    );
    assert!(
        settle_elapsed < Duration::from_millis(50),
        "a failed worker decode must not retry synchronously in settle_open: took {settle_elapsed:?}"
    );
    let cards = controller.project_cards();
    assert_eq!(cards.len(), 1);
    assert!(
        cards[0].picture.is_none(),
        "a retry would succeed against the now-repaired file -- \
         the worker's own failure must be final, never retried"
    );
}

// --- Diagnostics: manifest, write, destination, marking -------------------

use focusforge_gui::diagnostics::{DiagnosticsStatus, size_text};

/// A data directory: a live log and a rotated log numbered above it, and a
/// crash directory holding one minidump with no sibling report.
fn seed_diagnostics_dir(root: &Path) -> std::path::PathBuf {
    let data = root.join("focusforge");
    fs::create_dir_all(data.join("logs")).unwrap();
    fs::create_dir_all(data.join("crashes")).unwrap();
    fs::write(
        data.join("logs/focusforge.jsonl"),
        "{\"message\":\"live\"}\n",
    )
    .unwrap();
    fs::write(
        data.join("logs/focusforge.2.jsonl"),
        "{\"message\":\"rotated\"}\n",
    )
    .unwrap();
    fs::write(
        data.join("crashes/20260918T010203Z-focusforge-gui-1.dmp"),
        b"MDMP",
    )
    .unwrap();
    data
}

/// A data directory holding one panic report (no dump) and one native
/// crash (metadata plus dump), for the marking test.
fn seed_two_crashes(root: &Path) -> std::path::PathBuf {
    let data = root.join("focusforge");
    fs::create_dir_all(data.join("crashes")).unwrap();
    fs::write(
        data.join("crashes/20260918T010203Z-focusforge-gui-panic.json"),
        "{\"format\":1,\"payload\":\"boom\"}",
    )
    .unwrap();
    fs::write(
        data.join("crashes/20260918T010204Z-focusforge-gui-native.json"),
        "{\"format\":1,\"minidump\":\"20260918T010204Z-focusforge-gui-native.dmp\"}",
    )
    .unwrap();
    fs::write(
        data.join("crashes/20260918T010204Z-focusforge-gui-native.dmp"),
        b"MDMP",
    )
    .unwrap();
    data
}

/// Runs `controller.start_bundle_write()`, joins it, applies the one
/// `DiagnosticsWriteFinished` event it raises through `sink`, and returns
/// the resulting status -- the same choreography `bridge.rs`'s sink
/// installer performs, minus the `slint::invoke_from_event_loop` hop that
/// needs a running window.
fn run_write(
    controller: &mut AppController,
    events: &Arc<Mutex<Vec<ShellEvent>>>,
) -> DiagnosticsStatus {
    let before = events.lock().unwrap().len();
    controller.start_bundle_write().join().unwrap();
    let got = events.lock().unwrap()[before..].to_vec();
    let [ShellEvent::DiagnosticsWriteFinished(status)] = got.as_slice() else {
        panic!("expected exactly one DiagnosticsWriteFinished event, got {got:?}");
    };
    controller.finish_bundle_write(*status);
    *status
}

fn install_recording_sink(controller: &mut AppController) -> Arc<Mutex<Vec<ShellEvent>>> {
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));
    events
}

/// Runs `controller.start_manifest_scan()`, joins it, and applies every
/// `DiagnosticsScanFinished` event the sink recorded since the call started
/// -- there may be more than one when a scan started elsewhere (e.g. by
/// `set_include_minidumps`) is still in flight, but `finish_manifest_scan`'s
/// own generation guard means only the one this call started ever lands in
/// the controller's stored state. Asserts that one was applied.
fn run_scan(controller: &mut AppController, events: &Arc<Mutex<Vec<ShellEvent>>>) {
    let before = events.lock().unwrap().len();
    controller.start_manifest_scan().join().unwrap();
    let got = events.lock().unwrap()[before..].to_vec();
    assert!(
        got.iter()
            .any(|e| matches!(e, ShellEvent::DiagnosticsScanFinished { .. })),
        "expected at least one DiagnosticsScanFinished event, got {got:?}"
    );
    for event in got {
        if let ShellEvent::DiagnosticsScanFinished { generation, scan } = event {
            controller.finish_manifest_scan(generation, scan);
        }
    }
    assert!(
        !controller.diagnostics_view().loading,
        "the scan this call started must match the controller's current generation"
    );
}

#[test]
fn diagnostics_bundle_manifest() {
    let seeded_root = tempfile::tempdir().unwrap();
    let seeded = seed_diagnostics_dir(seeded_root.path());
    let empty_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_root.path().join("focusforge")).unwrap();
    let empty = empty_root.path().join("focusforge");

    for (data_dir, expect_note) in [(&seeded, false), (&empty, true)] {
        for include_minidumps in [false, true] {
            let mut controller = AppController::new();
            let events = install_recording_sink(&mut controller);
            controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
            // Joined so its own scan report has already landed before
            // `run_scan` takes its "since here" snapshot below -- otherwise
            // a straggling report can land in an unrelated later window.
            controller
                .set_include_minidumps(include_minidumps)
                .join()
                .unwrap();
            run_scan(&mut controller, &events);
            let view = controller.diagnostics_view();

            let reference = focusforge_diagnostics::bundle::manifest(
                &focusforge_diagnostics::bundle::Options {
                    data_dir: Some(data_dir.clone()),
                    config_dir: None,
                    include_minidumps,
                },
            );
            assert_eq!(
                view.entries
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>(),
                reference
                    .entries
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>(),
                "row order/names must equal bundle::manifest's, include_minidumps={include_minidumps}"
            );
            for (row, entry) in view.entries.iter().zip(reference.entries.iter()) {
                assert_eq!(
                    row.not_redacted, !entry.redacted,
                    "{}: not-redacted mark must match the manifest entry",
                    row.name
                );
            }
            assert_eq!(
                view.note.is_some(),
                expect_note,
                "the note row must appear only for the empty data directory"
            );
        }
    }
}

#[test]
fn diagnostics_bundle_manifest_value_column_units() {
    assert_eq!(size_text(0), "0 KB");
    assert_eq!(size_text(1024), "1 KB");
    assert_eq!(size_text(1024 * 1024 - 1), "1024 KB");
    assert_eq!(size_text(1024 * 1024), "1.0 MB");
    assert_eq!(size_text(1024 * 1024 + 1024 * 512), "1.5 MB");
}

#[test]
fn diagnostics_bundle_write() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    for include_minidumps in [false, true] {
        let mut controller = AppController::new();
        let events = install_recording_sink(&mut controller);
        controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
        // Joined for the same reason as `diagnostics_bundle_manifest`: its
        // own scan must land before `run_scan`'s window, or `run_write`'s
        // later one can catch the straggler instead.
        controller
            .set_include_minidumps(include_minidumps)
            .join()
            .unwrap();
        run_scan(&mut controller, &events);
        let view = controller.diagnostics_view();
        let out = root.path().join(format!("bundle-{include_minidumps}.zip"));
        controller.set_destination(out.to_str().unwrap());

        let status = run_write(&mut controller, &events);
        assert_eq!(status, DiagnosticsStatus::Written);
        assert_eq!(controller.diagnostics_status(), DiagnosticsStatus::Written);

        let file = fs::File::open(&out).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        let mut shown: Vec<String> = view.entries.iter().map(|e| e.name.clone()).collect();
        shown.sort();
        assert_eq!(names, shown, "archive entries must equal the shown rows");
        assert_eq!(
            names.iter().any(|n| n.ends_with(".dmp")),
            include_minidumps,
            "a minidump is in the archive iff the choice was made"
        );
    }
}

#[test]
fn diagnostics_bundle_write_repeats_without_an_extra_click() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    // A destination outside `default_destination()`'s own
    // `<data_dir>/diagnostics/` tree, so the fresh path finish derives next
    // can never collide with it regardless of the clock's granularity.
    let first_destination = root.path().join("manual-first.zip");
    controller.set_destination(first_destination.to_str().unwrap());

    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    run_scan(&mut controller, &events);
    let view = controller.diagnostics_view();
    assert_eq!(
        view.primary_label, "Create bundle",
        "the button must read \"Create bundle\" again, not \"Done\""
    );
    assert!(
        !view.busy,
        "the panel must not require an extra click to re-enable"
    );

    let second_destination = view.destination.clone();
    assert_ne!(
        Path::new(&second_destination),
        first_destination,
        "a finished write must re-derive a fresh destination"
    );
    assert!(
        !Path::new(&second_destination).exists(),
        "the fresh destination must not already exist"
    );

    // The same primary action, clicked again with no destination edit,
    // must write a second, different archive rather than closing anything.
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    assert!(
        first_destination.exists() && Path::new(&second_destination).exists(),
        "both archives must exist: {} and {second_destination}",
        first_destination.display()
    );
}

#[test]
fn diagnostics_bundle_destination() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    // A destination that already exists reports the existing-file state
    // and is written only once the overwrite is taken.
    let existing = root.path().join("existing.zip");
    fs::write(&existing, b"placeholder").unwrap();
    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.set_destination(existing.to_str().unwrap());
    assert_eq!(
        controller.diagnostics_status(),
        DiagnosticsStatus::ExistingFile,
        "a destination that already exists must report ExistingFile before any write"
    );
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    let contents = fs::read(&existing).unwrap();
    assert_ne!(
        contents, b"placeholder",
        "the overwrite must have actually replaced the file"
    );

    // A destination under a path component that is a regular file reports
    // the failure state and leaves neither the archive nor a `.partial`.
    let blocked_parent = root.path().join("blocked");
    fs::write(&blocked_parent, b"not a directory").unwrap();
    let unwritable = blocked_parent.join("bundle.zip");
    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.set_destination(unwritable.to_str().unwrap());
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Failed);
    assert!(!unwritable.exists(), "no archive must be left behind");
    assert!(
        !unwritable.with_file_name("bundle.zip.partial").exists(),
        "no .partial must be left behind"
    );
}

#[test]
fn diagnostics_bundle_marks_reported() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_two_crashes(root.path());
    let unrelated_root = tempfile::tempdir().unwrap();
    let unrelated_data_dir = seed_two_crashes(unrelated_root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.set_destination(root.path().join("bundle.zip").to_str().unwrap());
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);

    let crashes = data_dir.join("crashes");
    assert!(
        crashes
            .join("20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "the packaged panic report must be marked reported"
    );
    assert!(
        crashes
            .join("20260918T010204Z-focusforge-gui-native.reported")
            .exists(),
        "the packaged native crash must be marked reported"
    );
    let unrelated_crashes = unrelated_data_dir.join("crashes");
    assert!(
        !unrelated_crashes
            .join("20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "an unrelated data directory's crash must not be marked"
    );

    // With the crash directory made unwritable, the write still reports
    // success -- marking is best-effort.
    let root2 = tempfile::tempdir().unwrap();
    let data_dir2 = seed_two_crashes(root2.path());
    let crashes2 = data_dir2.join("crashes");
    let mut perms = fs::metadata(&crashes2).unwrap().permissions();
    let writable = perms.clone();
    perms.set_readonly(true);
    fs::set_permissions(&crashes2, perms).unwrap();

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir2.clone()), None);
    controller.set_destination(root2.path().join("bundle.zip").to_str().unwrap());
    let status = run_write(&mut controller, &events);

    fs::set_permissions(&crashes2, writable).unwrap();
    assert_eq!(
        status,
        DiagnosticsStatus::Written,
        "an unwritable crash directory must not turn the write into a failure"
    );
}

// --- Diagnostics: the background scan --------------------------------

/// Names of `bundle::manifest`'s entries for `data_dir`, sorted the way the
/// manifest itself already sorts the generated four and the collected rest.
fn reference_entry_names(data_dir: &Path, include_minidumps: bool) -> Vec<String> {
    focusforge_diagnostics::bundle::manifest(&focusforge_diagnostics::bundle::Options {
        data_dir: Some(data_dir.to_path_buf()),
        config_dir: None,
        include_minidumps,
    })
    .entries
    .iter()
    .map(|e| e.name.clone())
    .collect()
}

#[test]
fn diagnostics_scan_off_the_main_thread() {
    let seeded_root = tempfile::tempdir().unwrap();
    let seeded = common::seed_large_diagnostics_dir(seeded_root.path(), 20 * 1024 * 1024);
    let empty_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_root.path().join("focusforge")).unwrap();
    let empty = empty_root.path().join("focusforge");

    for data_dir in [&seeded, &empty] {
        let mut controller = AppController::new();
        let start = std::time::Instant::now();
        controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
        let view = controller.diagnostics_view();
        let elapsed = start.elapsed();
        assert!(
            view.entries.is_empty(),
            "the first view must carry no rows before any scan has completed, got {:?}",
            view.entries
        );
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "set_diagnostics_dirs plus diagnostics_view took {elapsed:?} over {}",
            data_dir.display()
        );
    }

    let reference = reference_entry_names(&seeded, false);
    assert!(
        !reference.is_empty(),
        "bundle::manifest itself must yield entries for the seeded directory"
    );
}

#[test]
fn diagnostics_scan_reports_through_the_sink() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    for include_minidumps in [false, true] {
        let mut controller = AppController::new();
        controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
        // Sets the choice and lets its own scan finish unobserved, so the
        // sink installed below records only the scan this test starts.
        controller
            .set_include_minidumps(include_minidumps)
            .join()
            .unwrap();

        let events = install_recording_sink(&mut controller);
        controller.start_manifest_scan().join().unwrap();
        let got = events.lock().unwrap().clone();
        let [ShellEvent::DiagnosticsScanFinished { generation, scan }] = got.as_slice() else {
            panic!("expected exactly one DiagnosticsScanFinished event, got {got:?}");
        };
        controller.finish_manifest_scan(*generation, scan.clone());
        let view = controller.diagnostics_view();

        let reference =
            focusforge_diagnostics::bundle::manifest(&focusforge_diagnostics::bundle::Options {
                data_dir: Some(data_dir.clone()),
                config_dir: None,
                include_minidumps,
            });
        assert_eq!(
            view.entries
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
            reference
                .entries
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
            "row order/names must equal bundle::manifest's, include_minidumps={include_minidumps}"
        );
        for (row, entry) in view.entries.iter().zip(reference.entries.iter()) {
            assert_eq!(
                row.not_redacted, !entry.redacted,
                "{}: not-redacted mark must match the manifest entry",
                row.name
            );
        }
    }
}

#[test]
fn diagnostics_rescan_on_minidump_choice() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    run_scan(&mut controller, &events);
    assert!(
        !controller.diagnostics_view().entries.is_empty(),
        "the seeded directory must show rows once the initial scan lands"
    );

    for include_minidumps in [true, false] {
        let before = events.lock().unwrap().len();
        let handle = controller.set_include_minidumps(include_minidumps);

        let mid_view = controller.diagnostics_view();
        assert!(
            mid_view.loading,
            "the view between the toggle and its report must be loading"
        );
        assert!(
            mid_view.entries.is_empty(),
            "the view between the toggle and its report must carry no rows, not the previous list"
        );

        handle.join().unwrap();
        let got = events.lock().unwrap()[before..].to_vec();
        let [ShellEvent::DiagnosticsScanFinished { generation, scan }] = got.as_slice() else {
            panic!("expected exactly one DiagnosticsScanFinished event, got {got:?}");
        };
        controller.finish_manifest_scan(*generation, scan.clone());

        let view = controller.diagnostics_view();
        let reference = reference_entry_names(&data_dir, include_minidumps);
        assert_eq!(
            view.entries
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
            reference,
            "the rescanned rows must equal bundle::manifest for include_minidumps={include_minidumps}"
        );
    }
}

#[test]
fn diagnostics_scan_unreadable_dir() {
    let root = tempfile::tempdir().unwrap();
    let blocked_parent = root.path().join("blocked");
    fs::write(&blocked_parent, b"not a directory").unwrap();
    let unreadable = blocked_parent.join("focusforge");

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(unreadable), None);
    run_scan(&mut controller, &events);
    let view = controller.diagnostics_view();

    assert!(
        !view.loading,
        "an unreadable data directory must still report a completed scan"
    );
    assert_eq!(
        view.entries
            .iter()
            .map(|e| e.name.clone())
            .collect::<Vec<_>>(),
        vec![
            "adapter.json",
            "configuration.json",
            "system.json",
            "version.json"
        ],
        "an unreadable data directory must still surface the generated entries"
    );
    assert_eq!(
        view.note.as_deref(),
        Some("No logs or crash evidence were found.")
    );
}

#[test]
fn diagnostics_scan_outlives_its_controller() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir), None);
    let handle = controller.start_manifest_scan();
    drop(controller);

    handle
        .join()
        .expect("the scan thread must not panic once its controller is gone");
    let got = events.lock().unwrap().clone();
    assert!(
        got.len() <= 1,
        "the sink must hold at most the one scan event, never a second, got {got:?}"
    );
    if let [event] = got.as_slice() {
        assert!(
            matches!(event, ShellEvent::DiagnosticsScanFinished { .. }),
            "the one recorded event must be the scan's own, got {event:?}"
        );
    }
}

#[test]
fn diagnostics_destination_field_scrolls_to_keep_cursor_visible() {
    let mut controller = AppController::new();
    assert_eq!(
        controller.diagnostics_destination_scroll_offset(),
        0.0,
        "a fresh controller must start unscrolled"
    );

    // The cursor sits past the visible width -- the field must scroll left
    // (a negative offset) just enough to bring it back inside.
    controller.diagnostics_destination_cursor_moved(140.0, 100.0);
    let offset = controller.diagnostics_destination_scroll_offset();
    assert!(
        offset < 0.0,
        "a cursor past the visible width must scroll the field, got {offset}"
    );
    assert!(
        (0.0..=100.0).contains(&(140.0 + offset)),
        "the cursor must land back inside the visible width: cursor 140 + offset {offset}"
    );

    // The cursor moves back near the field's start (Home, or backspacing to
    // a short prefix) -- the field must scroll back to show it rather than
    // leaving it hidden behind the still-scrolled offset.
    controller.diagnostics_destination_cursor_moved(2.0, 100.0);
    let offset = controller.diagnostics_destination_scroll_offset();
    assert!(
        (0.0..=100.0).contains(&(2.0 + offset)),
        "the cursor must land back inside the visible width after scrolling left: cursor 2 + offset {offset}"
    );

    // A cursor that is already inside the visible width leaves the offset
    // untouched.
    let mut steady = AppController::new();
    steady.diagnostics_destination_cursor_moved(50.0, 100.0);
    let before = steady.diagnostics_destination_scroll_offset();
    steady.diagnostics_destination_cursor_moved(55.0, 100.0);
    assert_eq!(
        steady.diagnostics_destination_scroll_offset(),
        before,
        "a cursor still inside the visible width must not move the offset"
    );
}

// --- Crash notice: pending count, dismiss, review, timeout, bundle write --

use focusforge_gui::SettingsSection;

/// [`seed_two_crashes`] plus a third crash that already carries its
/// reported marker -- still two unreported crashes.
fn seed_notice_crashes(root: &Path) -> std::path::PathBuf {
    let data = seed_two_crashes(root);
    let crashes = data.join("crashes");
    fs::write(
        crashes.join("20260918T010205Z-focusforge-gui-old.json"),
        "{\"format\":1,\"payload\":\"already seen\"}",
    )
    .unwrap();
    fs::write(
        crashes.join("20260918T010205Z-focusforge-gui-old.reported"),
        "2026-09-18T01:02:06Z",
    )
    .unwrap();
    data
}

#[test]
fn crash_notice_counts_and_dismisses() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());
    assert_eq!(controller.crash_notice_count(), 2);
    assert_eq!(controller.crash_notice_text(), "2 crashes found");

    controller.dismiss_crash_notice();
    assert!(!controller.crash_notice_visible());
    assert_eq!(controller.crash_notice_count(), 0);

    let mut after = AppController::new();
    after.set_diagnostics_dirs(Some(data_dir), None);
    after.scan_crash_notice();
    assert!(
        !after.crash_notice_visible(),
        "a fresh controller over the same directory must see nothing pending"
    );
    assert_eq!(after.crash_notice_count(), 0);

    // A second data directory with an empty crash directory counts 0 from
    // the start.
    let empty_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_root.path().join("focusforge/crashes")).unwrap();
    let mut empty = AppController::new();
    empty.set_diagnostics_dirs(Some(empty_root.path().join("focusforge")), None);
    empty.scan_crash_notice();
    assert!(!empty.crash_notice_visible());
    assert_eq!(empty.crash_notice_count(), 0);
}

#[test]
#[cfg(unix)]
fn crash_notice_unreadable_dir() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());
    let crashes = data_dir.join("crashes");
    let writable = fs::metadata(&crashes).unwrap().permissions();
    fs::set_permissions(&crashes, fs::Permissions::from_mode(0o000)).unwrap();

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir), None);
    controller.scan_crash_notice();

    fs::set_permissions(&crashes, writable).unwrap();

    assert!(
        !controller.crash_notice_visible(),
        "an unreadable crash directory must show no notice"
    );
    assert_eq!(controller.crash_notice_count(), 0);
}

#[test]
fn crash_notice_timeout_reports_nothing() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());
    let count_before = controller.crash_notice_count();

    controller.crash_notice_timed_out();
    assert!(!controller.crash_notice_visible());
    assert!(
        !data_dir
            .join("crashes/20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "a timeout must write no reported marker"
    );
    assert!(
        !data_dir
            .join("crashes/20260918T010204Z-focusforge-gui-native.reported")
            .exists(),
        "a timeout must write no reported marker"
    );

    let mut after = AppController::new();
    after.set_diagnostics_dirs(Some(data_dir), None);
    after.scan_crash_notice();
    assert!(
        after.crash_notice_visible(),
        "the evidence must still be pending after a timeout"
    );
    assert_eq!(after.crash_notice_count(), count_before);
}

#[test]
fn crash_notice_review_opens_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());

    let open = controller.review_crashes();
    assert!(open, "review must leave the settings modal open");
    assert_eq!(controller.settings_section(), SettingsSection::Diagnostics);
    assert!(!controller.crash_notice_visible());
    assert!(
        data_dir
            .join("crashes/20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "review must mark the pending evidence reported, same as dismiss"
    );
    assert!(
        data_dir
            .join("crashes/20260918T010204Z-focusforge-gui-native.reported")
            .exists()
    );

    let marker = data_dir.join("crashes/20260918T010203Z-focusforge-gui-panic.reported");
    let marked_at = fs::read_to_string(&marker).unwrap();
    let open_again = controller.review_crashes();
    assert!(open_again);
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        marked_at,
        "a second call must mark nothing further"
    );
}

#[test]
fn crash_notice_cleared_by_a_bundle_write() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_two_crashes(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());
    assert_eq!(controller.crash_notice_count(), 2);

    controller.set_destination(root.path().join("bundle.zip").to_str().unwrap());
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    assert!(
        !controller.crash_notice_visible(),
        "a bundle written from the window must hide the notice"
    );
    assert_eq!(controller.crash_notice_count(), 0);

    // A failed write leaves it showing.
    let root2 = tempfile::tempdir().unwrap();
    let data_dir2 = seed_two_crashes(root2.path());
    let mut controller2 = AppController::new();
    let events2 = install_recording_sink(&mut controller2);
    controller2.set_diagnostics_dirs(Some(data_dir2.clone()), None);
    controller2.scan_crash_notice();
    assert!(controller2.crash_notice_visible());

    let blocked_parent = data_dir2.parent().unwrap().join("blocked");
    fs::write(&blocked_parent, b"not a directory").unwrap();
    controller2.set_destination(blocked_parent.join("bundle.zip").to_str().unwrap());
    let status2 = run_write(&mut controller2, &events2);
    assert_eq!(status2, DiagnosticsStatus::Failed);
    assert!(
        controller2.crash_notice_visible(),
        "a failed write must leave the notice showing"
    );
    assert_eq!(controller2.crash_notice_count(), 2);
}

/// A controller with `fixtures/mantis-512` open as one tab, its scratch root
/// under `CARGO_TARGET_TMPDIR`. A first fuse writes nothing beside the frames.
fn controller_on_mantis_512(name: &str) -> AppController {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    let mantis_512 = common::workspace_root().join("fixtures").join("mantis-512");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[mantis_512]),
        focusforge_gui::OpenOutcome::Opened(_)
    ));
    controller
}

#[test]
fn a_default_fuse_of_mantis_512_parks_naming_the_unaligned_frames() {
    let mut controller = controller_on_mantis_512("default_fuse_mantis_512");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, std::time::Duration::from_secs(300));
    assert!(
        controller.job_failure_message().is_none(),
        "a default fuse completes rather than failing"
    );
    assert_eq!(controller.checkpoint_count(), Some(0));
    let names = ["00.tif", "01.tif", "07.tif"].map(String::from);
    assert_eq!(controller.pending_divergence(), Some(&names[..]));
}

#[test]
fn an_unresolved_align_id_is_refused_before_any_run() {
    let mut controller = controller_on_mantis_512("unresolved_align_id");
    controller.set_align_algorithm("");
    assert!(!controller.start_fuse());
    assert!(!controller.any_job_running());
    assert!(
        controller.last_refusal().is_some_and(|m| !m.is_empty()),
        "the refusal must be reported"
    );
    assert_eq!(
        controller.job_view().state,
        focusforge_gui::job::RunState::Idle
    );
}
