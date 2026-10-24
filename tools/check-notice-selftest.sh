#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Selftest for the NOTICE collector, renderer, and stale-record reporting.
set -euo pipefail

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    # The stub cargo is a script, which Windows cannot execute without a shebang loader; the
    # collector logic under test is platform independent and runs on Linux and macOS.
    echo "NOTICE selftest not run on Windows: the stub cargo cannot be executed there"
    exit 0
    ;;
esac

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

python3 - "$REPO" "$TMP" <<'PY'
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path

repo, tmp = map(Path, sys.argv[1:])
root = tmp / "repo"
(root / "tools/lib").mkdir(parents=True)
for relative in ("tools/check-notice.sh", "tools/lib/notice.py"):
    destination = root / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(repo / relative, destination)
for relative, content in {
    "Cargo.toml": '[workspace.package]\nlicense = "GPL-3.0-only"\n',
    "Cargo.lock": "locked input\n",
    "crates/gui/ui/shell/settings_modal/about_tab.slint": 'text: "Copyright © 2026 Johannes Sood";\n',
    "assets/fonts/README.md": "Inter [v4.1](https://example.invalid)\n",
    "assets/fonts/OFL.txt": "Inter OFL text\n",
    "assets/icons/LICENSE": "Lucide identity\nVersion vendored: 1.46.0\nISC License\nISC legal text\n---\nThe MIT License (MIT)\nFeather legal text\n",
}.items():
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")

targets = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin"]
def package(name, version, source, license_name, identifier=None):
    return {"id": identifier or f"{source or 'path'}#{name}@{version}", "name": name, "version": version, "source": source, "license": license_name}
def state(active_license="MIT", extra=None, path_dependency=False, linux_on_windows=False, malformed=False, missing_identity=False, reverse=False):
    trees = {target: ["focusforge-cli v0.1.0 (/workspace/cli)", "active v1.0.0"] for target in targets}
    trees[targets[0]].append("linux-only v1.0.0")
    trees[targets[1]].append("renamed-target v2.0.0")
    trees[targets[2]].append("same v1.0.0 (https://example.invalid/git)")
    trees[targets[1]].append("same v1.0.0")
    trees[targets[0]].extend(["same-version v1.0.0", "same-version v2.0.0"])
    if extra: trees[targets[0]].append(extra["tree"])
    if path_dependency: trees[targets[0]].append("path-only v1.0.0 (/outside/path-only)")
    if linux_on_windows: trees[targets[1]].append("linux-only v1.0.0")
    if malformed: trees[targets[0]].append("malformed cargo tree record")
    if missing_identity: trees[targets[0]].append("missing-runtime v1.0.0")
    metadata = {}
    for target in targets:
        packages = [
            package("focusforge-cli", "0.1.0", None, "GPL-3.0-only", "path+workspace#focusforge-cli@0.1.0"),
            package("active", "1.0.0", "registry+example", active_license),
            package("dev-only", "1.0.0", "registry+example", "MIT"),
            package("build-only", "1.0.0", "registry+example", "MIT"),
            package("proc-only", "1.0.0", "registry+example", "MIT"),
            package("optional-disabled", "1.0.0", "registry+example", "MIT"),
        ]
        if target == targets[0]: packages.append(package("linux-only", "1.0.0", "registry+example", "MIT"))
        if target == targets[0]: packages.extend([package("same-version", "1.0.0", "registry+example", "MIT"), package("same-version", "2.0.0", "registry+example", "MIT")])
        if target == targets[1]: packages.append(package("renamed-target", "2.0.0", "registry+example", "Apache-2.0"))
        if linux_on_windows and target == targets[1]: packages.append(package("linux-only", "1.0.0", "registry+example", "MIT"))
        if target == targets[2]: packages.append(package("same", "1.0.0", "git+https://example.invalid/git#abc", "MIT"))
        if target == targets[1]: packages.append(package("same", "1.0.0", "registry+example", "Apache-2.0"))
        if extra and target == targets[0]: packages.append(package(extra["name"], extra["version"], "registry+example", extra["license"]))
        if path_dependency and target == targets[0]: packages.append(package("path-only", "1.0.0", None, "MIT", "path+outside#path-only@1.0.0"))
        metadata[target] = {"packages": list(reversed(packages)) if reverse else packages, "workspace_members": ["path+workspace#focusforge-cli@0.1.0"]}
        if reverse: trees[target].reverse()
    return {"trees": trees, "metadata": metadata}

