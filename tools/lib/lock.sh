#!/usr/bin/env bash
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
# outer call holds the lock for the whole run, so the inner call is asking
# for a lock its own ancestor is holding and can never get it. `with_lock`
# recognises this shape (an owner pid recorded against the held lock that is
# an ancestor of the process asking for it) and refuses immediately instead
# of hanging; FF_LOCK_TIMEOUT (default 1800s) is the backstop for every other
# way a lock can go unreleased -- a crashed or killed process that never got
# to run its own cleanup.
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
    p="$(readlink -f target)"
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
    echo "$(pwd)/.locks"
  fi
}

FF_LOCK_TIMEOUT="${FF_LOCK_TIMEOUT:-1800}"

# Field 4 of /proc/<pid>/stat is its parent pid. `comm` (field 2) can itself
# contain spaces and parens, so split on the LAST ") " rather than the
# first -- same parsing run-logged.sh already does for its own /proc reads.
_lock_ppid() {  # _lock_ppid <pid>
  local stat rest
  stat="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
  rest="${stat##*) }"
  local f
  read -r -a f <<<"$rest"
  printf '%s' "${f[1]:-}"
}

# True if <candidate> is this process or an ancestor of it, walked through
# /proc. Bounded at 64 hops so a /proc read glitch (a pid recycled mid-walk)
# can never spin -- the real process tree here is a handful of shells deep.
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

# with_lock <build|gpu> [--] <command> [args...]
# Blocks until the named lock is free (printing one line to stderr if it had
# to wait, so a long silent hang reads as contention, not a hang), then runs
# the command while holding it. Exit code is the command's own, except 2 on a
# usage error, 3 refusing a self-deadlock, and 4 on an FF_LOCK_TIMEOUT wait
# that never got the lock.
#
# Lock ownership uses a real file descriptor (`exec {fd}>"$f"`, `flock` given
# that fd directly) rather than the external `flock <file> <command>` form,
# so acquiring the lock and running the command are two separate steps: the
# lock's own outcome is read before anything of the wrapped command's exit
# status exists to collide with it.
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
  local f="$dir/$name.lock" owner="$dir/$name.lock.owner"

  local fd
  exec {fd}>"$f"

  if ! flock -n "$fd"; then
    local holder
    holder="$(cat "$owner" 2>/dev/null)" || holder=""
    if [ -n "$holder" ] && _lock_is_ancestor "$holder"; then
      echo "with_lock: refusing to wait for the $name lock ($f) -- held by" \
           "pid $holder, an ancestor of this process. Do not wrap a script" \
           "that takes this lock itself in with-lock.sh again from the" \
           "outside." >&2
      exec {fd}>&-
      return 3
    fi
    echo ">>> waiting for the $name lock ($f) -- another process holds it" >&2
    if ! flock -w "$FF_LOCK_TIMEOUT" "$fd"; then
      echo "with_lock: timed out after ${FF_LOCK_TIMEOUT}s waiting for the" \
           "$name lock ($f). Another process likely holds it and is" \
           "wedged -- 'fuser $f' finds it before removing $owner or the" \
           "lock file by hand." >&2
      exec {fd}>&-
      return 4
    fi
  fi

  printf '%s' "$$" >"$owner"
  "$@"
  local status=$?
  rm -f "$owner"
  exec {fd}>&-
  return "$status"
}
