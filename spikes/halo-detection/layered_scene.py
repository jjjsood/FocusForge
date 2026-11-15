#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Layered synthetic scenes with an occluding foreground, and the signed halo label.

Subcommands (run from the repository root, workspace built):

    render    write frames, truth image, truth depth and, per stack method, the composite,
              Z-Map and signed label of every scene in a split
    controls  measure the label beside occluding edges against its far-band level, for the
              layered render and for the per-pixel `render()` of the same truth
    --self-check   renderer and label controls on constructed answers (numpy only)

Requires Python 3.14 and numpy.
"""

import argparse
import json
import os
import shutil
import struct
import subprocess
import sys
from compression import zstd

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "spikes", "guided-filter-fusion"))
import gff_fusion as gff  # noqa: E402  (wires rawtiff into synth_stack)

ss, rawtiff = gff.ss, gff.rawtiff
BLENDS = ("layered", "per-pixel-matte")


LABEL_SIGMA = 2.0  # px: Gaussian low pass of the composite-minus-truth luma difference
MATTE_SOFT = 1.0  # px: edge softening of the matte


def render_layered(fg, bg, alpha, fg_depth, bg_depth, sigma_step, noise_sd, seed, frames, blend="layered"):
    """Frames of a foreground layer over a background layer, each defocused at its own depth.

    The background is `ss.render`'s per-pixel bank at the constant depth `bg_depth`. Frame `k`
    adds the foreground as `blur(a*F, s) + (1 - blur(a, s)) * bg_k` with `s = sigma_step * |fg_depth - k|`,
    so a defocused foreground spreads over the background beside the matte edge. The
    `per-pixel-matte` blend, `a * blur(F, s) + (1 - a) * bg_k`, keeps the foreground inside the matte.
    Noise is drawn after compositing in `ss.render`'s order. Returns uint16 frames.
    """
    sigmas = np.arange(0.0, sigma_step * (frames - 1) + ss.SIGMA_QUANT, ss.SIGMA_QUANT)
    bank = np.stack([ss.blur(bg, s) for s in sigmas])
    rng = np.random.default_rng(seed)
    ys, xs = np.arange(ss.H)[:, None], np.arange(ss.W)[None, :]
    a = alpha[..., None]
    out = []
    for k in range(frames):
        pos = np.clip(sigma_step * abs(bg_depth - k) / ss.SIGMA_QUANT, 0, len(sigmas) - 1.0001) * np.ones((ss.H, ss.W))
        lo = pos.astype(np.int32)
        t = (pos - lo)[:, :, None]
        bg_k = bank[lo, ys, xs, :] * (1 - t) + bank[lo + 1, ys, xs, :] * t
        s = sigma_step * abs(fg_depth - k)
        if blend == "layered":
            f = ss.blur(a * fg, s) + (1 - ss.blur(a, s)) * bg_k
        else:
            f = a * ss.blur(fg, s) + (1 - a) * bg_k
        if noise_sd > 0:
            f = f + rng.normal(0.0, noise_sd, f.shape)
        out.append(np.clip(np.rint(f), 0, 65535).astype(np.uint16))
    return out


def truth(fg, bg, alpha):
    """The sharp image: sharp foreground over sharp background."""
    return alpha[..., None] * fg + (1 - alpha[..., None]) * bg


def truth_depth(alpha, fg_depth, bg_depth):
    """Frame index of the layer that owns each pixel: the foreground where alpha >= 0.5."""
    return np.where(alpha >= 0.5, fg_depth, bg_depth).astype(np.uint16)


def matte(seed):
    """Two to three seeded ellipses, edge softened; every background pixel in the corners lies far from an edge."""
    rng = np.random.default_rng(seed)
    yy, xx = np.mgrid[0 : ss.H, 0 : ss.W].astype(float)
    mask = np.zeros((ss.H, ss.W), bool)
    for _ in range(rng.integers(2, 4)):
        cx, cy = rng.integers(130, 383, 2)
        rx, ry, th = rng.uniform(40, 85), rng.uniform(40, 85), rng.uniform(0, np.pi)
        u = (xx - cx) * np.cos(th) + (yy - cy) * np.sin(th)
        v = -(xx - cx) * np.sin(th) + (yy - cy) * np.cos(th)
        mask |= (u / rx) ** 2 + (v / ry) ** 2 < 1.0
    return ss.blur(mask[..., None].astype(float), MATTE_SOFT)[..., 0]


def label(composite, truth_img):
    """Signed halo label: low-passed luma(composite) - luma(truth). Positive is glow, negative is darkening."""
    diff = ss.luma(composite.astype(float)) - ss.luma(truth_img.astype(float))
    return ss.blur(diff[..., None], LABEL_SIGMA)[..., 0]


# ---------------------------------------------------------------- scenes


def _mantis():
    cwd = os.getcwd()
    os.chdir(ROOT)  # synth_stack reads the committed crop by a repository-relative path
    try:
        return ss.scene_mantis()
    finally:
        os.chdir(cwd)


TEXTURES = {"easy": ss.scene_easy, "hard": ss.scene_hard, "mantis": lambda seed: _mantis()}
# name -> ((foreground texture, gain, offset), (background texture, gain, offset)); value = texture * gain + offset
SCENES = {
    "bright-on-dark": (("easy", 1.0, 0.0), ("hard", 0.5, 0.0)),
    "dark-on-bright": (("hard", 0.5, 0.0), ("easy", 0.5, 30000.0)),
    "mantis-on-hard": (("mantis", 1.0, 0.0), ("hard", 1.0, 0.0)),
    "easy-on-mantis": (("easy", 1.0, 0.0), ("mantis", 0.5, 0.0)),
}
SPLITS = {
    "calibration": [("bright-on-dark", s) for s in (1, 2, 3)] + [("dark-on-bright", s) for s in (1, 2, 3)],
    "heldout": [("bright-on-dark", 11), ("bright-on-dark", 12), ("dark-on-bright", 11), ("dark-on-bright", 12),
                ("mantis-on-hard", 11), ("easy-on-mantis", 11)],
}


def build_scene(name, seed):
    """Foreground texture, background texture and matte of one scene and seed."""
    (fk, fgain, foff), (bk, bgain, boff) = SCENES[name]
    return TEXTURES[fk](seed) * fgain + foff, TEXTURES[bk](seed) * bgain + boff, matte(seed)


# ---------------------------------------------------------------- CLI pipeline


def check_cli(cli):
    if not os.path.isfile(cli):
        sys.exit(f"{cli}: no such file; build the workspace first")


def run_cli(cli, what, args):
    p = subprocess.run([cli, *map(str, args)], capture_output=True, text=True)
    if p.returncode:
        sys.exit(f"{what}: focusforge-cli {args[0]} exited {p.returncode}\n{p.stderr[-800:]}")
    return p


def project_record(blob, name):
    """Record `name` of a project file: the footer indexes `{name: [offset, length]}` and ends the file."""
    (jl,) = struct.unpack("<Q", blob[-16:-8])
    if blob[:8] != b"FFPROJ\0\n" or blob[-8:] != b"FFPEND\0\n":
        sys.exit("not a project file: missing header or trailer magic")
    offset, length = json.loads(blob[-48 - jl : -48])[name]
    rec = blob[offset : offset + length]
    return zstd.decompress(rec) if rec[:4] == b"\x28\xb5\x2f\xfd" else rec


def stored_composite(project, out):
    """Writes a project's stored composite plane to a 16-bit TIFF."""
    blob = open(project, "rb").read()
    plane = json.loads(project_record(blob, "manifest.json")).get("composite")
    if plane is None or plane["byte_order"] != "little":
        sys.exit(f"{project}: no little-endian composite plane in the manifest")
    raw = project_record(blob, f"blobs/{plane['blob']}.zst")
    want = plane["width"] * plane["height"] * plane["samples_per_pixel"] * 2
    if len(raw) != want:
        sys.exit(f"{project}: composite blob is {len(raw)} bytes, expected {want}")
    rawtiff.write_tiff(out, plane["width"], plane["height"], raw, bits=16, spp=plane["samples_per_pixel"])


