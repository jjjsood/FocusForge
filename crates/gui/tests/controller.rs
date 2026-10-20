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
use focusforge_gpu::blend::Blender;
use focusforge_gpu::device::SharedDevice;
#[cfg(feature = "gpu-tests")]
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gui::appearance_settings::{AppearanceSettings, SourcePresentation};
use focusforge_gui::canvas::PaneSource;
use focusforge_gui::{AppController, DockTab, ShellEvent, ShellMode, performance_settings};

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

/// Every `... => { ... }` handler in `contents`, as its source line, named
/// handler, and trimmed body. Handles nested braces so a body that itself
/// contains a struct literal or a block does not truncate early.
fn handler_bodies(contents: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    let bytes = contents.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = contents[search_from..].find("=> {") {
        let arrow_pos = search_from + rel;
        let body_start = arrow_pos + "=> {".len();
        let line = contents[..arrow_pos].matches('\n').count() + 1;
        let handler = contents[..arrow_pos]
            .rsplit('\n')
            .next()
            .expect("arrow has a source line")
            .trim()
            .to_owned();

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
        out.push((
            line,
            handler,
            contents[body_start..body_end].trim().to_string(),
        ));
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
        for (line, handler, body) in handler_bodies(&contents) {
            if !is_single_root_call(&body) && !is_local_event_seam(&path, &handler) {
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

/// Local input state machines are the only permitted non-forwarding Slint
/// handlers. Their callback boundaries remain root forwards and are covered
/// by their input behavior tests.
fn is_local_event_seam(path: &std::path::Path, handler: &str) -> bool {
    matches!(
        (path.file_name().and_then(|name| name.to_str()), handler),
        (Some("ff_menu.slint"), "accessible-action-default")
            | (Some("ff_menu.slint"), "key-pressed(event)")
            | (Some("ff_slider.slint"), "pointer-event(event)")
            | (Some("ff_slider.slint"), "key-pressed(event)")
            | (Some("ff_image_row.slint"), "pointer-event(event)")
            | (Some("ff_image_row.slint"), "moved")
            | (Some("images_panel.slint"), "clicked")
            | (Some("images_panel.slint"), "key-pressed(event)")
            | (Some("appearance_tab.slint"), "changed menu-open")
            | (Some("appearance_tab.slint"), "previous-requested")
            | (Some("appearance_tab.slint"), "next-requested")
            | (Some("appearance_tab.slint"), "accepted")
            | (Some("appearance_tab.slint"), "dismissed")
            | (Some("app.slint"), "bell-activated")
            | (Some("app.slint"), "close-requested")
            | (Some("app.slint"), "link-activated")
            | (Some("app.slint"), "settings-close-requested")
            | (Some("settings_modal.slint"), "changed settings-open")
            | (Some("settings_modal.slint"), "changed settings-section")
            | (Some("settings_modal.slint"), "key-pressed(event)")
            | (Some("notification_center.slint"), "clicked")
            | (Some("notification_center.slint"), "changed selected")
            | (Some("notification_center.slint"), "key-pressed(event)")
    )
}

#[test]
fn local_event_seams_are_named_input_handlers_only() {
    let panel = common::ui_dir().join("shell/dock/images_panel.slint");
    assert!(is_local_event_seam(&panel, "key-pressed(event)"));
    assert!(
        !is_local_event_seam(&panel, "sort-selected(choice)"),
        "domain callbacks must remain root forwards"
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

// A bound on a hang, not a speed requirement: a debug build on a loaded hosted Windows runner
// needs well over a minute to fuse the 512 px fixture, and a settled run returns at once.
const SETTLE: Duration = Duration::from_secs(300);

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
    let root = open_mantis64_frames(controller, name, count);
    // Editor entry is under test, not alignment quality; an algorithm that
    // always converges keeps every run clean.
    controller.set_align_algorithm("dummy-align");
    root
}

/// [`open_mantis64_tab`] on the default aligner, which parks these frames
/// at its Keep/Discard question.
fn open_mantis64_frames(controller: &mut AppController, name: &str, count: usize) -> PathBuf {
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
    root
}

fn open_many_timed_frames(controller: &mut AppController, name: &str, count: usize) {
    let root = common::tree(&format!("{name}_frames"));
    for index in 0..count {
        let path = root.join(format!("{index:04}.tif"));
        focusforge_core::io::test_frames::write_frame_with_capture_time(
            &path,
            4,
            4,
            Some("2026:08:29 10:00:00"),
        )
        .expect("synthetic frame must write");
    }
    assert!(matches!(
        common::open_and_settle(controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("dummy-align");
}

/// Settles every run, the live one and each one promoted from the queue.
fn settle_all(controller: &mut AppController) {
    while common::wait_for_settle(controller, SETTLE).is_some() {}
}

/// [`open_mantis64_tab`], then fuses it into one held checkpoint.
fn open_fused_tab(controller: &mut AppController, name: &str, count: usize) -> PathBuf {
    open_fused_tab_on(controller, name, count, None)
}

/// [`open_fused_tab`] on fuse method `method` instead of the default.
fn open_fused_tab_on(
    controller: &mut AppController,
    name: &str,
    count: usize,
    method: Option<&str>,
) -> PathBuf {
    let root = open_mantis64_tab(controller, name, count);
    if let Some(method) = method {
        controller.set_fuse_algorithm(method);
    }
    // `Sealed`'s reference reads the raw frames, so the run applies no gains.
    controller.set_param_value(
        "prepare",
        "equalize",
        focusforge_core::params::ParamValue::Bool(false),
    );
    assert!(controller.start_fuse());
    settle_all(controller);
    assert_eq!(controller.checkpoint_count(), Some(1), "{name} must fuse");
    root
}

#[test]
fn alignment_off_and_crop_off_survive_save_and_reopen() {
    let mut controller = controller_with_scratch("align_crop_off");
    open_mantis64_tab(&mut controller, "align_crop_off", 3);
    controller.set_param_value(
        "align-switch",
        "align",
        focusforge_core::params::ParamValue::Bool(false),
    );
    controller.set_param_value(
        "finish",
        "auto-crop",
        focusforge_core::params::ParamValue::Bool(false),
    );
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    let path = save_to_proposed(&mut controller);

    let head = |path: &Path| {
        focusforge_core::project::Project::open(path)
            .expect("the saved project opens")
            .manifest()
            .clone()
    };
    let manifest = head(&path);
    assert!(manifest.align.is_none(), "no alignment ran");
    assert!(manifest.registration.crop.is_none(), "no crop applied");
    assert_eq!(manifest.composite.width, 64, "the full frame extent");

    let mut reopened = controller_with_scratch("align_crop_off_reopened");
    assert!(matches!(
        common::open_and_settle(&mut reopened, std::slice::from_ref(&path)),
        OpenOutcome::Opened(_)
    ));
    let switches = |controller: &AppController| -> (String, String) {
        let rows = controller.settings_panel().rows;
        let selected = |section: &str, id: &str| {
            rows.iter()
                .find(|row| row.id == id && row.section == section)
                .unwrap_or_else(|| panic!("no {id} row"))
                .selected
                .clone()
        };
        (
            selected("align-switch", "align"),
            selected("finish", "auto-crop"),
        )
    };
    assert_eq!(
        switches(&reopened),
        ("false".to_string(), "false".to_string())
    );

    assert!(reopened.start_fuse(), "the reopened project fuses again");
    settle_all(&mut reopened);
    assert_eq!(reopened.checkpoint_count(), Some(2));
    reopened.save_active_project(|_| panic!("a saved tab never asks where"));
    let manifest = head(&path);
    assert!(manifest.align.is_none() && manifest.registration.crop.is_none());
    assert_eq!(manifest.composite.width, 64);
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

/// The shape of the Z-Map at `path`, read straight from disk.
fn stored_info(path: &Path) -> ZMapInfo {
    ZMapReader::open(path)
        .expect("the tab's Z-Map must open")
        .info()
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
    assert_eq!(take_mode_changes(&events), vec![ShellMode::Editor]);
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
    let info_b = controller.edit_session().unwrap().info();
    assert_eq!(
        info_b,
        stored_info(&zmap_b),
        "tab 1's session is over its own Z-Map"
    );

    controller.select_project(0);
    assert_eq!(controller.mode(), ShellMode::Editor);
    let (_, zmap_a) = controller.materialize_active_tab_planes().unwrap();
    let info_a = controller.edit_session().unwrap().info();
    assert_eq!(
        info_a,
        stored_info(&zmap_a),
        "tab 0's session is over its own Z-Map"
    );
    assert_ne!(info_a, info_b, "2 frames against 3");

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
    assert_eq!(session.info(), stored_info(&newest));
    assert!(session.stack_ops().is_empty());
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

#[test]
fn undocking_refuses_the_last_pane_and_docks_back_idempotently() {
    for detached in [PaneSource::Frame, PaneSource::Composite] {
        let other = match detached {
            PaneSource::Frame => PaneSource::Composite,
            PaneSource::Composite => PaneSource::Frame,
        };
        let mut controller = AppController::new();
        let events = record_events(&mut controller);

        assert!(controller.undock_enabled(detached));
        controller.undock_requested(detached);
        controller.undock_requested(detached);
        controller.undock_requested(other);
        assert!(!controller.undock_enabled(detached));
        assert!(!controller.undock_enabled(other));
        controller.undocked_window_closed(detached);
        controller.undocked_window_closed(detached);

        assert!(controller.undock_enabled(PaneSource::Frame));
        assert!(controller.undock_enabled(PaneSource::Composite));
        assert_eq!(
            events.lock().unwrap().as_slice(),
            [
                ShellEvent::PaneUndockRequested(detached),
                ShellEvent::PaneDocked(detached),
            ]
        );
    }
}

#[test]
fn an_accepted_main_quit_closes_the_detached_window_first() {
    let mut controller = AppController::new();
    let events = record_events(&mut controller);
    controller.undock_requested(PaneSource::Composite);
    controller.window_close();
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            ShellEvent::PaneUndockRequested(PaneSource::Composite),
            ShellEvent::DetachedWindowCloseRequested,
            ShellEvent::WindowCloseRequested,
        ]
    );
}

#[test]
fn every_successful_quit_path_closes_the_detached_pane_first() {
    let assert_close_tail = |events: &Arc<Mutex<Vec<ShellEvent>>>| {
        let events = events.lock().unwrap();
        assert_eq!(
            &events[events.len() - 2..],
            [
                ShellEvent::DetachedWindowCloseRequested,
                ShellEvent::WindowCloseRequested,
            ]
        );
    };
    for detached in [PaneSource::Frame, PaneSource::Composite] {
        let mut immediate = AppController::new();
        immediate.undock_requested(detached);
        let events = record_events(&mut immediate);
        immediate.window_close();
        assert_close_tail(&events);
    }

    let mut discard = controller_with_scratch("detached_quit_without_saving");
    open_fused_tab(&mut discard, "detached_quit_without_saving", 3);
    discard.undock_requested(PaneSource::Composite);
    let events = record_events(&mut discard);
    discard.quit_without_saving();
    assert_close_tail(&events);

    let mut save = controller_with_scratch("detached_save_all_quit");
    open_fused_tab(&mut save, "detached_save_all_quit", 3);
    save.undock_requested(PaneSource::Frame);
    let events = record_events(&mut save);
    save.window_close();
    let target = common::tree("detached_save_all_quit").join("saved.ffp");
    assert!(save.close_guard_secondary_pressed(|_| Some(target.clone())));
    assert_close_tail(&events);

    let mut headless = controller_with_scratch("detached_save_all_quit_sync");
    open_fused_tab(&mut headless, "detached_save_all_quit_sync", 3);
    headless.undock_requested(PaneSource::Composite);
    let events = record_events(&mut headless);
    let target = common::tree("detached_save_all_quit_sync").join("saved.ffp");
    assert!(headless.save_all_and_quit(|_| Some(target.clone())));
    assert_close_tail(&events);
}

#[test]
fn quit_asks_only_when_work_would_be_lost() {
    for tabs in 0..=2 {
        let name = format!("quit_asks_unfused_{tabs}");
        let mut controller = controller_with_scratch(&name);
        for tab in 0..tabs {
            open_mantis64_tab(&mut controller, &format!("{name}_{tab}"), 3);
        }
        let events = record_events(&mut controller);
        controller.window_close();
        assert_eq!(
            events.lock().unwrap().as_slice(),
            [ShellEvent::WindowCloseRequested],
            "{tabs} never-saved tabs with no fuse close at once"
        );
        assert!(controller.pending_quit_copy().is_none(), "{tabs} tabs");
    }

    let mut controller = controller_with_scratch("quit_asks_fused_added");
    open_mantis64_tab(&mut controller, "quit_asks_fused_added_a", 3);
    open_fused_tab(&mut controller, "quit_asks_fused_added_b", 3);
    let events = record_events(&mut controller);
    controller.window_close();
    assert!(
        controller.pending_quit_copy().is_some(),
        "a fused, unsaved tab raises the question"
    );
    assert!(
        !events
            .lock()
            .unwrap()
            .contains(&ShellEvent::WindowCloseRequested),
        "the window stays until the question is answered"
    );
}

#[test]
fn cancelling_a_guarded_quit_keeps_the_detached_pane() {
    let mut controller = controller_with_scratch("detached_guarded_quit");
    open_mantis64_tab(&mut controller, "detached_guarded_quit", 3);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    controller.undock_requested(PaneSource::Composite);
    let events = record_events(&mut controller);
    controller.window_close();
    assert!(controller.pending_quit_copy().is_some());
    controller.cancel_quit();
    assert!(controller.pending_quit_copy().is_none());
    assert!(!controller.undock_enabled(PaneSource::Frame));
    assert!(
        !events
            .lock()
            .unwrap()
            .contains(&ShellEvent::DetachedWindowCloseRequested)
    );
}

#[test]
fn cancelling_the_guarded_save_picker_keeps_the_detached_pane() {
    let mut controller = controller_with_scratch("detached_save_picker_cancel");
    open_mantis64_tab(&mut controller, "detached_save_picker_cancel", 3);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    controller.undock_requested(PaneSource::Frame);
    let events = record_events(&mut controller);
    controller.window_close();
    assert!(controller.pending_quit_copy().is_some());
    assert!(!controller.close_guard_secondary_pressed(|_| None));
    assert!(controller.pending_quit_copy().is_none());
    assert_eq!(controller.detached_pane(), Some(PaneSource::Frame));
    assert!(!events.lock().unwrap().iter().any(|event| matches!(
        event,
        ShellEvent::DetachedWindowCloseRequested | ShellEvent::WindowCloseRequested
    )));
}

#[test]
fn failed_guarded_quit_save_keeps_the_detached_pane() {
    let mut controller = controller_with_scratch("detached_failed_quit_save");
    open_mantis64_tab(&mut controller, "detached_failed_quit_save", 3);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    controller.undock_requested(PaneSource::Composite);
    let events = record_events(&mut controller);
    controller.window_close();
    let absent = common::tree("detached_failed_quit_save").join("absent/x.ffp");
    assert!(!controller.close_guard_secondary_pressed(|_| Some(absent.clone())));
    assert!(!controller.undock_enabled(PaneSource::Frame));
    let events = events.lock().unwrap();
    assert!(!events.contains(&ShellEvent::DetachedWindowCloseRequested));
    assert!(!events.contains(&ShellEvent::WindowCloseRequested));
}

#[test]
fn detached_geometry_updates_independently_and_rejects_invalid_sizes() {
    let mut controller = controller_with_scratch("independent_detached_geometry");
    open_mantis64_tab(&mut controller, "independent_detached_geometry", 3);
    let left = PaneRect {
        x: 16.0,
        y: 92.0,
        width: 400.0,
        height: 800.0,
    };
    let right = PaneRect {
        x: 432.0,
        y: 92.0,
        width: 900.0,
        height: 500.0,
    };
    controller.set_pane_geometry(left, right);
    let before = controller.plane_draw().unwrap();

    for width in [f32::NAN, 0.0, -1.0, f32::INFINITY] {
        controller.set_pane_geometry_for(
            PaneSource::Composite,
            PaneRect {
                x: 0.0,
                y: 0.0,
                width,
                height: 600.0,
            },
        );
        assert_eq!(controller.plane_draw().unwrap(), before);
    }

    let detached = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 820.0,
        height: 600.0,
    };
    controller.set_pane_geometry_for(PaneSource::Composite, detached);
    let frame = controller.frame_draw().unwrap();
    let plane = controller.plane_draw().unwrap();
    assert!(frame.dst.2 <= left.width && frame.dst.3 <= left.height);
    assert!(plane.dst.2 <= detached.width && plane.dst.3 <= detached.height);
}

#[test]
fn main_resize_keeps_each_detached_panes_last_geometry() {
    for detached in [PaneSource::Frame, PaneSource::Composite] {
        let mut controller = controller_with_scratch(&format!("preserve_detached_{detached:?}"));
        open_mantis64_tab(
            &mut controller,
            &format!("preserve_detached_{detached:?}"),
            3,
        );
        let narrow = PaneRect {
            x: 0.0,
            y: 0.0,
            width: 80.0,
            height: 80.0,
        };
        let roomy = PaneRect {
            x: 100.0,
            y: 0.0,
            width: 400.0,
            height: 400.0,
        };
        let (left, right) = match detached {
            PaneSource::Frame => (narrow, roomy),
            PaneSource::Composite => (roomy, narrow),
        };
        controller.set_pane_geometry(left, right);
        controller.undock_requested(detached);
        assert_eq!(controller.detached_pane(), Some(detached));

        let replacement = PaneRect {
            x: 0.0,
            y: 0.0,
            width: 1_000.0,
            height: 1_000.0,
        };
        let resized_main = PaneRect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 200.0,
        };
        let (left, right) = match detached {
            PaneSource::Frame => (replacement, resized_main),
            PaneSource::Composite => (resized_main, replacement),
        };
        controller.set_pane_geometry(left, right);
        assert_eq!(controller.detached_pane(), Some(detached));

        let draw = match detached {
            PaneSource::Frame => controller.frame_draw().unwrap(),
            PaneSource::Composite => controller.plane_draw().unwrap(),
        };
        assert!(
            draw.dst.2 <= narrow.width,
            "main resize replaced {detached:?} geometry: {draw:?}"
        );
    }
}

#[test]
fn detached_side_survives_resize_and_valid_mode_switches() {
    let mut controller = controller_with_scratch("detached_mode_switch");
    open_fused_tab(&mut controller, "detached_mode_switch", 3);
    let left = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 200.0,
        height: 300.0,
    };
    let right = PaneRect {
        x: 220.0,
        y: 0.0,
        width: 400.0,
        height: 250.0,
    };
    controller.set_pane_geometry(left, right);
    controller.undock_requested(PaneSource::Frame);
    controller.set_pane_geometry(
        PaneRect {
            width: 900.0,
            height: 900.0,
            ..left
        },
        right,
    );
    assert_eq!(controller.detached_pane(), Some(PaneSource::Frame));
    assert!(controller.set_mode(ShellMode::Editor));
    assert_eq!(controller.detached_pane(), Some(PaneSource::Frame));
    assert!(controller.set_mode(ShellMode::Stacking));
    assert_eq!(controller.detached_pane(), Some(PaneSource::Frame));
}

#[test]
fn side_geometry_arrives_before_open_in_either_order() {
    for (first, second) in [
        (PaneSource::Frame, PaneSource::Composite),
        (PaneSource::Composite, PaneSource::Frame),
    ] {
        let name = format!("geometry_order_{first:?}");
        let mut controller = controller_with_scratch(&name);
        let frame = PaneRect {
            x: 0.0,
            y: 0.0,
            width: 180.0,
            height: 320.0,
        };
        let composite = PaneRect {
            x: 0.0,
            y: 0.0,
            width: 320.0,
            height: 180.0,
        };
        let rect = |side| match side {
            PaneSource::Frame => frame,
            PaneSource::Composite => composite,
        };
        controller.set_pane_geometry_for(first, rect(first));
        assert!(controller.frame_draw().is_none());
        controller.set_pane_geometry_for(second, rect(second));
        assert!(controller.frame_draw().is_none());

        open_mantis64_tab(&mut controller, &name, 3);
        controller.fit_if_unfitted();
        assert!(controller.frame_draw().is_some());
        assert!(controller.plane_draw().is_some());
    }
}

#[test]
fn detached_input_uses_its_own_local_coordinates() {
    let mut controller = controller_with_scratch("detached_local_input");
    open_mantis64_tab(&mut controller, "detached_local_input", 3);
    controller.set_pane_geometry(
        PaneRect {
            x: 16.0,
            y: 92.0,
            width: 40.0,
            height: 40.0,
        },
        PaneRect {
            x: 0.0,
            y: 0.0,
            width: 50.0,
            height: 50.0,
        },
    );
    let fitted = controller.zoom_readout();
    controller.wheel_for_pane(
        PaneSource::Composite,
        0.0,
        60.0,
        25.0,
        25.0,
        Modifiers::NONE,
    );
    controller.wheel_for_pane(
        PaneSource::Composite,
        0.0,
        60.0,
        25.0,
        25.0,
        Modifiers::NONE,
    );
    controller.wheel_for_pane(
        PaneSource::Composite,
        0.0,
        60.0,
        25.0,
        25.0,
        Modifiers::NONE,
    );
    assert_ne!(controller.zoom_readout(), fitted);
    let before_pan = controller.plane_draw().unwrap();
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Down,
        Button::Middle,
        25.0,
        25.0,
        NO_MODS,
    );
    controller.pointer_moved_for_pane(PaneSource::Composite, -75.0, -75.0);
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Up,
        Button::Middle,
        -75.0,
        -75.0,
        NO_MODS,
    );
    assert_ne!(controller.plane_draw().unwrap().dst, before_pan.dst);
}

#[test]
fn detached_pan_uses_the_input_table_and_modal_guard() {
    let mut controller = controller_with_scratch("detached_pan_modal_guard");
    open_mantis64_tab(&mut controller, "detached_pan_modal_guard", 3);
    let pane = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 80.0,
        height: 80.0,
    };
    controller.set_pane_geometry(pane, pane);
    for _ in 0..3 {
        controller.wheel_for_pane(PaneSource::Composite, 0.0, 60.0, 40.0, 40.0, NO_MODS);
    }
    let before = controller.plane_draw().unwrap();
    controller.set_align_algorithm("");
    assert!(!controller.start_fuse());
    assert!(controller.canvas_blocked());

    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Down,
        Button::Middle,
        40.0,
        40.0,
        NO_MODS,
    );
    controller.pointer_moved_for_pane(PaneSource::Composite, 0.0, 0.0);
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Up,
        Button::Middle,
        0.0,
        0.0,
        NO_MODS,
    );
    assert_eq!(controller.plane_draw().unwrap(), before);
}

