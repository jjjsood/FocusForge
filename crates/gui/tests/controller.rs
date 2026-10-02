// SPDX-License-Identifier: GPL-3.0-or-later

//! `AppController` is plain Rust: no Slint backend is initialised anywhere
//! in this file, and none is needed -- the callback-forwarding rule is what
//! these tests pin.

mod common;

use std::fs;
use std::sync::{Arc, Mutex};

use focusforge_core::align::Registration;
use focusforge_core::io::Rect;
use focusforge_core::project::{self, Op, Point, Project};
use focusforge_core::zmap::ZMapReader;
use focusforge_gui::{AppController, DockTab, ShellEvent, ShellMode};

/// True when `body` (already trimmed) is exactly one statement of the form
/// `root.<ident>(...)`, an optional trailing `accept`/`reject` (`scroll-event`
/// and the key handlers return an `EventResult`), a bare `accept`/`reject`
/// alone (a scrim swallowing the event with nothing to forward), a bare
/// literal assignment (`self.<ident> = <literal>;` or
/// `<child-id>.<ident> = <literal>;`), or one
/// `if <property-path> { <literal-assignment> }` with no `else` -- a small
/// Slint-side gate resetting itself. Nothing else widens: two statements, a
/// computed or forwarded right-hand side, or an `else` branch all still fail.
fn is_single_root_call(body: &str) -> bool {
    let body = body.trim();
    let stripped = body
        .strip_suffix("accept")
        .or_else(|| body.strip_suffix("reject"))
        .map(str::trim_end);
    if stripped.is_some_and(str::is_empty) {
        return true;
    }
    let body = stripped.unwrap_or(body);
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
fn is_single_root_call_allows_a_bare_accept_or_reject() {
    assert!(is_single_root_call("accept"));
    assert!(is_single_root_call("reject"));
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
    assert!(common::save_and_settle(controller, |proposed| {
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
    common::enter_editor(&mut reopened);
    assert_eq!(reopened.mode(), ShellMode::Editor);
    assert_eq!(
        *events.lock().unwrap(),
        vec![ShellEvent::ModeChanged(ShellMode::Editor)]
    );
}

#[test]
fn fuse_auto_enters_editor_on_the_viewed_tab() {
    let mut controller = controller_with_scratch("fuse_auto_editor");
    open_fused_tab(&mut controller, "fuse_auto_editor", 3);
    assert_eq!(
        controller.mode(),
        ShellMode::Editor,
        "a fuse on the viewed tab enters Editor"
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
    // A fuse now auto-enters Editor on the viewed tab; back out to Stacking
    // so this test's own frame-management calls below (Stacking-only) run.
    controller.set_mode(ShellMode::Stacking);
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
    controller.set_mode(ShellMode::Stacking);
    assert_eq!(controller.checkpoint_count(), Some(2));
    assert!(
        controller.editor_enabled(),
        "a newer checkpoint over the current frame set is editable again"
    );

    // The added file exists for `add_frames` but is deleted before
    // `confirm_frame_change` re-fuses, so the Add path hits the "a
    // referenced frame vanished" refusal.
    let added = root.join("added.tif");
    focusforge_core::io::test_frames::write_frame_with_capture_time(&added, 4, 4, None)
        .expect("a synthetic frame must write");
    common::add_and_settle(&mut controller, std::slice::from_ref(&added))
        .expect("the add must not refuse");
    fs::remove_file(&added).expect("the added frame must delete");
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
    controller.set_mode(ShellMode::Stacking);
    open_fused_tab(&mut controller, "edit_session_per_tab_b", 3);
    // A fuse now auto-enters Editor on the viewed tab (tab 1, just fused),
    // which opens its session at commit time, not lazily on a later manual
    // entry -- back to Stacking so the manual (re-)entry below, and the
    // per-tab isolation this test is actually about, are what's under test.
    controller.set_mode(ShellMode::Stacking);

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
        Some(0),
        "a save keeps the session, rebased onto the checkpoint it wrote"
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
    assert!(!controller.start_fuse(), "a dab in live asks first");
    controller.close_guard_confirmed();
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
    // `checkpoint_export_requested` is a no-op (no `CheckpointExportRequested`
    // raised) with no active tab; its positive case is
    // `tests/checkpoint_export.rs::export_button_opens_the_modal_for_the_current_position`.
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
            ShellEvent::WindowCloseRequested,
        ],
        "events must reach the sink in the order they were raised"
    );
}

/// A press on the drawn `Checkpoints` scrollbar thumb grabs it at the press
/// offset instead of jumping the list: the thumb math reads the content
/// height `checkpoints_panel()` builds, the same number the list draws
/// with. Only a press on the track outside the thumb may jump.
#[test]
fn pressing_the_checkpoints_scrollbar_thumb_grabs_it_without_jumping() {
    use focusforge_core::io::{TiffSpec, TiffWriter};
    use focusforge_core::params::ParamValues;
    use focusforge_core::project::{Metrics, SealRequest, ZMapStorage, append_ops_at, seal_at};
    use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

    let dir = common::tree("checkpoints_scrollbar_thumb");
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
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

    // Ten same-day checkpoints -- no day separator, so `content_height` is
    // exactly 10 * 40px = 400px, the fixed number the math below assumes.
    let project_path = dir.join("scrollbar.ffp");
    let noon = 1_756_468_800; // an arbitrary local noon
    seal_at(
        SealRequest {
            project_path: &project_path,
            staged_composite: &composite,
            staged_zmap: &zmap,
            sources: &[],
            algorithm: "laplacian-pyramid",
            params: &ParamValues::new(),
            align: None,
            registration: Registration::default(),
            composite_description: "staged".to_string(),
            label: None,
            metrics: Metrics::default(),
        },
        noon,
    )
    .unwrap();
    for i in 1..10i64 {
        let op = Op::BrushStroke {
            source_id: 0,
            path: vec![Point { x: 1, y: 1 }],
            radius: 0.5,
            softness: 0.0,
        };
        append_ops_at(
            &project_path,
            vec![op],
            None,
            noon + i * 60,
            ZMapStorage::Bounded,
        )
        .unwrap_or_else(|err| panic!("checkpoint {i} must append: {err}"));
    }

    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[project_path]);
    assert_eq!(controller.checkpoints_panel().content_height, 400.0);

    // A 400px list in a 100px viewport, scrolled to its midpoint: thumb
    // height 100*100/400 = 25px, thumb top 150*100/400 = 37.5px, so the
    // thumb spans [37.5, 62.5).
    controller.set_checkpoints_viewport_height(100.0);
    controller.scroll_checkpoints_panel(150.0);
    assert_eq!(controller.checkpoints_scroll_y(), 150.0);

    // Press inside the thumb: no jump.
    controller.checkpoints_scrollbar_pointer(PointerKind::Down, 50.0);
    assert_eq!(
        controller.checkpoints_scroll_y(),
        150.0,
        "a press on the thumb itself must not move the list"
    );

    // Dragging from there moves the list smoothly, by the drag delta.
    controller.checkpoints_scrollbar_dragged(70.0);
    assert_eq!(controller.checkpoints_scroll_y(), 230.0);
    controller.checkpoints_scrollbar_pointer(PointerKind::Up, 70.0);

    // A press on the track outside the thumb may jump.
    controller.scroll_checkpoints_panel(150.0 - controller.checkpoints_scroll_y());
    assert_eq!(controller.checkpoints_scroll_y(), 150.0);
    controller.checkpoints_scrollbar_pointer(PointerKind::Down, 90.0);
    assert_eq!(
        controller.checkpoints_scroll_y(),
        300.0,
        "a press on the bare track may jump, clamped to the list's end"
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

use common::{copy_frame, tree};

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
    let folder = tree("controller_shape_folder");
    copy_frame(&folder, "00.tif", 0);
    copy_frame(&folder, "01.tif", 1);

    let project_dir = tree("controller_shape_project");
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
            registration: focusforge_core::align::Registration::default(),
            composite_description: "staged".to_string(),
            label: None,
            metrics: Metrics::default(),
        })
        .unwrap();
        project_path
    };

    let frames_dir = tree("controller_shape_frames");
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
    let two_subfolders = tree("controller_tick_two_subfolders");
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
    let opened_root = tree("controller_term_opened");
    copy_frame(&opened_root, "00.tif", 0);
    copy_frame(&opened_root, "01.tif", 1);

    let question_root = tree("controller_term_question");
    copy_frame(&question_root.join("subject-a"), "00.tif", 0);
    copy_frame(&question_root.join("subject-b"), "00.tif", 1);

    let refused_root = tree("controller_term_refused");

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
    let first = tree("controller_dropped_first");
    copy_frame(&first, "00.tif", 0);
    let second = tree("controller_dropped_second");
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

    let root = tree("controller_thumbnail_worker");
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

    let root = tree("controller_question_thumbnail_worker");
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

    let root = tree("controller_thumbnail_failure");
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
    controller.toggle_projects_overlay();
    common::settle_thumbnails(&mut controller);
    assert!(
        controller.project_cards()[0].picture.is_none(),
        "opening the overlay must not decode the picture again"
    );
}

/// The card of the last open tab.
fn last_open_card(controller: &AppController) -> focusforge_gui::overlay::CardView {
    controller
        .project_cards()
        .into_iter()
        .rfind(|card| card.kind == "open")
        .expect("an open tab has a card")
}

