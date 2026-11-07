#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Regenerate the example images of the manual's focus-stacking chapters.

Usage:
    python3 -I tools/manual-examples.py [GROUP ...] [--cli PATH] [--check]

GROUP is one or more of `depth-of-field`, `halos`, `motion`, `gaps`; none
means all four. Run it from anywhere; paths resolve against the repository.

Every image comes from the checked-in `fixtures/mantis-512` frames (eight
512x512 16-bit crops of one 61-frame series of a praying mantis's head, in
focus order) and from FocusForge's own fusion:

  1. The frames an example needs are cut into `.scratch/manual-examples/`
     through `tools/lib/rawtiff.py` (a whole-pixel shift for the simulated
     camera movement is a shifted read window, nothing is resampled).
  2. `focusforge-cli stack ... --cpu --no-cache --no-progress` fuses them.
     `--cpu` keeps reruns byte-identical (a GPU adapter may differ by 1 LSB).
  3. `focusforge-cli export` writes the composite, the Z-Map and, with
     `--frame`, single source frames warped and cropped to the composite's own
     geometry, as 16-bit TIFF; `focusforge-cli retouch` applies the one brush
     stroke the halo repair example shows.
  4. This script crops the declared window, places panels side by side, and
     encodes a baseline JPEG itself (standard tables, no metadata segment
     beyond the JFIF header), so the bytes depend only on the pixels.
  5. Finished images are moved into `docs/assets/manual/<chapter>/`:
       depth-of-field/source-frames.jpg   depth-of-field/composite.jpg
       failure-images/halo-methods.jpg    failure-images/halo-retouch.jpg
       failure-images/motion.jpg          failure-images/focus-gap.jpg

Each group's function below declares its frames, windows and flags with the
reason for them. A full run takes about ten minutes with a debug build (the
alignment runs dominate); pass `--cli target/release/focusforge-cli` to use a
release build instead, and `--check` to confirm that it reproduces the
committed images.

`--check` does steps 1-4 and compares the result with the committed images
instead of moving anything; it exits 1 on any difference. `--cli` names the
`focusforge-cli` binary (default `target/debug/focusforge-cli`).

Python standard library only. The image rights notice in
`docs/assets/README.md` covers everything this script writes.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import subprocess
import sys
from array import array

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
import rawtiff  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FRAMES = os.path.join(REPO, "fixtures", "mantis-512", "frames")
SCRATCH = os.path.join(REPO, ".scratch", "manual-examples")
ASSETS = os.path.join(REPO, "docs", "assets", "manual")

JPEG_QUALITY = 85
GUTTER = 8  # px of white between panels
GUTTER_RGB = (255, 255, 255)

# ---------------------------------------------------------------------------
# 8-bit RGB images: (width, height, bytearray of r,g,b rows)
# ---------------------------------------------------------------------------


class Image:
    def __init__(self, width: int, height: int, pixels: bytearray | None = None):
        self.width = width
        self.height = height
        self.pixels = pixels if pixels is not None else bytearray(width * height * 3)

    def crop(self, x: int, y: int, w: int, h: int) -> "Image":
        if x < 0 or y < 0 or x + w > self.width or y + h > self.height:
            raise SystemExit(
                f"crop {x},{y},{w},{h} falls outside a {self.width}x{self.height} image"
            )
        out = bytearray()
        for row in range(y, y + h):
            start = (row * self.width + x) * 3
            out += self.pixels[start : start + w * 3]
        return Image(w, h, out)


