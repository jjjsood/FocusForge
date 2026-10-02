#!/usr/bin/env python3
"""Guided-filter two-scale fusion prototype and the measurements that judge it.

Subcommands (run from the repository root, workspace built):

    cases    build synthetic/real cases, run the three baselines, score them
             (--equalize off|on|both|product adds per-frame brightness equalization before fusion;
             --product-coverage, --stack-set KEY=VAL)
    fuse     fuse one frame directory with the guided-filter prototype
    score    score a composite against a case's reference
    reach    measure the prototype's filter support with an impulse
    scale    memory/read scaling over N frames, prototype and baselines
    montage  side-by-side 8-bit crop of composites as PNG
    --self-check   detector controls on a tiny constructed stack

Requires numpy; reads the large reference stack only through
FOCUSFORGE_REFERENCE_STACK.
"""

import argparse
import glob
import json
import os
import resource
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zlib

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "tools", "lib"))
sys.path.insert(0, os.path.join(ROOT, "spikes", "synthetic-ground-truth"))
import rawtiff  # noqa: E402
import synth_stack as ss  # noqa: E402

ss.rawtiff = rawtiff
CLI = os.path.join(ROOT, "target", "debug", "focusforge-cli")
SYNTH = os.path.join(ROOT, "spikes", "synthetic-ground-truth", "synth_stack.py")
ALGOS = ["weighted-average", "laplacian", "depth-map"]
BREAKS = ["uniform-weights", "ignore-mask", "per-channel-guide"]
EQ_BREAKS = ["wrong-reference", "ignore-mask", "unbounded-gain"]
EQ_BAND, EQ_BOUND, EQ_MIN_SAMPLES = (0.05, 0.95), (0.8, 1.25), 1000
EPS_BASE, EPS_DET = 0.3, 1e-6
GLINT_RING = (2, 8)  # annulus of the glint metrics, px outside the disc edge
GLINT_R, GLINT_DEPTH, GLINT_DEPTH_R = 3, 3.0, 14
FULL = 65535.0


# ---------------------------------------------------------------- metrics
# Counts are u16 counts.


def hair_contrast(L, segs):
    """Surround minus line luma per segment.

    A segment is (kind, line rows/cols, surround band 1, surround band 2, extent along the
    line); kind "h" indexes rows, "v" columns.
    """
    out = []
    for kind, line, b1, b2, (a0, a1) in segs:
        seg = L[:, a0:a1] if kind == "h" else L[a0:a1].T
        out.append(np.r_[seg[b1[0] : b1[1]], seg[b2[0] : b2[1]]].mean() - seg[line[0] : line[1]].mean())
    return np.array(out)


def band_mask(shape, rect, r=8):
    """The +/- r px band around the border of rect = [y0, y1, x0, x1]."""
    y0, y1, x0, x1 = rect
    m = np.zeros(shape, bool)
    m[y0 - r : y1 + r, x0 - r : x1 + r] = True
    m[y0 + r : y1 - r, x0 + r : x1 - r] = False
    return m


def glint_masks(shape, discs):
    """Union of the disc cores and of the annuli `GLINT_RING` px outside them; a disc is (cy, cx, r)."""
    y, x = np.mgrid[0 : shape[0], 0 : shape[1]]
    core, ring = np.zeros(shape, bool), np.zeros(shape, bool)
    for cy, cx, r in discs:
        d2 = (y - cy) ** 2 + (x - cx) ** 2
        core |= d2 <= r * r
        ring |= (d2 >= (r + GLINT_RING[0]) ** 2) & (d2 <= (r + GLINT_RING[1]) ** 2)
    return core, ring


def border_pull(comp, src):
    """Mean shortfall below the darkest valid source, over pixels with valid and invalid sources."""
    lo = np.full(comp.shape[:2], np.inf)
    nvalid = np.zeros(comp.shape[:2], int)
    for k in range(src.n):
        f, m = src.read(k)
        lo = np.where(m, np.minimum(lo, f.mean(2) * FULL), lo)
        nvalid += m
    sel = (nvalid < src.n) & (nvalid > 0)
    return float(np.maximum(0, lo - comp.astype(np.float64).mean(2))[sel].mean()) if sel.any() else 0.0


def metrics(comp, ref, meta, src=None):
    """Every metric the case's regions define, over pixels with at least one valid source.

    `glint_core` is the composite's mean luma over the glint discs divided by the truth's (1 keeps
    them); `glint_ring` is the luma RMS against the truth in the annuli around them.
    """
    c, r = comp.astype(np.float64), ref.astype(np.float64)
    valid = np.ones(c.shape[:2], bool)
    if src is not None and src.has_masks:
        valid = np.zeros_like(valid)
        for k in range(src.n):
            valid |= src.read(k)[1]
    Lc, Lr = c.mean(2), r.mean(2)
    mse = ((c - r) ** 2)[valid].mean()
    psnr = lambda e: float("inf") if e == 0 else float(10 * np.log10(FULL**2 / e))  # noqa: E731
    m = dict(psnr=psnr(mse))
    # Level-invariant: PSNR after the one global gain that least-squares fits the composite to the truth.
    g = (c * r)[valid].sum() / max((c * c)[valid].sum(), 1e-12)
    m["psnr_lvl"] = psnr(((np.clip(g * c, 0, FULL) - r) ** 2)[valid].mean())
    filled = np.where(valid[..., None], comp, ref)
    m["ssim"] = ss.block_mean_ssim(filled, ref)
    if meta.get("halo"):
        band = band_mask(valid.shape, meta["halo"]) & valid
        m["halo_rms"] = float(np.sqrt(((Lc - Lr)[band] ** 2).mean()))
    if meta.get("flat"):
        y0, y1, x0, x1 = meta["flat"]
        sel = np.zeros_like(valid)
        sel[y0:y1, x0:x1] = True
        m["flat_std"] = float((Lc - Lr)[sel & valid].std())
    ok = (c < 0.98 * FULL).all(2) & (r < 0.98 * FULL).all(2) & (Lc > 1) & (Lr > 1) & valid
    with np.errstate(divide="ignore", invalid="ignore"):
        m["chroma_err"] = float(np.abs(c / Lc[..., None] - r / Lr[..., None])[ok].mean())
    if meta.get("hairs"):
        m["hair_ret"] = float((hair_contrast(Lc, meta["hairs"]) / hair_contrast(Lr, meta["hairs"])).mean())
    if meta.get("glints"):
        core, ring = (k & valid for k in glint_masks(valid.shape, meta["glints"]))
        m["glint_core"] = float(Lc[core].mean() / Lr[core].mean())
        m["glint_ring"] = float(np.sqrt(((Lc - Lr)[ring] ** 2).mean()))
    if src is not None and src.has_masks:
        m["border_pull"] = border_pull(comp, src)
    return m


# ---------------------------------------------------------------- sources


