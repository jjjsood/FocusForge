#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Selftest for tools/check-fixtures.sh.
#
# Runs copies of tools/check-fixtures.sh against synthetic fixtures/ trees
# to prove each of its refusals fires for the right reason, and that its two
# passing shapes (an intact tree, and no fixtures at all yet) are not simply
# "everything fails" in disguise.
#
# Usage: tools/check-fixtures-selftest.sh
set -uo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="$SELF/tools/check-fixtures.sh"
if command -v python3 >/dev/null 2>&1; then
  PYTHON=python3
else
  PYTHON=python
fi

PASS=0
FAIL=0

fixture() {  # fixture <root> -- an intact two-frame fixture set plus the copied gate
  local root="$1"
  mkdir -p "$root/tools" "$root/fixtures/mantis-512/frames"
  cp "$GATE" "$root/tools/check-fixtures.sh"
  chmod +x "$root/tools/check-fixtures.sh"
  printf 'AAA' >"$root/fixtures/mantis-512/frames/00.tif"
  printf 'BBB' >"$root/fixtures/mantis-512/frames/01.tif"
  printf '%s\n' '# mantis-512 (selftest fixture)' >"$root/fixtures/mantis-512/README.md"
  "$PYTHON" - "$root/fixtures" <<'PY'
import hashlib
import sys
from pathlib import Path

root = Path(sys.argv[1])
paths = sorted(path for path in root.rglob("*") if path.is_file())
with (root / "MANIFEST.sha256").open("w", encoding="utf-8", newline="\n") as manifest:
    for path in paths:
        manifest.write(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.relative_to(root).as_posix()}\n")
PY
}

run_case() {  # run_case <name> <want-exit> <root> <needle>...
  local name="$1" want_exit="$2" root="$3"
  shift 3
  local out rc needle
  out="$(bash "$root/tools/check-fixtures.sh" 2>&1)"
  rc=$?
  if [ "$rc" -ne "$want_exit" ]; then
    echo "FAIL [$name]: expected exit $want_exit, got $rc"
    printf '%s\n' "$out" | sed 's/^/        /'
    FAIL=$((FAIL + 1))
    return
  fi
  for needle in "$@"; do
    if ! printf '%s' "$out" | grep -qF "$needle"; then
      echo "FAIL [$name]: exit $rc as expected, but output never said \"$needle\""
      printf '%s\n' "$out" | sed 's/^/        /'
      FAIL=$((FAIL + 1))
      return
    fi
  done
  echo "ok   [$name]"
  PASS=$((PASS + 1))
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- 1. an intact tree passes ----------------------------------------------
d="$TMP/c1"; fixture "$d"
run_case "intact tree" 0 "$d" "fixture files match the manifest"

# --- 2. no MANIFEST.sha256 at all: a clean skip, not a failure -------------
d="$TMP/c2"
mkdir -p "$d/tools" "$d/fixtures"
cp "$GATE" "$d/tools/check-fixtures.sh"
chmod +x "$d/tools/check-fixtures.sh"
run_case "no fixtures yet" 0 "$d" "SKIP: no fixtures yet"

# --- 3. one byte of a listed .tif flipped: sha256sum --check catches it ----
d="$TMP/c3"; fixture "$d"
printf '\x00' | dd of="$d/fixtures/mantis-512/frames/00.tif" bs=1 seek=1 count=1 \
  conv=notrunc status=none
run_case "tampered byte" 1 "$d" \
  "mantis-512/frames/00.tif: FAILED" \
  "FAIL: a fixture does not match MANIFEST.sha256."

# --- 4. an extra file the manifest does not list ---------------------------
d="$TMP/c4"; fixture "$d"
printf 'CCC' >"$d/fixtures/mantis-512/frames/02.tif"
run_case "unlisted extra file" 1 "$d" \
  "FAIL: files under fixtures/ that MANIFEST.sha256 does not list:" \
  "mantis-512/frames/02.tif"

# --- 5. a listed file deleted: sha256sum's own open-or-read failure --------
d="$TMP/c5"; fixture "$d"
rm "$d/fixtures/mantis-512/frames/00.tif"
run_case "missing listed file" 1 "$d" "FAILED open or read"

# --- 6. a corrupt manifest must not silently become an empty one ----------
d="$TMP/c6"; fixture "$d"
printf 'not a sha256 manifest\n' >"$d/fixtures/MANIFEST.sha256"
run_case "malformed manifest" 1 "$d" "FAIL: malformed MANIFEST.sha256"

echo
echo "--- $PASS passed, $FAIL failed ---"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
