#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/run-gpu-suite.sh [-- <extra cargo test args>]
#
# Runs the full `cargo test -p focusforge-gpu --features gpu-tests` suite
# under the GPU lock, with output logged to a file instead of piped through
# something like `| tail -60`. A trailing pipe like that buffers everything
# until the process exits, so a long run gives no way to tell progress from
# a hang -- this suite's own worst-case tiling tests (the forced-one-tile
# budget parity runs) can take well over an hour. `run_logged` prints the
# log path immediately and lets `tail -f` follow it while the run is still
# going, and folds the GPU lock wait into the same timed status line.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/run-logged.sh
. "$HERE/lib/run-logged.sh"

runlog_init "gpu-suite"
run_logged --tail-ok 40 "gpu-suite" \
  "$HERE/with-lock.sh" gpu -- cargo test -p focusforge-gpu --features gpu-tests -- --test-threads=1 "$@"
status=$?
runlog_summary "gate"
exit "$status"
