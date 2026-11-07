#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Measures RAW against uncompressed-TIFF stacking on synthetic N-copy stacks.
# Run with --help for usage.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: tools/bench-raw-throughput.sh [--frames 20,60,180] FRAME [-- <extra stack args>]

FRAME (required) is a RAW file: a name under $FOCUSFORGE_RAW_SAMPLES when that
is set and the name is not an existing path, else a path.

The frame is stacked once alone (the "develop probe" row) and that composite
exported as an uncompressed 16-bit TIFF, the twin. Each frame count then copies
the RAW frame and the twin N times into .scratch/bench-raw-throughput/{raw,tiff}-N/
and stacks each directory with `stack --json --no-align --no-crop --algorithm
laplacian` under /usr/bin/time -v. One row per run: format, frames, backend,
develops per frame (min/mean/max, empty for TIFF), non-null stage wall times,
wall seconds, user+system CPU seconds, CPU % and peak RSS.

Arguments after `--` reach every stack run, e.g. `-- --cpu --max-host-memory
822083584` to cap host memory for both formats alike. A RAW run also writes
its developed frames (width x height x 6 bytes each) to scratch for the
length of the run, and the free-space check counts the largest run's share.
A 180-frame sweep of a large frame takes hours: run it once, through
tools/with-lock.sh gpu when the stack uses the GPU.

Exit: 0 table printed or no CLI binary (SKIP); 1 no, missing or non-RAW frame,
too little free disk, no /usr/bin/time, or a failed run.
USAGE
}

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

FRAMES=20,60,180
FRAME=
EXTRA=()
while [ $# -gt 0 ]; do
  case "$1" in
    --frames) FRAMES="${2:?--frames needs a comma-separated list}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    --) shift; EXTRA=("$@"); break ;;
    -*) echo "error: unknown option $1" >&2; usage >&2; exit 1 ;;
    *) FRAME="$1"; shift ;;
  esac
done

if [ -z "$FRAME" ]; then
  echo "error: FRAME is required: a RAW file, or a name under \$FOCUSFORGE_RAW_SAMPLES" >&2
  usage >&2
  exit 1
fi

if [ -z "${CLI:-}" ]; then
  if [ -x target/release/focusforge-cli ]; then
    CLI=target/release/focusforge-cli
  else
    CLI=target/debug/focusforge-cli
  fi
fi
if [ ! -x "$CLI" ]; then
  echo "SKIP: no CLI binary yet ($CLI) -- run cargo build --workspace first" >&2
  exit 0
fi
if [ ! -x /usr/bin/time ]; then
  echo "error: /usr/bin/time is required (GNU time, for -v)" >&2
  exit 1
fi

if [ -n "${FOCUSFORGE_RAW_SAMPLES:-}" ] && [ ! -e "$FRAME" ]; then
  FRAME="$FOCUSFORGE_RAW_SAMPLES/$FRAME"
fi
if [ ! -f "$FRAME" ]; then
  echo "error: frame not found: $FRAME" >&2
  exit 1
fi

OUT=.scratch/bench-raw-throughput
mkdir -p "$OUT"
EXT="${FRAME##*.}"
STACK_ARGS=(--json --no-align --no-crop --algorithm laplacian)

# run <name> <frame-dir>: stack with timing; <name>.json, <name>.time, <name>.err
run() {
  local name="$1" dir="$2"
  rm -rf "$OUT/out/$name"
  mkdir -p "$OUT/out/$name"
  if ! /usr/bin/time -v -o "$OUT/$name.time" \
    "$CLI" stack "$dir" -o "$OUT/out/$name" "${STACK_ARGS[@]}" \
    ${EXTRA[@]+"${EXTRA[@]}"} >"$OUT/$name.json" 2>"$OUT/$name.err"; then
    echo "error: $name failed:" >&2
    tail -n 5 "$OUT/$name.err" >&2
    exit 1
  fi
}

echo "Developing $FRAME once (probe) and building the TIFF twin ..." >&2
rm -rf "$OUT/probe" "$OUT/out/probe" "$OUT/twin.tif"
mkdir -p "$OUT/probe"
cp "$FRAME" "$OUT/probe/00.$EXT"
run probe "$OUT/probe"
if ! python3 - "$OUT/probe.json" <<'PY'
import json
import sys