class ArraySource:
    """Frames held in memory as u16 (N, H, W, C), optional validity masks (N, H, W)."""

    def __init__(self, frames, masks=None):
        self.frames, self.masks = frames, masks
        self.n, self.h, self.w, self.c = frames.shape
        self.has_masks = masks is not None
        self.reads = [0] * self.n

    def _pixels(self, k, y0, y1, x0, x1):
        return self.frames[k, y0:y1, x0:x1]

    def read(self, k, rect=None):
        y0, y1, x0, x1 = rect or (0, self.h, 0, self.w)
        self.reads[k] += 1
        f = self._pixels(k, y0, y1, x0, x1).astype(np.float64) / FULL
        m = np.ones(f.shape[:2], bool) if self.masks is None else np.array(self.masks[k, y0:y1, x0:x1])
        return f, m


class TiffSource(ArraySource):
    """Frames read as rectangles straight from TIFF files; masks from a .npy file."""

    def __init__(self, paths, masks_path=None):
        self.paths = paths
        self.infos = [rawtiff.read_info(p) for p in paths]
        i0 = self.infos[0]
        if any((i.width, i.height, i.samples_per_pixel) != (i0.width, i0.height, i0.samples_per_pixel) for i in self.infos):
            raise ValueError("frames differ in size or channel count")
        self.n, self.h, self.w, self.c = len(paths), i0.height, i0.width, i0.samples_per_pixel
        self.masks = np.load(masks_path, mmap_mode="r") if masks_path else None
        self.has_masks = self.masks is not None
        self.reads = [0] * self.n

    def _pixels(self, k, y0, y1, x0, x1):
        buf = rawtiff.read_rect(self.paths[k], x0, y0, x1 - x0, y1 - y0, self.infos[k])
        return np.frombuffer(buf, dtype="<u2").reshape(y1 - y0, x1 - x0, self.c)


class SubSource:
    """A rectangle of another source, so a tile runs the same code as a whole image."""

    def __init__(self, parent, rect):
        self.parent, self.rect = parent, rect
        self.n, self.c = parent.n, parent.c
        self.h, self.w = rect[1] - rect[0], rect[3] - rect[2]

    def read(self, k):
        return self.parent.read(k, self.rect)


def frame_paths(d):
    sub = os.path.join(d, "frames")
    paths = sorted(glob.glob(os.path.join(sub if os.path.isdir(sub) else d, "*.tif")))
    if not paths:
        sys.exit(f"no .tif frames under {d}")
    return paths


def open_source(d):
    masks = os.path.join(d, "masks.npy")
    return TiffSource(frame_paths(d), masks if os.path.exists(masks) else None)


def read_u16(path):
    return TiffSource([path]).read(0)[0] * FULL


def write_u16(path, arr):
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    a = np.clip(np.rint(arr), 0, 65535).astype("<u2")
    rawtiff.write_tiff(path, a.shape[1], a.shape[0], a.tobytes(), bits=16, spp=a.shape[2])


def equalize(d, out_dir, brk=None):
    """Write brightness-equalized copies of the frames of `d` under `out_dir` and return the record.

    One gain per frame, shared by all channels: the median of reference luma over frame luma on
    pixels valid in both whose reference luma lies in the mid-tone band `EQ_BAND`. The reference
    is frame n // 2. The applied gain is clipped to `EQ_BOUND` (a frame with fewer than
    `EQ_MIN_SAMPLES` usable pixels gets gain 1 and is flagged `thin`), multiplies the encoded
    values and saturates at 65535. `brk` names a deliberately broken variant (negative control).
    """
    src = open_source(d)
    ref = 0 if brk == "wrong-reference" else src.n // 2
    fr, mr = src.read(ref)
    lr = fr.mean(2) * FULL
    band = (lr >= EQ_BAND[0] * FULL) & (lr <= EQ_BAND[1] * FULL)
    frames = []
    for k, path in enumerate(src.paths):
        f, m = src.read(k)
        valid = band if brk == "ignore-mask" else band & m & mr
        used = int(valid.sum())
        est = float(np.median(lr[valid] / np.maximum(f.mean(2)[valid] * FULL, 1))) if used >= EQ_MIN_SAMPLES else 1.0
        applied = est if brk == "unbounded-gain" else float(np.clip(est, *EQ_BOUND))
        scaled = f * FULL * applied
        write_u16(os.path.join(out_dir, "frames", os.path.basename(path)), scaled)
        frames.append(dict(frame=os.path.basename(path), estimated=est, applied=applied, clamped=applied != est,
                           thin=used < EQ_MIN_SAMPLES, samples_used=used, samples_saturated=int((np.rint(scaled) > 65535).sum())))
    if src.has_masks:
        shutil.copy(os.path.join(d, "masks.npy"), os.path.join(out_dir, "masks.npy"))
    rec = dict(reference=ref, band=list(EQ_BAND), bound=list(EQ_BOUND), min_samples=EQ_MIN_SAMPLES, brk=brk, frames=frames)
    json.dump(rec, open(os.path.join(out_dir, "equalize.json"), "w"), indent=1)
    return rec


# ---------------------------------------------------------------- prototype


def params(scale):
    """Window sizes at `scale`; 1.0 is the reference setting for a 0.25 MP image."""
    s = lambda x: max(1, int(round(x * scale)))  # noqa: E731
    gr = s(5)
    g = np.exp(-(np.arange(-gr, gr + 1) ** 2) / (2 * max(0.5, 5 * scale) ** 2))
    return dict(r_base=s(45), r_det=s(7), r_box=s(15), gauss=g / g.sum())


def reach(p):
    """Support radius of a weight in px: two nested guide windows plus the saliency footprint."""
    sal = 1 + len(p["gauss"]) // 2
    return dict(base=2 * p["r_base"] + sal, detail=2 * p["r_det"] + sal)


def _box(x, r):
    """Window sum of side 2r+1, clamped at the edges, over axes 0 and 1."""
    for ax in (0, 1):
        n = x.shape[ax]
        c = np.cumsum(x, axis=ax)
        z = np.zeros_like(np.take(c, [0], axis=ax))
        c = np.concatenate([z, c], axis=ax)
        i = np.arange(n)
        x = np.take(c, np.minimum(i + r, n - 1) + 1, axis=ax) - np.take(c, np.maximum(i - r, 0), axis=ax)
    return x


def _sepconv(x, k):
    r = len(k) // 2
    for ax in (0, 1):
        n = x.shape[ax]
        pad = [(0, 0)] * x.ndim
        pad[ax] = (r, r)
        xp = np.pad(x, pad)
        out = 0
        for i, w in enumerate(k):
            sl = [slice(None)] * x.ndim
            sl[ax] = slice(i, i + n)
            out = out + w * xp[tuple(sl)]
        x = out
    return x


def _saliency(L, m, gauss):
    """|3x3 Laplacian| smoothed by a normalised Gaussian; only fully valid footprints count."""
    lp = np.pad(L, 1, mode="edge")
    lap = np.abs(lp[:-2, 1:-1] + lp[2:, 1:-1] + lp[1:-1, :-2] + lp[1:-1, 2:] - 4 * L)
    mp = np.pad(m, 1, mode="edge")
    ok = (m & mp[:-2, 1:-1] & mp[2:, 1:-1] & mp[1:-1, :-2] & mp[1:-1, 2:]).astype(np.float64)
    den = _sepconv(ok, gauss)
    return np.where(den > 1e-9, _sepconv(lap * ok, gauss) / np.maximum(den, 1e-9), 0.0)


