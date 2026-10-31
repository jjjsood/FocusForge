#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Offline mutation checks for release packaging and publication policy."""
from __future__ import annotations

import ast
import copy
import fnmatch
import gzip
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import struct
import types
import tomllib
import urllib.error
import xml.etree.ElementTree as ET
import plistlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
NUMERIC = VERSION.split("-")[0]
PRERELEASE = f"{NUMERIC}-rc.99"
PACKAGE = ROOT / "tools/package-linux.sh"
CHECKER = ROOT / "tools/check-package-linux.sh"
NATIVE_COLLECTOR = ROOT / "tools/collect-native-artifacts.py"
SYMBOL_PACKAGER = ROOT / "tools/package-symbols.py"
SYMBOL_TOOLS = ROOT / "packaging/symbol-tools.toml"
spec = importlib.util.spec_from_file_location("release_gate", ROOT / "tools/check-release-gate.py")
gate = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(gate)
collector_spec = importlib.util.spec_from_file_location("native_collector", NATIVE_COLLECTOR)
collector = importlib.util.module_from_spec(collector_spec)
assert collector_spec and collector_spec.loader
collector_spec.loader.exec_module(collector)


def with_prerelease(manifest: str) -> str:
    bumped = manifest.replace(f'version = "{VERSION}"', f'version = "{PRERELEASE}"', 1)
    assert bumped != manifest, f"workspace version {VERSION} not rewritten to {PRERELEASE}"
    return bumped


