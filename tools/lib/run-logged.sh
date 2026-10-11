#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Bounded stdout for any tool that drives a noisy subprocess. Sourced, never
# executed -- tools/run-logged.sh is the command-line front end.
#
#   . "$(dirname "$0")/lib/run-logged.sh"
#   runlog_init                       # optional: names the run directory
#   run_logged "build" cargo build --workspace --all-targets
#   runlog_summary gate               # optional: table + exit status
#
# Output contract: a subprocess's full stdout+stderr goes to its own file and
# the terminal gets two lines -- `>>> <name>` before it starts, one PASS/FAIL
# line with duration, line count and log path after. A failure additionally
# prints the tail of its log, because a status line nobody can act on is worse
# than no line at all. Nothing is discarded, only relocated: the log path is on
# every row. This exists so a terminal keeps the summary instead of losing it
# to a passing suite's chatter.
#
# API
#   runlog_init [name]          Create the run directory. Called implicitly by
#                               the first run_logged; call it directly only to
#                               name the directory (default: the caller script).
#   run_logged [--tail-ok N] [--tail N] <name> <cmd> [args...]
#                               Run one command. Returns the command's own exit
#                               code, so a `set -e` caller still fails fast --
#                               after the status line has been printed.
#                               --tail-ok prints the last N lines on SUCCESS too
#                               (for a command whose last lines are the result).
#   run_logged_bg [--tail N] <name> <cmd> [args...]
#                               Same, started in the background. Prints its
#                               `>>>` line now and its PASS/FAIL line at
#                               runlog_join. For a long gate that nothing else
#                               depends on, so shorter gates run beside it.
#   runlog_join                 Wait for every run_logged_bg, print their status
#                               lines, and fold them into the summary. Returns 1
#                               if any of them failed. Safe to call with none
#                               outstanding.
#   runlog_summary [noun]       Print the collected table, the log directory and
#                               a verdict. Returns 1 if anything failed. Rows are
#                               ordered by when each command STARTED, so a
#                               background gate keeps its place in the table
#                               rather than jumping to the end.
#
# Environment (a script may also set the RUNLOG_* variables directly before
# sourcing, which is how tools/check-all.sh maps its own flags onto these):
#   FF_TOOL_VERBOSE=1   stream every command's output as well as logging it.
#                        Unbounded -- a caller with an output budget loses the
#                        summary, which is the whole thing this prevents.
#   FF_TOOL_TAIL=N      lines of a failing log printed inline (default 40, 0 none)
#   FF_TOOL_LOG_DIR=D   write into D instead of a fresh run directory (no pruning)
#   FF_TOOL_LOG_ROOT=D  parent of the run directories (default .logs/tool)
#   FF_TOOL_KEEP=N      finished run directories kept under the root (default 10)

# Sourcing this twice (a script that sources it and calls another that does)
# must not reset the arrays mid-run.
[ -n "${RUNLOG_SOURCED:-}" ] && return 0
RUNLOG_SOURCED=1

RUNLOG_REPO="${RUNLOG_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
RUNLOG_ROOT="${RUNLOG_ROOT:-${FF_TOOL_LOG_ROOT:-$RUNLOG_REPO/.logs/tool}}"
RUNLOG_DIR="${RUNLOG_DIR:-${FF_TOOL_LOG_DIR:-}}"
RUNLOG_VERBOSE="${RUNLOG_VERBOSE:-${FF_TOOL_VERBOSE:-0}}"
RUNLOG_TAIL="${RUNLOG_TAIL:-${FF_TOOL_TAIL:-40}}"
RUNLOG_KEEP="${RUNLOG_KEEP:-${FF_TOOL_KEEP:-10}}"

RUNLOG_NAMES=()
RUNLOG_STATUSES=()
RUNLOG_LOGS=()
RUNLOG_SECS=()
RUNLOG_ORDER=()
RUNLOG_FAILED=0
RUNLOG_COUNT=0

# Outstanding run_logged_bg jobs, cleared by runlog_join.
RUNLOG_BG_NAMES=()
RUNLOG_BG_LOGS=()
RUNLOG_BG_PIDS=()
RUNLOG_BG_STATUS=()
RUNLOG_BG_ORDER=()
RUNLOG_BG_TAILS=()

