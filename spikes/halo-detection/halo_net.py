#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""A small U-Net trained on layered scenes for both halo maps, scored at the halo gate."""

import argparse
import glob
import hashlib
import json
import os
import re
import shutil
import sys
import tempfile
import time
import zlib
from concurrent.futures import ProcessPoolExecutor

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import classical_maps as cm  # noqa: E402
import halo_gate  # noqa: E402
import layered_scene as ls  # noqa: E402
import pretrained_dbd as pd  # noqa: E402

gff, ss, rawtiff = ls.gff, ls.ss, ls.rawtiff


TRAIN_SEEDS = range(4, 10)  # calibration seeds 1-3 pick the checkpoint, 11 and above are held out, 10 is in neither
RESERVED = (3840, 2476, 512, 512)  # x, y, w, h of the reference crop in Mantis_Stacked.tif
MARGIN = 512  # px kept clear of the reserved rect by every training window
WIN = 512
# x, y of the eight training windows of Mantis_Stacked.tif: the rule printed by `generate --list-windows`, frozen.
WINDOWS = [(1536, 512), (1536, 2048), (2048, 1536), (2048, 2560), (2560, 1536), (3584, 0), (3584, 512), (4096, 512)]
FRAMES, SIGMAS, NOISES = (6, 8, 12), (0.3, 0.45, 0.6), (150.0, 250.0, 400.0)
MIN_STEP = 3  # smallest foreground-to-background frame distance of a training scene
HELDOUT_FRAMES = 8  # frames of every held-out and calibration scene
RISK_BAND = 24  # px: the map A target ramps to 0 here
B_SCALE = 100.0  # counts per unit of head B
WIDTHS = (16, 32, 64, 128)
CROP, BATCH, STEPS, EVAL_EVERY, LR = 256, 16, 6000, 500, 1e-3
NEAR_WEIGHT = 4.0  # loss weight of head B within `RISK_BAND` px of the contour
TIME_CAP, MEMORY_CAP = 30 * 60, 4 * 2**30  # seconds, bytes of graphics memory
TILE, TILE_MARGIN = 256, 64  # inference tile and its context margin, wider than the network's receptive radius
TILE_TOLERANCE = (1e-4, 1e-2)  # tiled against untiled: max |difference| of map A and of map B in counts
WEIGHTS_SHA256 = "391ebd8e270559ac270566e75b5c5a01c067e7233720cd861e5047ccbdbbd0ef"  # the checkpoint picked on the calibration split
WEIGHTS = os.path.join(pd.WORK, "net", "halo-net.pt")
CALIBRATION = os.path.join(ls.ROOT, ".scratch", "halo-scenes")


def parse_seeds(text):
    """`4-9` or `4,5,11` as a list of seeds."""
    out = []
    for part in text.split(","):
        lo, _, hi = part.partition("-")
        out += range(int(lo), int(hi or lo) + 1)
    return out


# ---------------------------------------------------------------- scenes and the split guard


def scene_spec(name):
    """((foreground texture, gain, offset), (background texture, gain, offset)) of a scene name: the four of
    `layered_scene.SCENES`, or the two procedural families with `easy` replaced by window k of Mantis_Stacked.tif."""
    if name in ls.SCENES:
        return ls.SCENES[name]
    if m := re.fullmatch(r"stack(\d+)-on-dark", name):
        return (f"stack{m[1]}", 1.0, 0.0), ("hard", 0.5, 0.0)
    if m := re.fullmatch(r"dark-on-stack(\d+)", name):
        return ("hard", 0.5, 0.0), (f"stack{m[1]}", 0.5, 30000.0)
    sys.exit(f"{name}: not a scene of this spike")


def window_clear(x, y):
    """Whether the window at (x, y) lies at least `MARGIN` px from the reserved rect."""
    rx, ry, rw, rh = RESERVED
    return not (x < rx + rw + MARGIN and x + WIN > rx - MARGIN and y < ry + rh + MARGIN and y + WIN > ry - MARGIN)


def check_disjoint(entries, windows=None):
    """Exits naming the first (scene, seed) that is held out, outside the training seeds, uses the `mantis` texture or
    a window near the reserved rect."""
    windows = WINDOWS if windows is None else windows
    for name, seed in entries:
        who = f"{name} seed {seed}"
        fg, bg = scene_spec(name)
        if (name, seed) in ls.SPLITS["heldout"]:
            sys.exit(f"{who}: a held-out scene")
        if seed not in TRAIN_SEEDS:
            sys.exit(f"{who}: seed {seed} is outside the training seeds {TRAIN_SEEDS.start}-{TRAIN_SEEDS.stop - 1}")
        for tex in (fg[0], bg[0]):
            if tex == "mantis":
                sys.exit(f"{who}: uses the mantis texture, reserved for the contact sheet")
            if tex.startswith("stack"):
                k = int(tex[5:])
                if k >= len(windows):
                    sys.exit(f"{who}: there is no window {k}")
                if not window_clear(*windows[k]):
                    sys.exit(f"{who}: window {k} at {windows[k]} lies within {MARGIN} px of the reserved rect {RESERVED}")


