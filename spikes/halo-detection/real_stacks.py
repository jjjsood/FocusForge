#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Real material for the halo maps: a search for focus stacks in photo folders.

Subcommands (run from the repository root):

    candidates   read frame metadata under one or more read-only roots, group each directory
                 by the product's 60 s capture gap, cut the groups into runs, drop runs whose
                 sampled embedded previews fail an alignment or focus-travel test, and write
                 one four-frame preview for each of the top-ranked runs plus an index
    controls     print the image-test measurements of named reference stacks and non-stacks
    run          stack one source (Mantis, a reference folder or a candidate) with each method
                 through the product's own grouping file, and export composite and Z-Map
    crops        cut the context crops of the fixed rects from every method's composite and Z-Map,
                 or write a gridded quarter-scale preview of one stack to choose rects on
    --self-check   grouping, run cutting, image test, preview layout and boundary cases on
                   constructed answers

The roots are only read, and an output path under any root is refused before anything is
written. Requires Python 3.14, numpy and Pillow; `candidates` also needs `exiftool`.
"""

import argparse
import hashlib
import io
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import tomllib
from collections import Counter
from concurrent.futures import ThreadPoolExecutor

import numpy as np
from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "tools", "lib"))
import rawtiff  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import halo_net as hn  # noqa: E402
import layered_scene as ls  # noqa: E402

METHODS = ls.gff.ALGOS

GROUP_GAP = 60  # s: frames further apart than this start a new group, as in the product
MIN_FRAMES = 4
FRAME_EXTS = [  # the product's raw extensions plus TIFF
    "ari", "arw", "cr2", "cr3", "crm", "crw", "dcr", "dcs", "dng", "erf", "iiq", "kdc", "mef",
    "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "raf", "raw", "rw2", "rwl", "srw", "3fr",
    "fff", "x3f", "qtk", "tif", "tiff",
]
TAGS = ["DateTimeOriginal", "FocalLength", "FNumber", "ExposureTime", "ISO", "FocusDistanceUpper",
        "FocusDistanceLower", "FocusDistance", "FocusBracketing", "Orientation", "FileSize", "Error"]
DIST_KEYS = ("FocusDistanceUpper", "FocusDistanceLower", "FocusDistance")
# a candidate's frames must agree on each of these
CONSTANT = (("FocalLength", "focal length"), ("FNumber", "f-number"), ("ExposureTime", "exposure time"), ("ISO", "ISO"))
MARK_BRACKET, MARK_CHANGES, MARK_CONSTANT = "focus bracketing", "focus distance changes", "focus constant, possibly rail or burst"
PREVIEW_TAGS = ("ThumbnailImage", "PreviewImage", "JpgFromRaw")  # usually smallest first
PREVIEW_MIN_SIDE = 1000
CELL, LABEL, HEADER = (800, 600), 28, 44
SKIP_FILL = (70, 70, 70)

_ORIENT_TEXT = {"horizontal (normal)": 1, "mirror horizontal": 2, "rotate 180": 3, "mirror vertical": 4,
                "mirror horizontal and rotate 270 cw": 5, "rotate 90 cw": 6,
                "mirror horizontal and rotate 90 cw": 7, "rotate 270 cw": 8}
_T = Image.Transpose
_ORIENT_OP = {2: _T.FLIP_LEFT_RIGHT, 3: _T.ROTATE_180, 4: _T.FLIP_TOP_BOTTOM, 5: _T.TRANSPOSE,
              6: _T.ROTATE_270, 7: _T.TRANSVERSE, 8: _T.ROTATE_90}


class RefusedOutput(Exception):
    pass


def parse_time(text):
    """Seconds of a `YYYY:MM:DD HH:MM:SS` capture time read as naive local time, else None."""
    if not isinstance(text, str) or not re.fullmatch(r"\d{4}:\d\d:\d\d \d\d:\d\d:\d\d", text):
        return None
    y, mo, d, h, mi, s = (int(v) for v in re.split(r"[: ]", text))
    if not (1 <= mo <= 12 and 1 <= d <= 31 and h <= 23 and mi <= 59 and s <= 60):
        return None
    y -= mo <= 2
    era, yoe = divmod(y, 400)
    doy = (153 * ((mo + 9) % 12) + 2) // 5 + d - 1
    days = era * 146097 + yoe * 365 + yoe // 4 - yoe // 100 + doy - 719468
    return days * 86400 + h * 3600 + mi * 60 + s


def _name(rec):
    return os.path.basename(rec["SourceFile"])


def group_directory(records):
    """One directory's records as time-sorted chunks split where the gap exceeds `GROUP_GAP`,
    and the records without a usable time."""
    timed, untimed = [], []
    for r in records:
        t = parse_time(r.get("DateTimeOriginal"))
        (timed if t is not None else untimed).append((t, r))
    timed.sort(key=lambda tr: (tr[0], _name(tr[1])))
    chunks, last = [], None
    for t, r in timed:
        if last is None or t - last > GROUP_GAP:
            chunks.append([])
        chunks[-1].append(r)
        last = t
    return chunks, [r for _, r in untimed]


def _norm(v):
    return round(v, 6) if isinstance(v, float) else v


def _truthy(v):
    return v not in (None, 0, "0", "", "Off", "No", False)


def judge(chunk):
    """Whether one chunk qualifies: `candidate`, the `reasons` either way, the focus `mark` and rejection `codes`."""
    n = len(chunk)
    reasons, codes = [], []
    if n < MIN_FRAMES:
        reasons.append(f"only {n} frames, at least {MIN_FRAMES} needed")
        codes.append("fewer than 4 frames")
    for key, name in CONSTANT:
        if len({_norm(r.get(key)) for r in chunk}) > 1:
            vals = sorted({str(_norm(r.get(key))) for r in chunk})
            reasons.append(f"{name} varies ({', '.join(vals[:4])})")
            codes.append(f"{name} varies")
    if codes:
        return {"candidate": False, "reasons": reasons, "mark": None, "codes": codes}
    times = sorted(parse_time(r["DateTimeOriginal"]) for r in chunk)
    first = chunk[0]
    reasons = [f"{n} frames in {times[-1] - times[0]} s, largest gap {max(b - a for a, b in zip(times, times[1:]))} s",
               f"constant focal length {first.get('FocalLength')} mm, f/{first.get('FNumber')}, {first.get('ExposureTime')} s, ISO {first.get('ISO')}"]
    tiffs = sum(r["SourceFile"].lower().endswith((".tif", ".tiff")) for r in chunk)
    if 0 < tiffs < n:
        reasons.append(f"{tiffs} TIFF frame(s) mixed with raw")
    have = [tuple(_norm(r.get(k)) for k in DIST_KEYS) for r in chunk]
    have = [d for d in have if any(x is not None for x in d)]
    if any(_truthy(r.get("FocusBracketing")) for r in chunk):
        mark = MARK_BRACKET
        reasons.append("the camera reports focus bracketing")
    elif len(set(have)) > 1:
        mark = MARK_CHANGES
        near = [d[0] if d[0] is not None else next(x for x in d if x is not None) for d in have]
        mono = near in (sorted(near), sorted(near, reverse=True))
        reasons.append(f"focus distance changes ({min(near)} to {max(near)} m{', monotonic' if mono else ''})")
    else:
        mark = MARK_CONSTANT
        reasons.append("focus distance constant" if have else "no focus distance in the metadata")
    return {"candidate": True, "reasons": reasons, "mark": mark, "codes": []}


def split_runs(chunk):
    """The chunk cut wherever a frame differs from the one before in a constant setting."""
    runs = [[chunk[0]]]
    for prev, r in zip(chunk, chunk[1:]):
        if any(_norm(prev.get(k)) != _norm(r.get(k)) for k, _ in CONSTANT):
            runs.append([])
        runs[-1].append(r)
    return runs


def search(records):
    """Candidates, rejected groups, untimed frames, unreadable files and the short runs cut off
    candidates, across all directories. A time group whose settings change is cut into runs of
    constant settings; it is rejected as a whole when no run holds `MIN_FRAMES` frames."""
    by_dir, skipped = {}, []
    for r in records:
        if r.get("Error"):
            skipped.append({"path": r["SourceFile"], "reason": r["Error"]})
        else:
            by_dir.setdefault(os.path.dirname(r["SourceFile"]), []).append(r)
    out = {"candidates": [], "rejected": [], "untimed": [], "skipped": skipped, "fragments": {"runs": 0, "frames": 0}}

    def entry(d, rows, j):
        return {"dir": d, "count": len(rows), "frames": [r["SourceFile"] for r in rows],
                "orientations": [r.get("Orientation", 1) for r in rows],
                "first_time": rows[0]["DateTimeOriginal"], "last_time": rows[-1]["DateTimeOriginal"], "reasons": j["reasons"]}

    for d in sorted(by_dir):
        chunks, untimed = group_directory(by_dir[d])
        if untimed:
            out["untimed"].append({"dir": d, "count": len(untimed)})
        for chunk in chunks:
            runs = split_runs(chunk)
            verdicts = [judge(run) for run in runs]
            if not any(v["candidate"] for v in verdicts):
                j = judge(chunk)
                out["rejected"].append({**entry(d, chunk, j), "codes": j["codes"]})
                continue
            for run, j in zip(runs, verdicts):
                if not j["candidate"]:
                    out["fragments"]["runs"] += 1
                    out["fragments"]["frames"] += len(run)
                    continue
                if len(runs) > 1:
                    j["reasons"].append(f"a run of {len(run)} of the {len(chunk)} frames in its 60 s group, cut where a setting changes")
                out["candidates"].append({**entry(d, run, j), "mark": j["mark"]})
    return out


def pick_indices(n):
    """First, last and two evenly spaced frames between them."""
    return [round(i * (n - 1) / 3) for i in range(4)]


def upright(img, orientation):
    """The image turned so that the metadata orientation reads upright."""
    code = orientation if isinstance(orientation, int) else _ORIENT_TEXT.get(str(orientation).lower(), 1)
    op = _ORIENT_OP.get(code)
    return img if op is None else img.transpose(op)


def exiftool_extract(path, tag):
    return subprocess.run(["exiftool", "-b", f"-{tag}", path], capture_output=True).stdout


def tiff_preview(path, long_side=1600):
    """A subsampled RGB view of an uncompressed 8 or 16-bit TIFF, read one row at a time."""
    info = rawtiff.read_info(path)
    if info.compression != 1 or info.planar_config != 1 or 3 in info.sample_format or info.bits_per_sample[0] not in (8, 16):
        raise ValueError("TIFF is compressed, planar or not integer")
    step = max(1, -(-max(info.width, info.height) // long_side))
    dtype = "<u2" if info.bits_per_sample[0] == 16 else "u1"
    rows = [np.frombuffer(rawtiff.read_rect(path, 0, y, info.width, 1, info), dtype).reshape(info.width, info.samples_per_pixel)[::step]
            for y in range(0, info.height, step)]
    arr = np.stack(rows)
    if dtype == "<u2":
        arr = arr >> 8
    arr = arr.astype(np.uint8)
    return Image.fromarray(arr[:, :, 0], "L").convert("RGB") if arr.shape[2] == 1 else Image.fromarray(arr[:, :, :3], "RGB")


def frame_preview(path, orientation, extract=exiftool_extract):
    """(upright PIL image, None) or (None, reason): the first embedded preview of at least
    `PREVIEW_MIN_SIDE` px on its long side, else the largest found; TIFF frames are subsampled."""
    try:
        if path.lower().endswith((".tif", ".tiff")):
            return upright(tiff_preview(path), orientation), None
        best = None
        for tag in PREVIEW_TAGS:
            data = extract(path, tag)
            if not data:
                continue
            img = Image.open(io.BytesIO(data))
            if max(img.size) >= PREVIEW_MIN_SIDE:
                best = img
                break
            if best is None or max(img.size) > max(best.size):
                best = img
        if best is None:
            return None, "no embedded preview"
        best.draft("RGB", (PREVIEW_MIN_SIDE, PREVIEW_MIN_SIDE))
        return upright(best.convert("RGB"), orientation), None
    except (ValueError, OSError, rawtiff.UnsupportedTiff) as e:
        return None, f"preview unreadable: {e}"


def layout(cells, names, folder, count):
    """2x2 sheet of (image | None, reason) cells under a header of folder, frame range and count."""
    from PIL import ImageFont

    font = ImageFont.load_default(size=18)
    w, h = CELL
    sheet = Image.new("RGB", (2 * w, HEADER + 2 * (h + LABEL)), (20, 20, 20))
    draw = ImageDraw.Draw(sheet)
    draw.text((8, 10), f"{folder}  |  {names[0]} .. {names[-1]}  |  {count} frames", fill=(255, 255, 255), font=font)
    for i, ((img, reason), name) in enumerate(zip(cells, names)):
        x, y = (i % 2) * w, HEADER + (i // 2) * (h + LABEL)
        draw.text((x + 8, y + 4), name if img is not None else f"{name}  (skipped: {reason})", fill=(220, 220, 220), font=font)
        if img is None:
            draw.rectangle((x, y + LABEL, x + w - 1, y + LABEL + h - 1), fill=SKIP_FILL)
        else:
            img = img.copy()
            img.thumbnail(CELL)
            sheet.paste(img, (x + (w - img.width) // 2, y + LABEL + (h - img.height) // 2))
    return sheet


def _stem(path):
    return os.path.splitext(os.path.basename(path))[0].lower()


def _dated(stem):
    return re.match(r"\d{8}-", stem) is not None


def _bare(stem):
    return re.sub(r"^\d{8}-", "", stem)


def _same_frame(ref, stem):
    """Equal stems; a name with no date prefix equals a dated one by its camera number alone."""
    return ref == stem or (not (_dated(ref) and _dated(stem)) and _bare(ref) == _bare(stem))


def reference_sets(root):
    """Directory name -> lower-case frame stems, for each TIFF directory directly under `root`."""
    sets = {}
    for d in sorted(os.listdir(root)):
        p = os.path.join(root, d)
        if os.path.isdir(p):
            stems = {_stem(f) for f in os.listdir(p) if f.lower().endswith((".tif", ".tiff"))}
            if stems:
                sets[d] = stems
    return sets


def match_reference(frames, refs):
    """The reference directory sharing at least half of its frame stems with `frames`, or None."""
    mine = {_stem(f) for f in frames}
    best, best_share = None, 0.0
    for name, stems in refs.items():
        share = sum(any(_same_frame(r.lower(), m) for m in mine) for r in stems) / len(stems)
        if share >= 0.5 and share > best_share:
            best, best_share = name, share
    return best


def refuse_output_under(out, roots):
    o = os.path.realpath(out)
    for r in roots:
        rr = os.path.realpath(r)
        if o == rr or o.startswith(rr + os.sep):
            raise RefusedOutput(f"output {out} lies under the read-only root {r}")


def existing_roots(roots):
    found = []
    for r in roots:
        if os.path.isdir(r):
            found.append(r)
        else:
            print(f"root not found: {r}", file=sys.stderr)
    return found


def read_records(root):
    cmd = ["exiftool", "-json", "-n", "-r", "-q", "-charset", "filename=utf8"]
    for e in FRAME_EXTS:
        cmd += ["-ext", e]
    cmd += [f"-{t}" for t in TAGS] + [root]
    out = subprocess.run(cmd, capture_output=True).stdout
    return json.loads(out) if out.strip() else []


FOCUS_STEP = 0.02  # m: smallest focus-distance move that counts as travel
GAP_FLOOR = 5  # s: a gap this short never separates two stacks
RUN_GAP_FACTOR = 4  # a gap over this many median gaps separates two stacks
MIN_RUN_FRAMES = 6
SAMPLE_FRAMES = 10
LUMA_SIDE = 640
ALIGN_SIDE = 128
TILE_GRID = (8, 6)  # columns, rows
# Fixed from the reference stacks and the negative controls, as measured by `controls`.
ALIGN_MIN_CORR = 0.75  # lowest correlation between consecutive sampled frames
ALIGN_MAX_SHIFT = 0.04  # largest shift between them, as a fraction of the long side
TRAVEL_MIN_DISTINCT = 3  # frames that are sharpest somewhere
TRAVEL_MAX_BEST_SHARE = 0.6  # share of textured tiles won by the best frame
TRAVEL_MIN_COHERENCE = 0.3  # neighbouring tiles whose sharpest frames lie close together
TRAVEL_MIN_PEAK = 1.1  # median of (sharpest frame / median frame) tile sharpness


def sample_indices(n, k=SAMPLE_FRAMES):
    """Up to `k` evenly spaced frame positions out of `n`, first and last included."""
    m = min(n, k)
    return sorted({round(i * (n - 1) / (m - 1)) for i in range(m)}) if m > 1 else [0]


def _distance(rec):
    return next((rec[k] for k in DIST_KEYS if rec.get(k) is not None), None)


def split_stack_runs(rows):
    """Time-sorted rows cut where the capture gap exceeds `RUN_GAP_FACTOR` median gaps (at least
    `GAP_FLOOR` s) and where the focus distance turns back against a travel of at least `FOCUS_STEP`."""
    t = [parse_time(r["DateTimeOriginal"]) for r in rows]
    gaps = [b - a for a, b in zip(t, t[1:])]
    limit = max(GAP_FLOOR, RUN_GAP_FACTOR * (statistics.median(gaps) if gaps else 0))
    cuts = {i for i, g in enumerate(gaps, 1) if g > limit}
    start = far = None
    for i, r in enumerate(rows):
        d = _distance(r)
        if d is None:
            continue
        if start is None or i in cuts:
            start = far = d
            continue
        if abs(d - start) > abs(far - start):
            far = d
        travel = abs(far - start)
        if travel >= FOCUS_STEP and abs(d - far) >= max(FOCUS_STEP, travel / 2) and (d - far) * (far - start) < 0:
            cuts.add(i)
            start = far = d
    bounds = [0, *sorted(cuts), len(rows)]
    return [rows[a:b] for a, b in zip(bounds, bounds[1:])]


def drop_minority_type(rows):
    """The rows of the larger of the raw and TIFF frames (exports sharing a capture time are not shots)."""
    tiff = [r for r in rows if r["SourceFile"].lower().endswith((".tif", ".tiff"))]
    raw = [r for r in rows if r not in tiff]
    return rows if not tiff or not raw else (raw if len(raw) >= len(tiff) else tiff)


def prefilter(rows):
    """The reason a run cannot be a stack from metadata alone, else None."""
    if len(rows) < MIN_RUN_FRAMES:
        return f"fewer than {MIN_RUN_FRAMES} frames after cutting at gaps and focus reversals ({len(rows)})"
    if len({r.get("Orientation", 1) for r in rows}) > 1:
        return "frames differ in camera orientation"
    return None


def _small(a, w, h):
    return np.asarray(Image.fromarray(a).resize((w, h), Image.Resampling.BOX), np.float64)


def _shift(a, b):
    """Offset in px between two same-size arrays, by phase correlation."""
    win = np.outer(np.hanning(a.shape[0]), np.hanning(a.shape[1]))
    fa, fb = np.fft.rfft2((a - a.mean()) * win), np.fft.rfft2((b - b.mean()) * win)
    r = fa * np.conj(fb)
    c = np.fft.irfft2(r / (np.abs(r) + 1e-9), a.shape)
    y, x = np.unravel_index(np.argmax(c), c.shape)
    y, x = (y - a.shape[0] if y > a.shape[0] // 2 else y), (x - a.shape[1] if x > a.shape[1] // 2 else x)
    return float(np.hypot(x, y))


def align_metrics(lumas):
    """Lowest correlation and largest shift (fraction of the long side) between consecutive frames."""
    h0, w0 = lumas[0].shape
    s = ALIGN_SIDE / max(h0, w0)
    w, h = max(8, round(w0 * s)), max(8, round(h0 * s))
    small = [_small(a, w, h) for a in lumas]
    corr = []
    for a, b in zip(small, small[1:]):
        a, b = a - a.mean(), b - b.mean()
        corr.append(float((a * b).sum() / (np.sqrt((a * a).sum() * (b * b).sum()) + 1e-9)))
    s2 = 256 / max(h0, w0)
    big = [_small(a, max(8, round(w0 * s2)), max(8, round(h0 * s2))) for a in lumas]
    shifts = [_shift(a, b) / 256 for a, b in zip(big, big[1:])]
    return {"corr_min": round(min(corr), 3), "shift_max": round(max(shifts), 3)}


def focus_travel(lumas):
    """Where the sharpest frame lies per tile: distinct winners, the best frame's share of the
    textured tiles, agreement of neighbouring tiles and how far the winner stands above the median frame."""
    cols, rows_n = TILE_GRID
    n = len(lumas)
    h, w = lumas[0].shape
    th, tw = h // rows_n, w // cols
    v = np.zeros((n, rows_n, cols))
    for k, a in enumerate(lumas):
        f = a[: th * rows_n, : tw * cols].astype(np.float64)
        lap = 4 * f[1:-1, 1:-1] - f[:-2, 1:-1] - f[2:, 1:-1] - f[1:-1, :-2] - f[1:-1, 2:]
        lap = np.pad(lap, 1)
        v[k] = lap.reshape(rows_n, th, cols, tw).var(axis=(1, 3))
    top = v.max(axis=0)
    textured = top >= np.median(top)
    win = v.argmax(axis=0)
    wt = win[textured]
    counts = np.bincount(wt, minlength=n)
    pairs = [(win[y, x], win[y, x + 1]) for y in range(rows_n) for x in range(cols - 1) if textured[y, x] and textured[y, x + 1]]
    pairs += [(win[y, x], win[y + 1, x]) for y in range(rows_n - 1) for x in range(cols) if textured[y, x] and textured[y + 1, x]]
    near = max(1, n // 5)
    coherence = sum(abs(int(a) - int(b)) <= near for a, b in pairs) / len(pairs) if pairs else 0.0
    peak = float(np.median(top[textured] / (np.median(v, axis=0)[textured] + 1e-9)))
    return {"distinct": int((counts > 0).sum()), "best_share": round(float(counts.max() / counts.sum()), 3),
            "coherence": round(float(coherence), 3), "peak": round(peak, 2)}


def image_test(lumas):
    """Measurements of a run's sampled frames and the test that drops it, if any."""
    m = {**align_metrics(lumas), **focus_travel(lumas)}
    drop = None
    if m["corr_min"] < ALIGN_MIN_CORR or m["shift_max"] > ALIGN_MAX_SHIFT:
        drop = "alignment"
    elif (m["distinct"] < TRAVEL_MIN_DISTINCT or m["best_share"] > TRAVEL_MAX_BEST_SHARE
          or m["coherence"] < TRAVEL_MIN_COHERENCE or m["peak"] < TRAVEL_MIN_PEAK):
        drop = "focus travel"
    m["score"] = round(m["coherence"] * (1 - m["best_share"]) * max(0.0, m["corr_min"]), 3)
    return m, drop


