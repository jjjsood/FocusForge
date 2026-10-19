#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/check-notice.sh [--write]
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

case "${1:-}" in
  '') mode=() ;;
  --write) mode=(--write) ;;
  *) echo "usage: tools/check-notice.sh [--write]" >&2; exit 2 ;;
esac

[ -f Cargo.toml ] || { echo "FAIL: Cargo.toml is required for NOTICE verification" >&2; exit 1; }
exec python3 tools/lib/notice.py --root "$REPO" "${mode[@]}"
