#!/usr/bin/env bash
# Usage: tools/bench-scaling.sh
#
# Small, fast-ish demonstration that the adaptive CPU/GPU budgets actually
# scale -- not a rigorous benchmark, a sanity comparison anyone can re-run
# on their own machine to see the same shape of numbers. Sweeps
# `focusforge-cli stack --align-algo pyramidal-ecc --lax` across a full
# grid: 6 budget points x 3 frame counts x {CPU, GPU} = 36 runs. Real
# alignment rather than --no-align -- but note the real cost this carries:
# pyramidal-ecc's solve dominates each run's wall_seconds (~20-30s at 8
# frames, scaling up with frame count, independent of the budget swept), so
# the concurrency signal this script exists to show up is a delta on top of
# that floor, not the whole number.
#
#   CPU curve: --max-host-memory swept to land in_flight_limit at exactly
#              1, 2, 4, 8, 16, 32 -- the byte values below are exact
#              multiples of bytes_per_tile_set for THIS fixture's geometry
#              (crates/core/src/tile/scheduler.rs's bytes_per_tile_set: 8
#              frames * 512 * 512 * 3 samples * 2 bytes = 12,582,912 B).
#   GPU curve: --max-vram swept the same way against device_bytes_per_tile
#              for this fixture (crates/gpu/src/budget.rs, measured
#              254,661,632 B) -- bare byte values, since that size isn't a
#              clean MiB multiple. The report has no device-side tile-count
#              field to print (only the byte budget itself,
#              max_device_bytes_in_flight); in_flight_limit in the JSON is
#              the CPU rayon pool's own size and does not move with this
#              sweep at all -- do not confuse the two.
#
# Both flags are ceiling-only: a value at or above the host/device's own
# derived budget is a silent no-op (provenance stays Host, not Explicit).
# The summary below flags any swept point where that happened instead of
# quietly plotting a wrong curve. If fixtures/mantis-512's geometry ever
# changes (frame count, tile size, algorithm), these byte values need
# re-deriving from the formulas above, not guessing.
#
# Each run's own frozen `--json` report already carries everything this
# needs (wall_seconds, max_bytes_in_flight, budget_provenance,
# max_device_bytes_in_flight, device_budget_provenance, peak_in_flight) --
# no new instrumentation, no cargo bench target, no new dependency. Total
# wall-clock is tens of minutes, not seconds, once real alignment and the
# 3x frame-count grid are both in the loop.
#
# Exit: 0 on success (prints three curve tables), 1 if the CLI binary is
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
  "$CLI" stack "$FIXTURE" --algo laplacian --align-algo pyramidal-ecc --lax --json \
    -o "$OUT/$name.tif" "$@" >"$OUT/$name.json"
}

# Same as run(), but against an explicit source directory instead of $FIXTURE
# -- used for the frame-count axis's synthetic 16/32-frame directories.
run_dir() {
  local name="$1" dir="$2"
  shift 2
  "$CLI" stack "$dir" --algo laplacian --align-algo pyramidal-ecc --lax --json \
    -o "$OUT/$name.tif" "$@" >"$OUT/$name.json"
}

echo "Running CPU budget sweep (6 points) against $FIXTURE ..." >&2
run cpu-1 --no-gpu --max-host-memory 12MiB
run cpu-2 --no-gpu --max-host-memory 24MiB
run cpu-4 --no-gpu --max-host-memory 48MiB
run cpu-8 --no-gpu --max-host-memory 96MiB
run cpu-16 --no-gpu --max-host-memory 192MiB
run cpu-32 --no-gpu --max-host-memory 384MiB

echo "Running GPU budget sweep (6 points) against $FIXTURE ..." >&2
run gpu-1 --max-vram 254661632
run gpu-2 --max-vram 509323264
run gpu-4 --max-vram 1018646528
run gpu-8 --max-vram 2037293056
run gpu-16 --max-vram 4074586112
run gpu-32 --max-vram 8149172224

echo "Building synthetic 16/32-frame directories (fixtures/mantis-512's 8 real frames, duplicated) ..." >&2
FRAMES_SRC="$FIXTURE/frames"
for n in 16 32; do
  dest="$OUT/frames-$n/frames"
  mkdir -p "$dest"
  i=0
  while [ "$i" -lt "$n" ]; do
    src_name=$(printf '%02d.tif' $((i % 8)))
    dst_name=$(printf '%02d.tif' "$i")
    cp "$FRAMES_SRC/$src_name" "$dest/$dst_name"
    i=$((i + 1))
  done