def _breaks(a, b):
    m = align_metrics([a, b])
    return m["corr_min"] < ALIGN_MIN_CORR or m["shift_max"] > ALIGN_MAX_SHIFT


def alignment_cuts(n, load):
    """Positions where the picture changes: between each pair of sampled frames that fail the alignment
    test, the first frame (found by bisection) that no longer aligns with the earlier sampled one."""
    pos = sample_indices(n)
    cuts = []
    for lo, hi in zip(pos, pos[1:]):
        if _breaks(load(lo), load(hi)):
            a = load(lo)
            while hi - lo > 1:
                mid = (lo + hi) // 2
                lo, hi = (mid, hi) if not _breaks(a, load(mid)) else (lo, mid)
            cuts.append(hi)
    return cuts


def load_luma(path, orientation, cache, extract=exiftool_extract):
    """(luma array of at most `LUMA_SIDE` px, None) or (None, reason); cached under `cache`."""
    f = os.path.join(cache, hashlib.sha1(path.encode()).hexdigest() + ".npy")
    if os.path.exists(f):
        return np.load(f), None
    img, why = frame_preview(path, orientation, extract)
    if img is None:
        return None, why
    g = img.convert("L")
    g.thumbnail((LUMA_SIDE, LUMA_SIDE))
    arr = np.asarray(g)
    np.save(f, arr)
    return arr, None


