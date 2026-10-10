#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Offline mutation checks for release packaging and publication policy."""
from __future__ import annotations

import ast
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import tempfile
import tomllib
import urllib.error
import xml.etree.ElementTree as ET
import plistlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = ROOT / "tools/package-linux.sh"
CHECKER = ROOT / "tools/check-package-linux.sh"
NATIVE_COLLECTOR = ROOT / "tools/collect-native-artifacts.py"
NATIVE_WORKFLOW = ROOT / ".github/workflows/package-native.yml"
spec = importlib.util.spec_from_file_location("release_gate", ROOT / "tools/check-release-gate.py")
gate = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(gate)
collector_spec = importlib.util.spec_from_file_location("native_collector", NATIVE_COLLECTOR)
collector = importlib.util.module_from_spec(collector_spec)
assert collector_spec and collector_spec.loader
collector_spec.loader.exec_module(collector)


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
        legal = directory / "legal.tsv"
        call([str(PACKAGE), "--write-legal-inputs", str(legal)])
        assert len(legal.read_text().splitlines()) == 11
        legal.write_text("kept\n")
        corrupt = directory / "corrupt.toml"
        corrupt.write_text(pins.read_text().replace('sha256 = "4b3f0a5f0ebbdb798f6db649d01b32ba1518376b6f7a0502b7d92b75cc2c8293"', 'sha256 = "z"'))
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
input_manifest="$out/current-inputs.sha256"; write_input_manifest "$input_manifest"; cp "$input_manifest" "$out/package-inputs.sha256"; printf 'source_revision=%s\nimage=%s\n' "$revision" "$image" > "$out/inventory.txt"; verify_reuse
'''
        out = directory / "out"
        shell(setup, str(PACKAGE), str(out))
        shell(setup + '\nprintf stale >> "$out/FocusForge-$version-x86_64.AppImage"; verify_reuse\n', str(PACKAGE), str(out), success=False)
        shell(setup + '\nrm "$out/package-inputs.sha256"; verify_reuse\n', str(PACKAGE), str(out), success=False)
        shell(setup + '\nprintf stale > "$out/stale.deb"; verify_reuse\n', str(PACKAGE), str(out), success=False)


def fake_appimage(directory: Path) -> tuple[Path, Path, Path]:
    expected, app = directory / "expected", directory / "appimage"
    (expected / "frames").mkdir(parents=True)
    for frame in ("00.tif", "01.tif", "02.tif"): (expected / "frames" / frame).write_bytes(b"")
    for name in ("LICENSE", "NOTICE"): shutil.copy(ROOT / name, expected / name)
    shutil.copy(ROOT / "assets/focusforge.desktop", expected / "focusforge.desktop")
    shutil.copytree(ROOT / "assets/build/hicolor", expected / "hicolor"); (expected / "VERSION").write_text("focusforge-cli 0.1.0\n")
    tree = app / "usr"
    for path in (tree / "bin", tree / "lib", tree / "share/applications", tree / "share/icons", tree / "share/doc/focusforge/native-licenses", tree / "share/doc/focusforge/type2-runtime/notices"): path.mkdir(parents=True, exist_ok=True)
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
    cli.write_text("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'focusforge-cli 0.1.0'; elif [ \"$1\" = --help ]; then echo help; elif [ \"$1\" = info ] && [ \"$2\" = --json ]; then printf '{\"frame_count\":3,\"unreadable_count\":0,\"files\":[{\"name\":\"00.tif\",\"width\":64,\"height\":64,\"samples_per_pixel\":3,\"bits_per_sample\":[16,16,16]},{\"name\":\"01.tif\",\"width\":64,\"height\":64,\"samples_per_pixel\":3,\"bits_per_sample\":[16,16,16]},{\"name\":\"02.tif\",\"width\":64,\"height\":64,\"samples_per_pixel\":3,\"bits_per_sample\":[16,16,16]}]}\\n'; else for n in 00 01 02; do echo \"$2/$n.tif 64x64 3ch 16-bit\"; done; fi\n")
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
        desktop.write_bytes((expected / "focusforge.desktop").read_bytes() + b"X-AppImage-Version=0.1.0\n")
        call(args)
        desktop.write_bytes((expected / "focusforge.desktop").read_bytes() + b"X-AppImage-Version=9.9.9\n"); call(args, False)
        desktop.write_bytes((expected / "focusforge.desktop").read_bytes() + b"X-AppImage-Version=0.1.0\nextra\n"); call(args, False)
        desktop.unlink(); desktop.symlink_to("usr/share/applications/focusforge.desktop")
        app.joinpath("usr/share/doc/focusforge/LICENSE").unlink(); call(args, False)
        shutil.copy(expected / "LICENSE", app / "usr/share/doc/focusforge/LICENSE")
        app.joinpath("focusforge.png").unlink(); app.joinpath("focusforge.png").symlink_to("missing.png"); call(args, False)
        app.joinpath("focusforge.png").unlink(); app.joinpath("focusforge.png").symlink_to("usr/share/icons/hicolor/256x256/apps/focusforge.png")
        icon = app / "usr/share/icons/hicolor/256x256/apps/focusforge.png"; icon.write_bytes(b"wrong"); call(args, False); shutil.copy(expected / "hicolor/256x256/apps/focusforge.png", icon)
        app.joinpath("usr/lib/libfontconfig.so.1").unlink(); call(args, False)
        shutil.copy(Path(shutil.which("true") or "/bin/true"), app / "usr/lib/libfontconfig.so.1")
        cli = app / "usr/bin/focusforge-cli"; cli.write_text(cli.read_text().replace("focusforge-cli 0.1.0", "focusforge-cli 9.9.9")); call(args, False)
        cli.write_text(cli.read_text().replace("focusforge-cli 9.9.9", "focusforge-cli 0.1.0"))
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
        gate.urllib.request.urlopen = lambda request, timeout: (_ for _ in ()).throw(urllib.error.URLError("offline"))
        try: gate.request_pages("https://api.github.com/runs", 1e20, False)
        except urllib.error.URLError: pass
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


def valid_graph(config: str, setup: str, release: str, native: str) -> bool:
    try:
        dist = tomllib.loads(config)
        targets = dist["dist"]["targets"]
        packages = dist["workspace"]["packages"]
        extras = dist["dist"]["extra-artifacts"]
    except (KeyError, TypeError, tomllib.TOMLDecodeError):
        return False
    if set(targets) != {"x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin"} or packages != ["focusforge-cli", "focusforge-gui"]:
        return False
    if dist["dist"].get("local-artifacts-jobs") != ["./package-native"] or dist["dist"].get("pr-run-mode") != "upload": return False
    linux = next((item for item in extras if item.get("build") == ["tools/package-linux.sh"]), None)
    native_extra = next((item for item in extras if item.get("build") == ["python3", "tools/collect-native-artifacts.py"]), None)
    if not linux or not native_extra or len(native_extra.get("artifacts", [])) != 4: return False
    checker = "python3 tools/check-release-gate.py --anonymous --sha \"$GITHUB_SHA\" --repository \"$GITHUB_REPOSITORY\""
    step = setup[setup.find("- name: Require exact check run"):setup.find("- name: Require locked release Cargo profile")]
    if checker not in step or "if: github.event_name != 'pull_request'" not in step or "continue-on-error" in step or "|| true" in step: return False
    if "plan-gate" in release: return False
    local, global_build, host = release.find("build-local-artifacts:"), release.find("build-global-artifacts:"), release.find("\n  host:")
    local_part = release[local:global_build]; global_part = release[global_build:host]; host_part = release[host:release.find("\n  announce:", host)]
    matrix = "matrix: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix }}"
    pinned_actions = [action for action in re.findall(r"uses:\s*['\"]?([^\s'\"]+)", release) if "@" in action]
    condition = "if: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null &&"
    generated_step = local_part[local_part.find("Require exact check run"):local_part.find("Require locked release Cargo profile")]
    run_line = next((line.strip().split(": ", 1)[1] for line in generated_step.splitlines() if line.strip().startswith("run: ")), "")
    try: generated_command = json.loads(run_line)
    except json.JSONDecodeError: return False
    native_actions = re.findall(r"uses:\s*['\"]?([^\s'\"]+)", native)
    def native_job(name: str) -> str | None:
        match = re.search(rf"^  {name}:\n(.*?)(?=^  [A-Za-z0-9_-]+:|\Z)", native, re.MULTILINE | re.DOTALL)
        return match.group(1) if match else None
    native_jobs = {name: native_job(name) for name in ("macos", "windows")}
    producer = {"macos": r"tools/package-macos\.sh(?:\s+--output\s+target/distrib/native/macos)?", "windows": r"tools/package-windows\.ps1(?:\s+-OutputDirectory\s+target/distrib/native/windows)?"}
    def guarded_job(job: str, command: str) -> bool:
        guard = re.search(r"- name: Require exact check run\n\s+if: github\.event_name != 'pull_request'\n\s+shell: \w+\n\s+run: GH_TOKEN= GITHUB_TOKEN= python3 tools/check-release-gate\.py --anonymous --sha \"\$GITHUB_SHA\" --repository \"\$GITHUB_REPOSITORY\"", job)
        header = job[:job.find("    steps:")]
        runs = list(re.finditer(rf"^\s+run:\s*{command}\s*$", job, re.MULTILINE))
        if "if:" in header or "needs:" in header or guard is None or len(runs) != 1 or guard.start() > runs[0].start(): return False
        step_start = job.rfind("      - name:", 0, runs[0].start())
        next_step = job.find("      - name:", runs[0].end())
        producer_step = job[step_start:None if next_step < 0 else next_step]
        return "if:" not in producer_step and "continue-on-error" not in producer_step and "|| true" not in producer_step
    guarded = all(native_jobs[name] is not None and guarded_job(native_jobs[name] or "", producer[name]) for name in producer)
    return min(local, global_build, host, local_part.find("Require exact check run"), local_part.find("dist build")) >= 0 and local_part.find("Require exact check run") < local_part.find("dist build") and generated_command == "GH_TOKEN= GITHUB_TOKEN= " + checker and re.search(r"if: ['\"]github\.event_name != 'pull_request'['\"]", generated_step) is not None and "continue-on-error" not in generated_step and "|| true" not in generated_step and "- custom-package-native" in global_part and "- build-local-artifacts" in host_part and "- custom-package-native" in host_part and "needs.build-local-artifacts.result == 'success'" in host_part and "needs.build-local-artifacts.result == 'skipped'" in host_part and "needs.custom-package-native.result == 'success'" in host_part and "needs.custom-package-native.result == 'skipped'" in host_part and matrix in local_part and condition in local_part and "uses: ./.github/workflows/package-native.yml" in release and len(native_jobs) == 2 and guarded and native.count("if: github.event_name != 'pull_request'") == 2 and native.count("GH_TOKEN= GITHUB_TOKEN= python3 tools/check-release-gate.py --anonymous --sha \"$GITHUB_SHA\" --repository \"$GITHUB_REPOSITORY\"") == 2 and "continue-on-error" not in native and all(re.fullmatch(r"[A-Za-z0-9_./-]+@[0-9a-f]{40}", action) for action in pinned_actions + native_actions)


def graph() -> None:
    config = (ROOT / "dist-workspace.toml").read_text(); setup = (ROOT / ".github/build-setup.yml").read_text()
    release = (ROOT / ".github/workflows/release.yml").read_text(); native = NATIVE_WORKFLOW.read_text(); assert valid_graph(config, setup, release, native)
    assert not valid_graph(config.replace('targets = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin"]', "targets = []"), setup, release, native)
    assert not valid_graph(config.replace('packages = ["focusforge-cli", "focusforge-gui"]', "packages = []"), setup, release, native)
    assert not valid_graph(config, setup.replace("if: github.event_name != 'pull_request'", "if: false", 1), release, native)
    assert not valid_graph(config, setup.replace('- name: Require exact check run', '- name: Require exact check run\n  continue-on-error: true'), release, native)
    assert not valid_graph(config, setup.replace(checker := 'python3 tools/check-release-gate.py --anonymous --sha "$GITHUB_SHA" --repository "$GITHUB_REPOSITORY"', checker + ' || true'), release, native)
    assert not valid_graph(config, setup, release.replace('Require exact check run', 'masked check run', 1), native)
    assert not valid_graph(config, setup, release.replace('- name: "Require exact check run"', '- name: "Require exact check run"\n        continue-on-error: true', 1), native)
    generated_run = next(line for line in release.splitlines() if 'tools/check-release-gate.py' in line)
    assert not valid_graph(config, setup, release.replace(generated_run, generated_run[:-1] + ' || true"', 1), native)
    assert not valid_graph(config, setup, release.replace("needs.build-local-artifacts.result == 'success'", "needs.build-local-artifacts.result == 'failure'", 1), native)
    assert not valid_graph(config, setup, release.replace("needs.custom-package-native.result == 'success'", "needs.custom-package-native.result == 'failure'", 1), native)
    assert not valid_graph(config, setup, release.replace("build-local-artifacts:", "plan-gate:\n  build-local-artifacts:", 1), native)
    assert not valid_graph(config, setup, release.replace("actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803", "actions/checkout@v6", 1), native)
    assert not valid_graph(config, setup, release, native.replace("if: github.event_name != 'pull_request'", "if: false", 1))
    assert not valid_graph(config, setup, release, native.replace("actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803", "actions/checkout@v6", 1))
    assert not valid_graph(config, setup, release.replace("      - custom-package-native\n", "", 1), native)
    for job, command in (("macos", "tools/package-macos.sh --output target/distrib/native/macos"), ("macos", "tools/package-macos.sh"), ("windows", "tools/package-windows.ps1 -OutputDirectory target/distrib/native/windows"), ("windows", "tools/package-windows.ps1")):
        start = native.index(f"  {job}:\n")
        insertion = native.index("      - name: Check native plan", start)
        injected = native[:insertion] + f"      - name: any name\n        shell: bash\n        run: {command}\n" + native[insertion:]
        assert not valid_graph(config, setup, release, injected)
    assert not valid_graph(config, setup, release, native.replace("      - name: Build native package", "      - name: Build native package\n        if: true", 1))
    assert not valid_graph(config, setup, release, native.replace("    runs-on: macos-14", "    if: false\n    runs-on: macos-14").replace("    runs-on: windows-2022", "    if: false\n    runs-on: windows-2022"))
    matrix = "matrix: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix }}"
    assert not valid_graph(config, setup, release.replace(matrix, "matrix: { include: [] }", 1), native)
    def workflow_job(name: str) -> str:
        match = re.search(rf"^  {name}:\n(.*?)(?=^  [A-Za-z0-9_-]+:|\Z)", release, re.MULTILINE | re.DOTALL)
        assert match
        return match.group(1)
    def needs(job: str) -> list[str]:
        match = re.search(r"^    needs:\n((?:      - [A-Za-z0-9_-]+\n)+)", job, re.MULTILINE)
        return re.findall(r"- ([A-Za-z0-9_-]+)", match.group(1)) if match else []
    local, custom, global_job, host, announce = (workflow_job(name) for name in ("build-local-artifacts", "custom-package-native", "build-global-artifacts", "host", "announce"))
    assert needs(local) == ["plan"] and needs(custom) == ["plan"] and needs(global_job) == ["plan", "build-local-artifacts", "custom-package-native"] and needs(host) == ["plan", "build-local-artifacts", "custom-package-native", "build-global-artifacts"] and needs(announce) == ["plan", "host"]
    local_condition = next(line.strip()[4:] for line in local.splitlines() if line.strip().startswith("if: "))
    custom_condition = next(line.strip()[4:] for line in custom.splitlines() if line.strip().startswith("if: "))
    host_condition = next(line.strip()[4:] for line in host.splitlines() if line.strip().startswith("if: "))
    announce_condition = next(line.strip()[4:] for line in announce.splitlines() if line.strip().startswith("if: "))

    def condition(expression: str, values: dict[str, object]) -> bool:
        expression = expression.removeprefix("${{").removesuffix("}}").strip()
        for name, value in sorted(values.items(), key=lambda pair: len(pair[0]), reverse=True):
            expression = expression.replace(name, repr(value))
        expression = expression.replace("&&", " and ").replace("||", " or ")
        tree = ast.parse(expression, mode="eval")
        assert all(isinstance(node, (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.Compare, ast.Eq, ast.NotEq, ast.Constant)) for node in ast.walk(tree))
        return bool(eval(compile(tree, "<workflow condition>", "eval"), {"__builtins__": {}}, {}))

    values = {
        "always()": True,
        "fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null": bool(tomllib.loads(config)["dist"]["targets"]),
        "fromJson(needs.plan.outputs.val).ci.github.pr_run_mode": "skip",
        "needs.plan.outputs.publishing": "true",
        "needs.plan.result": "success",
        "needs.build-local-artifacts.result": "success",
        "needs.custom-package-native.result": "success",
        "needs.build-global-artifacts.result": "success",
    }
    assert condition(local_condition, values) and condition(host_condition, values)
    for refused in ("failure", "cancelled"):
        assert not condition(host_condition, values | {"needs.build-local-artifacts.result": refused})
        assert not condition(host_condition, values | {"needs.build-global-artifacts.result": refused})
        assert not condition(host_condition, values | {"needs.plan.result": refused, "needs.build-local-artifacts.result": "skipped"})
    assert condition(host_condition, values | {"needs.build-local-artifacts.result": "skipped"})
    assert not condition(local_condition, values | {"fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null": False})
    assert not condition(host_condition, values | {"needs.plan.outputs.publishing": "false"})
    assert "- host" in announce and "if: ${{ always() && needs.host.result == 'success' }}" in announce
    assert "if: ${{ needs.plan.outputs.publishing == 'true' || fromJson(needs.plan.outputs.val).ci.github.pr_run_mode == 'upload' }}" in custom
    def states(publishing: bool, pr_mode: str, native_result: str = "success", collector_result: str = "success") -> tuple[str, str, str, str]:
        values = {"always()": True, "fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null": True, "fromJson(needs.plan.outputs.val).ci.github.pr_run_mode": pr_mode, "needs.plan.outputs.publishing": "true" if publishing else "false", "needs.plan.result": "success"}
        local_result = "success" if condition(local_condition, values) else "skipped"
        custom_result = native_result if condition(custom_condition, values) else "skipped"
        results = {"plan": "success", "build-local-artifacts": local_result, "custom-package-native": custom_result}
        global_result = collector_result if all(results[name] == "success" for name in needs(global_job)) else "skipped"
        results["build-global-artifacts"] = global_result
        values |= {f"needs.{name}.result": result for name, result in results.items()}
        host_result = "success" if condition(host_condition, values) else "skipped"
        values["needs.host.result"] = host_result
        return custom_result, global_result, host_result, "success" if condition(announce_condition, values) else "skipped"
    assert states(False, "upload") == ("success", "success", "skipped", "skipped")
    assert states(True, "skip") == ("success", "success", "success", "success")
    assert states(True, "skip", "failure") == ("failure", "skipped", "skipped", "skipped")
    assert states(True, "skip", "cancelled") == ("cancelled", "skipped", "skipped", "skipped")
    assert states(True, "skip", "success", "failure") == ("success", "failure", "skipped", "skipped")


def native_boundaries() -> None:
    plist = plistlib.loads((ROOT / "packaging/macos/Info.plist").read_bytes())
    assert {key: plist[key] for key in ("CFBundleExecutable", "CFBundleIconFile", "CFBundleShortVersionString", "CFBundleVersion")} == {"CFBundleExecutable": "focusforge-gui", "CFBundleIconFile": "focusforge", "CFBundleShortVersionString": "__VERSION__", "CFBundleVersion": "__VERSION__"}
    wix = ET.parse(ROOT / "packaging/windows/focusforge.wxs").getroot()
    package = next(item for item in wix if item.tag.endswith("Package"))
    files = [item for item in wix.iter() if item.tag.endswith("File")]
    assert wix.tag.endswith("Wix") and "UpgradeCode" in package.attrib and {(item.attrib["Name"], item.attrib["Source"], item.attrib.get("KeyPath")) for item in files} == {("focusforge-cli.exe", "$(var.Cli)", "yes"), ("focusforge-gui.exe", "$(var.Gui)", "yes"), ("LICENSE", "$(var.License)", "yes"), ("NOTICE", "$(var.Notice)", "yes"), ("focusforge.ico", "$(var.Icon)", "yes")}
    shortcut = next(item for item in wix.iter() if item.tag.endswith("Shortcut")); icon = next(item for item in wix.iter() if item.tag.endswith("Icon"))
    assert shortcut.attrib["Target"] == "[INSTALLFOLDER]focusforge-gui.exe" and shortcut.attrib["Icon"] == "FocusForgeIcon" and icon.attrib == {"Id": "FocusForgeIcon", "SourceFile": "$(var.Icon)"}
    call(["bash", "-n", str(ROOT / "tools/package-macos.sh"), str(ROOT / "tools/check-package-macos.sh")])
    call(["bash", str(ROOT / "tools/package-macos.sh"), "--output", "/tmp/outside-native-packages"], False)
    call(["python3", "-m", "py_compile", str(NATIVE_COLLECTOR)])
    old_plan = os.environ.get("DIST_PLAN")
    required = [f"FocusForge-{collector.version()}-universal.dmg", f"FocusForge-{collector.version()}-x86_64.msi", "native-artifacts.sha256", "native-artifacts.json"]
    plan = {"releases": [{"app_name": name, "app_version": collector.version(), "artifacts": [*(f"{name}-{target}.zip" for target in collector.TARGETS), *required]} for name in sorted(collector.PACKAGES)]}
    os.environ["DIST_PLAN"] = json.dumps(plan)
    try:
        collector.check_plan()
        plan["releases"][0]["artifacts"].append(f"{plan['releases'][0]['app_name']}-aarch64-pc-windows-msvc.zip")
        os.environ["DIST_PLAN"] = json.dumps(plan)
        try: collector.check_plan()
        except ValueError: pass
        else: raise AssertionError("additional target accepted")
    finally:
        if old_plan is None: os.environ.pop("DIST_PLAN", None)
        else: os.environ["DIST_PLAN"] = old_plan
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        native, output = directory / "native", directory / "output"
        old_native, old_output = collector.NATIVE, collector.OUTPUT
        collector.NATIVE, collector.OUTPUT = native, output
        try:
            revision = collector.revision()
            def restore() -> None:
                shutil.rmtree(native, ignore_errors=True)
                for platform, kind, suffix in (("macos", "dmg", "universal.dmg"), ("windows", "msi", "x86_64.msi")):
                    target = native / platform; target.mkdir(parents=True)
                    artifact = target / f"FocusForge-{collector.version()}-{suffix}"; artifact.write_bytes(platform.encode())
                    receipt = {"kind": kind, "version": collector.version(), "revision": revision, "artifact": artifact.name, "sha256": collector.sha256(artifact)}
                    (target / "native-receipt.json").write_text(json.dumps(receipt))
            restore(); collector.collect()
            baseline = {path.name: path.read_bytes() for path in output.iterdir()}
            def reject(mutate: object) -> None:
                restore(); mutate()
                try: collector.collect()
                except (TypeError, ValueError): pass
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
            reject(lambda: (native / "macos/FocusForge-0.1.0-universal.dmg").unlink())
            def duplicate() -> None:
                nested = native / "native/macos"; nested.mkdir(parents=True)
                shutil.copytree(native / "macos", nested, dirs_exist_ok=True)
            reject(duplicate)
            reject(lambda: receipt.unlink())
        finally:
            collector.NATIVE, collector.OUTPUT = old_native, old_output


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


def macos_checker_mock() -> None:
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp); mount = directory / "mount"; app = mount / "FocusForge.app/Contents"; binary = app / "MacOS"; resources = app / "Resources"; mock = directory / "mock"; log = directory / "hdiutil.log"
        binary.mkdir(parents=True); resources.mkdir(); mock.mkdir()
        shutil.copy(ROOT / "Cargo.toml", directory / "Cargo.toml")
        shutil.copy(ROOT / "LICENSE", directory / "LICENSE"); shutil.copy(ROOT / "NOTICE", directory / "NOTICE")
        shutil.copytree(ROOT / "assets/build", directory / "assets/build")
        shutil.copytree(ROOT / "fixtures/mantis-64", directory / "fixtures/mantis-64")
        for name in ("LICENSE", "NOTICE"): shutil.copy(ROOT / name, resources / name)
        shutil.copy(ROOT / "assets/build/focusforge.icns", resources / "focusforge.icns")
        version_file, info_file = directory / "version", directory / "info.json"
        version_file.write_text("0.1.0\n")
        for name in ("focusforge-cli", "focusforge-gui"):
            path = binary / name; path.write_text(f"#!/bin/sh\nif [ \"$1\" = --version ]; then echo \"focusforge-cli $(cat {version_file})\"; elif [ \"$1\" = info ]; then cat {info_file}; fi\n"); path.chmod(0o755)
        (app / "Info.plist").write_text("stub")
        template = directory / "template"; shutil.copytree(mount / "FocusForge.app", template / "FocusForge.app")
        commands = {"hdiutil": f"#!/bin/sh\necho \"$1\" >> {log}\nif [ \"$1\" = attach ]; then cp -R {template}/FocusForge.app \"$5\"/; fi\n", "lipo": "#!/bin/sh\necho \"${MOCK_ARCHES:-x86_64 arm64}\"\n", "file": "#!/bin/sh\necho 'Mach-O universal binary with 2 architectures: [x86_64] [arm64]'\n", "PlistBuddy": "#!/bin/sh\ncase \"$2\" in *Executable*) echo focusforge-gui;; *ShortVersion*|*BundleVersion*) echo 0.1.0;; *IconFile*) echo focusforge;; esac\n"}
        for name, body in commands.items(): path = mock / name; path.write_text(body); path.chmod(0o755)
        info = '{"frame_count":3,"unreadable_count":0,"files":[{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]}]}'
        info_file.write_text(info)
        (directory / "tools").mkdir()
        checker_copy = directory / "tools/check-package-macos.sh"
        original = (ROOT / "tools/check-package-macos.sh").read_text()
        assert original.count("/usr/libexec/PlistBuddy") == 4
        checker_copy.write_text(original.replace("/usr/libexec/PlistBuddy", str(mock / "PlistBuddy")))
        env = {**os.environ, "PATH": f"{mock}:{os.environ['PATH']}"}
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg"], text=True, capture_output=True, env=env)
        assert result.returncode == 0, result.stderr
        env["MOCK_ARCHES"] = "x86_64"
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg"], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        env.pop("MOCK_ARCHES")
        version_file.write_text("0.1.1\n")
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg"], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        version_file.write_text("0.1.0\n")
        template_resources = template / "FocusForge.app/Contents/Resources"
        (template_resources / "focusforge.icns").unlink()
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg"], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        shutil.copy(ROOT / "assets/build/focusforge.icns", template_resources / "focusforge.icns")
        (template_resources / "LICENSE").unlink()
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg"], text=True, capture_output=True, env=env)
        assert result.returncode != 0
        shutil.copy(ROOT / "LICENSE", template_resources / "LICENSE")
        info_file.write_text('{"frame_count":3,"unreadable_count":0,"files":[]}')
        result = subprocess.run(["bash", str(checker_copy), "dummy.dmg"], text=True, capture_output=True, env=env)
        assert result.returncode != 0 and log.read_text().count("detach") == 6


def macos_producer_mock() -> None:
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp); tools, mock = root / "tools", root / "mock"; tools.mkdir(); mock.mkdir()
        shutil.copy(ROOT / "Cargo.toml", root / "Cargo.toml")
        for name in ("LICENSE", "NOTICE"): shutil.copy(ROOT / name, root / name)
        shutil.copytree(ROOT / "assets/build", root / "assets/build")
        shutil.copytree(ROOT / "packaging/macos", root / "packaging/macos")
        producer_copy = tools / "package-macos.sh"; producer_copy.write_text((ROOT / "tools/package-macos.sh").read_text())
        commands = {
            "cargo": "#!/usr/bin/env bash\ntarget=; while (($#)); do if [[ $1 == --target ]]; then target=$2; shift 2; else shift; fi; done; mkdir -p \"target/$target/release\"; touch \"target/$target/release/focusforge-cli\" \"target/$target/release/focusforge-gui\"\n",
            "lipo": "#!/usr/bin/env bash\nif [[ $1 == -archs ]]; then echo \"${MOCK_ARCHES:-x86_64 arm64}\"; else while (($#)); do if [[ $1 == -output ]]; then touch \"$2\"; exit 0; fi; shift; done; fi\n",
            "hdiutil": "#!/usr/bin/env bash\n[[ -z ${MOCK_HDIUTIL_FAIL:-} ]] || exit 77; touch \"${!#}\"\n",
            "plutil": "#!/usr/bin/env bash\nexit 0\n",
            "shasum": "#!/usr/bin/env bash\nprintf '%064d  %s\\n' 0 \"${!#}\"\n",
            "git": "#!/usr/bin/env bash\necho 0123456789abcdef0123456789abcdef01234567\n",
            "mv": "#!/usr/bin/env bash\nif [[ ${MOCK_MV_PROMOTION_FAIL:-} && $1 == *.next && $2 == */macos ]]; then exit 77; fi; exec /bin/mv \"$@\"\n",
        }
        for name, body in commands.items():
            path = mock / name; path.write_text(body); path.chmod(0o755)
        env = {**os.environ, "PATH": f"{mock}:{os.environ['PATH']}"}
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode == 0, result.stderr
        output = root / "target/distrib/native/macos"
        artifact = output / "FocusForge-0.1.0-universal.dmg"; receipt = output / "native-receipt.json"
        assert artifact.exists() and receipt.exists()
        original = artifact.read_bytes(), receipt.read_bytes()
        env["MOCK_ARCHES"] = "x86_64"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode != 0 and (artifact.read_bytes(), receipt.read_bytes()) == original
        env.pop("MOCK_ARCHES")
        env["MOCK_HDIUTIL_FAIL"] = "1"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode == 77 and (artifact.read_bytes(), receipt.read_bytes()) == original
        env.pop("MOCK_HDIUTIL_FAIL")
        env["MOCK_MV_PROMOTION_FAIL"] = "1"
        result = subprocess.run(["bash", str(producer_copy)], cwd=root, text=True, capture_output=True, env=env)
        assert result.returncode != 0 and (artifact.read_bytes(), receipt.read_bytes()) == original


def windows_checker_mock() -> None:
    pwsh = shutil.which("pwsh")
    assert pwsh, "PowerShell is required for the Windows checker mock"
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp); tools = root / "tools"; tools.mkdir()
        checker_copy = tools / "check-package-windows.ps1"; checker_copy.write_text((ROOT / "tools/check-package-windows.ps1").read_text())
        for name in ("Cargo.toml", "LICENSE", "NOTICE"): shutil.copy(ROOT / name, root / name)
        shutil.copytree(ROOT / "assets/build", root / "assets/build")
        shutil.copytree(ROOT / "fixtures/mantis-64", root / "fixtures/mantis-64")
        runner = root / "runner.ps1"
        runner.write_text(r'''param([string]$Script, [string]$Msi)