/// Settles the running fuse without applying its picture.
fn settle_fuse_only(controller: &mut AppController) {
    let start = Instant::now();
    while controller.work_pending() {
        controller.settle_job();
        assert!(start.elapsed() < SETTLE, "the fuse did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_duplicated_tab_gets_its_picture_off_thread() {
    let mut controller = controller_with_scratch("dup_picture");
    open_fused_tab(&mut controller, "dup_picture", 3);
    assert!(controller.set_mode(ShellMode::Stacking));
    controller.remove_frame(0);
    assert!(controller.new_project_instead());

    assert!(
        last_open_card(&controller).picture.is_none(),
        "the picture is decoded off the UI thread, not while the tab opens"
    );
    common::settle_thumbnails(&mut controller);
    let picture = last_open_card(&controller)
        .picture
        .expect("the settled decode gives the new tab its first frame's picture");
    assert!(picture.rgba.iter().any(|&b| b != 0));
}

#[test]
fn a_fused_checkpoint_picture_is_dropped_for_a_closed_tab() {
    let mut controller = controller_with_scratch("closed_picture");
    open_mantis64_tab(&mut controller, "closed_picture_a", 2);
    let before = last_open_card(&controller)
        .picture
        .expect("an unfused tab carries its first frame");
    open_mantis64_tab(&mut controller, "closed_picture_b", 3);
    assert!(controller.start_fuse());
    settle_fuse_only(&mut controller);

    controller.close_project(1);
    common::settle_thumbnails(&mut controller);
    assert_eq!(controller.project_tabs().len(), 1);
    assert_eq!(
        last_open_card(&controller).picture.as_deref(),
        Some(&*before),
        "the closed tab's picture must not land on the remaining one"
    );
}

#[test]
fn an_older_picture_never_replaces_a_newer_one() {
    let fused_picture = |name: &str, frames: usize| {
        let mut controller = controller_with_scratch(name);
        open_fused_tab(&mut controller, name, frames);
        common::settle_thumbnails(&mut controller);
        last_open_card(&controller)
            .picture
            .expect("a fused tab has a picture")
    };
    let first = fused_picture("older_picture_first", 3);
    let second = fused_picture("older_picture_second", 2);
    assert_ne!(first.rgba, second.rgba, "the two fuses must look different");

    let mut controller = controller_with_scratch("older_picture");
    open_mantis64_tab(&mut controller, "older_picture", 3);
    assert!(controller.start_fuse());
    settle_fuse_only(&mut controller);
    assert!(controller.set_mode(ShellMode::Stacking));
    controller.toggle_frame_excluded(2);
    assert!(controller.start_fuse());
    settle_fuse_only(&mut controller);

    common::settle_thumbnails(&mut controller);
    assert_eq!(
        last_open_card(&controller).picture.as_deref(),
        Some(&*second),
        "the newer fuse's picture must win whatever order the decodes finish in"
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

// ---------------------------------------------------------------------
// Both panes' shared geometry: one image size and one viewport, whichever
// frame is selected, however the tab is zoomed or panned, and for a frame
// added after the tab's last fuse.
// ---------------------------------------------------------------------

/// `frame_draw` and `plane_draw` return the same visible source rect and the
/// same pane-local placement -- both panes drawing through the one shared
/// [`focusforge_gui::viewport::Viewport`].
fn assert_panes_correspond(controller: &AppController, case: &str) {
    let frame = controller
        .frame_draw()
        .unwrap_or_else(|| panic!("{case}: frame_draw must draw"));
    let plane = controller
        .plane_draw()
        .unwrap_or_else(|| panic!("{case}: plane_draw must draw"));
    assert_eq!(frame.src, plane.src, "{case}: same visible source rect");
    assert_eq!(frame.dst, plane.dst, "{case}: same pane-local placement");
}

/// On a tab holding a composite, `frame_draw` and `plane_draw` return the
/// same image size and the same visible source rect for every zoom step and
/// pan, whichever frame is selected; before the first fuse the left pane
/// still uses the raw frame's size.
#[test]
fn frame_and_plane_panes_share_one_size_and_rect() {
    let mut controller = controller_with_scratch("shared_pane_geometry");
    open_mantis64_tab(&mut controller, "shared_pane_geometry", 3);
    let left = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 300.0,
        height: 300.0,
    };
    let right = PaneRect {
        x: 308.0,
        y: 0.0,
        width: 300.0,
        height: 300.0,
    };
    controller.set_pane_geometry(left, right);

    assert_eq!(
        controller.frame_draw().unwrap().src,
        (0, 0, 64, 64),
        "before any fuse the left pane draws the raw frame's own size"
    );
    assert_panes_correspond(
        &controller,
        "before any fuse (no composite exists yet to actually render)",
    );

    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert_eq!(controller.checkpoint_count(), Some(1));
    // `commit_held` clears the viewport so the next fit re-derives it from
    // the new composite's own extent; re-installing the same pane geometry
    // re-fits it.
    controller.set_pane_geometry(left, right);

    for frame in 0..3 {
        controller.select_frame(frame);
        assert_panes_correspond(&controller, &format!("frame {frame} at fit"));

        while !controller.at_max_zoom() {
            controller.zoom_in();
        }
        assert_panes_correspond(&controller, &format!("frame {frame} at max zoom"));

        while !controller.at_min_zoom() {
            controller.zoom_out();
        }
        assert_panes_correspond(&controller, &format!("frame {frame} at min zoom"));

        controller.fit();
    }

    controller.zoom_in();
    controller.wheel(
        0.0,
        60.0,
        0.0,
        0.0,
        Modifiers {
            shift: true,
            ..Modifiers::NONE
        },
    );
    assert_panes_correspond(&controller, "after a pan");
}

/// A frame with no recorded transform (added to the tab after its last
/// fuse) is drawn through identity at the composite's crop, so the panes
/// still share one size.
#[test]
fn post_fuse_frame_uses_identity_at_the_composite_crop() {
    let mut controller = controller_with_scratch("post_fuse_frame");
    let root = open_fused_tab(&mut controller, "post_fuse_frame", 3);
    // A fuse now auto-enters Editor on the viewed tab; back out to Stacking
    // so `add_frames` below (Stacking-only) actually extends the frame set.
    controller.set_mode(ShellMode::Stacking);

    // The `editor_mode_requires_the_fused_frame_set` pattern: a missing
    // frame still lands in `tab.frames` (`apply_add_frames` runs
    // unconditionally) but refuses the re-fuse `start_fuse` would need to
    // give it its own transform, so the held checkpoint stays as it was.
    let added = root.join("post_fuse_added.tif");
    focusforge_core::io::test_frames::write_frame_with_capture_time(&added, 4, 4, None)
        .expect("a synthetic frame must write");
    common::add_and_settle(&mut controller, std::slice::from_ref(&added))
        .expect("the add must not refuse");
    fs::remove_file(&added).expect("the added frame must delete");
    assert!(
        !controller.confirm_frame_change(),
        "a missing frame refuses the re-fuse"
    );
    assert_eq!(
        controller.checkpoint_count(),
        Some(1),
        "no newer checkpoint lands"
    );

    controller.select_frame(3);
    let view = controller
        .selected_frame_view()
        .expect("a fused tab's selected frame always has a view");
    assert_eq!(
        view.transform,
        job::Transform::identity(),
        "a frame added after the fuse has no recorded transform"
    );
    let origin = controller
        .held_checkpoints()
        .last()
        .unwrap()
        .registration
        .origin();
    assert_eq!(view.origin, origin, "the composite's own crop origin");

    controller.set_pane_geometry(
        PaneRect {
            x: 0.0,
            y: 0.0,
            width: 300.0,
            height: 300.0,
        },
        PaneRect {
            x: 308.0,
            y: 0.0,
            width: 300.0,
            height: 300.0,
        },
    );
    controller.fit();
    assert_panes_correspond(&controller, "the added frame at identity");
}

// ---------------------------------------------------------------------
// The brush on a fused mantis-64 tab: strokes, brush controls, the cursor,
// the pipette, undo/redo, the live preview and the commit pass.
// ---------------------------------------------------------------------

use std::num::NonZeroU32;

use focusforge_core::io::{TiffSpec, TiffStack, TiffWriter};
use focusforge_core::retouch::{EditSession, apply, footprint};
use focusforge_core::zmap::{NO_SOURCE, ZMapWriter};
use focusforge_gui::brush::LiveStroke;
use focusforge_gui::canvas::overlay_edited;
use focusforge_gui::input_map::{Button, Modifiers, PointerKind};
use focusforge_gui::viewport::{PaneDraw, PaneRect};

/// The mantis-64 frames' own extent, which both panes draw at before any
/// fuse. This fixture's fuse (3 frames, `dummy-align`'s integer x-shifts,
/// cropped) leaves a `MAP_WIDTH` x `MAP_HEIGHT` composite -- the frames'
/// common area, a little smaller, anchored at a non-zero crop origin --
/// which both panes draw at once the tab holds one.
const FRAME: u32 = 64;
/// The composite this fixture's fuse always produces -- measured once
/// against `open_fused_tab(name, 3)` over `mantis-64`, not re-derived per
/// test.
const MAP_WIDTH: u32 = 58;
const MAP_HEIGHT: u32 = 60;
const SCALE: f32 = 4.0;
/// Each pane is `MAP_WIDTH` x `MAP_HEIGHT` at exactly 400 %, with a `BAND`
/// px band above and below it, so every image point below that sits on a
/// quarter-pixel grid maps to and from the screen without rounding.
const PANE_WIDTH: f32 = MAP_WIDTH as f32 * SCALE;
const BAND: f32 = (320.0 - MAP_HEIGHT as f32 * SCALE) / 2.0;
const LEFT: PaneRect = PaneRect {
    x: 0.0,
    y: 0.0,
    width: PANE_WIDTH,
    height: 320.0,
};
const RIGHT: PaneRect = PaneRect {
    x: PANE_WIDTH + 8.0,
    y: 0.0,
    width: PANE_WIDTH,
    height: 320.0,
};
const NO_MODS: Modifiers = Modifiers::NONE;
const CTRL: Modifiers = Modifiers {
    ctrl: true,
    ..Modifiers::NONE
};
const CTRL_SHIFT: Modifiers = Modifiers {
    ctrl: true,
    shift: true,
    ..Modifiers::NONE
};
const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::NONE
};
const ALT: Modifiers = Modifiers {
    alt: true,
    ..Modifiers::NONE
};

/// Binds `Alt + Wheel` to the brush softness in editor mode; the shipped
/// table leaves softness without a wheel binding.
fn bind_softness_to_alt_wheel(controller: &mut AppController) {
    use focusforge_gui::input_config::Provenance;
    use focusforge_gui::input_map::{Action, Binding, InputTable, Trigger};
    let mut table = InputTable::default();
    table.bindings.push(Binding {
        trigger: Trigger::Wheel(ALT),
        stacking: None,
        editor: Some(Action::BrushSoftness),
    });
    controller.install_input_table(table, Provenance::Defaults);
}

/// The active tab's whole fused map.
fn map_rect(controller: &AppController) -> Rect {
    controller.edit_session().unwrap().tile()
}

/// The screen point over the right pane whose paint point is image
/// `(x, y)` -- pixel centres sit on integers.
fn at(x: f32, y: f32) -> (f32, f32) {
    at_pane(RIGHT, x, y)
}

/// The screen point over `pane` whose paint point is image `(x, y)` -- both
/// panes draw at one size and one source rect, so `at`'s own formula holds
/// over either pane's rect, not just the right's.
fn at_pane(pane: PaneRect, x: f32, y: f32) -> (f32, f32) {
    (
        pane.x + (x + 0.5) * SCALE,
        pane.y + BAND + (y + 0.5) * SCALE,
    )
}

/// A fused three-frame mantis-64 tab in editor mode, both panes installed,
/// its resident display tiles 16 px so strokes cross tile edges.
fn open_editor(name: &str) -> (AppController, PathBuf) {
    let mut controller = controller_with_scratch(name);
    let root = open_fused_tab(&mut controller, name, 3);
    controller.set_resident_tile_size_for_test(16);
    controller.set_pane_geometry(LEFT, RIGHT);
    assert!(controller.set_mode(ShellMode::Editor));
    let map = map_rect(&controller);
    let draw = controller.plane_draw().expect("the fused tab draws");
    assert_eq!(draw.src, (0, 0, map.width, map.height));
    assert_eq!(
        draw.dst,
        (
            0.0,
            BAND,
            map.width as f32 * SCALE,
            map.height as f32 * SCALE
        ),
        "the whole composite at 400 %"
    );
    (controller, root)
}

/// Sets `Size` through its logarithmic slider.
fn set_size(controller: &mut AppController, size: f32) {
    let ceiling = controller.brush_toolbar_view().size_ceiling as f32;
    controller.set_brush_size_fraction(size.ln() / ceiling.ln());
}

fn press(controller: &mut AppController, point: (f32, f32)) {
    controller.pointer(PointerKind::Down, Button::Left, point.0, point.1, NO_MODS);
}

fn release(controller: &mut AppController, point: (f32, f32)) {
    controller.pointer(PointerKind::Up, Button::Left, point.0, point.1, NO_MODS);
}

/// A left drag through image points `path`, one motion event per point.
fn drag(controller: &mut AppController, path: &[(f32, f32)]) {
    let screen: Vec<(f32, f32)> = path.iter().map(|&(x, y)| at(x, y)).collect();
    press(controller, screen[0]);
    for &(x, y) in &screen[1..] {
        controller.pointer_moved(x, y);
    }
    release(controller, *screen.last().unwrap());
}

fn ops(controller: &AppController) -> Vec<Op> {
    controller.edit_session().unwrap().ops().cloned().collect()
}

fn indices(controller: &AppController) -> Vec<u16> {
    controller.edit_session().unwrap().indices().to_vec()
}

/// `rect` of the RGB TIFF at `path`.
fn read_plane(path: &Path, rect: Rect) -> Vec<u16> {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).unwrap();
    let mut plane = vec![0u16; (rect.width * rect.height * 3) as usize];
    stack.read_tile_into(0, rect, &mut plane).unwrap();
    plane
}

/// The tab's sealed planes and raw frames, read straight from disk, plus the
/// registration a stroke's own recomposite reads each raw frame through.
struct Sealed {
    map: Rect,
    indices: Vec<u16>,
    composite: Vec<u16>,
    frames: Vec<Vec<u16>>,
    registration: job::Registration,
}

impl Sealed {
    fn read(controller: &mut AppController, root: &Path) -> Self {
        let registration = controller
            .held_checkpoints()
            .last()
            .expect("a fused tab holds at least one checkpoint")
            .registration
            .clone();
        Self::read_with(controller, root, registration)
    }

    /// [`Self::read`] for a tab whose checkpoints are saved, so the
    /// registration of the row in view is given.
    fn read_with(
        controller: &mut AppController,
        root: &Path,
        registration: job::Registration,
    ) -> Self {
        let map = map_rect(controller);
        let frame = Rect {
            x: 0,
            y: 0,
            width: FRAME,
            height: FRAME,
        };
        let (composite, zmap) = controller.materialize_active_tab_planes().unwrap();
        Sealed {
            map,
            indices: read_whole_zmap(&zmap),
            composite: read_plane(&composite, map),
            frames: (0..3)
                .map(|i| read_plane(&root.join(format!("{i:02}.tif")), frame))
                .collect(),
            registration,
        }
    }

    /// What the renderer shows at step 1 over the whole image: the
    /// controller's own resident tiles overlaid on the sealed composite.
    fn displayed(&self, controller: &AppController) -> Vec<u16> {
        let view = controller.edited_view().unwrap();
        let mut samples = self.composite.clone();
        overlay_edited(
            &mut samples,
            &self.indices,
            view.indices,
            self.map,
            NonZeroU32::MIN,
            3,
            view.tiles,
        );
        samples
    }

    /// The reference: the edited index's own frame pixel wherever it differs
    /// from the sealed index, the sealed composite pixel elsewhere -- each
    /// frame pixel read at its own registered transform and the crop
    /// origin, the same view [`focusforge_core::retouch::EditSession`]'s own
    /// recomposite warps through. `dummy-align`'s shifts are exact integer
    /// translations, so the mapped point always lands on a source pixel
    /// centre with no rounding error.
    fn expected(&self, edited: &[u16]) -> Vec<u16> {
        let mut out = self.composite.clone();
        let origin = self.registration.origin();
        for (i, (&e, &s)) in edited.iter().zip(&self.indices).enumerate() {
            if e != s {
                let (x, y) = (i as u32 % self.map.width, i as u32 / self.map.width);
                let (rx, ry) = ((x + origin.0) as f64, (y + origin.1) as f64);
                let (fx, fy) = self
                    .registration
                    .transform(e as usize)
                    .map_point(rx, ry)
                    .expect("dummy-align's integer shifts never cross the horizon");
                let f = ((fy.round() as u32 * FRAME + fx.round() as u32) * 3) as usize;
                out[i * 3..i * 3 + 3].copy_from_slice(&self.frames[e as usize][f..f + 3]);
            }
        }
        out
    }

    fn assert_shown(&self, controller: &AppController, case: &str) {
        let edited = indices(controller);
        assert!(
            self.displayed(controller) == self.expected(&edited),
            "{case}: the displayed composite differs from the reference"
        );
    }
}

fn rounded(points: &[(f32, f32)]) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::new();
    for &(x, y) in points {
        let p = Point {
            x: x.round() as i32,
            y: y.round() as i32,
        };
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    out
}

fn dilate(rect: Rect, margin: u32, map: Rect) -> Rect {
    let x0 = rect.x.saturating_sub(margin);
    let y0 = rect.y.saturating_sub(margin);
    let x1 = (rect.x + rect.width + margin).min(map.width);
    let y1 = (rect.y + rect.height + margin).min(map.height);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// `corners` joined by straight runs, as `events` motion events spread
/// evenly over the whole length; every corner is delivered exactly.
fn polyline(corners: &[(f32, f32)], events: usize) -> Vec<(f32, f32)> {
    let lengths: Vec<f32> = corners
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
        .collect();
    let total: f32 = lengths.iter().sum();
    let mut out = vec![corners[0]];
    for (w, len) in corners.windows(2).zip(&lengths) {
        let n = ((len / total) * events as f32).round().max(1.0) as usize;
        for k in 1..=n {
            let t = k as f32 / n as f32;
            out.push(if k == n {
                w[1]
            } else {
                (
                    w[0].0 + (w[1].0 - w[0].0) * t,
                    w[0].1 + (w[1].1 - w[0].1) * t,
                )
            });
        }
    }
    out
}

#[test]
fn drag_produces_one_brush_stroke() {
    let (mut c, _) = open_editor("brush_one_stroke");
    set_size(&mut c, 8.0);
    c.set_brush_softness_fraction(0.25);
    c.select_frame(2);
    let radius = c.brush_toolbar_view().size / 2.0;

    let line: Vec<(f32, f32)> = (0..=20)
        .map(|i| (10.0 + 1.5 * i as f32, 10.0 + i as f32))
        .collect();
    drag(&mut c, &line);
    assert_eq!(
        ops(&c),
        vec![Op::BrushStroke {
            source_id: 2,
            path: vec![Point { x: 10, y: 10 }, Point { x: 40, y: 30 }],
            radius,
            softness: 0.25,
        }],
        "one op, its straight path resampled down to its two ends"
    );

    drag(&mut c, &[(20.0, 21.0)]);
    assert_eq!(
        ops(&c).last(),
        Some(&Op::BrushStroke {
            source_id: 2,
            path: vec![Point { x: 20, y: 21 }],
            radius,
            softness: 0.25,
        }),
        "a press-release with no motion is a one-point op"
    );
    assert_eq!(ops(&c).len(), 2);
    let before = indices(&c);

    press(&mut c, (100.0, 100.0));
    c.pointer_moved(RIGHT.x + 50.0, 100.0);
    c.flush_live_stroke();
    release(&mut c, (RIGHT.x + 50.0, 100.0));
    assert_eq!(ops(&c).len(), 2, "a drag starting on the left pane");

    // In the band above the image, with a 1 px brush.
    set_size(&mut c, 1.0);
    press(&mut c, (RIGHT.x + 100.0, 8.0));
    c.pointer_moved(RIGHT.x + 140.0, 20.0);
    c.flush_live_stroke();
    release(&mut c, (RIGHT.x + 140.0, 20.0));
    assert_eq!(ops(&c).len(), 2, "a stroke wholly off the image");
    assert_eq!(indices(&c), before);
}

#[test]
fn path_is_resampled_not_event_rate_dependent() {
    let (mut c, _) = open_editor("brush_event_rate");
    let corners = [(8.25, 9.0), (40.75, 12.5), (22.5, 50.25), (55.0, 44.75)];
    for size in [6.0f32, 13.0] {
        set_size(&mut c, size);
        drag(&mut c, &polyline(&corners, 40));
        drag(&mut c, &polyline(&corners, 4000));
        let ops = ops(&c);
        let [.., coarse, fine] = ops.as_slice() else {
            panic!("two strokes");
        };
        assert_eq!(coarse, fine, "size {size}: 40 and 4000 events");
        let Op::BrushStroke { path, .. } = fine else {
            panic!("a brush stroke");
        };
        assert!(path.len() < 40, "size {size}: {path:?}");
        assert_eq!(path.first(), Some(&Point { x: 8, y: 9 }));
        assert_eq!(path.last(), Some(&Point { x: 55, y: 45 }));
    }
}

#[test]
fn resampled_path_is_bounded_by_length() {
    let (mut c, _) = open_editor("brush_bounded");
    set_size(&mut c, 8.0);
    let radius = c.brush_toolbar_view().size / 2.0;
    let wobble: Vec<(f32, f32)> = (0..=4000)
        .map(|i| {
            let x = 5.0 + 53.0 * i as f32 / 4000.0;
            (x, 32.0 + 10.0 * (x / 6.0).sin() + 0.3 * (x * 7.0).sin())
        })
        .collect();
    let length: f32 = wobble
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
        .sum();
    drag(&mut c, &wobble);
    let Some(Op::BrushStroke { path, .. }) = ops(&c).pop() else {
        panic!("one stroke");
    };
    let bound = length / (radius / 4.0) + 2.0;
    assert!(
        path.len() as f32 <= bound,
        "{} points over a {length} px trace",
        path.len()
    );
    assert!(path.len() < 100, "{} points", path.len());
}

/// Opens the first two mantis-64 frames cropped to `width` x `height` as a
/// new active tab.
fn open_cropped_tab(controller: &mut AppController, name: &str, width: u32, height: u32) {
    let root = common::tree(&format!("{name}_frames"));
    let crop = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    for i in 0..2 {
        let stack = TiffStack::open_files(&[common::fixture_frame("mantis-64", i)]).unwrap();
        let mut samples = vec![0u16; (width * height * 3) as usize];
        stack.read_tile_into(0, crop, &mut samples).unwrap();
        let spec = TiffSpec {
            width,
            height,
            samples_per_pixel: 3,
            description: None,
        };
        let mut writer = TiffWriter::create(&root.join(format!("{i:02}.tif")), spec).unwrap();
        writer.write_tile(crop, &samples).unwrap();
        writer.finish().unwrap();
    }
    assert!(matches!(
        common::open_and_settle(controller, &[root]),
        OpenOutcome::Opened(_)
    ));
}

#[test]
fn brush_controls_convert_to_storage_units() {
    let (mut c, _) = open_editor("brush_units");
    let size = |c: &AppController| c.brush_toolbar_view().size;
    let soft = |c: &AppController| c.brush_toolbar_view().soft;
    let map = map_rect(&c);
    let ceiling = map.width.min(map.height);
    assert!(
        ceiling < FRAME,
        "the fused image is the frames' common area"
    );
    let view = c.brush_toolbar_view();
    assert_eq!(
        (view.size_ceiling, view.size, view.soft),
        (ceiling, ceiling as f32, 30.0),
        "the ceiling is the fused image's shorter side, the default min(64, ceiling)"
    );
    let ceiling = ceiling as f32;

    c.set_brush_size_fraction(0.5);
    let view = c.brush_toolbar_view();
    assert!((view.size - ceiling.sqrt()).abs() < 1e-3, "{}", view.size);
    assert!((view.size_fraction - 0.5).abs() < 1e-5);
    c.set_brush_size_fraction(1.0);
    assert_eq!(size(&c), ceiling, "the track's right end is the ceiling");
    c.set_brush_size_fraction(0.0);
    assert_eq!(size(&c), 1.0);

    let quarter_octave = 2f32.powf(0.25);
    c.set_brush_size_fraction(0.5);
    let s = size(&c);
    c.wheel(0.0, 60.0, 0.0, 0.0, SHIFT);
    assert!((size(&c) - s * quarter_octave).abs() < 1e-3);
    c.wheel(0.0, -120.0, 0.0, 0.0, SHIFT);
    assert!((size(&c) - s / quarter_octave).abs() < 1e-3);
    c.set_brush_size_fraction(1.0);
    c.wheel(0.0, 60.0, 0.0, 0.0, SHIFT);
    assert_eq!(size(&c), ceiling, "clamped at the ceiling");
    c.set_brush_size_fraction(0.0);
    c.wheel(0.0, -60.0, 0.0, 0.0, SHIFT);
    assert_eq!(size(&c), 1.0, "clamped at 1 px");

    assert_eq!(
        {
            c.wheel(0.0, 60.0, 0.0, 0.0, ALT);
            soft(&c)
        },
        30.0,
        "the shipped table binds no wheel to softness"
    );
    bind_softness_to_alt_wheel(&mut c);
    c.wheel(0.0, 60.0, 0.0, 0.0, ALT);
    assert_eq!(soft(&c), 35.0);
    c.set_brush_softness_fraction(1.0);
    c.wheel(0.0, 60.0, 0.0, 0.0, ALT);
    assert_eq!(soft(&c), 100.0, "clamped at 100");
    c.set_brush_softness_fraction(0.0);
    c.wheel(0.0, -60.0, 0.0, 0.0, ALT);
    assert_eq!(soft(&c), 0.0, "clamped at 0");

    c.set_brush_size_fraction(0.5);
    c.set_brush_softness_fraction(0.4);
    let view = c.brush_toolbar_view();
    for press_at in [(17.0, 42.0), (17.4, 41.6)] {
        drag(&mut c, &[press_at]);
        assert_eq!(
            ops(&c).last(),
            Some(&Op::BrushStroke {
                source_id: c.brush_source().unwrap(),
                path: vec![Point { x: 17, y: 42 }],
                radius: view.size / 2.0,
                softness: view.soft / 100.0,
            }),
            "pressed at {press_at:?}"
        );
    }

    c.select_frame(2);
    c.wheel(0.0, 60.0, 0.0, 0.0, CTRL);
    assert_eq!(c.brush_source(), Some(1), "Ctrl + Wheel steps the source");
    c.wheel(0.0, 120.0, 0.0, 0.0, CTRL);
    assert_eq!(c.brush_source(), Some(0), "clamped to the frame list");

    let mut narrow = controller_with_scratch("brush_units_narrow");
    open_cropped_tab(&mut narrow, "brush_units_narrow", 40, 24);
    narrow.set_align_algorithm("dummy-align");
    assert!(narrow.start_fuse());
    settle_all(&mut narrow);
    let held = narrow
        .held_checkpoints()
        .last()
        .cloned()
        .expect("the narrow tab fuses");
    let narrow_ceiling = held.width.min(held.height);
    assert!(held.width > held.height, "a non-square fused image");
    let view = narrow.brush_toolbar_view();
    assert_eq!(
        (view.size_ceiling, view.size),
        (narrow_ceiling, narrow_ceiling as f32)
    );
    narrow.set_brush_size_fraction(1.0);
    assert_eq!(size(&narrow), narrow_ceiling as f32);
}

#[test]
fn plain_wheel_over_the_images_list_steps_the_source_frame_in_both_modes() {
    let mut stacking = controller_with_scratch("images_wheel_stacking");
    open_mantis64_tab(&mut stacking, "images_wheel_stacking", 3);
    stacking.select_frame(2);
    stacking.images_list_wheel(60.0);
    assert_eq!(
        stacking.frame_pane_label(),
        "01.tif",
        "plain wheel over the list steps the source in stacking mode, matching \
         Ctrl + Wheel's own direction"
    );
    stacking.images_list_wheel(120.0);
    assert_eq!(
        stacking.frame_pane_label(),
        "00.tif",
        "clamped to the frame list"
    );

    let (mut editor, _) = open_editor("images_wheel_editor");
    editor.select_frame(2);
    editor.images_list_wheel(60.0);
    assert_eq!(
        editor.frame_pane_label(),
        "01.tif \u{b7} source 2 of 3",
        "the same wheel steps the source in editor mode too, with no Ctrl needed"
    );
}

// ---------------------------------------------------------------------
// The editor's `Images` legend and the overlay controls.
// ---------------------------------------------------------------------

use focusforge_gui::canvas::OverlayDraw;
use focusforge_gui::images::FrameSort;

#[test]
fn legend_rows_carry_the_z_map_palette() {
    let mut c = controller_with_scratch("legend_palette");
    open_fused_tab(&mut c, "legend_palette", 3);
    // Excluded after the fuse: the Z-Map is unchanged, so the legend must
    // still list, number and colour this frame like any other source.
    c.toggle_frame_excluded(1);
    assert!(c.set_mode(ShellMode::Editor));

    let panel = c.images_panel();
    let table = focusforge_core::zmap::overlay_table(3, None);
    for (i, row) in panel.rows.iter().enumerate() {
        assert_eq!(
            row.chip, table[i],
            "row {i}'s colour must come from the Z-Map palette"
        );
        assert_eq!(
            row.chip,
            focusforge_core::zmap::source_color(i as u16, 3),
            "row {i} must equal source_color(i, n) with nothing soloed"
        );
        assert_eq!(row.trailing_text, (i + 1).to_string());
    }
    assert!(
        !panel.rows[1].excluded,
        "an exclusion made after the fuse changes no Z-Map source"
    );
}

#[test]
fn legend_header_states() {
    let mut c = controller_with_scratch("legend_header");
    open_fused_tab(&mut c, "legend_header", 3);
    // A fuse now auto-enters Editor on the viewed tab; back out to Stacking
    // so the "before" half of this test observes the Stacking-mode legend.
    c.set_mode(ShellMode::Stacking);
    c.toggle_frame_excluded(1);
    assert_eq!(
        c.images_panel().header_text,
        "3 frames \u{b7} 1 excluded",
        "stacking keeps the exclusion count"
    );

    assert!(c.set_mode(ShellMode::Editor));
    assert_eq!(
        c.images_panel().header_text,
        "3 frames",
        "the editor drops the exclusion count"
    );

    // `images_panel()` runs a fused tab through this same pure builder --
    // the singular form at `n = 1` needs no real fuse to prove.
    let one = focusforge_gui::images::build_model(
        &[PathBuf::from("00.tif")],
        &[false],
        None,
        true,
        &std::collections::HashMap::new(),
    );
    assert_eq!(one.header_text, "1 frame");
}

#[test]
fn legend_lists_only_frames_and_rejects_a_position_past_them() {
    let mut c = controller_with_scratch("legend_no_source");
    open_fused_tab(&mut c, "legend_no_source", 3);
    c.set_mode(ShellMode::Stacking);
    assert!(c.set_mode(ShellMode::Editor));
    let panel = c.images_panel();
    assert_eq!(panel.rows.len(), 3, "the legend holds one row per frame");

    let before = c.brush_source();
    c.select_frame(3);
    assert_eq!(
        before,
        c.brush_source(),
        "select_frame(N) past the frame list changes nothing"
    );
    // A pipette pick landing on `NO_SOURCE` leaving the source unchanged is
    // covered by `pipette_reads_and_appends_nothing`.
}

#[test]
fn legend_current_source_row_follows_all_inputs() {
    let (mut c, _) = open_editor("legend_wash");
    let selected_row = |c: &AppController| {
        let panel = c.images_panel();
        let selected: Vec<usize> = panel
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.selected)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(selected.len(), 1, "exactly one row carries the wash");
        assert_eq!(
            Some(selected[0] as u16),
            c.brush_source(),
            "the washed row is always the brush source"
        );
        selected[0]
    };

    c.select_frame(2);
    assert_eq!(selected_row(&c), 2, "a row click moves the wash");

    c.wheel(0.0, 60.0, 0.0, 0.0, CTRL);
    assert_eq!(selected_row(&c), 1, "Ctrl + Wheel moves the wash");

    let map = map_rect(&c);
    let zmap = c.held_checkpoints().last().unwrap().zmap.clone();
    let indices = read_whole_zmap(&zmap);
    let differing = |current: u16| {
        (0..map.height)
            .flat_map(|y| (0..map.width).map(move |x| (x, y)))
            .find_map(|(x, y)| {
                let index = indices[(y * map.width + x) as usize];
                (index != current && index != NO_SOURCE).then_some(((x as f32, y as f32), index))
            })
            .expect("the fused map names more than one source")
    };

    let (pixel, picked) = differing(c.brush_source().unwrap());
    c.toggle_pipette();
    drag(&mut c, &[pixel]);
    assert_eq!(c.brush_source(), Some(picked), "the armed click picks");
    assert_eq!(
        selected_row(&c),
        picked as usize,
        "a pipette pick moves the wash"
    );

    let (pixel, picked) = differing(c.brush_source().unwrap());
    let alt_click_point = at(pixel.0, pixel.1);
    c.pointer(
        PointerKind::Down,
        Button::Left,
        alt_click_point.0,
        alt_click_point.1,
        ALT,
    );
    c.pointer(
        PointerKind::Up,
        Button::Left,
        alt_click_point.0,
        alt_click_point.1,
        ALT,
    );
    assert_eq!(c.brush_source(), Some(picked), "Alt + click picks");
    assert_eq!(
        selected_row(&c),
        picked as usize,
        "Alt + click moves the wash"
    );
}

#[test]
fn legend_has_no_mutation_affordances() {
    let (mut c, _) = open_editor("legend_no_mutation");
    let before = c.images_panel();
    c.toggle_frame_excluded(0);
    c.remove_frame(0);
    c.move_frame(0, 1);
    c.set_frame_sort(FrameSort::Name);
    assert!(
        matches!(
            c.add_frames(&[PathBuf::from("/tmp/does-not-matter.tif")]),
            Err(focusforge_gui::entry::ImportRefused::NotStacking)
        ),
        "editor mode refuses an add outright"
    );
    assert_eq!(
        c.images_panel(),
        before,
        "no mutation reaches the legend in editor mode"
    );
    assert!(c.pending_frame_change().is_none());
}

/// The `Composite` overlay's draw for `sources` fused sources at `opacity`.
fn overlay_for(sources: u32, opacity: f32) -> OverlayDraw {
    OverlayDraw {
        table: focusforge_core::zmap::overlay_table(sources, None),
        alpha: focusforge_core::display::alpha_from_opacity(opacity),
    }
}

#[test]
fn overlay_draws_in_stacking_on_a_fused_tab() {
    let mut c = controller_with_scratch("overlay_stacking");
    open_fused_tab(&mut c, "overlay_stacking", 3);
    assert!(c.set_mode(ShellMode::Editor));
    c.set_overlay_opacity_fraction(0.3);
    c.set_overlay_on(true);
    let in_editor = c.overlay_draw();

    assert!(c.set_mode(ShellMode::Stacking));
    assert_eq!(c.mode(), ShellMode::Stacking);
    assert!(c.overlay_available());
    assert_eq!(c.overlay_draw(), Some(overlay_for(3, 0.3)));
    assert_eq!(
        c.overlay_draw(),
        in_editor,
        "stacking draws what editor mode draws for the same state"
    );
}

#[test]
fn overlay_unavailable_without_a_finished_fuse() {
    let mut c = controller_with_scratch("overlay_unavailable");
    c.set_overlay_on(true);
    assert!(!c.overlay_available(), "no tab");
    assert_eq!(c.overlay_draw(), None, "no tab, toggle on");

    open_mantis64_tab(&mut c, "overlay_unavailable", 3);
    assert_eq!(c.checkpoint_count(), Some(0));
    assert!(!c.overlay_available(), "frames added, never fused");
    assert_eq!(c.overlay_draw(), None, "no checkpoint, toggle on");

    assert!(c.start_fuse());
    assert!(!c.overlay_available(), "the first fuse is still running");
    assert_eq!(c.overlay_draw(), None, "first fuse running, toggle on");
    settle_all(&mut c);
    assert!(c.overlay_available(), "the fuse finished");
    assert!(c.overlay_draw().is_some());
}

#[test]
fn overlay_follows_the_active_tab() {
    let mut c = controller_with_scratch("overlay_tabs");
    open_fused_tab(&mut c, "overlay_tabs_fused", 3);
    assert!(c.set_mode(ShellMode::Stacking));
    c.set_overlay_on(true);
    assert_eq!(c.overlay_draw(), Some(overlay_for(3, 0.65)));

    open_mantis64_tab(&mut c, "overlay_tabs_unfused", 2);
    assert_eq!(c.active_project(), Some(1));
    assert!(!c.overlay_available());
    assert_eq!(c.overlay_draw(), None, "the unfused tab draws nothing");
    assert!(c.overlay_view().on, "the toggle is untouched");

    c.select_project(0);
    assert!(c.overlay_available());
    assert_eq!(
        c.overlay_draw(),
        Some(overlay_for(3, 0.65)),
        "back on the fused tab the overlay draws again"
    );
}

#[test]
fn overlay_counts_fused_sources_not_frames() {
    let mut c = controller_with_scratch("overlay_sources");
    open_fused_tab(&mut c, "overlay_sources", 2);
    assert!(c.set_mode(ShellMode::Stacking));
    c.set_overlay_on(true);

    common::add_and_settle(&mut c, &[common::fixture_frame("mantis-64", 2)])
        .expect("the add must not refuse");
    assert!(c.pending_frame_change().is_some(), "a checkpoint exists");
    assert!(c.confirm_frame_change());
    assert_eq!(c.images_panel().rows.len(), 3, "the frame was added");
    assert_eq!(
        c.overlay_draw(),
        Some(overlay_for(2, 0.65)),
        "the table stays sized by the fused sources"
    );
}

#[test]
fn overlay_controls_match_the_render_parameters() {
    let mut c = controller_with_scratch("overlay_params");
    open_fused_tab(&mut c, "overlay_params", 3);
    assert!(c.set_mode(ShellMode::Stacking));
    assert_eq!(c.mode(), ShellMode::Stacking);
    assert_eq!(
        c.overlay_draw(),
        None,
        "stacking mode draws nothing until the toggle is on"
    );
    assert!(
        !c.overlay_view().on,
        "a fresh session opens with the overlay off"
    );
    assert_eq!(c.overlay_view().opacity, 0.65, "65% once turned on");

    assert!(c.set_mode(ShellMode::Editor));
    assert_eq!(
        c.overlay_draw(),
        None,
        "the fresh-session default stays off in editor mode too"
    );

    c.set_overlay_opacity_fraction(0.3);
    assert_eq!(c.overlay_draw(), None, "still off");

    c.set_overlay_on(true);
    assert_eq!(
        c.overlay_draw(),
        Some(OverlayDraw {
            table: focusforge_core::zmap::overlay_table(3, None),
            alpha: focusforge_core::display::alpha_from_opacity(0.3),
        }),
        "matches focusforge-cli retouch --overlay --opacity's own blend, with no --solo"
    );

    c.set_overlay_on(false);
    assert_eq!(c.overlay_draw(), None, "turning it back off");
}

#[test]
fn cursor_geometry_follows_radius_softness_and_zoom() {
    let (mut c, _) = open_editor("brush_cursor");
    c.set_brush_size_fraction(0.5);
    c.set_brush_softness_fraction(0.25);
    let radius = c.brush_toolbar_view().size / 2.0;
    for scale in [4.0f32, 5.0] {
        if scale == 5.0 {
            c.zoom_in();
        }
        let draw = c.plane_draw().unwrap();
        assert_eq!(draw.dst.2 / draw.src.2 as f32, scale);
        let view = c.brush_cursor_view().unwrap();
        let ring = radius * scale;
        let core = radius * 0.75 * scale;
        assert_eq!(view.extent, 2.0 * ring + 2.0);
        assert!(
            view.ring_commands
                .contains(&format!("A {ring:.2} {ring:.2}")),
            "{}",
            view.ring_commands
        );
        assert!(view.ring_commands.matches('M').count() > 1, "dashed");
        assert!(
            view.core_commands
                .contains(&format!("A {core:.2} {core:.2}")),
            "{}",
            view.core_commands
        );
    }

    c.set_brush_softness_fraction(0.0);
    let view = c.brush_cursor_view().unwrap();
    let ring = radius * 5.0;
    assert!(view.ring_commands.is_empty(), "no ring at softness 0");
    assert_eq!(view.core_commands.matches('M').count(), 1);
    assert!(
        view.core_commands
            .contains(&format!("A {ring:.2} {ring:.2}"))
    );

    c.set_brush_softness_fraction(0.5);
    c.pointer(
        PointerKind::Move,
        Button::Left,
        RIGHT.x + 20.0,
        60.0,
        NO_MODS,
    );
    let here = c.brush_cursor_view().unwrap();
    c.pointer(
        PointerKind::Move,
        Button::Left,
        RIGHT.x + 190.0,
        250.0,
        NO_MODS,
    );
    assert_eq!(
        c.brush_cursor_view().unwrap(),
        here,
        "the pointer moves nothing"
    );
}

#[test]
fn pipette_reads_and_appends_nothing() {
    let mut c = controller_with_scratch("brush_pipette");
    // Fused with a decoy tab active at settle time, so the auto-entry into
    // Editor a commit now triggers skips tab 0 (it isn't the viewed tab) --
    // its edit session must open fresh below, after the Z-Map is rewritten,
    // not stale from commit time.
    open_mantis64_tab(&mut c, "brush_pipette", 3);
    assert!(c.start_fuse());
    open_mantis64_tab(&mut c, "brush_pipette_decoy", 2);
    settle_all(&mut c);
    c.select_project(0);
    assert_eq!(c.checkpoint_count(), Some(1), "brush_pipette must fuse");
    assert_eq!(
        c.mode(),
        ShellMode::Stacking,
        "tab 0 was not the viewed tab at settle"
    );
    // An 8 x 8 block of `NO_SOURCE` in the sealed map's top-left corner.
    let zmap = c.held_checkpoints().last().unwrap().zmap.clone();
    let info = ZMapReader::open(&zmap).unwrap().info();
    let (width, height) = (info.width(), info.height());
    let mut sealed = read_whole_zmap(&zmap);
    for y in 0..8 {
        for x in 0..8 {
            sealed[(y * width + x) as usize] = NO_SOURCE;
        }
    }
    let map = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut writer = ZMapWriter::create(&zmap, info).unwrap();
    writer.write_tile(map, &sealed).unwrap();
    writer.finish().unwrap();
    c.set_pane_geometry(LEFT, RIGHT);
    assert!(c.set_mode(ShellMode::Editor));

    set_size(&mut c, 4.0);
    c.select_frame(1);
    drag(&mut c, &[(40.0, 40.0), (50.0, 44.0)]);
    c.select_frame(2);
    drag(&mut c, &[(20.0, 50.0), (30.0, 55.0)]);
    c.undo();
    let state = |c: &AppController| (ops(c), indices(c), c.edit_session().unwrap().can_redo());
    let before = state(&c);
    assert!(before.2, "one stroke waits on the redo stack");
    let edited = before.1.clone();
    let differing = |source: u16| {
        (8..height)
            .flat_map(|y| (8..width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                let index = edited[(y * width + x) as usize];
                index != source && index != NO_SOURCE
            })
            .map(|(x, y)| ((x as f32, y as f32), edited[(y * width + x) as usize]))
            .expect("the fused map names more than one source")
    };
    let off_image = (RIGHT.x + 100.0, 8.0);

    c.toggle_pipette();
    assert!(c.brush_toolbar_view().picking);
    let (pixel, picked) = differing(c.brush_source().unwrap());
    let events = record_events(&mut c);
    drag(&mut c, &[pixel]);
    assert_eq!(c.brush_source(), Some(picked), "the armed click picks");
    assert!(!c.brush_toolbar_view().picking, "and disarms the pipette");
    assert!(
        events.lock().unwrap().contains(&ShellEvent::EditorChanged),
        "the toolbar hears the disarm"
    );
    assert_eq!(state(&c), before);
    c.toggle_pipette();
    drag(&mut c, &[(30.0, 30.0), (40.0, 36.0), (50.0, 30.0)]);
    assert_eq!(state(&c), before, "a drag with the pipette armed");
    assert!(!c.brush_toolbar_view().picking);
    let source = c.brush_source();
    c.toggle_pipette();
    drag(&mut c, &[(3.0, 3.0)]);
    assert_eq!(c.brush_source(), source, "NO_SOURCE under the pointer");
    assert!(!c.brush_toolbar_view().picking, "a missed pick disarms too");
    c.toggle_pipette();
    press(&mut c, off_image);
    release(&mut c, off_image);
    assert_eq!(c.brush_source(), source, "a point off the image");
    assert!(!c.brush_toolbar_view().picking);

    let alt_click = |c: &mut AppController, point: (f32, f32)| {
        c.pointer(PointerKind::Down, Button::Left, point.0, point.1, ALT);
        c.pointer(PointerKind::Up, Button::Left, point.0, point.1, ALT);
    };
    let (pixel, picked) = differing(c.brush_source().unwrap());
    alt_click(&mut c, at(pixel.0, pixel.1));
    assert_eq!(c.brush_source(), Some(picked), "Alt + click picks");
    assert!(!c.brush_toolbar_view().picking, "without arming");
    c.toggle_pipette();
    let (pixel, picked) = differing(c.brush_source().unwrap());
    alt_click(&mut c, at(pixel.0, pixel.1));
    assert_eq!(c.brush_source(), Some(picked));
    assert!(
        c.brush_toolbar_view().picking,
        "Alt + click leaves an armed pipette armed"
    );
    c.toggle_pipette();
    alt_click(&mut c, at(3.0, 3.0));
    alt_click(&mut c, off_image);
    assert_eq!(c.brush_source(), Some(picked));
    assert_eq!(state(&c), before);

    c.key(true, "z", CTRL);
    assert!(
        ops(&c).is_empty(),
        "Ctrl + Z undoes the stroke before the picks"
    );
    assert_eq!(c.brush_source(), Some(picked), "and leaves the pick");

    c.toggle_pipette();
    let (pixel, picked) = differing(c.brush_source().unwrap());
    drag(&mut c, &[pixel]);
    assert!(ops(&c).is_empty(), "the pick opens no stroke");
    drag(&mut c, &[(40.0, 40.0), (50.0, 44.0)]);
    assert!(
        matches!(
            ops(&c)[..],
            [Op::BrushStroke { source_id, .. }] if source_id == picked
        ),
        "the next drag paints with the picked source: {:?}",
        ops(&c)
    );
}

#[test]
fn held_alt_shows_the_pipette_cursor() {
    let (mut c, _) = open_editor("brush_alt_cursor");
    let hover = |c: &mut AppController, mods: Modifiers| {
        let (x, y) = at(20.0, 30.0);
        c.pointer(PointerKind::Move, Button::Left, x, y, mods);
        c.brush_cursor_view().unwrap()
    };
    let idle = hover(&mut c, NO_MODS);
    assert!(!idle.picking);

    let held = hover(&mut c, ALT);
    assert!(held.picking, "Alt held shows the pipette");
    assert!(!c.brush_toolbar_view().picking, "without arming it");
    assert_eq!(hover(&mut c, NO_MODS), idle, "Alt released");

    hover(&mut c, ALT);
    c.clear_input_modifiers();
    assert_eq!(
        c.brush_cursor_view().unwrap(),
        idle,
        "focus loss clears Alt"
    );

    c.toggle_pipette();
    assert_eq!(hover(&mut c, NO_MODS), held, "the armed pipette, same view");
    assert_eq!(hover(&mut c, ALT), held);
}

// ---------------------------------------------------------------------
// The mirrored cursor: both panes' own `BrushCursorView.mirror_dx`.
// ---------------------------------------------------------------------

/// The image coordinate `point` (a screen position) maps to through `draw`,
/// the own `PaneDraw` of the pane `pane` places it over -- the inverse of
/// `Viewport::draw`'s forward projection, read pane-locally as `image-x`/
/// `image-y` are (`FFPane`'s `Image` sits inside its own pane's rect).
fn image_coord(draw: PaneDraw, pane: PaneRect, point: (f32, f32)) -> (f64, f64) {
    let local = (point.0 - pane.x, point.1 - pane.y);
    (
        f64::from(draw.src.0)
            + f64::from(local.0 - draw.dst.0) * f64::from(draw.src.2) / f64::from(draw.dst.2),
        f64::from(draw.src.1)
            + f64::from(local.1 - draw.dst.1) * f64::from(draw.src.3) / f64::from(draw.dst.3),
    )
}

/// Hovers `point` (over pane `home`) and proves the mirrored centre --
/// `point` shifted by `mirror_dx`, signed towards `other` -- lands inside
/// `other` and maps, through `other`'s own `PaneDraw`, to the same image
/// coordinate `home`'s own `PaneDraw` gives `point` itself.
fn assert_scene_point_matches(
    c: &mut AppController,
    home: PaneRect,
    other: PaneRect,
    point: (f32, f32),
    case: &str,
) {
    c.pointer(PointerKind::Move, Button::Left, point.0, point.1, NO_MODS);
    let mirror_dx = c.brush_cursor_view().unwrap().mirror_dx;
    let sign = if other.x > home.x { 1.0 } else { -1.0 };
    let mirrored = (point.0 + sign * mirror_dx, point.1);
    assert!(
        mirrored.0 >= other.x
            && mirrored.0 <= other.x + other.width
            && mirrored.1 >= other.y
            && mirrored.1 <= other.y + other.height,
        "{case}: mirrored centre {mirrored:?} lies outside {other:?}"
    );
    let (home_draw, other_draw) = if home.x < other.x {
        (c.frame_draw().unwrap(), c.plane_draw().unwrap())
    } else {
        (c.plane_draw().unwrap(), c.frame_draw().unwrap())
    };
    let home_point = image_coord(home_draw, home, point);
    let other_point = image_coord(other_draw, other, mirrored);
    assert!(
        (home_point.0 - other_point.0).abs() < 1e-2 && (home_point.1 - other_point.1).abs() < 1e-2,
        "{case}: home pane reads {home_point:?}, mirrored pane reads {other_point:?}"
    );
}

/// At fit, one zoom step in, after a pan and with a wider gutter, hovering
/// either pane places the other pane's mirrored centre over the same scene
/// point -- proven by mapping both back to image space through each pane's
/// own `PaneDraw`, so a hard-coded offset (rather than one following the
/// installed rects) would fail once the gutter widens.
#[test]
fn mirrored_cursor_tracks_the_same_scene_point() {
    let (mut c, _) = open_editor("mirrored_cursor_scene_point");
    let points = [(10.0_f32, 12.0_f32), (40.0, 45.0)];
    let mut right = RIGHT;

    let check_stage = |c: &mut AppController, right: PaneRect, stage: &str| {
        for &(ix, iy) in &points {
            assert_scene_point_matches(
                c,
                right,
                LEFT,
                at_pane(right, ix, iy),
                &format!("{stage}, hovering the right pane at ({ix}, {iy})"),
            );
            assert_scene_point_matches(
                c,
                LEFT,
                right,
                at_pane(LEFT, ix, iy),
                &format!("{stage}, hovering the left pane at ({ix}, {iy})"),
            );
        }
    };

    check_stage(&mut c, right, "at fit");

    c.zoom_in();
    check_stage(&mut c, right, "one zoom step in");

    c.wheel(0.0, 60.0, 0.0, 0.0, SHIFT);
    check_stage(&mut c, right, "after a pan");

    right = PaneRect {
        x: RIGHT.x + 16.0,
        ..RIGHT
    };
    c.set_pane_geometry(LEFT, right);
    check_stage(&mut c, right, "a wider gutter");
}

/// The one `BrushCursorView` carries both cursors' geometry: size, softness
/// and zoom change its extent/ring/core, but `mirror_dx` follows only the
/// installed pane rects, not the brush or the pointer.
#[test]
fn mirrored_cursor_shares_geometry() {
    let (mut c, _) = open_editor("mirrored_cursor_shares_geometry");
    let before = c.brush_cursor_view().unwrap();

    set_size(&mut c, 8.0);
    let after_size = c.brush_cursor_view().unwrap();
    assert_ne!(
        after_size.extent, before.extent,
        "size changes the shared geometry"
    );
    assert_eq!(
        after_size.mirror_dx, before.mirror_dx,
        "mirror_dx is independent of brush size"
    );

    c.set_brush_softness_fraction(0.75);
    let after_soft = c.brush_cursor_view().unwrap();
    assert_ne!(
        after_soft.core_commands, after_size.core_commands,
        "softness changes the core"
    );
    assert_eq!(
        after_soft.mirror_dx, before.mirror_dx,
        "mirror_dx is independent of softness"
    );

    c.zoom_in();
    let after_zoom = c.brush_cursor_view().unwrap();
    assert_ne!(
        after_zoom.extent, after_soft.extent,
        "zoom changes the shared geometry"
    );
    assert_eq!(
        after_zoom.mirror_dx, before.mirror_dx,
        "mirror_dx is independent of zoom"
    );

    let (x, y) = at(5.0, 5.0);
    c.pointer(PointerKind::Move, Button::Left, x, y, NO_MODS);
    assert_eq!(
        c.brush_cursor_view().unwrap(),
        after_zoom,
        "the pointer moves neither the shared geometry nor mirror_dx"
    );
}

/// With the pipette armed, the left pane's hover and the right pane's hover
/// at that point shifted by `mirror_dx` preview the same colour, the one an
/// Alt + click at the right-pane point then commits; none of arming,
/// hovering both panes or the commit itself appends a stroke.
#[test]
fn mirrored_cursor_marks_the_pipette_point() {
    let (mut c, _) = open_editor("mirrored_cursor_pipette");
    let before = ops(&c);
    assert!(before.is_empty(), "a fresh tab has no ops yet");

    let left_point = at_pane(LEFT, 29.0, 30.0);
    c.toggle_pipette();
    c.pointer(
        PointerKind::Move,
        Button::Left,
        left_point.0,
        left_point.1,
        NO_MODS,
    );
    let left_hover = c.brush_cursor_view().unwrap();
    assert!(
        left_hover.picking,
        "hovering the left pane previews the pick while armed"
    );

    let right_point = (left_point.0 + left_hover.mirror_dx, left_point.1);
    c.pointer(
        PointerKind::Move,
        Button::Left,
        right_point.0,
        right_point.1,
        NO_MODS,
    );
    let right_hover = c.brush_cursor_view().unwrap();
    assert_eq!(
        right_hover.pick_color, left_hover.pick_color,
        "the left point and its mirrored right point preview the same colour"
    );
    assert_eq!(
        ops(&c),
        before,
        "arming and hovering both panes appends nothing"
    );

    c.pointer(
        PointerKind::Down,
        Button::Left,
        right_point.0,
        right_point.1,
        ALT,
    );
    c.pointer(
        PointerKind::Up,
        Button::Left,
        right_point.0,
        right_point.1,
        ALT,
    );
    let picked = c.brush_source().expect("Alt + click commits a pick");
    assert_eq!(
        left_hover.pick_color,
        job::source_color(picked, c.frame_count().unwrap()),
        "the preview matches the source the click commits"
    );
    assert_eq!(ops(&c), before, "the pick itself opens no stroke");
}

/// `brush_cursor_view` is `None` with no active tab, and stays `None` once a
/// tab is active but before its first `set_pane_geometry` -- the one call
/// that both fits the viewport and installs the panes, so a fresh tab covers
/// the unfitted-viewport case and the no-panes-installed case at once.
#[test]
fn mirrored_cursor_absent_without_geometry() {
    let mut c = controller_with_scratch("mirrored_cursor_absent");
    assert!(c.brush_cursor_view().is_none(), "no active tab");

    open_fused_tab(&mut c, "mirrored_cursor_absent", 3);
    assert!(
        c.brush_cursor_view().is_none(),
        "an active tab before its first set_pane_geometry"
    );
}

#[test]
fn undo_redo_bindings_and_silent_discard() {
    let (mut c, root) = open_editor("brush_undo_redo");
    let sealed = Sealed::read(&mut c, &root);
    set_size(&mut c, 6.0);
    c.select_frame(1);
    drag(&mut c, &[(10.0, 10.0), (30.0, 20.0)]);
    c.select_frame(2);
    drag(&mut c, &[(20.0, 40.0), (44.0, 30.0)]);
    let both = ops(&c);
    let after_both = indices(&c);

    for (undo_key, redo_key) in [("z", "Z"), ("Z", "z")] {
        c.key(true, undo_key, CTRL);
        assert_eq!(ops(&c), both[..1], "undo as {undo_key:?}");
        assert!(c.brush_toolbar_view().can_redo);
        c.key(true, redo_key, CTRL_SHIFT);
        assert_eq!(ops(&c), both, "redo as {redo_key:?}");
        assert_eq!(indices(&c), after_both);
    }

    c.key(true, "z", CTRL);
    c.key(true, "z", CTRL);
    assert_eq!(indices(&c), sealed.indices, "undo is exact");
    assert!(
        sealed.displayed(&c) == sealed.composite,
        "the original composite"
    );
    c.key(true, "z", CTRL_SHIFT);
    assert_eq!(ops(&c), both[..1]);
    assert!(c.edit_session().unwrap().can_redo());

    let events = record_events(&mut c);
    drag(&mut c, &[(12.0, 50.0), (20.0, 54.0)]);
    assert!(
        !c.edit_session().unwrap().can_redo(),
        "a new stroke drops redo"
    );
    assert!(!c.brush_toolbar_view().can_redo);
    let said: Vec<ShellEvent> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| !matches!(event, ShellEvent::Input(_)))
        .cloned()
        .collect();
    assert_eq!(
        said,
        vec![ShellEvent::EditorChanged],
        "nothing announces it"
    );

    let now = (ops(&c), indices(&c));
    c.key(true, "z", CTRL_SHIFT);
    assert_eq!((ops(&c), indices(&c)), now, "nothing left to redo");
    sealed.assert_shown(&c, "after the new stroke");
}