def run_image_test(rows, cache, extract=exiftool_extract, workers=6):
    """`image_test` over `SAMPLE_FRAMES` frames of the run: (metrics, dropping test, reason)."""
    pick = [rows[i] for i in sample_indices(len(rows))]
    with ThreadPoolExecutor(workers) as ex:
        got = list(ex.map(lambda r: load_luma(r["SourceFile"], r.get("Orientation", 1), cache, extract), pick))
    bad = [(r, why) for r, (a, why) in zip(pick, got) if a is None]
    if bad:
        return {}, "preview", f"{os.path.basename(bad[0][0]['SourceFile'])}: {bad[0][1]}"
    m, drop = image_test([a for a, _ in got])
    return m, drop, None


def _slug(text):
    return re.sub(r"[^A-Za-z0-9]+", "-", text).strip("-")[:40] or "stack"


SHORTLIST_MAX = 15


def _run_entry(d, rows):
    return {"dir": d, "count": len(rows), "frames": [r["SourceFile"] for r in rows],
            "orientations": [r.get("Orientation", 1) for r in rows],
            "first_time": rows[0]["DateTimeOriginal"], "last_time": rows[-1]["DateTimeOriginal"]}


def _cache_loader(rows, cache):
    return lambda i: load_luma(rows[i]["SourceFile"], rows[i].get("Orientation", 1), cache)[0]


def test_run(rows, cache):
    """[(piece, metrics, dropping test, reason)] for a run; a run failing the alignment test is cut where
    the picture changes and each piece of at least `MIN_RUN_FRAMES` frames is tested again."""
    m, drop, reason = run_image_test(rows, cache)
    if drop != "alignment" or len(rows) < 2 * MIN_RUN_FRAMES:
        return [(rows, m, drop, reason)]
    load = _cache_loader(rows, cache)
    cuts = alignment_cuts(len(rows), load)
    if not cuts:
        return [(rows, m, drop, reason)]
    bounds = [0, *cuts, len(rows)]
    out = []
    for a, b in zip(bounds, bounds[1:]):
        piece = rows[a:b]
        why = prefilter(piece)
        if why:
            out.append((piece, {}, "prefilter", why))
        else:
            out += test_run(piece, cache)
    return out


def screen(cands, records, cache, say=lambda *a: None):
    """Cut each metadata candidate into stack runs, drop runs by metadata and by the image test,
    and rank the rest. Returns (passed, dropped); every dropped run names its test."""
    by = {r["SourceFile"]: r for r in records if not r.get("Error")}
    passed, dropped = [], []
    for n, c in enumerate(cands, 1):
        for run in split_stack_runs(drop_minority_type([by[f] for f in c["frames"]])):
            why = prefilter(run)
            results = [(run, {}, "prefilter", why)] if why else test_run(run, cache)
            for piece, m, drop, reason in results:
                e = _run_entry(c["dir"], piece)
                say(f"[{n}/{len(cands)}] {os.path.basename(c['dir'])} {len(piece)} frames: {drop or 'pass'} {m}")
                if drop:
                    dropped.append({**e, "test": drop, "reason": reason or "", "metrics": m})
                else:
                    j = judge(piece)
                    passed.append({**e, "mark": j["mark"], "reasons": j["reasons"], "metrics": m})
    passed.sort(key=lambda p: -p["metrics"]["score"])
    return passed, dropped


def write_previews(cands, out):
    for c in cands:
        pick = pick_indices(c["count"])
        names = [os.path.basename(c["frames"][k]) for k in pick]
        cells = [frame_preview(c["frames"][k], c["orientations"][k]) for k in pick]
        c["preview"] = os.path.join(out, f"{c['id']:03d}-{_slug(os.path.basename(c['dir']))}.jpg")
        layout(cells, names, c["dir"], c["count"]).save(c["preview"], quality=85)
        c["preview_skips"] = [{"path": c["frames"][k], "reason": r} for k, (img, r) in zip(pick, cells) if img is None]


def _fmt(m):
    if not m:
        return "-"
    return (f"corr {m['corr_min']}, shift {m['shift_max']}, distinct {m['distinct']}, best share {m['best_share']}, "
            f"coherence {m['coherence']}, peak {m['peak']}, score {m['score']}")


def _span(c):
    return f"{os.path.basename(c['frames'][0])} .. {os.path.basename(c['frames'][-1])}"


def write_index(res, out, roots_searched, roots_missing, ref_note):
    rej = Counter(code for g in res["rejected"] for code in g["codes"])
    by_test = Counter(d["test"] for d in res["dropped"])
    counts = {"shortlist": len(res["shown"]), "passed_image_test": len(res["passed"]),
              "metadata_candidates": res["metadata_candidates"], "stack_runs_after_cutting": len(res["passed"]) + len(res["dropped"]),
              "dropped_by_test": dict(by_test), "rejected_groups": len(res["rejected"]), "rejected_by_reason": dict(rej),
              "untimed_frames": sum(u["count"] for u in res["untimed"]), "skipped_files": len(res["skipped"])}
    doc = {"roots_searched": roots_searched, "roots_missing": roots_missing, "reference_sets": ref_note,
           "counts": counts, "candidates": res["shown"], "passed_not_shown": res["passed_rest"], "dropped": res["dropped"],
           "rejected": res["rejected"], "untimed": res["untimed"], "skipped": res["skipped"]}
    with open(os.path.join(out, "index.json"), "w") as fh:
        json.dump(doc, fh, indent=1, ensure_ascii=False)
    lines = ["# Candidate stacks", "", f"Roots searched: {', '.join(roots_searched) or 'none'}. Missing: {', '.join(roots_missing) or 'none'}.",
             f"Reference sets: {ref_note}", "",
             f"{counts['metadata_candidates']} metadata candidates (runs of 4 or more frames with constant settings) were cut at capture gaps "
             f"and focus reversals into {counts['stack_runs_after_cutting']} runs. Dropped by test: "
             f"{', '.join(f'{k} {v}' for k, v in sorted(by_test.items())) or 'none'}. {counts['passed_image_test']} passed the image test; "
             f"{counts['shortlist']} are shown. Rejected groups {counts['rejected_groups']} "
             f"({', '.join(f'{k}: {v}' for k, v in sorted(rej.items())) or 'none'}), untimed frames {counts['untimed_frames']}, "
             f"skipped files {counts['skipped_files']}.", "", "## Shortlist", "",
             "| # | preview | folder | frames | count | mark | reference set | image test |", "|---|---|---|---|---|---|---|---|"]
    for c in res["shown"]:
        lines.append(f"| {c['id']} | {os.path.basename(c['preview'])} | {c['dir']} | {_span(c)} | {c['count']} | {c['mark']} | "
                     f"{c.get('reference') or '-'} | {_fmt(c['metrics'])} |")
    lines += ["", "## Passed but not shown", ""] + [f"- {c['dir']} {_span(c)} ({c['count']}): {_fmt(c['metrics'])}" for c in res["passed_rest"]]
    lines += ["", "## Dropped runs", "", "| folder | frames | count | dropped by | why | reference set |", "|---|---|---|---|---|---|"]
    for d in res["dropped"]:
        why = d["reason"] or _fmt(d.get("metrics"))
        lines.append(f"| {d['dir']} | {_span(d)} | {d['count']} | {d['test']} | {why} | {d.get('reference') or '-'} |")
    lines += ["", "## Rejected groups (metadata)", "", "| folder | frames | count | reasons | reference set |", "|---|---|---|---|---|"]
    for g in res["rejected"]:
        lines.append(f"| {g['dir']} | {_span(g)} | {g['count']} | {'; '.join(g['reasons'])} | {g.get('reference') or '-'} |")
    lines += ["", "## Untimed frames", ""] + [f"- {u['dir']}: {u['count']}" for u in res["untimed"]]
    skips = res["skipped"] + [s for c in res["shown"] for s in c["preview_skips"]]
    lines += ["", "## Skipped", ""] + [f"- {s['path']}: {s['reason']}" for s in skips]
    with open(os.path.join(out, "index.md"), "w") as fh:
        fh.write("\n".join(lines) + "\n")
    return counts


