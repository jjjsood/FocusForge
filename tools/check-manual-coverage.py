#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Check that every feature in tests/ui/features/ has a manual section with an image.

Usage: python3 -I tools/check-manual-coverage.py [--selftest]

Each feature file carries one `Book:` line of comma-separated `page.md#anchor`
entries, the page relative to docs/. An entry passes when the page exists, a
heading on it has that id, and the section under that heading (up to the next
heading of the same or a higher level) holds an image whose file exists.
Heading ids follow mdBook's rule, so the check reads Markdown and needs no
mdBook. Exits 1 and prints one line per problem, starting with the row name.
"""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HEADING = re.compile(r"^(#{1,6})[ \t]+(.*?)[ \t#]*$")
FENCE = re.compile(r"^\s*(```|~~~)")
IMAGE = re.compile(r"!\[[^\]]*\]\(\s*<?([^)\s>]+)|<img\b[^>]*?\bsrc=[\"']([^\"']+)")
LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)")


def heading_id(text: str) -> str:
    """mdBook's `id_from_content`: lowercase alphanumerics, `_`, `-`; whitespace to `-`."""
    text = LINK.sub(r"\1", text).strip()
    out = []
    for ch in text:
        if ch.isalnum() or ch in "_-":
            out.append(ch.lower() if ch.isascii() else ch)
        elif ch.isspace():
            out.append("-")
    return "".join(out)


def sections(page: Path) -> dict[str, list[str]]:
    """Map each heading id on the page to the image paths in its section."""
    heads: list[tuple[int, str, int]] = []  # level, id, line index
    images: list[tuple[int, str]] = []  # line index, path
    seen: dict[str, int] = {}
    in_fence = False
    lines = page.read_text(encoding="utf-8").splitlines()
    for number, line in enumerate(lines):
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        if match := HEADING.match(line):
            base = heading_id(match.group(2))
            count = seen.get(base, 0)
            seen[base] = count + 1
            heads.append((len(match.group(1)), f"{base}-{count}" if count else base, number))
        for found in IMAGE.finditer(line):
            images.append((number, found.group(1) or found.group(2)))
    result: dict[str, list[str]] = {}
    for index, (level, ident, start) in enumerate(heads):
        end = next((h[2] for h in heads[index + 1:] if h[0] <= level), len(lines))
        result[ident] = [path for number, path in images if start < number < end]
    return result


def check(features_dir: Path, docs_dir: Path) -> tuple[list[str], int, int]:
    """Return the problem lines, the number of rows and the number of sections checked."""
    problems: list[str] = []
    pages: dict[Path, dict[str, list[str]]] = {}
    rows = checked = 0
    for feature in sorted(features_dir.glob("*.md")):
        if feature.name == "README.md":
            continue
        rows += 1
        row = feature.stem
        book = re.findall(r"^Book:[ \t]*(.*)$", feature.read_text(encoding="utf-8"), re.M)
        if len(book) != 1:
            problems.append(f"{row}: expected exactly one Book: line, found {len(book)}")
            continue
        for entry in (e.strip() for e in book[0].split(",") if e.strip()):
            page_name, _, anchor = entry.partition("#")
            if not anchor:
                problems.append(f"{row}: entry '{entry}' has no #anchor")
                continue
            checked += 1
            page = docs_dir / page_name
            if not page.is_file():
                problems.append(f"{row}: page {page_name} not found under docs/")
                continue
            if page not in pages:
                pages[page] = sections(page)
            if anchor not in pages[page]:
                problems.append(f"{row}: no heading with id '{anchor}' on {page_name}")
                continue
            images = pages[page][anchor]
            if not images:
                problems.append(f"{row}: section {entry} holds no image")
            for image in images:
                if not (page.parent / image).is_file():
                    problems.append(f"{row}: image {image} in section {entry} does not exist")
    return problems, rows, checked


def selftest() -> int:
    page = (
        "# Title\n\n## Set the brush: Size, and Soft\n\n### Detail\n\n![a](img/a.png)\n\n"
        "## Repeat\n\n<img src=\"img/b.png\" alt=\"b\">\n\n## Repeat\n\n![c](img/c.png)\n\n"
        "## Shell\n\n```sh\n# a comment\n```\n\n![d](img/d.png)\n\n"
        "## Bare\n\ntext only\n\n## Next\n\n![e](img/e.png)\n"
    )
    clean = {
        "one": "Book: guide/p.md#set-the-brush-size-and-soft, guide/p.md#repeat, guide/p.md#repeat-1, guide/p.md#shell",
        "two": "Book: guide/p.md#next",
    }
    cases = {
        "no Book line": ({"one": "Guides: x.md"}, {}, ["one: expected exactly one Book: line, found 0"]),
        "entry without anchor": ({"one": "Book: guide/p.md"}, {}, ["one: entry 'guide/p.md' has no #anchor"]),
        "deleted page": ({"one": "Book: guide/gone.md#next"}, {}, ["one: page guide/gone.md not found under docs/"]),
        "renamed heading": ({"one": "Book: guide/p.md#old-name"}, {}, ["one: no heading with id 'old-name' on guide/p.md"]),
        "heading inside a fence": ({"one": "Book: guide/p.md#a-comment"}, {}, ["one: no heading with id 'a-comment' on guide/p.md"]),
        "image only in the next sibling": ({"one": "Book: guide/p.md#bare"}, {}, ["one: section guide/p.md#bare holds no image"]),
        "missing image file": ({}, {"guide/img/e.png": None}, ["two: image img/e.png in section guide/p.md#next does not exist"]),
    }
    failures = []
    for label, (books, files, expected) in (("clean tree", ({}, {}, [])), *cases.items()):
        with tempfile.TemporaryDirectory() as tmp:
            features, docs = Path(tmp, "features"), Path(tmp, "docs")
            (docs / "guide/img").mkdir(parents=True)
            features.mkdir()
            (docs / "guide/p.md").write_text(page, encoding="utf-8")
            for name in "abcde":
                (docs / f"guide/img/{name}.png").write_bytes(b"png")
            (features / "README.md").write_text("no Book line needed here\n", encoding="utf-8")
            for row, line in {**clean, **books}.items():
                (features / f"{row}.md").write_text(f"# {row}\n\n{line}\n", encoding="utf-8")
            for name, content in files.items():
                (docs / name).unlink() if content is None else (docs / name).write_bytes(content)
            problems, rows, _ = check(features, docs)
            if problems != expected or rows != 2:
                failures.append(f"FAIL [{label}]: expected {expected}, got {problems} ({rows} rows)")
    for line in failures:
        print(line)
    if failures:
        return 1
    print(f"OK: {len(cases) + 1} selftest cases")
    return 0


def main() -> int:
    if "--selftest" in sys.argv[1:]:
        return selftest()
    problems, rows, checked = check(ROOT / "tests/ui/features", ROOT / "docs")
    for line in problems:
        print(line)
    if problems:
        return 1
    print(f"OK: {rows} rows, {checked} sections")
    return 0


if __name__ == "__main__":
    sys.exit(main())
