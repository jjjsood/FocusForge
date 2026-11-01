# SPDX-License-Identifier: GPL-3.0-or-later
"""Lina--Mayrand complex-wavelet fusion controls."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import numpy as np

from .fusion import NO_SOURCE
from . import metrics


LEVELS = 5
LOWPASS = (np.sqrt(2.0) * np.array([-3, 5, 30, 30, 5, -3], dtype=np.float64)
           + 1j * np.sqrt(30.0) * np.array([-1, -1, 2, 2, -1, -1], dtype=np.float64)) / 64.0
HIGHPASS = ((-1.0) ** np.arange(6)) * np.conjugate(LOWPASS)
CONFIGURATIONS = (
    {"id": "shared-raw", "consistency": False, "colour": "shared"},
    {"id": "shared-voted", "consistency": True, "colour": "shared"},
    {"id": "grey-reassign-raw", "consistency": False, "colour": "reassign"},
    {"id": "grey-reassign-voted", "consistency": True, "colour": "reassign"},
)


@dataclass(frozen=True)
class Coefficients:
    """One critically sampled complex decomposition."""

    lowpass: np.ndarray
    details: tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...]
    shape: tuple[int, int]


@dataclass(frozen=True)
class FusionResult:
    """A fused image and separate coefficient and pixel source maps."""

    composite: np.ndarray
    orientation_winners: tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...]
    raw_winners: tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...]
    support: tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...]
    pixel_sources: np.ndarray | None
    counts: dict[str, int]
    # Kept out of exported count JSON: tiled accounting needs the per-coefficient
    # rejected-vote ownership plane before it reduces counters.
    rejected_votes: tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...] | None = None


def _axis_analysis(values: np.ndarray, axis: int) -> tuple[np.ndarray, np.ndarray]:
    length = values.shape[axis]
    if length < 2 or length % 2:
        raise ValueError("complex-wavelet dimensions must be positive and even after padding")
    positions = (2 * np.arange(length // 2)[:, None] + np.arange(6) - 3) % length
    moved = np.moveaxis(values, axis, -1)
    samples = moved[..., positions]
    low = sum(samples[..., tap] * LOWPASS[tap] for tap in range(6))
    high = sum(samples[..., tap] * HIGHPASS[tap] for tap in range(6))
    return np.moveaxis(low, -1, axis), np.moveaxis(high, -1, axis)


def _axis_synthesis(low: np.ndarray, high: np.ndarray, axis: int, broken: str | None = None) -> np.ndarray:
    half = low.shape[axis]
    length = half * 2
    output = np.zeros((*low.shape[:axis], length, *low.shape[axis + 1:]), dtype=np.complex128)
    low_moved, high_moved, output_moved = (np.moveaxis(value, axis, -1) for value in (low, high, output))
    taps = LOWPASS if broken == "missing-conjugate" else np.conjugate(LOWPASS)
    high_taps = HIGHPASS if broken == "missing-conjugate" else np.conjugate(HIGHPASS)
    for tap in range(6):
        output_moved[..., (2 * np.arange(half) + tap - 3) % length] += low_moved * taps[tap] + high_moved * high_taps[tap]
    return np.moveaxis(output_moved, -1, axis)


def _axis_support(mask: np.ndarray, axis: int) -> tuple[np.ndarray, np.ndarray]:
    length = mask.shape[axis]
    positions = (2 * np.arange(length // 2)[:, None] + np.arange(6) - 3) % length
    moved = np.moveaxis(mask, axis, -1)
    support = np.all(moved[..., positions], axis=-1)
    return np.moveaxis(support, -1, axis), np.moveaxis(support, -1, axis)


def _one_level(values: np.ndarray) -> tuple[np.ndarray, tuple[np.ndarray, np.ndarray, np.ndarray]]:
    low_x, high_x = _axis_analysis(values, 1)
    low_low, low_high = _axis_analysis(low_x, 0)
    high_low, high_high = _axis_analysis(high_x, 0)
    return low_low, (low_high, high_low, high_high)


def _one_support(mask: np.ndarray) -> tuple[np.ndarray, tuple[np.ndarray, np.ndarray, np.ndarray]]:
    low_x, high_x = _axis_support(mask, 1)
    low_low, low_high = _axis_support(low_x, 0)
    high_low, high_high = _axis_support(high_x, 0)
    return low_low, (low_high, high_low, high_high)


def pad_input(values: np.ndarray, levels: int = LEVELS) -> tuple[np.ndarray, tuple[int, int]]:
    """Reflect-pad bottom and right to the transform period."""
    if values.ndim not in {2, 3} or not values.shape[0] or not values.shape[1]:
        raise ValueError("complex-wavelet input must have nonempty HxW samples")
    if not np.isfinite(values).all():
        raise ValueError("complex-wavelet input contains nonfinite samples")
    period = 1 << levels
    height, width = values.shape[:2]
    target_h = ((height + period - 1) // period) * period
    target_w = ((width + period - 1) // period) * period
    pads = ((0, target_h - height), (0, target_w - width)) + ((0, 0),) * (values.ndim - 2)
    return np.pad(values, pads, mode="reflect"), (height, width)


def pad_mask(mask: np.ndarray, shape: tuple[int, int], levels: int = LEVELS) -> np.ndarray:
    """Pad eligibility with false samples, never reflected validity."""
    if mask.shape != shape or mask.dtype != bool:
        raise ValueError("complex-wavelet mask shape must match image HxW")
    period = 1 << levels
    target_h = ((shape[0] + period - 1) // period) * period
    target_w = ((shape[1] + period - 1) // period) * period
    output = np.zeros((target_h, target_w), dtype=bool)
    output[:shape[0], :shape[1]] = mask
    return output


def analyse(values: np.ndarray, levels: int = LEVELS) -> Coefficients:
    """Analyse a padded scalar or channel image using periodic six-tap filters."""
    padded, shape = pad_input(np.asarray(values, dtype=np.complex128), levels)
    current = padded
    details = []
    for _ in range(levels):
        current, bands = _one_level(current)
        details.append(bands)
    return Coefficients(current, tuple(details), shape)


def analyse_mask(mask: np.ndarray, levels: int = LEVELS) -> tuple[np.ndarray, tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...]]:
    """Propagate exact periodic analysis footprints through one validity mask."""
    current = pad_mask(mask, mask.shape, levels)
    details = []
    for _ in range(levels):
        current, bands = _one_support(current)
        details.append(bands)
    return current, tuple(details)


def synthesise(coefficients: Coefficients, broken: str | None = None) -> np.ndarray:
    """Apply the conjugate-adjoint reverse transform and crop reflected padding."""
    current = coefficients.lowpass
    for bands in reversed(coefficients.details):
        low_high, high_low, high_high = bands
        low_x = _axis_synthesis(current, low_high, 0, broken)
        high_x = _axis_synthesis(high_low, high_high, 0, broken)
        current = _axis_synthesis(low_x, high_x, 1, broken)
    height, width = coefficients.shape
    return current[:height, :width]


def _reflect_indices(indices: np.ndarray, length: int) -> np.ndarray:
    """Map arbitrary positions through NumPy's even reflection, including one sample."""
    if length == 1:
        return np.zeros_like(indices)
    period = 2 * length - 2
    phase = np.mod(indices, period)
    return np.where(phase < length, phase, period - phase)