#[test]
fn detached_drag_ignores_another_windows_move_and_release() {
    let mut controller = controller_with_scratch("detached_cross_window_drag");
    open_mantis64_tab(&mut controller, "detached_cross_window_drag", 3);
    let pane = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 80.0,
        height: 80.0,
    };
    controller.set_pane_geometry(pane, pane);
    for _ in 0..3 {
        controller.wheel_for_pane(PaneSource::Composite, 0.0, 60.0, 40.0, 40.0, NO_MODS);
    }
    let before = controller.plane_draw().unwrap();
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Down,
        Button::Middle,
        40.0,
        40.0,
        NO_MODS,
    );
    controller.pointer_moved(0.0, 0.0);
    controller.pointer(PointerKind::Up, Button::Middle, 0.0, 0.0, NO_MODS);
    controller.pointer_moved_for_pane(PaneSource::Composite, 0.0, 0.0);
    assert_ne!(controller.plane_draw().unwrap().dst, before.dst);
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Up,
        Button::Middle,
        0.0,
        0.0,
        NO_MODS,
    );
}

#[test]
fn main_input_maps_through_the_hit_panes_rect_when_local_origins_overlap() {
    let (mut controller, _) = open_editor("main_input_skips_detached");
    let overlap = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 300.0,
        height: 180.0,
    };
    controller.set_pane_geometry(overlap, overlap);

    controller.undock_requested(PaneSource::Frame);
    let draw = controller.plane_draw().unwrap();
    let point = (
        draw.dst.0 + (20.5 - draw.src.0 as f32) * draw.dst.2 / draw.src.2 as f32,
        draw.dst.1 + (20.5 - draw.src.1 as f32) * draw.dst.3 / draw.src.3 as f32,
    );
    controller.pointer(PointerKind::Down, Button::Left, point.0, point.1, NO_MODS);
    controller.pointer(PointerKind::Up, Button::Left, point.0, point.1, NO_MODS);
    assert!(matches!(
        ops(&controller).as_slice(),
        [Op::BrushStroke { .. }]
    ));

    controller.undocked_window_closed(PaneSource::Frame);
    controller.undock_requested(PaneSource::Composite);
    let before = ops(&controller).len();
    controller.pointer(PointerKind::Down, Button::Left, point.0, point.1, NO_MODS);
    controller.pointer(PointerKind::Up, Button::Left, point.0, point.1, NO_MODS);
    // The rects are identical, so the left pane's press maps to the same
    // source point and paints.
    assert_eq!(ops(&controller).len(), before + 1);
}

#[test]
fn editor_paints_from_an_unequal_detached_composite_pane() {
    let (mut controller, _) = open_editor("unequal_detached_editor_paint");
    controller.set_pane_geometry(
        PaneRect {
            x: 16.0,
            y: 92.0,
            width: 180.0,
            height: 320.0,
        },
        PaneRect {
            x: 0.0,
            y: 0.0,
            width: 300.0,
            height: 180.0,
        },
    );
    let draw = controller.plane_draw().unwrap();
    let local = |x: f32, y: f32| {
        (
            draw.dst.0 + (x - draw.src.0 as f32 + 0.5) * draw.dst.2 / draw.src.2 as f32,
            draw.dst.1 + (y - draw.src.1 as f32 + 0.5) * draw.dst.3 / draw.src.3 as f32,
        )
    };
    let start = local(20.0, 20.0);
    let end = local(30.0, 25.0);
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Down,
        Button::Left,
        start.0,
        start.1,
        NO_MODS,
    );
    controller.pointer_moved_for_pane(PaneSource::Composite, end.0, end.1);
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Up,
        Button::Left,
        end.0,
        end.1,
        NO_MODS,
    );
    assert!(matches!(
        ops(&controller).as_slice(),
        [Op::BrushStroke { path, .. }]
            if path.first().is_some_and(|point| point.x == 20 && point.y == 20)
    ));
}

#[test]
fn unequal_composite_pipette_reads_the_exact_projected_source_index() {
    let (mut controller, _) = open_editor("unequal_composite_pipette");
    let left = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 180.0,
        height: 320.0,
    };
    let right = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 300.0,
        height: 180.0,
    };
    controller.set_pane_geometry(left, right);
    let map = map_rect(&controller);
    let zmap = controller.held_checkpoints().last().unwrap().zmap.clone();
    let values = read_whole_zmap(&zmap);
    let current = controller.brush_source().unwrap();
    let (x, y, expected) = (0..map.height)
        .flat_map(|y| (0..map.width).map(move |x| (x, y)))
        .find_map(|(x, y)| {
            let source = values[(y * map.width + x) as usize];
            (source != current && source != NO_SOURCE).then_some((x, y, source))
        })
        .expect("the fused map names a second source");
    let draw = controller.plane_draw().unwrap();
    let local = (
        draw.dst.0 + (x as f32 + 0.5 - draw.src.0 as f32) * draw.dst.2 / draw.src.2 as f32,
        draw.dst.1 + (y as f32 + 0.5 - draw.src.1 as f32) * draw.dst.3 / draw.src.3 as f32,
    );
    controller.toggle_pipette();
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Down,
        Button::Left,
        local.0,
        local.1,
        NO_MODS,
    );
    controller.pointer_for_pane(
        PaneSource::Composite,
        PointerKind::Up,
        Button::Left,
        local.0,
        local.1,
        NO_MODS,
    );
    assert_eq!(controller.brush_source(), Some(expected));
}

#[test]
fn editor_requires_a_real_unequal_composite_hit_before_painting() {
    let (mut controller, _) = open_editor("unequal_composite_hit_test");
    let left = PaneRect {
        x: 0.0,
        y: 0.0,
        width: 180.0,
        height: 320.0,
    };
    let right = PaneRect {
        x: 200.0,
        y: 0.0,
        width: 300.0,
        height: 180.0,
    };
    controller.set_pane_geometry(left, right);

    for outside in [(190.0, 20.0), (510.0, 20.0)] {
        controller.pointer(
            PointerKind::Down,
            Button::Left,
            outside.0,
            outside.1,
            NO_MODS,
        );
        controller.pointer(PointerKind::Up, Button::Left, outside.0, outside.1, NO_MODS);
    }
    assert!(ops(&controller).is_empty(), "gutter/outside must not paint");

    let draw = controller.plane_draw().unwrap();
    let point = (
        right.x + draw.dst.0 + (20.5 - draw.src.0 as f32) * draw.dst.2 / draw.src.2 as f32,
        right.y + draw.dst.1 + (20.5 - draw.src.1 as f32) * draw.dst.3 / draw.src.3 as f32,
    );
    controller.pointer(PointerKind::Down, Button::Left, point.0, point.1, NO_MODS);
    controller.pointer(PointerKind::Up, Button::Left, point.0, point.1, NO_MODS);
    assert!(matches!(
        ops(&controller).as_slice(),
        [Op::BrushStroke { path, .. }]
            if path.first().is_some_and(|point| point.x == 20 && point.y == 20)
    ));
}

/// A press on the drawn `Checkpoints` scrollbar thumb grabs it at the press
/// offset instead of jumping the list: the thumb math reads the content
/// height `checkpoints_panel()` builds, the same number the list draws
/// with. Only a press on the track outside the thumb may jump.
#[test]
fn pressing_the_checkpoints_scrollbar_thumb_grabs_it_without_jumping() {
    use focusforge_core::io::{TiffSpec, TiffWriter};
    use focusforge_core::params::ParamValues;
    use focusforge_core::project::{Metrics, SealRequest, append_ops_at, seal_at};
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
        append_ops_at(&project_path, vec![op], None, noon + i * 60)
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

    let outcome = controller.settle_open();

    assert!(
        matches!(outcome, Some(OpenOutcome::Opened(_))),
        "a header-valid frame with unreadable pixels still opens"
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
    // The reference pictures above are fused without gains; fuse the same way.
    controller.set_param_value(
        "prepare",
        "equalize",
        focusforge_core::params::ParamValue::Bool(false),
    );
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
            .map(|i| archive.by_index(i).unwrap().name().unwrap().to_string())
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

    // The cursor at the very start never pushes the text right of its origin.
    controller.diagnostics_destination_cursor_moved(0.0, 100.0);
    assert_eq!(controller.diagnostics_destination_scroll_offset(), 0.0);

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

fn crash_record(controller: &AppController) -> Option<focusforge_gui::notifications::Notification> {
    controller
        .notifications()
        .iter()
        .find(|n| n.kind == focusforge_gui::notifications::Kind::Crash)
        .cloned()
}

#[test]
fn crash_notice_counts_and_acknowledges() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    let record = crash_record(&controller).expect("unreported evidence is announced");
    assert_eq!(record.title, "Previous session stopped");
    assert!(
        record
            .message
            .starts_with("A previous session stopped unexpectedly. 2 crash reports were detected"),
        "{}",
        record.message
    );
    assert_eq!(record.crash.as_ref().unwrap().ids.len(), 2);
    assert!(!record.read);
    assert_eq!(controller.notifications().len(), 1, "one record per scan");

    controller
        .acknowledge_crash(record.id)
        .expect("the marks are written");
    let record = crash_record(&controller).unwrap();
    let crash = record.crash.unwrap();
    assert!(crash.acknowledged && crash.ids.is_empty() && crash.error.is_none());
    assert_eq!(crash.reports, 2, "the count outlives the cleared ids");

    let mut after = AppController::new();
    after.set_diagnostics_dirs(Some(data_dir), None);
    after.scan_crash_notice();
    assert!(
        crash_record(&after).is_none(),
        "a fresh controller over the same directory must see nothing pending"
    );

    let empty_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_root.path().join("focusforge/crashes")).unwrap();
    let mut empty = AppController::new();
    empty.set_diagnostics_dirs(Some(empty_root.path().join("focusforge")), None);
    empty.scan_crash_notice();
    assert!(empty.notifications().is_empty());
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

    let crash = crash_record(&controller)
        .and_then(|r| r.crash)
        .expect("an unreadable crash directory is announced as unavailable");
    assert!(crash.unavailable && crash.ids.is_empty());
}

#[test]
fn crash_record_survives_expiry_and_reading() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.visible_toast().is_some(), "the crash is toasted");

    controller.expire_toast(std::time::Instant::now() + std::time::Duration::from_secs(60));
    assert!(controller.visible_toast().is_none());
    controller.open_center();
    assert_eq!(
        controller.unread_notifications(),
        0,
        "viewing reads the record"
    );
    let crash = crash_record(&controller).unwrap().crash.unwrap();
    assert!(!crash.acknowledged && crash.ids.len() == 2);
    assert!(
        !data_dir
            .join("crashes/20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "neither expiry nor reading writes a reported marker"
    );

    let mut after = AppController::new();
    after.set_diagnostics_dirs(Some(data_dir), None);
    after.scan_crash_notice();
    assert!(
        crash_record(&after).is_some(),
        "the evidence is still pending"
    );
}

#[test]
#[cfg(unix)]
fn crash_failed_acknowledgement_keeps_evidence_and_its_error() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());
    let crashes = data_dir.join("crashes");
    let writable = fs::metadata(&crashes).unwrap().permissions();

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir), None);
    controller.scan_crash_notice();
    let id = crash_record(&controller).unwrap().id;

    fs::set_permissions(&crashes, fs::Permissions::from_mode(0o555)).unwrap();
    let outcome = controller.acknowledge_crash(id);
    let writable_now = fs::write(crashes.join("probe"), "x").is_ok();
    fs::set_permissions(&crashes, writable).unwrap();
    if writable_now {
        return; // privileged user: a read-only directory still accepts writes
    }

    let error = outcome.expect_err("the marks cannot be written");
    let crash = crash_record(&controller).unwrap().crash.unwrap();
    assert!(!crash.acknowledged, "the evidence stays unreported");
    assert_eq!(crash.ids.len(), 2, "the ids are kept for a retry");
    assert_eq!(crash.error.as_deref(), Some(error.as_str()));

    controller
        .acknowledge_crash(id)
        .expect("the retry succeeds");
    assert!(
        crash_record(&controller)
            .unwrap()
            .crash
            .unwrap()
            .acknowledged
    );
}

#[test]
fn missing_crash_directory_keeps_the_center_usable() {
    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(None, None);
    controller.scan_crash_notice();
    let record = crash_record(&controller).expect("the unavailable state is announced");
    assert!(record.crash.as_ref().unwrap().unavailable);

    controller.open_center();
    assert!(controller.notification_center_open());
    assert_eq!(controller.unread_notifications(), 0);
    assert!(
        controller.crash_diagnostics(record.id),
        "Diagnostics stays reachable"
    );
    assert_eq!(controller.settings_section(), SettingsSection::Diagnostics);
}

#[test]
fn settings_section_slugs_round_trip() {
    let mut slugs = Vec::new();
    for section in SettingsSection::ALL {
        assert_eq!(SettingsSection::from_slug(section.slug()), Some(section));
        slugs.push(section.slug());
    }
    let distinct: std::collections::HashSet<_> = slugs.iter().collect();
    assert_eq!(distinct.len(), SettingsSection::ALL.len(), "{slugs:?}");
    assert_eq!(
        slugs,
        [
            "performance",
            "appearance",
            "storage",
            "input-map",
            "diagnostics",
            "about"
        ]
    );

    for refused in ["general", "", "nope", "Diagnostics", "About"] {
        assert_eq!(
            SettingsSection::from_slug(refused),
            None,
            "{refused:?} must not fall back to a section"
        );
    }
}

#[test]
fn settings_open_on_performance_by_default() {
    assert_eq!(SettingsSection::default(), SettingsSection::Performance);
    let mut controller = AppController::new();
    assert!(controller.open_settings());
    assert_eq!(controller.settings_section(), SettingsSection::Performance);
}

#[test]
fn crash_notice_diagnostics_acknowledges_and_opens_settings() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    let id = crash_record(&controller).unwrap().id;

    let open = controller.crash_diagnostics(id);
    assert!(open, "Diagnostics must leave the settings modal open");
    assert_eq!(controller.settings_section(), SettingsSection::Diagnostics);
    assert!(
        crash_record(&controller)
            .unwrap()
            .crash
            .unwrap()
            .acknowledged
    );
    let marker = data_dir.join("crashes/20260918T010203Z-focusforge-gui-panic.reported");
    assert!(
        marker.exists(),
        "Diagnostics marks the evidence like Acknowledge"
    );
    assert!(
        data_dir
            .join("crashes/20260918T010204Z-focusforge-gui-native.reported")
            .exists()
    );

    let marked_at = fs::read_to_string(&marker).unwrap();
    assert!(controller.crash_diagnostics(id));
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
    assert_eq!(
        crash_record(&controller).unwrap().crash.unwrap().ids.len(),
        2
    );

    controller.set_destination(root.path().join("bundle.zip").to_str().unwrap());
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    let crash = crash_record(&controller).unwrap().crash.unwrap();
    assert!(
        crash.acknowledged,
        "a bundle written from the window acknowledges the record"
    );

    // A failed write leaves it unacknowledged.
    let root2 = tempfile::tempdir().unwrap();
    let data_dir2 = seed_two_crashes(root2.path());
    let mut controller2 = AppController::new();
    let events2 = install_recording_sink(&mut controller2);
    controller2.set_diagnostics_dirs(Some(data_dir2.clone()), None);
    controller2.scan_crash_notice();

    let blocked_parent = data_dir2.parent().unwrap().join("blocked");
    fs::write(&blocked_parent, b"not a directory").unwrap();
    controller2.set_destination(blocked_parent.join("bundle.zip").to_str().unwrap());
    let status2 = run_write(&mut controller2, &events2);
    assert_eq!(status2, DiagnosticsStatus::Failed);
    let crash2 = crash_record(&controller2).unwrap().crash.unwrap();
    assert!(!crash2.acknowledged && crash2.ids.len() == 2);
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
    let names = ["00.tif", "01.tif", "06.tif", "07.tif"].map(String::from);
    assert_eq!(controller.pending_divergence(), Some(&names[..]));

    controller.keep_fused().expect("a parked run keeps");
    let job::RunState::Fused { at, unaligned } = controller.job_view().state else {
        panic!("a kept run settles to Fused");
    };
    assert_eq!(unaligned, 4);
    assert_eq!(
        job::stage_word(&controller.job_view().state, None),
        format!("fused {at}"),
        "the bar names the run, the notice names the count"
    );
    let kept: Vec<_> = controller
        .notifications()
        .iter()
        .filter(|n| n.title == "4 frames unaligned")
        .collect();
    assert_eq!(kept.len(), 1, "keeping records the count once");
    assert!(kept[0].message.contains("4 frames unaligned"));
    let summary = controller.last_alignment().expect("a completed run");
    assert_eq!(
        (summary.source.as_str(), summary.backend.as_str()),
        ("computed", "cpu")
    );
    assert_eq!(summary.unaligned, 4);
    assert_eq!(summary.rerun, None);
}

/// The run state of tab `index`, read by selecting it.
fn tab_run(controller: &mut AppController, index: usize) -> job::RunState {
    controller.select_project(index);
    controller.job_view().state
}

