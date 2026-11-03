#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Check the winget manifests that tools/package-managers.py rendered, on Windows with winget.

  check-package-winget.py MANIFEST_DIR MSI VERSION

Fails unless the directory holds exactly the version, installer and default
locale manifests of one package version, their InstallerSha256 is the
SHA-256 of MSI -- the installer the same run built, so a pull request checks
what a tag submits -- and `winget validate` accepts them. Validation warnings
are printed and pass; winget-pkgs' own pipeline decides on those.
"""
from __future__ import annotations

import hashlib
import re
import shutil
import subprocess
import sys
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def field(text: str, name: str) -> list[str]:
    return re.findall(rf"^\s*-?\s*{name}: (.+)$", text, re.MULTILINE)


def problems(directory: Path, msi: Path, version: str) -> list[str]:
    files = sorted(path.name for path in directory.iterdir()) if directory.is_dir() else []
    identifiers = {name.split(".installer.")[0].split(".locale.")[0].removesuffix(".yaml") for name in files}
    if len(identifiers) != 1:
        return [f"{directory}: expected the manifests of one package, found {files}"]
    package = identifiers.pop()
    expected = sorted([f"{package}.yaml", f"{package}.installer.yaml", f"{package}.locale.en-US.yaml"])
    if files != expected:
        return [f"{directory}: expected {expected}, found {files}"]
    found: list[str] = []
    for name in files:
        text = (directory / name).read_text(encoding="utf-8")
        if field(text, "PackageIdentifier") != [package] or field(text, "PackageVersion") != [version]:
            found.append(f"{name}: PackageIdentifier/PackageVersion are not {package} {version}")
    installer = (directory / f"{package}.installer.yaml").read_text(encoding="utf-8")
    sums = field(installer, "InstallerSha256")
    if sums != [sha256(msi).upper()]:
        found.append(f"InstallerSha256 {sums} is not the SHA-256 of {msi.name}")
    if not any(url.endswith(f"/v{version}/{msi.name}") for url in field(installer, "InstallerUrl")):
        found.append(f"InstallerUrl does not name v{version}/{msi.name}")
    return found


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    directory, msi, version = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
    if not msi.is_file():
        print(f"FAIL  missing {msi}", file=sys.stderr)
        return 2
    found = problems(directory, msi, version)
    if not found:
        winget = shutil.which("winget")
        if not winget:
            found.append("winget is not on PATH")
        else:
            print(subprocess.run([winget, "--version"], capture_output=True, text=True).stdout.strip())
            result = subprocess.run([winget, "validate", "--disable-interactivity", "--manifest", str(directory)], capture_output=True, text=True)
            output = (result.stdout + result.stderr).strip()
            print(output)
            if result.returncode != 0 and "succeeded with warnings" not in output.lower():
                found.append(f"winget validate exited {result.returncode}")
    for problem in found:
        print(f"FAIL  {problem}", file=sys.stderr)
    if not found:
        print(f"OK    winget manifests {version}: one package, checksum of {msi.name}, accepted by winget validate")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