def write_scene(d, frames, truth_img, depth):
    """Writes the frames, truth image and truth depth; returns the truth image as stored (uint16)."""
    for k, f in enumerate(frames):
        ss.write_rgb(os.path.join(d, "frames", f"{k:02d}.tif"), f)
    ss.write_rgb(os.path.join(d, "truth.tif"), truth_img)
    ss.write_depth(os.path.join(d, "truth-depth.tif"), depth)
    return ss.read_tiff(os.path.join(d, "truth.tif"))


def run_methods(d, methods, cli, truth_stored):
    """`stack` per method over `<d>/frames`, then its stored composite, Z-Map and label under `d`.

    Returns {method: (project, label)}. A refused method or a failed run exits before that
    method's label is written.
    """
    out = {}
    for m in methods:
        proj = os.path.join(d, "projects", m)
        shutil.rmtree(proj, ignore_errors=True)
        os.makedirs(proj)
        rep = json.loads(run_cli(cli, m, ["stack", os.path.join(d, "frames"), "-o", proj, "--algorithm", m,
                                          "--no-align", "--no-crop", "--cpu", "--json"]).stdout)
        if len(rep["outputs"]) != 1:
            sys.exit(f"{d}: stack wrote {len(rep['outputs'])} projects, expected one")
        stored_composite(rep["outputs"][0], os.path.join(d, f"{m}.tif"))
        run_cli(cli, m, ["export", rep["outputs"][0], "--zmap", os.path.join(d, f"{m}-zmap.tif"), "--json"])
        lab = label(ss.read_tiff(os.path.join(d, f"{m}.tif")), truth_stored).astype(np.float32)
        np.save(os.path.join(d, f"{m}-label.npy"), lab)
        out[m] = (rep["outputs"][0], lab)
    return out


