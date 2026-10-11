#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Selftest for tools/lib/run-logged.sh failure propagation.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
cleanup() {
  local status=$?
  rm -rf "$TMP"
  [ ! -e "$TMP" ] || exit 1
  exit "$status"
}
trap cleanup EXIT

run_case() {
  local name="$1" want="$2"
  shift 2
  if "$@" >"$TMP/$name.out" 2>&1; then actual=0; else actual=$?; fi
  [ "$actual" -eq "$want" ] || { cat "$TMP/$name.out" >&2; echo "FAIL [$name]: $actual" >&2; exit 1; }
}

run_case empty 1 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/empty"; runlog_init test; runlog_summary gate
' _ "$REPO" "$TMP"
run_case foreground 1 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/foreground"; run_logged fail false; runlog_summary gate
' _ "$REPO" "$TMP"
run_case background 1 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/background"; run_logged_bg fail false; runlog_summary gate
' _ "$REPO" "$TMP"
run_case missing-status 1 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/missing-status"; run_logged_bg slow sleep 0.1
  RUNLOG_BG_STATUS[0]="$2/does-not-exist"; runlog_summary gate
' _ "$REPO" "$TMP"
run_case tee-failure 1 bash -c '
  tee() { cat >/dev/null; return 1; }
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/tee"; RUNLOG_VERBOSE=1; run_logged pass true; runlog_summary gate
' _ "$REPO" "$TMP"
run_case tee-priority 7 bash -c '
  tee() { cat >/dev/null; return 1; }
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/tee-priority"; RUNLOG_VERBOSE=1
  run_logged fail sh -c "exit 7"; status=$?
  runlog_summary gate || true
  [ "$status" -eq 7 ] || exit 99
  exit "$status"
' _ "$REPO" "$TMP"
run_case quiet-tee-failure 1 bash -c '
  tee() { cat >/dev/null; return 1; }
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/quiet-tee"; run_logged pass true; runlog_summary gate
' _ "$REPO" "$TMP"
run_case malformed-status 1 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/malformed-status"; run_logged_bg slow sleep 0.1
  wait "${RUNLOG_BG_PIDS[0]}" || true
  printf "0\\n" >"${RUNLOG_BG_STATUS[0]}"; runlog_summary gate
' _ "$REPO" "$TMP"
run_case owner-write 2 bash -c '
  source "$1/tools/lib/run-logged.sh"
  runlog_write_owner() { return 1; }
  RUNLOG_ROOT="$2/owner-root"; RUNLOG_DIR=""; runlog_init owner
' _ "$REPO" "$TMP"
run_case progress-write 2 bash -c '
  source "$1/tools/lib/run-logged.sh"
  runlog_progress() { return 1; }
  RUNLOG_DIR="$2/progress"; run_logged pass true
' _ "$REPO" "$TMP"
run_case liveness-unknown 0 bash -c '
  source "$1/tools/lib/run-logged.sh"
  mkdir -p "$2/live"; printf "1 unknown\\n" >"$2/live/.owner"
  stat() { return 1; }
  runlog_dir_is_live "$2/live"
' _ "$REPO" "$TMP"
run_case liveness-old-live 0 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_ROOT="$2/old-live-root"; RUNLOG_DIR=""; runlog_init old-live
  python3 -c "import os, sys, time; os.utime(sys.argv[1], (time.time() - 172800,) * 2)" "$RUNLOG_DIR"
  runlog_dir_is_live "$RUNLOG_DIR"
' _ "$REPO" "$TMP"
run_case liveness-unreadable-owner 0 bash -c '
  source "$1/tools/lib/run-logged.sh"
  mkdir -p "$2/unreadable"; printf "1 1\\n" >"$2/unreadable/.owner"
  read() { return 1; }
  runlog_dir_is_live "$2/unreadable"
' _ "$REPO" "$TMP"
finished_status=0
[ -d /proc ] && finished_status=1
run_case liveness-known-finished "$finished_status" bash -c '
  source "$1/tools/lib/run-logged.sh"
  mkdir -p "$2/finished"; printf "999999 1\\n" >"$2/finished/.owner"
  runlog_dir_is_live "$2/finished"
' _ "$REPO" "$TMP"
run_case liveness-no-proc 0 bash -c '
  source "$1/tools/lib/run-logged.sh"
  function [ {
    if builtin test "${!#}" = "]"; then
      set -- "${@:1:$(( $# - 1 ))}"
    fi
    if builtin test "$#" -eq 2 && builtin test "$1" = -d && builtin test "$2" = /proc; then
      return 1
    fi
    builtin test "$@"
  }
  mkdir -p "$2/no-proc"; printf "999999 1\\n" >"$2/no-proc/.owner"
  runlog_dir_is_live "$2/no-proc"
' _ "$REPO" "$TMP"
run_case summary-closed-stdout 0 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/closed-summary"; run_logged pass true
  exec 3>&1 1>&-
  runlog_summary gate; status=$?
  failed=$RUNLOG_FAILED
  exec 1>&3 3>&-
  [ "$status" -eq 1 ] && [ "$failed" -eq 1 ]
' _ "$REPO" "$TMP"
run_case bounded-tail 0 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/tail"; RUNLOG_TAIL=2
  run_logged noisy sh -c "printf one\\\\ntwo\\\\nthree\\\\nfour\\\\n; exit 9" >"$2/tail.out" 2>&1 || status=$?
  [ "${status:-0}" -eq 9 ] || exit 1
  grep -q "three" "$2/tail.out" && grep -q "four" "$2/tail.out"
  ! grep -q "one" "$2/tail.out" && ! grep -q "two" "$2/tail.out"
' _ "$REPO" "$TMP"
run_case bg-command-priority 0 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR="$2/bg-priority"
  progress_calls=0
  runlog_progress() { progress_calls=$((progress_calls + 1)); [ "$progress_calls" -eq 1 ]; }
  recorded=""
  runlog_record() { recorded="$5"; RUNLOG_FAILED=1; }
  run_logged_bg fail sh -c "exit 7"; runlog_summary gate || true
  [ "$recorded" -eq 7 ]
' _ "$REPO" "$TMP"
run_case bad-init 2 bash -c '
  source "$1/tools/lib/run-logged.sh"
  RUNLOG_DIR=/dev/null; runlog_init test
' _ "$REPO" "$TMP"
run_case canonical-init 2 "$REPO/tools/check-all.sh" --static --log-dir /dev/null

echo "OK: logging selftest preserves failures and rejects empty or unwritable runs"
