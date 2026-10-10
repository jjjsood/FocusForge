#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Validate native package receipts before cargo-dist creates checksums."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NATIVE = ROOT / "target/distrib"
OUTPUT = ROOT / ".scratch/native-artifacts"
TARGETS = {"x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin"}
PACKAGES = {"focusforge-cli", "focusforge-gui"}


def version() -> str:
    for line in (ROOT / "Cargo.toml").read_text().splitlines():
        if line.startswith("version = "):
            return line.split('"')[1]
    raise ValueError("workspace version is missing")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def revision() -> str:
    return os.environ.get("GITHUB_SHA") or subprocess.check_output(["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True).strip()


def check_plan() -> None:
    plan = json.loads(os.environ["DIST_PLAN"])
    releases = plan.get("releases") if isinstance(plan, dict) else None
    if not isinstance(releases, list) or len(releases) != len(PACKAGES):
        raise ValueError("release plan has an unexpected release set")
    by_name = {release.get("app_name"): release for release in releases if isinstance(release, dict)}
    if set(by_name) != PACKAGES:
        raise ValueError("release plan does not name both packages")
    for name, release in by_name.items():
        artifacts = release.get("artifacts")
        if release.get("app_version") != version() or not isinstance(artifacts, list):
            raise ValueError(f"invalid release metadata for {name}")
        found = {match.group(1) for item in artifacts if (match := re.fullmatch(rf"{re.escape(name)}-(.+)\.(?:zip|tar\.xz|tar\.gz)", item))}
        if found != TARGETS:
            raise ValueError(f"release plan has unexpected targets for {name}")
        required = {f"FocusForge-{version()}-universal.dmg", f"FocusForge-{version()}-x86_64.msi", "native-artifacts.sha256", "native-artifacts.json"}
        if not required.issubset(artifacts):
            raise ValueError(f"release plan omits native artifacts for {name}")


def receipt(directory: Path, kind: str, artifact: Path) -> dict[str, str]:
    data = json.loads((directory / "native-receipt.json").read_text())
    expected = {"kind": kind, "version": version(), "revision": revision(), "artifact": artifact.name, "sha256": sha256(artifact)}
    if data != expected:
        raise ValueError(f"invalid {kind} receipt")
    return data


def collect() -> None:
    expected = {
        "macos": ("dmg", f"FocusForge-{version()}-universal.dmg"),
        "windows": ("msi", f"FocusForge-{version()}-x86_64.msi"),
    }
    entries: list[tuple[str, Path, dict[str, str]]] = []
    for platform, (kind, name) in expected.items():
        directory = NATIVE / platform
        nested = NATIVE / "native" / platform
        if directory.exists() and nested.exists():
            raise ValueError(f"duplicate native inputs for {platform}")
        if not directory.is_dir() or set(item.name for item in directory.iterdir()) != {name, "native-receipt.json"}:
            raise ValueError(f"unexpected native inputs for {platform}")
        artifact = directory / name
        entries.append((platform, artifact, receipt(directory, kind, artifact)))
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=OUTPUT.parent, prefix="native-artifacts-") as temporary:
        staging = Path(temporary)
        manifest = {"version": version(), "revision": revision(), "artifacts": []}
        for platform, artifact, data in entries:
            destination = staging / artifact.name
            shutil.copyfile(artifact, destination)
            manifest["artifacts"].append({"platform": platform, **data})
        manifest_path = staging / "native-artifacts.json"
        manifest_path.write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
        sums = "".join(f"{sha256(path)}  {path.name}\n" for path in sorted(staging.glob("FocusForge-*")))
        (staging / "native-artifacts.sha256").write_text(sums)
        replacement = OUTPUT.with_name(OUTPUT.name + ".next")
        if replacement.exists():
            shutil.rmtree(replacement)
        shutil.move(str(staging), replacement)
        previous = OUTPUT.with_name(OUTPUT.name + ".previous")
        if previous.exists():
            shutil.rmtree(previous)
        if OUTPUT.exists():
            OUTPUT.rename(previous)
        replacement.rename(OUTPUT)
        if previous.exists():
            shutil.rmtree(previous)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check-plan", action="store_true")
    args = parser.parse_args()
    if args.check_plan:
        check_plan()
    else:
        collect()


if __name__ == "__main__":
    main()
