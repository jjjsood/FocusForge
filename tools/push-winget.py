#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Submit the winget manifests of a release to microsoft/winget-pkgs, on Windows.

  push-winget.py MANIFEST_DIR VERSION

Downloads the wingetcreate pinned in packaging/package-manager-tools.toml,
refuses it unless its SHA-256 matches, and runs `wingetcreate submit`, which
forks winget-pkgs into the token owner's account and opens the pull request
there. The first submission creates the package; later ones add a version.
A version winget-pkgs already holds is not submitted again. A pull request
still open from an earlier run is not detected: close the duplicate by hand.

The token comes from WINGET_TOKEN: a classic personal access token with the
public_repo scope, because a fine-grained token cannot open a pull request
on a repository its owner does not own.
"""
from __future__ import annotations

import hashlib
import importlib.util
import os
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("package_managers", ROOT / "tools/package-managers.py")
managers = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(managers)
PKGS = "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests"


def published_url(package: str, version: str) -> str:
    """Where winget-pkgs keeps a version's version manifest: manifests/<first letter>/<Publisher>/<Name>/<version>/."""
    publisher, name = package.split(".", 1)
    return f"{PKGS}/{publisher[0].lower()}/{publisher}/{name}/{version}/{package}.yaml"


def published(package: str, version: str) -> bool:
    request = urllib.request.Request(published_url(package, version), method="HEAD")
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.status == 200
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        raise


def fetch_checked(url: str, sha256: str, target: Path) -> None:
    with urllib.request.urlopen(url, timeout=300) as response:
        data = response.read()
    actual = hashlib.sha256(data).hexdigest()
    if actual != sha256:
        raise ValueError(f"{url}: SHA-256 {actual}, pinned {sha256}")
    target.write_bytes(data)


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    directory, version = Path(sys.argv[1]), sys.argv[2]
    package = managers.WINGET_ID
    if not (directory / f"{package}.yaml").is_file():
        print(f"FAIL  {directory} holds no {package}.yaml", file=sys.stderr)
        return 2
    token = os.environ.get("WINGET_TOKEN", "")
    if not token:
        print("FAIL  WINGET_TOKEN is not set", file=sys.stderr)
        return 1
    if published(package, version):
        print(f"OK    winget-pkgs already holds {package} {version}; nothing submitted")
        return 0
    pin = managers.tool_pin("wingetcreate")
    with tempfile.TemporaryDirectory() as temp:
        tool = Path(temp) / "wingetcreate.exe"
        fetch_checked(pin["url"], pin["sha256"], tool)
        title = f"New version: {package} version {version}"
        result = subprocess.run([str(tool), "submit", "--prtitle", title, "--token", token, str(directory)])
    if result.returncode != 0:
        print(f"FAIL  wingetcreate submit exited {result.returncode}", file=sys.stderr)
        return 1
    print(f"OK    submitted {package} {version} to microsoft/winget-pkgs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
