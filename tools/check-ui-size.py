#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Size budget for every Slint-generated Rust file of the current build.

Usage: tools/check-ui-size.py [--build NAME]...
       tools/check-ui-size.py --selftest
Builds: tests (default), theme-sheet; each is the no-op `cargo ... --no-run` of the
        gate stage that builds it, so run this after that stage.
Exit:   0 every file within budget, 1 any failure, 2 usage error.

The generated files are found from cargo's own `build-script-executed` messages, so a
stale `out/` directory of an earlier build is never seen. A generating package is a
workspace member with `slint-build` among its build-dependencies.

To raise a budget on purpose, edit its line in BUDGETS to the new size plus 10 %,
rounded up to the next 100,000 B, and give the reason in the commit message.
"""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
import tempfile
from pathlib import Path

# Each budget: the file's size on the current tree plus 10 %, rounded up to 100,000 B.
# (package, generated file, budget in bytes, build that must produce it)
BUDGETS: list[tuple[str, str, int, str]] = [
    ("focusforge-ui", "app.rs", 17400000, "tests"),
    ("focusforge-ui-comparison", "comparison_window.rs", 1100000, "tests"),
    ("focusforge-ui-theme-sheet", "theme_sheet.rs", 24400000, "theme-sheet"),
]

BUILDS: dict[str, list[str]] = {
    "tests": ["cargo", "test", "--workspace", "--no-run", "--message-format=json"],
    "theme-sheet": [
        "cargo", "test", "-p", "focusforge-gui", "--features", "theme-sheet",
        "--test", "theme", "--no-run", "--message-format=json",
    ],
}


def generating_packages(metadata: dict) -> dict[str, str]:
    """Workspace packages with `slint-build` among their build-dependencies: id -> name."""
    return {
        p["id"]: p["name"]
        for p in metadata["packages"]
        if any(d["name"] == "slint-build" and d["kind"] == "build" for d in p["dependencies"])
    }


def raise_line(size: int) -> str:
    new = -(-size * 11 // 1_000_000) * 100_000
    return (f"  to raise it on purpose: edit its line in BUDGETS to {new:,} B (size + 10 %, "
            "rounded up to 100,000 B) and give the reason in the commit message")


def check(selected: list[str], budgets: list[tuple[str, str, int, str]],
          generating: dict[str, str], runner) -> tuple[list[str], bool]:
    """Runs each selected build through `runner(args) -> (code, stdout, stderr)` and
    compares the generated files its messages name with `budgets`."""
    lines: list[str] = []
    reported: dict[str, set[tuple[str, str]]] = {}
    seen_dirs: set[tuple[str, str]] = set()
    files: list[tuple[str, str, int]] = []
    executed = compiled = 0
    failed = False

    def summary() -> tuple[list[str], bool]:
        skipped = sum(1 for ln in lines if ln.startswith("skip "))
        return lines + [f"checked {len(files)} files, skipped {skipped} budgets, "
                        f"cargo compiled {compiled} units"], failed

    for build in selected:
        code, out, err = runner(BUILDS[build])
        if code != 0:
            lines.append(f"FAIL --build {build}: cargo exited with {code}")
            lines += [f"  {ln}" for ln in err.splitlines()[-4:]]
            return summary()[0], True
        for raw in out.splitlines():
            try:
                msg = json.loads(raw)
            except ValueError:
                continue
            if msg.get("reason") == "compiler-artifact" and msg.get("fresh") is False:
                compiled += 1
            elif msg.get("reason") == "build-script-executed" and msg["package_id"] in generating:
                executed += 1
                pkg = generating[msg["package_id"]]
                new_dir = (pkg, msg["out_dir"]) not in seen_dirs
                seen_dirs.add((pkg, msg["out_dir"]))
                for f in sorted(Path(msg["out_dir"]).glob("*.rs")):
                    if new_dir:
                        files.append((pkg, f.name, f.stat().st_size))
                    reported.setdefault(build, set()).add((pkg, f.name))
    if executed == 0:
        lines.append("FAIL no generated Slint file reported; build the workspace first")
        return summary()[0], True

    by_file = {(p, f): b for p, f, b, _ in budgets}
    for pkg, name, size in files:
        budget = by_file.get((pkg, name))
        if budget is None:
            lines.append(f"FAIL {pkg} {name}: {size} B, no entry in BUDGETS")
            failed = True
        elif size > budget:
            lines += [f"FAIL {pkg} {name}: {size} B, budget {budget} B", raise_line(size)]
            failed = True
        else:
            lines.append(f"ok {pkg} {name}: {size} B of {budget} B")
    for pkg in sorted(set(generating.values()) - {p for p, *_ in budgets}):
        lines.append(f"FAIL {pkg}: generates Slint code but has no entry in BUDGETS")
        failed = True
    for pkg, name, _, build in budgets:
        if build not in selected:
            lines.append(f"skip {pkg} {name}: not built by --build {' --build '.join(selected)}")
        elif (pkg, name) not in reported.get(build, set()):
            lines.append(f"FAIL {pkg} {name}: budgeted for --build {build}, "
                         "but no such file was generated")
            failed = True
    return summary()[0], failed


def selftest() -> int:
    def mk(root: Path, name: str, files: dict[str, int]) -> Path:
        d = root / name
        d.mkdir(parents=True)
        for f, size in files.items():
            (d / f).write_bytes(b"x" * size)
        return d

    def messages(dirs: list[tuple[str, Path]], compiled: int = 0) -> str:
        lines = [json.dumps({"reason": "build-script-executed", "package_id": pid,
                             "out_dir": str(d)}) for pid, d in dirs]
        lines += [json.dumps({"reason": "compiler-artifact", "fresh": False})] * compiled
        lines += [json.dumps({"reason": "compiler-artifact", "fresh": True})]
        return "\n".join(lines + ["not json"]) + "\n"

    gen = {"id-a": "pkg-a", "id-b": "pkg-b"}
    budgets = [("pkg-a", "a.rs", 2_000_000, "tests"), ("pkg-b", "b.rs", 500_000, "theme-sheet")]
    raise_a = ("  to raise it on purpose: edit its line in BUDGETS to {} B (size + 10 %, "
               "rounded up to 100,000 B) and give the reason in the commit message")
    failures = 0
    total = 0

    def case(label: str, selected: list[str], runs: dict[str, tuple[int, str, str]],
             want: list[str], want_failed: bool, table=None, generating=None) -> None:
        nonlocal failures, total
        total += 1
        try:
            lines, failed = check(selected, table or budgets, generating or gen,
                                  lambda args: runs[next(n for n, a in BUILDS.items() if a == args)])
            got = lines[:-1]
        except Exception as e:  # noqa: BLE001 - a case that raises is a failed case
            got, failed = [f"raised {type(e).__name__}: {e}"], None
        if got != want or failed != want_failed:
            failures += 1
            print(f"FAIL {label}: expected {want} failed={want_failed}, got {got} failed={failed}")

    with tempfile.TemporaryDirectory(prefix="check-ui-size-selftest-") as t:
        root = Path(t)
        a_at = mk(root, "a-at", {"a.rs": 2_000_000})
        a_over = mk(root, "a-over", {"a.rs": 2_000_001})
        a_small = mk(root, "a-small", {"a.rs": 1_900_000})
        a_stale = mk(root, "a-stale", {"a.rs": 9_000_000})
        a_extra = mk(root, "a-extra", {"a.rs": 1_000, "other.rs": 7})
        b_ok = mk(root, "b-ok", {"b.rs": 400_000})
        b_empty = mk(root, "b-empty", {})
        tests = lambda dirs, n=0: {"tests": (0, messages(dirs, n), "")}  # noqa: E731
        both = lambda a, b: {"tests": (0, messages(a), ""), "theme-sheet": (0, messages(b), "")}  # noqa: E731

        case("at budget passes", ["tests"], tests([("id-a", a_at), ("id-b", b_empty)]),
             ["ok pkg-a a.rs: 2000000 B of 2000000 B",
              "skip pkg-b b.rs: not built by --build tests"], False)
        case("one byte over fails with package, file, size, budget and the raise rule",
             ["tests"], tests([("id-a", a_over), ("id-b", b_empty)]),
             ["FAIL pkg-a a.rs: 2000001 B, budget 2000000 B", raise_a.format("2,300,000"),
              "skip pkg-b b.rs: not built by --build tests"], True)
        case("two directories of one package are both checked", ["tests"],
             tests([("id-a", a_small), ("id-a", a_over)]),
             ["ok pkg-a a.rs: 1900000 B of 2000000 B",
              "FAIL pkg-a a.rs: 2000001 B, budget 2000000 B", raise_a.format("2,300,000"),
              "skip pkg-b b.rs: not built by --build tests"], True)
        case("one directory named by two builds is checked once", ["tests", "theme-sheet"],
             both([("id-a", a_at), ("id-b", b_ok)], [("id-a", a_at), ("id-b", b_ok)]),
             ["ok pkg-a a.rs: 2000000 B of 2000000 B", "ok pkg-b b.rs: 400000 B of 500000 B"],
             False)
        case("a larger stale directory cargo does not name is ignored", ["tests"],
             tests([("id-a", a_at), ("id-b", b_empty)]),
             ["ok pkg-a a.rs: 2000000 B of 2000000 B",
              "skip pkg-b b.rs: not built by --build tests"], False)
        case("a reported file with no entry fails naming package, file and size",
             ["tests"], tests([("id-a", a_extra), ("id-b", b_empty)]),
             ["ok pkg-a a.rs: 1000 B of 2000000 B",
              "FAIL pkg-a other.rs: 7 B, no entry in BUDGETS",
              "skip pkg-b b.rs: not built by --build tests"], True)
        case("a generating package with no entry at all fails naming it", ["tests"],
             tests([("id-a", a_at), ("id-b", b_empty)]),
             ["ok pkg-a a.rs: 2000000 B of 2000000 B",
              "FAIL pkg-b: generates Slint code but has no entry in BUDGETS"], True,
             table=[budgets[0]])
        case("an entry of a selected build whose file was not reported fails naming it",
             ["tests", "theme-sheet"], both([("id-a", a_at)], [("id-b", b_empty)]),
             ["ok pkg-a a.rs: 2000000 B of 2000000 B",
              "FAIL pkg-b b.rs: budgeted for --build theme-sheet, but no such file was generated"],
             True)
        case("both builds selected and within budget", ["tests", "theme-sheet"],
             both([("id-a", a_at)], [("id-b", b_ok)]),
             ["ok pkg-a a.rs: 2000000 B of 2000000 B", "ok pkg-b b.rs: 400000 B of 500000 B"],
             False)
        case("no build-script-executed for a generating package fails",
             ["tests"], {"tests": (0, messages([("id-other", a_at)]), "")},
             ["FAIL no generated Slint file reported; build the workspace first"], True)
        case("a non-zero cargo exit fails with its code and the stderr tail", ["tests"],
             {"tests": (101, "", "a\nb\nc\nd\ne\nf\n")},
             ["FAIL --build tests: cargo exited with 101", "  c", "  d", "  e", "  f"], True)

        total += 1
        with contextlib.redirect_stderr(io.StringIO()):
            got = main(["--build", "nope"])
        if got != 2:
            failures += 1
            print(f"FAIL an unknown --build exits 2: got {got}")

        total += 1
        meta = {"packages": [
            {"id": "id-a", "name": "pkg-a", "dependencies": [
                {"name": "slint-build", "kind": "build"}, {"name": "x", "kind": None}]},
            {"id": "id-b", "name": "pkg-b", "dependencies": [{"name": "slint-build", "kind": None}]},
            {"id": "id-c", "name": "pkg-c", "dependencies": [{"name": "serde", "kind": "build"}]},
        ]}
        if generating_packages(meta) != {"id-a": "pkg-a"}:
            failures += 1
            print(f"FAIL generating packages need slint-build as a build dependency: "
                  f"got {generating_packages(meta)}")

        total += 1
        lines, _ = check(["tests"], budgets, gen, lambda a: (0, messages([("id-a", a_at)], 2), ""))
        if lines[-1] != "checked 1 files, skipped 1 budgets, cargo compiled 2 units":
            failures += 1
            print(f"FAIL the last line counts files, skips and compiled units: got {lines[-1]!r}")

    if failures:
        print(f"check-ui-size --selftest: {failures} of {total} cases failed")
        return 1
    print(f"check-ui-size --selftest: all {total} cases pass")
    return 0


def main(argv: list[str]) -> int:
    if argv == ["--selftest"]:
        return selftest()
    names: list[str] = []
    it = iter(argv)
    for arg in it:
        name = next(it, None) if arg == "--build" else None
        if name not in BUILDS:
            print(f"usage: unknown argument or build {name or arg!r}; builds: {', '.join(BUILDS)}",
                  file=sys.stderr)
            return 2
        names.append(name)
    manifest = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
                              capture_output=True, text=True)
    if manifest.returncode != 0:
        print(f"FAIL cargo metadata exited with {manifest.returncode}\n{manifest.stderr}")
        return 1

    def run(args: list[str]) -> tuple[int, str, str]:
        p = subprocess.run(args, capture_output=True, text=True)
        return p.returncode, p.stdout, p.stderr

    lines, failed = check(names or ["tests"], BUDGETS, generating_packages(json.loads(manifest.stdout)), run)
    print("\n".join(lines))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
