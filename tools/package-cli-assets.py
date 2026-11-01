#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Stage and verify the parser-generated CLI resources shipped with packages."""

from __future__ import annotations

import argparse
import gzip
import io
from pathlib import Path


MANUALS = (
    "focusforge-cli.1", "focusforge-cli-info.1", "focusforge-cli-algorithms.1",
    "focusforge-cli-stack.1", "focusforge-cli-export.1", "focusforge-cli-retouch.1",
    "focusforge-cli-history.1", "focusforge-cli-align.1", "focusforge-cli-review.1",
    "focusforge-cli-cache.1", "focusforge-cli-diagnostics.1", "focusforge-cli-history-list.1",
    "focusforge-cli-history-delete.1", "focusforge-cli-history-compact.1", "focusforge-cli-cache-clear.1",
)
COMPLETIONS = {
    "focusforge-cli.bash": "bash-completion/completions/focusforge-cli",
    "_focusforge-cli": "zsh/vendor-completions/_focusforge-cli",
    "focusforge-cli.fish": "fish/vendor_completions.d/focusforge-cli.fish",
    "_focusforge-cli.ps1": "powershell/completions/_focusforge-cli.ps1",
}


def gzip_manual(content: bytes) -> bytes:
    output = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0, compresslevel=9) as stream:
        stream.write(content)
    data = output.getvalue()
    if data[4:8] != b"\0\0\0\0" or data[3] or data[9] != 255:
        raise ValueError("manual gzip header is not deterministic")
    return data


def sources(root: Path) -> dict[Path, bytes]:
    assets = root / "assets"
    manuals = assets / "man"
    completions = assets / "completions"
    expected = set(MANUALS)
    found = {path.name for path in manuals.iterdir() if path.is_file()} if manuals.is_dir() else set()
    if found != expected:
        raise ValueError(f"manual inventory differs: expected {sorted(expected)}, found {sorted(found)}")
    expected = set(COMPLETIONS)
    found = {path.name for path in completions.iterdir() if path.is_file()} if completions.is_dir() else set()
    if found != expected:
        raise ValueError(f"completion inventory differs: expected {sorted(expected)}, found {sorted(found)}")
    output: dict[Path, bytes] = {}
    for name in MANUALS:
        output[Path("share/man/man1") / f"{name}.gz"] = gzip_manual((manuals / name).read_bytes())
    for source, destination in COMPLETIONS.items():
        output[Path("share") / destination] = (completions / source).read_bytes()
    return output


def stage(root: Path, destination: Path) -> None:
    for relative, content in sources(root).items():
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)


def verify(root: Path, destination: Path) -> None:
    expected = sources(root)
    directories = {path.parent for path in expected}

    def owned(path: Path) -> bool:
        return (
            (path.parent == Path("share/man/man1") and path.name.startswith("focusforge-cli"))
            or (path.parent == Path("share/bash-completion/completions") and path.name.startswith("focusforge-cli"))
            or (path.parent == Path("share/fish/vendor_completions.d") and path.name.startswith("focusforge-cli"))
            or (path.parent == Path("share/zsh/vendor-completions") and path.name.startswith("_focusforge-cli"))
            or (path.parent == Path("share/powershell/completions") and path.name.startswith("_focusforge-cli"))
        )

    actual = {
        path.relative_to(destination)
        for directory in directories
        if (destination / directory).is_dir()
        for path in (destination / directory).iterdir()
        if path.is_file() and owned(path.relative_to(destination))
    }
    if actual != set(expected):
        raise ValueError(f"packaged CLI asset inventory differs: expected {sorted(map(str, expected))}, found {sorted(map(str, actual))}")
    for relative, content in expected.items():
        target = destination / relative
        if not target.is_file():
            raise ValueError(f"missing packaged CLI asset: {relative}")
        if target.read_bytes() != content:
            raise ValueError(f"packaged CLI asset differs: {relative}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("stage", "verify"))
    parser.add_argument("root", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    try:
        {"stage": stage, "verify": verify}[args.action](args.root.resolve(), args.destination.resolve())
    except (OSError, ValueError) as error:
        raise SystemExit(f"CLI asset packaging failed: {error}")


if __name__ == "__main__":
    main()
