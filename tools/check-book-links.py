#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Check every relative link and image in a built mdBook.

Usage: python3 -I tools/check-book-links.py [BOOK_DIR]

Walks every *.html file under BOOK_DIR (default: book), reads each href and
src, and reports a relative target that is missing from the build output, or
a #fragment that names no id on the target page. External links (http, https,
mailto) are not fetched. Exits 1 when anything is missing.
"""

from __future__ import annotations

import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

# mdBook's own chrome links to these; they are generated, not authored.
IGNORED_PAGES = {"print.html", "404.html", "toc.html"}


class Collector(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.links: list[tuple[str, str]] = []
        self.ids: set[str] = set()

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        for name, value in attrs:
            if value is None:
                continue
            if name in ("id", "name"):
                self.ids.add(value)
            elif name in ("href", "src"):
                self.links.append((tag, value))


def parse(path: Path) -> Collector:
    collector = Collector()
    collector.feed(path.read_text(encoding="utf-8"))
    return collector


def main() -> int:
    book = Path(sys.argv[1] if len(sys.argv) > 1 else "book").resolve()
    if not (book / "index.html").is_file():
        print(f"no built book at {book} (index.html missing)", file=sys.stderr)
        return 2
    pages = {path: parse(path) for path in sorted(book.rglob("*.html"))}
    problems: list[str] = []
    checked = 0
    for page, collected in pages.items():
        if page.name in IGNORED_PAGES:
            continue
        for tag, link in collected.links:
            parts = urlsplit(link)
            if parts.scheme or parts.netloc or link.startswith(("mailto:", "javascript:")):
                continue
            checked += 1
            target = page if not parts.path else (page.parent / unquote(parts.path)).resolve()
            if target.is_dir():
                target = target / "index.html"
            shown = page.relative_to(book)
            if not target.exists():
                problems.append(f"{shown}: <{tag}> {link} -> missing {target.relative_to(book) if target.is_relative_to(book) else target}")
                continue
            if parts.fragment and target.suffix == ".html":
                ids = pages[target].ids if target in pages else parse(target).ids
                if unquote(parts.fragment) not in ids:
                    problems.append(f"{shown}: <{tag}> {link} -> no id '{parts.fragment}' on {target.relative_to(book)}")
    for line in problems:
        print(line)
    print(f"{checked} relative links checked, {len(problems)} broken")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