def train_entries(seeds, windows):
    """Every training (scene, seed): both procedural families and both families of each window."""
    names = ["bright-on-dark", "dark-on-bright"] + [n for k in windows for n in (f"stack{k}-on-dark", f"dark-on-stack{k}")]
    return [(n, s) for s in seeds for n in names]


def draw_params(name, seed):
    """Render parameters of one training scene, a function of its name and seed."""
    rng = np.random.default_rng([seed, zlib.crc32(name.encode())])
    frames = int(rng.choice(FRAMES))
    while True:
        fg, bg = (int(v) for v in rng.integers(0, frames, 2))
        if abs(fg - bg) >= MIN_STEP:
            return dict(frames=frames, fg_depth=fg, bg_depth=bg, sigma_step=float(rng.choice(SIGMAS)), noise=float(rng.choice(NOISES)))


def pick_windows(rows):
    """Eight (x, y) evenly spaced by sharpness over the sharper half of `rows` ((x, y), variance)."""
    sharper = sorted(rows, key=lambda r: r[1])[len(rows) // 2 :]
    return sorted(sharper[i][0] for i in np.rint(np.linspace(0, len(sharper) - 1, 8)).astype(int))


def mantis_stacked():
    top = os.environ.get("FOCUSFORGE_REFERENCE_STACK")
    if not top or not os.path.isdir(top):
        sys.exit("generate needs FOCUSFORGE_REFERENCE_STACK set to the reference stack directory")
    found = glob.glob(os.path.join(top, "**", "Mantis_Stacked.tif"), recursive=True)
    if len(found) != 1:
        sys.exit(f"{top}: {len(found)} files named Mantis_Stacked.tif, expected one")
    return found[0]


def read_window(path, info, x, y):
    """The 512 x 512 window of `path` at (x, y) as float samples, read by tile."""
    buf = rawtiff.read_rect(path, x, y, WIN, WIN, info)
    return np.frombuffer(buf, "<u2").astype(np.float64).reshape(WIN, WIN, info.samples_per_pixel)[..., : ss.SPP]


def texture(tex, seed):
    if tex.startswith("stack"):
        path = mantis_stacked()
        return read_window(path, rawtiff.read_info(path), *WINDOWS[int(tex[5:])])
    return ls.TEXTURES[tex](seed)


def build(name, seed):
    """Foreground, background and matte of one scene and seed."""
    (fk, fgain, foff), (bk, bgain, boff) = scene_spec(name)
    return texture(fk, seed) * fgain + foff, texture(bk, seed) * bgain + boff, ls.matte(seed)


# ---------------------------------------------------------------- inputs and targets


def z_input(z, frames):
    """The Z-Map index over `frames - 1` (0 where unset) and the mask of set pixels."""
    valid = z != cm.NO_SOURCE
    return np.where(valid, z / (frames - 1), 0.0).astype(np.float32), valid.astype(np.float32)


def net_input(composite, z, frames):
    """(3, H, W): luma / 65535, the Z input, the valid mask."""
    zn, valid = z_input(z, frames)
    return np.stack([ss.luma(composite.astype(np.float64)) / 65535.0, zn, valid]).astype(np.float32)


def risk_target(depth, fg_depth):
    """Map A target: `(25 - d) / 25` for a pixel at distance d (0 to 24) from the contour of the layer at `fg_depth`, either side; 0 beyond."""
    dist, _ = ls.contour_bands((depth == fg_depth).astype(float))
    return np.where(dist >= 0, np.maximum(0, RISK_BAND + 1 - dist) / (RISK_BAND + 1), 0).astype(np.float32)


def b_target(label):
    """Head B target: the signed label in units of `B_SCALE` counts (positive glow, negative darkening)."""
    return (label / B_SCALE).astype(np.float32)


def maps_from_heads(a_logit, b):
    """(map A risk in [0, 1], map B in signed counts) from the two head outputs."""
    return 0.5 * (1.0 + np.tanh(np.asarray(a_logit, np.float64) / 2.0)), np.asarray(b, np.float64) * B_SCALE


def augment(a, k):
    """One of the eight flips and quarter turns of a (C, H, W) stack, the same for every channel."""
    a = np.rot90(a, k % 4, axes=(1, 2))
    return (a[:, :, ::-1] if k >= 4 else a).copy()


def padded_size(h, w):
    """The size rounded up to a multiple of 8, the pooling grid."""
    return -h % 8 + h, -w % 8 + w


def cores(h, w, tile):
    """(y0, y1, x0, x1) of the non-overlapping tile cores that cover an h x w image once; `tile` is a multiple of 8."""
    return [(y, min(y + tile, h), x, min(x + tile, w)) for y in range(0, h, tile) for x in range(0, w, tile)]


def rank_cells(cells):
    """(cells at their limit, summed margin) of (value, limit) pairs; the larger tuple wins."""
    return sum(v >= lim for v, lim in cells), sum(v - lim for v, lim in cells)


def require_pinned(pinned, path):
    """Exits unless `pinned` is set and is the SHA-256 of the file at `path`."""
    if pinned is None:
        sys.exit("WEIGHTS_SHA256 is not pinned; train, pick the checkpoint on the calibration split, and pin its digest first")
    if not os.path.exists(path):
        sys.exit(f"{path}: no such weights file")
    if (got := pd.sha256_of(path)) != pinned:
        sys.exit(f"{path}: SHA-256 {got} differs from the pinned {pinned}")


# ---------------------------------------------------------------- generate


def render_entry(job):
    """One training scene: truth, the three methods' composites, Z-Maps and labels, `risk-target.npy` and `scene.json`."""
    name, seed, out, cli = job
    p = draw_params(name, seed)
    fg, bg, alpha = build(name, seed)
    frames = ls.render_layered(fg, bg, alpha, p["fg_depth"], p["bg_depth"], p["sigma_step"], p["noise"], seed, p["frames"])
    d = os.path.join(out, f"{name}-s{seed}")
    shutil.rmtree(d, ignore_errors=True)
    depth = ls.truth_depth(alpha, p["fg_depth"], p["bg_depth"])
    ls.run_methods(d, gff.ALGOS, cli, ls.write_scene(d, frames, ls.truth(fg, bg, alpha), depth))
    np.save(os.path.join(d, "risk-target.npy"), risk_target(depth, p["fg_depth"]))
    with open(os.path.join(d, "scene.json"), "w") as f:
        json.dump(dict(scene=name, seed=seed, **p), f)
    for sub in ("frames", "projects"):
        shutil.rmtree(os.path.join(d, sub))
    return f"{name}-s{seed}"


def list_windows():
    """Sharpness of every 512 px grid rect of Mantis_Stacked.tif outside the reserved margin, and the rule's pick."""
    path = mantis_stacked()
    info = rawtiff.read_info(path)
    rows = []
    for y in range(0, info.height - WIN + 1, WIN):
        for x in range(0, info.width - WIN + 1, WIN):
            if not window_clear(x, y):
                print(f"{x:5} {y:5}  within {MARGIN} px of the reserved rect {RESERVED}")
                continue
            rows.append(((x, y), ss.laplacian_variance(read_window(path, info, x, y))))
            print(f"{x:5} {y:5}  laplacian variance {rows[-1][1]:12.1f}")
    print(f"WINDOWS = {pick_windows(rows)}")


def cmd_generate(args):
    if args.list_windows:
        return list_windows()
    if not args.out:
        sys.exit("generate needs --out")
    windows = [int(v) for v in args.windows.split(",")] if args.windows else range(len(WINDOWS))
    entries = train_entries(parse_seeds(args.seeds), windows)
    check_disjoint(entries)
    cli, out = os.path.abspath(args.cli), os.path.abspath(args.out)
    ls.check_cli(cli)
    t0 = time.perf_counter()
    with ProcessPoolExecutor(args.workers) as pool:
        for done in pool.map(render_entry, [(n, s, out, cli) for n, s in entries]):
            print(f"{done}: done")
    ls.listing(out)
    print(f"{len(entries)} scenes in {time.perf_counter() - t0:.0f} s")


# ---------------------------------------------------------------- the network (torch)


def device_for(name):
    """The CUDA device (exits naming the cause without one) or the CPU; convolutions run in full float32."""
    import torch

    torch.backends.cudnn.allow_tf32 = torch.backends.cuda.matmul.allow_tf32 = False
    return pd.require_gpu() if name == "cuda" else torch.device("cpu")


def build_net():
    """U-Net of three poolings returning (map A logit, head B), each (N, H, W). BatchNorm in eval mode acts per pixel, so a tile's output equals the whole image's."""
    import torch
    import torch.nn as nn
    import torch.nn.functional as F

    def block(i, o):
        return nn.Sequential(nn.Conv2d(i, o, 3, padding=1, bias=False), nn.BatchNorm2d(o), nn.ReLU(True),
                             nn.Conv2d(o, o, 3, padding=1, bias=False), nn.BatchNorm2d(o), nn.ReLU(True))

    class Net(nn.Module):
        def __init__(self):
            super().__init__()
            w = WIDTHS
            self.enc = nn.ModuleList([block(3, w[0]), block(w[0], w[1]), block(w[1], w[2])])
            self.mid = block(w[2], w[3])
            self.up = nn.ModuleList([nn.ConvTranspose2d(w[i + 1], w[i], 2, stride=2) for i in (2, 1, 0)])
            self.dec = nn.ModuleList([block(2 * w[i], w[i]) for i in (2, 1, 0)])
            self.head_a, self.head_b = nn.Conv2d(w[0], 1, 1), nn.Conv2d(w[0], 1, 1)

        def forward(self, x):
            skips = []
            for e in self.enc:
                x = e(x)
                skips.append(x)
                x = F.max_pool2d(x, 2)
            x = self.mid(x)
            for up, dec, skip in zip(self.up, self.dec, reversed(skips)):
                x = dec(torch.cat([up(x), skip], 1))
            return self.head_a(x)[:, 0], self.head_b(x)[:, 0]

    return Net()


def load_net(path, device):
    import torch

    net = build_net()
    net.load_state_dict(torch.load(path, map_location="cpu", weights_only=True))
    return net.to(device).eval()


def predict(net, x, device, tile=TILE, margin=TILE_MARGIN):
    """(head A logit, head B) of a (3, H, W) input as float32 arrays of its size. The image is reflect-padded by `margin`
    and to a multiple of 8; each `tile` core runs with its `margin` of context. `tile=None` runs the whole image in one pass."""
    import torch

    _, h, w = x.shape
    ph, pw = padded_size(h, w)
    if tile is None:
        tile, margin = max(ph, pw), 0
    pad = np.pad(x, ((0, 0), (margin, margin + ph - h), (margin, margin + pw - w)), mode="reflect")
    out = np.empty((2, ph, pw), np.float32)
    with torch.no_grad():
        for y0, y1, x0, x1 in cores(ph, pw, tile):
            heads = net(torch.from_numpy(pad[:, y0 : y1 + 2 * margin, x0 : x1 + 2 * margin]).to(device)[None])
            for i, head in enumerate(heads):
                out[i, y0:y1, x0:x1] = head[0, margin : margin + y1 - y0, margin : margin + x1 - x0].cpu().numpy()
    return out[0, :h, :w], out[1, :h, :w]


def read_case(scenes, name, seed, method, frames):
    """Network input of one scene and method."""
    d = os.path.join(scenes, f"{name}-s{seed}")
    return net_input(ss.read_tiff(os.path.join(d, f"{method}.tif")), gff.read_zmap(os.path.join(d, f"{method}-zmap.tif")), frames)


def load_train(scenes, entries):
    """(N, 5, H, W): input channels, map A target and head B target of every entry and method."""
    stacks = []
    for name, seed in entries:
        d = os.path.join(scenes, f"{name}-s{seed}")
        if not os.path.exists(os.path.join(d, "scene.json")):
            sys.exit(f"{d}: no generated scene; run generate for these seeds first")
        frames = json.load(open(os.path.join(d, "scene.json")))["frames"]
        risk = np.load(os.path.join(d, "risk-target.npy"))
        for m in gff.ALGOS:
            stacks.append(np.concatenate([read_case(scenes, name, seed, m, frames), risk[None], b_target(np.load(os.path.join(d, f"{m}-label.npy")))[None]]))
    return np.stack(stacks)


def load_calibration(scenes):
    """(method, input, label, contour geometry) of every calibration scene and method."""
    return [(m, read_case(scenes, n, s, m, HELDOUT_FRAMES), np.load(os.path.join(scenes, f"{n}-s{s}", f"{m}-label.npy")),
             halo_gate.scene_geometry(scenes, n, s)) for n, s in ls.SPLITS["calibration"] for m in gff.ALGOS]


def gate_rows(net, device, cases):
    """{method: (recall of map A, AUC of map B, sign agreement of map B)} pooled over `cases` at the gate's measures."""
    stats = {m: ([], []) for m in gff.ALGOS}
    for m, x, label, geometry in cases:
        risk, glow = maps_from_heads(*predict(net, x, device))
        stats[m][0].append(halo_gate.case_stats(risk, label, geometry, m, True))
        stats[m][1].append(halo_gate.case_stats(glow, label, geometry, m, False))
    out = {}
    for m, (a, b) in stats.items():
        pooled = halo_gate.pool(b)
        out[m] = (halo_gate.pool(a)["recall"], pooled["auc"], pooled["sign"])
    return out


def rows_cells(rows):
    """(value, limit) of every measure and method of `gate_rows`."""
    lim = halo_gate.LIMITS
    return [c for r in rows.values() for c in ((r[0], lim["recall"]), (r[1], lim["auc"]), (r[2], lim["sign"]))]


def print_rows(rows):
    for m, (recall, auc, sign) in rows.items():
        print(f"    {m:18} map A recall {recall:.3f}  map B auc {auc:.3f}  map B sign {sign:.3f}")


def cmd_train(args):
    entries = train_entries(parse_seeds(args.seeds), range(len(WINDOWS)))
    check_disjoint(entries)
    device = device_for("cuda")
    import torch
    import torch.nn.functional as F

    data, cal = load_train(args.scenes, entries), load_calibration(args.calibration)
    torch.manual_seed(0)
    rng = np.random.default_rng(0)
    net = build_net().to(device)
    opt = torch.optim.Adam(net.parameters(), LR)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, STEPS)
    print(f"train: {len(entries)} scenes x {len(gff.ALGOS)} methods, seeds {args.seeds}, {sum(p.numel() for p in net.parameters())} parameters, "
          f"{STEPS} steps of {BATCH} crops of {CROP} px, Adam {LR} cosine, seed 0, device {torch.cuda.get_device_name(0)}")
    torch.cuda.reset_peak_memory_stats()
    t0, best = time.perf_counter(), None
    for step in range(1, STEPS + 1):
        crops = []
        for i in rng.integers(0, len(data), BATCH):
            y, x = rng.integers(0, data.shape[2] - CROP + 1), rng.integers(0, data.shape[3] - CROP + 1)
            crops.append(augment(data[i, :, y : y + CROP, x : x + CROP], int(rng.integers(8))))
        batch = torch.from_numpy(np.stack(crops)).to(device)
        net.train()
        a, b = net(batch[:, :3])
        risk, lab = batch[:, 3], batch[:, 4]
        near = 1.0 + (NEAR_WEIGHT - 1.0) * (risk > 0).float()
        loss = F.binary_cross_entropy_with_logits(a, risk) + (F.smooth_l1_loss(b, lab, reduction="none") * near).mean()
        opt.zero_grad()
        loss.backward()
        opt.step()
        sched.step()
        if step % EVAL_EVERY == 0:
            net.eval()
            rows = gate_rows(net, device, cal)
            rank = rank_cells(rows_cells(rows))
            print(f"step {step:5}  loss {loss.item():.4f}  calibration cells at limit {rank[0]}/9 margin {rank[1]:+.3f}  {time.perf_counter() - t0:.0f} s")
            if best is None or rank > best[0]:
                best = (rank, step, rows, {k: v.detach().cpu().clone() for k, v in net.state_dict().items()})
            if (spent := time.perf_counter() - t0) > TIME_CAP or torch.cuda.max_memory_allocated() > MEMORY_CAP:
                sys.exit(f"stopped at step {step}: {spent:.0f} s, peak {torch.cuda.max_memory_allocated() / 2**30:.2f} GiB; the caps are 30 min and 4 GiB")
    wall = time.perf_counter() - t0
    os.makedirs(os.path.dirname(args.weights), exist_ok=True)
    torch.save(best[3], args.weights)
    print(f"pick: step {best[1]}, calibration cells at limit {best[0][0]}/9 margin {best[0][1]:+.3f}")
    print_rows(best[2])
    print(f"wall time {wall:.0f} s for {STEPS} steps  device {torch.cuda.get_device_name(0)}  peak allocated "
          f"{torch.cuda.max_memory_allocated() / 2**20:.0f} MiB  peak reserved {torch.cuda.max_memory_reserved() / 2**20:.0f} MiB")
    print(f"weights {args.weights}  {os.path.getsize(args.weights)} bytes  sha256 {pd.sha256_of(args.weights)}")