runlog_slug() {
  printf '%s' "$1" | tr ' /' '--' | tr -cd 'a-zA-Z0-9_-'
}

# Field 22 of /proc/<pid>/stat -- the process's start time in clock ticks
# since boot, which cannot be reused the way a pid can. Empty if the process
# is gone. `comm` (field 2) can itself contain spaces and parentheses, so
# split on the LAST ") " rather than the first, then read into an array
# (never `set -- $rest`, which would glob and clobber positionals).
runlog_proc_starttime() {
  local pid="$1" stat rest
  stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || return 0
  rest="${stat##*) }"
  local f
  read -r -a f <<<"$rest"
  printf '%s' "${f[19]:-}"
}

# Record this run's identity so a later prune pass -- this process's own or
# another one's, into the same root -- can tell the directory is still being
# written to. Only called on the prune=1 branch: a caller-named
# FF_TOOL_LOG_DIR/RUNLOG_DIR is not this run's to mark, and is instead
# protected by the explicit self-skip in the prune loop below. A dot-file in
# a run directory is already precedent (.status-NN); nothing globs `.*`.
runlog_write_owner() {
  local starttime
  starttime="$(runlog_proc_starttime "$$")"
  printf '%s %s\n' "$$" "${starttime:-unknown}" >"$RUNLOG_DIR/.owner"
}

runlog_dir_mtime() {  # runlog_dir_mtime <directory>
  stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null
}

# A candidate directory is finished only when its absent owner file or a
# readable Linux process identity proves that. Missing metadata, an unreadable
# owner, or a host without /proc stays live: pruning an unknown active run is
# worse than retaining an old directory.
runlog_dir_is_live() {
  local dir="$1" pid starttime mtime current
  [ -f "$dir/.owner" ] || return 1
  read -r pid starttime <"$dir/.owner" 2>/dev/null || return 0
  [ -n "$pid" ] && [ -n "$starttime" ] || return 0

  # GNU and BSD stat spell this differently. If neither works, retain the
  # candidate: pruning an unknown live run is worse than leaving one behind.
  mtime="$(runlog_dir_mtime "$dir")" || return 0

  # /proc unreadable at all means there is no way to prove the run finished.
  [ -d /proc ] || return 0
  [ "$starttime" = unknown ] && return 0

  [ -d "/proc/$pid" ] || return 1
  current="$(runlog_proc_starttime "$pid")"
  [ -n "$current" ] || return 0
  [ "$current" = "$starttime" ] && return 0
  return 1
}

# One append-only line per gate transition, for anything watching a run from
# outside it.
#
# Without this a watcher has to infer the run's state from the artefacts:
# count log files (one appears when a gate starts, so "done" is a floor and
# never exact), notice that a .status-NN is missing to tell a background stage
# is still going, and subtract log mtimes to recover how long each gate took.
# All three are guesses about this library's internals from the outside. The
# facts are free here, so they are written down instead.
#
#   start<TAB><order><TAB><epoch><TAB>fg|bg<TAB><name>
#   end<TAB><order><TAB><epoch><TAB><status>
#
# Append-only and one small line at a time, so a reader never sees a partial
# record and the background stage's own subshell can write into the same file
# as the parent. A watcher that finds no .progress at all is expected to fall
# back to its own devices or say nothing -- this file is a convenience for
# observers, never something a gate's own result depends on.
runlog_progress() {  # runlog_progress <record...>
  [ -n "${RUNLOG_DIR:-}" ] || return 0
  local line
  printf -v line '%s\t' "$@"
  printf '%s\n' "${line%$'\t'}" >>"$RUNLOG_DIR/.progress" 2>/dev/null
}