def call(args: list[str], success: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(args, text=True, capture_output=True)
    assert result.returncode == 0 if success else result.returncode != 0, (args, result.stdout, result.stderr)
    return result


def shell(script: str, *args: str, success: bool = True) -> None:
    call(["bash", "-c", script, "selftest", *args], success)


def release_run(sha: str, conclusion: object = "success", status: str = "completed", attempt: int = 2) -> dict[str, object]:
    return {"id": 7, "path": ".github/workflows/check.yml", "event": "push", "head_sha": sha, "created_at": f"2026-10-10T00:0{attempt}:00Z", "run_attempt": attempt, "status": status, "conclusion": conclusion}


def release_jobs(sha: str, attempt: int = 2) -> dict[str, object]:
    return {"workflow_run_id": 7, "head_sha": sha, "run_attempt": attempt, "total_count": 3, "pages": [{"jobs": [{"name": name, "run_id": 7, "head_sha": sha, "run_attempt": attempt, "status": "completed", "conclusion": "success"} for name in ("ubuntu-24.04", "macos-15", "windows-2022")]}]}


def producer() -> None:
    pins = ROOT / "packaging/linux-tools.toml"
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        shell('FOCUSFORGE_PACKAGE_OUT="$2"; source "$1"; test "$out" = "$root/.scratch/package-linux"', str(PACKAGE), str(directory / "unrequested-output"))
        def selected_values(path: Path) -> list[str]:
            return call(["bash", "-c", 'source "$1"; read_pin_values "$2"', "selftest", str(PACKAGE), str(path)]).stdout.splitlines()

        def expected_values(path: Path) -> list[str]:
            parsed = tomllib.loads(path.read_text())
            return [parsed[section][key] for section, key in (("linux", "container"), ("appimagetool", "url"), ("appimagetool", "sha256"), ("type2_runtime", "url"), ("type2_runtime", "sha256"))]

        assert selected_values(pins) == expected_values(pins)
        parts = pins.read_text().split("[[type2_runtime.legal_inputs]]")
        varied = directory / "varied-legal-inputs.toml"
        varied.write_text(parts[0] + "".join("[[type2_runtime.legal_inputs]]" + part for part in reversed(parts[1:])))
        assert selected_values(varied) == expected_values(varied)
        values = tomllib.loads(pins.read_text())
        nested_first = directory / "nested-legal-before-pins.toml"
        nested_first.write_text(
            "[legal]\n[[legal.inputs]]\nname = \"alternate\"\nurl = \"https://example.invalid/alternate-license\"\nsha256 = \"" + "f" * 64 + "\"\n\n"
            + "\n\n".join(
                f"[{section}]\n" + "\n".join(f"{key} = {json.dumps(value)}" for key, value in values[section].items() if isinstance(value, str))
                for section in ("linux", "appimagetool", "type2_runtime")
            )
            + "\n"
        )
        assert selected_values(nested_first) == expected_values(nested_first)
        commented = directory / "commented-headers.toml"
        legal_rows = []
        for index in range(11):
            legal_rows.append(
                "[[type2_runtime.legal_inputs]] # legal input\n"
                + f'name = "legal-{index}"\n'
                + f'url = "https://example.invalid/legal/{index}#fragment=a=b"\n'
                + f'sha256 = "{hashlib.sha256(f"legal-{index}".encode()).hexdigest()}"\n'
            )
        commented.write_text(
            "[linux] # selected container\n"
            + f'container = {json.dumps(values["linux"]["container"])}\n\n'
            + "[type2_runtime] # selected runtime\n"
            + 'url = "https://example.invalid/runtime#fragment=a=b"\n'
            + f'sha256 = "{hashlib.sha256(b"runtime-commented").hexdigest()}"\n\n'
            + "\n".join(legal_rows)
            + "\n[appimagetool] # selected after legal arrays\n"
            + 'url = "https://example.invalid/app#fragment=a=b"\n'
            + f'sha256 = "{hashlib.sha256(b"app-commented").hexdigest()}"\n'
        )
        assert selected_values(commented) == expected_values(commented)
        whitespace_headers = directory / "whitespace-headers.toml"
        whitespace_headers.write_text(
            commented.read_text()
            .replace("[linux] #", "[ linux ] #")
            .replace("[type2_runtime] #", "[ type2_runtime ] #")
            .replace("[[type2_runtime.legal_inputs]] #", "[[ type2_runtime.legal_inputs ]] #")
            .replace("[appimagetool] #", "[ appimagetool ] #")
        )
        assert selected_values(whitespace_headers) == expected_values(whitespace_headers)
        legal = directory / "legal.tsv"
        call([str(PACKAGE), "--write-legal-inputs", str(legal)])
        assert len(legal.read_text().splitlines()) == 11
        call([str(PACKAGE), "--write-legal-inputs", str(legal), str(varied)])
        assert len(legal.read_text().splitlines()) == 11
        call([str(PACKAGE), "--write-legal-inputs", str(legal), str(commented)])
        parsed_commented = tomllib.loads(commented.read_text())
        assert legal.read_text().splitlines() == [f'{item["name"]}\t{item["url"]}\t{item["sha256"]}' for item in parsed_commented["type2_runtime"]["legal_inputs"]]
        unsupported = directory / "unsupported-header.toml"
        unsupported.write_text(commented.read_text().replace("[appimagetool] # selected after legal arrays", "[appimagetool unsupported] # selected after legal arrays"))
        result = subprocess.run(["bash", "-c", 'source "$1"; read_pin_values "$2"', "selftest", str(PACKAGE), str(unsupported)], text=True, capture_output=True)
        assert result.returncode != 0 and result.stderr.strip() == "unsupported pin table header"
        legal.write_text("kept\n")
        call([str(PACKAGE), "--write-legal-inputs", str(legal), str(unsupported)], False)
        assert legal.read_text() == "kept\n"
        legal.write_text("kept\n")
        corrupt = directory / "corrupt.toml"
        corrupt.write_text(pins.read_text().replace('sha256 = "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"', 'sha256 = "z"'))
        call([str(PACKAGE), "--write-legal-inputs", str(legal), str(corrupt)], False)
        assert legal.read_text() == "kept\n"
        payload = directory / "payload"; payload.write_bytes(b"pinned")
        call([str(PACKAGE), "--verify-sha256", str(payload), hashlib.sha256(payload.read_bytes()).hexdigest()])
        call([str(PACKAGE), "--verify-sha256", str(payload), "0" * 64], False)
        setup = r'''
source "$1"
out="$2"; version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1); revision=$(git -C "$root" rev-parse HEAD)
image=$(python3 - "$root/packaging/linux-tools.toml" <<'PY'
import sys, tomllib
print(tomllib.load(open(sys.argv[1], "rb"))["linux"]["container"])
PY
)
mkdir -p "$out"; printf app > "$out/FocusForge-$version-x86_64.AppImage"; printf deb > "$out/focusforge_${version}_amd64.deb"
(cd "$out" && sha256sum "FocusForge-$version-x86_64.AppImage" "focusforge_${version}_amd64.deb" > artifacts.sha256)
mkdir -p "$out/symbols"
python3 -c 'import hashlib,json,tarfile,sys,shutil,importlib.util; root=__import__("pathlib").Path(sys.argv[1]); version=sys.argv[2]; spec=importlib.util.spec_from_file_location("package_symbols","tools/package-symbols.py"); module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); entries=[]
for name,ident in (("focusforge-cli","73DB7F2DB3A643B9DFD8FF6C4DFF41940"),("focusforge-gui","05CE7935D101B8E6E397B49D6F6716080")):
 native=root/"native"/(name+".debug"); native.parent.mkdir(exist_ok=True); shutil.copyfile("/bin/true",native); ident=module.elf_module_id(native); sym=root/"store"/name/ident/(name+".sym"); sym.parent.mkdir(parents=True); sym.write_text(f"MODULE Linux x86_64 {ident} {name}\nFILE 0 crates/{name}/src/main.rs\nFUNC 1000 10 0 {name.replace(chr(45),chr(95))}::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n"); h=lambda p:hashlib.sha256(p.read_bytes()).hexdigest(); entries.append({"name":name,"native":str(native.relative_to(root)),"native_sha256":h(native),"native_id":ident,"native_build_id":module.elf_build_id(native),"native_source_inspection":{"schema":module.SOURCE_INSPECTION_SCHEMA,"format":"elf-dwarf","native_inspection_sha256":h(native),"coverage":{"compile_units":1,"line_tables":1,"source_less_compile_units":0,"no_line_compile_units":0},"source_paths":[f"/work/crates/{name.split(chr(45),1)[1]}/src/main.rs"]},"symbols":[{"path":str(sym.relative_to(root)),"sha256":h(sym),"module_id":ident,"module_name":name}]})
(root/"symbol-inventory.json").write_text(json.dumps({"platform":"x86_64-unknown-linux-gnu","version":version,"entries":entries})); archive=root/f"FocusForge-{version}-x86_64-unknown-linux-gnu-symbols.tar.gz"; source=tarfile.open(archive,"w:gz"); [source.add(p,arcname=p.relative_to(root)) for p in [root/"symbol-inventory.json",*sorted((root/"native").rglob("*")),*sorted((root/"store").rglob("*"))] if p.is_file()]; source.close(); print(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}")' "$out/symbols" "$version" > "$out/symbols/symbols.sha256"
input_manifest="$out/current-inputs.sha256"; write_input_manifest "$input_manifest"; cp "$input_manifest" "$out/package-inputs.sha256"; printf 'source_revision=%s\nimage=%s\n' "$revision" "$image" > "$out/inventory.txt"; verify_reuse
'''
        out = directory / "out"
        shell(setup, str(PACKAGE), str(out))
        shell(setup + '\nprintf stale >> "$out/FocusForge-$version-x86_64.AppImage"; verify_reuse\n', str(PACKAGE), str(out), success=False)
        shell(setup + '\nrm "$out/package-inputs.sha256"; verify_reuse\n', str(PACKAGE), str(out), success=False)
        shell(setup + '\nprintf stale > "$out/stale.deb"; verify_reuse\n', str(PACKAGE), str(out), success=False)


def producer_foreign_owned_output_cleanup() -> None:
    image = tomllib.loads((ROOT / "packaging/linux-tools.toml").read_text())["linux"]["container"]
    with tempfile.TemporaryDirectory() as temp:
        output = Path(temp) / "out"
        output.mkdir()
        call([
            "docker", "run", "--rm", "-v", f"{output}:/out", image,
            "bash", "-c", "mkdir -p /out/check/symbolicate/stackwalk && printf old >/out/check/symbolicate/stackwalk/minidump-stackwalk && chown -R 1001:1001 /out/check",
        ])
        try:
            result = subprocess.run(
                [
                    "bash", "-c",
                    'source "$1"; image="$2"; owner_uid=$(id -u); owner_gid=$(id -g); clean_package_output "$3"',
                    "selftest", str(PACKAGE), image, str(output),
                ],
                text=True,
                capture_output=True,
            )
            assert result.returncode == 0, (result.stdout, result.stderr)
            assert not output.exists()
        finally:
            if output.exists():
                call([
                    "docker", "run", "--rm", "-v", f"{output}:/out", image,
                    "bash", "-c", "find /out -mindepth 1 -maxdepth 1 -exec rm -rf {} +",
                ])


def producer_cleanup_mock() -> None:
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        output = directory / "out"
        output.mkdir()
        (output / "check").mkdir()
        (output / "check/old-output").write_text("old")
        mock = directory / "mock"
        mock.mkdir()
        log = directory / "docker.log"
        docker = mock / "docker"
        docker.write_text("#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"$MOCK_DOCKER_LOG\"\n")
        docker.chmod(0o755)
        result = subprocess.run(
            [
                "bash", "-c",
                'source "$1"; PATH="$4:$PATH"; image=mock-image; owner_uid=$(id -u); owner_gid=$(id -g); printf "%s:%s\\n" "$owner_uid" "$owner_gid"; clean_package_output "$3"',
                "selftest", str(PACKAGE), "unused", str(output), str(mock),
            ],
            text=True,
            capture_output=True,
            env={**os.environ, "MOCK_DOCKER_LOG": str(log)},
        )
        assert result.returncode == 0, (result.stdout, result.stderr)
        assert not output.exists()
        expected_owner = result.stdout.strip()
        assert f"chown -R {expected_owner} /out" in log.read_text()


def cli_asset_helper_cases() -> None:
    helper_path = ROOT / "tools/package-cli-assets.py"
    helper_spec = importlib.util.spec_from_file_location("cli_assets", helper_path)
    helper = importlib.util.module_from_spec(helper_spec)
    assert helper_spec and helper_spec.loader
    helper_spec.loader.exec_module(helper)
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        source = directory / "product"
        shutil.copytree(ROOT / "assets/man", source / "assets/man")
        shutil.copytree(ROOT / "assets/completions", source / "assets/completions")
        expected = helper.sources(source)
        assert len(expected) == 19
        staged = []
        for destination in (
            directory / "appimage/usr",
            directory / "bundle/FocusForge.app/Contents/Resources",
            directory / "windows/FocusForge",
        ):
            helper.stage(source, destination)
            for relative in (
                Path("share/man/man1/system.1.gz"),
                Path("share/bash-completion/completions/apt"),
                Path("share/zsh/vendor-completions/_system"),
                Path("share/fish/vendor_completions.d/system.fish"),
                Path("share/powershell/completions/_system.ps1"),
            ):
                (destination / relative).write_bytes(b"unrelated")
            helper.verify(source, destination)
            staged.append(destination)
        assert {
            relative: (staged[0] / relative).read_bytes() for relative in expected
        } == {
            relative: (staged[1] / relative).read_bytes() for relative in expected
        }
        assert all(
            (destination / relative).read_bytes() == b"unrelated"
            for destination in staged
            for relative in (
                Path("share/man/man1/system.1.gz"),
                Path("share/bash-completion/completions/apt"),
                Path("share/zsh/vendor-completions/_system"),
                Path("share/fish/vendor_completions.d/system.fish"),
                Path("share/powershell/completions/_system.ps1"),
            )
        )
        for name in helper.MANUALS:
            data = (staged[0] / "share/man/man1" / f"{name}.gz").read_bytes()
            assert data[3] == 0 and data[4:8] == b"\0\0\0\0" and data[9] == 255
            assert gzip.decompress(data) == (source / "assets/man" / name).read_bytes()
        cases = (
            (Path("share/man/man1/focusforge-cli.1.gz"), "manual", ("focusforge-cli-extra.1.gz", "focusforge-cli-extra.txt")),
            (Path("share/bash-completion/completions/focusforge-cli"), "completion", ("focusforge-cli-extra", "focusforge-cli-extra.bak")),
        )
        for destination in staged:
            for relative, resource_class, orphans in cases:
                target = destination / relative
                original = target.read_bytes()
                target.unlink()
                try:
                    helper.verify(source, destination)
                except ValueError as error:
                    assert str(relative) in str(error), (resource_class, error)
                else:
                    raise AssertionError(f"missing {resource_class} passed verification")
                target.write_bytes(original + b"corrupt")
                try:
                    helper.verify(source, destination)
                except ValueError as error:
                    assert str(relative) in str(error), (resource_class, error)
                else:
                    raise AssertionError(f"corrupt {resource_class} passed verification")
                target.write_bytes(original)
                for name in orphans:
                    orphan = target.parent / name
                    orphan.write_bytes(b"orphan")
                    try:
                        helper.verify(source, destination)
                    except ValueError as error:
                        assert str(orphan.relative_to(destination)) in str(error), (resource_class, error)
                    else:
                        raise AssertionError(f"orphan {resource_class} passed verification")
                    orphan.unlink()
                helper.verify(source, destination)


def fake_appimage(directory: Path) -> tuple[Path, Path, Path]:
    expected, app = directory / "expected", directory / "appimage"
    (expected / "frames").mkdir(parents=True)
    for frame in ("00.tif", "01.tif", "02.tif"): (expected / "frames" / frame).write_bytes(b"")
    for name in ("LICENSE", "NOTICE"): shutil.copy(ROOT / name, expected / name)
    shutil.copy(ROOT / "assets/focusforge.desktop", expected / "focusforge.desktop")
    shutil.copytree(ROOT / "assets/build/hicolor", expected / "hicolor"); (expected / "VERSION").write_text(f"focusforge-cli {VERSION}\n")
    tree = app / "usr"
    for path in (tree / "bin", tree / "lib", tree / "share/applications", tree / "share/icons", tree / "share/doc/focusforge/native-licenses", tree / "share/doc/focusforge/type2-runtime/notices"): path.mkdir(parents=True, exist_ok=True)
    call(["/usr/bin/python3", str(ROOT / "tools/package-cli-assets.py"), "stage", str(ROOT), str(tree)])
    shutil.copytree(expected / "hicolor", tree / "share/icons/hicolor"); shutil.copy(expected / "focusforge.desktop", tree / "share/applications/focusforge.desktop")
    for name in ("LICENSE", "NOTICE"): shutil.copy(expected / name, tree / f"share/doc/focusforge/{name}")
    (tree / "share/doc/focusforge/native-notices.txt").write_text("libfontconfig\n")
    licenses = tree / "share/doc/focusforge/native-licenses"
    (licenses / "fake.copyright").write_text("/usr/share/common-licenses/GPL. and /usr/share/common-licenses/LGPL.\n")
    (licenses / "GPL").write_text("GPL\n"); (licenses / "LGPL").write_text("LGPL\n")
    (tree / "share/doc/focusforge/type2-runtime/RELINKING.md").write_text("dd6cebedcbddde9c82f89b011e8e1d40b6e43868\nlibfuse 3.15.0\n")
    notice = tree / "share/doc/focusforge/type2-runtime/notices/license"; notice.write_text("legal\n")
    legal = expected / "legal.sha256"; legal.write_text(f"{hashlib.sha256(notice.read_bytes()).hexdigest()}  notices/license\n")
    true = Path(shutil.which("true") or "/bin/true"); shutil.copy(true, tree / "lib/libfontconfig.so.1")
    cli = tree / "bin/focusforge-cli"
    cli.write_text("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'focusforge-cli " + VERSION + "'; elif [ \"$1\" = --help ]; then echo help; elif [ \"$1\" = info ] && [ \"$2\" = --json ]; then printf '{\"frame_count\":3,\"unreadable_count\":0,\"files\":[{\"name\":\"00.tif\",\"width\":64,\"height\":64,\"samples_per_pixel\":3,\"bits_per_sample\":[16,16,16]},{\"name\":\"01.tif\",\"width\":64,\"height\":64,\"samples_per_pixel\":3,\"bits_per_sample\":[16,16,16]},{\"name\":\"02.tif\",\"width\":64,\"height\":64,\"samples_per_pixel\":3,\"bits_per_sample\":[16,16,16]}]}\\n'; else for n in 00 01 02; do echo \"$2/$n.tif 64x64 3ch 16-bit\"; done; fi\n")
    gui = tree / "bin/focusforge-gui"; gui.write_text("#!/bin/sh\nexit 0\n")
    app.joinpath("AppRun").write_text("#!/bin/sh\nexec \"$(dirname \"$0\")/usr/bin/focusforge-gui\" \"$@\"\n")
    app.joinpath("focusforge.desktop").symlink_to("usr/share/applications/focusforge.desktop")
    app.joinpath("focusforge.png").symlink_to("usr/share/icons/hicolor/256x256/apps/focusforge.png")
    for path in (cli, gui, app / "AppRun"): path.chmod(0o755)
    return app, expected, legal


def checker() -> None:
    with tempfile.TemporaryDirectory() as temp:
        app, expected, legal = fake_appimage(Path(temp))
        args = ["bash", str(CHECKER), "--validate-appimage", str(app), str(expected), str(legal)]
        call(args)
        desktop = app / "focusforge.desktop"
        desktop.unlink()
        desktop.write_bytes((expected / "focusforge.desktop").read_bytes() + f"X-AppImage-Version={VERSION}\n".encode())
        call(args)
        desktop.write_bytes((expected / "focusforge.desktop").read_bytes() + b"X-AppImage-Version=9.9.9\n"); call(args, False)
        desktop.write_bytes((expected / "focusforge.desktop").read_bytes() + f"X-AppImage-Version={VERSION}\nextra\n".encode()); call(args, False)
        desktop.unlink(); desktop.symlink_to("usr/share/applications/focusforge.desktop")
        app.joinpath("usr/share/doc/focusforge/LICENSE").unlink(); call(args, False)
        shutil.copy(expected / "LICENSE", app / "usr/share/doc/focusforge/LICENSE")
        app.joinpath("focusforge.png").unlink(); app.joinpath("focusforge.png").symlink_to("missing.png"); call(args, False)
        app.joinpath("focusforge.png").unlink(); app.joinpath("focusforge.png").symlink_to("usr/share/icons/hicolor/256x256/apps/focusforge.png")
        icon = app / "usr/share/icons/hicolor/256x256/apps/focusforge.png"; icon.write_bytes(b"wrong"); call(args, False); shutil.copy(expected / "hicolor/256x256/apps/focusforge.png", icon)
        app.joinpath("usr/lib/libfontconfig.so.1").unlink(); call(args, False)
        shutil.copy(Path(shutil.which("true") or "/bin/true"), app / "usr/lib/libfontconfig.so.1")
        cli = app / "usr/bin/focusforge-cli"; cli.write_text(cli.read_text().replace(f"focusforge-cli {VERSION}", "focusforge-cli 9.9.9")); call(args, False)
        cli.write_text(cli.read_text().replace("focusforge-cli 9.9.9", f"focusforge-cli {VERSION}"))
        binary = app / "usr/lib/libfontconfig.so.1"; data = binary.read_bytes(); marker = next(item.encode() for item in ("GLIBC_2.34", "GLIBC_2.35", "GLIBC_2.36") if item.encode() in data)
        binary.write_bytes(data.replace(marker, b"GLIBC_9.99", 1)); call(args, False)


def gate_checks() -> None:
    sha = "a" * 40; good_runs = {"pages": [{"workflow_runs": [release_run(sha)]}]}; good_jobs = release_jobs(sha)
    assert gate.approved(good_runs, good_jobs, sha)
    for conclusion in ("failure", "cancelled", "skipped", "neutral", None): assert not gate.approved({"pages": [{"workflow_runs": [release_run(sha, conclusion)]}]}, good_jobs, sha)
    assert not gate.approved({"pages": [{"workflow_runs": []}]}, good_jobs, sha)
    assert not gate.approved({"pages": [{"workflow_runs": [release_run("b" * 40)]}]}, good_jobs, sha)
    assert not gate.approved({"pages": [{"workflow_runs": [release_run(sha, status="in_progress")]}]}, good_jobs, sha)
    assert not gate.approved({"pages": [{"workflow_runs": [release_run(sha, attempt=1), release_run(sha, "failure", attempt=2)]}]}, good_jobs, sha)
    assert gate.approved({"total_count": 1, "pages": [{"workflow_runs": []}, {"workflow_runs": [release_run(sha)]}]}, good_jobs, sha)
    bad = release_jobs(sha); bad["pages"][0]["jobs"][0]["conclusion"] = "skipped"; assert not gate.approved(good_runs, bad, sha)
    for field, value in (("head_sha", "b" * 40), ("run_id", 8), ("run_attempt", 3)):
        mutated = json.loads(json.dumps(good_jobs)); mutated["pages"][0]["jobs"][0][field] = value; assert not gate.approved(good_runs, mutated, sha)
    for field, value in (("head_sha", "b" * 40), ("workflow_run_id", 8), ("run_attempt", 3)):
        mutated = json.loads(json.dumps(good_jobs)); mutated[field] = value; assert not gate.approved(good_runs, mutated, sha)
    for conclusion in ("failure", "cancelled", "neutral"):
        mutated = json.loads(json.dumps(good_jobs)); mutated["pages"][0]["jobs"][0]["conclusion"] = conclusion; assert not gate.approved(good_runs, mutated, sha)
    missing = json.loads(json.dumps(good_jobs)); missing["pages"][0]["jobs"].pop(); missing["total_count"] = 2; assert not gate.approved(good_runs, missing, sha)
    duplicate = json.loads(json.dumps(good_jobs)); duplicate["pages"][0]["jobs"].append(duplicate["pages"][0]["jobs"][0]); duplicate["total_count"] = 4; assert not gate.approved(good_runs, duplicate, sha)
    with tempfile.TemporaryDirectory() as temp:
        runs, jobs = Path(temp) / "runs", Path(temp) / "jobs"; runs.write_text(json.dumps(good_runs)); jobs.write_text(json.dumps(good_jobs))
        call([str(ROOT / "tools/check-release-gate.py"), "--sha", sha, "--runs", str(runs), "--jobs", str(jobs)])
        runs.write_text("["); call([str(ROOT / "tools/check-release-gate.py"), "--sha", sha, "--runs", str(runs), "--jobs", str(jobs)], False)
    class Response(io.BytesIO):
        def __init__(self, body: dict[str, object], link: str = "") -> None: super().__init__(json.dumps(body).encode()); self.headers = {"Link": link}
        def __enter__(self) -> "Response": return self
        def __exit__(self, *args: object) -> None: self.close()
    original = gate.urllib.request.urlopen
    def fake(request: object, timeout: float) -> Response:
        return Response({"total_count": 1, "workflow_runs": [release_run(sha)]}) if request.full_url.endswith("page=2") else Response({"total_count": 1, "workflow_runs": []}, '<https://api.github.com/page=2>; rel="next"')
    gate.urllib.request.urlopen = fake
    try:
        assert gate.request_pages("https://api.github.com/runs", 1e20, False)["pages"][1]["workflow_runs"]
        gate.urllib.request.urlopen = lambda request, timeout: (_ for _ in ()).throw(urllib.error.HTTPError(request.full_url, 404, "Not Found", {}, None))
        try: gate.request_pages("https://api.github.com/runs", 1e20, False)
        except urllib.error.HTTPError: pass
        else: raise AssertionError("API failure accepted")
        try: gate.request_pages("https://api.github.com/runs", 0, False)
        except ValueError: pass
        else: raise AssertionError("pending timeout accepted")
    finally: gate.urllib.request.urlopen = original
    original_pages, original_clock, original_sleep, original_remote = gate.request_pages, gate.time.monotonic, gate.time.sleep, gate.remote
    ticks = iter((0.0, 0.1, 0.2, 2.0)); sleeps: list[float] = []
    def pending(url: str, deadline: float, use_token: bool) -> dict:
        return {"pages": [{"workflow_runs": [release_run(sha, status="in_progress")]}]} if "/workflows/" in url else {"pages": [{"jobs": []}]}
    gate.request_pages = pending; gate.time.monotonic = lambda: next(ticks); gate.time.sleep = sleeps.append
    try:
        runs, pending_jobs = gate.remote("owner/repo", sha, 1, False); assert sleeps and not gate.approved(runs, pending_jobs, sha)
        gate.remote = lambda *args: (_ for _ in ()).throw(urllib.error.URLError("offline")); assert gate.main(["--sha", sha, "--repository", "owner/repo"]) == 2
    finally:
        gate.request_pages, gate.time.monotonic, gate.time.sleep, gate.remote = original_pages, original_clock, original_sleep, original_remote
    clock = [0.0]; pauses: list[float] = []; requests: list[str] = []
    original_pages, original_clock, original_sleep = gate.request_pages, gate.time.monotonic, gate.time.sleep
    def delayed_success(url: str, deadline: float, use_token: bool) -> dict:
        requests.append(url)
        if "/workflows/" in url:
            return good_runs if clock[0] >= 180 else {"pages": [{"workflow_runs": [release_run(sha, status="in_progress")]}]}
        return json.loads(json.dumps(good_jobs))
    def advance(seconds: float) -> None:
        pauses.append(seconds); clock[0] += seconds
    gate.request_pages = delayed_success; gate.time.monotonic = lambda: clock[0]; gate.time.sleep = advance
    try:
        runs, jobs = gate.remote("owner/repo", sha, 3600, False)
        assert gate.approved(runs, jobs, sha) and pauses == [90, 90] and len(requests) == 4
    finally:
        gate.request_pages, gate.time.monotonic, gate.time.sleep = original_pages, original_clock, original_sleep
    calls: list[tuple[str, str, int, bool]] = []
    original_remote = gate.remote
    gate.remote = lambda repository, commit, wait, token: (calls.append((repository, commit, wait, token)) or (good_runs, good_jobs))
    try:
        assert gate.main(["--sha", sha, "--repository", "owner/repo"]) == 0
        assert calls == [("owner/repo", sha, 3600, True)]
    finally:
        gate.remote = original_remote


def gate_retry_cases() -> None:
    url = "https://api.github.com/runs"
    page = {"total_count": 0, "workflow_runs": []}
    class Reply(io.BytesIO):
        headers = {"Link": ""}
        def __enter__(self) -> "Reply": return self
        def __exit__(self, *args: object) -> None: self.close()
    def failure(code: int, **headers: str) -> urllib.error.HTTPError:
        return urllib.error.HTTPError(url, code, "stub", headers, None)
    def run(*outcomes: Exception, limit: float = 1e6) -> tuple[object, list[float], int]:
        script, sleeps, clock = list(outcomes), [], [0.0]
        def urlopen(request: object, timeout: float) -> Reply:
            if script:
                raise script.pop(0)
            return Reply(json.dumps(page).encode())
        def pause(seconds: float) -> None: sleeps.append(seconds); clock[0] += seconds
        saved = gate.urllib.request.urlopen, gate.time.sleep, gate.time.monotonic, gate.time.time
        gate.urllib.request.urlopen, gate.time.sleep, gate.time.monotonic, gate.time.time = urlopen, pause, lambda: clock[0], lambda: 1000.0
        try:
            try: result: object = gate.request_pages(url, limit, True)
            except (ValueError, urllib.error.HTTPError) as error: result = error
        finally:
            gate.urllib.request.urlopen, gate.time.sleep, gate.time.monotonic, gate.time.time = saved
        return result, sleeps, len(script)
    result, sleeps, _ = run(failure(403, **{"X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "1030"}))
    assert result == {"pages": [page], "total_count": 0} and sleeps == [31.0], (result, sleeps)
    result, sleeps, _ = run(failure(403, **{"Retry-After": "7"}))
    assert isinstance(result, dict) and sleeps == [7.0]
    result, sleeps, _ = run(failure(429))
    assert isinstance(result, dict) and sleeps == [5.0]
    result, sleeps, _ = run(failure(502), urllib.error.URLError("offline"), TimeoutError("slow"))
    assert isinstance(result, dict) and sleeps == [5.0, 10.0, 20.0]
    for permanent in (failure(403), failure(403, **{"X-RateLimit-Remaining": "12"}), failure(404), failure(401)):
        result, sleeps, left = run(permanent, failure(429))
        assert isinstance(result, urllib.error.HTTPError) and result.code == permanent.code and not sleeps and left == 1
    result, sleeps, _ = run(*[failure(403, **{"X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "99999"})] * 5, limit=200)
    assert isinstance(result, ValueError) and "HTTP Error 403" in str(result) and "timed out" in str(result) and sum(sleeps) == 200, (result, sleeps)
    saved_main, saved_open = gate.main, gate.urllib.request.urlopen
    gate.urllib.request.urlopen = lambda request, timeout: (_ for _ in ()).throw(failure(401))
    try: assert saved_main(["--sha", "a" * 40, "--repository", "owner/repo"]) == 2
    finally: gate.urllib.request.urlopen = saved_open


def workflow_jobs(release: str) -> dict[str, str]:
    body = release[release.index("\njobs:\n"):]
    return {match.group(1): match.group(2) for match in re.finditer(r"^  ([A-Za-z0-9_-]+):\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", body, re.MULTILINE | re.DOTALL)}


def job_steps(job: str) -> list[str]:
    return re.split(r"^      - ", job[job.index("    steps:\n"):], flags=re.MULTILINE)[1:]


def valid_release(release: str) -> bool:
    """The invariants the release workflow keeps: a gate before any build, publishing only on a tag, pinned actions."""
    jobs = workflow_jobs(release)
    if set(jobs) != {"meta", "package-linux", "package-macos", "package-windows", "assemble", "publish"}: return False
    if not re.search(r"^permissions: \{\}$", release, re.MULTILINE): return False
    if re.search(r"continue-on-error|\|\| true|--anonymous|pull_request_target", release): return False
    if any(not re.fullmatch(r"[0-9a-f]{40}", action.split("@", 1)[1]) for action in re.findall(r"uses:\s*([^\s]+)", release)): return False
    if any("timeout-minutes:" not in job for job in jobs.values()): return False
    write = [name for name, job in jobs.items() if "contents: write" in job]
    if write != ["publish"]: return False
    checker = 'python3 tools/check-release-gate.py --sha "$GITHUB_SHA" --repository "$GITHUB_REPOSITORY"'
    gate = [step for step in job_steps(jobs["meta"]) if "check-release-gate.py" in step]
    if len(gate) != 1 or "if: github.ref_type == 'tag'" not in gate[0] or "GH_TOKEN: ${{ github.token }}" not in gate[0] or f"run: {checker}" not in gate[0]: return False
    if "actions: read" not in jobs["meta"]: return False
    packagers = {"package-linux": "tools/package-linux.sh", "package-macos": "tools/package-macos.sh", "package-windows": "tools/package-windows.ps1"}
    for name, command in packagers.items():
        job = jobs[name]
        if not re.search(r"^    needs: meta$", job, re.MULTILINE) or re.search(r"^    if:", job, re.MULTILINE): return False
        built = [step for step in job_steps(job) if re.search(rf"run: {re.escape(command)}\b", step)]
        if len(built) != 1 or "if:" in built[0]: return False
    if not re.search(r"^    needs: \[meta, package-linux, package-macos, package-windows\]$", jobs["assemble"], re.MULTILINE): return False
    if "tools/release-assets.py assemble" not in jobs["assemble"] or "tools/collect-native-artifacts.py" not in jobs["assemble"]: return False
    publish = jobs["publish"]
    if not re.search(r"^    if: github\.ref_type == 'tag'$", publish, re.MULTILINE) or not re.search(r"^    needs: \[meta, assemble\]$", publish, re.MULTILINE): return False
    if re.findall(r"^          (?:name|pattern): (\S+)$", publish, re.MULTILINE) != ["release-assets"]: return False
    if "gh release create" not in publish or "--verify-tag" not in publish or "gh release" in "".join(job for name, job in jobs.items() if name != "publish"): return False
    # Diagnostics and receipts upload after a failure; nothing else may run on `always()`.
    for job in jobs.values():
        for step in job_steps(job):
            if "if: always()" in step and "actions/upload-artifact@" not in step: return False
    return True


def graph() -> None:
    release = (ROOT / ".github/workflows/release.yml").read_text()
    assert valid_release(release)
    assert not (ROOT / "dist-workspace.toml").exists() and not (ROOT / ".github/build-setup.yml").exists() and not (ROOT / ".github/workflows/package-native.yml").exists()
    gate_if = "        if: github.ref_type == 'tag'\n        env:\n          GH_TOKEN"
    refused = {
        "gate skipped": release.replace(gate_if, "        if: false\n        env:\n          GH_TOKEN", 1),
        "gate without token": release.replace("          GH_TOKEN: ${{ github.token }}\n        run: python3 tools/check-release-gate.py", "        run: python3 tools/check-release-gate.py", 1),
        "gate anonymous": release.replace("tools/check-release-gate.py --sha", "tools/check-release-gate.py --anonymous --sha", 1),
        "gate tolerated": release.replace('"$GITHUB_REPOSITORY"\n\n  package-linux', '"$GITHUB_REPOSITORY" || true\n\n  package-linux', 1),
        "gate step tolerated": release.replace("      - name: Require exact check run\n", "      - name: Require exact check run\n        continue-on-error: true\n", 1),
        "no actions permission": release.replace("      actions: read\n", "", 1),
        "workflow-wide rights": release.replace("permissions: {}", "permissions:\n  contents: write", 1),
        "assemble may write": release.replace("  assemble:\n    needs: [meta, package-linux, package-macos, package-windows]\n    runs-on: ubuntu-24.04\n    timeout-minutes: 15\n    permissions:\n      contents: read", "  assemble:\n    needs: [meta, package-linux, package-macos, package-windows]\n    runs-on: ubuntu-24.04\n    timeout-minutes: 15\n    permissions:\n      contents: write", 1),
        "publish on pull requests": release.replace("    if: github.ref_type == 'tag'\n    needs: [meta, assemble]", "    needs: [meta, assemble]", 1),
        "publish without verified tag": release.replace(" --verify-tag", "", 1),
        "publish skips assemble": release.replace("    needs: [meta, assemble]", "    needs: [meta]", 1),
        "publish takes native trees": release.replace("          name: release-assets\n          path: release-assets\n      - name: Create GitHub Release", "          pattern: native-*\n          path: release-assets\n      - name: Create GitHub Release", 1),
        "build ahead of the gate": release.replace("  package-macos:\n    needs: meta", "  package-macos:\n    needs: []", 1),
        "build skipped": release.replace("      - name: Build native package\n        run: tools/package-macos.sh", "      - name: Build native package\n        if: false\n        run: tools/package-macos.sh", 1),
        "build tolerated": release.replace("      - name: Build Linux package\n", "      - name: Build Linux package\n        continue-on-error: true\n", 1),
        "assemble ignores a platform": release.replace("[meta, package-linux, package-macos, package-windows]", "[meta, package-linux, package-macos]", 1),
        "unpinned action": release.replace("actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803", "actions/checkout@v6", 1),
        "no timeout": release.replace("    timeout-minutes: 10\n", "", 1),
        "always() on a build step": release.replace("      - name: Check Linux package\n", "      - name: Check Linux package\n        if: always()\n", 1),
        "release created outside publish": release.replace("      - name: Assemble the release\n", "      - name: Assemble the release\n        run: gh release create x\n", 1),
    }
    for name, mutated in refused.items():
        assert mutated != release, f"mutation did not apply: {name}"
        assert not valid_release(mutated), f"accepted: {name}"


def native_identity() -> str:
    match = re.search(r'const APP_ID: &str = "([^"]+)";', (ROOT / "crates/gui/src/native_notify.rs").read_text())
    assert match, "APP_ID not found in the notification adapter"
    return match.group(1)


def assert_native_identity(plist: dict, wix: ET.Element) -> None:
    identity = native_identity()
    assert plist.get("CFBundleIdentifier") == identity, "bundle identifier differs from the notification identity"
    shortcut = next(item for item in wix.iter() if item.tag.endswith("Shortcut"))
    properties = [item.attrib for item in shortcut if item.tag.endswith("ShortcutProperty")]
    assert len(properties) == 1 and properties[0]["Key"] == "System.AppUserModel.ID" and properties[0].get("Value") == identity, "shortcut AppUserModelID differs from the notification identity"


def native_identity_cases(plist: dict, wix: ET.Element) -> None:
    assert_native_identity(plist, wix)
    def plist_with(**changes: str) -> dict:
        mutated = copy.deepcopy(plist); mutated.update(changes); return mutated
    def wix_with(mutate: object) -> ET.Element:
        mutated = copy.deepcopy(wix); mutate(next(item for item in mutated.iter() if item.tag.endswith("Shortcut"))); return mutated
    def drop(shortcut: ET.Element) -> None:
        for item in list(shortcut): shortcut.remove(item)
    def attribute(name: str, value: str) -> object:
        return lambda shortcut: next(item for item in shortcut).set(name, value)
    missing_plist = copy.deepcopy(plist); del missing_plist["CFBundleIdentifier"]
    rejected = [
        (missing_plist, wix), (plist_with(CFBundleIdentifier="org.focusforge.app"), wix), (plist_with(CFBundleIdentifier="org.focusforge.focusforge"), wix),
        (plist, wix_with(drop)), (plist, wix_with(attribute("Value", "org.focusforge.app"))), (plist, wix_with(attribute("Key", "System.AppUserModel.RelaunchCommand"))),
        (plist, wix_with(lambda shortcut: shortcut.append(copy.deepcopy(next(item for item in shortcut))))),
    ]
    for bad_plist, bad_wix in rejected:
        try: assert_native_identity(bad_plist, bad_wix)
        except AssertionError: pass
        else: raise AssertionError("native identity mismatch accepted")


def native_boundaries() -> None:
    plist = plistlib.loads((ROOT / "packaging/macos/Info.plist").read_bytes())
    assert {key: plist[key] for key in ("CFBundleExecutable", "CFBundleIconFile", "CFBundleShortVersionString", "CFBundleVersion")} == {"CFBundleExecutable": "focusforge-gui", "CFBundleIconFile": "focusforge", "CFBundleShortVersionString": "__VERSION__", "CFBundleVersion": "__VERSION__"}
    wix = ET.parse(ROOT / "packaging/windows/focusforge.wxs").getroot()
    package = next(item for item in wix if item.tag.endswith("Package"))
    files = [item for item in wix.iter() if item.tag.endswith("File")]
    root_files = {(item.attrib["Name"], item.attrib["Source"], item.attrib.get("KeyPath")) for item in files if "Name" in item.attrib}
    asset_sources = {item.attrib["Source"] for item in files if item.attrib["Source"].startswith("$(var.CliAssets)")}
    asset_components = [
        component for component in wix.iter() if component.tag.endswith("Component")
        and any(file.attrib.get("Source", "").startswith("$(var.CliAssets)") for file in component if file.tag.endswith("File"))
    ]
    assert wix.tag.endswith("Wix") and "UpgradeCode" in package.attrib and root_files == {("focusforge-cli.exe", "$(var.Cli)", "yes"), ("focusforge-gui.exe", "$(var.Gui)", "yes"), ("LICENSE", "$(var.License)", "yes"), ("NOTICE", "$(var.Notice)", "yes"), ("focusforge.ico", "$(var.Icon)", "yes")}
    assert len(asset_sources) == 19 and "$(var.CliAssets)\\powershell\\completions\\_focusforge-cli.ps1" in asset_sources
    assert len(asset_components) == 19 and all(
        len([file for file in component if file.tag.endswith("File")]) == 1
        and next(file for file in component if file.tag.endswith("File")).attrib.get("KeyPath") == "yes"
        for component in asset_components
    )
    shortcut = next(item for item in wix.iter() if item.tag.endswith("Shortcut")); icon = next(item for item in wix.iter() if item.tag.endswith("Icon"))
    assert shortcut.attrib["Target"] == "[INSTALLFOLDER]focusforge-gui.exe" and shortcut.attrib["Icon"] == "FocusForgeIcon" and icon.attrib == {"Id": "FocusForgeIcon", "SourceFile": "$(var.Icon)"}
    native_identity_cases(plist, wix)
    call(["bash", "-n", str(ROOT / "tools/package-macos.sh"), str(ROOT / "tools/check-package-macos.sh")])
    call(["bash", str(ROOT / "tools/package-macos.sh"), "--output", "/tmp/outside-native-packages"], False)
    call(["python3", "-c", "import pathlib; compile(pathlib.Path(__import__('sys').argv[1]).read_text(), __import__('sys').argv[1], 'exec')", str(NATIVE_COLLECTOR)])
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        native, output = directory / "native", directory / "output"
        old_native, old_output = collector.NATIVE, collector.OUTPUT
        collector.NATIVE, collector.OUTPUT = native, output
        try:
            revision = collector.revision()
            def restore() -> None:
                shutil.rmtree(native, ignore_errors=True)
                symbol_spec = importlib.util.spec_from_file_location("symbol_packager_for_collector", SYMBOL_PACKAGER)
                symbol_module = importlib.util.module_from_spec(symbol_spec)
                assert symbol_spec and symbol_spec.loader
                symbol_spec.loader.exec_module(symbol_module)
                def native_fixture(path: Path, target_name: str, identifier: str, logical: str) -> None:
                    if target_name.endswith("windows-msvc"):
                        guid = __import__("uuid").UUID(hex=identifier[:32]).bytes_le; age = int(identifier[32:], 16)
                        path.write_bytes(msf_pdb(guid, age)); return
                    raw = bytes.fromhex(identifier[:32])
                    dwarf = path / "Contents/Resources/DWARF" / logical; dwarf.parent.mkdir(parents=True); dwarf.write_bytes(struct.pack("<IIIIIIII", 0xFEEDFACF, 0x100000C, 3, 0xA, 1, 24, 0, 0) + struct.pack("<II", 0x1B, 24) + raw)
                for platform, kind, suffix in (("macos", "dmg", "aarch64.dmg"), ("windows", "msi", "x86_64.msi")):
                    target = native / platform; target.mkdir(parents=True)
                    artifact = target / f"FocusForge-{collector.version()}-{suffix}"; artifact.write_bytes(platform.encode())
                    receipt = {"kind": kind, "version": collector.version(), "revision": revision, "artifact": artifact.name, "sha256": collector.sha256(artifact)}
                    (target / "native-receipt.json").write_text(json.dumps(receipt))
                    for symbol_target in {"macos": ("aarch64-apple-darwin",), "windows": ("x86_64-pc-windows-msvc",)}[platform]:
                        symbols = target / "symbols" / symbol_target
                        native_inputs = {}; symbols_by_name = {}
                        module_os, module_cpu, suffix = {"aarch64-apple-darwin": ("Mac", "arm64", ".dSYM"), "x86_64-pc-windows-msvc": ("Windows", "x86_64", ".pdb")}[symbol_target]
                        for name, identifier in (("focusforge-cli", "73DB7F2DB3A643B9DFD8FF6C4DFF41940"), ("focusforge-gui", "05CE7935D101B8E6E397B49D6F6716080")):
                            debug_name = f"{name}{suffix}" if suffix != ".pdb" else f"actual-{name}.pdb"
                            debug = symbols / "native" / debug_name; debug.parent.mkdir(parents=True, exist_ok=True)
                            native_fixture(debug, symbol_target, identifier, name)
                            native_inputs[name] = debug
                            module_name = debug_name if suffix == ".pdb" else name
                            store = symbols / f"store/{module_name}/{identifier}"; store.mkdir(parents=True); symbol = store / f"{module_name}.sym"; symbol.write_text(f"MODULE {module_os} {module_cpu} {identifier} {module_name}\nFILE 0 crates/{name}/src/main.rs\nFUNC 1000 10 0 {name.replace('-', '_')}::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n"); symbols_by_name[name] = [symbol]
                        manifest = symbol_module.write_inventory(symbols, symbol_target, collector.version(), native_inputs, symbols_by_name, fixture_inspections(symbol_module, symbol_target, native_inputs))
                        archive = symbols / f"FocusForge-{collector.version()}-{symbol_target}-symbols.tar.gz"
                        with tarfile.open(archive, "w:gz") as output:
                            for path in sorted([*(symbols / "native").rglob("*"), *(symbols / "store").rglob("*"), manifest]):
                                if path.is_file(): output.add(path, arcname=path.relative_to(symbols))
                        (symbols / "symbols.sha256").write_text(f"{symbol_module.digest(archive)}  {archive.name}\n")
            restore(); collector.collect()
            baseline = {path.name: path.read_bytes() for path in output.iterdir()}
            def reject(mutate: object) -> None:
                restore(); mutate()
                try: collector.collect()
                except (TypeError, ValueError, subprocess.CalledProcessError): pass
                else: raise AssertionError("collector accepted malformed native input")
                assert baseline == {path.name: path.read_bytes() for path in output.iterdir()}
            receipt = native / "macos/native-receipt.json"
            def receipt_field(field: str, value: object) -> object:
                def mutate() -> None:
                    body = json.loads(receipt.read_text()); body[field] = value; receipt.write_text(json.dumps(body))
                return mutate
            for field, value in (("revision", "0" * 40), ("version", "9.9.9"), ("artifact", "wrong.dmg"), ("kind", "msi"), ("sha256", "0" * 64)):
                reject(receipt_field(field, value))
            reject(lambda: receipt.write_text("{"))
            reject(lambda: (native / f"macos/FocusForge-{VERSION}-aarch64.dmg").unlink())
            reject(lambda: shutil.rmtree(native / "macos/symbols/aarch64-apple-darwin"))
            def wrong_target() -> None:
                path = native / "windows/symbols/x86_64-pc-windows-msvc/symbol-inventory.json"
                body = json.loads(path.read_text()); body["platform"] = "aarch64-apple-darwin"; path.write_text(json.dumps(body))
            reject(wrong_target)
            reject(lambda: (native / f"windows/symbols/x86_64-pc-windows-msvc/FocusForge-{VERSION}-x86_64-pc-windows-msvc-symbols.tar.gz").write_bytes(b"corrupt"))
            reject(lambda: (native / "macos/symbols/aarch64-apple-darwin/stale").write_text("stale"))
            def duplicate() -> None:
                nested = native / "native/macos"; nested.mkdir(parents=True)
                shutil.copytree(native / "macos", nested, dirs_exist_ok=True)
            reject(duplicate)
            reject(lambda: receipt.unlink())
        finally:
            collector.NATIVE, collector.OUTPUT = old_native, old_output


def symbol_tool_cases() -> None:
    """Pinned tool metadata and symbol stores reject stale or mismatched IDs."""
    assert SYMBOL_PACKAGER.is_file(), "symbol packager is missing"
    assert SYMBOL_TOOLS.is_file(), "symbol tool pins are missing"
    spec = importlib.util.spec_from_file_location("symbol_packager", SYMBOL_PACKAGER)
    module = importlib.util.module_from_spec(spec)
    assert spec and spec.loader
    spec.loader.exec_module(module)
    symbolicate_spec = importlib.util.spec_from_file_location("symbolicate", ROOT / "tools/symbolicate.py")
    symbolicate = importlib.util.module_from_spec(symbolicate_spec)
    assert symbolicate_spec and symbolicate_spec.loader
    symbolicate_spec.loader.exec_module(symbolicate)
    module.check_config(SYMBOL_TOOLS)
    try:
        module.build(types.SimpleNamespace(output=ROOT, output_root=ROOT))
    except ValueError:
        pass
    else:
        raise AssertionError("symbol packager accepted repository output root")
    with tempfile.TemporaryDirectory() as temporary:
        cache = Path(temporary)
        payload = cache / "tool.tar.xz"
        with tarfile.open(payload, "w:xz") as archive:
            body = cache / "minidump-stackwalk"; body.write_bytes(b"tool"); archive.add(body, arcname="minidump-stackwalk")
        manifest = {"artifacts": {payload.name: {"checksums": {"sha256": module.digest(payload)}, "target_triples": ["x86_64-unknown-linux-gnu"]}}}
        (cache / "stackwalk-0.0-manifest.json").write_text(json.dumps(manifest))
        pins = {"stackwalk": {"version": "0.0", "manifest_sha256": module.digest(cache / "stackwalk-0.0-manifest.json"), "artifact": [{"target": "x86_64-unknown-linux-gnu", "url": f"https://example.invalid/{payload.name}", "sha256": module.digest(payload), "executable": "minidump-stackwalk"}]}}
        original = module.urllib.request.urlopen
        module.urllib.request.urlopen = lambda *_args, **_kwargs: (_ for _ in ()).throw(AssertionError("cached tool attempted network"))
        try:
            assert module.install(pins, "stackwalk", "x86_64-unknown-linux-gnu", cache).is_file()
            payload.write_bytes(b"corrupt")
            try: module.install(pins, "stackwalk", "x86_64-unknown-linux-gnu", cache)
            except AssertionError: pass
            else: raise AssertionError("corrupt cached archive accepted without a verified replacement")
        finally:
            module.urllib.request.urlopen = original
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "native").mkdir()
        native = root / "native/focusforge-cli.debug"; shutil.copyfile("/bin/true", native)
        gui_native = root / "native/focusforge-gui.debug"; shutil.copyfile("/bin/true", gui_native)
        cli_id, gui_id = module.elf_module_id(native), module.elf_module_id(gui_native)
        store = root / f"store/focusforge-cli/{cli_id}"
        store.mkdir(parents=True)
        symbol = store / "focusforge-cli.sym"
        symbol.write_text(f"MODULE Linux x86_64 {cli_id} focusforge-cli\nFILE 0 crates/cli/src/main.rs\nFUNC 1000 10 0 focusforge_cli::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
        gui_store = root / f"store/focusforge-gui/{gui_id}"; gui_store.mkdir(parents=True)
        (gui_store / "focusforge-gui.sym").write_text(f"MODULE Linux x86_64 {gui_id} focusforge-gui\nFILE 0 crates/gui/src/main.rs\nFUNC 1000 10 0 focusforge_gui::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
        fixture_native = {"focusforge-cli": native, "focusforge-gui": gui_native}
        manifest = module.write_inventory(root, "x86_64-unknown-linux-gnu", VERSION, fixture_native, inspections=fixture_inspections(module, "x86_64-unknown-linux-gnu", fixture_native))
        module.verify_inventory(root, manifest)
        symbol.write_text("MODULE Linux x86_64 00000000000000000000000000000000 focusforge-cli\nFILE 0 crates/cli/src/main.rs\nFUNC 1000 10 0 focusforge_cli::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
        try:
            module.verify_inventory(root, manifest)
        except ValueError:
            pass
        else:
            raise AssertionError("mismatched Breakpad module ID accepted")
        symbol.write_text(f"MODULE Linux x86_64 {cli_id} focusforge-cli\nFILE 0 crates/cli/src/main.rs\nFUNC 1000 10 0 focusforge_cli::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
        report = {"modules": [{"filename": "focusforge-cli", "debug_file": "focusforge-cli", "debug_id": cli_id, "loaded_symbols": True}], "crashing_thread": {"frames": [{"function": "focusforge_cli::main", "module": "focusforge-cli", "file": r"C:\work\crates\cli\src\main.rs", "line": 522}]}}
        symbolicate.verify_report(report, module.verify_inventory(root, manifest))
        bad = json.loads(json.dumps(report)); bad["modules"][0]["debug_id"] = "0" * 33
        try: symbolicate.verify_report(bad, module.verify_inventory(root, manifest))
        except ValueError: pass
        else: raise AssertionError("symbolicate accepted a mismatched loaded module")
        bad = json.loads(json.dumps(report)); bad["crashing_thread"]["frames"][0]["line"] = 0
        try: symbolicate.verify_report(bad, module.verify_inventory(root, manifest))
        except ValueError: pass
        else: raise AssertionError("symbolicate accepted a missing application line")
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary); native = {}; binary = {}
        for name in ("focusforge-cli", "focusforge-gui"):
            source = root / "crates" / name.removeprefix("focusforge-") / "src/main.rs"; source.parent.mkdir(parents=True); source.write_text("fn main() {}\n")
            path = root / f"{name}.debug"
            result = subprocess.run(["rustc", "--edition=2021", "--crate-name", name.replace("-", "_"), "-g", "--remap-path-prefix=/home=/rustc", f"--remap-path-prefix={root}=/work", str(source), "-o", str(path)], cwd=root, text=True, capture_output=True)
            assert result.returncode == 0, (result.stdout, result.stderr)
            native[name] = path; binary[name] = path
        identifiers = {name: module.elf_module_id(path) for name, path in native.items()}
        dump_syms = root / "dump_syms"
        dump_syms.write_text("#!/bin/sh\nset -eu\nstore= binary=\nwhile [ $# -gt 0 ]; do if [ \"$1\" = --store ]; then store=$2; shift 2; else binary=$1; shift; fi; done\nname=$(basename \"$binary\" .debug)\ncrate=$(printf '%s' \"$name\" | tr '-' '_')\nid=$(printf '%s\\n' \"$MODULE_IDS\" | tr ',' '\\n' | sed -n \"s/^$name=//p\")\nmkdir -p \"$store/$name/$id\"\nprintf 'MODULE Linux x86_64 %s %s\\nFILE 0 /root/.cargo/registry/src/index.crates.io/dependency/src/lib.rs\\nFUNC 1000 10 0 %s::main\\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\\n' \"$id\" \"$name\" \"$crate\" > \"$store/$name/$id/$name.sym\"\n")
        dump_syms.chmod(0o755)
        original = os.environ.get("MODULE_IDS"); os.environ["MODULE_IDS"] = ",".join(f"{name}={identifier}" for name, identifier in identifiers.items())
        try:
            output = root / "output"
            module.build(types.SimpleNamespace(platform="x86_64-unknown-linux-gnu", version=VERSION, dump_syms=str(dump_syms), output=output, output_root=root, native=[f"{name}={path}" for name, path in native.items()], binary=[f"{name}={path}" for name, path in binary.items()], arch=None, symbols_per_native=1))
        finally:
            if original is None: os.environ.pop("MODULE_IDS", None)
            else: os.environ["MODULE_IDS"] = original
        for name in native:
            symbol = output / "store" / name / identifiers[name] / f"{name}.sym"
            assert "FILE 0 dependency/lib.rs" in symbol.read_text()


def linux_known_main_cases(sealed_container: bool = False) -> None:
    module = symbol_module()
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary); tree, symbols, mock = root / "tree", root / "symbols", root / "mock"; (tree / "bin").mkdir(parents=True); mock.mkdir()
        native: dict[str, Path] = {}; symbol_files: dict[str, list[Path]] = {}
        for name in ("focusforge-cli", "focusforge-gui"):
            crate = name.replace("-", "_"); source = root / f"crates/{name.removeprefix('focusforge-')}/src/main.rs"; source.parent.mkdir(parents=True); source.write_text("fn main() { println!(\"fixture\"); }\n")
            debug = symbols / "native" / f"{name}.debug"; debug.parent.mkdir(parents=True, exist_ok=True)
            result = subprocess.run(["rustc", "--edition=2021", "--crate-name", crate, "-g", "--remap-path-prefix=/home=/rustc", f"--remap-path-prefix={root}=/work", str(source), "-o", str(debug)], cwd=root, text=True, capture_output=True)
            assert result.returncode == 0, (result.stdout, result.stderr)
            shipping = tree / "bin" / name; shutil.copyfile(debug, shipping); call(["objcopy", "--strip-debug", str(shipping)]); call(["objcopy", f"--add-gnu-debuglink={debug}", str(shipping)])
            identifier = module.elf_module_id(debug); symbol = symbols / "store" / name / identifier / f"{name}.sym"; symbol.parent.mkdir(parents=True); symbol.write_text(f"MODULE Linux x86_64 {identifier} {name}\nFILE 0 crates/{name.removeprefix('focusforge-')}/src/main.rs\nFUNC 1000 10 0 {crate}::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
            native[name], symbol_files[name] = debug, [symbol]
        module.write_inventory(symbols, "x86_64-unknown-linux-gnu", VERSION, native, symbol_files, {name: module.inspect_native("x86_64-unknown-linux-gnu", path, name) for name, path in native.items()})
        def validate_main(extra: dict[str, str] | None = None, success: bool = True) -> None:
            env = {**os.environ, "PATH": f"{mock}:{os.environ['PATH']}", "MOCK_PRIVATE_SOURCE": str(root / "private/crates/cli/src/main.rs"), **(extra or {})}
            result = subprocess.run(["bash", "-c", 'source "$1"; validate_main_address focusforge-cli "$2/bin/focusforge-cli" "$3/native/focusforge-cli.debug"; validate_main_address focusforge-gui "$2/bin/focusforge-gui" "$3/native/focusforge-gui.debug"', "selftest", str(CHECKER), str(tree), str(symbols)], text=True, capture_output=True, env=env)
            assert (result.returncode == 0) == success, (result.stdout, result.stderr)
        nm = mock / "nm"; nm.write_text("#!/bin/sh\n/usr/bin/nm \"$@\" | awk 'BEGIN { no=ENVIRON[\"MOCK_NO_MAIN\"]; bad=ENVIRON[\"MOCK_BAD_ADDRESS\"] && ARGV[0] !~ /native/; zero=ENVIRON[\"MOCK_ZERO_ADDRESS\"] } no && $NF ~ /^focusforge_(cli|gui)::main$/ { next } bad && $NF == \"focusforge_cli::main\" { $1=\"deadbeef\" } zero && $NF ~ /^focusforge_(cli|gui)::main$/ { $1=\"0000000000000000\" } { print }'\n"); nm.chmod(0o755)
        addr2line = mock / "addr2line"; addr2line.write_text("#!/bin/sh\ncase \"$*\" in *focusforge-cli.debug*) export MOCK_CLI=1;; esac\n/usr/bin/addr2line \"$@\" | awk 'NR == 2 && ENVIRON[\"MOCK_BAD_SOURCE\"] { print \"wrong.rs\" \":1\"; next } NR == 2 && ENVIRON[\"MOCK_RELATIVE_SOURCE\"] && ENVIRON[\"MOCK_CLI\"] { print \"crates/cli/src/main.rs\" \":1\"; next } NR == 2 && ENVIRON[\"MOCK_PRIVATE_CRATES\"] { print ENVIRON[\"MOCK_PRIVATE_SOURCE\"] \":1\"; next } NR == 2 && ENVIRON[\"MOCK_NON_MAIN_SOURCE\"] { print \"crates/cli/src/lib.rs\" \":1\"; next } NR == 2 && ENVIRON[\"MOCK_TRAVERSAL_SOURCE\"] { print \"crates/cli/../gui/src/main.rs\" \":1\"; next } { print }'\n"); addr2line.chmod(0o755)
        validate_main()
        validate_main({"MOCK_NO_MAIN": "1"}, False)
        validate_main({"MOCK_BAD_ADDRESS": "1"}, False)
        validate_main({"MOCK_BAD_SOURCE": "1"}, False)
        validate_main({"MOCK_ZERO_ADDRESS": "1"}, False)
        validate_main({"MOCK_RELATIVE_SOURCE": "1"})
        validate_main({"MOCK_PRIVATE_CRATES": "1"}, False)
        validate_main({"MOCK_NON_MAIN_SOURCE": "1"}, False)
        validate_main({"MOCK_TRAVERSAL_SOURCE": "1"}, False)
        if not sealed_container:
            return
        image = subprocess.run(["docker", "build", "-q", "-"], input="FROM ubuntu@sha256:5ec03bb3441e8b0bf3b4f9cd4629a1ae763010dc3035bb8da3ae6cf026486401\nRUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq binutils file python3\n", text=True, capture_output=True)
        assert image.returncode == 0, (image.stdout, image.stderr)
        image_name = image.stdout.strip()
        version = subprocess.run(["docker", "run", "--rm", "--network=none", image_name, "python3", "-c", "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"], text=True, capture_output=True)
        assert version.returncode == 0 and version.stdout.strip() == "3.10", (version.stdout, version.stderr)
        def validate_container(caller: str, extra: dict[str, str] | None = None, success: bool = True, missing_config: bool = False) -> None:
            args = ["docker", "run", "--rm", "--network=none", "-v", f"{ROOT}:/product:ro", "-v", f"{CHECKER}:/checks/checker.sh:ro", "-v", f"{tree}:/tree:ro", "-v", f"{symbols}:/symbols:ro", "-v", f"{mock}:/mock:ro", "-e", "PATH=/mock:/usr/bin:/bin"]
            if missing_config:
                overlay = root / "missing-config/packaging"; overlay.mkdir(parents=True, exist_ok=True)
                args.extend(("-v", f"{overlay}:/product/packaging:ro"))
            for key, value in (extra or {}).items(): args.extend(("-e", f"{key}={value}"))
            command = 'source /checks/checker.sh; validate_symbols /tree /symbols' if missing_config else 'test -f /product/packaging/symbol-tools.toml; source /checks/checker.sh; validate_symbols /tree /symbols'
            result = subprocess.run([*args, image_name, "bash", "-c", command], text=True, capture_output=True)
            if missing_config:
                assert result.returncode != 0 and "FileNotFoundError" in result.stderr and "'/product/packaging/symbol-tools.toml'" in result.stderr, (caller, result.stdout, result.stderr)
                return
            assert (result.returncode == 0) == success, (caller, result.stdout, result.stderr)
        validate_container("appimage")
        validate_container("debian")
        validate_container("missing config", missing_config=True)
        original = (tree / "bin/focusforge-cli").read_bytes(); bad = root / "other/focusforge-cli.debug"; bad.parent.mkdir(); bad.write_bytes(native["focusforge-cli"].read_bytes() + b"bad")
        call(["objcopy", "--remove-section", ".gnu_debuglink", str(tree / "bin/focusforge-cli")]); call(["objcopy", f"--add-gnu-debuglink={bad}", str(tree / "bin/focusforge-cli")])
        validate_container("debuglink crc", success=False)
        (tree / "bin/focusforge-cli").write_bytes(original)
        inventory = symbols / "symbol-inventory.json"; original_inventory = inventory.read_bytes()
        native["focusforge-cli"].write_bytes(native["focusforge-cli"].read_bytes() + b"changed")
        validate_container("changed native", success=False)
        native["focusforge-cli"].write_bytes(native["focusforge-cli"].read_bytes()[:-7])
        inventory.write_text('{}')
        validate_container("inventory", success=False)
        inventory.write_bytes(original_inventory)


def sealed_linux_checker_container_cases() -> None:
    linux_known_main_cases(sealed_container=True)


def linux_crash_roundtrip_cases() -> None:
    source = CHECKER.read_text()
    bodies = re.findall(r"^force_crash\(\) \{\n(.*?)^\}", source, re.MULTILINE | re.DOTALL)
    assert len(bodies) == 2
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        for body_index, original_body in enumerate(bodies):
            for artifact in ("missing_dump", "missing_metadata", "missing_both", "complete"):
                crash = root / f"{body_index}-{artifact}"
                symbols = crash / "artifacts/symbols"; symbols.mkdir(parents=True)
                (symbols / f"FocusForge-{VERSION}-x86_64-unknown-linux-gnu-symbols.tar.gz").write_bytes(b"symbols")
                symbolicate = crash / "symbolicate.py"; symbolicate.write_text("raise SystemExit(0)\n")
                body = original_body.replace("/checks/crashes", str(crash / "crashes")).replace("/artifacts/symbols", str(symbols)).replace("/product/tools/symbolicate.py", str(symbolicate))
                for name, arguments in (("focusforge-cli", "algorithms"), ("focusforge-gui", "")):
                    binary = crash / name
                    binary.write_text(f"#!/bin/sh\nname=$(basename \"$0\")\ncase \"$name\" in focusforge-cli) [ \"${{1:-}}\" = algorithms ] || {{ echo parser >&2; exit 64; }};; focusforge-gui) [ \"$#\" -eq 0 ] || {{ echo parser >&2; exit 65; }};; esac\nmkdir -p \"$FOCUSFORGE_DATA_DIR/focusforge/crashes\"\ncase {artifact} in missing_metadata|complete) printf MDMP > \"$FOCUSFORGE_DATA_DIR/focusforge/crashes/crash.dmp\";; esac\ncase {artifact} in missing_dump|complete) printf '{{\"binary\":\"%s\",\"minidump\":\"crash.dmp\",\"pid\":%s,\"version\":\"{VERSION}\",\"timestamp\":\"%s\"}}' \"$name\" \"$$\" \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" > \"$FOCUSFORGE_DATA_DIR/focusforge/crashes/crash.json\";; esac\nexit 1\n")
                    binary.chmod(0o755)
                    command = f"set -euo pipefail; version={VERSION}; force_crash() {{\n{body}\n}}\nforce_crash {name} {binary} {arguments}"
                    result = subprocess.run(["bash", "-c", command], text=True, capture_output=True)
                    if artifact == "complete":
                        assert result.returncode == 0, (result.stdout, result.stderr)
                    else:
                        missing = "dump" if artifact == "missing_both" else artifact.removeprefix("missing_")
                        assert result.returncode != 0 and f"missing crash artifact: {missing}" in result.stderr and "Traceback" not in result.stderr, (result.stdout, result.stderr)


def source_privacy_cases() -> None:
    module = symbol_module()
    assert module.normalized_source("/rust/deps/hashbrown-0.16.1/src/raw/mod.rs") == "dependency/mod.rs"
    for source, expected in (
        ("/root/.cargo/git/checkouts/cv-9d8edb0abe48fcfc/d271a9a/akaze/src/lib.rs", "dependency/lib.rs"),
        ("/out/target/release/build/focusforge-gui/out/app.rs", "dependency/app.rs"),
        ("/usr/include/x86_64-linux-gnu/bits/string_fortified.h", "dependency/string_fortified.h"),
        ("/usr/lib/gcc/x86_64-linux-gnu/11/include/emmintrin.h", "dependency/emmintrin.h"),
    ):
        assert module.normalized_source(source) == expected
    for source in ("/root/.cargo/git/checkouts/../../private/main.rs", "/out/target/../../private/main.rs", "/usr/include-private/secret.h", "/usr/lib/gcc/x86_64-linux-gnu/11/private/secret.h"):
        try: module.normalized_source(source)
        except ValueError: pass
        else: raise AssertionError(f"private dependency source accepted: {source}")
    assert module.safe_source_less_cu("crates/cli/src/main.rs/@/focusforge_cli.fixture")
    assert module.safe_source_less_cu("/root/.cargo/registry/src/index.crates.io/example/src/lib.rs/@/example.fixture")
    for value in ("../private/checkout/@/fixture", "private checkout/src/main.rs/@/fixture", "/private/checkout/src/main.rs/@/fixture", "crates/cli/src/main.rs"):
        assert not module.safe_source_less_cu(value)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        clean_source = root / "crates/cli/src/main.rs"; clean_source.parent.mkdir(parents=True); clean_source.write_text("fn main() { println!(\"fixture\"); }\n")
        clean = root / "clean.debug"
        result = subprocess.run(["rustc", "--edition=2021", "--crate-name", "focusforge_cli", "-g", "--remap-path-prefix=/home=/rustc", f"--remap-path-prefix={root}=/work", str(clean_source), "-o", str(clean)], cwd=root, text=True, capture_output=True)
        assert result.returncode == 0, (result.stdout, result.stderr)
        inspection = module.inspect_native("x86_64-unknown-linux-gnu", clean, "focusforge-cli")
        assert inspection["format"] == "elf-dwarf" and inspection["coverage"]["line_tables"] > 0 and any(path.endswith("crates/cli/src/main.rs") for path in inspection["source_paths"])
        collision = root / "collision.debug"; shutil.copyfile(clean, collision); collision.write_bytes(collision.read_bytes() + b"\0\0/a\0\0")
        assert module.inspect_native("x86_64-unknown-linux-gnu", collision, "focusforge-cli")["source_paths"] == inspection["source_paths"]
        records = root / "records"; records.mkdir(); reader = records / "readelf"
        reader.write_text("#!/usr/bin/env python3\nimport os, sys\nfrom pathlib import Path\nprint(Path(os.environ['SOURCE_PRIVACY_INFO'] if '--debug-dump=info' in sys.argv else os.environ['SOURCE_PRIVACY_LINE']).read_text(), end='')\n")
        reader.chmod(0o755)
        info = """ <0><b>: Abbrev Number: 1 (DW_TAG_compile_unit)
 DW_AT_name : crates/cli/src/main.rs
 DW_AT_comp_dir : /work
 DW_AT_stmt_list : 0
 <0><b0>: Abbrev Number: 1 (DW_TAG_compile_unit)
 DW_AT_name : crates/cli/src/main.rs/@/focusforge_cli.fixture
 DW_AT_comp_dir : /work
 DW_AT_stmt_list : 64
"""
        main_table = """ Offset: 0
 The Directory Table:
 1 /work/crates/cli/src

 The File Name Table:
 1 1 0 0 main.rs

"""
        (records / "info").write_text(info)
        old = dict(os.environ)
        def inspect_records(rawline: str) -> dict:
            (records / "line").write_text(rawline)
            os.environ.update(PATH=f"{records}:{old['PATH']}", SOURCE_PRIVACY_INFO=str(records / "info"), SOURCE_PRIVACY_LINE=str(records / "line"))
            return module.inspect_native("x86_64-unknown-linux-gnu", clean, "focusforge-cli")
        def rejects(label: str, rawline: str, message: str) -> None:
            try: inspect_records(rawline)
            except ValueError as error: assert str(error).startswith(message), (label, error)
            else: raise AssertionError(f"{label} accepted")
        try:
            empty_clean = main_table + """ Offset: 64
 The Directory Table:
 1 /work/crates/cli/src/empty-directory-metadata

 The File Name Table:

"""
            assert "/work/crates/cli/src/empty-directory-metadata" in inspect_records(empty_clean)["source_paths"]
            rejects("private empty codegen directory", empty_clean.replace("/work/crates/cli/src/empty-directory-metadata", "/private checkout.Åsa"), "private source path in native symbols")
            unbound_clean = main_table + """ Offset: 64
 The Directory Table:
 1 /work/crates/cli/src

 The File Name Table:

 Offset: 128
 The Directory Table:
 1 /work/crates/cli/src/unbound-directory-metadata

 The File Name Table:
 1 1 0 0 orphan.rs

"""
            unbound = inspect_records(unbound_clean)["source_paths"]
            assert "/work/crates/cli/src/unbound-directory-metadata" in unbound and "/work/crates/cli/src/unbound-directory-metadata/orphan.rs" in unbound
            rejects("private unbound directory", unbound_clean.replace("/work/crates/cli/src/unbound-directory-metadata", "/private checkout.Åsa"), "private source path in native symbols")
            rejects("unresolved relative unbound directory", unbound_clean.replace("/work/crates/cli/src/unbound-directory-metadata", "relative-source-basis"), "unresolved DWARF line table basis")
        finally:
            os.environ.clear(); os.environ.update(old)
        for suffix in ("private checkout.Åsa", 'private "quoted" checkout.Åsa'):
            private_root = root / suffix; private_root.mkdir(); private_source = private_root / "main.rs"; private_source.write_text("fn main() {}\n")
            private = root / f"{len(suffix)}.debug"; result = subprocess.run(["rustc", "--edition=2021", "--crate-name", "focusforge_cli", "-g", "main.rs", "-o", str(private)], cwd=private_root, text=True, capture_output=True)
            assert result.returncode == 0, (result.stdout, result.stderr)
            try: module.inspect_native("x86_64-unknown-linux-gnu", private, "focusforge-cli")
            except ValueError: pass
            else: raise AssertionError("private DWARF compilation directory accepted")
        symbols = root / "symbols"; native = {"focusforge-cli": clean, "focusforge-gui": collision}
        for name, path in native.items():
            identifier = module.elf_module_id(path); symbol = symbols / "store" / name / identifier / f"{name}.sym"; symbol.parent.mkdir(parents=True); symbol.write_text(f"MODULE Linux x86_64 {identifier} {name}\nFILE 0 crates/{name.removeprefix('focusforge-')}/src/main.rs\nFUNC 1000 10 0 {name.replace('-', '_')}::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
        copied = {}
        for name, source in native.items():
            destination = symbols / "native" / f"{name}.debug"; destination.parent.mkdir(exist_ok=True); shutil.copyfile(source, destination); copied[name] = destination
        inspections = {name: module.inspect_native("x86_64-unknown-linux-gnu", path, name) for name, path in copied.items()}
        manifest = module.write_inventory(symbols, "x86_64-unknown-linux-gnu", VERSION, copied, inspections=inspections)
        module.verify_inventory(symbols, manifest)
        inventory = json.loads(manifest.read_text())
        for label, mutate in (("stale", lambda record: record.__setitem__("native_inspection_sha256", "0" * 64)), ("missing coverage", lambda record: record.pop("coverage")), ("private record", lambda record: record.__setitem__("source_paths", ["/private/checkout/main.rs"]))):
            changed = json.loads(json.dumps(inventory)); mutate(changed["entries"][0]["native_source_inspection"]); manifest.write_text(json.dumps(changed))
            try: module.verify_inventory(symbols, manifest)
            except ValueError: pass
            else: raise AssertionError(f"{label} native source inspection accepted")
        manifest.write_text(json.dumps(inventory))
        changed_native = copied["focusforge-cli"]; changed_native.write_bytes(changed_native.read_bytes() + b"changed"); rehashed = json.loads(json.dumps(inventory)); rehashed["entries"][0]["native_sha256"] = module.digest(changed_native); manifest.write_text(json.dumps(rehashed))
        try: module.verify_inventory(symbols, manifest)
        except ValueError: pass
        else: raise AssertionError("rehash with stale native source inspection accepted")
        shutil.copyfile(clean, changed_native); manifest.write_text(json.dumps(inventory))
        pdb = root / "source.pdb"; pdb.write_bytes(msf_pdb_with_source(bytes(range(16)), 0x20, "/work/crates/cli/src/main.rs"))
        pdb_inspection = module.inspect_windows_native(pdb)
        assert pdb_inspection["source_paths"] == ["/work", "/work/crates/cli/src/main.rs"] and pdb_inspection["coverage"]["module_c13_records"] == 1 and pdb_inspection["coverage"]["environment_records"] == 2
        bare_pdb = root / "no-module-sources.pdb"; bare_pdb.write_bytes(msf_pdb_with_source(bytes(range(16)), 0x20, "/work/crates/cli/src/main.rs", module_sources=False))
        assert "/work/crates/cli/src/main.rs" in module.inspect_windows_native(bare_pdb)["source_paths"]
        linked_pdb = root / "linked-names.pdb"; linked_pdb.write_bytes(msf_pdb_with_source(bytes(range(16)), 0x20, "/work/crates/cli/src/main.rs", names_stream=True))
        assert module.inspect_windows_native(linked_pdb)["source_paths"] == module.inspect_windows_native(pdb)["source_paths"]
        broken_pdb = bytearray(linked_pdb.read_bytes()); broken_pdb[8 * 512:8 * 512 + 4] = bytes(4); linked_pdb.write_bytes(broken_pdb)
        try: module.inspect_windows_native(linked_pdb)
        except ValueError as error: assert "/names" in str(error), error
        else: raise AssertionError("PDB with a damaged /names stream accepted")
        trusted_pdb = root / "trusted-source.pdb"; trusted_pdb.write_bytes(msf_pdb_with_source(bytes(range(16)), 0x20, "C:/work/crates/cli/src/nested space/main.rs"))
        assert "C:/work/crates/cli/src/nested space/main.rs" in module.inspect_windows_native(trusted_pdb)["source_paths"]
        for index, private_source in enumerate(("/private checkout.Åsa/main.rs", "E:/private 用户/main.rs", "//server/private/checkout/main.rs")):
            private_pdb = root / f"private-source-{index}.pdb"; private_pdb.write_bytes(msf_pdb_with_source(bytes(range(16)), 0x20, private_source))
            try: module.inspect_windows_native(private_pdb)
            except ValueError: pass
            else: raise AssertionError("private PDB FileInfo source accepted")
        fpm = bytearray(pdb.read_bytes()); marker_offset = 512 + 64; fpm[marker_offset:marker_offset + 6] = b"/a\0\0\0\0"; fpm_path = root / "fpm-marker.pdb"; fpm_path.write_bytes(fpm)
        assert module.pdb_info(fpm_path) == (bytes(range(16)), 0x20)
        malformed = bytearray(pdb.read_bytes()); end = malformed.find(b"/work/crates/cli/src/main.rs\0\0", 7 * 512); assert end >= 0; malformed[end + len(b"/work/crates/cli/src/main.rs\0")] = ord("x"); malformed_path = root / "malformed-env.pdb"; malformed_path.write_bytes(malformed)
        try: module.inspect_windows_native(malformed_path)
        except ValueError: pass
        else: raise AssertionError("odd CodeView environment fields accepted")
        for platform in ("aarch64-apple-darwin", "x86_64-pc-windows-msvc"):
            portable = root / platform
            if platform.endswith("apple-darwin"):
                archive = write_macos_symbols(portable, platform)
            else:
                shipping = root / "shipping-windows"; shipping.mkdir(); archive = write_windows_symbols(portable, shipping)
            module.verify_archive(portable, archive)
            inventory_path = portable / "symbol-inventory.json"; portable_inventory = json.loads(inventory_path.read_text())
            record = portable_inventory["entries"][0]["native_source_inspection"]
            if platform.endswith("apple-darwin"): record["coverage"]["slices"] = ["x86_64"]
            else: record["native_inspection_sha256"] = "0" * 64
            inventory_path.write_text(json.dumps(portable_inventory))
            try: module.verify_inventory(portable)
            except ValueError: pass
            else: raise AssertionError(f"portable {platform} inspection mutation accepted")


def hosted_runner(module: types.ModuleType, root: str, hosted: bool = True):
    """Pretend the checkout lives at `root`, on a hosted runner or not."""
    import contextlib

    @contextlib.contextmanager
    def scope():
        saved_root, saved_env = module.ROOT, os.environ.get("GITHUB_ACTIONS")
        module.ROOT = Path(root)
        if hosted: os.environ["GITHUB_ACTIONS"] = "true"
        else: os.environ.pop("GITHUB_ACTIONS", None)
        try: yield
        finally:
            module.ROOT = saved_root
            if saved_env is None: os.environ.pop("GITHUB_ACTIONS", None)
            else: os.environ["GITHUB_ACTIONS"] = saved_env
    return scope()


def hosted_runner_path_cases() -> None:
    module = symbol_module()
    windows, macos = "D:/a/FocusForge/FocusForge", "/Users/runner/work/FocusForge/FocusForge"
    accepted = (
        (windows, r"D:\a\FocusForge\FocusForge\crates\gui\src\main.rs", "crates/gui/src/main.rs"),
        (windows, r"d:\a\FocusForge\FocusForge\crates\gui\src\main.rs", "crates/gui/src/main.rs"),
        (windows, r"C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\slint-1.17.1\ui\lib.rs", "dependency/lib.rs"),
        (windows, r"C:\Users\runneradmin\.cargo\git\checkouts\cv-9d8edb0abe48fcfc\d271a9a\akaze\src\lib.rs", "dependency/lib.rs"),
        (windows, "/rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf\\library\\core\\src\\ops\\function.rs", "dependency/function.rs"),
        (windows, r"D:\rustc\90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf\library\core\src\ptr\mod.rs", "dependency/mod.rs"),
        (windows, r"D:\a\FocusForge\FocusForge\target\x86_64-pc-windows-msvc\release\build\focusforge-gui-1\out\app.rs", "dependency/app.rs"),
        (windows, r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\MSVC\14.44.35207\include\vector", "dependency/vector"),
        (windows, r"C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\ucrt\stdio.h", "dependency/stdio.h"),
        (windows, r"D:\a\_work\1\s\src\vctools\crt\vcstartup\src\startup\exe_main.cpp", "dependency/exe_main.cpp"),
        (windows, r"C:\Users\runneradmin\.rustup\toolchains\1.95.0-x86_64-pc-windows-msvc\lib\rustlib\etc\intrinsic.natvis", "dependency/intrinsic.natvis"),
        (windows, r"\\?\D:\a\FocusForge\FocusForge\crates\cli\src\main.rs", "crates/cli/src/main.rs"),
        (windows, r"\\?\C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\slint-1.17.1\ui\lib.rs", "dependency/lib.rs"),
        (macos, f"{macos}/crates/cli/src/main.rs", "crates/cli/src/main.rs"),
        (macos, "/Users/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/clap-4.6.6/src/lib.rs", "dependency/lib.rs"),
        (macos, "/Applications/Xcode_16.4.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX15.5.sdk/usr/include/stdio.h", "dependency/stdio.h"),
        (macos, "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/math.h", "dependency/math.h"),
    )
    rejected = (
        (windows, r"C:\Users\runneradmin\private\main.rs"),
        (windows, r"\\?\C:\Users\runneradmin\private\main.rs"),
        (windows, r"D:\a\FocusForge\FocusForge-other\crates\cli\src\main.rs"),
        (windows, r"D:\a\FocusForge\FocusForge\..\secret\main.rs"),
        (windows, r"D:\a\FocusForge\FocusForge\private\main.rs"),
        (windows, r"C:\Program Files\Microsoft Visual Studio\..\..\Users\runneradmin\secret.h"),
        (macos, "/Users/runner/Documents/secret.rs"),
        (macos, f"{macos}/../private/main.rs"),
        (macos, "/Users/runner/.cargo/registry/src/../../../private/main.rs"),
    )
    for root, source, expected in accepted:
        with hosted_runner(module, root):
            assert module.normalized_source(source) == expected, (source, module.normalized_source(source))
    for root, source in rejected:
        with hosted_runner(module, root):
            try: module.normalized_source(source)
            except ValueError as error: assert source in str(error), (source, str(error))
            else: raise AssertionError(f"private runner source accepted: {source}")
    long_source = "D:/other/" + "x" * 400 + "/main.rs"
    try: module.normalized_source(long_source)
    except ValueError as error:
        shown = str(error).split(": ", 1)[1]
        assert 0 < len(shown) <= 200 and long_source.startswith(shown.removesuffix("...")), shown
    else: raise AssertionError("private long source accepted")
    with tempfile.TemporaryDirectory() as temporary:
        symbol = Path(temporary) / "app.sym"
        good = r"D:\a\FocusForge\FocusForge\crates\gui\src\main.rs"
        symbol.write_text(f"MODULE windows x86_64 {'A' * 33} app.pdb\nFILE 0 {good}\nFILE 1 C:\\Users\\runneradmin\\private\\one.rs\nFILE 2 C:\\Users\\runneradmin\\private\\two.rs\nFUNC 1 1 0 f\n")
        with hosted_runner(module, windows):
            try: module.canonicalize_symbol_sources(symbol)
            except ValueError as error: assert r"C:\Users\runneradmin\private\one.rs" in str(error) and r"C:\Users\runneradmin\private\two.rs" in str(error), str(error)
            else: raise AssertionError("private FILE records accepted")
            symbol.write_text(f"MODULE windows x86_64 {'A' * 33} app.pdb\nFILE 0 {good}\nFUNC 1 1 0 f\n")
            module.canonicalize_symbol_sources(symbol)
        assert "FILE 0 crates/gui/src/main.rs\n" in symbol.read_text()
    pdb_source = r"D:\a\FocusForge\FocusForge\crates\cli\src\main.rs"
    with tempfile.TemporaryDirectory() as temporary:
        pdb = Path(temporary) / "runner.pdb"; pdb.write_bytes(msf_pdb_with_source(bytes(range(16)), 0x20, pdb_source))
        with hosted_runner(module, windows):
            assert "/work/crates/cli/src/main.rs" in module.inspect_windows_native(pdb)["source_paths"]
        with hosted_runner(module, windows, hosted=False):
            try: module.inspect_windows_native(pdb)
            except ValueError as error: assert str(error).startswith("private source path in native symbols: ") and "D:/a/FocusForge/FocusForge/crates/cli/src/main.rs" in str(error), str(error)
            else: raise AssertionError("builder checkout accepted outside a hosted runner")
        backslash = Path(temporary) / "backslash.pdb"; backslash.write_bytes(msf_pdb_with_source(bytes(range(16)), 0x20, r"C:\work\crates\cli\src\main.rs", ("/work", "/work/crates/cli")))
        assert "C:/work/crates/cli/src/main.rs" in module.inspect_windows_native(backslash)["source_paths"]


def macos_source_cases() -> None:
    module = symbol_module()
    grouped = """0x00000000: DW_TAG_compile_unit
 DW_AT_name ("crates/cli/src/one.rs")
 DW_AT_comp_dir ("/work")
 DW_AT_stmt_list (0x00000000)
0x00000020: DW_TAG_compile_unit
 DW_AT_name ("crates/cli/src/two.rs")
 DW_AT_comp_dir ("/work")
 DW_AT_stmt_list (0x00000040)
.debug_line[0x00000000]
include_directories[1] = "/work/crates/cli/src"
file_names[1] = {
 name: "one.rs"
 dir_index: 1
}
.debug_line[0x00000040]
include_directories[1] = "/work/crates/cli/src"
file_names[1] = {
 name: "two.rs"
 dir_index: 1
}
"""
    def inspect(output: str, auxiliary: dict | None = None) -> dict:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); bundle = root / "focusforge-cli.dSYM"; dwarf = bundle / "Contents/Resources/DWARF/focusforge-cli"; dwarf.parent.mkdir(parents=True); dwarf.write_bytes(thin_macho())
            (bundle / "Contents/Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": "focusforge-cli"}))
            if auxiliary is not None:
                resource = bundle / "Contents/Resources/00000000000000000000000000000001.plist"
                resource.write_bytes(plistlib.dumps(auxiliary))
            mock = root / "mock"; mock.mkdir(); dumper = mock / "dwarfdump"
            dumper.write_text("#!/bin/sh\ncat <<'EOF'\n" + output + "EOF\n"); dumper.chmod(0o755)
            old = os.environ["PATH"]; os.environ["PATH"] = str(mock) + ":" + old
            try: return module.inspect_macos_native(bundle, "focusforge-cli")
            finally: os.environ["PATH"] = old
    record = inspect(grouped)
    assert record["coverage"]["slices"] == ["arm64"] and "/work/crates/cli/src/two.rs" in record["source_paths"]
    no_line = grouped.replace(".debug_line", """0x00000060: DW_TAG_compile_unit
 DW_AT_name ("crates/cli/src/no-line.rs")
 DW_AT_comp_dir ("/work")
.debug_line""", 1)
    assert "/work/crates/cli/src/no-line.rs" in inspect(no_line)["source_paths"]
    private_no_line = no_line.replace('DW_AT_name ("crates/cli/src/no-line.rs")\n DW_AT_comp_dir ("/work")', 'DW_AT_name ("crates/cli/src/no-line.rs")\n DW_AT_comp_dir ("/private/checkout")')
    unused_clean = grouped.replace('include_directories[1] = "/work/crates/cli/src"', 'include_directories[1] = "/work/crates/cli/src"\ninclude_directories[2] = "/work/crates/unused"', 1)
    assert "/work/crates/unused" in inspect(unused_clean)["source_paths"]
    unused_private = unused_clean.replace('include_directories[2] = "/work/crates/unused"', 'include_directories[2] = "/private/checkout"')
    empty_table = grouped + ".debug_line[0x00000200]\n.debug_line[0x00000221]\n"
    assert inspect(empty_table)["source_paths"] == inspect(grouped)["source_paths"]
    orphan_table = grouped + '.debug_line[0x00000200]\nfile_names[1] = {\n name: "orphan.rs"\n dir_index: 0\n}\n'
    try: inspect(orphan_table)
    except ValueError as error: assert str(error).startswith("unbound dSYM line table for arm64"), str(error)
    else: raise AssertionError("unreferenced table with files accepted")
    missing_table = grouped.replace(".debug_line[0x00000040]", ".debug_line[0x00000041]")
    for label, output, message in (("private no-line", private_no_line, "private source path in native symbols"), ("unused directory", unused_private, "private source path in native symbols"), ("missing stmt table", missing_table, "missing dSYM line table for arm64")):
        try: inspect(output)
        except ValueError as error: assert str(error).startswith(message), (label, str(error))
        else: raise AssertionError(f"{label} accepted")
    assert inspect(grouped, {"DBGBuildSourcePath": "/work", "DBGSourcePath": "/work/crates/cli/src"})["coverage"]["auxiliary_source_records"] == 1
    assert inspect(grouped, {"DBGSourcePathRemapping": {"/work": "/work/crates/cli/src"}})["coverage"]["auxiliary_source_records"] == 1
    for label, auxiliary in (("private plist", {"DBGBuildSourcePath": "/private/checkout", "DBGSourcePath": "/private/checkout"}), ("malformed plist", {"DBGBuildSourcePath": "/work"}), ("private remap", {"DBGSourcePathRemapping": {"/work": "/private/checkout"}})):
        try: inspect(grouped, auxiliary)
        except ValueError: pass
        else: raise AssertionError(f"{label} accepted")
    runner = "/Users/runner/work/FocusForge/FocusForge"; registry = "/Users/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/foo-1.0.0"
    def real(version: int, directories: list[tuple[int, str]], files: list[tuple[int, str]]) -> str:
        text = f"""0x00000000: Compile Unit: length = 0x00000046, format = DWARF32, version = 0x000{version}, abbr_offset = 0x0000, addr_size = 0x08 (next unit at 0x0000004a)

0x0000000c: DW_TAG_compile_unit
              DW_AT_producer\t("clang LLVM (rustc version 1.95.0)")
              DW_AT_language\t(DW_LANG_Rust)
              DW_AT_name\t("{registry}/src/lib.rs/@/foo.cgu.0")
              DW_AT_stmt_list\t(0x00000000)
              DW_AT_comp_dir\t("{runner}")

0x0000002a:   DW_TAG_namespace
                DW_AT_name\t("foo")

0x00000050: Compile Unit: length = 0x00000020, format = DWARF32, version = 0x000{version}, abbr_offset = 0x0000, addr_size = 0x08 (next unit at 0x00000074)

0x0000005c: DW_TAG_compile_unit
              DW_AT_name\t("crates/cli/src/main.rs/@/focusforge_cli.cgu.0")
              DW_AT_comp_dir\t("{runner}")

.debug_line contents:
debug_line[0x00000000]
Line table prologue:
    total_length: 0x0000004e
          format: DWARF32
         version: {version}
 prologue_length: 0x00000030
 min_inst_length: 1
max_ops_per_inst: 1
 default_is_stmt: 1
       line_base: -5
      line_range: 14
     opcode_base: 13
standard_opcode_lengths[DW_LNS_copy] = 0
"""
        text += "".join(f'include_directories[{index:3}] = "{value}"\n' for index, value in directories)
        for index, (directory, name) in enumerate(files, 1 if version < 5 else 0):
            text += f'file_names[{index:3}]:\n           name: "{name}"\n      dir_index: {directory}\n' + ("   md5_checksum: 00000000000000000000000000000000\n" if version >= 5 else "       mod_time: 0x00000000\n         length: 0x00000000\n")
        return text + "\nAddress            Line   Column File   ISA Discriminator OpIndex Flags\n------------------ ------ ------ ------ --- ------------- ------- -------------\n0x0000000100001000      5      0      1   0             0       0  is_stmt\n"
    core = "/rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/core/src"
    four = real(4, [(1, f"{registry}/src"), (2, core)], [(0, "crates/cli/src/extra.rs"), (1, "lib.rs"), (2, "mod.rs")])
    five = real(5, [(0, runner), (1, f"{registry}/src")], [(0, "crates/cli/src/main.rs"), (1, "lib.rs")])
    lib = "/root/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/foo-1.0.0/src/lib.rs"
    for label, output, expected in (("DWARF 4 directory indexes from 1", four, {lib, f"{core}/mod.rs", "/work/crates/cli/src/extra.rs"}), ("DWARF 5 directory index 0", five, {lib, "/work/crates/cli/src/main.rs"})):
        with hosted_runner(module, runner):
            found = set(inspect(output)["source_paths"])
        assert expected <= found and "/work" in found and all(not path.startswith("/Users/") for path in found), (label, sorted(found))
        with hosted_runner(module, runner, hosted=False):
            try: inspect(output)
            except ValueError as error: assert str(error).startswith("private source path in native symbols: ") and "/Users/runner/" in str(error), (label, str(error))
            else: raise AssertionError(f"{label}: builder checkout accepted outside a hosted runner")
    for label, output in (("DWARF 4 directory past the table", four.replace("dir_index: 2", "dir_index: 7")), ("DWARF 5 directory past the table", five.replace("dir_index: 1", "dir_index: 7"))):
        with hosted_runner(module, runner):
            try: inspect(output)
            except ValueError as error: assert "invalid dSYM line directory for arm64: index 7, 2 directories" in str(error), (label, str(error))
            else: raise AssertionError(f"{label} accepted")


def msf_pdb(guid: bytes, age: int) -> bytearray:
    page_size, page_count = 512, 6
    pdb = bytearray(page_size * page_count)
    pdb[:32] = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0"
    struct.pack_into("<IIIIII", pdb, 32, page_size, 1, page_count, 16, 0, 5)
    struct.pack_into("<I", pdb, 5 * page_size, 3)
    struct.pack_into("<IIII", pdb, 3 * page_size, 2, 0xFFFFFFFF, 28, 4)
    struct.pack_into("<I", pdb, 4 * page_size + 8, age)
    pdb[4 * page_size + 12:4 * page_size + 28] = guid
    return pdb


def msf_pdb_with_source(guid: bytes, age: int, source: str, cwds: tuple[str, ...] = ("/work",), module_sources: bool = True, names_stream: bool = False) -> bytearray:
    # names_stream: the linker moves file names into the PDB-wide /names stream and leaves only checksums in the module.
    page_size, page_count = 512, 9 if names_stream else 8
    pdb = bytearray(page_size * page_count)
    pdb[:32] = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0"
    source_bytes = source.encode() + b"\0"; c13 = struct.pack("<II", 0xF3, len(source_bytes)) + source_bytes + b"\0" * ((-len(source_bytes)) % 4) + struct.pack("<IIIBB", 0xF4, 8, 0, 0, 0) + b"\0\0"
    if not module_sources: c13 = struct.pack("<II", 0xF2, 4) + b"\0" * 4
    if names_stream: c13 = struct.pack("<IIIBB", 0xF4, 8, 1, 0, 0) + b"\0\0"
    environment = b"\0" + b"".join(b"cwd\0" + cwd.encode() + b"\0" for cwd in cwds) + b"src\0" + source_bytes + b"\0"; environment += b"\0" * ((-(4 + len(environment))) % 4); symbol_record = struct.pack("<HH", 2 + len(environment), 0x113D) + environment; symbols = struct.pack("<I", 4) + symbol_record
    module = bytearray(len(symbols) + len(c13)); module[:len(symbols)] = symbols; module[len(symbols):] = c13
    record = bytearray(64); struct.pack_into("<HIII", record, 34, 4, len(symbols), 0, len(c13)); record += b"module\0object\0"; record += b"\0" * ((-len(record)) % 4)
    file_info = struct.pack("<HHHHI", 1, 1, 0, 1, 0) + source_bytes
    dbi_size = 64 + len(record) + len(file_info)
    info_tail = struct.pack("<I", 7) + b"/names\0" + struct.pack("<IIIIIII", 1, 1, 1, 1, 0, 0, 5) if names_stream else b""
    names = struct.pack("<III", 0xEFFEEFFE, 1, 1 + len(source_bytes)) + b"\0" + source_bytes
    directory = struct.pack("<IIIIIIIII", 5, 0xFFFFFFFF, 28 + len(info_tail), 0xFFFFFFFF, dbi_size, len(module), 4, 6, 7)
    if names_stream: directory = struct.pack("<IIIIIIIIII", 6, 0xFFFFFFFF, 28 + len(info_tail), 0xFFFFFFFF, dbi_size, len(module), len(names), 4, 6, 7) + struct.pack("<I", 8)
    struct.pack_into("<IIIIII", pdb, 32, page_size, 1, page_count, len(directory), 0, 5)
    struct.pack_into("<I", pdb, 5 * page_size, 3); pdb[3 * page_size:3 * page_size + len(directory)] = directory
    struct.pack_into("<IIII", pdb, 4 * page_size, 2, 0xFFFFFFFF, 28, 4); struct.pack_into("<I", pdb, 4 * page_size + 8, age); pdb[4 * page_size + 12:4 * page_size + 28] = guid; pdb[4 * page_size + 28:4 * page_size + 28 + len(info_tail)] = info_tail
    if names_stream: pdb[8 * page_size:8 * page_size + len(names)] = names
    dbi = memoryview(pdb)[6 * page_size:7 * page_size]; struct.pack_into("<iII", dbi, 0, -1, 19990903, age); struct.pack_into("<II", dbi, 24, len(record), 0); struct.pack_into("<II", dbi, 32, 0, len(file_info)); dbi[64:64 + len(record)] = record; dbi[64 + len(record):64 + len(record) + len(file_info)] = file_info; pdb[7 * page_size:7 * page_size + len(module)] = module
    return pdb


def windows_identity_cases() -> None:
    spec = importlib.util.spec_from_file_location("symbol_packager_windows", SYMBOL_PACKAGER)
    module = importlib.util.module_from_spec(spec); assert spec and spec.loader; spec.loader.exec_module(module)
    def fixtures(root: Path, guid: bytes, age: int, pdb_guid: bytes | None = None, pdb_age: int | None = None) -> tuple[Path, Path]:
        pe = bytearray(0x700); pe[:2] = b"MZ"; struct.pack_into("<I", pe, 0x3C, 0x80); pe[0x80:0x84] = b"PE\0\0"; struct.pack_into("<H", pe, 0x84, 0x8664); struct.pack_into("<H", pe, 0x86, 1); struct.pack_into("<H", pe, 0x94, 240); struct.pack_into("<H", pe, 0x98, 0x20B); struct.pack_into("<I", pe, 0x98 + 108, 16); struct.pack_into("<II", pe, 0x98 + 112 + 48, 0x1000, 28); struct.pack_into("<IIII", pe, 0x188 + 8, 0x200, 0x1000, 0x200, 0x200); struct.pack_into("<II", pe, 0x200 + 12, 2, 32); struct.pack_into("<I", pe, 0x200 + 24, 0x240); pe[0x240:0x244] = b"RSDS"; pe[0x244:0x254] = guid; struct.pack_into("<I", pe, 0x254, age); pe[0x258:0x260] = b"real.pdb\0"
        pdb = msf_pdb(pdb_guid if pdb_guid is not None else guid, pdb_age if pdb_age is not None else age)
        pe_path, pdb_path = root / "app.exe", root / "app.pdb"; pe_path.write_bytes(pe); pdb_path.write_bytes(pdb); return pe_path, pdb_path
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary); guid = bytes(range(16)); pe, pdb = fixtures(root, guid, 0x20)
        module.verify_windows([f"focusforge-cli={pe},{pdb}"])
        assert module.pe_rsds_record(pe)[2].endswith("real.pdb")
        for changed_guid, changed_age in ((bytes(range(1, 17)), None), (None, 0x21)):
            pe, pdb = fixtures(root, guid, 0x20, changed_guid, changed_age)
            try: module.verify_windows([f"focusforge-cli={pe},{pdb}"])
            except ValueError: pass
            else: raise AssertionError("PE/PDB GUID or age mismatch accepted")
        for name, mutate in (
            ("block-map-fpm1", lambda data: struct.pack_into("<I", data, 52, 1)),
            ("block-map-fpm2", lambda data: struct.pack_into("<I", data, 52, 2)),
            ("directory-fpm1", lambda data: struct.pack_into("<I", data, 5 * 512, 1)),
            ("directory-fpm2", lambda data: struct.pack_into("<I", data, 5 * 512, 2)),
            ("info-fpm1", lambda data: struct.pack_into("<I", data, 3 * 512 + 12, 1)),
            ("info-fpm2", lambda data: struct.pack_into("<I", data, 3 * 512 + 12, 2)),
            ("duplicate-stream", lambda data: (struct.pack_into("<I", data, 44, 24), struct.pack_into("<IIIIII", data, 3 * 512, 3, 0xFFFFFFFF, 28, 28, 4, 4))),
        ):
            invalid = root / f"{name}.pdb"; invalid.write_bytes(msf_pdb(guid, 0x20))
            data = bytearray(invalid.read_bytes()); mutate(data); invalid.write_bytes(data)
            try: module.pdb_info(invalid)
            except ValueError: pass
            else: raise AssertionError(f"PDB allocation accepted: {name}")


def wix_version_cases() -> None:
    pwsh = shutil.which("pwsh")
    assert pwsh, "PowerShell 7+ is required for WiX version cases"
    source = (ROOT / "tools/package-windows.ps1").read_text()
    match = re.search(r"(\$wixVersion = @\(& dotnet \$wix --version\)\nif \(\$LASTEXITCODE.*?\{ throw 'WiX 4\.0\.5 cannot run on this runner' \})", source)
    assert match, "current WiX version check not found"
    with tempfile.TemporaryDirectory() as temp:
        runner = Path(temp) / "wix-version.ps1"
        runner.write_text("param([string]$Case)\n$ErrorActionPreference='Stop'\nfunction dotnet { switch ($Case) { 'good' { '4.0.5+b9b2f1b4' }; 'wrong' { '4.0.6' }; 'empty' { }; 'multiple' { '4.0.5+b9b2f1b4'; 'unexpected' }; 'badexit' { '4.0.5+b9b2f1b4' } }; $global:LASTEXITCODE=if ($Case -eq 'badexit') {1} else {0} }\n$wix='mock.dll'\n" + match.group(1))
        for case, success in (("good", True), ("wrong", False), ("empty", False), ("multiple", False), ("badexit", False)):
            result = subprocess.run([pwsh, "-NoProfile", "-File", str(runner), case], text=True, capture_output=True)
            assert (result.returncode == 0) == success, (case, result.stderr)


def symbol_module() -> types.ModuleType:
    spec = importlib.util.spec_from_file_location("symbol_packager_fixture", SYMBOL_PACKAGER)
    module = importlib.util.module_from_spec(spec)
    assert spec and spec.loader
    spec.loader.exec_module(module)
    return module


def fixture_inspections(module: types.ModuleType, platform: str, native: dict[str, Path]) -> dict[str, dict]:
    coverage = {"compile_units": 1, "line_tables": 1, "source_less_compile_units": 0, "no_line_compile_units": 0} if platform == "x86_64-unknown-linux-gnu" else {"slices": ["arm64"], "auxiliary_source_records": 0} if platform.endswith("apple-darwin") else {"dbi_stream": 3, "file_records": 1, "module_c13_records": 0, "environment_records": 0}
    kind = {"x86_64-unknown-linux-gnu": "elf-dwarf", "aarch64-apple-darwin": "macho-dwarf", "x86_64-pc-windows-msvc": "pdb-dbi"}[platform]
    return {name: {"schema": module.SOURCE_INSPECTION_SCHEMA, "format": kind, "native_inspection_sha256": module.digest(path), "coverage": coverage, "source_paths": [f"/work/crates/{name.removeprefix('focusforge-')}/src/main.rs"]} for name, path in native.items()}


def thin_macho() -> bytes:
    identifier = bytes.fromhex("00000000000000000000000000000001")
    return struct.pack("<IIIIIIII", 0xFEEDFACF, 0x100000C, 3, 0xA, 1, 24, 0, 0) + struct.pack("<II", 0x1B, 24) + identifier


def write_dsym(path: Path, name: str) -> None:
    dwarf = path / "Contents/Resources/DWARF" / name
    dwarf.parent.mkdir(parents=True, exist_ok=True)
    dwarf.write_bytes(thin_macho())
    (path / "Contents/Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": name}))


def write_macos_symbols(root: Path, target: str) -> Path:
    module = symbol_module()
    native: dict[str, Path] = {}
    symbols: dict[str, list[Path]] = {}
    os_name, cpu, _ = module.platform_header(target)
    for name in ("focusforge-cli", "focusforge-gui"):
        debug = root / "native" / f"{name}.dSYM"
        write_dsym(debug, name)
        identifier = module.native_module_id(target, debug, name)
        symbol = root / "store" / name / identifier / f"{name}.sym"
        symbol.parent.mkdir(parents=True, exist_ok=True)
        symbol.write_text(f"MODULE {os_name} {cpu} {identifier} {name}\nFILE 0 crates/{name.removeprefix('focusforge-')}/src/main.rs\nFUNC 1000 10 0 {name.replace('-', '_')}::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
        native[name], symbols[name] = debug, [symbol]
    manifest = module.write_inventory(root, target, VERSION, native, symbols, fixture_inspections(module, target, native))
    archive = root / f"FocusForge-{VERSION}-{target}-symbols.tar.gz"
    with tarfile.open(archive, "w:gz") as output:
        for path in sorted([*(root / "native").rglob("*"), *(root / "store").rglob("*"), manifest]):
            if path.is_file(): output.add(path, arcname=path.relative_to(root))
    module.verify_archive(root, archive)
    (root / "symbols.sha256").write_text(f"{module.digest(archive)}  {archive.name}\n")
    return archive


def write_windows_native(root: Path, name: str, guid: bytes, age: int) -> tuple[Path, Path]:
    pe = bytearray(0x700); pe[:2] = b"MZ"; struct.pack_into("<I", pe, 0x3C, 0x80); pe[0x80:0x84] = b"PE\0\0"; struct.pack_into("<H", pe, 0x84, 0x8664); struct.pack_into("<H", pe, 0x86, 1); struct.pack_into("<H", pe, 0x94, 240); struct.pack_into("<H", pe, 0x98, 0x20B); struct.pack_into("<I", pe, 0x98 + 108, 16); struct.pack_into("<II", pe, 0x98 + 112 + 48, 0x1000, 28); struct.pack_into("<IIII", pe, 0x188 + 8, 0x200, 0x1000, 0x200, 0x200); struct.pack_into("<II", pe, 0x200 + 12, 2, 32); struct.pack_into("<I", pe, 0x200 + 24, 0x240); pe[0x240:0x244] = b"RSDS"; pe[0x244:0x254] = guid; struct.pack_into("<I", pe, 0x254, age); pe[0x258:0x260] = f"{name}.pdb\0".encode()
    pdb = msf_pdb(guid, age)
    exe, debug = root / f"{name}.exe", root / f"{name}.pdb"; exe.write_bytes(pe); debug.write_bytes(pdb)
    return exe, debug


def write_windows_symbols(root: Path, shipping: Path) -> Path:
    module = symbol_module()
    native: dict[str, Path] = {}
    symbols: dict[str, list[Path]] = {}
    for offset, name in enumerate(("focusforge-cli", "focusforge-gui")):
        exe, pdb = write_windows_native(shipping, name, bytes(range(offset * 16, offset * 16 + 16)), 0x20 + offset)
        debug = root / "native" / f"{name}.pdb"; debug.parent.mkdir(parents=True, exist_ok=True); shutil.copyfile(pdb, debug)
        identifier = module.native_module_id("x86_64-pc-windows-msvc", debug, name)
        symbol = root / "store" / debug.name / identifier / f"{debug.stem}.sym"
        symbol.parent.mkdir(parents=True, exist_ok=True)
        symbol.write_text(f"MODULE Windows x86_64 {identifier} {debug.name}\nFILE 0 crates/{name.removeprefix('focusforge-')}/src/main.rs\nFUNC 1000 10 0 {name.replace('-', '_')}::main\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\n")
        native[name], symbols[name] = debug, [symbol]
        assert module.pe_rsds(exe) == module.pdb_info(debug)
    manifest = module.write_inventory(root, "x86_64-pc-windows-msvc", VERSION, native, symbols, fixture_inspections(module, "x86_64-pc-windows-msvc", native))
    archive = root / f"FocusForge-{VERSION}-x86_64-pc-windows-msvc-symbols.tar.gz"
    with tarfile.open(archive, "w:gz") as output:
        for path in sorted([*(root / "native").rglob("*"), *(root / "store").rglob("*"), manifest]):
            if path.is_file(): output.add(path, arcname=path.relative_to(root))
    module.verify_archive(root, archive)
    (root / "symbols.sha256").write_text(f"{module.digest(archive)}  {archive.name}\n")
    return archive


def macos_checker_mock() -> None:
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp); mount = directory / "mount"; app = mount / "FocusForge.app/Contents"; binary = app / "MacOS"; resources = app / "Resources"; mock = directory / "mock"; log = directory / "hdiutil.log"
        binary.mkdir(parents=True); resources.mkdir(); mock.mkdir(); (directory / "tools").mkdir()
        shutil.copy(ROOT / "Cargo.toml", directory / "Cargo.toml")
        shutil.copy(ROOT / "LICENSE", directory / "LICENSE"); shutil.copy(ROOT / "NOTICE", directory / "NOTICE")
        shutil.copytree(ROOT / "assets/build", directory / "assets/build")
        shutil.copytree(ROOT / "assets/man", directory / "assets/man")
        shutil.copytree(ROOT / "assets/completions", directory / "assets/completions")
        shutil.copytree(ROOT / "fixtures/mantis-64", directory / "fixtures/mantis-64")
        shutil.copy(ROOT / "tools/package-symbols.py", directory / "tools/package-symbols.py")
        shutil.copy(ROOT / "tools/package-cli-assets.py", directory / "tools/package-cli-assets.py")
        shutil.copy(ROOT / "tools/symbolicate.py", directory / "tools/symbolicate.py")
        smoke = directory / "tools/smoke-release.sh"
        smoke.write_text("#!/bin/sh\n[ -z \"${MOCK_SMOKE_FAIL:-}\" ] || exit 73\n[ -x \"$1\" ] && [ ! -e \"$2\" ] || exit 74\nmkdir -p \"$2\"\n")
        smoke.chmod(0o755)
        shutil.copytree(ROOT / "packaging", directory / "packaging")
        for name in ("LICENSE", "NOTICE"): shutil.copy(ROOT / name, resources / name)
        shutil.copy(ROOT / "assets/build/focusforge.icns", resources / "focusforge.icns")
        call(["/usr/bin/python3", str(directory / "tools/package-cli-assets.py"), "stage", str(directory), str(resources)])
        version_file, info_file = directory / "version", directory / "info.json"
        version_file.write_text(f"{VERSION}\n")
        info = '{"frame_count":3,"unreadable_count":0,"files":[{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]}]}'
        for name in ("focusforge-cli", "focusforge-gui"):
            path = binary / name; path.write_text(f"#!/bin/sh\nif [ -n \"$FF_FORCE_CRASH\" ]; then case {name} in focusforge-cli) [ \"${{1:-}}\" = algorithms ] || exit 64;; focusforge-gui) [ \"$#\" -eq 0 ] || exit 65;; esac; mkdir -p \"$FOCUSFORGE_DATA_DIR/focusforge/crashes\"; case \"${{MOCK_CRASH_ARTIFACT:-complete}}\" in missing_metadata|complete) printf MDMP > \"$FOCUSFORGE_DATA_DIR/focusforge/crashes/crash.dmp\";; esac; case \"${{MOCK_CRASH_ARTIFACT:-complete}}\" in missing_dump|complete) printf '{{\"binary\":\"{name}\",\"minidump\":\"crash.dmp\",\"pid\":%s,\"version\":\"%s\",\"timestamp\":\"%s\"}}' \"$$\" \"$(cat {version_file})\" \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" > \"$FOCUSFORGE_DATA_DIR/focusforge/crashes/crash.json\";; esac; exit 1; elif [ \"$1\" = --version ]; then echo \"focusforge-cli $(cat {version_file})\"; elif [ \"$1\" = info ]; then cat {info_file}; fi\n"); path.chmod(0o755)
        gui = binary / "focusforge-gui"
        gui.write_text(gui.read_text() + "if [ \"$1\" = --exit-after ]; then while [ $# -gt 0 ]; do if [ \"$1\" = --screenshot ]; then printf '\\211PNG\\r\\n\\032\\n\\000\\000\\000\\rIHDR\\000\\000\\000\\001\\000\\000\\000\\001' > \"$2\"; exit 0; fi; shift; done; exit 64; fi\n")
        symbols = directory / "symbols"
        write_macos_symbols(symbols / "aarch64-apple-darwin", "aarch64-apple-darwin")
        (app / "Info.plist").write_text("stub")
        (app / "_CodeSignature").mkdir(); (app / "_CodeSignature/CodeResources").write_text("sealed\n")
        template = directory / "template"; shutil.copytree(mount / "FocusForge.app", template / "FocusForge.app")
        stackwalk = directory / "stackwalk"; stackwalk.write_text("#!/bin/sh\nexit 0\n"); stackwalk.chmod(0o755)
        bridge = directory / "symbolicate-bridge.py"; bridge.write_text("import importlib.util, json, os, subprocess, sys\nfrom pathlib import Path\nsource = Path(__file__).with_name('tools') / 'symbolicate.py'\nspec = importlib.util.spec_from_file_location('symbolicate_fixture', source); module = importlib.util.module_from_spec(spec); assert spec and spec.loader; spec.loader.exec_module(module)\nmodule._module.install = lambda *_: Path(os.environ['MOCK_STACKWALK'])\nreport = {'modules': [{'filename': 'focusforge-cli', 'debug_file': 'focusforge-cli', 'debug_id': '000000000000000000000000000000010', 'loaded_symbols': True}], 'crashing_thread': {'frames': [{'function': 'focusforge_cli::main', 'module': 'focusforge-cli', 'file': 'crates/cli/src/main.rs', 'line': 1}]}}\nmodule.subprocess.run = lambda *args, **kwargs: subprocess.CompletedProcess(args[0], 0, json.dumps(report), '')\nsys.argv = [str(source), *sys.argv[1:]]; module.main()\n")
        commands = {"codesign": "#!/usr/bin/env bash\napp=${!#}\n[[ -f $app/Contents/_CodeSignature/CodeResources ]] || { echo \"$app: code object is not signed at all\" >&2; exit 1; }\nif [[ $1 == --verify ]]; then printf '%s: valid on disk\\n%s: satisfies its Designated Requirement\\n' \"$app\" \"$app\" >&2; else printf 'Executable=%s/Contents/MacOS/focusforge-gui\\nSignature=adhoc\\nSealed Resources%s\\n' \"$app\" \"${MOCK_SEALED- version=2 rules=13 files=40}\" >&2; fi\n", "hdiutil": f"#!/bin/sh\necho \"$1\" >> {log}\nif [ \"$1\" = attach ]; then cp -R {template}/FocusForge.app \"$5\"/; fi\n", "lipo": "#!/bin/sh\necho \"${MOCK_ARCHES:-arm64}\"\n", "file": "#!/bin/sh\necho 'Mach-O 64-bit executable arm64'\n", "otool": "#!/bin/sh\nexit 0\n", "dwarfdump": "#!/bin/sh\nprintf 'UUID: 00000000-0000-0000-0000-000000000001 (arm64) %s\\n' \"$2\"\n", "python3": f"#!/bin/sh\nif [ \"$1\" = '{directory}/tools/symbolicate.py' ]; then shift; exec /usr/bin/python3 '{bridge}' \"$@\"; fi\nexec /usr/bin/python3 \"$@\"\n", "PlistBuddy": "#!/bin/sh\ncase \"$2\" in *Executable*) echo focusforge-gui;; *ShortVersion*|*BundleVersion*) echo \"${MOCK_PLIST_VERSION:-" + NUMERIC + "}\";; *IconFile*) echo focusforge;; *Identifier*) echo \"${MOCK_BUNDLE_ID-org.focusforge.FocusForge}\";; esac\n"}
        for name, body in commands.items(): path = mock / name; path.write_text(body); path.chmod(0o755)
        info_file.write_text(info)
        checker_copy = directory / "tools/check-package-macos.sh"
        original = (ROOT / "tools/check-package-macos.sh").read_text()
        assert original.count("/usr/libexec/PlistBuddy") == 5
        checker_copy.write_text(original.replace("/usr/libexec/PlistBuddy", str(mock / "PlistBuddy")))
        env = {**os.environ, "PATH": f"{mock}:{os.environ['PATH']}", "MOCK_STACKWALK": str(stackwalk)}
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode == 0, (result.stdout, result.stderr)
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env | {"MOCK_SMOKE_FAIL": "1"})
        assert result.returncode != 0
        for missing in ("missing_dump", "missing_metadata", "missing_both"):
            templates = list((template / "FocusForge.app/Contents/MacOS").iterdir())
            original_binaries = {path: path.read_text() for path in templates}
            for path, text in original_binaries.items(): path.write_text(text.replace("${MOCK_CRASH_ARTIFACT:-complete}", missing))
            result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
            for path, text in original_binaries.items(): path.write_text(text)
            expected = "dump" if missing == "missing_both" else missing.removeprefix("missing_")
            assert result.returncode != 0 and f"missing crash artifact: {expected}" in result.stderr and "Traceback" not in result.stderr, (result.stdout, result.stderr)
        env["MOCK_ARCHES"] = "x86_64"
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        env.pop("MOCK_ARCHES")
        for wrong in ("org.focusforge.app", ""):
            result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env | {"MOCK_BUNDLE_ID": wrong})
            assert result.returncode != 0 and f"invalid bundle identifier: {wrong}" in result.stderr, (result.stdout, result.stderr)
        version_file.write_text("0.1.1\n")
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        version_file.write_text(f"{VERSION}\n")
        template_resources = template / "FocusForge.app/Contents/Resources"
        (template_resources / "focusforge.icns").unlink()
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        shutil.copy(ROOT / "assets/build/focusforge.icns", template_resources / "focusforge.icns")
        (template_resources / "LICENSE").unlink()
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        shutil.copy(ROOT / "LICENSE", template_resources / "LICENSE")
        (symbols / "aarch64-apple-darwin/native/focusforge-cli.dSYM/Contents/Resources/DWARF/focusforge-cli").write_bytes(b"wrong")
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        write_macos_symbols(symbols / "aarch64-apple-darwin", "aarch64-apple-darwin")
        info_file.write_text('{"frame_count":3,"unreadable_count":0,"files":[]}')
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode != 0 and log.read_text().count("detach") == 13
        info_file.write_text(info)
        version_file.write_text(f"{PRERELEASE}\n")
        (directory / "Cargo.toml").write_text(with_prerelease((directory / "Cargo.toml").read_text()))
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode == 0 and "Signature=adhoc" in result.stdout, (result.stdout, result.stderr)
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env | {"MOCK_PLIST_VERSION": PRERELEASE})
        assert result.returncode != 0
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env | {"MOCK_SEALED": "=none"})
        assert result.returncode != 0 and "unsealed bundle resources: FocusForge.app" in result.stderr, (result.stdout, result.stderr)
        shutil.rmtree(template / "FocusForge.app/Contents/_CodeSignature")
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg", str(symbols)], text=True, capture_output=True, env=env)
        assert result.returncode != 0 and "invalid code signature: FocusForge.app" in result.stderr, (result.stdout, result.stderr)


def macos_producer_mock() -> None:
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp); tools, mock = root / "tools", root / "mock"; tools.mkdir(); mock.mkdir()
        shutil.copy(ROOT / "Cargo.toml", root / "Cargo.toml")
        for name in ("LICENSE", "NOTICE"): shutil.copy(ROOT / name, root / name)
        shutil.copytree(ROOT / "assets/build", root / "assets/build")
        shutil.copytree(ROOT / "assets/man", root / "assets/man")
        shutil.copytree(ROOT / "assets/completions", root / "assets/completions")
        shutil.copytree(ROOT / "packaging", root / "packaging")
        shutil.copy(ROOT / "tools/package-symbols.py", tools / "package-symbols.py")
        shutil.copy(ROOT / "tools/package-cli-assets.py", tools / "package-cli-assets.py")
        producer_copy = tools / "package-macos.sh"; producer_copy.write_text((ROOT / "tools/package-macos.sh").read_text())
        macho = root / "macho"; macho.write_bytes(thin_macho())
        dump_syms = mock / "dump_syms"; dump_syms.write_text("#!/usr/bin/env bash\nset -euo pipefail\nbinary=${!#}; store=\nwhile (($#)); do case $1 in --store) store=$2; shift 2;; *) shift;; esac; done\nname=$(basename \"$binary\")\nid=000000000000000000000000000000010\nmkdir -p \"$store/$name/$id\"\nprintf 'MODULE Mac arm64 %s %s\\nFILE 0 crates/%s/src/main.rs\\nFUNC 1000 10 0 %s::main\\nSTACK CFI INIT 1000 10 .cfa: $rsp 8 +\\n' \"$id\" \"$name\" \"${name#focusforge-}\" \"${name//-/_}\" > \"$store/$name/$id/$name.sym\"\n"); dump_syms.chmod(0o755)
        commands = {
            "cargo": "#!/usr/bin/env bash\ntarget=; while (($#)); do if [[ $1 == --target ]]; then target=$2; shift 2; else shift; fi; done; mkdir -p \"target/$target/release\"; cp \"$MOCK_MACHO\" \"target/$target/release/focusforge-cli\"; cp \"$MOCK_MACHO\" \"target/$target/release/focusforge-gui\"\n",
            "lipo": "#!/usr/bin/env bash\necho \"${MOCK_ARCHES:-arm64}\"\n",
            "hdiutil": "#!/usr/bin/env bash\ncount=$(( $(cat \"$MOCK_HDIUTIL_COUNT\" 2>/dev/null || echo 0) + 1 )); echo \"$count\" > \"$MOCK_HDIUTIL_COUNT\"\nargs=(\"$@\"); for ((i = 0; i < $#; i++)); do [[ ${args[i]} == -srcfolder ]] && src=${args[i + 1]}; done; echo \"$(du -sm \"$src\" | cut -f1) $*\" >> \"$MOCK_HDIUTIL_ARGS\"\n[[ -z ${MOCK_PLIST_OUT:-} ]] || cp \"$src/Contents/Info.plist\" \"$MOCK_PLIST_OUT\"\nif ! diff <(cd \"$src\" && find . -type f ! -path './Contents/_CodeSignature/*' -exec sha256sum {} + | sort) \"$src/Contents/_CodeSignature/CodeResources\" >/dev/null 2>&1; then echo 'hdiutil: mock source folder does not match its seal' >&2; exit 78; fi\nif (( count <= ${MOCK_HDIUTIL_FAIL:-0} )); then echo \"hdiutil: create failed - mock reason $count\" >&2; exit 77; fi\ntouch \"${!#}\"\n",
            "codesign": "#!/usr/bin/env bash\nif [[ -n ${MOCK_CODESIGN_FAIL:-} ]]; then echo 'codesign: mock signing error' >&2; exit 9; fi\napp=${!#}; mkdir -p \"$app/Contents/_CodeSignature\"\n(cd \"$app\" && find . -type f ! -path './Contents/_CodeSignature/*' -exec sha256sum {} + | sort) > \"$app/Contents/_CodeSignature/CodeResources\"\n",
            "sleep": "#!/usr/bin/env bash\nexit 0\n",
            "plutil": "#!/usr/bin/env bash\nexit 0\n",
            "shasum": "#!/usr/bin/env bash\nif [[ -n ${MOCK_SHASUM_FAIL:-} ]]; then echo 'shasum: mock read error' >&2; exit 5; fi\nsha256sum \"${!#}\"\n",
            "git": "#!/usr/bin/env bash\necho 0123456789abcdef0123456789abcdef01234567\n",
            "dsymutil": "#!/usr/bin/env bash\nsource=$1; while (($#)); do if [[ $1 == -o ]]; then mkdir -p \"$2/Contents/Resources/DWARF\"; cp \"$MOCK_MACHO\" \"$2/Contents/Resources/DWARF/$(basename \"$source\")\"; printf '<plist version=\"1.0\"><dict/></plist>\\n' > \"$2/Contents/Info.plist\"; exit 0; fi; shift; done\n",
            "strip": "#!/usr/bin/env bash\nwhile (($#)); do if [[ $1 == -o ]]; then cp \"$MOCK_MACHO\" \"$2\"; head -c 8000000 /dev/zero >> \"$2\"; exit 0; fi; shift; done\n",
            "dwarfdump": "#!/usr/bin/env bash\ncase \" $* \" in *' --debug-info '*) root=${MOCK_DWARF_ROOT:-/work}; cat <<EOF\n0x00000000: DW_TAG_compile_unit\n              DW_AT_name (\"$root/crates/cli/src/main.rs\")\n              DW_AT_comp_dir (\"$root\")\n              DW_AT_stmt_list (0x00000000)\n.debug_line[0x00000000]\ninclude_directories[1] = \"$root/crates/cli/src\"\nfile_names[1] = {\n  name: \"main.rs\"\n  dir_index: 1\n}\nEOF\n;; *) printf 'UUID: 00000000-0000-0000-0000-000000000001 (arm64) %s\\n' \"$2\";; esac\n",
            "python3": f"#!/usr/bin/env bash\nif [[ $2 == install ]]; then echo '{dump_syms}'; exit 0; fi\nexec /usr/bin/python3 \"$@\"\n",
            "mv": "#!/usr/bin/env bash\nif [[ ${MOCK_MV_PROMOTION_FAIL:-} && $1 == *.next && $2 == */macos ]]; then exit 77; fi; exec /bin/mv \"$@\"\n",
        }
        for name, body in commands.items():
            path = mock / name; path.write_text(body); path.chmod(0o755)
        env = {**os.environ, "PATH": f"{mock}:{os.environ['PATH']}", "LC_ALL": "C", "MOCK_MACHO": str(macho), "MOCK_HDIUTIL_COUNT": str(root / "hdiutil-count"), "MOCK_HDIUTIL_ARGS": str(root / "hdiutil-args"), "MOCK_PLIST_OUT": str(root / "captured-info.plist")}
        def bundle_versions() -> list[str]:
            plist = plistlib.loads((root / "captured-info.plist").read_bytes())
            return [plist["CFBundleShortVersionString"], plist["CFBundleVersion"]]
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode == 0, result.stderr
        assert bundle_versions() == [NUMERIC, NUMERIC], bundle_versions()
        output = root / "target/distrib/native/macos"
        artifact = output / f"FocusForge-{VERSION}-aarch64.dmg"; receipt = output / "native-receipt.json"
        assert artifact.exists() and receipt.exists()
        call(["/usr/bin/python3", str(tools / "package-symbols.py"), "verify", str(output / "symbols/aarch64-apple-darwin")])
        original = artifact.read_bytes(), receipt.read_bytes()
        env["MOCK_DWARF_ROOT"] = "/private checkout.Åsa"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode != 0 and (artifact.read_bytes(), receipt.read_bytes()) == original
        env.pop("MOCK_DWARF_ROOT")
        env["MOCK_ARCHES"] = "x86_64"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode != 0 and (artifact.read_bytes(), receipt.read_bytes()) == original
        env.pop("MOCK_ARCHES")
        diagnostics = root / ".scratch/package-macos-diagnostics/diagnostics.log"
        counter, arg_log = Path(env["MOCK_HDIUTIL_COUNT"]), Path(env["MOCK_HDIUTIL_ARGS"])
        def hdiutil_sizes() -> tuple[int, list[int]]:
            lines = arg_log.read_text().splitlines()
            sized = [re.search(r" -size (\d+)m ", line) for line in lines]
            assert all(sized), lines
            return int(lines[0].split()[0]), [int(match.group(1)) for match in sized]
        counter.unlink(missing_ok=True); arg_log.unlink(missing_ok=True)
        env["MOCK_HDIUTIL_FAIL"] = "3"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode == 77 and (artifact.read_bytes(), receipt.read_bytes()) == original, result.stderr
        assert "hdiutil" in result.stderr and "77" in result.stderr and "mock reason 3" in result.stderr, result.stderr
        assert "Filesystem" in result.stdout, result.stdout
        logged = diagnostics.read_text()
        assert all(f"mock reason {n}" in logged for n in (1, 2, 3)), logged
        assert "hdiutil create failed with exit status 77 after 3 attempts" in result.stderr, result.stderr
        app_mb, sizes = hdiutil_sizes()
        assert len(sizes) == 3 and sizes[0] >= app_mb + 50 and sizes[0] < sizes[1] < sizes[2], (app_mb, sizes)
        env.pop("MOCK_HDIUTIL_FAIL")
        counter.unlink(missing_ok=True)
        env["MOCK_SHASUM_FAIL"] = "1"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode != 0 and (artifact.read_bytes(), receipt.read_bytes()) == original, result.stderr
        assert any("shasum" in line and "failed with exit status 5" in line for line in result.stderr.splitlines()), result.stderr
        assert "failed with exit status 5" in diagnostics.read_text()
        env.pop("MOCK_SHASUM_FAIL")
        counter.unlink(missing_ok=True)
        env["MOCK_CODESIGN_FAIL"] = "1"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode == 9 and not counter.exists() and (artifact.read_bytes(), receipt.read_bytes()) == original, result.stderr
        assert any("codesign" in line and "failed with exit status 9" in line for line in result.stderr.splitlines()), result.stderr
        assert "failed with exit status 9" in diagnostics.read_text()
        env.pop("MOCK_CODESIGN_FAIL")
        if shutil.which("codesign") is None:
            (mock / "codesign").rename(mock / "codesign.aside")
            result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
            (mock / "codesign.aside").rename(mock / "codesign")
            assert result.returncode == 1 and "missing codesign" in result.stderr, result.stderr
        env["MOCK_MV_PROMOTION_FAIL"] = "1"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode != 0 and (artifact.read_bytes(), receipt.read_bytes()) == original
        env.pop("MOCK_MV_PROMOTION_FAIL")
        counter.unlink(missing_ok=True); arg_log.unlink(missing_ok=True)
        env["MOCK_HDIUTIL_FAIL"] = "1"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode == 0 and artifact.exists() and receipt.exists(), result.stderr
        logged = diagnostics.read_text()
        assert "mock reason 1" in logged and "exit status 77" in logged, logged
        assert len(hdiutil_sizes()[1]) == 2, arg_log.read_text()
        env.pop("MOCK_HDIUTIL_FAIL")
        script = producer_copy.read_text()
        sign = 'codesign --force --deep --sign - "$app"\n'
        stage = 'python3 "$root/tools/package-cli-assets.py" stage "$root" "$app/Contents/Resources"\n'
        for mutated in (script.replace(sign, ""), script.replace(sign, "").replace(stage, sign + stage)):
            assert mutated != script
            producer_copy.write_text(mutated); counter.unlink(missing_ok=True)
            result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
            assert result.returncode == 78 and "does not match its seal" in result.stderr and (artifact.read_bytes(), receipt.read_bytes()) == original, result.stderr
        producer_copy.write_text(script)
        counter.unlink(missing_ok=True)
        cargo_toml = root / "Cargo.toml"
        cargo_toml.write_text(with_prerelease(cargo_toml.read_text()))
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode == 0, result.stderr
        assert bundle_versions() == [NUMERIC, NUMERIC], bundle_versions()
        full = f"FocusForge-{PRERELEASE}-aarch64.dmg"
        assert (output / full).exists() and not artifact.exists()
        receipt_text = (output / "native-receipt.json").read_text()
        assert f'"version":"{PRERELEASE}"' in receipt_text and f'"artifact":"{full}"' in receipt_text, receipt_text
        assert list((output / "symbols/aarch64-apple-darwin").glob(f"FocusForge-{PRERELEASE}-aarch64-apple-darwin-symbols.tar.gz"))


def linux_producer_fresh_root() -> None:
    creator = 'mkdir -p "$root/.scratch"\n'
    script = (ROOT / "tools/package-linux.sh").read_text()
    def run(text: str) -> tuple[subprocess.CompletedProcess, Path, str]:
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp); root, mock, log = base / "root", base / "mock", base / "docker.log"
            (root / "tools").mkdir(parents=True); (root / "packaging").mkdir(); mock.mkdir()
            shutil.copy(ROOT / "Cargo.toml", root / "Cargo.toml")
            shutil.copy(ROOT / "packaging/linux-tools.toml", root / "packaging/linux-tools.toml")
            (root / "tools/package-linux.sh").write_text(text)
            (mock / "git").write_text("#!/usr/bin/env bash\ncase \"$*\" in *rev-parse*) echo 0123456789abcdef0123456789abcdef01234567;; *ls-files*) printf 'Cargo.toml\\0packaging/linux-tools.toml\\0';; *) exit 64;; esac\n")
            (mock / "docker").write_text(f"#!/usr/bin/env bash\necho \"$*\" >> '{log}'\nexit 97\n")
            for name in ("git", "docker"): (mock / name).chmod(0o755)
            env = {**os.environ, "PATH": f"{mock}:{os.environ['PATH']}", "LC_ALL": "C"}
            env.pop("FOCUSFORGE_PACKAGE_REUSE", None)
            assert sorted(path.name for path in root.iterdir()) == ["Cargo.toml", "packaging", "tools"]
            result = subprocess.run(["bash", str(root / "tools/package-linux.sh")], cwd=root, text=True, capture_output=True, env=env)
            return result, root, log.read_text() if log.exists() else ""
    result, root, docker = run(script)
    assert result.returncode == 97 and "mktemp" not in result.stderr, (result.returncode, result.stderr)
    assert docker.count("\n") == 1 and f"-v {root}:/src:ro" in docker, docker
    mutated = script.replace(creator, "", 1)
    assert mutated != script
    result, _, docker = run(mutated)
    assert result.returncode != 0 and "mktemp: failed to create file" in result.stderr and not docker, (result.returncode, result.stderr, docker)


