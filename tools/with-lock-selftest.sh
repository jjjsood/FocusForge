#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Isolated checks for the lock directory and exclusion behavior.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCK_LIB="$REPO/tools/lib/lock.sh"
# Physical path: the lock pool path is canonical, and mktemp may return a symlinked prefix (macOS /var).
TMP="$(cd -P "$(mktemp -d)" && pwd)"
trap 'rm -rf "$TMP"' EXIT

FAIL=0

lock_dir() {
  local checkout="$1"
  (
    cd "$checkout"
    # shellcheck source=lib/lock.sh
    . "$LOCK_LIB"
    _lock_dir
  )
}

expect_dir() {
  local name="$1" checkout="$2" expected="$3" actual
  actual="$(lock_dir "$checkout")"
  if [ "$actual" = "$expected" ]; then
    echo "ok   [$name]"
  else
    echo "FAIL [$name]: expected $expected, got $actual" >&2
    FAIL=1
  fi
}

make_repo() {
  local dir="$1"
  mkdir -p "$dir"
  (
    unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
    git -C "$dir" init -q
  )
}

make_link() {
  local checkout="$1" target="$2"
  mkdir -p "$target"
  ln -s "$target" "$checkout/target"
}

repo="$TMP/repository with spaces"
make_repo "$repo"

make_link "$repo" "$repo/.pool-one/cargo-target/direct"
expect_dir "hidden pool direct" "$repo" "$repo/.locks"

rm "$repo/target"
make_link "$repo" "$repo/.pool-two/cargo-target/slot/leaf"
expect_dir "hidden pool nested" "$repo" "$repo/.locks"

other="$TMP/other repository"
make_repo "$other"
rm "$repo/target"
make_link "$repo" "$repo/.pool-one/cargo-target/hook"
GIT_DIR="$other/.git" GIT_WORK_TREE="$other" GIT_INDEX_FILE="$other/.git/index" \
  expect_dir "hidden pool ignores inherited Git context" "$repo" "$repo/.locks"

plain="$TMP/plain checkout"
mkdir -p "$plain"
expect_dir "plain checkout" "$plain" "$plain/.locks"

plain_alias="$TMP/plain checkout alias"
ln -s "$plain" "$plain_alias"
expect_dir "plain checkout alias" "$plain_alias" "$plain/.locks"

shared="$TMP/shared pool"
plain_link="$TMP/plain linked checkout"
mkdir -p "$plain_link"
make_link "$plain_link" "$shared/cargo-target/slot/leaf"
expect_dir "non-Git pool" "$plain_link" "$shared/.locks"

run_lock() {  # run_lock <checkout> <backend> <timeout> <lock args...>
  local checkout="$1" backend="$2" timeout="$3"
  shift 3
  (
    cd "$checkout"
    FF_LOCK_BACKEND="$backend" FF_LOCK_TIMEOUT="$timeout" \
      bash -c '. "$1"; shift; with_lock "$@"' -- "$LOCK_LIB" "$@"
  )
}

wait_ready() {  # wait_ready <holder name> <pid> <ready path>
  local name="$1" holder="$2" ready="$3" deadline=$((SECONDS + 2))
  while [ ! -f "$ready" ]; do
    if ! kill -0 "$holder" 2>/dev/null; then
      echo "$name exited before readiness" >&2
      return 1
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "$name did not signal readiness within 2 seconds" >&2
      return 1
    fi
    sleep 0.02
  done
}

reap_holder() {  # reap_holder <pid>; only called for this selftest's children
  local holder="$1"
  kill -TERM "$holder" 2>/dev/null || true
  sleep 0.1
  kill -KILL "$holder" 2>/dev/null || true
  wait "$holder" 2>/dev/null || true
}

require_ready() {  # require_ready <holder name> <pid> <ready path>
  if wait_ready "$@"; then
    return 0
  fi
  echo "FAIL [$1]" >&2
  FAIL=1
  return 1
}

case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) windows=1 ;; *) windows=0 ;; esac

NO_FLOCK_PATH="$TMP/no-flock-bin"
if [ "$windows" = 1 ]; then
  # Git for Windows ships no flock, and a symlinked MSYS tool cannot start away from its DLLs.
  NO_FLOCK_PATH="$PATH"
