# SPDX-License-Identifier: GPL-3.0-or-later
"""Noise-robust focus-score candidates, their registered constants and checks.

Each arm replaces only the per-pixel measure the current score sums; selection,
eligibility erosion, ties, blending and the window radius stay those of the
method it runs in.
"""

from __future__ import annotations

from hashlib import sha256
from pathlib import Path
import tempfile

import numpy as np

from . import fusion
from .protocol import FAMILIES, FRAME_COUNT, TRAINING_SEEDS, SeedTriple
from .runner import _case_dir, _write_json, baseline_call_count, case_split, gff, prepare_case, stack_command
from .scenes import _apply_hairs, _layout, render_family

GAIN = 0.5
EXPONENT = 1
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


def noise_floor(scores: np.ndarray, eligible: np.ndarray, gain: float = GAIN) -> np.ndarray:
    """Subtract `(1 + gain)` times the per-pixel least eligible score, clamped at zero.

    Pixels with fewer than two eligible frames have no floor and keep their scores.
    """
    least = np.where(eligible, scores, np.inf).min(axis=0)
    least = np.where(eligible.sum(axis=0) >= 2, least, 0.0).astype(np.float32)
    return np.maximum(scores - np.float32(1.0 + gain) * least, np.float32(0.0)).astype(np.float32)


def smooth5(plane: np.ndarray) -> np.ndarray:
    """Blur HxWxC with the separable `[1,4,6,4,1]/16` kernel, edge-clamped, without decimation."""
    kernel = np.array([1, 4, 6, 4, 1], dtype=np.float32) / np.float32(16)
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
    measure = np.abs(_offset_mean(plane, -1, 1) - _offset_mean(plane, ring_low, ring_high)).sum(axis=2)
    return fusion.score_plane(measure[..., None].astype(np.float32), radius)


def ring_difference(plane: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
    """`|mean(disk r<=1) - mean(ring 3<r<=5)|` per channel, summed over channels, then windowed."""
    return _ring(plane, radius, 9, 25)


def ring_difference_gapless(plane: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
    """`|mean(disk r<=1) - mean(ring 1<r<=3)|` per channel, summed over channels, then windowed."""
    return _ring(plane, radius, 1, 9)


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


def fusion_reference_hashes() -> dict[str, object]:
    """SHA-256 of composite and winner maps of the unmodified score replays on each eight-frame member."""
    methods = {"details": lambda frames: fusion.details(frames, **FUSION_PARAMETERS["details"], score=None),
               "weighted-average": lambda frames: fusion.weighted_average(frames, **FUSION_PARAMETERS["weighted-average"], score=None),
               "depth-map-replay": lambda frames: fusion.depth_map_replay(frames, **FUSION_PARAMETERS["depth-map-replay"], score=None)}
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
        raise AssertionError("stack command uses a retired option or drops a --set")
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


def _check_ring_impulse() -> None:
    v = 1000.0
    plane = _impulse(v, 2 * v)
    # Window radius 1 sums the 3x3 neighbourhood of the per-pixel measure; both channels add.
    # Centre: five neighbours see the impulse in the disk (v/5 each). Gapless ring also sees it from the four corners (v/24 each).
    # Four pixels right: six neighbours see it in the 3 < r <= 5 ring (v/52 each); only (3, 0) sees it in the 1 < r <= 3 ring (v/24).
    expected = {
        ring_difference: {(6, 5): 3 * v, (6, 9): 3 * 6 * v / 52},
        ring_difference_gapless: {(6, 5): 3 * (v + 4 * v / 24), (6, 9): 3 * v / 24},
    }
    for arm, cells in expected.items():
        score = arm(plane, plane, 1)
        for cell, value in cells.items():
            if not np.isclose(score[cell], value, rtol=1e-5):
                raise AssertionError(f"{arm.__name__} at {cell} is {score[cell]}, expected {value}")


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
)


def self_checks() -> list[str]:
    """Run every check and return one `name: ok` line each; a failed check raises."""
    lines = []
    for name, check in CHECKS:
        check()
        lines.append(f"{name}: ok")
    return lines
