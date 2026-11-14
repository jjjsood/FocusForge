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
#   --quick      a change's working check; the full run stays the build. The
#                nextest `quick` profile (.config/nextest.toml) instead of the
#                whole workspace, clippy only over the crates with a changed
#                path, and none of the stages that only the full run needs:
#                `build`, `check (no default features)`, `fixture e2e`,
#                `cargo test --doc`, `rustdoc`, `trace feature`, `advisories`.
#   --verbose    also stream every gate's output to the terminal. Unbounded:
#                a caller with a fixed output budget will lose the summary.
#   --tail N     lines of a failing gate's log to print inline (default 40,
#                0 prints none).
#   --log-dir D  write logs to D instead of .logs/gate/run-<stamp>/.
#
# Output contract: gate output goes to per-gate logs; stdout stays two
# lines per gate. `fixture e2e` runs the built binary in the background,
# cargo-free, after `build`.
# Cargo and direct binary runs go through tools/with-lock.sh build; clippy
# stays out of --static because it compiles; RUSTFLAGS is never set here.
# Every cargo stage runs with CARGO_BUILD_JOBS=min(24, cores) unless the caller
# sets it (tools/lib/cargo-jobs.sh holds the cap). A cold
# `cargo build --workspace --all-targets` at 24 jobs peaks at 13.0 GB.
# `manual book` (mdBook build plus link check) runs only when mdbook is on PATH;
# otherwise it prints a SKIP line.
#
# Changed paths are those since the merge base with develop (FF_GATE_BASE
# overrides the branch), committed or not. With --static and --quick the
# selftests of the tooling itself run only when tools/, .githooks/, .github/
# or .config/ changed; the full run always runs them. The full run builds,
# lints and tests the theme sheet (`theme sheet feature`) and checks its size
# budget only when a path under crates/ui*/ or crates/gui/, or the root
# Cargo.toml, Cargo.lock or rust-toolchain.toml, changed; it always does in CI
# and when no merge base resolves.
#
# Green-tree cache: a full or quick run that passes on a clean tree records the
# tree id in the common git dir, shared by every checkout. A later run on
# the same clean tree whose mode a record covers (full covers quick and static,
# quick covers static) prints `CACHED:` and exits 0 without running anything.
# A full run that skipped the theme sheet stage records `full-partial`, which
# covers quick and static but not a later full run.
# Off in CI and with FF_GATE_NO_CACHE=1.
#
# Exit code: 0 all gates passed, 1 at least one failed.
set -uo pipefail  # deliberately not -e: every gate runs, then the summary

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

ARGS=("$@")
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

# shellcheck source=lib/cargo-jobs.sh
. "$REPO/tools/lib/cargo-jobs.sh"
CARGO_BUILD_JOBS="$(cargo_jobs)"
export CARGO_BUILD_JOBS

# On a Linux desktop the test stages briefly saturate every core and push
# memory into swap, which freezes the session. A building run therefore
# re-executes itself in a transient systemd user scope: above MemoryHigh the
# kernel reclaims from the gate instead of from the desktop, and CPUWeight
# only matters under contention, so an idle machine still runs at full speed.
# A caller already inside a scope with a memory.high keeps that limit, since
# the new scope is a sibling and would otherwise escape it.
# FF_GATE_MEMORY_HIGH / FF_GATE_WEIGHT (CPU and IO) tune it; FF_GATE_SCOPE=0 or CI
# turns it off; without a reachable user manager the run goes on unscoped.
if [ "$STATIC_ONLY" -eq 0 ] && [ -z "${CI:-}" ] && [ "${FF_GATE_SCOPE:-1}" != 0 ] \
   && [ "$(uname -s)" = Linux ] && command -v systemd-run >/dev/null 2>&1 \
   && systemctl --user show -p Version >/dev/null 2>&1; then
  mem_high="${FF_GATE_MEMORY_HIGH:-85%}"
  outer="$(cat "/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/memory.high" 2>/dev/null)"
  [ -z "${FF_GATE_MEMORY_HIGH:-}" ] && [ -n "$outer" ] && [ "$outer" != max ] && mem_high="$outer"
  exec env FF_GATE_SCOPE=0 systemd-run --user --scope --quiet --collect --unit "check-all-$$-$RANDOM" \
    -p MemoryHigh="$mem_high" -p CPUWeight="${FF_GATE_WEIGHT:-50}" -p IOWeight="${FF_GATE_WEIGHT:-50}" \
    "$BASH" "$REPO/tools/check-all.sh" ${ARGS+"${ARGS[@]}"}
fi

[ -f Cargo.toml ] || {
  echo "SKIP check-all: no Cargo.toml"
  exit 0
}

MODE=full
[ "$QUICK" -eq 1 ] && MODE=quick
[ "$STATIC_ONLY" -eq 1 ] && MODE=static