def listing(root):
    """Every image and label under `root` with its width x height x samples."""
    for dp, _, names in sorted(os.walk(root)):
        if os.sep + "projects" in dp + os.sep:
            continue
        for n in sorted(names):
            path = os.path.join(dp, n)
            if n.endswith(".tif"):
                i = rawtiff.read_info(path)
                dims = f"{i.width}x{i.height}x{i.samples_per_pixel}"
            elif n.endswith(".npy"):
                h, w = np.load(path, mmap_mode="r").shape
                dims = f"{w}x{h}x1 float32"
            else:
                continue
            print(f"{os.path.relpath(path, root)}  {dims}")


def scene_frames(args, name, seed):
    """Matte, float truth image, truth depth and layered frames of scene `name` at `seed`."""
    fg, bg, alpha = build_scene(name, seed)
    frames = render_layered(fg, bg, alpha, args.fg_depth, args.bg_depth, args.sigma_step, args.noise, seed, args.frames)
    return alpha, truth(fg, bg, alpha), truth_depth(alpha, args.fg_depth, args.bg_depth), frames


def cmd_render(args):
    check_cli(args.cli)
    scenes = SPLITS["calibration"] + SPLITS["heldout"] if args.split == "all" else SPLITS[args.split]
    for name, seed in scenes:
        d = os.path.join(args.out, f"{name}-s{seed}")
        _, t, depth, frames = scene_frames(args, name, seed)
        run_methods(d, args.methods, args.cli, write_scene(d, frames, t, depth))
        print(f"{name}-s{seed}: done")
    listing(args.out)


BAND_COLUMNS = (*gff.GLOW_BANDS, (4, 24))
THRESHOLD_PERCENTILE = 99.9


def band_means(lab, alpha):
    """Mean |label| per band of `BAND_COLUMNS` on the background side of the alpha = 0.5 contour, the signed mean of the
    4-24 band, and the |label| of every pixel beyond 48 px."""
    dist, inside = contour_bands(alpha)
    bg = ~inside
    in_band = lambda lo, hi: bg & (dist >= lo) & (dist <= hi)  # noqa: E731
    cols = [np.abs(lab[in_band(lo, hi)]).mean() for lo, hi in BAND_COLUMNS]
    return cols, lab[in_band(4, 24)].mean(), np.abs(lab[bg & (dist == -1)])


def frame_lift(frame, truth_img, alpha):
    """Signed mean luma of the frame focused on the background minus the sharp truth, 4-24 px outside the matte edge."""
    dist, inside = contour_bands(alpha)
    near = ~inside & (dist >= 4) & (dist <= 24)
    return (ss.luma(frame.astype(float)) - ss.luma(truth_img))[near].mean()


def auc_by_distance(dist, halo):
    """AUC of the score -dist (closer to the edge ranks higher) for halo against non-halo pixels, ties counted half."""
    pos, neg = np.bincount(dist[halo]), np.bincount(dist[~halo])
    n = max(len(pos), len(neg))
    pos, neg = np.pad(pos, (0, n - len(pos))), np.pad(neg, (0, n - len(neg)))
    farther = neg.sum() - np.cumsum(neg)
    return (pos * (farther + 0.5 * neg)).sum() / (pos.sum() * neg.sum())


