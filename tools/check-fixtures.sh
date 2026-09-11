#!/usr/bin/env bash
# Usage: tools/check-fixtures.sh
#
# The fixtures are generated -- cut out of a 16 GB reference stack that is not
# in this repository and that most checkouts will never have. That makes them
# the one part of the tree nobody can reproduce on demand, so their integrity
# has to be checked here instead: MANIFEST.sha256 records every file, and this
# gate verifies both directions.
#
# Regenerating needs FOCUSFORGE_REFERENCE_STACK set: tools/make-fixtures.sh
#
# Exit: 0 pass or skip, 1 fail.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

if [ ! -f fixtures/MANIFEST.sha256 ]; then
  echo "SKIP: no fixtures yet"
  exit 0
fi

status=0

if ! (cd fixtures && sha256sum --check --quiet MANIFEST.sha256); then
  echo "FAIL: a fixture does not match MANIFEST.sha256." >&2
  echo "  Regenerate with: tools/make-fixtures.sh" >&2
  status=1
fi

listed="$(sed -n 's/^[0-9a-f]\{64\}  //p' fixtures/MANIFEST.sha256 | LC_ALL=C sort)"
present="$(cd fixtures && find . -type f ! -name MANIFEST.sha256 -printf '%P\n' | LC_ALL=C sort)"
unlisted="$(comm -13 <(printf '%s\n' "$listed") <(printf '%s\n' "$present"))"

if [ -n "$unlisted" ]; then
  echo "FAIL: files under fixtures/ that MANIFEST.sha256 does not list:" >&2
  printf '%s\n' "$unlisted" | sed 's/^/  /' >&2
  echo "  A fixture is generated, never added by hand. Regenerate from the" >&2
  echo "  harness (tools/make-fixtures.sh) so the manifest covers it." >&2
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "OK: $(printf '%s\n' "$listed" | wc -l) fixture files match the manifest"
fi
exit "$status"
