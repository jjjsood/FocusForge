#!/usr/bin/env bash
# Usage: tools/run-acceptance.sh              -- the unlimited run
#        tools/run-acceptance.sh --limit 8G   -- the same run under an
#                                                 enforced cgroup v2 memory
#                                                 ceiling with swap disabled
#        tools/run-acceptance.sh --inside-scope 8G -- internal re-entry only,
#                                                 never invoked by hand
#
# The acceptance run: the whole 61-frame, 16 GB reference stack through the
# real CLI, end to end, at full size. This is not a gate and never runs in
# check-all.sh -- it takes minutes, and a gate that takes minutes stops being
# run. Two things happen here:
#
#   1. `focusforge-cli stack` over a 61-entry symlink farm built from the
#      reference stack (never read directly -- see the glob trap below),
#      producing a composite and a Z-Map under .scratch/acceptance/unlimited/.
#   2. The identical run again under `systemd-run --user --scope
#      -p MemoryMax=8G -p MemorySwapMax=0`, producing the same two files
#      under .scratch/acceptance/limited/. Completing without an OOM kill is
#      the assertion; memory.peak, memory.swap.peak and the anon/file split
#      are recorded, never thresholded.
#
# Uses the release binary: the dev profile is far too slow at this scale.
#
# Why not `ulimit -v` for the memory ceiling: `crates/core/src/io/tiff/stack.rs`
# holds one mmap per frame for the stack's lifetime, so after the first tile
# row all 61 mappings are live -- 16 GB of address space with an arbitrarily
# small resident set. An address-space limit would fail for a reason
# unrelated to the property under test. `prlimit --rss` is a dead end too:
# RLIMIT_RSS has been a kernel no-op since Linux 2.4.
#
# Outputs go under .scratch/; source frames are never modified.
#
# Environment overrides, for driving this exact script end to end against a
# small fixture instead of the reference stack (so the whole code path --
# farm build, headers-only check, release run, cgroup dance -- is exercised cheaply
# and repeatably). None of these change the default, real-run behaviour:
#   FOCUSFORGE_REFERENCE_STACK source frame directory; a relative value
#                              resolves against the repo root; unset SKIPs
#                              the whole run before the release-CLI check
#   ACCEPTANCE_FRAME_GLOB     glob for source frames within it (default:
#                              *IMG_*.tif)
#   ACCEPTANCE_EXPECTED_FRAMES expected farm size (default: 61)
#   ACCEPTANCE_OUT_DIR        base output directory (default:
#                              .scratch/acceptance)
#   ACCEPTANCE_CLI            CLI binary to run (default:
#                              target/release/focusforge-cli)
#
# Exit: 0 on a completed run, or a clean SKIP when
#       FOCUSFORGE_REFERENCE_STACK is unset.
#       1 on any refusal (bad ground, not a run failure in the usual sense).
#       2 on a usage error.
#       Whatever the CLI itself returned, on a run failure -- that captured
#       status, not systemd-run's, is the run's verdict.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

MODE="${1:-}"
LIMIT="${2:-}"

case "$MODE" in
  "") ;;
  --limit|--inside-scope)
    if [ -z "$LIMIT" ]; then
      echo "usage: tools/run-acceptance.sh [--limit SIZE]" >&2
      exit 2
    fi
    ;;
  *)
    echo "usage: tools/run-acceptance.sh [--limit SIZE]" >&2
    exit 2
    ;;
esac

echo "== tools/run-acceptance.sh ($([ -n "$MODE" ] && echo "${MODE} ${LIMIT}" || echo "unlimited")) =="
echo "This run reads the reference stack in full, takes minutes, and writes"
echo "composites under .scratch/."
echo

OUT_DIR="${ACCEPTANCE_OUT_DIR:-.scratch/acceptance}"
FRAME_GLOB="${ACCEPTANCE_FRAME_GLOB:-*IMG_*.tif}"
EXPECTED_FRAMES="${ACCEPTANCE_EXPECTED_FRAMES:-61}"
CLI="${ACCEPTANCE_CLI:-target/release/focusforge-cli}"

# --- refuse to measure with a CLI older than the sources -------------------
# Refuses a release binary older than the sources rather than rebuilding, so
# a compile never lands in the measured wall clock and stale code is never
# measured.
if [ ! -x "$CLI" ]; then
  echo "refuse: CLI $CLI does not exist or is not executable" >&2
  echo "hint: cargo build --release --workspace --bin focusforge-cli" >&2
  exit 1