#[test]
fn stroke_commit_recomposites_its_tick_rects() {
    let (mut c, root) = open_editor("brush_commit");
    let sealed = Sealed::read(&mut c, &root);
    set_size(&mut c, 20.0);
    c.set_brush_softness_fraction(0.3);
    c.select_frame(1);
    let radius = c.brush_toolbar_view().size / 2.0;
    let margin = (radius / 8.0).ceil() as u32 + 2;
    let info = c.edit_session().unwrap().info();

    // Zig-zag runs of about 1.7 px, shorter than the resample spacing
    // (radius / 4 = 2.5 px), so the stored path cuts every corner, drifting
    // diagonally across several 16 px tiles.
    let zigzag: Vec<(f32, f32)> = (0..60)
        .map(|i| {
            let t = i as f32;
            (6.0 + 0.6 * t, 20.0 + 0.4 * t + (i % 2) as f32 * 1.6)
        })
        .collect();
    let screen: Vec<(f32, f32)> = zigzag.iter().map(|&(x, y)| at(x, y)).collect();
    press(&mut c, screen[0]);
    let mut ticks = Vec::new();
    for chunk in screen[1..].chunks(7) {
        for &(x, y) in chunk {
            c.pointer_moved(x, y);
        }
        c.flush_live_stroke();
        let tick = c.take_recomposited();
        assert_eq!(tick.len(), 1, "one recomposite per flush");
        ticks.extend(tick);
    }
    release(&mut c, *screen.last().unwrap());

    let commit = c.take_recomposited();
    let map = map_rect(&c);
    let expected: Vec<Rect> = ticks.iter().map(|&r| dilate(r, margin, map)).collect();
    assert_eq!(commit, expected, "one pass over the dilated tick rects");
    let op = ops(&c).pop().unwrap();
    let bbox = footprint(&op, info).unwrap().unwrap();
    assert!(!commit.contains(&bbox), "never the stroke's bounding box");
    sealed.assert_shown(&c, "after release");

    c.undo();
    assert_eq!(
        c.take_recomposited(),
        vec![bbox],
        "undo recomposites its region"
    );
    sealed.assert_shown(&c, "after undo");
    assert!(sealed.displayed(&c) == sealed.composite);
    c.redo();
    assert_eq!(
        c.take_recomposited(),
        vec![bbox],
        "redo recomposites its region"
    );
    sealed.assert_shown(&c, "after redo");
}