def cmd_candidates(args):
    try:
        refuse_output_under(args.out, args.root)
    except RefusedOutput as e:
        print(f"refused: {e}", file=sys.stderr)
        return 2
    roots = existing_roots(args.root)
    if not roots:
        print("none of the given roots exists", file=sys.stderr)
        return 1
    if shutil.which("exiftool") is None:
        print("exiftool is required", file=sys.stderr)
        return 1
    os.makedirs(args.out, exist_ok=True)
    saved = os.path.join(args.out, "records.json")
    if args.reuse_records:
        with open(saved) as fh:
            records = json.load(fh)
    else:
        records = [r for root in roots for r in read_records(root)]
        with open(saved, "w") as fh:
            json.dump(records, fh, ensure_ascii=False)
    res = search(records)
    cache = os.path.join(args.out, "luma-cache")
    os.makedirs(cache, exist_ok=True)
    res["metadata_candidates"] = len(res["candidates"])
    res["passed"], res["dropped"] = screen(res["candidates"], records, cache, lambda *a: print(*a, flush=True))
    ref_root = os.environ.get("FOCUSFORGE_REFERENCE_STACK")
    refs = reference_sets(ref_root) if ref_root and os.path.isdir(ref_root) else {}
    for g in res["passed"] + res["dropped"] + res["rejected"]:
        g["reference"] = match_reference(g["frames"], refs)
    top = res["passed"][:SHORTLIST_MAX]
    top += [c for c in res["passed"][SHORTLIST_MAX:] if c["reference"]]
    res["shown"] = top
    res["passed_rest"] = [c for c in res["passed"] if c not in top]
    for i, c in enumerate(res["shown"], 1):
        c["id"] = i
    for stale in os.listdir(args.out):
        if re.fullmatch(r"\d{3}-.*\.jpg", stale):
            os.remove(os.path.join(args.out, stale))
    write_previews(res["shown"], args.out)
    note = f"{len(refs)} directories read from the reference stack root" if refs else "FOCUSFORGE_REFERENCE_STACK unset or missing, no marks"
    counts = write_index(res, args.out, roots, [r for r in args.root if r not in roots], note)
    print(json.dumps(counts))
    print(f"files read {len(records)}; previews {len(res['shown'])} in {args.out}")
    return 0


def _rows_between(records, d, first, last):
    rows = sorted((r for r in records if not r.get("Error") and os.path.dirname(r["SourceFile"]) == d),
                  key=lambda r: (parse_time(r["DateTimeOriginal"]) or 0, _name(r)))
    stems = [_stem(r["SourceFile"]) for r in rows]
    return rows[stems.index(first.lower()): stems.index(last.lower()) + 1]


def cmd_controls(args):
    """Image-test measurements of the reference stacks (positive controls) and named runs (negative controls)."""
    with open(args.records) as fh:
        records = json.load(fh)
    os.makedirs(args.cache, exist_ok=True)
    ref_root = os.environ.get("FOCUSFORGE_REFERENCE_STACK")
    if not ref_root or not os.path.isdir(ref_root):
        print("FOCUSFORGE_REFERENCE_STACK is not set to a directory", file=sys.stderr)
        return 1
    cases = []
    for name, stems in sorted(reference_sets(ref_root).items()):
        rows = sorted((r for r in records if not r.get("Error") and _stem(r["SourceFile"]) in stems),
                      key=lambda r: (parse_time(r["DateTimeOriginal"]) or 0, _name(r)))
        if len(rows) >= MIN_RUN_FRAMES and name in args.reference:
            cases.append((f"positive {name}", rows))
    for spec in args.negative or []:
        d, first, last = spec.rsplit(":", 2)
        cases.append((f"negative {os.path.basename(d)} {first}..{last}", _rows_between(records, d, first, last)))
    print(f"thresholds: corr_min >= {ALIGN_MIN_CORR}, shift_max <= {ALIGN_MAX_SHIFT}, distinct >= {TRAVEL_MIN_DISTINCT}, "
          f"best_share <= {TRAVEL_MAX_BEST_SHARE}, coherence >= {TRAVEL_MIN_COHERENCE}, peak >= {TRAVEL_MIN_PEAK}")
    for label, rows in cases:
        m, drop, reason = run_image_test(rows, args.cache)
        print(f"{label:60} n={len(rows):3} {_fmt(m)} -> {('dropped by ' + drop + (': ' + reason if reason else '')) if drop else 'pass'}", flush=True)
    return 0


STACK_GROUP = "stack"
REFERENCE_VAR = "FOCUSFORGE_REFERENCE_STACK"
ALIGN_ALGO = "pyramidal-ecc"
DEFAULT_INDEX = os.path.join(ROOT, ".scratch", "halo-detection", "candidates", "index.json")
MANTIS_FRAMES = 61
MANTIS_REFERENCE = os.path.join("mantis", "reference", "zerene-pmax-20261009", "Mantis_Stacked.tif")
CROP_SIDE, CONTEXT = 512, 128  # px: rect side, context margin per side
PEAK_MIN = 0.03  # lowest phase-correlation peak of a tile that enters the fit; flat tiles peak below 0.013
MIN_TILES = 4
PREVIEW_SCALE, PREVIEW_GRID = 4, 512  # quarter-scale preview, grid line every 512 full-resolution px

# Rects of 512 x 512 px. Mantis `xy` is in the coordinates of the reference result and is moved into the
# composite by phase correlation; every other `xy` is in composite coordinates. `edge` says what the rect shows.
CROPS = [
    {"stack": "mantis", "name": "antenna", "xy": (2112, 384), "edge": "sharp left antenna across defocused grey background"},
    {"stack": "mantis", "name": "right-eye", "xy": (2880, 960), "edge": "right compound eye's outer edge against the background"},
    {"stack": "mantis", "name": "reserved", "xy": (3840, 2476), "edge": "featureless defocused background, no depth edge (control)"},
    {"stack": "damselfly", "name": "eye-edge", "xy": (448, 1152), "edge": "left compound eye's outer edge against the defocused green background"},
    {"stack": "damselfly", "name": "bark-ridge", "xy": (1792, 1536), "edge": "sharp bark ridge and droplets against the defocused background above"},
    {"stack": "beetle", "name": "shoulder", "xy": (320, 160), "edge": "left shoulder rim and body silhouette against the yellow background"},
    {"stack": "beetle", "name": "bristles", "xy": (2048, 3520), "edge": "sharp rear bristles against the defocused background"},
    {"stack": "spider", "name": "leg-edge", "xy": (1568, 512), "edge": "sharp front leg joint against the defocused rear leg and wall"},
    {"stack": "spider", "name": "face", "xy": (2304, 896), "edge": "eyes and fangs in focus against defocused body and legs"},
    {"stack": "pick-006", "name": "iris-rim", "xy": (2432, 2048), "edge": "sharp iris rim and skin against the defocused eye interior and bumps"},
    {"stack": "pick-006", "name": "bump-edge", "xy": (512, 1408), "edge": "sharp skin bumps and a speck silhouetted against the defocused background"},
    {"stack": "pick-008", "name": "iris-top", "xy": (3840, 1280), "edge": "sharp speckled iris rim against the eyelid above and the dark pupil"},
    {"stack": "pick-008", "name": "nostril", "xy": (1152, 2048), "edge": "nostril rim against the defocused background"},
    {"stack": "pick-012", "name": "antenna", "xy": (3328, 1152), "edge": "sharp antenna segments across the defocused yellow background"},
    {"stack": "pick-012", "name": "head-scales", "xy": (1792, 1280), "edge": "scales beside the eye against the defocused yellow background"},
    {"stack": "pick-015", "name": "antenna", "xy": (1024, 1024), "edge": "sharp antenna against the defocused blue and pink background"},
    {"stack": "pick-015", "name": "leg", "xy": (3456, 2688), "edge": "sharp front leg crossing the defocused background"},
    {"stack": "pick-002", "name": "joint-left", "xy": (1024, 2048), "edge": "sharp spined leg segment and joint against the defocused background"},
    {"stack": "pick-002", "name": "leg-right", "xy": (5632, 2304), "edge": "sharp textured leg segment against the defocused background"},
]


def build_groups(walked, stack_frames):
    """The walked partition with `stack_frames` moved into one group called `stack`; groups left empty are dropped."""
    found = {f for g in walked["group"] for f in g["frames"]}
    missing = [f for f in stack_frames if f not in found]
    if missing:
        raise ValueError(f"frames the walk did not find: {', '.join(missing)}")
    want, groups = set(stack_frames), []
    for g in walked["group"]:
        rest = [f for f in g["frames"] if f not in want]
        if rest:
            groups.append({"name": f"walked-{g['name']}" if g["name"] == STACK_GROUP else g["name"], "frames": rest})
    return {"group": [{"name": STACK_GROUP, "frames": list(stack_frames)}] + groups}


def groups_toml(groups):
    lines = []
    for g in groups["group"]:
        lines += ["[[group]]", f"name = {json.dumps(g['name'], ensure_ascii=False)}", "frames = ["]
        lines += [f"    {json.dumps(f, ensure_ascii=False)}," for f in g["frames"]] + ["]", ""]
    return "\n".join(lines)


def resolve_stack(args, ref_root):
    """(name, directory, frames relative to it) of the stack the arguments name; exits when it cannot be read."""
    if args.pick is not None:
        with open(args.index) as fh:
            cand = next((c for c in json.load(fh)["candidates"] if c["id"] == args.pick), None)
        if cand is None:
            sys.exit(f"pick {args.pick} is not in {args.index}")
        return f"pick-{args.pick:03d}", cand["dir"], [os.path.relpath(f, cand["dir"]) for f in cand["frames"]]
    if not ref_root:
        sys.exit(f"{REFERENCE_VAR} is not set to the reference stack folder")
    name = "mantis" if args.mantis else args.reference
    d = os.path.join(ref_root, name)
    if name in ("", ".", "..") or os.sep in name or not os.path.isdir(d):
        sys.exit(f"{d}: not a directory under {REFERENCE_VAR}")
    frames = sorted(f for f in os.listdir(d) if f.lower().endswith((".tif", ".tiff")) and os.path.isfile(os.path.join(d, f)))
    if args.mantis and len(frames) != MANTIS_FRAMES:
        sys.exit(f"{d}: {len(frames)} top-level tif frames found, expected {MANTIS_FRAMES}")
    return name, d, frames


def _save_json(path, doc):
    with open(path, "w") as fh:
        json.dump(doc, fh, indent=1)