else
  mkdir -p "$NO_FLOCK_PATH"
  for command in bash basename cat chmod dirname env git grep ln mkdir mktemp ps python3 rm sh sleep touch true; do
    ln -s "$(type -P "$command")" "$NO_FLOCK_PATH/$command"
  done
fi

run_lock_without_flock() {  # run_lock_without_flock <checkout> <timeout> <lock args...>
  local checkout="$1" timeout="$2"
  shift 2
  (
    cd "$checkout"
    PATH="$NO_FLOCK_PATH" FF_LOCK_TIMEOUT="$timeout" \
      bash -c '. "$1"; shift; with_lock "$@"' -- "$LOCK_LIB" "$@"
  )
}

expect_status() {  # expect_status <name> <expected> <command...>
  local name="$1" expected="$2" actual
  shift 2
  set +e
  "$@"
  actual=$?
  set -e
  if [ "$actual" = "$expected" ]; then
    echo "ok   [$name]"
  else
    echo "FAIL [$name]: expected exit $expected, got $actual" >&2
    FAIL=1
  fi
}

exercise_backend() {  # exercise_backend <flock|native>
  local backend="$1" space="$TMP/lock space $1" ready="$TMP/$1-ready" holder out script
  mkdir -p "$space"

  expect_status "$backend child exit" 37 \
    run_lock "$space" "$backend" 2 build -- sh -c 'exit 37'

  if [ "$backend" = native ]; then
    script="$TMP/native shell child.sh"
    printf '%s\n' '#!/usr/bin/env bash' 'exit 23' >"$script"
    chmod +x "$script"
    expect_status "native shell script" 23 \
      run_lock "$space" "$backend" 2 build -- "$script"
    expect_status "native rejects nonfinite timeout" 2 \
      python3 "$REPO/tools/lib/native-lock.py" "$space/nonfinite.lock" nan -- true

    rm -f "$ready" "$TMP/native-escaped" "$TMP/native-contender"
    python3 "$REPO/tools/lib/native-lock.py" "$space/lifetime.lock" 5 -- \
      bash -c 'printf held >"$1"; sleep 1; [ ! -e "$2" ] || printf escaped >"$3"' \
      bash "$ready" "$TMP/native-contender" "$TMP/native-escaped" &
    holder=$!
    require_ready "native lifetime holder" "$holder" "$ready" || { reap_holder "$holder"; return; }
    kill -KILL "$holder"
    wait "$holder" || true
    expect_status "native immediate contender" 0 \
      python3 "$REPO/tools/lib/native-lock.py" "$space/lifetime.lock" 2 -- touch "$TMP/native-contender"
    sleep 1.1
    if [ -f "$TMP/native-escaped" ]; then
      echo "FAIL [native holder death]: protected child outlived its lock holder" >&2
      FAIL=1
    else
      echo "ok   [native holder death terminates child]"
    fi
    expect_status "native killed holder releases" 0 \
      python3 "$REPO/tools/lib/native-lock.py" "$space/lifetime.lock" 2 -- true
    rm -f "$ready"
  fi

  run_lock "$space" "$backend" 5 build -- sh -c 'printf held >"$1"; sleep 1' sh "$ready" \
    >"$TMP/$backend-holder.log" 2>&1 &
  holder=$!
  require_ready "$backend contention holder" "$holder" "$ready" || { reap_holder "$holder"; return; }
  expect_status "$backend timeout" 4 \
    run_lock "$space" "$backend" 0.1 build -- true
  wait "$holder"
  expect_status "$backend competing process" 0 \
    run_lock "$space" "$backend" 2 build -- true

  out="$(run_lock "$space" "$backend" 2 build -- \
    bash -c '. "$1"; with_lock build -- true' -- "$LOCK_LIB" 2>&1)" || actual=$?
  actual="${actual:-0}"
  if [ "$actual" = 3 ] && printf '%s' "$out" | grep -q 'refusing to wait'; then
    echo "ok   [$backend nested acquisition]"
  else
    echo "FAIL [$backend nested acquisition]: exit $actual: $out" >&2
    FAIL=1
  fi
  unset actual

  rm -f "$ready"
  run_lock "$space" "$backend" 5 build -- \
    sh -c 'printf held >"$1"; kill -KILL $$' sh "$ready" >"$TMP/$backend-killed.log" 2>&1 &
  holder=$!
  require_ready "$backend killed holder" "$holder" "$ready" || { reap_holder "$holder"; return; }
  wait "$holder" || true
  expect_status "$backend holder exit releases" 0 \
    run_lock "$space" "$backend" 2 build -- true
}