#[test]
fn every_edit_has_a_cli_equivalent() {
    let (mut c, _) = open_editor("brush_cli");
    set_size(&mut c, 6.0);
    c.select_frame(1);
    drag(&mut c, &[(10.0, 10.0), (30.0, 25.0), (12.0, 40.0)]);
    c.select_frame(2);
    drag(&mut c, &[(45.0, 45.0)]);
    c.undo();
    c.select_frame(0);
    drag(&mut c, &[(50.0, 8.0), (20.0, 58.0)]);
    let ops = ops(&c);
    assert_eq!(ops.len(), 2);

    let (_, zmap) = c.materialize_active_tab_planes().unwrap();
    let info = ZMapReader::open(&zmap).unwrap().info();
    let mut replay = EditSession::new(info, map_rect(&c), read_whole_zmap(&zmap)).unwrap();
    for op in &ops {
        replay.append(op.clone()).unwrap();
    }
    assert_eq!(replay.indices(), c.edit_session().unwrap().indices());

    let json = serde_json::to_string(&ops).unwrap();
    let parsed: Vec<Op> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, ops);
}

#[test]
fn live_stroke_previews_each_new_segment() {
    let (mut c, root) = open_editor("brush_live");
    let sealed = Sealed::read(&mut c, &root);
    set_size(&mut c, 10.0);
    c.set_brush_softness_fraction(0.3);
    c.select_frame(2);
    drag(&mut c, &[(50.0, 10.0), (56.0, 14.0)]);
    drag(&mut c, &[(8.0, 56.0)]);
    c.undo();
    c.select_frame(1);
    c.take_recomposited();
    let session = |c: &AppController| {
        let s = c.edit_session().unwrap();
        (
            s.op_count(),
            s.ops().cloned().collect::<Vec<_>>(),
            s.can_redo(),
        )
    };
    bind_softness_to_alt_wheel(&mut c);
    let before = session(&c);
    assert!(before.2, "one stroke waits on the redo stack");
    let before_indices = indices(&c);
    let view = c.brush_toolbar_view();
    let (source, radius, softness) = (1u16, view.size / 2.0, view.soft / 100.0);
    let info = c.edit_session().unwrap().info();
    let stroke = |path: Vec<Point>| Op::BrushStroke {
        source_id: source,
        path,
        radius,
        softness,
    };

    // A quarter-pixel trace, some steps sub-pixel, wandering across tiles.
    let trace: Vec<(f32, f32)> = (0..45)
        .map(|i| {
            let t = i as f32;
            let jog = if i % 3 == 0 { 1.25 } else { 0.0 };
            (10.0 + 0.75 * t, 12.0 + 0.5 * (t * 0.5).floor() + jog)
        })
        .collect();
    press(&mut c, at(trace[0].0, trace[0].1));
    c.flush_live_stroke();
    assert!(
        c.take_recomposited().is_empty(),
        "a flush with no motion recomposites nothing"
    );

    let mut flushed = 0;
    for (k, end) in [4usize, 5, 13, 21, 22, 30, 44].into_iter().enumerate() {
        for &(x, y) in &trace[flushed + 1..=end] {
            let (sx, sy) = at(x, y);
            c.pointer_moved(sx, sy);
        }
        if k == 2 {
            let held = indices(&c);
            c.key(true, "z", CTRL);
            c.key(true, "z", CTRL_SHIFT);
            assert_eq!(indices(&c), held, "undo/redo mid-drag change nothing");
            assert_eq!(session(&c), before);
            c.wheel(0.0, 60.0, 0.0, 0.0, SHIFT);
            c.wheel(0.0, 60.0, 0.0, 0.0, ALT);
            c.wheel(0.0, 60.0, 0.0, 0.0, CTRL);
            assert_ne!(c.brush_toolbar_view().size, view.size);
            assert_ne!(c.brush_toolbar_view().soft, view.soft);
            assert_ne!(c.brush_source(), Some(source));
        }
        c.flush_live_stroke();
        let segment = stroke(rounded(&trace[flushed..=end]));
        let region = footprint(&segment, info).unwrap();
        assert_eq!(
            c.take_recomposited(),
            region.into_iter().collect::<Vec<_>>(),
            "flush {k}: exactly the new segment's footprint, once"
        );
        let mut painted = before_indices.clone();
        apply(
            &stroke(rounded(&trace[..=end])),
            info,
            sealed.map,
            &mut painted,
        )
        .unwrap();
        assert!(
            indices(&c) == painted,
            "flush {k}: the preview paints the rounded trace so far"
        );
        sealed.assert_shown(&c, &format!("flush {k}"));
        assert_eq!(
            session(&c),
            before,
            "flush {k}: the op list waits for release"
        );
        c.flush_live_stroke();
        assert!(c.take_recomposited().is_empty(), "flush {k}: nothing new");
        flushed = end;
    }

    release(&mut c, at(trace[44].0, trace[44].1));
    let mut live = LiveStroke::new(source, radius, softness, trace[0].0, trace[0].1);
    for w in trace.windows(2) {
        live.push(w[1].0 - w[0].0, w[1].1 - w[0].1);
    }
    let committed = stroke(live.finish());
    let (count, ops, can_redo) = session(&c);
    assert_eq!(count, before.0 + 1, "exactly one op on release");
    assert!(!can_redo);
    assert_eq!(
        ops.last(),
        Some(&committed),
        "the frozen source, radius and softness"
    );
    let mut fresh = EditSession::new(info, sealed.map, before_indices).unwrap();
    fresh.append(committed).unwrap();
    assert!(
        indices(&c) == fresh.indices(),
        "a fresh session with that op"
    );
    sealed.assert_shown(&c, "after release");
}

