#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Assemble the FocusForge lockup: the mark plus the wordmark, text baked to paths.

This is an authoring step, not part of the asset build — it needs Inter
installed and it rewrites checked-in SVGs, so it runs by hand when the type
or the proportions change, not on every `make-logo-assets.sh`.

Every measurement is queried from Inkscape rather than guessed, so the pieces
stay aligned when the type size or the mark scale is changed. The text is
converted to paths on the way out: the repository then carries no font file
and no font licence, and the lockup renders identically everywhere.

Usage:
    make-wordmark.py [ASSETS_DIR]      # default: assets/ next to this repo root
"""

import pathlib
import subprocess
import sys

MARK_W, MARK_H = 96.0, 96.5     # ink box of assets/logo.svg
FS = 74.22                      # cap height 54 -- exactly the anvil's height
GAP = 24.0                      # four modules of the mark's 6 unit grid
MARK_SCALE = 1.0                # beside the word the mark stands at its own size

DARK, LIGHT, ACCENT = "#23303B", "#F2F4F6", "#E8763A"


def mark_shapes(base: str) -> str:
    return (
        f'<g fill="{base}">'
        '<rect x="16" y="58" width="96" height="6"/>'
        '<rect x="25" y="70" width="78" height="6"/>'
        '<rect x="34" y="82" width="60" height="6"/>'
        '<rect x="25" y="94" width="78" height="6"/>'
        '<rect x="16" y="106" width="96" height="6"/></g>'
        f'<g fill="{ACCENT}" transform="rotate(-21.801 50 35)">'
        '<rect x="41" y="20" width="18" height="30"/>'
        '<rect x="59" y="32" width="36" height="6"/></g>'
    )


def word(base: str) -> str:
    return f'<tspan fill="{base}">Focus</tspan><tspan fill="{ACCENT}">Forge</tspan>'


def measure(out: pathlib.Path, base: str) -> dict[str, float]:
    """Ink box of the word, measured against a baseline at y=500."""
    probe = out / "_measure.svg"
    probe.write_text(
        '<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="700" viewBox="0 0 1200 700">'
        f'<text id="t" x="0" y="500" font-family="Inter" font-weight="600" '
        f'font-size="{FS}">{word(base)}</text></svg>'
    )
    result = subprocess.run(
        ["inkscape", str(probe), "--query-id=t", "--query-x", "--query-y",
         "--query-width", "--query-height"],
        capture_output=True, text=True, check=True)
    probe.unlink()
    x, y, w, h = (float(v) for v in result.stdout.split())
    return {"lsb": x, "cap": 500 - y, "w": w, "desc": (y + h) - 500}


def mark(base: str, scale: float, dx: float) -> str:
    return (f'<g transform="translate({dx:.2f} 0) scale({scale:g}) translate(-16 -15.5)">'
            f'{mark_shapes(base)}</g>')


def emit(out: pathlib.Path, name: str, body: str, w: float, h: float) -> None:
    src = out / f"_{name}-src.svg"
    src.write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:.2f} {h:.2f}" '
        f'width="{w:.2f}" height="{h:.2f}" role="img" aria-labelledby="{name}-title">'
        f'<title id="{name}-title">FocusForge</title>{body}</svg>'
    )
    subprocess.run(["inkscape", str(src), "--export-text-to-path", "--export-plain-svg",
                    "-o", str(out / f"{name}.svg")], capture_output=True, check=True)
    src.unlink()


def build(out: pathlib.Path, suffix: str, base: str) -> None:
    m = measure(out, base)
    mw, mh = MARK_W * MARK_SCALE, MARK_H * MARK_SCALE

    # Baseline on the mark's bottom edge, cap height equal to the anvil's own
    # height: the F then covers the stack exactly, top plate to foot. The
    # descender of the g hangs below the line, as it should -- aligning the ink
    # box instead would lift the whole word off the baseline.
    emit(out, f"wordmark-horizontal{suffix}",
         mark(base, MARK_SCALE, 0)
         + f'<text x="{mw + GAP - m["lsb"]:.2f}" y="{mh:.2f}" font-family="Inter" '
           f'font-weight="600" font-size="{FS}">{word(base)}</text>',
         mw + GAP + m["w"], mh + m["desc"])


def main(argv: list[str]) -> int:
    out = pathlib.Path(argv[1]) if len(argv) > 1 else \
        pathlib.Path(__file__).resolve().parents[2] / "assets"
    if not out.is_dir():
        print(f"SKIP: no {out} to write into")
        return 0

    fonts = subprocess.run(["fc-list", ":family"], capture_output=True, text=True)
    if "Inter" not in fonts.stdout:
        print("SKIP: Inter is not installed, so the wordmark cannot be set")
        return 0

    build(out, "", DARK)
    build(out, "-inverse", LIGHT)
    print(f"make-wordmark: two lockups written to {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
