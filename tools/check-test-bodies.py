#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Two rules over every non-ignored `#[test]` body under crates/*/src and crates/*/tests.

Usage: tools/check-test-bodies.py [--root DIR]
       tools/check-test-bodies.py --selftest
Exit:  0 clean, 1 findings (one `file:line test_name: rule` line each), 2 usage error.

Rule `assertion-free`: a test must contain an assertion. Counted as one: any
call or macro whose name contains `assert` (`assert_eq!`, `prop_assert!`,
`assert_send_sync::<T>()`, `.debug_assert()`), `panic!`, `unreachable!`,
`.expect(`, `.expect_err(`, `.unwrap_err(`, a `?` in a test returning a
`Result`, `#[should_panic]`, or a call to a function of the same file or of
the crate's `tests/common` that itself contains one (transitively). The
comment `// assertion-free: <reason>` in the body exempts a test whose claim is
that it compiles.

Rule `fixed workspace .scratch path`: a test must not build a path from
`workspace_root()` or `CARGO_MANIFEST_DIR` and `.scratch` in one expression;
concurrent tests would share and overwrite the file. A per-test directory
(`CARGO_TARGET_TMPDIR`, or a temporary working directory) is the place.

Tests carrying `#[ignore ...]` are skipped by both rules.
"""

from __future__ import annotations

import bisect
import re
import sys
import tempfile
from pathlib import Path

RAW_STRING = re.compile(r'b?r(#*)"')
DIRECT = re.compile(
    r"\b\w*assert\w*\s*(?:!|\(|::\s*<)"
    r"|\b(?:panic|unreachable)\s*!"
    r"|\.\s*(?:expect|expect_err|unwrap_err)\s*\("
)
CALL = re.compile(r"\b([A-Za-z_]\w*)\s*(?:::\s*<[^;{}()]*>)?\s*\(")
FN_HEAD = re.compile(
    r"\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe)\s+)*fn\s+(\w+)"
)
FN_ANY = re.compile(r"\bfn\s+(\w+)")
EXEMPT = re.compile(r"//\s*assertion-free:(.*)")
RULE_ASSERT = "assertion-free"
RULE_UNREASONED = "assertion-free exemption without a reason"
RULE_SCRATCH = "fixed workspace .scratch path"


def mask(src: str, keep_strings: bool) -> tuple[str, list[tuple[int, str]]]:
    """`src` with comments (and unless `keep_strings`, string and char literal
    contents) overwritten by spaces, newlines kept, so indexes stay aligned.
    Also returns every comment as (offset, text)."""
    out = list(src)
    n = len(src)
    comments: list[tuple[int, str]] = []

    def blank(a: int, b: int) -> None:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    i = 0
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            comments.append((i, src[i:j]))
            blank(i, j)
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            comments.append((i, src[i:j]))
            blank(i, j)
            i = j
        elif (
            c in "rb"
            and not (i > 0 and (src[i - 1].isalnum() or src[i - 1] == "_"))
            and (m := RAW_STRING.match(src, i))
        ):
            end = src.find('"' + m.group(1), m.end())
            end = n if end < 0 else end
            if not keep_strings:
                blank(m.end(), end)
            i = end + 1 + len(m.group(1))
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            if not keep_strings:
                blank(i + 1, j)
            i = j + 1
        elif c == "'":
            if src.startswith("\\", i + 1):
                j = src.find("'", i + 3)
            elif i + 2 < n and src[i + 2] == "'":
                j = i + 2
            else:
                i += 1  # a lifetime
                continue
            j = n if j < 0 else j
            if not keep_strings:
                blank(i + 1, j)
            i = j + 1
        else:
            i += 1
    return "".join(out), comments


def match_braces(code: str) -> dict[int, int]:
    pairs: dict[int, int] = {}
    stack: list[int] = []
    for i, c in enumerate(code):
        if c == "{":
            stack.append(i)
        elif c == "}" and stack:
            pairs[stack.pop()] = i
    return pairs


def body_open(code: str, pos: int, pairs: dict[int, int]) -> int | None:
    """Offset of the `{` opening the body of the function whose signature
    starts at `pos`; None for a declaration. A `;` inside parentheses or
    brackets (`[f32; 9]`) does not end the signature."""
    depth = 0
    for i in range(pos, len(code)):
        c = code[i]
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
        elif depth == 0 and c == ";":
            return None
        elif depth == 0 and c == "{":
            return i if i in pairs else None
    return None


def fn_bodies(code: str, pairs: dict[int, int]) -> dict[str, list[str]]:
    """Every function with a body in `code`: name -> bodies."""
    found: dict[str, list[str]] = {}
    for m in FN_ANY.finditer(code):
        open_ = body_open(code, m.end(), pairs)
        if open_ is not None:
            found.setdefault(m.group(1), []).append(code[open_ + 1 : pairs[open_]])
    return found


def asserting_names(table: dict[str, list[str]]) -> set[str]:
    """Functions of `table` that contain an assertion, directly or through
    another function of `table`."""
    names = {n for n, bodies in table.items() if any(DIRECT.search(b) for b in bodies)}
    changed = True
    while changed:
        changed = False
        for n, bodies in table.items():
            if n in names:
                continue
            if any(c in names for b in bodies for c in CALL.findall(b)):
                names.add(n)
                changed = True
    return names


def attribute_block(code: str, start: int, end: int) -> tuple[str, int]:
    """The attributes around the `#[test]` at [start, end) joined, and the
    offset just past the last attribute that follows it."""
    attrs: list[str] = []
    p = start
    while True:
        q = p
        while q > 0 and code[q - 1].isspace():
            q -= 1
        if q == 0 or code[q - 1] != "]":
            break
        depth, k = 0, q - 1
        while k >= 0:
            depth += {"]": 1, "[": -1}.get(code[k], 0)
            if depth == 0:
                break
            k -= 1
        if k < 1 or code[k - 1] != "#":
            break
        attrs.append(code[k - 1 : q])
        p = k - 1
    p = end
    while True:
        while p < len(code) and code[p].isspace():
            p += 1
        if not code.startswith("#[", p):
            break
        depth, k = 0, p + 1
        while k < len(code):
            depth += {"[": 1, "]": -1}.get(code[k], 0)
            if depth == 0:
                break
            k += 1
        attrs.append(code[p : k + 1])
        p = k + 1
    return " ".join(attrs), p


def scan_file(
    path: Path, rel: str, common: dict[str, list[str]]
) -> tuple[int, list[tuple[str, int, str, str]]]:
    src = path.read_text(encoding="utf-8")
    code, comments = mask(src, keep_strings=False)
    text, _ = mask(src, keep_strings=True)
    pairs = match_braces(code)
    table = {n: list(b) for n, b in common.items()}
    for n, bodies in fn_bodies(code, pairs).items():
        table.setdefault(n, []).extend(bodies)
    asserting = asserting_names(table)
    line_starts = [0] + [m.end() for m in re.finditer(r"\n", src)]
    count = 0
    findings: list[tuple[str, int, str, str]] = []
    for m in re.finditer(r"#\[test\]", code):
        attrs, p = attribute_block(code, m.start(), m.end())
        head = FN_HEAD.match(code, p)
        open_ = body_open(code, head.end(), pairs) if head else None
        if head is None or open_ is None:
            continue
        count += 1
        if "#[ignore" in attrs:
            continue
        name = head.group(1)
        line = bisect.bisect_right(line_starts, head.start(1))
        close = pairs[open_]
        body = code[open_ + 1 : close]
        signature = code[head.end() : open_]

        exempt = [
            c for off, c in comments if open_ < off < close and EXEMPT.match(c)
        ]
        direct = DIRECT.search(body) or "should_panic" in attrs
        through_result = "?" in body and "->" in signature and "Result" in signature
        through_helper = any(c in asserting for c in CALL.findall(body))
        if exempt:
            if not EXEMPT.match(exempt[0]).group(1).strip():
                findings.append((rel, line, name, RULE_UNREASONED))
        elif not (direct or through_result or through_helper):
            findings.append((rel, line, name, RULE_ASSERT))

        raw_body = text[open_ + 1 : close]
        for anchor in re.finditer(r"workspace_root\s*\(|CARGO_MANIFEST_DIR", raw_body):
            semi = body.find(";", anchor.start())
            expression = raw_body[anchor.start() : semi if semi >= 0 else len(raw_body)]
            if ".scratch" in expression:
                findings.append((rel, line, name, RULE_SCRATCH))
                break
    return count, findings


def scan(root: Path) -> tuple[int, list[tuple[str, int, str, str]]]:
    total = 0
    findings: list[tuple[str, int, str, str]] = []
    for crate in sorted((root / "crates").glob("*")):
        common: dict[str, list[str]] = {}
        for helper in sorted((crate / "tests" / "common").rglob("*.rs")):
            code, _ = mask(helper.read_text(encoding="utf-8"), keep_strings=False)
            for n, bodies in fn_bodies(code, match_braces(code)).items():
                common.setdefault(n, []).extend(bodies)
        files = sorted(
            [*(crate / "src").rglob("*.rs"), *(crate / "tests").rglob("*.rs")]
        )
        for f in files:
            count, found = scan_file(f, f.relative_to(root).as_posix(), common)
            total += count
            findings += found
    return total, findings


def selftest() -> int:
    def test(body: str, attrs: str = "", name: str = "t", sig: str = "") -> str:
        return f"{attrs}#[test]\nfn {name}() {sig}{{\n{body}\n}}\n"

    cases: list[tuple[str, dict[str, str], list[tuple[str, str]]]] = []

    def ok(label: str, files: dict[str, str]) -> None:
        cases.append((label, files, []))

    def bad(label: str, files: dict[str, str], *expected: tuple[str, str]) -> None:
        cases.append((label, files, list(expected)))

    t = "crates/c/tests/t.rs"
    bad("no assertion", {t: test("let _x = 1;")}, ("line 2 t", RULE_ASSERT))
    for label, body in {
        "assert!": "assert!(true);",
        "assert_eq!": "assert_eq!(1, 1);",
        "prop_assert!": "prop_assert!(true);",
        "expect": "Some(1).expect(\"x\");",
        "unwrap_err": "Err::<u8, u8>(1).unwrap_err();",
        "panic!": "panic!(\"x\");",
        "assert_* call": "assert_send_sync::<u8>();",
        "debug_assert": "cmd.debug_assert();",
    }.items():
        ok(label, {t: test(body)})
    ok("? in a Result test", {t: test("f()?;", sig="-> Result<(), E> ")})
    bad("? in a plain test", {t: test("f()?;")}, ("line 2 t", RULE_ASSERT))
    ok("should_panic before", {t: test("f();", attrs="#[should_panic]\n")})
    ok("should_panic after", {t: "#[test]\n#[should_panic]\nfn t() {\n f();\n}\n"})
    ok(
        "same-file helper that asserts",
        {t: "fn h() { assert!(true); }\n" + test("h();")},
    )
    ok(
        "helper with a ; in its signature",
        {t: "fn h(a: [u8; 9]) { assert!(a.len() > 0); }\n" + test("h([0; 9]);")},
    )
    bad(
        "same-file helper that does not assert",
        {t: "fn h() { let _ = 1; }\n" + test("h();")},
        ("line 3 t", RULE_ASSERT),
    )
    ok(
        "common helper through a second helper",
        {
            "crates/c/tests/common/mod.rs": "pub fn a() { b(); }\nfn b() { assert!(true); }\n",
            t: test("a();"),
        },
    )
    bad(
        "helper of another crate does not count",
        {
            "crates/d/tests/common/mod.rs": "pub fn a() { assert!(true); }\n",
            t: test("a();"),
        },
        ("line 2 t", RULE_ASSERT),
    )
    ok(
        "mod tests",
        {"crates/c/src/lib.rs": "#[cfg(test)]\nmod tests {\n" + test("assert!(true);") + "}\n"},
    )
    bad(
        "behind cfg(feature)",
        {t: test("let _x = 1;", attrs="#[cfg(feature = \"gpu-tests\")]\n")},
        ("line 3 t", RULE_ASSERT),
    )
    bad(
        "braces in string, raw string and char literals",
        {
            t: test(
                'let _a = "}}"; let _b = r#"{ " } "#; let _c = \'{\'; let _d = \'\\\'\';',
                name="first",
            )
            + test("assert!(true);", name="second")
        },
        ("line 2 first", RULE_ASSERT),
    )
    bad(
        "assert! only in a string literal",
        {t: test('let _s = "assert!(true)";')},
        ("line 2 t", RULE_ASSERT),
    )
    bad(
        "assert! only in a comment",
        {t: test("// assert!(true)\nlet _x = 1;")},
        ("line 2 t", RULE_ASSERT),
    )
    ok("ignored measurement", {t: test("let _x = 1;", attrs='#[ignore = "measurement: prints"]\n')})
    ok("exemption with a reason", {t: test("// assertion-free: compile-time check\nlet _x = 1;")})
    bad(
        "exemption without a reason",
        {t: test("// assertion-free:\nlet _x = 1;")},
        ("line 2 t", RULE_UNREASONED),
    )
    ok("cwd.join(\".scratch\")", {t: test('let _p = cwd.join(".scratch"); assert!(true);')})
    bad(
        "workspace_root split over lines",
        {t: test('assert!(true);\nlet _p = common::workspace_root()\n    .join(".scratch")\n    .join("x");')},
        ("line 2 t", RULE_SCRATCH),
    )
    bad(
        "CARGO_MANIFEST_DIR",
        {t: test('assert!(true);\nlet _p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/x");')},
        ("line 2 t", RULE_SCRATCH),
    )
    ok(
        "ignored test writing .scratch",
        {t: test('assert!(true); let _p = workspace_root().join(".scratch");', attrs='#[ignore = "human run"]\n')},
    )
    ok(
        "helper outside a test body reading .scratch",
        {t: 'fn read() -> P { workspace_root().join(".scratch/m.json") }\n' + test("assert!(true);")},
    )
    ok(
        "workspace_root and .scratch in separate statements",
        {t: test('let r = workspace_root(); assert!(true); let _s = ".scratch";')},
    )

    failed = 0
    for label, files, expected in cases:
        with tempfile.TemporaryDirectory(prefix="check-test-bodies-selftest-") as d:
            root = Path(d)
            for rel, content in files.items():
                (root / rel).parent.mkdir(parents=True, exist_ok=True)
                (root / rel).write_text(content, encoding="utf-8")
            _, found = scan(root)
        got = [(f"line {ln} {n}", rule) for r, ln, n, rule in found]
        if got != expected:
            failed += 1
            print(f"FAIL {label}: expected {expected}, got {got}")
    if failed:
        print(f"check-test-bodies --selftest: {failed} of {len(cases)} cases failed")
        return 1
    print(f"check-test-bodies --selftest: all {len(cases)} cases pass")
    return 0


def main(argv: list[str]) -> int:
    if argv == ["--selftest"]:
        return selftest()
    root = Path(__file__).resolve().parent.parent
    if len(argv) == 2 and argv[0] == "--root":
        root = Path(argv[1])
    elif argv:
        print(__doc__, file=sys.stderr)
        return 2
    total, findings = scan(root)
    for rel, line, name, rule in findings:
        print(f"{rel}:{line} {name}: {rule}")
    if findings:
        return 1
    print(f"check-test-bodies: {total} tests, no findings")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