$ErrorActionPreference = 'Stop'
function global:msiexec.exe {
  param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
  $product = Join-Path $env:ProgramFiles 'FocusForge'
  $shortcut = Join-Path $env:ProgramData 'Microsoft/Windows/Start Menu/Programs/FocusForge/FocusForge.lnk'
  if ($Arguments[0] -eq '/i') {
    New-Item -ItemType Directory -Force -Path $product, (Split-Path $shortcut) | Out-Null
    @('#!/usr/bin/env bash', 'if [[ $1 == --version ]]; then echo focusforge-cli 0.1.0; elif [[ $1 == info ]]; then printf "%s\n" "$MOCK_INFO"; fi') | Set-Content (Join-Path $product 'focusforge-cli.exe')
    & chmod +x (Join-Path $product 'focusforge-cli.exe')
    Set-Content (Join-Path $product 'focusforge-gui.exe') gui
    Copy-Item (Join-Path $PSScriptRoot 'LICENSE') $product
    Copy-Item (Join-Path $PSScriptRoot 'NOTICE') $product
    Copy-Item (Join-Path $PSScriptRoot 'assets/build/focusforge.ico') $product
    Copy-Item (Join-Path $PSScriptRoot 'assets/build/focusforge.ico') (Join-Path $env:ProgramData 'cache.ico')
    Set-Content $shortcut shortcut
  } elseif ($Arguments[0] -eq '/x') { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $product, $shortcut }
  $global:LASTEXITCODE = 0
}
function global:New-Object {
  param([string]$ComObject)
  if ($ComObject -ne 'WScript.Shell') { throw "unexpected COM object: $ComObject" }
  $shell = [pscustomobject]@{}
  $shell | Add-Member -MemberType ScriptMethod -Name CreateShortcut -Value {
    param($Path)
    $index = if ($env:MOCK_ICON_INDEX) { $env:MOCK_ICON_INDEX } else { '0' }
    [pscustomobject]@{ TargetPath = (Join-Path $env:ProgramFiles 'FocusForge/focusforge-gui.exe'); IconLocation = ((Join-Path $env:ProgramData 'cache.ico') + ',' + $index) }
  }
  $shell
}
& $Script -Msi $Msi
''')
        info = '{"frame_count":3,"unreadable_count":0,"files":[{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]},{"width":64,"height":64,"samples_per_pixel":3,"bits_per_sample":[16,16,16]}]}'
        env = {**os.environ, "ProgramFiles": str(root / "program-files"), "ProgramData": str(root / "program-data"), "MOCK_INFO": info}
        for mutation, success in (({}, True), ({"MOCK_INFO": '{"frame_count":3,"unreadable_count":0,"files":[]}'}, False), ({"MOCK_ICON_INDEX": "99"}, False)):
            case_env = env | mutation
            result = subprocess.run([pwsh, "-NoProfile", "-File", str(runner), str(checker_copy), "mock.msi"], text=True, capture_output=True, env=case_env)
            assert (result.returncode == 0) == success, result.stderr
            assert not (root / "program-files/FocusForge").exists()
            assert not (root / "program-data/Microsoft/Windows/Start Menu/Programs/FocusForge/FocusForge.lnk").exists()


if __name__ == "__main__":
    producer(); checker(); gate_checks(); graph(); native_boundaries(); wix_version_cases(); macos_checker_mock(); macos_producer_mock(); windows_checker_mock(); print("packaging selftest: PASS")
