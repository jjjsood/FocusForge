#!/usr/bin/env bash
set -euo pipefail
# Run a command in its own systemd user scope with a hard RAM cap (default 32G,
# swap off), so a runaway out-of-core stack job is killed alone instead of
# taking the machine with it. Technical backstop, NOT a replacement for the
# ASK-FIRST rule in this project's rules. RAM only -- a systemd scope's
# MemoryMax has no VRAM equivalent, so a runaway wgpu allocation is not caught
# by this cap.
# Usage: tools/with-mem-limit.sh <command> [args...]
#   tools/with-mem-limit.sh target/release/focusforge-cli stack ../reference-stack -o out.tif
#   FF_MEM_MAX=48G tools/with-mem-limit.sh cargo test --workspace   # raise
#   FF_MEM_MAX=off tools/with-mem-limit.sh ...                      # disable
# Exit code: the command's own, or 137 when the cap killed it.

[ "$#" -gt 0 ] || { echo "usage: tools/with-mem-limit.sh <command> [args...]" >&2; exit 2; }

MEM_MAX="${FF_MEM_MAX:-32G}"

# FF_MEM_SCOPE means "already capped" — set below, never by hand. A second
# systemd-run would open a SIBLING scope under user.slice, escaping the outer
# cap rather than nesting inside it.
if [ -n "${FF_MEM_SCOPE:-}" ] || [ "$MEM_MAX" = off ]; then
  exec "$@"
fi

export FF_MEM_SCOPE=1
status=0
systemd-run --user --scope -q --same-dir \
  -p MemoryMax="$MEM_MAX" -p MemorySwapMax=0 -p OOMPolicy=kill -- "$@" || status=$?

if [ "$status" -eq 137 ]; then
  echo "with-mem-limit: killed by the $MEM_MAX cap. Run one stack job per" >&2
  echo "  invocation, or raise FF_MEM_MAX." >&2
fi
exit "$status"