def _guided(I, p, r, eps, mf, cnt):
    """Guided filter of `p` by `I`; window statistics count valid pixels only."""
    mean = lambda x: _box(x * mf, r) / cnt  # noqa: E731
    mI, mp = mean(I), mean(p)
    a = (mean(I * p) - mI * mp) / (mean(I * I) - mI * mI + eps)
    return mean(a) * I + mean(mp - a * mI)


def fuse_stack(src, scale=1.0, brk=None, diag=None):
    """Two-scale guided-filter fusion of `src`. Returns (u16 composite, covered mask).

    Pass 1 streams the frames into a running saliency argmax; pass 2 streams them again
    into weighted sums, so state is independent of the frame count. `brk` names a
    deliberately broken variant (negative control). `diag`, if given, receives each
    frame's raw base/detail weight planes.
    """
    p = params(scale)
    h, w, c, n = src.h, src.w, src.c, src.n
    ng = c if brk == "per-channel-guide" else 1
    best = np.full((h, w, ng), -np.inf)
    win = np.full((h, w, ng), -1, np.int32)

    def read(k):
        f, m = src.read(k)
        m = np.ones_like(m) if brk == "ignore-mask" else m
        return f, m, ([f[..., i] for i in range(c)] if ng == c else [f.mean(2)])

    if brk != "uniform-weights":
        for k in range(n):
            _, m, guides = read(k)
            for j, g in enumerate(guides):
                s = np.where(m, _saliency(g, m, p["gauss"]), -np.inf)
                up = s > best[..., j]
                best[..., j][up], win[..., j][up] = s[up], k
    sb, sd = np.zeros((h, w, c)), np.zeros((h, w, c))
    nb, nd = np.zeros((h, w, ng)), np.zeros((h, w, ng))
    anyvalid = np.zeros((h, w), bool)
    for k in range(n):
        f, m, guides = read(k)
        mf = m.astype(np.float64)
        anyvalid |= m
        cnt_box = np.maximum(_box(mf, p["r_box"]), 1)
        base = _box(f * mf[..., None], p["r_box"]) / cnt_box[..., None]
        det = (f - base) * mf[..., None]
        if brk == "uniform-weights":
            wb = wd = mf[..., None]
            pk = mf[..., None]
        else:
            pk = ((win == k) & m[..., None]).astype(np.float64)
            cb, cd = np.maximum(_box(mf, p["r_base"]), 1), np.maximum(_box(mf, p["r_det"]), 1)
            wb = np.stack([np.clip(_guided(g, pk[..., j], p["r_base"], EPS_BASE, mf, cb), 0, 1) for j, g in enumerate(guides)], 2)
            wd = np.stack([np.clip(_guided(g, pk[..., j], p["r_det"], EPS_DET, mf, cd), 0, 1) for j, g in enumerate(guides)], 2)
            wb, wd = wb * mf[..., None], wd * mf[..., None]
            if diag is not None:
                diag.setdefault("wb", {})[k], diag.setdefault("wd", {})[k] = wb[..., 0].copy(), wd[..., 0].copy()
        sb += (wb + 1e-6 * pk) * base
        sd += (wd + 1e-6 * pk) * det
        nb += wb + 1e-6 * pk
        nd += wd + 1e-6 * pk
    covered = anyvalid if brk == "uniform-weights" else (win >= 0).any(2)
    out = (sb / np.maximum(nb, 1e-12) + sd / np.maximum(nd, 1e-12)) * FULL
    out = np.clip(np.rint(out), 0, 65535).astype(np.uint16)
    out[~covered] = 0
    return out, covered


def fuse_tiled(src, tile, margin, scale=1.0, brk=None):
    """Whole-image result assembled from tiles of `tile` px read with `margin` px of context."""
    out = np.zeros((src.h, src.w, src.c), np.uint16)
    covered = np.zeros((src.h, src.w), bool)
    for y0 in range(0, src.h, tile):
        for x0 in range(0, src.w, tile):
            y1, x1 = min(y0 + tile, src.h), min(x0 + tile, src.w)
            ry0, ry1 = max(y0 - margin, 0), min(y1 + margin, src.h)
            rx0, rx1 = max(x0 - margin, 0), min(x1 + margin, src.w)
            o, cv = fuse_stack(SubSource(src, (ry0, ry1, rx0, rx1)), scale, brk)
            out[y0:y1, x0:x1] = o[y0 - ry0 : y1 - ry0, x0 - rx0 : x1 - rx0]
            covered[y0:y1, x0:x1] = cv[y0 - ry0 : y1 - ry0, x0 - rx0 : x1 - rx0]
    return out, covered


# ---------------------------------------------------------------- self-check

# Thresholds sit between the honest value and the value of the variant that must trip
# them, measured at SELF_SCALE on the stacks below.
SELF_SCALE = 0.03
SELF = dict(ssim_min=0.98, hair_min=0.9, chroma_max=0.0105, pull_max=100.0, glint_min=0.9, ring_max=800.0)
SELF_HAIRS = [("h", (24, 25), (18, 20), (28, 30), (22, 30)), ("h", (24, 25), (18, 20), (28, 30), (36, 42)),
              ("v", (32, 34), (26, 28), (38, 40), (8, 18)), ("v", (32, 34), (26, 28), (38, 40), (28, 38))]


SELF_GLINTS = [(12, 27, 2), (36, 27, 2)]


def tiny_stack(kind):
    """48x64, N=3, depth bands by column, thin hairs and one invalid strip.

    `plain`: noise-free, for the blur and invalid-source detectors. `tint`: a flat
    noisy third and per-frame gain/tint, where winners can disagree between channels.
    `glint`: `plain` with two saturated discs at the depth of the middle frame.
    """
    h, w, n = 48, 64, 3
    rng = np.random.default_rng(5)
    tex = ss.blur(rng.normal(0, 1, (h, w, 3)), 1.0)
    src = (tex - tex.min()) / (tex.max() - tex.min()) * 30000 + 15000
    if kind == "tint":
        src[:, 43:] = np.array([28000, 30000, 32000])
    src[24, 4:60] -= 12000
    src[6:42, 32:34] -= 12000
    if kind == "glint":
        src[glint_masks((h, w), SELF_GLINTS)[0]] = 65535
    depth = np.where(np.arange(w) < 21, 0, np.where(np.arange(w) < 43, 1, 2))[None, :].repeat(h, 0)
    bank = {s: ss.blur(src, s) for s in (0.0, 1.5, 3.0)}
    gain, noise = (np.array([1.15, 0.85, 1.0]), 100.0) if kind == "tint" else (np.ones(n), 0.0)
    tint = (1 + 0.01 * np.array([[1, 0, -1], [-0.5, 0.8, 0], [0, -0.7, 1]]) * (kind == "tint")) * gain[:, None]
    frames = []
    for k in range(n):
        sig = 1.5 * np.abs(depth - k)
        f = np.zeros_like(src)
        for s, b in bank.items():
            f = np.where((sig == s)[..., None], b, f)
        frames.append(np.clip(np.rint(f * tint[k] + rng.normal(0, noise, f.shape)), 0, 65535).astype(np.uint16))
    frames = np.stack(frames)
    masks = np.ones((n, h, w), bool)
    frames[1, :, 0:6] = 0
    masks[1, :, 0:6] = False
    ref = np.clip(np.rint(src), 0, 65535).astype(np.uint16)
    return ArraySource(frames, masks), ref, dict(hairs=SELF_HAIRS, glints=SELF_GLINTS if kind == "glint" else None)