GREEN_TREES=""
CLEAN_TREE=""
if [ -z "${CI:-}" ] && [ -z "${FF_GATE_NO_CACHE:-}" ] \
  && common="$(git rev-parse --git-common-dir 2>/dev/null)" \
  && [ -z "$(git status --porcelain 2>/dev/null)" ]; then
  GREEN_TREES="$(cd "$common" && pwd)/focusforge-green-trees"
  CLEAN_TREE="$(git rev-parse 'HEAD^{tree}')"
  covers='full'
  [ "$MODE" = full ] || covers='full|full-partial|quick'
  if hit="$(grep -E "^$CLEAN_TREE ($covers) " "$GREEN_TREES" 2>/dev/null | tail -n 1)" && [ -n "$hit" ]; then
    read -r _ hit_mode hit_at <<<"$hit"
    echo "CACHED: tree $CLEAN_TREE passed the $hit_mode gate at $hit_at; nothing ran (FF_GATE_NO_CACHE=1 forces a run)"
    exit 0
  fi
fi

# Paths changed since the merge base, committed or not, plus untracked ones;
# fails when no base resolves (a CI clone without develop).
changed_paths() {
  local base
  base="$(git merge-base HEAD "${FF_GATE_BASE:-develop}" 2>/dev/null)" || return 1
  { git diff --name-only "$base" --; git ls-files --others --exclude-standard; } | sort -u
}
CHANGED=ALL
[ -n "${CI:-}" ] || CHANGED="$(changed_paths)" || CHANGED=ALL

TOOLS_CHANGED=1
if [ "$MODE" != full ] && [ "$CHANGED" != ALL ] \
  && ! grep -qE '^(tools/|\.githooks/|\.github/|\.config/)' <<<"$CHANGED"; then
  TOOLS_CHANGED=0
fi

# 1 when the theme sheet stages run: a UI crate, the GUI or a root workspace input changed.
UI_CHANGED=0
if [ "$CHANGED" = ALL ] \
  || grep -qE '^(crates/(ui[^/]*|gui)/|Cargo\.(toml|lock)$|rust-toolchain\.toml$)' <<<"$CHANGED"; then
  UI_CHANGED=1
fi

RUNLOG_ROOT="$REPO/.logs/gate"
RUNLOG_DIR="$LOG_DIR"
RUNLOG_VERBOSE="$VERBOSE"
RUNLOG_TAIL="$TAIL_LINES"
# shellcheck source=lib/run-logged.sh
. "$REPO/tools/lib/run-logged.sh"
runlog_init run || exit $?

# A hosted runner that is lost takes its step logs with it. With FF_GATE_HEARTBEAT=<seconds> a
# resource line is printed on that interval, and the streamed job log keeps the last ones.
if [ -n "${FF_GATE_HEARTBEAT:-}" ]; then
  (
    while sleep "$FF_GATE_HEARTBEAT"; do
      echo "heartbeat $(date +%H:%M:%S)" \
        "$(free -m 2>/dev/null | awk '/^Mem/ {print "mem_used_mb=" $3 " mem_avail_mb=" $7}')" \
        "$(df -Pm "$REPO" | awk 'NR==2 {print "disk_avail_mb=" $4}')" \
        "target_mb=$(du -sm "$REPO/target" 2>/dev/null | cut -f1)" \
        "top_rss_kb=$(ps -eo rss=,comm= --sort=-rss 2>/dev/null | head -1)"
    done
  ) &
  heartbeat_pid=$!
  trap 'kill "$heartbeat_pid" 2>/dev/null' EXIT
fi

# A failing gate must not stop the run: every gate reports, then the summary.
run_gate() { run_logged "$@" || true; }

# A selftest of the tooling: skipped while the tooling is unchanged.
tool_selftest() {
  if [ "$TOOLS_CHANGED" -eq 1 ]; then
    run_gate "$@"
  else
    echo "SKIP $1: tooling unchanged since the merge base"
  fi
}

# The packages --quick lints: every crate under crates/ with a changed path, or
# the workspace when a workspace-wide input changed or no base resolves. Empty
# when no crate changed. gpu-tests stays on for the crates that have it.
quick_clippy_args() {
  CLIPPY_ARGS=()
  if [ "$CHANGED" = ALL ] || grep -qE '^(Cargo\.(toml|lock)|rust-toolchain\.toml|clippy\.toml|\.cargo/)' <<<"$CHANGED"; then
    local all_gpu_tests="focusforge-gpu/gpu-tests,focusforge-gui/gpu-tests,focusforge-cli/gpu-tests"
    CLIPPY_ARGS=(--workspace --features "$all_gpu_tests")
    return
  fi
  local dir name feats=()
  while read -r dir; do
    [ -f "crates/$dir/Cargo.toml" ] || continue
    name="$(awk -F'"' '/^name = /{print $2; exit}' "crates/$dir/Cargo.toml")"
    CLIPPY_ARGS+=(-p "$name")
    grep -q '^gpu-tests' "crates/$dir/Cargo.toml" && feats+=("$name/gpu-tests")
  done < <(sed -nE 's#^crates/([^/]+)/.*#\1#p' <<<"$CHANGED" | sort -u)
  [ "${#feats[@]}" -eq 0 ] || CLIPPY_ARGS+=(--features "$(IFS=,; echo "${feats[*]}")")
}

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