/// Polls until some tab holds a parked run, within the budget a mantis-512
/// fuse needs.
fn settle_until_parked(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.parked_run_waiting() {
        controller.settle_job();
        assert!(
            start.elapsed() < Duration::from_secs(300),
            "no run parked within the budget"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

const MANTIS_512_UNALIGNED: [&str; 4] = ["00.tif", "01.tif", "06.tif", "07.tif"];
const MANTIS_64_UNALIGNED: [&str; 2] = ["00.tif", "02.tif"];

/// Tab 0 (mantis-512) parks at its question; `tails` are the tabs queued
/// behind it, each `true` for `dummy-align` and `false` for the default
/// aligner. Checks the queue head runs while tab 0 waits and every tab
/// settles to an outcome of its own, then answers each question.
fn queue_behind_a_park(name: &str, tails: &[bool], keep_first: bool, answer_while_live: bool) {
    let mut c = controller_on_mantis_512(name);
    for (i, dummy) in tails.iter().enumerate() {
        let tab = format!("{name}_{i}");
        if *dummy {
            open_mantis64_tab(&mut c, &tab, 3);
        } else {
            open_mantis64_frames(&mut c, &tab, 3);
        }
    }
    c.set_slow_run(2, Duration::from_secs(3));
    c.select_project(0);
    assert!(c.start_fuse());
    for i in 1..=tails.len() {
        c.select_project(i);
        assert!(c.start_fuse(), "tab {i} joins the queue");
        assert_eq!(
            c.job_view().state,
            job::RunState::Queued { position: i + 1 }
        );
    }

    settle_until_parked(&mut c);
    assert_eq!(tab_run(&mut c, 1), job::RunState::Running, "the head runs");
    assert!(c.parked_run_waiting(), "tab 0's park counts from tab 1");
    for i in 2..=tails.len() {
        assert_eq!(tab_run(&mut c, i), job::RunState::Queued { position: i });
    }
    c.select_project(0);
    assert_eq!(
        c.pending_divergence(),
        Some(&MANTIS_512_UNALIGNED.map(String::from)[..])
    );
    let answer_first = |c: &mut AppController| {
        c.select_project(0);
        if keep_first {
            c.keep_fused().expect("tab 0's park keeps");
            assert_eq!(c.checkpoint_count(), Some(1));
        } else {
            c.discard_fused();
            assert_eq!(c.checkpoint_count(), Some(0));
        }
        assert_eq!(c.pending_divergence(), None);
    };
    if answer_while_live {
        answer_first(&mut c);
    }

    settle_all(&mut c);
    assert!(c.last_refusal().is_none());
    for (i, dummy) in tails.iter().enumerate() {
        c.select_project(i + 1);
        if *dummy {
            assert_eq!(c.checkpoint_count(), Some(1), "tab {} fused", i + 1);
            assert_eq!(c.pending_divergence(), None);
        } else {
            assert_eq!(c.checkpoint_count(), Some(0));
            assert_eq!(
                c.pending_divergence(),
                Some(&MANTIS_64_UNALIGNED.map(String::from)[..]),
                "tab {} parks at its own question",
                i + 1
            );
        }
    }
    if !answer_while_live {
        assert!(c.parked_run_waiting(), "tab 0 is still waiting");
        c.select_project(0);
        assert_eq!(
            c.pending_divergence(),
            Some(&MANTIS_512_UNALIGNED.map(String::from)[..])
        );
        answer_first(&mut c);
    }
    for (i, dummy) in tails.iter().enumerate() {
        if !dummy {
            c.select_project(i + 1);
            c.keep_fused().expect("the tail's own park keeps");
            assert_eq!(c.checkpoint_count(), Some(1));
        }
    }
    assert!(!c.parked_run_waiting(), "every question is answered");
    assert!(!c.work_pending());
}

#[test]
fn queued_tab_runs_past_a_keep_question() {
    // Queue lengths 1 and 2, `dummy-align` and the default aligner on the
    // promoted tabs, Keep and Discard on tab 0, answered during and after
    // the promoted run.
    queue_behind_a_park("queue_past_park_a", &[true], true, true);
    queue_behind_a_park("queue_past_park_b", &[false], false, false);
    queue_behind_a_park("queue_past_park_c", &[true, false], true, false);
    queue_behind_a_park("queue_past_park_d", &[false, true], false, true);
}

/// Tab 0 parked, tab 1 running slowly, tabs 2 and 3 queued behind it.
fn parked_tab_with_two_queued(name: &str) -> AppController {
    let mut c = controller_on_mantis_512(name);
    for i in 0..3 {
        open_mantis64_tab(&mut c, &format!("{name}_{i}"), 3);
    }
    c.set_slow_run(2, Duration::from_secs(3));
    c.select_project(0);
    assert!(c.start_fuse());
    for i in 1..=3 {
        c.select_project(i);
        assert!(c.start_fuse());
    }
    settle_until_parked(&mut c);
    assert_eq!(tab_run(&mut c, 1), job::RunState::Running);
    c
}

#[test]
fn queue_survives_closing_a_parked_tab() {
    let mut c = parked_tab_with_two_queued("queue_close_parked");
    c.request_close(0);
    assert!(!c.parked_run_waiting(), "closing drops the park");
    assert_eq!(c.project_tabs().len(), 3);
    assert_eq!(tab_run(&mut c, 0), job::RunState::Running);
    assert_eq!(tab_run(&mut c, 1), job::RunState::Queued { position: 2 });
    assert_eq!(tab_run(&mut c, 2), job::RunState::Queued { position: 3 });
    settle_all(&mut c);
    assert!(!c.work_pending());
    for i in 0..3 {
        c.select_project(i);
        assert_eq!(c.checkpoint_count(), Some(1), "tab {i} ran exactly once");
    }
}

#[test]
fn queue_survives_cancelling_a_queued_tab() {
    let mut c = parked_tab_with_two_queued("queue_cancel_queued");
    c.select_project(2);
    c.cancel_fuse();
    assert_eq!(c.job_view().state, job::RunState::Idle);
    assert_eq!(tab_run(&mut c, 3), job::RunState::Queued { position: 2 });
    assert!(c.parked_run_waiting(), "tab 0 still waits");
    settle_all(&mut c);
    assert!(!c.work_pending());
    for (i, runs) in [(1, 1), (2, 0), (3, 1)] {
        c.select_project(i);
        assert_eq!(c.checkpoint_count(), Some(runs), "tab {i}");
    }
    c.select_project(0);
    assert_eq!(
        c.pending_divergence(),
        Some(&MANTIS_512_UNALIGNED.map(String::from)[..])
    );
}

#[test]
fn a_clean_run_raises_no_unaligned_notice() {
    let mut controller = controller_with_scratch("clean_run_no_notice");
    open_mantis64_tab(&mut controller, "clean_run_no_notice", 3);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert!(
        controller
            .notifications()
            .iter()
            .all(|n| !n.title.ends_with("unaligned")),
        "a clean run records no unaligned notice"
    );
    assert_eq!(
        controller
            .last_alignment()
            .expect("a completed run")
            .unaligned,
        0
    );
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
use focusforge_core::retouch::blend;
use focusforge_core::retouch::footprint;
use focusforge_core::zmap::{NO_SOURCE, ZMapInfo, ZMapWriter};
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
    use focusforge_gui::input_map::{Action, Binding, InputTable, Trigger};
    let mut table = InputTable::default();
    table.bindings.push(Binding {
        trigger: Trigger::Wheel(ALT),
        stacking: None,
        editor: Some(Action::BrushSoftness),
    });
    controller.install_input_table(table);
}

/// The active tab's whole fused map.
fn map_rect(controller: &AppController) -> Rect {
    let info = controller.edit_session().unwrap().info();
    Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    }
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
    open_editor_on(name, None)
}

/// [`open_editor`] over a run of fuse method `method` instead of the default.
fn open_editor_on(name: &str, method: Option<&str>) -> (AppController, PathBuf) {
    let mut controller = controller_with_scratch(name);
    controller.set_resident_tile_size_for_test(16);
    let root = open_fused_tab_on(&mut controller, name, 3, method);
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

/// A left drag through image points `path`, one motion event per point, over
/// the right pane.
fn drag(controller: &mut AppController, path: &[(f32, f32)]) {
    drag_on(controller, RIGHT, path);
}

/// A left drag through image points `path` as drawn over `pane`.
fn drag_on(controller: &mut AppController, pane: PaneRect, path: &[(f32, f32)]) {
    let screen: Vec<(f32, f32)> = path.iter().map(|&(x, y)| at_pane(pane, x, y)).collect();
    drag_screen(controller, &screen);
}

/// A left drag through screen points, one motion event per point.
fn drag_screen(controller: &mut AppController, screen: &[(f32, f32)]) {
    press(controller, screen[0]);
    for &(x, y) in &screen[1..] {
        controller.pointer_moved(x, y);
    }
    release(controller, *screen.last().unwrap());
    common::settle_region_renders(controller);
}

fn ops(controller: &AppController) -> Vec<Op> {
    controller.edit_session().unwrap().ops().cloned().collect()
}

/// `rect` of the RGB TIFF at `path`.
fn read_plane(path: &Path, rect: Rect) -> Vec<u16> {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).unwrap();
    let mut plane = vec![0u16; (rect.width * rect.height * 3) as usize];
    stack.read_tile_into(0, rect, &mut plane).unwrap();
    plane
}

/// The tab's sealed planes and raw frames, read straight from disk, plus the
/// registration a stroke's own region render reads each raw frame through.
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
        overlay_edited(&mut samples, self.map, NonZeroU32::MIN, 3, view.tiles);
        samples
    }

    /// Frame `frame` over the whole map, read at its own registered
    /// transform and the crop origin, the same view a stroke blends from.
    /// `dummy-align`'s shifts are exact integer translations, so the mapped
    /// point always lands on a source pixel centre with no rounding error.
    fn frame_view(&self, frame: usize) -> Vec<u16> {
        let origin = self.registration.origin();
        let mut out = vec![0u16; (self.map.width * self.map.height * 3) as usize];
        for i in 0..(self.map.width * self.map.height) as usize {
            let (x, y) = (i as u32 % self.map.width, i as u32 / self.map.width);
            let (rx, ry) = ((x + origin.0) as f64, (y + origin.1) as f64);
            let (fx, fy) = self
                .registration
                .transform(frame)
                .map_point(rx, ry)
                .expect("dummy-align's integer shifts never cross the horizon");
            let f = ((fy.round() as u32 * FRAME + fx.round() as u32) * 3) as usize;
            out[i * 3..i * 3 + 3].copy_from_slice(&self.frames[frame][f..f + 3]);
        }
        out
    }

    /// `op` blended over `out`, a whole-map composite, from its frame.
    fn blend_over(&self, out: &mut [u16], op: &Op, info: ZMapInfo) {
        let Op::BrushStroke { source_id, .. } = op else {
            panic!("only brush strokes are blended");
        };
        let src = self.frame_view(usize::from(*source_id));
        blend::apply(op, info, self.map, out, self.map, &src, 3).unwrap();
    }

    /// The reference: the sealed composite with each of `ops` blended over
    /// it in order from its frame.
    fn expected(&self, ops: &[Op], info: ZMapInfo) -> Vec<u16> {
        let mut out = self.composite.clone();
        for op in ops {
            self.blend_over(&mut out, op, info);
        }
        out
    }

    fn assert_shown(&self, controller: &AppController, case: &str) {
        let info = controller.edit_session().unwrap().info();
        assert!(
            self.displayed(controller) == self.expected(&ops(controller), info),
            "{case}: the displayed composite differs from the reference"
        );
    }
}

/// What the renderer shows over the whole image: the tab's sealed composite
/// under the controller's resident tiles.
fn displayed_whole(controller: &mut AppController) -> Vec<u16> {
    let map = map_rect(controller);
    let (composite, _) = controller.materialize_active_tab_planes().unwrap();
    let mut samples = read_plane(&composite, map);
    let view = controller.edited_view().unwrap();
    overlay_edited(&mut samples, map, NonZeroU32::MIN, 3, view.tiles);
    samples
}

/// The head checkpoint of the saved project at `path`, rendered whole by the
/// one function every export and view goes through.
fn saved_render(path: &Path) -> Vec<u16> {
    let mut project = saved(path);
    let id = project.checkpoint().id.clone();
    let image = project::CheckpointImage::prepare(&project, &id).expect("the checkpoint prepares");
    let full = image.full();
    let mut out = vec![0u16; (full.width * full.height * 3) as usize];
    image
        .render(Some(&mut project), full, &mut |band: &[u16], rect: Rect| {
            let row = rect.width as usize * 3;
            for y in 0..rect.height as usize {
                let at = ((rect.y as usize + y) * full.width as usize + rect.x as usize) * 3;
                out[at..at + row].copy_from_slice(&band[y * row..(y + 1) * row]);
            }
            Ok(())
        })
        .expect("the checkpoint renders");
    out
}

/// A drag through image points `path` with a flush and a settle after every
/// motion, so each segment is a tick of its own.
fn drag_ticked(controller: &mut AppController, path: &[(f32, f32)]) {
    press(controller, at(path[0].0, path[0].1));
    for &(x, y) in &path[1..] {
        let (sx, sy) = at(x, y);
        controller.pointer_moved(sx, sy);
        controller.flush_live_stroke();
        common::settle_region_renders(controller);
    }
    let last = path[path.len() - 1];
    release(controller, at(last.0, last.1));
    common::settle_region_renders(controller);
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

    press(&mut c, (100.0, 100.0));
    c.pointer_moved(RIGHT.x + 50.0, 100.0);
    c.flush_live_stroke();
    release(&mut c, (RIGHT.x + 50.0, 100.0));
    common::settle_region_renders(&mut c);
    assert_eq!(ops(&c).len(), 3, "a drag starting on the left pane");
    assert!(
        matches!(ops(&c).last(), Some(Op::BrushStroke { source_id: 2, .. })),
        "it paints the selected source"
    );
    common::settle_region_renders(&mut c);
    let before = displayed_whole(&mut c);

    // In the band above the image, with a 1 px brush.
    set_size(&mut c, 1.0);
    press(&mut c, (RIGHT.x + 100.0, 8.0));
    c.pointer_moved(RIGHT.x + 140.0, 20.0);
    c.flush_live_stroke();
    release(&mut c, (RIGHT.x + 140.0, 20.0));
    common::settle_region_renders(&mut c);
    assert_eq!(ops(&c).len(), 3, "a stroke wholly off the image");
    assert_eq!(displayed_whole(&mut c), before);
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
        "01.tif",
        "the same wheel steps the source in editor mode too, with no Ctrl needed"
    );
}

#[test]
fn image_preview_opening_a_short_list_after_a_scrolled_long_list_keeps_bounds_valid() {
    let mut controller = controller_with_scratch("image_preview_tab_window");
    open_many_timed_frames(&mut controller, "image_preview_first", 1_000);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.set_images_viewport_height(120.0);
    controller.select_frame(500);
    controller.images_viewport_scrolled(500.0 * 120.0);

    open_many_timed_frames(&mut controller, "image_preview_short", 1);
    assert_eq!(controller.frame_pane_label(), "0000.tif");
    assert_eq!(controller.images_scroll_y(), 0.0);
}

#[test]
fn image_preview_opening_an_empty_list_after_a_scrolled_long_list_keeps_bounds_valid() {
    let mut controller = controller_with_scratch("image_preview_empty_tab_window");
    open_many_timed_frames(&mut controller, "image_preview_empty_first", 1_000);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.set_images_viewport_height(120.0);
    controller.select_frame(500);
    controller.images_viewport_scrolled(500.0 * 120.0);

    open_many_timed_frames(&mut controller, "image_preview_empty_second", 1);
    controller.remove_frame(0);
    assert!(controller.images_panel().rows.is_empty());

    controller.select_project(0);
    controller.select_project(1);
    assert!(controller.images_panel().rows.is_empty());
    assert_eq!(controller.images_scroll_y(), 0.0);
}

#[test]
fn tab_switches_and_active_closes_reanchor_image_preview_windows() {
    let mut controller = controller_with_scratch("image_preview_tab_window");
    open_many_timed_frames(&mut controller, "image_preview_first", 1_000);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.set_images_viewport_height(120.0);
    controller.select_frame(500);
    controller.images_viewport_scrolled(500.0 * 120.0);

    open_many_timed_frames(&mut controller, "image_preview_second", 1_000);
    controller.select_frame(800);
    controller.images_viewport_scrolled(800.0 * 120.0);
    controller.select_project(0);
    assert_eq!(controller.frame_pane_label(), "0500.tif");
    assert_eq!(controller.images_scroll_y(), 500.0 * 120.0);

    controller.select_project(1);
    controller.close_project(1);
    assert_eq!(controller.active_project(), Some(0));
    assert_eq!(controller.frame_pane_label(), "0500.tif");
    controller.refresh_source_previews();
    assert_eq!(controller.images_scroll_y(), 500.0 * 120.0);
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
        ops: Arc::from([]),
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
            ops: Arc::from([]),
            alpha: focusforge_core::display::alpha_from_opacity(0.3),
        }),
        "matches display::overlay_cpu's own blend, with no solo source"
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
    let moved = c.brush_cursor_view().unwrap();
    assert_eq!(moved.extent, here.extent);
    assert_eq!(moved.ring_commands, here.ring_commands);
    assert_eq!(moved.core_commands, here.core_commands);
    assert_ne!(moved.composite_position, here.composite_position);
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
    common::enter_editor(&mut c);

    set_size(&mut c, 4.0);
    c.select_frame(1);
    drag(&mut c, &[(40.0, 40.0), (50.0, 44.0)]);
    c.select_frame(2);
    drag(&mut c, &[(20.0, 50.0), (30.0, 55.0)]);
    c.undo();
    let state = |c: &AppController| (ops(c), c.edit_session().unwrap().can_redo());
    let before = state(&c);
    assert!(before.1, "one stroke waits on the redo stack");
    let differing = |source: u16| {
        (8..height)
            .flat_map(|y| (8..width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                let index = sealed[(y * width + x) as usize];
                index != source && index != NO_SOURCE
            })
            .map(|(x, y)| ((x as f32, y as f32), sealed[(y * width + x) as usize]))
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

/// A saved fuse row whose Z-Map blob fails to decode still renders, opens
/// the editor and takes strokes; only the overlay and the pipette's reading
/// off every stroke are lost.
#[test]
fn zmap_unreadable_leaves_the_image_working() {
    use focusforge_gui::canvas::CanvasFeed;

    let mut c = controller_with_scratch("zmap_unreadable");
    open_fused_tab(&mut c, "zmap_unreadable_frames", 3);
    let (path, older, _head) = common::save_two_fuses(&mut c);
    common::corrupt_blob(&path, &saved_zmap_blob(&path, &older));

    let mut c = reopen(&path, "zmap_unreadable_reopened");
    c.set_pane_geometry(LEFT, RIGHT);
    c.set_overlay_on(true);
    let row = row_index(&c, &older);
    c.select_checkpoint_row(row);
    assert!(matches!(c.canvas_feed(None), CanvasFeed::Clear));
    assert!(c.overlay_available(), "the row's planes are still pending");
    common::settle_render(&mut c);
    match c.canvas_feed(None) {
        CanvasFeed::Load {
            composite, zmap, ..
        } => {
            assert!(composite.is_file(), "the composite loads");
            assert_eq!(zmap, None, "without a Z-Map");
        }
        other => panic!("the settled row must load, got {other:?}"),
    }
    assert!(!c.overlay_available(), "no Z-Map, no overlay");
    assert_eq!(c.overlay_draw(), None);

    common::enter_editor(&mut c);
    c.select_frame(1);
    common::paint(
        &mut c,
        Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 20, y: 20 }],
            radius: 4.0,
            softness: 0.0,
        },
    );
    assert_eq!(ops(&c).len(), 1, "a stroke appends");
    let (sx, sy) = at(50.0, 50.0);
    c.pointer(PointerKind::Down, Button::Left, sx, sy, ALT);
    c.pointer(PointerKind::Up, Button::Left, sx, sy, ALT);
    assert_eq!(
        c.brush_source(),
        Some(1),
        "a pick off every stroke reads nothing"
    );
}

#[test]
fn pipette_reads_the_overlay() {
    let (mut c, _) = open_editor("pipette_overlay");
    let zmap = c.held_checkpoints().last().unwrap().zmap.clone();
    let stored = read_whole_zmap(&zmap);
    let info = ZMapReader::open(&zmap).unwrap().info();
    let (width, height) = (info.width(), info.height());
    let (x, y, under) = (8..height - 8)
        .flat_map(|y| (8..width - 8).map(move |x| (x, y)))
        .find_map(|(x, y)| {
            let source = stored[(y * width + x) as usize];
            (source != NO_SOURCE).then_some((x, y, source))
        })
        .expect("the fused map names a source away from its edge");
    let painter = (under + 1) % 3;
    set_size(&mut c, 8.0);
    c.set_brush_softness_fraction(0.0);
    c.select_frame(usize::from(painter));
    let (px, py) = (x as f32, y as f32);
    drag(&mut c, &[(px - 3.0, py), (px, py), (px + 3.0, py)]);
    assert!(
        matches!(
            ops(&c)[..],
            [Op::BrushStroke { source_id, .. }] if source_id == painter
        ),
        "a stroke from another frame covers the pixel"
    );

    // A pixel clear of the stroke whose stored frame is neither the painter's
    // nor absent.
    let (ox, oy, off) = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .find_map(|(x, y)| {
            let source = stored[(y * width + x) as usize];
            let clear = x.abs_diff(px as u32) > 16 || y.abs_diff(py as u32) > 16;
            (clear && source != NO_SOURCE && source != painter).then_some((x, y, source))
        })
        .expect("the map names another frame clear of the stroke");
    let (off_x, off_y) = (ox as f32, oy as f32);

    let alt_click = |c: &mut AppController, (x, y): (f32, f32)| {
        let (sx, sy) = at(x, y);
        c.pointer(PointerKind::Down, Button::Left, sx, sy, ALT);
        c.pointer(PointerKind::Up, Button::Left, sx, sy, ALT);
    };
    c.select_frame(usize::from(under));
    alt_click(&mut c, (px, py));
    assert_eq!(
        c.brush_source(),
        Some(painter),
        "Alt + click over the stroke reads the frame the overlay shows"
    );
    c.select_frame(usize::from(painter));
    alt_click(&mut c, (off_x, off_y));
    assert_eq!(
        c.brush_source(),
        Some(off),
        "Alt + click off every stroke reads the stored map"
    );

    c.select_frame(usize::from(under));
    c.toggle_pipette();
    let (sx, sy) = at(px, py);
    c.pointer(PointerKind::Move, Button::Left, sx, sy, NO_MODS);
    assert_eq!(
        c.brush_cursor_view().unwrap().pick_color,
        job::source_color(painter, c.frame_count().unwrap()),
        "the armed cursor previews the overlay's frame"
    );
    let (sx, sy) = at(off_x, off_y);
    c.pointer(PointerKind::Move, Button::Left, sx, sy, NO_MODS);
    assert_eq!(
        c.brush_cursor_view().unwrap().pick_color,
        job::source_color(off, c.frame_count().unwrap()),
        "off the stroke the cursor previews the stored frame"
    );
    drag(&mut c, &[(px, py)]);
    assert_eq!(c.brush_source(), Some(painter), "the armed pick");
    assert_eq!(ops(&c).len(), 1, "picking appends nothing");
}

/// The overlay's colour at every pixel of the map: the active tab's
/// `overlay_draw()` over the stored indices at `zmap`.
fn overlay_pixels(c: &AppController, zmap: &Path) -> Vec<[u8; 3]> {
    let draw = c.overlay_draw().expect("the overlay is on");
    let info = ZMapReader::open(zmap).unwrap().info();
    let whole = Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    };
    let stored = read_whole_zmap(zmap);
    let mut colors = vec![[0u8; 3]; stored.len()];
    focusforge_core::retouch::overlay_colors(
        &draw.ops,
        info,
        whole,
        NonZeroU32::MIN,
        &draw.table,
        &stored,
        &mut colors,
    );
    colors
}

#[test]
fn live_overlay_follows_the_drag() {
    let (mut c, _) = open_editor("live_overlay_drag");
    let zmap = c.held_checkpoints().last().unwrap().zmap.clone();
    let info = ZMapReader::open(&zmap).unwrap().info();
    let width = info.width() as usize;
    let table = focusforge_core::zmap::overlay_table(c.frame_count().unwrap(), None);
    c.set_overlay_on(true);
    set_size(&mut c, 8.0);
    c.set_brush_softness_fraction(0.0);
    let untouched = overlay_pixels(&c, &zmap);
    assert!(
        c.overlay_draw().unwrap().ops.is_empty(),
        "no strokes, no ops"
    );

    c.select_frame(2);
    press(&mut c, at(12.0, 20.0));
    let (sx, sy) = at(30.0, 20.0);
    c.pointer_moved(sx, sy);
    c.flush_live_stroke();
    common::settle_region_renders(&mut c);
    let held = c.overlay_draw().unwrap().ops;
    assert!(ops(&c).is_empty(), "the stroke is not an op while held");
    let [
        Op::BrushStroke {
            source_id, path, ..
        },
    ] = &held[..]
    else {
        panic!("the held overlay draws the live stroke: {held:?}");
    };
    assert_eq!(*source_id, 2);
    assert_eq!(
        path.last(),
        Some(&Point { x: 30, y: 20 }),
        "its path so far"
    );
    let mid = overlay_pixels(&c, &zmap);
    assert_eq!(mid[20 * width + 25], table[2], "the core shows the painter");
    assert_eq!(mid[0], untouched[0], "far from the stroke the map shows");

    release(&mut c, at(30.0, 20.0));
    common::settle_region_renders(&mut c);
    assert_eq!(
        c.overlay_draw().unwrap().ops.to_vec(),
        c.edit_session().unwrap().drawn_ops(),
        "after release the overlay lists the session's drawn ops"
    );
    assert_eq!(ops(&c).len(), 1);

    c.select_frame(1);
    drag(&mut c, &[(24.0, 14.0), (24.0, 30.0)]);
    let both = overlay_pixels(&c, &zmap);
    assert_eq!(
        both[24 * width + 24],
        table[1],
        "the later stroke is on top"
    );
    assert_eq!(both[20 * width + 14], table[2], "the first still shows");

    c.undo();
    assert_eq!(c.overlay_draw().unwrap().ops.len(), 1);
    assert_eq!(
        overlay_pixels(&c, &zmap)[24 * width + 24],
        untouched[24 * width + 24],
        "undo takes the later stroke's colour away"
    );
    c.redo();
    assert_eq!(overlay_pixels(&c, &zmap), both, "redo restores it");
}

