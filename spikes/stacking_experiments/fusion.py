# SPDX-License-Identifier: GPL-3.0-or-later
"""Independent NumPy selection twins for current CPU fusion rules."""

from __future__ import annotations

from dataclasses import dataclass
from collections.abc import Callable
import numpy as np

NO_SOURCE = np.uint16(65535)


@dataclass(frozen=True)
class LevelView:
    """One pyramid level's details, scores, eligibility, and selected source."""

    details: np.ndarray
    scores: np.ndarray
    eligible: np.ndarray
    winners: np.ndarray


@dataclass(frozen=True)
class PyramidLevel:
    """Gaussian/detail inputs and coverage for one pyramid level."""

    gaussian: np.ndarray
    details: np.ndarray
    raw_coverage: np.ndarray | None
    score_radius: int
    reach: int
    scores: np.ndarray
    eligible: np.ndarray
    winners: np.ndarray


@dataclass(frozen=True)
class PyramidView:
    """Per-level inputs plus final residual inputs."""

    levels: tuple[PyramidLevel, ...]
    residual_inputs: np.ndarray
    residual_reach: int


@dataclass(frozen=True)
class FusionResult:
    """One full fusion replay result with its level-zero source map."""

    composite: np.ndarray
    winners: np.ndarray
    eligible: np.ndarray


Score = Callable[[np.ndarray, np.ndarray, int], np.ndarray]
ScoreAdjust = Callable[[np.ndarray, np.ndarray], np.ndarray]


def _f32(value: np.ndarray | float) -> np.ndarray | np.float32:
    return np.asarray(value, dtype=np.float32) if isinstance(value, np.ndarray) else np.float32(value)


def _edge_index(index: int, size: int) -> int:
    return min(max(index, 0), size - 1)


def reduce(plane: np.ndarray) -> np.ndarray:
    """Reduce one HxWxC plane with the pinned five-tap f32 kernel."""
    height, width, channels = plane.shape
    source = plane.astype(np.float32, copy=False)
    out_w, out_h = (width + 1) // 2, (height + 1) // 2
    kernel = tuple(np.float32(v) for v in (1 / 16, 4 / 16, 6 / 16, 4 / 16, 1 / 16))
    x = 2 * np.arange(out_w)
    padded = np.pad(source, ((0, 0), (2, 2), (0, 0)), mode="edge")
    horizontal = np.zeros((height, out_w, channels), dtype=np.float32)
    for offset, weight in zip(range(-2, 3), kernel):
        horizontal = np.float32(horizontal + np.float32(padded[:, x + offset + 2] * weight))
    y = 2 * np.arange(out_h)
    padded = np.pad(horizontal, ((2, 2), (0, 0), (0, 0)), mode="edge")
    output = np.zeros((out_h, out_w, channels), dtype=np.float32)
    for offset, weight in zip(range(-2, 3), kernel):
        output = np.float32(output + np.float32(padded[y + offset + 2] * weight))
    return output


def expand(plane: np.ndarray, height: int, width: int) -> np.ndarray:
    """Expand one plane using the pinned even/odd f32 kernels."""
    src_h, src_w, channels = plane.shape
    source = plane.astype(np.float32, copy=False)
    xi = np.arange(width) // 2
    horizontal = np.empty((src_h, width, channels), dtype=np.float32)
    even = np.arange(width) % 2 == 0
    horizontal[:, even] = np.float32(
        np.float32(source[:, np.clip(xi[even] - 1, 0, src_w - 1)] * np.float32(0.125))
        + np.float32(source[:, xi[even]] * np.float32(0.75))
        + np.float32(source[:, np.clip(xi[even] + 1, 0, src_w - 1)] * np.float32(0.125))
    )
    horizontal[:, ~even] = np.float32(
        np.float32(source[:, xi[~even]] * np.float32(0.5))
        + np.float32(source[:, np.clip(xi[~even] + 1, 0, src_w - 1)] * np.float32(0.5))
    )
    yi = np.arange(height) // 2
    output = np.empty((height, width, channels), dtype=np.float32)
    even = np.arange(height) % 2 == 0
    output[even] = np.float32(
        np.float32(horizontal[np.clip(yi[even] - 1, 0, src_h - 1)] * np.float32(0.125))
        + np.float32(horizontal[yi[even]] * np.float32(0.75))
        + np.float32(horizontal[np.clip(yi[even] + 1, 0, src_h - 1)] * np.float32(0.125))
    )
    output[~even] = np.float32(
        np.float32(horizontal[yi[~even]] * np.float32(0.5))
        + np.float32(horizontal[np.clip(yi[~even] + 1, 0, src_h - 1)] * np.float32(0.5))
    )
    return output


