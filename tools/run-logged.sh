#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Run ONE noisy command with bounded stdout: a status line here, the full
# output in a log file, the tail of it inline if it failed.
#
# Usage: tools/run-logged.sh [options] <name> [--] <command> [args...]
#   tools/run-logged.sh "build" -- cargo build --workspace --all-targets
#   tools/run-logged.sh --tail 80 "tests" -- cargo test --workspace
#   tools/run-logged.sh --tail-ok 5 "info" -- target/debug/focusforge-cli info path/to/frames
#
#   --verbose      stream the output as well (unbounded -- defeats the point)
#   --tail N       lines of the log printed inline on FAILURE (default 40, 0 none)
#   --tail-ok N    lines printed on SUCCESS too, for a command whose last lines
#                  are the result you actually wanted
#   --log-dir D    write into D instead of a fresh .logs/tool/ run dir
#
# This is the front end for the noisy commands that live outside tools/ --
# `cargo build --workspace`, `cargo test --workspace`, a long `focusforge-cli`
# run. The tools/ scripts that drive a noisy subprocess themselves source
# tools/lib/run-logged.sh directly instead of shelling out to this.
#
# Exit code: the command's own.
set -uo pipefail

TOOLS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  echo "usage: tools/run-logged.sh [--verbose] [--tail N] [--tail-ok N] [--log-dir D] <name> [--] <command> [args...]" >&2
  exit 2
}

TAIL_OK=0
declare -a OPTS=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --verbose|-v) RUNLOG_VERBOSE=1; shift ;;
    --tail) [ "$#" -ge 2 ] || usage; OPTS+=(--tail "$2"); shift 2 ;;
    --tail-ok) [ "$#" -ge 2 ] || usage; TAIL_OK="$2"; shift 2 ;;
    --log-dir) [ "$#" -ge 2 ] || usage; RUNLOG_DIR="$2"; shift 2 ;;
    --) shift; break ;;
    -*) usage ;;
    *) break ;;
  esac
done

[ "$#" -ge 1 ] || usage
NAME="$1"; shift
[ "${1:-}" = "--" ] && shift
[ "$#" -ge 1 ] || usage

OPTS+=(--tail-ok "$TAIL_OK")

# shellcheck source=lib/run-logged.sh
. "$TOOLS/lib/run-logged.sh"

run_logged "${OPTS[@]}" "$NAME" "$@"
exit $?