#[test]
fn live_overlay_shows_the_selected_checkpoint() {
    let (path, chain) = saved_chain("live_overlay_selected", &[20, 40]);
    let saved_ops: Vec<Op> = saved(&path)
        .history()
        .op_chain(&chain[2])
        .unwrap()
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(saved_ops.len(), 2);

    let mut c = reopen(&path, "live_overlay_selected_reopened");
    c.set_overlay_on(true);
    let shown = |c: &AppController| c.overlay_draw().expect("the overlay is on").ops.to_vec();
    c.select_checkpoint_row(row_index(&c, &chain[2]));
    assert_eq!(shown(&c), saved_ops, "a saved row with no session");
    c.select_checkpoint_row(row_index(&c, &chain[1]));
    assert_eq!(shown(&c), saved_ops[..1], "its parent's chain");
    c.select_checkpoint_row(row_index(&c, &chain[0]));
    assert!(shown(&c).is_empty(), "the unedited root carries none");

    c.select_checkpoint_row(row_index(&c, &chain[2]));
    c.set_pane_geometry(LEFT, RIGHT);
    common::enter_editor(&mut c);
    assert_eq!(shown(&c), saved_ops, "entering the editor keeps them");
    set_size(&mut c, 8.0);
    c.select_frame(1);
    drag(&mut c, &[(10.0, 44.0), (30.0, 44.0)]);
    let with_new = shown(&c);
    assert_eq!(with_new.len(), 3);
    assert_eq!(with_new[..2], saved_ops[..]);
    assert!(c.save_active_project(|_| None));
    assert_eq!(shown(&c), with_new, "each stroke once after a save");

    // A held op-bearing row draws its chain through its session.
    let mut held = open_editor("live_overlay_held").0;
    held.set_overlay_on(true);
    set_size(&mut held, 8.0);
    held.select_frame(1);
    drag(&mut held, &[(12.0, 20.0), (36.0, 26.0)]);
    held.select_frame(2);
    drag(&mut held, &[(16.0, 34.0), (40.0, 30.0)]);
    let strokes = ops(&held);
    assert!(held.save_checkpoint());
    let ops_row = row_index(&held, "held:1");
    held.select_checkpoint_row(row_index(&held, "held:0"));
    held.select_checkpoint_row(ops_row);
    assert_eq!(shown(&held), strokes, "a held row with no session");
    common::enter_editor(&mut held);
    assert_eq!(shown(&held), strokes, "and through its session");
}

#[test]
fn live_overlay_never_reaches_save_or_export() {
    let scenario = |overlay: bool, name: &str| {
        let (mut c, root) = open_editor(name);
        let sealed = Sealed::read(&mut c, &root);
        set_size(&mut c, 12.0);
        c.set_brush_softness_fraction(0.5);
        c.select_frame(0);
        drag(&mut c, &[(40.0, 50.0), (50.0, 54.0)]);
        let path = save_to_proposed(&mut c);
        c.select_frame(1);
        drag(&mut c, &[(10.0, 12.0), (30.0, 20.0)]);
        c.select_frame(2);
        drag(&mut c, &[(20.0, 40.0), (44.0, 30.0)]);
        let before = fs::read(&path).unwrap();
        c.set_overlay_on(true);
        c.set_overlay_opacity_fraction(0.8);
        c.set_overlay_on(false);
        assert!(
            fs::read(&path).unwrap() == before,
            "toggling the overlay writes nothing"
        );
        c.set_overlay_on(overlay);
        let shown = sealed.displayed(&c);
        assert!(c.save_active_project(|_| None));
        let render = saved_render(&path);
        assert!(
            shown == render,
            "the live view and the saved render ignore the overlay"
        );
        (render, ops(&c).len(), saved(&path).checkpoint().ops.clone())
    };
    let off = scenario(false, "live_overlay_off");
    let on = scenario(true, "live_overlay_on");
    assert!(on.0 == off.0, "the render is the same with the overlay on");
    assert_eq!(on.2, off.2, "the saved ops are the same");
    assert_eq!(on.2.len(), 2);
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
// The mirrored cursor: one source point projected into each pane.
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
/// the projected point in `other` -- lands inside
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
    let cursor = c.brush_cursor_view().unwrap();
    let local = if other.x > home.x {
        cursor.composite_position
    } else {
        cursor.frame_position
    };
    let mirrored = (other.x + local.0, other.y + local.1);
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

/// The selected frame's pixel at image `(x, y)`, read as the left pane draws it.
fn selected_frame_pixel(c: &AppController, x: u32, y: u32) -> Vec<u16> {
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
    let mut pixel = vec![0u16; 3];
    source
        .read_into(
            Rect {
                x,
                y,
                width: 1,
                height: 1,
            },
            NonZeroU32::MIN,
            &mut pixel,
        )
        .expect("the selected frame's point reads");
    pixel
}

/// A drag over the left pane and the same drag shifted to the cursor's
/// mirrored point over the right pane append equal ops and show equal pixels,
/// at fit, one zoom step in, after a pan and with a wider gutter; the left
/// pane's frame is never written.
#[test]
fn mirrored_brush_paints_the_same_stroke_from_either_pane() {
    let (mut c, _) = open_editor("mirrored_brush_either_pane");
    set_size(&mut c, 6.0);
    c.select_frame(2);

    let check_stage = |c: &mut AppController, right: PaneRect, stage: &str| {
        let start = at_pane(LEFT, 29.0, 30.0);
        c.pointer(PointerKind::Move, Button::Left, start.0, start.1, NO_MODS);
        let mirrored = c.brush_cursor_view().unwrap().composite_position;
        let mirror_dx = right.x + mirrored.0 - start.0;
        let mirror_dy = right.y + mirrored.1 - start.1;
        let left_path: Vec<(f32, f32)> = (0..4)
            .map(|i| (start.0 + 6.0 * i as f32, start.1 + 3.0 * i as f32))
            .collect();
        let right_path: Vec<(f32, f32)> = left_path
            .iter()
            .map(|&(x, y)| (x + mirror_dx, y + mirror_dy))
            .collect();
        let ops_before = ops(c).len();

        drag_screen(c, &right_path);
        let right_ops = ops(c);
        let right_image = displayed_whole(c);
        assert_eq!(
            right_ops.len(),
            ops_before + 1,
            "{stage}: the right-pane stroke"
        );
        c.undo();
        common::settle_region_renders(c);
        assert_eq!(ops(c).len(), ops_before, "{stage}: undo");

        let frame_before = selected_frame_pixel(c, 29, 30);
        drag_screen(c, &left_path);
        assert_eq!(ops(c), right_ops, "{stage}: the left pane's op");
        assert!(
            displayed_whole(c) == right_image,
            "{stage}: the left pane's pixels"
        );
        assert_eq!(
            selected_frame_pixel(c, 29, 30),
            frame_before,
            "{stage}: the left pane's frame is unchanged"
        );
        c.undo();
        common::settle_region_renders(c);
    };

    check_stage(&mut c, RIGHT, "at fit");
    c.zoom_in();
    check_stage(&mut c, RIGHT, "one zoom step in");
    c.wheel(0.0, 60.0, 0.0, 0.0, SHIFT);
    check_stage(&mut c, RIGHT, "after a pan");
    let wider = PaneRect {
        x: RIGHT.x + 16.0,
        ..RIGHT
    };
    c.set_pane_geometry(LEFT, wider);
    check_stage(&mut c, wider, "a wider gutter");
}

/// A press in the gutter or outside both panes opens no stroke, even when the
/// drag then moves into a pane.
#[test]
fn a_press_between_the_panes_opens_no_stroke() {
    let (mut c, _) = open_editor("press_between_panes");
    set_size(&mut c, 8.0);
    c.select_frame(2);
    let before_ops = ops(&c);
    let inside = at(30.0, 30.0);

    for (case, start) in [
        ("the gutter", (LEFT.x + LEFT.width + 4.0, 160.0)),
        ("below both panes", (RIGHT.x + 50.0, 330.0)),
        (
            "right of the right pane",
            (RIGHT.x + RIGHT.width + 4.0, 160.0),
        ),
    ] {
        press(&mut c, start);
        c.pointer_moved(inside.0, inside.1);
        c.flush_live_stroke();
        release(&mut c, inside);
        common::settle_region_renders(&mut c);
        assert_eq!(ops(&c), before_ops, "a press in {case}");
    }
}

#[test]
fn mirrored_cursor_projects_into_unequal_window_shapes() {
    let (mut c, _) = open_editor("mirrored_cursor_unequal_windows");
    for (width, height) in [(80.0, 160.0), (160.0, 80.0)] {
        let detached = PaneRect {
            width,
            height,
            ..RIGHT
        };
        c.set_pane_geometry(LEFT, detached);
        c.fit();
        let left_center = (LEFT.x + LEFT.width / 2.0, LEFT.y + LEFT.height / 2.0);
        let right_center = (detached.x + width / 2.0, detached.y + height / 2.0);
        assert_scene_point_matches(&mut c, LEFT, detached, left_center, "main to detached");
        assert_scene_point_matches(&mut c, detached, LEFT, right_center, "detached to main");
    }
}

/// The one `BrushCursorView` carries both cursors' geometry: size, softness
/// and zoom change its extent/ring/core; both positions follow the pointer.
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
    assert_eq!(after_size.frame_position, before.frame_position);
    assert_eq!(after_size.composite_position, before.composite_position);

    c.set_brush_softness_fraction(0.75);
    let after_soft = c.brush_cursor_view().unwrap();
    assert_ne!(
        after_soft.core_commands, after_size.core_commands,
        "softness changes the core"
    );
    assert_eq!(after_soft.frame_position, before.frame_position);
    assert_eq!(after_soft.composite_position, before.composite_position);

    c.zoom_in();
    let after_zoom = c.brush_cursor_view().unwrap();
    assert_ne!(
        after_zoom.extent, after_soft.extent,
        "zoom changes the shared geometry"
    );

    let (x, y) = at(5.0, 5.0);
    c.pointer(PointerKind::Move, Button::Left, x, y, NO_MODS);
    let after_pointer = c.brush_cursor_view().unwrap();
    assert_eq!(after_pointer.extent, after_zoom.extent);
    assert_eq!(after_pointer.ring_commands, after_zoom.ring_commands);
    assert_eq!(after_pointer.core_commands, after_zoom.core_commands);
    assert_ne!(after_pointer.frame_position, after_zoom.frame_position);
}

/// With the pipette armed, the left pane's hover and the right pane's hover
/// at the projected right-pane point preview the same colour, the one an
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

    let right_point = (
        RIGHT.x + left_hover.composite_position.0,
        RIGHT.y + left_hover.composite_position.1,
    );
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

    // An Alt + click leaves the pipette armed; a plain press picks and disarms.
    assert!(c.brush_cursor_view().unwrap().picking, "still armed");
    let other_point = at_pane(LEFT, 5.0, 50.0);
    c.pointer(
        PointerKind::Move,
        Button::Left,
        other_point.0,
        other_point.1,
        NO_MODS,
    );
    let other_hover = c.brush_cursor_view().unwrap();
    press(&mut c, other_point);
    release(&mut c, other_point);
    let picked = c.brush_source().expect("a left-pane press commits a pick");
    assert_eq!(
        other_hover.pick_color,
        job::source_color(picked, c.frame_count().unwrap()),
        "the left-pane press commits the previewed source"
    );
    assert!(
        !c.brush_cursor_view().unwrap().picking,
        "the pick disarms the pipette"
    );
    assert_eq!(ops(&c), before, "a left-pane pick opens no stroke");
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
    common::settle_region_renders(&mut c);
    let after_both = sealed.displayed(&c);

    for (undo_key, redo_key) in [("z", "Z"), ("Z", "z")] {
        c.key(true, undo_key, CTRL);
        assert_eq!(ops(&c), both[..1], "undo as {undo_key:?}");
        assert!(c.brush_toolbar_view().can_redo);
        c.key(true, redo_key, CTRL_SHIFT);
        assert_eq!(ops(&c), both, "redo as {redo_key:?}");
        common::settle_region_renders(&mut c);
        assert!(sealed.displayed(&c) == after_both, "redo as {redo_key:?}");
    }

    c.key(true, "z", CTRL);
    c.key(true, "z", CTRL);
    assert!(ops(&c).is_empty(), "undo is exact");
    common::settle_region_renders(&mut c);
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
        .filter(|event| !matches!(event, ShellEvent::Input(_) | ShellEvent::RegionRenderReady))
        .cloned()
        .collect();
    assert_eq!(
        said,
        vec![ShellEvent::EditorChanged],
        "nothing announces it"
    );

    let now = ops(&c);
    c.key(true, "z", CTRL_SHIFT);
    assert_eq!(ops(&c), now, "nothing left to redo");
    common::settle_region_renders(&mut c);
    sealed.assert_shown(&c, "after the new stroke");
}

#[test]
fn stroke_commit_renders_its_tick_rects() {
    let (mut c, root) = open_editor("brush_commit");
    let sealed = Sealed::read(&mut c, &root);
    set_size(&mut c, 20.0);
    c.set_brush_softness_fraction(0.3);
    c.select_frame(1);
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
        common::settle_region_renders(&mut c);
        let tick = c.take_rendered_regions();
        assert_eq!(tick.len(), 1, "one region render per flush");
        ticks.extend(tick);
    }
    release(&mut c, *screen.last().unwrap());
    common::settle_region_renders(&mut c);

    let commit = c.take_rendered_regions();
    let map = map_rect(&c);
    let op = ops(&c).pop().unwrap();
    let bbox = footprint(&op, info).unwrap().unwrap();
    let mut cells = std::collections::BTreeSet::new();
    for r in ticks.iter().chain([&bbox]) {
        for tx in r.x / 16..=(r.x + r.width - 1) / 16 {
            for ty in r.y / 16..=(r.y + r.height - 1) / 16 {
                cells.insert((tx, ty));
            }
        }
    }
    let expected: Vec<Rect> = cells
        .into_iter()
        .map(|(tx, ty)| Rect {
            x: tx * 16,
            y: ty * 16,
            width: 16.min(map.width - tx * 16),
            height: 16.min(map.height - ty * 16),
        })
        .collect();
    assert_eq!(commit, expected, "one pass over the touched 16 px tiles");
    assert!(!commit.contains(&bbox), "never the stroke's bounding box");
    sealed.assert_shown(&c, "after release");

    c.undo();
    common::settle_region_renders(&mut c);
    assert_eq!(
        c.take_rendered_regions(),
        vec![bbox],
        "undo renders its region"
    );
    sealed.assert_shown(&c, "after undo");
    assert!(sealed.displayed(&c) == sealed.composite);
    c.redo();
    common::settle_region_renders(&mut c);
    assert_eq!(
        c.take_rendered_regions(),
        vec![bbox],
        "redo renders its region"
    );
    sealed.assert_shown(&c, "after redo");
}

#[test]
fn every_edit_has_a_cli_equivalent() {
    let (mut c, root) = open_editor("brush_cli");
    let sealed = Sealed::read(&mut c, &root);
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
    common::settle_region_renders(&mut c);

    let json = serde_json::to_string(&ops).unwrap();
    let parsed: Vec<Op> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, ops);

    let info = c.edit_session().unwrap().info();
    assert!(
        sealed.displayed(&c) == sealed.expected(&parsed, info),
        "the serialized ops replay to the displayed image"
    );
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
    common::settle_region_renders(&mut c);
    c.take_rendered_regions();
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
    common::settle_region_renders(&mut c);
    assert!(
        c.take_rendered_regions().is_empty(),
        "a flush with no motion renders nothing"
    );

    let mut flushed = 0;
    for (k, end) in [4usize, 5, 13, 21, 22, 30, 44].into_iter().enumerate() {
        for &(x, y) in &trace[flushed + 1..=end] {
            let (sx, sy) = at(x, y);
            c.pointer_moved(sx, sy);
        }
        if k == 2 {
            let held = sealed.displayed(&c);
            c.key(true, "z", CTRL);
            c.key(true, "z", CTRL_SHIFT);
            assert!(
                sealed.displayed(&c) == held,
                "undo/redo mid-drag change nothing"
            );
            assert_eq!(session(&c), before);
            c.wheel(0.0, 60.0, 0.0, 0.0, SHIFT);
            c.wheel(0.0, 60.0, 0.0, 0.0, ALT);
            c.wheel(0.0, 60.0, 0.0, 0.0, CTRL);
            assert_ne!(c.brush_toolbar_view().size, view.size);
            assert_ne!(c.brush_toolbar_view().soft, view.soft);
            assert_ne!(c.brush_source(), Some(source));
        }
        c.flush_live_stroke();
        common::settle_region_renders(&mut c);
        let segment = stroke(rounded(&trace[flushed..=end]));
        let region = footprint(&segment, info).unwrap();
        assert_eq!(
            c.take_rendered_regions(),
            region.into_iter().collect::<Vec<_>>(),
            "flush {k}: exactly the new segment's footprint, once"
        );
        let mut shown = sealed.expected(&before.1, info);
        sealed.blend_over(&mut shown, &stroke(rounded(&trace[..=end])), info);
        assert!(
            sealed.displayed(&c) == shown,
            "flush {k}: the blended trace so far over the pre-stroke samples"
        );
        assert_eq!(
            session(&c),
            before,
            "flush {k}: the op list waits for release"
        );
        c.flush_live_stroke();
        common::settle_region_renders(&mut c);
        assert!(
            c.take_rendered_regions().is_empty(),
            "flush {k}: nothing new"
        );
        flushed = end;
    }

    release(&mut c, at(trace[44].0, trace[44].1));
    common::settle_region_renders(&mut c);
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
    sealed.assert_shown(&c, "after release");
}

/// A brush stroke that paints frame `i` renders the composite from the
/// frame-as-fused view -- the pixel under the stroke equals a
/// [`FrameSource`] read of that frame through `selected_frame_view` at the
/// same point.
///
/// [`FrameSource`]: focusforge_gui::canvas::FrameSource
#[test]
fn stroke_matches_left_pane() {
    // The Z-Map names the source of every pixel only for a method that
    // selects pixels; a pyramid blend mixes neighbours at coarse levels.
    let (mut c, root) = open_editor_on("stroke_matches_left_pane", Some("depth-map"));
    c.select_frame(2);
    set_size(&mut c, 8.0);
    drag(&mut c, &[(20.0, 20.0)]);

    let sealed = Sealed::read(&mut c, &root);
    let point = (20 * sealed.map.width + 20) as usize;
    let stroked = sealed.displayed(&c)[point * 3..point * 3 + 3].to_vec();

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
        stroked, left_pixel,
        "the stroked pixel must equal the left pane's own read at the same point"
    );
}

/// Fuses `count` mantis-64 frames on `depth-map` with equalization on, then
/// opens the editor over the run.
fn open_equalized_editor(name: &str, count: usize) -> AppController {
    use focusforge_core::params::ParamValue;

    let mut controller = controller_with_scratch(name);
    controller.set_resident_tile_size_for_test(16);
    open_mantis64_tab(&mut controller, name, count);
    controller.set_fuse_algorithm("depth-map");
    controller.set_param_value("prepare", "equalize", ParamValue::Bool(true));
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert_eq!(controller.checkpoint_count(), Some(1), "{name} must fuse");
    controller.set_pane_geometry(LEFT, RIGHT);
    assert!(controller.set_mode(ShellMode::Editor));
    controller
}

/// The prepare section lists the equalization switch (on), its bound and its
/// target; a default run is equalized, switching it off records an unequalized
/// run, and the bound and target the panel holds reach the run.
#[test]
fn equalize_rows_reach_the_run() {
    use focusforge_core::params::ParamValue;

    let mut controller = common::controller_with_frames("equalize_rows", 3);
    // The default aligner parks these frames on a diverged one; a run that
    // always converges keeps the held checkpoint this test reads.
    controller.set_align_algorithm("dummy-align");
    controller.toggle_section_more("prepare");
    let rows: Vec<_> = controller
        .settings_panel()
        .rows
        .into_iter()
        .filter(|r| r.section == "prepare")
        .collect();
    let row = |id: &str| {
        rows.iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no {id} row"))
    };
    assert_eq!(row("equalize").selected, "true", "on by default");
    assert_eq!(row("equalize-max-gain").value_text, "1.25");

    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, SETTLE);
    let default_run = controller.held_checkpoints().last().cloned().unwrap();
    let default_equalization = default_run
        .registration
        .equalization
        .expect("a default run is equalized");
    assert_eq!(
        (
            default_equalization.max_gain,
            default_equalization.target_stops
        ),
        (1.25, 0.0),
        "a default run uses the default bound and the neutral target"
    );

    controller.set_param_value("prepare", "equalize", ParamValue::Bool(false));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, SETTLE);
    let off = controller.held_checkpoints().last().cloned().unwrap();
    assert_eq!(
        off.registration.equalization, None,
        "switched off, the run is not equalized"
    );

    controller.set_param_value("prepare", "equalize", ParamValue::Bool(true));
    controller.set_param_value("prepare", "equalize-max-gain", ParamValue::Float(1.5));
    controller.set_param_value("prepare", "equalize-target", ParamValue::Float(0.3));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, SETTLE);
    let on = controller.held_checkpoints().last().cloned().unwrap();
    let recorded = on.registration.equalization.expect("the run was equalized");
    assert_eq!(recorded.max_gain, 1.5);
    assert_eq!(recorded.target_stops, 0.3, "the target reaches the run");
    assert_eq!(recorded.gains.len(), 3, "one gain per frame");
}

/// A frame the equalization bound clamps is reported on the window's info
/// toast, naming the frame, the bound and the bound that would cover it; a
/// bound wide enough for the stack raises nothing.
#[test]
fn settings_journey_equalization_reports_a_clamped_frame() {
    use focusforge_core::equalize::UNIT_GAIN;
    use focusforge_core::params::ParamValue;

    let mut controller = common::controller_with_frames("equalize_clamped_notice", 8);
    // The default aligner parks eight mantis frames on a diverged one; a run
    // that always converges keeps the held checkpoint this test reads.
    controller.set_align_algorithm("dummy-align");
    controller.set_param_value("prepare", "equalize", ParamValue::Bool(true));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, SETTLE);
    assert_eq!(
        controller.held_checkpoints().len(),
        1,
        "the default run holds"
    );
    assert!(
        controller.notifications().is_empty(),
        "the default bound 1.25 covers the stack"
    );

    controller.set_param_value("prepare", "equalize-max-gain", ParamValue::Float(1.01));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, SETTLE);
    assert_eq!(
        controller.held_checkpoints().len(),
        2,
        "the tight run holds"
    );
    let notices = controller.notifications();
    assert!(notices.len() > 1, "each clamped frame has its own record");
    let notice = notices[0].message.as_str();
    assert!(notice.contains("frame "), "{notice}");
    assert!(notice.contains("1.01"), "{notice}");
    assert!(notice.contains("would cover it"), "{notice}");
    assert!(!notice.contains(" more)"), "no collapsed count: {notice}");

    let named: usize = notice
        .strip_prefix("frame ")
        .and_then(|rest| rest.split(':').next())
        .and_then(|index| index.parse().ok())
        .unwrap_or_else(|| panic!("the notice starts with its frame: {notice}"));
    let recorded = controller
        .held_checkpoints()
        .last()
        .and_then(|held| held.registration.equalization.clone())
        .expect("the tight run was equalized");
    let gain = f64::from(recorded.gains[named]) / f64::from(UNIT_GAIN);
    assert!(
        (gain - 1.01).abs() < 1e-3 || (gain - 1.0 / 1.01).abs() < 1e-3,
        "frame {named} sits at the bound, got {gain}"
    );
}

