#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Default parallel job count for the gate's cargo stages (sourced).
#
# Cargo's own default is the core count, and the Slint UI crate is one rustc
# process of over 13 GB, so the gate caps the jobs it hands cargo.

# Highest default job count.
CARGO_JOBS_CAP=24

# Prints $CARGO_BUILD_JOBS when set and non-empty, else min(cap, cores).
# Optional arguments replace the cap and the core count.
cargo_jobs() {
  local cap="${1:-$CARGO_JOBS_CAP}" cores="${2:-}"
  if [ -n "${CARGO_BUILD_JOBS:-}" ]; then
    printf '%s\n' "$CARGO_BUILD_JOBS"
    return
  fi
  [ -n "$cores" ] || cores="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
  printf '%s\n' "$((cap < cores ? cap : cores))"
}
