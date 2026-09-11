#!/usr/bin/env bash
# Cut the checked-in fixture crops out of the reference stack, or verify that
# the checked-in ones still match what the generator would produce.
#
# The crops are small windows (64x64 and 512x512) of the 8192x5464 frames, so
# the whole job is header reads and strip seeks: seconds, single-digit
# megabytes read, never a frame loaded. The set definitions -- which frame
# indices, which rect -- live in tools/lib/make_fixtures.py, which is also
# where a new set is added.
#
# Usage: tools/make-fixtures.sh [--stack DIR] [--out DIR] [--check|--manifest-only]
#   --stack DIR       reference stack to cut from (default: $FOCUSFORGE_REFERENCE_STACK)
#   --out DIR         fixture tree to write     (default: fixtures)
#   --check           regenerate into a temporary directory and compare byte for
#                     byte against --out; exit 1 on any difference, 0 on match.
#                     This is what the `fixture manifest` gate leans on: a
#                     hand-edited fixture fails as loudly as a stale one.
#   --manifest-only   re-hash --out in place (e.g. after hand-adding a golden/
#                     file) without touching the reference stack or regenerating
#                     any fixture. Needs no reference stack, so it runs even when
#                     the stack directory does not exist -- checked before the
#                     SKIP below.
#
# Exit: 0 generated / matched / re-hashed (or skipped -- no reference stack and
# neither --check nor --manifest-only), 1 on a mismatch or a bad input, 2 on a
# usage error.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

STACK_DIR=""
OUT_DIR=""
CHECK=0
MANIFEST_ONLY=0

usage() {
  echo "usage: tools/make-fixtures.sh [--stack DIR] [--out DIR] [--check|--manifest-only]" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --stack) [ "$#" -ge 2 ] || usage; STACK_DIR="$2"; shift 2 ;;
    --out) [ "$#" -ge 2 ] || usage; OUT_DIR="$2"; shift 2 ;;
    --check) CHECK=1; shift ;;
    --manifest-only) MANIFEST_ONLY=1; shift ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
done

STACK_DIR="${STACK_DIR:-${FOCUSFORGE_REFERENCE_STACK:-}}"
OUT_DIR="${OUT_DIR:-$REPO/fixtures}"

# --manifest-only re-hashes an already-written tree; it needs no reference
# stack, so it is handled before the "no reference stack" SKIP below.
if [ "$MANIFEST_ONLY" -eq 1 ]; then
  exec python3 "$REPO/tools/lib/make_fixtures.py" --out "$OUT_DIR" --manifest-only
fi

if [ -z "$STACK_DIR" ]; then
  echo "SKIP: FOCUSFORGE_REFERENCE_STACK is not set"
  exit 0
fi

if [ ! -d "$STACK_DIR" ]; then
  echo "refuse: $STACK_DIR is not a directory" >&2
  exit 1
fi

ARGS=(--stack "$STACK_DIR" --out "$OUT_DIR")
[ "$CHECK" -eq 1 ] && ARGS+=(--check)

exec python3 "$REPO/tools/lib/make_fixtures.py" "${ARGS[@]}"
