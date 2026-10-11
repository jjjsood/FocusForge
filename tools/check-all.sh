#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/check-all.sh [--static|--quick] [--verbose] [--tail N] [--log-dir DIR]
#
# Every gate this repository has, in one command, with a pass/fail summary at
# the end. This script is the canonical build gate: local runs remain required
# for acceptance and merges, while hosted CI calls this same command for
# additional native-platform evidence.
#
#   --static     only the fast, read-only checks -- no builds, no test suites.
#                This is the set .githooks/pre-commit runs, so running it by
#                hand tells you whether a commit will pass.
#   --quick      every gate except the minutes-long ones: the nextest
#                `quick` profile (.config/nextest.toml) instead of the whole
#                workspace, and no `trace feature` or `advisories` stage. A
#                change's working check; the full run stays the build.
#   --verbose    also stream every gate's output to the terminal. Unbounded:
#                a caller with a fixed output budget will lose the summary.
#   --tail N     lines of a failing gate's log to print inline (default 40,
#                0 prints none).
#   --log-dir D  write logs to D instead of .logs/gate/run-<stamp>/.
#
# Output contract: gate output goes to per-gate logs; stdout stays two
# lines per gate. `fixture e2e` runs the built binary in the background,
# cargo-free, after `build`. `build (no default features)` runs first so
# `build`'s GPU-enabled binary is written last, the one later stages use.
# Cargo and direct binary runs go through tools/with-lock.sh build; clippy
# stays out of --static because it compiles; RUSTFLAGS is never set here.
#
# Exit code: 0 all gates passed, 1 at least one failed.
set -uo pipefail  # deliberately not -e: every gate runs, then the summary

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

STATIC_ONLY=0
QUICK=0
VERBOSE=0
TAIL_LINES=40
LOG_DIR=""

usage() {
  echo "usage: tools/check-all.sh [--static|--quick] [--verbose] [--tail N] [--log-dir DIR]" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --static) STATIC_ONLY=1; shift ;;
    --quick) QUICK=1; shift ;;
    --verbose|-v) VERBOSE=1; shift ;;
    --tail) [ "$#" -ge 2 ] || usage; TAIL_LINES="$2"; shift 2 ;;
    --log-dir) [ "$#" -ge 2 ] || usage; LOG_DIR="$2"; shift 2 ;;
    *) usage ;;
  esac
done

[ "$STATIC_ONLY" -eq 1 ] && [ "$QUICK" -eq 1 ] && usage

case "$TAIL_LINES" in
  ''|*[!0-9]*) usage ;;
esac

[ -f Cargo.toml ] || {
  echo "SKIP check-all: no Cargo.toml"
  exit 0
}

RUNLOG_ROOT="$REPO/.logs/gate"
RUNLOG_DIR="$LOG_DIR"
RUNLOG_VERBOSE="$VERBOSE"
RUNLOG_TAIL="$TAIL_LINES"
# shellcheck source=lib/run-logged.sh
. "$REPO/tools/lib/run-logged.sh"
runlog_init run || exit $?

# A failing gate must not stop the run: every gate reports, then the summary.
run_gate() { run_logged "$@" || true; }

# What the workspace test run left in the temporary directory it was given.
# Anything at all is a failure: every test owns its directory through a guard
# that removes it. The entries are listed, not counted, because cargo and
# rustc write here too when a stage of this script compiles anything.
test_tmp_residue() {
  local dir="$1"
  local left
  left="$(ls -A "$dir" 2>/dev/null)"
  [ -z "$left" ] && return 0
  echo "the workspace test run left these behind in ${dir#"$REPO"/}:"
  ls -lA "$dir"
  return 1
}

# --- static checks: seconds each, no side effects ---------------------------
run_gate "rustfmt"          cargo fmt --all --check
run_gate "licenses"         tools/check-licenses.sh
run_gate "source headers"   tools/check-headers.sh
run_gate "source headers selftest" tools/check-headers-selftest.sh
run_gate "NOTICE"           tools/with-lock.sh build -- tools/check-notice.sh
run_gate "NOTICE selftest"  tools/check-notice-selftest.sh
run_gate "logging selftest" tools/check-logging-selftest.sh
run_gate "workflow selftest" python3 tools/check-workflow-selftest.py
run_gate "release smoke selftest" tools/smoke-release-selftest.sh
run_gate "fixture manifest" tools/check-fixtures.sh
run_gate "fixture manifest selftest" tools/check-fixtures-selftest.sh
run_gate "lock directory selftest" tools/with-lock-selftest.sh
run_gate "tiff-diff selftest" python3 tools/tiff-diff.py --selftest
run_gate "image-metrics selftest" python3 tools/image-metrics.py --selftest
run_gate "rawtiff selftest" python3 tools/lib/rawtiff.py --selftest

