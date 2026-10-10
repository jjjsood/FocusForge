#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Offline mutation checks for release packaging and publication policy."""
from __future__ import annotations

import ast
import hashlib
import importlib.util
import io
import json
import re
import shutil
import subprocess
import tempfile
import tomllib
import urllib.error
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = ROOT / "tools/package-linux.sh"
CHECKER = ROOT / "tools/check-package-linux.sh"
spec = importlib.util.spec_from_file_location("release_gate", ROOT / "tools/check-release-gate.py")
gate = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(gate)


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


def valid_graph(config: str, setup: str, release: str) -> bool:
    try:
        dist = tomllib.loads(config)
        targets = dist["dist"]["targets"]
        packages = dist["workspace"]["packages"]
        extras = dist["dist"]["extra-artifacts"]
    except (KeyError, TypeError, tomllib.TOMLDecodeError):
        return False
    if set(targets) != {"x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin"} or packages != ["focusforge-cli", "focusforge-gui"]:
        return False
    if not extras or any(item.get("build") != ["tools/package-linux.sh"] or not item.get("artifacts") for item in extras): return False
    checker = "python3 tools/check-release-gate.py --anonymous --sha \"$GITHUB_SHA\" --repository \"$GITHUB_REPOSITORY\""
    step = setup[setup.find("- name: Require exact check run"):setup.find("- name: Require locked release Cargo profile")]
    if checker not in step or "if:" in step or "continue-on-error" in step or "|| true" in step: return False
    if "plan-gate" in release: return False
    local, global_build, host = release.find("build-local-artifacts:"), release.find("build-global-artifacts:"), release.find("\n  host:")
    local_part = release[local:global_build]; host_part = release[host:release.find("\n  announce:", host)]
    matrix = "matrix: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix }}"
    pinned_actions = re.findall(r"uses:\s*['\"]?([^\s'\"]+)", release)
    condition = "if: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null &&"
    generated_step = local_part[local_part.find("Require exact check run"):local_part.find("Require locked release Cargo profile")]
    run_line = next((line.strip().split(": ", 1)[1] for line in generated_step.splitlines() if line.strip().startswith("run: ")), "")
    try: generated_command = json.loads(run_line)
    except json.JSONDecodeError: return False
    return min(local, global_build, host, local_part.find("Require exact check run"), local_part.find("dist build")) >= 0 and local_part.find("Require exact check run") < local_part.find("dist build") and generated_command == "GH_TOKEN= GITHUB_TOKEN= " + checker and "if:" not in generated_step and "continue-on-error" not in generated_step and "|| true" not in generated_step and "- build-local-artifacts" in host_part and "needs.build-local-artifacts.result == 'success'" in host_part and "needs.build-local-artifacts.result == 'skipped'" in host_part and matrix in local_part and condition in local_part and all(re.fullmatch(r"[A-Za-z0-9_./-]+@[0-9a-f]{40}", action) for action in pinned_actions)


def graph() -> None:
    config = (ROOT / "dist-workspace.toml").read_text(); setup = (ROOT / ".github/build-setup.yml").read_text()
    release = (ROOT / ".github/workflows/release.yml").read_text(); assert valid_graph(config, setup, release)
    assert not valid_graph(config.replace('targets = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin"]', "targets = []"), setup, release)
    assert not valid_graph(config.replace('packages = ["focusforge-cli", "focusforge-gui"]', "packages = []"), setup, release)
    assert not valid_graph(config, setup.replace('- name: Require exact check run', '- name: Require exact check run\n  if: false'), release)
    assert not valid_graph(config, setup.replace('- name: Require exact check run', '- name: Require exact check run\n  continue-on-error: true'), release)
    assert not valid_graph(config, setup.replace(checker := 'python3 tools/check-release-gate.py --anonymous --sha "$GITHUB_SHA" --repository "$GITHUB_REPOSITORY"', checker + ' || true'), release)
    assert not valid_graph(config, setup, release.replace('Require exact check run', 'masked check run', 1))
    assert not valid_graph(config, setup, release.replace('- name: "Require exact check run"', '- name: "Require exact check run"\n        if: false', 1))
    assert not valid_graph(config, setup, release.replace('- name: "Require exact check run"', '- name: "Require exact check run"\n        continue-on-error: true', 1))
    generated_run = next(line for line in release.splitlines() if 'tools/check-release-gate.py' in line)
    assert not valid_graph(config, setup, release.replace(generated_run, generated_run[:-1] + ' || true"', 1))
    assert not valid_graph(config, setup, release.replace("needs.build-local-artifacts.result == 'success'", "needs.build-local-artifacts.result == 'failure'", 1))
    assert not valid_graph(config, setup, release.replace("needs.build-local-artifacts.result == 'success'", "needs.build-local-artifacts.result == 'skipped'", 1))
    assert not valid_graph(config, setup, release.replace("build-local-artifacts:", "plan-gate:\n  build-local-artifacts:", 1))
    assert not valid_graph(config, setup, release.replace("if: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null", "if: ${{ false && fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null", 1))
    assert not valid_graph(config, setup, release.replace("actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803", "actions/checkout@v6", 1))
    matrix = "matrix: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix }}"
    assert not valid_graph(config, setup, release.replace(matrix, "matrix: { include: [] }", 1))
    local = release[release.find("build-local-artifacts:"):release.find("build-global-artifacts:")]
    host = release[release.find("\n  host:"):release.find("\n  announce:")]
    local_condition = next(line.strip()[4:] for line in local.splitlines() if line.strip().startswith("if: "))
    host_condition = next(line.strip()[4:] for line in host.splitlines() if line.strip().startswith("if: "))

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


if __name__ == "__main__":
    producer(); checker(); gate_checks(); graph(); print("packaging selftest: PASS")
