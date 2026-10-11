#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/synth-align.sh
#
# Generates a synthetic focus stack with a KNOWN per-frame misregistration
# (spikes/synthetic-ground-truth/synth_stack.py --warp) and drives
# `focusforge-cli align --json` over it twice, writing
# .scratch/synth-align/measurement.json -- the artefact
# crates/cli/tests/synthetic_ground_truth.rs asserts against.
#
# The generator is Python so its resampler stays independent of the Rust
# warp under test (crate::align::warp); tools/tiff-diff.py plays the same
# role for image outputs.
#
# Exit: 0 pass or skip, 1 fail.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

CLI=target/debug/focusforge-cli
[ -f "$CLI.exe" ] && CLI="$CLI.exe"
OUT=.scratch/synth-align
GENERATOR=spikes/synthetic-ground-truth/synth_stack.py

if command -v python3 >/dev/null 2>&1; then
  PYTHON=python3
elif command -v python >/dev/null 2>&1; then
  PYTHON=python
else
  echo "FAIL: tools/synth-align.sh needs Python 3" >&2
  exit 1
fi

if [ ! -x "$CLI" ]; then
  echo "SKIP: no CLI binary yet ($CLI)"
  exit 0
fi

if ! "$PYTHON" -c 'import numpy' >/dev/null 2>&1; then
  echo "FAIL: tools/synth-align.sh needs numpy -- install it with 'python3 -m pip install numpy' (see CONTRIBUTING.md, Tech stack)" >&2
  exit 1
fi

echo "+ $PYTHON $GENERATOR --warp 2.0 --scene hard --seed 11 --out $OUT --algo pyramidal-ecc"
"$PYTHON" "$GENERATOR" --warp 2.0 --scene hard --seed 11 --out "$OUT" --algo pyramidal-ecc

if [ ! -f "$OUT/measurement.json" ]; then
  echo "FAIL: $OUT/measurement.json was not written" >&2
  exit 1
fi
"$PYTHON" -c "import json, sys; json.load(open(sys.argv[1]))" "$OUT/measurement.json" \
  || { echo "FAIL: $OUT/measurement.json is not valid JSON" >&2; exit 1; }

echo "OK: synthetic alignment truth artefact written to $OUT/measurement.json"
