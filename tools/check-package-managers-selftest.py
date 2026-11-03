#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Selftest for tools/package-managers.py and tools/push-package-repo.sh: green cases beside each refusal.

Offline: the push cases use local bare repositories instead of the AUR or GitHub.
"""
from __future__ import annotations

import hashlib
import importlib.util
import os
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("package_managers", ROOT / "tools/package-managers.py")
managers = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(managers)
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
        dmg = f"FocusForge-{version}-aarch64.dmg"
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "SHA256SUMS").write_text(sums_for(version))
            written = managers.render(version, directory / "SHA256SUMS", REPO, directory / "out")
            assert [path.relative_to(directory / "out").as_posix() for path in written] == ["aur/PKGBUILD", "homebrew/Casks/focusforge.rb"]
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
            cask = (directory / "out/homebrew/Casks/focusforge.rb").read_text()
            assert f'  version "{version}"\n  sha256 "{sums[dmg]}"\n' in cask
            assert f'  url "https://github.com/{REPO}/releases/download/v#{{version}}/FocusForge-#{{version}}-aarch64.dmg"\n' in cask
            assert cask.count("\n  manpage ") == len(managers.cli_assets.MANUALS)
            for manual in managers.cli_assets.MANUALS:
                assert f"/share/man/man1/{manual}.gz\"\n" in cask, manual
            for stanza in ("bash_completion", "fish_completion", "zsh_completion"):
                assert cask.count(f"\n  {stanza} ") == 1, stanza
            assert cask.rstrip().endswith("end") and cask.count(" do\n") == 2 and "depends_on arch: :arm64" in cask


def recipe_refusals() -> None:
    sums = managers.parse_sums(sums_for("1.2.3"))
    good = sums_for("1.2.3")
    refused(lambda: managers.parse_sums(good.replace("  ", " ", 1)), "line 1")
    refused(lambda: managers.parse_sums(good + good.splitlines(keepends=True)[0]), "twice")
    refused(lambda: managers.parse_sums(good.upper()), "line 1")
    without_deb = {name: digest for name, digest in sums.items() if not name.endswith(".deb")}
    refused(lambda: managers.pkgbuild("1.2.3", without_deb, REPO), "focusforge_1.2.3_amd64.deb")
    without_dmg = {name: digest for name, digest in sums.items() if not name.endswith(".dmg")}
    refused(lambda: managers.cask("1.2.3", without_dmg, REPO), "FocusForge-1.2.3-aarch64.dmg")
    refused(lambda: managers.pkgbuild("1.2.3", sums, "not a repo"), "OWNER/NAME")
    refused(lambda: managers.cask("1.2.3", sums, "owner/name/extra"), "OWNER/NAME")
    refused(lambda: managers.pkgbuild("v1.2.3", sums, REPO), "not a release version")
    refused(lambda: managers.pkgbuild("1.2", sums, REPO), "not a release version")
    refused(lambda: managers.bash_array(("it's",)), "cannot quote")
    # The command reports a refusal as exit 1, not a traceback.
    with tempfile.TemporaryDirectory() as temp:
        bad = Path(temp) / "SHA256SUMS"
        bad.write_text("not a checksum line\n")
        result = subprocess.run(["python3", str(ROOT / "tools/package-managers.py"), "render", "--version", "1.2.3", "--sums", str(bad), "--repo", REPO, "--out", temp],
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
        (files / "Casks").mkdir(parents=True)
        (files / "PKGBUILD").write_text("one\n")
        (files / "Casks/focusforge.rb").write_text("cask\n")

        first = push(str(remote), "master", files, "focusforge-bin 1.0.0")
        assert first.returncode == 0 and "pushed" in first.stdout, first
        assert git("--git-dir", str(remote), "log", "--format=%s", "master", cwd=directory) == "focusforge-bin 1.0.0"
        assert git("--git-dir", str(remote), "show", "master:Casks/focusforge.rb", cwd=directory) == "cask"

        again = push(str(remote), "master", files, "focusforge-bin 1.0.0")
        assert again.returncode == 0 and "nothing pushed" in again.stdout, again
        assert git("--git-dir", str(remote), "rev-list", "--count", "master", cwd=directory) == "1"

        # A file the release does not write (a tap's README) survives an update.
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
        no_token = push("https://github.example.invalid/owner/homebrew-tap.git", "main", files, "x")
        assert no_token.returncode == 1 and "PACKAGE_REPO_TOKEN is not set" in no_token.stderr, no_token


def main() -> int:
    recipe_refusals()
    # The recipes are read back through bash and pushed with a bash script that only ever runs on
    # Linux release runners; the Windows gate keeps the pure-Python cases.
    if os.name == "nt":
        print("OK: package-manager refusals hold (recipe read-back and push cases need a POSIX bash; skipped on Windows)")
        return 0
    recipes()
    push_cases()
    print("OK: package-manager recipes render from SHA256SUMS and push idempotently; refusals hold")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
