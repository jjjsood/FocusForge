#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/check-licenses.sh [--advisories]
#
# The GPLv3 compatibility gate. FocusForge links Slint under its GPLv3 option,
# which makes the whole application GPLv3, and that only holds if every
# dependency's license is compatible. `cargo deny` enforces the allowlist in
# deny.toml; adding a crate outside it needs a licence review.
#
#   (no flag)      cargo deny check licenses bans sources
#   --advisories   cargo deny check advisories -- the RUSTSEC database
#
# Advisories are separate so an upstream RUSTSEC publication cannot block a
# commit.
#
# Exit: 0 pass or skip, 1 fail, 2 usage error.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

MODE=(licenses bans sources)
case "${1:-}" in
  --advisories) MODE=(advisories) ;;
  "") ;;
  *) echo "usage: tools/check-licenses.sh [--advisories]" >&2; exit 2 ;;
esac

if [ ! -f deny.toml ]; then
  echo "SKIP: deny.toml not committed yet"
  exit 0
fi

if ! command -v cargo-deny >/dev/null 2>&1; then
  echo "FAIL: deny.toml exists but cargo-deny is not installed." >&2
  echo "  cargo install cargo-deny --locked" >&2
  exit 1
fi

exec cargo deny check "${MODE[@]}"
