#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/run-gpu-suite.sh [<extra cargo nextest run args>]
#
# Runs `cargo nextest run -p focusforge-gpu --features gpu-tests --profile gpu`
# under the GPU lock with FF_REQUIRE_GPU=1 (no adapter, or a Cpu-type one,
# fails instead of skipping). Output goes to a log file, not a pipe such as
# `| tail -60`, which buffers until exit and hides progress from a hang.
# `run_logged` prints the log path at once, so `tail -f` follows the run.
# Per-test durations land in target/nextest/gpu/junit.xml.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/run-logged.sh
. "$HERE/lib/run-logged.sh"

runlog_init "gpu-suite"
run_logged --tail-ok 40 "gpu-suite" \
  "$HERE/with-lock.sh" gpu -- env FF_REQUIRE_GPU=1 \
  cargo nextest run -p focusforge-gpu --features gpu-tests --profile gpu "$@"
status=$?
runlog_summary "gate"
exit "$status"
