# SPDX-License-Identifier: GPL-3.0-or-later
"""Validity-aware frame-index fields for stack experiments."""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from . import fusion


KERNEL = np.asarray((0.085, 0.25, 0.33, 0.25, 0.085), dtype=np.float32)
FILL_SWEEPS = 3
FILL_COARSE_VISITS = 3
SMOOTH_STAGES = 3


@dataclass(frozen=True)
class FillResult:
    """Filled field and locations resolved from trusted support."""

    field: np.ndarray
    resolved: np.ndarray


@dataclass(frozen=True)
class FieldInputs:
    """Current score intermediates and fixed-eligibility depth-map controls."""

    raw_scores: np.ndarray
    smoothed_scores: np.ndarray
    eligibility: np.ndarray
    raw_winners: np.ndarray
    current_winners: np.ndarray


@dataclass(frozen=True)
class FieldResult:
    """One field construction rung with its composite and diagnostics."""

    composite: np.ndarray
    field: np.ndarray
    trusted: np.ndarray
    filled: np.ndarray
    unresolved: np.ndarray
    adjacent_fallback: np.ndarray
    no_source: np.ndarray


def _conv5(values: np.ndarray) -> np.ndarray:
    padded = np.pad(values.astype(np.float32, copy=False), ((0, 0), (2, 2)), mode="edge")
    horizontal = sum(np.float32(weight) * padded[:, offset:offset + values.shape[1]]
                     for offset, weight in enumerate(KERNEL))
    padded = np.pad(horizontal, ((2, 2), (0, 0)), mode="edge")
    return np.asarray(sum(np.float32(weight) * padded[offset:offset + values.shape[0]]
                          for offset, weight in enumerate(KERNEL)), dtype=np.float32)


