#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Selftest for the package-manager tools: green cases beside each refusal.

Covers tools/package-managers.py, the offline parts of check-package-winget.py,
push-winget.py and flatpak-cargo-sources.py, and tools/push-package-repo.sh.
Offline: the push cases use local bare repositories instead of the AUR, and
nothing reaches winget-pkgs, crates.io or flatpak-builder-tools.
"""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load(name: str, file: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / "tools" / file)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


managers = load("package_managers", "package-managers.py")
winget_check = load("check_package_winget", "check-package-winget.py")
winget_push = load("push_winget", "push-winget.py")
cargo_sources = load("flatpak_cargo_sources", "flatpak-cargo-sources.py")
PUSH = ROOT / "tools/push-package-repo.sh"
REPO = "owner/Project"


def sums_for(version: str) -> str:
    names = managers.release_assets.payload(version)
    return "".join(f"{hashlib.sha256(name.encode()).hexdigest()}  {name}\n" for name in names)


def refused(call, needle: str) -> None:
    try:
        call()
    except ValueError as error:
        assert needle in str(error), (needle, str(error))
    else:
        raise AssertionError(f"accepted, expected a refusal naming {needle!r}")


def bash(script: str, cwd: Path) -> str:
    result = subprocess.run(["bash", "-euc", script], cwd=cwd, capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    return result.stdout


def recipes() -> None:
    for version, pkgver in (("1.2.3", "1.2.3"), ("1.2.3-rc.4", "1.2.3rc.4")):
        sums = managers.parse_sums(sums_for(version))
        deb = f"focusforge_{version}_amd64.deb"
        msi = f"FocusForge-{version}-x86_64.msi"
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "SHA256SUMS").write_text(sums_for(version))
            written = managers.render(version, directory / "SHA256SUMS", REPO, directory / "out")
            assert [path.relative_to(directory / "out").as_posix() for path in written] == [
                "aur/PKGBUILD", "winget/FocusForge.FocusForge.yaml", "winget/FocusForge.FocusForge.installer.yaml",
                "winget/FocusForge.FocusForge.locale.en-US.yaml",
            ]
            recipe = directory / "out/aur"
            # The PKGBUILD is what makepkg sources: read it back through bash, not through a regex.
            fields = bash(
                'source ./PKGBUILD; printf "%s\\n" "$pkgname" "$pkgver" "$pkgrel" "${#source[@]}" "${source[0]}" "${sha256sums[0]}" "${#depends[@]}" "${arch[*]}"; declare -F package',
                recipe,
            ).splitlines()
            assert fields == [
                "focusforge-bin", pkgver, "1", "1",
                f"focusforge-bin-{pkgver}.deb::https://github.com/{REPO}/releases/download/v{version}/{deb}",
                sums[deb], str(len(managers.AUR_DEPENDS)), "x86_64", "package",
            ], fields
            assert "-" not in fields[1]
            winget(directory, version, msi)


def winget(directory: Path, version: str, msi: str) -> None:
    """The rendered manifests pass the winget check's own file rules against an installer with the hashed content."""
    manifests = directory / "out/winget"
    installer = (manifests / "FocusForge.FocusForge.installer.yaml").read_text()
    assert f"InstallerUrl: https://github.com/{REPO}/releases/download/v{version}/{msi}\n" in installer
    assert "InstallerType: wix\n" in installer and "Scope: machine\n" in installer
    assert f"- UpgradeCode: '{managers.upgrade_code()}'\n" in installer and managers.upgrade_code().startswith("{")
    for path in manifests.iterdir():
        text = path.read_text()
        assert f"ManifestVersion: {managers.WINGET_MANIFEST_VERSION}\n" in text and text.count("PackageIdentifier: FocusForge.FocusForge\n") == 1, path.name
    # check-package-winget compares InstallerSha256 with the local MSI; the synthetic sums hash the file name.
    local = directory / msi
    local.write_bytes(msi.encode())
    assert winget_check.problems(manifests, local, version) == []
    local.write_bytes(b"another build")
    assert any("InstallerSha256" in problem for problem in winget_check.problems(manifests, local, version))
    local.write_bytes(msi.encode())
    assert any("PackageVersion" in problem for problem in winget_check.problems(manifests, local, version + ".1"))
    (manifests / "extra.yaml").write_text("x")
    assert any("one package" in problem for problem in winget_check.problems(manifests, local, version))
    (manifests / "extra.yaml").unlink()
    (manifests / "FocusForge.FocusForge.locale.en-US.yaml").unlink()
    assert any("expected" in problem for problem in winget_check.problems(manifests, local, version))


