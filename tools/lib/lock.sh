#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Cross-process mutex for target/ and the GPU, shared by every checkout
# whose `target` points into the same cargo-target pool:
#
#   build  target/ and the binaries under it. Cargo already takes its own
#          exclusive lock on target/ during compilation, so two `cargo build`
#          calls from different checkouts already queue on each other. What
#          cargo's lock does NOT cover is a process invoking the already-built
#          binary directly (`target/debug/focusforge-cli ...`, the sanctioned
#          way to drive the CLI without re-linking) while another checkout's
#          `cargo build`/`clippy`/`test` relinks that exact file underneath
#          it -- the stale-binary race ("unexpected argument" from a binary
#          that was mid-overwrite, not a real code bug).
#   gpu    the one physical GPU. Cargo's target/ lock is released once
#          compilation finishes; the actual test/binary EXECUTION that follows
#          runs unlocked, so two checkouts each driving a GPU-touching run
#          (`cargo test --features gpu-tests`, `focusforge-cli stack|align|fuse`,
#          a GUI launch) at the same time compete for the same VRAM and can
#          OOM-kill each other.
#
# Usage (sourced):
#   . tools/lib/lock.sh
#   with_lock build -- cargo build --workspace --all-targets
#   with_lock gpu   -- cargo test --features gpu-tests -- --test-threads=1
#
# tools/with-lock.sh is the command-line front end for a raw shell command
# that isn't already going through a script that sources this.
#
# Never wrap a script that already takes a lock itself (check-all.sh takes
# the build lock per gate) in with-lock.sh again from the outside -- the
# outer call holds the lock for the whole run, so the inner call asks for a
# lock its own ancestor holds. The held-resource marker refuses that shape;
# FF_LOCK_TIMEOUT (default 1800s) bounds other waits.
#
# One lock per resource; a command that compiles and executes holds it for both.

# The lock directory has to be the ONE physical location every checkout's
# `target` symlink points into. Walk up from the resolved link to the
# `cargo-target` directory itself (not into a pool slot below it), then use
# its containing Git checkout when present. Non-Git pools keep `.locks` next
# to the pool.
_lock_dir() {
  if [ -L target ]; then
    local p pool repo
    p="$(cd -P target && pwd)"
    for _ in 1 2 3; do
      [ "$(basename "$p")" = "cargo-target" ] && break
      p="$(dirname "$p")"
    done
    pool="$(dirname "$p")"
    repo="$(
      env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE \
        git -C "$pool" rev-parse --show-toplevel 2>/dev/null
    )" || repo=""
    echo "${repo:-$pool}/.locks"
  else
    # No shared-target symlink -- most likely a plain checkout. A local
    # .locks/ still makes the function safe to call, it just isn't shared
    # with anyone.
    echo "$(pwd -P)/.locks"
  fi
}

FF_LOCK_TIMEOUT="${FF_LOCK_TIMEOUT:-1800}"