# The manual build, as book.yml runs it: any mdBook warning fails, then every
# relative link and anchor in the output is checked. Writes book/ (not committed).
manual_book() {
  local out
  out="$(mdbook build 2>&1)" || { echo "$out"; return 1; }
  echo "$out"
  if grep -qE '(WARN|ERROR)' <<<"$out"; then
    echo "mdBook reported warnings or errors" >&2
    return 1
  fi
  python3 -I tools/check-book-links.py book
}

# --- static checks: seconds each, no side effects ---------------------------
run_gate "rustfmt"          cargo fmt --all --check
run_gate "licenses"         tools/check-licenses.sh
run_gate "source headers"   tools/check-headers.sh
tool_selftest "source headers selftest" tools/check-headers-selftest.sh
# cargo tree/metadata --offline only: no build slot needed.
run_gate "NOTICE"           tools/check-notice.sh
tool_selftest "NOTICE selftest"  tools/check-notice-selftest.sh
tool_selftest "logging selftest" tools/check-logging-selftest.sh
tool_selftest "workflow selftest" python3 tools/check-workflow-selftest.py
tool_selftest "release assets selftest" python3 tools/check-release-assets-selftest.py
tool_selftest "release smoke selftest" tools/smoke-release-selftest.sh
run_gate "fixture manifest" tools/check-fixtures.sh
tool_selftest "fixture manifest selftest" tools/check-fixtures-selftest.sh
tool_selftest "lock directory selftest" tools/with-lock-selftest.sh
tool_selftest "cargo jobs selftest" tools/cargo-jobs-selftest.sh
tool_selftest "tiff-diff selftest" python3 tools/tiff-diff.py --selftest
tool_selftest "image-metrics selftest" python3 tools/image-metrics.py --selftest
tool_selftest "rawtiff selftest" python3 tools/lib/rawtiff.py --selftest
tool_selftest "download selftest" python3 tools/lib/download.py --selftest
tool_selftest "make-fixtures selftest" python3 tools/lib/make_fixtures.py --selftest
run_gate "test bodies"      python3 tools/check-test-bodies.py
tool_selftest "test bodies selftest" python3 tools/check-test-bodies.py --selftest
tool_selftest "generated UI size selftest" python3 tools/check-ui-size.py --selftest
tool_selftest "manual screenshot lists" python3 tools/manual-screenshots.py --selftest
run_gate "manual coverage" python3 tools/check-manual-coverage.py
tool_selftest "manual coverage selftest" python3 tools/check-manual-coverage.py --selftest