# ---------------------------------------------------------------- inference on held-out scenes and real crops


def heldout_case(scenes):
    """Name, seed and network input of the first held-out scene under the first method."""
    name, seed = ls.SPLITS["heldout"][0]
    return name, seed, read_case(scenes, name, seed, gff.ALGOS[0], HELDOUT_FRAMES)


def cmd_tile_check(args):
    require_pinned(WEIGHTS_SHA256, args.weights)
    device = device_for("cuda")
    net = load_net(args.weights, device)
    name, seed, x = heldout_case(args.scenes)
    x = x[:, :437, :389]
    whole, tiled = (maps_from_heads(*predict(net, x, device, **kw)) for kw in (dict(tile=None), dict(tile=128)))
    inner = np.s_[TILE_MARGIN:-TILE_MARGIN, TILE_MARGIN:-TILE_MARGIN]
    diffs = [float(np.abs(a[inner] - b[inner]).max()) for a, b in zip(whole, tiled)]
    print(f"tile-check {name}-s{seed} cropped to {x.shape[1]} rows x {x.shape[2]} columns, tile 128 against untiled, pixels at least {TILE_MARGIN} px from the border")
    print(f"max |difference| map A {diffs[0]:.3e} (limit {TILE_TOLERANCE[0]:.0e})  map B {diffs[1]:.3e} counts (limit {TILE_TOLERANCE[1]:.0e})")
    sys.exit(0 if all(d <= t for d, t in zip(diffs, TILE_TOLERANCE)) else 1)


