#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Renders a focus stack whose true Z-Map is known, then scores the fusion against it.

With `--warp`, it renders a known misregistration instead; that path is used
by `tools/synth-align.sh` (a workspace gate stage) and
`crates/cli/tests/synthetic_ground_truth.rs`.

Frame `k` is the source image defocused by `sigma = --sigma-step * |D - k|`, where
`D` is a continuous synthetic depth map. The true Z-Map is therefore `round(D)`,
exactly, which is the one thing no real stack can tell us.

Run from the repository root, with `focusforge-cli`
already built:

    python3 spikes/synthetic-ground-truth/synth_stack.py --scene hard
    python3 spikes/synthetic-ground-truth/synth_stack.py --scene easy --sigma-step 1.1

`--warp AMP` (AMP > 0) selects a second, independent instrument instead: it
renders the same stack with a known, seeded per-frame misregistration,
verifies the resampler's warp direction with two checks, and (unless
`--frames-only`) drives `focusforge-cli align --json` twice and writes
`<out>/measurement.json` -- see `## Ground-truth alignment` below.

Requires numpy and Pillow.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile

import numpy as np

W = H = 512
SPP = 3
NFRAMES = 8
SIGMA_QUANT = 0.2  # blur-bank resolution, in sigma
OUT = ".scratch/spike-synth"
CLI = os.path.join("target", "debug", "focusforge-cli" + (".exe" if os.name == "nt" else ""))
HARNESS_LIB = "tools/lib"  # from the repository root; override with --harness

ZMAP_UNSET = 0xFFFF  # core::zmap reserves u16::MAX


# ---------------------------------------------------------------- blur


def gauss1d(sigma):
    if sigma <= 1e-6:
        return np.array([1.0])
    r = max(1, int(np.ceil(3.0 * sigma)))
    x = np.arange(-r, r + 1, dtype=np.float64)
    k = np.exp(-(x * x) / (2.0 * sigma * sigma))
    return k / k.sum()


def blur(img, sigma):
    """Separable Gaussian on a float (H, W, C) array, edge-replicated."""
    k = gauss1d(sigma)
    if k.size == 1:
        return img.copy()
    r = k.size // 2
    out = np.zeros_like(img)
    pad = np.pad(img, ((0, 0), (r, r), (0, 0)), mode="edge")
    for i, w in enumerate(k):
        out += w * pad[:, i : i + img.shape[1], :]
    pad = np.pad(out, ((r, r), (0, 0), (0, 0)), mode="edge")
    out = np.zeros_like(img)
    for i, w in enumerate(k):
        out += w * pad[i : i + img.shape[0], :, :]
    return out


# ---------------------------------------------------------------- scenes


