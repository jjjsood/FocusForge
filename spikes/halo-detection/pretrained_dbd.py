#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""A pretrained defocus-blur detector, run unchanged on the layered scenes and scored at the halo gate."""

import argparse
import hashlib
import http.client
import http.server
import itertools
import os
import shutil
import sys
import tempfile
import threading
import urllib.request
import zipfile
import tarfile
from concurrent.futures import ProcessPoolExecutor

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import classical_maps as cm  # noqa: E402
import halo_gate  # noqa: E402
import layered_scene as ls  # noqa: E402

gff, ss = ls.gff, ls.ss

UPSTREAM_SHA = "6a59c51059d78af344cbd50192726cbe488a9daf"
UPSTREAM_URL = f"https://codeload.github.com/vinthony/depth-distillation/tar.gz/{UPSTREAM_SHA}"
DRIVE = "https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t"
DRIVE_IDS = {"res_best.pth": "1HrzFqXSJY2Zxd9qvdKC7_Glxljjd27sf", "resnext.zip": "1o7zQvDef9mAEzbQeHAwMSi9nK_QEhhVZ"}
BACKBONE = "resnext_101_32x4d.pth"  # the member of resnext.zip the decoder's backbone loads
SHA256 = {
    "res_best.pth": "483ffdcde63220560da6b3625c85690eaf0fa5691788625776165670d5abe401",
    "resnext.zip": "c34cca9003338cbcd00c8067cb5a7e7d47dd85b3340c37b91b803eaa8fafbcdc",
    "resnext_101_32x4d.pth": "f3edcb80fe5f8875ea2c9fdc21fd488f1928ab9c0d88e9527cd854fd6fe18b64",
}  # file name -> hex digest; the sums the first `fetch` printed
SIZES = ("320", "native")
GRID = dict(threshold=(4, 8, 16, 32, 64), band_hi=(12, 24, 48), median=(1, 5, 9, 15))  # threshold in 8-bit depth units
# Chosen by `tune` on the calibration split, per size (cells at limit, then summed margin).
PARAMS = {
    "320": dict(threshold=4, band_hi=48, median=15),
    "native": dict(threshold=4, band_hi=12, median=9),
}
MASK_IS_BLUR = False  # `polarity`: the sigmoid mask is high on in-focus pixels, so the blur probability is 1 - mask
WORK = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), ".scratch", "halo-detection")


def to8(x16):
    """16-bit samples as the 8-bit samples the detector's own preprocessing expects."""
    return np.rint(np.asarray(x16, np.float64) / 257.0).astype(np.uint8)


def depth_to8(depth):
    """The distilled depth output clipped to [0, 1] and stored as 8-bit levels (uint16 for the Z-Map functions)."""
    return np.rint(np.clip(depth, 0.0, 1.0) * 255.0).astype(np.uint16)


def map_a(depth, params):
    """Map A: `classical_maps.risk_map` over the 8-bit depth levels; the threshold is in depth levels."""
    return cm.risk_map(depth_to8(depth), params)


def native_ok(shape):
    """Native resolution needs both sides at a multiple of 32, the decoder's coarsest skip scale."""
    return shape[0] % 32 == 0 and shape[1] % 32 == 0


def pick(rows, limit):
    """The parameter set of `rows` ((params, per-method recalls)) with the most methods at `limit`, ties by the larger
    summed margin, then the first listed."""
    best = None
    for params, recalls in rows:
        rank = (sum(r >= limit for r in recalls), sum(r - limit for r in recalls))
        if best is None or rank > best[0]:
            best = (rank, params)
    return best[1]


def frozen(table, sizes):
    """Exits naming every size of `sizes` without a frozen parameter set in `table`."""
    if missing := [s for s in sizes if not table.get(s)]:
        sys.exit(f"no frozen parameters for size {', '.join(missing)}; run tune and freeze PARAMS first")


# ---------------------------------------------------------------- fetch


