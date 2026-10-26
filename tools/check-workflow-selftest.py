#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Small structural guard for the single canonical CI workflow."""
from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/check.yml"
SHA = re.compile(r"@[0-9a-f]{40}(?:\s+# v[0-9]+)?$")


def errors(text: str) -> list[str]:
    text = re.sub(r"\s+#.*$", "", text, flags=re.M)
    found: list[str] = []
    for required in ("push:", "pull_request:", "ubuntu-24.04", "macos-15", "windows-2022",
                     "fail-fast: false", "contents: read", "tools/check-all.sh --log-dir .logs/gate/ci",
                     "if: always()", "include-hidden-files: true", "if-no-files-found: error",
                     "cargo fetch --locked", "CARGO_DENY_VERSION: 0.20.2", "NEXTEST_VERSION: 0.9.144",
                     "numpy==2.2.6", "python-version: '3.12.10'", "checksum mismatch", "!target/tmp",
                     "timeout-minutes: 240", "NEXTEST_RETRIES: '2'"):
        if required not in text:
            found.append(f"missing {required!r}")
    if not re.search(r"^on:\n  push:\n    tags-ignore:\n      - '\*\*'\n  pull_request:\n", text, re.M):
        found.append("trigger must be branch pushes and pull requests, never tags")
    canonical = re.findall(r"^\s*run:\s*tools/check-all\.sh --log-dir \.logs/gate/ci\s*$", text, re.M)
    if len(canonical) != 1:
        found.append("canonical gate must occur exactly once without masking")
    if re.search(r"^\s*continue-on-error:\s*true\s*$", text, re.M):
        found.append("workflow masks a step failure")
    if re.search(r"cargo\s+(?:fmt|clippy|test|deny)\b", text):
        found.append("workflow duplicates a gate command")
    for action in ("actions/checkout", "actions/cache", "actions/setup-python", "actions/upload-artifact"):
        line = next((line.strip() for line in text.splitlines() if action in line), "")
        if not SHA.search(line):
            found.append(f"{action} is not pinned to a full immutable SHA")
    upload = re.search(r"name: Upload gate logs(?P<body>.*?)(?=\n      - |\Z)", text, re.S)
    if not upload or "path: .logs/gate/ci/" not in upload.group("body"):
        found.append("artifact upload is not limited to gate logs")
    return found


def expect_rejected(label: str, text: str) -> None:
    if not errors(text):
        raise SystemExit(f"FAIL [{label}]: malformed workflow was accepted")


def main() -> int:
    text = WORKFLOW.read_text(encoding="utf-8")
    if issues := errors(text):
        for issue in issues:
            print(f"FAIL: {issue}")
        return 1
    expect_rejected("matrix", text.replace("windows-2022", "windows-missing"))
    expect_rejected("trigger", text.replace("pull_request:", "workflow_dispatch:"))
    expect_rejected("tag trigger", text.replace("    tags-ignore:\n      - '**'\n", ""))
    expect_rejected("copied gate", text.replace("tools/check-all.sh --log-dir .logs/gate/ci", "cargo test --workspace"))
    expect_rejected("masked gate", text.replace("tools/check-all.sh --log-dir .logs/gate/ci", "tools/check-all.sh --log-dir .logs/gate/ci || true"))
    expect_rejected("artifact condition", text.replace("if: always()", "if: success()"))
    expect_rejected("artifact scope", text.replace("path: .logs/gate/ci/", "path: .logs/"))
    expect_rejected("mutable action", text.replace("@d23441a48e516b6c34aea4fa41551a30e30af803", "@v6"))
    expect_rejected("unbounded gate", text.replace("timeout-minutes: 240", ""))
    expect_rejected("cache keeps target/tmp", text.replace("!target/tmp", ""))
    expect_rejected("no flaky-test retries", text.replace("NEXTEST_RETRIES: '2'", ""))
    print("OK: workflow keeps one pinned canonical gate and narrow diagnostics")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