/// A brush stroke that paints frame `i` recomposites the composite from the
/// frame-as-fused view -- the recomposited pixel under the stroke equals a
/// [`FrameSource`] read of that frame through `selected_frame_view` at the
/// same point.
///
/// [`FrameSource`]: focusforge_gui::canvas::FrameSource
#[test]
fn stroke_matches_left_pane() {
    let (mut c, root) = open_editor("stroke_matches_left_pane");
    c.select_frame(2);
    set_size(&mut c, 8.0);
    drag(&mut c, &[(20.0, 20.0)]);

    let sealed = Sealed::read(&mut c, &root);
    let point = (20 * sealed.map.width + 20) as usize;
    let recomposited = sealed.displayed(&c)[point * 3..point * 3 + 3].to_vec();

    let view = c
        .selected_frame_view()
        .expect("a fused, editable tab's selected frame always has a view");
    let mut source = focusforge_gui::canvas::FrameSource::default();
    source
        .set(
            &c.selected_frame_path().unwrap(),
            Some(view),
            job::host_budget(),
        )
        .expect("the selected frame opens");
    let mut left_pixel = vec![0u16; 3];
    source
        .read_into(
            Rect {
                x: 20,
                y: 20,
                width: 1,
                height: 1,
            },
            NonZeroU32::MIN,
            &mut left_pixel,
        )
        .expect("the left pane's own point reads");

    assert_eq!(
        recomposited, left_pixel,
        "the recomposited pixel must equal the left pane's own read at the same point"
    );
}

