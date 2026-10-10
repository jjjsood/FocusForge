#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/bench-scaling.sh
#
# Fast-ish demonstration that the adaptive CPU/GPU budgets scale -- a
# sanity check, not a rigorous benchmark; re-run it to see the same shape
# of numbers. Sweeps `focusforge-cli stack --align-algorithm pyramidal-ecc
#` across a full grid: 6 budget points x 3 frame counts x {CPU, GPU}
# = 36 runs, real alignment. pyramidal-ecc's solve dominates wall time
# (~20-30s at 8 frames), so the concurrency signal is a delta on that floor.
#
#   Budget axis: --max-host-memory (CPU) / --max-vram (GPU) swept to 6
#              points that are exact multiples of one tile's accounted host
#              bytes (a probe run's accounted_bytes_in_flight / in_flight_limit)
#              / device_bytes_per_tile (GPU: 53,098,496 B for a 512x512
#              warped tile at any frame count).
#              The byte VALUES stay fixed across the frame axis; neither a
#              host tile's bytes nor a device tile's depend on frame count,
#              so the CPU and GPU in-flight limits must be the same at 8, 16
#              and 32 frames.
#   Frame axis: 8 (the real fixture), 16, 32 frames -- 16/32 built by
#              duplicating the 8 real fixtures/mantis-512 frames cyclically
#              into $OUT/frames-{16,32}/frames/ (bytewise-sorted names,
#              zero-padded; no content/hash uniqueness check, so reused
#              frames are accepted).
#
# Both flags are ceiling-only: a value at or above the host/device's own
# derived budget is a silent no-op (provenance stays Host, not Explicit).
# The summary flags any grid cell where that happened instead of quietly
# plotting a wrong number. If fixtures/mantis-512's geometry ever changes
# (frame count, tile size, algorithm), the 8-frame byte values need
# re-deriving (the CPU unit is probed on every run; the GPU one is not).
#
# Each run's own frozen `--json` report already carries everything this
# needs (wall_seconds, max_bytes_in_flight, budget_provenance,
# max_device_bytes_in_flight, device_budget_provenance, peak_in_flight).
# Takes tens of minutes, not seconds, once real alignment and the 3x
# frame-count grid are both in the loop.
#
# Exit: 0 on success (prints two grids, CPU and GPU), 1 if the CLI binary
# is missing or any run fails.

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

# run <name> <frame-dir> [extra CLI args...]
run() {
  local name="$1" dir="$2"
  shift 2
  mkdir -p "$OUT/$name"
  "$CLI" stack "$dir" --algorithm laplacian --align-algorithm pyramidal-ecc --json \
    -o "$OUT/$name" "$@" >"$OUT/$name.json"
}

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

dir_for() {
  case "$1" in
    8) echo "$FIXTURE" ;;
    16) echo "$OUT/frames-16" ;;
    32) echo "$OUT/frames-32" ;;
  esac
}

echo "Probing one tile's accounted host bytes ..." >&2
run cpu-probe "$FIXTURE" --cpu
cpu_unit=$(python3 - "$OUT/cpu-probe.json" <<'PY'
import json
import sys

r = json.load(open(sys.argv[1]))
print(r["accounted_bytes_in_flight"] // r["in_flight_limit"])
PY
)

echo "Running CPU grid (6 budget points x 3 frame counts) ..." >&2
for target in 1 2 4 8 16 32; do
  bytes=$((cpu_unit * target))
  for frames in 8 16 32; do
    dir="$(dir_for "$frames")"
    run "cpu-${target}-f${frames}" "$dir" --cpu --max-host-memory "${bytes}"
  done
done

echo "Running GPU grid (6 budget points x 3 frame counts) ..." >&2
for target in 1 2 4 8 16 32; do
  bytes=$((53098496 * target))
  for frames in 8 16 32; do
    dir="$(dir_for "$frames")"
    run "gpu-${target}-f${frames}" "$dir" --max-vram "${bytes}"
  done
done

python3 - "$OUT" <<'PY'
import json
import sys

out = sys.argv[1]
targets = [1, 2, 4, 8, 16, 32]
frame_counts = [8, 16, 32]


def load(name):
    with open(f"{out}/{name}.json") as f:
        return json.load(f)


def print_cpu_grid():
    # StackReport.peak_in_flight (crates/core/src/tile/scheduler.rs) is the
    # HOST tile scheduler's own high-water mark -- exactly what
    # --max-host-memory governs, so this grid's peak_in_flight column is a
    # direct, correct read of the swept budget's effect.
    title = "CPU grid (--max-host-memory x frame count, --cpu)"
    print(f"\n{title}")
    header = f"{'target':>8}"
    for f in frame_counts:
        header += f" | f={f:<2} peak_in_flight (wall_s)"
    print(header)

    total = 0.0
    cells = {}
    for target in targets:
        row = f"{target:>8}"
        for frames in frame_counts:
            r = load(f"cpu-{target}-f{frames}")
            cells[(target, frames)] = r
            total += r["wall_seconds"]
            flag = "*" if r["budget_provenance"] != "Explicit" else " "
            row += f" | {r['peak_in_flight']:>2}{flag}          ({r['wall_seconds']:>7.2f})"
        print(row)
    print(f"subtotal: {total:.2f}s")

    for target in targets:
        f8 = cells[(target, 8)]["peak_in_flight"]
        f32 = cells[(target, 32)]["peak_in_flight"]
        if f8 != f32:
            print(
                f"  target={target}: peak_in_flight changes with frame count "
                f"({f8} at 8 frames -> {f32} at 32 frames) -- a host tile's "
                "accounted bytes should not depend on the frame count"
            )
    return total


def print_gpu_grid():
    # NOT the same shape as the CPU grid, and deliberately not printed as
    # one: StackReport.peak_in_flight is ALWAYS the host scheduler's own
    # counter (same field, same source, as the CPU grid above), because the
    # JSON report has no field for the GPU device's own realized in-flight
    # tile count. So --max-vram's effect is only observable here via the
    # budget BYTES themselves (max_device_bytes_in_flight) and their
    # provenance, not via any concurrency count.
    title = "GPU grid (--max-vram x frame count, default GPU path)"
    print(f"\n{title}")
    header = f"{'target':>8}"
    for f in frame_counts:
        header += f" | f={f:<2} max_device_bytes (wall_s)"
    print(header)

    total = 0.0
    for target in targets:
        row = f"{target:>8}"
        for frames in frame_counts:
            r = load(f"gpu-{target}-f{frames}")
            total += r["wall_seconds"]
            flag = "*" if r["device_budget_provenance"] != "Explicit" else " "
            mib = r["max_device_bytes_in_flight"] / (1024 * 1024)
            row += f" | {mib:>7.1f}MiB{flag} ({r['wall_seconds']:>7.2f})"
        print(row)
    print(f"subtotal: {total:.2f}s")
    print(
        "  every cell's device_budget_provenance is Explicit (ceiling always "
        "bound, confirmed per-cell) -- but peak_in_flight is not printed "
        "here: it is the host scheduler's own counter (see the CPU grid "
        "above), not the device's, and stays flat regardless of --max-vram."
    )
    return total


cpu_total = print_cpu_grid()
gpu_total = print_gpu_grid()

print(f"\ntotal wall-clock (both grids, 36 runs): {cpu_total + gpu_total:.2f}s")
print(
    "\nSingle run each, one machine, smallest fixture in the repo -- a "
    "sanity check that the scaling is real and in the right direction, "
    "not a statistically rigorous benchmark or a release performance claim."
)
PY