def _reduce(values: np.ndarray, support: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    height, width = values.shape
    pad_y, pad_x = height % 2, width % 2
    padded_values = np.pad(values, ((0, pad_y), (0, pad_x)), mode="edge")
    padded_support = np.pad(support, ((0, pad_y), (0, pad_x)), mode="edge")
    samples = padded_values.reshape((height + pad_y) // 2, 2, (width + pad_x) // 2, 2).transpose(0, 2, 1, 3)
    masks = padded_support.reshape((height + pad_y) // 2, 2, (width + pad_x) // 2, 2).transpose(0, 2, 1, 3)
    count = masks.sum(axis=(2, 3))
    total = np.where(masks, samples, 0.0).sum(axis=(2, 3), dtype=np.float32)
    return np.divide(total, count, out=np.zeros_like(total), where=count != 0).astype(np.float32), count != 0


def _expand(values: np.ndarray, height: int, width: int) -> np.ndarray:
    x = np.arange(width) // 2
    y = np.arange(height) // 2
    return values[np.minimum(y, values.shape[0] - 1)[:, None], np.minimum(x, values.shape[1] - 1)[None, :]].astype(np.float32)


def _smooth_reduce(values: np.ndarray, support: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    weights = _conv5(support.astype(np.float32))[::2, ::2]
    total = _conv5(np.where(support, values, 0.0))[::2, ::2]
    return np.divide(total, weights, out=np.zeros_like(total), where=weights > 0.0), weights > 0.0


def _smooth_expand(values: np.ndarray, support: np.ndarray, height: int, width: int) -> tuple[np.ndarray, np.ndarray]:
    upsampled = np.zeros((height, width), dtype=np.float32)
    upsampled_support = np.zeros((height, width), dtype=bool)
    upsampled[::2, ::2] = values
    upsampled_support[::2, ::2] = support
    weights = _conv5(upsampled_support.astype(np.float32))
    total = _conv5(upsampled)
    return np.divide(total, weights, out=np.zeros_like(total), where=weights > 0.0), weights > 0.0


def _sweep(values: np.ndarray, support: np.ndarray, pinned: np.ndarray, pins: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    weights = _conv5(support.astype(np.float32))
    total = _conv5(np.where(support, values, 0.0))
    next_support = support | (weights > 0.0)
    next_values = np.divide(total, weights, out=values.astype(np.float32, copy=True), where=weights > 0.0)
    next_values[pinned] = pins[pinned]
    return next_values.astype(np.float32), next_support


def fill_indices(labels: np.ndarray, trusted: np.ndarray, max_depth: int = 0) -> FillResult:
    """Diffuse trusted indices through a finite multiscale field while preserving pins."""
    if labels.ndim != 2 or trusted.shape != labels.shape:
        raise ValueError("labels and trusted must be matching two-dimensional arrays")
    if max_depth < 0:
        raise ValueError("max_depth must be nonnegative")
    pins = labels.astype(np.float32, copy=True)
    pinned = trusted.astype(bool, copy=True)

    def relax(values: np.ndarray, support: np.ndarray, pin_values: np.ndarray, pin_support: np.ndarray,
              depth: int) -> tuple[np.ndarray, np.ndarray]:
        if depth == 0:
            for _sweep_index in range(FILL_SWEEPS):
                values, support = _sweep(values, support, pin_support, pin_values)
            return values, support
        for _visit in range(FILL_COARSE_VISITS):
            coarse_values, coarse_support = _reduce(values, support)
            coarse_pins, coarse_pinned = _reduce(pin_values, pin_support)
            coarse_values, coarse_support = relax(coarse_values, coarse_support, coarse_pins, coarse_pinned, depth - 1)
            values = _expand(coarse_values, values.shape[0], values.shape[1])
            support = _expand(coarse_support.astype(np.float32), values.shape[0], values.shape[1]) > 0.0
            for _sweep_index in range(FILL_SWEEPS):
                values, support = _sweep(values, support, pin_support, pin_values)
        return values, support

    values = pins.copy()
    support = pinned.copy()
    values, support = relax(values, support, pins, pinned, max_depth)
    values[trusted] = labels[trusted]
    return FillResult(values, support & ~trusted)


def full_fill_depth(height: int, width: int) -> int:
    """Return the reduction count needed to reach one clamped coarse cell."""
    if height < 1 or width < 1:
        raise ValueError("field dimensions must be positive")
    depth = 0
    while height > 1 or width > 1:
        height, width = (height + 1) // 2, (width + 1) // 2
        depth += 1
    return depth


def smooth_indices(field: np.ndarray, support: np.ndarray | None = None, stages: int = SMOOTH_STAGES) -> np.ndarray:
    """Apply the registered reduce/expand smoothing stages to a finite index field."""
    if stages < 0:
        raise ValueError("stages must be nonnegative")
    levels = [field.astype(np.float32, copy=False)]
    supports = [np.isfinite(field) if support is None else support.astype(bool, copy=False)]
    for _stage in range(stages):
        reduced, reduced_support = _smooth_reduce(levels[-1], supports[-1])
        levels.append(reduced)
        supports.append(reduced_support)
    output = levels[-1]
    output_support = supports[-1]
    for original, original_support in zip(reversed(levels[:-1]), reversed(supports[:-1])):
        output, output_support = _smooth_expand(output, output_support, original.shape[0], original.shape[1])
        output_support &= original_support
    return np.where(output_support, output, 0.0).astype(np.float32)


def field_inputs(frames: np.ndarray, coverage: np.ndarray | None = None) -> FieldInputs:
    """Build current raw and smoothed score controls with fixed eligibility16."""
    details = np.stack([fusion.detail(frame) for frame in frames])
    raw = np.stack([fusion.score_plane(value, 4) for value in details])
    smoothed = np.stack([fusion.score_plane(value[..., None], 8) for value in raw])
    eligibility = np.ones(raw.shape, dtype=bool) if coverage is None else np.stack(
        [fusion.eligible(mask, 16) for mask in coverage]
    )
    return FieldInputs(raw, smoothed, eligibility, fusion.select(raw, eligibility), fusion.select(smoothed, eligibility))


def trusted_indices(inputs: FieldInputs, threshold: float) -> tuple[np.ndarray, np.ndarray]:
    """Return raw-score winners trusted by an absolute eligible-frame threshold."""
    available = inputs.eligibility.any(axis=0)
    maximum = np.max(np.where(inputs.eligibility, inputs.raw_scores, -np.inf), axis=0)
    trusted = available & (maximum >= np.float32(threshold))
    return inputs.raw_winners.astype(np.float32), trusted


def adjacent_synthesis(frames: np.ndarray, field: np.ndarray, eligibility: np.ndarray,
                      c0: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Blend only adjacent eligible frames and retain C0 where neither is usable."""
    count, height, width, _channels = frames.shape
    output = np.zeros_like(frames[0], dtype=np.float32)
    fallback = np.zeros((height, width), dtype=bool)
    no_source = ~eligibility.any(axis=0)
    clipped = np.clip(field, 0.0, float(count - 1))
    lower = np.floor(clipped).astype(np.intp)
    upper = lower + 1
    yy, xx = np.indices((height, width))
    lower_live = eligibility[lower, yy, xx]
    upper_live = (upper < count) & eligibility[np.minimum(upper, count - 1), yy, xx]
    lower_weight = np.maximum(0.0, 1.0 - np.abs(clipped - lower)).astype(np.float32)
    upper_weight = np.maximum(0.0, 1.0 - np.abs(clipped - upper)).astype(np.float32)
    lower_weight = np.where(lower_live & ~upper_live, 1.0, lower_weight * lower_live)
    upper_weight = np.where(upper_live & ~lower_live, 1.0, upper_weight * upper_live)
    total = lower_weight + upper_weight
    live = total > 0.0
    if np.any(live):
        first = frames[lower, yy, xx].astype(np.float32)
        second = frames[np.minimum(upper, count - 1), yy, xx].astype(np.float32)
        output[live] = ((first[live] * lower_weight[live, None] + second[live] * upper_weight[live, None]) /
                        total[live, None])
    fallback = ~live & ~no_source
    output[fallback] = c0[fallback].astype(np.float32)
    return fusion.quantize_u16(output), fallback, no_source


def rounded_synthesis(frames: np.ndarray, field: np.ndarray, eligibility: np.ndarray,
                      c0: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Copy rounded eligible labels and retain C0 for invalid rounded labels."""
    count, height, width, _channels = frames.shape
    winners = np.rint(np.clip(field, 0.0, count - 1)).astype(np.intp)
    yy, xx = np.indices((height, width))
    no_source = ~eligibility.any(axis=0)
    chosen = eligibility[winners, yy, xx]
    fallback = ~chosen & ~no_source
    output = np.zeros_like(frames[0])
    output[chosen] = frames[winners[chosen], yy[chosen], xx[chosen]]
    output[fallback] = c0[fallback]
    return output, fallback, no_source


def build_field(frames: np.ndarray, coverage: np.ndarray | None, threshold: float, rung: str,
                fill_depth: int = 0) -> FieldResult:
    """Build one C0, C0r, A1–A4 or N1 index-field rung."""
    inputs = field_inputs(frames, coverage)
    c0 = fusion.gather(frames, inputs.current_winners)
    no_source = inputs.current_winners == fusion.NO_SOURCE
    if rung == "C0":
        empty = np.zeros_like(no_source)
        return FieldResult(c0, inputs.current_winners.astype(np.float32), empty, empty, empty, empty, no_source)
    if rung == "C0r":
        composite = fusion.gather(frames, inputs.raw_winners)
        empty = np.zeros_like(no_source)
        return FieldResult(composite, inputs.raw_winners.astype(np.float32), empty, empty, empty, empty, no_source)
    if rung == "N1":
        total = np.zeros_like(inputs.smoothed_scores[0], dtype=np.float32)
        count = np.zeros_like(inputs.smoothed_scores[0], dtype=np.uint16)
        for scores, eligible in zip(inputs.smoothed_scores, inputs.eligibility):
            total = np.float32(total + np.where(eligible, scores, 0.0))
            count += eligible
        mean = np.divide(total, count, out=np.zeros_like(total), where=count != 0)
        below = np.max(np.where(inputs.eligibility, inputs.smoothed_scores, -np.inf), axis=0) < mean * np.float32(1.5)
        winners = inputs.current_winners.copy()
        winners[below & inputs.eligibility[-1]] = frames.shape[0] - 1
        empty = np.zeros_like(no_source)
        return FieldResult(fusion.gather(frames, winners), winners.astype(np.float32), empty, empty, empty, empty, no_source)
    raw_field, trusted = trusted_indices(inputs, threshold)
    if rung == "A4":
        c0_field = np.where(no_source, 0.0, inputs.current_winners).astype(np.float32)
        smooth = np.clip(smooth_indices(c0_field, ~no_source), 0.0, frames.shape[0] - 1)
        smooth[no_source] = 0.0
        composite, fallback, _ = adjacent_synthesis(frames, smooth, inputs.eligibility, c0)
        smooth[no_source] = np.float32(fusion.NO_SOURCE)
        empty = np.zeros_like(no_source)
        return FieldResult(composite, smooth, trusted, empty, empty, fallback, no_source)
    filled = fill_indices(raw_field, trusted, max_depth=fill_depth)
    field = filled.field
    unresolved = ~trusted & ~filled.resolved & ~no_source
    field[unresolved] = inputs.current_winners[unresolved]
    if rung in {"A2", "A3"}:
        field = smooth_indices(field, ~no_source)
    field = np.clip(field, 0.0, frames.shape[0] - 1)
    field[unresolved] = inputs.current_winners[unresolved]
    if rung == "A1" or rung == "A2":
        composite, fallback, _ = rounded_synthesis(frames, field, inputs.eligibility, c0)
        field[no_source] = np.float32(fusion.NO_SOURCE)
        return FieldResult(composite, field, trusted, filled.resolved & ~no_source, unresolved, fallback, no_source)
    if rung == "A3":
        composite, fallback, _ = adjacent_synthesis(frames, field, inputs.eligibility, c0)
        field[no_source] = np.float32(fusion.NO_SOURCE)
        return FieldResult(composite, field, trusted, filled.resolved & ~no_source, unresolved, fallback, no_source)
    raise ValueError(f"unknown index-field rung: {rung}")


def self_checks() -> list[str]:
    """Exercise pinning, finite fill, endpoint synthesis, and rung separation."""
    labels = np.tile(np.arange(9, dtype=np.float32), (9, 1))
    trusted = np.ones((9, 9), dtype=bool)
    trusted[3:6, 3:6] = False
    filled = fill_indices(labels, trusted)
    if not np.array_equal(filled.field[trusted], labels[trusted]) or not np.isfinite(filled.field).all():
        raise AssertionError("index-field pin or finite-fill invariant failed")
    hole = ~trusted
    if np.abs(filled.field[hole] - labels[hole]).max() > 0.025:
        raise AssertionError("index-field finite ramp-hole bound exceeded")
    single_trusted = np.ones((9, 9), dtype=bool)
    single_trusted[4, 4] = False
    single = fill_indices(labels, single_trusted)
    if not np.allclose(single.field[~single_trusted], labels[~single_trusted], atol=1e-5):
        raise AssertionError("index-field single-pixel ramp oracle failed")
    broken_pin = filled.field.copy()
    broken_pin[trusted] += 1.0
    if np.array_equal(broken_pin[trusted], labels[trusted]):
        raise AssertionError("index-field moved-pin detector did not fire")
    one_pin = np.zeros((5, 5), dtype=bool)
    one_pin[2, 2] = True
    if not fill_indices(np.full((5, 5), 3.0, dtype=np.float32), one_pin).resolved.any():
        raise AssertionError("index-field isolated pin did not propagate")
    line = np.zeros((5, 7), dtype=bool)
    line[2] = True
    line_values = np.broadcast_to(np.arange(7, dtype=np.float32), line.shape)
    line_fill = fill_indices(line_values, line)
    if not np.array_equal(line_fill.field[line], line_values[line]):
        raise AssertionError("index-field line pins moved")
    recursive = fill_indices(line_values, line, max_depth=2)
    if not np.array_equal(recursive.field[line], line_values[line]) or not np.isfinite(recursive.field).all():
        raise AssertionError("index-field recursive pins moved")
    frames = np.array([[[[0, 0, 0], [100, 100, 100]]], [[[10, 10, 10], [200, 200, 200]]]], dtype=np.uint16)
    eligible = np.ones((2, 1, 2), dtype=bool)
    output, fallback, no_source = adjacent_synthesis(frames, np.array([[0.0, 0.5]], dtype=np.float32), eligible, frames[0])
    if output[0, 0, 0] != 0 or output[0, 1, 0] != 150 or fallback.any() or no_source.any():
        raise AssertionError("index-field adjacent weights are incorrect")
    single, _fallback, _none = adjacent_synthesis(frames[:1], np.array([[0.0, 0.0]], dtype=np.float32), eligible[:1], frames[0])
    if not np.array_equal(single, frames[0]):
        raise AssertionError("index-field single-frame endpoint changed samples")
    only_upper = eligible.copy()
    only_upper[0, 0, 0] = False
    upper_output, upper_fallback, _ = adjacent_synthesis(frames, np.array([[0.0, 0.5]], dtype=np.float32), only_upper, frames[0])
    if upper_output[0, 0, 0] != 10 or upper_fallback[0, 0]:
        raise AssertionError("index-field sole adjacent upper source fell back")
    all_unknown = build_field(frames, np.zeros(frames.shape[:3], dtype=bool), 1.0, "A3")
    if not all_unknown.no_source.all() or all_unknown.composite.any() or not np.all(all_unknown.field == fusion.NO_SOURCE):
        raise AssertionError("index-field all-unknown no-source invariant failed")
    third = np.concatenate((frames, np.full_like(frames[:1], 250)))
    third_eligible = np.ones(third.shape[:3], dtype=bool)
    expected, _fallback, _none = adjacent_synthesis(third, np.full((1, 2), 0.5, dtype=np.float32), third_eligible, third[0])
    broken_third = expected.copy()
    broken_third[0, 0] = third[2, 0, 0]
    if np.array_equal(expected, broken_third):
        raise AssertionError("index-field third-frame blend detector did not fire")
    fallback = np.full((1, 2), 2, dtype=np.uint16)
    if np.array_equal(fallback, np.rint(np.array([[0.0, 0.5]], dtype=np.float32)).astype(np.uint16)):
        raise AssertionError("index-field last-frame fallback detector did not fire")
    stack = np.arange(2 * 41 * 43 * 3, dtype=np.uint16).reshape(2, 41, 43, 3)
    a4_low = build_field(stack, np.ones(stack.shape[:3], dtype=bool), 1.0, "A4")
    a4_high = build_field(stack, np.ones(stack.shape[:3], dtype=bool), 9.0, "A4")
    if not np.array_equal(a4_low.composite, a4_high.composite) or not np.array_equal(a4_low.field, a4_high.field):
        raise AssertionError("index-field A4 changed with validity threshold")
    constant = smooth_indices(np.full((9, 11), 3.25, dtype=np.float32), np.ones((9, 11), dtype=bool))
    if not np.allclose(constant, 3.25, atol=1e-6) or smooth_indices(np.array([[7.0]], dtype=np.float32))[0, 0] != 7.0:
        raise AssertionError("index-field smoothing failed its constant oracle")
    odd_ramp = smooth_indices(np.add.outer(np.arange(9, dtype=np.float32), np.arange(11, dtype=np.float32)))
    if odd_ramp.shape != (9, 11) or not np.isfinite(odd_ramp).all():
        raise AssertionError("index-field smoothing failed on an odd ramp")
    if full_fill_depth(512, 512) != 9 or full_fill_depth(9, 11) != 4:
        raise AssertionError("index-field full-depth reduction count is incorrect")
    return ["index-field-pins", "index-field-ramp", "index-field-adjacent", "index-field-endpoints", "index-field-no-source", "index-field-mutants", "index-field-a4-threshold", "index-field-smoothing", "index-field-full-depth"]