class FetchError(Exception):
    """A download that cannot be trusted; the message names the URL."""


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def download(url, part):
    """Streams `url` into `part`; refuses an HTML page and a body shorter than its Content-Length. Removes `part` on failure."""
    try:
        with urllib.request.urlopen(url, timeout=60) as r:
            if r.headers.get_content_type() == "text/html":
                raise FetchError(f"{url}: served an HTML page, not a file")
            want, got = r.headers.get("Content-Length"), 0
            with open(part, "wb") as f:
                while chunk := r.read(1 << 20):
                    f.write(chunk)
                    got += len(chunk)
        if want is not None and got != int(want):
            raise FetchError(f"{url}: body is {got} bytes, Content-Length said {want}")
    except FetchError:
        _drop(part)
        raise
    except (OSError, http.client.HTTPException) as e:
        _drop(part)
        raise FetchError(f"{url}: {e}") from e


def _drop(path):
    if os.path.exists(path):
        os.remove(path)


def settle(part, dest, sha, label):
    """Renames `part` to `dest` when its digest is the pinned one (any digest when none is pinned)."""
    got = sha256_of(part)
    if sha is not None and got != sha:
        os.remove(part)
        raise FetchError(f"{label}: SHA-256 {got} differs from the pinned {sha}")
    os.replace(part, dest)


def fetch_file(url, dest, sha):
    """'verified' when `dest` already holds the pinned digest (nothing is downloaded), otherwise downloads and returns 'downloaded'."""
    if os.path.exists(dest) and sha is not None and sha256_of(dest) == sha:
        return "verified"
    download(url, dest + ".part")
    settle(dest + ".part", dest, sha, url)
    return "downloaded"


def fetch_upstream(work):
    """The upstream code at the pinned commit under `<work>/upstream/`, refused unless its LICENSE is MIT."""
    top = os.path.join(work, "upstream", f"depth-distillation-{UPSTREAM_SHA}")
    if os.path.isdir(top) and "MIT License" in open(os.path.join(top, "LICENSE")).read():
        return "verified"
    os.makedirs(work, exist_ok=True)
    archive = os.path.join(work, "upstream.tar.gz.part")
    download(UPSTREAM_URL, archive)
    try:
        shutil.rmtree(top, ignore_errors=True)
        with tarfile.open(archive) as t:
            t.extractall(os.path.join(work, "upstream"), filter="data")
        if "MIT License" not in open(os.path.join(top, "LICENSE")).read():
            raise FetchError(f"{UPSTREAM_URL}: LICENSE is not MIT")
    except (OSError, tarfile.TarError) as e:
        shutil.rmtree(top, ignore_errors=True)
        raise FetchError(f"{UPSTREAM_URL}: {e}") from e
    except FetchError:
        shutil.rmtree(top, ignore_errors=True)
        raise
    finally:
        _drop(archive)
    return "downloaded"


def extract_backbone(zip_path, dest, sha):
    """The one member of `zip_path` named `BACKBONE`, written to `dest` after its digest matches."""
    if os.path.exists(dest) and sha is not None and sha256_of(dest) == sha:
        return "verified"
    with zipfile.ZipFile(zip_path) as z:
        names = [n for n in z.namelist() if n.endswith(BACKBONE) and not n.startswith("__MACOSX/")]
        if len(names) != 1:
            raise FetchError(f"{zip_path}: {len(names)} members end in {BACKBONE}")
        with z.open(names[0]) as src, open(dest + ".part", "wb") as out:
            shutil.copyfileobj(src, out)
    settle(dest + ".part", dest, sha, f"{zip_path}:{names[0]}")
    return "extracted"


def cmd_fetch(args):
    weights = os.path.join(args.work, "weights")
    os.makedirs(weights, exist_ok=True)
    try:
        print(f"{fetch_upstream(args.work):10} upstream {UPSTREAM_SHA} (MIT)")
        for name, ident in DRIVE_IDS.items():
            print(f"{fetch_file(args.weights_url or DRIVE.format(id=ident), os.path.join(weights, name), SHA256.get(name)):10} {name}")
        print(f"{extract_backbone(os.path.join(weights, 'resnext.zip'), os.path.join(weights, BACKBONE), SHA256.get(BACKBONE)):10} {BACKBONE}")
    except (FetchError, zipfile.BadZipFile) as e:
        sys.exit(f"fetch failed: {e}")
    for name in (*DRIVE_IDS, BACKBONE):
        path = os.path.join(weights, name)
        print(f"{name}  {os.path.getsize(path)} bytes  sha256 {sha256_of(path)}{'' if name in SHA256 else '  (not pinned)'}")