def halo_controls(layered_runs, thresholds):
    """Per method, over the layered calibration runs: what a halo pixel (|label| >= threshold, background side) looks like.

    Reports the share of halo pixels inside the 0-24 px band and that band's share of the image (what a mark
    of the whole band scores), the AUC of the distance-to-edge score inside the band, and the share of halo
    pixels that are glow, per scene.
    """
    for m, th in thresholds.items():
        inband = total = area = n = 0
        dists, halos, glow = [], [], {}
        for name, mm, alpha, lab in layered_runs:
            if mm != m:
                continue
            dist, inside = contour_bands(alpha)
            bg = ~inside
            band = bg & (dist >= 0) & (dist <= 24)
            halo = bg & (np.abs(lab) >= th)
            total, inband, area, n = total + halo.sum(), inband + (halo & band).sum(), area + band.sum(), n + band.size
            dists.append(dist[band]), halos.append(halo[band])
            g = glow.setdefault(name, [0, 0])
            g[0], g[1] = g[0] + (halo & (lab > 0)).sum(), g[1] + halo.sum()
        auc = auc_by_distance(np.concatenate(dists), np.concatenate(halos))
        sign = " ".join(f"{k}={v[0] / max(v[1], 1):.2f}" for k, v in glow.items())
        print(f"halo-controls {m:18} halo px {total}, inside 0-24 band {inband / total:.2f}, band area share {area / n:.3f}, "
              f"AUC(-distance) in band {auc:.2f}, glow share: {sign}")


def cmd_controls(args):
    """Layered against per-pixel label levels beside the matte edge, on the calibration split."""
    check_cli(args.cli)
    head = "".join(f"{f'{lo}-{hi}':>9}" for lo, hi in BAND_COLUMNS)
    print(f"{'scene':20}{'method':18}{'render':11}{head}{'far':>9}{'4-24/far':>10}{'4-24 signed':>13}")
    layered_runs, first = [], None
    for name, seed in SPLITS["calibration"]:
        alpha, t, depth, layered = scene_frames(args, name, seed)
        per_pixel = ss.render(t, depth.astype(float), args.sigma_step, args.noise, seed, args.frames)
        for tag, frames in (("layered", layered), ("per-pixel", per_pixel)):
            d = os.path.join(args.out, f"{name}-s{seed}", tag)
            runs = run_methods(d, args.methods, args.cli, write_scene(d, frames, t, depth))
            lift = frame_lift(frames[int(args.bg_depth)], t, alpha)
            print(f"{f'{name}-s{seed}':20}{'frame k=bg-depth':18}{tag:11}{'':45}{'':9}{'':10}{lift:13.1f}")
            for m, (project, lab) in runs.items():
                cols, signed, far = band_means(lab, alpha)
                if not far.size:
                    sys.exit(f"{name}-s{seed}: no background pixel lies more than 48 px from the matte edge")
                row = "".join(f"{c:9.1f}" for c in cols)
                print(f"{f'{name}-s{seed}':20}{m:18}{tag:11}{row}{far.mean():9.1f}{cols[-1] / far.mean():10.1f}{signed:13.1f}")
                if tag == "layered":
                    layered_runs.append((name, m, alpha, lab))
            if first is None and tag == "layered":
                first = (d, runs)
    thresholds = {m: np.percentile(np.concatenate([np.abs(lab[~(a >= 0.5) & (contour_bands(a)[0] == -1)])
                                                   for _, mm, a, lab in layered_runs if mm == m]), THRESHOLD_PERCENTILE)
                  for m in args.methods}
    for m, th in thresholds.items():
        print(f"threshold {m:18} p{THRESHOLD_PERCENTILE} of far-band |label| over the calibration split: {th:.1f} counts")
    halo_controls(layered_runs, thresholds)
    d, runs = first
    for m, (project, _) in runs.items():
        exported = os.path.join(d, f"{m}-export.tif")
        run_cli(args.cli, m, ["export", project, "-o", exported, "--json"])
        diff = np.abs(ss.read_tiff(exported).astype(int) - ss.read_tiff(os.path.join(d, f"{m}.tif")).astype(int)).max()
        print(f"export-vs-stored {m:18} max |export -o - stored composite| = {diff} counts")


# ---------------------------------------------------------------- self-check


def contour_bands(alpha, limit=48):
    """Distance of every pixel from the alpha = 0.5 contour (-1 beyond `limit`) and the mask inside it."""
    inside = alpha >= 0.5
    pad = np.pad(inside, 1, mode="edge")
    edge = (pad[:-2, 1:-1] != inside) | (pad[2:, 1:-1] != inside) | (pad[1:-1, :-2] != inside) | (pad[1:-1, 2:] != inside)
    return gff.edge_distance(edge, limit)[0], inside


