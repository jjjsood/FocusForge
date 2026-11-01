# SPDX-License-Identifier: GPL-3.0-or-later
"""Noise-derived shrinkage of immutable selected Laplacian details."""

from __future__ import annotations

from dataclasses import dataclass
import numpy as np

from . import consistency, fusion
from .fusion import NO_SOURCE


@dataclass(frozen=True)
class ShrinkResult:
    """One streamed Details reconstruction with unchanged source maps."""

    composite: np.ndarray
    zmap: np.ndarray
    raw_winners: tuple[np.ndarray, ...]
    final_winners: tuple[np.ndarray, ...]
    residual: np.ndarray
    thresholds: tuple[float, ...]


def shrink_tuple(detail: np.ndarray, threshold: float) -> np.ndarray:
    """Shrink every tuple channel by one f64-energy factor."""
    if threshold < 0.0 or not np.isfinite(threshold):
        raise ValueError("threshold must be finite and nonnegative")
    if threshold == 0.0:
        return detail
    energy = np.sum(np.asarray(detail, dtype=np.float64) ** 2, axis=-1)
    factor = np.divide(energy - threshold, energy, out=np.zeros_like(energy), where=energy > threshold)
    return np.asarray(detail * factor[..., None], dtype=np.float32)


def _mad(values: np.ndarray) -> float | None:
    finite = np.asarray(values, dtype=np.float64)
    finite = finite[np.isfinite(finite)]
    if finite.size == 0:
        return None
    center = float(np.median(finite))
    return float(np.median(np.abs(finite - center)) / 0.6745)


def estimate_thresholds(source: object, multiplier: float) -> tuple[float, ...]:
    """Estimate one global squared-energy threshold per level without frame retention."""
    if multiplier < 0.0 or not np.isfinite(multiplier):
        raise ValueError("multiplier must be finite and nonnegative")
    if multiplier == 0.0:
        return (0.0,) * 5
    per_level: list[list[float]] = [[] for _ in range(5)]
    for frame in range(int(source.n)):
        view, _mask = consistency._one_frame_views(source, frame)
        for level, entry in enumerate(view.levels):
            valid = entry.eligible[0] & np.isfinite(entry.details[0]).all(axis=2)
            estimate = _mad(entry.details[0][valid])
            if estimate is not None:
                per_level[level].append(estimate)
    if any(not values for values in per_level):
        raise ValueError("noise estimate has empty support")
    return tuple(float(multiplier * float(np.median(values)) ** 2) for values in per_level)


def match_strength(target: float | None, trials: list[tuple[float, float | None]]) -> tuple[float, float, float] | None:
    """Choose registered finite control strength by closest available noise."""
    if target is None or not np.isfinite(target):
        return None
    eligible = [(strength, measured) for strength, measured in trials
                if measured is not None and np.isfinite(measured)]
    if not eligible:
        return None
    strength, measured = min(eligible, key=lambda item: abs(float(item[1]) - target))
    return float(strength), float(measured), abs(float(measured) - target)


