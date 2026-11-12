#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Isolated checks for the default parallel job count of the gate's cargo stages.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/cargo-jobs.sh
. "$REPO/tools/lib/cargo-jobs.sh"

FAIL=0

expect_jobs() {  # expect_jobs <name> <wanted> <CARGO_BUILD_JOBS or "-unset"> [cap [cores]]
  local name="$1" wanted="$2" env_value="$3" actual
  shift 3
  if [ "$env_value" = -unset ]; then
    actual="$(env -u CARGO_BUILD_JOBS bash -c '. "$1"; shift; cargo_jobs "$@"' -- "$REPO/tools/lib/cargo-jobs.sh" "$@")"
  else
    actual="$(CARGO_BUILD_JOBS="$env_value" bash -c '. "$1"; shift; cargo_jobs "$@"' -- "$REPO/tools/lib/cargo-jobs.sh" "$@")"
  fi
  if [ "$actual" = "$wanted" ]; then
    echo "ok   [$name]"
  else
    echo "FAIL [$name]: expected '$wanted', got '$actual'" >&2
    FAIL=1
  fi
}

cores="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
min_default=$((CARGO_JOBS_CAP < cores ? CARGO_JOBS_CAP : cores))

expect_jobs "unset gives min(cap, cores)" "$min_default" -unset
expect_jobs "empty gives min(cap, cores)" "$min_default" ""
expect_jobs "unset below the cap on a big machine" 24 -unset 24 32
expect_jobs "unset on a machine above the cap" 16 -unset 16 64
expect_jobs "cap above the core count gives the core count" 4 -unset 24 4
expect_jobs "empty with a cap above the core count" 4 "" 24 4
expect_jobs "CARGO_BUILD_JOBS=1 passes through" 1 1 24 32
expect_jobs "CARGO_BUILD_JOBS=2 passes through on a small machine" 2 2 24 1
expect_jobs "a value above the cap passes through" 48 48 24 32

[ "$FAIL" -eq 0 ] || exit 1
echo "OK: cargo jobs selftest"