fi
newest_src="$(find crates Cargo.toml Cargo.lock -name target -prune -o \
  \( -name '*.rs' -o -name '*.wgsl' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) \
  -newer "$CLI" -print 2>/dev/null | head -n 1 || true)"
if [ -n "$newest_src" ]; then
  echo "refuse: $CLI is older than the workspace sources -- it would measure stale code" >&2
  echo "        first newer source found: $newest_src" >&2
  echo "hint: cargo build --release --workspace --bin focusforge-cli" >&2
  exit 1
fi
echo "OK: $CLI is newer than every workspace source"

# --- resolve the source frame directory ------------------------------------
#
# frame_paths + uniform_check make stacking the source frame directory
# directly impossible rather than merely ill-advised: it holds 62 *.tif
# files, Mantis_Stacked.tif among them, and the byte-order/rows-per-strip
# mismatch between it and the 61 source frames trips the strict uniformity
# guard in crates/cli/src/main.rs before a tile is ever read. So the run
# never points at that directory directly; it stacks a 61-entry symlink
# farm built below, which also keeps the source frame directory untouched.
if [ -n "${ACCEPTANCE_STACK_DIR:-}" ]; then
  STACK="$ACCEPTANCE_STACK_DIR"
  if [ ! -d "$STACK" ]; then
    echo "refuse: ACCEPTANCE_STACK_DIR=$STACK is not a directory" >&2
    exit 1
  fi
  STACK="$(cd "$STACK" && pwd)"
else
  HARNESS=""
  for candidate in "$REPO/../../.." "$REPO/../reference-stack"; do
    if [ -f "$candidate/tools/lib/rawtiff.py" ]; then
      HARNESS="$(cd "$candidate" && pwd)"
      break
    fi
  done

  if [ -z "$HARNESS" ] || [ ! -d "$HARNESS/Stack" ]; then
    echo "SKIP: no reachable reference stack next to this checkout (looked for" \
         "tools/lib/rawtiff.py under \$REPO/../../.. and \$REPO/../reference-stack)"
    exit 0
  fi
  STACK="$HARNESS/Stack"
fi

if [ ! -x "$CLI" ]; then
  echo "refuse: $CLI is not built -- run: cargo build --release -p focusforge-cli" >&2
  exit 1
fi

limit_to_bytes() {
  # systemd's K/M/G/T suffixes are base-1024
  # (man systemd.resource-control -- MemoryMax=): 8G == 8 * 1024^3 ==
  # 8589934592. A bare number (no suffix) is already bytes.
  local v="$1"
  case "$v" in
    *K) echo $(( ${v%K} * 1024 )) ;;
    *M) echo $(( ${v%M} * 1024 * 1024 )) ;;
    *G) echo $(( ${v%G} * 1024 * 1024 * 1024 )) ;;
    *T) echo $(( ${v%T} * 1024 * 1024 * 1024 * 1024 )) ;;
    *) echo "$v" ;;
  esac
}

# --- the input farm and its headers-only check, before any long work -------
#
# Skipped entirely for --inside-scope: that re-entry runs in the same
# checkout the --limit invocation already built the farm in (a memory cgroup
# does not isolate the filesystem), so redoing it here would just cost a
# second, pointless headers-only CLI call inside the timed scope.
build_farm() {
  rm -rf "$OUT_DIR/frames"
  mkdir -p "$OUT_DIR/frames"

  shopt -s nullglob
  local candidates=( "$STACK"/$FRAME_GLOB )
  shopt -u nullglob

  local count=0
  local f name
  for f in "${candidates[@]}"; do
    name="$(basename "$f")"
    case "$name" in
      *Stacked*) continue ;;
    esac
    ln -s "$f" "$OUT_DIR/frames/$name"
    count=$((count + 1))
  done

  if [ "$count" -ne "$EXPECTED_FRAMES" ]; then
    echo "refuse: expected $EXPECTED_FRAMES symlinks in $OUT_DIR/frames, built $count" >&2
    # A zero count is almost always the glob, not a missing stack: the default
    # only matches the reference stack's own frame naming, so pointing this
    # script at any other frame directory needs ACCEPTANCE_FRAME_GLOB set
    # too. Say so here rather than leaving a correct refusal that reads as a
    # mystery.
    if [ "$count" -eq 0 ]; then
      echo "hint: no file in $STACK matched ACCEPTANCE_FRAME_GLOB='$FRAME_GLOB'" >&2
    fi
    exit 1
  fi
  if ls "$OUT_DIR/frames" | grep -q Stacked; then
    echo "refuse: the farm contains an entry matching \"Stacked\"" >&2
    exit 1
  fi
  echo "OK: built $count-entry frame farm at $OUT_DIR/frames"

  "$CLI" info --json "$OUT_DIR/frames" > "$OUT_DIR/info.json"
  if ! grep -q "\"frame_count\": $EXPECTED_FRAMES" "$OUT_DIR/info.json"; then
    echo "refuse: $OUT_DIR/info.json does not report frame_count $EXPECTED_FRAMES" >&2
    exit 1
  fi
  if ! grep -q '"uniform_layout": true' "$OUT_DIR/info.json"; then
    echo "refuse: $OUT_DIR/info.json does not report uniform_layout true" >&2
    exit 1
  fi
  echo "OK: $OUT_DIR/info.json reports frame_count $EXPECTED_FRAMES, uniform_layout true"

  # A missing output parent is refused before any long work starts.
  mkdir -p "$OUT_DIR/unlimited" "$OUT_DIR/limited"
}