# ---------------------------------------------------------------- tune and write


def raw_path(work, size, name, seed, method, kind):
    return os.path.join(work, "dbd-raw", size, f"{name}-s{seed}", f"{method}-{kind}.npy")


def tune_case(job):
    """Unsigned gate stats of every grid candidate of map A on one calibration scene and method."""
    scenes, work, size, name, seed, method = job
    depth = depth_to8(np.load(raw_path(work, size, name, seed, method, "depth")))
    label, geometry = np.load(os.path.join(scenes, f"{name}-s{seed}", f"{method}-label.npy")), halo_gate.scene_geometry(scenes, name, seed)
    out, top = {}, max(GRID["band_hi"])
    for med in GRID["median"]:
        z = cm.median_zmap(depth, med)
        for thr in GRID["threshold"]:
            dist, _ = cm.risk_bands(z, thr, top)
            for hi in GRID["band_hi"]:
                m = np.where((dist >= 0) & (dist <= hi), hi + 1 - dist, 0)
                out[(thr, hi, med)] = halo_gate.case_stats(m, label, geometry, method, True)
    return method, out


def cmd_tune(args):
    """Scores every candidate on the calibration split only, per size, and prints the pick as a PARAMS entry."""
    for size in SIZES:
        jobs = [(args.scenes, args.work, size, n, s, m) for n, s in ls.SPLITS["calibration"] for m in gff.ALGOS]
        pooled = {}
        with ProcessPoolExecutor(args.workers) as pool:
            for method, out in pool.map(tune_case, jobs):
                for key, st in out.items():
                    pooled.setdefault(key, {}).setdefault(method, []).append(st)
        print(f"tune: size {size}, calibration split {ls.SPLITS['calibration']}")
        rows = []
        for thr, hi, med in itertools.product(*GRID.values()):
            recalls = [halo_gate.pool(pooled[(thr, hi, med)][m])["recall"] for m in gff.ALGOS]
            c = dict(threshold=thr, band_hi=hi, median=med)
            rows.append((c, recalls))
            print(f"{size:7} {c}  " + "  ".join(f"{m} recall={r:.3f}" for m, r in zip(gff.ALGOS, recalls)))
        print(f"CHOSEN {size}: {pick(rows, halo_gate.LIMITS['recall'])}")


def cmd_write(args):
    """Held-out map A and map B of every size under `<out>/dbd-risk-<size>` and `<out>/dbd-blur-<size>`."""
    frozen(PARAMS, SIZES)
    if MASK_IS_BLUR is None:
        sys.exit("the mask polarity is not fixed; run polarity and set MASK_IS_BLUR first")
    for size in SIZES:
        print(f"PARAMS {size}: {PARAMS[size]}")
    for size in SIZES:
        for name, seed in ls.SPLITS["heldout"]:
            for method in gff.ALGOS:
                depth = np.load(raw_path(args.work, size, name, seed, method, "depth"))
                mask = np.load(raw_path(args.work, size, name, seed, method, "mask"))
                cm.save_map(args.out, f"dbd-risk-{size}", name, seed, method, map_a(depth, PARAMS[size]))
                cm.save_map(args.out, f"dbd-blur-{size}", name, seed, method, mask if MASK_IS_BLUR else 1.0 - mask)


# ---------------------------------------------------------------- the detector (torch, graphics card)

FG_FRAME, BG_FRAME = 1, 6  # frames of the rendered scenes focused on the foreground and on the background
MEAN, STD = (0.485, 0.456, 0.406), (0.229, 0.224, 0.225)


