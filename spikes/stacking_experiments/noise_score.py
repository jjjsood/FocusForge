# SPDX-License-Identifier: GPL-3.0-or-later
"""Noise-robust focus-score candidates, their registered constants and checks.

Each arm replaces only the per-pixel measure the current score sums; selection,
eligibility erosion, ties, blending and the window radius stay those of the
method it runs in.
"""

from __future__ import annotations

import contextlib
from hashlib import sha256
import io
import json
from pathlib import Path
import re
import tempfile
import time

import numpy as np

from . import fusion
from .protocol import FAMILIES, FRAME_COUNT, TRAINING_SEEDS, SeedTriple
from .runner import (METHODS, PROVENANCE_MODULES, _case_dir, _focus_fuse, _focus_inputs, _tiled_focus, _write_json, baseline_call_count,
                     case_split, gff, prepare_case, replay_control_checks, replay_focus_baselines, run_focus_configurations,
                     sha256_path, stack_command, tile_boundaries, tile_read_bound)
from .scenes import _apply_hairs, _layout, render_family

GAIN = 0.5
EXPONENT = 1
SMOOTH5_TAPS = (1, 4, 6, 4, 1)
RING_DISK = (-1, 1)
RING_135 = (9, 25)
RING_GAPLESS = (1, 9)
SHORT_FRAME_COUNTS = (3, 5)
FRAME_COUNTS = (*SHORT_FRAME_COUNTS, FRAME_COUNT)
MEMBERS = (("thin-hairs", "default"), ("flat-noise", "noise-250"), ("flat-noise", "noise-1000"))
BASELINE_MEMBERS = tuple((family, member, frames) for family, member in MEMBERS for frames in FRAME_COUNTS)

# Frame SHA-256 of the registered eight-frame members at the first training seed, as written by `write_u16`.
EIGHT_FRAME_SHA256 = {
    ("thin-hairs", "default"): (
        "3d7b511395d5c14d09052eeeb92ec2f6dc075a0272a804ff7399ee4de4eb5fd6",
        "19ea1f4fdd6916ab0f05e60d1a8e4773cef4706620a73ff53ce7748a560c3d84",
        "07f98e7a7b8a72cf947b7969d32697ce72b798867d5d529c570e91f0cfe4cb9d",
        "51b460ac527eb54a8e35e62705e10a9f1dc8d712aea93e273961f112db146355",
        "b2a9336e2a8ae2ba5668a1a1ab9d6fd55d263191814fdfc99248c135630540e1",
        "71a6ac5ca1f509b3e0800c809db6b9e88ca7cc384936053e3106fb555ecf9a9f",
        "d5583e13031e9e8915ca0b04dd7b498fe89554d85338d12ea4330c1f35d912a2",
        "74d867297ab8ad107e7918996b94e4b7daf101d8397c1e6cb0c6c77631b7667b",
    ),
    ("flat-noise", "noise-250"): (
        "8463f693992d464b127f2f9e74a81959e7c90e44c1508e5029e5068cebb8c4e8",
        "4457f3205a00bbfce0cb3e292407058e4bfbbd3445ba4cd2b0821cbd45c62b89",
        "7720dc949bc23b5651a4eba02eaedf3d14a5d25c08dc11316a6e0540b6a7c83b",
        "cd2b830cef8c75816a677505fdaedf7144a3876d13f13b9245a567e21cff7618",
        "e2ed8b245beeadeb626c0ef3d487dcdf0548dfb79066d635adf9d24681565879",
        "6ee7bd7bb22834030bde5e827fdfb5f87ac991204e2c1660f293fe0d9109f7c4",
        "5af5143becf5e7f7312613ec3ec0aa673038d1e20952b5c91019b834fbafc5a6",
        "fc84a592afaa2b3ee1ec82f57675a3ee006224f4e94094fed8b2e43b6724c98e",
    ),
    ("flat-noise", "noise-1000"): (
        "f821917baad83c130331dfb0d512827a9062f60d4b89b32f9eab9d89fa0b286c",
        "d4f91ae23f0aa40c61992753261373a0750d33707c7c45f5fab4408c8de7dc6f",
        "a1cff9e4c1b28114eafc83162792572ba6b28d25b59562437634cc489318b225",
        "d015a00b932ad00fa2c042b360ae99ec2ea92bee74fa6ad5a5fbc3d45c31e0ee",
        "faf2642a6acee01318ce7d79775a997f4574b48f75b88a30a970650f4a226ac2",
        "20d284f9d3db00b9bef2a14fc13cd4b992d8d0ff3dab2394b29e55bb634f0f20",
        "8c8a4f898c000fa8aed1d3588899c9c4f736cb565994debf5a72d1307413b99f",
        "d9f5e7516d2d0e3390ff15423297e65e493f008670f5ce864532973ff3cec0de",
    ),
}


# Composite and winner SHA-256 of the current-score replays on the eight-frame members at the first training seed.
FUSION_REPLAY_SHA256 = {
    ("thin-hairs", "default", "details"): (
        "15cc7970d1db3d2766a2f5fa00bc6b362d2abd0692b792ac15843ab4f5b2fb02",
        "58ff9e81671ea7d8d80e1a49fd4b8c6074784b890b99f5745c4e78cce7bb259b",
    ),
    ("thin-hairs", "default", "weighted-average"): (
        "b3a8797ef573d9d74a0674ada08eb8f246afbb6bf827989d3da9f8631aef17ee",
        "72f391aef7d3880afefce128d0655d5d2bfca36a69a6119dd354a3ca148a808e",
    ),
    ("thin-hairs", "default", "depth-map-replay"): (
        "2d6557a3807e57974f5fcd73c2876ccb3afb91fa752a657f8ae1618df9ac93bb",
        "c38becb5b6435f39633c4340b77a9bb2efe442592f6cfea13a4655bb7b37bf98",
    ),
    ("flat-noise", "noise-250", "details"): (
        "24db160ad0a0d70a16a5a275b399a054aa66f575216b1efbedc74a1ca2b41455",
        "4a571b5a6931da19f0a5d23e4ff43f232fd38a2301154035d81f55dd738453db",
    ),
    ("flat-noise", "noise-250", "weighted-average"): (
        "55532e6dd654a01621277f5c8124f4dd7e5a3322b3361caa8c93af61c3272e5b",
        "147306ea553699dd51bf89250d7c8eb34a3e177908b46452628394babce01373",
    ),
    ("flat-noise", "noise-250", "depth-map-replay"): (
        "9ea2fb6502ae77cef81e1235709321c37751e1659d6834b9f5eefe23ca0f20ad",
        "90b33f7257a52be77a3ff658d3a18c9d69e370fedfa8a9176b23ab3ccf68bb32",
    ),
    ("flat-noise", "noise-1000", "details"): (
        "0651b924109dcbe2f2ea52b269f618cb9864224b94fb73c3241aeea741e270cf",
        "5de4f8db0831ea30d6784e6e840dcf10d627a4e4bbf57d09ac3e927578028bca",
    ),
    ("flat-noise", "noise-1000", "weighted-average"): (
        "84e03ebb4de20263f769b4f794135b7a88c4992aa0b9c84fd594a3312f454783",
        "c1d13f9b5b7c9ba8a362b035cea82dd0b5a88678f18c2d2da834876aa00c4225",
    ),
    ("flat-noise", "noise-1000", "depth-map-replay"): (
        "c98a5480b341332a4b00fe49f41cf58931a07247688ed76343e132aeaccefa09",
        "b8f208295ad0a2ca8d78ba98b33dbc592d8ef8e02e73f25006d97f8e1abd908a",
    ),
}