_lock_native_path() {  # _lock_native_path <path>
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

_lock_python() {
  if command -v python3 >/dev/null 2>&1; then
    command -v python3
  elif command -v python >/dev/null 2>&1; then
    command -v python
  else
    return 1
  fi
}

_lock_ppid() {  # _lock_ppid <pid>
  local stat rest ppid fields
  if [ -r "/proc/$1/stat" ]; then
    stat="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
    rest="${stat##*) }"
    read -r -a fields <<<"$rest"
    printf '%s' "${fields[1]:-}"
  else
    ppid="$(ps -o ppid= -p "$1" 2>/dev/null)" || return 1
    printf '%s' "${ppid//[[:space:]]/}"
  fi
}

_lock_is_ancestor() {  # _lock_is_ancestor <candidate-pid>
  local target="$1" pid="$$" hop
  for ((hop = 0; hop < 64; hop++)); do
    [ "$pid" = "$target" ] && return 0
    [ "$pid" -le 1 ] && return 1
    pid="$(_lock_ppid "$pid")" || return 1
    [ -n "$pid" ] || return 1
  done
  return 1
}

_lock_is_held() {  # _lock_is_held <file>
  local held
  while IFS= read -r held; do
    # A native Python between two MSYS shells hands the variable on as a Windows path.
    if [ -n "$held" ] && command -v cygpath >/dev/null 2>&1; then held="$(cygpath -u "$held")"; fi
    [ -n "$held" ] && [ "$held" = "$1" ] && return 0
  done <<<"${FF_LOCK_HELD:-}"
  return 1
}

_lock_held_with() {  # _lock_held_with <file>
  if [ -n "${FF_LOCK_HELD:-}" ]; then
    printf '%s\n%s' "$FF_LOCK_HELD" "$1"
  else
    printf '%s' "$1"
  fi
}

_lock_owner_is_ancestor() {  # _lock_owner_is_ancestor <file>
  local holder ignored
  holder=""
  read -r holder ignored 2>/dev/null <"$1.owner" || [ -n "$holder" ] || return 1
  [ -n "$holder" ] && _lock_is_ancestor "$holder"
}

_with_flock() {  # _with_flock <resource> <file> <command...>
  local name="$1" f="$2" owner="$2.owner" fd status holder ignored held
  shift 2
  exec {fd}>"$f"
  if ! flock -n "$fd"; then
    holder=""
    read -r holder ignored 2>/dev/null <"$owner" || [ -n "$holder" ] || holder=""
    if [ -n "$holder" ] && _lock_is_ancestor "$holder"; then
      echo "with_lock: refusing to wait for the $name lock ($f) -- held by an ancestor of this process" >&2
      exec {fd}>&-
      return 3
    fi
    echo ">>> waiting for the $name lock ($f) -- another process holds it" >&2
    if ! flock -w "$FF_LOCK_TIMEOUT" "$fd"; then
      echo "with_lock: timed out after ${FF_LOCK_TIMEOUT}s waiting for the $name lock ($f)" >&2
      exec {fd}>&-
      return 4
    fi
  fi
  printf '%s\n' "$$" >"$owner"
  held="$(_lock_held_with "$f")"
  FF_LOCK_HELD="$held" "$@"
  status=$?
  rm -f "$owner"
  exec {fd}>&-
  return "$status"
}

# with_lock <build|gpu> [--] <command> [args...]
# Blocks until the named lock is free (printing one line to stderr if it had
# to wait, so a long silent hang reads as contention, not a hang), then runs
# the command while holding it. Exit code is the command's own, except 2 on a
# usage error, 3 refusing a self-deadlock, and 4 on an FF_LOCK_TIMEOUT wait
# that never got the lock.
#
with_lock() {
  local name="$1"
  shift
  case "$name" in
    build|gpu) ;;
    *)
      echo "with_lock: unknown lock '$name' (want build or gpu)" >&2
      return 2
      ;;
  esac
  [ "${1:-}" = "--" ] && shift
  [ "$#" -ge 1 ] || { echo "with_lock: need a command" >&2; return 2; }

  local dir
  dir="$(_lock_dir)"
  mkdir -p "$dir"
  dir="$(cd -P "$dir" && pwd)"
  local f="$dir/$name.lock" backend="${FF_LOCK_BACKEND:-auto}"
  case "$backend" in
    auto)
      if command -v flock >/dev/null 2>&1 && [ "${BASH_VERSINFO[0]:-0}" -ge 4 ]; then
        backend=flock
      else
        backend=native
      fi
      ;;
    flock|native) ;;
    *) echo "with_lock: unknown backend '$backend'" >&2; return 2 ;;
  esac
  if _lock_owner_is_ancestor "$f"; then
    echo "with_lock: refusing to wait for the $name lock ($f) -- held by an ancestor of this process" >&2
    return 3
  fi
  if _lock_is_held "$f"; then
    echo "with_lock: refusing to wait for the $name lock ($f) -- held by an ancestor of this process" >&2
    return 3
  fi
  if [ "$backend" = flock ]; then
    _with_flock "$name" "$f" "$@"
  else
    local python owner status cleanup_status token
    python="$(_lock_python)" || { echo "with_lock: needs Python 3 for native locks" >&2; return 2; }
    owner="$f.owner"
    token="$$:${RANDOM:-0}:${RANDOM:-0}"
    if FF_LOCK_HELD="$(_lock_held_with "$f")" FF_LOCK_OWNER="$(_lock_native_path "$owner")" FF_LOCK_OWNER_TOKEN="$token" FF_LOCK_SHELL="$(_lock_native_path "${BASH:-bash}")" \
      "$python" "$(dirname "${BASH_SOURCE[0]}")/native-lock.py" \
      "$(_lock_native_path "$f")" "$FF_LOCK_TIMEOUT" -- "$@"; then
      status=0
    else
      status=$?
    fi
    if "$python" "$(dirname "${BASH_SOURCE[0]}")/native-lock.py" --cleanup \
      "$(_lock_native_path "$f")" 0 "$(_lock_native_path "$owner")" "$token" >/dev/null 2>&1; then
      cleanup_status=0
    else
      cleanup_status=$?
    fi
    if [ "$cleanup_status" -ne 0 ] && [ "$cleanup_status" -ne 4 ] && [ "$status" -eq 0 ]; then
      return "$cleanup_status"
    fi
    return "$status"
  fi
}