def require_gpu():
    """The CUDA device; exits naming the cause when torch has no usable card or no kernels for it, never falls back to the CPU."""
    import torch

    if not torch.cuda.is_available():
        sys.exit(f"no usable GPU: torch.cuda.is_available() is false (torch {torch.__version__}, CUDA build {torch.version.cuda})")
    arch = "sm_%d%d" % torch.cuda.get_device_capability(0)
    if arch not in torch.cuda.get_arch_list():
        sys.exit(f"no usable GPU: this PyTorch build has no kernels for {torch.cuda.get_device_name(0)} ({arch}); its arch list is "
                 f"{' '.join(torch.cuda.get_arch_list())}")
    return torch.device("cuda:0")


def cmd_check_env(args):
    import torch

    device = require_gpu()
    g = torch.Generator().manual_seed(0)
    a, b = (torch.randint(-8, 9, (64, 64), generator=g).float() for _ in range(2))
    if not torch.equal(a @ b, (a.to(device) @ b.to(device)).cpu()):
        sys.exit("an integer-valued matrix product differs between the CPU and the graphics card")
    print(f"torch {torch.__version__}  CUDA {torch.version.cuda}  device {torch.cuda.get_device_name(0)}  "
          f"{'sm_%d%d' % torch.cuda.get_device_capability(0)} in arch list  matmul equal on cpu and cuda")


def load_model(work, device):
    """The upstream model on `device` with the checksummed weights, in eval mode."""
    import torch

    weights = os.path.join(work, "weights")
    paths_ = {n: os.path.join(weights, n) for n in ("res_best.pth", BACKBONE)}
    for n, path in paths_.items():
        if n not in SHA256:
            sys.exit(f"{n} has no pinned SHA-256; pin the sums `fetch` printed first")
        if not os.path.exists(path) or sha256_of(path) != SHA256[n]:
            sys.exit(f"{path} is missing or differs from its pinned SHA-256; run fetch")
    sys.path.insert(0, os.path.join(work, "upstream", f"depth-distillation-{UPSTREAM_SHA}"))
    import paths  # the upstream module the backbone reads its weights path from

    paths.resnext_101_32_path = paths_[BACKBONE]
    import models

    net = models.resskspp()
    net.load_state_dict(torch.load(paths_["res_best.pth"], map_location="cpu"))
    return net.to(device).eval()


def detect(net, img16, size, device):
    """(blur-mask sigmoid, distilled depth) as float32 arrays of the image's size, from one forward pass of the unchanged detector."""
    import torch
    import torch.nn.functional as F
    from PIL import Image
    from torchvision import transforms as T

    if size == "native" and not native_ok(img16.shape[:2]):
        sys.exit(f"native size needs sides that are multiples of 32, the image is {img16.shape[1]} x {img16.shape[0]}")
    steps = [T.Resize((320, 320), T.InterpolationMode.BICUBIC)] if size == "320" else []
    x = T.Compose(steps + [T.ToTensor(), T.Normalize(MEAN, STD)])(Image.fromarray(to8(img16))).unsqueeze(0).to(device)
    with torch.no_grad():
        (gs, *_), (ds, *_) = net(x)
        out = [F.interpolate(t, size=img16.shape[:2], mode="bilinear", align_corners=False)[0, 0] for t in (torch.sigmoid(gs), ds)]
    return out[0].float().cpu().numpy(), out[1].float().cpu().numpy()


def timed(net, img16, size, device):
    """One `detect` call's (mask, depth, seconds, peak bytes of graphics memory); the clock stops after a device synchronise."""
    import time
    import torch

    if device.type == "cuda":
        torch.cuda.synchronize()
        torch.cuda.reset_peak_memory_stats()
    t = time.perf_counter()
    mask, depth = detect(net, img16, size, device)
    if device.type == "cuda":
        torch.cuda.synchronize()
    return mask, depth, time.perf_counter() - t, torch.cuda.max_memory_allocated() if device.type == "cuda" else 0


def scene_dir(scenes, name, seed):
    return os.path.join(scenes, f"{name}-s{seed}")