def fuse_streaming(source: object, multiplier: float, thresholds: tuple[float, ...] | None = None,
                   attenuation: float | None = None) -> tuple[ShrinkResult, dict[str, int]]:
    """Select once, then shrink copied details in a second bounded sample pass."""
    if attenuation is not None and (attenuation < 0.0 or attenuation > 1.0 or not np.isfinite(attenuation)):
        raise ValueError("attenuation must be finite in [0, 1]")
    estimated = thresholds is None and multiplier != 0.0
    values = estimate_thresholds(source, multiplier) if thresholds is None else thresholds
    if len(values) != 5 or any(value < 0.0 or not np.isfinite(value) for value in values):
        raise ValueError("five finite nonnegative thresholds are required")
    height, width = int(source.h), int(source.w)
    raw = [np.full(((height + (1 << level) - 1) // (1 << level),
                   (width + (1 << level) - 1) // (1 << level)), NO_SOURCE, dtype=np.uint16) for level in range(5)]
    best = [np.zeros(plane.shape, dtype=np.float32) for plane in raw]
    seen = [np.zeros(plane.shape, dtype=bool) for plane in raw]
    residual_sum: np.ndarray | None = None
    residual_count: np.ndarray | None = None
    for frame in range(int(source.n)):
        view, mask = consistency._one_frame_views(source, frame)
        for level, entry in enumerate(view.levels):
            eligible, score = entry.eligible[0], entry.scores[0]
            better = eligible & (~seen[level] | (score > best[level]))
            raw[level][better] = frame
            best[level][better] = score[better]
            seen[level] |= eligible
        residual_mask = np.ones(view.residual_inputs.shape[1:3], dtype=bool) if mask.all() else consistency._eligible(mask, view.residual_reach)[::32, ::32]
        residual = view.residual_inputs[0]
        if residual_sum is None:
            residual_sum = np.zeros_like(residual, dtype=np.float32)
            residual_count = np.zeros(residual.shape[:2], dtype=np.uint16)
        residual_sum = np.float32(residual_sum + np.where(residual_mask[..., None], residual, 0.0).astype(np.float32))
        residual_count += residual_mask
    if residual_sum is None or residual_count is None:
        raise ValueError("source has no frames")
    selected = [np.zeros((*plane.shape, int(source.c)), dtype=np.float32) for plane in raw]
    for frame in range(int(source.n)):
        view, _mask = consistency._one_frame_views(source, frame)
        for level, entry in enumerate(view.levels):
            chosen = raw[level] == frame
            detail = entry.details[0]
            selected_detail = shrink_tuple(detail, values[level]) if attenuation is None else np.asarray(detail * attenuation, dtype=np.float32)
            selected[level][chosen] = selected_detail[chosen]
    output = np.divide(residual_sum, residual_count[..., None], out=np.zeros_like(residual_sum), where=residual_count[..., None] != 0).astype(np.float32)
    residual = output.copy()
    for detail in reversed(selected):
        output = np.float32(fusion.expand(output, detail.shape[0], detail.shape[1]) + detail)
    result = ShrinkResult(fusion.quantize_u16(output), raw[0], tuple(raw), tuple(raw), residual, tuple(float(value) for value in values))
    samples = int(source.n * (3 if estimated else 2))
    masks = samples
    masked = consistency._has_file_masks(source)
    return result, {"sample_reads": samples, "estimator_sample_reads": int(source.n) if estimated else 0,
                    "extra_mask_pass_reads": 0,
                    "total_mask_reads": masks if masked else 0,
                    "mask_defaults_generated": masks if not masked else 0, "frame_count": int(source.n)}


def fuse_tiled(source: object, multiplier: float, tile: int, margin: int = 160) -> tuple[ShrinkResult, dict[str, int], dict[str, object]]:
    """Assemble phase-aligned tile interiors using one unchanged global threshold vector."""
    thresholds = estimate_thresholds(source, multiplier)
    interior = ((tile - 2 * margin) // 32) * 32
    if interior <= 0:
        raise ValueError("tile margin leaves no interior")
    xs, ys = [0, *range(interior, source.w, interior), source.w], [0, *range(interior, source.h, interior), source.h]
    shapes = [((source.h + (1 << level) - 1) // (1 << level), (source.w + (1 << level) - 1) // (1 << level)) for level in range(5)]
    raw = [np.full(shape, NO_SOURCE, dtype=np.uint16) for shape in shapes]
    image = np.zeros((source.h, source.w, source.c), dtype=np.uint16)
    residual = np.zeros(((source.h + 31) // 32, (source.w + 31) // 32, source.c), dtype=np.float32)
    estimator_reads = int(source.n) if multiplier != 0.0 else 0
    counters: dict[str, int] = {"frame_count": int(source.n), "tile_count": 0, "sample_reads": estimator_reads,
                                "estimator_sample_reads": estimator_reads, "total_mask_reads": estimator_reads if consistency._has_file_masks(source) else 0,
                                "mask_defaults_generated": estimator_reads if not consistency._has_file_masks(source) else 0}
    windows = []

    class Rect:
        def __init__(self, rect: tuple[int, int, int, int]) -> None:
            self.parent, self.rect, self.n, self.c = source, rect, source.n, source.c
            self.h, self.w = rect[1] - rect[0], rect[3] - rect[2]

        def read(self, frame: int) -> tuple[np.ndarray, np.ndarray]:
            return self.parent.read(frame, self.rect)

    for y0, y1 in zip(ys, ys[1:]):
        for x0, x1 in zip(xs, xs[1:]):
            oy0, oy1, ox0, ox1 = max(0, y0 - margin), min(source.h, y1 + margin), max(0, x0 - margin), min(source.w, x1 + margin)
            result, counts = fuse_streaming(Rect((oy0, oy1, ox0, ox1)), multiplier, thresholds)
            image[y0:y1, x0:x1] = result.composite[y0 - oy0:y1 - oy0, x0 - ox0:x1 - ox0]
            for level, stride in enumerate((1, 2, 4, 8, 16)):
                gy0, gy1, gx0, gx1 = y0 // stride, (y1 + stride - 1) // stride, x0 // stride, (x1 + stride - 1) // stride
                ly0, ly1, lx0, lx1 = (y0 - oy0) // stride, (y1 - oy0 + stride - 1) // stride, (x0 - ox0) // stride, (x1 - ox0 + stride - 1) // stride
                raw[level][gy0:gy1, gx0:gx1] = result.raw_winners[level][ly0:ly1, lx0:lx1]
            ry0, ry1, rx0, rx1 = y0 // 32, (y1 + 31) // 32, x0 // 32, (x1 + 31) // 32
            lry0, lry1, lrx0, lrx1 = (y0 - oy0) // 32, (y1 - oy0 + 31) // 32, (x0 - ox0) // 32, (x1 - ox0 + 31) // 32
            residual[ry0:ry1, rx0:rx1] = result.residual[lry0:lry1, lrx0:lrx1]
            counters["tile_count"] += 1
            for key, value in counts.items():
                if key != "frame_count":
                    counters[key] = counters.get(key, 0) + value
            windows.append({"interior_xywh": [x0, y0, x1 - x0, y1 - y0], "read_xywh": [ox0, oy0, ox1 - ox0, oy1 - oy0]})
    return ShrinkResult(image, raw[0], tuple(raw), tuple(raw), residual, thresholds), counters, {"margin": margin, "interior": interior, "vertical": xs[1:-1], "horizontal": ys[1:-1], "windows": windows}


def self_checks() -> list[str]:
    tiny = np.array([[[1.0e-20, -2.0e-20, 3.0e-20]]], dtype=np.float32)
    assert shrink_tuple(tiny, 0.0) is tiny
    assert np.array_equal(shrink_tuple(np.array([[[3.0, -4.0]]], dtype=np.float32), 25.0), np.zeros((1, 1, 2), dtype=np.float32))
    scaled = shrink_tuple(np.array([[[3.0, -4.0]]], dtype=np.float32), 9.0)
    assert np.array_equal(scaled, np.array([[[48.0 / 25.0, -64.0 / 25.0]]], dtype=np.float32))
    frames = np.arange(3 * 35 * 67 * 3, dtype=np.uint16).reshape(3, 35, 67, 3)
    coverage = np.ones((3, 35, 67), dtype=bool)
    coverage[0, 0, 0] = False

    class Source:
        n, h, w, c, masks = 3, 35, 67, 3, coverage

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return frames[frame].astype(np.float64) / 65535.0, coverage[frame]

    identity, counts = fuse_streaming(Source(), 0.0)
    current, _ = consistency.fuse_streaming(Source(), "identity")
    assert np.array_equal(identity.composite, current.composite)
    assert np.array_equal(identity.zmap, current.zmap)
    assert all(np.array_equal(left, right) for left, right in zip(identity.raw_winners, identity.final_winners))
    assert counts["sample_reads"] == 6
    attenuated, _counts = fuse_streaming(Source(), 0.0, attenuation=0.5)
    assert not np.array_equal(attenuated.composite, identity.composite)
    assert match_strength(1.0, [(0.25, 1.5), (0.5, 0.5)]) == (0.25, 1.5, 0.5)
    assert match_strength(1.0, [(0.25, None)]) is None
    try:
        empty = type("Empty", (), {"n": 1, "h": 1, "w": 1, "c": 3, "masks": None,
                                     "read": staticmethod(lambda _frame: (np.zeros((1, 1, 3)), np.zeros((1, 1), dtype=bool)))})()
        estimate_thresholds(empty, 1.0)
    except ValueError as error:
        assert str(error) == "noise estimate has empty support"
    else:
        raise AssertionError("empty support was accepted")
    constant = np.full((2, 193, 197, 3), 32768, dtype=np.uint16)
    variants = (np.ones((2, 193, 197), dtype=bool), np.pad(np.ones((2, 189, 193), dtype=bool), ((0, 0), (2, 2), (2, 2))))
    checks = 0
    for masks in variants:
        class ConstantSource:
            n, h, w, c = 2, 193, 197, 3

            @staticmethod
            def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
                return constant[frame].astype(np.float64) / 65535.0, masks[frame]

        reference, _counts = fuse_streaming(ConstantSource(), 0.0)
        for multiplier in (0.0, 1.0, 2.0, 4.0, 8.0):
            observed, _counts = fuse_streaming(ConstantSource(), multiplier)
            assert np.array_equal(observed.composite, reference.composite)
            assert np.array_equal(observed.zmap, reference.zmap)
            checks += 1
    assert checks == 10
    tiled_frames = np.arange(3 * 545 * 547 * 3, dtype=np.uint16).reshape(3, 545, 547, 3)
    tiled_masks = np.ones((3, 545, 547), dtype=bool)
    tiled_masks[0, :2] = False

    class TiledSource:
        n, h, w, c, masks = 3, 545, 547, 3, tiled_masks

        @staticmethod
        def read(frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
            y0, y1, x0, x1 = rect or (0, 545, 0, 547)
            return tiled_frames[frame, y0:y1, x0:x1].astype(np.float64) / 65535.0, tiled_masks[frame, y0:y1, x0:x1]

    for multiplier in (0.0, 2.0):
        whole, _counts = fuse_streaming(TiledSource(), multiplier)
        tiled, _counts, plan = fuse_tiled(TiledSource(), multiplier, 512)
        assert len(plan["windows"]) == 9 and whole.thresholds == tiled.thresholds
        assert np.array_equal(whole.composite, tiled.composite) and np.array_equal(whole.zmap, tiled.zmap)
        assert all(np.array_equal(left, right) for left, right in zip(whole.raw_winners, tiled.raw_winners))
        assert np.array_equal(whole.residual, tiled.residual)
    one, two = estimate_thresholds(TiledSource(), 1.0), estimate_thresholds(TiledSource(), 2.0)
    assert np.allclose(two, np.asarray(one) * 2.0, rtol=0.0, atol=0.0)
    return ["shrink tuple", "shrink identity", "shrink coverage", "shrink matching", "shrink empty support", "shrink noise-free controls", "shrink tiled global thresholds"]
