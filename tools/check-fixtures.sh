#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/check-fixtures.sh
#
# Checks fixtures/MANIFEST.sha256 both ways: every listed file still hashes
# to what it says, and no file present under fixtures/ is unlisted (a
# fixture added by hand, which no generator run would reproduce).
#
# Regenerating needs FOCUSFORGE_REFERENCE_STACK set: tools/make-fixtures.sh
#
# Exit: 0 pass or skip, 1 fail.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

if [ ! -f fixtures/MANIFEST.sha256 ]; then
  echo "SKIP: no fixtures yet"
  exit 0
fi

status=0

if command -v python3 >/dev/null 2>&1; then
  PYTHON=python3
elif command -v python >/dev/null 2>&1; then
  PYTHON=python
else
  echo "FAIL: tools/check-fixtures.sh needs Python 3" >&2
  exit 1
fi

"$PYTHON" - fixtures <<'PY'
import hashlib
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
manifest = root / "MANIFEST.sha256"
entries = []
malformed = False
for line in manifest.read_text(encoding="utf-8").splitlines():
    match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
    if match:
        entries.append((match.group(2), match.group(1)))
    elif line:
        malformed = True

status = int(malformed)
if malformed:
    print("FAIL: malformed MANIFEST.sha256.", file=sys.stderr)
for relative, expected in sorted(entries):
    path = root / relative
    try:
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        print(f"{relative}: FAILED open or read", file=sys.stderr)
        status = 1
    else:
        if actual != expected:
            print(f"{relative}: FAILED", file=sys.stderr)
            status = 1

if status:
    print("FAIL: a fixture does not match MANIFEST.sha256.", file=sys.stderr)
    print("  Regenerate with FOCUSFORGE_REFERENCE_STACK set: tools/make-fixtures.sh", file=sys.stderr)

present = sorted(
    path.relative_to(root).as_posix()
    for path in root.rglob("*")
    if path.is_file() and path.name != "MANIFEST.sha256"
)
unlisted = sorted(set(present) - {relative for relative, _expected in entries})
if unlisted:
    print("FAIL: files under fixtures/ that MANIFEST.sha256 does not list:", file=sys.stderr)
    for path in unlisted:
        print(f"  {path}", file=sys.stderr)
    print("  A fixture is generated, never added by hand. Regenerate with", file=sys.stderr)
    print("  FOCUSFORGE_REFERENCE_STACK set (tools/make-fixtures.sh) so the", file=sys.stderr)
    print("  manifest covers it.", file=sys.stderr)
    status = 1

if not status:
    print(f"OK: {len(entries)} fixture files match the manifest")
sys.exit(status)
PY