def cmd_polarity(args):
    """Mean mask over the foreground and the background of `bright-on-dark-s1` in the frame focused on each."""
    device = require_gpu()
    net = load_model(args.work, device)
    d = scene_dir(args.scenes, "bright-on-dark", 1)
    fg = ss.read_tiff(os.path.join(d, "truth-depth.tif"))[..., 0] == halo_gate.FG_DEPTH
    means = {}
    for label, frame in (("foreground in focus", FG_FRAME), ("background in focus", BG_FRAME)):
        mask, _ = detect(net, ss.read_tiff(os.path.join(d, "frames", f"{frame:02d}.tif")), "320", device)
        means[label] = (float(mask[fg].mean()), float(mask[~fg].mean()))
        print(f"frame {frame:02d} ({label}): mean mask over foreground {means[label][0]:.4f}, over background {means[label][1]:.4f}")
    (f1, b1), (f6, b6) = means.values()
    # in-focus minus defocused mean per frame; the frame with the larger contrast decides
    gaps = {FG_FRAME: f1 - b1, BG_FRAME: b6 - f6}
    frame = max(gaps, key=lambda k: abs(gaps[k]))
    print(f"mask minus defocused mean over the in-focus region: frame {FG_FRAME:02d} {gaps[FG_FRAME]:+.4f}, frame {BG_FRAME:02d} {gaps[BG_FRAME]:+.4f}")
    print(f"frame {frame:02d} has the larger contrast: the mask is the "
          f"{'sharpness' if gaps[frame] > 0 else 'blur'} probability" + ("" if gaps[FG_FRAME] * gaps[BG_FRAME] > 0 else "; the other frame disagrees"))


def cmd_infer(args):
    """Raw mask and depth of every scene of the split and method under `<work>/dbd-raw/<size>/`, with time and peak memory."""
    device = require_gpu()
    net = load_model(args.work, device)
    todo = [c for sp in (("calibration", "heldout") if args.split == "all" else (args.split,)) for c in ls.SPLITS[sp]]
    first = ss.read_tiff(os.path.join(scene_dir(args.scenes, *todo[0]), f"{gff.ALGOS[0]}.tif"))
    detect(net, first, args.size, device)  # warm-up: kernel selection and allocator growth stay out of the timings
    for name, seed in todo:
        for method in gff.ALGOS:
            comp = ss.read_tiff(os.path.join(scene_dir(args.scenes, name, seed), f"{method}.tif"))
            mask, depth, secs, peak = timed(net, comp, args.size, device)
            for kind, arr in (("mask", mask), ("depth", depth)):
                path = raw_path(args.work, args.size, name, seed, method, kind)
                os.makedirs(os.path.dirname(path), exist_ok=True)
                np.save(path, arr)
            print(f"{args.size:7} {name}-s{seed:<3} {method:18} {secs * 1000:8.1f} ms  peak {peak / 2**20:8.1f} MiB")


def cmd_bench(args):
    """Wall time per megapixel at both sizes on one held-out composite, parameter count and weight sizes."""
    import statistics
    import torch

    device = require_gpu() if args.device == "cuda" else torch.device("cpu")
    net = load_model(args.work, device)
    name, seed = ls.SPLITS["heldout"][0]
    comp = ss.read_tiff(os.path.join(scene_dir(args.scenes, name, seed), f"{gff.ALGOS[0]}.tif"))
    print(f"device {args.device}  torch threads {torch.get_num_threads()}  composite {name}-s{seed} {comp.shape[1]} x {comp.shape[0]}")
    for size in SIZES:
        side = 320 if size == "320" else comp.shape[0]
        timed(net, comp, size, device)  # warm-up
        t = [timed(net, comp, size, device)[2] for _ in range(5)]
        print(f"{size:7} model input {side} x {side if size == '320' else comp.shape[1]}  median of 5 {statistics.median(t) * 1000:.1f} ms  "
              f"{statistics.median(t) / (side * (side if size == '320' else comp.shape[1]) / 1e6) * 1000:.1f} ms per megapixel")
    print(f"parameters {sum(p.numel() for p in net.parameters())}")
    for n in ("res_best.pth", "resnext.zip", BACKBONE):
        print(f"weights {n} {os.path.getsize(os.path.join(args.work, 'weights', n))} bytes")