def cmd_write(args):
    require_pinned(WEIGHTS_SHA256, args.weights)
    print(f"WEIGHTS_SHA256 {WEIGHTS_SHA256}")
    device = device_for("cuda")
    net = load_net(args.weights, device)
    for name, seed in ls.SPLITS["heldout"]:
        for m in gff.ALGOS:
            risk, glow = maps_from_heads(*predict(net, read_case(args.scenes, name, seed, m, HELDOUT_FRAMES), device))
            cm.save_map(args.out, "net-risk", name, seed, m, risk)
            cm.save_map(args.out, "net-glow", name, seed, m, glow)
    print(f"wrote net-risk and net-glow for {len(ls.SPLITS['heldout'])} scenes x {len(gff.ALGOS)} methods under {args.out}")


def cmd_infer(args):
    """`risk.npy` (map A in [0, 1]) and `glow.npy` (map B in signed counts) of one composite and Z-Map of any size."""
    require_pinned(WEIGHTS_SHA256, args.weights)
    composite, z = ss.read_tiff(args.composite), gff.read_zmap(args.zmap)
    cm.check_shapes(composite, z)
    device = device_for(args.device)
    risk, glow = maps_from_heads(*predict(load_net(args.weights, device), net_input(composite, z, args.frames), device))
    os.makedirs(args.out_dir, exist_ok=True)
    np.save(os.path.join(args.out_dir, "risk.npy"), risk.astype(np.float32))
    np.save(os.path.join(args.out_dir, "glow.npy"), glow.astype(np.float32))
    print(f"wrote risk.npy and glow.npy, {composite.shape[1]} x {composite.shape[0]}, under {args.out_dir}")