exercise_resource_identity() {
  local outer="$TMP/outer space" inner="$TMP/inner space" alias out actual cycle nested backend
  mkdir -p "$outer" "$inner"

  expect_status "same name different lock directory" 0 \
    run_lock "$outer" native 2 build -- \
      bash -c 'cd "$2"; . "$1"; with_lock build -- true' -- "$LOCK_LIB" "$inner"

  cycle="$TMP/lock cycle.sh"
  nested="$TMP/lock cycle nested.sh"
  printf '%s\n' '#!/usr/bin/env bash' '. "$1"' 'with_lock gpu -- bash "$2" "$1"' >"$cycle"
  printf '%s\n' '#!/usr/bin/env bash' '. "$1"' 'with_lock build -- true' >"$nested"
  chmod +x "$cycle" "$nested"
  out="$(run_lock "$outer" native 2 build -- bash "$cycle" "$LOCK_LIB" "$nested" 2>&1)" || actual=$?
  actual="${actual:-0}"
  if [ "$actual" = 3 ] && printf '%s' "$out" | grep -q 'refusing to wait'; then
    echo "ok   [build gpu build nested cycle]"
  else
    echo "FAIL [build gpu build nested cycle]: exit $actual: $out" >&2
    FAIL=1
  fi
  unset actual

  alias="$TMP/outer alias"
  ln -s "$outer" "$alias"
  out="$(run_lock "$outer" native 2 build -- \
    bash -c 'cd "$2"; . "$1"; with_lock build -- true' -- "$LOCK_LIB" "$alias" 2>&1)" || actual=$?
  actual="${actual:-0}"
  if [ "$actual" = 3 ] && printf '%s' "$out" | grep -q 'refusing to wait'; then
    echo "ok   [same directory symlink nested]"
  else
    echo "FAIL [same directory symlink nested]: exit $actual: $out" >&2
    FAIL=1
  fi
  unset actual

  for backend in native; do
    if [ "$backend" = flock ] && ! command -v flock >/dev/null 2>&1; then
      continue
    fi
    if [ "$windows" = 1 ]; then
      # The native holder's process id is a Windows one, which the MSYS process table cannot walk;
      # the nested cases above cover the same refusal through the held-lock marker.
      echo "skip [$backend owner ancestor]: not detectable by process id on Windows"
      continue
    fi
    out="$(run_lock "$outer" "$backend" 2 build -- \
      bash -c 'unset FF_LOCK_HELD; . "$1"; with_lock build -- true' -- "$LOCK_LIB" 2>&1)" || actual=$?
    actual="${actual:-0}"
    if [ "$actual" = 3 ] && printf '%s' "$out" | grep -q 'held by an ancestor'; then
      echo "ok   [$backend owner ancestor]"
    else
      echo "FAIL [$backend owner ancestor]: exit $actual: $out" >&2
      FAIL=1
    fi
    unset actual
  done
  if command -v flock >/dev/null 2>&1; then
    backend=flock
    out="$(run_lock "$outer" "$backend" 2 build -- \
      bash -c 'unset FF_LOCK_HELD; . "$1"; with_lock build -- true' -- "$LOCK_LIB" 2>&1)" || actual=$?
    actual="${actual:-0}"
    if [ "$actual" = 3 ] && printf '%s' "$out" | grep -q 'held by an ancestor'; then
      echo "ok   [$backend owner ancestor]"
    else
      echo "FAIL [$backend owner ancestor]: exit $actual: $out" >&2
      FAIL=1
    fi
    unset actual
  fi
}

