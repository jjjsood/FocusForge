#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Halo gate scorer: recall at a marked-area share, ranking AUC and sign agreement of per-pixel maps."""

import argparse
import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import layered_scene as ls  # noqa: E402

LIMITS = {"recall": 0.80, "auc": 0.75, "sign": 0.90}
# 99.9th percentile of |label| over the far band of the calibration scenes, per method, in 16-bit counts
HALO_THRESHOLD = {"weighted-average": 46.5, "laplacian": 69.8, "depth-map": 72.7}
AREA_SHARE = 0.15  # marked share of the image pixels that map A is cut to
NEAR = 24  # px from the matte contour: where map B is ranked and its sign judged
FG_DEPTH = 1  # foreground frame index of the rendered scenes


def clean(score):
    """Scores as float with NaN (unmeasured) counted as 0, the lowest rank."""
    return np.nan_to_num(np.asarray(score, dtype=np.float64), nan=0.0)


def marked_hits(score, halo, share=AREA_SHARE):
    """Halo pixels inside the top `share` of the image by score; only score > 0 is marked and a tie at the cut counts fractionally."""
    s = clean(score)
    budget = round(share * s.size, 9)
    positive = s[s > 0]
    if positive.size <= budget:
        return float(halo[s > 0].sum())
    k = int(np.ceil(budget))
    cut = np.partition(positive, positive.size - k)[positive.size - k]
    above, tied = s > cut, s == cut
    return float(halo[above].sum() + (budget - above.sum()) / tied.sum() * halo[tied].sum())


def recall_at_area(score, halo, share=AREA_SHARE):
    """Share of halo pixels inside the marked area (NaN when there is no halo pixel)."""
    return marked_hits(score, halo, share) / halo.sum() if halo.any() else float("nan")


def ranking_auc(score, halo):
    """AUC of the score for halo against non-halo pixels, ties counted half (NaN when one class is empty)."""
    if not halo.any() or halo.all():
        return float("nan")
    _, rank = np.unique(clean(score).ravel(), return_inverse=True)
    return ls.auc_by_distance(rank.max() - rank.ravel(), halo.ravel())


def sign_hits(m, label, halo):
    return int((np.sign(clean(m))[halo] == np.sign(label)[halo]).sum())


def sign_agreement(m, label, halo):
    """Share of halo pixels where sign(map) equals sign(label); an unmeasured pixel never agrees."""
    return sign_hits(m, label, halo) / halo.sum() if halo.any() else float("nan")


# ---------------------------------------------------------------- scoring


def scene_geometry(scenes, name, seed):
    """Contour distance (-1 beyond 48 px) and foreground mask of one rendered scene."""
    depth = ls.ss.read_tiff(os.path.join(scenes, f"{name}-s{seed}", "truth-depth.tif"))[..., 0]
    return ls.contour_bands((depth == FG_DEPTH).astype(float))


def case_stats(m, label, geometry, method, unsigned=False):
    """The per-scene counts and band pixels `pool` combines into the pooled measures of one method."""
    dist, inside = geometry
    halo = ~inside & (np.abs(label) >= HALO_THRESHOLD[method])
    st = dict(n_halo=int(halo.sum()), hits=marked_hits(m, halo))
    if not unsigned:
        band = ~inside & (dist >= 0) & (dist <= NEAR)
        st.update(score=np.abs(clean(m))[band].astype(np.float32), halo=halo[band], n_near=int((halo & band).sum()),
                  sign_hits=sign_hits(m, label, halo & band))
    return st


def pool(stats):
    """Pooled measures over the scenes of one method: recall always, `auc` and `sign` when the stats carry them."""
    out = dict(recall=sum(s["hits"] for s in stats) / max(sum(s["n_halo"] for s in stats), 1))
    if "score" in stats[0]:
        out["auc"] = ranking_auc(np.concatenate([s["score"] for s in stats]), np.concatenate([s["halo"] for s in stats]))
        out["sign"] = sum(s["sign_hits"] for s in stats) / max(sum(s["n_near"] for s in stats), 1)
    return out


