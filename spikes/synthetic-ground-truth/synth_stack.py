#!/usr/bin/env python3
"""Renders a focus stack whose true Z-Map is known, then scores the fusion against it.

Spike code: throwaway by contract. Nothing in `crates/` may depend on this, and
the workspace gates do not cover it.

Frame `k` is the source image defocused by `sigma = --sigma-step * |D - k|`, where
`D` is a continuous synthetic depth map. The true Z-Map is therefore `round(D)`,
exactly, which is the one thing no real stack can tell us.

Run from the repository root, with `focusforge-cli`
already built:

    python3 spikes/synthetic-ground-truth/synth_stack.py --scene hard
    python3 spikes/synthetic-ground-truth/synth_stack.py --scene easy --sigma-step 1.1

Requires numpy and Pillow.
"""

import argparse
import os
import subprocess
import sys

import numpy as np

W = H = 512
SPP = 3
NFRAMES = 8
SIGMA_QUANT = 0.2  # blur-bank resolution, in sigma
OUT = ".scratch/spike-synth"
CLI = "./target/debug/focusforge-cli"
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
    to focus on. Deliberately the scene the algorithm finds easy."""
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


def depth_map():
    """Continuous depth in [0, NFRAMES-1]: a tilted plane for smooth gradients,
    a spherical cap for curvature, and a hard step that generates the depth
    discontinuity a pyramid blend has to cope with."""
    yy, xx = np.mgrid[0:H, 0:W]
    u, v = xx / (W - 1.0), yy / (H - 1.0)
    d = 0.55 * u + 0.25 * v
    cx, cy, r = 0.35, 0.65, 0.28
    rr = np.sqrt((u - cx) ** 2 + (v - cy) ** 2)
    cap = np.where(rr < r, np.sqrt(np.maximum(r * r - rr * rr, 0.0)) / r, 0.0)
    d = d + 0.45 * cap
    d[STEP] += 0.45
    d = (d - d.min()) / (d.max() - d.min())
    return d * (NFRAMES - 1)


STEP = (slice(80, 200), slice(300, 470))  # the hard depth step, for the error report


# ---------------------------------------------------------------- render


def render(src, depth, sigma_step, noise_sd, seed=11):
    """frames[k] = src defocused by sigma = sigma_step * |depth - k|.

    Blurring is done once into a bank of quantized sigmas and interpolated per
    pixel: the bank does not depend on k, so 8 frames cost one bank.
    """
    sigmas = np.arange(0.0, sigma_step * (NFRAMES - 1) + SIGMA_QUANT, SIGMA_QUANT)
    bank = np.stack([blur(src, s) for s in sigmas])
    rng = np.random.default_rng(seed)
    ys, xs = np.arange(H)[:, None], np.arange(W)[None, :]
    frames = []
    for k in range(NFRAMES):
        pos = np.clip(sigma_step * np.abs(depth - k) / SIGMA_QUANT, 0, len(sigmas) - 1.0001)
        lo = pos.astype(np.int32)
        t = (pos - lo)[:, :, None]
        f = bank[lo, ys, xs, :] * (1 - t) + bank[lo + 1, ys, xs, :] * t
        if noise_sd > 0:
            f = f + rng.normal(0.0, noise_sd, f.shape)
        frames.append(np.clip(np.rint(f), 0, 65535).astype(np.uint16))
    return frames


def write_rgb(path, arr):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    pixels = np.clip(np.rint(arr), 0, 65535).astype(np.uint16).tobytes()
    rawtiff.write_tiff(path, W, H, pixels, bits=16, spp=SPP)


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
    return np.stack(frames)[np.clip(zidx, 0, NFRAMES - 1), ys, xs, :]


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
         "--algo", "laplacian", "--set", f"levels={levels}"],
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


def run_scene(name, src, depth, sigma_step, noise_sd, levels_list):
    truth_z = np.clip(np.rint(depth), 0, NFRAMES - 1).astype(np.uint16)
    truth_img = np.clip(np.rint(src), 0, 65535).astype(np.uint16)
    frames = render(src, depth, sigma_step, noise_sd)
    for k, f in enumerate(frames):
        write_rgb(f"{OUT}/{name}/frames/{k:02d}.tif", f)
    write_rgb(f"{OUT}/{name}-truth.tif", src)

    print(f"\n===== scene {name}   sigma-step={sigma_step}  noise sd={noise_sd} =====")
    print(ROW.format("candidate", "exact%", "within1%", "MAD", "PSNR_GT", "SSIM_GT", "lapvar"))
    print("-" * 82)

    # Controls. These are the spike's own correctness check: the true Z-Map must
    # win outright and a random one must lose outright, or the scoring is broken
    # and nothing below it means anything.
    show("CONTROL true z-map", score(gather(frames, truth_z), truth_z, truth_img, truth_z))
    rz = np.random.default_rng(3).integers(0, NFRAMES, (H, W)).astype(np.uint16)
    show("CONTROL random z-map", score(gather(frames, rz), rz, truth_img, truth_z))
    best = max(
        (score(f, np.full((H, W), k, np.uint16), truth_img, truth_z) for k, f in enumerate(frames)),
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


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--scene", default="hard", choices=[*SCENES, "all"])
    p.add_argument("--sigma-step", type=float, default=0.45,
                   help="blur sigma added per depth plane; small means adjacent "
                        "frames look alike, which is what a real fine-stepped stack does")
    p.add_argument("--noise", type=float, default=250.0, help="per-frame additive noise sd, in 16-bit counts")
    p.add_argument("--levels", default="1,2,3,4,5")
    p.add_argument("--harness", default=HARNESS_LIB, help="path to this repository's tools/lib, relative to the working directory")
    a = p.parse_args()

    sys.path.insert(0, a.harness)
    global rawtiff
    import rawtiff  # noqa: PLC0415  -- needs the path set above

    if not os.path.exists(CLI):
        sys.exit(f"{CLI} not found -- build the workspace first")
    os.makedirs(OUT, exist_ok=True)
    depth = depth_map()
    for name in SCENES if a.scene == "all" else [a.scene]:
        run_scene(name, SCENES[name](), depth, a.sigma_step, a.noise,
                  [int(x) for x in a.levels.split(",")])
    print(f"\noutputs in {OUT}/")


if __name__ == "__main__":
    main()