# --- the run itself: identical command, different OUT ----------------------
#
# `--no-crop`: `stack` applies the auto-crop by default, which would shrink
# the composite below the full 8192x5464 extent that
# `crates/cli/tests/acceptance.rs` compares against `Mantis_Stacked.tif`
# (`tools/tiff-diff.py` refuses any dimension mismatch outright).
# `--align-algo pyramidal-ecc --lax`: `--lax` is not optional here -- a
# 61-frame sweep anchored on frame 30 is a longer baseline than the
# 8-frame fixture on which frame 0 already diverges, and a strict refusal
# would waste the whole run.
# Wrapped in both locks (tools/lib/lock.sh): `build`, because this reads
# target/release/$CLI directly and a concurrent `cargo build` running at
# the same time would relink it mid-read; `gpu`, because `stack` drives
# real wgpu compute on the one physical GPU this machine has, and a second
# concurrent GPU-touching run at the same time is exactly what OOM-killed a
# run before.
run_stack() {
  local out="$1"
  tools/with-lock.sh build -- tools/with-lock.sh gpu -- \
    /usr/bin/time -v "$CLI" stack "$OUT_DIR/frames" \
      -o "$out/out.tif" --zmap "$out/out-zmap.tif" \
      --algo laplacian --set levels=5 --json \
      --no-crop --align-algo pyramidal-ecc --lax \
      > "$out/stack.json" 2> "$out/time-v.txt"
}

# `align`, once per mode, in the unlimited branch only: the `--inside-scope`
# branch stays a single `run_stack`, whose own in-process alignment is
# exactly what the memory ceiling is being measured with, and a second
# standalone align inside the scope would spend minutes measuring nothing
# new. `--lax` for the same reason `run_stack` above carries it.
run_align() {
  local out="$1"
  local mode="$2"
  tools/with-lock.sh build -- tools/with-lock.sh gpu -- \
    /usr/bin/time -v "$CLI" align "$OUT_DIR/frames" \
      --algo pyramidal-ecc --set "mode=$mode" --lax --json \
      > "$out/align-$mode.json" 2> "$out/align-$mode-time-v.txt"
}

print_summary() {
  local out="$1"
  if [ -f "$out/stack.json" ]; then
    python3 -c '
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as exc:
    print("(could not parse " + sys.argv[1] + ": " + str(exc) + ")")
    sys.exit(0)
for key in ("wall_seconds", "tiles", "frame_tiles_read", "in_flight_limit", "peak_in_flight"):
    print(key + "=" + str(d.get(key)))
' "$out/stack.json"
  fi
  for mode in translation rotation-scale; do
    if [ -f "$out/align-$mode.json" ]; then
      python3 -c '
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as exc:
    print("(could not parse " + sys.argv[1] + ": " + str(exc) + ")")
    sys.exit(0)
alignments = d.get("alignments", [])
max_drift = 0.0
for e in alignments:
    drift = (e.get("tx", 0.0) ** 2 + e.get("ty", 0.0) ** 2) ** 0.5
    max_drift = max(max_drift, drift)
print("align-" + sys.argv[2] + ": frame_count=" + str(d.get("frame_count"))
      + " failure_count=" + str(d.get("failure_count"))
      + " max_translation=" + str(round(max_drift, 2)))
' "$out/align-$mode.json" "$mode"
    fi
  done
  grep -E 'Maximum resident set size|Elapsed \(wall clock\) time' "$out/time-v.txt" 2>/dev/null || true
}