def cmd_run(args):
    """Stack one source with every method and export composite and Z-Map; resumable per method."""
    name, d, frames = resolve_stack(args, os.environ.get(REFERENCE_VAR))
    try:
        refuse_output_under(args.out, [d])
    except RefusedOutput as e:
        print(f"refused: {e}", file=sys.stderr)
        return 2
    out = os.path.abspath(args.out)
    sdir = os.path.join(out, name)
    cli = ls.gff.CLI
    ls.check_cli(cli)
    os.chdir(ROOT)  # the stage cache and resume state live under ./.scratch
    times_path = os.path.join(sdir, "times.json")
    times = json.load(open(times_path)) if os.path.exists(times_path) else {}
    todo = [m for m in METHODS if not all(os.path.exists(os.path.join(sdir, m, f)) for f in ("composite.tif", "zmap.tif"))]
    if todo:
        walk = os.path.join(sdir, "walk")
        os.makedirs(walk, exist_ok=True)
        walked, groups = os.path.join(sdir, "walked.toml"), os.path.join(sdir, "groups.toml")
        ls.run_cli(cli, "walk", ["stack", d, "-o", walk, "--dry-run", "--write-groups", walked, "--algorithm", METHODS[0],
                                 "--align-algorithm", ALIGN_ALGO, "--json"])
        with open(walked, "rb") as fh:
            try:
                built = build_groups(tomllib.load(fh), frames)
            except ValueError as e:
                sys.exit(f"{name}: {e}")
        with open(groups, "w") as fh:
            fh.write(groups_toml(built))
    for m in METHODS:
        mdir = os.path.join(sdir, m)
        comp, zmap, proj = (os.path.join(mdir, f) for f in ("composite.tif", "zmap.tif", "stack.ffp"))
        if m not in todo:
            print(f"{name} {m}: done, skipped", flush=True)
            continue
        if not (os.path.exists(proj) and "stack_seconds" in times.get(m, {})):
            shutil.rmtree(mdir, ignore_errors=True)
            os.makedirs(mdir)
            rep = json.loads(ls.run_cli(cli, f"{name} {m}", ["stack", d, "-o", mdir, "--groups", groups, "--only", STACK_GROUP,
                                                              "--algorithm", m, "--align-algorithm", ALIGN_ALGO, "--json"]).stdout)
            if rep["frame_count"] != len(frames) or rep["outputs"] != [proj]:
                sys.exit(f"{name} {m}: the project has {rep['frame_count']} frames and outputs {rep['outputs']}, expected {len(frames)} and [{proj}]")
            times[m] = {"stack_seconds": rep["wall_seconds"], "frames": rep["frame_count"], "size": [rep["output_width"], rep["output_height"]]}
            _save_json(times_path, times)
            print(f"{name} {m}: {rep['frame_count']} frames, {rep['output_width']}x{rep['output_height']}, stack {rep['wall_seconds']:.1f} s", flush=True)
        t0 = time.monotonic()
        part = [os.path.join(mdir, f"{k}.part.tif") for k in ("composite", "zmap")]
        ls.run_cli(cli, f"{name} {m}", ["export", proj, "-o", part[0], "--zmap", part[1], "--json"])
        os.replace(part[0], comp)
        os.replace(part[1], zmap)
        times[m]["export_seconds"] = round(time.monotonic() - t0, 3)
        _save_json(times_path, times)
        print(f"{name} {m}: export {times[m]['export_seconds']:.1f} s", flush=True)
    sizes = {m: (lambda i: [i.width, i.height])(rawtiff.read_info(os.path.join(sdir, m, "composite.tif"))) for m in METHODS}
    if len({tuple(s) for s in sizes.values()}) != 1:
        print(f"{name}: the three composites differ in size: {sizes}", file=sys.stderr)
        return 1
    print(f"{name}: {len(frames)} frames, composites {sizes[METHODS[0]][0]}x{sizes[METHODS[0]][1]}, times in {times_path}")
    return 0


