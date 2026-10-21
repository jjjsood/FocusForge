#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Selftest for tools/check-headers.sh.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="$REPO/tools/check-headers.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/tools" "$TMP/crates/demo/src" "$TMP/crates/demo/ui" \
  "$TMP/crates/demo/shaders" "$TMP/crates/demo/data" "$TMP/spikes/demo" "$TMP/.githooks" "$TMP/fixtures" "$TMP/assets"
cp "$GATE" "$TMP/tools/check-headers.sh"

write() {
  mkdir -p "$(dirname "$1")"
  printf '%b' "$2" >"$1"
}

write "$TMP/crates/demo/src/lib.rs" '// SPDX-License-Identifier: GPL-3.0-or-later\n'
write "$TMP/crates/demo/ui/app.slint" '// SPDX-License-Identifier: GPL-3.0-or-later\n'
write "$TMP/crates/demo/shaders/pixel.wgsl" '// SPDX-License-Identifier: GPL-3.0-or-later\n'
write "$TMP/tools/ok.sh" '#!/usr/bin/env bash\n# SPDX-License-Identifier: GPL-3.0-or-later\n'
write "$TMP/tools/ok.py" '#!/usr/bin/env python3\n# coding: utf-8\n# SPDX-License-Identifier: GPL-3.0-or-later\n'
write "$TMP/spikes/demo/probe.py" '# SPDX-License-Identifier: GPL-3.0-or-later\n'
write "$TMP/.githooks/pre-commit" '#!/usr/bin/env bash\n# SPDX-License-Identifier: GPL-3.0-or-later\n'
write "$TMP/Cargo.toml" '[package]\n'
write "$TMP/fixtures/example.rs" 'not checked\n'
write "$TMP/target/generated.rs" 'not checked\n'
write "$TMP/assets/vendor.rs" 'not checked\n'
write "$TMP/crates/demo/data/font.icc" 'not checked\n'

"$TMP/tools/check-headers.sh" >/dev/null

check_failure() {
  local path="$1" content="$2" out
  write "$TMP/$path" "$content"
  if out="$($TMP/tools/check-headers.sh 2>&1)"; then
    echo "FAIL: expected $path to fail" >&2
    exit 1
  fi
  case "$out" in *"$path: missing opening SPDX header"*) ;; *)
    echo "FAIL: $path was not named" >&2
    printf '%s\n' "$out" >&2
    exit 1
  esac
  rm "$TMP/$path"
}

check_failure crates/demo/src/missing.rs 'fn main() {}\n'
check_failure crates/demo/src/wrong.rs '// SPDX-License-Identifier: MIT\n'
check_failure crates/demo/ui/missing.slint 'export component Demo {}\n'
check_failure crates/demo/shaders/missing.wgsl 'fn main() {}\n'
check_failure tools/missing.sh '#!/usr/bin/env bash\necho missing\n'
check_failure tools/missing.py '#!/usr/bin/env python3\n# coding: utf-8\nprint("missing")\n'
check_failure .githooks/missing '#!/usr/bin/env bash\necho missing\n'
check_failure spikes/demo/body_only.py 'print("SPDX-License-Identifier: GPL-3.0-or-later")\n'

echo "OK: header selftest covers source classes, opening position, and exclusions"
