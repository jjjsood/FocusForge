#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Selftest for tools/release-assets.py: each refusal gets a red case beside the green one."""
from __future__ import annotations

import importlib.util
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("release_assets", ROOT / "tools/release-assets.py")
assets = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(assets)

VERSION = "1.2.3-rc.4"
CHANGELOG = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Stacking.\n\n## [1.0.0] - 2026-01-01\n\n- Old.\n"


def refused(problems: list[str], needle: str) -> None:
    assert any(needle in problem for problem in problems), (needle, problems)


def source(directory: Path) -> Path:
    inputs = directory / "in"
    inputs.mkdir()
    for name in assets.payload(VERSION):
        (inputs / name).write_bytes(name.encode())
    return inputs


def archives(directory: Path) -> None:
    (directory / "LICENSE").write_text("license")
    (directory / "NOTICE").write_text("notice")
    binary = directory / "focusforge-cli"
    binary.write_bytes(b"binary")
    tarball = assets.cli_archive(binary, "x86_64-unknown-linux-gnu", directory / "out", directory)
    with tarfile.open(tarball) as handle:
        members = {member.name: member for member in handle.getmembers()}
    folder = "focusforge-cli-x86_64-unknown-linux-gnu"
    assert set(members) == {f"{folder}/focusforge-cli", f"{folder}/LICENSE", f"{folder}/NOTICE"}
    assert members[f"{folder}/focusforge-cli"].mode == 0o755 and tarball.name == f"{folder}.tar.xz"
    archive = assets.cli_archive(binary, "x86_64-pc-windows-msvc", directory / "out", directory)
    with zipfile.ZipFile(archive) as handle:
        assert sorted(handle.namelist()) == sorted(f"focusforge-cli-x86_64-pc-windows-msvc/{name}" for name in ("focusforge-cli.exe", "LICENSE", "NOTICE"))
    for target, path in (("riscv64-unknown-linux-gnu", binary), ("x86_64-unknown-linux-gnu", directory / "missing")):
        try:
            assets.cli_archive(path, target, directory / "out", directory)
        except ValueError:
            pass
        else:
            raise AssertionError(f"accepted {target} {path.name}")


def changelog() -> None:
    assert assets.changelog_section(CHANGELOG, "1.0.0") == "- Old."
    assert assets.changelog_section(CHANGELOG, VERSION) == "### Added\n\n- Stacking."
    try:
        assets.changelog_section("# Changelog\n\n## [Unreleased]\n", VERSION)
    except ValueError:
        pass
    else:
        raise AssertionError("empty changelog accepted")


def release(directory: Path) -> None:
    changes = directory / "CHANGELOG.md"
    changes.write_text(CHANGELOG)
    out = directory / "release"
    assert assets.assemble(source(directory), out, VERSION, changes, "o/r") == []
    assert assets.check(out, VERSION) == []
    notes = (out / "notes.md").read_text()
    # Installers come before the command-line archives, and the names link to this tag.
    assert notes.index(f"FocusForge-{VERSION}-x86_64.msi") < notes.index("focusforge-cli-x86_64-pc-windows-msvc.zip")
    assert f"https://github.com/o/r/releases/download/v{VERSION}/FocusForge-{VERSION}-aarch64.dmg" in notes and "- Stacking." in notes
    files = out / "files"
    assert sorted(item.name for item in files.iterdir()) == sorted([*assets.payload(VERSION), "SHA256SUMS"])

    for extra in ("extra.json", "symbols.tar.gz"):
        (files / extra).write_text("x")
        refused(assets.check(out, VERSION), f"unexpected file {extra}")
        (files / extra).unlink()

    name = f"FocusForge-{VERSION}-x86_64.msi"
    kept = (files / name).read_bytes()
    (files / name).unlink()
    refused(assets.check(out, VERSION), f"release would lack {name}")
    (files / name).mkdir()
    refused(assets.check(out, VERSION), f"non-file: {name}")
    (files / name).rmdir()
    (files / name).write_bytes(b"")
    refused(assets.check(out, VERSION), f"empty file: {name}")
    (files / name).write_bytes(kept + b"tampered")
    refused(assets.check(out, VERSION), "SHA256SUMS: no matching line")
    (files / name).write_bytes(kept)
    assert assets.check(out, VERSION) == []

    link = files / "focusforge-cli-aarch64-apple-darwin.tar.xz"
    original = link.read_bytes()
    link.unlink()
    link.symlink_to(directory / "in" / link.name)
    refused(assets.check(out, VERSION), "non-file")
    link.unlink()
    link.write_bytes(original)

    (out / "notes.md").write_text("no names here")
    refused(assets.check(out, VERSION), "notes.md does not name")
    (out / "notes.md").write_text(notes)
    (out / "stray").write_text("x")
    refused(assets.check(out, VERSION), "expected exactly files/ and notes.md")
    (out / "stray").unlink()
    refused(assets.check(directory / "nowhere", VERSION), "expected files/ and notes.md")

    (directory / "partial").mkdir()
    refused(assets.assemble(directory / "partial", directory / "partial-out", VERSION, changes, None), "release would lack")


def main() -> None:
    changelog()
    with tempfile.TemporaryDirectory() as temp:
        archives(Path(temp))
    with tempfile.TemporaryDirectory() as temp:
        release(Path(temp))
    print("release assets selftest: PASS")


if __name__ == "__main__":
    main()
