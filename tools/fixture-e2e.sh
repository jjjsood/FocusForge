#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/fixture-e2e.sh
#
# Drive the built CLI over the checked-in fixtures. This is the gate that
# catches what `cargo test` structurally cannot: that the binary someone
# actually runs starts, parses its arguments, and reads a real TIFF off disk.
# A workspace can be green on every unit test and still ship a CLI that panics
# on startup.
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

echo "+ $CLI info fixtures/mantis-512"
"$CLI" info fixtures/mantis-512

echo "+ $CLI info --json fixtures/mantis-512"
JSON_OUT="$("$CLI" info --json fixtures/mantis-512)"

printf '%s' "$JSON_OUT" | grep -q '"frame_count": 8' \
  || { echo "FAIL: fixture e2e -- missing fact: frame_count 8" >&2; exit 1; }
echo "OK: fact present -- frame_count: 8"

printf '%s' "$JSON_OUT" | grep -q '"uniform_layout": true' \
  || { echo "FAIL: fixture e2e -- missing fact: uniform_layout true" >&2; exit 1; }
echo "OK: fact present -- uniform_layout: true"

printf '%s' "$JSON_OUT" | grep -q '"total_bytes": 12617712' \
  || { echo "FAIL: fixture e2e -- missing fact: total_bytes 12617712" >&2; exit 1; }
echo "OK: fact present -- total_bytes: 12617712"

printf '%s' "$JSON_OUT" | grep -q '"rows_per_strip": 1' \
  || { echo "FAIL: fixture e2e -- missing fact: rows_per_strip 1" >&2; exit 1; }
echo "OK: fact present -- rows_per_strip: 1"

echo "+ $CLI info fixtures/mantis-512/frames/00.tif"
SINGLE_OUT="$("$CLI" info fixtures/mantis-512/frames/00.tif)"
printf '%s' "$SINGLE_OUT" | grep -q 'mmap fast path: yes' \
  || { echo "FAIL: fixture e2e -- missing fact: mmap fast path: yes" >&2; exit 1; }
echo "OK: fact present -- mmap fast path: yes"

# `align`, cheap: `--algorithm pyramidal-ecc` is explicit because the all-targets
# debug build also registers a test algorithm, which makes the bare default
# ambiguous. `reference-policy=anchor` is explicit because the pinned counts
# below are those of registering every frame against the middle anchor. The
# default fallback records divergent frames. Never a `stack` invocation here:
# that is minutes of debug-profile fusion this cheap gate must not pay for.
echo "+ $CLI align fixtures/mantis-512 --algorithm pyramidal-ecc --set reference-policy=anchor --json"
ALIGN_OUT="$("$CLI" align fixtures/mantis-512 --algorithm pyramidal-ecc --set reference-policy=anchor --json)"

printf '%s' "$ALIGN_OUT" | grep -q '"frame_count": 8' \
  || { echo "FAIL: fixture e2e -- align missing fact: frame_count 8" >&2; exit 1; }
echo "OK: fact present -- align frame_count: 8"

printf '%s' "$ALIGN_OUT" | grep -q '"reference": 3' \
  || { echo "FAIL: fixture e2e -- align missing fact: reference 3" >&2; exit 1; }
echo "OK: fact present -- align reference: 3"

# Frames 0, 1 and 7 diverge against the middle anchor; frame 6 (scale 1.1075)
# is flagged validation-failed by the off-centre check.
printf '%s' "$ALIGN_OUT" | grep -q '"failure_count": 4' \
  || { echo "FAIL: fixture e2e -- align missing fact: failure_count 4" >&2; exit 1; }
echo "OK: fact present -- align failure_count: 4 (per-frame divergence, recorded)"

# Removed processing flags must not appear in README.md or docs/.
echo "+ grepping the docs tree for removed processing flags"
REMOVED_HITS="$(grep -rlnE -- '--(algo|align-algo|no-gpu|lax)\b' README.md docs/ 2>/dev/null || true)"
if [ -n "$REMOVED_HITS" ]; then
  echo "FAIL: removed processing flags found in: $REMOVED_HITS" >&2
  exit 1
fi
echo "OK: removed processing flags absent from README.md and docs/"

echo "OK: fixture e2e"