// ---------------------------------------------------------------------
// A modal or the Projects overlay hides the brush cursor and blocks the
// canvas beneath it.
// ---------------------------------------------------------------------

/// Raises the one grouping question from two single-frame subfolders --
/// the same ambiguous shape a real folder scan produces, small fixture
/// frames standing in for a user's own subject folders.
fn raise_question(controller: &mut AppController, name: &str) {
    let root = common::tree(&format!("{name}_question"));
    for (subject, frame) in [("subject-a", 0usize), ("subject-b", 1)] {
        let dir = root.join(subject);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(
            common::fixture_frame("mantis-64", frame),
            dir.join("00.tif"),
        )
        .unwrap();
    }
    assert!(matches!(
        common::open_and_settle(controller, &[root]),
        OpenOutcome::Question(..)
    ));
}

/// Raises the "No frames here" refusal from an empty folder -- whether
/// classification refuses it synchronously or a worker settles it, either
/// way [`AppController::last_refusal`] ends up `Some`.
fn raise_refusal(controller: &mut AppController, name: &str) {
    let root = common::tree(&format!("{name}_refusal"));
    if let Some(handle) = controller.open_paths(&[root]) {
        handle.join().expect("the load worker must not panic");
        assert!(matches!(
            controller.settle_open(),
            Some(OpenOutcome::Refused(_))
        ));
    }
    assert!(
        controller.last_refusal().is_some(),
        "an empty folder must refuse"
    );
}