# Correction factors that bring each frame of `eq_stack` to the reference frame (index 2).
EQ_TRUTH = [1.1, 1.4, 1.0, 1.2, 0.9]
EQ_GAIN_TOL = 0.005
EQ_PATCH = (slice(4, 12), slice(50, 58))


def eq_stack(d):
    """48x64, N=5 smooth mid-tone texture; frame k is the reference divided by EQ_TRUTH[k].

    Frame 0 has 40 invalid columns (62 % of the frame) holding the constant 18000, a
    mid-tone value, with its mask False. Frame 3 holds a near-white patch the reference
    does not have, which saturates when the frame is scaled up.
    """
    rng = np.random.default_rng(7)
    tex = ss.blur(rng.normal(0, 1, (48, 64, 3)), 1.0)
    base = (tex - tex.min()) / (tex.max() - tex.min()) * 30000 + 15000
    frames, masks = [], np.ones((5, 48, 64), bool)
    for k, g in enumerate(EQ_TRUTH):
        f = np.rint(base / g)
        if k == 0:
            f[:, :40], masks[0, :, :40] = 18000, False
        if k == 3:
            f[EQ_PATCH] = 62000
        write_u16(os.path.join(d, "frames", f"{k:02d}.tif"), f)
    np.save(os.path.join(d, "masks.npy"), masks)


def self_check_equalizer():
    """The honest equalizer recovers the known gains; each broken variant must trip its detector."""
    lo, hi = EQ_BOUND
    ok = True
    for brk, expect in [(None, set()), ("wrong-reference", {"gain_err"}), ("ignore-mask", {"gain_err"}),
                        ("unbounded-gain", {"bound"})]:
        with tempfile.TemporaryDirectory() as tmp:
            eq_stack(tmp)
            rec = equalize(tmp, os.path.join(tmp, "out"), brk)
            est = np.array([f["estimated"] for f in rec["frames"]])
            app = np.array([f["applied"] for f in rec["frames"]])
            patch = read_u16(os.path.join(tmp, "out", "frames", "03.tif"))[EQ_PATCH]
            got = {k for k, v in dict(
                gain_err=np.abs(est - EQ_TRUTH).max() > EQ_GAIN_TOL,
                bound=(app < lo - 1e-9).any() or (app > hi + 1e-9).any(),
                clamp=not (rec["frames"][1]["clamped"] and abs(app[1] - hi) < 1e-9),
                wrap=bool((patch != 65535).any()) or rec["frames"][3]["samples_saturated"] == 0).items() if v}
            good = got == expect if brk is None else expect <= got
            ok &= good
            print(f"eq    {brk or 'honest':18} {'ok  ' if good else 'FAIL'} fired={sorted(got)} expected={sorted(expect)} "
                  f"max gain error={np.abs(est - EQ_TRUTH).max():.4f} applied={np.round(app, 3).tolist()}")
    return ok


def self_check_level():
    """A composite that is an exact scalar multiple of the truth scores high on `psnr_lvl`, low on `psnr`."""
    ref = tiny_stack("plain")[1]
    m = metrics(np.rint(ref * 0.9).astype(np.uint16), ref, {})
    ok = m["psnr_lvl"] > 80 and m["psnr"] < 35
    print(f"level {'ok  ' if ok else 'FAIL'} scalar multiple 0.9: psnr_lvl={m['psnr_lvl']:.2f} psnr={m['psnr']:.2f}")
    return ok


def self_check_commands():
    """Baselines run on registered input unless asked; `--set` pairs are forwarded; coverage masks follow the transforms; every case has an in-frame crop."""
    base = stack_cmd("d", "laplacian", "448", "o")
    fwd = stack_cmd("d", "laplacian", "448", "o", sets=["levels=3", "equalize=true"])
    cov = stack_cmd("raw", "depth-map", "1024", "o", align=True)
    shift = [dict(transform=dict(m=[[1, 0, 10], [0, 1, 0], [0, 0, 1]]))]
    ident = [dict(transform=dict(m=[[1, 0, 0], [0, 1, 0], [0, 0, 1]]))]
    mk = product_masks(shift + ident, 48, 64)
    checks = dict(
        registered="--no-align" in base and "--set" not in base and "--no-gpu" in base,
        forwarded=[fwd[i + 1] for i, c in enumerate(fwd) if c == "--set"] == ["levels=3", "equalize=true"],
        coverage="--no-align" not in cov and "pyramidal-ecc" in cov and "--no-crop" in cov and "--no-gpu" in cov,
        masks=bool(mk[1].all()) and int((~mk[0]).sum()) == 10 * 48 and bool(mk[0][:, :54].all()),
        crops=all(0 <= y0 < y1 <= 512 and 0 <= x0 < x1 <= 512 for y0, y1, x0, x1 in map(crop_rect, [*CASES, "align-border"])))
    for k, v in checks.items():
        print(f"cmd   {k:18} {'ok  ' if v else 'FAIL'}")
    return all(checks.values())