def scene_easy(seed=7):
    """Texture everywhere, including a fine checker: every pixel has something
    to focus on. Deliberately the scene the algorithm finds easy.

    Never combine with `--warp > 0`: the checkerboard aliases under sub-pixel
    warp plus defocus, and `run_synthetic_alignment` refuses the combination.
    """
    rng = np.random.default_rng(seed)
    base = rng.normal(0.0, 1.0, (H, W, SPP))
    img = blur(base, 1.5) * 2.2 + blur(base, 6.0) * 4.0
    yy, xx = np.mgrid[0:H, 0:W]
    img += 0.9 * (((xx // 2 + yy // 2) % 2) * 2.0 - 1.0)[:, :, None]
    for _ in range(14):
        cx, cy = rng.integers(40, W - 40), rng.integers(40, H - 40)
        r = rng.integers(12, 45)
        img[(xx - cx) ** 2 + (yy - cy) ** 2 < r * r] += rng.normal(0.0, 2.0, SPP)
    img = (img - img.min()) / (img.max() - img.min())
    return np.clip(img * 60000.0 + 2000.0, 0, 65535)


def scene_hard(seed=7):
    """Texture amplitude modulated across the frame, so large regions are
    near-flat. That is where a focus measure has nothing to measure, and where
    real stacks speckle."""
    rng = np.random.default_rng(seed)
    yy, xx = np.mgrid[0:H, 0:W]
    u, v = xx / (W - 1.0), yy / (H - 1.0)
    base = rng.normal(0.0, 1.0, (H, W, SPP))
    fine = blur(base, 1.2) * 3.0
    coarse = blur(base, 9.0) * 5.0
    env = np.exp(-(((u - 0.3) ** 2 + (v - 0.35) ** 2) / 0.030)) + np.exp(
        -(((u - 0.72) ** 2 + (v - 0.7) ** 2) / 0.020)
    )
    env = np.clip(env, 0.0, 1.0)[:, :, None]
    img = coarse * 0.35 + fine * (0.06 + 0.94 * env)
    img = (img - img.min()) / (img.max() - img.min())
    return np.clip(img * 26000.0 + 18000.0, 0, 65535)


def scene_mantis():
    """The committed mantis crop: realistic texture statistics.

    It is itself another stacker's output, so its residual artefacts become part
    of the 'truth' here. That is exactly why the two synthetic scenes exist.
    """
    path = "fixtures/mantis-512/reference.tif"
    info = rawtiff.read_info(path)
    buf = rawtiff.read_rect(path, 0, 0, info.width, info.height, info)
    a = np.frombuffer(buf, dtype="<u2").astype(np.float64)
    return a.reshape(info.height, info.width, info.samples_per_pixel)[:H, :W, :SPP].copy()


SCENES = {"easy": scene_easy, "hard": scene_hard, "mantis": scene_mantis}


def depth_map(frames=NFRAMES, relief="standard"):
    """Continuous depth in [0, NFRAMES-1]: a tilted plane for smooth gradients,
    a spherical cap for curvature, and a hard step that generates the depth
    discontinuity a pyramid blend has to cope with."""
    yy, xx = np.mgrid[0:H, 0:W]
    u, v = xx / (W - 1.0), yy / (H - 1.0)
    if relief == "shallow-tilt":
        plane_u, plane_v = 0.275, 0.125
    elif relief in ("standard", "steep-step"):
        plane_u, plane_v = 0.55, 0.25
    else:
        raise ValueError(f"unknown relief: {relief}")
    d = plane_u * u + plane_v * v
    cx, cy, r = 0.35, 0.65, 0.28
    rr = np.sqrt((u - cx) ** 2 + (v - cy) ** 2)
    cap = np.where(rr < r, np.sqrt(np.maximum(r * r - rr * rr, 0.0)) / r, 0.0)
    d = d + 0.45 * cap
    d[STEP] += 0.90 if relief == "steep-step" else 0.45
    d = (d - d.min()) / (d.max() - d.min())
    return d * (frames - 1)


STEP = (slice(80, 200), slice(300, 470))  # the hard depth step, for the error report


# ---------------------------------------------------------------- render


def render(src, depth, sigma_step, noise_sd, seed=11, frames=NFRAMES):
    """frames[k] = src defocused by sigma = sigma_step * |depth - k|.

    Blurring is done once into a bank of quantized sigmas and interpolated per
    pixel: the bank does not depend on k, so 8 frames cost one bank.
    """
    sigmas = np.arange(0.0, sigma_step * (frames - 1) + SIGMA_QUANT, SIGMA_QUANT)
    bank = np.stack([blur(src, s) for s in sigmas])
    rng = np.random.default_rng(seed)
    ys, xs = np.arange(H)[:, None], np.arange(W)[None, :]
    rendered = []
    for k in range(frames):
        pos = np.clip(sigma_step * np.abs(depth - k) / SIGMA_QUANT, 0, len(sigmas) - 1.0001)
        lo = pos.astype(np.int32)
        t = (pos - lo)[:, :, None]
        f = bank[lo, ys, xs, :] * (1 - t) + bank[lo + 1, ys, xs, :] * t
        if noise_sd > 0:
            f = f + rng.normal(0.0, noise_sd, f.shape)
        rendered.append(np.clip(np.rint(f), 0, 65535).astype(np.uint16))
    return rendered


def write_rgb(path, arr):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    pixels = np.clip(np.rint(arr), 0, 65535).astype(np.uint16).tobytes()
    rawtiff.write_tiff(path, W, H, pixels, bits=16, spp=SPP)


def write_depth(path, arr):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    rawtiff.write_tiff(path, W, H, arr.astype("<u2", copy=False).tobytes(), bits=16, spp=1)


def read_tiff(path):
    info = rawtiff.read_info(path)
    buf = rawtiff.read_rect(path, 0, 0, info.width, info.height, info)
    a = np.frombuffer(buf, dtype="<u2").astype(np.uint16)
    return a.reshape(info.height, info.width, info.samples_per_pixel)


# ---------------------------------------------------------------- metrics
# Mirrors crates/core/src/fuse/metrics.rs: luma is the mean of the spp samples,
# variance is population (divide by n), PSNR is over every sample and not just
# luma, SSIM is the mean over non-overlapping complete 8x8 luma blocks.


def luma(a):
    return a.mean(axis=2)


def laplacian_variance(a):
    L = luma(a)
    v = L[:-2, 1:-1] + L[2:, 1:-1] + L[1:-1, :-2] + L[1:-1, 2:] - 4.0 * L[1:-1, 1:-1]
    return float(v.var())


def psnr(a, b):
    mse = float(((a.astype(np.float64) - b.astype(np.float64)) ** 2).mean())
    return float("inf") if mse == 0.0 else 10.0 * np.log10(65535.0**2 / mse)


C1 = (0.01 * 65535.0) ** 2
C2 = (0.03 * 65535.0) ** 2


def block_mean_ssim(a, b):
    la, lb = luma(a), luma(b)
    by, bx = la.shape[0] // 8, la.shape[1] // 8
    if by == 0 or bx == 0:
        return 0.0
    def blocks(x):
        return x[: by * 8, : bx * 8].reshape(by, 8, bx, 8).transpose(0, 2, 1, 3).reshape(by, bx, 64)
    A, B = blocks(la), blocks(lb)
    ma, mb = A.mean(2), B.mean(2)
    va = ((A - ma[:, :, None]) ** 2).mean(2)
    vb = ((B - mb[:, :, None]) ** 2).mean(2)
    cov = ((A - ma[:, :, None]) * (B - mb[:, :, None])).mean(2)
    s = ((2 * ma * mb + C1) * (2 * cov + C2)) / ((ma**2 + mb**2 + C1) * (va + vb + C2))
    return float(s.mean())


# ---------------------------------------------------------------- scoring


def gather(frames, zidx):
    """The composite a Z-Map implies: pick frame zidx[y, x] at every pixel."""
    ys, xs = np.mgrid[0:H, 0:W]
    return np.stack(frames)[np.clip(zidx, 0, len(frames) - 1), ys, xs, :]


def score(comp, zidx, truth_img, truth_z):
    valid = zidx != ZMAP_UNSET
    d = np.abs(zidx[valid].astype(np.int32) - truth_z[valid].astype(np.int32))
    return dict(
        exact=float((d == 0).mean()),
        within1=float((d <= 1).mean()),
        mad=float(d.mean()),
        psnr=psnr(comp, truth_img),
        ssim=block_mean_ssim(comp, truth_img),
        lapvar=laplacian_variance(comp),
    )


ROW = "{:<24} {:>7} {:>8} {:>7} {:>9} {:>8} {:>13}"


def show(name, s):
    print(
        ROW.format(
            name,
            f"{s['exact'] * 100:.2f}",
            f"{s['within1'] * 100:.2f}",
            f"{s['mad']:.3f}",
            f"{s['psnr']:.3f}",
            f"{s['ssim']:.4f}",
            f"{s['lapvar']:.1f}",
        )
    )


def spearman(x, y):
    rx = np.argsort(np.argsort(x)).astype(float)
    ry = np.argsort(np.argsort(y)).astype(float)
    return float(np.corrcoef(rx, ry)[0, 1])


def run_cli(name, levels):
    out, zm = f"{OUT}/{name}-l{levels}.tif", f"{OUT}/{name}-l{levels}-zmap.tif"
    r = subprocess.run(
        [CLI, "stack", f"{OUT}/{name}", "-o", out, "--zmap", zm,
         "--algorithm", "laplacian", "--set", f"levels={levels}"],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        return None, None, r.stderr.strip()
    return read_tiff(out), read_tiff(zm)[:, :, 0], None


def error_report(err, truth_img):
    """Where the errors sit: against local texture energy, and at the depth step.

    The texture measure is taken on the true all-in-focus image, so it says how
    much signal was available, not how much the fusion produced.
    """
    L = luma(truth_img.astype(np.float64))
    lap = np.abs(L[:-2, 1:-1] + L[2:, 1:-1] + L[1:-1, :-2] + L[1:-1, 2:] - 4 * L[1:-1, 1:-1])
    lap = np.pad(lap, 1, mode="edge")
    B = 16
    def blockify(x):
        n = H // B
        return x[: n * B, : n * B].reshape(n, B, n, B).transpose(0, 2, 1, 3).reshape(n, n, B * B)
    tex = blockify(lap).mean(2)
    errb = blockify((err > 0).astype(np.float64)).mean(2)
    q = np.quantile(tex, [0, 0.25, 0.5, 0.75, 1.0])
    print("\n  z-map error rate vs local texture energy (16x16 blocks):")
    print(f"    {'quartile':<10}{'mean |lap|':>12}{'error rate':>14}")
    for i in range(4):
        hi = tex <= q[i + 1] if i == 3 else tex < q[i + 1]
        m = (tex >= q[i]) & hi
        print(f"    Q{i + 1:<9}{tex[m].mean():12.1f}{errb[m].mean() * 100:13.2f}%")
    inside = (err[STEP] > 0).mean() * 100
    mask = np.ones_like(err, dtype=bool)
    mask[STEP] = False
    print(f"    at the hard depth step: {inside:.2f}%   elsewhere: {(err[mask] > 0).mean() * 100:.2f}%")
    print(f"    errors of |delta| >= 2: {(err >= 2).mean() * 100:.2f}% of all pixels")


def save_error_map(path, truth_z, zidx, err):
    from PIL import Image

    def norm(a):
        a = a.astype(np.float64)
        return ((a - a.min()) / max(a.max() - a.min(), 1e-9) * 255).astype(np.uint8)

    panel = np.hstack([norm(truth_z), norm(zidx), norm(np.minimum(err, 3))])
    Image.fromarray(panel).save(path)
    print(f"  wrote {path}  (true z-map | predicted z-map | error)")


# ---------------------------------------------------------------- driver


def run_scene(name, src, depth, sigma_step, noise_sd, levels_list, frames=NFRAMES):
    truth_z = np.clip(np.rint(depth), 0, frames - 1).astype(np.uint16)
    truth_img = np.clip(np.rint(src), 0, 65535).astype(np.uint16)
    rendered = render(src, depth, sigma_step, noise_sd, frames=frames)
    for k, f in enumerate(rendered):
        write_rgb(f"{OUT}/{name}/frames/{k:02d}.tif", f)
    write_rgb(f"{OUT}/{name}-truth.tif", src)

    print(f"\n===== scene {name}   sigma-step={sigma_step}  noise sd={noise_sd} =====")
    print(ROW.format("candidate", "exact%", "within1%", "MAD", "PSNR_GT", "SSIM_GT", "lapvar"))
    print("-" * 82)

    # Controls. These are the spike's own correctness check: the true Z-Map must
    # win outright and a random one must lose outright, or the scoring is broken
    # and nothing below it means anything.
    show("CONTROL true z-map", score(gather(rendered, truth_z), truth_z, truth_img, truth_z))
    rz = np.random.default_rng(3).integers(0, frames, (H, W)).astype(np.uint16)
    show("CONTROL random z-map", score(gather(rendered, rz), rz, truth_img, truth_z))
    best = max(
        (score(f, np.full((H, W), k, np.uint16), truth_img, truth_z) for k, f in enumerate(rendered)),
        key=lambda s: s["psnr"],
    )
    show("CONTROL best single frame", best)

    rows = []
    for lv in levels_list:
        comp, z, err = run_cli(name, lv)
        if err:
            print(f"  levels={lv}: REFUSED  {err[:150]}")
            continue
        s = score(comp, z, truth_img, truth_z)
        rows.append((lv, s, z))
        show(f"laplacian levels={lv}", s)

    if len(rows) >= 3:
        lv = [r[0] for r in rows]
        ex = np.array([r[1]["exact"] for r in rows])
        ps = np.array([r[1]["psnr"] for r in rows])
        ss = np.array([r[1]["ssim"] for r in rows])
        lp = np.array([r[1]["lapvar"] for r in rows])
        print(
            f"\n  best exact% at levels={lv[int(ex.argmax())]}"
            f"   best PSNR_GT at levels={lv[int(ps.argmax())]}"
            f"   best SSIM_GT at levels={lv[int(ss.argmax())]}"
            f"   best lapvar at levels={lv[int(lp.argmax())]}"
        )
        print(
            f"  spread: exact% {ex.min() * 100:.2f}..{ex.max() * 100:.2f}"
            f"   PSNR_GT {ps.min():.3f}..{ps.max():.3f}"
            f"   SSIM_GT {ss.min():.4f}..{ss.max():.4f}"
        )
        print(
            f"  spearman(lapvar, exact%) = {spearman(lp, ex):+.3f}"
            f"   spearman(SSIM_GT, exact%) = {spearman(ss, ex):+.3f}"
        )
        zbest = rows[int(ex.argmax())][2].astype(np.int32)
        err = np.abs(np.where(zbest == ZMAP_UNSET, truth_z, zbest) - truth_z.astype(np.int32))
        error_report(err, truth_img)
        save_error_map(f"{OUT}/{name}-errmap.png", truth_z, zbest, err)


# ---------------------------------------------------------------- ground-truth alignment
#
# `--warp AMP` (AMP > 0) renders the same kind of stack, but deliberately
# misregisters every non-reference frame by a KNOWN similarity transform, so
# `focusforge-cli align --json`'s recovered transform can be compared against
# ground truth instead of against a residual.


def similarity_matrix(scale, rot_deg, tx, ty, cx, cy):
    """`translate(tx,ty) @ translate(cx,cy) @ rotscale(scale,rot) @
    translate(-cx,-cy)` -- reference-to-frame, matching the convention
    `Transform` pins (integer pixel centres, no half-pixel offset)."""
    rad = np.deg2rad(rot_deg)
    c, s = np.cos(rad), np.sin(rad)
    rotscale = np.array([[scale * c, -scale * s, 0.0], [scale * s, scale * c, 0.0], [0.0, 0.0, 1.0]])
    to_origin = np.array([[1.0, 0.0, -cx], [0.0, 1.0, -cy], [0.0, 0.0, 1.0]])
    back = np.array([[1.0, 0.0, cx], [0.0, 1.0, cy], [0.0, 0.0, 1.0]])
    translate = np.array([[1.0, 0.0, tx], [0.0, 1.0, ty], [0.0, 0.0, 1.0]])
    return translate @ back @ rotscale @ to_origin


def warp_transforms(amp, seed, reference, frames=NFRAMES):
    """One reference-to-frame matrix per frame. Exact identity at `d == 0`
    (the reference frame), with NO rng draw consumed for it -- load-bearing
    for reproducibility, because it keeps every OTHER frame's draw
    independent of where the reference happens to sit. One rng, frames drawn
    in ascending `k` order."""
    rng = np.random.default_rng(seed)
    cx, cy = (W - 1) / 2.0, (H - 1) / 2.0
    mats = []
    for k in range(frames):
        d = k - reference
        if d == 0:
            mats.append(np.eye(3))
            continue
        tx = amp * (1.10 * d + rng.normal(0.0, 0.30))
        ty = amp * (-0.80 * d + rng.normal(0.0, 0.30))
        rot = amp * (0.060 * d + rng.normal(0.0, 0.010))  # degrees
        scale = 1.0 + amp * (0.0012 * d + rng.normal(0.0, 0.0002))
        mats.append(similarity_matrix(scale, rot, tx, ty, cx, cy))
    return mats


def similarity_parts(m):
    """The same decomposition as `Transform::similarity_parts()`:
    `scale = hypot(m00, m10)`, `radians = atan2(m10, m00)`, `tx = m02`,
    `ty = m12`."""
    scale = float(np.hypot(m[0][0], m[1][0]))
    rotation_deg = float(np.degrees(np.arctan2(m[1][0], m[0][0])))
    tx = float(m[0][2])
    ty = float(m[1][2])
    return scale, rotation_deg, tx, ty


def _cubic_kernel(x, a=-0.5):
    """The standard Keys cubic-convolution kernel, `a = -0.5` (Catmull-Rom) --
    the kernel phase README decision 3 pins. At `x = 1` this is exactly `0`
    and at `x = 0` exactly `1`, which is what makes the impulse direction
    check exact arithmetic."""
    x = np.abs(x)
    w1 = (a + 2.0) * x**3 - (a + 3.0) * x**2 + 1.0
    w2 = a * x**3 - 5.0 * a * x**2 + 8.0 * a * x - 4.0 * a
    return np.where(x <= 1.0, w1, np.where(x < 2.0, w2, 0.0))


def _cubic_weights(t):
    """The four tap weights at offsets `-1, 0, 1, 2` from `floor(s)`, for
    fractional position `t = s - floor(s)`. At `t = 0` this is exactly
    `[0, 1, 0, 0]`."""
    return (_cubic_kernel(t + 1.0), _cubic_kernel(t), _cubic_kernel(1.0 - t), _cubic_kernel(2.0 - t))


def resample(img, m):
    """The independent resampler: destination-driven via the INVERSE,
    `frame_k[p] = source[M_k^-1 . p]`, so that warping `frame_k` forward by
    `M_k` the way the Rust crate's own warp path does recovers the source.
    Integer pixel centres, no half-pixel offset; source coordinates clamped
    to `[0, W-1] x [0, H-1]` (edge replicate). Written from the crate's
    transform convention and the standard Catmull-Rom cubic, never ported
    from the crate's own warp module."""
    minv = np.linalg.inv(m)
    h, w = img.shape[0], img.shape[1]
    yy, xx = np.mgrid[0:h, 0:w].astype(np.float64)
    denom = minv[2, 0] * xx + minv[2, 1] * yy + minv[2, 2]
    sx = (minv[0, 0] * xx + minv[0, 1] * yy + minv[0, 2]) / denom
    sy = (minv[1, 0] * xx + minv[1, 1] * yy + minv[1, 2]) / denom
    sxc = np.clip(sx, 0.0, w - 1.0)
    syc = np.clip(sy, 0.0, h - 1.0)
    x0 = np.floor(sxc).astype(np.int64)
    y0 = np.floor(syc).astype(np.int64)
    tx = sxc - x0
    ty = syc - y0
    wx = _cubic_weights(tx)
    wy = _cubic_weights(ty)
    out = np.zeros_like(img, dtype=np.float64)
    for j, dy in enumerate((-1, 0, 1, 2)):
        row = np.zeros_like(img, dtype=np.float64)
        yidx = np.clip(y0 + dy, 0, h - 1)
        for i, dx in enumerate((-1, 0, 1, 2)):
            xidx = np.clip(x0 + dx, 0, w - 1)
            row += wx[i][:, :, None] * img[yidx, xidx, :]
        out += wy[j][:, :, None] * row
    return out


def direction_check_impulse(verbose=True):
    """(a) An impulse at `(100, 100)` warped by `translation(+9, +5)` must
    produce EXACTLY ONE non-zero sample, at exactly `(109, 105)`, carrying
    the exact input value. Returns `(ok, nonzero_count, (x, y) or None)`."""
    src = np.zeros((H, W, 1), dtype=np.float64)
    value = 12345.0
    src[100, 100, 0] = value
    m = similarity_matrix(1.0, 0.0, 9.0, 5.0, 0.0, 0.0)
    out = resample(src, m)
    nz = np.argwhere(out[:, :, 0] != 0.0)  # (y, x) pairs
    loc = (int(nz[0][1]), int(nz[0][0])) if len(nz) == 1 else None
    exact_value = float(out[nz[0][0], nz[0][1], 0]) if len(nz) == 1 else None
    ok = len(nz) == 1 and loc == (109, 105) and exact_value == value
    if verbose:
        print(
            f"impulse in=(100, 100) translation=(+9, +5) -> "
            f"nonzero_samples={len(nz)} location={loc} value={exact_value}"
        )
        print("OK" if ok else "FAILED: expected exactly one non-zero sample at (109, 105)")
    return ok, len(nz), loc


def run_align(out_dir, algo, mode, lax):
    cmd = [CLI, "align", out_dir, "--json", "--algorithm", algo, "--set", f"mode={mode}",
           "--set", "reference-policy=anchor"]
    if not lax:
        cmd.append("--strict")
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed (exit {r.returncode}): {r.stderr.strip()[:600]}")
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError as e:
        sys.exit(f"{' '.join(cmd)} produced invalid JSON: {e}\nstdout[:400]={r.stdout[:400]!r}")


def run_synthetic_alignment(args):
    """The `--warp > 0` path. Composition order is fixed and load-bearing:
    defocus, then warp, then sensor noise -- `render()` is called with
    `noise_sd=0.0`, each frame is warped, and the per-frame Gaussian noise is
    added AFTER the warp, because warping a noise field correlates it and a
    correlated noise field is easier for ECC than the sensor noise it stands
    in for."""
    if args.scene == "easy":
        sys.exit(
            "refusing --scene easy with --warp > 0: its 2-pixel checkerboard "
            "(synth_stack.py's scene_easy, 2-pixel period) aliases under "
            "sub-pixel warp plus defocus and produces a misleading error "
            "number -- use --scene hard"
        )

    out_dir = args.out
    shutil.rmtree(out_dir, ignore_errors=True)
    os.makedirs(out_dir, exist_ok=True)

    reference = (args.frames - 1) // 2
    depth = depth_map(args.frames, args.relief)
    src = SCENES[args.scene]()

    sharp_frames = render(
        src, depth, args.sigma_step, noise_sd=0.0, seed=args.seed, frames=args.frames
    )
    mats = warp_transforms(args.warp, args.seed, reference, frames=args.frames)
    warped = [resample(f.astype(np.float64), m) for f, m in zip(sharp_frames, mats)]

    noise_rng = np.random.default_rng(args.seed ^ 0x5A5A5A5A)
    final_frames = []
    for f in warped:
        noisy = f + (noise_rng.normal(0.0, args.noise, f.shape) if args.noise > 0 else 0.0)
        final_frames.append(np.clip(np.rint(noisy), 0, 65535).astype(np.uint16))

    for k, fr in enumerate(final_frames):
        write_rgb(os.path.join(out_dir, "frames", f"{k:02d}.tif"), fr)

    truth = []
    for k, m in enumerate(mats):
        scale, rotation_deg, tx, ty = similarity_parts(m)
        truth.append(
            {
                "frame": k,
                "name": f"{k:02d}.tif",
                "matrix": m.tolist(),
                "scale": scale,
                "rotation_deg": rotation_deg,
                "tx": tx,
                "ty": ty,
            }
        )

    ok_a, n_a, loc_a = direction_check_impulse(verbose=False)
    if not ok_a:
        sys.exit(
            f"direction check (impulse) FAILED: nonzero_samples={n_a} "
            f"location={loc_a}, expected exactly 1 at (109, 105)"
        )

    direction_checks = {
        "impulse_in": [100, 100],
        "impulse_translation": [9, 5],
        "impulse_out": [109, 105],
        "nonzero_samples": n_a,
    }

    generator = {
        "script": "spikes/synthetic-ground-truth/synth_stack.py",
        "scene": args.scene,
        "seed": args.seed,
        "warp_amplitude": args.warp,
        "sigma_step": args.sigma_step,
        "noise_sd": args.noise,
        "width": W,
        "height": H,
        "samples_per_pixel": SPP,
        "frame_count": args.frames,
        "reference": reference,
        "resampler": "independent numpy Catmull-Rom (a=-0.5), destination-driven via M^-1, edge-replicate clamp",
        "compose_order": ["defocus", "warp", "sensor-noise"],
        "cli_algo": args.algo,
    }

    if args.frames_only:
        with open(os.path.join(out_dir, "truth.json"), "w") as fh:
            json.dump(
                {"generator": generator, "direction_checks": direction_checks, "truth": truth},
                fh,
                indent=2,
                sort_keys=True,
            )
        print(f"wrote {out_dir}/frames/ and {out_dir}/truth.json (no CLI invoked)")
        return

    if not os.path.exists(CLI):
        sys.exit(f"{CLI} not found -- build the workspace first")

    # Strict for the good run -- a divergence there must fail loudly. Lax for
    # the broken run -- the break is EXPECTED to strain the solver.
    rs_report = run_align(out_dir, args.algo, "rotation-scale", lax=False)
    tr_report = run_align(out_dir, args.algo, "translation", lax=True)

    truth_by_frame = {t["frame"]: t for t in truth}
    sign_agree = 0
    qualifying = 0
    disagreements = []
    for entry in rs_report["alignments"]:
        k = entry["frame"]
        true_tx = truth_by_frame[k]["tx"]
        if abs(true_tx) >= 0.5:
            qualifying += 1
            if (entry["tx"] > 0) == (true_tx > 0):
                sign_agree += 1
            else:
                disagreements.append(k)
    direction_checks["sign_agreement_frames"] = sign_agree

    if qualifying < 6:
        sys.exit(
            f"direction check (b) FAILED: only {qualifying} frames have "
            f"|true tx| >= 0.5px (need >= 6) -- widen --warp"
        )
    if disagreements:
        sys.exit(
            f"direction check (b) FAILED: sign(recovered tx) disagreed with "
            f"sign(true tx) on frames {disagreements} -- the resampler direction "
            f"is likely inverted"
        )

    measurement = {
        "schema": "focusforge synthetic-alignment-truth v1",
        "generator": generator,
        "direction_checks": direction_checks,
        "truth": truth,
        "runs": {"rotation-scale": rs_report, "translation": tr_report},
    }

    with tempfile.NamedTemporaryFile("w", dir=out_dir, delete=False, encoding="utf-8") as fh:
        tmp_path = fh.name
        json.dump(measurement, fh, indent=2, sort_keys=True)
    os.replace(tmp_path, os.path.join(out_dir, "measurement.json"))
    print(
        f"wrote {out_dir}/measurement.json  "
        f"(direction checks: nonzero_samples={n_a}, sign_agreement={sign_agree}/{qualifying})"
    )


def run_unwarped_frames_only(args):
    """Write registered frames, their sharp reference, and rounded depth truth."""
    if args.scene == "all":
        sys.exit("--frames-only requires one scene")
    shutil.rmtree(args.out, ignore_errors=True)
    os.makedirs(args.out, exist_ok=True)
    depth = depth_map(args.frames, args.relief)
    source = SCENES[args.scene]()
    rendered = render(source, depth, args.sigma_step, args.noise, args.seed, args.frames)
    for index, frame in enumerate(rendered):
        write_rgb(os.path.join(args.out, "frames", f"{index:02d}.tif"), frame)
    write_rgb(os.path.join(args.out, "reference.tif"), source)
    truth = np.clip(np.rint(depth), 0, args.frames - 1).astype(np.uint16)
    write_depth(os.path.join(args.out, "truth-zmap.tif"), truth)
    metadata = {
        "frames": args.frames,
        "height": H,
        "noise": args.noise,
        "noise_seed": args.seed,
        "relief": args.relief,
        "scene": args.scene,
        "sigma_step": args.sigma_step,
        "texture_seed": 7,
        "width": W,
    }
    with open(os.path.join(args.out, "truth.json"), "w", encoding="utf-8", newline="\n") as fh:
        json.dump(metadata, fh, indent=2, sort_keys=True)
        fh.write("\n")
    print(f"wrote {args.out}/frames/, reference.tif, truth-zmap.tif and truth.json (no CLI invoked)")


def main():
    global OUT, rawtiff
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--scene", default="hard", choices=[*SCENES, "all"])
    p.add_argument("--sigma-step", type=float, default=0.45,
                   help="blur sigma added per depth plane; small means adjacent "
                        "frames look alike, which is what a real fine-stepped stack does")
    p.add_argument("--noise", type=float, default=250.0, help="per-frame additive noise sd, in 16-bit counts")
    p.add_argument("--levels", default="1,2,3,4,5")
    p.add_argument("--harness", default=HARNESS_LIB, help="path to this repository's tools/lib, relative to the working directory")
    p.add_argument("--warp", type=float, default=0.0,
                   help="misregistration amplitude; > 0 selects the ground-truth "
                        "alignment run and REPLACES the fusion-scoring run")
    p.add_argument("--seed", type=int, default=11,
                   help="seed for the per-frame warp model and the post-warp sensor noise")
    p.add_argument("--frames", type=int, default=NFRAMES,
                   help="number of focus planes (2 through 65535)")
    p.add_argument("--relief", choices=("standard", "steep-step", "shallow-tilt"), default="standard",
                   help="depth relief profile")
    p.add_argument("--out", default=OUT, help="output directory")
    p.add_argument("--algo", default="pyramidal-ecc",
                   help="alignment algorithm id passed to `align --algorithm` (--warp path only)")
    p.add_argument("--frames-only", action="store_true",
                   help="render frames and truth files without invoking the CLI")
    p.add_argument("--self-check", action="store_true",
                   help="run the direction verification alone and exit")
    a = p.parse_args()

    if not 2 <= a.frames <= ZMAP_UNSET:
        p.error("--frames must be an integer from 2 through 65535")

    if a.self_check:
        ok, _n, _loc = direction_check_impulse()
        sys.exit(0 if ok else 1)

    sys.path.insert(0, a.harness)
    import rawtiff  # noqa: PLC0415  -- needs the path set above

    if a.warp > 0:
        run_synthetic_alignment(a)
        return

    if a.frames_only:
        run_unwarped_frames_only(a)
        return

    if not os.path.exists(CLI):
        sys.exit(f"{CLI} not found -- build the workspace first")
    OUT = a.out
    os.makedirs(OUT, exist_ok=True)
    depth = depth_map(a.frames, a.relief)
    for name in SCENES if a.scene == "all" else [a.scene]:
        run_scene(name, SCENES[name](), depth, a.sigma_step, a.noise,
                  [int(x) for x in a.levels.split(",")], a.frames)
    print(f"\noutputs in {OUT}/")


if __name__ == "__main__":
    main()