def read_tiff8(path: str) -> Image:
    """A 16-bit (or 8-bit) RGB or grey TIFF as 8-bit RGB, rounded to nearest."""
    info = rawtiff.read_info(path)
    raw = rawtiff.read_rect(path, 0, 0, info.width, info.height, info)
    if info.bytes_per_sample == 2:
        samples = array("H")
        samples.frombytes(raw)
        values = bytes((v * 255 + 32767) // 65535 for v in samples)
    else:
        values = bytes(raw)
    if info.samples_per_pixel == 1:
        values = bytes(v for v in values for _ in range(3))
    elif info.samples_per_pixel != 3:
        raise SystemExit(f"{path}: {info.samples_per_pixel} samples per pixel, expected 1 or 3")
    return Image(info.width, info.height, bytearray(values))


def read_zmap(path: str) -> tuple[int, int, list[int]]:
    info = rawtiff.read_info(path)
    samples = array("H")
    samples.frombytes(rawtiff.read_rect(path, 0, 0, info.width, info.height, info))
    return info.width, info.height, list(samples)


def side_by_side(panels: list[Image]) -> Image:
    height = panels[0].height
    if any(p.height != height for p in panels):
        raise SystemExit("panels of one image must share a height")
    width = sum(p.width for p in panels) + GUTTER * (len(panels) - 1)
    out = Image(width, height)
    gutter = bytes(GUTTER_RGB) * GUTTER
    for row in range(height):
        line = bytearray()
        for i, panel in enumerate(panels):
            if i:
                line += gutter
            start = row * panel.width * 3
            line += panel.pixels[start : start + panel.width * 3]
        out.pixels[row * width * 3 : (row + 1) * width * 3] = line
    return out


# ---------------------------------------------------------------------------
# Baseline JPEG encoder (ITU T.81): 4:4:4, standard Annex K tables scaled to
# a quality the way libjpeg does. Deterministic: the bytes depend only on the
# pixels and the quality.
# ---------------------------------------------------------------------------

_ZIGZAG = (
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
)  # fmt: skip

_LUMA_Q = (
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55,
    14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62,
    18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92,
    49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
)  # fmt: skip

_CHROMA_Q = (
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99,
    24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99, 99, 99, 99,
) + (99,) * 32  # fmt: skip

_DC_LUMA = ((0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0), tuple(range(12)))
_DC_CHROMA = ((0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0), tuple(range(12)))
_AC_LUMA = (
    (0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D),
    bytes.fromhex(
        "01020300041105122131410613516107227114328191a1082342b1c11552d1f0"
        "2433627282090a161718191a25262728292a3435363738393a43444546474849"
        "4a535455565758595a636465666768696a737475767778797a83848586878889"
        "8a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5"
        "c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8"
        "f9fa"
    ),
)
_AC_CHROMA = (
    (0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77),
    bytes.fromhex(
        "000102031104052131061241510761711322328108144291a1b1c109233352f0"
        "156272d10a162434e125f11718191a262728292a35363738393a434445464748"
        "494a535455565758595a636465666768696a737475767778797a828384858687"
        "88898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3"
        "c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8e9eaf2f3f4f5f6f7f8"
        "f9fa"
    ),
)

_COS = [
    [(math.sqrt(0.5) if u == 0 else 1.0) * 0.5 * math.cos((2 * x + 1) * u * math.pi / 16) for x in range(8)]
    for u in range(8)
]


def _scaled_table(base: tuple, quality: int) -> list[int]:
    scale = 5000 // quality if quality < 50 else 200 - 2 * quality
    return [min(255, max(1, (q * scale + 50) // 100)) for q in base]


def _huffman_codes(table: tuple) -> dict[int, tuple[int, int]]:
    bits, values = table
    if sum(bits) != len(values):
        raise AssertionError("Huffman table counts and values disagree")
    codes: dict[int, tuple[int, int]] = {}
    code = 0
    k = 0
    for length in range(1, 17):
        for _ in range(bits[length - 1]):
            codes[values[k]] = (code, length)
            code += 1
            k += 1
        code <<= 1
    return codes


class _BitWriter:
    def __init__(self) -> None:
        self.out = bytearray()
        self.acc = 0
        self.nbits = 0

    def write(self, value: int, length: int) -> None:
        self.acc = (self.acc << length) | (value & ((1 << length) - 1))
        self.nbits += length
        while self.nbits >= 8:
            self.nbits -= 8
            byte = (self.acc >> self.nbits) & 0xFF
            self.out.append(byte)
            if byte == 0xFF:
                self.out.append(0x00)
        self.acc &= (1 << self.nbits) - 1

    def flush(self) -> bytes:
        if self.nbits:
            self.write((1 << (8 - self.nbits)) - 1, 8 - self.nbits)
        return bytes(self.out)


def _dct_quantize(block: list[float], qtable: list[int]) -> list[int]:
    """Forward 8x8 DCT of a level-shifted block, quantized, in zigzag order."""
    rows = []
    for y in range(8):
        r = block[y * 8 : y * 8 + 8]
        rows.append([sum(c[x] * r[x] for x in range(8)) for c in _COS])
    coef = [0.0] * 64
    for u in range(8):
        column = [rows[y][u] for y in range(8)]
        for v, c in enumerate(_COS):
            coef[v * 8 + u] = sum(c[y] * column[y] for y in range(8))
    out = []
    for k in range(64):
        value = coef[_ZIGZAG[k]] / qtable[_ZIGZAG[k]]
        out.append(int(value + 0.5) if value >= 0 else -int(-value + 0.5))
    return out


def _magnitude(value: int) -> tuple[int, int]:
    size = abs(value).bit_length()
    return size, (value if value >= 0 else value + (1 << size) - 1)


def encode_jpeg(image: Image, quality: int = JPEG_QUALITY) -> bytes:
    w, h = image.width, image.height
    px = image.pixels
    planes: list[list[float]] = [[], [], []]
    for i in range(0, w * h * 3, 3):
        r, g, b = px[i], px[i + 1], px[i + 2]
        planes[0].append(0.299 * r + 0.587 * g + 0.114 * b - 128.0)
        planes[1].append(-0.168736 * r - 0.331264 * g + 0.5 * b)
        planes[2].append(0.5 * r - 0.418688 * g - 0.081312 * b)
    luma_q = _scaled_table(_LUMA_Q, quality)
    chroma_q = _scaled_table(_CHROMA_Q, quality)
    tables = (
        (luma_q, _huffman_codes(_DC_LUMA), _huffman_codes(_AC_LUMA)),
        (chroma_q, _huffman_codes(_DC_CHROMA), _huffman_codes(_AC_CHROMA)),
        (chroma_q, _huffman_codes(_DC_CHROMA), _huffman_codes(_AC_CHROMA)),
    )
    bw = _BitWriter()
    previous_dc = [0, 0, 0]
    for by in range(0, h, 8):
        for bx in range(0, w, 8):
            for comp in range(3):
                plane = planes[comp]
                block = [
                    plane[min(by + y, h - 1) * w + min(bx + x, w - 1)] for y in range(8) for x in range(8)
                ]
                qtable, dc_codes, ac_codes = tables[comp]
                coefs = _dct_quantize(block, qtable)
                diff = coefs[0] - previous_dc[comp]
                previous_dc[comp] = coefs[0]
                size, bits = _magnitude(diff)
                code, length = dc_codes[size]
                bw.write(code, length)
                if size:
                    bw.write(bits, size)
                run = 0
                for k in range(1, 64):
                    value = coefs[k]
                    if value == 0:
                        run += 1
                        continue
                    while run > 15:
                        code, length = ac_codes[0xF0]
                        bw.write(code, length)
                        run -= 16
                    size, bits = _magnitude(value)
                    code, length = ac_codes[(run << 4) | size]
                    bw.write(code, length)
                    bw.write(bits, size)
                    run = 0
                if run:
                    code, length = ac_codes[0x00]
                    bw.write(code, length)
    scan = bw.flush()

    def segment(marker: int, payload: bytes) -> bytes:
        return bytes((0xFF, marker)) + (len(payload) + 2).to_bytes(2, "big") + payload

    def dht(cls_id: int, table: tuple) -> bytes:
        bits, values = table
        return bytes((cls_id,)) + bytes(bits) + bytes(values)

    out = bytearray(b"\xff\xd8")
    out += segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")
    out += segment(0xDB, bytes((0,)) + bytes(luma_q[_ZIGZAG[k]] for k in range(64)))
    out += segment(0xDB, bytes((1,)) + bytes(chroma_q[_ZIGZAG[k]] for k in range(64)))
    out += segment(
        0xC0,
        bytes((8,)) + h.to_bytes(2, "big") + w.to_bytes(2, "big") + bytes((3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1)),
    )
    out += segment(
        0xC4,
        dht(0x00, _DC_LUMA) + dht(0x10, _AC_LUMA) + dht(0x01, _DC_CHROMA) + dht(0x11, _AC_CHROMA),
    )
    out += segment(0xDA, bytes((3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0)))
    out += scan
    out += b"\xff\xd9"
    return bytes(out)


# ---------------------------------------------------------------------------
# Fusing through focusforge-cli
# ---------------------------------------------------------------------------


class Fused:
    """A fused project and where its composite sits in the reference frame.

    With alignment on, the composite is cropped to the area every frame
    covers; `crop_x`/`crop_y` is that crop's corner in the alignment
    reference frame's pixels, which is what lets two stacks over the same
    reference be cut to the same window.
    """

    def __init__(self, project: str, crop_x: int, crop_y: int):
        self.project = project
        self.crop_x = crop_x
        self.crop_y = crop_y


class Runner:
    def __init__(self, cli: str, work: str):
        self.cli = cli
        self.work = work
        self.fused: dict[str, Fused] = {}

    def run(self, *args: str) -> str:
        result = subprocess.run(
            [self.cli, *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=False
        )
        if result.returncode != 0:
            sys.stderr.write(result.stdout + result.stderr)
            raise SystemExit(f"focusforge-cli {args[0]} exited {result.returncode}")
        return result.stdout

    def fuse(
        self, name: str, frames: list[tuple[int, int, int]], size: tuple[int, int], *flags: str, source: str = FRAMES
    ) -> Fused:
        """Cut `frames` into a fresh stack directory and fuse it.

        Each frame is (index, x, y): a `size` read starting at (x, y) of
        `<source>/NN.tif`, by default the fixture frames. Files keep that
        number, so a thinned stack still names its frames by focus position.
        A name already fused in this run is reused.
        """
        if name in self.fused:
            return self.fused[name]
        stack_dir = os.path.join(self.work, name)
        out_dir = os.path.join(self.work, name + "-project")
        for path in (stack_dir, out_dir):
            shutil.rmtree(path, ignore_errors=True)
        os.makedirs(os.path.join(stack_dir, "frames"))
        os.makedirs(out_dir)
        w, h = size
        for index, x, y in frames:
            path = os.path.join(source, f"{index:02d}.tif")
            info = rawtiff.read_info(path)
            pixels = rawtiff.read_rect(path, x, y, w, h, info)
            rawtiff.write_tiff(os.path.join(stack_dir, "frames", f"{index:02d}.tif"), w, h, pixels)
        report = json.loads(
            self.run("stack", stack_dir, "-o", out_dir, "--cpu", "--no-cache", "--no-progress", "--json", *flags)
        )
        projects = sorted(f for f in os.listdir(out_dir) if f.endswith(".ffp"))
        if len(projects) != 1:
            raise SystemExit(f"expected one project in {out_dir}, found {projects}")
        crop = report.get("crop") or {"x": 0, "y": 0}
        fused = Fused(os.path.join(out_dir, projects[0]), crop["x"], crop["y"])
        self.fused[name] = fused
        align = report.get("align") or {}
        fallback = [a["name"] for a in align.get("alignments", []) if a.get("fallback")]
        print(
            f"  fused {name}: {len(frames)} frames, crop at {crop['x']},{crop['y']}"
            + (f", not registered: {' '.join(fallback)}" if fallback else "")
        )
        return fused

    def composite(self, fused: Fused, project: str | None = None) -> Image:
        project = project or fused.project
        path = project[:-4] + "-composite.tif"
        self.run("export", project, "-o", path, "--exif", "off", "--icc", "off")
        return read_tiff8(path)

    def frame_tiff(self, fused: Fused, position: int, path: str | None = None) -> str:
        """Writes source frame `position` (0-based, in the stack's order) as
        fused: warped and cropped to the composite. Returns the TIFF's path."""
        path = path or fused.project[:-4] + f"-frame{position:02d}.tif"
        self.run("export", fused.project, "--frame", str(position), "--frame-output", path)
        return path

    def frame(self, fused: Fused, position: int) -> Image:
        return read_tiff8(self.frame_tiff(fused, position))

    def zmap(self, fused: Fused) -> tuple[int, int, list[int]]:
        path = fused.project[:-4] + "-zmap.tif"
        self.run("export", fused.project, "--zmap", path)
        return read_zmap(path)

    def retouched(self, fused: Fused, ops: list) -> Image:
        """The composite after `ops`, applied to a copy of the project."""
        copy = fused.project[:-4] + "-retouched.ffp"
        shutil.copyfile(fused.project, copy)
        ops_path = fused.project[:-4] + "-ops.json"
        with open(ops_path, "w", encoding="utf-8") as fh:
            json.dump(ops, fh)
        self.run("retouch", copy, "--ops", ops_path)
        return self.composite(fused, copy)


def colour_zmap(zmap: tuple[int, int, list[int]], colours: dict[int, tuple[int, int, int]], other: tuple) -> Image:
    width, height, values = zmap
    pixels = bytearray()
    for value in values:
        pixels += bytes(colours.get(value, other))
    return Image(width, height, pixels)


def enlarge(image: Image, factor: int) -> Image:
    """Nearest-neighbour enlargement: every pixel becomes a factor x factor square."""
    out = bytearray()
    for row in range(image.height):
        line = bytearray()
        for col in range(image.width):
            start = (row * image.width + col) * 3
            line += image.pixels[start : start + 3] * factor
        out += line * factor
    return Image(image.width * factor, image.height * factor, out)


def in_reference(fused: Fused, x: int, y: int, w: int, h: int) -> tuple[int, int, int, int]:
    """A window given in the reference frame's pixels, in the composite's own."""
    return x - fused.crop_x, y - fused.crop_y, w, h


# ---------------------------------------------------------------------------
# The examples. Windows are (x, y, w, h); frames are fixture indices 00-07.
# ---------------------------------------------------------------------------

ORANGE = (240, 140, 30)
BLUE = (40, 110, 220)
GREY = (205, 205, 205)

ALL_FRAMES = [(i, 0, 0) for i in range(8)]
FULL = (512, 512)
MOVE_X, MOVE_Y = 20, 12
DETAILS = ("--algorithm", "laplacian")


def depth_of_field(r: Runner, emit) -> None:
    # Default settings (alignment and crop on), Details method. Window: the
    # antenna (sharp in frame 02) and the rim of the head with its hairs
    # (sharp in frame 03), above the soft part of the head.
    fused = r.fuse("all-details", ALL_FRAMES, FULL, *DETAILS)
    window = (45, 0, 320, 224)
    frames = [r.frame(fused, i).crop(*window) for i in (2, 3, 5)]
    emit("depth-of-field/source-frames.jpg", side_by_side(frames))
    composite = r.composite(fused).crop(*window)
    zmap = colour_zmap(r.zmap(fused), {2: ORANGE, 3: BLUE}, GREY).crop(*window)
    emit("depth-of-field/composite.jpg", side_by_side([composite, zmap]))


def halos(r: Runner, emit) -> None:
    # The same eight frames under each fusion method, default settings. Window:
    # the antenna against the dark background and the rim of the head, the two
    # depth edges of this crop.
    window = (109, 0, 256, 224)
    panels = []
    for algorithm in ("laplacian", "weighted-average", "depth-map"):
        name = "all-details" if algorithm == "laplacian" else "all-" + algorithm
        fused = r.fuse(name, ALL_FRAMES, FULL, "--algorithm", algorithm)
        panels.append(r.composite(fused).crop(*window))
    emit("failure-images/halo-methods.jpg", side_by_side(panels))
    # Where the antenna (near) crosses the rim (far), Details keeps the rim from
    # frame 03 over a soft antenna. One brush stroke from frame 02 along the
    # antenna repairs it. Shown enlarged 2x.
    fused = r.fuse("all-details", ALL_FRAMES, FULL, *DETAILS)
    stroke = {
        "brush-stroke": {
            "source_id": 2,
            "path": [{"x": 222, "y": 96}, {"x": 186, "y": 150}, {"x": 160, "y": 205}],
            "radius": 16.0,
            "softness": 0.5,
        }
    }
    window = (120, 80, 160, 160)
    before = r.composite(fused).crop(*window)
    after = r.retouched(fused, [stroke]).crop(*window)
    emit("failure-images/halo-retouch.jpg", side_by_side([enlarge(before, 2), enlarge(after, 2)]))


def motion(r: Runner, emit) -> None:
    # Simulated movement between frames 02 and 03: frames 03-07 are read MOVE_X px
    # right and MOVE_Y px down, so their content sits that far left and up.
    # Left: the default stack's lined-up frames (`--frame`), moved, fused with
    # alignment off. Right: the same movement on the fixture frames, fused with
    # translation-only alignment (rotation + scale falls back to identity in this
    # small featureless window). The window is in default-stack composite pixels.
    base = r.fuse("all-details", ALL_FRAMES, FULL, *DETAILS)
    lined_up = os.path.join(r.work, "motion-lined-up")
    os.makedirs(lined_up, exist_ok=True)
    for i in range(8):
        r.frame_tiff(base, i, os.path.join(lined_up, f"{i:02d}.tif"))
    moved = [(i, 2, 2) for i in range(3)] + [(i, 2 + MOVE_X, 2 + MOVE_Y) for i in range(3, 8)]
    simulated = r.fuse("motion-unaligned", moved, (336, 336), *DETAILS, "--no-align", source=lined_up)
    moved = [(i, 4, 4) for i in range(3)] + [(i, 4 + MOVE_X, 4 + MOVE_Y) for i in range(3, 8)]
    realigned = r.fuse("motion-aligned", moved, (480, 480), *DETAILS, "--align-set", "mode=translation")
    x, y, w, h = 55, 20, 240, 240
    # Frame 03's pixel (x, y) of the lined-up frames, in each stack's reference.
    left = in_reference(simulated, x - 2 - MOVE_X, y - 2 - MOVE_Y, w, h)
    right = in_reference(realigned, x + base.crop_x - 4 - MOVE_X, y + base.crop_y - 4 - MOVE_Y, w, h)
    panels = [r.composite(simulated).crop(*left), r.composite(realigned).crop(*right)]
    emit("failure-images/motion.jpg", side_by_side(panels))


def gaps(r: Runner, emit) -> None:
    # A real thinned subset: all eight frames against the same frames without
    # frame 02, the only one with the antenna sharp. Alignment off for both, so
    # the two windows match pixel for pixel. Window: the antenna, its base and
    # the fine hair below it.
    full = r.fuse("gap-all", ALL_FRAMES, FULL, *DETAILS, "--no-align")
    thinned = r.fuse("gap-without-02", [(i, 0, 0) for i in (0, 1, 3, 4, 5, 6, 7)], FULL, *DETAILS, "--no-align")
    window = (122, 2, 384, 384)
    panels = [r.composite(f).crop(*in_reference(f, *window)) for f in (full, thinned)]
    emit("failure-images/focus-gap.jpg", side_by_side(panels))


GROUPS = {
    "depth-of-field": depth_of_field,
    "halos": halos,
    "motion": motion,
    "gaps": gaps,
}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Regenerate the manual's focus-stacking example images.")
    parser.add_argument("groups", nargs="*", metavar="GROUP", help="one of: " + ", ".join(GROUPS) + " (default: all)")
    parser.add_argument("--cli", default=os.path.join(REPO, "target", "debug", "focusforge-cli"))
    parser.add_argument("--check", action="store_true", help="compare with the committed images, write nothing")
    args = parser.parse_args(argv)
    sys.stdout.reconfigure(line_buffering=True)
    unknown = [g for g in args.groups if g not in GROUPS]
    if unknown:
        parser.error(f"unknown group {', '.join(unknown)}; choose from {', '.join(GROUPS)}")
    if not (os.path.isfile(args.cli) and os.access(args.cli, os.X_OK)):
        print(f"no focusforge-cli at {args.cli}: build it (cargo build -p focusforge-cli) or pass --cli", file=sys.stderr)
        return 2
    work = os.path.join(SCRATCH, "work")
    out = os.path.join(SCRATCH, "out")
    for path in (work, out):
        shutil.rmtree(path, ignore_errors=True)
        os.makedirs(path)
    written: list[str] = []

    def emit(relative: str, image: Image) -> None:
        path = os.path.join(out, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        data = encode_jpeg(image)
        with open(path, "wb") as fh:
            fh.write(data)
        written.append(relative)
        print(f"  {relative}: {image.width}x{image.height}, {len(data) // 1024} KB")

    runner = Runner(os.path.abspath(args.cli), work)
    for group in args.groups or list(GROUPS):
        print(group)
        GROUPS[group](runner, emit)

    differ = []
    for relative in written:
        source = os.path.join(out, relative)
        target = os.path.join(ASSETS, relative)
        if args.check:
            with open(source, "rb") as a:
                same = os.path.isfile(target) and a.read() == open(target, "rb").read()
            if not same:
                differ.append(relative)
        else:
            os.makedirs(os.path.dirname(target), exist_ok=True)
            shutil.move(source, target)
    if args.check:
        for relative in differ:
            print(f"differs from docs/assets/manual/{relative}")
        print("check: " + ("FAILED" if differ else f"all {len(written)} images match"))
        return 1 if differ else 0
    print(f"wrote {len(written)} images under docs/assets/manual/")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