# ---------------------------------------------------------------- self-check


class _Server:
    """A local HTTP server answering every path with one fixed reply, for the fetch refusals."""

    def __init__(self, body, ctype, claimed=None):
        claimed = len(body) if claimed is None else claimed

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(claimed))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.httpd.server_port}/weights"

    def __enter__(self):
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()


def run_cases():
    cases = {}
    cases["to8_ends_and_rounding"] = (to8(np.array([0, 65535, 257, 128, 129], np.uint16)).tolist() == [0, 255, 1, 0, 1])
    cases["depth_quantizer_clips"] = (depth_to8(np.array([-0.2, 1.3, 0.0, 1.0, 0.2])).tolist() == [0, 255, 0, 255, 51])

    # Map A of a quantized depth step: a vertical step 0.4 high (102 depth units) between columns 179 and 180, and a
    # tilted one; the same integers read as a Z-Map give the same band through the classical function.
    params = dict(threshold=16, band_hi=12, median=1)
    depth = np.where(np.indices((512, 512))[1] >= 180, 0.65, 0.25)
    a = map_a(depth, params)
    cases["map_a_vertical_step_bands"] = (a[:, 179] == 13).all() and (a[:, 180] == 13).all() and (a[:, 178] == 12).all() \
        and (a[:, 167] == 1).all() and not a[:, 166].any() and (a[:, 192] == 1).all() and not a[:, 193].any()
    for med in (1, 5):
        p = dict(params, median=med)
        tilt = np.where(np.add.outer(np.arange(512), -np.arange(512) * 2) >= -300, 0.88, 0.12)
        z = np.where(tilt == 0.88, 224, 31)  # 0.88 * 255 = 224.4, 0.12 * 255 = 30.6
        cases[f"map_a_equals_classical_on_equivalent_zmap_median_{med}"] = (
            np.array_equal(map_a(tilt, p), cm.risk_map(z.astype(np.uint16), p)) and map_a(tilt, p).any())
    cases["map_a_step_below_threshold_marks_nothing"] = not map_a(np.where(np.indices((64, 64))[1] >= 32, 0.53, 0.5), params).any()

    # fetch: every refusal leaves neither a part file nor a destination file, and a verified file is kept
    good = os.urandom(5000)
    digest = hashlib.sha256(good).hexdigest()
    with tempfile.TemporaryDirectory() as tmp:
        def refused(name, url, sha):
            dest = os.path.join(tmp, name)
            try:
                fetch_file(url, dest, sha)
            except FetchError as e:
                return url in str(e) and not os.path.exists(dest) and not os.path.exists(dest + ".part")
            return False

        with _Server(b"<html>Virus scan warning</html>", "text/html; charset=utf-8") as s:
            cases["fetch_refuses_html_body"] = refused("html", s.url, digest)
        with _Server(good[:3000], "application/octet-stream", claimed=len(good)) as s:
            cases["fetch_refuses_short_body"] = refused("short", s.url, digest)
        with _Server(good, "application/octet-stream") as s:
            cases["fetch_refuses_checksum_mismatch"] = refused("sum", s.url, "0" * 64)
            dest = os.path.join(tmp, "ok")
            status = fetch_file(s.url, dest, digest)
            cases["fetch_keeps_matching_file"] = status == "downloaded" and open(dest, "rb").read() == good
        cases["fetch_unreachable_names_url_and_keeps_verified"] = refused("dead", "http://127.0.0.1:9/none", digest) and \
            fetch_file("http://127.0.0.1:9/none", os.path.join(tmp, "ok"), digest) == "verified"

    # the backbone is the one member named for it; macOS metadata members beside it are not candidates
    with tempfile.TemporaryDirectory() as tmp:
        def zipped(*members):
            path = os.path.join(tmp, f"z{len(os.listdir(tmp))}.zip")
            with zipfile.ZipFile(path, "w") as z:
                for name, data in members:
                    z.writestr(name, data)
            return path

        dest = os.path.join(tmp, BACKBONE)
        archive = zipped(("resnext/", b""), (f"resnext/{BACKBONE}", good), (f"__MACOSX/resnext/._{BACKBONE}", b"sidecar"))
        cases["backbone_ignores_macos_sidecar"] = extract_backbone(archive, dest, digest) == "extracted" and open(dest, "rb").read() == good
        for name, members in (("none", [("resnext/other.pth", good)]), ("two", [(f"a/{BACKBONE}", good), (f"b/{BACKBONE}", good)])):
            gone = os.path.join(tmp, f"{name}.pth")
            try:
                extract_backbone(zipped(*members), gone, digest)
                cases[f"backbone_refuses_{name}_members"] = False
            except FetchError:
                cases[f"backbone_refuses_{name}_members"] = not os.path.exists(gone) and not os.path.exists(gone + ".part")

    # a native size must reach the 1/32 decoder scale
    cases["native_side_multiple_of_32"] = native_ok((512, 512)) and not native_ok((512, 500)) and not native_ok((100, 512))

    # tune's pick: most cells at limit, then summed margin, then the first of equals
    rows = [(dict(i=0), [0.9, 0.7, 0.7]), (dict(i=1), [0.85, 0.85, 0.5]), (dict(i=2), [0.95, 0.85, 0.5]), (dict(i=3), [0.95, 0.85, 0.5])]
    cases["tune_pick_rule"] = pick(rows, 0.80) == dict(i=2)

    # write refuses without a frozen set for a size
    try:
        frozen({"320": None}, ("320",))
        cases["write_refuses_unfrozen"] = False
    except SystemExit as e:
        cases["write_refuses_unfrozen"] = "320" in str(e.code)
    cases["frozen_set_returned"] = frozen({"320": dict(threshold=4)}, ("320",)) is None
    return cases


