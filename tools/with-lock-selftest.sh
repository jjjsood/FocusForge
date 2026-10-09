#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Isolated checks for the lock directory selected from a target link.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCK_LIB="$REPO/tools/lib/lock.sh"
TMP="$(mktemp -d)"
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

shared="$TMP/shared pool"
plain_link="$TMP/plain linked checkout"
mkdir -p "$plain_link"
make_link "$plain_link" "$shared/cargo-target/slot/leaf"
expect_dir "non-Git pool" "$plain_link" "$shared/.locks"

[ "$FAIL" -eq 0 ] || exit 1
echo "OK: lock directory selftest"