def self_check(blend="layered"):
    """Renderer and label against answers known by construction."""
    frames, sigma_step, bg_depth, fg_depth = 4, 1.0, 3, 0
    alpha = matte(5)
    flat = lambda v: np.full((ss.H, ss.W, ss.SPP), v)  # noqa: E731
    textured_fg, textured_bg = ss.scene_easy(3), ss.scene_hard(4)
    plane = np.full((ss.H, ss.W), float(bg_depth))
    kw = dict(fg_depth=fg_depth, bg_depth=bg_depth, sigma_step=sigma_step, frames=frames, blend=blend)

    zero = render_layered(textured_fg, textured_bg, np.zeros_like(alpha), noise_sd=250.0, seed=11, **kw)
    want = ss.render(textured_bg, plane, sigma_step, 250.0, seed=11, frames=frames)
    one = render_layered(textured_fg, textured_bg, np.ones_like(alpha), noise_sd=0.0, seed=11, **kw)
    sharp_fg = [np.clip(np.rint(ss.blur(textured_fg, sigma_step * abs(fg_depth - k))), 0, 65535).astype(np.uint16) for k in range(frames)]

    bright, dark = flat(50000.0), flat(10000.0)
    spread = render_layered(bright, dark, alpha, noise_sd=0.0, seed=11, **kw)[bg_depth].astype(float)
    none = render_layered(bright, dark, np.zeros_like(alpha), noise_sd=0.0, seed=11, **kw)[bg_depth].astype(float)
    dist, inside = contour_bands(alpha)
    near = ~inside & (dist >= 4) & (dist <= 24)
    far = ~inside & (dist == -1)
    lift = ss.luma(spread - none)

    t = truth(bright, dark, alpha)
    ring = ~inside & (dist >= 1) & (dist <= 6)
    fringe = np.where(ring, 3000.0, 0.0)[..., None]
    glow, dim = label(t + fringe, t), label(t - fringe, t)
    checks = dict(
        alpha_zero_matches_render=all(np.array_equal(a, b) for a, b in zip(zero, want)),
        alpha_one_matches_foreground=all(np.array_equal(a, b) for a, b in zip(one, sharp_fg)),
        defocus_spread=lift[near].mean() > 20.0 and abs(lift[far].mean()) < 1.0,
        label_sign=glow[ring & (dist == 3)].min() > 100.0 and dim[ring & (dist == 3)].max() < -100.0 and np.abs(glow[far]).max() < 1e-6,
        label_identity=np.abs(label(t, t)).max() == 0.0)
    for k, v in checks.items():
        print(f"layered  {k:32} {'ok  ' if v else 'FAIL'}")
    return all(checks.values())


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--self-check", action="store_true")
    p.add_argument("--break", dest="blend", choices=BLENDS[1:], default="layered",
                   help="with --self-check: run the controls on a deliberately wrong blend")
    sub = p.add_subparsers(dest="cmd")
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--frames", type=int, default=8)
    common.add_argument("--fg-depth", type=float, default=1.0, help="foreground frame index")
    common.add_argument("--bg-depth", type=float, default=6.0, help="background frame index")
    common.add_argument("--sigma-step", type=float, default=0.45, help="blur sigma per frame of depth difference")
    common.add_argument("--noise", type=float, default=250.0, help="per-frame sensor noise sd, in 16-bit counts")
    common.add_argument("--methods", type=lambda v: v.split(","), default=gff.ALGOS, help="comma-separated stack methods")
    common.add_argument("--cli", default=os.path.join(ROOT, "target", "debug", "focusforge-cli"))
    common.add_argument("--out", required=True)
    r = sub.add_parser("render", parents=[common], help="frames, truth and per-method composite, Z-Map and label")
    r.add_argument("--split", choices=("calibration", "heldout", "all"), default="all")
    sub.add_parser("controls", parents=[common], help="premise controls on the calibration split")
    args = p.parse_args()
    if args.self_check:
        sys.exit(0 if self_check(args.blend) else 1)
    if args.cmd is None:
        p.error("a subcommand or --self-check is required")
    for a in ("fg_depth", "bg_depth"):
        if not 0 <= getattr(args, a) <= args.frames - 1:
            p.error(f"--{a.replace('_', '-')} {getattr(args, a)} is outside [0, {args.frames - 1}]")
    if args.fg_depth == args.bg_depth:
        p.error("--fg-depth and --bg-depth are equal; the matte edge needs a depth step")
    args.out, args.cli = os.path.abspath(args.out), os.path.abspath(args.cli)
    {"render": cmd_render, "controls": cmd_controls}[args.cmd](args)


if __name__ == "__main__":
    main()