/// Three fuses on one tab, one per method: each held checkpoint carries its
/// own method, its own advanced value and the equalization bound, and no
/// method's value leaks into another's run.
#[test]
fn settings_journey_fuses_each_method_with_its_settings() {
    use focusforge_core::params::ParamValue;

    let mut controller = common::controller_with_frames("journey_each_method", 3);
    controller.set_align_algorithm("dummy-align");
    controller.toggle_section_more("fuse");
    controller.toggle_section_more("prepare");
    let methods = [
        ("laplacian", "levels", ParamValue::Int(4)),
        ("weighted-average", "radius", ParamValue::Int(6)),
        ("depth-map", "score-radius", ParamValue::Int(5)),
    ];

    for (index, (id, field, value)) in methods.iter().enumerate() {
        controller.set_fuse_algorithm(id);
        controller.set_param_value("fuse", field, value.clone());
        if *id != "depth-map" {
            controller.set_param_value("prepare", "equalize", ParamValue::Bool(true));
        }
        controller.set_param_value("prepare", "equalize-max-gain", ParamValue::Float(1.01));
        assert!(controller.start_fuse(), "{id} starts");
        common::wait_for_settle(&mut controller, SETTLE);
        let held = controller.held_checkpoints();
        assert_eq!(held.len(), index + 1, "{id} holds one more checkpoint");
        let run = held.last().unwrap();
        assert_eq!(run.algorithm, *id);
        assert_eq!(run.params.get(*field), Some(value), "{id}: {field}");
        for (other, other_field, _) in &methods {
            if other != id {
                assert_eq!(
                    run.params.get(*other_field),
                    None,
                    "{id} has no {other_field}"
                );
            }
        }
        let recorded = run
            .registration
            .equalization
            .as_ref()
            .unwrap_or_else(|| panic!("{id} was equalized"));
        assert_eq!(recorded.max_gain, 1.01, "{id}");
    }
}

/// A `depth-map` run equalizes without the switch being touched; turning the
/// switch off records an unequalized run.
#[test]
fn focus_map_run_equalizes_by_default_and_an_explicit_off_wins() {
    use focusforge_core::params::ParamValue;

    let mut controller = common::controller_with_frames("focus_map_equalize_default", 3);
    // The default aligner parks these frames on a diverged one; a run that
    // always converges keeps the held checkpoint this test reads.
    controller.set_align_algorithm("dummy-align");
    controller.set_fuse_algorithm("depth-map");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, SETTLE);
    let on = controller.held_checkpoints().last().cloned().unwrap();
    assert!(
        on.registration.equalization.is_some(),
        "equalized by default"
    );

    controller.set_param_value("prepare", "equalize", ParamValue::Bool(false));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, SETTLE);
    let off = controller.held_checkpoints().last().cloned().unwrap();
    assert_eq!(off.registration.equalization, None, "an explicit off wins");
}

/// On an equalized `depth-map` tab a stroke's rendered pixel is the left
/// pane's read of that frame, and that read carries the frame's gain.
#[test]
fn equalized_stroke_matches_left_pane() {
    use focusforge_core::equalize::UNIT_GAIN;
    use focusforge_gui::canvas::{FrameSource, FrameView};

    let mut c = open_equalized_editor("equalized_stroke", 3);
    let held = c.held_checkpoints().last().cloned().unwrap();
    let gains = held.registration.equalization.expect("equalized run").gains;
    assert_ne!(gains[2], UNIT_GAIN, "frame 2 has a gain to apply");

    // A point the fuse took from another frame, so the stroke has to
    // render it rather than keep the sealed pixel.
    let root = held.sources[0].parent().unwrap().to_path_buf();
    let sealed = Sealed::read(&mut c, &root);
    let width = sealed.map.width as usize;
    let at_index = sealed
        .indices
        .iter()
        .position(|&i| i != 2)
        .expect("the fuse took some pixel from another frame");
    let (px, py) = (at_index % width, at_index / width);
    c.select_frame(2);
    set_size(&mut c, 8.0);
    drag(&mut c, &[(px as f32, py as f32)]);
    let stroked = sealed.displayed(&c)[at_index * 3..at_index * 3 + 3].to_vec();

    let view = c
        .selected_frame_view()
        .expect("a fused tab's frame has a view");
    let read = |view: FrameView| {
        let mut source = FrameSource::default();
        source
            .set(
                &c.selected_frame_path().unwrap(),
                Some(view),
                job::host_budget(),
            )
            .expect("the selected frame opens");
        let mut pixel = vec![0u16; 3];
        let rect = Rect {
            x: px as u32,
            y: py as u32,
            width: 1,
            height: 1,
        };
        source
            .read_into(rect, NonZeroU32::MIN, &mut pixel)
            .expect("the point reads");
        pixel
    };
    assert_eq!(stroked, read(view), "stroke and left pane agree");
    let ungained = read(FrameView {
        gain: UNIT_GAIN,
        ..view
    });
    assert_ne!(stroked, ungained, "the pixel carries the frame's gain");
}

// ---------------------------------------------------------------------
// Region renders run on a worker; the controller installs what they render.
// ---------------------------------------------------------------------

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::io::{FrameDesc, FrameReader};

/// What a [`GatedReader`] does with a read it serves off the test thread.
#[derive(Clone, Copy)]
enum Fault {
    None,
    Error,
    Panic,
}

/// A reader over a session's own frames that records the thread of every
/// read and holds a read made off the test thread until its gate opens, for
/// at most ten seconds, so a read that runs on the calling thread is
/// recorded and never blocks the test.
struct GatedReader {
    inner: Arc<dyn FrameReader>,
    owner: std::thread::ThreadId,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    fault: Arc<Mutex<Fault>>,
    reads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}

/// The test's handle on a [`GatedReader`] it installed.
struct Gate {
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    fault: Arc<Mutex<Fault>>,
    reads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}

impl Gate {
    /// Swaps the active session's frame reader for a closed gate over it.
    fn install(controller: &mut AppController) -> Gate {
        let edit = controller
            .edit_session_mut()
            .expect("the tab has an edit session");
        let gate = Gate {
            gate: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
            fault: Arc::new(Mutex::new(Fault::None)),
            reads: Arc::new(Mutex::new(Vec::new())),
        };
        edit.sources = Arc::new(GatedReader {
            inner: edit.sources.clone(),
            owner: std::thread::current().id(),
            gate: gate.gate.clone(),
            fault: gate.fault.clone(),
            reads: gate.reads.clone(),
        });
        gate
    }

    fn open(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }

    fn set_fault(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }

    fn read_threads(&self) -> Vec<std::thread::ThreadId> {
        self.reads.lock().unwrap().clone()
    }

    /// How many reads a worker served.
    fn off_thread_reads(&self) -> usize {
        let me = std::thread::current().id();
        self.read_threads().iter().filter(|id| **id != me).count()
    }
}

impl TileReader for GatedReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let me = std::thread::current().id();
        self.reads.lock().unwrap().push(me);
        if me != self.owner {
            let (open, changed) = &*self.gate;
            let (open, timeout) = changed
                .wait_timeout_while(open.lock().unwrap(), Duration::from_secs(10), |open| !*open)
                .unwrap();
            let timed_out = timeout.timed_out() && !*open;
            drop(open);
            if timed_out {
                return Err(AlgorithmError::Failed {
                    algorithm: "gated reader".to_string(),
                    detail: "the gate never opened".to_string(),
                });
            }
            let fault = *self.fault.lock().unwrap();
            match fault {
                Fault::None => {}
                Fault::Error => {
                    return Err(AlgorithmError::Failed {
                        algorithm: "gated reader".to_string(),
                        detail: "injected read failure".to_string(),
                    });
                }
                Fault::Panic => panic!("injected reader panic"),
            }
        }
        self.inner.read_tile_into(frame, rect, out)
    }
}

impl FrameReader for GatedReader {
    fn len(&self) -> usize {
        self.inner.len()
    }

    fn dimensions(&self) -> (u32, u32) {
        self.inner.dimensions()
    }

    fn frame(&self, index: usize) -> &FrameDesc {
        self.inner.frame(index)
    }
}

/// [`drag`] without settling: the stroke commits, its regions stay queued.
fn drag_unsettled(controller: &mut AppController, path: &[(f32, f32)]) {
    let screen: Vec<(f32, f32)> = path.iter().map(|&(x, y)| at(x, y)).collect();
    press(controller, screen[0]);
    for &(x, y) in &screen[1..] {
        controller.pointer_moved(x, y);
    }
    release(controller, *screen.last().unwrap());
}

/// Waits until `events` holds a `RegionRenderReady`, i.e. a worker parked.
fn wait_for_region_render_ready(events: &Mutex<Vec<ShellEvent>>) {
    let start = Instant::now();
    while !events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, ShellEvent::RegionRenderReady))
    {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "no worker parked a result"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_stroke_renders_off_the_ui_thread() {
    let (mut c, root) = open_editor("region_render_off_thread");
    let sealed = Sealed::read(&mut c, &root);
    let gate = Gate::install(&mut c);
    set_size(&mut c, 12.0);
    c.select_frame(1);

    // A live tick: queued for a worker, so nothing shows and nothing is gathered yet.
    let path: Vec<(f32, f32)> = (0..12)
        .map(|i| (8.0 + 3.0 * i as f32, 20.0 + i as f32))
        .collect();
    let screen: Vec<(f32, f32)> = path.iter().map(|&(x, y)| at(x, y)).collect();
    press(&mut c, screen[0]);
    for &(x, y) in &screen[1..] {
        c.pointer_moved(x, y);
    }
    c.flush_live_stroke();
    assert!(ops(&c).is_empty(), "the tick records no op");
    assert!(
        sealed.displayed(&c) == sealed.composite,
        "the tick shows nothing until a worker installs it"
    );
    assert!(
        c.take_rendered_regions().is_empty(),
        "the tick gathered nothing"
    );

    // The commit and the undo return the same way.
    release(&mut c, *screen.last().unwrap());
    assert_eq!(ops(&c).len(), 1);
    assert!(
        c.take_rendered_regions().is_empty(),
        "the commit gathered nothing"
    );
    let info = c.edit_session().unwrap().info();
    let bbox = footprint(&ops(&c)[0], info).unwrap().unwrap();
    c.undo();
    assert!(ops(&c).is_empty(), "the undo stepped at once");
    assert!(c.edit_session().unwrap().can_redo());
    assert!(
        c.take_rendered_regions().is_empty(),
        "the undo gathered nothing"
    );

    gate.open();
    common::settle_region_renders(&mut c);
    let log = c.take_rendered_regions();
    // The tick's region, then the release's re-render of each 16 px tile the
    // tick and the stroke meet (tile column by tile column), then the undo.
    let tick = log[0];
    let (width, height) = (info.width(), info.height());
    let tiles_of = |regions: [Rect; 2]| {
        let mut keys = std::collections::BTreeSet::new();
        for r in regions {
            for ty in r.y / 16..=(r.y + r.height - 1) / 16 {
                for tx in r.x / 16..=(r.x + r.width - 1) / 16 {
                    keys.insert((tx, ty));
                }
            }
        }
        keys.into_iter().map(|(tx, ty)| Rect {
            x: tx * 16,
            y: ty * 16,
            width: 16.min(width - tx * 16),
            height: 16.min(height - ty * 16),
        })
    };
    let mut expected = vec![tick];
    expected.extend(tiles_of([tick, bbox]));
    expected.push(bbox);
    assert_eq!(log, expected, "tick, the touched tiles, undo");
    assert!(gate.off_thread_reads() > 0, "the worker read the frames");
    assert_eq!(
        gate.off_thread_reads(),
        gate.read_threads().len(),
        "no read ran on the calling thread"
    );
    sealed.assert_shown(&c, "after the undo settles");
}

/// Edits count as settled only with no live stroke and no region render
/// queued or in flight; a settle after the last install makes them settled.
#[test]
fn edits_settle_after_the_last_region_render() {
    let (mut c, _root) = open_editor("edits_settle");
    let gate = Gate::install(&mut c);
    set_size(&mut c, 12.0);
    c.select_frame(1);
    assert!(c.edits_settled(), "an untouched session is settled");

    let path: Vec<(f32, f32)> = (0..12)
        .map(|i| (8.0 + 3.0 * i as f32, 20.0 + i as f32))
        .collect();
    let screen: Vec<(f32, f32)> = path.iter().map(|&(x, y)| at(x, y)).collect();
    press(&mut c, screen[0]);
    assert!(!c.edits_settled(), "a live stroke is not settled");
    for &(x, y) in &screen[1..] {
        c.pointer_moved(x, y);
    }
    c.flush_live_stroke();
    assert!(
        !c.edits_settled(),
        "a stroke tick with its region render in flight is not settled"
    );

    release(&mut c, *screen.last().unwrap());
    assert!(
        !c.edits_settled(),
        "a committed stroke whose region render is unfinished is not settled"
    );
    assert!(
        !c.settle_region_renders(),
        "the gated worker has not parked its result"
    );
    assert!(!c.edits_settled(), "still in flight after a settle poll");

    gate.open();
    common::settle_region_renders(&mut c);
    assert!(c.edits_settled(), "settled after the last install");

    drag(&mut c, &[(10.0, 10.0), (30.0, 20.0)]);
    assert!(c.edits_settled(), "a settled drag leaves nothing pending");
}

#[test]
fn background_region_render_matches_the_synchronous_one() {
    let name = "region_render_matches_sync";
    let (mut c, root) = open_editor(name);
    let sealed = Sealed::read(&mut c, &root);
    let (composite, zmap) = c.materialize_active_tab_planes().unwrap();
    let registration = sealed.registration.clone();
    let gate = Gate::install(&mut c);
    set_size(&mut c, 14.0);

    // Queued without settling between them, so later regions batch behind
    // the first: three sources, an undo, a redo, then a stroke across the
    // undone area.
    c.select_frame(1);
    drag_unsettled(&mut c, &[(8.0, 10.0), (30.0, 18.0)]);
    c.select_frame(2);
    drag_unsettled(&mut c, &[(40.0, 12.0), (20.0, 30.0)]);
    c.select_frame(0);
    drag_unsettled(&mut c, &[(10.0, 40.0), (46.0, 44.0)]);
    let info = c.edit_session().unwrap().info();
    let strokes = ops(&c);
    let bbox = |i: usize| footprint(&strokes[i], info).unwrap().unwrap();
    c.undo();
    c.undo();
    c.redo();
    c.select_frame(2);
    drag_unsettled(&mut c, &[(5.0, 36.0), (50.0, 46.0)]);
    gate.open();
    common::settle_region_renders(&mut c);

    let regions = c.take_rendered_regions();
    assert!(
        regions.windows(3).any(|w| w == [bbox(2), bbox(1), bbox(1)]),
        "the undos and the redo install their regions in order: {regions:?}"
    );
    sealed.assert_shown(&c, "after every region settled");

    // The same final ops composed synchronously: opened as a chain over the
    // same composite and frames.
    let mut fresh = job::open_edit_session(
        Some(&zmap),
        &composite,
        &(0..3)
            .map(|i| root.join(format!("{i:02}.tif")))
            .collect::<Vec<_>>(),
        registration,
        16,
        job::host_budget(),
        Vec::new(),
        ops(&c),
    )
    .expect("a second session over the same frames opens");
    assert_eq!(fresh.stack_ops(), ops(&c), "the same op list");
    fresh.tiles.take_rendered_regions();

    let view = c.edited_view().unwrap();
    let shown = |tiles: &focusforge_gui::canvas::ResidentTiles| {
        let mut samples = sealed.composite.clone();
        overlay_edited(&mut samples, sealed.map, NonZeroU32::MIN, 3, tiles);
        samples
    };
    assert!(
        shown(view.tiles) == shown(&fresh.tiles),
        "the displayed image equals the synchronous render's"
    );
}

#[test]
fn a_region_render_for_a_replaced_session_is_dropped() {
    // (a) A row restored while a stroke's regions are in flight.
    let name = "region_render_restored";
    let (mut c, root) = open_editor(name);
    let sealed = Sealed::read(&mut c, &root);
    let events = record_events(&mut c);
    let gate = Gate::install(&mut c);
    set_size(&mut c, 8.0);
    c.select_frame(2);
    drag_unsettled(&mut c, &[(40.0, 30.0), (50.0, 44.0)]);
    assert!(c.save_checkpoint());
    c.select_checkpoint_row(row_index(&c, "held:0"));
    common::settle_session(&mut c);
    assert!(
        c.edit_session().unwrap().stack_ops().is_empty(),
        "the restored row's own chain"
    );
    gate.open();
    wait_for_region_render_ready(&events);
    common::settle_region_renders(&mut c);
    assert!(
        c.take_rendered_regions().is_empty(),
        "the replaced session's regions never reach the new one"
    );
    assert!(sealed.displayed(&c) == sealed.composite);

    // (b) A tab closed while its regions are in flight.
    let name = "region_render_closed";
    let (mut c, _) = open_editor(name);
    let events = record_events(&mut c);
    let gate = Gate::install(&mut c);
    set_size(&mut c, 8.0);
    c.select_frame(1);
    drag_unsettled(&mut c, &[(10.0, 10.0), (30.0, 20.0)]);
    c.close_project(0);
    gate.open();
    wait_for_region_render_ready(&events);
    common::settle_region_renders(&mut c);
    assert!(c.edit_session().is_none());

    // (c) A plain save is not a replacement: its regions land.
    let name = "region_render_saved";
    let (mut c, _) = open_editor(name);
    let gate = Gate::install(&mut c);
    set_size(&mut c, 8.0);
    c.select_frame(1);
    drag_unsettled(&mut c, &[(10.0, 10.0), (30.0, 20.0)]);
    save_to_proposed(&mut c);
    gate.open();
    common::settle_region_renders(&mut c);
    assert!(
        !c.take_rendered_regions().is_empty(),
        "the save kept the session, so its regions land"
    );
    assert!(!c.edit_session().unwrap().stack_ops().is_empty());

    // (d) A result for a switched-away tab lands on its own tab.
    let name = "region_render_switched";
    let (mut c, _) = open_editor(name);
    let events = record_events(&mut c);
    let gate = Gate::install(&mut c);
    set_size(&mut c, 8.0);
    c.select_frame(1);
    drag_unsettled(&mut c, &[(10.0, 10.0), (30.0, 20.0)]);
    open_mantis64_tab(&mut c, &format!("{name}_other"), 2);
    gate.open();
    wait_for_region_render_ready(&events);
    common::settle_region_renders(&mut c);
    assert!(
        c.take_rendered_regions().is_empty(),
        "nothing lands on the active tab"
    );
    c.select_project(0);
    assert!(
        !c.take_rendered_regions().is_empty(),
        "the first tab's tiles took its own regions"
    );
}

#[test]
fn a_failed_background_region_render_keeps_the_edit() {
    let (mut c, root) = open_editor("region_render_fails");
    let sealed = Sealed::read(&mut c, &root);
    let gate = Gate::install(&mut c);
    gate.open();
    set_size(&mut c, 10.0);
    c.select_frame(1);

    for (fault, case) in [(Fault::Error, "a failed read"), (Fault::Panic, "a panic")] {
        gate.set_fault(fault);
        let before = ops(&c).len();
        drag(&mut c, &[(8.0, 12.0 + 8.0 * before as f32), (40.0, 20.0)]);
        assert_eq!(ops(&c).len(), before + 1, "{case}: the stroke stays");
        assert!(
            c.take_rendered_regions().is_empty(),
            "{case}: nothing installed"
        );
        assert!(
            sealed.displayed(&c) == sealed.composite,
            "{case}: the pane keeps its previous pixels"
        );
    }
    assert_eq!(ops(&c).len(), 2);

    c.undo();
    assert_eq!(ops(&c).len(), 1, "undo still steps the history");
    common::settle_region_renders(&mut c);
    c.take_rendered_regions();

    gate.set_fault(Fault::None);
    drag(&mut c, &[(10.0, 50.0), (44.0, 54.0)]);
    assert!(
        !c.take_rendered_regions().is_empty(),
        "later strokes settle"
    );
    c.redo();
    c.undo();
    c.undo();
    c.undo();
    common::settle_region_renders(&mut c);
    sealed.assert_shown(&c, "after the history unwinds");
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
/// stroke, so the edit session's op list and the displayed image stay exactly
/// as they were.
fn assert_drag_blocked(c: &mut AppController, case: &str) {
    let before_ops = ops(c);
    let before_image = displayed_whole(c);
    press(c, at_pane(LEFT, 10.0, 10.0));
    c.pointer_moved(at_pane(LEFT, 20.0, 15.0).0, at_pane(LEFT, 20.0, 15.0).1);
    release(c, at_pane(LEFT, 20.0, 15.0));
    press(c, at(10.0, 10.0));
    c.pointer_moved(at(20.0, 15.0).0, at(20.0, 15.0).1);
    release(c, at(20.0, 15.0));
    assert_eq!(ops(c), before_ops, "{case}: a blocked drag must add no op");
    common::settle_region_renders(c);
    assert!(
        displayed_whole(c) == before_image,
        "{case}: a blocked drag must not touch the image"
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

#[test]
fn refusal_dismissed_without_picker() {
    use focusforge_gui::ChromeKey;
    use focusforge_gui::input_map::Modifiers;
    let escape = |c: &mut AppController| c.chrome_key("\u{1b}", Modifiers::NONE);

    let mut controller = AppController::new();
    raise_refusal(&mut controller, "refusal_escape_folder_empty");
    assert!(controller.modal_open());
    assert_eq!(escape(&mut controller), ChromeKey::RefusalDismissed);
    assert!(
        !controller.modal_open(),
        "Escape closes the folder-empty refusal"
    );

    let dir = fixture_copy("projects", "refusal_escape_titled");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[dir.join("corrupt.ffp")]),
        OpenOutcome::Refused(_)
    ));
    assert!(controller.refusal_title().is_some(), "a titled refusal");
    assert!(controller.modal_open());
    assert_eq!(escape(&mut controller), ChromeKey::RefusalDismissed);
    assert!(!controller.modal_open(), "Escape closes a titled refusal");

    controller.toggle_projects_overlay();
    raise_refusal(&mut controller, "refusal_escape_over_overlay");
    assert_eq!(escape(&mut controller), ChromeKey::RefusalDismissed);
    assert!(!controller.modal_open());
    assert!(
        controller.projects_overlay_open(),
        "the refusal closes first; the overlay stays open"
    );
    assert_eq!(escape(&mut controller), ChromeKey::Consumed);
    assert!(!controller.projects_overlay_open());
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
    let shown = session
        .stored_source(x as u32, 20)
        .expect("the column lies inside the map");
    session
        .append(Op::BrushStroke {
            source_id: u16::from(shown == 0),
            path: vec![Point { x, y: 20 }],
            radius: 4.0,
            softness: 0.0,
        })
        .expect("a dab inside the map applies");
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

/// `id`'s Z-Map blob in the saved project.
fn saved_zmap_blob(path: &Path, id: &str) -> String {
    saved(path)
        .history()
        .get(id)
        .expect("the checkpoint is saved")
        .zmap_blob
        .clone()
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
    let painted_ops = ops(&controller);
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
    assert_eq!(child.ops, painted_ops);
    assert_eq!(child.zmap_blob, root.zmap_blob, "the fused ancestor's map");
    assert_eq!(current_row(&controller), Some(child.id.clone()));
    let session = controller.edit_session().expect("the session survives");
    assert_eq!((session.op_count(), session.stack_ops()), (0, painted_ops));
    assert!(!controller.save_enabled());

    // An opened project painted on its older, non-head row.
    let (path, chain) = saved_chain("auto_checkpoint_older", &[20]);
    let mut controller = reopen(&path, "auto_checkpoint_older_reopened");
    assert_eq!(current_row(&controller), Some(chain[1].clone()));
    let root_row = row_index(&controller, &chain[0]);
    controller.select_checkpoint_row(root_row);
    common::enter_editor(&mut controller);
    paint(&mut controller, 40);
    let painted_ops = ops(&controller);
    assert!(controller.save_enabled());
    assert!(controller.save_active_project(|_| None));

    let added = added_checkpoint(&path, &chain);
    assert_eq!(
        added.parent.as_deref(),
        Some(chain[0].as_str()),
        "not the head"
    );
    assert_eq!(added.ops, painted_ops);
    assert_eq!(added.zmap_blob, saved_zmap_blob(&path, &chain[0]));
    assert_eq!(current_row(&controller), Some(added.id));
    let session = controller.edit_session().expect("the session survives");
    assert_eq!((session.op_count(), session.stack_ops()), (0, painted_ops));
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
    common::enter_editor(&mut opened);
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
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&path, perms.clone()).unwrap();
    assert!(!controller.save_active_project(|_| None));
    assert_eq!(fs::read(&path).unwrap(), before, "the file is untouched");
    assert!(controller.save_enabled(), "the delete is still queued");
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&path, perms).unwrap();

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
    common::enter_editor(&mut controller);
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
    common::enter_editor(&mut controller);
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
    common::enter_editor(&mut controller);
    assert!(controller.queue_checkpoint_delete(&chain[2]));
    paint(&mut controller, 50);
    let painted_ops = ops(&controller);
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
    assert_eq!(added.ops, painted_ops, "an op list over the hidden row");
    assert_eq!(added.zmap_blob, saved_zmap_blob(&path, &chain[0]));
    assert_eq!(current_row(&controller), Some(added.id));
    assert_eq!(controller.edit_session().unwrap().op_count(), 0);
}

