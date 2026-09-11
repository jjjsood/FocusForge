#!/usr/bin/env bash
# Usage: tools/run-acceptance.sh [--mem-limit SIZE]
#
# The acceptance run: the whole 61-frame, 16 GB reference stack through the
# real CLI, end to end. This is not a gate and never runs in check-all.sh --
# it is the manual test that answers the two questions the fixtures cannot.
#
#   1. Does the output resemble a correctly focus-stacked image? Compared
#      against Mantis_Stacked.tif, which a different stacker produced, so a
#      loose tolerance: it is a similarity floor, not a target.
#   2. Does it fit in memory? A full load of this stack as f32 needs ~43 GB
#      against ~40 GB available, so "tiling works" is a claim only this run
#      can support. That makes the peak RSS the primary result, not a note.
#
# Intended behaviour once `focusforge-cli stack` exists (the stack's path
# Intended behaviour once `focusforge-cli stack` exists (the reference
# stack's path depends on where this repo sits, resolved relative to the
# checkout root):
# Intended behaviour once `focusforge-cli stack` exists:
#
# wrapped in `/usr/bin/time -v` so the run records its wall time alongside the
# diff numbers. All output under .scratch/.
#
# Exit: 2 -- refuses, for now.

set -euo pipefail

MEM_LIMIT=""
case "${1:-}" in
  --mem-limit)
    [ "$#" -ge 2 ] || { echo "usage: tools/run-acceptance.sh [--mem-limit SIZE]" >&2; exit 2; }
    MEM_LIMIT="$2"
    ;;
  "") ;;
  *) echo "usage: tools/run-acceptance.sh [--mem-limit SIZE]" >&2; exit 2 ;;
esac

echo "usage: tools/run-acceptance.sh [--mem-limit SIZE]"
echo
echo "refuses: the CLI has no \`stack\` subcommand."
echo "Until then the fixtures are the whole of the automated evidence:"
echo "  tools/check-all.sh"
[ -n "$MEM_LIMIT" ] && echo "(--mem-limit $MEM_LIMIT noted; it will wrap the run in tools/with-mem-limit.sh)"
exit 2
