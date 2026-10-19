// SPDX-License-Identifier: GPL-3.0-or-later

//! The icon rule, read back off the vendored SVG bytes. Upstream ships
//! exactly the rule this crate wants, so "obeys the rule" and "is
//! unmodified Lucide" are the same assertion -- these tests read attribute
//! text, never render a pixel or link an SVG crate.

mod common;

use std::fs;
use std::path::Path;

/// Every top-level tag in `contents`, in document order: `<svg ...>`,
/// `<path .../>`, `<rect .../>`, and so on. Closing tags (`</svg>`) and
/// comments are skipped by the caller, not here -- callers that need only
/// opening/self-closing tags filter by `starts_with('<') && !starts_with("</")`.
fn tags(contents: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel_start) = contents[i..].find('<') {
        let start = i + rel_start;
        let Some(rel_end) = contents[start..].find('>') else {
            break;
        };
        let end = start + rel_end;
        out.push(&contents[start..=end]);
        i = end + 1;
    }
    out
}

fn is_element_tag(tag: &str) -> bool {
    !tag.starts_with("</") && !tag.starts_with("<!--") && !tag.starts_with("<?")
}

/// The value of attribute `name` in `tag`, if present as `name="value"`.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let idx = tag.find(&needle)? + needle.len();
    let rest = &tag[idx..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn element_name(tag: &str) -> &str {
    tag.trim_start_matches('<')
        .trim_end_matches("/>")
        .trim_end_matches('>')
        .split_whitespace()
        .next()
        .unwrap_or("")
}

/// Numeric-attribute names a Lucide primitive element (other than `<path>`,
/// whose `d` mini-language these tests deliberately do not parse -- no SVG
/// crate for six attributes) can carry.
const NUMERIC_ATTRS: &[&str] = &["cx", "cy", "r", "x", "y", "x1", "y1", "x2", "y2"];

fn check_icon(path: &Path) -> Result<(), String> {
    let contents = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let all_tags: Vec<&str> = tags(&contents)
        .into_iter()
        .filter(|t| is_element_tag(t))
        .collect();

    let svg_tag = all_tags
        .iter()
        .find(|t| element_name(t) == "svg")
        .ok_or_else(|| format!("{}: no <svg> root element", path.display()))?;

    // A solid-fill variant of a glyph carries `fill="currentColor"`.
    let fill = if path.file_name().and_then(|n| n.to_str()) == Some("bell-filled.svg") {
        "currentColor"
    } else {
        "none"
    };
    for (name, expected) in [
        ("viewBox", "0 0 24 24"),
        ("fill", fill),
        ("stroke", "currentColor"),
        ("stroke-width", "2"),
        ("stroke-linecap", "round"),
        ("stroke-linejoin", "round"),
    ] {
        match attr(svg_tag, name) {
            Some(v) if v == expected => {}
            Some(v) => {
                return Err(format!(
                    "{}: <svg {name}=\"{v}\">, expected \"{expected}\"",
                    path.display()
                ));
            }
            None => return Err(format!("{}: <svg> missing {name}", path.display())),
        }
    }

    if contents.contains("<style") {
        return Err(format!("{}: carries a <style> element", path.display()));
    }

    for tag in &all_tags {
        if element_name(tag) == "svg" {
            continue;
        }
        if attr(tag, "fill").is_some() || attr(tag, "stroke").is_some() {
            return Err(format!(
                "{}: child element overrides fill/stroke: {tag}",
                path.display()
            ));
        }
        if element_name(tag) == "rect" {
            for corner in ["rx", "ry"] {
                if let Some(v) = attr(tag, corner) {
                    let parsed: f64 = v.parse().map_err(|_| {
                        format!("{}: <rect {corner}=\"{v}\"> not numeric", path.display())
                    })?;
                    if parsed > 2.0 {
                        return Err(format!(
                            "{}: <rect {corner}=\"{v}\"> exceeds the 2-unit corner",
                            path.display()
                        ));
                    }
                }
            }
        }
        for name in NUMERIC_ATTRS {
            if let Some(v) = attr(tag, name) {
                let parsed: f64 = v
                    .parse()
                    .map_err(|_| format!("{}: {name}=\"{v}\" not numeric", path.display()))?;
                if !(0.0..=24.0).contains(&parsed) {
                    return Err(format!(
                        "{}: {name}=\"{v}\" falls outside the 24 grid",
                        path.display()
                    ));
                }
            }
        }
    }

    Ok(())
}

#[test]
fn every_icon_obeys_the_rule() {
    let dir = common::icons_dir();
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("svg"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no SVGs found under {}", dir.display());

    let mut failures = Vec::new();
    for path in entries {
        if let Err(msg) = check_icon(&path) {
            failures.push(msg);
        }
    }
    assert!(
        failures.is_empty(),
        "icon rule violations:\n{}",
        failures.join("\n")
    );
}

#[test]
fn licence_accompanies_the_vendored_icons() {
    let path = common::icons_dir().join("LICENSE");
    let contents = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        contents.contains("ISC"),
        "{}: does not mention ISC",
        path.display()
    );
    assert!(
        contents.contains("Version vendored"),
        "{}: does not name an upstream version",
        path.display()
    );
    assert!(
        contents.contains("Glyph count shipped"),
        "{}: does not record the glyph count shipped",
        path.display()
    );
}