#[test]
fn a_snapshot_during_a_fuse_holds_no_files() {
    let (path, chain) = saved_chain("snapshot_during_fuse", &[20]);
    let mut controller = reopen(&path, "snapshot_during_fuse_reopened");
    controller.set_align_algorithm("dummy-align");
    common::enter_editor(&mut controller);
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
    let painted_ops = ops(&controller);

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
    assert_eq!(
        controller.notifications().len(),
        1,
        "the failed save is recorded"
    );
    assert_eq!(
        controller.checkpoint_count(),
        Some(2),
        "the automatic checkpoint stays"
    );
    assert_eq!(current_row(&controller), Some("held:1".to_string()));
    let session = controller.edit_session().unwrap();
    assert_eq!((session.op_count(), session.stack_ops()), (0, painted_ops));
    assert!(controller.save_enabled());

    let path = save_to_proposed(&mut controller);
    assert_eq!(saved_ids(&path).len(), 2, "a retry writes the same history");
}

/// Editing from an older, non-head checkpoint renders through that
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
    common::enter_editor(&mut controller);
    assert_eq!(map_rect(&controller).width, MAP_WIDTH, "the root's own map");
    controller.select_frame(1);
    let view = controller.selected_frame_view().unwrap();
    assert_eq!(view.origin, root_registration.origin());

    let sealed = Sealed::read_with(&mut controller, &frames, root_registration);
    set_size(&mut controller, 6.0);
    drag(&mut controller, &[(10.0, 10.0), (30.0, 20.0)]);
    assert_eq!(ops(&controller).len(), 1, "the stroke is appended");
    sealed.assert_shown(&controller, "an older checkpoint's edit");

    // The head's registration would have drawn something else.
    let wrong = Sealed::read_with(&mut controller, &frames, head_registration);
    assert!(
        wrong.displayed(&controller)
            != wrong.expected(&ops(&controller), controller.edit_session().unwrap().info()),
        "the check can tell the two registrations apart"
    );
}

/// Fuses `name`'s frames twice, then selects the older held row in
/// Stacking: a fused row whose session is dropped and not reopened.
fn fused_twice_without_session(controller: &mut AppController, name: &str) {
    open_fused_tab(controller, name, 2);
    assert!(controller.start_fuse());
    settle_all(controller);
    assert!(controller.set_mode(ShellMode::Stacking));
    controller.select_checkpoint_row(row_index(controller, "held:0"));
    assert!(
        controller.edit_session().is_none(),
        "{name}: no session yet"
    );
}

/// How many times `events` holds an `EditorChanged`.
fn editor_changes(events: &Mutex<Vec<ShellEvent>>) -> usize {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, ShellEvent::EditorChanged))
        .count()
}

#[test]
fn entering_the_editor_opens_its_session_off_thread() {
    let name = "session_off_thread";
    let mut controller = controller_with_scratch(name);
    controller.set_resident_tile_size_for_test(16);
    fused_twice_without_session(&mut controller, name);
    let (_, zmap) = controller
        .materialize_active_tab_planes()
        .expect("a held fused row has planes");
    let events = record_events(&mut controller);

    // A plain fused row: the click switches, the settle installs.
    assert!(controller.set_mode(ShellMode::Editor));
    assert_eq!(controller.mode(), ShellMode::Editor);
    assert_eq!(take_mode_changes(&events), vec![ShellMode::Editor]);
    assert!(
        controller.edit_session().is_none(),
        "the session is not open until the settle"
    );
    common::settle_session(&mut controller);
    let session = controller.edit_session().expect("the settle installs it");
    assert_eq!(session.info(), stored_info(&zmap));
    assert!(session.stack_ops().is_empty());
    assert_eq!(session.op_count(), 0);
    let plain = displayed_whole(&mut controller);

    // Two op-bearing rows, the second built on the first, strokes either
    // side of a 16 px tile edge.
    paint(&mut controller, 14);
    assert!(controller.save_checkpoint());
    paint(&mut controller, 33);
    assert!(controller.save_checkpoint());
    let chained = controller.edit_session().unwrap().stack_ops();
    assert_eq!(chained.len(), 2);
    assert!(controller.set_mode(ShellMode::Stacking));
    controller.select_checkpoint_row(row_index(&controller, "held:0"));
    assert!(controller.edit_session().is_none());
    take_mode_changes(&events);

    controller.select_checkpoint_row(row_index(&controller, "held:3"));
    assert!(
        controller.edit_session().is_none(),
        "an op-bearing row opens off the selecting call too"
    );
    assert!(controller.set_mode(ShellMode::Editor));
    assert_eq!(take_mode_changes(&events), vec![ShellMode::Editor]);
    assert!(controller.edit_session().is_none());
    common::settle_session(&mut controller);
    let session = controller.edit_session().expect("the settle installs it");
    assert_eq!(session.stack_ops(), chained, "the chain is applied");
    assert_eq!(session.op_count(), 0, "the replay is rebased away");
    assert!(
        !controller.take_rendered_regions().is_empty(),
        "the replay filled the resident tiles"
    );
    common::settle_region_renders(&mut controller);
    assert!(
        displayed_whole(&mut controller) != plain,
        "the replay painted the chain's pixels"
    );
}

#[test]
fn a_session_lands_only_on_its_own_tab_and_generation() {
    // Closing the tab mid-open drops the result.
    let name = "session_closed_tab";
    let mut controller = controller_with_scratch(name);
    fused_twice_without_session(&mut controller, name);
    let events = record_events(&mut controller);
    assert!(controller.set_mode(ShellMode::Editor));
    controller.close_project(0);
    common::settle_session(&mut controller);
    assert!(controller.edit_session().is_none());
    assert_eq!(editor_changes(&events), 0, "nothing was installed");

    // Restoring another row mid-open drops the result; the replacement
    // installs.
    let name = "session_restored_row";
    let mut controller = controller_with_scratch(name);
    open_fused_tab(&mut controller, name, 2);
    paint(&mut controller, 20);
    let painted_ops = ops(&controller);
    assert!(controller.save_checkpoint());
    assert!(controller.set_mode(ShellMode::Stacking));
    controller.select_checkpoint_row(row_index(&controller, "held:0"));
    let events = record_events(&mut controller);
    assert!(controller.set_mode(ShellMode::Editor));
    controller.select_checkpoint_row(row_index(&controller, "held:1"));
    assert!(controller.edit_session().is_none());
    common::settle_session(&mut controller);
    assert_eq!(
        controller
            .edit_session()
            .expect("the replacement lands")
            .stack_ops(),
        painted_ops,
        "the row selected last owns the session"
    );
    assert_eq!(
        editor_changes(&events),
        1,
        "the overtaken result is dropped"
    );

    // A tab switch mid-open leaves the new tab without a session; the first
    // tab keeps its own.
    let name = "session_switched_tab";
    let mut controller = controller_with_scratch(name);
    fused_twice_without_session(&mut controller, name);
    open_mantis64_tab(&mut controller, &format!("{name}_unfused"), 2);
    controller.select_project(0);
    assert!(controller.set_mode(ShellMode::Editor));
    controller.select_project(1);
    common::settle_session(&mut controller);
    assert!(
        controller.edit_session().is_none(),
        "the newly active tab got nothing"
    );
    controller.select_project(0);
    assert!(
        controller.edit_session().is_some(),
        "the first tab kept its own"
    );
}

#[test]
fn a_failed_session_open_drops_back_to_stacking() {
    let name = "session_open_fails";
    let mut controller = controller_with_scratch(name);
    fused_twice_without_session(&mut controller, name);
    let (composite, _) = controller
        .materialize_active_tab_planes()
        .expect("a held fused row has planes");
    fs::OpenOptions::new()
        .write(true)
        .open(&composite)
        .unwrap()
        .set_len(0)
        .unwrap();
    let events = record_events(&mut controller);

    assert!(controller.set_mode(ShellMode::Editor));
    common::settle_session(&mut controller);
    assert_eq!(controller.mode(), ShellMode::Stacking);
    assert!(controller.edit_session().is_none());
    assert_eq!(
        take_mode_changes(&events),
        vec![ShellMode::Editor, ShellMode::Stacking]
    );
}

#[test]
fn a_save_settling_mid_open_restarts_the_session() {
    let name = "session_save_mid_open";
    let mut controller = controller_with_scratch(name);
    fused_twice_without_session(&mut controller, name);
    let events = record_events(&mut controller);
    assert!(controller.set_mode(ShellMode::Editor));
    save_to_proposed(&mut controller);

    common::settle_session(&mut controller);
    assert_eq!(controller.mode(), ShellMode::Editor);
    assert!(
        controller.edit_session().is_some(),
        "the restarted open reads the saved row"
    );
    assert_eq!(editor_changes(&events), 1, "the overtaken open is dropped");
}

/// The fuse row `id`'s shown value on the active tab.
fn active_fuse_value(controller: &AppController, id: &str) -> String {
    controller
        .settings_panel()
        .rows
        .into_iter()
        .find(|r| r.section == "fuse" && r.id == id)
        .unwrap_or_else(|| panic!("no fuse row {id}"))
        .value_text
}

/// The finish section's `auto-crop` toggle on the active tab.
fn active_crop_selected(controller: &AppController) -> String {
    controller
        .settings_panel()
        .rows
        .into_iter()
        .find(|r| r.section == "finish" && r.id == "auto-crop")
        .expect("the finish section has an auto-crop row")
        .selected
}

#[test]
fn duplicated_tab_keeps_crop_and_remembered_values() {
    use focusforge_core::params::ParamValue;

    let mut controller = common::controller_with_frames("duplicate_keeps_remembered", 2);
    controller.set_fuse_algorithm("weighted-average");
    controller.set_param_value("fuse", "weight-exponent", ParamValue::Float(4.0));
    controller.set_fuse_algorithm("depth-map");
    controller.set_param_value("fuse", "score-radius", ParamValue::Int(7));
    controller.set_param_value("finish", "auto-crop", ParamValue::Bool(false));
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, std::time::Duration::from_secs(120));

    assert_eq!(controller.checkpoint_count(), Some(1), "the run completed");
    assert!(controller.set_mode(ShellMode::Stacking));
    controller.remove_frame(0);
    assert!(controller.pending_frame_change().is_some());
    assert!(controller.new_project_instead());
    assert_eq!(controller.project_tabs().len(), 2);

    assert_eq!(
        active_crop_selected(&controller),
        "false",
        "the copy keeps the crop choice"
    );
    controller.toggle_section_more("fuse");
    assert_eq!(active_fuse_value(&controller, "score-radius"), "7");
    controller.set_fuse_algorithm("weighted-average");
    assert_eq!(
        active_fuse_value(&controller, "weight-exponent"),
        "4",
        "the copy carries the values remembered for other methods"
    );
    controller.set_param_value("fuse", "weight-exponent", ParamValue::Float(6.0));

    controller.select_project(0);
    controller.toggle_section_more("fuse");
    assert_eq!(active_fuse_value(&controller, "score-radius"), "7");
    controller.set_fuse_algorithm("weighted-average");
    assert_eq!(
        active_fuse_value(&controller, "weight-exponent"),
        "4",
        "editing the copy leaves the original's remembered values alone"
    );
}

// ---------------------------------------------------------------------
// The live view goes through the soft blend: the dithered index brush
// never paints what the canvas shows.
// ---------------------------------------------------------------------

/// The strokes the live-view tests paint: three softness classes, each a
/// press-and-release dab then a ticked drag from another frame.
fn paint_scripted_strokes(c: &mut AppController) {
    set_size(c, 10.0);
    for (i, softness) in [0.0f32, 0.5, 1.0].into_iter().enumerate() {
        c.set_brush_softness_fraction(softness);
        c.select_frame((i + 1) % 3);
        let x = 8.0 + 16.0 * i as f32;
        drag(c, &[(x, 10.0)]);
        drag_ticked(
            c,
            &[
                (x - 4.0, 24.0),
                (x + 4.0, 32.0),
                (x - 3.0, 40.0),
                (x + 5.0, 52.0),
            ],
        );
    }
    assert_eq!(ops(c).len(), 6);
}

/// Gives `c` the blend kernel on `device`, when there is one.
fn attach_blend_device(c: &mut AppController, device: Option<&SharedDevice>) {
    if let Some(shared) = device {
        c.attach_blender(Blender::new(shared.device(), shared.queue()));
    }
}

/// Paints [`paint_scripted_strokes`] in an editor over three frames, equalized
/// or not, and checks the released live view against the saved render.
fn live_view_matches_saved_render(prefix: &str, device: Option<&SharedDevice>) {
    for equalized in [false, true] {
        let name = format!("{prefix}{}", if equalized { "_equalized" } else { "" });
        let mut c = if equalized {
            open_equalized_editor(&name, 3)
        } else {
            open_editor(&name).0
        };
        attach_blend_device(&mut c, device);
        paint_scripted_strokes(&mut c);
        let shown = displayed_whole(&mut c);
        let path = save_to_proposed(&mut c);
        assert!(
            shown == saved_render(&path),
            "equalized {equalized}: the live view differs from the saved render"
        );
    }
}

#[test]
fn live_view_after_release_equals_the_saved_render() {
    live_view_matches_saved_render("live_equals_saved", None);
}

#[cfg(feature = "gpu-tests")]
#[test]
fn live_stroke_on_the_device_matches_the_saved_render() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    live_view_matches_saved_render("live_equals_saved_device", Some(shared));
}

#[cfg(feature = "gpu-tests")]
#[test]
fn live_stroke_on_the_device_matches_the_host_blend() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    for equalized in [false, true] {
        let shown = |device: Option<&SharedDevice>, name: &str| {
            let mut c = if equalized {
                open_equalized_editor(name, 3)
            } else {
                open_editor(name).0
            };
            attach_blend_device(&mut c, device);
            paint_scripted_strokes(&mut c);
            displayed_whole(&mut c)
        };
        let on_device = shown(
            Some(shared),
            &format!("live_host_vs_device_{equalized}_gpu"),
        );
        let on_host = shown(None, &format!("live_host_vs_device_{equalized}_cpu"));
        assert!(
            on_device == on_host,
            "equalized {equalized}: the device blend differs from the host blend sample for sample"
        );
    }
}

#[test]
fn live_soft_edge_is_a_blend_not_a_dither() {
    let (mut c, root) = open_editor("live_soft_edge");
    let sealed = Sealed::read(&mut c, &root);
    let info = c.edit_session().unwrap().info();
    set_size(&mut c, 20.0);
    c.set_brush_softness_fraction(0.8);
    c.select_frame(1);
    let radius = c.brush_toolbar_view().size / 2.0;
    let softness = c.brush_toolbar_view().soft / 100.0;
    let stroke = Op::BrushStroke {
        source_id: 1,
        path: rounded(&[(20.0, 30.0), (36.0, 30.0)]),
        radius,
        softness,
    };
    let source = sealed.frame_view(1);

    let check = |c: &AppController, case: &str| {
        let shown = sealed.displayed(c);
        let mut between = 0;
        for y in 0..sealed.map.height {
            for x in 0..sealed.map.width {
                let alpha = blend::alpha(&stroke, info, x, y).unwrap();
                for k in 0..3 {
                    let i = ((y * sealed.map.width + x) * 3) as usize + k;
                    let (base, src) = (sealed.composite[i], source[i]);
                    assert_eq!(
                        shown[i],
                        blend::blend_sample(base, src, alpha),
                        "{case}: sample {k} of ({x}, {y}) at alpha {alpha}"
                    );
                    if alpha > 0
                        && alpha < 65536
                        && shown[i] > base.min(src)
                        && shown[i] < base.max(src)
                    {
                        between += 1;
                    }
                }
            }
        }
        assert!(
            between > 0,
            "{case}: the soft ring holds intermediate values"
        );
    };

    press(&mut c, at(20.0, 30.0));
    let (sx, sy) = at(36.0, 30.0);
    c.pointer_moved(sx, sy);
    c.flush_live_stroke();
    common::settle_region_renders(&mut c);
    check(&c, "during the drag");
    release(&mut c, at(36.0, 30.0));
    common::settle_region_renders(&mut c);
    assert_eq!(ops(&c), vec![stroke.clone()]);
    check(&c, "after release");
}

#[test]
fn live_tick_reblends_only_its_segment_footprint() {
    live_tick_reblends_only_its_footprint("live_tick", None);
}

#[cfg(feature = "gpu-tests")]
#[test]
fn live_tick_on_the_device_reblends_only_its_segment_footprint() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    live_tick_reblends_only_its_footprint("live_tick_device", Some(shared));
}

/// Drags a path that doubles back, a tick at a time: each tick renders one
/// region, the newest segment's footprint, and shows the path so far over the
/// pre-stroke samples.
fn live_tick_reblends_only_its_footprint(name: &str, device: Option<&SharedDevice>) {
    let (mut c, root) = open_editor(name);
    attach_blend_device(&mut c, device);
    let sealed = Sealed::read(&mut c, &root);
    let info = c.edit_session().unwrap().info();
    set_size(&mut c, 12.0);
    c.set_brush_softness_fraction(0.5);
    c.select_frame(2);
    let radius = c.brush_toolbar_view().size / 2.0;
    let softness = c.brush_toolbar_view().soft / 100.0;
    let stroke = |path: Vec<Point>| Op::BrushStroke {
        source_id: 2,
        path,
        radius,
        softness,
    };

    // Segments that double back over each other, so alpha would compound
    // were each segment blended on its own.
    let trace = [
        (10.0, 20.0),
        (30.0, 24.0),
        (14.0, 28.0),
        (34.0, 32.0),
        (12.0, 36.0),
        (40.0, 40.0),
    ];
    let pre = sealed.composite.clone();
    press(&mut c, at(trace[0].0, trace[0].1));
    for end in 1..trace.len() {
        let (sx, sy) = at(trace[end].0, trace[end].1);
        c.pointer_moved(sx, sy);
        c.flush_live_stroke();
        common::settle_region_renders(&mut c);
        let segment = stroke(rounded(&trace[end - 1..=end]));
        assert_eq!(
            c.take_rendered_regions(),
            footprint(&segment, info)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            "tick {end}: one region, the new segment's footprint"
        );
        let mut expected = pre.clone();
        sealed.blend_over(&mut expected, &stroke(rounded(&trace[..=end])), info);
        assert!(
            sealed.displayed(&c) == expected,
            "tick {end}: the path so far over the pre-stroke samples"
        );
    }
    release(&mut c, at(trace[5].0, trace[5].1));
    common::settle_region_renders(&mut c);
    sealed.assert_shown(&c, "after release");
}

