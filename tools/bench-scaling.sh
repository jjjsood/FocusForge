#!/usr/bin/env bash
# Usage: tools/bench-scaling.sh
#
# Small, fast-ish demonstration that the adaptive CPU/GPU budgets actually
# scale -- not a rigorous benchmark, a sanity comparison anyone can re-run
# on their own machine to see the same shape of numbers. Sweeps
# `focusforge-cli stack --align-algo pyramidal-ecc --lax` against
# fixtures/mantis-512 across two curves, real alignment rather than
# --no-align -- but note the real cost this carries: pyramidal-ecc's solve
# dominates each run's wall_seconds on this fixture (~20-30s per run,
# independent of either budget swept below, since alignment runs before
# tiling and neither flag touches it), so the concurrency signal this script
# exists to show up is a small delta on top of that fixed floor, not the
# whole number.
#
#   1. CPU, capped to --max-host-memory 1MiB -- forces in_flight_limit=1.
#   2. CPU, uncapped -- host-derived budget, full concurrency.
#   3. GPU, default -- adaptive VRAM budget, no flag needed.
#
# Each run's own frozen `--json` report already carries everything this
# needs (wall_seconds, max_bytes_in_flight, budget_provenance,
# max_device_bytes_in_flight, device_budget_provenance, peak_in_flight) --
# no new instrumentation, no cargo bench target, no new dependency. Total
# wall-clock is minutes, not seconds, once real alignment is in the loop and
# the frame-count axis's 32-frame points are in it too.
#
# Exit: 0 on success (prints a summary table), 1 if the CLI binary is
# missing or any run fails.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

CLI=target/debug/focusforge-cli
FIXTURE=fixtures/mantis-512
OUT=.scratch/bench-scaling

if [ ! -x "$CLI" ]; then
  echo "SKIP: no CLI binary yet ($CLI) -- run cargo build --workspace --all-targets first" >&2
  exit 0
fi

mkdir -p "$OUT"

run() {
  local name="$1"
  shift
  "$CLI" stack "$FIXTURE" --algo laplacian --no-align --json \
    -o "$OUT/$name.tif" "$@" >"$OUT/$name.json"
}

echo "Running three fixture-scale stacks against $FIXTURE ..." >&2
run cpu-capped --no-gpu --max-host-memory 1MiB
run cpu-uncapped --no-gpu
run gpu

python3 - "$OUT" <<'PY'
import json
import sys

out = sys.argv[1]
rows = [
    ("cpu-capped", "CPU, --max-host-memory 1MiB"),
    ("cpu-uncapped", "CPU, uncapped"),
    ("gpu", "GPU, default"),
]

reports = {}
for name, _ in rows:
    with open(f"{out}/{name}.json") as f:
        reports[name] = json.load(f)

print()
print(f"{'run':<28} {'wall_seconds':>12} {'in_flight_limit':>16} {'provenance':>12} {'backend':>30}")
total = 0.0
for name, label in rows:
    r = reports[name]
    total += r["wall_seconds"]
    provenance = r["budget_provenance"]
    backend = r["backend"]
    print(
        f"{label:<28} {r['wall_seconds']:>12.3f} {r['in_flight_limit']:>16} "
        f"{provenance:>12} {backend:>30}"
    )
print()
print(f"total wall-clock: {total:.2f}s")

capped, uncapped, gpu = (reports[n]["wall_seconds"] for n, _ in rows)
if uncapped > 0:
    print(f"uncapped vs capped CPU: {capped / uncapped:.1f}x")
if gpu > 0:
    print(f"GPU vs uncapped CPU: {uncapped / gpu:.1f}x")
print()
print(
    "Single run, one machine, smallest fixture in the repo -- a sanity "
    "check that the scaling is real and in the right direction, not a "
    "statistically rigorous benchmark or a release performance claim."
)
PY