done

echo "Running frame-count sweep (8/16/32 frames, default adaptive budgets, CPU+GPU) ..." >&2
run       frames-8-cpu  --no-gpu
run       frames-8-gpu
run_dir   frames-16-cpu "$OUT/frames-16" --no-gpu
run_dir   frames-16-gpu "$OUT/frames-16"
run_dir   frames-32-cpu "$OUT/frames-32" --no-gpu
run_dir   frames-32-gpu "$OUT/frames-32"

python3 - "$OUT" <<'PY'
import json
import sys

out = sys.argv[1]

cpu_targets = [1, 2, 4, 8, 16, 32]
gpu_targets = [1, 2, 4, 8, 16, 32]

def load(name):
    with open(f"{out}/{name}.json") as f:
        return json.load(f)

def print_curve(title, names, targets, budget_field, provenance_field):
    print(f"\n{title}")
    print(f"{'target':>8} {'wall_seconds':>12} {budget_field:>24} {provenance_field:>16}")
    reports = [load(n) for n in names]
    for target, r in zip(targets, reports):
        budget = r[budget_field]
        provenance = r[provenance_field]
        flag = "  <- ceiling did not bind" if provenance != "Explicit" else ""
        print(f"{target:>8} {r['wall_seconds']:>12.3f} {budget:>24} {provenance:>16}{flag}")
    total = sum(r["wall_seconds"] for r in reports)
    print(f"subtotal: {total:.2f}s")
    first, last = reports[0]["wall_seconds"], reports[-1]["wall_seconds"]
    if last > 0:
        print(f"target={targets[0]} vs target={targets[-1]}: {first / last:.1f}x")
    return total

cpu_names = [f"cpu-{n}" for n in cpu_targets]
gpu_names = [f"gpu-{n}" for n in gpu_targets]

# "target" here is the requested in-flight tile count; the report field that
# actually moves with the sweep is the byte budget itself
# (max_bytes_in_flight / max_device_bytes_in_flight) -- in_flight_limit is
# the CPU rayon pool's own size (decoupled from the byte budget, always
# available_parallelism), never the GPU device's tile count, so it is not a
# meaningful column for the GPU curve.
cpu_total = print_curve(
    "CPU curve (--max-host-memory swept, --no-gpu)",
    cpu_names, cpu_targets, "max_bytes_in_flight", "budget_provenance",
)
gpu_total = print_curve(
    "GPU curve (--max-vram swept, default GPU path)",
    gpu_names, gpu_targets, "max_device_bytes_in_flight", "device_budget_provenance",
)

def print_frame_curve():
    print("\nFrame-count curve (8/16/32 frames, default adaptive budgets)")
    print(
        f"{'frames':>8} {'path':>4} {'wall_seconds':>12} "
        f"{'peak_in_flight':>14} {'budget_bytes':>14} {'backed_off':>10}"
    )
    total = 0.0
    rows = []
    for n in (8, 16, 32):
        for path, field, prov_field in (
            ("cpu", "max_bytes_in_flight", "budget_provenance"),
            ("gpu", "max_device_bytes_in_flight", "device_budget_provenance"),
        ):
            r = load(f"frames-{n}-{path}")
            total += r["wall_seconds"]
            rows.append((n, path, r))
            print(
                f"{n:>8} {path:>4} {r['wall_seconds']:>12.3f} "
                f"{r['peak_in_flight']:>14} {r[field]:>14} "
                f"{str(r['backed_off']):>10}"
            )
    print(f"subtotal: {total:.2f}s")
    for path in ("cpu", "gpu"):
        first = next(r for n, p, r in rows if p == path and n == 8)
        last = next(r for n, p, r in rows if p == path and n == 32)
        print(
            f"{path} peak_in_flight, 8 frames vs 32 frames: "
            f"{first['peak_in_flight']} -> {last['peak_in_flight']} "
            "(same fixed budget, more bytes per tile-set as frame_count grows "
            "-- concurrency should shrink gracefully here, not collapse)"
        )
    return total

frame_total = print_frame_curve()

print(f"\ntotal wall-clock (all three curves): {cpu_total + gpu_total + frame_total:.2f}s")
print(
    "\nSingle run each, one machine, smallest fixture in the repo -- a "
    "sanity check that the scaling is real and in the right direction, "
    "not a statistically rigorous benchmark or a release performance claim."
)
PY