def phase_shift(a, b):
    """(dx, dy, peak): content at (x, y) of `a` lies at (x + dx, y + dy) of `b`; `peak` is the correlation maximum in 0..1."""
    h, w = a.shape
    win = np.outer(np.hanning(h), np.hanning(w))
    fa, fb = np.fft.rfft2((a - a.mean()) * win), np.fft.rfft2((b - b.mean()) * win)
    cross = fb * np.conj(fa)
    corr = np.fft.irfft2(cross / (np.abs(cross) + 1e-12), s=a.shape)
    iy, ix = np.unravel_index(np.argmax(corr), corr.shape)
    return int(ix - w if ix > w // 2 else ix), int(iy - h if iy > h // 2 else iy), float(corr[iy, ix])


def read_luma(path, x, y, w, h, info=None):
    info = info or rawtiff.read_info(path)
    if info.bits_per_sample[0] != 16:
        raise ValueError(f"{path}: luma reads 16-bit TIFFs")
    px = np.frombuffer(rawtiff.read_rect(path, x, y, w, h, info), "<u2").reshape(h, w, info.samples_per_pixel)
    return px.astype(np.float32).mean(axis=2)


def anchor_grid(width, height, cols, rows, inset):
    """Centres of a cols x rows grid of tiles spread over the image, `inset` px from its border."""
    return [(int(inset + (width - 2 * inset) * (i + 0.5) / cols), int(inset + (height - 2 * inset) * (j + 0.5) / rows)) for j in range(rows) for i in range(cols)]


def local_shifts(ref, comp, anchors, tile, predict=lambda x, y: (0, 0)):
    """(x, y, dx, dy, peak) per anchor: the `phase_shift` of the `tile` px luma square at (x, y) of `ref` against the one of `comp`
    displaced by `predict(x, y)`, read through `read_rect`; dx, dy include the prediction. Anchors whose tiles leave an image are skipped."""
    ri, ci = rawtiff.read_info(ref), rawtiff.read_info(comp)
    half, out = tile // 2, []
    for x, y in anchors:
        px, py = predict(x, y)
        x0, y0 = x - half, y - half
        if min(x0, y0, x0 + px, y0 + py) < 0 or x0 + tile > ri.width or y0 + tile > ri.height or x0 + px + tile > ci.width or y0 + py + tile > ci.height:
            continue
        dx, dy, peak = phase_shift(read_luma(ref, x0, y0, tile, tile, ri), read_luma(comp, x0 + px, y0 + py, tile, tile, ci))
        out.append((x, y, dx + px, dy + py, peak))
    return out


def measure_transform(ref, comp, grid=(8, 5), tile=512):
    """Similarity (a, b, tx, ty) taking `ref` coordinates into `comp`, with the tiles used and the largest residual in px.

    A grid of `tile` px tiles gives local shifts and a robust fit through them; the tiles are read again
    at the offset the fit predicts, so a few percent of scale no longer decorrelates them, and refitted.
    """
    ri, ci = rawtiff.read_info(ref), rawtiff.read_info(comp)
    w, h = min(ri.width, ci.width), min(ri.height, ci.height)
    anchors = anchor_grid(w, h, *grid, tile // 2)
    textured = lambda pts: [p for p in pts if p[4] >= PEAK_MIN]
    pts = textured(local_shifts(ref, comp, anchors, tile))
    for tol, _ in ((8.0, 0), (3.0, 1), (3.0, 2)):
        if len(pts) < MIN_TILES:
            raise ValueError(f"only {len(pts)} of {len(anchors)} tiles correlate; the images share too little texture")
        fit = fit_similarity(pts, tol)
        a, b, tx, ty = fit[0]
        predict = lambda x, y: (int(np.rint(a * x - b * y + tx - x)), int(np.rint(b * x + a * y + ty - y)))
        pts = textured(local_shifts(ref, comp, anchors, tile, predict))
    return fit_similarity(pts)


def fit_similarity(points, tolerance=3.0):
    """Least-squares similarity x' = a x - b y + tx, y' = b x + a y + ty through the points' (x, y) -> (x + dx, y + dy).

    Returns ((a, b, tx, ty), points kept, largest residual in px); a point further than `tolerance` px
    (or 3 median residuals) from the fit is dropped and the fit repeated while three points remain.
    """
    pts = [(x, y, x + dx, y + dy) for x, y, dx, dy, _ in points]
    while True:
        m = np.zeros((2 * len(pts), 4))
        t = np.zeros(2 * len(pts))
        for i, (x, y, xp, yp) in enumerate(pts):
            m[2 * i], m[2 * i + 1] = (x, -y, 1, 0), (y, x, 0, 1)
            t[2 * i], t[2 * i + 1] = xp, yp
        u = np.linalg.lstsq(m, t, rcond=None)[0]
        res = np.hypot(*(m @ u - t).reshape(-1, 2).T)
        limit = max(tolerance, 3 * float(np.median(res)))
        keep = [p for p, r in zip(pts, res) if r <= limit]
        if len(keep) == len(pts) or len(keep) < 3:
            return tuple(float(v) for v in u), len(pts), float(res.max())
        pts = keep


def overlaps(rect, others):
    x, y, w, h = rect
    return any(x < ox + ow and ox < x + w and y < oy + oh and oy < y + h for ox, oy, ow, oh in others)


def move_rect(rect, transform, width, height):
    """`rect` seen through the similarity `transform` (a, b, tx, ty), as a square of its own side around the mapped centre.

    ValueError when it leaves the image or overlaps a training window seen through the same transform
    (the bounding box of the mapped window).
    """
    x, y, w, h = rect
    a, b, tx, ty = transform
    cx, cy = x + w / 2, y + h / 2
    moved = (int(np.rint(a * cx - b * cy + tx - w / 2)), int(np.rint(b * cx + a * cy + ty - h / 2)), w, h)
    if moved[0] < 0 or moved[1] < 0 or moved[0] + w > width or moved[1] + h > height:
        raise ValueError(f"rect {moved} leaves the {width}x{height} composite")
    for wx, wy in hn.WINDOWS:
        win = ls.gff.own_rect((wx, wy, CROP_SIDE, CROP_SIDE), transform)
        if overlaps(moved, [win]):
            raise ValueError(f"rect {moved} overlaps training window {win} ({wx},{wy} in the reference result)")
    return moved


def context_rect(rect, width, height, margin=CONTEXT):
    """`rect` grown by `margin` px per side, clipped to the image."""
    x, y, w, h = rect
    x0, y0, x1, y1 = max(0, x - margin), max(0, y - margin), min(width, x + w + margin), min(height, y + h + margin)
    return x0, y0, x1 - x0, y1 - y0


def write_preview(src, out):
    """Quarter-scale PNG of a 16-bit TIFF read in bands, with a labelled grid every 512 full-resolution px."""
    info = rawtiff.read_info(src)
    s, band = PREVIEW_SCALE, 256
    w, h = info.width // s * s, info.height // s * s
    rows = []
    for y in range(0, h, band):
        n = min(band, h - y)
        px = np.frombuffer(rawtiff.read_rect(src, 0, y, w, n, info), "<u2").reshape(n // s, s, w // s, s, info.samples_per_pixel)
        rows.append((px.astype(np.float32).mean(axis=(1, 3)) / 257).round().astype(np.uint8))
    img = Image.fromarray(np.concatenate(rows, axis=0))
    draw = ImageDraw.Draw(img)
    for g in range(0, w, PREVIEW_GRID):
        draw.line([(g // s, 0), (g // s, h // s)], fill=(0, 255, 255), width=1)
        draw.text((g // s + 2, 2), str(g), fill=(255, 255, 0))
    for g in range(0, h, PREVIEW_GRID):
        draw.line([(0, g // s), (w // s, g // s)], fill=(0, 255, 255), width=1)
        draw.text((2, g // s + 2), str(g), fill=(255, 255, 0))
    img.save(out)
    return img.size


def crop_table(rows):
    lines = ["| stack | name | composite rect x,y,w,h | source rect | transform a,b,tx,ty (tiles, residual px) | context rect | shows |", "|---|---|---|---|---|---|---|"]
    for r in rows:
        src = "reference result " + ",".join(map(str, r["source_rect"])) if r["source_rect"] else "composite"
        shift = "{:.5f},{:.5f},{:.1f},{:.1f} ({}, {:.1f})".format(*r["transform"], r["tiles"], r["residual"]) if r["transform"] else "-"
        lines.append(f"| {r['stack']} | {r['name']} | {','.join(map(str, r['rect']))} | {src} | {shift} | {','.join(map(str, r['context']))} | {r['edge']} |")
    return "\n".join(lines)


def cmd_crops(args):
    """`--preview STACK` writes a gridded preview of its weighted-average composite; otherwise every CROPS rect is cut."""
    out = os.path.abspath(args.out)
    if args.preview:
        src = os.path.join(out, args.preview, METHODS[0], "composite.tif")
        if not os.path.exists(src):
            sys.exit(f"{src}: run the stack first")
        dst = os.path.join(out, args.preview, "preview.png")
        print(f"{dst}: {write_preview(src, dst)}")
        return 0
    ref_root = os.environ.get(REFERENCE_VAR)
    rows, fits = [], {}
    crops = CROPS
    if args.rect:  # rects given as STACK:NAME:X:Y in composite coordinates replace the table, for fixtures
        crops = [{"stack": st, "name": n, "xy": (int(x), int(y)), "edge": "given on the command line"} for st, n, x, y in (r.split(":") for r in args.rect)]
    for c in crops:
        comps = {m: os.path.join(out, c["stack"], m, "composite.tif") for m in METHODS}
        if not all(os.path.exists(p) for p in comps.values()):
            print(f"{c['stack']} {c['name']}: stack not run, skipped")
            continue
        info = rawtiff.read_info(comps[METHODS[0]])
        rect = c["xy"] + (args.side, args.side)
        fit = source = None
        if c["stack"] == "mantis":
            if not ref_root:
                sys.exit(f"{REFERENCE_VAR} is not set to the reference stack folder")
            fit = fits.setdefault("mantis", measure_transform(os.path.join(ref_root, MANTIS_REFERENCE), comps[METHODS[0]]))
            source = rect
            try:
                rect = move_rect(rect, fit[0], info.width, info.height)
            except ValueError as e:
                sys.exit(f"{c['stack']} {c['name']}: {e}")
        elif rect[0] < 0 or rect[1] < 0 or rect[0] + args.side > info.width or rect[1] + args.side > info.height:
            sys.exit(f"{c['stack']} {c['name']}: rect {rect} is outside the {info.width}x{info.height} composite")
        ctx = context_rect(rect, info.width, info.height)
        for m in METHODS:
            for kind in ("composite", "zmap"):
                ls.gff.crop_image(os.path.join(out, c["stack"], m, f"{kind}.tif"), ctx, os.path.join(out, "crops", c["stack"], c["name"], m, f"{kind}.tif"))
        rows.append({"stack": c["stack"], "name": c["name"], "rect": list(rect), "source_rect": list(source) if source else None,
                     "transform": list(fit[0]) if fit else None, "tiles": fit[1] if fit else None, "residual": fit[2] if fit else None, "context": list(ctx), "edge": c["edge"]})
    if rows:
        os.makedirs(os.path.join(out, "crops"), exist_ok=True)
        _save_json(os.path.join(out, "crops", "crops.json"), rows)
    print(crop_table(rows))
    return 0


def synthetic_stack(n, kind, h=420, w=640, seed=1):
    """Luma frames of a smooth scene with a texture that is sharp in column band k of frame k
    (`stack`), or: shifted between frames (`moving`), sharp in frame 0 only (`one-sharp`), noise (`noise`)."""
    rng = np.random.default_rng(seed)
    low = np.asarray(Image.fromarray((rng.random((6, 8)) * 255).astype(np.uint8)).resize((w, h), Image.Resampling.BICUBIC), np.float64)
    tex = rng.normal(0, 30, (h, w))
    band = lambda k: (np.arange(w) * n // w == k)[None, :]
    frames = []
    for k in range(n):
        if kind == "stack":
            f = low + tex * np.where(band(k), 1.0, 0.05)
        elif kind == "moving":
            f = np.roll(low + tex * np.where(band(k), 1.0, 0.05), 60 * k, axis=1)
        elif kind == "one-sharp":
            f = low + tex * (1.0 if k == 0 else 0.05)
        else:
            f = low + rng.normal(0, 30, (h, w))
        frames.append(np.clip(f, 0, 255).astype(np.uint8))
    return frames



def _selftest() -> int:
    ok = True

    def check(name, cond, detail=""):
        nonlocal ok
        print(f"  {'ok  ' if cond else 'FAIL'} {name}{(': ' + str(detail)) if detail and not cond else ''}")
        ok = ok and bool(cond)

    def rec(name, t, d="/data/shoot", fl=100, f=8, exp=0.01, iso=100, fd=None, fb=0, orient=1):
        r = {"SourceFile": f"{d}/{name}", "DateTimeOriginal": t, "FocalLength": fl, "FNumber": f,
             "ExposureTime": exp, "ISO": iso, "FocusBracketing": fb, "Orientation": orient}
        if fd is not None:
            r["FocusDistanceUpper"] = fd
            r["FocusDistanceLower"] = fd - 0.01
        return r

    def at(sec):
        return f"2025:06:08 10:{sec // 60:02d}:{sec % 60:02d}"

    def run(records):
        return group_directory(records)

    print("grouping (the product's gap rule):")
    gap = lambda g: [rec(f"A{i}.dng", at(i * 20 + (g if i >= 2 else 0)), fd=1.0 + i) for i in range(4)]
    # frames at 0, 20, then 40+g, 60+g: the gap between frame 1 and 2 is 20 + g
    chunks, untimed = run(gap(40))
    check("a 60 s gap does not split", len(chunks) == 1 and not untimed, [len(c) for c in chunks])
    chunks, _ = run(gap(41))
    check("a 61 s gap splits", [len(c) for c in chunks] == [2, 2], [len(c) for c in chunks])
    shuffled = list(reversed(gap(0)))
    chunks, _ = run(shuffled)
    check("frames sort by (time, name)", [r["SourceFile"].rsplit("/", 1)[1] for r in chunks[0]] == ["A0.dng", "A1.dng", "A2.dng", "A3.dng"])
    chunks, untimed = run(gap(0) + [rec("U1.dng", None), {"SourceFile": "/data/shoot/U2.dng"}])
    check("untimed frames are listed apart", len(chunks) == 1 and len(chunks[0]) == 4 and len(untimed) == 2, (len(chunks), len(untimed)))
    check("an invalid time is untimed", parse_time("2025:13:08 10:00:00") is None and parse_time("2025-06-08 10:00:00") is None and parse_time("2025:06:08 10:00:01") - parse_time("2025:06:08 10:00:00") == 1)

    print("candidate rule:")
    run4 = lambda **kw: [rec(f"B{i}.dng", at(i * 2), **{**kw, **({"fd": 1.0 + i} if kw.get("fd") == "ramp" else {})}) for i in range(4)]
    j = judge(run4(fd="ramp"))
    check("a focus-distance ramp is kept, marked and called monotonic", j["candidate"] and j["mark"] == "focus distance changes" and "monotonic" in j["reasons"][-1], j)
    j = judge([rec(f"M{i}.dng", at(i), fd=d) for i, d in enumerate([1.0, 3.0, 2.0, 4.0])])
    check("a zigzag focus distance is marked but not monotonic", j["mark"] == "focus distance changes" and "monotonic" not in j["reasons"][-1], j)
    j = judge(run4(fd=1.0))
    check("a constant-focus run is kept and marked", j["candidate"] and j["mark"] == "focus constant, possibly rail or burst", j)
    j = judge(run4())
    check("a run without a focus distance is kept and marked", j["candidate"] and j["mark"] == "focus constant, possibly rail or burst" and any("no focus distance" in r for r in j["reasons"]), j)
    j = judge(run4(fb=1, fd=1.0))
    check("camera-reported focus bracketing is marked first", j["candidate"] and j["mark"] == "focus bracketing", j)
    j = judge([rec(f"C{i}.dng", at(i), exp=0.01 * (i + 1)) for i in range(4)])
    check("an exposure bracket is rejected", not j["candidate"] and any("exposure time" in r for r in j["reasons"]), j)
    j = judge([rec(f"D{i}.dng", at(i), iso=100 * (i + 1)) for i in range(4)])
    check("an ISO change is rejected", not j["candidate"] and any("ISO" in r for r in j["reasons"]), j)
    j = judge([rec(f"E{i}.dng", at(i), f=4 + 2 * i) for i in range(4)])
    check("an f-number change is rejected", not j["candidate"] and any("f-number" in r for r in j["reasons"]), j)
    j = judge([rec(f"F{i}.dng", at(i), fl=24 + 10 * i) for i in range(4)])
    check("a focal-length change is rejected", not j["candidate"] and any("focal length" in r for r in j["reasons"]), j)
    mixed = run4(fd="ramp")
    mixed[0]["SourceFile"] = "/data/shoot/export.tif"
    j = judge(mixed)
    check("a run that mixes raw and TIFF frames says so", j["candidate"] and any("1 TIFF" in r for r in j["reasons"]), j)
    j = judge(run4(fd="ramp")[:3])
    check("a three-frame run is rejected", not j["candidate"] and any("3 frames" in r for r in j["reasons"]), j)

    print("search over several directories:")
    recs = ([rec(f"G{i}.dng", at(i), d="/data/a", fd=1.0 + i) for i in range(5)]
            + [rec(f"H{i}.dng", at(i), d="/data/b") for i in range(3)]
            + [{"SourceFile": "/data/b/Z.dng", "FileSize": 0, "Error": "File is empty"}])
    res = search(recs)
    check("one candidate, one rejected group, one skipped file",
          len(res["candidates"]) == 1 and len(res["rejected"]) == 1 and len(res["skipped"]) == 1, {k: len(v) for k, v in res.items()})
    check("the skip names the reason", res["skipped"][0]["reason"] == "File is empty", res["skipped"])
    check("a candidate carries its frame range and count", res["candidates"][0]["count"] == 5 and res["candidates"][0]["frames"][0].endswith("G0.dng"), res["candidates"][0])

    recs = [rec(f"I{i}.dng", at(i), iso=100 if i < 4 else 200, fd=1.0 + i) for i in range(8)]
    res = search(recs)
    check("an ISO change inside a time group splits it into two candidates",
          [c["count"] for c in res["candidates"]] == [4, 4] and not res["rejected"], res)
    recs = [rec(f"J{i}.dng", at(i), fd=1.0 + i) for i in range(5)] + [rec("J5.dng", at(5), fl=0, f=None)]
    res = search(recs)
    check("a stray frame is cut off the run and counted",
          [c["count"] for c in res["candidates"]] == [5] and res["fragments"] == {"runs": 1, "frames": 1}, res)
    res = search([rec(f"K{i}.dng", at(i), exp=0.01 * (i + 1)) for i in range(4)])
    check("an exposure bracket stays one rejected group", not res["candidates"] and len(res["rejected"]) == 1 and res["rejected"][0]["count"] == 4, res)

    print("preview layout:")
    check("10 frames pick 0, 3, 6, 9", pick_indices(10) == [0, 3, 6, 9], pick_indices(10))
    check("4 frames pick 0, 1, 2, 3", pick_indices(4) == [0, 1, 2, 3], pick_indices(4))
    check("5 frames pick 0, 1, 3, 4", pick_indices(5) == [0, 1, 3, 4], pick_indices(5))
    marker = Image.new("RGB", (4, 2))
    marker.putpixel((0, 0), (255, 0, 0))
    up = upright(marker, "Rotate 270 CW")
    check("Rotate 270 CW is turned upright", up.size == (2, 4) and up.getpixel((0, 3)) == (255, 0, 0), (up.size, up.getpixel((0, 3))))
    check("numeric orientation 8 equals the same turn", upright(marker, 8).getpixel((0, 3)) == (255, 0, 0))
    check("orientation 1 leaves the image", upright(marker, 1).size == (4, 2))
    cells = [(Image.new("RGB", (300, 200), (200, 50, 50)), None), (None, "no embedded preview"), (Image.new("RGB", (200, 300), (50, 200, 50)), None), (Image.new("RGB", (300, 200)), None)]
    sheet = layout(cells, ["a.dng", "b.dng", "c.dng", "d.dng"], "shoot", 4)
    check("the 2x2 layout has the header and four cells", sheet.size == (2 * CELL[0], HEADER + 2 * (CELL[1] + LABEL)), sheet.size)
    check("a skipped cell is grey, not the sheet background", sheet.getpixel((CELL[0] + 5, HEADER + LABEL + 5)) == SKIP_FILL, sheet.getpixel((CELL[0] + 5, HEADER + LABEL + 5)))

    print("preview source:")
    p, why = frame_preview("x.dng", 1, lambda path, tag: b"")
    check("a frame without an embedded preview is skipped with a reason", p is None and "no embedded preview" in why, (p, why))
    big, small = io.BytesIO(), io.BytesIO()
    Image.new("RGB", (1600, 1000), (9, 9, 9)).save(big, "JPEG")
    Image.new("RGB", (160, 100), (9, 9, 9)).save(small, "JPEG")
    served = {"ThumbnailImage": small.getvalue(), "PreviewImage": big.getvalue(), "JpgFromRaw": big.getvalue()}
    p, why = frame_preview("x.dng", 1, lambda path, tag: served[tag])
    check("the first preview of at least 1000 px is taken", p is not None and p.size == (1600, 1000), (p and p.size, why))
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "f.tif")
        w, h = 3000, 1200
        px = np.zeros((h, w, 3), "<u2")
        px[:, :, 0] = (np.arange(w)[None, :] * 20).astype("<u2")
        rawtiff.write_tiff(path, w, h, px.tobytes())
        p, why = frame_preview(path, 1, lambda *a: b"")
        check("an uncompressed 16-bit TIFF is subsampled strip by strip", p is not None and max(p.size) <= 1600 and p.getpixel((p.size[0] - 1, 0))[0] > p.getpixel((1, 0))[0], (p and p.size, why))

    print("reference sets:")
    refs = {"beetle": {"IMG_1", "IMG_2", "IMG_3", "IMG_4"}}
    check("date-prefixed and plain stems match a reference set",
          match_reference(["/x/20250608-IMG_1.dng", "/x/img_2.CR3", "/x/IMG_3.cr3", "/x/IMG_9.cr3"], refs) == "beetle")
    check("a minority overlap does not match", match_reference(["/x/IMG_1.dng", "/x/IMG_8.dng", "/x/IMG_9.dng"], refs) is None)
    dated = {"beetle": {"20250608-img_1", "20250608-img_2", "20250608-img_3", "20250608-img_4"}}
    check("a dated name matches only on the same date",
          match_reference([f"/x/20250608-IMG_{i}.dng" for i in (1, 2, 3)], dated) == "beetle"
          and match_reference([f"/x/20260612-IMG_{i}.dng" for i in (1, 2, 3, 4)], dated) is None)
    check("an undated name matches a dated reference by its camera number",
          match_reference([f"/x/IMG_{i}.CR3" for i in (1, 2, 3)], dated) == "beetle")

    print("image test:")
    stack_t = synthetic_stack(8, "stack")
    m, drop = image_test(stack_t)
    check("a stack with the sharp region moving across the picture passes", drop is None and m["distinct"] == 8, (m, drop))
    m, drop = image_test(synthetic_stack(8, "moving"))
    check("frames that move between shots fail the alignment test", drop == "alignment", (m, drop))
    m, drop = image_test(synthetic_stack(8, "one-sharp"))
    check("one frame sharp everywhere fails the focus-travel test", drop == "focus travel" and m["best_share"] == 1.0, (m, drop))
    m = focus_travel(synthetic_stack(8, "noise"))
    check("independent noise frames have no sharp peak per tile", m["peak"] < TRAVEL_MIN_PEAK, m)
    two = synthetic_stack(8, "stack", seed=1) + synthetic_stack(8, "stack", seed=2)
    cuts = alignment_cuts(len(two), lambda i: two[i])
    check("two different scenes in one run are cut where the picture changes", cuts == [8], cuts)
    check("one scene is not cut", alignment_cuts(8, lambda i: stack_t[i]) == [])
    check("sample positions include first and last and stay within the run", sample_indices(61) == [0, 7, 13, 20, 27, 33, 40, 47, 53, 60] and sample_indices(4) == [0, 1, 2, 3])
    rows = [rec(f"S{i}.dng", at(i * 6 if i < 8 else i * 6 + 120), fd=0.30 + 0.01 * (i if i < 8 else 16 - i)) for i in range(12)]
    cuts = [len(r) for r in split_stack_runs(rows)]
    check("a long gap and a focus reversal each cut a run", cuts == [8, 4], cuts)
    check("a steady run is not cut", [len(r) for r in split_stack_runs(run4(fd="ramp"))] == [4])
    mixed = [rec(f"U{i}.dng", at(i)) for i in range(8)] + [rec("export.tif", at(3))]
    check("a TIFF export among raw frames is left out of the run", len(drop_minority_type(mixed)) == 8 and all(r["SourceFile"].endswith(".dng") for r in drop_minority_type(mixed)))
    rows = [rec(f"T{i}.dng", at(i), orient=1 if i < 5 else 8) for i in range(8)]
    check("a run in two camera orientations is dropped by metadata", "orientation" in prefilter(rows), prefilter(rows))
    check("a run of five frames is dropped by metadata", prefilter(run4(fd="ramp") + [rec("X.dng", at(9))]) is not None)

    print("boundaries:")
    with tempfile.TemporaryDirectory() as tmp:
        root_a, root_b = os.path.join(tmp, "a"), os.path.join(tmp, "b")
        os.makedirs(root_a)
        try:
            refuse_output_under(os.path.join(root_a, "x"), [root_a, root_b])
            check("an output under a root is refused", False)
        except RefusedOutput as e:
            check("an output under a root is refused and names the root", root_a in str(e), str(e))
        try:
            refuse_output_under(os.path.join(tmp, "a", "..", "a", "y", "z"), [root_a])
            check("a dotted path into a root is refused", False)
        except RefusedOutput:
            check("a dotted path into a root is refused", True)
        refuse_output_under(os.path.join(tmp, "out"), [root_a, root_b])
        check("an output beside the roots is accepted", True)
        out = os.path.join(tmp, "out")
        err = io.StringIO()
        old, sys.stderr = sys.stderr, err
        try:
            found = existing_roots([root_a, root_b])
            rc_none = main(["candidates", "--root", root_b, "--root", root_b + "2", "--out", out])
        finally:
            sys.stderr = old
        check("a missing root is named on stderr and the other kept", found == [root_a] and root_b in err.getvalue(), (found, err.getvalue()))
        check("every root missing exits 1 and creates no output", rc_none == 1 and not os.path.exists(out), rc_none)
        rc_refused = main(["candidates", "--root", root_a, "--out", os.path.join(root_a, "x")])
        check("the refused output exits non-zero and creates nothing", rc_refused != 0 and not os.path.exists(os.path.join(root_a, "x")), rc_refused)
        out2, frames_dir = os.path.join(tmp, "out2"), os.path.join(root_a, "f")
        os.makedirs(out2)
        os.makedirs(frames_dir)
        rows = []
        for i, f in enumerate(synthetic_stack(8, "stack")):
            path = os.path.join(frames_dir, f"L{i}.tif")
            rawtiff.write_tiff(path, f.shape[1], f.shape[0], np.repeat(f[:, :, None].astype("<u2") * 256, 3, axis=2).tobytes())
            rows.append(rec(f"f/L{i}.tif", at(i), d=root_a, fd=0.30 + 0.01 * i))
        with open(os.path.join(out2, "records.json"), "w") as fh:
            json.dump(rows, fh)
        old, sys.stdout = sys.stdout, io.StringIO()
        try:
            rc_reuse = main(["candidates", "--root", root_a, "--out", out2, "--reuse-records"])
        finally:
            sys.stdout = old
        index = json.load(open(os.path.join(out2, "index.json")))
        jpgs = [f for f in os.listdir(out2) if f.endswith(".jpg")]
        check("a synthetic stack on disk passes both tests and gets one preview",
              rc_reuse == 0 and len(jpgs) == index["counts"]["shortlist"] == 1 and not index["dropped"], (rc_reuse, jpgs, index["dropped"]))
        rows = rows[:5]
        with open(os.path.join(out2, "records.json"), "w") as fh:
            json.dump(rows, fh)
        old, sys.stdout = sys.stdout, io.StringIO()
        try:
            main(["candidates", "--root", root_a, "--out", out2, "--reuse-records"])
        finally:
            sys.stdout = old
        index = json.load(open(os.path.join(out2, "index.json")))
        check("a run below the frame floor is dropped with its test named and no preview",
              index["counts"]["shortlist"] == 0 and [d["test"] for d in index["dropped"]] == ["prefilter"], index["dropped"])

    print("stack runs (groups file, sources):")
    walked = {"group": [{"name": "a", "frames": ["s/1.tif", "s/2.tif", "x.dng"]}, {"name": "b", "frames": ["s/3.tif"]},
                        {"name": "stack", "frames": ["t1.tif", "t2.tif"]}]}
    stack_frames = ["s/1.tif", "s/2.tif", "s/3.tif"]
    groups = build_groups(walked, stack_frames)
    listed = [f for g in groups["group"] for f in g["frames"]]
    named = {g["name"]: g["frames"] for g in groups["group"]}
    check("the stack group holds exactly the stack, in order", named.get(STACK_GROUP) == stack_frames, named)
    check("every walked frame is listed once", sorted(listed) == sorted(f for g in walked["group"] for f in g["frames"]), listed)
    check("a walked group left empty is dropped and another keeps its frames",
          "b" not in named and named.get("a") == ["x.dng"], named)
    check("a walked group already called stack is renamed, not merged", len(named) == 3 and named.get("walked-stack") == ["t1.tif", "t2.tif"], named)
    check("the groups file reads back as written", tomllib.loads(groups_toml(groups)) == groups)
    try:
        build_groups(walked, stack_frames + ["s/9.tif"])
        check("a stack frame the walk did not find is refused", False)
    except ValueError as e:
        check("a stack frame the walk did not find is refused and named", "s/9.tif" in str(e), str(e))
    with tempfile.TemporaryDirectory() as tmp:
        refroot, other = os.path.join(tmp, "ref"), os.path.join(tmp, "elsewhere")
        for d, names in ((os.path.join(refroot, "mantis"), [f"M{i:02d}.tif" for i in range(60)]), (os.path.join(refroot, "beetle"), ["b1.tif", "b2.TIF", "note.txt"])):
            os.makedirs(os.path.join(d, "sub"))
            for n in names:
                open(os.path.join(d, n), "w").close()
            open(os.path.join(d, "sub", "extra.tif"), "w").close()
        err, old = io.StringIO(), sys.stderr
        sys.stderr = err
        try:
            unset = [resolve_stack(argparse.Namespace(mantis=True, reference=None, pick=None, index=None), None)]
        except SystemExit as e:
            unset = [e]
        try:
            sixty = resolve_stack(argparse.Namespace(mantis=True, reference=None, pick=None, index=None), refroot)
        except SystemExit as e:
            sixty = e
        finally:
            sys.stderr = old
        check("an unset reference variable stops the Mantis step, naming it", isinstance(unset[0], SystemExit) and "FOCUSFORGE_REFERENCE_STACK" in str(unset[0]), unset)
        check("a Mantis folder without 61 tiffs is refused with the count", isinstance(sixty, SystemExit) and "60" in str(sixty), sixty)
        name, d, frames = resolve_stack(argparse.Namespace(mantis=False, reference="beetle", pick=None, index=None), refroot)
        check("a reference folder yields its top-level tiffs only, sorted", name == "beetle" and frames == ["b1.tif", "b2.TIF"], frames)
        idx = os.path.join(tmp, "index.json")
        json.dump({"candidates": [{"id": 7, "dir": other, "frames": [os.path.join(other, "z2.dng"), os.path.join(other, "z1.dng")]}]}, open(idx, "w"))
        name, d, frames = resolve_stack(argparse.Namespace(mantis=False, reference=None, pick=7, index=idx), refroot)
        check("a pick reads its folder and frames from the index, relative, in index order", name == "pick-007" and d == other and frames == ["z2.dng", "z1.dng"], (name, d, frames))
        try:
            resolve_stack(argparse.Namespace(mantis=False, reference=None, pick=8, index=idx), refroot)
            check("a pick missing from the index is refused", False)
        except SystemExit as e:
            check("a pick missing from the index is refused and named", "8" in str(e), str(e))

    print("crops (shift, windows, context):")
    rng = np.random.default_rng(7)
    smooth = np.asarray(Image.fromarray((rng.random((12, 16)) * 255).astype(np.uint8)).resize((1600, 1200), Image.Resampling.BICUBIC), np.float64)
    big = np.clip(smooth + rng.normal(0, 8, smooth.shape), 0, 255)
    moved = np.roll(big, (-21, 37), axis=(0, 1))
    check("phase correlation recovers a known (+37, -21) shift", phase_shift(big[200:1000, 300:1300], moved[200:1000, 300:1300])[:2] == (37, -21))
    scale, tx, ty = 0.985, -40.0, -20.0
    warped = np.asarray(Image.fromarray(big.astype(np.float32), "F").transform((1500, 1150), Image.Transform.AFFINE, (1 / scale, 0, -tx / scale, 0, 1 / scale, -ty / scale),
                                                                              Image.Resampling.BICUBIC), np.float64)
    with tempfile.TemporaryDirectory() as tmp:
        paths = []
        for k, a in enumerate((big, warped)):
            p = os.path.join(tmp, f"i{k}.tif")
            rgb = np.repeat((a * 255).astype("<u2")[:, :, None], 3, axis=2)
            rawtiff.write_tiff(p, a.shape[1], a.shape[0], rgb.tobytes(), bits=16, spp=3)
            paths.append(p)
        (a, b, ftx, fty), used, _ = measure_transform(paths[0], paths[1], (4, 3), 512)
        check("a scale of 0.985 and a shift between two TIFFs are recovered from tiles over the picture",
              abs(a - scale) < 0.003 and abs(b) < 0.003 and abs(ftx - tx) < 3 and abs(fty - ty) < 3, (a, b, ftx, fty))
        good = [(x, y, a * x + ftx - x, a * y + fty - y, 0.5) for x, y in anchor_grid(1600, 1200, 4, 3, 256)]
        good[0] = (good[0][0], good[0][1], good[0][2] + 90, good[0][3] - 70, 0.5)
        a2, _, tx2, _ = fit_similarity(good)[0]
        check("one wrong tile does not move the fit", abs(a2 - scale) < 0.003 and abs(tx2 - tx) < 3, (a2, tx2))
    w0 = hn.WINDOWS[0]
    ident = (1.0, 0.0, 0.0, 0.0)
    check("a rect 1 px into a window is refused", overlaps((w0[0] - 511, w0[1], 512, 512), [(w0[0], w0[1], 512, 512)]))
    check("a rect touching a window is kept", not overlaps((w0[0] - 512, w0[1], 512, 512), [(w0[0], w0[1], 512, 512)]))
    try:
        move_rect((w0[0] - 511, w0[1], 512, 512), ident, 5000, 4000)
        check("a rect on a window is refused", False)
    except ValueError as e:
        check("a rect on a window is refused and names the window", f"{w0[0]}" in str(e), str(e))
    check("a rect clear of every window is moved by the transform", move_rect((2112, 384, 512, 512), (1, 0, -85, -71), 5000, 4000) == (2027, 313, 512, 512))
    check("a scaled transform keeps the rect 512 px wide around the mapped centre", move_rect((2112, 384, 512, 512), (0.97, 0, -135, -33), 5000, 4000) == (1906, 332, 512, 512))
    try:
        move_rect((2112, 384, 512, 512), (1, 0, -3000, 0), 5000, 4000)
        check("a rect mapped out of the image is refused", False)
    except ValueError as e:
        check("a rect mapped out of the image is refused", "leaves" in str(e), str(e))
    check("a context margin of 128 px stays whole inside the image", context_rect((1000, 900, 512, 512), 4000, 3000) == (872, 772, 768, 768))
    check("a context margin is clipped at the top-left border", context_rect((0, 50, 512, 512), 4000, 3000) == (0, 0, 640, 690))
    check("a context margin is clipped at the bottom-right border", context_rect((3488, 2488, 512, 512), 4000, 3000) == (3360, 2360, 640, 640))
    mantis = [c for c in CROPS if c["stack"] == "mantis"]
    check("the Mantis rects are the decided three", [(c["name"], c["xy"]) for c in mantis] == [("antenna", (2112, 384)), ("right-eye", (2880, 960)), ("reserved", (3840, 2476))], mantis)
    check("no Mantis rect touches a training window", not any(overlaps(c["xy"] + (CROP_SIDE, CROP_SIDE), [w + (CROP_SIDE, CROP_SIDE) for w in hn.WINDOWS]) for c in mantis))
    check("every non-Mantis stack has at most two rects", max(Counter(c["stack"] for c in CROPS if c["stack"] != "mantis").values(), default=0) <= 2)
    check("every rect has a distinct name within its stack", len({(c["stack"], c["name"]) for c in CROPS}) == len(CROPS))

    print("self-check passed" if ok else "self-check FAILED")
    return 0 if ok else 1


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--self-check", action="store_true")
    sub = ap.add_subparsers(dest="cmd")
    c = sub.add_parser("candidates", help="search read-only roots for likely focus stacks")
    c.add_argument("--root", action="append", required=True, help="a directory tree to search; repeat for more")
    c.add_argument("--out", required=True, help="where previews and the index go; refused under any --root")
    c.add_argument("--reuse-records", action="store_true", help="read <out>/records.json from an earlier run instead of running exiftool")
    k = sub.add_parser("controls", help="image-test measurements of the reference stacks and named negative runs")
    k.add_argument("--records", required=True, help="records.json written by candidates")
    k.add_argument("--cache", required=True, help="luma cache folder")
    k.add_argument("--reference", action="append", default=[], help="reference directory to measure; repeat")
    k.add_argument("--negative", action="append", help="DIR:FIRST_STEM:LAST_STEM of a run that is not a stack; repeat")
    r = sub.add_parser("run", help="stack one source with every method and export composite and Z-Map")
    src = r.add_mutually_exclusive_group(required=True)
    src.add_argument("--mantis", action="store_true", help="the 61 frames under the reference stack folder's mantis directory")
    src.add_argument("--reference", help="a directory of the reference stack folder, all its top-level frames")
    src.add_argument("--pick", type=int, help="candidate id in --index")
    r.add_argument("--index", default=DEFAULT_INDEX, help="index.json written by candidates")
    r.add_argument("--out", required=True, help="folder for <stack>/<method>/...; refused under the stack's folder")
    p = sub.add_parser("crops", help="cut the context crops of every CROPS rect, or write a gridded preview of one stack")
    p.add_argument("--out", required=True, help="the folder `run` wrote into")
    p.add_argument("--preview", metavar="STACK", help="write <out>/<STACK>/preview.png instead")
    p.add_argument("--rect", action="append", metavar="STACK:NAME:X:Y", help="cut this composite-coordinate rect instead of the CROPS table; repeat")
    p.add_argument("--side", type=int, default=CROP_SIDE, help="rect side in px")
    args = ap.parse_args(argv)
    if args.self_check:
        return _selftest()
    if args.cmd == "candidates":
        return cmd_candidates(args)
    if args.cmd == "controls":
        return cmd_controls(args)
    if args.cmd == "run":
        return cmd_run(args)
    if args.cmd == "crops":
        return cmd_crops(args)
    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