def self_check():
    """Every detector must stay silent on the honest prototype and fire on its broken variant."""
    fires = lambda m: {  # noqa: E731
        k for k, v in dict(
            blur_ssim=m["ssim"] < SELF["ssim_min"], blur_hair=m["hair_ret"] < SELF["hair_min"],
            chroma=m["chroma_err"] > SELF["chroma_max"], pull=m["border_pull"] > SELF["pull_max"],
            glint_core=m.get("glint_core", 1.0) < SELF["glint_min"], glint_ring=m.get("glint_ring", 0.0) > SELF["ring_max"]).items() if v}
    # (stack kind, variant, detectors that count, detectors that must fire; the honest run fires none)
    plan = [("plain", None, {"blur_ssim", "blur_hair", "pull"}, set()),
            ("plain", "uniform-weights", {"blur_ssim", "blur_hair", "pull"}, {"blur_ssim", "blur_hair"}),
            ("plain", "ignore-mask", {"blur_ssim", "blur_hair", "pull"}, {"pull"}),
            ("tint", None, {"chroma"}, set()),
            ("tint", "per-channel-guide", {"chroma"}, {"chroma"}),
            ("glint", None, {"glint_core", "glint_ring"}, set()),
            ("glint", "uniform-weights", {"glint_core", "glint_ring"}, {"glint_core", "glint_ring"})]
    ok = True
    for kind, brk, counted, expect in plan:
        src, ref, meta = tiny_stack(kind)
        comp, _ = fuse_stack(src, scale=SELF_SCALE, brk=brk)
        m = metrics(comp, ref, meta, src)
        got = fires(m) & counted
        good = got == expect if brk is None else expect <= got
        ok &= good
        print(f"{kind:5} {brk or 'honest':18} {'ok  ' if good else 'FAIL'} fired={sorted(got)} expected={sorted(expect)} "
              + " ".join(f"{k}={v:.4f}" for k, v in m.items()))
    # Identical frames must come back unchanged whatever the weights are.
    edge = np.full((48, 64, 3), 10000, np.int64)
    edge[:, 32:] = 50000
    for name, img in (("constant", np.full((48, 64, 3), 30000, np.int64)), ("edge", edge)):
        comp, _ = fuse_stack(ArraySource(np.stack([img] * 3).astype(np.uint16)), scale=SELF_SCALE)
        dev = int(np.abs(comp.astype(np.int64) - img).max())
        ok &= dev <= 1
        print(f"{name:5} identical frames  {'ok  ' if dev <= 1 else 'FAIL'} max deviation {dev} counts")
    ok &= self_check_equalizer()
    ok &= self_check_level()
    ok &= self_check_commands()
    print("self-check", "passed" if ok else "FAILED")
    return 0 if ok else 1


# ---------------------------------------------------------------- cases

CASES = {"halo": ("hard", 250), "flat": ("hard", 1000), "easy": ("easy", 250),
         "hairs": ("hard", 250), "glints": ("hard", 250), "tint": ("hard", 250), "border": ("hard", 250),
         "gain-ramp": ("hard", 250), "gain-offset": ("hard", 250), "flicker": ("hard", 250)}
DEFAULT_CASES = "halo,flat,easy,hairs,tint,border"
HAIRS_512 = [("h", (460, 461), (454, 456), (464, 466), (30, 100)), ("h", (460, 461), (454, 456), (464, 466), (130, 190)),
             ("v", (110, 112), (104, 106), (116, 118), (415, 450)), ("v", (110, 112), (104, 106), (116, 118), (470, 495))]
STRIPS = {2: (slice(None), slice(0, 30)), 5: (slice(0, 20), slice(None)), 6: (slice(None), slice(482, 512))}


def run(cmd):
    p = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if p.returncode:
        sys.exit(f"{' '.join(map(str, cmd))} failed (exit {p.returncode})\n{p.stdout[-400:]}\n{p.stderr[-800:]}")
    return p


def stack_cmd(d, algo, tile, out, sets=(), align=False):
    """The `stack` command line of a baseline: registered input and CPU only.

    `align` runs the product's own alignment instead (rotation-scale, uncropped, so coverage
    applies at the borders); `sets` are `KEY=VAL` pairs for `--set`.
    """
    cmd = [CLI, "stack", d, "--algo", algo]
    cmd += ["--align-algo", "pyramidal-ecc", "--align-set", "mode=rotation-scale"] if align else ["--no-align"]
    cmd += ["--no-crop", "--no-gpu", "--tile-size", tile, "--json", "-o", out]
    return cmd + [x for kv in sets for x in ("--set", kv)]


def synth(scene, noise, seed, n, out):
    run([sys.executable, SYNTH, "--frames-only", "--scene", scene, "--frames", n, "--noise", noise, "--seed", seed, "--out", out])


def transform_mask(m, h, w):
    """Destination pixels whose centre maps (dst to src, `m`) inside 0..=dim-1 of the source frame."""
    y, x = np.mgrid[0:h, 0:w].astype(np.float64)
    d = m[2][0] * x + m[2][1] * y + m[2][2]
    with np.errstate(divide="ignore", invalid="ignore"):
        ux, uy = (m[0][0] * x + m[0][1] * y + m[0][2]) / d, (m[1][0] * x + m[1][1] * y + m[1][2]) / d
    return (d > 1e-6) & (ux >= 0) & (ux <= w - 1) & (uy >= 0) & (uy <= h - 1)


def product_masks(entries, h, w):
    """Per-frame coverage masks of a report's `alignments` entries."""
    return np.stack([transform_mask(e["transform"]["m"], h, w) for e in entries])


# Rectangle [y0, y1, x0, x1] of each case's class crop; `halo`'s covers its +/-8 px band.
HALO_CROP = [72, 208, 292, 478]
CROPS = {"halo": HALO_CROP, "tint": HALO_CROP, "gain-ramp": HALO_CROP, "gain-offset": HALO_CROP, "flicker": HALO_CROP,
         "flat": [432, 508, 4, 80], "hairs": [400, 500, 20, 200], "glints": [206, 254, 16, 156],
         "border": [0, 128, 0, 128], "align-border": [0, 128, 0, 128]}


def crop_rect(name):
    return CROPS.get(name, [0, 256, 0, 256])


def drift(name, seed, n):
    """Per-frame (gain, offset in counts) of a brightness-drift case; frame n // 2 stays at (1, 0).

    `gain-ramp`: gain linear in the frame index, 0.92 at frame 0. `gain-offset`: the same ramp
    plus an offset linear in the index (-300 at frame 0). `flicker`: alternating 1 +/- 0.06 with
    a 0.01 jitter, frame 0 a dip to 0.72, so its correction (1.39) exceeds the equalizer bound.
    """
    m = n // 2
    t = (np.arange(n) - m) / max(m, n - 1 - m)
    if name == "flicker":
        gain = 1 + 0.06 * (-1.0) ** np.arange(n) + np.random.default_rng(seed + 100).uniform(-0.01, 0.01, n)
        gain[0] = 0.72
    else:
        gain = 1 + 0.08 * t
    offset = 300.0 * t if name == "gain-offset" else np.zeros(n)
    gain[m], offset[m] = 1.0, 0.0
    return gain, offset


