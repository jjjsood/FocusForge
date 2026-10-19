// SPDX-License-Identifier: GPL-3.0-or-later

//! The design language as code, read back through the generated Rust API.
//! Every value here is pinned so the `.slint` theme and this file cannot
//! drift apart.
//!
//! No display server and no GPU: `i_slint_backend_testing::init_no_event_loop()`
//! simulates a windowing system without one and renders no pixels. The
//! backend is per-thread and a second `init` on the same thread is an error,
//! so every test reaches the probe through [`probe`], which guards the
//! `init` call behind a `thread_local!`.

mod common;

use std::fs;

use focusforge_gui::{Theme, ThemeProbe};
use slint::{Color, Global};

/// A headless `ThemeProbe` instance; the `thread_local!` guard makes a
/// repeat call on the same thread safe.
fn probe() -> ThemeProbe {
    thread_local! {
        static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INIT.with(|done| {
        if !done.get() {
            i_slint_backend_testing::init_no_event_loop();
            done.set(true);
        }
    });
    ThemeProbe::new().unwrap()
}

fn hex_to_rgb(v: u32) -> (u8, u8, u8) {
    (
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    )
}

fn assert_color(actual: Color, expected_hex: u32, name: &str) {
    let expected = hex_to_rgb(expected_hex);
    let got = (actual.red(), actual.green(), actual.blue());
    assert_eq!(
        got, expected,
        "token `{name}` = {got:02x?}, expected {expected_hex:#08x}"
    );
}

#[test]
fn tokens_match_the_recorded_values() {
    let probe = probe();
    let theme = Theme::get(&probe);

    // The 20 semantic colours.
    assert_color(theme.get_surface_canvas(), 0x080808, "surface.canvas");
    assert_color(theme.get_surface_base(), 0x121212, "surface.base");
    assert_color(theme.get_surface_raised(), 0x1A1A1A, "surface.raised");
    assert_color(theme.get_surface_overlay(), 0x242424, "surface.overlay");
    assert_color(theme.get_surface_hover(), 0x242424, "surface.hover");
    assert_color(theme.get_surface_disabled(), 0x3A3A3A, "surface.disabled");
    assert_color(theme.get_border_default(), 0x2E2E2E, "border.default");
    assert_color(theme.get_border_strong(), 0x3A3A3A, "border.strong");
    assert_color(theme.get_text_primary(), 0xF2F2F2, "text.primary");
    assert_color(theme.get_text_secondary(), 0xC8C8C8, "text.secondary");
    assert_color(theme.get_text_muted(), 0x9A9A9A, "text.muted");
    assert_color(theme.get_text_disabled(), 0x5A5A5A, "text.disabled");
    assert_color(theme.get_text_on_accent(), 0x080808, "text.onAccent");
    assert_color(theme.get_accent_default(), 0xE8763A, "accent.default");
    assert_color(theme.get_accent_hover(), 0xF08A55, "accent.hover");
    assert_color(theme.get_accent_pressed(), 0xC85F28, "accent.pressed");
    assert_color(theme.get_accent_subtle(), 0x2A1810, "accent.subtle");
    assert_color(theme.get_status_failure(), 0xD9544D, "status.failure");
    assert_color(theme.get_status_warning(), 0xD9A441, "status.warning");
    assert_color(theme.get_status_success(), 0x5AA469, "status.success");

    // Type roles -- size / weight / line-height / letter-spacing / case.
    let section = theme.get_text_section();
    assert_eq!(section.size, 10.0, "text.section size");
    assert_eq!(section.weight, 600, "text.section weight");
    assert_eq!(section.line_height, 1.2, "text.section line-height");
    assert_eq!(section.letter_spacing, 0.6, "text.section letter-spacing");
    assert!(section.uppercase, "text.section uppercase");

    let body = theme.get_text_body();
    assert_eq!(body.size, 12.0, "text.body size");
    assert_eq!(body.weight, 400, "text.body weight");
    assert_eq!(body.line_height, 1.45, "text.body line-height");
    assert_eq!(body.letter_spacing, 0.0, "text.body letter-spacing");
    assert!(!body.uppercase, "text.body uppercase");

    let value = theme.get_text_value();
    assert_eq!(value.size, 12.0, "text.value size");
    assert_eq!(value.weight, 500, "text.value weight");
    assert_eq!(value.line_height, 1.45, "text.value line-height");
    assert_eq!(value.letter_spacing, 0.0, "text.value letter-spacing");
    assert!(!value.uppercase, "text.value uppercase");

    let emphasis = theme.get_text_emphasis();
    assert_eq!(emphasis.size, 13.0, "text.emphasis size");
    assert_eq!(emphasis.weight, 400, "text.emphasis weight");
    assert_eq!(emphasis.line_height, 1.4, "text.emphasis line-height");
    assert_eq!(emphasis.letter_spacing, 0.0, "text.emphasis letter-spacing");
    assert!(!emphasis.uppercase, "text.emphasis uppercase");

    let title = theme.get_text_title();
    assert_eq!(title.size, 15.0, "text.title size");
    assert_eq!(title.weight, 600, "text.title weight");
    assert_eq!(title.line_height, 1.3, "text.title line-height");
    assert_eq!(title.letter_spacing, 0.0, "text.title letter-spacing");
    assert!(!title.uppercase, "text.title uppercase");

    let display = theme.get_text_display();
    assert_eq!(display.size, 20.0, "text.display size");
    assert_eq!(display.weight, 600, "text.display weight");
    assert_eq!(display.line_height, 1.25, "text.display line-height");
    assert_eq!(display.letter_spacing, 0.0, "text.display letter-spacing");
    assert!(!display.uppercase, "text.display uppercase");

    // Spacing -- named by step, not by sequential index.
    assert_eq!(theme.get_space_1(), 4.0, "space-1");
    assert_eq!(theme.get_space_2(), 8.0, "space-2");
    assert_eq!(theme.get_space_3(), 12.0, "space-3");
    assert_eq!(theme.get_space_4(), 16.0, "space-4");
    assert_eq!(theme.get_space_6(), 24.0, "space-6");
    assert_eq!(theme.get_space_8(), 32.0, "space-8");
    assert_eq!(theme.get_space_12(), 48.0, "space-12");

    // Radii.
    assert_eq!(theme.get_radius_sm(), 3.0, "radius-sm");
    assert_eq!(theme.get_radius_md(), 5.0, "radius-md");
    assert_eq!(theme.get_radius_lg(), 8.0, "radius-lg");
    assert_eq!(theme.get_radius_full(), 999.0, "radius-full");

    // Icon size ladder -- 20 / 16 / 14.
    assert_eq!(theme.get_icon_lg(), 20.0, "icon-lg");
    assert_eq!(theme.get_icon_md(), 16.0, "icon-md");
    assert_eq!(theme.get_icon_sm(), 14.0, "icon-sm");
}

#[test]
fn accent_is_the_only_brand_hue() {
    let probe = probe();
    let theme = Theme::get(&probe);

    let non_neutrals: [(&str, Color); 7] = [
        ("accent.default", theme.get_accent_default()),
        ("accent.hover", theme.get_accent_hover()),
        ("accent.pressed", theme.get_accent_pressed()),
        ("accent.subtle", theme.get_accent_subtle()),
        ("status.failure", theme.get_status_failure()),
        ("status.warning", theme.get_status_warning()),
        ("status.success", theme.get_status_success()),
    ];
    let non_neutral_names: Vec<&str> = non_neutrals.iter().map(|(n, _)| *n).collect();

    let all: [(&str, Color); 20] = [
        ("surface.canvas", theme.get_surface_canvas()),
        ("surface.base", theme.get_surface_base()),
        ("surface.raised", theme.get_surface_raised()),
        ("surface.overlay", theme.get_surface_overlay()),
        ("surface.hover", theme.get_surface_hover()),
        ("surface.disabled", theme.get_surface_disabled()),
        ("border.default", theme.get_border_default()),
        ("border.strong", theme.get_border_strong()),
        ("text.primary", theme.get_text_primary()),
        ("text.secondary", theme.get_text_secondary()),
        ("text.muted", theme.get_text_muted()),
        ("text.disabled", theme.get_text_disabled()),
        ("text.onAccent", theme.get_text_on_accent()),
        ("accent.default", theme.get_accent_default()),
        ("accent.hover", theme.get_accent_hover()),
        ("accent.pressed", theme.get_accent_pressed()),
        ("accent.subtle", theme.get_accent_subtle()),
        ("status.failure", theme.get_status_failure()),
        ("status.warning", theme.get_status_warning()),
        ("status.success", theme.get_status_success()),
    ];

    for (name, color) in all {
        let zero_chroma = color.red() == color.green() && color.green() == color.blue();
        let is_named_non_neutral = non_neutral_names.contains(&name);
        assert!(
            zero_chroma || is_named_non_neutral,
            "token `{name}` = {:02x}{:02x}{:02x} is a brand hue outside the recorded seven",
            color.red(),
            color.green(),
            color.blue()
        );
    }

    // The accent itself is exactly #E8763A -- matching assets/logo.svg.
    assert_color(theme.get_accent_default(), 0xE8763A, "accent.default");
}

/// Extracts the `export global Theme { ... }` block from `ui/theme.slint`,
/// up to (not including) `ThemeProbe`'s declaration.
fn theme_block(contents: &str) -> &str {
    let start = contents
        .find("export global Theme {")
        .expect("theme.slint must declare `export global Theme`");
    let end = contents
        .find("export component ThemeProbe")
        .expect("theme.slint must declare `ThemeProbe`");
    &contents[start..end]
}

#[test]
fn semantic_values_reference_primitives() {
    let path = common::ui_dir().join("theme.slint");
    let contents = fs::read_to_string(&path).unwrap();

    assert!(
        !contents.contains("export global Primitives"),
        "Primitives must not be exported: specimen-ramp \
         values are unreachable from anywhere else by construction"
    );
    assert!(
        contents.contains("global Primitives {"),
        "Primitives global not found in {}",
        path.display()
    );

    let block = theme_block(&contents);
    for (i, line) in block.lines().enumerate() {
        if line.contains("<color>") {
            assert!(
                !line.contains('#'),
                "theme.slint:{}: semantic colour repeats a hex literal instead \
                 of referencing a primitive: {line}",
                i + 1
            );
            assert!(
                line.contains("Primitives."),
                "theme.slint:{}: semantic colour does not reference Primitives: {line}",
                i + 1
            );
        }
    }
}

/// True when `line` contains a colour literal: a `#` followed by 3, 4, 6 or
/// 8 hex digits, or an `rgb(` / `rgba(` / `hsv(` call.
fn line_has_color_literal(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                j += 1;
            }
            let len = j - (i + 1);
            if matches!(len, 3 | 4 | 6 | 8) {
                return true;
            }
        }
        i += 1;
    }
    line.contains("rgb(") || line.contains("rgba(") || line.contains("hsv(")
}

