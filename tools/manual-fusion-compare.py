#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Regenerate the manual's side-by-side of the three fusion methods.

Usage:
    python3 -I tools/manual-fusion-compare.py [--cli PATH]

Fuses fixtures/mantis-512/frames once per method with focusforge-cli
(default target/debug/focusforge-cli), exports each composite as an 8-bit
PNG under .scratch/manual-fusion-compare/, and assembles
docs/assets/manual/stacking/stacking-fusion-methods.jpg: the three results
labelled with the names the window uses, above an enlarged strip of the same
area. Needs Pillow; the label font is the bundled Inter.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parent.parent
FRAMES = REPO / "fixtures" / "mantis-512" / "frames"
SCRATCH = REPO / ".scratch" / "manual-fusion-compare"
OUT = REPO / "docs" / "assets" / "manual" / "stacking" / "stacking-fusion-methods.jpg"
# CLI id and the label the window shows for it.
METHODS = [("laplacian", "Details"), ("weighted-average", "Soft transitions"), ("depth-map", "Focus map")]
ZOOM_BOX = (20, 85, 200, 200)  # bristles on the rim and the smooth area below it


def fuse(cli: Path, method: str) -> Path:
    work = SCRATCH / method
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)
    subprocess.run([str(cli), "stack", str(FRAMES), "--algorithm", method, "-o", str(work),
                    "--cpu", "--no-cache", "--no-progress"], check=True)
    png = work / "out.png"
    subprocess.run([str(cli), "export", str(work / "frames.ffp"), "-o", str(png), "--png-bits", "8"], check=True)
    return png


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--cli", type=Path, default=REPO / "target" / "debug" / "focusforge-cli")
    args = parser.parse_args()
    if not args.cli.exists():
        print(f"{args.cli} missing: build it with `cargo build -p focusforge-cli`", file=sys.stderr)
        return 2
    images = [Image.open(fuse(args.cli, method)).convert("RGB") for method, _ in METHODS]
    w, h = images[0].size
    gap, label_h = 12, 34
    zw, zh = ZOOM_BOX[2] - ZOOM_BOX[0], ZOOM_BOX[3] - ZOOM_BOX[1]
    zoom_h = round(zh * w / zw)
    canvas = Image.new("RGB", (3 * w + 2 * gap, label_h + h + gap + zoom_h), (255, 255, 255))
    draw = ImageDraw.Draw(canvas)
    font = ImageFont.truetype(str(REPO / "assets" / "fonts" / "Inter-SemiBold.ttf"), 18)
    for k, (image, (_, label)) in enumerate(zip(images, METHODS)):
        x = k * (w + gap)
        draw.text((x + (w - draw.textlength(label, font=font)) / 2, 7), label, fill=(30, 30, 30), font=font)
        canvas.paste(image, (x, label_h))
        canvas.paste(image.crop(ZOOM_BOX).resize((w, zoom_h), Image.LANCZOS), (x, label_h + h + gap))
        draw.rectangle((x + ZOOM_BOX[0], label_h + ZOOM_BOX[1], x + ZOOM_BOX[2], label_h + ZOOM_BOX[3]),
                       outline=(255, 255, 255), width=1)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    tmp = OUT.with_suffix(".tmp.jpg")
    canvas.save(tmp, "JPEG", quality=85, optimize=True)
    tmp.replace(OUT)
    print(f"wrote {OUT.relative_to(REPO)} {canvas.width}x{canvas.height} {OUT.stat().st_size // 1024} KB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