def cmd_bench(args):
    """Wall time per megapixel of tiled inference on one held-out composite reflected out to 2048 x 2048."""
    import statistics
    import torch

    require_pinned(WEIGHTS_SHA256, args.weights)
    device = device_for(args.device)
    net = load_net(args.weights, device)
    name, seed, x = heldout_case(args.scenes)
    side = 2048
    big = np.pad(x, ((0, 0), ((side - x.shape[1]) // 2,) * 2, ((side - x.shape[2]) // 2,) * 2), mode="reflect")
    print(f"device {args.device}  torch threads {torch.get_num_threads()}  composite {name}-s{seed} reflected to {big.shape[2]} x {big.shape[1]}, "
          f"tile {TILE} margin {TILE_MARGIN}")
    predict(net, big, device)  # warm-up
    if device.type == "cuda":
        torch.cuda.reset_peak_memory_stats()
    times = []
    for _ in range(5):
        t = time.perf_counter()
        predict(net, big, device)
        times.append(time.perf_counter() - t)
    mp = big.shape[1] * big.shape[2] / 1e6
    print(f"median of 5 {statistics.median(times) * 1000:.1f} ms  {statistics.median(times) / mp * 1000:.1f} ms per megapixel")
    if device.type == "cuda":
        print(f"peak allocated {torch.cuda.max_memory_allocated() / 2**20:.0f} MiB")
    print(f"parameters {sum(p.numel() for p in net.parameters())}")
    print(f"weights {args.weights} {os.path.getsize(args.weights)} bytes")


# ---------------------------------------------------------------- self-check


def refused(entries, windows=None):
    """The message of the refusal `check_disjoint` raises for `entries`, or None when it accepts them."""
    try:
        check_disjoint(entries, windows)
    except SystemExit as e:
        return str(e.code)
    return None


def run_cases():
    cases = {}
    cases["parse_seeds"] = parse_seeds("4-9") == [4, 5, 6, 7, 8, 9] and parse_seeds("4,11") == [4, 11] and parse_seeds("7") == [7]

    # split guard: held-out scenes, seeds outside 4-9, the mantis texture, windows near the reserved rect
    far = [(0, 0), (2816, 0), (4096, 0)]  # the last two sit just outside the 512 px margin of (3840, 2476)
    msg = refused([("bright-on-dark", 11)])
    cases["guard_refuses_heldout_scene_and_names_it"] = msg is not None and "bright-on-dark" in msg and "11" in msg
    cases["guard_refuses_calibration_seed_3"] = "seed 3" in (refused([("dark-on-bright", 3)]) or "")
    cases["guard_refuses_unused_seed_10"] = "seed 10" in (refused([("bright-on-dark", 10)]) or "")
    cases["guard_refuses_seed_11_of_a_new_name"] = "seed 11" in (refused([("stack0-on-dark", 11)], far) or "")
    cases["guard_refuses_mantis_texture"] = all("mantis" in (refused([(n, 5)]) or "") for n in ("mantis-on-hard", "easy-on-mantis"))
    cases["guard_refuses_window_touching_reserved_margin"] = all(
        "stack0-on-dark" in (refused([("stack0-on-dark", 5)], [w]) or "") for w in [(4300, 2000), (3328 - 511, 1964 + 1), (3840, 2476), (4864 - 1, 3499)])
    cases["guard_refuses_unknown_scene_and_window"] = refused([("nonsense", 5)]) is not None and refused([("stack9-on-dark", 5)], far) is not None
    cases["guard_accepts_window_scene_and_procedural"] = refused([("stack2-on-dark", 7), ("dark-on-stack1", 4), ("bright-on-dark", 9)]) is None \
        and refused([("stack1-on-dark", 4)], far) is None and refused([("dark-on-stack2", 9)], far) is None
    cases["guard_reports_first_bad_entry"] = "seed 12" in (refused([("bright-on-dark", 4), ("dark-on-bright", 12), ("bright-on-dark", 3)]) or "")
    cases["windows_frozen_clear_of_reserved_rect"] = len(WINDOWS) == 8 and refused([(f"stack{k}-on-dark", 4) for k in range(8)]) is None

    # network input: Z-Map index over frames - 1, unset pixels 0 with valid 0
    z = np.array([[0, 7, 3], [65535, 7, 0]])
    zn, valid = z_input(z, 8)
    cases["z_input_eight_frames"] = np.allclose(zn, [[0, 1, 3 / 7], [0, 1, 0]]) and valid.tolist() == [[1, 1, 1], [0, 1, 1]]
    zn12, valid12 = z_input(np.array([[0, 11, 65535]]), 12)
    cases["z_input_twelve_frames"] = np.allclose(zn12, [[0, 1, 0]]) and valid12.tolist() == [[1, 1, 0]]
    cases["net_input_stacks_luma_z_valid"] = np.allclose(net_input(np.full((2, 2, 3), 32767.5), np.array([[0, 7], [65535, 7]]), 8)[:, 1, 0],
                                                         [0.5, 0.0, 0.0]) and net_input(np.zeros((4, 5, 3)), np.zeros((4, 5), int), 8).shape == (3, 4, 5)

    # map A target on a diagonal step at a foreground depth other than 1: 1 at the contour, 0 beyond 24 px, both sides
    n = 128
    yy, xx = np.indices((n, n))
    depth = np.where(xx + yy < n, 3, 6).astype(np.uint16)  # foreground (frame 3) above the anti-diagonal
    t = risk_target(depth, 3)
    on = (xx + yy == n - 1) | (xx + yy == n)
    cases["risk_target_one_at_contour_both_sides"] = (t[on] == 1.0).all() and t.dtype == np.float32
    cases["risk_target_ramp_and_cutoff"] = np.allclose(t[(xx + yy == n - 1 - 10)], 1 - 10 / 25) and not t[xx + yy < n - 1 - 24].any() \
        and not t[xx + yy > n + 24].any() and (t[xx + yy == n + 24] > 0).all() and (t[xx + yy == n - 1 - 24] > 0).all() and not t[xx + yy == n - 1 - 25].any()
    cases["risk_target_follows_the_foreground_depth_argument"] = not risk_target(depth, 5).any()

    # map B target and the output conversion keep the label's sign: positive is glow, negative is darkening
    lab = np.array([[-300.0, 0.0, 120.0]], np.float32)
    cases["b_target_scale_and_sign"] = np.allclose(b_target(lab), [[-3.0, 0.0, 1.2]]) and np.allclose(maps_from_heads(np.zeros((1, 3)), b_target(lab))[1], lab)
    risk, glow = maps_from_heads(np.array([[-40.0, 0.0, 40.0]]), np.array([[-2.0, 0.0, 2.0]]))
    cases["heads_to_maps_risk_in_unit_interval_glow_in_counts"] = np.allclose(risk, [[0, 0.5, 1]], atol=1e-6) and np.allclose(glow, [[-200, 0, 200]])

    # augmentation moves inputs and targets alike and never touches a sample's value
    a = np.arange(5 * 7, dtype=np.float32).reshape(5, 7)
    stack = np.stack([a, 2 * a, -a])
    out = [augment(stack, k) for k in range(8)]
    cases["augment_eight_distinct_and_related"] = len({o[0].tobytes() for o in out}) == 8 and all(
        np.array_equal(o[1], 2 * o[0]) and np.array_equal(o[2], -o[0]) and sorted(o[0].ravel()) == sorted(a.ravel()) for o in out)

    # tile planner: every pixel of the padded image in exactly one core, origins on the pooling grid
    h, w = 437, 389
    ph, pw = padded_size(h, w)
    count = np.zeros((ph, pw), int)
    boxes = cores(ph, pw, 128)
    for y0, y1, x0, x1 in boxes:
        count[y0:y1, x0:x1] += 1
    cases["tile_cores_cover_once"] = (ph, pw) == (440, 392) and (count == 1).all() and all(b[0] % 8 == 0 and b[2] % 8 == 0 for b in boxes)
    cases["tile_cores_when_image_smaller_than_tile"] = cores(40, 24, 256) == [(0, 40, 0, 24)]

    # checkpoint pick: most cells at their limit, then the summed margin
    at_limit, margin = rank_cells([(0.9, 0.8), (0.7, 0.75), (0.95, 0.9)])
    cases["rank_counts_cells_at_limit_and_sums_margins"] = at_limit == 2 and abs(margin - 0.1) < 1e-12
    cases["rank_prefers_more_cells_over_margin"] = rank_cells([(0.81, 0.8), (0.76, 0.75)]) > rank_cells([(0.99, 0.8), (0.1, 0.75)])

    # write refuses unless the weights are the pinned ones
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "w.pt")
        open(path, "wb").write(b"weights")
        digest = hashlib.sha256(b"weights").hexdigest()
        cases["pin_accepts_matching_file"] = require_pinned(digest, path) is None
        cases["pin_refuses_none"] = "WEIGHTS_SHA256" in (str_exit(require_pinned, None, path))
        cases["pin_refuses_other_file"] = digest[:12] in str_exit(require_pinned, "0" * 64, path)
        cases["pin_refuses_missing_file"] = path + "x" in str_exit(require_pinned, digest, path + "x")

    # window pick: the sharper half, eight evenly spaced by sharpness
    rows = [((i, 0), float(i)) for i in range(40)]
    picked = pick_windows(rows)
    cases["pick_windows_evenly_spaced_in_sharper_half"] = len(picked) == 8 and min(p[0] for p in picked) >= 20 and picked == sorted(picked, key=lambda p: p[0]) \
        and len(set(picked)) == 8
    # the training set: 108 scenes, each drawn once and the same on every call, with a usable depth step
    entries = train_entries(TRAIN_SEEDS, range(len(WINDOWS)))
    drawn = [draw_params(n, s) for n, s in entries]
    cases["train_set_is_108_scenes_and_passes_the_guard"] = len(entries) == len(set(entries)) == 108 and refused(entries) is None
    cases["draw_params_repeats_and_stays_in_range"] = drawn == [draw_params(n, s) for n, s in entries] and all(
        d["frames"] in FRAMES and 0 <= d["fg_depth"] < d["frames"] and 0 <= d["bg_depth"] < d["frames"] and abs(d["fg_depth"] - d["bg_depth"]) >= MIN_STEP
        and d["sigma_step"] in SIGMAS and d["noise"] in NOISES for d in drawn)
    cases["draw_params_varies_over_scenes"] = len({(d["frames"], d["sigma_step"], d["noise"]) for d in drawn}) > 10 and any(d["fg_depth"] > d["bg_depth"] for d in drawn)
    return cases


def str_exit(fn, *args):
    """The exit message of `fn(*args)`, or '' when it returns."""
    try:
        fn(*args)
    except SystemExit as e:
        return str(e.code)
    return ""


def self_check():
    ok = True
    for name, v in run_cases().items():
        print(f"halo-net  {name:60} {'ok  ' if v else 'FAIL'}")
        ok &= bool(v)
    return ok


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--self-check", action="store_true")
    sub = p.add_subparsers(dest="cmd")

    def add(name, help, weights=True, scenes=None):
        sp = sub.add_parser(name, help=help)
        if weights:
            sp.add_argument("--weights", default=WEIGHTS, help="the checkpoint file")
        if scenes:
            sp.add_argument("--scenes", required=True, help=scenes)
        return sp

    g = add("generate", "training scenes of the requested seeds, fused with all three methods", weights=False)
    g.add_argument("--seeds", default="4-9", help="seeds such as 4-9 or 4,5")
    g.add_argument("--windows", help="indices of the Mantis_Stacked.tif windows to use (default all)")
    g.add_argument("--out")
    g.add_argument("--list-windows", action="store_true", help="print the sharpness of every candidate window and the rule's pick")
    g.add_argument("--workers", type=int, default=4)
    g.add_argument("--cli", default=os.path.join(ls.ROOT, "target", "debug", "focusforge-cli"))
    t = add("train", "train on the generated scenes and pick the checkpoint on the calibration split", scenes="generated training scenes")
    t.add_argument("--seeds", default="4-9")
    t.add_argument("--calibration", default=CALIBRATION, help="render directory holding the calibration scenes")
    add("tile-check", "tiled against untiled inference on a held-out crop", scenes="render directory")
    add("write", "held-out map arms net-risk and net-glow", scenes="render directory").add_argument("--out", required=True)
    i = add("infer", "the two maps of one composite and Z-Map of any size")
    i.add_argument("--composite", required=True)
    i.add_argument("--zmap", required=True)
    i.add_argument("--frames", type=int, required=True, help="frames of the stack the Z-Map indexes")
    i.add_argument("--out-dir", required=True)
    i.add_argument("--device", choices=("cuda", "cpu"), default="cuda")
    add("bench", "wall time per megapixel, parameter count and weight size", scenes="render directory").add_argument("--device", choices=("cuda", "cpu"), default="cuda")
    args = p.parse_args()
    if args.self_check:
        ok = self_check()
        print("self-check passed" if ok else "self-check FAILED")
        sys.exit(0 if ok else 1)
    if args.cmd is None:
        p.error("a subcommand or --self-check is required")
    {"generate": cmd_generate, "train": cmd_train, "tile-check": cmd_tile_check, "write": cmd_write, "infer": cmd_infer,
     "bench": cmd_bench}[args.cmd](args)


if __name__ == "__main__":
    main()