def winget_submission() -> None:
    assert winget_push.published_url("FocusForge.FocusForge", "1.2.3") == (
        "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/f/FocusForge/FocusForge/1.2.3/FocusForge.FocusForge.yaml")
    for name in ("wingetcreate", "flatpak_cargo_generator"):
        pin = managers.tool_pin(name)
        assert pin["url"].startswith("https://") and len(pin["sha256"]) == 64, name
    assert all("==" in requirement for requirement in managers.tool_pin("flatpak_cargo_generator")["requirements"])
    refused(lambda: managers.tool_pin("homebrew"), "has no [homebrew]")
    # Refused before anything is downloaded or submitted.
    with tempfile.TemporaryDirectory() as temp:
        clean = {key: value for key, value in os.environ.items() if key != "WINGET_TOKEN"}
        (Path(temp) / "FocusForge.FocusForge.yaml").write_text("x")
        for directory, needle, code in ((temp, "WINGET_TOKEN is not set", 1), (str(Path(temp) / "missing"), "holds no", 2)):
            result = subprocess.run([sys.executable, str(ROOT / "tools/push-winget.py"), directory, "1.2.3"], capture_output=True, text=True, env=clean)
            assert result.returncode == code and needle in result.stderr, result


LOCK = """version = 4

[[package]]
name = "focusforge-core"
version = "0.1.0"

[[package]]
name = "a"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "%s"

[[package]]
name = "b"
version = "0.2.0"
source = "git+https://example.invalid/b?rev=0123abc#0123abcdef0123abcdef0123abcdef0123abcd"
""" % ("a" * 64)


def flatpak_sources() -> None:
    good = [
        {"type": "archive", "archive-type": "tar-gzip", "url": "https://static.crates.io/crates/a/a-1.0.0.crate", "sha256": "a" * 64, "dest": "cargo/vendor/a-1.0.0"},
        {"type": "inline", "contents": json.dumps({"package": "a" * 64, "files": {}}), "dest": "cargo/vendor/a-1.0.0", "dest-filename": ".cargo-checksum.json"},
        {"type": "git", "url": "https://example.invalid/b", "commit": "0123abcdef0123abcdef0123abcdef0123abcd", "dest": "flatpak-cargo/git/b-0123abc"},
        {"type": "inline", "contents": "[source.crates-io]", "dest": "cargo", "dest-filename": "config"},
    ]
    assert cargo_sources.coverage(LOCK, good) == []
    def without(index: int) -> list:
        return [item for position, item in enumerate(good) if position != index]
    assert any("no archive for a" in problem for problem in cargo_sources.coverage(LOCK, without(0)))
    assert any("no git source" in problem for problem in cargo_sources.coverage(LOCK, without(2)))
    assert any("cargo/config" in problem for problem in cargo_sources.coverage(LOCK, without(3)))
    stale = [dict(good[0], sha256="b" * 64), *good[1:]]
    assert any("not the lockfile checksum" in problem for problem in cargo_sources.coverage(LOCK, stale))
    assert cargo_sources.coverage(LOCK, {"not": "a list"}) == ["cargo-sources.json is not a list of source objects"]
    assert any("unsupported source" in problem for problem in cargo_sources.coverage(LOCK + '\n[[package]]\nname = "c"\nversion = "1.0.0"\nsource = "sparse+https://example.invalid/"\n', good))
    # The real lockfile's packages all have a source the check understands.
    real = (ROOT / "Cargo.lock").read_text()
    assert not [problem for problem in cargo_sources.coverage(real, good) if "unsupported source" in problem]


def recipe_refusals() -> None:
    sums = managers.parse_sums(sums_for("1.2.3"))
    good = sums_for("1.2.3")
    refused(lambda: managers.parse_sums(good.replace("  ", " ", 1)), "line 1")
    refused(lambda: managers.parse_sums(good + good.splitlines(keepends=True)[0]), "twice")
    refused(lambda: managers.parse_sums(good.upper()), "line 1")
    without_deb = {name: digest for name, digest in sums.items() if not name.endswith(".deb")}
    refused(lambda: managers.pkgbuild("1.2.3", without_deb, REPO), "focusforge_1.2.3_amd64.deb")
    without_msi = {name: digest for name, digest in sums.items() if not name.endswith(".msi")}
    refused(lambda: managers.winget("1.2.3", without_msi, REPO), "FocusForge-1.2.3-x86_64.msi")
    refused(lambda: managers.pkgbuild("1.2.3", sums, "not a repo"), "OWNER/NAME")
    refused(lambda: managers.winget("1.2.3", sums, "owner/name/extra"), "OWNER/NAME")
    refused(lambda: managers.winget("1.2", sums, REPO), "not a release version")
    refused(lambda: managers.pkgbuild("v1.2.3", sums, REPO), "not a release version")
    refused(lambda: managers.pkgbuild("1.2", sums, REPO), "not a release version")
    refused(lambda: managers.bash_array(("it's",)), "cannot quote")
    # The command reports a refusal as exit 1, not a traceback.
    with tempfile.TemporaryDirectory() as temp:
        bad = Path(temp) / "SHA256SUMS"
        bad.write_text("not a checksum line\n")
        result = subprocess.run([sys.executable, str(ROOT / "tools/package-managers.py"), "render", "--version", "1.2.3", "--sums", str(bad), "--repo", REPO, "--out", temp],
                                capture_output=True, text=True)
        assert result.returncode == 1 and "FAIL" in result.stderr and "Traceback" not in result.stderr, result


