#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Classical halo maps from the Z-Map: risk zones beside depth steps and the signed glow measured inside them."""

import argparse
import itertools
import os
import sys
from concurrent.futures import ProcessPoolExecutor

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import halo_gate  # noqa: E402
import layered_scene as ls  # noqa: E402

gff, ss = ls.gff, ls.ss
NO_SOURCE = 65535  # Z-Map value of a pixel no frame supplied


ARMS = ("risk-zones", "glow-self", "glow-focus-map")
EXTRA_ARMS = {"risk-zones-dm-zmap": "risk-zones", "glow-self-dm-zmap": "glow-self"}  # the same maps read from depth-map's Z-Map
GRID = dict(threshold=(1, 2, 3, 4), band_hi=(12, 24, 48), sigma=(1.0, 2.0, 4.0), median=(1, 5, 9, 15))
# Chosen by `tune` on the calibration split (cells at their limit, then summed margin).
PARAMS = {
    "risk-zones": dict(threshold=4, band_hi=48, median=15),
    "glow-self": dict(threshold=4, band_hi=24, sigma=4.0, median=15),
    "glow-focus-map": dict(threshold=4, band_hi=48, sigma=4.0, median=15),
}


def median_zmap(z, k, rows=32):
    """k x k median of the Z-Map over its set pixels (the lower median when the window holds an even count).

    Unset pixels neither enter a median nor receive one, so they stay `NO_SOURCE`; `k = 1` returns `z` unchanged.
    """
    if k == 1:
        return z.copy()
    r, (h, w) = k // 2, z.shape
    pad = np.pad(z.astype(np.uint16), r, constant_values=NO_SOURCE)
    windows = np.lib.stride_tricks.sliding_window_view(pad, (k, k))
    out = np.empty_like(z)
    for a in range(0, h, rows):
        win = np.sort(windows[a : a + rows].reshape(-1, w, k * k), axis=-1)  # NO_SOURCE is the largest value
        n = (win != NO_SOURCE).sum(-1)
        out[a : a + rows] = np.take_along_axis(win, (np.maximum(n - 1, 0) // 2)[..., None], -1)[..., 0]
    return np.where(z == NO_SOURCE, NO_SOURCE, out).astype(z.dtype)


def depth_steps(z, threshold):
    """Step pixels (both pixels of every 4-neighbour pair of set indices differing by `threshold` or more) and their side.

    Side is +1 for a pixel that is mostly the higher index of its pairs, -1 otherwise, 0 off a step.
    """
    z, unset = z.astype(np.int64), z == NO_SOURCE
    step, lift = np.zeros(z.shape, bool), np.zeros(z.shape, np.int64)
    for hi, lo in ((np.s_[1:], np.s_[:-1]), (np.s_[:, 1:], np.s_[:, :-1])):
        d = z[hi] - z[lo]
        hit = ~unset[hi] & ~unset[lo] & (np.abs(d) >= threshold)
        step[hi] |= hit
        step[lo] |= hit
        lift[hi] += np.where(hit, d, 0)
        lift[lo] -= np.where(hit, d, 0)
    return step, np.where(step, np.where(lift < 0, -1, 1), 0).astype(np.int8)


def risk_bands(z, threshold, limit, median=1):
    """Distance (4-connected, -1 beyond `limit` and on unset pixels) from the nearest depth step and that step pixel's side."""
    z = median_zmap(z, median)
    step, side = depth_steps(z, threshold)
    dist, near = gff.edge_distance(step, limit)
    seen = (dist >= 0) & (z != NO_SOURCE)
    return np.where(seen, dist, -1), np.where(seen, side.ravel()[near], 0).astype(np.int8)


def risk_map(z, params):
    """Map A: `band_hi + 1 - distance` within `band_hi` px of a depth step, 0 elsewhere; reads the Z-Map only."""
    dist, _ = risk_bands(z, params["threshold"], params["band_hi"], params["median"])
    return np.where((dist >= 0) & (dist <= params["band_hi"]), params["band_hi"] + 1 - dist, 0).astype(np.float32)


def glow_values(signal, dist, side, params, opposite_far=False):
    """Map B from a luminance signal and bands computed to `2 * band_hi`: the low-passed signal inside the band minus the
    mean low-passed signal of the far band on the pixel's own side, over a `4 * band_hi + 1` window. NaN outside the band
    and where the window holds no far-band pixel of that side."""
    hi = params["band_hi"]
    low = ss.blur(signal[..., None], params["sigma"])[..., 0]
    level = {}
    for s in (1, -1):
        far = ((dist > hi) & (side == s)).astype(float)
        n = gff.box_mean(far, 4 * hi + 1)
        level[s] = np.divide(gff.box_mean(low * far, 4 * hi + 1), n, out=np.full(low.shape, np.nan), where=n > 0)
    own = (lambda s: level[-s]) if opposite_far else (lambda s: level[s])
    return np.where((dist >= 0) & (dist <= hi), low - np.where(side > 0, own(1), own(-1)), np.nan).astype(np.float32)


def check_shapes(composite, z, reference=None):
    """Refuses a Z-Map or reference whose size differs from the composite's, naming both sizes."""
    wh = lambda a: f"{a.shape[1]} x {a.shape[0]}"  # noqa: E731
    for name, other in (("Z-Map", z), ("reference", reference)):
        if other is not None and other.shape[:2] != composite.shape[:2]:
            sys.exit(f"the {name} is {wh(other)}, the composite is {wh(composite)}")


def glow_map(composite, z, params, reference=None, brk=None):
    """Map B: signed per-pixel glow inside map A's band. Self: the composite's own luminance. With `reference`: the
    composite's luminance minus the reference's, so a level offset between the two cancels with the far-band level."""
    check_shapes(composite, z, reference)
    signal = ss.luma(composite.astype(float))
    if reference is not None:
        signal = signal - ss.luma(reference.astype(float))
    dist, side = risk_bands(z, params["threshold"], 2 * params["band_hi"], params["median"])
    return glow_values(signal, dist, side, params, opposite_far=brk == "far-from-opposite-side")


def make_map(arm, composite, z, params, reference=None, brk=None):
    """The map of `arm` for one composite and Z-Map (map A ignores the composite); `brk` names a deliberately broken variant."""
    check_shapes(composite, z, reference)
    if brk == "edges-from-luma":
        z = np.rint(ss.luma(composite.astype(float)) / 1000.0).astype(np.int64)
    if arm == "risk-zones":
        return risk_map(z, params)
    if arm == "glow-focus-map" and reference is None:
        sys.exit("glow-focus-map needs a reference composite")
    return glow_map(composite, z, params, reference if arm == "glow-focus-map" else None, brk)


# ---------------------------------------------------------------- self-check

P = dict(threshold=2, band_hi=12, sigma=1.0, median=1)
AMP = 3000.0  # injected glow, counts
HIGH, LOW = 30000.0, 15000.0  # layer levels either side of the step
TOL = 0.10  # recovered glow within 10 % of the injected amplitude
INJECT = 9  # px from the step the glow is injected to
CHECK = (4, 6)  # px from the step where the recovered glow is judged
QUIET = 0.05  # |value| on the side without glow, relative to the amplitude


def step_scene(kind, n=512):
    """z, side (+1 higher index), and distance of every pixel from the step, for a step by construction; `inner` marks the
    pixels whose nearest step pixels lie inside the image."""
    y, x = np.indices((n, n))
    s, at = {"horizontal": (y, n // 2), "vertical": (x, n // 2), "diagonal": (y + x, n)}[kind]
    high = s >= at
    dist = np.where(high, s - at, at - 1 - s)
    inner = np.ones((n, n), bool) if kind != "diagonal" else (y >= 100) & (y < n - 100) & (x >= 100) & (x < n - 100)
    return np.where(high, 5, 0), np.where(high, 1, -1), dist, inner


def composite(side, dist, glow_side=None, amp=0.0, offset=0.0):
    """Flat layers HIGH over LOW with `amp` counts added on `glow_side` within INJECT px of the step."""
    luma = np.where(side > 0, HIGH, LOW) + offset
    if glow_side is not None:
        luma = luma + np.where((side == glow_side) & (dist <= INJECT), amp, 0.0)
    return np.repeat(luma[..., None], 3, 2)


def say(name, ok, notes):
    notes[name] = bool(ok)


def run_cases(brk=None):
    """Every case of the self-check, with the variant `brk` ('edges-from-luma', 'far-from-opposite-side') or None."""
    cases = {}
    mk = lambda *a, **kw: make_map(*a, brk=brk, **kw)  # noqa: E731
    near = lambda d, lo=CHECK[0], hi=CHECK[1]: (d >= lo) & (d <= hi)  # noqa: E731

    for kind in ("horizontal", "vertical", "diagonal"):
        z, side, dist, inner = step_scene(kind)
        flat = composite(side, dist)
        score = mk("risk-zones", flat, z, P)
        want = np.where(dist <= P["band_hi"], P["band_hi"] + 1 - dist, 0)
        bd, bs = risk_bands(z, P["threshold"], P["band_hi"])
        say(f"a_{kind}_band_and_score", np.array_equal(score[inner], want[inner]), cases)
        say(f"a_{kind}_side_and_distance", np.array_equal(bs[inner & (dist <= 12)], side[inner & (dist <= 12)])
            and np.array_equal(bd[inner], np.where(dist <= 12, dist, -1)[inner]), cases)
        for glow_side, amp in ((1, AMP), (-1, -AMP)):
            v = mk("glow-self", composite(side, dist, glow_side, amp), z, P)
            on, off = inner & (side == glow_side), inner & (side == -glow_side)
            ok = (np.abs(v[on & near(dist)] - amp) < TOL * abs(amp)).all() and (np.abs(v[off & near(dist, 4, 12)]) < QUIET * abs(amp)).all()
            say(f"b_{kind}_{'glow' if amp > 0 else 'darkening'}_on_{'high' if glow_side > 0 else 'low'}_side", ok, cases)
            say(f"b_{kind}_far_band_unmeasured_{'+' if amp > 0 else '-'}", np.isnan(v[inner & (dist > P["band_hi"])]).all()
                and not np.isnan(v[inner & (dist <= P["band_hi"])]).any(), cases)

    z, side, dist, inner = step_scene("horizontal")
    other = composite(side, dist, 1, AMP)
    say("a_independent_of_composite", np.array_equal(mk("risk-zones", composite(side, dist), z, P), mk("risk-zones", other, z, P)), cases)

    ref = composite(side, dist)
    v = mk("glow-focus-map", composite(side, dist, 1, AMP, offset=700.0), z, P, reference=ref)
    off_only = mk("glow-focus-map", composite(side, dist, offset=700.0), z, P, reference=ref)
    say("b_focus_map_recovers_glow_through_offset", (np.abs(v[(side > 0) & near(dist)] - AMP) < TOL * AMP).all(), cases)
    say("b_focus_map_global_offset_is_no_glow", np.nanmax(np.abs(off_only)) < 1e-6 and not np.isnan(off_only[dist <= 12]).any(), cases)

    flat_z = np.full((512, 512), 7)
    c = composite(side, dist)
    say("boundary_no_step_is_zero_and_nan", not mk("risk-zones", c, flat_z, P).any() and np.isnan(mk("glow-self", c, flat_z, P)).all(), cases)

    stripe = z.copy()
    stripe[:, 200:210] = NO_SOURCE
    s_a, s_b = mk("risk-zones", c, stripe, P), mk("glow-self", composite(side, dist, 1, AMP), stripe, P)
    only_unset = np.zeros_like(z)
    only_unset[:, 200:210] = NO_SOURCE
    say("boundary_unset_never_in_band", not s_a[:, 200:210].any() and np.isnan(s_b[:, 200:210]).all() and s_a[:, :200].any(), cases)
    say("boundary_unset_jump_is_not_a_step", not mk("risk-zones", c, only_unset, P).any(), cases)

    y = np.indices((512, 512))[0]
    top = np.where(y >= 3, 1, -1)
    bz = np.where(y >= 3, 5, 0)
    bdist = np.where(y >= 3, y - 3, 2 - y)
    bv = mk("glow-self", composite(top, bdist, 1, AMP), bz, P)
    bs = mk("risk-zones", c, bz, P)
    say("boundary_border_step_measured_where_band_fits", (np.abs(bv[near(bdist) & (top > 0)] - AMP) < TOL * AMP).all()
        and np.array_equal(bs, np.where(bdist <= 12, 13 - bdist, 0)), cases)
    say("boundary_side_without_far_band_is_nan", np.isnan(bv[:3]).all(), cases)

    small = np.where(np.indices((40, 40))[0] >= 20, 5, 0)
    wide = dict(P, band_hi=24)
    say("boundary_narrow_image_all_nan", np.isnan(mk("glow-self", np.full((40, 40, 3), 9000.0), small, wide)).all()
        and mk("risk-zones", np.full((40, 40, 3), 9000.0), small, wide).any(), cases)

    def refused(*a, **kw):
        try:
            mk(*a, **kw)
        except SystemExit as e:
            return "256 x 256" in str(e.code) and "512 x 512" in str(e.code)
        return False
    small_z, small_c = np.zeros((256, 256), int), np.zeros((256, 256, 3))
    say("boundary_mismatched_shapes_refused", refused("risk-zones", c, small_z, P) and refused("glow-self", c, small_z, P)
        and refused("glow-focus-map", c, z, P, reference=small_c), cases)

    speck = z.copy()
    speck[100, 100] = 9
    holes = np.where(np.indices((64, 64)).sum(0) % 7 == 0, 3, 1)
    holes[10:20, 10:20] = NO_SOURCE
    say("median_k1_is_identity", np.array_equal(median_zmap(speck, 1), speck) and np.array_equal(median_zmap(holes, 1), holes), cases)
    say("median_unset_stay_unset_never_filled", all(np.array_equal(median_zmap(holes, k) == NO_SOURCE, holes == NO_SOURCE) for k in (5, 9, 15)), cases)
    say("median_removes_speck_keeps_step", np.array_equal(median_zmap(speck, 5), z) and np.array_equal(median_zmap(speck, 15), z), cases)
    say("median_speck_gives_no_risk_zone", mk("risk-zones", c, speck, P)[:200].any() and not mk("risk-zones", c, speck, dict(P, median=5))[:200].any(), cases)
    return cases


def self_check():
    ok = True
    for name, v in run_cases().items():
        print(f"classical  {name:52} {'ok  ' if v else 'FAIL'}")
        ok &= v
    for brk in ("edges-from-luma", "far-from-opposite-side"):
        caught = not all(run_cases(brk).values())
        print(f"classical  broken variant {brk:34} {'caught' if caught else 'NOT CAUGHT'}")
        ok &= caught
    return ok


# ---------------------------------------------------------------- tuning, writing, mapping


def measures(arm):
    return ("recall",) if arm == "risk-zones" else ("auc", "sign")


def candidates(arm):
    """Parameter sets of the grid; map A has no `sigma`."""
    keys = [k for k in GRID if k != "sigma" or arm != "risk-zones"]
    return [dict(zip(keys, v)) for v in itertools.product(*(GRID[k] for k in keys))]


def read_case(scenes, name, seed, method):
    """Composite, Z-Map, depth-map composite as reference (None for depth-map), label and contour geometry of one scene and method."""
    d = os.path.join(scenes, f"{name}-s{seed}")
    ref = None if method == "depth-map" else ss.read_tiff(os.path.join(d, "depth-map.tif"))
    return (ss.read_tiff(os.path.join(d, f"{method}.tif")), gff.read_zmap(os.path.join(d, f"{method}-zmap.tif")), ref,
            np.load(os.path.join(d, f"{method}-label.npy")), halo_gate.scene_geometry(scenes, name, seed))


def tune_case(job):
    """Gate stats of every grid candidate of every arm on one calibration scene and method."""
    scenes, name, seed, method = job
    comp, z, ref, label, geometry = read_case(scenes, name, seed, method)
    signals = {"glow-self": ss.luma(comp.astype(float))}
    if ref is not None:
        signals["glow-focus-map"] = signals["glow-self"] - ss.luma(ref.astype(float))
    stats = lambda m, unsigned: halo_gate.case_stats(m, label, geometry, method, unsigned)  # noqa: E731
    out = {}
    for med in GRID["median"]:
        zm = median_zmap(z, med)
        for thr, hi in itertools.product(GRID["threshold"], GRID["band_hi"]):
            dist, side = risk_bands(zm, thr, 2 * hi)
            out[("risk-zones", thr, hi, None, med)] = stats(np.where((dist >= 0) & (dist <= hi), hi + 1 - dist, 0), True)
            for sigma in GRID["sigma"]:
                for arm, signal in signals.items():
                    out[(arm, thr, hi, sigma, med)] = stats(glow_values(signal, dist, side, dict(band_hi=hi, sigma=sigma)), False)
    return method, out


def cmd_tune(args):
    """Scores every candidate on the calibration split only; prints the grid and, per arm, the set with the most
    measure-and-method cells at their limit (ties: the larger summed margin to the limits)."""
    jobs = [(args.scenes, n, s, m) for n, s in ls.SPLITS["calibration"] for m in gff.ALGOS]
    pooled = {}
    with ProcessPoolExecutor(args.workers) as pool:
        for method, out in pool.map(tune_case, jobs):
            for key, st in out.items():
                pooled.setdefault(key, {}).setdefault(method, []).append(st)
    print(f"tune: calibration split {ls.SPLITS['calibration']}")
    for arm in ARMS:
        best = None
        for c in candidates(arm):
            by_method = {m: halo_gate.pool(st) for m, st in pooled[(arm, c["threshold"], c["band_hi"], c.get("sigma"), c["median"])].items()}
            cells = [(r[k], halo_gate.LIMITS[k]) for r in by_method.values() for k in measures(arm)]
            rank = (sum(v >= lim for v, lim in cells), sum(v - lim for v, lim in cells))
            shown = "  ".join(f"{m} " + " ".join(f"{k}={r[k]:.3f}" for k in measures(arm)) for m, r in by_method.items())
            print(f"{arm:15} {c}  cells at limit {rank[0]}/{len(cells)} margin {rank[1]:+.3f}  {shown}")
            best = max(best, (rank, c), key=lambda t: t[0]) if best else (rank, c)
        print(f"CHOSEN {arm}: {best[1]}  (cells at limit {best[0][0]}, summed margin {best[0][1]:+.3f})")


def save_map(out, arm, name, seed, method, m):
    path = os.path.join(out, arm, f"{name}-s{seed}", f"{method}-map.npy")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    np.save(path, m.astype(np.float32))


def cmd_write(args):
    """Maps of every arm for the held-out scenes and methods under `<out>/<arm>/<scene>-s<seed>/<method>-map.npy`."""
    if missing := [a for a in ARMS if a not in PARAMS]:
        sys.exit(f"no frozen parameters for {', '.join(missing)}; run tune and freeze PARAMS first")
    for arm in ARMS:
        print(f"PARAMS {arm}: {PARAMS[arm]}")
    for name, seed in ls.SPLITS["heldout"]:
        for method in gff.ALGOS:
            comp, z, ref, _, _ = read_case(args.scenes, name, seed, method)
            for arm in ARMS:
                if arm != "glow-focus-map" or ref is not None:
                    save_map(args.out, arm, name, seed, method, make_map(arm, comp, z, PARAMS[arm], ref))
            if args.zmap_from:
                zs = gff.read_zmap(os.path.join(args.scenes, f"{name}-s{seed}", f"{args.zmap_from}-zmap.tif"))
                for extra, arm in EXTRA_ARMS.items():
                    save_map(args.out, extra, name, seed, method, make_map(arm, comp, zs, PARAMS[arm]))


def cmd_map(args):
    """One composite and Z-Map (and reference) to one map file."""
    comp, z = ss.read_tiff(args.composite), gff.read_zmap(args.zmap)
    ref = ss.read_tiff(args.reference) if args.reference else None
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    np.save(args.out, make_map(args.arm, comp, z, PARAMS[args.arm], ref).astype(np.float32))


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--self-check", action="store_true")
    sub = p.add_subparsers(dest="cmd")
    t = sub.add_parser("tune", help="grid over the calibration split")
    t.add_argument("--scenes", required=True, help="render directory")
    t.add_argument("--workers", type=int, default=16)
    w = sub.add_parser("write", help="maps of the held-out split with the frozen PARAMS")
    w.add_argument("--scenes", required=True, help="render directory")
    w.add_argument("--out", required=True)
    w.add_argument("--zmap-from", choices=("depth-map",), help="also write the maps of EXTRA_ARMS from this method's Z-Map")
    m = sub.add_parser("map", help="one map from a composite and Z-Map")
    m.add_argument("--arm", choices=ARMS, required=True)
    m.add_argument("--composite", required=True)
    m.add_argument("--zmap", required=True)
    m.add_argument("--reference")
    m.add_argument("--out", required=True)
    args = p.parse_args()
    if args.self_check:
        ok = self_check()
        print("self-check passed" if ok else "self-check FAILED")
        sys.exit(0 if ok else 1)
    if args.cmd is None:
        p.error("a subcommand or --self-check is required")
    {"tune": cmd_tune, "write": cmd_write, "map": cmd_map}[args.cmd](args)


if __name__ == "__main__":
    main()