def _source_read(source: object, frame: int) -> tuple[np.ndarray, np.ndarray]:
    label = str(getattr(source, "paths", [f"frame {frame}"])[frame] if getattr(source, "paths", None) else f"frame {frame}")
    values, mask = source.read(frame)
    raw_values = np.asarray(values)
    if not np.issubdtype(raw_values.dtype, np.floating):
        raise ValueError(f"complex-wavelet source {label} must be normalized floating samples")
    values = np.asarray(raw_values, dtype=np.float64)
    mask = np.asarray(mask, dtype=bool)
    if values.ndim != 3 or values.shape[:2] != (source.h, source.w) or values.shape[2] not in {1, 3} or values.shape[2] != source.c:
        raise ValueError(f"complex-wavelet source {label} must be same-sized fixed-channel grey or RGB images")
    if mask.shape != values.shape[:2]:
        raise ValueError(f"complex-wavelet source {label} mask shape does not match frame")
    if not np.isfinite(values).all() or np.any(values < 0.0) or np.any(values > 1.0):
        raise ValueError(f"complex-wavelet source {label} contains nonfinite or out-of-domain samples")
    return values, mask


def _gather(current: Coefficients, winners: tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...], frame: int) -> Coefficients:
    details = []
    for bands, picked in zip(current.details, winners):
        details.append(tuple(np.where((chosen == frame)[..., None] if band.ndim == 3 else chosen == frame, band, 0.0)
                             for band, chosen in zip(bands, picked)))
    return Coefficients(np.zeros_like(current.lowpass), tuple(details), current.shape)


def _per_channel_mutant_coefficients(source: object) -> tuple[Coefficients, tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...], tuple[tuple[np.ndarray, np.ndarray, np.ndarray], ...]]:
    """Deliberately wrong control: select each RGB coefficient channel independently."""
    coefficients, masks = [], []
    for frame in range(int(source.n)):
        values, mask = _source_read(source, frame)
        if values.shape[2] != 3:
            raise ValueError("per-channel mutant needs RGB input")
        coefficients.append(analyse(values))
        masks.append(analyse_mask(mask))
    low_sum = np.zeros_like(coefficients[0].lowpass)
    low_count = np.zeros(coefficients[0].lowpass.shape[:2], dtype=np.uint16)
    for item, (low_live, _details) in zip(coefficients, masks):
        low_sum += np.where(low_live[..., None], item.lowpass, 0.0)
        low_count += low_live
    selected, winners, support = [], [], []
    for level in range(LEVELS):
        selected_bands, winner_bands, support_bands = [], [], []
        for band in range(3):
            shape = coefficients[0].details[level][band].shape
            winner = np.full(shape, NO_SOURCE, dtype=np.uint16)
            best = np.zeros(shape, dtype=np.float64)
            live_union = np.zeros(shape[:2], dtype=bool)
            for frame, (item, (_low_live, detail_live)) in enumerate(zip(coefficients, masks)):
                live = detail_live[level][band]
                score = np.abs(item.details[level][band]) ** 2
                better = live[..., None] & ((winner == NO_SOURCE) | (score > best))
                winner[better] = frame
                best[better] = score[better]
                live_union |= live
            gathered = np.zeros(shape, dtype=np.complex128)
            for frame, item in enumerate(coefficients):
                picked = winner == frame
                gathered[picked] = item.details[level][band][picked]
            selected_bands.append(gathered)
            winner_bands.append(winner)
            support_bands.append(live_union)
        selected.append(tuple(selected_bands))
        winners.append(tuple(winner_bands))
        support.append(tuple(support_bands))
    low = np.divide(low_sum, low_count[..., None], out=np.zeros_like(low_sum), where=low_count[..., None] != 0)
    return Coefficients(low, tuple(selected), coefficients[0].shape), tuple(winners), tuple(support)


def _per_channel_mutant_fuse(source: object) -> FusionResult:
    selected, winners, support = _per_channel_mutant_coefficients(source)
    image = np.clip(np.rint(synthesise(selected).real * 65535.0), 0, 65535).astype(np.uint16)
    return FusionResult(image, winners, winners, support, None, {"untouched": 0, "voted": 0, "rejected": 0})