def detail(plane: np.ndarray) -> np.ndarray:
    """Return level-zero detail, preserving f32 arithmetic."""
    return np.float32(plane.astype(np.float32) - expand(reduce(plane), plane.shape[0], plane.shape[1]))


def _reduce_coverage(mask: np.ndarray) -> np.ndarray:
    """Keep a reduced sample valid only when its clamped 2x2 footprint is valid."""
    height, width = mask.shape
    padded = np.pad(mask, ((0, height % 2), (0, width % 2)), mode="edge")
    return padded[0::2, 0::2] & padded[0::2, 1::2] & padded[1::2, 0::2] & padded[1::2, 1::2]


def level_score_radius(radius: int, level: int) -> int:
    """Return the Details score radius for one pyramid level."""
    if level < 0 or radius < 1:
        raise ValueError("level must be nonnegative and radius positive")
    return max(1, radius >> level)


def level_reach(level: int, radius: int) -> int:
    """Return level-zero support reach for one score plane."""
    score_radius = level_score_radius(radius, level)
    return 2 * (2 ** (level + 1) - 1) + 2**level * score_radius


def residual_reach(levels: int) -> int:
    """Return level-zero support reach of the final Gaussian residual."""
    if levels < 1:
        raise ValueError("levels must be positive")
    return 2 * (2**levels - 1)


def ascending_f32_mean(inputs: np.ndarray) -> np.ndarray:
    """Average frame inputs in ascending order with f32 accumulation."""
    if inputs.ndim != 4 or inputs.shape[0] == 0:
        raise ValueError("inputs must be nonempty N,H,W,C")
    total = np.zeros_like(inputs[0], dtype=np.float32)
    for frame in inputs:
        total = np.float32(total + frame.astype(np.float32, copy=False))
    return np.float32(total / np.float32(inputs.shape[0]))


def per_level_eligible(coverage: np.ndarray, level: int, radius: int) -> np.ndarray:
    """Erode original coverage by full-resolution reach, then sample one pyramid level."""
    if coverage.ndim != 3:
        raise ValueError("coverage must be N,H,W")
    stride = 2**level
    reach = level_reach(level, radius)
    return np.stack([eligible(mask, reach)[::stride, ::stride] for mask in coverage])


def pyramid_views(frames: np.ndarray, levels: int, radius: int = 8, coverage: np.ndarray | None = None,
                  score: Score | None = None, adjust: ScoreAdjust | None = None) -> PyramidView:
    """Build Gaussian/detail levels and independent score winners without collapse.

    `adjust` receives each level's N,H,W scores and eligibility and returns the scores selection reads.
    """
    if frames.ndim != 4 or frames.shape[0] == 0:
        raise ValueError("frames must be nonempty N,H,W,C")
    if levels < 1:
        raise ValueError("levels must be positive")
    if coverage is not None and coverage.shape != frames.shape[:3]:
        raise ValueError("coverage must be N,H,W")
    gaussian = frames.astype(np.float32, copy=False)
    current_coverage = coverage
    result = []
    for level in range(levels):
        reduced = np.stack([reduce(frame) for frame in gaussian])
        details = np.stack([np.float32(frame - expand(next_frame, frame.shape[0], frame.shape[1]))
                            for frame, next_frame in zip(gaussian, reduced)])
        score_radius = level_score_radius(radius, level)
        scores = np.stack([score_plane(detail_plane, score_radius) if score is None else score(plane, detail_plane, score_radius)
                           for plane, detail_plane in zip(gaussian, details)])
        eligible_masks = np.ones(scores.shape, dtype=bool) if coverage is None else per_level_eligible(coverage, level, radius)
        if adjust is not None:
            scores = adjust(scores, eligible_masks)
        result.append(PyramidLevel(gaussian, details, current_coverage, score_radius, level_reach(level, radius), scores,
                                   eligible_masks, select(scores, eligible_masks)))
        gaussian = reduced
        if current_coverage is not None:
            current_coverage = np.stack([_reduce_coverage(mask) for mask in current_coverage])
    return PyramidView(tuple(result), gaussian, residual_reach(levels))


def score_plane(plane: np.ndarray, radius: int) -> np.ndarray:
    """Separable ascending-order absolute-detail score."""
    height, width, channels = plane.shape
    source = plane.astype(np.float32, copy=False)
    padded = np.pad(source, ((0, 0), (radius, radius), (0, 0)), mode="edge")
    rows = np.zeros((height, width), dtype=np.float32)
    for dx in range(-radius, radius + 1):
        shifted = padded[:, dx + radius:dx + radius + width]
        for channel in range(channels):
            rows = np.float32(rows + np.abs(shifted[..., channel]))
    padded = np.pad(rows, ((radius, radius), (0, 0)), mode="edge")
    output = np.zeros((height, width), dtype=np.float32)
    for dy in range(-radius, radius + 1):
        output = np.float32(output + padded[dy + radius:dy + radius + height])
    return output