runlog_init() {
  [ -n "${RUNLOG_DIR_READY:-}" ] && return 0
  local name="${1:-$(runlog_slug "$(basename "${0%.sh}")")}"
  local prune=0
  if [ -z "$RUNLOG_DIR" ]; then
    RUNLOG_DIR="$RUNLOG_ROOT/$name-$(date +%Y%m%d-%H%M%S)-$$"
    prune=1
  fi
  [ -n "${RUNLOG_NO_PRUNE:-}" ] && prune=0
  mkdir -p "$RUNLOG_DIR" || {
    echo "run-logged: cannot create log dir: $RUNLOG_DIR" >&2
    return 2
  }
  # Keep the last few FINISHED runs only. Bounded by construction: the glob is
  # this library's own root directory, never an argument. A directory that is
  # still being written to -- this process's own, or another one's sharing
  # the root -- is never a deletion candidate and never counts against
  # RUNLOG_KEEP; only a directory that has actually finished does.
  if [ "$prune" -eq 1 ]; then
    if ! runlog_write_owner; then
      echo "run-logged: cannot write log owner: $RUNLOG_DIR/.owner" >&2
      return 2
    fi
    local old kept=0
    while read -r old; do
      [ -d "$old" ] || continue
      if [ "$old" != "$RUNLOG_DIR" ] && runlog_dir_is_live "$old"; then
        continue
      fi
      kept=$((kept + 1))
      [ "$kept" -gt "$RUNLOG_KEEP" ] || continue
      # Never delete the directory of the process doing the pruning, even if
      # something above went wrong -- explicit, not merely implied by the
      # owner file just written.
      [ "$old" = "$RUNLOG_DIR" ] && continue
      case "$old" in
        "$RUNLOG_ROOT"/*) rm -rf -- "$old" ;;
      esac
    done < <(ls -1dt "$RUNLOG_ROOT"/*-* 2>/dev/null)
  fi
  RUNLOG_DIR_READY=1
}

# Append one finished command to the table and print its status line. Shared by
# the foreground and background paths so a row looks the same either way.
runlog_record() {
  local order="$1" name="$2" log="$3" elapsed="$4" status="$5" tail_fail="$6" tail_ok="${7:-0}"
  local lines
  lines="$(wc -l <"$log" 2>/dev/null | tr -d ' ')"

  RUNLOG_ORDER+=("$order")
  RUNLOG_NAMES+=("$name")
  RUNLOG_LOGS+=("$log")
  RUNLOG_SECS+=("$elapsed")
  if [ "$status" -eq 0 ]; then
    RUNLOG_STATUSES+=("PASS")
    printf 'PASS %s (%ss, %s lines) %s\n' "$name" "$elapsed" "${lines:-0}" "${log#"$RUNLOG_REPO"/}"
    if [ "$RUNLOG_VERBOSE" -eq 0 ] && [ "$tail_ok" -gt 0 ]; then
      tail -n "$tail_ok" "$log"
    fi
  else
    RUNLOG_STATUSES+=("FAIL")
    RUNLOG_FAILED=1
    printf 'FAIL %s (exit %s, %ss, %s lines) %s\n' \
      "$name" "$status" "$elapsed" "${lines:-0}" "${log#"$RUNLOG_REPO"/}"
    if [ "$RUNLOG_VERBOSE" -eq 0 ] && [ "$tail_fail" -gt 0 ]; then
      echo "--- last $tail_fail lines ---"
      tail -n "$tail_fail" "$log"
      echo "--- end $name ---"
    fi
  fi
}

run_logged() {
  local tail_ok=0 tail_fail="$RUNLOG_TAIL"
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --tail-ok) tail_ok="$2"; shift 2 ;;
      --tail) tail_fail="$2"; shift 2 ;;
      *) break ;;
    esac
  done
  [ "$#" -ge 2 ] || { echo "run_logged: need a name and a command" >&2; return 2; }

  runlog_init || return 2
  local name="$1"; shift
  RUNLOG_COUNT=$((RUNLOG_COUNT + 1))
  local order="$RUNLOG_COUNT" log
  log="$(printf '%s/%02d-%s.log' "$RUNLOG_DIR" "$RUNLOG_COUNT" "$(runlog_slug "$name")")"

  # pipefail decides whether the verbose path reads the command's status or
  # tee's; restore whatever the caller had, since it is a shell-wide setting.
  local had_pipefail=0
  case "$SHELLOPTS" in *:pipefail:*|pipefail:*|*:pipefail) had_pipefail=1 ;; esac
  set -o pipefail

  local t0=$SECONDS status=0
  if ! runlog_progress start "$order" "$(date +%s)" fg "$name"; then
    echo "run-logged: cannot write progress for $name" >&2
    RUNLOG_FAILED=1
    return 2
  fi
  # The log path goes out BEFORE the command runs as well as after it: a long
  # quiet run is followable with `tail -f` only if its path is already on screen.
  if [ "$RUNLOG_VERBOSE" -eq 1 ]; then
    echo ">>> $name"
  else
    echo ">>> $name (log: ${log#"$RUNLOG_REPO"/})"
  fi
  if [ "$RUNLOG_VERBOSE" -eq 1 ]; then
    local command_status tee_status
    if "$@" 2>&1 | tee "$log"; then
      command_status="${PIPESTATUS[0]}" tee_status="${PIPESTATUS[1]}"
    else
      command_status="${PIPESTATUS[0]}" tee_status="${PIPESTATUS[1]}"
    fi
    if [ "$command_status" -ne 0 ]; then
      status="$command_status"
    else
      status="$tee_status"
    fi
  else
    local command_status tee_status
    if "$@" 2>&1 | tee "$log" >/dev/null; then
      command_status="${PIPESTATUS[0]}" tee_status="${PIPESTATUS[1]}"
    else
      command_status="${PIPESTATUS[0]}" tee_status="${PIPESTATUS[1]}"
    fi
    if [ "$command_status" -ne 0 ]; then
      status="$command_status"
    else
      status="$tee_status"
    fi
  fi
  [ "$had_pipefail" -eq 1 ] || set +o pipefail

  if ! runlog_progress end "$order" "$(date +%s)" "$status"; then
    echo "run-logged: cannot write completion progress for $name" >&2
    [ "$status" -ne 0 ] || status=2
  fi
  runlog_record "$order" "$name" "$log" "$((SECONDS - t0))" "$status" "$tail_fail" "$tail_ok"
  return "$status"
}

# A gate nothing else waits on, started now and reported at runlog_join, so
# shorter gates run beside it instead of after it.
#
# The subshell cannot hand its result back through the RUNLOG_* arrays -- a fork
# copies them -- so it writes `<status> <elapsed>` to a status file and the
# parent, which still owns the table, reads it at join. The log path and the
# table position are allocated HERE, before the fork, which is what keeps the
# summary ordered by start time rather than by who finished first.
run_logged_bg() {
  local tail_fail="$RUNLOG_TAIL"
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --tail) tail_fail="$2"; shift 2 ;;
      *) break ;;
    esac
  done
  [ "$#" -ge 2 ] || { echo "run_logged_bg: need a name and a command" >&2; return 2; }

  runlog_init || return 2
  local name="$1"; shift
  RUNLOG_COUNT=$((RUNLOG_COUNT + 1))
  local order="$RUNLOG_COUNT" log status_file
  log="$(printf '%s/%02d-%s.log' "$RUNLOG_DIR" "$RUNLOG_COUNT" "$(runlog_slug "$name")")"
  status_file="$(printf '%s/.status-%02d' "$RUNLOG_DIR" "$RUNLOG_COUNT")"
  rm -f "$status_file"

  if ! runlog_progress start "$order" "$(date +%s)" bg "$name"; then
    echo "run-logged: cannot write progress for $name" >&2
    RUNLOG_FAILED=1
    return 2
  fi
  echo ">>> $name (background, log: ${log#"$RUNLOG_REPO"/})"
  # Always to the file, never tee'd: two streams at once is unreadable,
  # and the summary is the thing FF_TOOL_VERBOSE must not cost the caller.
  (
    t0=$SECONDS
    if "$@" 2>&1 | tee "$log" >/dev/null; then
      command_status="${PIPESTATUS[0]}" tee_status="${PIPESTATUS[1]}"
    else
      command_status="${PIPESTATUS[0]}" tee_status="${PIPESTATUS[1]}"
    fi
    if [ "$command_status" -ne 0 ]; then status="$command_status"; else status="$tee_status"; fi
    printf '%s %s\n' "$status" "$((SECONDS - t0))" >"$status_file" || exit 2
    runlog_progress end "$order" "$(date +%s)" "$status" || exit 2
    exit "$status"
  ) &

  RUNLOG_BG_ORDER+=("$order")
  RUNLOG_BG_NAMES+=("$name")
  RUNLOG_BG_LOGS+=("$log")
  RUNLOG_BG_PIDS+=("$!")
  RUNLOG_BG_STATUS+=("$status_file")
  RUNLOG_BG_TAILS+=("$tail_fail")
}

runlog_join() {
  local i status elapsed wait_status
  for i in "${!RUNLOG_BG_PIDS[@]}"; do
    if wait "${RUNLOG_BG_PIDS[$i]}" 2>/dev/null; then wait_status=0; else wait_status=$?; fi
    # The status file, not `wait`'s code: the subshell's own exit status is the
    # printf's. A missing file means the subshell died before writing one (a
    # kill, an out-of-space), which must read as a failure, not as a pass.
    if [ -r "${RUNLOG_BG_STATUS[$i]}" ] &&
       read -r status elapsed <"${RUNLOG_BG_STATUS[$i]}" &&
       [[ "$status" =~ ^[0-9]+$ ]] && [[ "$elapsed" =~ ^[0-9]+$ ]]; then
      if [ "$wait_status" -ne 0 ] && [ "$status" -eq 0 ]; then
        echo "run-logged: ${RUNLOG_BG_NAMES[$i]} ended with inconsistent status -- treating as failed" >&2
        status="$wait_status"
      fi
    else
      status=127
      elapsed=0
      echo "run-logged: ${RUNLOG_BG_NAMES[$i]} left an invalid status file -- treating as failed" >&2
    fi
    runlog_record "${RUNLOG_BG_ORDER[$i]}" "${RUNLOG_BG_NAMES[$i]}" "${RUNLOG_BG_LOGS[$i]}" \
      "$elapsed" "$status" "${RUNLOG_BG_TAILS[$i]}"
  done
  RUNLOG_BG_ORDER=()
  RUNLOG_BG_NAMES=()
  RUNLOG_BG_LOGS=()
  RUNLOG_BG_PIDS=()
  RUNLOG_BG_STATUS=()
  RUNLOG_BG_TAILS=()
  return "$RUNLOG_FAILED"
}

runlog_summary() {
  local noun="${1:-step}"
  # A caller that forgot to join must not lose a gate. A no-op once joined, and
  # NOT redirected: a background failure's tail is the actionable part.
  runlog_join || true
  if [ "$RUNLOG_COUNT" -eq 0 ]; then
    echo "run-logged: no ${noun}s were recorded" >&2
    RUNLOG_FAILED=1
  fi
  local output indexes verdict
  if ! indexes="$(for i in "${!RUNLOG_NAMES[@]}"; do printf '%s %s\n' "${RUNLOG_ORDER[$i]}" "$i"; done | sort -n -k1,1 | cut -d' ' -f2)"; then
    echo "run-logged: cannot order summary" >&2
    RUNLOG_FAILED=1
    indexes=""
  fi
  output=$'\n=== summary ===\n'
  # By start order, not by array order: a background gate finishes after the
  # foreground ones that were started later, and a table that reordered itself
  # run to run cannot be diffed against a previous run's.
  local i line
  while read -r i; do
    [ -n "$i" ] || continue
    printf -v line '%-4s %-22s %4ss  %s\n' \
      "${RUNLOG_STATUSES[$i]}" "${RUNLOG_NAMES[$i]}" "${RUNLOG_SECS[$i]}" \
      "${RUNLOG_LOGS[$i]#"$RUNLOG_REPO"/}"
    output+="$line"
  done <<<"$indexes"
  output+="logs: ${RUNLOG_DIR#"$RUNLOG_REPO"/}"$'\n'
  if [ "$RUNLOG_FAILED" -eq 0 ]; then
    output+="all ${noun}s passed"$'\n'
  else
    output+="at least one $noun failed"$'\n'
  fi
  verdict="$RUNLOG_FAILED"
  if ! printf '%s' "$output"; then
    RUNLOG_FAILED=1
    return 1
  fi
  return "$verdict"
}