def windows_checker_mock() -> None:
    pwsh = shutil.which("pwsh")
    assert pwsh, "PowerShell is required for the Windows checker mock"
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp) / "space root"; root.mkdir(); tools = root / "tools"; tools.mkdir()
        checker_copy = tools / "check-package-windows.ps1"; checker_copy.write_text((ROOT / "tools/check-package-windows.ps1").read_text())
        shutil.copy(ROOT / "tools/package-symbols.py", tools / "package-symbols.py")
        shutil.copy(ROOT / "tools/symbolicate.py", tools / "symbolicate.py")
        shutil.copytree(ROOT / "packaging", root / "packaging")
        for name in ("Cargo.toml", "LICENSE", "NOTICE"): shutil.copy(ROOT / name, root / name)
        shutil.copytree(ROOT / "assets/build", root / "assets/build")
        shutil.copytree(ROOT / "assets/man", root / "assets/man")
        shutil.copytree(ROOT / "assets/completions", root / "assets/completions")
        shutil.copy(ROOT / "tools/package-cli-assets.py", tools / "package-cli-assets.py")
        smoke = tools / "smoke-release.sh"
        smoke.write_text("#!/bin/sh\n[ -z \"${MOCK_SMOKE_FAIL:-}\" ] || exit 73\n[ -x \"$1\" ] && [ ! -e \"$2\" ] || exit 74\nmkdir -p \"$2\"\n")
        smoke.chmod(0o755)
        shutil.copytree(ROOT / "fixtures/mantis-64", root / "fixtures/mantis-64")
        mock = root / "mock"; mock.mkdir()
        cygpath = mock / "cygpath"; cygpath.write_text("#!/bin/sh\nshift\nprintf '%s\\n' \"$1\"\n"); cygpath.chmod(0o755)
        shipping, template = root / "shipping", root / "symbols-template"; shipping.mkdir()
        write_windows_symbols(template, shipping)
        stackwalk = root / "stackwalk"; stackwalk.write_text("#!/bin/sh\nexit 0\n"); stackwalk.chmod(0o755)
        bridge = root / "symbolicate-bridge.py"; bridge.write_text("import importlib.util, json, os, subprocess, sys, uuid\nfrom pathlib import Path\nsource = Path(__file__).with_name('tools') / 'symbolicate.py'\nspec = importlib.util.spec_from_file_location('symbolicate_fixture', source); module = importlib.util.module_from_spec(spec); assert spec and spec.loader; spec.loader.exec_module(module)\nmodule._module.install = lambda *_: Path(os.environ['MOCK_STACKWALK'])\nidentifier = uuid.UUID(bytes_le=bytes(range(16))).hex.upper() + '20'\nreport = {'modules': [{'filename': 'focusforge-cli.pdb', 'debug_file': 'focusforge-cli.pdb', 'debug_id': identifier, 'loaded_symbols': True}], 'crashing_thread': {'frames': [{'function': 'focusforge_cli::main', 'module': 'focusforge-cli.pdb', 'file': 'crates/cli/src/main.rs', 'line': 1}]}}\nmodule.subprocess.run = lambda *args, **kwargs: subprocess.CompletedProcess(args[0], 0, json.dumps(report), '')\nsys.argv = [str(source), *sys.argv[1:]]; module.main()\n")
        runner = root / "runner.ps1"
        runner.write_text(r'''param([string]$Script, [string]$Msi)
$ErrorActionPreference = 'Stop'
function global:python {
  param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
  $tool = if ($Arguments.Count) { [IO.Path]::GetFileName($Arguments[0]) } else { '' }
  if ($tool -eq 'package-symbols.py' -and $Arguments.Count -gt 1 -and $Arguments[1] -eq 'install') { Write-Output (Join-Path $PSScriptRoot 'stackwalk'); $global:LASTEXITCODE = 0; return }
  if ($tool -eq 'package-symbols.py' -and $Arguments.Count -gt 1 -and $Arguments[1] -eq 'verify-windows') {
    $mapped = @($Arguments[0], $Arguments[1])
    foreach ($argument in ($Arguments[2..($Arguments.Count - 1)] -join ' ' -split '(?=--identity=)' | Where-Object { $_ })) {
      if ($argument -notlike '--identity=*') { $mapped += $argument; continue }
      $name = $argument.Substring(11).Split('=')[0]
      $mapped += "--identity=$name=$(Join-Path $PSScriptRoot ('shipping/' + $name + '.exe')),$(Join-Path $PSScriptRoot ('symbols/native/' + $name + '.pdb'))"
    }
    & /usr/bin/python3 @mapped; $global:LASTEXITCODE = $LASTEXITCODE; return
  }
  if ($tool -eq 'symbolicate.py') { & /usr/bin/python3 (Join-Path $PSScriptRoot 'symbolicate-bridge.py') @($Arguments[1..($Arguments.Count - 1)]); $global:LASTEXITCODE = $LASTEXITCODE; return }
  & /usr/bin/python3 @Arguments; $global:LASTEXITCODE = $LASTEXITCODE
}
function global:msiexec.exe {
  param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
  $product = Join-Path $env:ProgramFiles 'FocusForge'
  $shortcut = Join-Path $env:ProgramData 'Microsoft/Windows/Start Menu/Programs/FocusForge/FocusForge.lnk'
  if ($Arguments[0] -eq '/i') {
    New-Item -ItemType Directory -Force -Path $product, (Split-Path $shortcut) | Out-Null
    @('#!/bin/sh', 'exec bash -c "$(tail -n +4 "$0")" "$0" "$@"', ('#' + 'x' * 400), 'if [[ -n $FF_FORCE_CRASH ]]; then name=$(basename "$0"); if [[ $name == focusforge-cli.exe && ${1:-} != algorithms ]]; then exit 64; fi; if [[ $name == focusforge-gui.exe && $# -ne 0 ]]; then exit 65; fi; printf "%s:%s:%s\\n" "$name" "$#" "${1:-}" >> "$MOCK_INVOCATIONS"; mkdir -p "$FOCUSFORGE_DATA_DIR/focusforge/crashes"; case "${MOCK_CRASH_ARTIFACT:-complete}" in missing_metadata|complete) printf MDMP > "$FOCUSFORGE_DATA_DIR/focusforge/crashes/crash.dmp";; esac; case "${MOCK_CRASH_ARTIFACT:-complete}" in missing_dump|complete) printf "{\"binary\":\"$name\",\"minidump\":\"crash.dmp\",\"pid\":%s,\"version\":\"__VERSION__\",\"timestamp\":\"%s\"}" "$$" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$FOCUSFORGE_DATA_DIR/focusforge/crashes/crash.json";; esac; exit 1; elif [[ $1 == --version ]]; then echo focusforge-cli __VERSION__; elif [[ $1 == info ]]; then printf "%s\n" "$MOCK_INFO"; fi') | Set-Content (Join-Path $product 'focusforge-cli.exe')
    # Line 3 of the mock script (bash rejects NUL bytes in its first 80, hence the sh trampoline) carries a PE header: e_lfanew at 0x3C, 'PE' at 0x80, optional-header Subsystem at 0xDC.
    function Set-MockSubsystem([string]$Path, [byte]$Subsystem) {
      $bytes = [IO.File]::ReadAllBytes($Path)
      $bytes[0x3C] = 0x80; $bytes[0x3D] = 0; $bytes[0x3E] = 0; $bytes[0x3F] = 0
      $bytes[0x80] = 0x50; $bytes[0x81] = 0x45; $bytes[0x82] = 0; $bytes[0x83] = 0
      $bytes[0xDC] = $Subsystem; $bytes[0xDD] = 0
      [IO.File]::WriteAllBytes($Path, $bytes)
    }
    Set-MockSubsystem (Join-Path $product 'focusforge-cli.exe') 3
    & chmod +x (Join-Path $product 'focusforge-cli.exe')
    Copy-Item (Join-Path $product 'focusforge-cli.exe') (Join-Path $product 'focusforge-gui.exe')
    Set-MockSubsystem (Join-Path $product 'focusforge-gui.exe') $(if ($env:MOCK_GUI_CONSOLE) { 3 } else { 2 })
    & chmod +x (Join-Path $product 'focusforge-gui.exe')
    @('if [[ $1 == --exit-after ]]; then while (($#)); do if [[ $1 == --screenshot ]]; then printf "\211PNG\r\n\032\n\000\000\000\rIHDR\000\000\000\001\000\000\000\001" > "$2"; exit 0; fi; shift; done; exit 64; fi') | Add-Content (Join-Path $product 'focusforge-gui.exe')
    Copy-Item (Join-Path $PSScriptRoot 'LICENSE') $product
    Copy-Item (Join-Path $PSScriptRoot 'NOTICE') $product
    Copy-Item (Join-Path $PSScriptRoot 'assets/build/focusforge.ico') $product
    Copy-Item (Join-Path $PSScriptRoot 'assets/build/focusforge.ico') (Join-Path $env:ProgramData 'cache.ico')
    & /usr/bin/python3 (Join-Path $PSScriptRoot 'tools/package-cli-assets.py') stage $PSScriptRoot $product
    if ($LASTEXITCODE -ne 0) { throw 'mock CLI asset staging failed' }
    Set-Content $shortcut shortcut
  } elseif ($Arguments[0] -eq '/x') { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $product, $shortcut }
  $global:LASTEXITCODE = 0
}
function global:New-Object {
  param([string]$ComObject)
  if ($ComObject -eq 'Shell.Application') {
    $application = [pscustomobject]@{}
    $application | Add-Member -MemberType ScriptMethod -Name NameSpace -Value {
      param($Directory)
      $folder = [pscustomobject]@{}
      $folder | Add-Member -MemberType ScriptMethod -Name ParseName -Value {
        param($Name)
        $item = [pscustomobject]@{ Name = $Name }
        $item | Add-Member -MemberType ScriptMethod -Name ExtendedProperty -Value {
          param($Key)
          if ($Key -ne 'System.AppUserModel.ID' -or $this.Name -ne 'FocusForge.lnk') { return '' }
          if ($env:MOCK_AUMID -eq 'absent') { return '' }
          if ($env:MOCK_AUMID) { return $env:MOCK_AUMID }
          'org.focusforge.FocusForge'
        }
        $item
      }
      $folder
    }
    return $application
  }
  if ($ComObject -ne 'WScript.Shell') { throw "unexpected COM object: $ComObject" }
  $shell = [pscustomobject]@{}
  $shell | Add-Member -MemberType ScriptMethod -Name CreateShortcut -Value {
    param($Path)
    $index = if ($env:MOCK_ICON_INDEX) { $env:MOCK_ICON_INDEX } else { '0' }
    [pscustomobject]@{ TargetPath = (Join-Path $env:ProgramFiles 'FocusForge/focusforge-gui.exe'); IconLocation = ((Join-Path $env:ProgramData 'cache.ico') + ',' + $index) }
  }
  $shell
}
if ($env:MOCK_BAD_PDB) { $path = Join-Path $PSScriptRoot 'symbols/native/focusforge-cli.pdb'; $bytes = [IO.File]::ReadAllBytes($path); $bytes[1544] = $bytes[1544] + 1; [IO.File]::WriteAllBytes($path, $bytes) }
if ($env:MOCK_BAD_ID) { $path = Join-Path $PSScriptRoot 'symbols/symbol-inventory.json'; $inventory = Get-Content $path -Raw | ConvertFrom-Json; $inventory.entries[0].native_id = '0' * 33; $inventory | ConvertTo-Json -Depth 8 | Set-Content $path }
if ($env:MOCK_MISSING_PAYLOAD) { Remove-Item (Join-Path $PSScriptRoot 'symbols/native/focusforge-cli.pdb') }
if ($env:MOCK_BAD_ARCHIVE) { Add-Content (Get-ChildItem (Join-Path $PSScriptRoot 'symbols') -Filter '*.tar.gz' | Select-Object -First 1) 'wrong' }
& $Script -Msi $Msi -Symbols (Join-Path $PSScriptRoot 'symbols')
'''.replace("__VERSION__", VERSION))
        info = '{"frame_count":3,"unreadable_count":0,"files":[{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]}]}'
        invocations = root / "invocations"
        env = {**os.environ, "PATH": f"{mock}:{os.environ['PATH']}", "ProgramFiles": str(root / "program-files"), "ProgramData": str(root / "program-data"), "MOCK_INFO": info, "MOCK_STACKWALK": str(stackwalk), "MOCK_INVOCATIONS": str(invocations)}
        for mutation, success in (({}, True), ({"MOCK_SMOKE_FAIL": "1"}, False), ({"MOCK_CRASH_ARTIFACT": "missing_dump"}, False), ({"MOCK_CRASH_ARTIFACT": "missing_metadata"}, False), ({"MOCK_CRASH_ARTIFACT": "missing_both"}, False), ({"MOCK_INFO": '{"frame_count":3,"unreadable_count":0,"files":[]}'}, False), ({"MOCK_ICON_INDEX": "99"}, False), ({"MOCK_BAD_PDB": "1"}, False), ({"MOCK_BAD_ID": "1"}, False), ({"MOCK_MISSING_PAYLOAD": "1"}, False), ({"MOCK_BAD_ARCHIVE": "1"}, False), ({"MOCK_AUMID": "org.focusforge.app"}, False), ({"MOCK_AUMID": "absent"}, False), ({"MOCK_GUI_CONSOLE": "1"}, False)):
            shutil.rmtree(root / "symbols", ignore_errors=True)
            shutil.copytree(template, root / "symbols")
            case_env = env | mutation
            result = subprocess.run([pwsh, "-NoProfile", "-File", str(runner), str(checker_copy), "mock.msi"], text=True, capture_output=True, env=case_env)
            assert (result.returncode == 0) == success, result.stderr
            if "MOCK_CRASH_ARTIFACT" in mutation:
                missing = "dump" if mutation["MOCK_CRASH_ARTIFACT"] == "missing_both" else mutation["MOCK_CRASH_ARTIFACT"].removeprefix("missing_")
                assert f"crash is missing {missing}" in result.stderr, result.stderr
            if "MOCK_AUMID" in mutation:
                assert "invalid Start-menu shortcut AppUserModelID:" in result.stderr, result.stderr
            if "MOCK_GUI_CONSOLE" in mutation:
                assert "focusforge-gui.exe must be a windowed binary (PE subsystem 2)" in result.stderr, result.stderr
            assert not (root / "program-files/FocusForge").exists()
            assert not (root / "program-data/Microsoft/Windows/Start Menu/Programs/FocusForge/FocusForge.lnk").exists()
        assert invocations.read_text().splitlines()[:2] == ["focusforge-cli.exe:1:algorithms", "focusforge-gui.exe:0:"]