fixture = tmp / "state.json"
fixture.write_text(json.dumps(state()), encoding="utf-8")
bin_dir = tmp / "bin"; bin_dir.mkdir()
cargo = bin_dir / "cargo"
cargo.write_text("""#!/usr/bin/env python3
import json, os, sys
state = json.load(open(os.environ["NOTICE_FIXTURE"], encoding="utf-8"))
if state.get("fail"):
    print("fixture cargo failure", file=sys.stderr); raise SystemExit(9)
target = sys.argv[sys.argv.index("--target") + 1] if "--target" in sys.argv else sys.argv[sys.argv.index("--filter-platform") + 1]
if sys.argv[1] == "tree": print("\\n".join(state["trees"][target]))
elif sys.argv[1] == "metadata": print(json.dumps(state["metadata"][target]))
else: raise SystemExit(2)
""", encoding="utf-8")
cargo.chmod(cargo.stat().st_mode | stat.S_IXUSR)
environment = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}", "NOTICE_FIXTURE": str(fixture)}
check = [str(root / "tools/check-notice.sh")]
def run(*arguments, want=0):
    result = subprocess.run([*check, *arguments], env=environment, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode != want:
        raise AssertionError((arguments, result.returncode, result.stdout, result.stderr))
    return result
def digest(path): return hashlib.sha256(path.read_bytes()).hexdigest()

empty = tmp / "empty/tools"; empty.mkdir(parents=True)
shutil.copy2(repo / "tools/check-notice.sh", empty / "check-notice.sh")
missing = subprocess.run([str(empty / "check-notice.sh")], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
assert missing.returncode != 0 and "Cargo.toml" in missing.stderr

run("--write"); run()
notice, lock = root / "NOTICE", root / "Cargo.lock"
first = notice.read_bytes()
assert b"dev-only" not in first and b"build-only" not in first and b"proc-only" not in first and b"optional-disabled" not in first
assert b"Linux x86_64" in first and b"Windows x86_64" in first and b"macOS arm64" in first and b"macOS x86_64" not in first
assert b"Package: renamed-target 2.0.0" in first and first.count(b"Package: same 1.0.0") == 2 and first.count(b"Package: same-version") == 2
run("--write"); assert first == notice.read_bytes()
fixture.write_text(json.dumps(state(reverse=True)), encoding="utf-8")
run("--write"); assert first == notice.read_bytes()

fixture.write_text(json.dumps(state(extra={"name": "added", "version": "1.0.0", "license": "MIT", "tree": "added v1.0.0"})), encoding="utf-8")
before_notice, before_lock = digest(notice), digest(lock)
result = run(want=1)
assert "added Package: added 1.0.0" in result.stderr and digest(notice) == before_notice and digest(lock) == before_lock, result.stderr
run("--write"); run()

fixture.write_text(json.dumps(state(active_license="Apache-2.0")), encoding="utf-8")
result = run(want=1)
assert "changed Package: active 1.0.0" in result.stderr and "removed Package: added 1.0.0" in result.stderr
run("--write"); run()

fixture.write_text(json.dumps(state(active_license="Apache-2.0", linux_on_windows=True)), encoding="utf-8")
assert "changed Package: linux-only 1.0.0" in run(want=1).stderr
run("--write"); run()
(root / "assets/fonts/OFL.txt").write_text("Changed Inter OFL text\n", encoding="utf-8")
assert "changed Asset: Inter 4.1" in run(want=1).stderr
run("--write"); run()

before_notice, before_lock = digest(notice), digest(lock)
fixture.write_text(json.dumps(state(active_license=None)), encoding="utf-8")
assert "active 1.0.0 has no declared license" in run(want=1).stderr and digest(notice) == before_notice and digest(lock) == before_lock
fixture.write_text(json.dumps(state(path_dependency=True)), encoding="utf-8")
assert "runtime path dependency lacks a registry or git source: path-only 1.0.0" in run(want=1).stderr and digest(notice) == before_notice and digest(lock) == before_lock
fixture.write_text(json.dumps(state(malformed=True)), encoding="utf-8")
assert "unrecognized Cargo tree package" in run(want=1).stderr and digest(notice) == before_notice and digest(lock) == before_lock
fixture.write_text(json.dumps(state(missing_identity=True)), encoding="utf-8")
assert "missing package identity for missing-runtime 1.0.0" in run(want=1).stderr and digest(notice) == before_notice and digest(lock) == before_lock

before_notice, before_lock = digest(notice), digest(lock)
fixture.write_text(json.dumps({"fail": True}), encoding="utf-8")
result = run(want=1)
assert "fixture cargo failure" in result.stderr and digest(notice) == before_notice and digest(lock) == before_lock
PY

echo "OK: NOTICE selftest covers the collector, renderer, stale records, and failure atomicity"
