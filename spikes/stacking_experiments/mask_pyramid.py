# SPDX-License-Identifier: GPL-3.0-or-later
"""Hard finest-selection fusion with streamed coarse mask pyramids."""

from __future__ import annotations

from dataclasses import dataclass
from hashlib import sha256
from pathlib import Path
import subprocess
import sys

import numpy as np

from . import consistency, fusion, metrics
from .fusion import NO_SOURCE


LEVELS = 5
EPSILON = np.float32(np.finfo(np.float32).eps)


@dataclass(frozen=True)
class MaskPyramidResult:
    """One mask-pyramid composite and its immutable finest winner map."""

    composite: np.ndarray
    zmap: np.ndarray
    uncovered: tuple[int, ...]
    fallbacks: tuple[int, ...]
    partition_bound: float
    level_diagnostics: tuple[dict[str, object], ...]


class _TrackedSource:
    """Count the source reads made by the bounded streaming passes."""

    def __init__(self, source: object) -> None:
        self.source = source
        self.n, self.h, self.w, self.c = source.n, source.h, source.w, source.c
        self.masks = getattr(source, "masks", None)
        self.sample_reads = 0
        self.mask_reads = 0
        self.mask_defaults_generated = 0

    def read(self, frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
        self.sample_reads += 1
        if self.masks is None:
            self.mask_defaults_generated += 1
        else:
            self.mask_reads += 1
        if rect is None:
            return self.source.read(frame)
        return self.source.read(frame, rect)


def partition_bound(frame_count: int) -> float:
    """Return the registered f32 normalization bound for one source count."""
    if frame_count < 1:
        raise ValueError("frame count must be positive")
    terms = 2 * frame_count + 8
    epsilon = float(EPSILON)
    return terms * epsilon / (1.0 - terms * epsilon)


def _shape(height: int, width: int, level: int) -> tuple[int, int]:
    stride = 1 << level
    return (height + stride - 1) // stride, (width + stride - 1) // stride


def finest_winners(source: object) -> np.ndarray:
    """Stream the current Details level-zero strict winner selection."""
    height, width = int(source.h), int(source.w)
    winners = np.full((height, width), NO_SOURCE, dtype=np.uint16)
    best = np.zeros((height, width), dtype=np.float32)
    seen = np.zeros((height, width), dtype=bool)
    for frame in range(int(source.n)):
        view, _mask = consistency._one_frame_views(source, frame)
        entry = view.levels[0]
        live, score = entry.eligible[0], entry.scores[0]
        better = live & (~seen | (score > best))
        winners[better] = frame
        best = np.where(better, score, best)
        seen |= live
        del view, _mask, entry, live, score, better
    return winners


def _mask_levels(winners: np.ndarray, frame: int, rule: str) -> list[np.ndarray]:
    fine = (winners == frame).astype(np.float32)
    if rule == "uniform":
        return [np.ones(_shape(winners.shape[0], winners.shape[1], level), dtype=np.float32) for level in range(LEVELS + 1)]
    levels = [fine]
    if rule == "hard":
        return [fine[::1 << level, ::1 << level] for level in range(LEVELS + 1)]
    if rule != "gaussian":
        raise ValueError("unknown mask rule")
    current = fine[..., None]
    for _level in range(LEVELS):
        current = fusion.reduce(current)
        levels.append(current[..., 0])
    return levels


def _residual_live(mask: np.ndarray, reach: int) -> np.ndarray:
    if mask.all():
        return np.ones(_shape(mask.shape[0], mask.shape[1], LEVELS), dtype=bool)
    return consistency._eligible(mask, reach)[::1 << LEVELS, ::1 << LEVELS]


def _fuse(source: object, rule: str, winners: np.ndarray, respect_coverage: bool = True) -> MaskPyramidResult:
    height, width, channels = int(source.h), int(source.w), int(source.c)
    expected = (height, width)
    if winners.shape != expected:
        raise ValueError("finest winners do not match source geometry")
    numerator = [np.zeros((*_shape(height, width, level), channels), dtype=np.float32) for level in range(LEVELS + 1)]
    denominator = [np.zeros(_shape(height, width, level), dtype=np.float32) for level in range(LEVELS + 1)]
    mean_sum = [np.zeros_like(value) for value in numerator]
    mean_count = [np.zeros(value.shape[:2], dtype=np.uint16) for value in numerator]
    active_hashes = [sha256() for _ in numerator]
    for frame in range(int(source.n)):
        view, mask = consistency._one_frame_views(source, frame)
        masks = _mask_levels(winners, frame, rule)
        entries = [(entry.details[0], entry.eligible[0]) for entry in view.levels]
        entries.append((view.residual_inputs[0], _residual_live(mask, view.residual_reach)))
        for level, ((values, live), weights) in enumerate(zip(entries, masks)):
            if not respect_coverage:
                live = np.ones_like(live, dtype=bool)
            active_weight = np.where(live, weights, 0.0).astype(np.float32)
            active_hashes[level].update(active_weight.tobytes())
            np.add(numerator[level], values.astype(np.float32) * active_weight[..., None], out=numerator[level])
            np.add(denominator[level], active_weight, out=denominator[level])
            np.add(mean_sum[level], np.where(live[..., None], values, 0.0).astype(np.float32), out=mean_sum[level])
            mean_count[level] += live
        del view, mask, masks, entries, values, live, weights, active_weight
    coefficients: list[np.ndarray] = []
    diagnostics: list[dict[str, object]] = []
    uncovered, fallbacks = [], []
    for values, weights, total, count in zip(numerator, denominator, mean_sum, mean_count):
        valid = count != 0
        fallback = valid & (weights < EPSILON)
        coefficient = np.divide(values, weights[..., None], out=np.zeros_like(values), where=weights[..., None] >= EPSILON).astype(np.float32)
        plain_mean = np.divide(total, count[..., None], out=np.zeros_like(total), where=count[..., None] != 0).astype(np.float32)
        coefficient[fallback] = plain_mean[fallback]
        coefficients.append(coefficient)
        uncovered.append(int((~valid).sum()))
        fallbacks.append(int(fallback.sum()))
        diagnostics.append({"active_weight_sha256": active_hashes[len(diagnostics)].hexdigest(),
                            "normalization_denominator_sha256": sha256(weights.tobytes()).hexdigest(),
                            "eligible_support_pixels": int(valid.sum()), "eligible_support_sha256": sha256(valid.tobytes()).hexdigest(),
                            "fallback_pixels": int(fallback.sum()), "fallback_sha256": sha256(fallback.tobytes()).hexdigest(),
                            "uncovered_pixels": int((~valid).sum()), "uncovered_sha256": sha256((~valid).tobytes()).hexdigest()})
    output = coefficients[-1]
    for detail in reversed(coefficients[:-1]):
        output = np.float32(fusion.expand(output, detail.shape[0], detail.shape[1]) + detail)
    output[winners == NO_SOURCE] = 0.0
    return MaskPyramidResult(fusion.quantize_u16(output), winners.copy(), tuple(uncovered), tuple(fallbacks), partition_bound(int(source.n)), tuple(diagnostics))


def fuse_streaming(source: object, rule: str = "gaussian") -> tuple[MaskPyramidResult, dict[str, int]]:
    """Fuse one source using a finest pass and one bounded coefficient pass."""
    if rule not in {"gaussian", "hard", "uniform"}:
        raise ValueError("unknown mask rule")
    tracked = _TrackedSource(source)
    winners = finest_winners(tracked)
    result = _fuse(tracked, rule, winners)
    return result, {"sample_reads": tracked.sample_reads, "mask_reads": tracked.mask_reads,
                    "mask_defaults_generated": tracked.mask_defaults_generated, "frame_count": int(source.n)}


def _slice_winners(winners: np.ndarray, rect: tuple[int, int, int, int]) -> np.ndarray:
    y0, y1, x0, x1 = rect
    return winners[y0:y1, x0:x1]


def fuse_tiled(source: object, rule: str = "gaussian", tile: int = 512, margin: int = 160) -> tuple[MaskPyramidResult, dict[str, int], dict[str, object]]:
    """Fuse genuine tile reads with the global finest map shared by all interiors."""
    if margin < 136:
        raise ValueError("tile margin is below the registered reach")
    if margin % 32:
        raise ValueError("tile margin must preserve the registered 32-pixel phase")
    interior = ((tile - 2 * margin) // 32) * 32
    if interior <= 0:
        raise ValueError("tile margin leaves no interior")
    tracked = _TrackedSource(source)
    winners = finest_winners(tracked)
    image = np.zeros((int(source.h), int(source.w), int(source.c)), dtype=np.uint16)
    zmap = np.full(winners.shape, NO_SOURCE, dtype=np.uint16)
    uncovered = np.zeros(LEVELS + 1, dtype=np.int64)
    fallbacks = np.zeros(LEVELS + 1, dtype=np.int64)
    counts = {"frame_count": int(source.n), "tile_count": 0}
    windows = []

    class Rect:
        def __init__(self, rect: tuple[int, int, int, int]) -> None:
            self.parent, self.rect, self.n, self.c = tracked, rect, tracked.n, tracked.c
            self.h, self.w = rect[1] - rect[0], rect[3] - rect[2]

        def read(self, frame: int) -> tuple[np.ndarray, np.ndarray]:
            return self.parent.read(frame, self.rect)

    xs = [0, *range(interior, int(source.w), interior), int(source.w)]
    ys = [0, *range(interior, int(source.h), interior), int(source.h)]
    for y0, y1 in zip(ys, ys[1:]):
        for x0, x1 in zip(xs, xs[1:]):
            read = (max(0, y0 - margin), min(int(source.h), y1 + margin), max(0, x0 - margin), min(int(source.w), x1 + margin))
            local = _fuse(Rect(read), rule, _slice_winners(winners, read))
            ly0, lx0 = y0 - read[0], x0 - read[2]
            image[y0:y1, x0:x1] = local.composite[ly0:ly0 + y1 - y0, lx0:lx0 + x1 - x0]
            zmap[y0:y1, x0:x1] = local.zmap[ly0:ly0 + y1 - y0, lx0:lx0 + x1 - x0]
            uncovered += np.asarray(local.uncovered)
            fallbacks += np.asarray(local.fallbacks)
            counts["tile_count"] += 1
            windows.append({"interior_xywh": [x0, y0, x1 - x0, y1 - y0], "read_xywh": [read[2], read[0], read[3] - read[2], read[1] - read[0]],
                            "level_diagnostics": local.level_diagnostics, "overlap_accounting": "read halos overlap; these counts are per window"})
    result = MaskPyramidResult(image, zmap, tuple(int(value) for value in uncovered), tuple(int(value) for value in fallbacks), partition_bound(int(source.n)), tuple())
    counts.update({"sample_reads": tracked.sample_reads, "mask_reads": tracked.mask_reads,
                   "mask_defaults_generated": tracked.mask_defaults_generated})
    return result, counts, {"margin": margin, "interior": interior, "vertical": xs[1:-1], "horizontal": ys[1:-1], "windows": windows,
                             "global_finest_winners": True}


def _normalized_sum(weights: np.ndarray) -> float:
    denominator = np.float32(0.0)
    for weight in weights.astype(np.float32):
        denominator = np.float32(denominator + weight)
    total = np.float32(0.0)
    for weight in weights.astype(np.float32):
        total = np.float32(total + np.float32(weight / denominator))
    return float(total)


def _actual_partition_error(winners: np.ndarray, coverage: np.ndarray, rule: str) -> float:
    """Independently sum the registered mask weights on every coefficient plane."""
    maximum = 0.0
    for level in range(LEVELS + 1):
        terms = []
        for frame in range(coverage.shape[0]):
            weight = _mask_levels(winners, frame, rule)[level]
            reach = fusion.residual_reach(LEVELS) if level == LEVELS else fusion.level_reach(level, 8)
            stride = 1 << level
            live = consistency._eligible(coverage[frame], reach)[::stride, ::stride]
            terms.append(np.where(live, weight, 0.0).astype(np.float32))
        denominator = np.zeros_like(terms[0])
        for term in terms:
            denominator = np.float32(denominator + term)
        observed = np.zeros_like(denominator)
        for term in terms:
            observed = np.float32(observed + np.divide(term, denominator, out=np.zeros_like(term), where=denominator >= EPSILON))
        valid = denominator >= EPSILON
        if valid.any():
            maximum = max(maximum, float(np.max(np.abs(observed[valid] - np.float32(1.0)))))
    return maximum


def self_checks(output: Path | None = None) -> list[dict[str, object]]:
    """Exercise mask selection, coverage, partition, and tiled reconstruction."""
    height, width = 545, 547
    ramp = np.arange(height * width, dtype=np.uint32).reshape(height, width)
    frames = np.stack([np.stack([(ramp + 271 * frame + 17 * channel) % 65536 for channel in range(3)], axis=-1)
                       for frame in range(3)]).astype(np.uint16)
    masks = np.ones((3, height, width), dtype=bool)
    masks[1, 273, 271] = False

    class Source:
        n, h, w, c = 3, height, width, 3

        @staticmethod
        def read(frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
            y0, y1, x0, x1 = rect or (0, height, 0, width)
            return frames[frame, y0:y1, x0:x1].astype(np.float64) / 65535.0, masks[frame, y0:y1, x0:x1]

    Source.masks = masks

    winners = finest_winners(Source())
    baseline = fusion.details_level0(frames, masks).winners
    assert np.array_equal(winners, baseline)
    tile_max, tile_labels = 0, 0
    root = Path(__file__).resolve().parents[2]
    artifact_root = output or root / ".scratch" / "mask-pyramid" / "self-check"
    artifact_root.mkdir(parents=True, exist_ok=True)
    pairs = []
    for rule in ("gaussian", "hard", "uniform"):
        whole, _counts = fuse_streaming(Source(), rule)
        tiled, _tile_counts, plan = fuse_tiled(Source(), rule)
        assert len(plan["windows"]) == 9 and plan["global_finest_winners"]
        image_delta = int(np.max(np.abs(whole.composite.astype(np.int64) - tiled.composite.astype(np.int64))))
        label_delta = int(np.count_nonzero(whole.zmap != tiled.zmap))
        tile_max, tile_labels = max(tile_max, image_delta), max(tile_labels, label_delta)
        assert image_delta <= 1 and label_delta == 0
        directory = artifact_root / rule
        directory.mkdir(parents=True, exist_ok=True)
        whole_image, tiled_image = directory / "whole.tif", directory / "tiled.tif"
        whole_zmap, tiled_zmap = directory / "whole-zmap.tif", directory / "tiled-zmap.tif"
        for path, value in ((whole_image, whole.composite), (tiled_image, tiled.composite),
                            (whole_zmap, whole.zmap[..., None]), (tiled_zmap, tiled.zmap[..., None])):
            metrics.gff.write_u16(path, value)
        image_diff = subprocess.run([sys.executable, str(root / "tools" / "tiff-diff.py"), "--tol", "1", str(whole_image), str(tiled_image)], capture_output=True, text=True)
        label_diff = subprocess.run([sys.executable, str(root / "tools" / "tiff-diff.py"), "--tol", "0", "--zmap", str(whole_zmap), str(tiled_zmap)], capture_output=True, text=True)
        assert image_diff.returncode == 0 and label_diff.returncode == 0
        pairs.append({"rule": rule, "dimensions_hwc": list(whole.composite.shape),
                      "whole": str(whole_image), "tiled": str(tiled_image), "whole_zmap": str(whole_zmap), "tiled_zmap": str(tiled_zmap),
                      "whole_sha256": sha256(whole_image.read_bytes()).hexdigest(), "tiled_sha256": sha256(tiled_image.read_bytes()).hexdigest(),
                      "whole_zmap_sha256": sha256(whole_zmap.read_bytes()).hexdigest(), "tiled_zmap_sha256": sha256(tiled_zmap.read_bytes()).hexdigest(),
                      "image_diff": image_diff.stdout.strip(), "label_diff": label_diff.stdout.strip()})
    try:
        fuse_tiled(Source(), "gaussian", margin=135)
    except ValueError as error:
        assert "registered reach" in str(error)
    else:
        raise AssertionError("sub-reach tile margin was accepted")
    try:
        fuse_tiled(Source(), "gaussian", margin=136)
    except ValueError as error:
        assert "32-pixel phase" in str(error)
    else:
        raise AssertionError("phase-incompatible tile margin was accepted")
    impulse = frames.copy()
    impulse[2, 272, 273] = np.clip(impulse[2, 272, 273].astype(np.int64) + 12000, 0, 65535).astype(np.uint16)

    class Impulse:
        n, h, w, c = 3, height, width, 3

        @staticmethod
        def read(frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
            y0, y1, x0, x1 = rect or (0, height, 0, width)
            return impulse[frame, y0:y1, x0:x1].astype(np.float64) / 65535.0, masks[frame, y0:y1, x0:x1]

    Impulse.masks = masks
    plain_impulse, _counts = fuse_streaming(Source(), "gaussian")
    changed_impulse, _counts = fuse_streaming(Impulse(), "gaussian")
    yy, xx = np.ogrid[:height, :width]
    outside = np.maximum(np.abs(yy - 272), np.abs(xx - 273)) > 136
    impulse_difference = np.abs(plain_impulse.composite.astype(np.int64) - changed_impulse.composite.astype(np.int64))
    assert not impulse_difference[outside].any() and np.array_equal(plain_impulse.zmap[outside], changed_impulse.zmap[outside])
    identical = np.full((2, 35, 67, 3), 32768, dtype=np.uint16)

    class Identical:
        n, h, w, c, masks = 2, 35, 67, 3, None

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return identical[frame].astype(np.float64) / 65535.0, np.ones((35, 67), dtype=bool)

    result, _counts = fuse_streaming(Identical())
    assert np.array_equal(result.composite, identical[0]) and np.all(result.zmap == 0)
    forced = np.full((35, 67), 1, dtype=np.uint16)
    selected = _fuse(Identical(), "gaussian", forced)
    identity_delta = int(np.max(np.abs(selected.composite.astype(np.int64) - identical[1].astype(np.int64))))
    assert identity_delta <= 1
    selected_ramp = _fuse(Source(), "gaussian", np.full((height, width), 2, dtype=np.uint16))
    selected_delta = int(np.max(np.abs(selected_ramp.composite.astype(np.int64) - frames[2].astype(np.int64))))
    assert selected_delta <= 1
    for frame_count in (1, 2, 8):
        mono = np.stack([np.full((19, 23, 1), 1000 + frame, dtype=np.uint16) for frame in range(frame_count)])

        class Mono:
            n, h, w, c, masks = frame_count, 19, 23, 1, None

            @staticmethod
            def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
                return mono[frame].astype(np.float64) / 65535.0, np.ones((19, 23), dtype=bool)

        observed, _counts = fuse_streaming(Mono())
        assert observed.zmap.shape == (19, 23) and observed.composite.shape == (19, 23, 1)
    partition_error = 0.0
    for count in (1, 2, 8, 32):
        weights = np.linspace(1.0, float(count), count, dtype=np.float32)
        assert abs(_normalized_sum(weights) - 1.0) <= partition_bound(count)
        probe_winners = (np.indices((33, 35)).sum(axis=0) % count).astype(np.uint16)
        probe_coverage = np.ones((count, 33, 35), dtype=bool)
        if count > 1:
            probe_coverage[0, 16, 17] = False
        for rule in ("gaussian", "hard", "uniform"):
            observed = _actual_partition_error(probe_winners, probe_coverage, rule)
            partition_error = max(partition_error, observed)
            assert observed <= partition_bound(count)
    empty = np.zeros((1, 35, 67), dtype=bool)

    class Empty:
        n, h, w, c, masks = 1, 35, 67, 3, empty

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return identical[0].astype(np.float64) / 65535.0, empty[0]

    uncovered, _counts = fuse_streaming(Empty())
    assert not uncovered.composite.any() and np.all(uncovered.zmap == NO_SOURCE)
    partial = np.ones((2, 35, 67), dtype=bool)
    partial[0] = False
    partial_frames = np.stack([np.zeros((35, 67, 3), dtype=np.uint16), np.full((35, 67, 3), 65535, dtype=np.uint16)])

    class Partial:
        n, h, w, c = 2, 35, 67, 3

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return partial_frames[frame].astype(np.float64) / 65535.0, partial[frame]

    Partial.masks = partial
    wrong = np.zeros((35, 67), dtype=np.uint16)
    covered = _fuse(Partial(), "gaussian", wrong)
    ignored = _fuse(Partial(), "gaussian", wrong, respect_coverage=False)
    covered_pull = metrics.gff.border_pull(covered.composite, Partial())
    ignored_pull = metrics.gff.border_pull(ignored.composite, Partial())
    assert covered.fallbacks[0] > 0 and ignored.fallbacks[0] == 0 and ignored_pull > covered_pull
    holes = np.ones((2, 35, 67), dtype=bool)
    holes[:, 17, 33] = False

    class Hole:
        n, h, w, c, masks = 2, 35, 67, 3, holes

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return partial_frames[frame].astype(np.float64) / 65535.0, holes[frame]

    cleared, _counts = fuse_streaming(Hole())
    assert cleared.zmap[17, 33] == NO_SOURCE and not cleared.composite[17, 33].any()
    return [
        {"name": "mask pyramid finest labels", "mismatched_pixels": int(np.count_nonzero(winners != baseline))},
        {"name": "mask pyramid tile parity", "max_abs": tile_max, "label_mismatched_pixels": tile_labels, "windows": 9},
        {"name": "mask pyramid TIFF pairs", "pairs": pairs},
        {"name": "mask pyramid impulse reach", "reach_pixels": 136, "outside_changed_samples": int(np.count_nonzero(impulse_difference[outside]))},
        {"name": "mask pyramid identity", "max_abs": identity_delta, "all_frame_j_max_abs": selected_delta},
        {"name": "mask pyramid mono frame counts", "counts": [1, 2, 8]},
        {"name": "mask pyramid partition", "max_error": partition_error, "max_bound_n32": partition_bound(32)},
        {"name": "mask pyramid uncovered", "cleared_pixels": 1},
        {"name": "mask pyramid coverage mutant", "covered_border_pull": covered_pull, "ignored_border_pull": ignored_pull},
    ]