def fuse(source: object, configuration: dict[str, object]) -> FusionResult:
    """Fuse one source with bounded streaming coefficient state."""
    if not 1 <= int(source.n) < int(NO_SOURCE):
        raise ValueError("complex-wavelet frame count exceeds u16 source-index capacity")
    configuration = dict(configuration)
    broken = configuration.pop("broken", None)
    if broken not in {None, "ignore-mask", "real-only-magnitude", "per-channel-selection", "inconsistent-vote-gather"}:
        raise ValueError("unknown complex-wavelet controlled mutation")
    if any(configuration == known for known in CONFIGURATIONS) is False:
        raise ValueError("unknown complex-wavelet configuration")
    if broken == "per-channel-selection":
        return _per_channel_mutant_fuse(source)
    if broken is not None:
        configuration["broken"] = broken
    best: list[list[np.ndarray]] | None = None
    raw: list[list[np.ndarray]] | None = None
    seen: list[list[np.ndarray]] | None = None
    low_sum = low_count = None
    channels = None
    for frame in range(int(source.n)):
        values, mask = _source_read(source, frame)
        if configuration.get("broken") == "ignore-mask":
            mask = np.ones_like(mask)
        channels = values.shape[2]
        working = values.mean(axis=2) if configuration["colour"] == "reassign" else values
        coefficients = analyse(working)
        support_low, support_details = analyse_mask(mask)
        if low_sum is None:
            low_sum = np.zeros_like(coefficients.lowpass)
            low_count = np.zeros(coefficients.lowpass.shape[:2], dtype=np.uint16)
        low_sum += np.where(support_low[..., None] if low_sum.ndim == 3 else support_low, coefficients.lowpass, 0.0)
        low_count += support_low
        if best is None:
            best = [[np.zeros(band.shape[:2], dtype=np.float64) for band in bands] for bands in coefficients.details]
            raw = [[np.full(band.shape[:2], NO_SOURCE, dtype=np.uint16) for band in bands] for bands in coefficients.details]
            seen = [[np.zeros(band.shape[:2], dtype=bool) for band in bands] for bands in coefficients.details]
        for level, (bands, live_bands) in enumerate(zip(coefficients.details, support_details)):
            for orientation, (band, live) in enumerate(zip(bands, live_bands)):
                score = band.real ** 2 if configuration.get("broken") == "real-only-magnitude" else np.abs(band) ** 2
                if score.ndim == 3:
                    score = score.sum(axis=2)
                assert best is not None and raw is not None and seen is not None
                better = live & (~seen[level][orientation] | (score > best[level][orientation]))
                raw[level][orientation][better] = frame
                best[level][orientation][better] = score[better]
                seen[level][orientation] |= live
    if low_sum is None or low_count is None or channels is None or raw is None:
        raise ValueError("complex-wavelet source has no frames")
    final = [[band.copy() for band in bands] for bands in raw]
    support = [[np.zeros(band.shape, dtype=bool) for band in bands] for bands in raw]
    rejected_maps = [[np.zeros(band.shape, dtype=bool) for band in bands] for bands in raw]
    for frame in range(int(source.n)):
        values, mask = _source_read(source, frame)
        _low, details = analyse_mask(mask)
        for level, bands in enumerate(details):
            for orientation, live in enumerate(bands):
                support[level][orientation] |= live
                if configuration["consistency"]:
                    other = [raw[level][(orientation + 1) % 3], raw[level][(orientation + 2) % 3]]
                    majority = np.where(other[0] == other[1], other[0], NO_SOURCE)
                    wanted = majority == frame
                    # The deliberate gather mutation leaves the correct voted map
                    # behind, then gathers stale raw coefficients below.
                    final[level][orientation][wanted & live] = frame
                    rejected_maps[level][orientation] |= wanted & ~live
    vote_counts = {"untouched": 0, "voted": 0, "rejected": 0, "per_band": []}
    for level, (raw_bands, final_bands, support_bands) in enumerate(zip(raw, final, support)):
        for orientation, (before, after, owned) in enumerate(zip(raw_bands, final_bands, support_bands)):
            voted = int(np.count_nonzero((before != after) & owned))
            untouched = int(np.count_nonzero((before == after) & owned))
            rejected_here = int(np.count_nonzero(rejected_maps[level][orientation]))
            vote_counts["voted"] += voted
            vote_counts["untouched"] += untouched
            vote_counts["rejected"] += rejected_here
            vote_counts["per_band"].append({"level": level, "orientation": orientation, "owned_support": int(owned.sum()),
                                             "untouched": untouched, "voted": voted, "rejected": rejected_here})
    selected = None
    for frame in range(int(source.n)):
        values, _mask = _source_read(source, frame)
        working = values.mean(axis=2) if configuration["colour"] == "reassign" else values
        gathered = _gather(analyse(working), tuple(tuple(bands) for bands in raw if configuration.get("broken") == "inconsistent-vote-gather")
                           if configuration.get("broken") == "inconsistent-vote-gather" else tuple(tuple(bands) for bands in final), frame)
        if selected is None:
            selected = gathered
        else:
            selected = Coefficients(selected.lowpass, tuple(tuple(a + b for a, b in zip(left, right)) for left, right in zip(selected.details, gathered.details)), selected.shape)
    assert selected is not None
    low = np.divide(low_sum, low_count[..., None] if low_sum.ndim == 3 else low_count, out=np.zeros_like(low_sum), where=(low_count[..., None] if low_sum.ndim == 3 else low_count) != 0)
    image = synthesise(Coefficients(low, selected.details, selected.shape)).real
    pixel_sources = None
    if configuration["colour"] == "reassign":
        pixel_sources = np.full((source.h, source.w), NO_SOURCE, dtype=np.uint16)
        best = np.full((source.h, source.w), np.inf, dtype=np.float64)
        rgb = np.zeros((source.h, source.w, channels), dtype=np.float64)
        target = image
        for frame in range(int(source.n)):
            values, mask = _source_read(source, frame)
            distance = np.abs(values.mean(axis=2) - target)
            choose = mask & (distance < best)
            rgb[choose] = values[choose]
            pixel_sources[choose] = frame
            best[choose] = distance[choose]
        image = rgb
        image[pixel_sources == NO_SOURCE] = 0.0
    image = np.clip(np.rint(image * 65535.0), 0, 65535).astype(np.uint16)
    return FusionResult(image, tuple(tuple(bands) for bands in final), tuple(tuple(bands) for bands in raw),
                        tuple(tuple(bands) for bands in support), pixel_sources, vote_counts,
                        tuple(tuple(bands) for bands in rejected_maps))


def theoretical_reach(levels: int = LEVELS) -> int:
    """Return the filter-footprint bound; impulse controls establish usable margin."""
    return ((6 - 1) * ((1 << levels) - 1))