def score_arm(scenes, maps, unsigned, split="heldout"):
    """{method: pooled measures, or None when the arm has no map for it}; a map for only some scenes, or of the wrong shape, exits."""
    result = {}
    for method in ls.gff.ALGOS:
        files = [os.path.join(maps, f"{n}-s{s}", f"{method}-map.npy") for n, s in ls.SPLITS[split]]
        have = [os.path.exists(f) for f in files]
        if not any(have):
            result[method] = None
            continue
        if not all(have):
            missing = [f for f, h in zip(files, have) if not h]
            sys.exit(f"{method}: map missing for {len(missing)} of {len(files)} {split} scenes, first {missing[0]}")
        stats = []
        for (n, s), f in zip(ls.SPLITS[split], files):
            m, label = np.load(f), np.load(os.path.join(scenes, f"{n}-s{s}", f"{method}-label.npy"))
            if m.shape != label.shape:
                sys.exit(f"{f}: map is {m.shape[1]} x {m.shape[0]}, the label is {label.shape[1]} x {label.shape[0]}")
            stats.append(case_stats(m, label, scene_geometry(scenes, n, s), method, unsigned))
        result[method] = pool(stats)
    return result


def rows(arm, scored, unsigned):
    """Printable rows: arm, method, measure, value, limit, pass/fail (`n/a` for a method without maps)."""
    out = []
    for method, measures in scored.items():
        for k in (("recall",) if unsigned else ("auc", "sign")):
            if measures is None:
                out.append(f"{arm:16} {method:18} {k:7} n/a")
            else:
                v = measures[k]
                out.append(f"{arm:16} {method:18} {k:7} {v:6.3f}  limit >= {LIMITS[k]:.2f}  {'pass' if v >= LIMITS[k] else 'fail'}")
    return out


def self_check():
    """The measures against maps whose answers are known by construction."""
    h = w = 20
    halo = np.zeros((h, w), bool)
    halo[:3, :] = True  # 15 % of the pixels
    perfect = halo.astype(np.float32)
    tie = np.ones((h, w), np.float32)
    tie[:2] = 2.0  # 40 pixels at 2, the rest at 1; the 15 % cut (60 pixels) falls inside the tie of 1s
    tie_halo = np.zeros((h, w), bool)
    tie_halo[:2, :] = True  # 40 halo pixels among the 2s
    tie_halo[10:12, :] = True  # and 40 among the 1s
    lab = np.where(halo, 100.0, 0.0) * np.where(np.indices((h, w)).sum(0) % 2, 1, -1)
    checks = dict(
        perfect_recall=recall_at_area(perfect, halo) == 1.0,
        perfect_auc=ranking_auc(perfect, halo) == 1.0,
        inverted_auc=ranking_auc(1.0 - perfect, halo) == 0.0,
        constant_auc=ranking_auc(np.full((h, w), 3.0), halo) == 0.5,
        tied_recall=abs(recall_at_area(tie, tie_halo) - (40 + 40 * (60 - 40) / 360) / 80) < 1e-12,
        nan_ranks_lowest=recall_at_area(np.where(halo, np.nan, 1.0), halo) == 0.0 and ranking_auc(np.where(halo, np.nan, 1.0), halo) == 0.0,
        nothing_marked=recall_at_area(np.zeros((h, w)), halo) == 0.0,
        sign_perfect=sign_agreement(np.sign(lab) * 5, lab, halo) == 1.0,
        sign_flipped_or_nan=sign_agreement(-np.sign(lab), lab, halo) == 0.0 and sign_agreement(np.full((h, w), np.nan), lab, halo) == 0.0,
    )
    for k, v in checks.items():
        print(f"gate  {k:32} {'ok  ' if v else 'FAIL'}")
    return all(checks.values())


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--self-check", action="store_true")
    sub = p.add_subparsers(dest="cmd")
    sc = sub.add_parser("score", help="held-out gate measures of one arm's map files")
    sc.add_argument("--scenes", required=True, help="render directory")
    sc.add_argument("--maps", required=True, help="arm directory holding <scene>-s<seed>/<method>-map.npy")
    sc.add_argument("--unsigned", action="store_true", help="a map A arm: recall only")
    args = p.parse_args()
    if args.self_check:
        ok = self_check()
        print("self-check passed" if ok else "self-check FAILED")
        sys.exit(0 if ok else 1)
    if args.cmd is None:
        p.error("a subcommand or --self-check is required")
    print("\n".join(rows(os.path.basename(os.path.normpath(args.maps)), score_arm(args.scenes, args.maps, args.unsigned), args.unsigned)))


if __name__ == "__main__":
    main()