/// Lines (1-indexed) in `contents` that carry a colour literal.
fn color_literal_lines(contents: &str) -> Vec<usize> {
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| line_has_color_literal(line))
        .map(|(i, _)| i + 1)
        .collect()
}

#[test]
fn no_colour_literal_outside_the_theme() {
    let files = common::find_slint_files(&common::ui_dir());

    let mut violations = Vec::new();
    for path in files {
        if path.file_name().and_then(|n| n.to_str()) == Some("theme.slint") {
            continue;
        }
        let contents = fs::read_to_string(&path).unwrap();
        for line in color_literal_lines(&contents) {
            violations.push(format!("{}:{line}", path.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "colour literal(s) found outside theme.slint: {violations:?}"
    );
}

#[test]
fn the_colour_scanner_catches_a_literal() {
    // `no_colour_literal_outside_the_theme` passing proves nothing on its
    // own: a walk that silently finds zero violations looks identical
    // whether the scanner works or is vacuously never matching anything.
    // This test proves the scanner itself catches a real literal, so the
    // other test's green means "scanned and clean", not "scanned nothing".
    let sample = "component Foo inherits Rectangle {\n    background: #ff0000;\n}\n";
    assert_eq!(
        color_literal_lines(sample),
        vec![2],
        "scanner failed to catch an inline colour literal"
    );
}

/// `EnumVariant.label` is the translatable string a GUI combo box shows,
/// so no type role may fix a control's width to an English string's
/// measured size.
#[test]
fn no_type_role_fixes_a_width() {
    let path = common::ui_dir().join("theme.slint");
    let contents = fs::read_to_string(&path).unwrap();
    assert!(
        !contents.contains("width"),
        "{} declares something width-ish; EnumVariant.label \
         must be free to translate to any length",
        path.display()
    );
}

#[test]
fn inter_is_the_font_family_and_ships_beside_the_theme() {
    let probe = probe();
    let theme = Theme::get(&probe);
    assert_eq!(theme.get_font_family().as_str(), "Inter");

    let fonts = common::fonts_dir();
    for name in [
        "Inter-Regular.ttf",
        "Inter-Medium.ttf",
        "Inter-SemiBold.ttf",
        "OFL.txt",
    ] {
        assert!(
            fonts.join(name).is_file(),
            "{} missing under {}",
            name,
            fonts.display()
        );
    }
}

/// Names after `export component` in `code`.
fn exported_components(code: &str) -> Vec<String> {
    code.split("export component ")
        .skip(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        })
        .collect()
}

/// Every exported component under `ui/components/` is imported by the
/// component sheet and instantiated there; a component the sheet does not
/// draw fails here.
#[test]
fn every_component_is_on_the_component_sheet() {
    let ui = common::ui_dir();
    let sheet =
        common::strip_line_comments(&fs::read_to_string(ui.join("theme_sheet.slint")).unwrap());
    let imports: Vec<&str> = sheet
        .split("import ")
        .skip(1)
        .map(|statement| statement.split(';').next().unwrap())
        .collect();

    let mut checked = 0;
    let mut missing = Vec::new();
    for path in common::find_slint_files(&ui.join("components")) {
        let code = common::strip_line_comments(&fs::read_to_string(&path).unwrap());
        let file = path.file_name().unwrap().to_str().unwrap();
        for name in exported_components(&code) {
            checked += 1;
            let imported = imports.iter().any(|statement| {
                statement.contains(&format!("components/{file}"))
                    && statement
                        .split(['{', '}', ','])
                        .any(|imported| imported.trim() == name)
            });
            let drawn =
                sheet.contains(&format!(" {name} {{")) || sheet.contains(&format!("\n{name} {{"));
            if !imported || !drawn {
                missing.push(format!(
                    "{file}: {name} (imported {imported}, drawn {drawn})"
                ));
            }
        }
    }
    assert!(checked >= 40, "the scan found only {checked} components");
    assert!(
        missing.is_empty(),
        "not on the component sheet: {missing:?}"
    );
}