case "$MODE" in
  "")
    build_farm
    for mode in translation rotation-scale; do
      echo "+ $CLI align $OUT_DIR/frames --algo pyramidal-ecc --set mode=$mode --lax --json ..."
      run_align "$OUT_DIR/unlimited" "$mode"
    done
    echo "OK: align stage complete (both modes)"
    echo "+ $CLI stack $OUT_DIR/frames -o $OUT_DIR/unlimited/out.tif ..."
    run_stack "$OUT_DIR/unlimited"
    echo "OK: unlimited run complete"
    print_summary "$OUT_DIR/unlimited"
    ;;

  --limit)
    build_farm
    echo "+ systemd-run --user --scope -p MemoryMax=$LIMIT -p MemorySwapMax=0" \
         "-- $REPO/tools/run-acceptance.sh --inside-scope $LIMIT"
    # --scope, not a service: `man systemd-run` DESCRIPTION -- a transient
    # scope is executed by systemd-run itself as the parent process and
    # inherits the caller's execution environment (cwd, env, exit status),
    # and execution is synchronous. No --quiet: the printed
    # "Running as unit: run-rNNN.scope" is what makes
    # `journalctl --user -u run-rNNN.scope` findable afterwards.
    exec systemd-run --user --scope -p "MemoryMax=$LIMIT" -p MemorySwapMax=0 \
        -- "$REPO/tools/run-acceptance.sh" --inside-scope "$LIMIT"
    ;;

  --inside-scope)
    # Safe to invoke directly; the real caller is the `--limit` branch's
    # re-exec, which already built $OUT_DIR/limited via build_farm.
    mkdir -p "$OUT_DIR/limited"
    cg="/sys/fs/cgroup$(awk -F: '$1=="0"{print $3}' /proc/self/cgroup)"
    want_bytes="$(limit_to_bytes "$LIMIT")"

    mem_max="$(cat "$cg/memory.max" 2>/dev/null || echo unknown)"
    mem_swap_max="$(cat "$cg/memory.swap.max" 2>/dev/null || echo unknown)"
    {
      echo "memory.max: $mem_max"
      echo "memory.swap.max: $mem_swap_max"
      echo "memory.current: $(cat "$cg/memory.current" 2>/dev/null || echo unknown)"
    } > "$OUT_DIR/limited/cgroup-before.txt"

    # Refuse unless the limit actually took: a silently unenforced MemoryMax
    # in a transient --user scope is a known failure mode on older systemd
    # (<https://access.redhat.com/solutions/7099227>, RHEL-8-class). A
    # ceiling run that "passed" under an unenforced limit is the one
    # failure mode indistinguishable from success, which is exactly why
    # this reads the cgroup back rather than trusting systemd-run's exit
    # status.
    if [ "$mem_max" != "$want_bytes" ] || [ "$mem_swap_max" != "0" ]; then
      echo "refuse: cgroup limit was not enforced -- memory.max=$mem_max" \
           "(wanted $want_bytes), memory.swap.max=$mem_swap_max (wanted 0)" >&2
      exit 1
    fi
    echo "OK: memory.max=$mem_max memory.swap.max=$mem_swap_max, limit confirmed in force"

    echo "+ $CLI stack $OUT_DIR/frames -o $OUT_DIR/limited/out.tif ... (under $LIMIT)"
    set +e
    run_stack "$OUT_DIR/limited"
    status=$?
    set -e

    # Dump always, pass or fail, while the cgroup still exists -- a --scope
    # cgroup is destroyed on exit, so this is the only chance to read it.
    {
      echo "memory.peak: $(cat "$cg/memory.peak" 2>/dev/null || echo unknown)"
      echo "memory.swap.peak: $(cat "$cg/memory.swap.peak" 2>/dev/null || echo unknown)"
      echo "memory.current: $(cat "$cg/memory.current" 2>/dev/null || echo unknown)"
      echo "memory.zswap.current: $(cat "$cg/memory.zswap.current" 2>/dev/null || echo unknown)"
      echo "memory.events:"
      cat "$cg/memory.events" 2>/dev/null || echo unknown
      echo "memory.stat (anon/file/inactive_file/active_file/slab/sock):"
      grep -E '^(anon|file|inactive_file|active_file|slab|sock) ' "$cg/memory.stat" 2>/dev/null || echo unknown
    } > "$OUT_DIR/limited/cgroup-after.txt"

    if [ "$status" -eq 0 ]; then
      echo "OK: limited run complete, exit $status"
    else
      echo "FAILED: limited run exited $status -- see $OUT_DIR/limited/time-v.txt and cgroup-after.txt" >&2
    fi
    print_summary "$OUT_DIR/limited"
    exit "$status"
    ;;
esac