def build_case(name, seed, n, root):
    """Write one synthetic case under root and return its metadata."""
    d = os.path.join(root, f"{name}-s{seed}")
    h = w = 512
    meta = dict(case=name, seed=seed, dir=d, frames=n, reference=os.path.join(d, "reference.tif"),
                halo=[80, 200, 300, 470], flat=[440, 500, 12, 72])
    shutil.rmtree(d, ignore_errors=True)
    if name == "align-border":
        raw = os.path.join(d, "raw")
        run([sys.executable, SYNTH, "--warp", 4, "--frames-only", "--scene", "hard", "--frames", n, "--noise", 250, "--seed", seed, "--out", raw])
        os.makedirs(os.path.join(d, "frames"))
        rep = json.loads(run([CLI, "align", raw, "--json", "--no-gpu", "--algo", "pyramidal-ecc", "--set", "mode=rotation-scale",
                              "--write-warped", os.path.join(d, "frames")]).stdout)
        masks = product_masks(rep["alignments"], h, w)
        zero = [bool((read_u16(os.path.join(d, "frames", e["name"])) == 0).all(2)[~mk].all()) for e, mk in zip(rep["alignments"], masks)]
        meta["mask_check"] = dict(invalid_fraction=float(1 - masks.mean()), invalid_pixels_all_zero=all(zero))
        np.save(os.path.join(d, "masks.npy"), masks)
        write_u16(meta["reference"], ss.SCENES["hard"]())
        json.dump(rep, open(os.path.join(d, "align.json"), "w"))
    else:
        scene, noise = CASES[name]
        meta.update(scene=scene, noise=noise)
        if name == "hairs":
            src = ss.SCENES["hard"]()
            depth = ss.depth_map(n, "standard")
            src[460, 20:200] -= 14000
            src[405:500, 110:112] -= 14000
            depth[452:468, 20:200] = 1.5
            depth[405:500, 104:118] = 5.5
            for k, f in enumerate(ss.render(src, depth, 0.45, noise, seed, n)):
                write_u16(os.path.join(d, "frames", f"{k:02d}.tif"), f)
            write_u16(meta["reference"], src)
            meta["hairs"] = HAIRS_512
        elif name == "glints":
            src = ss.SCENES["hard"]()
            depth = ss.depth_map(n, "standard")
            meta["glints"] = [[cy, cx, GLINT_R] for cy in (230, 290, 350) for cx in (40, 130, 220, 330)]
            y, x = np.mgrid[0:h, 0:w]
            for cy, cx, r in meta["glints"]:
                d2 = (y - cy) ** 2 + (x - cx) ** 2
                src[d2 <= r * r] = 65535
                depth[d2 <= GLINT_DEPTH_R**2] = GLINT_DEPTH
            for k, f in enumerate(ss.render(src, depth, 0.45, noise, seed, n)):
                write_u16(os.path.join(d, "frames", f"{k:02d}.tif"), f)
            write_u16(meta["reference"], src)
        else:
            synth(scene, noise, seed, n, d)
        paths = frame_paths(d)
        if name == "tint":
            rng = np.random.default_rng(seed + 100)
            for p in paths[:n]:
                write_u16(p, read_u16(p) * ((1 + rng.uniform(-0.05, 0.05)) * (1 + rng.uniform(-0.03, 0.03, 3))))
        if name in ("gain-ramp", "gain-offset", "flicker"):
            gain, offset = drift(name, seed, n)
            meta["drift"] = dict(gain=gain.tolist(), offset=offset.tolist())
            for p, g, o in zip(paths[:n], gain, offset):
                write_u16(p, read_u16(p) * g + o)
        if name == "border":
            masks = np.ones((n, h, w), bool)
            for k, (ys, xs) in STRIPS.items():
                a = read_u16(paths[k])
                a[ys, xs] = 0
                write_u16(paths[k], a)
                masks[k][ys, xs] = False
            np.save(os.path.join(d, "masks.npy"), masks)
    json.dump(meta, open(os.path.join(d, "case.json"), "w"), indent=1)
    return meta


def real_cases(names, root, rect, nframes):
    """Cases without truth: the committed mantis fixture and a crop of the reference stack."""
    out = []
    if "mantis" in names:
        out.append(dict(case="mantis", seed=0, dir=os.path.join(ROOT, "fixtures", "mantis-512"), reference=None))
    if "stack-crop" in names:
        top = os.environ.get("FOCUSFORGE_REFERENCE_STACK")
        if not top or not os.path.isdir(top):
            sys.exit("stack-crop needs FOCUSFORGE_REFERENCE_STACK set to the reference stack directory")
        files = sorted(f for f in glob.glob(os.path.join(top, "*.tif")) if "Stacked" not in os.path.basename(f))
        pick = [files[int(round(i))] for i in np.linspace(0, len(files) - 1, nframes)]
        x, y, cw, ch = rect
        d = os.path.join(root, "stack-crop")
        shutil.rmtree(d, ignore_errors=True)
        for k, f in enumerate(pick):
            info = rawtiff.read_info(f)
            buf = rawtiff.read_rect(f, x, y, cw, ch, info)
            write_u16(os.path.join(d, "frames", f"{k:02d}.tif"), np.frombuffer(buf, "<u2").reshape(ch, cw, info.samples_per_pixel))
        out.append(dict(case="stack-crop", seed=0, dir=d, reference=None, rect=list(rect),
                        frame_files=[os.path.basename(f) for f in pick]))
        json.dump(out[-1], open(os.path.join(d, "case.json"), "w"), indent=1)
    return out


def run_baseline(d, algo, tile, work, sets=(), align=False):
    """`stack` then `export` on the CPU path; peak RSS from the time wrapper.

    The row carries the report's `equalization` when `--set equalize=true` ran and, with `align`,
    the report's `align` block under `align`.
    """
    o = os.path.join(work, f"{algo}-t{tile}")
    shutil.rmtree(o, ignore_errors=True)
    os.makedirs(o)
    cmd = ["/usr/bin/time", "-v"] + stack_cmd(d, algo, tile, o, sets, align)
    p = run(cmd)
    rss = int(next(ln for ln in p.stderr.splitlines() if "Maximum resident set size" in ln).split()[-1])
    rep = json.loads(p.stdout)
    run([CLI, "export", rep["outputs"][0], "-o", f"{o}.tif", "--json"])
    row = dict(comp=f"{o}.tif", rss_kb=rss, wall_s=rep["wall_seconds"], tiles=rep["tiles"],
               reads_per_frame=rep["frame_tiles_read"] / rep["frame_count"], accounted_bytes=rep["accounted_bytes_in_flight"],
               cmd=" ".join(str(c) for c in cmd[2:]))
    if rep["equalization"]:
        row["equalization"] = rep["equalization"]
    if align:
        row["align"], row["project"] = rep["align"], rep["outputs"][0]
    return row


def aggregate(rows):
    g = {}
    for r in rows:
        for k, v in r["metrics"].items():
            g.setdefault((r["case"], k, r["algo"], r["arm"]), []).append(v)
    lines = ["case metric algo arm | per seed | mean spread(max-min) sd"]
    for (case, k, algo, arm), v in sorted(g.items()):
        v = np.array(v)
        lines.append(f"{case:11} {k:11} {algo:17} {arm:3} | " + " ".join(f"{x:10.4f}" for x in v)
                     + f" | {v.mean():10.4f} {v.max() - v.min():9.4f} {v.std(ddof=1) if len(v) > 1 else 0:9.4f}")
    return "\n".join(lines)


