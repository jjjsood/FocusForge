#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Validate the macOS and Windows package receipts and symbol archives before they are released."""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NATIVE = ROOT / "target/distrib"
OUTPUT = ROOT / ".scratch/native-artifacts"


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


def receipt(directory: Path, kind: str, artifact: Path) -> dict[str, str]:
    data = json.loads((directory / "native-receipt.json").read_text())
    expected = {"kind": kind, "version": version(), "revision": revision(), "artifact": artifact.name, "sha256": sha256(artifact)}
    if data != expected:
        raise ValueError(f"invalid {kind} receipt")
    return data


def collect() -> None:
    expected = {
        "macos": ("dmg", f"FocusForge-{version()}-aarch64.dmg"),
        "windows": ("msi", f"FocusForge-{version()}-x86_64.msi"),
    }
    entries: list[tuple[str, Path, dict[str, str]]] = []
    for platform, (kind, name) in expected.items():
        directory = NATIVE / platform
        nested = NATIVE / "native" / platform
        if directory.exists() and nested.exists():
            raise ValueError(f"duplicate native inputs for {platform}")
        if not directory.is_dir() or set(item.name for item in directory.iterdir()) != {name, "native-receipt.json", "symbols"}:
            raise ValueError(f"unexpected native inputs for {platform}")
        artifact = directory / name
        symbols = directory / "symbols"
        targets = {"macos": ("aarch64-apple-darwin",), "windows": ("x86_64-pc-windows-msvc",)}[platform]
        if set(child.name for child in symbols.iterdir()) != set(targets):
            raise ValueError(f"unexpected {platform} symbol targets")
        for target in targets:
            symbol_root = symbols / target
            symbol_archive = symbol_root / f"FocusForge-{version()}-{target}-symbols.tar.gz"
            if set(path.name for path in symbol_root.iterdir()) != {"native", "store", "symbol-inventory.json", "symbols.sha256", symbol_archive.name}:
                raise ValueError(f"unexpected {target} symbol inputs")
            subprocess.run([sys.executable, str(ROOT / "tools/package-symbols.py"), "verify", str(symbol_root)], check=True)
            inventory = json.loads((symbol_root / "symbol-inventory.json").read_text())
            symbol_checksum = symbol_root / "symbols.sha256"
            if inventory.get("platform") != target or inventory.get("version") != version() or not symbol_archive.is_file() or not symbol_checksum.is_file() or symbol_checksum.read_text().split()[:2] != [sha256(symbol_archive), symbol_archive.name]:
                raise ValueError(f"invalid {target} symbol archive")
            subprocess.run([sys.executable, str(ROOT / "tools/package-symbols.py"), "verify-archive", str(symbol_root), str(symbol_archive)], check=True)
        entries.append((platform, artifact, receipt(directory, kind, artifact)))
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=OUTPUT.parent, prefix="native-artifacts-") as temporary:
        staging = Path(temporary)
        manifest = {"version": version(), "revision": revision(), "artifacts": []}
        for platform, artifact, data in entries:
            destination = staging / artifact.name
            shutil.copyfile(artifact, destination)
            targets = {"macos": ("aarch64-apple-darwin",), "windows": ("x86_64-pc-windows-msvc",)}[platform]
            symbols = []
            for target in targets:
                symbol_archive = NATIVE / platform / "symbols" / target / f"FocusForge-{version()}-{target}-symbols.tar.gz"
                shutil.copyfile(symbol_archive, staging / symbol_archive.name)
                symbols.append({"target": target, "archive": symbol_archive.name, "sha256": sha256(symbol_archive)})
            manifest["artifacts"].append({"platform": platform, **data, "symbols": symbols})
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


if __name__ == "__main__":
    collect()