#[test]
fn brush_cursor_hidden_under_every_modal() {
    // Settings.
    {
        let (mut c, _) = open_editor("cursor_settings");
        assert!(c.brush_cursor_view().is_some(), "settings: visible before");
        c.open_settings();
        assert!(c.brush_cursor_view().is_none(), "settings: armed=false");
        c.toggle_pipette();
        assert!(c.brush_cursor_view().is_none(), "settings: armed=true");
        c.toggle_pipette();
        c.close_settings();
        assert!(
            c.brush_cursor_view().is_some(),
            "settings: visible after close"
        );
    }
    // The close guard.
    {
        let (mut c, _) = open_editor("cursor_close_guard");
        assert!(
            c.brush_cursor_view().is_some(),
            "close guard: visible before"
        );
        let index = c.active_project().unwrap();
        c.request_close(index);
        assert!(c.pending_close().is_some());
        assert!(c.brush_cursor_view().is_none(), "close guard: armed=false");
        c.toggle_pipette();
        assert!(c.brush_cursor_view().is_none(), "close guard: armed=true");
        c.toggle_pipette();
        c.cancel_close();
        assert!(
            c.brush_cursor_view().is_some(),
            "close guard: visible after close"
        );
    }
    // The frame-set warning -- raised from Stacking mode (the only mode
    // `remove_frame` accepts), cleared back into Editor before the checks:
    // `brush_cursor_view` has no mode check of its own.
    {
        let (mut c, _) = open_editor("cursor_frame_warning");
        assert!(
            c.brush_cursor_view().is_some(),
            "frame warning: visible before"
        );
        c.set_mode(ShellMode::Stacking);
        c.remove_frame(0);
        assert!(c.pending_frame_change().is_some());
        assert!(c.set_mode(ShellMode::Editor));
        assert!(
            c.brush_cursor_view().is_none(),
            "frame warning: armed=false"
        );
        c.toggle_pipette();
        assert!(c.brush_cursor_view().is_none(), "frame warning: armed=true");
        c.toggle_pipette();
        c.cancel_frame_change();
        assert!(
            c.brush_cursor_view().is_some(),
            "frame warning: visible after close"
        );
    }
    // The grouping question.
    {
        let (mut c, _) = open_editor("cursor_question");
        assert!(c.brush_cursor_view().is_some(), "question: visible before");
        raise_question(&mut c, "cursor_question");
        assert!(c.brush_cursor_view().is_none(), "question: armed=false");
        c.toggle_pipette();
        assert!(c.brush_cursor_view().is_none(), "question: armed=true");
        c.toggle_pipette();
        c.cancel_question();
        assert!(
            c.brush_cursor_view().is_some(),
            "question: visible after close"
        );
    }
    // The refusal dialog.
    {
        let (mut c, _) = open_editor("cursor_refusal");
        assert!(c.brush_cursor_view().is_some(), "refusal: visible before");
        raise_refusal(&mut c, "cursor_refusal");
        assert!(c.brush_cursor_view().is_none(), "refusal: armed=false");
        c.toggle_pipette();
        assert!(c.brush_cursor_view().is_none(), "refusal: armed=true");
        c.toggle_pipette();
        c.dismiss_refusal();
        assert!(
            c.brush_cursor_view().is_some(),
            "refusal: visible after close"
        );
    }
    // The Projects overlay.
    {
        let (mut c, _) = open_editor("cursor_overlay");
        assert!(c.brush_cursor_view().is_some(), "overlay: visible before");
        c.toggle_projects_overlay();
        assert!(c.brush_cursor_view().is_none(), "overlay: armed=false");
        c.toggle_pipette();
        assert!(c.brush_cursor_view().is_none(), "overlay: armed=true");
        c.toggle_pipette();
        c.toggle_projects_overlay();
        assert!(
            c.brush_cursor_view().is_some(),
            "overlay: visible after close"
        );
    }
    // Keep/Discard divergence, on a Stacking-mode tab: no test hook parks a
    // divergent run on a tab already holding a checkpoint, and
    // `brush_cursor_view` has no mode check either way.
    {
        let mut c = controller_on_mantis_512("cursor_divergence");
        c.set_pane_geometry(LEFT, RIGHT);
        assert!(
            c.brush_cursor_view().is_some(),
            "divergence: visible before"
        );
        assert!(c.start_fuse());
        settle_all(&mut c);
        assert!(c.pending_divergence().is_some());
        assert!(c.brush_cursor_view().is_none(), "divergence: armed=false");
        c.toggle_pipette();
        assert!(c.brush_cursor_view().is_none(), "divergence: armed=true");
        c.toggle_pipette();
        assert!(c.keep_fused().is_some());
        assert!(c.pending_divergence().is_none());
        // `keep_fused` clears the viewport for a refit against the new
        // checkpoint's own extent -- `fit_if_unfitted` is what every real
        // repaint runs before drawing a cursor over it.
        c.fit_if_unfitted();
        assert!(
            c.brush_cursor_view().is_some(),
            "divergence: visible after Keep"
        );
    }
}

/// Records everything a canvas-blocking modal must leave untouched: zoom,
/// pan, the selected source and the pipette's armed state.
fn assert_wheel_key_pipette_blocked(c: &mut AppController, case: &str) {
    let draw = c.plane_draw();
    c.wheel(0.0, 60.0, 0.0, 0.0, NO_MODS);
    assert_eq!(
        c.plane_draw(),
        draw,
        "{case}: a plain wheel must not zoom or pan"
    );

    let source = c.brush_source();
    c.wheel(0.0, 60.0, 0.0, 0.0, CTRL);
    assert_eq!(
        c.brush_source(),
        source,
        "{case}: Ctrl + wheel must not step the source"
    );

    let ops_before = c.edit_session().map(|session| session.ops().count());
    c.key(true, "z", CTRL);
    c.key(true, "Z", CTRL_SHIFT);
    assert_eq!(
        c.edit_session().map(|session| session.ops().count()),
        ops_before,
        "{case}: Ctrl + Z / Ctrl + Shift + Z must step nothing"
    );

    c.toggle_pipette();
    assert!(c.brush_toolbar_view().picking);
    press(c, at(30.0, 30.0));
    release(c, at(30.0, 30.0));
    assert!(
        c.brush_toolbar_view().picking,
        "{case}: a blocked click must not disarm the pipette"
    );
    c.toggle_pipette();
}

/// A brush drag over either pane, blocked: neither pane's press starts a
/// stroke, so the edit session's own op count and index buffer -- the
/// in-memory twin of the Z-Map a commit would write -- stay exactly as
/// they were.
fn assert_drag_blocked(c: &mut AppController, case: &str) {
    let before_ops = ops(c);
    let before_indices = indices(c);
    press(c, at_pane(LEFT, 10.0, 10.0));
    c.pointer_moved(at_pane(LEFT, 20.0, 15.0).0, at_pane(LEFT, 20.0, 15.0).1);
    release(c, at_pane(LEFT, 20.0, 15.0));
    press(c, at(10.0, 10.0));
    c.pointer_moved(at(20.0, 15.0).0, at(20.0, 15.0).1);
    release(c, at(20.0, 15.0));
    assert_eq!(ops(c), before_ops, "{case}: a blocked drag must add no op");
    assert_eq!(
        indices(c),
        before_indices,
        "{case}: a blocked drag must not touch the Z-Map"
    );
}

#[test]
fn canvas_input_blocked_under_every_modal() {
    {
        let (mut c, _) = open_editor("blocked_settings");
        c.open_settings();
        assert_drag_blocked(&mut c, "settings");
        assert_wheel_key_pipette_blocked(&mut c, "settings");
        c.close_settings();
    }
    {
        let (mut c, _) = open_editor("blocked_close_guard");
        let index = c.active_project().unwrap();
        c.request_close(index);
        assert!(c.pending_close().is_some());
        assert_drag_blocked(&mut c, "close guard");
        assert_wheel_key_pipette_blocked(&mut c, "close guard");
        c.cancel_close();
    }
    {
        let (mut c, _) = open_editor("blocked_frame_warning");
        c.set_mode(ShellMode::Stacking);
        c.remove_frame(0);
        assert!(c.pending_frame_change().is_some());
        assert!(c.set_mode(ShellMode::Editor));
        assert_drag_blocked(&mut c, "frame warning");
        assert_wheel_key_pipette_blocked(&mut c, "frame warning");
        c.cancel_frame_change();
    }
    {
        let (mut c, _) = open_editor("blocked_question");
        raise_question(&mut c, "blocked_question");
        assert_drag_blocked(&mut c, "grouping question");
        assert_wheel_key_pipette_blocked(&mut c, "grouping question");
        c.cancel_question();
    }
    {
        let (mut c, _) = open_editor("blocked_refusal");
        raise_refusal(&mut c, "blocked_refusal");
        assert_drag_blocked(&mut c, "refusal");
        assert_wheel_key_pipette_blocked(&mut c, "refusal");
        c.dismiss_refusal();
    }
    {
        let (mut c, _) = open_editor("blocked_overlay");
        c.toggle_projects_overlay();
        assert_drag_blocked(&mut c, "projects overlay");
        assert_wheel_key_pipette_blocked(&mut c, "projects overlay");
        c.toggle_projects_overlay();
    }
    {
        let mut c = controller_on_mantis_512("blocked_divergence");
        c.set_pane_geometry(LEFT, RIGHT);
        assert!(c.start_fuse());
        settle_all(&mut c);
        assert!(c.pending_divergence().is_some());
        assert_wheel_key_pipette_blocked(&mut c, "divergence");
        assert!(c.keep_fused().is_some());
    }
}

#[test]
fn modal_opening_mid_stroke_leaves_no_live_drag() {
    let (mut c, _) = open_editor("modal_mid_stroke");
    set_size(&mut c, 8.0);
    c.select_frame(1);

    // A live stroke, half drawn, then a modal opens mid-drag.
    press(&mut c, at(10.0, 10.0));
    c.pointer_moved(at(20.0, 15.0).0, at(20.0, 15.0).1);
    c.open_settings();
    // Further motion while blocked commits no further stamp.
    c.pointer_moved(at(50.0, 40.0).0, at(50.0, 40.0).1);
    release(&mut c, at(50.0, 40.0));
    assert_eq!(
        ops(&c),
        vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 10, y: 10 }, Point { x: 20, y: 15 }],
            radius: c.brush_toolbar_view().size / 2.0,
            softness: c.brush_toolbar_view().soft_fraction,
        }],
        "the stroke ends on the next pointer-up, with nothing added after the modal opened"
    );

    // A press that arrives while the modal is still open starts no drag,
    // so nothing is left live once it closes.
    let before = ops(&c);
    press(&mut c, at(30.0, 30.0));
    c.pointer_moved(at(35.0, 32.0).0, at(35.0, 32.0).1);
    release(&mut c, at(35.0, 32.0));
    c.close_settings();
    assert_eq!(
        ops(&c),
        before,
        "a press under a modal must start no drag, and closing it must not resume one"
    );
}

#[test]
fn refusal_dialog_counts_as_modal() {
    let mut controller = controller_on_mantis_512("refusal_modal");
    controller.set_align_algorithm("");
    assert!(!controller.modal_open(), "nothing raised yet");
    assert!(!controller.canvas_blocked());

    assert!(!controller.start_fuse());
    assert!(
        controller.last_refusal().is_some_and(|m| !m.is_empty()),
        "the refusal must be reported"
    );
    assert!(
        controller.modal_open(),
        "a raised refusal counts as an open modal"
    );
    assert!(controller.canvas_blocked());

    controller.dismiss_refusal();
    assert!(!controller.modal_open(), "answering clears it");
    assert!(!controller.canvas_blocked());
}

// ---------------------------------------------------------------------
// In-memory checkpoints and the project save.
// ---------------------------------------------------------------------

/// The active tab's current checkpoint row, by its list id.
fn current_row(controller: &AppController) -> Option<String> {
    controller
        .checkpoints_panel()
        .entries
        .iter()
        .find(|entry| entry.selected)
        .and_then(|entry| entry.id.clone())
}