def aggregate_delta(rows):
    """ON minus OFF per case, metric and method over the seeds, then the composite differences."""
    key = lambda r: (r["case"], r["seed"], r["algo"])  # noqa: E731
    off = {key(r): r for r in rows if r["arm"] == "off"}
    pairs = [(off[key(r)], r) for r in rows if r["arm"] == "on" and key(r) in off]
    g = {}
    for o, n in pairs:
        for k, v in n.get("metrics", {}).items():
            g.setdefault((n["case"], k, n["algo"]), []).append(v - o["metrics"][k])
    lines = ["case metric algo | on minus off per seed | mean"]
    for (case, k, algo), v in sorted(g.items()):
        lines.append(f"{case:11} {k:11} {algo:17} | " + " ".join(f"{x:+10.4f}" for x in v) + f" | {np.mean(v):+10.4f}")
    lines.append("case seed algo | max abs sample difference, differing samples, mean abs / OFF mean (ON vs OFF composite)")
    lines += [f"{n['case']:11} {n['seed']:3} {n['algo']:17} | {n['vs_off']['max_abs']:6} {n['vs_off']['differing']:9} "
              f"{n['vs_off']['rel_mean_abs']:.5f}" for _, n in pairs]
    return "\n".join(lines)


def product_coverage(meta, align, project, out):
    """Source of the product's own alignment run: its frames as fused (`export --frame`) and the
    coverage masks of the same run's transforms, plus the largest difference of those transforms from
    the ones the case's own `align` pass recorded (the two passes need not agree).
    """
    entries = align["alignments"]
    os.makedirs(out, exist_ok=True)
    for k in range(len(entries)):
        run([CLI, "export", project, "--frame", k, "--frame-output", os.path.join(out, f"{k:02d}.tif"), "--json"])
    ref = json.load(open(os.path.join(meta["dir"], "align.json")))["alignments"]
    diff = max(abs(np.array(e["transform"]["m"]) - np.array(r["transform"]["m"])).max() for e, r in zip(entries, ref))
    src = TiffSource(frame_paths(out))
    src.masks, src.has_masks = product_masks(entries, src.h, src.w), True
    return src, float(diff)


