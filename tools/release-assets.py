#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Build, assemble and check the files a FocusForge release publishes.

A release holds the installers, one command-line archive per platform and a
single SHA256SUMS. Symbols and build receipts stay workflow artifacts.
"""
from __future__ import annotations

import argparse
import hashlib
import re
import shutil
import sys
import tarfile
import zipfile
from pathlib import Path

# Release target -> (platform, archive suffix).
CLI_TARGETS = {
    "x86_64-pc-windows-msvc": ("windows", "zip"),
    "aarch64-apple-darwin": ("macos", "tar.xz"),
    "x86_64-unknown-linux-gnu": ("linux", "tar.xz"),
}
PLATFORMS = (
    ("windows", "Windows (x64)"),
    ("macos", "macOS (Apple silicon)"),
    ("linux", "Linux (x64)"),
)
SUMS = "SHA256SUMS"


def installers(version: str) -> dict[str, list[str]]:
    return {
        "windows": [f"FocusForge-{version}-x86_64.msi"],
        "macos": [f"FocusForge-{version}-aarch64.dmg"],
        "linux": [f"FocusForge-{version}-x86_64.AppImage", f"focusforge_{version}_amd64.deb"],
    }


def cli_archives(version: str) -> dict[str, str]:
    return {platform: f"focusforge-cli-{target}.{suffix}" for target, (platform, suffix) in CLI_TARGETS.items()}


def payload(version: str) -> list[str]:
    """The files a release uploads, SHA256SUMS excluded."""
    names = [name for group in installers(version).values() for name in group]
    return sorted(names + list(cli_archives(version).values()))


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def cli_archive(binary: Path, target: str, out: Path, root: Path) -> Path:
    if target not in CLI_TARGETS:
        raise ValueError(f"unknown target {target}")
    suffix = CLI_TARGETS[target][1]
    folder = f"focusforge-cli-{target}"
    exe = "focusforge-cli.exe" if suffix == "zip" else "focusforge-cli"
    members = [(binary, exe, 0o755), (root / "LICENSE", "LICENSE", 0o644), (root / "NOTICE", "NOTICE", 0o644)]
    for source, _, _ in members:
        if not source.is_file():
            raise ValueError(f"missing input: {source}")
    out.mkdir(parents=True, exist_ok=True)
    archive = out / f"{folder}.{suffix}"
    if suffix == "zip":
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as handle:
            for source, name, mode in members:
                info = zipfile.ZipInfo(f"{folder}/{name}", (1980, 1, 1, 0, 0, 0))
                info.external_attr = mode << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                handle.writestr(info, source.read_bytes())
    else:
        with tarfile.open(archive, "w:xz") as handle:
            for source, name, mode in members:
                info = tarfile.TarInfo(f"{folder}/{name}")
                info.size = source.stat().st_size
                info.mode = mode
                with source.open("rb") as stream:
                    handle.addfile(info, stream)
    return archive


def changelog_section(text: str, version: str) -> str:
    """The body under `## [version]`, else under `## [Unreleased]` (a candidate has no section yet)."""
    sections: dict[str, list[str]] = {}
    current = None
    for line in text.splitlines():
        heading = re.match(r"## \[([^\]]+)\]", line)
        if heading:
            current = heading.group(1)
            sections[current] = []
        elif current is not None:
            sections[current].append(line)
    for key in (version, "Unreleased"):
        if key in sections and "".join(sections[key]).strip():
            return "\n".join(sections[key]).strip()
    raise ValueError(f"CHANGELOG has no entries for {version} or Unreleased")


def notes(version: str, changelog: str, repo: str | None) -> str:
    def link(name: str) -> str:
        if repo:
            return f"[{name}](https://github.com/{repo}/releases/download/v{version}/{name})"
        return f"`{name}`"

    rows = []
    for platform, title in PLATFORMS:
        installer = ", ".join(link(name) for name in installers(version)[platform])
        rows.append(f"| **{title}** | {installer} | {link(cli_archives(version)[platform])} |")
    table = "\n".join(["| | Installer | Command line only |", "|---|---|---|", *rows])
    return (
        f"## What's new\n\n{changelog_section(changelog, version)}\n\n"
        f"## Download\n\n{table}\n\n"
        f"`{SUMS}` lists the SHA-256 of every file here: `sha256sum -c {SUMS} --ignore-missing`.\n"
    )


def check(out: Path, version: str) -> list[str]:
    """Problems with an assembled release directory; empty when it is exactly what gets published."""
    errors: list[str] = []
    files = out / "files"
    if not out.is_dir() or not files.is_dir():
        return [f"{out}: expected files/ and notes.md"]
    if sorted(item.name for item in out.iterdir()) != ["files", "notes.md"]:
        errors.append(f"{out}: expected exactly files/ and notes.md")
    expected = sorted([*payload(version), SUMS])
    found = sorted(item.name for item in files.iterdir())
    for name in sorted(set(expected) - set(found)):
        errors.append(f"release would lack {name}")
    for name in sorted(set(found) - set(expected)):
        errors.append(f"unexpected file {name}")
    for name in found:
        path = files / name
        if path.is_symlink() or not path.is_file():
            errors.append(f"non-file: {name}")
        elif path.stat().st_size == 0:
            errors.append(f"empty file: {name}")
    if errors:
        return errors
    sums = (files / SUMS).read_text().splitlines()
    want = [f"{sha256(files / name)}  {name}" for name in payload(version)]
    for line in sorted(set(want) - set(sums)):
        errors.append(f"{SUMS}: no matching line for {line.split('  ', 1)[1]} (checksum differs or line missing)")
    for line in sorted(set(sums) - set(want)):
        errors.append(f"{SUMS}: unexpected line {line}")
    body = (out / "notes.md").read_text() if (out / "notes.md").is_file() else ""
    for name in payload(version):
        if name not in body:
            errors.append(f"notes.md does not name {name}")
    return errors


def assemble(source: Path, out: Path, version: str, changelog: Path, repo: str | None) -> list[str]:
    missing = [name for name in payload(version) if not (source / name).is_file()]
    if missing:
        return [f"release would lack {name}" for name in missing]
    if out.exists():
        shutil.rmtree(out)
    (out / "files").mkdir(parents=True)
    for name in payload(version):
        shutil.copyfile(source / name, out / "files" / name)
    (out / "files" / SUMS).write_text("".join(f"{sha256(out / 'files' / name)}  {name}\n" for name in payload(version)))
    (out / "notes.md").write_text(notes(version, changelog.read_text(), repo))
    return check(out, version)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    archive = commands.add_parser("cli-archive", help="pack a built focusforge-cli with LICENSE and NOTICE")
    archive.add_argument("--binary", type=Path, required=True)
    archive.add_argument("--target", required=True, choices=sorted(CLI_TARGETS))
    archive.add_argument("--out", type=Path, required=True)
    archive.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    build = commands.add_parser("assemble", help="turn a directory of release files into the published set")
    build.add_argument("source", type=Path)
    build.add_argument("--out", type=Path, required=True)
    build.add_argument("--version", required=True)
    build.add_argument("--changelog", type=Path, default=Path("CHANGELOG.md"))
    build.add_argument("--repo", help="OWNER/NAME, turns the names in the notes into download links")
    verify = commands.add_parser("check", help="fail unless a directory is exactly what a release publishes")
    verify.add_argument("out", type=Path)
    verify.add_argument("--version", required=True)
    args = parser.parse_args()
    try:
        if args.command == "cli-archive":
            print(cli_archive(args.binary, args.target, args.out, args.root))
            return 0
        problems = (
            assemble(args.source, args.out, args.version, args.changelog, args.repo)
            if args.command == "assemble"
            else check(args.out, args.version)
        )
    except (OSError, ValueError) as error:
        problems = [str(error)]
    for problem in problems:
        print(f"FAIL  {problem}", file=sys.stderr)
    if not problems:
        print(f"OK    release {args.version}: {len(payload(args.version)) + 1} files")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