exercise_owner_cleanup() {  # exercise_owner_cleanup <flock|native>
  local backend="$1" space="$TMP/owner cleanup $1" ready="$TMP/owner-ready-$1" holder before after owner
  mkdir -p "$space"
  owner="$space/.locks/build.lock.owner"
  run_lock "$space" "$backend" 5 build -- sh -c 'printf held >"$1"; sleep 1' sh "$ready" \
    >"$TMP/owner-holder.log" 2>&1 &
  holder=$!
  require_ready "$backend owner holder" "$holder" "$ready" || { reap_holder "$holder"; return; }
  before="$(cat "$owner")"
  expect_status "native timeout preserves foreign owner" 4 \
    run_lock "$space" native 0.1 build -- true
  after="$(cat "$owner" 2>/dev/null || true)"
  if [ "$after" = "$before" ]; then
    echo "ok   [foreign owner retained]"
  else
    echo "FAIL [foreign owner retained]: expected $before, got $after" >&2
    FAIL=1
  fi
  wait "$holder"
  expect_status "native owner cleanup" 0 \
    run_lock "$space" native 2 build -- true
  if [ ! -e "$owner" ]; then
    echo "ok   [native owner cleanup removed own marker]"
  else
    echo "FAIL [native owner cleanup removed own marker]: $owner remains" >&2
    FAIL=1
  fi
}

exercise_cleanup_handoff() {
  local space="$TMP/cleanup handoff" ready="$TMP/cleanup-handoff-ready" holder owner before after
  mkdir -p "$space"
  mkdir -p "$space/.locks"
  owner="$space/.locks/build.lock.owner"
  printf '%s\n' "stale old-token" >"$owner"
  run_lock "$space" native 5 build -- sh -c 'printf held >"$1"; sleep 1' sh "$ready" \
    >"$TMP/cleanup-handoff-holder.log" 2>&1 &
  holder=$!
  require_ready "cleanup handoff holder" "$holder" "$ready" || { reap_holder "$holder"; return; }
  before="$(cat "$owner")"
  expect_status "native cleanup yields to successor" 4 \
    python3 "$REPO/tools/lib/native-lock.py" --cleanup "$space/.locks/build.lock" 0 "$owner" old-token
  after="$(cat "$owner" 2>/dev/null || true)"
  if [ "$after" = "$before" ] && kill -0 "$holder" 2>/dev/null; then
    echo "ok   [native successor owner retained]"
  else
    echo "FAIL [native successor owner retained]: owner changed or holder exited" >&2
    FAIL=1
  fi
  wait "$holder"
  if [ ! -e "$owner" ]; then
    echo "ok   [native successor cleanup]"
  else
    echo "FAIL [native successor cleanup]: $owner remains" >&2
    FAIL=1
  fi
}

exercise_readiness_failure() {
  local ready="$TMP/never-ready" holder
  bash -c 'exit 7' &
  holder=$!
  if wait_ready "intentional failed holder" "$holder" "$ready"; then
    echo "FAIL [failed holder readiness]" >&2
    FAIL=1
  else
    echo "ok   [failed holder readiness]"
  fi
  wait "$holder" || true
}

exercise_live_holder_timeout() {
  local ready="$TMP/live-never-ready" holder started elapsed
  bash -c 'sleep 60' &
  holder=$!
  started=$SECONDS
  if wait_ready "intentional live holder" "$holder" "$ready"; then
    echo "FAIL [live holder readiness]" >&2
    FAIL=1
  else
    elapsed=$((SECONDS - started))
    if [ "$elapsed" -le 3 ]; then
      echo "ok   [live holder readiness]"
    else
      echo "FAIL [live holder readiness]: returned after ${elapsed}s" >&2
      FAIL=1
    fi
  fi
  reap_holder "$holder"
}

run_without_flock_suite() {
  PATH="$NO_FLOCK_PATH" FF_LOCK_SELFTEST_NO_FLOCK=1 bash "$0"
}

if command -v flock >/dev/null 2>&1; then
  exercise_backend flock
fi
if [ ! -x "$REPO/tools/lib/native-lock.py" ]; then
  echo "FAIL [native helper]: tools/lib/native-lock.py is not executable" >&2
  FAIL=1
fi
exercise_backend native
mkdir -p "$TMP/auto native"
expect_status "auto native without flock" 0 \
  run_lock_without_flock "$TMP/auto native" 2 build -- bash -c 'exit 0'
exercise_resource_identity
exercise_owner_cleanup native
if command -v flock >/dev/null 2>&1; then
  exercise_owner_cleanup flock
fi
exercise_cleanup_handoff
exercise_readiness_failure
exercise_live_holder_timeout
if [ "${FF_LOCK_SELFTEST_NO_FLOCK:-}" != 1 ]; then
  expect_status "whole suite without flock" 0 run_without_flock_suite
fi

[ "$FAIL" -eq 0 ] || exit 1
echo "OK: lock directory selftest"
