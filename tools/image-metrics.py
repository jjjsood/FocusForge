#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Reference-free quality numbers for a fused composite, read in row bands.

Usage: tools/image-metrics.py COMPOSITE.tif [--zmap ZMAP.tif] [--vs OTHER.tif] [--json]
       tools/image-metrics.py --selftest
Exit:  0 report printed, 1 unreadable or incomparable input, 2 usage error.

Inputs are uncompressed, strip-based 8- or 16-bit TIFF files (`focusforge-cli
export` writes one by default). Rows are read in bands through
tools/lib/rawtiff.py, so memory stays bounded for any image size.

`--selftest` builds synthetic images whose answers are known by construction
and checks every measure and every refusal against them.
"""

from __future__ import annotations

import argparse
import io
import json
import math
import operator
import os
import random
import struct
import sys
import tempfile
import textwrap
from array import array

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
import rawtiff  # noqa: E402

BAND_ROWS = 64
STRIPE_RADIUS = 4

COMPOSITE_KEYS = ("clip_low", "clip_high", "stripe_rows", "stripe_columns", "grain")
PAIR_KEYS = ("mean_abs_diff", "max_abs_diff", "differing_fraction")

EXPLAIN = {
    "clip_low": (
        "share of samples equal to 0, the darkest value. It rises when shadows are crushed or a "
        "blend leaves its range. A low value does not show that shadow detail survived, and black "
        "the scene really contains counts too."
    ),
    "clip_high": (
        "share of samples at the brightest value of the bit depth. Same reading as clip_low for "
        "highlights; lamps and reflections that are saturated in the source frames count too."
    ),
    "stripe_rows": (
        "largest deviation, in sample counts, of one row's mean level from the mean of the 9 rows "
        "around it (the 4 outermost rows at each edge are left out). A bar a few rows high or a "
        "seam between exposures raises it; a slow gradient does not. Rows of real scene "
        "structure raise it too, so one value proves neither a stripe nor its absence."
    ),
    "stripe_columns": "the same measure over column means.",
    "grain": (
        "standard deviation, in sample counts, of each pixel minus the mean of its 3x3 "
        "neighbourhood, over the whole image. Sensor noise, noise amplified by blending and fine "
        "real detail raise it; smoothing lowers it without showing that anything is better. No "
        "flat-area mask is applied, so it does not isolate noise."
    ),
    "coherence": (
        "fraction of Z-Map pixels whose present 8 neighbours all name the same source frame as "
        "the pixel (the figure `focusforge-cli review` reports). 1 means one source everywhere; "
        "lower means patchier source choice. A smoother map is not proof of a correct one, and a "
        "scene with many depth layers has seams of its own."
    ),
    "mean_abs_diff": (
        "mean over all samples of |A - B|, in sample counts, between the two composites. It says "
        "how far apart they are, not which one is closer to the true scene."
    ),
    "max_abs_diff": "largest |A - B| over all samples; one stray sample sets it.",
    "differing_fraction": "share of samples where A and B differ at all.",
}

BASELINE = (
    "No value here is a limit and none ranks a composite. Stripe and grain carry the scene's own "
    "structure, so a reading means something only next to a baseline of the same scene: run a "
    "second method's composite of the same stack (or a known-good composite of that scene) "
    "through this tool and compare the same metric; --vs adds how far the two differ."
)


class Refused(Exception):
    """An input the tool cannot compare, with the file and what was found."""


def _bands(path: str, info: rawtiff.TiffInfo, band_rows: int):
    """Yield `(rows, planes)`: one full-width band, one `array` of samples per channel."""
    code = "H" if info.bytes_per_sample == 2 else "B"
    spp = info.samples_per_pixel
    for y in range(0, info.height, band_rows):
        rows = min(band_rows, info.height - y)
        raw = array(code)
        raw.frombytes(rawtiff.read_rect(path, 0, y, info.width, rows, info))
        if code == "H" and sys.byteorder == "big":
            raw.byteswap()
        yield rows, [raw[c::spp] for c in range(spp)]


def _deviation(values: list, r: int = STRIPE_RADIUS):
    """Largest |value - mean of its (2r+1) window|, the r outermost entries per side left out."""
    n = len(values)
    if n <= 2 * r:
        return None
    return max(abs(values[i] - sum(values[i - r : i + r + 1]) / (2 * r + 1)) for i in range(r, n - r))


class _Channel:
    """Composite measures of one channel, fed one row at a time, top to bottom."""

    def __init__(self, width: int, maxval: int):
        self.width = width
        self.maxval = maxval
        self.rows = 0
        self.low = self.high = 0
        self.row_means = []
        self.col_sums = [0] * width
        self.window = []  # the last three rows
        self.n = self.total = self.total_sq = 0  # exact integer sums of the grain residual

    def push(self, row) -> None:
        self.rows += 1
        self.low += row.count(0)
        self.high += row.count(self.maxval)
        self.row_means.append(sum(row) / self.width)
        self.col_sums = list(map(operator.add, self.col_sums, row))
        self.window.append(row)
        if len(self.window) < 3:
            return
        if self.width >= 3:
            above, mid, below = self.window
            s = list(map(operator.add, map(operator.add, above, mid), below))
            # nine times the residual: 9 * pixel - sum of its 3x3 block
            d = [9 * v - p - q - t for v, p, q, t in zip(mid[1:-1], s, s[1:], s[2:])]
            self.n += len(d)
            self.total += sum(d)
            self.total_sq += sum(map(operator.mul, d, d))
        del self.window[0]

    def results(self) -> dict:
        samples = self.rows * self.width
        grain = None
        if self.n:
            grain = math.sqrt(max(self.total_sq / self.n - (self.total / self.n) ** 2, 0.0)) / 9
        return dict(
            clip_low=self.low / samples,
            clip_high=self.high / samples,
            stripe_rows=_deviation(self.row_means),
            stripe_columns=_deviation([c / self.rows for c in self.col_sums]),
            grain=grain,
        )


class _Pair:
    """Absolute difference of one channel of two composites, fed row by row."""

    def __init__(self):
        self.samples = self.total = self.biggest = self.differing = 0

    def push(self, a, b) -> None:
        diff = list(map(abs, map(operator.sub, a, b)))
        self.samples += len(diff)
        self.total += sum(diff)
        self.biggest = max(self.biggest, max(diff))
        self.differing += len(diff) - diff.count(0)

    def results(self) -> dict:
        return dict(
            mean_abs_diff=self.total / self.samples,
            max_abs_diff=self.biggest,
            differing_fraction=self.differing / self.samples,
        )


def _agree(row: list, other: list, same_row: bool) -> list:
    """Per pixel of `row`: does every present neighbour of it in `other` equal it?

    Out-of-range columns are skipped, never a mismatch. In the pixel's own row the
    pixel itself is not a neighbour.
    """
    left = [True] + [a == b for a, b in zip(row[1:], other)]
    right = [a == b for a, b in zip(row, other[1:])] + [True]
    if same_row:
        return [a and b for a, b in zip(left, right)]
    return [a and m == n and b for a, b, m, n in zip(left, right, row, other)]


class _Coherence:
    """All-eight-identical fraction of an index plane, fed row by row, top to bottom.

    A row is scored once the row below it has arrived, the last row at the end, so a
    pixel on a band boundary sees both neighbouring rows. Absent rows at the top and
    bottom edge are skipped, never a mismatch.
    """

    def __init__(self, width: int):
        self.width = width
        self.prev = self.held = None
        self.identical = self.scored = 0

    def _score(self, nxt) -> None:
        flags = _agree(self.held, self.held, True)
        for other in (self.prev, nxt):
            if other is not None:
                flags = [f and g for f, g in zip(flags, _agree(self.held, other, False))]
        self.identical += sum(flags)
        self.scored += self.width
        self.prev = self.held

    def push(self, row: list) -> None:
        if self.held is not None:
            self._score(row)
        self.held = row

    def result(self) -> float:
        self._score(None)
        return self.identical / self.scored


def _describe(info: rawtiff.TiffInfo) -> str:
    return (
        f"{info.width}x{info.height}, {info.samples_per_pixel} channel(s), "
        f"{info.bits_per_sample[0]}-bit"
    )


def _check_zmap(info: rawtiff.TiffInfo, composite: rawtiff.TiffInfo) -> None:
    if (info.width, info.height) != (composite.width, composite.height) or (
        info.samples_per_pixel,
        info.bits_per_sample[0],
    ) != (1, 16):
        raise Refused(
            f"{info.path}: {_describe(info)}; a Z-Map must be one 16-bit channel of "
            f"{composite.width}x{composite.height} like the composite"
        )


def _check_pair(info: rawtiff.TiffInfo, composite: rawtiff.TiffInfo) -> None:
    key = lambda i: (i.width, i.height, i.samples_per_pixel, i.bits_per_sample[0])  # noqa: E731
    if key(info) != key(composite):
        raise Refused(
            f"{info.path}: {_describe(info)}; --vs needs the composite's {_describe(composite)}"
        )


def measure(path: str, zmap: str | None = None, vs: str | None = None, band_rows: int = BAND_ROWS):
    """All numbers of one run: `{"info", "metrics", "zmap", "pair"}`; the last two are None unless asked."""
    info = rawtiff.read_info(path)
    zinfo = rawtiff.read_info(zmap) if zmap else None
    vinfo = rawtiff.read_info(vs) if vs else None
    if zinfo:
        _check_zmap(zinfo, info)
    if vinfo:
        _check_pair(vinfo, info)

    w, spp = info.width, info.samples_per_pixel
    chans = [_Channel(w, (1 << info.bits_per_sample[0]) - 1) for _ in range(spp)]
    pairs = [_Pair() for _ in range(spp)] if vinfo else None
    coh = _Coherence(w) if zinfo else None
    others = zip(
        _bands(path, info, band_rows),
        _bands(vs, vinfo, band_rows) if vinfo else iter(lambda: None, 0),
        _bands(zmap, zinfo, band_rows) if zinfo else iter(lambda: None, 0),
    )
    for (rows, planes), other, zband in others:
        for r in range(rows):
            lo, hi = r * w, (r + 1) * w
            for c in range(spp):
                chans[c].push(planes[c][lo:hi])
                if pairs:
                    pairs[c].push(planes[c][lo:hi], other[1][c][lo:hi])
            if coh:
                coh.push(zband[1][0][lo:hi].tolist())

    res = [ch.results() for ch in chans]
    return {
        "info": info,
        "metrics": {k: [r[k] for r in res] for k in COMPOSITE_KEYS},
        "zmap": {"path": zmap, "coherence": coh.result()} if coh else None,
        "pair": {"path": vs, **{k: [p.results()[k] for p in pairs] for k in PAIR_KEYS}}
        if pairs
        else None,
    }


def _channel_names(spp: int) -> list:
    if spp == 1:
        return ["grey"]
    if spp in (3, 4):
        return ["R", "G", "B", "A"][:spp]
    return [f"ch{i}" for i in range(spp)]


def _fmt(v) -> str:
    return "n/a" if v is None else f"{v:.6g}"


def _explain(keys) -> str:
    wrap = lambda k: textwrap.fill(  # noqa: E731
        f"{k}: {EXPLAIN[k]}", width=78, subsequent_indent="    "
    )
    return "\n".join(wrap(k) for k in keys)


def _human(rep: dict, path: str) -> str:
    info = rep["info"]
    names = _channel_names(info.samples_per_pixel)
    out = [f"composite: {path}", f"  {_describe(info)} ({', '.join(names)})", ""]

    def table(rows):
        out.append(f"{'metric':<20}" + "".join(f"{n:>14}" for n in names))
        for key, vals in rows:
            out.append(f"{key:<20}" + "".join(f"{_fmt(v):>14}" for v in vals))

    table([(k, rep["metrics"][k]) for k in COMPOSITE_KEYS])
    keys = list(COMPOSITE_KEYS)
    if rep["zmap"]:
        out += ["", f"z-map: {rep['zmap']['path']}", f"{'coherence':<20}{rep['zmap']['coherence']:.4f}"]
        keys.append("coherence")
    if rep["pair"]:
        out += ["", f"pair, composite against: {rep['pair']['path']}"]
        table([(k, rep["pair"][k]) for k in PAIR_KEYS])
        keys += PAIR_KEYS
    out += ["", "What the numbers mean", _explain(keys), "", textwrap.fill(BASELINE, width=78)]
    return "\n".join(out)


def _json(rep: dict, path: str) -> str:
    info = rep["info"]
    obj = {
        "composite": path,
        "width": info.width,
        "height": info.height,
        "channels": info.samples_per_pixel,
        "bits": info.bits_per_sample[0],
        **rep["metrics"],
    }
    if rep["zmap"]:
        obj["zmap"] = rep["zmap"]
    if rep["pair"]:
        obj["pair"] = rep["pair"]
    return json.dumps(obj, indent=2)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="image-metrics.py",
        description="Reference-free quality numbers for a fused composite (clipping, stripe "
        "energy, grain), optionally the Z-Map coherence and the difference to a second composite.",
        epilog="What the numbers mean\n" + _explain(EXPLAIN) + "\n\n" + textwrap.fill(BASELINE, 78),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("composite", help="uncompressed 8- or 16-bit TIFF")
    parser.add_argument("--zmap", metavar="FILE", help="exported Z-Map (one 16-bit channel, same size)")
    parser.add_argument("--vs", metavar="FILE", help="second composite of the same size, channels and bit depth")
    parser.add_argument("--json", action="store_true", help="print one JSON object instead of the table")
    return parser


def main(argv=None) -> int:
    argv = sys.argv[1:] if argv is None else list(argv)
    if argv == ["--selftest"]:
        return _selftest()
    try:
        args = _parser().parse_args(argv)
    except SystemExit as exc:
        return exc.code if isinstance(exc.code, int) else 2
    try:
        rep = measure(args.composite, args.zmap, args.vs)
    except (OSError, rawtiff.UnsupportedTiff, Refused) as exc:
        hint = ""
        if isinstance(exc, rawtiff.UnsupportedTiff):
            hint = " (export with `--tiff-compression none`, the default, to write a readable file)"
        print(f"image-metrics: {exc}{hint}", file=sys.stderr)
        return 1
    print((_json if args.json else _human)(rep, args.composite))
    return 0


# ----------------------------------------------------------------------------- selftest


def _selftest() -> int:
    """Synthetic images whose answers are known by construction.

    Composites are a gradient plus seeded noise with one defect injected at a
    time; Z-Maps and pairs are built so the expected value is arithmetic.
    """
    import statistics

    failed = 0
    total = 0

    def ok(label: str, cond: bool, detail: str = "") -> None:
        nonlocal failed, total
        total += 1
        if cond:
            print(f"ok   {label}")
        else:
            failed += 1
            print(f"FAIL {label}" + (f": {detail}" if detail else ""))

    def run(argv):
        buf = io.StringIO()
        real = sys.stdout, sys.stderr
        sys.stdout = sys.stderr = buf
        try:
            code = main(argv)
        finally:
            sys.stdout, sys.stderr = real
        return code, buf.getvalue()

    def write(path, w, h, samples, spp=3, bits=16, **kw):
        fmt = "<%d%s" % (len(samples), "H" if bits == 16 else "B")
        rawtiff.write_tiff(path, w, h, struct.pack(fmt, *samples), bits=bits, spp=spp, **kw)

    def m(path, **kw):
        return measure(path, **kw)["metrics"]

    w, h = 64, 48
    rng = random.Random(7)
    unit = [rng.gauss(0, 1) for _ in range(w * h * 3)]

    def scene(noise=20.0, edit=None):
        out = []
        for y in range(h):
            for x in range(w):
                for c in range(3):
                    v = 20000 + 100 * x + 5000 * c + noise * unit[(y * w + x) * 3 + c]
                    out.append(edit(x, y, c, int(round(v))) if edit else int(round(v)))
        return out

    clean = scene()
    bar = scene(edit=lambda x, y, c, v: v + 6000 if 20 <= y < 23 else v)
    flip = random.Random(3)
    sat = {i for i in range(len(clean)) if flip.random() < 0.05}
    clipped = [65535 if i in sat else v for i, v in enumerate(clean)]
    zero_patch = scene(edit=lambda x, y, c, v: 0 if x < 8 and y < 8 else v)
    noisy = scene(noise=200.0)

    with tempfile.TemporaryDirectory(prefix="image-metrics-selftest-") as d:
        p = lambda n: os.path.join(d, n)  # noqa: E731

        # --- layout independence: byte order x strip height x band height
        got = {}
        for order in ("II", "MM"):
            for rps in (1, 7):
                write(p("clean.tif"), w, h, clean, byte_order=order, rows_per_strip=rps)
                got[(order, rps, 5)] = m(p("clean.tif"), band_rows=5)
        for band in (1, h):
            got[("II", 1, band)] = m(p("clean.tif"), band_rows=band)
        ref = got[("II", 1, 5)]
        ok(
            "clean values equal across II/MM x strip 1/7 x band 1/5/full",
            all(v == ref for v in got.values()),
            str({k: v for k, v in got.items() if v != ref}),
        )
        write(p("clean.tif"), w, h, clean)
        base = m(p("clean.tif"))
        ok("clean control reports every composite measure", all(len(base[k]) == 3 for k in COMPOSITE_KEYS))
        ok("clean control has no clipping", base["clip_low"] == [0.0] * 3 and base["clip_high"] == [0.0] * 3, str(base))

        # --- independent whole-buffer references on the clean image
        plane = clean[0::3]
        means = [sum(plane[y * w : (y + 1) * w]) / w for y in range(h)]
        ref_rows = max(abs(means[i] - sum(means[i - 4 : i + 5]) / 9) for i in range(4, h - 4))
        ok("stripe_rows equals a direct window loop", math.isclose(base["stripe_rows"][0], ref_rows, rel_tol=1e-9), f"{base['stripe_rows'][0]} vs {ref_rows}")
        resid = [
            (9 * plane[y * w + x] - sum(plane[(y + j) * w + x + i] for j in (-1, 0, 1) for i in (-1, 0, 1))) / 9
            for y in range(1, h - 1)
            for x in range(1, w - 1)
        ]
        ref_grain = statistics.pstdev(resid)
        ok("grain equals pstdev of the 3x3 residual", math.isclose(base["grain"][0], ref_grain, rel_tol=1e-9), f"{base['grain'][0]} vs {ref_grain}")

        # --- each defect moves its own measure; the control does not
        write(p("bar.tif"), w, h, bar)
        b = m(p("bar.tif"))
        ok(
            "3-row bar: stripe_rows above 20x clean and above 2000",
            b["stripe_rows"][0] > 20 * base["stripe_rows"][0] and b["stripe_rows"][0] > 2000,
            f"{b['stripe_rows']} vs clean {base['stripe_rows']}",
        )
        ok("3-row bar leaves clipping and stripe_columns alone", b["clip_high"] == base["clip_high"] and b["stripe_columns"][0] < 2 * base["stripe_columns"][0] + 1, str(b))
        write(p("clip.tif"), w, h, clipped)
        k = m(p("clip.tif"))
        ok("5% saturated samples: clip_high above 0.04, clip_low unchanged", k["clip_high"][0] > 0.04 and k["clip_low"] == base["clip_low"], str(k))
        write(p("zero.tif"), w, h, zero_patch)
        z = m(p("zero.tif"))
        ok("zero patch: clip_low is 64 of 3072 per channel", z["clip_low"][0] == 64 / (w * h) and z["clip_high"] == base["clip_high"], str(z))
        write(p("noisy.tif"), w, h, noisy)
        n = m(p("noisy.tif"))
        ok("noise 200 against 20: grain above 5x clean, clipping unchanged", n["grain"][0] > 5 * base["grain"][0] and n["clip_high"] == base["clip_high"], f"{n['grain']} vs {base['grain']}")

        # --- exact answers by construction
        gh, gw = 10, 10
        write(p("uniform.tif"), gw, gh, [1000] * (gw * gh), spp=1)
        u = m(p("uniform.tif"))
        ok("uniform image: every measure is 0 except the absent ones", u["grain"] == [0.0] and u["stripe_rows"] == [0.0] and u["stripe_columns"] == [0.0] and u["clip_low"] == [0.0], str(u))
        write(p("checker.tif"), gw, gh, [1000 * ((x + y) % 2) for y in range(gh) for x in range(gw)], spp=1)
        c = m(p("checker.tif"))
        ok("checkerboard 0/1000: grain is 4000/9", math.isclose(c["grain"][0], 4000 / 9, rel_tol=1e-12), str(c["grain"]))
        ok("checkerboard 0/1000: clip_low is exactly one half", c["clip_low"] == [0.5], str(c["clip_low"]))
        rh = 30
        write(p("row.tif"), 12, rh, [1900 if y == 15 else 1000 for y in range(rh) for _ in range(12)], spp=1)
        r = m(p("row.tif"))
        ok("one row of +900: stripe_rows is 800, stripe_columns is 0", r["stripe_rows"] == [800.0] and r["stripe_columns"] == [0.0], str(r))
        write(p("g8.tif"), 6, 4, [255] * 6 + [0] * 6 + [7] * 12, spp=1, bits=8)
        g8 = m(p("g8.tif"))
        ok("8-bit grey: clip_high counts 255, clip_low counts 0", g8["clip_high"] == [6 / 24] and g8["clip_low"] == [6 / 24], str(g8))
        write(p("tiny.tif"), 2, 2, [1, 2, 3, 4], spp=1)
        t = m(p("tiny.tif"))
        ok("2x2 image: stripe and grain are absent, not a crash", t["stripe_rows"] == [None] and t["stripe_columns"] == [None] and t["grain"] == [None], str(t))
        write(p("flat5.tif"), 5, 5, [1000 * (x % 2) for y in range(5) for x in range(5)], spp=1)
        f5 = m(p("flat5.tif"))
        ok("5x5 image: stripes absent, grain present", f5["stripe_rows"] == [None] and f5["grain"][0] is not None, str(f5))

        # --- Z-Map coherence
        zw, zh = 23, 19

        def ref_coherence(v):
            n = 0
            for y in range(zh):
                for x in range(zw):
                    cen = v[y * zw + x]
                    n += all(
                        v[(y + dy) * zw + x + dx] == cen
                        for dy in (-1, 0, 1)
                        for dx in (-1, 0, 1)
                        if (dx or dy) and 0 <= y + dy < zh and 0 <= x + dx < zw
                    )
            return n / (zw * zh)

        write(p("zc.tif"), zw, zh, [0] * (zw * zh * 3))
        frng = random.Random(11)
        maps = {
            "uniform": ([5] * (zw * zh), 1.0),
            "checkerboard": ([(x + y) % 2 for y in range(zh) for x in range(zw)], 0.0),
            "one seam": ([int(x >= 11) for y in range(zh) for x in range(zw)], (zw - 2) / zw),
        }
        for name, (vals, want) in maps.items():
            write(p("zm.tif"), zw, zh, vals, spp=1)
            got = {b: measure(p("zc.tif"), zmap=p("zm.tif"), band_rows=b)["zmap"]["coherence"] for b in (1, 5, zh)}
            ok(f"z-map {name}: coherence is {want:.6g} at every band height", all(v == want for v in got.values()), str(got))
        blocks = [
            frng.randrange(3) if frng.random() < 0.1 else ((y // 4) + (x // 4)) % 3
            for y in range(zh)
            for x in range(zw)
        ]
        want = ref_coherence(blocks)
        for order, rps in (("II", 1), ("MM", 7)):
            write(p("zm.tif"), zw, zh, blocks, spp=1, byte_order=order, rows_per_strip=rps)
            got = {b: measure(p("zc.tif"), zmap=p("zm.tif"), band_rows=b)["zmap"]["coherence"] for b in (1, 5, zh)}
            ok(f"z-map random blocks {order}/{rps}: streamed equals the whole-buffer rule at band 1/5/full", all(v == want for v in got.values()) and 0 < want < 1, f"{got} vs {want}")

        # --- pair
        same = measure(p("clean.tif"), vs=p("clean.tif"))["pair"]
        ok("identical files: pair is all zero", all(v == [0.0] * 3 or v == [0] * 3 for k, v in same.items() if k != "path"), str(same))
        edited = list(clean)
        for (px, py, ch), delta in {(0, 0, 0): 5, (10, 3, 1): 100, (20, 40, 2): -40}.items():
            edited[(py * w + px) * 3 + ch] += delta
        write(p("edit.tif"), w, h, edited)
        pr = measure(p("clean.tif"), vs=p("edit.tif"))["pair"]
        npx = w * h
        ok(
            "one sample +5 and two changed pixels: max, mean and fraction exact",
            pr["max_abs_diff"] == [5, 100, 40]
            and pr["mean_abs_diff"] == [5 / npx, 100 / npx, 40 / npx]
            and pr["differing_fraction"] == [1 / npx] * 3,
            str(pr),
        )

        # --- command line: table, json, help
        write(p("zmap-ok.tif"), w, h, [0] * (w * h), spp=1)
        code, text = run([p("clean.tif"), "--zmap", p("zmap-ok.tif"), "--vs", p("edit.tif")])
        ok("table: exit 0 with every metric and its meaning", code == 0 and all(f"{k}:" in text for k in EXPLAIN) and "baseline of the same scene" in text, text[-300:])
        code, text = run([p("clean.tif"), "--json"])
        obj = json.loads(text) if code == 0 else {}
        want_keys = ["composite", "width", "height", "channels", "bits", *COMPOSITE_KEYS]
        ok("json: fixed keys, no zmap or pair, no version", list(obj) == want_keys, str(list(obj)))
        code, text = run([p("clean.tif"), "--json", "--zmap", p("zmap-ok.tif"), "--vs", p("edit.tif")])
        obj = json.loads(text) if code == 0 else {}
        ok(
            "json with --zmap and --vs adds zmap and pair",
            list(obj)[-2:] == ["zmap", "pair"]
            and list(obj["zmap"]) == ["path", "coherence"]
            and list(obj["pair"]) == ["path", *PAIR_KEYS],
            str(list(obj)),
        )
        code, text = run([p("tiny.tif"), "--json"])
        ok("json: absent values are null", code == 0 and json.loads(text)["grain"] == [None], text)
        code, text = run(["--help"])
        ok("help: explains every metric and the baseline limit", code == 0 and all(f"{k}:" in text for k in EXPLAIN) and "baseline of the same scene" in text, text[-200:])

        # --- refusals
        write(p("lzw.tif"), w, h, clean, compression=5)
        write(p("tiled.tif"), w, h, clean, tile=(16, 16))
        write(p("small.tif"), w // 2, h // 2, [0] * (w * h // 4), spp=1)
        write(p("zm3.tif"), w, h, clean)
        write(p("zm8.tif"), w, h, [0] * (w * h), spp=1, bits=8)
        write(p("vs-gray.tif"), w, h, [0] * (w * h), spp=1)
        write(p("vs-8.tif"), w, h, [0] * (w * h * 3), bits=8)
        refusals = [
            ("LZW composite", [p("lzw.tif")], p("lzw.tif"), "Compression=5"),
            ("tiled composite", [p("tiled.tif")], p("tiled.tif"), "tiled"),
            ("missing file", [p("nope.tif")], p("nope.tif"), "No such file"),
            ("z-map of another size", [p("clean.tif"), "--zmap", p("small.tif")], p("small.tif"), f"{w // 2}x{h // 2}"),
            ("3-channel z-map", [p("clean.tif"), "--zmap", p("zm3.tif")], p("zm3.tif"), "3 channel"),
            ("8-bit z-map", [p("clean.tif"), "--zmap", p("zm8.tif")], p("zm8.tif"), "8-bit"),
            ("--vs of another size", [p("clean.tif"), "--vs", p("small.tif")], p("small.tif"), f"{w // 2}x{h // 2}"),
            ("--vs of another channel count", [p("clean.tif"), "--vs", p("vs-gray.tif")], p("vs-gray.tif"), "1 channel"),
            ("--vs of another bit depth", [p("clean.tif"), "--vs", p("vs-8.tif")], p("vs-8.tif"), "8-bit"),
        ]
        for label, argv, named, found in refusals:
            code, text = run(argv)
            ok(f"refused, exit 1, names file and finding: {label}", code == 1 and named in text and found in text, f"exit {code}: {text}")
        code, text = run([p("lzw.tif")])
        ok("LZW refusal names the uncompressed export", "--tiff-compression none" in text, text)
        code, _ = run([p("clean.tif"), "--bogus"])
        ok("unknown flag exits 2", code == 2)
        code, _ = run([])
        ok("no arguments exits 2", code == 2)

    if failed:
        print(f"image-metrics --selftest: {failed} of {total} cases failed")
        return 1
    print(f"image-metrics --selftest: all {total} cases pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
