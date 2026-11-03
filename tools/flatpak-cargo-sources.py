#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Write cargo-sources.json, the offline Rust crate sources of a Flatpak build.

  flatpak-cargo-sources.py OUT [--lock Cargo.lock]

Runs flatpak-cargo-generator from flatpak-builder-tools, at the commit and
SHA-256 pinned in packaging/package-manager-tools.toml, in a private virtual
environment with its pinned requirements. The result is then checked against
the lockfile: every crates.io package must appear as an archive with the
lockfile's checksum and every git package as its locked commit. A Flathub
manifest lists the file under its module's `sources`. `--verify-only OUT`
runs the check alone, on an existing file. Offline cases live in
check-package-managers-selftest.py.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import tomllib
import urllib.request
import venv
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("package_managers", ROOT / "tools/package-managers.py")
managers = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(managers)
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def coverage(lock_text: str, sources: object) -> list[str]:
    """What the generated sources miss or get wrong for this lockfile; empty when they cover it."""
    if not isinstance(sources, list) or not all(isinstance(item, dict) for item in sources):
        return ["cargo-sources.json is not a list of source objects"]
    archives = {item.get("url"): item.get("sha256") for item in sources if item.get("type") == "archive"}
    commits = {item.get("commit") for item in sources if item.get("type") == "git"}
    found: list[str] = []
    crates = gits = 0
    for package in tomllib.loads(lock_text).get("package", []):
        source, name, version = package.get("source"), package["name"], package["version"]
        if source == REGISTRY:
            crates += 1
            url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
            if url not in archives:
                found.append(f"no archive for {name} {version}")
            elif archives[url] != package.get("checksum"):
                found.append(f"{name} {version}: sha256 {archives[url]} is not the lockfile checksum")
        elif isinstance(source, str) and source.startswith("git+"):
            gits += 1
            commit = source.rsplit("#", 1)[-1]
            if commit not in commits:
                found.append(f"no git source at {commit} for {name} {version}")
        elif source is not None:
            found.append(f"{name} {version}: unsupported source {source}")
    if not any(item.get("type") == "inline" and item.get("dest") == "cargo" and item.get("dest-filename") == "config" for item in sources):
        found.append("no cargo/config that redirects crates-io to the vendored sources")
    if not crates:
        found.append("the lockfile names no crates.io package")
    return found


def generate(lock: Path, out: Path) -> None:
    pin = managers.tool_pin("flatpak_cargo_generator")
    with tempfile.TemporaryDirectory() as temp:
        work = Path(temp)
        with urllib.request.urlopen(pin["url"], timeout=120) as response:
            script = response.read()
        actual = hashlib.sha256(script).hexdigest()
        if actual != pin["sha256"]:
            raise ValueError(f"{pin['url']}: SHA-256 {actual}, pinned {pin['sha256']}")
        (work / "flatpak-cargo-generator.py").write_bytes(script)
        venv.create(work / "venv", with_pip=True)
        python = work / "venv" / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")
        subprocess.run([str(python), "-m", "pip", "install", "--quiet", "--disable-pip-version-check", *pin["requirements"]], check=True)
        out.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([str(python), str(work / "flatpak-cargo-generator.py"), str(lock.resolve()), "-o", str(out.resolve())], check=True, cwd=work)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("out", type=Path)
    parser.add_argument("--lock", type=Path, default=ROOT / "Cargo.lock")
    parser.add_argument("--verify-only", action="store_true", help="check an existing OUT against the lockfile")
    args = parser.parse_args()
    try:
        if not args.verify_only:
            generate(args.lock, args.out)
        problems = coverage(args.lock.read_text(encoding="utf-8"), json.loads(args.out.read_text(encoding="utf-8")))
    except (OSError, ValueError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        problems = [str(error)]
    for problem in problems:
        print(f"FAIL  {problem}", file=sys.stderr)
    if not problems:
        print(f"OK    {args.out}: covers every crates.io and git package of {args.lock.name}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