def cmd_cases(a):
    os.makedirs(a.out, exist_ok=True)
    names = a.cases.split(",")
    seeds = [int(x) for x in a.seeds.split(",")]
    metas = [build_case(c, s, a.frames, a.out) for c in names if c in CASES or c == "align-border" for s in seeds]
    metas += real_cases(names, a.out, tuple(int(x) for x in a.rect.split(",")), a.crop_frames)
    arms = ["off", "on"] if a.equalize == "both" else [a.equalize]
    rows = []
    for meta in metas:
        work = meta["dir"] if meta["reference"] else os.path.join(a.out, meta["case"])
        os.makedirs(work, exist_ok=True)
        comps, case_rows = {}, []
        cover = a.product_coverage and meta["case"] == "align-border"
        for arm in arms:
            d, awork, tag, sets = meta["dir"], work, "", list(a.stack_set)
            if arm == "on":
                d = awork = os.path.join(work, "equalized")
                equalize(meta["dir"], d)
                tag = "-eq"
            if arm == "product":
                sets.append("equalize=true")
            src = open_source(d)
            arm_rows = []
            for algo in a.methods.split(","):
                b = run_baseline(os.path.join(d, "raw") if cover else d, algo, a.tile_size, awork, sets, cover)
                if cover:
                    align = b.pop("align")
                    src, align_diff = product_coverage(meta, align, b.pop("project"), os.path.join(awork, f"{algo}-frames"))
                    b.update(coverage="product", align_reference=align["reference"], align_vs_align_command=align_diff)
                comps[algo + tag] = b.pop("comp")
                arm_rows.append(dict(case=meta["case"], seed=meta["seed"], algo=algo, arm=arm, tile=a.tile_size, **b))
                if sets:
                    arm_rows[-1]["stack_set"] = sets
            if a.candidate:
                t0 = time.time()
                out, cov = fuse_stack(src, a.scale, a.brk)
                comps["gff" + tag] = os.path.join(awork, "gff.tif")
                write_u16(comps["gff" + tag], out)
                arm_rows.append(dict(case=meta["case"], seed=meta["seed"], algo="gff", arm=arm, scale=a.scale, brk=a.brk,
                                     uncovered=int((~cov).sum()), wall_s=round(time.time() - t0, 3)))
            for r in arm_rows:
                r["comp"] = comps[r["algo"] + tag]
                if arm == "on":
                    r["equalize"] = os.path.join(d, "equalize.json")
                if meta["reference"]:
                    r["metrics"] = metrics(read_u16(r["comp"]).astype(np.uint16), read_u16(meta["reference"]).astype(np.uint16), meta, src)
                if arm == "on" and "off" in arms:
                    off = read_u16(comps[r["algo"]])
                    diff = np.rint(np.abs(read_u16(r["comp"]) - off))
                    r["vs_off"] = dict(max_abs=int(diff.max()), differing=int((diff > 0).sum()),
                                       rel_mean_abs=float(diff.mean() / max(off.mean(), 1)))
                print(json.dumps(r), flush=True)
            case_rows += arm_rows
        rows += case_rows
        if meta["reference"]:
            rect, files, order = crop_rect(meta["case"]), [meta["reference"], *comps.values()], ["truth", *comps]
        else:
            rect = (0, min(src.h, 256), 0, min(src.w, 256)) if src.h < 700 else (src.h // 2 - 128, src.h // 2 + 128, src.w // 2 - 128, src.w // 2 + 128)
            files, order = list(comps.values()), list(comps)
        png = os.path.join(work, "montage.png")
        montage(png, rect, files)
        print(json.dumps(dict(case=meta["case"], montage=png, order=order, rect=list(rect))), flush=True)
    json.dump(rows, open(os.path.join(a.out, "results.json"), "w"), indent=1)
    print("# aggregate\n" + aggregate([r for r in rows if r.get("metrics")]))
    if "on" in arms and "off" in arms:
        print("# on minus off\n" + aggregate_delta(rows))


# ---------------------------------------------------------------- fuse / score / montage


def montage(path, rect, files):
    """Side-by-side 8-bit crops (contrast taken from the first image) written as PNG."""
    y0, y1, x0, x1 = rect
    tiles = [TiffSource([f]).read(0, rect)[0] for f in files]
    lo, hi = np.percentile(tiles[0], [0.5, 99.5])
    img8 = [np.clip((t - lo) / max(hi - lo, 1e-9), 0, 1) * 255 for t in tiles]
    gap = np.full((y1 - y0, 4, img8[0].shape[2]), 255.0)
    row = np.concatenate([x for t in img8 for x in (t, gap)][:-1], axis=1).astype(np.uint8)
    if row.shape[2] == 1:
        row = np.repeat(row, 3, axis=2)
    raw = b"".join(b"\0" + r.tobytes() for r in row)
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))  # noqa: E731
    with open(path, "wb") as fh:
        fh.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", row.shape[1], row.shape[0], 8, 2, 0, 0, 0))
                 + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def cmd_fuse(a):
    rss0 = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    src = open_source(a.dir)
    p = params(a.scale)
    t0 = time.time()
    if a.tile:
        margin = a.margin if a.margin is not None else reach(p)["base"]
        out, cov = fuse_tiled(src, a.tile, margin, a.scale, a.brk)
    else:
        margin = None
        out, cov = fuse_stack(src, a.scale, a.brk)
    wall = time.time() - t0
    write_u16(a.out, out)
    ng = src.c if a.brk == "per-channel-guide" else 1
    print(json.dumps(dict(cmd=sys.argv, out=a.out, frames=src.n, size=[src.w, src.h], channels=src.c, scale=a.scale,
                          radii=dict(base=p["r_base"], detail=p["r_det"], box=p["r_box"], gauss=len(p["gauss"]) // 2), reasoned_reach=reach(p),
                          tile=a.tile, margin=margin, brk=a.brk, reads_per_frame=sorted(set(src.reads)),
                          planes=dict(pass1=2 * ng, pass2=2 * src.c + 2 * ng), uncovered=int((~cov).sum()), wall_s=round(wall, 3),
                          rss_start_kb=rss0, rss_peak_kb=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)))


def cmd_score(a):
    meta = json.load(open(os.path.join(a.case, "case.json")))
    if not meta.get("reference"):
        sys.exit(f"{a.case} has no reference: judge it with `montage`")
    src = open_source(meta["dir"])
    print(json.dumps(dict(cmd=sys.argv, case=meta["case"], seed=meta["seed"], comp=a.comp,
                          metrics=metrics(read_u16(a.comp).astype(np.uint16), read_u16(meta["reference"]).astype(np.uint16), meta, src))))


def cmd_montage(a):
    rect = tuple(int(x) for x in a.rect.split(","))
    montage(a.out, rect, a.files)
    print(json.dumps(dict(cmd=sys.argv, out=a.out, order=a.files, rect=list(rect))))


# ---------------------------------------------------------------- reach / scale


def cmd_reach(a):
    """Impulse in the second of two constant frames; the farthest pixel where its weight moves."""
    n = a.size
    fr = np.full((2, n, n, 3), 32768, np.uint16)
    fr[1, n // 2, n // 2] = 65535
    diag = {}
    fuse_stack(ArraySource(fr), a.scale, diag=diag)
    c = n // 2
    res = {}
    for layer in ("wb", "wd"):
        res[layer] = {}
        for thr in (1e-3, 1e-4, 1e-6, 1e-9):
            ys, xs = np.nonzero(diag[layer][1] > thr)
            res[layer][f"{thr:g}"] = int(max(np.abs(ys - c).max(), np.abs(xs - c).max())) if ys.size else None
    print(json.dumps(dict(cmd=sys.argv, size=n, scale=a.scale, reasoned_reach=reach(params(a.scale)), farthest_px_by_weight_threshold=res)))


def cmd_scale(a):
    os.makedirs(a.out, exist_ok=True)
    rows = []
    tiles = a.tile_sizes.split(",")
    for n in [int(x) for x in a.ns.split(",")]:
        d = os.path.join(a.out, f"n{n}")
        shutil.rmtree(d, ignore_errors=True)
        synth("hard", 250, 11, n, d)
        t = json.loads(run([sys.executable, os.path.abspath(__file__), "fuse", d, "-o", os.path.join(d, "gff.tif")]).stdout)
        part = [dict(n=n, method="gff-prototype", wall_s=t["wall_s"], rss_peak_kb=t["rss_peak_kb"], rss_start_kb=t["rss_start_kb"],
                     reads_per_frame=t["reads_per_frame"][0], planes=t["planes"])]
        for tile in tiles:
            for algo in ALGOS:
                b = run_baseline(d, algo, tile, d)
                part.append(dict(n=n, method=algo, tile=tile, wall_s=b["wall_s"], rss_peak_kb=b["rss_kb"], reads_per_frame=b["reads_per_frame"],
                                 tiles=b["tiles"], accounted_bytes=b["accounted_bytes"]))
        print(json.dumps(part), flush=True)
        rows += part
        shutil.rmtree(d)
    json.dump(rows, open(os.path.join(a.out, "scale.json"), "w"), indent=1)


# ---------------------------------------------------------------- command line


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--self-check", action="store_true", help="run every detector against the honest prototype and its broken variants")
    sub = p.add_subparsers(dest="cmd")
    c = sub.add_parser("cases", help="build cases, run the baselines (and with --candidate the prototype), score them")
    c.add_argument("--out", required=True)
    c.add_argument("--cases", default=DEFAULT_CASES, help=f"{','.join(CASES)},align-border,mantis,stack-crop")
    c.add_argument("--equalize", choices=["off", "on", "both", "product"], default="off",
                   help="per-frame brightness equalization before fusion; `product` is the product's own (`stack --set equalize=true`) on the unequalized frames")
    c.add_argument("--product-coverage", action="store_true",
                   help="align-border: stack the raw frames with the product's alignment, masks from that run's own transforms")
    c.add_argument("--stack-set", action="append", default=[], metavar="KEY=VAL", help="forwarded to every baseline as --set (repeatable)")
    c.add_argument("--seeds", default="11,12,13")
    c.add_argument("--frames", type=int, default=8)
    c.add_argument("--methods", default=",".join(ALGOS))
    c.add_argument("--tile-size", default="1024", help="read-tile size for the baselines; a tile covering the whole image makes the run a one-tile control")
    c.add_argument("--candidate", action="store_true")
    c.add_argument("--scale", type=float, default=1.0)
    c.add_argument("--break", dest="brk", choices=BREAKS)
    c.add_argument("--rect", default="0,0,1024,1024", help="X,Y,W,H of the stack-crop case")
    c.add_argument("--crop-frames", type=int, default=8)
    f = sub.add_parser("fuse", help="guided-filter fusion of a frame directory")
    f.add_argument("dir")
    f.add_argument("-o", "--out", required=True)
    f.add_argument("--scale", type=float, default=1.0, help="radius factor; 1.0 is set for a 0.25 MP image")
    f.add_argument("--tile", type=int)
    f.add_argument("--margin", type=int, help="px of context per tile side (default: the reasoned base reach)")
    f.add_argument("--break", dest="brk", choices=BREAKS)
    s = sub.add_parser("score", help="metrics of a composite against a case's reference")
    s.add_argument("case")
    s.add_argument("comp")
    m = sub.add_parser("montage", help="side-by-side 8-bit crop as PNG")
    m.add_argument("out")
    m.add_argument("--rect", required=True, help="Y0,Y1,X0,X1")
    m.add_argument("files", nargs="+")
    r = sub.add_parser("reach", help="measure the weight support with an impulse")
    r.add_argument("--size", type=int, default=512)
    r.add_argument("--scale", type=float, default=1.0)
    k = sub.add_parser("scale", help="wall time, peak RSS and reads over N frames")
    k.add_argument("--out", required=True)
    k.add_argument("--ns", default="2,8,16,32")
    k.add_argument("--tile-sizes", default="1024")
    a = p.parse_args()
    if a.self_check:
        sys.exit(self_check())
    if not a.cmd:
        p.error("a subcommand or --self-check is required")
    if a.cmd in ("cases", "scale") and not os.path.exists(CLI):
        sys.exit(f"{CLI} not found: build the workspace first")
    {"cases": cmd_cases, "fuse": cmd_fuse, "score": cmd_score, "montage": cmd_montage, "reach": cmd_reach, "scale": cmd_scale}[a.cmd](a)


if __name__ == "__main__":
    main()