#[test]
fn undo_redo_restore_the_blended_samples() {
    let (mut c, root) = open_editor("live_undo_redo");
    let sealed = Sealed::read(&mut c, &root);
    let info = c.edit_session().unwrap().info();
    set_size(&mut c, 16.0);
    c.set_brush_softness_fraction(0.6);
    for (frame, from, to) in [
        (0, (10.0, 20.0), (40.0, 26.0)),
        (1, (14.0, 30.0), (44.0, 22.0)),
        (2, (12.0, 24.0), (42.0, 36.0)),
    ] {
        c.select_frame(frame);
        drag(&mut c, &[from, to]);
    }
    let all = ops(&c);
    assert_eq!(all.len(), 3);
    sealed.assert_shown(&c, "three strokes");
    let first_two = sealed.expected(&all[..2], info);
    let last_region = footprint(&all[2], info).unwrap().unwrap();

    c.undo();
    common::settle_region_renders(&mut c);
    assert_eq!(c.edit_session().unwrap().op_count(), 2);
    assert!(
        sealed.displayed(&c) == first_two,
        "undo shows the first two"
    );
    assert!(c.take_rendered_regions().ends_with(&[last_region]));
    c.redo();
    common::settle_region_renders(&mut c);
    assert_eq!(c.edit_session().unwrap().op_count(), 3);
    sealed.assert_shown(&c, "redo restores the third");
    c.undo();
    common::settle_region_renders(&mut c);
    assert!(
        sealed.displayed(&c) == first_two,
        "a second undo after the redo"
    );

    let shown = displayed_whole(&mut c);
    let path = save_to_proposed(&mut c);
    assert!(
        shown == saved_render(&path),
        "the saved render of two strokes"
    );
}

#[test]
fn reopened_soft_checkpoint_shows_the_blend() {
    // A held op-bearing row replays its chain over the base it was taken from.
    let mut c = open_editor("reopen_held").0;
    set_size(&mut c, 14.0);
    c.set_brush_softness_fraction(0.7);
    c.select_frame(1);
    drag(&mut c, &[(12.0, 20.0), (36.0, 26.0)]);
    c.select_frame(2);
    drag(&mut c, &[(16.0, 34.0), (40.0, 30.0)]);
    assert!(c.save_checkpoint());
    let ops_row = row_index(&c, "held:1");
    c.select_checkpoint_row(row_index(&c, "held:0"));
    c.select_checkpoint_row(ops_row);
    common::enter_editor(&mut c);
    assert_eq!(
        c.edit_session().unwrap().op_count(),
        0,
        "the chain is rebased"
    );
    let shown = displayed_whole(&mut c);
    let path = save_to_proposed(&mut c);
    assert!(
        shown == saved_render(&path),
        "the replayed chain differs from the saved render"
    );

    // A saved soft checkpoint opens on its rendered planes.
    let mut reopened = reopen(&path, "reopen_saved");
    common::enter_editor(&mut reopened);
    assert!(displayed_whole(&mut reopened) == saved_render(&path));
}

#[test]
fn live_stroke_at_the_image_edge_matches_the_saved_render() {
    let (mut c, _) = open_editor("live_edge");
    set_size(&mut c, 20.0);
    c.set_brush_softness_fraction(0.5);
    c.select_frame(1);
    drag_ticked(&mut c, &[(2.0, 2.0), (12.0, 5.0)]);
    c.select_frame(2);
    drag_ticked(&mut c, &[(55.0, 57.0), (46.0, 50.0)]);
    assert_eq!(ops(&c).len(), 2);
    let shown = displayed_whole(&mut c);
    let path = save_to_proposed(&mut c);
    assert!(
        shown == saved_render(&path),
        "strokes across the image border"
    );
}

// ---------------------------------------------------------------------
// Unsaved strokes are an in-memory op list over the saved position.
// ---------------------------------------------------------------------

#[test]
fn brush_stroke_appends_soft_op() {
    let (mut c, _) = open_editor("brush_soft_op");
    set_size(&mut c, 14.0);
    c.select_frame(2);
    let view = c.brush_toolbar_view();
    assert_ne!(view.size, 8.0, "a size other than the default");
    let radius = view.size / 2.0;
    for (at_slider, softness, x) in [(0.0, 0.0f32, 12.0), (1.0, 1.0, 36.0)] {
        c.set_brush_softness_fraction(at_slider);
        assert_eq!(c.brush_toolbar_view().soft / 100.0, softness);
        drag(&mut c, &[(x, 20.0), (x + 6.0, 30.0)]);
    }
    assert_eq!(
        ops(&c),
        vec![
            Op::BrushStroke {
                source_id: 2,
                path: vec![Point { x: 12, y: 20 }, Point { x: 18, y: 30 }],
                radius,
                softness: 0.0,
            },
            Op::BrushStroke {
                source_id: 2,
                path: vec![Point { x: 36, y: 20 }, Point { x: 42, y: 30 }],
                radius,
                softness: 1.0,
            },
        ]
    );
}

/// `ops` over the sealed composite at `composite`, rendered whole by the
/// same function an export of unsaved strokes goes through.
fn pending_render(composite: &Path, root: &Path, sealed: &Sealed, ops: Vec<Op>) -> Vec<u16> {
    let state = project::PendingState {
        composite: composite.to_path_buf(),
        ops,
        sources: (0..3).map(|i| root.join(format!("{i:02}.tif"))).collect(),
        registration: sealed.registration.clone(),
        reference: 0,
        description: String::new(),
    };
    let image = project::CheckpointImage::prepare_pending(&state).expect("the state prepares");
    let full = image.full();
    let mut out = vec![0u16; (full.width * full.height * 3) as usize];
    image
        .render(None, full, &mut |band: &[u16], rect: Rect| {
            let row = rect.width as usize * 3;
            for y in 0..rect.height as usize {
                let at = ((rect.y as usize + y) * full.width as usize + rect.x as usize) * 3;
                out[at..at + row].copy_from_slice(&band[y * row..(y + 1) * row]);
            }
            Ok(())
        })
        .expect("the state renders");
    out
}

#[test]
fn undo_redo_moves_the_state_position() {
    let (mut c, root) = open_editor("undo_redo_position");
    let sealed = Sealed::read(&mut c, &root);
    let composite = c.materialize_active_tab_planes().unwrap().0;
    // The shown image must equal the render of the ops it stacks over the base.
    let assert_renders = |c: &AppController, case: &str| {
        let stacked = c.edit_session().unwrap().stack_ops();
        assert!(
            sealed.displayed(c) == pending_render(&composite, &root, &sealed, stacked),
            "{case}: the shown image differs from the render of its ops"
        );
    };
    set_size(&mut c, 16.0);
    c.set_brush_softness_fraction(0.6);
    for (frame, from, to) in [
        (0, (10.0, 20.0), (40.0, 26.0)),
        (1, (14.0, 30.0), (44.0, 22.0)),
        (2, (12.0, 24.0), (42.0, 36.0)),
    ] {
        c.select_frame(frame);
        drag(&mut c, &[from, to]);
    }
    let all = ops(&c);
    assert_eq!(all.len(), 3);
    assert_renders(&c, "three strokes");

    for k in 1..=3 {
        c.undo();
        common::settle_region_renders(&mut c);
        assert_eq!(ops(&c), all[..3 - k], "undo {k}");
        assert_renders(&c, &format!("undo {k}"));
    }
    assert!(!c.edit_session().unwrap().can_undo());
    c.take_rendered_regions();
    c.undo();
    common::settle_region_renders(&mut c);
    assert!(ops(&c).is_empty() && c.take_rendered_regions().is_empty());
    assert_renders(&c, "undo with nothing pending");
    assert!(sealed.displayed(&c) == sealed.composite);

    for k in 1..=3 {
        c.redo();
        common::settle_region_renders(&mut c);
        assert_eq!(ops(&c), all[..k], "redo {k}, in order");
        assert_renders(&c, &format!("redo {k}"));
    }

    c.undo();
    c.undo();
    common::settle_region_renders(&mut c);
    assert!(c.edit_session().unwrap().can_redo());
    c.select_frame(2);
    drag(&mut c, &[(30.0, 8.0), (34.0, 14.0)]);
    assert!(!c.edit_session().unwrap().can_redo(), "a stroke ends redo");
    assert_eq!(ops(&c).len(), 2);
    assert_eq!(ops(&c)[0], all[0]);
    assert_renders(&c, "the new stroke after an undo");

    let shown = displayed_whole(&mut c);
    let path = save_to_proposed(&mut c);
    assert!(
        shown == saved_render(&path),
        "the saved render of the pending strokes"
    );
}

#[test]
fn pending_strokes_append_on_save() {
    let (mut c, root) = open_editor("pending_append_on_save");
    let sealed = Sealed::read(&mut c, &root);
    set_size(&mut c, 12.0);
    c.set_brush_softness_fraction(0.5);
    c.select_frame(0);
    drag(&mut c, &[(40.0, 50.0), (50.0, 54.0)]);
    let path = save_to_proposed(&mut c);
    let before = fs::read(&path).unwrap();
    let known = saved_ids(&path);

    c.select_frame(1);
    drag(&mut c, &[(10.0, 12.0), (30.0, 20.0)]);
    c.select_frame(2);
    drag(&mut c, &[(20.0, 40.0), (44.0, 30.0)]);
    let pending = ops(&c);
    assert_eq!(pending.len(), 2);
    assert!(
        fs::read(&path).unwrap() == before,
        "two strokes write nothing"
    );
    c.request_close(0);
    assert_eq!(
        c.pending_close().expect("two strokes ask").body,
        "2 unsaved brush strokes."
    );
    c.cancel_close();

    let shown = sealed.displayed(&c);
    assert!(c.save_active_project(|_| None));
    let added = added_checkpoint(&path, &known);
    assert_eq!(added.ops, pending);
    assert!(
        shown == saved_render(&path),
        "the newest checkpoint renders the live view"
    );

    let mut reopened = reopen(&path, "pending_append_reopened");
    common::enter_editor(&mut reopened);
    assert!(displayed_whole(&mut reopened) == shown);
}

#[test]
fn strokes_after_first_save_show_the_saved_render() {
    let (mut c, root) = open_editor("strokes_after_first_save");
    let sealed = Sealed::read(&mut c, &root);
    let (held, _) = c.materialize_active_tab_planes().unwrap();
    let path = save_to_proposed(&mut c);
    assert!(!held.exists(), "a save removes the row's held composite");

    set_size(&mut c, 12.0);
    c.set_brush_softness_fraction(0.5);
    c.select_frame(1);
    drag(&mut c, &[(40.0, 50.0), (50.0, 54.0)]);
    c.select_frame(2);
    drag(&mut c, &[(48.0, 40.0), (56.0, 28.0)]);
    let strokes = ops(&c);
    assert_eq!(strokes.len(), 2);
    let info = c.edit_session().unwrap().info();

    let _ = c.materialize_active_tab_planes();
    common::settle_render(&mut c);
    let shown = displayed_whole(&mut c);
    assert!(c.save_active_project(|_| None));
    let rendered = saved_render(&path);
    let differing = shown.iter().zip(&rendered).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the live view and the saved render differ in {differing} samples"
    );
    assert!(
        shown == sealed.expected(&strokes, info),
        "the live view differs from the reference blend"
    );
}

#[cfg(unix)]
#[test]
fn a_session_renders_after_its_composite_file_is_removed() {
    let (mut c, root) = open_editor("session_composite_removed");
    let sealed = Sealed::read(&mut c, &root);
    let (composite, zmap) = c.materialize_active_tab_planes().unwrap();
    let scratch = root.join("copies");
    fs::create_dir_all(&scratch).unwrap();
    let (composite_copy, zmap_copy) = (scratch.join("composite.tif"), scratch.join("zmap.tif"));
    fs::copy(&composite, &composite_copy).unwrap();
    fs::copy(&zmap, &zmap_copy).unwrap();

    let mut session = job::open_edit_session(
        Some(&zmap_copy),
        &composite_copy,
        &(0..3)
            .map(|i| root.join(format!("{i:02}.tif")))
            .collect::<Vec<_>>(),
        sealed.registration.clone(),
        16,
        job::host_budget(),
        Vec::new(),
        Vec::new(),
    )
    .expect("a session over the copies opens");
    fs::remove_file(&composite_copy).unwrap();
    fs::remove_file(&zmap_copy).unwrap();

    let region = Rect {
        x: sealed.map.width - 16,
        y: sealed.map.height - 16,
        width: 16,
        height: 16,
    };
    session
        .render_region_now(&job::RegionRender {
            region,
            pre: job::PreImage::Base,
            ops: Vec::new(),
            ends_drag: false,
        })
        .expect("the removed composite is still read through its mapping");
    let mut resident = vec![0u16; 16 * 16 * 3];
    session.tiles.overlay_into(region, &mut resident);
    let mut expected = Vec::new();
    for y in region.y..region.y + region.height {
        let at = ((y * sealed.map.width + region.x) * 3) as usize;
        expected.extend_from_slice(&sealed.composite[at..at + 48]);
    }
    assert!(
        resident == expected,
        "the resident tile holds the composite"
    );
}

#[test]
fn strokes_on_a_reopened_project_show_the_saved_render() {
    let (mut c, root) = open_editor("strokes_reopened");
    let sealed = Sealed::read(&mut c, &root);
    let path = save_to_proposed(&mut c);

    let mut reopened = reopen(&path, "strokes_reopened_session");
    reopened.set_resident_tile_size_for_test(16);
    reopened.set_pane_geometry(LEFT, RIGHT);
    common::enter_editor(&mut reopened);
    let _ = reopened.materialize_active_tab_planes();
    common::settle_render(&mut reopened);
    let reread = Sealed::read_with(&mut reopened, &root, sealed.registration.clone());

    set_size(&mut reopened, 9.0);
    reopened.set_brush_softness_fraction(0.25);
    reopened.select_frame(0);
    drag(&mut reopened, &[(12.0, 10.0), (30.0, 22.0)]);
    reopened.select_frame(2);
    drag(&mut reopened, &[(40.0, 36.0), (52.0, 52.0)]);
    let strokes = ops(&reopened);
    assert_eq!(strokes.len(), 2);
    let info = reopened.edit_session().unwrap().info();

    let shown = displayed_whole(&mut reopened);
    assert!(reopened.save_active_project(|_| None));
    assert!(
        shown == saved_render(&path),
        "the live view differs from the saved render"
    );
    assert!(
        shown == reread.expected(&strokes, info),
        "the live view differs from the reference blend"
    );
}

// The two tests below read process-wide GPU state through the accessors that
// never open an adapter. No test in this binary runs a GPU fuse, so neither
// depends on the other's order.

#[test]
fn performance_readout_does_not_probe() {
    let controller = AppController::new();
    let view = controller.performance_view();
    assert!(
        focusforge_gpu::harness::settled_backend().is_none(),
        "building the view must not bring a backend up"
    );
    assert_eq!(view.vram_budget, performance_settings::NOT_PROBED);
    assert_eq!(view.backend, "");
    assert!(view.host_budget.ends_with("iB"), "{}", view.host_budget);
    assert!(view.vram_cap.enabled && view.gpu_enabled);
    assert_eq!(view.note, "");
}

#[test]
fn performance_vram_cap_pending_note() {
    use focusforge_core::algorithm::AlignAlgorithm;

    let config = tempfile::tempdir().unwrap();
    let mut controller = controller_with_scratch("performance_vram_note");
    open_mantis64_tab(&mut controller, "performance_vram_note", 2);
    controller.install_recent(Vec::new(), config.path().to_path_buf());

    controller.set_vram_cap("1GiB");
    assert_eq!(controller.performance_view().note, "");
    assert_eq!(
        controller.job_request_for(0).unwrap().max_vram,
        Some(1 << 30),
        "an edit before the cap settles reaches the next run"
    );

    let (lazy, _) = focusforge_gpu::harness::align_backend_for(
        focusforge_core::align::PyramidalEcc.id(),
        false,
        None,
        |_| {},
    );
    assert!(lazy.is_some());
    assert!(focusforge_gpu::harness::vram_cap_settled());
    assert!(focusforge_gpu::harness::settled_backend().is_none());

    controller.set_vram_cap("2GiB");
    assert_eq!(
        controller.performance_view().note,
        performance_settings::VRAM_PENDING_NOTE
    );
    assert_eq!(
        performance_settings::load(config.path()).max_vram_bytes(),
        Some(2 << 30),
        "the edit is saved for the next launch"
    );

    controller.set_vram_cap("8G");
    let view = controller.performance_view();
    assert_eq!(
        (view.vram_cap.text.as_str(), view.vram_cap.invalid),
        ("8G", true)
    );
    assert_eq!(
        performance_settings::load(config.path()).max_vram_bytes(),
        Some(2 << 30),
        "text that does not parse leaves the stored cap"
    );
}

#[test]
fn performance_cap_field_scrolls_to_keep_the_cursor_visible() {
    let mut controller = AppController::new();
    let offsets = |controller: &AppController| {
        let view = controller.performance_view();
        (view.host_cap.scroll_offset, view.vram_cap.scroll_offset)
    };
    assert_eq!(
        offsets(&controller),
        (0.0, 0.0),
        "a fresh view is unscrolled"
    );

    controller.host_memory_cap_cursor_moved(140.0, 100.0);
    let (host, vram) = offsets(&controller);
    assert!(
        (0.0..=100.0).contains(&(140.0 + host)) && host < 0.0,
        "the cursor lands back inside the field, got offset {host}"
    );
    assert_eq!(vram, 0.0, "the other field stays where it was");

    controller.vram_cap_cursor_moved(150.0, 120.0);
    let (host_after, vram) = offsets(&controller);
    assert_eq!(host_after, host, "the host field keeps its own offset");
    assert!((0.0..=120.0).contains(&(150.0 + vram)) && vram < 0.0);

    controller.host_memory_cap_cursor_moved(0.0, 100.0);
    controller.vram_cap_cursor_moved(0.0, 120.0);
    assert_eq!(
        offsets(&controller),
        (0.0, 0.0),
        "the cursor back at the start shows the start, never right of it"
    );
}

use focusforge_gui::{StoragePath, StorageTone, StorageView, storage_view};

/// A `<root>/focusforge`-shaped config and data directory pair under `root`.
fn storage_dirs(root: &Path) -> (PathBuf, PathBuf) {
    (root.join("config/focusforge"), root.join("data/focusforge"))
}

#[test]
fn storage_view_shows_the_four_locations() {
    let root = tempfile::tempdir().unwrap();
    let (config, data) = storage_dirs(root.path());
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("settings.toml"), "").unwrap();
    fs::create_dir_all(data.join("logs")).unwrap();
    let scratch = data.join("scratch");

    let view = storage_view(Some(&config), Some(&data), &scratch, None);
    let shown = |row: &StoragePath| (row.path.clone(), row.missing, row.available);
    assert_eq!(
        shown(&view.settings_file),
        (
            config.join("settings.toml").display().to_string(),
            false,
            true
        )
    );
    assert_eq!(
        shown(&view.logs),
        (data.join("logs").display().to_string(), false, true)
    );
    assert_eq!(
        shown(&view.crashes),
        (data.join("crashes").display().to_string(), true, true),
        "an absent directory is marked missing"
    );
    assert_eq!(
        shown(&view.scratch),
        (scratch.display().to_string(), true, true)
    );

    // Other directories give other rows: nothing is remembered between calls.
    let (config2, data2) = storage_dirs(&root.path().join("second"));
    let view2 = storage_view(Some(&config2), Some(&data2), &data2.join("scratch"), None);
    assert_eq!(
        view2.settings_file.path,
        config2.join("settings.toml").display().to_string()
    );
    assert_eq!(view2.logs.path, data2.join("logs").display().to_string());
    assert!(view2.logs.missing && view2.settings_file.missing);
}

#[test]
fn storage_view_takes_its_rows_from_the_installed_directories() {
    let root = tempfile::tempdir().unwrap();
    let (config, data) = storage_dirs(&root.path().join("installed-marker"));
    let mut controller = AppController::new();
    controller.install_recent(Vec::new(), config.clone());
    controller.set_diagnostics_dirs(Some(data.clone()), None);
    controller.set_scratch_root_for_test(data.join("scratch"));

    let view = controller.storage_view();
    for (row, leaf) in [
        (&view.settings_file, "settings.toml"),
        (&view.logs, "logs"),
        (&view.crashes, "crashes"),
        (&view.scratch, "scratch"),
    ] {
        assert!(row.available, "{leaf} resolved");
        assert!(
            row.path.contains("installed-marker") && row.path.ends_with(leaf),
            "{leaf} row shows {:?}",
            row.path
        );
    }
}

#[test]
fn storage_view_without_directories() {
    let fallback = std::env::temp_dir().join("focusforge-scratch");
    let none = storage_view(None, None, &fallback, None);
    for row in [&none.settings_file, &none.logs, &none.crashes] {
        assert!(
            !row.available && row.path.is_empty() && !row.missing,
            "{row:?}"
        );
    }
    assert!(none.scratch.available);
    assert_eq!(none.scratch.path, fallback.display().to_string());

    let root = tempfile::tempdir().unwrap();
    let (config, data) = storage_dirs(root.path());
    let no_data = storage_view(Some(&config), None, &fallback, None);
    assert!(no_data.settings_file.available);
    assert!(!no_data.logs.available && !no_data.crashes.available);

    let no_config = storage_view(None, Some(&data), &fallback, None);
    assert!(!no_config.settings_file.available);
    assert!(no_config.logs.available && no_config.crashes.available);
}

#[test]
fn storage_view_abbreviates_the_home_directory() {
    // The test spells its paths with `/`; on Windows the view joins them with `\`.
    let home = Path::new("/home/someone");
    let config = home.join(".config/focusforge");
    let data = Path::new("/srv/data/focusforge");
    let scratch = home.join(".local/scratch");
    let view = storage_view(Some(&config), Some(data), &scratch, Some(home));
    assert_eq!(
        view.settings_file.path.replace('\\', "/"),
        "~/.config/focusforge/settings.toml"
    );
    assert_eq!(view.scratch.path.replace('\\', "/"), "~/.local/scratch");
    assert_eq!(
        view.logs.path.replace('\\', "/"),
        "/srv/data/focusforge/logs",
        "a path outside the home directory stays absolute"
    );

    let sibling = storage_view(None, None, Path::new("/home/someone2/scratch"), Some(home));
    assert_eq!(
        sibling.scratch.path.replace('\\', "/"),
        "/home/someone2/scratch",
        "a sibling directory sharing the home name's prefix is not under it"
    );
    let at_home = storage_view(None, None, home, Some(home));
    assert_eq!(at_home.scratch.path.replace('\\', "/"), "~");
}

/// Writes one stage-cache entry file under `<scratch>/stage-cache/<stage>/`.
fn seed_stage_entry(scratch: &Path, stage: &str, name: &str) {
    let dir = scratch.join("stage-cache").join(stage);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), b"{}").unwrap();
}