def self_check():
    ok = True
    for name, v in run_cases().items():
        print(f"pretrained  {name:52} {'ok  ' if v else 'FAIL'}")
        ok &= bool(v)
    return ok


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--self-check", action="store_true")
    sub = p.add_subparsers(dest="cmd")

    def add(name, help, scenes=True):
        sp = sub.add_parser(name, help=help)
        sp.add_argument("--work", default=WORK, help="untracked folder holding the environment, upstream code, weights and raw outputs")
        if scenes:
            sp.add_argument("--scenes", required=True, help="render directory")
        return sp

    add("check-env", help="the PyTorch build runs a tensor on the graphics card", scenes=False)
    add("fetch", help="upstream code and weights into the work folder, checksummed", scenes=False).add_argument(
        "--weights-url", help="read both weights files from this URL instead of Google Drive")
    add("polarity", help="mean mask over the foreground and background of one scene")
    i = add("infer", help="raw detector outputs of the calibration and held-out scenes")
    i.add_argument("--size", choices=SIZES, required=True)
    i.add_argument("--split", choices=("calibration", "heldout", "all"), default="all")
    add("tune", help="map A grid over the calibration split").add_argument("--workers", type=int, default=16)
    add("write", help="held-out map A and map B arm directories").add_argument("--out", required=True)
    b = add("bench", help="wall time per megapixel, parameter count and weight sizes")
    b.add_argument("--device", choices=("cuda", "cpu"), default="cuda")
    args = p.parse_args()
    if args.self_check:
        ok = self_check()
        print("self-check passed" if ok else "self-check FAILED")
        sys.exit(0 if ok else 1)
    if args.cmd is None:
        p.error("a subcommand or --self-check is required")
    {"check-env": cmd_check_env, "fetch": cmd_fetch, "polarity": cmd_polarity, "infer": cmd_infer, "tune": cmd_tune,
     "write": cmd_write, "bench": cmd_bench}[args.cmd](args)


if __name__ == "__main__":
    main()
