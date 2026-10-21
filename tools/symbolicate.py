#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Symbolicate one minidump using only a verified local symbol archive."""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import tarfile
import tempfile
import importlib.util
import os
import platform
import sys
from pathlib import Path

_spec = importlib.util.spec_from_file_location("package_symbols", Path(__file__).with_name("package-symbols.py"))
assert _spec and _spec.loader
_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_module)
digest = _module.digest
verify_inventory = _module.verify_inventory
verify_archive = _module.verify_archive
normalized_source = _module.normalized_source


def scratch_root() -> Path:
    return Path(os.environ.get("FOCUSFORGE_SYMBOL_SCRATCH", Path(__file__).resolve().parent.parent / ".scratch"))


def verify_report(report: dict, inventory: dict) -> None:
    """Require one loaded application module on the crashing source frame."""
    frames = report.get("crashing_thread", {}).get("frames", [])
    modules = report.get("modules", [])
    known = {symbol["module_name"]: (entry["name"], symbol["module_id"])
             for entry in inventory["entries"] for symbol in entry["symbols"]}
    loaded = {
        item.get("filename"): (item.get("debug_file"), item.get("debug_id"))
        for item in modules
        if item.get("loaded_symbols") and item.get("filename")
    }
    for frame in frames:
        module = frame.get("module")
        application, identifier = known.get(loaded.get(module, (None, None))[0], (None, None))
        if module not in loaded or not application or loaded[module][1] != identifier:
            continue
        if not str(frame.get("function", "")).startswith(application.replace("-", "_")):
            continue
        source = normalized_source(str(frame.get("file", frame.get("source", ""))))
        if source.startswith("crates/") and isinstance(frame.get("line"), int) and not isinstance(frame["line"], bool) and frame["line"] > 0:
            return
    seen = "; ".join(
        f"{frame.get('module')}!{frame.get('function')} {frame.get('file', frame.get('source'))}:{frame.get('line')}"
        for frame in frames[:8]
    )
    applications = ", ".join(
        f"{item.get('filename')} debug_id={item.get('debug_id')} loaded={item.get('loaded_symbols')} "
        f"missing={item.get('missing_symbols')} corrupt={item.get('corrupt_symbols')} expected={known.get(item.get('debug_file'), (None, None))[1]}"
        for item in modules
        if item.get("filename") in {name for name, _ in known.values()} or item.get("debug_file") in known
    )
    raise ValueError(
        f"crashing thread lacks an application source frame (first frames: {seen or 'none'}; "
        f"crash: {report.get('crash_info')}; modules: {applications or 'none'})"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("minidump", type=Path); parser.add_argument("symbol_archive", type=Path)
    args = parser.parse_args()
    if not args.minidump.is_file() or not args.symbol_archive.is_file():
        raise SystemExit("minidump and archive must exist")
    machine, system = platform.machine().lower(), platform.system()
    target = {("Linux", "x86_64"): "x86_64-unknown-linux-gnu", ("Linux", "amd64"): "x86_64-unknown-linux-gnu", ("Darwin", "arm64"): "aarch64-apple-darwin", ("Windows", "amd64"): "x86_64-pc-windows-msvc"}.get((system, machine))
    if not target:
        raise SystemExit(f"unsupported host target: {system}/{machine}")
    scratch = scratch_root()
    stackwalk = _module.install(_module.check_config(), "stackwalk", target, scratch / "symbol-tools")
    if not stackwalk.is_file():
        raise SystemExit("pinned stackwalker is missing")
    checksums = [args.symbol_archive.with_suffix(args.symbol_archive.suffix + ".sha256"), args.symbol_archive.parent / "symbols.sha256", args.symbol_archive.parent / "native-artifacts.sha256"]
    valid_checksum = False
    for checksum in checksums:
        if not checksum.is_file():
            continue
        for line in checksum.read_text(encoding="utf-8").splitlines():
            parts = line.split()
            if len(parts) >= 2 and parts[1].lstrip("*") == args.symbol_archive.name:
                valid_checksum = parts[0] == digest(args.symbol_archive)
                break
        if valid_checksum:
            break
    if not valid_checksum:
        raise SystemExit("symbol archive checksum mismatch")
    extraction = scratch / "symbolicate"
    extraction.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=extraction, prefix="focusforge-symbols-") as temporary:
        root = Path(temporary)
        with tarfile.open(args.symbol_archive, "r:gz") as source:
            members = source.getmembers()
            names = [member.name for member in members]
            if len(names) != len(set(names)) or any(Path(member.name).is_absolute() or ".." in Path(member.name).parts or not member.isfile() for member in members):
                raise SystemExit("unsafe symbol archive")
            for member in members:
                source.extract(member, root)
        inventory = verify_archive(root, args.symbol_archive)
        if args.symbol_archive.name != f"FocusForge-{inventory['version']}-{inventory['platform']}-symbols.tar.gz":
            raise SystemExit("symbol archive name does not match its inventory")
        result = subprocess.run([str(stackwalk), "--json", "--symbols-path", str(root / "store"), str(args.minidump)], text=True, capture_output=True, check=True)
        report = json.loads(result.stdout)
        verify_report(report, inventory)
        print(result.stdout, end="")


if __name__ == "__main__":
    main()