def score_plane_f32_alternate(plane: np.ndarray, radius: int) -> np.ndarray:
    """Deliberately different f32 row accumulation for order calibration."""
    height, width, channels = plane.shape
    source = plane.astype(np.float32, copy=False)
    padded = np.pad(source, ((0, 0), (radius, radius), (0, 0)), mode="edge")
    rows = np.zeros((height, width), dtype=np.float32)
    for channel in range(channels):
        for dx in range(-radius, radius + 1):
            rows = np.float32(rows + np.abs(padded[:, dx + radius:dx + radius + width, channel]))
    padded_rows = np.pad(rows, ((radius, radius), (0, 0)), mode="edge")
    output = np.zeros((height, width), dtype=np.float32)
    for dy in range(-radius, radius + 1):
        output = np.float32(output + padded_rows[dy + radius:dy + radius + height])
    return output


def score_plane_f64_reference(plane: np.ndarray, radius: int) -> np.ndarray:
    """Reference f64 score order over the same f32 detail coefficients."""
    height, width, channels = plane.shape
    source = plane.astype(np.float32, copy=False)
    padded = np.pad(source, ((0, 0), (radius, radius), (0, 0)), mode="edge")
    rows = np.zeros((height, width), dtype=np.float64)
    for dx in range(-radius, radius + 1):
        shifted = padded[:, dx + radius:dx + radius + width]
        for channel in range(channels):
            rows += np.abs(shifted[..., channel], dtype=np.float32).astype(np.float64)
    padded_rows = np.pad(rows, ((radius, radius), (0, 0)), mode="edge")
    output = np.zeros((height, width), dtype=np.float64)
    for dy in range(-radius, radius + 1):
        output += padded_rows[dy + radius:dy + radius + height]
    return output


def select_f64(scores: np.ndarray, live: np.ndarray) -> np.ndarray:
    """Apply the identical first-eligible strict argmax to f64 scores."""
    frames, height, width = scores.shape
    winners = np.full((height, width), NO_SOURCE, dtype=np.uint16)
    best = np.zeros((height, width), dtype=np.float64)
    any_live = np.zeros((height, width), dtype=bool)
    for frame in range(frames):
        better = live[frame] & (~any_live | (scores[frame] > best))
        winners[better] = frame
        best = np.where(better, scores[frame], best)
        any_live |= live[frame]
    return winners


def eligible(coverage: np.ndarray, reach: int) -> np.ndarray:
    """Erode coverage by a Chebyshev square of the requested reach."""
    padded = np.pad(coverage, reach, mode="edge")
    output = np.ones_like(coverage, dtype=bool)
    height, width = coverage.shape
    for dy in range(2 * reach + 1):
        for dx in range(2 * reach + 1):
            output &= padded[dy:dy + height, dx:dx + width]
    return output


def select(scores: np.ndarray, live: np.ndarray) -> np.ndarray:
    """Pick the first eligible strict maximum, with no-source for empty pixels."""
    frames, height, width = scores.shape
    winners = np.full((height, width), NO_SOURCE, dtype=np.uint16)
    best = np.zeros((height, width), dtype=np.float32)
    any_live = np.zeros((height, width), dtype=bool)
    for frame in range(frames):
        better = live[frame] & (~any_live | (scores[frame] > best))
        winners[better] = frame
        best = np.where(better, scores[frame], best)
        any_live |= live[frame]
    return winners


def details_level0(frames: np.ndarray, coverage: np.ndarray | None = None, radius: int = 8) -> LevelView:
    """Expose level-zero Details selection intermediates."""
    details = np.stack([detail(frame) for frame in frames])
    scores = np.stack([score_plane(frame, radius) for frame in details])
    masks = np.ones(scores.shape, dtype=bool) if coverage is None else np.stack([eligible(mask, 2 + radius) for mask in coverage])
    return LevelView(details, scores, masks, select(scores, masks))


def depth_map(frames: np.ndarray, coverage: np.ndarray | None = None, score_radius: int = 4, smooth_radius: int = 8) -> LevelView:
    """Expose Depth map score smoothing and selected winners."""
    details = np.stack([detail(frame) for frame in frames])
    raw = np.stack([score_plane(frame, score_radius) for frame in details])
    scores = np.stack([score_plane(plane[..., None], smooth_radius) for plane in raw])
    masks = np.ones(scores.shape, dtype=bool) if coverage is None else np.stack([eligible(mask, 16) for mask in coverage])
    return LevelView(details, scores, masks, select(scores, masks))


