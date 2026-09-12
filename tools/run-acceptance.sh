#!/usr/bin/env bash
# Usage: tools/run-acceptance.sh
#
# The acceptance run: the whole 61-frame, 16 GB reference stack through the
# real CLI, end to end. This is not a gate and never runs in check-all.sh --
# it takes minutes, and a gate that takes minutes stops being run. It is the
# manual test that answers the question the fixtures cannot: does the output
# resemble a correctly focus-stacked image at full size? Compared against
# Mantis_Stacked.tif, which a different stacker produced, so a loose tolerance:
# it is a similarity floor, not a target.
#
#   1. `focusforge-cli stack` over a 61-entry symlink farm built from the
#      source frames (never read directly -- see the glob trap below),
#      producing a composite and a Z-Map under .scratch/acceptance/unlimited/.
#   2. The identical run again under `systemd-run --user --scope
#      -p MemoryMax=8G -p MemorySwapMax=0`, producing the same two files
#      under .scratch/acceptance/limited/. Completing without an OOM kill
#      is the assertion; memory.peak, memory.swap.peak and the anon/file
#      split are recorded, never thresholded.
#
# Both runs use target/release/focusforge-cli, never target/debug. This
# diverges from tools/fixture-e2e.sh, which deliberately runs the debug
# binary so that check-all.sh can run it in the background beside the
# cargo gates (cargo holds an exclusive lock on target/, so a background
# cargo command would just queue behind the foreground one). This script
# is outside the gate entirely, its numbers are a GPU-speedup baseline,
# and the workspace Cargo.toml has no [profile.*] section at all -- dev is
# opt-level = 0, and unoptimised float loops over ~8.2 G samples plausibly
# turn a 4-minute run into an hour.
#
# Why not `ulimit -v` for the memory ceiling: `crates/core/src/io/tiff/stack.rs`
# holds one mmap per frame for the stack's lifetime, so after the first tile
# row all 61 mappings are live -- 16 GB of address space with an arbitrarily
# small resident set. An address-space limit would fail for a reason
# unrelated to the property under test. `prlimit --rss` is a dead end too:
# RLIMIT_RSS has been a kernel no-op since Linux 2.4.
#
# wrapped in `/usr/bin/time -v` so the run records its wall time alongside the
# diff numbers. All output under .scratch/.
#
# Exit: 2 -- refuses, for now.

set -euo pipefail

case "${1:-}" in
  "") ;;
  *) echo "usage: tools/run-acceptance.sh" >&2; exit 2 ;;
esac

echo "usage: tools/run-acceptance.sh"
echo
echo "refuses: the CLI has no \`stack\` subcommand."
echo "Until then the fixtures are the whole of the automated evidence:"
echo "  tools/check-all.sh"
exit 2
