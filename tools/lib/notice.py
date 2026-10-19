#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Render and verify the dependency NOTICE from locked Cargo release graphs."""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path

TARGETS = (
    ("x86_64-unknown-linux-gnu", "Linux x86_64"),
    ("x86_64-pc-windows-msvc", "Windows x86_64"),
    ("aarch64-apple-darwin", "macOS arm64"),
    ("x86_64-apple-darwin", "macOS x86_64"),
)
TREE_PACKAGE = re.compile(r"^([A-Za-z0-9_-]+) v([^\s]+)(?: \(([^)]+)\))?$")
ABOUT_COPYRIGHT = re.compile(r"Copyright © [^\";]+")
VERSION = re.compile(r"Version vendored: ([0-9.]+)")


def run(command: list[str], root: Path) -> str:
    completed = subprocess.run(command, cwd=root, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if completed.returncode:
        raise ValueError(f"Cargo command failed ({completed.returncode}): {' '.join(command)}\n{completed.stderr.rstrip()}")
    return completed.stdout


def tree_members(root: Path, target: str) -> set[tuple[str, str, str | None]]:
    command = [
        "cargo", "tree", "--locked", "--offline", "-p", "focusforge-cli", "-p", "focusforge-gui",
        "--target", target, "-e", "normal,no-proc-macro", "--prefix", "none", "--format", "{p}|{l}",
    ]
    return parse_tree_members(run(command, root), target)


def parse_tree_members(output: str, target: str) -> set[tuple[str, str, str | None]]:
    members: set[tuple[str, str, str | None]] = set()
    for raw in output.splitlines():
        package = raw.split("|", 1)[0].rstrip()
        if not package:
            continue
        match = TREE_PACKAGE.match(package)
        if not match:
            raise ValueError(f"unrecognized Cargo tree package for {target}: {package}")
        members.add((match.group(1), match.group(2), match.group(3)))
    if not members:
        raise ValueError(f"Cargo tree returned no runtime packages for {target}")
    return members


def metadata_index(root: Path, target: str) -> tuple[dict[tuple[str, str], list[dict[str, object]]], set[str]]:
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", target]
    metadata = json.loads(run(command, root))
    index: dict[tuple[str, str], list[dict[str, object]]] = defaultdict(list)
    for package in metadata["packages"]:
        index[(package["name"], package["version"])].append({
            "id": package["id"],
            "name": package["name"],
            "version": package["version"],
            "source": package["source"],
            "license": package["license"],
        })
    return index, set(metadata["workspace_members"])


def selected_components(
    members: set[tuple[str, str, str | None]],
    index: dict[tuple[str, str], list[dict[str, object]]],
    workspace_members: set[str],
    target: str,
) -> list[dict[str, object]]:
    selected: list[dict[str, object]] = []
    for name, version, source_hint in members:
        candidates = index.get((name, version), [])
        if source_hint is not None:
            candidates = [
                candidate for candidate in candidates
                if candidate["source"] is None
                or str(candidate["source"] or "").removeprefix("git+").startswith(source_hint)
            ]
        if not candidates:
            raise ValueError(f"missing package identity for {name} {version} on {target}")
        if len(candidates) != 1:
            raise ValueError(f"ambiguous package identity for {name} {version} on {target}")
        package = candidates[0]
        if package["source"] is None:
            if package["id"] in workspace_members:
                continue
            raise ValueError(f"runtime path dependency lacks a registry or git source: {name} {version}")
        if not package["license"]:
            raise ValueError(f"{name} {version} has no declared license")
        selected.append(package)
    return selected


def release_components(root: Path) -> list[dict[str, object]]:
    by_identity: dict[tuple[str, str, str], dict[str, object]] = {}
    for target, label in TARGETS:
        index, workspace_members = metadata_index(root, target)
        for package in selected_components(tree_members(root, target), index, workspace_members, target):
            identity = (str(package["name"]), str(package["version"]), str(package["source"]))
            component = by_identity.setdefault(identity, {**package, "targets": set()})
            component["targets"].add(label)  # type: ignore[index]
    if not by_identity:
        raise ValueError("Cargo metadata did not resolve any third-party runtime package")
    return sorted(by_identity.values(), key=lambda item: (str(item["name"]), str(item["version"]), str(item["source"])))


def source_assets(root: Path) -> dict[str, str]:
    about = (root / "crates/gui/ui/shell/settings_modal/about_tab.slint").read_text(encoding="utf-8")
    copyright_match = ABOUT_COPYRIGHT.search(about)
    if not copyright_match:
        raise ValueError("could not read FocusForge copyright from the About tab")
    workspace = (root / "Cargo.toml").read_text(encoding="utf-8")
    license_match = re.search(r'^license\s*=\s*"([^"]+)"$', workspace, re.MULTILINE)
    if not license_match:
        raise ValueError("could not read workspace license")
    fonts_readme = (root / "assets/fonts/README.md").read_text(encoding="utf-8")
    icons_license = (root / "assets/icons/LICENSE").read_text(encoding="utf-8")
    font_version = re.search(r"\[v([0-9.]+)\]", fonts_readme)
    icon_version = VERSION.search(icons_license)
    if not font_version or not icon_version:
        raise ValueError("could not read bundled asset versions")
    return {
        "copyright": copyright_match.group(0),
        "license": license_match.group(1),
        "inter_version": font_version.group(1),
        "inter_text": (root / "assets/fonts/OFL.txt").read_text(encoding="utf-8").rstrip(),
        "lucide_version": icon_version.group(1),
        "lucide_text": icons_license[icons_license.index("ISC License"):].rstrip(),
    }


def validate_components(components: list[dict[str, object]]) -> None:
    seen: set[tuple[str, str, str]] = set()
    for component in components:
        name = component.get("name")
        version = component.get("version")
        source = component.get("source")
        license_name = component.get("license")
        targets = component.get("targets")
        if not all(isinstance(value, str) and value for value in (name, version, source, license_name)):
            raise ValueError(f"invalid component identity: {component!r}")
        if not isinstance(targets, (set, list, tuple)) or not targets or not all(isinstance(value, str) and value for value in targets):
            raise ValueError(f"invalid target membership for {name} {version}")
        identity = (name, version, source)
        if identity in seen:
            raise ValueError(f"duplicate component identity: {name} {version} {source}")
        seen.add(identity)


def render(components: list[dict[str, object]], assets: dict[str, str]) -> str:
    validate_components(components)
    ordered = sorted(components, key=lambda item: (str(item["name"]), str(item["version"]), str(item["source"])))
    lines = [
        "FocusForge NOTICE",
        "",
        assets["copyright"],
        f"Application license: {assets['license']}",
        "",
        "This file lists Cargo runtime dependencies for the default CLI and GUI release builds.",
        "Target labels show where each component is active. Native runtime libraries are documented by release packaging.",
        "",
        "Third-party Rust components",
    ]
    for component in ordered:
        labels = ", ".join(sorted(component["targets"]))
        lines.extend((
            f"- Package: {component['name']} {component['version']}",
            f"  Source: {component['source']}",
            f"  Declared license: {component['license']}",
            f"  Targets: {labels}",
        ))
    lines.extend((
        "",
        f"- Asset: Inter {assets['inter_version']}",
        "  License: OFL-1.1",
        "  License text:",
        assets["inter_text"],
        "",
        f"- Asset: Lucide {assets['lucide_version']}",
        "  License: ISC; includes the Feather MIT attribution below.",
        "  License text:",
        assets["lucide_text"],
        "",
    ))
    return "\n".join(lines)


def component_records(text: str) -> dict[str, str]:
    lines = text.splitlines()
    starts = [index for index, line in enumerate(lines) if line.startswith("- Package: ") or line.startswith("- Asset: ")]
    records: dict[str, str] = {}
    for position, start in enumerate(starts):
        end = starts[position + 1] if position + 1 < len(starts) else len(lines)
        key = lines[start]
        if key.startswith("- Package: "):
            key = f"{key}\n{lines[start + 1]}"
        records[key] = "\n".join(lines[start:end])
    return records


def record_name(key: str) -> str:
    return key.split("\n", 1)[0]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path)
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--input", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if bool(args.root) == bool(args.input):
        parser.error("supply exactly one of --root or --input")
    if args.input:
        data = json.loads(args.input.read_text(encoding="utf-8"))
        notice = render(data["components"], data["assets"])
        if args.output:
            args.output.write_text(notice, encoding="utf-8")
        else:
            sys.stdout.write(notice)
        return 0
    root = args.root.resolve()
    components = release_components(root)
    generated = render(components, source_assets(root))
    notice_path = root / "NOTICE"
    if args.write:
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=root, delete=False) as temporary:
            temporary.write(generated)
            temporary_path = Path(temporary.name)
        os.replace(temporary_path, notice_path)
        print(f"OK: wrote {notice_path.name}")
        return 0
    current = notice_path.read_text(encoding="utf-8") if notice_path.exists() else ""
    if current != generated:
        print("FAIL: NOTICE is stale; run tools/check-notice.sh --write", file=sys.stderr)
        expected = component_records(generated)
        actual = component_records(current)
        for line in sorted(expected.keys() - actual.keys()):
            print(f"  added {record_name(line)[2:]}", file=sys.stderr)
        for line in sorted(actual.keys() - expected.keys()):
            print(f"  removed {record_name(line)[2:]}", file=sys.stderr)
        for line in sorted(expected.keys() & actual.keys()):
            if expected[line] != actual[line]:
                print(f"  changed {record_name(line)[2:]}", file=sys.stderr)
        return 1
    print(f"OK: NOTICE has {len(components)} runtime components")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