if [ "$STATIC_ONLY" -eq 0 ]; then
  # Type-checks the no-GPU path, test targets included, and writes no binary.
  # The UI crates are excluded: their Cargo.toml pins the wgpu-only Slint
  # backend (`renderer-femtovg-wgpu`) unconditionally, not behind the `gpu`
  # feature, so with them in this stage would still compile wgpu and prove
  # nothing about the no-GPU path.
  [ "$QUICK" -eq 1 ] || run_gate "check (no default features)" \
                             tools/with-lock.sh build -- \
                             cargo check --workspace --all-targets \
                             --no-default-features --exclude focusforge-gui \
                             --exclude 'focusforge-ui*'

  # Must come first among the GPU-enabled stages: it produces the binary the
  # background stage runs, and it holds the target/ lock the cargo gates below
  # queue on anyway.
  # --quick leaves this stage and the no-GPU check to the full run: nextest
  # builds what the tests need, and clippy type-checks the remaining targets.
  if [ "$QUICK" -eq 0 ]; then
    run_gate "build"          tools/with-lock.sh build -- cargo build --workspace --all-targets

    # The long background stage, started now and joined at the end. Not a
    # cargo command -- see the header.
    run_logged_bg "fixture e2e" tools/with-lock.sh build -- tools/fixture-e2e.sh
  fi

  # Cargo-free, so no lock. Linux only: several groups feed /bin/true to a
  # parser that accepts only x86-64 ELF, and shell out to bash. The two PowerShell groups stay in
  # the full run before package builds.
  if [ "$(uname -s)" = Linux ]; then
    if [ "$TOOLS_CHANGED" -eq 1 ]; then
      run_logged_bg "packaging selftest" python3 tools/check-packaging-selftest.py --no-pwsh
    else
      echo "SKIP packaging selftest: tooling unchanged since the merge base"
    fi
  fi

  # Foreground and BEFORE `cargo test`, never a background stage: `cargo
  # test` reads what this writes (.scratch/synth-align/measurement.json), so
  # a background stage joined at the end would race the test into skipping.
  run_gate "synthetic alignment truth" tools/synth-align.sh

  # --- remaining gates --------------------------------------------------
  # The gpu-tests parity test tree is linted here, never run.
  if [ "$QUICK" -eq 0 ]; then
    run_gate "clippy"         tools/with-lock.sh build -- cargo clippy --workspace --all-targets --features focusforge-gpu/gpu-tests,focusforge-gui/gpu-tests,focusforge-cli/gpu-tests -- -D warnings
  else
    quick_clippy_args
    if [ "${#CLIPPY_ARGS[@]}" -eq 0 ]; then
      echo "SKIP clippy: no crate changed since the merge base"
    else
      run_gate "clippy"       tools/with-lock.sh build -- cargo clippy "${CLIPPY_ARGS[@]}" --all-targets -- -D warnings
    fi
  fi

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
  [ "$QUICK" -eq 1 ] || run_gate "cargo test --doc" tools/with-lock.sh build -- \
                              env FF_REQUIRE_SYNTH_ALIGN="$REQUIRE_SYNTH" cargo test --doc --workspace
  # The off-by-default `trace` feature's own suite: the stage above resolves
  # no profiling crate at all, and the clippy stage compiles the feature
  # without running anything. ~13.5s on an already-compiled tree, most of it
  # one real stack run over fixtures/mantis-512.
  [ "$QUICK" -eq 1 ] || run_gate "trace feature"  tools/with-lock.sh build -- \
                              cargo test -p focusforge-cli --features trace --test trace_cli
  # The theme sheet is built out of the stages above; this one lints it and runs
  # its tests, when a UI path or workspace input changed.
  if [ "$QUICK" -eq 0 ]; then
    if [ "$UI_CHANGED" -eq 1 ]; then
      run_gate "theme sheet feature" tools/with-lock.sh build -- \
                              sh -c 'cargo clippy -p focusforge-gui --features theme-sheet --all-targets -- -D warnings \
                                && cargo nextest run -p focusforge-gui --features theme-sheet --test theme'
    else
      echo "SKIP theme sheet feature: no UI path or workspace input changed since the merge base"
    fi
  fi
  # Asks cargo for the builds the stages above made and compares the generated Slint files
  # with their budgets. Linux only: the sizes come from the .slint sources, not the OS.
  if [ "$(uname -s)" = Linux ]; then
    UI_SIZE_BUILDS="--build tests"
    [ "$QUICK" -eq 0 ] && [ "$UI_CHANGED" -eq 1 ] && UI_SIZE_BUILDS="--build tests --build theme-sheet"
    # shellcheck disable=SC2086
    run_gate "generated UI size" tools/with-lock.sh build -- python3 tools/check-ui-size.py $UI_SIZE_BUILDS
  fi
  [ "$QUICK" -eq 1 ] || run_gate "rustdoc"        tools/with-lock.sh build -- \
                              env RUSTDOCFLAGS="-D warnings" \
                              cargo doc --workspace --no-deps --document-private-items
  # Separate from `licenses` on purpose: an advisory is a fact about the world
  # that can turn red without this repository changing, so it must not be able
  # to block a commit through the pre-commit hook.
  [ "$QUICK" -eq 1 ] || run_gate "advisories"     tools/check-licenses.sh --advisories

  # Needs the mdbook pinned in .github/workflows/book.yml; without it the book
  # job in CI is the only check.
  if command -v mdbook >/dev/null 2>&1; then
    run_gate "manual book"  manual_book
  else
    echo "SKIP manual book: mdbook not on PATH (see MDBOOK_VERSION in .github/workflows/book.yml)"
  fi

  runlog_join || true
fi

runlog_summary gate

if [ "$RUNLOG_FAILED" -eq 0 ] && [ -n "$CLEAN_TREE" ] && [ "$MODE" != static ] \
  && [ -z "$(git status --porcelain)" ] && [ "$(git rev-parse 'HEAD^{tree}')" = "$CLEAN_TREE" ]; then
  rec_mode="$MODE"
  [ "$MODE" = full ] && [ "$UI_CHANGED" -eq 0 ] && rec_mode=full-partial
  echo "$CLEAN_TREE $rec_mode $(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$GREEN_TREES"
fi
exit "$RUNLOG_FAILED"