class _GlobalWindow:
    """A 32-phase-aligned view of the padded periodic outer domain."""

    def __init__(self, source: object, y0: int, x0: int, size: int) -> None:
        self.source, self.n, self.c, self.h, self.w = source, source.n, source.c, size, size
        padded_h = ((source.h + 31) // 32) * 32
        padded_w = ((source.w + 31) // 32) * 32
        row_domain = np.arange(y0, y0 + size) % padded_h
        column_domain = np.arange(x0, x0 + size) % padded_w
        self.row_live, self.column_live = row_domain < source.h, column_domain < source.w
        self.rows = _reflect_indices(row_domain, source.h)
        self.columns = _reflect_indices(column_domain, source.w)
        self.read_windows: list[dict[str, int]] = []

    def read(self, frame: int) -> tuple[np.ndarray, np.ndarray]:
        y0, y1 = int(self.rows.min()), int(self.rows.max()) + 1
        x0, x1 = int(self.columns.min()), int(self.columns.max()) + 1
        values, mask = self.source.read(frame, (y0, y1, x0, x1))
        self.read_windows.append({"frame": frame, "x": x0, "y": y0, "width": x1 - x0, "height": y1 - y0,
                                  "bytes": int((x1 - x0) * (y1 - y0) * self.c * 2)})
        data = values[np.ix_(self.rows - y0, self.columns - x0)]
        live = np.asarray(mask)[np.ix_(self.rows - y0, self.columns - x0)]
        return data, live & self.row_live[:, None] & self.column_live[None, :]


def fuse_tiled(source: object, configuration: dict[str, object], tile: int = 512, margin: int | None = None) -> tuple[FusionResult, dict[str, object]]:
    """Fuse actual phase-aligned windows while retaining the global padded boundary."""
    reach = theoretical_reach()
    margin = ((reach + 31) // 32) * 32 if margin is None else margin
    if margin < reach or margin % 32:
        raise ValueError("complex-wavelet tile margin is below required theoretical reach or breaks 32-pixel phase")
    interior = ((tile - 2 * margin) // 32) * 32
    if interior <= 0:
        raise ValueError("complex-wavelet tile margin leaves no interior")
    image = np.zeros((source.h, source.w, source.c), dtype=np.uint16)
    padded_h = ((source.h + 31) // 32) * 32
    padded_w = ((source.w + 31) // 32) * 32
    shapes = [(padded_h // (1 << (level + 1)), padded_w // (1 << (level + 1))) for level in range(LEVELS)]
    raw = [[np.full(shape, NO_SOURCE, dtype=np.uint16) for _ in range(3)] for shape in shapes]
    final = [[np.full(shape, NO_SOURCE, dtype=np.uint16) for _ in range(3)] for shape in shapes]
    support = [[np.zeros(shape, dtype=bool) for _ in range(3)] for shape in shapes]
    rejected_maps = [[np.zeros(shape, dtype=bool) for _ in range(3)] for shape in shapes]
    pixel = np.full((source.h, source.w), NO_SOURCE, dtype=np.uint16) if configuration["colour"] == "reassign" else None
    windows, counts = [], {"untouched": 0, "voted": 0, "rejected": 0, "per_band": []}
    for y0 in range(0, source.h, interior):
        for x0 in range(0, source.w, interior):
            y1, x1 = min(y0 + interior, source.h), min(x0 + interior, source.w)
            view = _GlobalWindow(source, y0 - margin, x0 - margin, tile)
            local = fuse(view, configuration)
            ly0, lx0 = margin, margin
            image[y0:y1, x0:x1] = local.composite[ly0:ly0 + y1 - y0, lx0:lx0 + x1 - x0]
            if pixel is not None and local.pixel_sources is not None:
                pixel[y0:y1, x0:x1] = local.pixel_sources[ly0:ly0 + y1 - y0, lx0:lx0 + x1 - x0]
            for level, stride in enumerate((2, 4, 8, 16, 32)):
                gy0, gy1 = y0 // stride, (y1 + stride - 1) // stride
                gx0, gx1 = x0 // stride, (x1 + stride - 1) // stride
                ly, lx = ly0 // stride, lx0 // stride
                for band in range(3):
                    owned = (slice(ly, ly + gy1 - gy0), slice(lx, lx + gx1 - gx0))
                    raw_owned = local.raw_winners[level][band][owned]
                    final_owned = local.orientation_winners[level][band][owned]
                    support_owned = local.support[level][band][owned]
                    rejected_owned = (local.rejected_votes[level][band][owned] if local.rejected_votes is not None
                                      else np.zeros_like(support_owned))
                    raw[level][band][gy0:gy1, gx0:gx1] = raw_owned
                    final[level][band][gy0:gy1, gx0:gx1] = final_owned
                    support[level][band][gy0:gy1, gx0:gx1] = support_owned
                    rejected_maps[level][band][gy0:gy1, gx0:gx1] = rejected_owned
                    voted = int(np.count_nonzero((raw_owned != final_owned) & support_owned))
                    untouched = int(np.count_nonzero((raw_owned == final_owned) & support_owned))
                    rejected = int(np.count_nonzero(rejected_owned))
                    counts["voted"] += voted
                    counts["untouched"] += untouched
                    counts["rejected"] += rejected
                    counts["per_band"].append({"level": level, "orientation": band,
                                                "global_origin_yx": [gy0, gx0],
                                                "owned_shape": [gy1 - gy0, gx1 - gx0],
                                                "owned_support": int(support_owned.sum()),
                                                "untouched": untouched, "voted": voted,
                                                "rejected": rejected,
                                                "uncovered": int(np.count_nonzero(~support_owned))})
            windows.append({"interior_xywh": [x0, y0, x1 - x0, y1 - y0], "global_window_xywh": [x0 - margin, y0 - margin, tile, tile],
                            "physical_subreads": view.read_windows})
    return FusionResult(image, tuple(tuple(item) for item in final), tuple(tuple(item) for item in raw),
                        tuple(tuple(item) for item in support), pixel, counts,
                        tuple(tuple(item) for item in rejected_maps)), {"margin": margin, "theoretical_reach": reach, "interior": interior, "windows": windows}


def _write(path: Path, values: np.ndarray) -> None:
    metrics.gff.write_u16(path, values if values.ndim == 3 else values[..., None])


def _mutant_check(broken: str) -> None:
    rng = np.random.default_rng(91)
    frames = rng.random((3, 64, 64, 3))
    mask = np.ones((3, 64, 64), dtype=bool)
    mask[1, 20:40, 20:40] = False
    class Tiny:
        n, h, w, c = 3, 64, 64, 3
        def __init__(self, samples: np.ndarray, live: np.ndarray) -> None:
            self.samples, self.live = samples, live
        def read(self, frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
            y0, y1, x0, x1 = rect or (0, 64, 0, 64)
            return self.samples[frame, y0:y1, x0:x1], self.live[frame, y0:y1, x0:x1]
    configuration = dict(CONFIGURATIONS[1] if broken == "inconsistent-vote-gather" else CONFIGURATIONS[0])
    honest = fuse(Tiny(frames, mask), configuration)
    if broken == "per-channel-selection":
        mutant = fuse(Tiny(frames, mask), {**configuration, "broken": broken})
        selected, winners, _support = _per_channel_mutant_coefficients(Tiny(frames, mask))
        coefficients = [analyse(frame) for frame in frames]
        observed = False
        for level, bands in enumerate(winners):
            for band, winner in enumerate(bands):
                conflicts = np.argwhere(np.ptp(winner, axis=2) != 0)
                if not len(conflicts):
                    continue
                y, x = (int(value) for value in conflicts[0])
                tuple_winner = tuple(int(value) for value in winner[y, x])
                expected = np.array([coefficients[frame].details[level][band][y, x, channel]
                                     for channel, frame in enumerate(tuple_winner)])
                actual = selected.details[level][band][y, x]
                if not np.array_equal(actual, expected):
                    raise AssertionError("per-channel mutant gather did not use its selected coefficient tuple")
                if any(np.array_equal(actual, item.details[level][band][y, x]) for item in coefficients):
                    raise AssertionError("tiny per-channel oracle did not prove a mixed RGB coefficient tuple")
                observed = True
                break
            if observed:
                break
        if not observed:
            raise AssertionError("tiny per-channel oracle did not create conflicting source tuples")
        if np.array_equal(honest.composite, mutant.composite):
            raise AssertionError("per-channel mutant did not alter the independent tiny composite")
        raise AssertionError("complex-wavelet detector caught per-channel-selection")
    if broken == "ignore-mask":
        # Poison only a region declared invalid in frame 1.  The independent
        # eligible-mask oracle must be invariant; the broken selector sees it.
        poisoned = frames.copy()
        poisoned[1, ~mask[1]] = 1.0
        masked_honest = fuse(Tiny(poisoned, mask), configuration)
        mutant = fuse(Tiny(poisoned, mask), {**configuration, "broken": broken})
        expected_eligible, expected_ignored = [], []
        for live_stack in (mask, np.ones_like(mask)):
            per_level = []
            for level in range(LEVELS):
                bands = []
                for band in range(3):
                    winner = np.full(honest.raw_winners[level][band].shape, NO_SOURCE, dtype=np.uint16)
                    best = np.zeros(winner.shape, dtype=np.float64)
                    for frame, values in enumerate(poisoned):
                        live = analyse_mask(live_stack[frame])[1][level][band]
                        score = np.abs(analyse(values).details[level][band]) ** 2
                        score = score.sum(axis=2)
                        better = live & ((winner == NO_SOURCE) | (score > best))
                        winner[better], best[better] = frame, score[better]
                    bands.append(winner)
                per_level.append(tuple(bands))
            (expected_eligible if live_stack is mask else expected_ignored).append(tuple(per_level))
        expected_eligible, expected_ignored = expected_eligible[0], expected_ignored[0]
        for level in range(LEVELS):
            for band in range(3):
                if not np.array_equal(honest.raw_winners[level][band], expected_eligible[level][band]):
                    raise AssertionError("eligible-mask oracle disagrees with honest selection")
                if not np.array_equal(masked_honest.raw_winners[level][band], expected_eligible[level][band]):
                    raise AssertionError("invalid source perturbation leaked into honest selection")
                if not np.array_equal(mutant.raw_winners[level][band], expected_ignored[level][band]):
                    raise AssertionError("ignore-mask mutant did not select through invalid coefficients")
        if all(np.array_equal(expected_eligible[level][band], expected_ignored[level][band]) for level in range(LEVELS) for band in range(3)):
            raise AssertionError("ignore-mask oracle did not expose a poisoned invalid coefficient")
        raise AssertionError("complex-wavelet detector caught ignore-mask")
    mutant = fuse(Tiny(frames, mask), {**configuration, "broken": broken})
    if broken == "real-only-magnitude":
        expected = []
        coefficients = [analyse(frame) for frame in frames]
        masks_by_frame = [analyse_mask(item) for item in mask]
        for level in range(LEVELS):
            bands = []
            for band in range(3):
                winner = np.full(coefficients[0].details[level][band].shape[:2], NO_SOURCE, dtype=np.uint16)
                best = np.zeros(winner.shape, dtype=np.float64)
                for frame, (item, (_low, detail_live)) in enumerate(zip(coefficients, masks_by_frame)):
                    score = item.details[level][band].real ** 2
                    if score.ndim == 3:
                        score = score.sum(axis=2)
                    better = detail_live[level][band] & ((winner == NO_SOURCE) | (score > best))
                    winner[better], best[better] = frame, score[better]
                bands.append(winner)
            expected.append(tuple(bands))
        if any(not np.array_equal(mutant.raw_winners[level][band], expected[level][band]) for level in range(LEVELS) for band in range(3)):
            raise AssertionError("real-only mutant did not implement the independent real-energy oracle")
        if all(np.array_equal(honest.raw_winners[level][band], expected[level][band]) for level in range(LEVELS) for band in range(3)):
            raise AssertionError("tiny real-energy oracle did not expose nonzero-imaginary selection")
        raise AssertionError("complex-wavelet detector caught real-only-magnitude")
    if broken == "inconsistent-vote-gather":
        expected = []
        for level in range(LEVELS):
            bands = [item.copy() for item in honest.raw_winners[level]]
            for orientation in range(3):
                other = [honest.raw_winners[level][(orientation + 1) % 3], honest.raw_winners[level][(orientation + 2) % 3]]
                majority = np.where(other[0] == other[1], other[0], NO_SOURCE)
                for frame in range(3):
                    bands[orientation][(majority == frame) & honest.support[level][orientation]] = frame
            expected.append(tuple(bands))
        if any(not np.array_equal(honest.orientation_winners[level][band], expected[level][band]) for level in range(LEVELS) for band in range(3)):
            raise AssertionError("vote control did not implement independent two-of-three oracle")
        if any(not np.array_equal(mutant.orientation_winners[level][band], expected[level][band]) for level in range(LEVELS) for band in range(3)):
            raise AssertionError("vote-gather mutant did not leave the independently expected voted maps")
        if all(np.array_equal(expected[level][band], honest.raw_winners[level][band]) for level in range(LEVELS) for band in range(3)):
            raise AssertionError("tiny vote oracle did not create a changed majority coefficient")
        if np.array_equal(mutant.composite, honest.composite):
            raise AssertionError("stale raw-coefficient gather did not alter the tiny voted reconstruction")
        raise AssertionError("complex-wavelet detector caught inconsistent-vote-gather")
    changed = (any(not np.array_equal(a, b) for left, right in zip(honest.orientation_winners, mutant.orientation_winners) for a, b in zip(left, right))
               or any(not np.array_equal(a, b) for left, right in zip(honest.support, mutant.support) for a, b in zip(left, right))
               or not np.array_equal(honest.composite, mutant.composite))
    if not changed:
        raise AssertionError(f"complex-wavelet mutant {broken} did not alter its independent tiny control")
    raise AssertionError(f"complex-wavelet detector caught {broken}")


def self_checks(output: Path, broken: str | None = None) -> list[dict[str, object]]:
    """Run small independent transform, selection, mask, and tile controls."""
    if broken not in {None, "missing-conjugate", "ignore-mask", "per-channel-selection", "real-only-magnitude", "inconsistent-vote-gather"}:
        raise ValueError("unknown complex-wavelet broken variant")
    output.mkdir(parents=True, exist_ok=True)
    if broken in {"ignore-mask", "per-channel-selection", "real-only-magnitude", "inconsistent-vote-gather"}:
        _mutant_check(broken)
    records = []
    rng = np.random.default_rng(41)
    matrix_shapes = ((64, 64), (96, 128), (65, 97), (97, 65))
    matrix_signals = ("constant", "ramp", "impulse", "random")
    control_index = 0
    for shape in matrix_shapes:
        y, x = np.mgrid[:shape[0], :shape[1]]
        for channels in (1, 3):
            for signal in matrix_signals:
                if signal == "constant":
                    plane = np.full(shape, 17321, dtype=np.uint16)
                elif signal == "ramp":
                    plane = ((y * 257 + x * 911) % 65536).astype(np.uint16)
                elif signal == "impulse":
                    plane = np.zeros(shape, dtype=np.uint16)
                    plane[shape[0] // 2, shape[1] // 2] = 65535
                    plane[0, 0] = 32768
                else:
                    plane = rng.integers(0, 65536, shape, dtype=np.uint16)
                encoded = plane[..., None] if channels == 1 else np.stack((plane, np.roll(plane, 1, axis=0), np.roll(plane, 1, axis=1)), axis=-1)
                for levels in (1, 5):
                    values = encoded.astype(np.float64) / 65535.0
                    coeff = analyse(values, levels)
                    restored = synthesise(coeff, "missing-conjugate" if broken == "missing-conjugate" else None)
                    rounded = np.clip(np.rint(restored.real * 65535.0), 0, 65535).astype(np.uint16)
                    expected = encoded
                    path = output / f"roundtrip-{control_index:02d}.tif"
                    source_path = output / f"input-{control_index:02d}.tif"
                    _write(path, rounded)
                    _write(source_path, expected)
                    different = int(np.count_nonzero(rounded != expected))
                    if different:
                        raise AssertionError("complex-wavelet rounded round-trip differs")
                    records.append({"name": f"roundtrip-{control_index:02d}-{signal}-c{channels}-l{levels}", "shape": list(shape),
                                    "max_real_error": float(np.max(np.abs(restored.real - values))),
                                    "max_imaginary_residual": float(np.max(np.abs(restored.imag))), "different_samples": different,
                                    "input": str(source_path), "output": str(path)})
                    control_index += 1
    if control_index != 64:
        raise AssertionError("complex-wavelet public round-trip matrix is not the registered 64 controls")
    identity_values = rng.integers(0, 65536, (64, 64, 3), dtype=np.uint16).astype(np.float64) / 65535.0
    class IdentitySource:
        h, w, c = 64, 64, 3
        def __init__(self, count: int, live: bool = True) -> None:
            self.n, self.live = count, live
        def read(self, frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
            y0, y1, x0, x1 = rect or (0, 64, 0, 64)
            return identity_values[y0:y1, x0:x1], np.full((y1-y0, x1-x0), self.live, dtype=bool)
    encoded_identity = np.rint(identity_values * 65535.0).astype(np.uint16)
    for count in (1, 2, 8, 32):
        for configuration in CONFIGURATIONS:
            identity = fuse(IdentitySource(count), configuration)
            if not np.array_equal(identity.composite, encoded_identity):
                raise AssertionError("complex-wavelet identical divisible stack did not preserve encoded input")
            if any(np.any(item == NO_SOURCE) for level in identity.orientation_winners for item in level):
                raise AssertionError("complex-wavelet identical divisible stack has uncovered detail map")
            if any(np.any(item != 0) for level in identity.orientation_winners for item in level):
                raise AssertionError("complex-wavelet strict first-eligible tie did not retain frame zero")
        invalid = fuse(IdentitySource(count, live=False), CONFIGURATIONS[1])
        if np.any(invalid.composite) or any(np.any(item != NO_SOURCE) for level in invalid.orientation_winners for item in level):
            raise AssertionError("complex-wavelet all-invalid stack did not remain explicitly uncovered")
        if any(int(invalid.counts[name]) for name in ("untouched", "voted", "rejected")):
            raise AssertionError("complex-wavelet all-invalid stack counted unsupported coefficients as owned")
        records.append({"name": f"identity-n{count}", "different_samples": 0, "all_invalid_zero": True})
    complementary = np.repeat(identity_values[None, ...], 2, axis=0)
    complementary_masks = np.zeros((2, 64, 64), dtype=bool)
    complementary_masks[0, :, :32] = True
    complementary_masks[1, :, 32:] = True
    class MatrixSource:
        def __init__(self, samples: np.ndarray, live: np.ndarray) -> None:
            self.samples, self.live = samples, live
            self.n, self.h, self.w, self.c = samples.shape
        def read(self, frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
            y0, y1, x0, x1 = rect or (0, self.h, 0, self.w)
            return self.samples[frame, y0:y1, x0:x1], self.live[frame, y0:y1, x0:x1]
    complementary_source = MatrixSource(complementary, complementary_masks)
    poisoned = complementary.copy()
    poisoned[0, :, 32:] = 1.0
    poisoned[1, :, :32] = 0.0
    safe = fuse(complementary_source, CONFIGURATIONS[1])
    perturbed = fuse(MatrixSource(poisoned, complementary_masks), CONFIGURATIONS[1])
    if (not np.array_equal(safe.composite, perturbed.composite)
            or any(not np.array_equal(a, b) for aa, bb in zip(safe.orientation_winners, perturbed.orientation_winners) for a, b in zip(aa, bb))
            or any(not np.array_equal(a, b) for aa, bb in zip(safe.raw_winners, perturbed.raw_winners) for a, b in zip(aa, bb))):
        raise AssertionError("invalid complementary source samples changed the synthesis-safe output")
    odd_values = rng.integers(0, 65536, (65, 97, 3), dtype=np.uint16).astype(np.float64) / 65535.0
    odd_live = np.zeros((1, 65, 97), dtype=bool)
    odd_live[:, 16:49, 16:81] = True
    odd_source = MatrixSource(odd_values[None, ...], odd_live)
    odd_safe = fuse(odd_source, CONFIGURATIONS[0])
    altered_odd = odd_values.copy()
    altered_odd[~odd_live[0]] = 1.0
    odd_perturbed = fuse(MatrixSource(altered_odd[None, ...], odd_live), CONFIGURATIONS[0])
    if not np.array_equal(odd_safe.composite, odd_perturbed.composite):
        raise AssertionError("odd masked eligible synthesis changed after an invalid-sample perturbation")
    # Erode the valid input by the full analysis+synthesis bound, not a finest-band mask.
    core_values = rng.integers(0, 65536, (545, 547, 3), dtype=np.uint16).astype(np.float64) / 65535.0
    core_live = np.zeros((3, 545, 547), dtype=bool)
    core_live[1:, 8:-8, 8:-8] = True
    core_support = np.zeros((545, 547), dtype=bool)
    margin = ((theoretical_reach() + 31) // 32) * 32
    core_support[8 + margin:545 - 8 - margin, 8 + margin:547 - 8 - margin] = True
    if not core_support.any():
        raise AssertionError("odd masked identity control has no synthesis-safe pixels")
    core_source = MatrixSource(np.stack([core_values] * 3), core_live)
    for configuration in CONFIGURATIONS:
        core_result = fuse(core_source, configuration)
        if not np.array_equal(core_result.composite[core_support], np.rint(core_values[core_support] * 65535.0).astype(np.uint16)):
            raise AssertionError("odd masked eligible synthesis did not preserve its proven safe support")
        supported = [item[live] for level, masks in zip(core_result.orientation_winners, core_result.support) for item, live in zip(level, masks)]
        if not any(item.size for item in supported) or any(np.any(item != 1) for item in supported):
            raise AssertionError("public strict tie did not choose its first eligible nonzero source")
    border_pull = metrics.gff.border_pull(fuse(IdentitySource(2), CONFIGURATIONS[0]).composite, IdentitySource(2))
    if border_pull != 0.0:
        raise AssertionError("complex-wavelet identical stack has nonzero border pull")
    records.append({"name": "complementary-mask-invalid-perturbation", "different_samples": 0,
                    "uncovered_coefficients": int(sum(np.count_nonzero(item == NO_SOURCE) for level in safe.orientation_winners for item in level)),
                    "odd_masked_synthesis_identity": True,
                    "odd_masked_safe_pixels": int(core_support.sum()), "first_eligible_nonzero_source": True,
                    "border_pull": border_pull})
    try:
        fuse_tiled(IdentitySource(1), CONFIGURATIONS[0], margin=128)
    except ValueError:
        records.append({"name": "undersized-margin-refusal", "margin": 128, "required_theoretical_reach": theoretical_reach()})
    else:
        raise AssertionError("complex-wavelet accepted undersized tile margin")
    small_height, small_width = 65, 97
    small_plane = np.arange(small_height * small_width, dtype=np.uint32).reshape(small_height, small_width)
    small_frames = np.stack([np.stack(((small_plane + 101 * frame) % 65536,
                                       (small_plane + 211 * frame) % 65536,
                                       (small_plane + 307 * frame) % 65536), axis=-1)
                             for frame in range(3)]).astype(np.uint16)
    small_masks = np.ones((3, small_height, small_width), dtype=bool)
    small_masks[2, 19:43, 31:67] = False
    small_paths = []
    for frame in range(3):
        path = output / "physical-65x97" / f"{frame:02d}.tif"
        _write(path, small_frames[frame])
        small_paths.append(path)
    small_mask_path = output / "physical-65x97" / "masks.npy"
    np.save(small_mask_path, small_masks)
    small_array = MatrixSource(small_frames.astype(np.float64) / 65535.0, small_masks)
    small_tiff = metrics.gff.TiffSource(small_paths, small_mask_path)
    for configuration in CONFIGURATIONS:
        small_whole = fuse(small_array, configuration)
        small_tiled, small_plan = fuse_tiled(small_tiff, configuration)
        small_pixel_equal = ((small_whole.pixel_sources is None and small_tiled.pixel_sources is None)
                             or (small_whole.pixel_sources is not None and small_tiled.pixel_sources is not None
                                 and np.array_equal(small_whole.pixel_sources, small_tiled.pixel_sources)))
        if (not np.array_equal(small_whole.composite, small_tiled.composite)
                or any(not np.array_equal(a, b) for aa, bb in zip(small_whole.orientation_winners, small_tiled.orientation_winners) for a, b in zip(aa, bb))
                or any(not np.array_equal(a, b) for aa, bb in zip(small_whole.raw_winners, small_tiled.raw_winners) for a, b in zip(aa, bb))
                or any(not np.array_equal(a, b) for aa, bb in zip(small_whole.support, small_tiled.support) for a, b in zip(aa, bb))
                or not small_pixel_equal):
            raise AssertionError("65x97 physical TIFF whole/tile raw final support or pixel map differs")
        small_reads = [entry for window in small_plan["windows"] for entry in window["physical_subreads"]]
        records.append({"name": f"physical-65x97-{configuration['id']}", "different_samples": 0,
                        "raw_final_support_pixel_equal": True, "reads": small_reads})
    height, width = 545, 547
    plane = np.arange(height * width, dtype=np.uint32).reshape(height, width)
    frames = np.stack([np.stack([(plane + 137 * frame + channel) % 65536 for channel in range(3)], axis=-1) for frame in range(3)]).astype(np.uint16)
    masks = np.ones((3, height, width), dtype=bool)
    masks[1, 210:334, 201:346] = False
    class Source:
        n, h, w, c = 3, height, width, 3
        def read(self, frame: int, rect: tuple[int, int, int, int] | None = None) -> tuple[np.ndarray, np.ndarray]:
            y0, y1, x0, x1 = rect or (0, height, 0, width)
            current = masks[frame]
            return frames[frame, y0:y1, x0:x1].astype(np.float64) / 65535.0, current[y0:y1, x0:x1]
    for configuration in CONFIGURATIONS:
        whole = fuse(Source(), configuration)
        tiled, plan = fuse_tiled(Source(), configuration)
        whole_path, tiled_path = output / f"whole-{configuration['id']}.tif", output / f"tiled-{configuration['id']}.tif"
        _write(whole_path, whole.composite)
        _write(tiled_path, tiled.composite)
        map_differences, raw_map_differences, support_differences = [], [], []
        for level, (whole_bands, tiled_bands) in enumerate(zip(whole.orientation_winners, tiled.orientation_winners)):
            for band, (whole_map, tiled_map) in enumerate(zip(whole_bands, tiled_bands)):
                whole_map_path = output / f"whole-{configuration['id']}-l{level}-b{band}.tif"
                tiled_map_path = output / f"tiled-{configuration['id']}-l{level}-b{band}.tif"
                raw_whole_path = output / f"whole-{configuration['id']}-raw-l{level}-b{band}.tif"
                raw_tiled_path = output / f"tiled-{configuration['id']}-raw-l{level}-b{band}.tif"
                support_whole_path = output / f"whole-{configuration['id']}-support-l{level}-b{band}.tif"
                support_tiled_path = output / f"tiled-{configuration['id']}-support-l{level}-b{band}.tif"
                _write(whole_map_path, whole_map)
                _write(tiled_map_path, tiled_map)
                _write(raw_whole_path, whole.raw_winners[level][band])
                _write(raw_tiled_path, tiled.raw_winners[level][band])
                _write(support_whole_path, whole.support[level][band].astype(np.uint16))
                _write(support_tiled_path, tiled.support[level][band].astype(np.uint16))
                map_differences.append(int(np.count_nonzero(whole_map != tiled_map)))
                raw_map_differences.append(int(np.count_nonzero(whole.raw_winners[level][band] != tiled.raw_winners[level][band])))
                support_differences.append(int(np.count_nonzero(whole.support[level][band] != tiled.support[level][band])))
        pixel_difference = 0 if whole.pixel_sources is None else int(np.count_nonzero(whole.pixel_sources != tiled.pixel_sources))
        if whole.pixel_sources is not None:
            _write(output / f"whole-{configuration['id']}-pixel-sources.tif", whole.pixel_sources)
            _write(output / f"tiled-{configuration['id']}-pixel-sources.tif", tiled.pixel_sources)
        if (not np.array_equal(whole.composite, tiled.composite) or pixel_difference or any(map_differences)
                or any(raw_map_differences) or any(support_differences)):
            raise AssertionError(f"complex-wavelet tiled control differs from whole output; final={map_differences} raw={raw_map_differences} support={support_differences}")
        per_band = tiled.counts["per_band"]
        if (sum(int(item["owned_support"]) for item in per_band) != sum(int(item.sum()) for level in tiled.support for item in level)
                or any(int(item["untouched"]) + int(item["voted"]) != int(item["owned_support"]) for item in per_band)):
            raise AssertionError("tiled owned coefficient counters include halo or unsupported coefficients")
        records.append({"name": f"whole-tile-{configuration['id']}", "max_abs": 0, "different_samples": 0,
                        "whole": str(whole_path), "tiled": str(tiled_path), "orientation_support_pixels": [int(item.sum()) for level in whole.support for item in level],
                        "votes": whole.counts, "orientation_map_differences": map_differences, "raw_map_differences": raw_map_differences,
                        "support_differences": support_differences, "pixel_source_differences": pixel_difference, "tile": plan})
        physical = output / "physical-source"
        paths = []
        for frame in range(3):
            path = physical / f"{frame:02d}.tif"
            _write(path, frames[frame])
            paths.append(path)
        mask_path = physical / "masks.npy"
        np.save(mask_path, masks)
        tiff = metrics.gff.TiffSource(paths, mask_path)
        physical_tiled, physical_plan = fuse_tiled(tiff, configuration)
        if (not np.array_equal(whole.composite, physical_tiled.composite)
                or any(not np.array_equal(a, b) for left, right in zip(whole.orientation_winners, physical_tiled.orientation_winners) for a, b in zip(left, right))
                or any(not np.array_equal(a, b) for left, right in zip(whole.raw_winners, physical_tiled.raw_winners) for a, b in zip(left, right))
                or any(not np.array_equal(a, b) for left, right in zip(whole.support, physical_tiled.support) for a, b in zip(left, right))):
            raise AssertionError("physical TIFF tiled control differs from array whole output")
        if ((whole.pixel_sources is None) != (physical_tiled.pixel_sources is None)
                or (whole.pixel_sources is not None and not np.array_equal(whole.pixel_sources, physical_tiled.pixel_sources))):
            raise AssertionError("physical TIFF tiled pixel source map differs from array whole output")
        reads = [entry for window in physical_plan["windows"] for entry in window["physical_subreads"]]
        full_bytes = height * width * 3 * 2
        if not reads or not any(entry["bytes"] < full_bytes for entry in reads):
            raise AssertionError("physical TIFF tile control did not perform a restricted subread")
        records.append({"name": "physical-tiff-subreads", "reads": reads, "full_frame_bytes": full_bytes,
                        "restricted_reads": sum(entry["bytes"] < full_bytes for entry in reads)})
    impulse_frames = np.zeros_like(frames)
    impulse_frames[0, 0, 0, 0] = 65535
    impulse_frames[1, height // 2, width // 2, 1] = 65535
    impulse_frames[2, height - 1, width - 1, 2] = 65535
    saved_frames = frames
    frames = impulse_frames
    for configuration in CONFIGURATIONS:
        impulse_whole = fuse(Source(), configuration)
        impulse_tiled, impulse_plan = fuse_tiled(Source(), configuration, margin=160)
        impulse_final = [int(np.count_nonzero(left != right)) for left_bands, right_bands in zip(impulse_whole.orientation_winners, impulse_tiled.orientation_winners) for left, right in zip(left_bands, right_bands)]
        impulse_support = [int(np.count_nonzero(left != right)) for left_bands, right_bands in zip(impulse_whole.support, impulse_tiled.support) for left, right in zip(left_bands, right_bands)]
        impulse_image = int(np.count_nonzero(impulse_whole.composite != impulse_tiled.composite))
        if impulse_image or any(impulse_final) or any(impulse_support):
            raise AssertionError("complex-wavelet empirical impulse tile reach differs from whole output")
        records.append({"name": f"empirical-impulse-reach-{configuration['id']}", "margin": 160,
                        "theoretical_reach": theoretical_reach(), "image_differences": impulse_image,
                        "orientation_map_differences": impulse_final, "support_differences": impulse_support,
                        "tile": impulse_plan})
    frames = saved_frames
    constant_frames = np.full_like(frames, 32768)
    edge_impulse_frames = constant_frames.copy()
    edge_impulse_frames[0, 0, 0, :] = 65535
    coefficient_delta_path = output / "edge-coefficient-deltas.npz"
    def footprint(values: np.ndarray) -> list[dict[str, object]]:
        delta = analyse(values.astype(np.float64) / 65535.0)
        reference = analyse(constant_frames[0].astype(np.float64) / 65535.0)
        footprint_records, retained = [], {}
        for level, (bands, baseline_bands) in enumerate(zip(delta.details, reference.details)):
            stride = 1 << (level + 1)
            for orientation, (band, baseline_band) in enumerate(zip(bands, baseline_bands)):
                difference = band - baseline_band
                real = np.argwhere(np.abs(difference.real) > 1e-12)
                imag = np.argwhere(np.abs(difference.imag) > 1e-12)
                coeff_h, coeff_w = difference.shape[:2]
                def toroidal_extent(points: np.ndarray) -> dict[str, int]:
                    if not len(points):
                        return {"max_y_px": 0, "max_x_px": 0, "max_radius_px": 0}
                    yy = np.minimum(points[:, 0], coeff_h - points[:, 0]) * stride
                    xx = np.minimum(points[:, 1], coeff_w - points[:, 1]) * stride
                    return {"max_y_px": int(yy.max()), "max_x_px": int(xx.max()),
                            "max_radius_px": int(np.maximum(yy, xx).max())}
                retained[f"l{level}b{orientation}_real"] = difference.real
                retained[f"l{level}b{orientation}_imag"] = difference.imag
                footprint_records.append({"level": level, "orientation": orientation, "stride": stride,
                                          "coefficient_shape": [coeff_h, coeff_w],
                                          "real_nonzero": int(len(real)), "imag_nonzero": int(len(imag)),
                                          "real_toroidal_extent": toroidal_extent(real),
                                          "imag_toroidal_extent": toroidal_extent(imag)})
        np.savez(coefficient_delta_path, **retained)
        return footprint_records
    measured_coefficients = footprint(edge_impulse_frames[0])
    if any(max(item["real_toroidal_extent"]["max_radius_px"], item["imag_toroidal_extent"]["max_radius_px"]) > 160
           for item in measured_coefficients):
        raise AssertionError("measured coefficient influence exceeds the frozen tile margin")
    frames = constant_frames
    constant_results = {item["id"]: fuse(Source(), item) for item in CONFIGURATIONS}
    # The padded global corner is deliberately ineligible.  Measure the
    # coefficient wrap there above, then measure a selectable interior impulse
    # for the reconstructed synthesis footprint.
    synthesis_impulse_frames = constant_frames.copy()
    synthesis_impulse_frames[0, height // 2, width // 2, :] = 65535
    frames = synthesis_impulse_frames
    for configuration in CONFIGURATIONS:
        changed = fuse(Source(), configuration)
        synthesis_delta = np.argwhere(changed.composite != constant_results[configuration["id"]].composite)
        delta_path = output / f"edge-synthesis-delta-{configuration['id']}.npy"
        np.save(delta_path, changed.composite.astype(np.int32) - constant_results[configuration["id"]].composite.astype(np.int32))
        if len(synthesis_delta):
            dy0 = np.abs(synthesis_delta[:, 0] - height // 2)
            dx0 = np.abs(synthesis_delta[:, 1] - width // 2)
            dy = np.minimum(dy0, height - dy0)
            dx = np.minimum(dx0, width - dx0)
            synthesis_extent = {"max_y_px": int(dy.max()), "max_x_px": int(dx.max()),
                                "max_radius_px": int(np.maximum(dy, dx).max())}
        else:
            synthesis_extent = {"max_y_px": 0, "max_x_px": 0, "max_radius_px": 0}
        if synthesis_extent["max_radius_px"] > 160:
            raise AssertionError("measured synthesis influence exceeds the frozen tile margin")
        records.append({"name": f"measured-global-edge-footprint-{configuration['id']}", "coefficient_real_imag": measured_coefficients,
                        "coefficient_delta_arrays": str(coefficient_delta_path), "synthesis_delta_array": str(delta_path),
                        "synthesis_changed_samples": int(len(synthesis_delta)), "synthesis_toroidal_extent": synthesis_extent,
                        "constant_stack": True, "global_edge_impulse": [0, 0],
                        "synthesis_impulse": [height // 2, width // 2]})
    frames = saved_frames
    shift = np.zeros((64, 64), dtype=np.float64)
    shift[17:47, 29] = 1.0
    shifts = {"x": np.roll(shift, 1, axis=1), "y": np.roll(shift, 1, axis=0)}
    energy = [float((np.abs(band) ** 2).sum()) for bands in analyse(shift).details for band in bands]
    shifted_energy = {axis: [float((np.abs(band) ** 2).sum()) for bands in analyse(value).details for band in bands]
                      for axis, value in shifts.items()}
    real_energy = [float((band.real ** 2).sum()) for bands in analyse(shift).details for band in bands]
    shifted_real_energy = {axis: [float((band.real ** 2).sum()) for bands in analyse(value).details for band in bands]
                           for axis, value in shifts.items()}
    shift_raw_path = output / "shift-input.npy"
    shifted_raw_path = output / "shifted-inputs.npz"
    np.save(shift_raw_path, shift)
    np.savez(shifted_raw_path, **shifts)
    if broken == "real-only-magnitude":
        energy = real_energy
    if np.array_equal(np.asarray(energy), np.asarray(real_energy)):
        raise AssertionError("complex magnitude control became real-only")
    records.append({"name": "shift-energy", "complex": energy, "shifted_complex": shifted_energy, "real_only": real_energy,
                    "shifted_real_only": shifted_real_energy, "raw_input": str(shift_raw_path), "raw_shifted_inputs": str(shifted_raw_path)})
    return records
