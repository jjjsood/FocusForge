#!/usr/bin/env bash
# Cross-process mutex for target/ and the GPU, shared by every checkout
# whose `target` points into the same cargo-target pool:
#
#   build  target/ and the binaries under it. Cargo already takes its own
#          exclusive lock on target/ during compilation, so two `cargo build`
#          calls from different sessions already queue on each other. What
#          cargo's lock does NOT cover is a session invoking the already-built
#          binary directly (`target/debug/focusforge-cli ...`, the sanctioned
#          way to drive the CLI without re-linking) while another session's
#          `cargo build`/`clippy`/`test` relinks that exact file underneath
#          it -- the stale-binary race ("unexpected argument" from a binary
#          that was mid-overwrite, not a real code bug).
#   gpu    the one physical GPU. Cargo's target/ lock is released once
#          compilation finishes; the actual test/binary EXECUTION that follows
#          runs unlocked, so two sessions each driving a GPU-touching run
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
# One lock per resource; a command that compiles and executes holds it for both.

# The lock directory has to be the ONE physical location every checkout's
# `target` symlink points into. Walk up from the resolved link to the
# `cargo-target` directory itself (not into a pool slot below it, so every
# slot shares the same build/gpu lock) and keep `.locks` one level above it,
# next to the pool.
_lock_dir() {
  if [ -L target ]; then
    local p
    p="$(readlink -f target)"
    for _ in 1 2 3; do
      [ "$(basename "$p")" = "cargo-target" ] && break
      p="$(dirname "$p")"
    done
    echo "$(dirname "$p")/.locks"
  else
    # No shared-target symlink -- most likely a plain checkout. A local
    # .locks/ still makes the function safe to call, it just isn't shared
    # with anyone.
    echo "$(pwd)/.locks"
  fi
}

# with_lock <build|gpu> [--] <command> [args...]
# Blocks until the named lock is free (printing one line to stderr if it had
# to wait, so a long silent hang reads as contention, not a hang), then runs
# the command while holding it. Exit code is the command's own.
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

  local dir
  dir="$(_lock_dir)"
  mkdir -p "$dir"
  local f="$dir/$name.lock"

  if ! flock -n "$f" true 2>/dev/null; then
    echo ">>> waiting for the $name lock ($f) -- another session is using it" >&2
  fi
  flock "$f" "$@"
}
