#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/check-headers.sh
#
# Checks SPDX headers on project-authored source files.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

python3 - "$REPO" <<'PY'
from __future__ import annotations

import os
import sys
from pathlib import Path

root = Path(sys.argv[1])
spdx = "SPDX-License-Identifier: GPL-3.0-or-later"

# Excluded roots: assets/ is vendored or binary material; fixtures/, target/ and
# .scratch/ are generated data; Cargo manifests and lockfiles have no portable
# source-comment form. Only the authored source classes below are checked.
patterns = {
    "crates": {".rs", ".slint", ".wgsl"},
    "tools": {".sh", ".py"},
    "spikes": {".rs", ".wgsl", ".py"},
}


def covered(path: Path) -> bool:
    relative = path.relative_to(root)
    if relative.parts[0] == ".githooks":
        return path.is_file()
    return relative.parts[0] in patterns and path.suffix in patterns[relative.parts[0]]


def header_line(path: Path) -> str:
    lines = path.read_text(encoding="utf-8").splitlines()
    index = 0
    if lines and lines[0].startswith("#!"):
        index += 1
    if index < len(lines) and "coding" in lines[index] and lines[index].lstrip().startswith("#"):
        index += 1
    return lines[index] if index < len(lines) else ""


def expected_header(path: Path) -> str:
    prefix = "#" if path.suffix in {".sh", ".py"} or path.parent.name == ".githooks" else "//"
    return f"{prefix} {spdx}"


bad: list[str] = []
checked = 0
for base in (*patterns, ".githooks"):
    directory = root / base
    if not directory.exists():
        continue
    for path in sorted(candidate for candidate in directory.rglob("*") if candidate.is_file() and covered(candidate)):
        checked += 1
        try:
            line = header_line(path)
        except UnicodeDecodeError:
            bad.append(f"{path.relative_to(root).as_posix()}: not UTF-8 source")
            continue
        if line != expected_header(path):
            bad.append(f"{path.relative_to(root).as_posix()}: missing opening SPDX header")

if bad:
    print("FAIL: source files missing SPDX-License-Identifier: GPL-3.0-or-later:", file=sys.stderr)
    print(*[f"  {line}" for line in bad], sep="\n", file=sys.stderr)
    raise SystemExit(1)
print(f"OK: SPDX headers cover {checked} project-authored source files")
PY
