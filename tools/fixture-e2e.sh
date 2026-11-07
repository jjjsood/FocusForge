#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/fixture-e2e.sh
#
# Drive the built CLI over the checked-in fixtures: the binary someone actually
# runs starts, parses an `align` command, reads a real TIFF off disk and
# reports every frame. The CLI integration tests pin the report's contents.
#
# It runs the ALREADY-BUILT binary rather than invoking cargo, which is what
# lets check-all.sh run it in the background beside the cargo gates -- cargo
# holds an exclusive lock on target/, so a background cargo command would just
# queue behind the foreground one. Keep this cargo-free.
#
# Everything it touches stays under fixtures/ and .scratch/ -- this gate
# must never read the reference stack; that is tools/run-acceptance.sh.
#
# Exit: 0 pass or skip, 1 fail.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

# Keeps this gate's own runs out of the developer's real log file.
export FOCUSFORGE_DATA_DIR="$REPO/.scratch/fixture-e2e-data"

CLI=target/debug/focusforge-cli
[ -f "$CLI.exe" ] && CLI="$CLI.exe"

if [ ! -x "$CLI" ]; then
  echo "SKIP: no CLI binary yet ($CLI)"
  exit 0
fi

if [ ! -d fixtures/mantis-512 ]; then
  echo "SKIP: no fixtures/mantis-512 yet"
  exit 0
fi

# `--algorithm pyramidal-ecc` is explicit because the all-targets debug build
# also registers a test algorithm, which makes the bare default ambiguous.
# `reference-policy=anchor` keeps the run near 5 s in a debug build; the default
# policy takes about 20 s on the same fixture. Never a `stack` invocation here:
# that is minutes of debug-profile fusion this cheap gate must not pay for.
echo "+ $CLI align fixtures/mantis-512 --algorithm pyramidal-ecc --set reference-policy=anchor --json"
"$CLI" align fixtures/mantis-512 --algorithm pyramidal-ecc --set reference-policy=anchor --json | python3 -c '
import json, sys
report = json.load(sys.stdin)
frames, entries = report.get("frame_count"), len(report.get("alignments", []))
if frames != 8:
    sys.exit(f"FAIL: fixture e2e -- align report: frame_count {frames}, want 8")
if entries != 8:
    sys.exit(f"FAIL: fixture e2e -- align report: {entries} alignments, want 8")
'
echo "OK: align report parsed -- frame_count 8, 8 alignments"

# Removed processing flags must not appear in README.md or docs/.
echo "+ grepping the docs tree for removed processing flags"
REMOVED_HITS="$(grep -rlnE -- '--(algo|align-algo|no-gpu|lax)\b' README.md docs/ 2>/dev/null || true)"
if [ -n "$REMOVED_HITS" ]; then
  echo "FAIL: removed processing flags found in: $REMOVED_HITS" >&2
  exit 1
fi
echo "OK: removed processing flags absent from README.md and docs/"

echo "OK: fixture e2e"
