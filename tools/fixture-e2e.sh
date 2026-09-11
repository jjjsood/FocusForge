#!/usr/bin/env bash
# Usage: tools/fixture-e2e.sh
#
# Drive the built CLI over the checked-in fixtures. This is the gate that
# catches what `cargo test` structurally cannot: that the binary someone
# actually runs starts, parses its arguments, and reads a real TIFF off disk.
# A workspace can be green on every unit test and still ship a CLI that panics
# on startup.
#
# It runs the ALREADY-BUILT binary rather than invoking cargo, which is what
# lets check-all.sh run it in the background beside the cargo gates -- cargo
# holds an exclusive lock on target/, so a background cargo command would just
# queue behind the foreground one. Keep this cargo-free.
#
# Everything it touches stays under fixtures/ and .scratch/ -- this gate
# must never read the reference stack; that is tools/run-acceptance.sh.
#
# Exit: 0 pass or skip, 1 fail.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

CLI=target/debug/focusforge-cli

if [ ! -x "$CLI" ]; then
  echo "SKIP: no CLI binary yet ($CLI)"
  exit 0
fi

if [ ! -d fixtures/mantis-512 ]; then
  echo "SKIP: no fixtures/mantis-512 yet"
  exit 0
fi

echo "+ $CLI info fixtures/mantis-512"
"$CLI" info fixtures/mantis-512

echo "OK: fixture e2e"
