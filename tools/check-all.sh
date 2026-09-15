#!/usr/bin/env bash
# Usage: tools/check-all.sh [--static] [--verbose] [--tail N] [--log-dir DIR]
#
# Every gate this repository has, in one command, with a pass/fail summary at
# the end. There is no hosted CI and no remote, so this script *is* the build:
# what it reports is the whole of what is enforced.
#
#   --static     only the fast, read-only checks -- no builds, no test suites.
#                This is the set .githooks/pre-commit runs, so running it by
#                hand tells you whether a commit will pass.
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
VERBOSE=0
TAIL_LINES=40
LOG_DIR=""

usage() {
  echo "usage: tools/check-all.sh [--static] [--verbose] [--tail N] [--log-dir DIR]" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --static) STATIC_ONLY=1; shift ;;
    --verbose|-v) VERBOSE=1; shift ;;
    --tail) [ "$#" -ge 2 ] || usage; TAIL_LINES="$2"; shift 2 ;;
    --log-dir) [ "$#" -ge 2 ] || usage; LOG_DIR="$2"; shift 2 ;;
    *) usage ;;
  esac
done

case "$TAIL_LINES" in
  ''|*[!0-9]*) usage ;;
esac

# Before anything creates a log directory: until the first workspace item lands
# there is no crate to check, and every gate below would report a confusing
# red for a repository that is simply empty. A clean skip is the honest answer.
[ -f Cargo.toml ] || {
  echo "SKIP check-all: no Cargo.toml yet -- the first workspace item creates it"
  exit 0
}

RUNLOG_ROOT="$REPO/.logs/gate"
RUNLOG_DIR="$LOG_DIR"
RUNLOG_VERBOSE="$VERBOSE"
RUNLOG_TAIL="$TAIL_LINES"
# shellcheck source=lib/run-logged.sh
. "$REPO/tools/lib/run-logged.sh"
runlog_init run

# A failing gate must not stop the run: every gate reports, then the summary.
run_gate() { run_logged "$@" || true; }

# --- static checks: seconds each, no side effects ---------------------------
run_gate "rustfmt"          cargo fmt --all --check
run_gate "licenses"         tools/check-licenses.sh
run_gate "fixture manifest" tools/check-fixtures.sh
run_gate "fixture manifest selftest" tools/check-fixtures-selftest.sh

if [ "$STATIC_ONLY" -eq 0 ]; then
  # Build-only: proves the no-GPU path still compiles, test targets included.
  # Must run BEFORE `build` -- see the two-track header comment above for why
  # the order is load-bearing.
  run_gate "build (no default features)" \
                             tools/with-lock.sh build -- \
                             cargo build --workspace --all-targets --no-default-features

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
  run_gate "clippy"         tools/with-lock.sh build -- cargo clippy --workspace --all-targets --all-features -- -D warnings

  # FF_REQUIRE_SYNTH_ALIGN=1 turns the test's skip into a failure when the
  # artefact exists: a skip inside the gate must read as a bug, not a pass.
  REQUIRE_SYNTH=0
  [ -f .scratch/synth-align/measurement.json ] && REQUIRE_SYNTH=1
  run_gate "cargo test"     tools/with-lock.sh build -- \
                              env FF_REQUIRE_SYNTH_ALIGN="$REQUIRE_SYNTH" cargo test --workspace
  run_gate "rustdoc"        tools/with-lock.sh build -- \
                              env RUSTDOCFLAGS="-D warnings" \
                              cargo doc --workspace --no-deps --document-private-items
  # Separate from `licenses` on purpose: an advisory is a fact about the world
  # that can turn red without this repository changing, so it must not be able
  # to block a commit through the pre-commit hook.
  run_gate "advisories"     tools/check-licenses.sh --advisories

  runlog_join || true
fi

runlog_summary gate
exit "$RUNLOG_FAILED"