/// A file in each scratch sibling the clear must leave alone.
fn seed_scratch_siblings(scratch: &Path) -> Vec<PathBuf> {
    ["resume", "projects", "planes"]
        .into_iter()
        .map(|dir| {
            let sentinel = scratch.join(dir).join("sentinel");
            fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            fs::write(&sentinel, b"keep").unwrap();
            sentinel
        })
        .collect()
}

fn storage_status(controller: &AppController) -> (String, StorageTone) {
    let view: StorageView = controller.storage_view();
    (view.status, view.status_tone)
}

#[test]
fn storage_clear_removes_only_stage_entries() {
    let scratch = tempfile::tempdir().unwrap();
    let scratch = scratch.path();
    seed_stage_entry(scratch, "align", "0123456789abcdef.json");
    seed_stage_entry(scratch, "align", "fedcba9876543210.json");
    seed_stage_entry(scratch, "other", "00000000000000aa.json");
    let sentinels = seed_scratch_siblings(scratch);

    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(scratch.to_path_buf());
    controller.clear_stage_cache();

    assert_eq!(
        storage_status(&controller),
        ("Cleared 3 stage entries.".to_string(), StorageTone::Success)
    );
    assert!(
        fs::read_dir(scratch.join("stage-cache"))
            .unwrap()
            .next()
            .is_none(),
        "no stage entry directory is left"
    );
    for sentinel in &sentinels {
        assert!(sentinel.exists(), "{} was removed", sentinel.display());
    }

    seed_stage_entry(scratch, "align", "0123456789abcdef.json");
    controller.clear_stage_cache();
    assert_eq!(storage_status(&controller).0, "Cleared 1 stage entry.");
}

#[test]
fn storage_clear_on_an_empty_scratch_root() {
    let scratch = tempfile::tempdir().unwrap();
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(scratch.path().to_path_buf());
    assert!(!scratch.path().join("stage-cache").exists());

    controller.clear_stage_cache();

    assert_eq!(
        storage_status(&controller),
        ("Cleared 0 stage entries.".to_string(), StorageTone::Success)
    );
}

#[test]
fn storage_clear_is_refused_while_a_run_is_live() {
    let scratch = common::tree("storage_refused_scratch");
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(scratch.clone());
    seed_stage_entry(&scratch, "align", "0123456789abcdef.json");
    let sentinels = seed_scratch_siblings(&scratch);
    assert_eq!(
        storage_status(&controller),
        (String::new(), StorageTone::None),
        "nothing to report before any clear"
    );
    assert!(controller.storage_view().clear_enabled);

    open_mantis64_tab(&mut controller, "storage_refused_a", 2);
    open_mantis64_tab(&mut controller, "storage_refused_b", 2);
    controller.select_project(0);
    assert!(controller.start_fuse());
    controller.select_project(1);
    assert!(!controller.job_running(), "the front tab holds no run");
    assert!(controller.any_job_running(), "a background tab does");

    let view = controller.storage_view();
    assert!(!view.clear_enabled);
    assert_eq!(
        (view.status.as_str(), view.status_tone),
        ("A run is active.", StorageTone::Warning)
    );
    controller.clear_stage_cache();
    assert!(
        scratch
            .join("stage-cache/align/0123456789abcdef.json")
            .exists(),
        "the refused call removed an entry"
    );
    for sentinel in &sentinels {
        assert!(sentinel.exists());
    }
    assert_eq!(
        controller.storage_view().status_tone,
        StorageTone::Warning,
        "a refused call reports no result"
    );

    settle_all(&mut controller);
    let view = controller.storage_view();
    assert!(view.clear_enabled, "the button is enabled again");
    assert_eq!(
        (view.status.as_str(), view.status_tone),
        ("", StorageTone::None)
    );
}

#[cfg(unix)]
#[test]
fn storage_clear_failure_is_the_status_line() {
    use std::os::unix::fs::PermissionsExt;

    /// Restores the directory's write bit so the temp dir can be removed
    /// even when an assertion fails.
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let scratch = tempfile::tempdir().unwrap();
    seed_stage_entry(scratch.path(), "align", "0123456789abcdef.json");
    let stage_cache = scratch.path().join("stage-cache");
    let _restore = Restore(stage_cache.clone());
    fs::set_permissions(&stage_cache, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(stage_cache.join(".probe"), b"").is_ok() {
        eprintln!("skipping: this filesystem does not enforce directory permissions");
        return;
    }

    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(scratch.path().to_path_buf());
    let rows_before = controller.storage_view();
    controller.clear_stage_cache();

    let (status, tone) = storage_status(&controller);
    assert_eq!(tone, StorageTone::Failure);
    assert_eq!(status, "Could not clear the cache: Permission denied.");
    assert!(
        !status.contains(scratch.path().to_str().unwrap()),
        "the line names no path"
    );
    let rows_after = controller.storage_view();
    assert_eq!(rows_after.settings_file, rows_before.settings_file);
    assert_eq!(rows_after.scratch, rows_before.scratch);
    assert!(rows_after.clear_enabled, "the controller keeps working");
}

/// A cause that is not a permission error reads the same way: the io text
/// without its `(os error N)` tail, ending in one period.
#[cfg(unix)]
#[test]
fn storage_clear_failure_line_names_the_cause_without_the_os_error_code() {
    let scratch = tempfile::tempdir().unwrap();
    fs::write(scratch.path().join("stage-cache"), b"not a directory").unwrap();

    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(scratch.path().to_path_buf());
    controller.clear_stage_cache();

    assert_eq!(
        storage_status(&controller),
        (
            "Could not clear the cache: File exists.".to_string(),
            StorageTone::Failure
        )
    );
}

mod notification {
    use super::{AppController, SETTLE, common};
    use focusforge_core::params::ParamValue;
    use focusforge_gui::notifications::{Kind, Notification};

    fn every_frame_clamps(name: &str, excluded: Option<usize>) -> AppController {
        let mut controller = common::controller_with_frames(name, 4);
        controller.set_align_algorithm("dummy-align");
        controller.set_param_value("prepare", "equalize", ParamValue::Bool(true));
        controller.set_param_value("prepare", "equalize-target", ParamValue::Float(0.5));
        controller.set_param_value("prepare", "equalize-max-gain", ParamValue::Float(1.25));
        if let Some(index) = excluded {
            controller.toggle_frame_excluded(index);
        }
        assert!(controller.start_fuse());
        common::wait_for_settle(&mut controller, SETTLE);
        controller
    }

    fn warnings(controller: &AppController) -> Vec<Notification> {
        controller
            .notifications()
            .iter()
            .filter(|n| n.kind == Kind::Warning)
            .cloned()
            .collect()
    }

    #[test]
    fn every_brightness_warning_becomes_its_own_record() {
        let controller = every_frame_clamps("notification_every_warning", None);
        let records = warnings(&controller);
        assert_eq!(records.len(), 4, "one record per clamped frame");
        for (i, record) in records.iter().enumerate() {
            assert!(
                record.message.starts_with(&format!("frame {i}:")),
                "{}",
                record.message
            );
            assert!(record.message.contains("1.25"), "{}", record.message);
        }
    }

    #[test]
    fn a_brightness_record_names_its_frame_file() {
        let controller = every_frame_clamps("notification_frame_file", Some(0));
        let records = warnings(&controller);
        assert_eq!(
            records.len(),
            3,
            "the excluded frame is not part of the run"
        );
        let files: Vec<_> = records.iter().map(|r| r.frame.as_deref()).collect();
        assert_eq!(files, [Some("01.tif"), Some("02.tif"), Some("03.tif")]);
        assert!(
            records[0].message.starts_with("frame 0:"),
            "the index is the run's own"
        );
    }

    #[test]
    fn kept_unaligned_frames_record_the_count() {
        let mut controller = super::controller_on_mantis_512("notification_kept_unaligned");
        assert!(controller.start_fuse());
        common::wait_for_settle(&mut controller, std::time::Duration::from_secs(300));
        assert!(
            controller
                .notifications()
                .iter()
                .all(|n| !n.title.ends_with("unaligned")),
            "nothing is recorded before Keep"
        );
        controller.keep_fused().expect("a parked run keeps");
        let kept: Vec<_> = controller
            .notifications()
            .iter()
            .filter(|n| n.title == "4 frames unaligned")
            .collect();
        assert_eq!(kept.len(), 1, "Keep records the count once");
        assert_eq!(kept[0].kind, Kind::Warning);
        assert!(kept[0].message.contains("4 frames unaligned"));
        assert!(kept[0].project_title.is_some());
    }

    #[test]
    fn a_settled_run_publishes_once_across_polls() {
        let mut controller = every_frame_clamps("notification_once", None);
        let before = controller.notifications().len();
        assert!(before > 0);
        for _ in 0..5 {
            assert!(controller.settle_job().is_none());
        }
        assert_eq!(controller.notifications().len(), before);
    }

    #[test]
    fn refused_add_records_its_full_message() {
        let mut controller = common::controller_with_frames("notification_refused", 3);
        controller.set_align_algorithm("dummy-align");
        assert!(controller.start_fuse());
        let refused = controller
            .add_frames(&[common::fixture_frame("mantis-64", 4)])
            .expect_err("a running job refuses an add");
        let record = controller
            .notifications()
            .last()
            .expect("the refusal is recorded");
        assert_eq!(record.message, refused.to_string());
        assert_eq!(record.kind, Kind::Failure);
        assert_eq!(
            controller.notifications().len(),
            1,
            "one refusal, one record"
        );
        common::wait_for_settle(&mut controller, SETTLE);
    }

    #[test]
    fn bell_over_projects_closes_projects_and_opens_the_center() {
        let mut controller = AppController::new();
        controller.toggle_projects_overlay();
        controller.set_diagnostics_dirs(None, None);
        controller.scan_crash_notice();
        assert_eq!(controller.unread_notifications(), 1);
        assert!(
            controller.visible_toast().is_none(),
            "Projects shows no toast"
        );

        controller.open_center();
        assert!(!controller.projects_overlay_open());
        assert!(controller.notification_center_open());
        assert_eq!(controller.unread_notifications(), 0);
    }

    #[test]
    fn opening_projects_closes_the_center_without_reading() {
        let mut controller = AppController::new();
        controller.open_center();
        controller.set_window_focused(1, false);
        controller.set_diagnostics_dirs(None, None);
        controller.scan_crash_notice();
        assert_eq!(
            controller.unread_notifications(),
            1,
            "background arrival stays unread"
        );

        controller.toggle_projects_overlay();
        assert!(!controller.notification_center_open());
        assert_eq!(controller.unread_notifications(), 1);
    }

    #[test]
    fn closing_projects_shows_unread_without_toast_replay() {
        let mut controller = AppController::new();
        controller.toggle_projects_overlay();
        controller.set_diagnostics_dirs(None, None);
        controller.scan_crash_notice();
        controller.toggle_projects_overlay();
        assert!(
            controller.visible_toast().is_none(),
            "nothing suppressed replays"
        );
        assert!(!controller.notification_center_open());
        assert_eq!(controller.unread_notifications(), 1);
    }
}

mod native_delivery {
    use super::{AppController, SETTLE, common};
    use focusforge_gui::native_notify;
    use focusforge_gui::notifications::{Banner, context_line};
    use std::cell::Cell;

    fn crash_notice(controller: &mut AppController) {
        controller.set_diagnostics_dirs(None, None);
        controller.scan_crash_notice();
    }

    #[test]
    fn background_arrival_is_banner_once() {
        let mut controller = common::controller_with_frames("native_once", 3);
        controller.set_align_algorithm("dummy-align");
        controller.set_window_focused(1, false);
        crash_notice(&mut controller);
        assert!(controller.start_fuse());
        controller
            .add_frames(&[common::fixture_frame("mantis-64", 4)])
            .expect_err("a running job refuses an add");

        let titles: Vec<_> = controller
            .take_native_notifications()
            .into_iter()
            .map(|b| b.title)
            .collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert_ne!(titles[0], titles[1]);
        for focused in [true, false, true, false] {
            controller.set_window_focused(1, focused);
            assert!(controller.take_native_notifications().is_empty());
        }
        common::wait_for_settle(&mut controller, SETTLE);
    }

    #[test]
    fn focus_moving_between_app_windows_is_not_background() {
        let mut controller = AppController::new();
        controller.set_window_focused(1, true);
        controller.set_window_focused(1, false);
        controller.set_window_focused(2, true);
        crash_notice(&mut controller);
        controller.set_window_focused(2, false);
        controller.set_window_focused(1, true);
        crash_notice(&mut controller);
        assert!(controller.take_native_notifications().is_empty());

        controller.set_window_focused(1, false);
        crash_notice(&mut controller);
        assert_eq!(controller.take_native_notifications().len(), 1);
    }

    #[test]
    fn destroyed_focused_window_stops_counting() {
        let mut controller = AppController::new();
        controller.set_window_focused(2, true);
        controller.set_window_focused(2, false);
        crash_notice(&mut controller);
        assert_eq!(controller.take_native_notifications().len(), 1);
    }

    #[test]
    fn projects_suppresses_native_delivery() {
        let mut controller = AppController::new();
        controller.toggle_projects_overlay();
        controller.set_window_focused(1, false);
        crash_notice(&mut controller);
        assert_eq!(controller.unread_notifications(), 1);
        assert!(controller.take_native_notifications().is_empty());
    }

    #[test]
    fn foreground_arrival_is_never_native() {
        let mut controller = AppController::new();
        controller.set_window_focused(1, true);
        crash_notice(&mut controller);
        assert!(controller.take_native_notifications().is_empty());
    }

    #[test]
    fn banner_carries_title_and_context_only() {
        let mut controller = AppController::new();
        controller.set_window_focused(1, false);
        crash_notice(&mut controller);
        let record = controller.notifications()[0].clone();
        let banners = controller.take_native_notifications();
        assert_eq!(
            banners,
            [Banner {
                title: record.title.clone(),
                body: context_line(&record),
            }]
        );
        assert!(!banners[0].body.contains(&record.message));
    }

    #[test]
    fn native_send_failure_keeps_history_and_unread() {
        let mut controller = AppController::new();
        controller.set_window_focused(1, false);
        crash_notice(&mut controller);
        crash_notice(&mut controller);
        let banners = controller.take_native_notifications();
        assert_eq!(banners.len(), 2);

        let calls = Cell::new(0);
        native_notify::deliver(banners, |_| {
            calls.set(calls.get() + 1);
            Err::<(), _>("no notification service")
        });
        assert_eq!(calls.get(), 2, "one attempt per banner, no retry");
        assert_eq!(controller.notifications().len(), 2);
        assert_eq!(controller.unread_notifications(), 2);
        assert!(controller.take_native_notifications().is_empty());
    }
}

#[test]
fn an_open_hold_keeps_the_load_in_flight() {
    let root = common::tree("open_hold_frames");
    for i in 0..3 {
        common::copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    let mut controller = AppController::new();
    controller.set_open_hold(std::time::Duration::from_millis(600));
    let started = std::time::Instant::now();
    let handle = controller
        .open_paths(&[root])
        .expect("the load starts on a folder of frames");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        controller.load_in_flight(),
        "the hold keeps the load in flight"
    );
    handle.join().expect("the load worker did not panic");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(600),
        "the load finished after {:?}",
        started.elapsed()
    );
    controller
        .settle_open()
        .expect("the load parked an outcome");
    assert!(!controller.load_in_flight());
}

// --- Checked-in folder and project fixtures: each opens to the result it
// exists for. Every case works on a copy under `CARGO_TARGET_TMPDIR`. -----

/// A copy of `fixtures/<set>` named `name` under `CARGO_TARGET_TMPDIR`.
fn fixture_copy(set: &str, name: &str) -> PathBuf {
    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let dest = tree(name);
    copy_dir(&common::workspace_root().join("fixtures").join(set), &dest);
    dest
}

#[test]
fn fixture_two_stacks_ask_the_grouping_question() {
    let root = fixture_copy("open-two-stacks", "fixture_two_stacks");
    let mut controller = AppController::new();
    match common::open_and_settle(&mut controller, &[root]) {
        OpenOutcome::Question(question, _) => {
            assert_eq!(question.frame_count, 6);
            assert_eq!(question.project_count, 2);
        }
        _ => panic!("two stacks in one folder must raise the grouping question"),
    }
    assert!(controller.pending_question().is_some());
    assert!(controller.project_tabs().is_empty());

    common::answer_and_settle(&mut controller, OpenAs::NProjects);
    assert_eq!(controller.project_tabs().len(), 2);
}

#[test]
fn fixture_root_and_subfolder_reports_the_skipped_subfolder() {
    let root = fixture_copy("open-root-and-subfolder", "fixture_root_and_subfolder");
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.project_tabs().len(), 1);
    assert_eq!(
        controller.images_panel().header_text,
        "3 frames \u{b7} 0 excluded"
    );

    let record = controller
        .notifications()
        .first()
        .expect("the skipped subfolder must be reported");
    assert_eq!(record.title, "Some images were skipped");
    assert_eq!(
        record.message,
        format!("1 subfolder skipped:\n{}", root.join("notes").display())
    );
    let cards = controller.project_cards();
    let skipped = cards
        .iter()
        .find(|card| card.kind == "not-a-project")
        .expect("the skipped subfolder keeps a card");
    assert_eq!(skipped.project_name, "notes");
}

#[test]
fn fixture_rejected_stack_opens_then_fails_and_retries() {
    let root = fixture_copy("open-rejected", "fixture_rejected");
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("fixture_rejected_scratch"));
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.project_tabs().len(), 1);

    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    assert!(
        matches!(controller.job_view().state, job::RunState::Failed(_)),
        "the cut frame must fail the run, got {:?}",
        controller.job_view().state
    );
    let message = controller.job_failure_message().unwrap_or_default();
    assert!(message.contains("02.tif"), "{message}");

    assert!(controller.retry_fuse(), "a failed run must be retryable");
    assert!(controller.job_running());
    common::wait_for_settle(&mut controller, Duration::from_secs(60));
    assert!(matches!(
        controller.job_view().state,
        job::RunState::Failed(_)
    ));
}

#[test]
fn fixture_corrupt_project_is_refused() {
    let dir = fixture_copy("projects", "fixture_corrupt_project");
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[dir.join("corrupt.ffp")]),
        OpenOutcome::Refused(_)
    ));
    assert_eq!(
        controller.last_refusal(),
        Some("This file is not a FocusForge project.")
    );
    assert_eq!(
        controller.refusal_title(),
        Some("This project cannot be opened")
    );
    assert!(controller.project_tabs().is_empty());
}

#[test]
fn fixture_mismatched_project_raises_the_mismatch_and_opens_anyway() {
    let dir = fixture_copy("projects", "fixture_mismatched_project");
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[dir.join("mismatch.ffp")]),
        OpenOutcome::Mismatch(_)
    ));
    assert!(controller.pending_mismatch().is_some());
    assert!(controller.project_tabs().is_empty());

    controller.open_anyway();
    assert!(controller.pending_mismatch().is_none());
    assert_eq!(controller.project_tabs().len(), 1);
}

#[test]
fn fixture_project_without_its_frames_opens_as_sources_missing() {
    let dir = fixture_copy("projects", "fixture_missing_project");
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[dir.join("missing.ffp")]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.project_tabs().len(), 1);
    assert_eq!(controller.gallery_failure_kind(0), Some("sources-missing"));
    let (title, _body, file_name) = controller
        .sources_notice()
        .expect("the missing frames raise the notice");
    assert_eq!(title, "The frames are no longer there");
    assert_eq!(file_name, "missing");
}

#[test]
fn fixture_valid_project_opens_over_its_frames() {
    let dir = fixture_copy("projects", "fixture_valid_project");
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[dir.join("valid.ffp")]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.project_tabs().len(), 1);
    assert!(controller.pending_mismatch().is_none());
    assert!(controller.sources_notice().is_none());
    assert_eq!(controller.gallery_failure_kind(0), None);
    assert_eq!(
        controller.images_panel().header_text,
        "3 frames \u{b7} 0 excluded"
    );
}

/// A stroke of the shape a driver synthesises -- one press, one move to a far
/// point, one release -- a long drag flushed every few moves, and a drag in
/// stacking mode each reach their settled render in under a second.
#[test]
fn stroke_of_the_journey_shape_lands_quickly() {
    use std::time::{Duration, Instant};

    const BUDGET: Duration = Duration::from_secs(1);
    let (mut c, _root) = open_editor("stroke_of_the_journey_shape");
    set_size(&mut c, 8.0);
    let centre = at(MAP_WIDTH as f32 / 2.0, MAP_HEIGHT as f32 / 2.0);
    let far = at(MAP_WIDTH as f32 - 1.0, MAP_HEIGHT as f32 - 1.0);

    let started = Instant::now();
    press(&mut c, centre);
    c.pointer_moved(far.0, far.1);
    c.flush_live_stroke();
    release(&mut c, far);
    common::settle_region_renders(&mut c);
    let one_move = started.elapsed();
    assert_eq!(ops(&c).len(), 1, "the one-move stroke is appended");

    let started = Instant::now();
    press(&mut c, centre);
    for step in 0..200 {
        let t = step as f32 / 199.0;
        let x = 4.0 + (MAP_WIDTH as f32 - 8.0) * t;
        let y = MAP_HEIGHT as f32 / 2.0 + 20.0 * (t * 12.0).sin();
        let point = at(x, y);
        c.pointer_moved(point.0, point.1);
        if step % 7 == 6 {
            c.flush_live_stroke();
        }
    }
    release(&mut c, far);
    common::settle_region_renders(&mut c);
    let long_drag = started.elapsed();
    assert_eq!(ops(&c).len(), 2, "the long drag is appended");

    assert!(c.set_mode(ShellMode::Stacking));
    let started = Instant::now();
    drag_screen(&mut c, &[centre, far]);
    let stacking = started.elapsed();
    assert_eq!(ops(&c).len(), 2, "a stacking-mode drag appends no stroke");

    println!("one-move stroke: {one_move:?}");
    println!("200-move drag, flush every 7: {long_drag:?}");
    println!("stacking-mode drag: {stacking:?}");
    for (name, took) in [
        ("one-move stroke", one_move),
        ("200-move drag", long_drag),
        ("stacking-mode drag", stacking),
    ] {
        assert!(took < BUDGET, "{name} took {took:?}, over {BUDGET:?}");
    }
}