sys.exit(0 if json.load(open(sys.argv[1]))["develop_counts"] is not None else 1)
PY
then
  echo "error: $FRAME is not a RAW frame (the probe stack reports no develop counts)" >&2
  exit 1
fi
"$CLI" export "$OUT/out/probe"/*.ffp -o "$OUT/twin.tif" >/dev/null 2>"$OUT/twin.err"
if command -v tiffinfo >/dev/null 2>&1 \
  && ! tiffinfo "$OUT/twin.tif" 2>/dev/null | grep -q 'Compression Scheme: None'; then
  echo "error: $OUT/twin.tif is compressed" >&2
  exit 1
fi

# Σ N·(raw bytes + twin bytes) plus one f32 project per run, plus the largest
# run's developed frames (N·width·height·6 bytes, removed when its run ends).
read -r need < <(python3 - "$OUT/probe.json" "$FRAME" "$OUT/twin.tif" "$FRAMES" <<'PY'
import json
import os
import sys

report = json.load(open(sys.argv[1]))
project = report["width"] * report["height"] * 3 * 4
scratch = report["width"] * report["height"] * 3 * 2
per_frame = os.path.getsize(sys.argv[2]) + os.path.getsize(sys.argv[3])
counts = [int(n) for n in sys.argv[4].split(",")]
print(sum(n * per_frame + 2 * project for n in counts) + max(counts) * scratch)
PY
)
avail=$(df -B1 --output=avail "$OUT" | tail -n 1 | tr -d ' ')
if [ "$need" -gt "$avail" ]; then
  echo "error: the stacks need $need bytes but $OUT has $avail bytes free" >&2
  exit 1
fi

IFS=, read -r -a COUNTS <<<"$FRAMES"
for n in "${COUNTS[@]}"; do
  for kind in raw tiff; do
    rm -rf "$OUT/$kind-$n"
    mkdir -p "$OUT/$kind-$n"
    if [ "$kind" = raw ]; then src="$FRAME"; ext="$EXT"; else src="$OUT/twin.tif"; ext=tif; fi
    i=0
    while [ "$i" -lt "$n" ]; do
      cp "$src" "$OUT/$kind-$n/$(printf '%03d' "$i").$ext"
      i=$((i + 1))
    done
  done
done

for n in "${COUNTS[@]}"; do
  for kind in raw tiff; do
    echo "Stacking $kind x $n ..." >&2
    run "$kind-$n" "$OUT/$kind-$n"
    rm -rf "$OUT/out/$kind-$n"
  done
done

python3 - "$OUT" "$FRAMES" <<'PY'
import json
import re
import sys

out = sys.argv[1]
rows = [("develop probe", "raw", 1, "probe")]
for n in (int(x) for x in sys.argv[2].split(",")):
    rows += [(f"{k}", k, n, f"{k}-{n}") for k in ("raw", "tiff")]


def timing(name):
    text = open(f"{out}/{name}.time").read()
    grab = lambda pat: re.search(pat, text).group(1)
    cpu = float(grab(r"User time \(seconds\): ([\d.]+)")) + float(
        grab(r"System time \(seconds\): ([\d.]+)")
    )
    return cpu, grab(r"Percent of CPU this job got: (\d+)%"), grab(
        r"Maximum resident set size \(kbytes\): (\d+)"
    )


head = ("row", "format", "N", "backend", "dev min/mean/max", "setup", "align", "fuse",
        "finalize", "wall s", "cpu s", "cpu %", "rss KiB")
table = [head]
for label, kind, n, name in rows:
    r = json.load(open(f"{out}/{name}.json"))
    counts = r["develop_counts"]
    dev = "" if counts is None else f"{min(counts)}/{sum(counts) / len(counts):.2f}/{max(counts)}"
    st = r["stages"]
    cell = lambda k: "" if st[k] is None else f"{st[k]:.2f}"
    cpu, pct, rss = timing(name)
    table.append((label, kind, str(n), r["backend"], dev, cell("setup"), cell("align"),
                  cell("fuse"), cell("finalize"), f"{r['wall_seconds']:.2f}",
                  f"{cpu:.2f}", pct, rss))
widths = [max(len(row[i]) for row in table) for i in range(len(head))]
for row in table:
    print("  ".join(c.ljust(w) for c, w in zip(row, widths)).rstrip())
PY
