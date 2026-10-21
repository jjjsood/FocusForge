#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Compare two TIFFs sample by sample, or summarise a Z-Map index plane.

The tolerance argument is the whole point. Two things get compared in this
project and they are not the same claim:

  - our output against `golden/*.tif` -- our own reviewed output, frozen. Tight
    tolerance, usually 0: a difference is a behaviour change someone signs off on.
  - our output against `reference.tif` -- a crop of a stack made by a different
    stacker. Loose tolerance: it is a similarity floor, and demanding an exact
    match would be demanding that we reimplemented someone else's algorithm.

So `--tol` defaults to 0 (exact) and the caller raises it deliberately.

`--zmap` switches to reading A and B as single-channel index maps: a Z-Map
names a source frame per pixel, so its mean and its PSNR are meaningless
arithmetic on labels. What matters there is which indices appear and how often,
which is what the histogram shows.

Usage: tools/tiff-diff.py A.tif B.tif [--tol N] [--zmap] [--force]
       tools/tiff-diff.py --selftest
Exit:  0 max abs difference <= tol, 1 above it or inputs incomparable,
       2 usage error.

`--selftest` builds synthetic pairs whose difference is known by construction
and checks the verdict and the reported maximum against it. This tool is the
oracle every golden comparison in the project rests on, so "it printed a
number" is not evidence that the number was right.
"""

from __future__ import annotations

import argparse
import math
import os
import sys
from array import array
from collections import Counter

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
import rawtiff  # noqa: E402

# This tool reads both images in full, so it is for fixture-sized files. A
# reference-stack frame is 268 MB and comparing two of them in Python is a
# mistake, not a slow success -- refuse rather than swap.
MAX_BYTES = 256 * 1024 * 1024

try:
    import numpy as _np
except ImportError:  # the stdlib path below is complete, not a degraded mode
    _np = None


def _samples(path: str, info: rawtiff.TiffInfo):
    """All samples of an image as an unsigned integer sequence."""
    data = rawtiff.read_rect(path, 0, 0, info.width, info.height, info=info)
    if info.bytes_per_sample == 1:
        return _np.frombuffer(data, dtype=_np.uint8) if _np is not None else array("B", data)
    if _np is not None:
        return _np.frombuffer(data, dtype="<u2")
    vals = array("H")
    vals.frombytes(data)
    if sys.byteorder != "little":
        vals.byteswap()
    return vals


def _stats(a, b, tol: int):
    """(max abs diff, mean abs diff, samples above tol) over two sequences."""
    if _np is not None:
        diff = _np.abs(a.astype(_np.int64) - b.astype(_np.int64))
        return int(diff.max()), float(diff.mean()), int((diff > tol).sum())
    worst = 0
    total = 0
    over = 0
    for x, y in zip(a, b):
        d = x - y if x > y else y - x
        total += d
        if d > worst:
            worst = d
        if d > tol:
            over += 1
    return worst, total / len(a), over


def _bbox(a, b, tol: int, width: int, height: int, spp: int):
    """Bounding rect `(x, y, w, h)` of pixels differing beyond `tol` in any
    sample, or `None` when no pixel differs."""
    if _np is not None:
        diff = _np.abs(a.astype(_np.int64) - b.astype(_np.int64)).reshape(height, width, spp)
        differs = (diff > tol).any(axis=2)
        if not differs.any():
            return None
        ys, xs = _np.nonzero(differs)
        x0, x1 = int(xs.min()), int(xs.max())
        y0, y1 = int(ys.min()), int(ys.max())
        return (x0, y0, x1 - x0 + 1, y1 - y0 + 1)
    x0 = y0 = x1 = y1 = None
    for pixel in range(width * height):
        base = pixel * spp
        if not any(abs(int(a[base + s]) - int(b[base + s])) > tol for s in range(spp)):
            continue
        x, y = pixel % width, pixel // width
        x0 = x if x0 is None else min(x0, x)
        x1 = x if x1 is None else max(x1, x)
        y0 = y if y0 is None else min(y0, y)
        y1 = y if y1 is None else max(y1, y)
    if x0 is None:
        return None
    return (x0, y0, x1 - x0 + 1, y1 - y0 + 1)


def _histogram(label: str, values, peak: int) -> None:
    counts = Counter(int(v) for v in values)
    total = sum(counts.values())
    print(
        f"{label}: min={min(counts)} max={max(counts)} distinct={len(counts)} "
        f"samples={total}"
    )
    for index, count in counts.most_common(8):
        print(f"    index {index:5d}  {count:9d}  {100.0 * count / total:5.1f}%")
    if len(counts) > 8:
        print(f"    ... {len(counts) - 8} further indices")
    if max(counts) > peak:
        print(f"    warning: index {max(counts)} exceeds the {peak}-value range")


def _selftest() -> int:
    """Synthetic pairs with a difference known by construction.

    Every case writes its inputs with rawtiff.write_tiff, so the expected
    maximum absolute difference is arithmetic rather than a previous run's
    output frozen into an assertion.
    """
    import struct
    import tempfile

    w, h = 8, 4

    def rgb16(fn, width=w, height=h):
        return b"".join(
            struct.pack("<3H", *fn(x, y)) for y in range(height) for x in range(width)
        )

    def gray16(fn):
        return b"".join(struct.pack("<H", fn(x, y)) for y in range(h) for x in range(w))

    base = rgb16(lambda x, y: (1000 + 7 * x, 2000 + 11 * y, 3000))
    # one sample raised by exactly 5; every other sample identical
    off5 = bytearray(base)
    struct.pack_into("<H", off5, 0, 1005)
    # two pixels differing, (5, 1) and (2, 3), everything else identical
    two_px = bytearray(base)
    for x, y in ((5, 1), (2, 3)):
        struct.pack_into("<H", two_px, ((y * w + x) * 3) * 2, 4000)

    cases = []  # (label, argv, expected exit, substrings that must appear)
    with tempfile.TemporaryDirectory(prefix="tiff-diff-selftest-") as d:
        p = lambda n: os.path.join(d, n)  # noqa: E731
        rawtiff.write_tiff(p("a.tif"), w, h, base)
        rawtiff.write_tiff(p("a-copy.tif"), w, h, base)
        rawtiff.write_tiff(p("b5.tif"), w, h, bytes(off5))
        rawtiff.write_tiff(p("two-px.tif"), w, h, bytes(two_px))
        rawtiff.write_tiff(p("wide.tif"), w * 2, h, rgb16(lambda x, y: (0, 0, 0), w * 2, h))
        rawtiff.write_tiff(p("z1.tif"), w, h, gray16(lambda x, y: x % 3), bits=16, spp=1)
        rawtiff.write_tiff(p("z2.tif"), w, h, gray16(lambda x, y: (x + 1) % 3), bits=16, spp=1)
        # identical files pass at the default tolerance of 0
        cases.append(("identical", [p("a.tif"), p("a-copy.tif")], 0,
                      ["max abs diff:  0", "OK: within tolerance 0"]))
        # a single sample off by 5 fails at tol 0 and 4, passes at 5
        cases.append(("off-by-5 at tol 0", [p("a.tif"), p("b5.tif")], 1,
                      ["max abs diff:  5", "samples > tol(0): 1 of 96", "FAIL"]))
        cases.append(("off-by-5 at tol 4", [p("a.tif"), p("b5.tif"), "--tol", "4"], 1,
                      ["max abs diff:  5"]))
        cases.append(("off-by-5 at tol 5", [p("a.tif"), p("b5.tif"), "--tol", "5"], 0,
                      ["OK: within tolerance 5"]))
        # different dimensions are incomparable, not a large difference
        cases.append(("incomparable", [p("a.tif"), p("wide.tif")], 1,
                      ["FAIL: incomparable images"]))
        # --zmap reports which indices appear, and refuses a 3-channel input
        cases.append(("zmap histogram", [p("z1.tif"), p("z2.tif"), "--zmap"], 1,
                      ["A indices: min=0 max=2 distinct=3", "pixels differing: 32 of 32"]))
        cases.append(("zmap on rgb", [p("a.tif"), p("a-copy.tif"), "--zmap"], 1,
                      ["--zmap needs a single-channel index map"]))
        # a negative tolerance is a usage error, not a tolerance of zero
        cases.append(("negative tol", [p("a.tif"), p("a-copy.tif"), "--tol", "-1"], 2, []))
        # --bbox reports the bounding rect of differing pixels
        cases.append(("bbox single pixel", [p("a.tif"), p("b5.tif"), "--bbox"], 1,
                      ["differing bbox: x=0 y=0 w=1 h=1"]))
        cases.append(("bbox two pixels", [p("a.tif"), p("two-px.tif"), "--bbox"], 1,
                      ["differing bbox: x=2 y=1 w=4 h=3"]))

        failed = 0
        for label, argv, want_exit, want_text in cases:
            import io as _io

            buf = _io.StringIO()
            real_out, real_err = sys.stdout, sys.stderr
            sys.stdout = sys.stderr = buf
            try:
                got = main(argv)
            finally:
                sys.stdout, sys.stderr = real_out, real_err
            text = buf.getvalue()
            problems = []
            if got != want_exit:
                problems.append(f"exit {got}, wanted {want_exit}")
            for needle in want_text:
                if needle not in text:
                    problems.append(f"missing {needle!r}")
            if problems:
                failed += 1
                print(f"FAIL {label}: {'; '.join(problems)}")
                print("    " + text.replace("\n", "\n    ").rstrip())
            else:
                print(f"ok   {label}")

    if failed:
        print(f"tiff-diff --selftest: {failed} of {len(cases)} cases failed")
        return 1
    print(f"tiff-diff --selftest: all {len(cases)} cases pass")
    return 0


def main(argv=None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    if list(argv) == ["--selftest"]:
        return _selftest()

    parser = argparse.ArgumentParser(
        prog="tiff-diff.py",
        description="Compare two TIFFs sample by sample, or summarise a Z-Map.",
    )
    parser.add_argument("a")
    parser.add_argument("b")
    parser.add_argument(
        "--tol", type=int, default=0, help="largest per-sample difference still a pass"
    )
    parser.add_argument(
        "--zmap",
        action="store_true",
        help="read both files as single-channel index maps and print index histograms",
    )
    parser.add_argument(
        "--force", action="store_true", help=f"allow inputs larger than {MAX_BYTES // 2**20} MB"
    )
    parser.add_argument(
        "--bbox",
        action="store_true",
        help="print the bounding rect of pixels differing beyond --tol",
    )
    args = parser.parse_args(argv)

    if args.tol < 0:
        print("tiff-diff: --tol must not be negative", file=sys.stderr)
        return 2

    try:
        for path in (args.a, args.b):
            size = os.path.getsize(path)
            if size > MAX_BYTES and not args.force:
                print(
                    f"tiff-diff: {path} is {size / 2**20:.0f} MB -- this tool reads whole "
                    f"images and is meant for fixtures. Pass --force to insist.",
                    file=sys.stderr,
                )
                return 2
        info_a = rawtiff.read_info(args.a)
        info_b = rawtiff.read_info(args.b)
    except (OSError, rawtiff.UnsupportedTiff) as exc:
        print(f"tiff-diff: {exc}", file=sys.stderr)
        return 2

    shape_a = (info_a.width, info_a.height, info_a.samples_per_pixel, info_a.bits_per_sample[0])
    shape_b = (info_b.width, info_b.height, info_b.samples_per_pixel, info_b.bits_per_sample[0])
    if shape_a != shape_b:
        print(
            f"FAIL: incomparable images\n"
            f"  A {args.a}: {shape_a[0]}x{shape_a[1]} {shape_a[2]}ch {shape_a[3]}-bit\n"
            f"  B {args.b}: {shape_b[0]}x{shape_b[1]} {shape_b[2]}ch {shape_b[3]}-bit"
        )
        return 1

    if args.zmap and info_a.samples_per_pixel != 1:
        print(
            f"FAIL: --zmap needs a single-channel index map, "
            f"{args.a} has {info_a.samples_per_pixel} samples per pixel"
        )
        return 1

    peak = (1 << info_a.bits_per_sample[0]) - 1
    a = _samples(args.a, info_a)
    b = _samples(args.b, info_b)
    worst, mean, over = _stats(a, b, args.tol)

    print(f"A {args.a}")
    print(f"B {args.b}")
    print(f"{info_a.width}x{info_a.height} {info_a.samples_per_pixel}ch {info_a.bits_per_sample[0]}-bit")

    if args.zmap:
        _histogram("A indices", a, peak)
        _histogram("B indices", b, peak)
        print(f"pixels differing: {over} of {len(a)}")
    else:
        if worst == 0:
            psnr = "inf (identical)"
        else:
            mse = _mse(a, b)
            psnr = f"{10.0 * math.log10(peak * peak / mse):.2f} dB"
        print(f"max abs diff:  {worst}")
        print(f"mean abs diff: {mean:.4f}")
        print(f"PSNR:          {psnr}")
        print(f"samples > tol({args.tol}): {over} of {len(a)}")

    if args.bbox:
        bbox = _bbox(a, b, args.tol, info_a.width, info_a.height, info_a.samples_per_pixel)
        if bbox is None:
            print("differing bbox: none")
        else:
            x, y, bw, bh = bbox
            print(f"differing bbox: x={x} y={y} w={bw} h={bh}")

    if worst <= args.tol:
        print(f"OK: within tolerance {args.tol}")
        return 0
    print(f"FAIL: max abs diff {worst} exceeds tolerance {args.tol}")
    return 1


def _mse(a, b) -> float:
    if _np is not None:
        diff = a.astype(_np.float64) - b.astype(_np.float64)
        return float((diff * diff).mean())
    total = 0
    for x, y in zip(a, b):
        d = x - y
        total += d * d
    return total / len(a)


if __name__ == "__main__":
    sys.exit(main())