def git(*args: str, cwd: Path) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def push(remote: str, branch: str, files: Path, message: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    clean = {key: value for key, value in os.environ.items() if not key.startswith(("PACKAGE_REPO_", "GIT_CONFIG_"))}
    return subprocess.run(["bash", str(PUSH), remote, branch, str(files), message], capture_output=True, text=True, env={**clean, **(env or {})})


def push_cases() -> None:
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        remote = directory / "package.git"
        git("init", "--quiet", "--bare", str(remote), cwd=directory)
        files = directory / "files"
        (files / "sub").mkdir(parents=True)
        (files / "PKGBUILD").write_text("one\n")
        (files / "sub/nested.txt").write_text("nested\n")

        first = push(str(remote), "master", files, "focusforge-bin 1.0.0")
        assert first.returncode == 0 and "pushed" in first.stdout, first
        assert git("--git-dir", str(remote), "log", "--format=%s", "master", cwd=directory) == "focusforge-bin 1.0.0"
        assert git("--git-dir", str(remote), "show", "master:sub/nested.txt", cwd=directory) == "nested"

        again = push(str(remote), "master", files, "focusforge-bin 1.0.0")
        assert again.returncode == 0 and "nothing pushed" in again.stdout, again
        assert git("--git-dir", str(remote), "rev-list", "--count", "master", cwd=directory) == "1"

        # A file the release does not write (a README) survives an update.
        clone = directory / "clone"
        git("clone", "--quiet", str(remote), str(clone), cwd=directory)
        (clone / "README.md").write_text("tap\n")
        git("add", "README.md", cwd=clone)
        git("-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "--quiet", "-m", "readme", cwd=clone)
        git("push", "--quiet", "origin", "HEAD:master", cwd=clone)
        (files / "PKGBUILD").write_text("two\n")
        update = push(str(remote), "master", files, "focusforge-bin 1.0.1")
        assert update.returncode == 0 and "pushed" in update.stdout, update
        assert git("--git-dir", str(remote), "show", "master:PKGBUILD", cwd=directory) == "two"
        assert git("--git-dir", str(remote), "show", "master:README.md", cwd=directory) == "tap"
        assert git("--git-dir", str(remote), "rev-list", "--count", "master", cwd=directory) == "3"

        # A repository with history on another branch is a misconfiguration, not a place to start a new one.
        wrong = push(str(remote), "main", files, "x")
        assert wrong.returncode == 1 and "no branch main" in wrong.stderr, wrong
        empty = directory / "empty"
        empty.mkdir()
        nothing = push(str(remote), "master", empty, "x")
        assert nothing.returncode == 2 and "holds no files" in nothing.stderr, nothing
        # Credentials are required up front, before anything connects.
        no_key = push("ssh://aur@aur.example.invalid/focusforge-bin.git", "master", files, "x")
        assert no_key.returncode == 1 and "PACKAGE_REPO_SSH_KEY is not set" in no_key.stderr, no_key
        no_pin = push("ssh://aur@aur.example.invalid/focusforge-bin.git", "master", files, "x", {"PACKAGE_REPO_SSH_KEY": "key"})
        assert no_pin.returncode == 1 and "PACKAGE_REPO_SSH_FINGERPRINT is not set" in no_pin.stderr, no_pin
        no_token = push("https://github.example.invalid/owner/package.git", "main", files, "x")
        assert no_token.returncode == 1 and "PACKAGE_REPO_TOKEN is not set" in no_token.stderr, no_token


def main() -> int:
    recipe_refusals()
    winget_submission()
    flatpak_sources()
    # The recipes are read back through bash and pushed with a bash script that only ever runs on
    # Linux release runners; the Windows gate keeps the pure-Python cases.
    if os.name == "nt":
        print("OK: package-manager refusals, winget and Flatpak source cases hold (recipe read-back and push cases need a POSIX bash; skipped on Windows)")
        return 0
    recipes()
    push_cases()
    print("OK: AUR and winget recipes render from SHA256SUMS, Flatpak sources are checked against Cargo.lock, pushes are idempotent; refusals hold")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