def smoke_callers() -> None:
    linux = (ROOT / "tools/check-package-linux.sh").read_text()
    macos = (ROOT / "tools/check-package-macos.sh").read_text()
    windows = (ROOT / "tools/check-package-windows.ps1").read_text()
    release = (ROOT / ".github/workflows/release.yml").read_text()
    jobs = workflow_jobs(release)
    assert linux.count("tools/smoke-release.sh") == 2
    assert '"$tree/bin/focusforge-cli"' in linux and '"/usr/bin/focusforge-cli"' in linux
    assert 'tools/smoke-release.sh" "$app/Contents/MacOS/focusforge-cli"' in macos
    assert "cygpath -u \"$1\"" in windows and "cygpath -u \"$2\"" in windows and "cygpath -u \"$3\"" in windows
    assert "if ($LASTEXITCODE -ne 0) { throw 'installed CLI smoke failed' }" in windows
    for name, builder, checker in (("package-linux", "tools/package-linux.sh", "tools/check-package-linux.sh .scratch/package-linux"), ("package-macos", "tools/package-macos.sh", "tools/check-package-macos.sh"), ("package-windows", "tools/package-windows.ps1", "tools/check-package-windows.ps1")):
        steps = job_steps(jobs[name])
        built = [index for index, step in enumerate(steps) if re.search(rf"run: {re.escape(builder)}\b", step)]
        checked = [index for index, step in enumerate(steps) if re.search(rf"run: {re.escape(checker)}\b", step)]
        assert len(built) == 1 and len(checked) == 1 and built[0] < checked[0], name
        assert "if:" not in steps[checked[0]] and "|| true" not in steps[checked[0]] and "continue-on-error" not in steps[checked[0]]
    upload = "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9"
    receipt_specs = (
        (jobs["package-macos"], "tools/check-package-macos.sh", "package-smoke-receipts-macos", (".scratch/check-package-macos.*/smoke",), True),
        (jobs["package-windows"], "tools/check-package-windows.ps1", "package-smoke-receipts-windows", (".scratch/check-package-windows/*/smoke",), True),
        (jobs["package-linux"], "tools/check-package-linux.sh .scratch/package-linux", "package-smoke-receipts-linux", (".scratch/package-linux/check/result/appimage/smoke", ".scratch/package-linux/check/result/deb-smoke"), False),
    )
    for source, checker, name, roots, launch in receipt_specs:
        checked = source.index(checker)
        step = source.index("- name: Upload package smoke receipts", checked)
        end = source.find("\n      - ", step)
        block = source[step:len(source) if end < 0 else end]
        assert checked < step and f"name: {name}" in block and upload in block
        assert "if: always()" in block and "include-hidden-files: true" in block and "if-no-files-found: warn" in block
        assert "continue-on-error" not in block and "|| true" not in block
        suffixes = ("commands.log", "exit-status", "installed-cli.sha256", "source-os", "logs/*.stdout", "logs/*.stderr", "logs/*.exit")
        paths = {line.strip() for line in block.split("path: |\n", 1)[1].split("          include-hidden-files: true", 1)[0].splitlines() if line.strip()}
        expected_paths = {f"{root}/{suffix}" for root in roots for suffix in suffixes}
        if launch: expected_paths |= {f"{root.rsplit('/smoke', 1)[0]}/launch.png" for root in roots}
        assert paths == expected_paths
    diagnostics_path = ".scratch/package-macos-diagnostics/"
    macos_job = jobs["package-macos"]
    producer = macos_job.index("tools/package-macos.sh --output")
    check = macos_job.index("tools/check-package-macos.sh", producer)
    step = macos_job.index("- name: Upload package build diagnostics", producer)
    block = macos_job[step:macos_job.index("      - name:", step + 1)]
    assert producer < step < check and "if: always()" in block and upload in block
    assert "name: package-build-diagnostics-macos" in block and "include-hidden-files: true" in block and "if-no-files-found: warn" in block
    assert "continue-on-error" not in block and "|| true" not in block
    assert [line.split("path:", 1)[1].strip() for line in block.splitlines() if line.strip().startswith("path:")] == [diagnostics_path]
    assert diagnostics_path.rstrip("/") in (ROOT / "tools/package-macos.sh").read_text()


if __name__ == "__main__":
    producer(); producer_cleanup_mock(); checker(); cli_asset_helper_cases(); gate_checks(); gate_retry_cases(); graph(); native_boundaries(); symbol_tool_cases(); linux_known_main_cases(); linux_crash_roundtrip_cases(); source_privacy_cases(); hosted_runner_path_cases(); macos_source_cases(); windows_identity_cases(); wix_version_cases(); macos_checker_mock(); macos_producer_mock(); linux_producer_fresh_root(); windows_checker_mock(); smoke_callers(); print("packaging selftest: PASS")