fn row_index(controller: &AppController, id: &str) -> usize {
    controller
        .checkpoints_panel()
        .entries
        .iter()
        .position(|entry| entry.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no row for {id}"))
}

/// A stroke at column `x` that changes the map there: the source the pixel
/// does not show yet.
fn paint(controller: &mut AppController, x: i32) {
    let session = controller
        .edit_session_mut()
        .expect("the tab has an edit session");
    let width = session.tile().width as usize;
    let shown = session.indices()[20 * width + x as usize];
    session
        .append(Op::BrushStroke {
            source_id: u16::from(shown == 0),
            path: vec![Point { x, y: 20 }],
            radius: 4.0,
            softness: 0.0,
        })
        .expect("a dab inside the map applies");
}

/// `set_mode(Editor)`, waiting out the render an op-bearing row needs first.
fn enter_editor(controller: &mut AppController) {
    let start = std::time::Instant::now();
    while !controller.set_mode(ShellMode::Editor) {
        assert!(start.elapsed() < SETTLE, "the editor did not open");
        controller.settle_checkpoint_render();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn reopen(path: &Path, name: &str) -> AppController {
    let mut controller = controller_with_scratch(name);
    assert!(matches!(
        common::open_and_settle(&mut controller, &[path.to_path_buf()]),
        OpenOutcome::Opened(_)
    ));
    controller
}

fn saved(path: &Path) -> Project {
    Project::open(path).expect("the saved project opens")
}

/// `id`'s Z-Map as a saved project materializes it.
fn saved_map(path: &Path, id: &str, name: &str) -> Vec<u16> {
    let mut project = saved(path);
    let checkpoint = project
        .history()
        .get(id)
        .expect("the checkpoint is saved")
        .clone();
    let out = common::tree(name).join("zmap.tif");
    project::materialize_zmap(&mut project, &checkpoint, &out).expect("the Z-Map materializes");
    read_whole_zmap(&out)
}

fn saved_ids(path: &Path) -> Vec<String> {
    saved(path)
        .history()
        .all()
        .iter()
        .map(|c| c.id.clone())
        .collect()
}

/// The one saved checkpoint of `path` that is not in `known`.
fn added_checkpoint(path: &Path, known: &[String]) -> project::Checkpoint {
    let project = saved(path);
    let added: Vec<&project::Checkpoint> = project
        .history()
        .all()
        .into_iter()
        .filter(|c| !known.contains(&c.id))
        .collect();
    assert_eq!(added.len(), 1, "exactly one checkpoint is new");
    added[0].clone()
}

/// Fuses two frames, saves, then paints and saves once more per entry of
/// `strokes`. Returns the project file and its checkpoint ids, oldest first.
fn saved_chain(name: &str, strokes: &[i32]) -> (PathBuf, Vec<String>) {
    let mut controller = controller_with_scratch(name);
    open_fused_tab(&mut controller, &format!("{name}_frames"), 2);
    let path = save_to_proposed(&mut controller);
    let mut chain = vec![saved(&path).checkpoint().id.clone()];
    for &x in strokes {
        paint(&mut controller, x);
        assert!(controller.save_active_project(|_| None));
        chain.push(saved(&path).checkpoint().id.clone());
    }
    (path, chain)
}

#[test]
fn saving_a_modified_live_writes_an_automatic_checkpoint_first() {
    // A never-saved project: fuse, paint, save.
    let mut controller = controller_with_scratch("auto_checkpoint_new");
    open_fused_tab(&mut controller, "auto_checkpoint_new_frames", 2);
    paint(&mut controller, 20);
    let painted = controller.edit_session().unwrap().indices().to_vec();
    let path = save_to_proposed(&mut controller);

    let project = saved(&path);
    assert_eq!(project.history().all().len(), 2, "root plus one child");
    let child = project.checkpoint().clone();
    let root = project
        .history()
        .get(child.parent.as_deref().unwrap())
        .unwrap();
    assert!(root.parent.is_none() && root.ops.is_empty());
    assert_eq!(
        child.ops.len(),
        1,
        "the automatic checkpoint carries the stroke"
    );
    assert_eq!(
        saved_map(&path, &child.id, "auto_checkpoint_new_map"),
        painted
    );
    assert_eq!(current_row(&controller), Some(child.id.clone()));
    let session = controller.edit_session().expect("the session survives");
    assert_eq!(
        (session.op_count(), session.indices()),
        (0, painted.as_slice())
    );
    assert!(!controller.save_enabled());

    // An opened project painted on its older, non-head row.
    let (path, chain) = saved_chain("auto_checkpoint_older", &[20]);
    let mut controller = reopen(&path, "auto_checkpoint_older_reopened");
    assert_eq!(current_row(&controller), Some(chain[1].clone()));
    let root_row = row_index(&controller, &chain[0]);
    controller.select_checkpoint_row(root_row);
    enter_editor(&mut controller);
    paint(&mut controller, 40);
    let painted = controller.edit_session().unwrap().indices().to_vec();
    assert!(controller.save_enabled());
    assert!(controller.save_active_project(|_| None));

    let added = added_checkpoint(&path, &chain);
    assert_eq!(
        added.parent.as_deref(),
        Some(chain[0].as_str()),
        "not the head"
    );
    assert_eq!(
        saved_map(&path, &added.id, "auto_checkpoint_older_map"),
        painted
    );
    assert_eq!(current_row(&controller), Some(added.id));
    let session = controller.edit_session().expect("the session survives");
    assert_eq!(
        (session.op_count(), session.indices()),
        (0, painted.as_slice())
    );
    assert!(!controller.save_enabled());
}

#[test]
fn a_fuse_records_the_current_row_as_its_parent() {
    let (path, chain) = saved_chain("fuse_parent", &[20]);
    let mut controller = reopen(&path, "fuse_parent_reopened");
    controller.set_align_algorithm("dummy-align");
    let root_row = row_index(&controller, &chain[0]);
    controller.select_checkpoint_row(root_row);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert_eq!(controller.held_checkpoints().len(), 1);
    assert!(controller.save_active_project(|_| None));

    let fused = added_checkpoint(&path, &chain);
    assert!(fused.ops.is_empty());
    assert_eq!(
        fused.parent.as_deref(),
        Some(chain[0].as_str()),
        "not the head"
    );
    let project = saved(&path);
    assert_eq!(
        project.history().get(&chain[1]).unwrap().parent.as_deref(),
        Some(chain[0].as_str())
    );
}

#[test]
fn save_is_enabled_only_with_something_to_write() {
    let mut controller = controller_with_scratch("save_enabled_rule");
    open_mantis64_tab(&mut controller, "save_enabled_rule_frames", 2);
    assert!(!controller.save_enabled(), "never fused");

    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert!(controller.save_enabled(), "the first fuse has finished");
    let path = save_to_proposed(&mut controller);
    assert!(!controller.save_enabled(), "nothing left to write");

    let mut opened = reopen(&path, "save_enabled_rule_reopened");
    assert!(!opened.save_enabled(), "an unmodified opened project");
    enter_editor(&mut opened);
    assert!(!opened.save_enabled(), "an open session with no stroke");
    paint(&mut opened, 20);
    assert!(opened.save_enabled(), "a modified live");
    assert!(opened.edit_session_mut().unwrap().undo().is_some());
    assert!(!opened.save_enabled(), "undo brought live back to zero ops");
}

#[test]
fn a_pending_delete_is_applied_by_the_project_save() {
    let (path, chain) = saved_chain("pending_delete", &[20, 40]);
    let b = &chain[2];
    let mut controller = reopen(&path, "pending_delete_reopened");
    assert_eq!(current_row(&controller), Some(b.clone()));
    assert!(!controller.save_enabled());

    assert!(!controller.queue_checkpoint_delete("no-such-checkpoint"));
    assert!(!controller.save_enabled(), "refusals change nothing");

    assert!(controller.queue_checkpoint_delete(b));
    assert!(!controller.queue_checkpoint_delete(b), "already queued");
    assert!(
        controller.save_enabled(),
        "a pending delete is something to write"
    );
    let before = fs::read(&path).unwrap();

    // A save that cannot write keeps the delete and leaves the file alone.
    let mut blocker = path.clone().into_os_string();
    blocker.push(".saving");
    let blocker = PathBuf::from(blocker);
    fs::create_dir(&blocker).unwrap();
    assert!(!controller.save_active_project(|_| None));
    assert_eq!(fs::read(&path).unwrap(), before, "the file is untouched");
    assert!(controller.save_enabled(), "the delete is still queued");
    fs::remove_dir(&blocker).unwrap();

    // The current row's delete leaves it stored hidden and unlisted.
    assert!(controller.save_active_project(|_| None));
    assert_eq!(saved_ids(&path).len(), 3);
    let project = saved(&path);
    assert!(project.history().get(b).unwrap().hidden);
    assert!(project.history().visible().iter().all(|c| &c.id != b));
    assert_eq!(current_row(&controller), None);
    assert!(!controller.save_enabled());

    // The root is deletable; the last listed row is not.
    let (path, chain) = saved_chain("pending_delete_root", &[20, 40]);
    let mut controller = reopen(&path, "pending_delete_root_reopened");
    assert!(controller.queue_checkpoint_delete(&chain[0]));
    assert!(controller.queue_checkpoint_delete(&chain[1]));
    assert!(
        !controller.queue_checkpoint_delete(&chain[2]),
        "the last listed row stays"
    );
    assert!(controller.save_active_project(|_| None));
    assert_eq!(saved_ids(&path).len(), 3);
    let project = saved(&path);
    let visible: Vec<&str> = project
        .history()
        .visible()
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(visible, [chain[2].as_str()]);

    // A delete leaves the edit session open.
    let (path, chain) = saved_chain("pending_delete_session", &[20]);
    let mut controller = reopen(&path, "pending_delete_session_reopened");
    enter_editor(&mut controller);
    assert!(controller.queue_checkpoint_delete(&chain[1]));
    assert!(controller.save_active_project(|_| None));
    assert!(controller.edit_session().is_some());
    assert_eq!(current_row(&controller), None);
    // Live still stands on the deleted row: a stroke lands under it.
    paint(&mut controller, 50);
    assert!(controller.save_active_project(|_| None));
    assert_eq!(
        saved(&path).checkpoint().parent.as_deref(),
        Some(chain[1].as_str())
    );

    // A checkpoint an in-memory entry sits under is deletable; the save
    // writes the entry under it.
    let (path, chain) = saved_chain("pending_delete_entry", &[20, 40]);
    let mut controller = reopen(&path, "pending_delete_entry_reopened");
    enter_editor(&mut controller);
    paint(&mut controller, 30);
    assert!(controller.snapshot_live(0));
    assert!(controller.queue_checkpoint_delete(&chain[1]));
    assert!(controller.save_active_project(|_| None));
    assert_eq!(saved_ids(&path).len(), 4);
    assert!(saved(&path).history().get(&chain[1]).unwrap().hidden);

    // A fuse finishing on a pending-deleted row lands under it; the save
    // hides the row and keeps it.
    let (path, chain) = saved_chain("pending_delete_fuse", &[20]);
    let mut controller = reopen(&path, "pending_delete_fuse_reopened");
    controller.set_align_algorithm("dummy-align");
    assert!(controller.queue_checkpoint_delete(&chain[1]));
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert!(controller.save_active_project(|_| None));
    let project = saved(&path);
    assert_eq!(project.history().all().len(), 3);
    assert!(
        project.history().get(&chain[1]).unwrap().hidden,
        "the delete applied and the row stays stored"
    );
    assert_eq!(
        project.checkpoint().parent.as_deref(),
        Some(chain[1].as_str())
    );
}

#[test]
fn edits_on_a_row_queued_for_delete_are_saved_under_it() {
    let (path, chain) = saved_chain("deleted_origin", &[20, 40]);
    let mut controller = reopen(&path, "deleted_origin_reopened");
    enter_editor(&mut controller);
    assert!(controller.queue_checkpoint_delete(&chain[2]));
    paint(&mut controller, 50);
    let painted = controller.edit_session().unwrap().indices().to_vec();
    assert!(controller.save_active_project(|_| None));

    let project = saved(&path);
    assert!(
        project.history().get(&chain[2]).unwrap().hidden,
        "the origin is hidden and stays stored"
    );
    assert!(project.history().visible().iter().all(|c| c.id != chain[2]));
    assert_eq!(project.history().all().len(), 4);
    let added = project.checkpoint().clone();
    assert_eq!(added.parent.as_deref(), Some(chain[2].as_str()));
    assert!(!added.ops.is_empty(), "an op list over the hidden row");
    assert_eq!(saved_map(&path, &added.id, "deleted_origin_map"), painted);
    assert_eq!(current_row(&controller), Some(added.id));
    assert_eq!(controller.edit_session().unwrap().op_count(), 0);
}

#[test]
fn a_snapshot_during_a_fuse_holds_no_files() {
    let (path, chain) = saved_chain("snapshot_during_fuse", &[20]);
    let mut controller = reopen(&path, "snapshot_during_fuse_reopened");
    controller.set_align_algorithm("dummy-align");
    enter_editor(&mut controller);
    assert!(controller.queue_checkpoint_delete(&chain[1]));

    assert!(controller.start_fuse());
    paint(&mut controller, 40);
    assert!(controller.snapshot_live(0), "an op-bearing entry is taken");
    settle_all(&mut controller);

    let held = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("snapshot_during_fuse_reopened_scratch")
        .join("held");
    let tab_dirs: Vec<_> = fs::read_dir(&held)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(tab_dirs.len(), 1, "one tab holds files");
    assert_eq!(
        fs::read_dir(&tab_dirs[0]).unwrap().count(),
        1,
        "only the fuse owns a directory"
    );
}

#[test]
fn a_save_that_does_not_write_loses_nothing() {
    let mut controller = controller_with_scratch("save_loses_nothing");
    open_fused_tab(&mut controller, "save_loses_nothing_frames", 2);
    paint(&mut controller, 20);
    let painted = controller.edit_session().unwrap().indices().to_vec();

    assert!(!controller.save_active_project(|_| None));
    assert_eq!(
        controller.checkpoint_count(),
        Some(1),
        "no automatic checkpoint"
    );
    assert_eq!(controller.edit_session().unwrap().op_count(), 1);
    assert!(controller.save_enabled());

    let missing = common::tree("save_loses_nothing_target")
        .join("gone")
        .join("p.ffp");
    assert!(!controller.save_active_project(|_| Some(missing.clone())));
    assert!(controller.take_settled_failure().is_some());
    assert_eq!(
        controller.checkpoint_count(),
        Some(2),
        "the automatic checkpoint stays"
    );
    assert_eq!(current_row(&controller), Some("held:1".to_string()));
    let session = controller.edit_session().unwrap();
    assert_eq!(
        (session.op_count(), session.indices()),
        (0, painted.as_slice())
    );
    assert!(controller.save_enabled());

    let path = save_to_proposed(&mut controller);
    assert_eq!(saved_ids(&path).len(), 2, "a retry writes the same history");
}

/// Editing from an older, non-head checkpoint recomposites through that
/// checkpoint's own registration: the head here was fused without alignment
/// or crop, the root with both.
#[test]
fn an_older_checkpoint_edits_through_its_own_registration() {
    let name = "older_registration";
    let mut controller = controller_with_scratch(name);
    let frames = open_fused_tab(&mut controller, name, 3);
    let path = save_to_proposed(&mut controller);
    let mut controller = reopen(&path, "older_registration_reopened");
    controller.set_align_algorithm("pyramidal-ecc");
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    // The real aligner cannot register these frames, so its result waits on
    // the Keep/Discard question.
    assert!(controller.pending_divergence().is_some());
    controller.keep_fused().expect("the parked result is kept");
    assert!(controller.save_active_project(|_| None));

    let project = saved(&path);
    let head = project.checkpoint().clone();
    let root = head.parent.clone().expect("the second fuse is a child");
    let root_registration = project.fuse_state(&root).unwrap().registration;
    let head_registration = project.fuse_state(&head.id).unwrap().registration;
    assert_ne!(
        root_registration, head_registration,
        "the two checkpoints are registered apart"
    );

    controller.set_resident_tile_size_for_test(16);
    controller.set_pane_geometry(LEFT, RIGHT);
    controller.select_checkpoint_row(row_index(&controller, &root));
    controller.fit();
    enter_editor(&mut controller);
    assert_eq!(map_rect(&controller).width, MAP_WIDTH, "the root's own map");
    controller.select_frame(1);
    let view = controller.selected_frame_view().unwrap();
    assert_eq!(view.origin, root_registration.origin());

    let sealed = Sealed::read_with(&mut controller, &frames, root_registration);
    set_size(&mut controller, 6.0);
    drag(&mut controller, &[(10.0, 10.0), (30.0, 20.0)]);
    assert_ne!(indices(&controller), sealed.indices, "the stroke painted");
    sealed.assert_shown(&controller, "an older checkpoint's edit");

    // The head's registration would have drawn something else.
    let wrong = Sealed::read_with(&mut controller, &frames, head_registration);
    assert!(
        wrong.displayed(&controller) != wrong.expected(&indices(&controller)),
        "the check can tell the two registrations apart"
    );
}