if [ "$STATIC_ONLY" -eq 0 ]; then
  # Build-only: proves the no-GPU path still compiles, test targets included.
  # Must run BEFORE `build` -- see the header comment above for why the order
  # is load-bearing. `--exclude focusforge-gui`: the GUI's Slint backend is
  # wgpu-only by construction (crates/gui/Cargo.toml pins
  # `renderer-femtovg-wgpu` unconditionally, not behind the `gpu` feature),
  # so it always compiles wgpu regardless of `--no-default-features` -- with
  # it included this stage would still link wgpu and would prove nothing
  # about the no-GPU path.
  run_gate "build (no default features)" \
                             tools/with-lock.sh build -- \
                             cargo build --workspace --all-targets \
                             --no-default-features --exclude focusforge-gui

  # Must come first among the GPU-enabled stages: it produces the binary the
  # background stage runs, and it holds the target/ lock the cargo gates below
  # queue on anyway.
  run_gate "build"          tools/with-lock.sh build -- cargo build --workspace --all-targets

  # The long background stage, started now and joined at the end. Not a
  # cargo command -- see the header.
  run_logged_bg "fixture e2e" tools/with-lock.sh build -- tools/fixture-e2e.sh

  # Foreground and BEFORE `cargo test`, never a background stage: `cargo
  # test` reads what this writes (.scratch/synth-align/measurement.json), so
  # a background stage joined at the end would race the test into skipping.
  run_gate "synthetic alignment truth" tools/synth-align.sh

  # --- remaining gates --------------------------------------------------
  # Default features only, not --all-features: that flag pulled in gpu-tests
  # (the parity test tree), a third feature-fingerprint compiled on every run
  # just to be linted, never run. Lint that tree explicitly when it changes:
  # cargo clippy -p focusforge-gpu --features gpu-tests --all-targets -- -D warnings
  run_gate "clippy"         tools/with-lock.sh build -- cargo clippy --workspace --all-targets -- -D warnings

  # FF_REQUIRE_SYNTH_ALIGN=1 turns the test's skip into a failure when the
  # artefact exists: a skip inside the gate must read as a bug, not a pass.
  REQUIRE_SYNTH=0
  [ -f .scratch/synth-align/measurement.json ] && REQUIRE_SYNTH=1
  # nextest runs binaries in parallel; doctests need the separate `cargo
  # test --doc` invocation right after, since nextest cannot run them.
  #
  # A private TMPDIR, emptied first, so the stage after can see what the run
  # abandoned. /tmp is a bounded tmpfs on a current Linux desktop, which a
  # leaking test fills within a few runs.
  TEST_TMP="$REPO/.scratch/test-tmp"
  rm -rf "$TEST_TMP"
  mkdir -p "$TEST_TMP"
  NEXTEST_PROFILE=default
  [ "$QUICK" -eq 1 ] && NEXTEST_PROFILE=quick
  run_gate "cargo nextest"  tools/with-lock.sh build -- \
                              env FF_REQUIRE_SYNTH_ALIGN="$REQUIRE_SYNTH" TMPDIR="$TEST_TMP" TMP="$TEST_TMP" TEMP="$TEST_TMP" \
                              cargo nextest run --workspace --no-fail-fast \
                              --profile "$NEXTEST_PROFILE"
  run_gate "test tmp residue" test_tmp_residue "$TEST_TMP"
  run_gate "cargo test --doc" tools/with-lock.sh build -- \
                              env FF_REQUIRE_SYNTH_ALIGN="$REQUIRE_SYNTH" cargo test --doc --workspace
  # The off-by-default `trace` feature's own suite: the stage above resolves
  # no profiling crate at all, and the clippy stage compiles the feature
  # without running anything. ~13.5s on an already-compiled tree, most of it
  # one real stack run over fixtures/mantis-512.
  [ "$QUICK" -eq 1 ] || run_gate "trace feature"  tools/with-lock.sh build -- \
                              cargo test -p focusforge-cli --features trace --test trace_cli
  run_gate "rustdoc"        tools/with-lock.sh build -- \
                              env RUSTDOCFLAGS="-D warnings" \
                              cargo doc --workspace --no-deps --document-private-items
  # Separate from `licenses` on purpose: an advisory is a fact about the world
  # that can turn red without this repository changing, so it must not be able
  # to block a commit through the pre-commit hook.
  [ "$QUICK" -eq 1 ] || run_gate "advisories"     tools/check-licenses.sh --advisories

  runlog_join || true
fi

runlog_summary gate
exit "$RUNLOG_FAILED"