def noise_floor(scores: np.ndarray, eligible: np.ndarray, gain: float = GAIN) -> np.ndarray:
    """Subtract `(1 + gain)` times the per-pixel least eligible score, clamped at zero.

    Pixels with fewer than two eligible frames have no floor and keep their scores.
    """
    least = np.where(eligible, scores, np.inf).min(axis=0)
    least = np.where(eligible.sum(axis=0) >= 2, least, 0.0).astype(np.float32)
    return np.maximum(scores - np.float32(1.0 + gain) * least, np.float32(0.0)).astype(np.float32)


def smooth5(plane: np.ndarray) -> np.ndarray:
    """Blur HxWxC with the separable `[1,4,6,4,1]/16` kernel, edge-clamped, without decimation."""
    kernel = np.array(SMOOTH5_TAPS, dtype=np.float32) / np.float32(sum(SMOOTH5_TAPS))
    out = plane.astype(np.float32, copy=False)
    for axis in (1, 0):
        padded = np.pad(out, [(2, 2) if a == axis else (0, 0) for a in range(3)], mode="edge")
        size = out.shape[axis]
        out = sum(np.float32(w) * np.take(padded, range(i, i + size), axis=axis) for i, w in enumerate(kernel)).astype(np.float32)
    return out


def smoothed_laplacian(plane: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
    """The current windowed Laplacian score, read from a once-smoothed copy of the detail."""
    return fusion.score_plane(smooth5(detail), radius)


def _offset_mean(plane: np.ndarray, low: int, high: int) -> np.ndarray:
    """Mean over offsets with `low < dx^2 + dy^2 <= high` (`low = -1` includes the centre), edge-clamped."""
    height, width = plane.shape[:2]
    reach = int(np.sqrt(high))
    padded = np.pad(plane.astype(np.float64, copy=False), ((reach, reach), (reach, reach), (0, 0)), mode="edge")
    total, count = np.zeros(plane.shape, dtype=np.float64), 0
    for dy in range(-reach, reach + 1):
        for dx in range(-reach, reach + 1):
            if low < dx * dx + dy * dy <= high:
                total += padded[reach + dy:reach + dy + height, reach + dx:reach + dx + width]
                count += 1
    return total / count


def _ring(plane: np.ndarray, radius: int, ring_low: int, ring_high: int) -> np.ndarray:
    measure = np.abs(_offset_mean(plane, *RING_DISK) - _offset_mean(plane, ring_low, ring_high)).sum(axis=2)
    return fusion.score_plane(measure[..., None].astype(np.float32), radius)


def ring_difference(plane: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
    """`|mean(disk r<=1) - mean(ring 3<r<=5)|` per channel, summed over channels, then windowed."""
    return _ring(plane, radius, *RING_135)


def ring_difference_gapless(plane: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
    """`|mean(disk r<=1) - mean(ring 1<r<=3)|` per channel, summed over channels, then windowed."""
    return _ring(plane, radius, *RING_GAPLESS)


ARMS = {"noise-floor": (None, noise_floor), "smoothed-laplacian": (smoothed_laplacian, None),
        "ring-135": (ring_difference, None), "ring-gapless": (ring_difference_gapless, None)}
# Added reach per arm in units of `2**level` level-0 pixels.
ADDED_REACH = {"noise-floor": 0, "smoothed-laplacian": 2, "ring-135": 3, "ring-gapless": 1}
CONFIGURATIONS = tuple({"id": f"{method}-{arm}", "method": method, "arm": arm, "matched_control": f"{method}-current", "classification": "candidate",
                        "can_advance": True}
                       for method in METHODS for arm in ARMS)
# Tile margins of the current score in level-0 pixels.
PRODUCT_MARGIN = {"laplacian": 160, "weighted-average": 20, "depth-map": 16}


def added_reach(arm: str, level: int) -> int:
    """Level-0 pixels the arm's score reaches beyond the current score at a pyramid level."""
    return ADDED_REACH[arm] * 2**level


def _deepest_level(method: str) -> int:
    return FUSION_PARAMETERS["details"]["levels"] - 1 if method == "laplacian" else 0


def widened_margin(margin: int, arm: str, method: str, levels: int) -> int:
    """Tile margin plus the arm's added reach, rounded up to the pyramid alignment so crops keep the level phase."""
    alignment = 1 << levels
    return -(-(margin + added_reach(arm, _deepest_level(method))) // alignment) * alignment


def floor_erased_pixels(scores: np.ndarray, eligible: np.ndarray, gain: float = GAIN) -> int:
    """Pixels with two or more eligible frames where the floor leaves every eligible score at zero."""
    adjusted = noise_floor(scores, eligible, gain)
    return int(((eligible.sum(axis=0) >= 2) & ~(eligible & (adjusted > 0)).any(axis=0)).sum())


def arm_replay(row: dict[str, object], configuration: dict[str, object], return_plan: bool = False):
    """Run one arm on a baseline input at the recorded tile plan widened by the arm's added reach."""
    score, adjust = ARMS[str(configuration["arm"])]
    method, recorded = str(row["method"]), row["tile_boundaries"]
    frames, coverage = _focus_inputs(row)
    margin = widened_margin(int(recorded["margin"]), str(configuration["arm"]), method, int(recorded["levels"]))
    plan = tile_boundaries(frames.shape[2], frames.shape[1], int(row["tile"]), margin, int(recorded["levels"]))
    if max(tile_read_bound(frames.shape[2], frames.shape[1], plan).values()) > int(row["tile"]):
        raise AssertionError("candidate tile plan exceeds its registered tile size")
    result = _tiled_focus(frames, coverage, method, list(plan["vertical"]), list(plan["horizontal"]), margin, score, adjust=adjust)
    return (result, plan) if return_plan else result


def _family(name: str):
    return next(family for family in FAMILIES if family.name == name)


def hair_clip_fraction(seed: SeedTriple, frames: int) -> float:
    """Fraction of hair pixels whose depth offset is cut short by the `0..frames-1` range."""
    before, metadata = _layout(seed.layout, frames)
    # Lifting the depth by a constant keeps the offsets clear of both clip bounds.
    lift = 1000.0
    lifted = before + lift
    _apply_hairs(np.zeros((*before.shape, 3)), lifted, metadata["hairs"], 2 * int(lift))
    unclipped = lifted - lift
    hair = np.abs(unclipped - before) > 0.5
    after = render_family(_family("thin-hairs"), seed, "default", frames).depth
    return float((np.abs(after[hair] - unclipped[hair]) > 1e-6).mean())


FUSION_PARAMETERS = {"details": {"levels": 5, "radius": 8}, "weighted-average": {"radius": 16},
                     "depth-map-replay": {"score_radius": 4, "smooth_radius": 8}}


def fusion_reference_hashes(adjust: fusion.ScoreAdjust | None = None) -> dict[str, object]:
    """SHA-256 of composite and winner maps of the unmodified score replays on each eight-frame member."""
    methods = {"details": lambda frames: fusion.details(frames, **FUSION_PARAMETERS["details"], score=None, adjust=adjust),
               "weighted-average": lambda frames: fusion.weighted_average(frames, **FUSION_PARAMETERS["weighted-average"], score=None, adjust=adjust),
               "depth-map-replay": lambda frames: fusion.depth_map_replay(frames, **FUSION_PARAMETERS["depth-map-replay"], score=None, adjust=adjust)}
    seed, cases = TRAINING_SEEDS[0], []
    for family, member in MEMBERS:
        frames = render_family(_family(family), seed, member).frames
        for method, replay in methods.items():
            result = replay(frames)
            cases.append({"family": family, "member": member, "method": method,
                          "composite_sha256": sha256(result.composite.tobytes()).hexdigest(),
                          "winners_sha256": sha256(result.winners.tobytes()).hexdigest()})
    return {"seed": seed.text(), "frames": FRAME_COUNT, "parameters": FUSION_PARAMETERS, "cases": cases}


def render_cases(root: Path, seed: SeedTriple, frame_counts: tuple[int, ...], allow_holdout: bool = False) -> dict[str, object]:
    """Write every registered member at each frame count and record the hair clip fractions."""
    split = case_split(seed, allow_holdout)
    cases = []
    for family, member in MEMBERS:
        for frames in frame_counts:
            path, data, _ = prepare_case(root, family, member, seed, allow_holdout, frames)
            cases.append({"family": family, "member": member, "frames": frames, "path": str(path)})
    document = {"split": split, "seed": seed.text(), "cases": cases,
                "hair_clip_fraction": {str(frames): hair_clip_fraction(seed, frames) for frames in frame_counts}}
    _write_json(root / "cases.json", document)
    return document


def _frame_hashes(family: str, member: str, seed: SeedTriple, frames: int) -> tuple[list[str], str]:
    data = render_family(_family(family), seed, member, frames)
    with tempfile.TemporaryDirectory() as directory:
        hashes = []
        for index, frame in enumerate(data.frames):
            path = Path(directory) / f"{index:02d}.tif"
            gff.write_u16(path, frame)
            hashes.append(sha256(path.read_bytes()).hexdigest())
    return hashes, sha256(data.truth.tobytes()).hexdigest()


def _check_short_stack_determinism() -> None:
    for family, member in MEMBERS:
        for frames in SHORT_FRAME_COUNTS:
            first, second = (_frame_hashes(family, member, TRAINING_SEEDS[0], frames) for _ in range(2))
            if first != second or len(first[0]) != frames:
                raise AssertionError(f"{family}/{member} at {frames} frames is not deterministic")


def _check_eight_frame_identity() -> None:
    for (family, member), expected in EIGHT_FRAME_SHA256.items():
        if tuple(_frame_hashes(family, member, TRAINING_SEEDS[0], FRAME_COUNT)[0]) != expected:
            raise AssertionError(f"{family}/{member} eight-frame render differs from the recorded frame hashes")


def _check_clip_fraction_ordering() -> None:
    fractions = [hair_clip_fraction(TRAINING_SEEDS[0], frames) for frames in FRAME_COUNTS]
    # At three frames the depth range is 0..2 and every vertical-hair offset (-1.0) is cut at zero: those 292 of 472 hair pixels are always clipped.
    if not all(a >= b for a, b in zip(fractions, fractions[1:])) or not fractions[0] > fractions[-1] or fractions[0] < 292 / 472:
        raise AssertionError(f"hair clip fraction is not non-increasing in the frame count: {fractions}")


def _check_unregistered_family_refused() -> None:
    colour = next(family for family in FAMILIES if family.name == "colour-edges")
    try:
        render_family(colour, TRAINING_SEEDS[0], None, 5)
    except ValueError:
        return
    raise AssertionError("a family outside the registered members rendered at a short frame count")


def _check_baseline_matrix() -> None:
    if len(BASELINE_MEMBERS) != 9 or baseline_call_count(TRAINING_SEEDS, BASELINE_MEMBERS) != 540:
        raise AssertionError("the registered baseline matrix is not 9 members and 540 runs")
    if baseline_call_count(TRAINING_SEEDS) != 360:
        raise AssertionError("the all-family baseline count changed")
    root = Path("root")
    seed = TRAINING_SEEDS[0]
    if _case_dir(root, "thin-hairs", "default", seed) != _case_dir(root, "thin-hairs", "default", seed, FRAME_COUNT):
        raise AssertionError("eight-frame case directory moved")
    if _case_dir(root, "thin-hairs", "default", seed, 3) == _case_dir(root, "thin-hairs", "default", seed, 5):
        raise AssertionError("short-stack case directories collide")


def _check_stack_command_spelling() -> None:
    argv = stack_command(Path("case"), "laplacian", 512, Path("out"), ("equalize=false", "levels=5"))
    for flag in ("--algorithm", "--no-align", "--no-crop", "--cpu", "--json", "--tile-size", "--output"):
        if flag not in argv:
            raise AssertionError(f"stack command lacks {flag}")
    if "--no-gpu" in argv or "--algo" in argv or argv.count("--set") != 2:
        raise AssertionError("stack command uses an unsupported option or drops a --set")
    aligned = stack_command(Path("case"), "depth-map", 1024, Path("out"), (), align=True)
    if "--align-algorithm" not in aligned or "--no-align" in aligned or "mode=rotation-scale" not in aligned:
        raise AssertionError("aligned stack command is wrong")


def _impulse(value: float, second: float) -> np.ndarray:
    plane = np.zeros((13, 11, 2), dtype=np.float32)
    plane[6, 5] = (value, second)
    return plane


def _check_ring_constant_plane() -> None:
    plane = np.empty((13, 11, 2), dtype=np.float32)
    plane[..., 0], plane[..., 1] = 7.5, 3.0
    for arm in (ring_difference, ring_difference_gapless):
        if np.abs(arm(plane, plane, 1)).max() != 0.0:
            raise AssertionError(f"{arm.__name__} is nonzero on a constant plane")


def _ring_impulse_matches(arm, registered_as) -> bool:
    v = 1000.0
    plane = _impulse(v, 2 * v)
    # Window radius 1 sums the 3x3 neighbourhood of the per-pixel measure; both channels add.
    # Centre: five neighbours see the impulse in the disk (v/5 each). Gapless ring also sees it from the four corners (v/24 each).
    # Four pixels right: six neighbours see it in the 3 < r <= 5 ring (v/52 each); only (3, 0) sees it in the 1 < r <= 3 ring (v/24).
    expected = {
        ring_difference: {(6, 5): 3 * v, (6, 9): 3 * 6 * v / 52},
        ring_difference_gapless: {(6, 5): 3 * (v + 4 * v / 24), (6, 9): 3 * v / 24},
    }[registered_as]
    score = arm(plane, plane, 1)
    return all(np.isclose(score[cell], value, rtol=1e-5) for cell, value in expected.items())


def _check_ring_impulse() -> None:
    for arm in (ring_difference, ring_difference_gapless):
        if not _ring_impulse_matches(arm, arm):
            raise AssertionError(f"{arm.__name__} impulse response differs from its registered geometry")


def _check_ring_geometry_cross_fails() -> None:
    for arm, other in ((ring_difference, ring_difference_gapless), (ring_difference_gapless, ring_difference)):
        if _ring_impulse_matches(arm, other):
            raise AssertionError(f"{arm.__name__} also matches the {other.__name__} geometry")


def _check_noise_floor() -> None:
    scores = np.array([[[4.0, 1.0, 5.0, 8.0]], [[10.0, 10.0, 7.0, 9.0]], [[6.0, 6.0, 2.0, 3.0]]], dtype=np.float32)
    live = np.array([[[True, False, True, False]], [[True, True, False, False]], [[True, True, False, True]]])
    expected = np.array([[[0.0, 0.0, 5.0, 8.0]], [[4.0, 1.0, 7.0, 9.0]], [[0.0, 0.0, 2.0, 3.0]]], dtype=np.float32)
    # columns: all eligible (floor 4); frame 0 out (floor 6); a single eligible frame (floor 0, scores unchanged)
    got = noise_floor(scores, live)
    if got.dtype != np.float32 or not np.array_equal(got, expected):
        raise AssertionError(f"noise_floor returned {got.tolist()}")


def _check_smooth5_kernel() -> None:
    plane = np.zeros((13, 11, 2), dtype=np.float32)
    plane[6, 5] = (256.0, 0.0)
    out = smooth5(plane)
    expected = {(6, 5): 36.0, (6, 6): 24.0, (5, 5): 24.0, (6, 7): 6.0, (4, 5): 6.0, (7, 6): 16.0, (8, 7): 1.0, (6, 8): 0.0}
    if any(out[cell + (0,)] != value for cell, value in expected.items()) or out[..., 1].any() or out.shape != plane.shape or out.sum() != 256.0:
        raise AssertionError("smooth5 does not apply the [1,4,6,4,1]/16 kernel per axis")
    edge = np.zeros((5, 5, 1), dtype=np.float32)
    edge[:, 0] = 16.0
    if smooth5(edge)[2, 0, 0] != 11.0:
        raise AssertionError("smooth5 does not clamp at the edge")


def _check_smoothed_laplacian_keeps_coefficients() -> None:
    rng = np.random.default_rng(5)
    frames = rng.integers(0, 65535, (3, 37, 41, 3)).astype(np.float32)
    plain = fusion.pyramid_views(frames, 3, 8)
    smooth = fusion.pyramid_views(frames, 3, 8, score=smoothed_laplacian)
    for before, after in zip(plain.levels, smooth.levels):
        if not np.array_equal(before.details, after.details) or not np.array_equal(before.gaussian, after.gaussian):
            raise AssertionError("smoothing the scoring copy altered the level coefficients")
        if np.array_equal(before.scores, after.scores):
            raise AssertionError("smoothed score equals the plain score")
        picked = fusion.gather(after.details, after.winners)
        if not np.array_equal(picked, np.take_along_axis(after.details, after.winners[None, ..., None].astype(np.intp), 0)[0]):
            raise AssertionError("selected coefficients are not original coefficients")


def _check_fusion_hook_identity() -> None:
    for adjust in (None, lambda scores, eligible: scores):
        recorded = {(case["family"], case["member"], case["method"]): (case["composite_sha256"], case["winners_sha256"])
                    for case in fusion_reference_hashes(adjust)["cases"]}
        if recorded != FUSION_REPLAY_SHA256:
            moved = sorted(key for key in recorded if recorded[key] != FUSION_REPLAY_SHA256.get(key))
            raise AssertionError(f"score-adjust hook {'identity' if adjust else 'default'} moved the replays of {moved}")


def _check_residual_mean_f64() -> None:
    rng = np.random.default_rng(11)
    inputs = (rng.random((8, 17, 19, 3)) * 65535).astype(np.float32)
    masks = rng.random((8, 17, 19)) < 0.8
    exact = fusion._eligible_mean_f64(inputs, masks).astype(np.float32)
    f32_sum = np.zeros(inputs.shape[1:], dtype=np.float32)
    for frame, mask in zip(inputs, masks):
        f32_sum = np.float32(f32_sum + np.where(mask[..., None], frame, 0.0).astype(np.float32))
    if np.array_equal(f32_sum / np.maximum(masks.sum(axis=0), 1)[..., None].astype(np.float32), exact):
        raise AssertionError("the constructed residual-mean input does not separate f32 from f64 accumulation")
    if not np.array_equal(fusion._eligible_mean(inputs, masks), exact):
        raise AssertionError("the Details residual mean is not the f64 accumulation cast to f32 once")


def _scores_and_live() -> tuple[np.ndarray, np.ndarray]:
    rng = np.random.default_rng(3)
    return (rng.random((4, 9, 11)) * 100).astype(np.float32), rng.random((4, 9, 11)) < 0.8


def _check_noise_floor_nonnegative() -> None:
    scores, live = _scores_and_live()
    if noise_floor(scores, live).min() < 0:
        raise AssertionError("noise_floor returned a negative score")


def _zero_on_equal_contrast(adjust) -> bool:
    scores = np.full((3, 4, 5), 7.0, dtype=np.float32)
    return not adjust(scores, np.ones(scores.shape, dtype=bool)).any()


def _without_floor(scores: np.ndarray, eligible: np.ndarray) -> np.ndarray:
    return np.maximum(scores - np.float32(1.0 + GAIN) * np.zeros_like(scores[0]), np.float32(0.0))


def _check_noise_floor_equal_contrast_zero() -> None:
    if not _zero_on_equal_contrast(noise_floor):
        raise AssertionError("noise_floor leaves a score where every eligible frame shows the same contrast")


def _check_noise_floor_omitted_fails() -> None:
    if _zero_on_equal_contrast(_without_floor):
        raise AssertionError("the equal-contrast row passes with the floor left out")


def _check_noise_floor_one_eligible() -> None:
    scores = np.array([[[9.0]], [[2.0]], [[5.0]]], dtype=np.float32)
    live = np.array([[[False]], [[True]], [[False]]])
    adjusted = noise_floor(scores, live)
    if adjusted[1, 0, 0] != 2.0 or fusion.select(adjusted, live)[0, 0] != 1:
        raise AssertionError("a lone eligible frame lost its unchanged score or its win")


def _check_noise_floor_zero_strip() -> None:
    rng = np.random.default_rng(9)
    frames = rng.integers(0, 65536, (3, 40, 40, 3)).astype(np.float32)
    frames[0, :, 8:32] = 0.0
    coverage = np.ones(frames.shape[:3], dtype=bool)
    coverage[0, :, 8:32] = False
    inside = np.s_[4:36, 16:24]
    if fusion.score_plane(fusion.detail(frames[0]), 3)[inside].max() != 0.0:
        raise AssertionError("the zero strip does not score zero")
    masked = fusion.pyramid_views(frames, 1, 3, coverage, adjust=noise_floor).levels[0].scores[1:, inside[0], inside[1]]
    without = fusion.pyramid_views(frames[1:], 1, 3, coverage[1:], adjust=noise_floor).levels[0].scores[:, inside[0], inside[1]]
    unmasked = fusion.pyramid_views(frames, 1, 3, adjust=noise_floor).levels[0].scores[1:, inside[0], inside[1]]
    if not np.array_equal(masked, without):
        raise AssertionError("a masked zero strip changed the floor")
    if np.array_equal(unmasked, without):
        raise AssertionError("the zero strip does not set the floor when it is eligible")


def _check_smoothed_depth_copy_exact() -> None:
    frames = np.random.default_rng(5).integers(0, 65536, (3, 37, 41, 3), dtype=np.uint16)
    result = fusion.depth_map_replay(frames, score=smoothed_laplacian)
    if not np.array_equal(result.composite, fusion.gather(frames, result.winners)):
        raise AssertionError("the smoothed-score Depth map composite is not a copy of the winning original sample")


def _check_smoothed_copy_variant_fails() -> None:
    frames = np.random.default_rng(5).integers(0, 65536, (3, 37, 41, 3), dtype=np.uint16)
    result = fusion.depth_map_replay(frames, score=smoothed_laplacian)
    variant = fusion.quantize_u16(fusion.gather(np.stack([smooth5(frame) for frame in frames]), result.winners))
    if np.array_equal(result.composite, variant):
        raise AssertionError("a composite copied from the smoothed frames equals the original-sample copy")


def _offsets(low: int, high: int) -> list[tuple[int, int]]:
    return [(dy, dx) for dy in range(-5, 6) for dx in range(-5, 6) if low < dy * dy + dx * dx <= high]


def _direct_ring(plane: np.ndarray, y: int, x: int, radius: int, ring: tuple[int, int]) -> float:
    height, width = plane.shape[:2]

    def mean(py: int, px: int, low: int, high: int) -> np.ndarray:
        taps = [plane[min(max(py + dy, 0), height - 1), min(max(px + dx, 0), width - 1)].astype(np.float64) for dy, dx in _offsets(low, high)]
        return sum(taps) / len(taps)
    window = [(min(max(y + wy, 0), height - 1), min(max(x + wx, 0), width - 1)) for wy in range(-radius, radius + 1) for wx in range(-radius, radius + 1)]
    return float(sum(np.abs(mean(py, px, *RING_DISK) - mean(py, px, *ring)).sum() for py, px in window))


def _check_ring_direct_reference() -> None:
    if [len(_offsets(*taps)) for taps in (RING_DISK, RING_135, RING_GAPLESS)] != [5, 52, 24]:
        raise AssertionError("ring offset counts differ from the registered 5, 52 and 24")
    plane = (np.random.default_rng(13).random((15, 17, 3)) * 1000).astype(np.float32)
    for arm, ring in ((ring_difference, RING_135), (ring_difference_gapless, RING_GAPLESS)):
        score = arm(plane, plane, 2)
        for y, x in ((0, 0), (7, 8), (14, 16), (3, 15)):
            if not np.isclose(score[y, x], _direct_ring(plane, y, x, 2, ring), rtol=1e-4):
                raise AssertionError(f"{arm.__name__} at {(y, x)} differs from the direct disk and ring reference")


def _check_shortest_member_defined() -> None:
    frames = render_family(_family("thin-hairs"), TRAINING_SEEDS[0], "default", min(SHORT_FRAME_COUNTS)).frames
    for arm, (score, adjust) in ARMS.items():
        view = fusion.pyramid_views(frames, FUSION_PARAMETERS["details"]["levels"], FUSION_PARAMETERS["details"]["radius"], score=score, adjust=adjust)
        if any(not np.isfinite(level.scores).all() or (level.winners == fusion.NO_SOURCE).any() for level in view.levels):
            raise AssertionError(f"{arm} leaves a non-finite score or an undefined Details winner on the three-frame member")
        for method in ("weighted-average", "depth-map"):
            if (_focus_fuse(frames, None, method, score, adjust=adjust).winners == fusion.NO_SOURCE).any():
                raise AssertionError(f"{arm} leaves an undefined {method} winner on the three-frame member")


def _score_reach(method: str, score, adjust, limit: int) -> int:
    """Smallest Chebyshev distance beyond which no sample moves the deepest selection score at the centre.

    Every sample farther than the tested distance is replaced by independent noise, so the result is exact
    for the sampled content. Frame 0 has a quarter of the contrast of the others so the noise floor of
    arm 1 stays below the other scores.
    """
    level = _deepest_level(method)
    centre = -(-(limit + 1) // 32) * 32
    size = 2 * centre + 1
    rng = np.random.default_rng(31)
    scale = np.array([0.25, 1.0, 1.0], dtype=np.float32)[:, None, None, None]
    base, other = (np.float32(32768) + (rng.random((3, size, size, 3), dtype=np.float32) - np.float32(0.5)) * np.float32(65535) * scale
                   for _ in range(2))
    outside = np.abs(np.indices((size, size)) - centre).max(axis=0)

    def centre_scores(frames: np.ndarray) -> np.ndarray:
        recorded = []

        def record(scores: np.ndarray, eligible: np.ndarray) -> np.ndarray:
            adjusted = scores if adjust is None else adjust(scores, eligible)
            recorded.append(adjusted[:, centre >> len(recorded), centre >> len(recorded)].copy())
            return adjusted
        _focus_fuse(frames, None, method, score, adjust=record)
        return recorded[level]
    reference = centre_scores(base)

    def moved(distance: int) -> bool:
        return not np.array_equal(centre_scores(np.where((outside > distance)[None, ..., None], other, base)), reference)
    if moved(limit):
        raise AssertionError(f"{method} score at the centre moves beyond the search limit {limit}")
    low, high = (0, 0) if not moved(0) else (0, limit)
    while high - low > 1:
        middle = (low + high) // 2
        low, high = (middle, high) if moved(middle) else (low, middle)
    return high


_CONTROL_REACH: dict[str, int] = {}


def arm_reach(method: str, arm: str, variant: tuple | None = None) -> dict[str, object]:
    """Measure one arm's score reach against the current score's plus the registered added reach."""
    score, adjust = ARMS[arm] if variant is None else variant
    level, smoothing = _deepest_level(method), FUSION_PARAMETERS["depth-map-replay"]["smooth_radius"] if method == "depth-map" else 0
    erosion = {"laplacian": fusion.level_reach(level, FUSION_PARAMETERS["details"]["radius"]),
               "weighted-average": FUSION_PARAMETERS["weighted-average"]["radius"] + 4,
               "depth-map": 2 * FUSION_PARAMETERS["depth-map-replay"]["score_radius"] + smoothing}[method]
    if method not in _CONTROL_REACH:
        _CONTROL_REACH[method] = _score_reach(method, None, None, erosion + 2 ** (level + 1) + 2)
    bound = _CONTROL_REACH[method] + added_reach(arm, level)
    measured = _score_reach(method, score, adjust, bound + 2 ** (level + 1) + 2)
    return {"method": method, "arm": arm, "level": level, "control_reach": _CONTROL_REACH[method], "added_reach": added_reach(arm, level),
            "registered_bound": bound, "measured_reach": measured, "smoothing_reach": smoothing, "erosion": erosion,
            "within_registered_bound": measured <= bound, "exceeds_erosion": measured + smoothing > erosion}


def _reach_check(method: str, arm: str):
    def check() -> None:
        row = arm_reach(method, arm)
        if not row["within_registered_bound"]:
            raise AssertionError(f"{method}/{arm} reaches {row['measured_reach']}, past the registered {row['registered_bound']}")
    return check


def _check_arm_reach_wider_variant_fails() -> None:
    def twice(plane: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
        return fusion.score_plane(smooth5(smooth5(detail)), radius)
    if arm_reach("weighted-average", "smoothed-laplacian", (twice, None))["within_registered_bound"]:
        raise AssertionError("a doubly smoothed score stays within the registered added reach")


def _tile_check(configuration: dict[str, object]):
    def check() -> None:
        method, arm = str(configuration["method"]), str(configuration["arm"])
        score, adjust = ARMS[arm]
        frames = np.random.default_rng(27).integers(0, 65536, size=(2, 544, 544, 3), dtype=np.uint16)
        coverage = np.ones(frames.shape[:3], dtype=bool)
        coverage[:, 272, 272] = False
        one = _focus_fuse(frames, coverage, method, score, adjust=adjust)
        margin = widened_margin(PRODUCT_MARGIN[method], arm, method, FUSION_PARAMETERS["details"]["levels"] if method == "laplacian" else 1)
        tiled = _tiled_focus(frames, coverage, method, [192, 384], [192, 384], margin, score, adjust=adjust)
        delta = np.abs(one.composite.astype(np.int64) - tiled.composite.astype(np.int64))
        if delta.max() > (1 if method == "laplacian" else 0) or not np.array_equal(one.winners, tiled.winners):
            raise AssertionError(f"{configuration['id']} tiled at margin {margin} differs from one tile by {delta.max()} on "
                                 f"{(delta != 0).sum()} samples and {(one.winners != tiled.winners).sum()} Z-map pixels")
    return check


def _replay_control(name: str):
    def check() -> None:
        if name not in replay_control_checks():
            raise AssertionError(f"{name} is not rejected")
    return check


def _arms_workspace(root: Path) -> tuple[Path, Path, Path, str]:
    registration = root / "report.md"
    registration.write_text("# Report\n\n### Registration\n\nfrozen text\n\n## Findings\n\nlater\n")
    runs = root / "runs"
    runs.mkdir()
    (runs / "baseline.json").write_text("[]\n")
    (runs / "allowances.json").write_text("[]\n")
    registered = registration_sha256(registration)
    checkpoint = root / "checkpoint.json"
    write_checkpoint(registration, runs, checkpoint, registered)
    return registration, checkpoint, runs, registered


def _edit_checkpoint(checkpoint: Path, edit) -> None:
    document = json.loads(checkpoint.read_text())
    edit(document)
    checkpoint.write_text(json.dumps(document))


def _refusal(mutate):
    def check() -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            registration, checkpoint, runs, registered = _arms_workspace(root)
            mutate(registration, checkpoint, runs)
            out = root / "out"
            for refused in (lambda: validate_arms(registration, checkpoint, runs, registered),
                            lambda: run_arms(registration, checkpoint, runs, out, True, [], registered)):
                try:
                    refused()
                except ValueError:
                    continue
                raise AssertionError("arms accepted an input that no longer matches the registration or checkpoint")
            if out.exists():
                raise AssertionError("a refused arms run created its output directory")
    return check


def _check_arms_accepts_bound_inputs() -> None:
    with tempfile.TemporaryDirectory() as directory:
        registration, checkpoint, runs, registered = _arms_workspace(Path(directory))
        if validate_arms(registration, checkpoint, runs, registered)["registration_sha256"] != registered:
            raise AssertionError("validation did not return the bound registration hash")


def _check_arms_exits_nonzero() -> None:
    from . import __main__ as command

    with tempfile.TemporaryDirectory() as directory:
        out = Path(directory) / "out"
        argv = ["noise-score", "arms", "--split", "training", "--registration", str(Path(directory) / "missing.md"),
                "--checkpoint", str(Path(directory) / "checkpoint.json"), "--runs", directory, "--out", str(out)]
        message = io.StringIO()
        with contextlib.redirect_stderr(message):
            try:
                command.main(argv)
            except SystemExit as error:
                if not error.code or out.exists() or "--registration must name" not in message.getvalue():
                    raise AssertionError("a refused arms command exited zero, created its output directory or failed for another reason")
                return
        raise AssertionError("the arms command accepted a missing registration")


def _break(name: str, value):
    return lambda registration, checkpoint, runs: _edit_checkpoint(checkpoint, lambda document: document.update({name: value}))


def _break_constant(registration: Path, checkpoint: Path, runs: Path) -> None:
    _edit_checkpoint(checkpoint, lambda document: document["constants"].update({"gain": 0.75}))


def _break_module(registration: Path, checkpoint: Path, runs: Path) -> None:
    _edit_checkpoint(checkpoint, lambda document: document["module_sha256"].update({"fusion.py": "0" * 64}))


def _break_configuration(registration: Path, checkpoint: Path, runs: Path) -> None:
    _edit_checkpoint(checkpoint, lambda document: document["configurations"].append("laplacian-ring-9"))


REFUSALS = (
    ("arms-refuses-missing-registration", lambda registration, checkpoint, runs: registration.unlink()),
    ("arms-refuses-missing-section", lambda registration, checkpoint, runs: registration.write_text("# Report\n\n## Findings\n")),
    ("arms-refuses-altered-registration", lambda registration, checkpoint, runs: registration.write_text(registration.read_text().replace("frozen", "thawed"))),
    ("arms-refuses-missing-checkpoint", lambda registration, checkpoint, runs: checkpoint.unlink()),
    ("arms-refuses-checkpoint-registration", _break("registration_sha256", "0" * 64)),
    ("arms-refuses-stale-modules", _break_module),
    ("arms-refuses-unregistered-configuration", _break_configuration),
    ("arms-refuses-changed-constant", _break_constant),
    ("arms-refuses-changed-baseline", lambda registration, checkpoint, runs: (runs / "baseline.json").write_text("[ ]\n")),
    ("arms-refuses-changed-allowances", lambda registration, checkpoint, runs: (runs / "allowances.json").write_text("[ ]\n")),
)


REGISTRATION_SHA256 = "d6e0b6b59261b54d20111b99d2d72dce9fd78c62afb57fbb14f708b77ba5e8ec"
CHECKPOINT_SCHEMA = 1
REPLAY_ROWS = 270
SESSION_SECONDS = 7200


def registration_sha256(path: Path) -> str:
    """SHA-256 of the report text from the `### Registration` heading up to the next `## ` heading."""
    if not path.is_file():
        raise ValueError("--registration must name the registration report")
    text = path.read_text(encoding="utf-8")
    start = re.search(r"^### Registration$", text, re.MULTILINE)
    if start is None:
        raise ValueError("registration report has no `### Registration` section")
    end = re.compile(r"^## ", re.MULTILINE).search(text, start.end())
    return sha256(text[start.start():None if end is None else end.start() - 1].encode("utf-8")).hexdigest()


def registered_constants() -> dict[str, object]:
    """The constants an arm run is bound to, in checkpoint form."""
    return {"gain": GAIN, "exponent": EXPONENT, "smoothing_taps": list(SMOOTH5_TAPS), "ring_disk": list(RING_DISK),
            "ring_135": list(RING_135), "ring_gapless": list(RING_GAPLESS), "added_reach": ADDED_REACH,
            "frame_counts": list(FRAME_COUNTS), "members": [list(member) for member in MEMBERS]}


def _module_hashes() -> dict[str, str]:
    return {name: sha256_path(Path(__file__).with_name(name)) for name in PROVENANCE_MODULES}


def _runs_hashes(runs: Path) -> tuple[str, str]:
    paths = (runs / "baseline.json", runs / "allowances.json")
    if not all(path.is_file() for path in paths):
        raise ValueError("--runs must hold baseline.json and allowances.json")
    return tuple(sha256_path(path) for path in paths)


def write_checkpoint(registration: Path, runs: Path, out: Path, registered: str = REGISTRATION_SHA256) -> dict[str, object]:
    """Bind the registration, baseline records, constants, configurations and live module hashes."""
    section = registration_sha256(registration)
    if section != registered:
        raise ValueError("registration section differs from the registered text")
    baseline, allowances = _runs_hashes(runs)
    document = {"schema_version": CHECKPOINT_SCHEMA, "registration_sha256": section, "baseline_sha256": baseline,
                "allowances_sha256": allowances, "constants": registered_constants(),
                "configurations": [configuration["id"] for configuration in CONFIGURATIONS], "module_sha256": _module_hashes()}
    _write_json(out, document)
    return document


def validate_arms(registration: Path, checkpoint: Path, runs: Path, registered: str = REGISTRATION_SHA256) -> dict[str, object]:
    """Refuse, before any output exists, a run whose registration, checkpoint or inputs no longer match."""
    section = registration_sha256(registration)
    if section != registered:
        raise ValueError("registration section differs from the registered text")
    if not checkpoint.is_file():
        raise ValueError("--checkpoint must name the execution checkpoint")
    try:
        document = json.loads(checkpoint.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("checkpoint is not valid JSON") from error
    if document.get("schema_version") != CHECKPOINT_SCHEMA or document.get("registration_sha256") != section:
        raise ValueError("checkpoint binds a different registration")
    if document.get("module_sha256") != _module_hashes():
        raise ValueError("checkpoint module hashes differ from the live rig files")
    if document.get("configurations") != [configuration["id"] for configuration in CONFIGURATIONS]:
        raise ValueError("checkpoint configurations differ from the registered twelve")
    if document.get("constants") != json.loads(json.dumps(registered_constants())):
        raise ValueError("checkpoint constants differ from the registered ones")
    if (document.get("baseline_sha256"), document.get("allowances_sha256")) != _runs_hashes(runs):
        raise ValueError("baseline or allowances records differ from the checkpoint")
    return {**document, "checkpoint_sha256": sha256_path(checkpoint)}


def floor_erased_records(runs: Path) -> list[dict[str, object]]:
    """Per member, seed and method, the pixels whose eligible scores the arm-1 floor clears (Details per level)."""
    records = []
    for row in json.loads((runs / "baseline.json").read_text()):
        if row["repeat"] != 0 or row["tile"] != 1024:
            continue
        counts: list[int] = []

        def count(scores: np.ndarray, eligible: np.ndarray) -> np.ndarray:
            counts.append(floor_erased_pixels(scores, eligible))
            return scores
        frames, coverage = _focus_inputs(row)
        _focus_fuse(frames, coverage, str(row["method"]), adjust=count)
        records.append({"family": row["family"], "member": row["member"], "frames": row["frames"], "seed": row["seed"],
                        "method": row["method"], "floor_erased_pixels": counts})
    return records


def run_arms(registration: Path, checkpoint: Path, runs: Path, out: Path, replay_only: bool, command: list[str],
             registered: str = REGISTRATION_SHA256) -> dict[str, object]:
    """Validate the bindings, replay the current score exactly, then run the twelve arm configurations."""
    started = time.monotonic()
    bound = validate_arms(registration, checkpoint, runs, registered)
    budget = {"session_cap_seconds": SESSION_SECONDS, "started_monotonic": started, "exhausted": False}
    replay = replay_focus_baselines(runs, REPLAY_ROWS)
    out.mkdir(parents=True, exist_ok=True)
    candidates, floor_erased, reach = [], [], []
    if not replay_only:
        candidates = run_focus_configurations(runs, out, [dict(configuration) for configuration in CONFIGURATIONS], runs / "allowances.json",
                                              budget, configure=arm_replay)
        for record in candidates:
            record["registration_sha256"], record["checkpoint_sha256"] = bound["registration_sha256"], bound["checkpoint_sha256"]
        floor_erased = floor_erased_records(runs)
        reach = [arm_reach(method, arm) for method in METHODS for arm in ARMS]
    budget["measured_seconds"] = time.monotonic() - budget.pop("started_monotonic")
    comparison = {"command": command, "registration_sha256": bound["registration_sha256"], "checkpoint_sha256": bound["checkpoint_sha256"],
                  "replay": replay, "candidate_rows": candidates, "floor_erased": floor_erased, "reach": reach, "budget": budget}
    _write_json(out / "comparison.json", comparison)
    return comparison


CHECKS = (
    ("short-stack-determinism", _check_short_stack_determinism),
    ("eight-frame-frame-hashes", _check_eight_frame_identity),
    ("hair-clip-fraction-ordering", _check_clip_fraction_ordering),
    ("unregistered-family-refused", _check_unregistered_family_refused),
    ("baseline-matrix", _check_baseline_matrix),
    ("stack-command-spelling", _check_stack_command_spelling),
    ("ring-constant-plane", _check_ring_constant_plane),
    ("ring-impulse", _check_ring_impulse),
    ("noise-floor", _check_noise_floor),
    ("smooth5-kernel", _check_smooth5_kernel),
    ("smoothed-laplacian-keeps-coefficients", _check_smoothed_laplacian_keeps_coefficients),
    ("fusion-hook-identity", _check_fusion_hook_identity),
    ("residual-mean-f64", _check_residual_mean_f64),
    ("noise-floor-nonnegative", _check_noise_floor_nonnegative),
    ("noise-floor-equal-contrast-zero", _check_noise_floor_equal_contrast_zero),
    ("noise-floor-omitted-fails", _check_noise_floor_omitted_fails),
    ("noise-floor-one-eligible", _check_noise_floor_one_eligible),
    ("noise-floor-zero-strip", _check_noise_floor_zero_strip),
    ("smoothed-depth-copy-exact", _check_smoothed_depth_copy_exact),
    ("smoothed-copy-variant-fails", _check_smoothed_copy_variant_fails),
    ("ring-direct-reference", _check_ring_direct_reference),
    ("ring-geometry-cross-fails", _check_ring_geometry_cross_fails),
    ("shortest-member-defined", _check_shortest_member_defined),
    ("replay-image-rejected", _replay_control("replay-image-rejected")),
    ("replay-winner-rejected", _replay_control("replay-winner-rejected")),
    ("arms-accepts-bound-inputs", _check_arms_accepts_bound_inputs),
    *((name, _refusal(mutate)) for name, mutate in REFUSALS),
    ("arms-exits-nonzero", _check_arms_exits_nonzero),
    *((f"arm-reach-{method}-{arm}", _reach_check(method, arm)) for method in METHODS for arm in ARMS),
    ("arm-reach-wider-variant-fails", _check_arm_reach_wider_variant_fails),
    *((f"arm-tile-{configuration['id']}", _tile_check(configuration)) for configuration in CONFIGURATIONS),
)


def self_checks() -> list[str]:
    """Run every check and return one `name: ok` line each; a failed check raises."""
    lines = []
    for name, check in CHECKS:
        check()
        lines.append(f"{name}: ok")
    return lines
