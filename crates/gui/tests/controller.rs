// SPDX-License-Identifier: GPL-3.0-or-later

//! `AppController` is plain Rust: no Slint backend is initialised anywhere
//! in this file, and none is needed -- the callback-forwarding rule is what
//! these tests pin.

mod common;

use std::fs;
use std::sync::{Arc, Mutex};

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
    is_self_literal_assignment(body)
}

/// `self.<ident> = <literal>;` where `<ident>` is a single plain identifier
/// (no `.`, no operator) and `<literal>` is `true`, `false`, a quoted string,
/// or a number with an optional unit suffix (`1`, `0.5`, `1px`) -- never a
/// bare identifier (which would be another property, e.g. `self.x = y;`) or
/// anything with a call or an operator in it. See [`is_single_root_call`].
fn is_self_literal_assignment(body: &str) -> bool {
    let Some(rest) = body.strip_prefix("self.") else {
        return false;
    };
    let Some((lhs, rhs_with_semi)) = rest.split_once('=') else {
        return false;
    };
    let lhs = lhs.trim();
    if lhs.is_empty()
        || !lhs
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return false;
    }
    let Some(rhs) = rhs_with_semi.trim().strip_suffix(';') else {
        return false;
    };
    body.matches(';').count() == 1 && is_literal_token(rhs.trim())
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

#[test]
fn editor_mode_is_unreachable_in_this_phase() {
    let mut controller = AppController::new();
    assert!(
        !controller.editor_enabled(),
        "editor_enabled() must be false"
    );

    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    assert!(
        !controller.set_mode(ShellMode::Editor),
        "set_mode(Editor) must return false"
    );
    assert!(
        events.lock().unwrap().is_empty(),
        "set_mode(Editor) must emit no ModeChanged"
    );
    assert_eq!(
        controller.mode(),
        ShellMode::Stacking,
        "mode must not have changed"
    );
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