def gather(frames: np.ndarray, winners: np.ndarray) -> np.ndarray:
    """Copy selected encoded samples, zeroing no-source pixels."""
    output = np.zeros_like(frames[0])
    yy, xx = np.indices(winners.shape)
    live = winners != NO_SOURCE
    output[live] = frames[winners[live], yy[live], xx[live]]
    return output


def quantize_u16(plane: np.ndarray) -> np.ndarray:
    """Mirror product f32 clamping and half-up u16 quantization."""
    return np.floor(np.clip(plane.astype(np.float32), 0, 65535) + np.float32(0.5)).astype(np.uint16)


def _eligible_mean(inputs: np.ndarray, masks: np.ndarray) -> np.ndarray:
    return _eligible_mean_f64(inputs, masks).astype(np.float32)


def _eligible_mean_f64(inputs: np.ndarray, masks: np.ndarray) -> np.ndarray:
    total = np.zeros_like(inputs[0], dtype=np.float64)
    count = np.zeros(inputs.shape[1:3], dtype=np.uint16)
    for frame, mask in zip(inputs, masks):
        total += np.where(mask[..., None], frame, 0.0).astype(np.float64)
        count += mask
    return np.divide(total, count[..., None], out=np.zeros_like(total), where=count[..., None] != 0)


def details(frames: np.ndarray, coverage: np.ndarray | None = None, levels: int = 5, radius: int = 8,
            score: Score | None = None, adjust: ScoreAdjust | None = None) -> FusionResult:
    """Replay Details selection, residual averaging, and reverse collapse."""
    view = pyramid_views(frames, levels, radius, coverage, score, adjust)
    selected = [gather(level.details, level.winners).astype(np.float32) for level in view.levels]
    residual_masks = np.ones(view.residual_inputs.shape[:3], dtype=bool) if coverage is None else np.stack(
        [eligible(mask, view.residual_reach) for mask in coverage])[..., ::2**levels, ::2**levels]
    output = _eligible_mean(view.residual_inputs, residual_masks)
    for level, selected_detail in zip(reversed(view.levels), reversed(selected)):
        output = np.float32(expand(output, selected_detail.shape[0], selected_detail.shape[1]) + selected_detail)
    return FusionResult(quantize_u16(output), view.levels[0].winners, view.levels[0].eligible)


def weighted_average(frames: np.ndarray, coverage: np.ndarray | None = None, radius: int = 16,
                     score: Score | None = None, adjust: ScoreAdjust | None = None) -> FusionResult:
    """Replay Weighted Average with f64 score-power accumulators."""
    details = np.stack([detail(frame) for frame in frames])
    scores = np.stack([score_plane(value, radius) if score is None else score(frame, value, radius)
                       for frame, value in zip(frames, details)])
    masks = np.ones(scores.shape, dtype=bool) if coverage is None else np.stack([eligible(mask, radius + 4) for mask in coverage])
    if adjust is not None:
        scores = adjust(scores, masks)
    numerator = np.zeros_like(frames[0], dtype=np.float64)
    denominator = np.zeros(scores.shape[1:], dtype=np.float64)
    for frame, values, mask in zip(frames, scores, masks):
        weight = np.where(mask, values.astype(np.float64) ** 4.0, 0.0)
        numerator += frame.astype(np.float64) * weight[..., None]
        denominator += weight
    fallback = _eligible_mean_f64(frames, masks)
    output = np.divide(numerator, denominator[..., None], out=fallback, where=denominator[..., None] != 0)
    return FusionResult(quantize_u16(output), select(scores, masks), masks)


def depth_map_replay(frames: np.ndarray, coverage: np.ndarray | None = None, score_radius: int = 4,
                     smooth_radius: int = 8, eligibility_reach: int | None = None,
                     score: Score | None = None, adjust: ScoreAdjust | None = None) -> FusionResult:
    """Replay Depth Map smoothing and exact winner-copy composition."""
    details = np.stack([detail(frame) for frame in frames])
    raw = np.stack([score_plane(value, score_radius) if score is None else score(frame, value, score_radius)
                    for frame, value in zip(frames, details)])
    reach = 2 * score_radius + smooth_radius if eligibility_reach is None else eligibility_reach
    masks = np.ones(raw.shape, dtype=bool) if coverage is None else np.stack([eligible(mask, reach) for mask in coverage])
    if adjust is not None:
        raw = adjust(raw, masks)
    scores = np.stack([score_plane(value[..., None], smooth_radius) for value in raw])
    winners = select(scores, masks)
    return FusionResult(gather(frames, winners), winners, masks)
