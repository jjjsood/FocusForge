# SPDX-License-Identifier: GPL-3.0-or-later
"""Fixed encoded-RGB photometric correction experiment semantics."""

from __future__ import annotations

from dataclasses import dataclass, field
from hashlib import sha256
import json
from pathlib import Path
import re

import numpy as np

from .protocol import FRAME_COUNT, IMAGE_SIZE, SIGMA_STEP, TRAINING_SEEDS, SeedTriple, require_seed
from .scenes import SceneData, _apply_hairs, _layout, ss

REFERENCE = (FRAME_COUNT - 1) // 2
LOW, HIGH = .8, 1.25
VARIANTS = ("global-gain-offset", "channel-gain-offset", "static-linear-x", "frame-quadratic", "cross-xy", "unchanged-focus", "bracket")
FITTED_ARMS = ("global-gain", "channel-gain-offset", "spatial-multiplier")
REQUIRED_FUSED = ("psnr", "ssim", "edge_halo", "hair_retention", "chroma_drift", "label_exact", "label_within_one", "label_mad")
NULL_FUSED = ("flat_noise", "border_contamination", "seam_energy", "tile_difference", "geometry_error", "alignment_drift", "alignment_failures", "gradient_energy", "map_coherence")
HIGHER_FUSED = ("psnr", "ssim", "hair_retention", "label_exact", "label_within_one")
IDENTITY_PARAMETERS = {
    "global-gain": (("gain_deviation", "dimensionless"),),
    "channel-gain-offset": (("gain_red_deviation", "dimensionless"), ("gain_green_deviation", "dimensionless"),
                              ("gain_blue_deviation", "dimensionless"), ("offset_red_deviation", "encoded code counts"),
                              ("offset_green_deviation", "encoded code counts"), ("offset_blue_deviation", "encoded code counts")),
    "spatial-multiplier": (("field_max_deviation", "dimensionless"),),
}


@dataclass(frozen=True)
class Fit:
    """Estimated and applied correction, including every refusal binding."""

    arm: str
    values: np.ndarray
    identity: bool
    thin: bool
    rank: int
    clamped: bool
    estimated: np.ndarray = field(default_factory=lambda: np.empty(0, dtype=np.float64))
    reason: str | None = None
    sample_count: int = 0
    reference: int = REFERENCE
    shrink: float = 1.0

    def record(self) -> dict[str, object]:
        return {"arm": self.arm, "estimated": self.estimated.tolist(), "applied": self.values.tolist(),
                "sample_count": self.sample_count, "rank": self.rank, "identity": self.identity,
                "thin": self.thin, "clamped": bool(self.clamped), "reason": self.reason,
                "shrink": self.shrink, "reference": self.reference}


def _coordinates(height: int, width: int) -> tuple[np.ndarray, np.ndarray]:
    y, x = np.mgrid[:height, :width]
    xx = np.zeros_like(x, dtype=np.float64) if width == 1 else 2.0 * x / (width - 1) - 1.0
    yy = np.zeros_like(y, dtype=np.float64) if height == 1 else 2.0 * y / (height - 1) - 1.0
    return xx, yy


def _coverage(coverage: np.ndarray | None, frame: int, reference: int, height: int, width: int) -> np.ndarray:
    if coverage is None:
        return np.ones((height, width), dtype=bool)
    values = np.asarray(coverage, dtype=bool)
    if values.shape != (FRAME_COUNT, height, width):
        raise ValueError("photometric coverage shape differs from frames")
    return values[frame] & values[reference]


def _samples(frames: np.ndarray, coverage: np.ndarray | None, frame: int, reference: int) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
    source, target = frames[frame].astype(np.float64), frames[reference].astype(np.float64)
    height, width = source.shape[:2]
    step = max(1, int(np.ceil(np.sqrt(width * height / 262144))))
    valid = _coverage(coverage, frame, reference, height, width)
    luma_source, luma_target = source.mean(axis=2), target.mean(axis=2)
    valid &= np.isfinite(luma_source) & np.isfinite(luma_target)
    valid &= (luma_target >= .05 * 65535) & (luma_target <= .95 * 65535) & (luma_source > 0)
    grid = np.zeros_like(valid)
    grid[::step, ::step] = True
    ys, xs = np.nonzero(valid & grid)
    return source[ys, xs], target[ys, xs], xs, ys


def _identity(arm: str, width: int = 1, rank: int = 0, thin: bool = False, reason: str | None = None,
              sample_count: int = 0, reference: int = REFERENCE) -> Fit:
    if arm == "global-gain":
        values = np.array([1.0], dtype=np.float64)
    elif arm == "channel-gain-offset":
        values = np.array((1., 0., 1., 0., 1., 0.), dtype=np.float64)
    elif arm == "spatial-multiplier":
        values = np.array((1., 0., 0., 0., 0.), dtype=np.float64)
    else:
        values = np.ones(width, dtype=np.float64)
    return Fit(arm, values, True, thin, rank, False, values.copy(), reason, sample_count, reference)


def _global(frames: np.ndarray, coverage: np.ndarray | None, frame: int, reference: int = REFERENCE) -> Fit:
    if frame == reference:
        source, _, _, _ = _samples(frames, coverage, frame, reference)
        return _identity("global-gain", rank=1, thin=len(source) < 1000, reason="reference", sample_count=len(source), reference=reference)
    source, target, _, _ = _samples(frames, coverage, frame, reference)
    count = len(source)
    if count < 1000:
        return _identity("global-gain", rank=1, thin=True, reason="thin", sample_count=count, reference=reference)
    ratio = float(np.median(target.mean(axis=1) / source.mean(axis=1)))
    if not np.isfinite(ratio):
        return _identity("global-gain", rank=1, reason="nonfinite", sample_count=count, reference=reference)
    applied = float(np.clip(ratio, LOW, HIGH))
    return Fit("global-gain", np.array([applied]), False, False, 1, applied != ratio, np.array([ratio]), None, count, reference)


def _channel(frames: np.ndarray, coverage: np.ndarray | None, frame: int, reference: int = REFERENCE) -> Fit:
    if frame == reference:
        return _identity("channel-gain-offset", 6, rank=2, reason="reference", reference=reference)
    source, target, _, _ = _samples(frames, coverage, frame, reference)
    count = len(source)
    if count < 1000:
        return _identity("channel-gain-offset", 6, rank=0, thin=True, reason="thin", sample_count=count, reference=reference)
    values, estimated, ranks = [], [], []
    try:
        for channel in range(3):
            design = np.column_stack((source[:, channel] / 65535.0, np.ones(count)))
            solved, _, rank, _ = np.linalg.lstsq(design, target[:, channel] / 65535.0, rcond=None)
            ranks.append(int(rank))
            if rank < 2 or not np.all(np.isfinite(solved)):
                return _identity("channel-gain-offset", 6, rank=min(ranks), reason="rank-or-nonfinite", sample_count=count, reference=reference)
            estimated_gain, estimated_offset = float(solved[0]), float(solved[1] * 65535.0)
            gain = float(np.clip(estimated_gain, LOW, HIGH))
            endpoints = (.05 * 65535.0, .95 * 65535.0)
            minimum = max((LOW - gain) * endpoint for endpoint in endpoints)
            maximum = min((HIGH - gain) * endpoint for endpoint in endpoints)
            values.extend((gain, float(np.clip(estimated_offset, minimum, maximum))))
            estimated.extend((estimated_gain, estimated_offset))
    except np.linalg.LinAlgError:
        return _identity("channel-gain-offset", 6, rank=min(ranks, default=0), reason="solver-failure", sample_count=count, reference=reference)
    applied = np.asarray(values, dtype=np.float64)
    return Fit("channel-gain-offset", applied, False, False, min(ranks), not np.array_equal(applied, np.asarray(estimated)), np.asarray(estimated), None, count, reference)


def _spatial_extrema(coefficients: np.ndarray) -> tuple[float, float]:
    candidates = [-1.0, 1.0]
    for linear, quadratic in ((coefficients[1], coefficients[2]), (coefficients[3], coefficients[4])):
        if quadratic != 0:
            point = -linear / (2.0 * quadratic)
            if -1.0 <= point <= 1.0:
                candidates.append(float(point))
    values = [coefficients[0] + coefficients[1] * x + coefficients[2] * x * x + coefficients[3] * y + coefficients[4] * y * y for x in candidates for y in candidates]
    return min(values), max(values)


def _spatial_bound(solved: np.ndarray) -> tuple[np.ndarray, float]:
    """Contract a finite spatial field toward identity with literal bounds."""
    minimum, maximum = _spatial_extrema(solved)
    fallback = np.array((1., 0., 0., 0., 0.), dtype=np.float64)
    if not np.isfinite((minimum, maximum)).all():
        return fallback, 0.0
    if LOW <= minimum and maximum <= HIGH:
        return np.array(solved, dtype=np.float64), 1.0
    low = np.nextafter(LOW, np.inf)
    high = np.nextafter(HIGH, -np.inf)
    factor = 1.0
    if maximum > high:
        factor = min(factor, (high - 1.0) / (maximum - 1.0))
    if minimum < low:
        factor = min(factor, (1.0 - low) / (1.0 - minimum))
    factor = float(np.nextafter(max(0.0, factor), 0.0))
    applied = np.array(solved, dtype=np.float64)
    applied[0] = 1.0 + factor * (applied[0] - 1.0)
    applied[1:] *= factor
    low, high = _spatial_extrema(applied)
    if (not np.isfinite((low, high)).all() or not (LOW <= low and high <= HIGH)
            or low <= 0.0 or high <= 0.0
            or abs(np.log2(low)) > np.log2(HIGH) or abs(np.log2(high)) > np.log2(HIGH)):
        return fallback, 0.0
    return applied, factor


def _spatial(frames: np.ndarray, coverage: np.ndarray | None, frame: int, reference: int = REFERENCE) -> Fit:
    if frame == reference:
        return _identity("spatial-multiplier", 5, rank=5, reason="reference", reference=reference)
    source, target, xs, ys = _samples(frames, coverage, frame, reference)
    count = len(source)
    if count < 1000:
        return _identity("spatial-multiplier", 5, rank=0, thin=True, reason="thin", sample_count=count, reference=reference)
    height, width = frames.shape[1:3]
    if height <= 1 or width <= 1:
        return _identity("spatial-multiplier", 5, rank=0, reason="degenerate-dimensions", sample_count=count, reference=reference)
    x = 2.0 * xs / (width - 1) - 1.0
    y = 2.0 * ys / (height - 1) - 1.0
    design = np.column_stack((np.ones(count), x, x * x, y, y * y))
    try:
        solved, _, rank, _ = np.linalg.lstsq(design, target.mean(axis=1) / source.mean(axis=1), rcond=None)
    except np.linalg.LinAlgError:
        return _identity("spatial-multiplier", 5, rank=0, reason="solver-failure", sample_count=count, reference=reference)
    if rank < 5 or not np.all(np.isfinite(solved)):
        return _identity("spatial-multiplier", 5, rank=int(rank), reason="rank-or-nonfinite", sample_count=count, reference=reference)
    minimum, maximum = _spatial_extrema(solved)
    if not np.isfinite((minimum, maximum)).all():
        return _identity("spatial-multiplier", 5, rank=int(rank), reason="nonfinite", sample_count=count, reference=reference)
    applied, factor = _spatial_bound(solved)
    return Fit("spatial-multiplier", applied, False, False, int(rank), factor != 1.0, np.asarray(solved), None, count, reference, factor)


def estimate(arm: str, frames: np.ndarray, coverage: np.ndarray | None, frame: int, reference: int = REFERENCE) -> Fit:
    """Estimate one registered arm against the fixed reference frame."""
    array = np.asarray(frames)
    if array.ndim != 4 or array.shape[0] != FRAME_COUNT or array.shape[-1] != 3:
        raise ValueError("photometric estimator requires eight RGB frames")
    if not 0 <= frame < FRAME_COUNT or not 0 <= reference < FRAME_COUNT:
        raise ValueError("photometric frame binding is outside range")
    if arm == "off":
        return _identity("off", rank=1, reason="off", reference=reference)
    if arm == "global-gain":
        return _global(array, coverage, frame, reference)
    if arm == "channel-gain-offset":
        return _channel(array, coverage, frame, reference)
    if arm == "spatial-multiplier":
        return _spatial(array, coverage, frame, reference)
    raise ValueError(f"unknown photometric arm: {arm}")


def estimate_samples(arm: str, source: np.ndarray, target: np.ndarray, xs: np.ndarray, ys: np.ndarray,
                     width: int, height: int, frame: int, reference: int = REFERENCE) -> Fit:
    """Estimate one arm from the physical full-frame sample grid used by streaming real input."""
    count = len(source)
    if arm == "off":
        return _identity("off", rank=1, reason="off", sample_count=count, reference=reference)
    if frame == reference:
        widths = {"global-gain": 1, "channel-gain-offset": 6, "spatial-multiplier": 5}
        ranks = {"global-gain": 1, "channel-gain-offset": 2, "spatial-multiplier": 5}
        return _identity(arm, widths[arm], ranks[arm], thin=count < 1000, reason="reference", sample_count=count, reference=reference)
    if count < 1000:
        return _identity(arm, {"global-gain": 1, "channel-gain-offset": 6, "spatial-multiplier": 5}[arm], thin=True, reason="thin", sample_count=count, reference=reference)
    if arm == "global-gain":
        ratio = float(np.median(target.mean(axis=1) / source.mean(axis=1)))
        if not np.isfinite(ratio): return _identity(arm, rank=1, reason="nonfinite", sample_count=count, reference=reference)
        applied = float(np.clip(ratio, LOW, HIGH))
        return Fit(arm, np.array([applied]), False, False, 1, applied != ratio, np.array([ratio]), None, count, reference)
    if arm == "channel-gain-offset":
        values, estimated, ranks = [], [], []
        try:
            for channel in range(3):
                solved, _, rank, _ = np.linalg.lstsq(np.column_stack((source[:, channel] / 65535., np.ones(count))), target[:, channel] / 65535., rcond=None)
                ranks.append(int(rank))
                if rank < 2 or not np.all(np.isfinite(solved)): return _identity(arm, 6, rank=min(ranks), reason="rank-or-nonfinite", sample_count=count, reference=reference)
                gain0, offset0 = float(solved[0]), float(solved[1] * 65535.)
                gain = float(np.clip(gain0, LOW, HIGH)); ends = (.05 * 65535., .95 * 65535.)
                offset = float(np.clip(offset0, max((LOW-gain)*end for end in ends), min((HIGH-gain)*end for end in ends)))
                values.extend((gain, offset)); estimated.extend((gain0, offset0))
        except np.linalg.LinAlgError:
            return _identity(arm, 6, rank=min(ranks, default=0), reason="solver-failure", sample_count=count, reference=reference)
        applied = np.asarray(values); return Fit(arm, applied, False, False, min(ranks), not np.array_equal(applied, np.asarray(estimated)), np.asarray(estimated), None, count, reference)
    if width <= 1 or height <= 1:
        return _identity(arm, 5, reason="degenerate-dimensions", sample_count=count, reference=reference)
    x, y = 2. * xs / (width - 1) - 1., 2. * ys / (height - 1) - 1.
    try:
        solved, _, rank, _ = np.linalg.lstsq(np.column_stack((np.ones(count), x, x*x, y, y*y)), target.mean(axis=1) / source.mean(axis=1), rcond=None)
    except np.linalg.LinAlgError:
        return _identity(arm, 5, reason="solver-failure", sample_count=count, reference=reference)
    if rank < 5 or not np.all(np.isfinite(solved)): return _identity(arm, 5, rank=int(rank), reason="rank-or-nonfinite", sample_count=count, reference=reference)
    minimum, maximum = _spatial_extrema(solved)
    if not np.isfinite((minimum, maximum)).all(): return _identity(arm, 5, rank=int(rank), reason="nonfinite", sample_count=count, reference=reference)
    applied, factor = _spatial_bound(solved)
    return Fit(arm, applied, False, False, int(rank), factor != 1., np.asarray(solved), None, count, reference, factor)


def apply(frame: np.ndarray, fit: Fit, origin_xy: tuple[int, int] = (0, 0), full_shape: tuple[int, int] | None = None) -> np.ndarray:
    """Apply one fit with global coordinates and registered rounding."""
    if fit.identity or fit.arm == "off":
        return np.asarray(frame, dtype=np.uint16).copy()
    source = np.asarray(frame, dtype=np.float64)
    if fit.arm == "global-gain":
        gain = int(np.floor(fit.values[0] * 65536.0 + .5))
        return np.minimum((source.astype(np.uint64) * gain + 32768) >> 16, 65535).astype(np.uint16)
    if fit.arm == "channel-gain-offset":
        gains, offsets = fit.values[::2], fit.values[1::2]
        return np.clip(np.rint(source * gains + offsets), 0, 65535).astype(np.uint16)
    if fit.arm == "spatial-multiplier":
        height, width = full_shape or source.shape[:2]
        x0, y0 = origin_xy
        x = np.zeros(source.shape[1], dtype=np.float64) if width == 1 else 2.0 * np.arange(x0, x0 + source.shape[1]) / (width - 1) - 1.0
        y = np.zeros(source.shape[0], dtype=np.float64) if height == 1 else 2.0 * np.arange(y0, y0 + source.shape[0]) / (height - 1) - 1.0
        field = fit.values[0] + fit.values[1] * x[None, :] + fit.values[2] * x[None, :] ** 2 + fit.values[3] * y[:, None] + fit.values[4] * y[:, None] ** 2
        return np.clip(np.rint(source * field[..., None]), 0, 65535).astype(np.uint16)
    raise ValueError(f"unsupported photometric arm: {fit.arm}")


def _lighting(variant: str, frame: int, height: int, width: int) -> tuple[np.ndarray, np.ndarray]:
    position = (frame - REFERENCE) / 4.0
    x, y = _coordinates(height, width)
    gain = np.ones((height, width, 3), dtype=np.float64)
    offset = np.zeros_like(gain)
    if variant == "global-gain-offset":
        gain *= 1.0 + .06 * position; offset += 220.0 * position
    elif variant == "channel-gain-offset":
        gain *= 1.0 + np.array((.06, -.04, .03)) * position; offset += np.array((180., -140., 110.)) * position
    elif variant == "static-linear-x": gain *= 1.0 + .025 * x[..., None]
    elif variant == "frame-quadratic": gain *= 1.0 + .04 * position * (x * x + y * y)[..., None]
    elif variant == "cross-xy": gain *= 1.0 + .04 * position * (x * y)[..., None]
    elif variant == "unchanged-focus": pass
    elif variant == "bracket": gain *= 2.0 ** (frame - REFERENCE)
    else: raise ValueError(f"unknown lighting variant: {variant}")
    return gain, offset


def build_case(seed: SeedTriple, variant: str) -> SceneData:
    """Render focus first, then lighting, then the one shared independent noise stream."""
    require_seed(seed)
    if variant not in VARIANTS: raise ValueError(f"unknown lighting variant: {variant}")
    depth, metadata = _layout(seed.layout)
    truth = ss.scene_hard(seed.texture).astype(np.float64)
    _apply_hairs(truth, depth, metadata["hairs"])
    height, width = truth.shape[:2]
    clean_focus = ss.render(truth, depth, SIGMA_STEP, 0, seed.noise, FRAME_COUNT)
    reference_gain, reference_offset = _lighting(variant, REFERENCE, height, width)
    rng = np.random.default_rng(seed.noise)
    observed, targets, clipped, noise, noise_hashes = [], [], [], [], []
    for frame, clean in enumerate(clean_focus):
        gain, offset = _lighting(variant, frame, height, width)
        sample_noise = rng.normal(0.0, 250.0, clean.shape)
        source_lit = np.clip(np.rint(clean.astype(np.float64) * gain + offset), 0, 65535)
        target_lit = np.clip(np.rint(clean.astype(np.float64) * reference_gain + reference_offset), 0, 65535)
        observed.append(np.clip(np.rint(source_lit + sample_noise), 0, 65535).astype(np.uint16))
        targets.append(np.clip(np.rint(target_lit + sample_noise), 0, 65535).astype(np.uint16))
        clipped.append(float(((clean.astype(np.float64) * gain + offset < 0) | (clean.astype(np.float64) * gain + offset > 65535)).mean()))
        noise.append(sample_noise)
        noise_hashes.append(sha256(np.asarray(sample_noise, dtype=np.float64).tobytes()).hexdigest())
    truth_ref = np.clip(np.rint(truth * reference_gain + reference_offset), 0, 65535).astype(np.uint16)
    metadata.update({"family": "thin-hairs", "member": "default", "reference_frame": REFERENCE,
                     "coverage": np.ones((FRAME_COUNT, height, width), dtype=bool), "variant": variant,
                     "reference_lighting_frames": np.stack(targets), "clip_fraction": clipped,
                     "noise_seed": seed.noise, "noise": np.stack(noise), "noise_sha256": noise_hashes,
                     "truth_reference_lighting": truth_ref, "lighting_order": "focus-lighting-noise"})
    return SceneData(np.stack(observed), truth_ref, depth, metadata)


def oracle(frame: np.ndarray, variant: str, frame_index: int) -> np.ndarray:
    height, width = frame.shape[:2]
    gain, offset = _lighting(variant, frame_index, height, width)
    reference_gain, reference_offset = _lighting(variant, REFERENCE, height, width)
    return np.clip(np.rint((np.asarray(frame, dtype=np.float64) - offset) / gain * reference_gain + reference_offset), 0, 65535).astype(np.uint16)


def frame_error(actual: np.ndarray, target: np.ndarray, coverage: np.ndarray) -> dict[str, float | str | None]:
    valid = np.asarray(coverage, dtype=bool)
    if not valid.any():
        return {"frame_rgb_mae": None, "frame_luma_rmse": None, "reason": "empty-common-coverage"}
    delta = np.asarray(actual, dtype=np.float64)[valid] - np.asarray(target, dtype=np.float64)[valid]
    return {"frame_rgb_mae": float(np.abs(delta).mean()), "frame_luma_rmse": float(np.sqrt(np.mean(delta.mean(axis=1) ** 2))), "reason": None}


def whole_tile_equal(frame: np.ndarray, fit: Fit, tile: int) -> bool:
    whole = apply(frame, fit)
    output = np.empty_like(whole)
    height, width = frame.shape[:2]
    for y0 in range(0, height, tile):
        for x0 in range(0, width, tile):
            y1, x1 = min(height, y0 + tile), min(width, x0 + tile)
            output[y0:y1, x0:x1] = apply(frame[y0:y1, x0:x1], fit, (x0, y0), (height, width))
    return np.array_equal(whole, output)


def command_plan() -> dict[str, dict[str, int]]:
    return {"parity": {"rows": 35}, "calibration-off": {"rows": 210, "frame_error_records": 560},
            "calibration-global": {"rows": 210, "frame_error_records": 560, "identity_group_records": 240, "identity_scalar_records": 640},
            "quality": {"rows": 315}, "repeat": {"rows": 315}, "real": {"rows": 12}}


def stage_rows(stage: str) -> list[dict[str, object]]:
    plan = command_plan()
    if stage not in plan: raise ValueError(f"unknown photometric stage {stage}")
    if stage == "parity": return [{"variant": variant, "seed": seed.as_tuple() if hasattr(seed, "as_tuple") else [seed.layout, seed.texture, seed.noise], "method": "laplacian", "tile": 512, "control": "equalize"} for variant in VARIANTS for seed in TRAINING_SEEDS]
    if stage.startswith("calibration"):
        control = "off" if stage == "calibration-off" else "global-gain"
        return [{"variant": variant, "seed": [seed.layout, seed.texture, seed.noise], "method": method, "tile": 512, "repeat": repeat, "control": control} for variant in VARIANTS for seed in TRAINING_SEEDS for method in ("laplacian", "weighted-average", "depth-map") for repeat in (0, 1)]
    if stage in ("quality", "repeat"):
        return [{"variant": variant, "seed": [seed.layout, seed.texture, seed.noise], "method": method, "tile": 512, "arm": arm, "repeat": 0 if stage == "quality" else 1} for variant in VARIANTS for seed in TRAINING_SEEDS for method in ("laplacian", "weighted-average", "depth-map") for arm in ("channel-gain-offset", "spatial-multiplier", "oracle")]
    return [{"arm": arm, "method": method, "tile": 512, "source": "stack", "reference": REFERENCE} for arm in ("off", *FITTED_ARMS) for method in ("laplacian", "weighted-average", "depth-map")]


def identity_observations(seed: SeedTriple, fits: dict[str, list[Fit]], repeat: int) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for arm in FITTED_ARMS:
        for frame, fit in enumerate(fits[arm]):
            group = {"seed": [seed.layout, seed.texture, seed.noise], "repeat": repeat, "arm": arm, "frame": frame, "fit": fit.record()}
            records.append(group)
            if arm == "global-gain":
                scalar = (("gain_deviation", abs(fit.values[0] - 1.0), "dimensionless"),)
            elif arm == "channel-gain-offset":
                scalar = tuple((name, abs(fit.values[index] - baseline), units) for index, (name, baseline, units) in enumerate((("gain_red_deviation", 1., "dimensionless"), ("offset_red_deviation", 0., "encoded code counts"), ("gain_green_deviation", 1., "dimensionless"), ("offset_green_deviation", 0., "encoded code counts"), ("gain_blue_deviation", 1., "dimensionless"), ("offset_blue_deviation", 0., "encoded code counts"))))
            else:
                minimum, maximum = _spatial_extrema(fit.values); scalar = (("field_max_deviation", max(abs(minimum - 1.), abs(maximum - 1.)), "dimensionless"),)
            group["scalars"] = [{"parameter": name, "value": float(value), "units": units} for name, value, units in scalar]
    return records


def fused_fields(values: dict[str, float | None], reasons: dict[str, str]) -> dict[str, object]:
    """Keep the eight applicable fused values finite and every other registered field explicitly unavailable."""
    if set(values) != set(REQUIRED_FUSED) or set(reasons) != set(NULL_FUSED):
        raise ValueError("photometric fused metric fields do not match the fixed applicability registry")
    if any(value is None or not np.isfinite(value) for value in values.values()) or any(not isinstance(reason, str) or not reason for reason in reasons.values()):
        raise ValueError("photometric fused fields contain nonfinite values or missing null reasons")
    return {"metrics": {name: float(values[name]) for name in REQUIRED_FUSED}, "null_metrics": {name: {"value": None, "reason": reasons[name]} for name in NULL_FUSED}}


def reduce_allowances(rows: list[dict[str, object]], key: tuple[str, ...]) -> list[dict[str, object]]:
    """Reduce only complete two-repeat calibration observations without zero floors or transferred keys."""
    groups: dict[tuple[object, ...], list[dict[str, object]]] = {}
    for row in rows:
        if not all(name in row for name in (*key, "seed", "repeat", "value")) or not np.isfinite(row["value"]):
            raise ValueError("photometric allowance row is incomplete or nonfinite")
        groups.setdefault(tuple(row[name] for name in key), []).append(row)
    reduced = []
    for identity, members in sorted(groups.items()):
        by_seed: dict[tuple[object, ...], dict[int, float]] = {}
        for row in members:
            seed, repeat = tuple(row["seed"]), row["repeat"]
            if repeat not in (0, 1) or repeat in by_seed.setdefault(seed, {}): raise ValueError("photometric allowance has duplicate or invalid repeat")
            by_seed[seed][repeat] = float(row["value"])
        expected_seeds = {(seed.layout, seed.texture, seed.noise) for seed in TRAINING_SEEDS}
        if set(by_seed) != expected_seeds or any(set(values) != {0, 1} for values in by_seed.values()):
            raise ValueError("photometric allowance lacks five seeds or two repeats")
        seed_spread = max(values[0] for values in by_seed.values()) - min(values[0] for values in by_seed.values())
        repeat_spread = max(abs(values[1]-values[0]) for values in by_seed.values())
        reduced.append({**dict(zip(key, identity)), "seed_spread": seed_spread, "repeat_spread": repeat_spread, "allowance": max(seed_spread, repeat_spread)})
    return reduced


def _quality_problem(reason: str) -> dict[str, object]:
    candidates = {arm: {"verdict": "DEFER", "checks": [{"outcome": "defer", "reason": reason}]} for arm in FITTED_ARMS[1:]}
    return {"state": "DEFER", "reason": reason, "candidates": candidates,
            "oracle": {"advancement": False, "reason": "diagnostic only"}}


def _quality_number(value: object) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not np.isfinite(value):
        raise ValueError("quality value is missing or nonfinite")
    return float(value)


def _quality_sha(value: object) -> str:
    if not isinstance(value, str) or len(value) != 64 or any(char not in "0123456789abcdef" for char in value):
        raise ValueError("quality hash binding is invalid")
    return value


def _quality_seed(value: object) -> tuple[int, int, int]:
    if not isinstance(value, list) or len(value) != 3 or any(isinstance(item, bool) or not isinstance(item, int) for item in value):
        raise ValueError("quality seed is invalid")
    return tuple(value)


def _quality_allowance(value: object) -> float:
    allowance = _quality_number(value)
    if allowance < 0:
        raise ValueError("quality allowance is negative")
    return allowance


def _quality_calibration(document: object, control: str, registration_sha256: str) -> tuple[dict[tuple[object, ...], float], dict[tuple[object, ...], float], dict[tuple[object, ...], float], dict[tuple[object, ...], float]]:
    if not isinstance(document, dict) or document.get("registration_sha256") != registration_sha256 or document.get("control") != control:
        raise ValueError("quality calibration binding is invalid")
    expected_seeds = {(seed.layout, seed.texture, seed.noise) for seed in TRAINING_SEEDS}
    expected_fused = {(variant, seed, method, metric, repeat) for variant in VARIANTS for seed in expected_seeds for method in ("laplacian", "weighted-average", "depth-map") for metric in REQUIRED_FUSED for repeat in (0, 1)}
    expected_fused_allowances = {(variant, method, metric) for variant in VARIANTS for method in ("laplacian", "weighted-average", "depth-map") for metric in REQUIRED_FUSED}
    expected_frames = {(variant, seed, frame, repeat) for variant in VARIANTS for seed in expected_seeds for frame in range(FRAME_COUNT) for repeat in (0, 1)}
    expected_frame_allowances = {(variant, frame, metric) for variant in VARIANTS for frame in range(FRAME_COUNT) for metric in ("frame_rgb_mae", "frame_luma_rmse")}
    fused: dict[tuple[object, ...], float] = {}
    for row in document.get("fused", []):
        if not isinstance(row, dict) or row.get("control") != control or row.get("tile") != 512:
            raise ValueError("quality calibration fused row is invalid")
        key = (row.get("variant"), _quality_seed(row.get("seed")), row.get("method"), row.get("metric"), row.get("repeat"))
        if key in fused or key not in expected_fused:
            raise ValueError("quality calibration fused keys are incomplete or duplicate")
        fused[key] = _quality_number(row.get("value"))
    if set(fused) != expected_fused:
        raise ValueError("quality calibration fused keys are incomplete or duplicate")
    fused_allowances: dict[tuple[object, ...], float] = {}
    for row in document.get("fused_allowances", []):
        if not isinstance(row, dict) or row.get("control") != control or row.get("tile") != 512:
            raise ValueError("quality calibration fused allowance is invalid")
        key = (row.get("variant"), row.get("method"), row.get("metric"))
        if key in fused_allowances or key not in expected_fused_allowances:
            raise ValueError("quality calibration fused allowance keys are incomplete or duplicate")
        fused_allowances[key] = _quality_allowance(row.get("allowance"))
    if set(fused_allowances) != expected_fused_allowances:
        raise ValueError("quality calibration fused allowance keys are incomplete or duplicate")
    frames: dict[tuple[object, ...], float] = {}
    for row in document.get("frame_errors", []):
        if not isinstance(row, dict) or row.get("control") != control or row.get("units") != "encoded code counts" or row.get("direction") != "lower":
            raise ValueError("quality calibration frame row is invalid")
        base = (row.get("variant"), _quality_seed(row.get("seed")), row.get("frame"), row.get("repeat"))
        if base not in expected_frames:
            raise ValueError("quality calibration frame keys are incomplete or duplicate")
        for metric in ("frame_rgb_mae", "frame_luma_rmse"):
            key = (*base, metric)
            if key in frames:
                raise ValueError("quality calibration frame keys are incomplete or duplicate")
            frames[key] = _quality_number(row.get(metric))
    if set((key[0], key[1], key[2], key[3]) for key in frames) != expected_frames or len(frames) != 2 * len(expected_frames):
        raise ValueError("quality calibration frame keys are incomplete or duplicate")
    frame_allowances: dict[tuple[object, ...], float] = {}
    for row in document.get("frame_allowances", []):
        if not isinstance(row, dict) or row.get("control") != control or row.get("units") != "encoded code counts" or row.get("direction") != "lower":
            raise ValueError("quality calibration frame allowance is invalid")
        key = (row.get("variant"), row.get("frame"), row.get("metric"))
        if key in frame_allowances or key not in expected_frame_allowances:
            raise ValueError("quality calibration frame allowance keys are incomplete or duplicate")
        frame_allowances[key] = _quality_allowance(row.get("allowance"))
    if set(frame_allowances) != expected_frame_allowances:
        raise ValueError("quality calibration frame allowance keys are incomplete or duplicate")
    return fused, fused_allowances, frames, frame_allowances


def _quality_identity(document: object) -> dict[tuple[object, ...], float]:
    if not isinstance(document, dict):
        raise ValueError("quality identity allowances are missing")
    expected = {(arm, frame, parameter, units) for arm, parameters in IDENTITY_PARAMETERS.items() for frame in range(FRAME_COUNT) for parameter, units in parameters}
    allowances: dict[tuple[object, ...], float] = {}
    for row in document.get("identity_allowances", []):
        if not isinstance(row, dict) or row.get("direction") != "lower":
            raise ValueError("quality identity allowance is invalid")
        key = (row.get("arm"), row.get("frame"), row.get("parameter"), row.get("units"))
        if key in allowances or key not in expected:
            raise ValueError("quality identity allowance keys are incomplete or duplicate")
        allowances[key] = _quality_allowance(row.get("allowance"))
    if set(allowances) != expected:
        raise ValueError("quality identity allowance keys are incomplete or duplicate")
    return allowances


def _quality_fits(row: dict[str, object], arm: str) -> list[dict[str, object]]:
    fits = row.get("fits")
    if not isinstance(fits, list) or len(fits) != FRAME_COUNT:
        raise ValueError("quality fit records are incomplete")
    width = 6 if arm == "channel-gain-offset" else 5
    checked = []
    for fit in fits:
        if not isinstance(fit, dict) or not isinstance(fit.get("applied"), list) or len(fit["applied"]) != width or not isinstance(fit.get("clamped"), bool):
            raise ValueError("quality fit record is invalid")
        checked.append({"applied": [_quality_number(value) for value in fit["applied"]], "clamped": fit["clamped"]})
    return checked


def _quality_identity_values(arm: str, fit: dict[str, object]) -> dict[str, float]:
    applied = fit["applied"]
    if arm == "global-gain":
        return {"gain_deviation": abs(applied[0] - 1.0)}
    if arm == "channel-gain-offset":
        return {"gain_red_deviation": abs(applied[0] - 1.0), "offset_red_deviation": abs(applied[1]),
                "gain_green_deviation": abs(applied[2] - 1.0), "offset_green_deviation": abs(applied[3]),
                "gain_blue_deviation": abs(applied[4] - 1.0), "offset_blue_deviation": abs(applied[5])}
    low, high = _spatial_extrema(np.asarray(applied, dtype=np.float64))
    return {"field_max_deviation": max(abs(low - 1.0), abs(high - 1.0))}


def _quality_check(checks: list[dict[str, object]], outcome: bool, reason: str, key: dict[str, object], measured: object, threshold: object) -> bool:
    checks.append({"outcome": "pass" if outcome else "fail", "reason": reason, "key": key, "measured": measured, "threshold": threshold})
    return outcome


def quality_verdict(rows: list[dict[str, object]], off_calibration: dict[str, object], global_calibration: dict[str, object], registration_sha256: str) -> dict[str, object]:
    """Compare complete candidate rows with their sealed matched controls."""
    try:
        registration_sha256 = _quality_sha(registration_sha256)
        global_fused, global_fused_allowances, global_frames, global_frame_allowances = _quality_calibration(global_calibration, "global-gain", registration_sha256)
        off_fused, off_fused_allowances, _, _ = _quality_calibration(off_calibration, "off", registration_sha256)
        identity_allowances = _quality_identity(global_calibration)
        expected_seeds = {(seed.layout, seed.texture, seed.noise) for seed in TRAINING_SEEDS}
        expected = {(variant, seed, method, arm) for variant in VARIANTS for seed in expected_seeds for method in ("laplacian", "weighted-average", "depth-map") for arm in ("channel-gain-offset", "spatial-multiplier", "oracle")}
        indexed: dict[tuple[object, ...], dict[str, object]] = {}
        bindings: dict[tuple[object, ...], tuple[str, str]] = {}
        for row in rows:
            if not isinstance(row, dict) or row.get("registration_sha256") != registration_sha256 or row.get("tile") != 512 or row.get("repeat") != 0:
                raise ValueError("quality row binding is invalid")
            key = (row.get("variant"), _quality_seed(row.get("seed")), row.get("method"), row.get("arm"))
            if key in indexed or key not in expected:
                raise ValueError("quality row keys are incomplete or duplicate")
            source, noise = _quality_sha(row.get("source_case_sha256")), _quality_sha(row.get("noise_array_sha256"))
            case_key = key[:2]
            if case_key in bindings and bindings[case_key] != (source, noise):
                raise ValueError("quality source or noise binding differs across matched arms")
            bindings[case_key] = (source, noise)
            fused = row.get("fused")
            if not isinstance(fused, dict) or not isinstance(fused.get("metrics"), dict) or set(fused["metrics"]) != set(REQUIRED_FUSED):
                raise ValueError("quality fused metrics are incomplete")
            for value in fused["metrics"].values(): _quality_number(value)
            errors = row.get("frame_errors")
            if not isinstance(errors, list) or len(errors) != FRAME_COUNT:
                raise ValueError("quality frame errors are incomplete")
            for error in errors:
                if not isinstance(error, dict): raise ValueError("quality frame error is invalid")
                for metric in ("frame_rgb_mae", "frame_luma_rmse"): _quality_number(error.get(metric))
            if key[3] != "oracle": row = {**row, "fits": _quality_fits(row, str(key[3]))}
            indexed[key] = row
        if set(indexed) != expected:
            raise ValueError("quality row keys are incomplete or duplicate")
    except (TypeError, ValueError, KeyError) as error:
        return _quality_problem(str(error))

    candidates: dict[str, dict[str, object]] = {}
    for arm, target in (("channel-gain-offset", "channel-gain-offset"), ("spatial-multiplier", "frame-quadratic")):
        checks: list[dict[str, object]] = []
        for seed in expected_seeds:
            for frame in range(FRAME_COUNT):
                if frame == REFERENCE: continue
                row = indexed[(target, seed, "laplacian", arm)]
                for metric in ("frame_rgb_mae", "frame_luma_rmse"):
                    candidate = _quality_number(row["frame_errors"][frame][metric])
                    baseline = global_frames[(target, seed, frame, 0, metric)]
                    allowance = global_frame_allowances[(target, frame, metric)]
                    _quality_check(checks, baseline - candidate > 2.0 * allowance, "benefit is not strictly greater than two allowances",
                                   {"variant": target, "seed": list(seed), "frame": frame, "metric": metric},
                                   {"baseline": baseline, "candidate": candidate, "improvement": baseline - candidate}, {"strictly_greater_than": 2.0 * allowance, "allowance": allowance})
            for method in ("laplacian", "weighted-average", "depth-map"):
                row = indexed[(target, seed, method, arm)]
                candidate = _quality_number(row["fused"]["metrics"]["psnr"])
                baseline = global_fused[(target, seed, method, "psnr", 0)]
                allowance = global_fused_allowances[(target, method, "psnr")]
                _quality_check(checks, candidate - baseline > 2.0 * allowance, "benefit is not strictly greater than two allowances",
                               {"variant": target, "seed": list(seed), "method": method, "metric": "psnr"},
                               {"baseline": baseline, "candidate": candidate, "improvement": candidate - baseline}, {"strictly_greater_than": 2.0 * allowance, "allowance": allowance})
        for variant in VARIANTS:
            if variant == "bracket": continue
            for seed in expected_seeds:
                row = indexed[(variant, seed, "laplacian", arm)]
                for frame in range(FRAME_COUNT):
                    for metric in ("frame_rgb_mae", "frame_luma_rmse"):
                        candidate = _quality_number(row["frame_errors"][frame][metric])
                        baseline = global_frames[(variant, seed, frame, 0, metric)]
                        allowance = global_frame_allowances[(variant, frame, metric)]
                        _quality_check(checks, candidate <= baseline + allowance, "frame error regresses by more than one allowance",
                                       {"variant": variant, "seed": list(seed), "frame": frame, "metric": metric},
                                       {"baseline": baseline, "candidate": candidate},
                                       {"at_most": baseline + allowance, "allowance": allowance})
                for method in ("laplacian", "weighted-average", "depth-map"):
                    row = indexed[(variant, seed, method, arm)]
                    for metric in REQUIRED_FUSED:
                        candidate = _quality_number(row["fused"]["metrics"][metric])
                        baseline = global_fused[(variant, seed, method, metric, 0)]
                        allowance = global_fused_allowances[(variant, method, metric)]
                        pass_veto = candidate >= baseline - allowance if metric in HIGHER_FUSED else candidate <= baseline + allowance
                        _quality_check(checks, pass_veto, "objective metric regresses by more than one allowance",
                                       {"variant": variant, "seed": list(seed), "method": method, "metric": metric},
                                       {"baseline": baseline, "candidate": candidate}, {"limit": baseline - allowance if metric in HIGHER_FUSED else baseline + allowance, "allowance": allowance, "direction": "higher" if metric in HIGHER_FUSED else "lower"})
        for seed in expected_seeds:
            for method in ("laplacian", "weighted-average", "depth-map"):
                row = indexed[("unchanged-focus", seed, method, arm)]
                for metric in REQUIRED_FUSED:
                    candidate = _quality_number(row["fused"]["metrics"][metric])
                    baseline = off_fused[("unchanged-focus", seed, method, metric, 0)]
                    allowance = off_fused_allowances[("unchanged-focus", method, metric)]
                    _quality_check(checks, abs(candidate - baseline) <= allowance, "focus fused change exceeds control allowance",
                                   {"variant": "unchanged-focus", "seed": list(seed), "method": method, "metric": metric},
                                   {"absolute_change": abs(candidate - baseline), "baseline": baseline, "candidate": candidate}, {"at_most": allowance, "allowance": allowance})
            fits = indexed[("unchanged-focus", seed, "laplacian", arm)]["fits"]
            for frame, fit in enumerate(fits):
                values = _quality_identity_values(arm, fit)
                for parameter, units in IDENTITY_PARAMETERS[arm]:
                    allowance = identity_allowances[(arm, frame, parameter, units)]
                    _quality_check(checks, values[parameter] <= allowance, "focus parameter exceeds identity allowance",
                                   {"seed": list(seed), "frame": frame, "parameter": parameter, "units": units}, values[parameter], {"at_most": allowance, "allowance": allowance})
        for seed in expected_seeds:
            for method in ("laplacian", "weighted-average", "depth-map"):
                fits = indexed[("bracket", seed, method, arm)]["fits"]
                for frame, fit in enumerate(fits):
                    if frame == REFERENCE: continue
                    maximum = np.log2(1.25) / abs(frame - REFERENCE)
                    if arm == "channel-gain-offset":
                        ratios = [(channel, endpoint, (fit["applied"][2 * channel] * endpoint + fit["applied"][2 * channel + 1]) / endpoint) for channel in range(3) for endpoint in (.05 * 65535, .95 * 65535)]
                    else:
                        low, high = _spatial_extrema(np.asarray(fit["applied"], dtype=np.float64)); ratios = [("field", "minimum", low), ("field", "maximum", high)]
                    for component, point, ratio in ratios:
                        removed = abs(np.log2(ratio)) / abs(frame - REFERENCE) if ratio > 0 else float("inf")
                        _quality_check(checks, bool(fit["clamped"]), "bracket member lacks required clamp flag",
                                       {"seed": list(seed), "method": method, "frame": frame, "component": component, "point": point}, fit["clamped"], {"required": True})
                        _quality_check(checks, removed <= maximum, "bracket removed EV share exceeds bound",
                                       {"seed": list(seed), "method": method, "frame": frame, "component": component, "point": point}, {"ratio": ratio, "removed_share": removed}, {"at_most": maximum})
        candidates[arm] = {"verdict": "NO_GO" if any(check["outcome"] == "fail" for check in checks) else "GO", "checks": checks}
    return {"state": "COMPLETE", "registration_sha256": registration_sha256, "candidates": candidates,
            "oracle": {"advancement": False, "reason": "diagnostic only", "rows": sum(1 for key in indexed if key[3] == "oracle")}}


def source_identity(paths: list[Path]) -> dict[str, str]:
    return {str(path.resolve()): sha256(path.read_bytes()).hexdigest() for path in paths}


def parity_global(frames: np.ndarray, coverage: np.ndarray, report: object) -> dict[str, object]:
    """Compare one native equalization report with the same global-Q16 inputs."""
    if not isinstance(report, dict) or report.get("reference") != REFERENCE or not isinstance(report.get("frames"), list) or len(report["frames"]) != FRAME_COUNT:
        raise ValueError("photometric native parity report has an invalid reference or frame count")
    rows = []
    for index, native in enumerate(report["frames"]):
        fit = estimate("global-gain", frames, coverage, index)
        if not isinstance(native, dict) or any(native.get(name) != value for name, value in (("samples", fit.sample_count), ("thin", fit.thin), ("clamped", fit.clamped))):
            raise ValueError("photometric native parity count or flag differs")
        applied = native.get("applied")
        estimated = native.get("estimated")
        if not isinstance(applied, (int, float)) or not isinstance(estimated, (int, float)) or not np.isfinite((applied, estimated)).all():
            raise ValueError("photometric native parity gain is nonfinite")
        if applied != fit.values[0] or estimated != fit.estimated[0]:
            raise ValueError("photometric native parity gain differs")
        q16 = int(np.floor(fit.values[0] * 65536.0 + .5))
        rows.append({"frame": index, "samples": fit.sample_count, "thin": fit.thin, "clamped": fit.clamped,
                     "estimated": float(fit.estimated[0]), "applied": float(fit.values[0]), "q16": q16,
                     "raw_gain_discrepancy": float(estimated - fit.estimated[0]),
                     "applied_gain_discrepancy": float(applied - fit.values[0])})
    return {"reference": REFERENCE, "frames": rows,
            "max_raw_gain_discrepancy": max(abs(row["raw_gain_discrepancy"]) for row in rows),
            "max_applied_gain_discrepancy": max(abs(row["applied_gain_discrepancy"]) for row in rows)}


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result: raise ValueError("photometric JSON contains a duplicate key")
        result[key] = value
    return result


def registration_document(path: Path) -> tuple[dict[str, object], str]:
    if not path.is_file(): raise ValueError("photometric registration is missing")
    raw = path.read_bytes(); document = json.loads(raw, object_pairs_hook=_unique_object)
    if not isinstance(document, dict) or document.get("schema_version") != 1 or document.get("reference_frame") != REFERENCE:
        raise ValueError("photometric registration schema or reference is invalid")
    expected_rows = {"source_constructed_cli": 4, "training_parity_cli": 35,
                     "calibration_off": 210, "calibration_global": 210,
                     "quality": 315, "repeat": 315, "real": 12}
    if tuple(document.get("variants", ())) != VARIANTS or document.get("row_counts") != expected_rows:
        raise ValueError("photometric registration matrix differs from fixed contract")
    original = document.get("original_inputs")
    if not isinstance(original, dict) or set(original) != {"protocol", "baseline", "allowances", "preparation"}:
        raise ValueError("photometric original input bindings are incomplete")
    for binding in original.values():
        if not isinstance(binding, dict) or not Path(str(binding.get("path", ""))).is_file() or sha256(Path(str(binding["path"])).read_bytes()).hexdigest() != binding.get("sha256"):
            raise ValueError("photometric original input is missing or stale")
    return document, sha256(raw).hexdigest()


def _json_document(path: Path, message: str) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(), object_pairs_hook=_unique_object)
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(message) from error
    if not isinstance(value, dict):
        raise ValueError(message)
    return value


def _binding(binding: object, message: str) -> tuple[Path, str]:
    if not isinstance(binding, dict) or set(binding) != {"path", "sha256"}:
        raise ValueError(message)
    path, digest = binding.get("path"), binding.get("sha256")
    if not isinstance(path, str) or not isinstance(digest, str) or len(digest) != 64:
        raise ValueError(message)
    target = Path(path)
    if not target.is_file() or sha256(target.read_bytes()).hexdigest() != digest:
        raise ValueError(message)
    return target, digest


def _source_hashes(registration: dict[str, object]) -> dict[str, str]:
    frozen = registration.get("source_before_implementation")
    if not isinstance(frozen, dict) or len(frozen) != 13 or any(not isinstance(name, str) or not isinstance(digest, str) for name, digest in frozen.items()):
        raise ValueError("photometric registration source dependencies are invalid")
    root = Path.cwd()
    changed = {"spikes/stacking_experiments/runner.py", "spikes/stacking_experiments/__main__.py", "spikes/stacking_experiments/README.md"}
    result: dict[str, str] = {}
    for relative, frozen_digest in frozen.items():
        source = root / relative
        if not source.is_file():
            raise ValueError("photometric source dependency is missing")
        digest = sha256(source.read_bytes()).hexdigest()
        if relative not in changed and digest != frozen_digest:
            raise ValueError("photometric frozen source dependency is stale")
        result[str(source.resolve())] = digest
    implementation = root / "spikes/stacking_experiments/photometric.py"
    if not implementation.is_file():
        raise ValueError("photometric implementation source is missing")
    result[str(implementation.resolve())] = sha256(implementation.read_bytes()).hexdigest()
    return result


def _source_envelope_proof(registration: dict[str, object], digest: str, bindings: dict[str, Path]) -> None:
    expected_source = _source_hashes(registration)
    prior = _json_document(bindings["prior"], "photometric source prior is invalid")
    if (set(prior) != {"schema", "registration_sha256", "original_inputs", "source_before_implementation"}
            or prior.get("schema") != "photometric-source-prior-v1" or prior.get("registration_sha256") != digest
            or prior.get("original_inputs") != registration.get("original_inputs")
            or prior.get("source_before_implementation") != registration.get("source_before_implementation")):
        raise ValueError("photometric source prior does not bind the registration")
    readiness = _json_document(bindings["readiness"], "photometric source readiness is invalid")
    prior_binding = {"path": str(bindings["prior"]), "sha256": sha256(bindings["prior"].read_bytes()).hexdigest()}
    if (set(readiness) != {"schema", "state", "registration_sha256", "source_hashes", "prior"}
            or readiness.get("schema") != "photometric-source-readiness-v1" or readiness.get("state") != "source-ready"
            or readiness.get("registration_sha256") != digest or readiness.get("source_hashes") != expected_source
            or readiness.get("prior") != prior_binding):
        raise ValueError("photometric source readiness does not bind current sources")
    audit = _json_document(bindings["audit_seal"], "photometric source audit proof is invalid")
    readiness_binding = {"path": str(bindings["readiness"]), "sha256": sha256(bindings["readiness"].read_bytes()).hexdigest()}
    if (set(audit) != {"schema", "result", "registration_sha256", "source_hashes", "prior", "readiness"}
            or audit.get("schema") != "photometric-source-audit-v1" or audit.get("result") != "PASS"
            or audit.get("registration_sha256") != digest or audit.get("source_hashes") != expected_source
            or audit.get("prior") != prior_binding or audit.get("readiness") != readiness_binding):
        raise ValueError("photometric source audit proof does not bind current sources")


def _source_envelope(path: Path) -> tuple[dict[str, object], str]:
    envelope = _json_document(path, "photometric source envelope is invalid")
    if envelope.get("schema") != "photometric-stage-ready-v1" or envelope.get("state") != "source-ready":
        raise ValueError("photometric source envelope is invalid")
    paths = {name: _binding(envelope.get(name), "photometric source envelope binding is invalid")[0]
             for name in ("registration", "readiness", "prior", "audit_seal")}
    registration, digest = registration_document(paths["registration"])
    _source_envelope_proof(registration, digest, paths)
    return registration, digest


def _calibration_binding(binding: object, control: str, digest: str) -> tuple[Path, dict[str, object]]:
    path, _ = _binding(binding, "photometric allowance calibration binding is invalid")
    document = _json_document(path, "photometric allowance calibration is invalid")
    _quality_calibration(document, control, digest)
    if control == "global-gain":
        _quality_identity(document)
    return path, document


def _allowance_envelope_proof(registration: dict[str, object], digest: str, bindings: dict[str, Path]) -> None:
    source_path = bindings["prior"]
    source_registration, source_digest = _source_envelope(source_path)
    if source_digest != digest or source_registration != registration:
        raise ValueError("photometric allowance source binding is stale")
    readiness = _json_document(bindings["readiness"], "photometric allowance readiness is invalid")
    source_binding = {"path": str(source_path), "sha256": sha256(source_path.read_bytes()).hexdigest()}
    if (set(readiness) != {"schema", "state", "registration_sha256", "source_ready", "calibration_off", "calibration_global"}
            or readiness.get("schema") != "photometric-allowance-readiness-v1" or readiness.get("state") != "allowance-ready"
            or readiness.get("registration_sha256") != digest or readiness.get("source_ready") != source_binding):
        raise ValueError("photometric allowance readiness does not bind source evidence")
    off_path, _ = _calibration_binding(readiness.get("calibration_off"), "off", digest)
    global_path, _ = _calibration_binding(readiness.get("calibration_global"), "global-gain", digest)
    audit = _json_document(bindings["audit_seal"], "photometric allowance audit proof is invalid")
    readiness_binding = {"path": str(bindings["readiness"]), "sha256": sha256(bindings["readiness"].read_bytes()).hexdigest()}
    expected = {"schema", "result", "registration_sha256", "source_ready", "readiness", "calibration_off", "calibration_global"}
    if (set(audit) != expected or audit.get("schema") != "photometric-allowance-audit-v1" or audit.get("result") != "PASS"
            or audit.get("registration_sha256") != digest or audit.get("source_ready") != source_binding
            or audit.get("readiness") != readiness_binding
            or audit.get("calibration_off") != {"path": str(off_path), "sha256": sha256(off_path.read_bytes()).hexdigest()}
            or audit.get("calibration_global") != {"path": str(global_path), "sha256": sha256(global_path.read_bytes()).hexdigest()}):
        raise ValueError("photometric allowance audit proof does not bind calibration")


def stage_envelope(path: Path, stage: str) -> tuple[dict[str, object], str, dict[str, object]]:
    """Open one immutable admitted-stage envelope and its prospective registration."""
    raw = path.read_bytes()
    envelope = json.loads(raw, object_pairs_hook=_unique_object)
    states = {"parity": "source-ready", "calibration-off": "source-ready", "calibration-global": "source-ready",
              "quality": "allowance-ready", "repeat": "allowance-ready", "real": "allowance-ready"}
    if not isinstance(envelope, dict) or envelope.get("schema") != "photometric-stage-ready-v1" or envelope.get("state") != states[stage]:
        raise ValueError("photometric stage envelope does not admit this stage")
    bindings = []
    for name in ("registration", "readiness", "prior", "audit_seal"):
        binding = envelope.get(name)
        if not isinstance(binding, dict) or not isinstance(binding.get("path"), str) or not isinstance(binding.get("sha256"), str):
            raise ValueError("photometric stage envelope binding is invalid")
        target = Path(binding["path"])
        if not target.is_file() or sha256(target.read_bytes()).hexdigest() != binding["sha256"]:
            raise ValueError("photometric stage envelope binding is stale")
        bindings.append((name, target))
    registration, digest = registration_document(dict(bindings)["registration"])
    binding_paths = dict(bindings)
    if envelope.get("state") == "source-ready":
        _source_envelope_proof(registration, digest, binding_paths)
    if envelope.get("state") == "allowance-ready":
        _allowance_envelope_proof(registration, digest, binding_paths)
    if stage.startswith("calibration"):
        sidecar_path = path.parent / "parity-ready.json"
        if not sidecar_path.is_file():
            raise ValueError("photometric parity-ready sidecar is missing")
        sidecar = json.loads(sidecar_path.read_text(), object_pairs_hook=_unique_object)
        if not isinstance(sidecar, dict) or set(sidecar) != {"schema", "registration_sha256", "source_ready", "readiness", "parity"} or sidecar.get("schema") != "photometric-parity-ready-v1" or sidecar.get("registration_sha256") != digest:
            raise ValueError("photometric parity-ready sidecar is invalid")
        source_ready = sidecar["source_ready"]
        if not isinstance(source_ready, dict) or set(source_ready) != {"path", "sha256"} or not isinstance(source_ready.get("path"), str) or not isinstance(source_ready.get("sha256"), str) or Path(source_ready["path"]).resolve() != path.resolve() or source_ready["sha256"] != sha256(raw).hexdigest():
            raise ValueError("photometric parity-ready source binding is stale")
        readiness = sidecar["readiness"]
        if not isinstance(readiness, dict) or set(readiness) != {"path", "sha256"} or not isinstance(readiness.get("path"), str) or not isinstance(readiness.get("sha256"), str):
            raise ValueError("photometric parity-ready readiness binding is invalid")
        readiness_path = Path(readiness["path"])
        if not readiness_path.is_file() or sha256(readiness_path.read_bytes()).hexdigest() != readiness["sha256"]:
            raise ValueError("photometric parity-ready readiness binding is stale")
        parity = sidecar["parity"]
        if not isinstance(parity, dict) or set(parity) != {"path", "sha256", "audit_seal", "audit_seal_sha256"} or any(not isinstance(parity.get(name), str) for name in parity):
            raise ValueError("photometric parity proof is invalid")
        artifact, seal = Path(parity["path"]), Path(parity["audit_seal"])
        if (not artifact.is_file() or not seal.is_file() or sha256(artifact.read_bytes()).hexdigest() != parity["sha256"]
                or sha256(seal.read_bytes()).hexdigest() != parity["audit_seal_sha256"]):
            raise ValueError("photometric parity proof is stale")
        parity_document = json.loads(artifact.read_text(), object_pairs_hook=_unique_object)
        rows = parity_document.get("rows") if isinstance(parity_document, dict) else None
        expected_keys = {(variant, (seed.layout, seed.texture, seed.noise), "laplacian", 512) for variant in VARIANTS for seed in TRAINING_SEEDS}
        keys = {(row.get("variant"), tuple(row.get("seed", ())), row.get("method"), row.get("tile")) for row in rows or [] if isinstance(row, dict)}
        if (not isinstance(parity_document, dict) or parity_document.get("stage") != "parity" or parity_document.get("registration_sha256") != digest
                or not isinstance(rows, list) or len(rows) != 35 or len(keys) != 35 or keys != expected_keys
                or any(not isinstance(row, dict) or not isinstance(row.get("native"), dict) or row["native"].get("native_exit") != 0
                       or not _parity_details(row.get("parity")) for row in rows)):
            raise ValueError("photometric parity proof lacks exact passing rows")
        audit = json.loads(seal.read_text(), object_pairs_hook=_unique_object)
        audit_parity = audit.get("parity") if isinstance(audit, dict) else None
        if (not isinstance(audit, dict) or set(audit) != {"schema", "registration_sha256", "result", "parity"}
                or audit.get("schema") != "photometric-parity-audit-v1" or audit.get("registration_sha256") != digest
                or audit.get("result") != "PASS" or not isinstance(audit_parity, dict) or set(audit_parity) != {"path", "sha256"}
                or audit_parity.get("path") != parity["path"] or audit_parity.get("sha256") != parity["sha256"]):
            raise ValueError("photometric parity audit is invalid")
        return registration, digest, sidecar
    return registration, digest, envelope


def _parity_details(value: object) -> bool:
    if not isinstance(value, dict) or set(value) != {"reference", "frames", "max_raw_gain_discrepancy", "max_applied_gain_discrepancy"} or value.get("reference") != REFERENCE:
        return False
    frames = value.get("frames")
    if not isinstance(frames, list) or len(frames) != FRAME_COUNT:
        return False
    expected = {"frame", "samples", "thin", "clamped", "estimated", "applied", "q16", "raw_gain_discrepancy", "applied_gain_discrepancy"}
    if any(not isinstance(row, dict) or set(row) != expected or isinstance(row.get("frame"), bool) or not isinstance(row.get("frame"), int) or row["frame"] != index
           or isinstance(row.get("samples"), bool) or not isinstance(row.get("samples"), int) or not 0 <= row["samples"] <= 262144
           or not isinstance(row.get("thin"), bool) or not isinstance(row.get("clamped"), bool)
           or isinstance(row.get("q16"), bool) or not isinstance(row.get("q16"), int)
           or any(isinstance(row.get(name), bool) or not isinstance(row.get(name), (int, float)) or not np.isfinite(row[name])
                  for name in ("estimated", "applied", "raw_gain_discrepancy", "applied_gain_discrepancy"))
           or row["estimated"] <= 0.0
           or row["thin"] != (row["samples"] < 1000)
           or ((row["thin"] or index == REFERENCE) and (row["estimated"] != 1.0 or row["applied"] != 1.0 or row["clamped"]))
           or (not (row["thin"] or index == REFERENCE) and (row["applied"] != float(np.clip(row["estimated"], LOW, HIGH)) or row["clamped"] != (row["applied"] != row["estimated"])))
           or row["q16"] != int(np.floor(row["applied"] * 65536.0 + .5))
           or row["raw_gain_discrepancy"] != 0.0 or row["applied_gain_discrepancy"] != 0.0
           or ((row["thin"] or index == REFERENCE) and row["q16"] != 65536)
           for index, row in enumerate(frames)):
        return False
    raw = max(abs(row["raw_gain_discrepancy"]) for row in frames)
    applied = max(abs(row["applied_gain_discrepancy"]) for row in frames)
    return (not isinstance(value.get("max_raw_gain_discrepancy"), bool) and isinstance(value.get("max_raw_gain_discrepancy"), (int, float))
            and np.isfinite(value["max_raw_gain_discrepancy"]) and value["max_raw_gain_discrepancy"] == raw
            and not isinstance(value.get("max_applied_gain_discrepancy"), bool) and isinstance(value.get("max_applied_gain_discrepancy"), (int, float))
            and np.isfinite(value["max_applied_gain_discrepancy"]) and value["max_applied_gain_discrepancy"] == applied)


def _receipt_command(argv: list[str]) -> bool:
    command = list(argv)
    while command:
        head = Path(command[0]).name
        if head == "with-lock.sh" and len(command) >= 3 and command[1:3] == ["build", "--"]:
            command = command[3:]
        elif head == "time":
            command = command[1:]
            while command and command[0].startswith("-"):
                option = command.pop(0)
                if option in ("-o", "--output") and command:
                    command.pop(0)
        elif head == "timeout":
            command = command[1:]
            while command and (command[0].startswith("-") or command[0].endswith("s")):
                command.pop(0)
        elif head == "env":
            command = command[1:]
            while command and "=" in command[0]:
                command.pop(0)
        else:
            break
    return ((len(command) >= 2 and Path(command[0]).name == "focusforge-cli" and command[1] in ("stack", "export"))
            or (len(command) >= 4 and Path(command[0]).name.startswith("python")
                and command[1:4] == ["-m", "spikes.stacking_experiments", "photometric"]))


def readiness_document(runs: Path, registration_sha256: str, stage: str) -> dict[str, object]:
    states = {"parity": "source-ready", "calibration-off": "parity-ready", "calibration-global": "parity-ready", "quality": "allowance-ready", "repeat": "allowance-ready", "real": "allowance-ready"}
    path = runs / "photometric" / f"{stage}-admission.json"
    if not path.is_file(): raise ValueError("photometric immutable readiness evidence is missing")
    document = json.loads(path.read_text(), object_pairs_hook=_unique_object)
    if stage not in states:
        raise ValueError("photometric readiness stage is unknown")
    if not isinstance(document, dict) or document.get("schema") != "photometric-readiness-v1" or document.get("state") != states[stage] or document.get("registration_sha256") != registration_sha256:
        raise ValueError("photometric readiness evidence does not admit this stage")
    budget = document.get("budget")
    if (document.get("matrix") != command_plan() or isinstance(document.get("budget_remaining_seconds"), bool)
            or not isinstance(document.get("budget_remaining_seconds"), (int, float))
            or not np.isfinite(document["budget_remaining_seconds"]) or not isinstance(budget, dict)):
        raise ValueError("photometric readiness matrix or budget is invalid")
    limits = {"parity": 900, "calibration-off": 2500, "calibration-global": 2500, "quality": 4000, "repeat": 4000, "real": 1000}
    timeout = budget.get("timeout_seconds")
    remaining = budget.get("stage_remaining_seconds")
    session = budget.get("session_remaining_seconds")
    total = budget.get("total_remaining_seconds")
    protected = budget.get("protected_reserve_seconds")
    grace = budget.get("kill_grace_seconds")
    if (any(isinstance(value, bool) or not isinstance(value, (int, float)) or not np.isfinite(value) or value <= 0
            for value in (timeout, remaining, session, total))
            or isinstance(protected, bool) or not isinstance(protected, (int, float)) or not np.isfinite(protected) or protected < 0
            or grace != 10 or remaining > limits[stage] or session > 7200 or total > 21600
            or timeout > limits[stage] or timeout + grace > remaining or timeout + grace + protected > session or timeout + grace + protected > total
            or document["budget_remaining_seconds"] != remaining):
        raise ValueError("photometric readiness budget does not admit this stage")
    if not isinstance(document.get("source_hashes"), dict) or not isinstance(document.get("input_hashes"), dict) or not isinstance(document.get("body_receipts"), list) or not isinstance(document.get("finite_counts"), dict) or not document["source_hashes"] or not document["input_hashes"] or not document["body_receipts"]:
        raise ValueError("photometric readiness hash, receipt, or finite-count bindings are missing")
    for group in (document["source_hashes"], document["input_hashes"]):
        if any(not isinstance(path, str) or not isinstance(value, str) or len(value) != 64
               or any(char not in "0123456789abcdef" for char in value)
               or not Path(path).is_file() or sha256(Path(path).read_bytes()).hexdigest() != value
               for path, value in group.items()):
            raise ValueError("photometric readiness hash binding is invalid")
    registration_path, registration_digest = _binding(document.get("registration"), "photometric readiness registration binding is invalid")
    registration, actual_digest = registration_document(registration_path)
    if registration_digest != registration_sha256 or actual_digest != registration_sha256 or document["source_hashes"] != _source_hashes(registration):
        raise ValueError("photometric readiness source registry is incomplete or stale")
    originals = registration["original_inputs"]
    expected_inputs = {str(Path(binding["path"]).resolve()): binding["sha256"] for binding in originals.values()}
    if not expected_inputs.items() <= document["input_hashes"].items() or (stage == "parity" and document["input_hashes"] != expected_inputs):
        raise ValueError("photometric readiness input registry is incomplete or stale")
    if any(not isinstance(value, int) or isinstance(value, bool) or value < 0 for value in document["finite_counts"].values()):
        raise ValueError("photometric readiness hash binding is invalid")
    required = {"parity": {"source_controls": 4},
                "calibration-off": {"source_controls": 4, "parity": 35},
                "calibration-global": {"source_controls": 4, "parity": 35},
                "quality": {"source_controls": 4, "parity": 35, "calibration-off": 210, "calibration-global": 210,
                            "calibration-off-frame-errors": 560, "calibration-global-frame-errors": 560,
                            "identity-groups": 240, "identity-scalars": 640, "identity-allowance-keys": 64},
                "repeat": {"source_controls": 4, "parity": 35, "calibration-off": 210, "calibration-global": 210,
                           "calibration-off-frame-errors": 560, "calibration-global-frame-errors": 560,
                           "identity-groups": 240, "identity-scalars": 640, "identity-allowance-keys": 64, "quality": 315},
                "real": {"source_controls": 4, "parity": 35, "calibration-off": 210, "calibration-global": 210,
                         "calibration-off-frame-errors": 560, "calibration-global-frame-errors": 560,
                         "identity-groups": 240, "identity-scalars": 640, "identity-allowance-keys": 64}}
    if any(document["finite_counts"].get(name) != count for name, count in required[stage].items()):
        raise ValueError("photometric readiness finite counts do not prove prior stages")
    if stage.startswith("calibration"):
        parity = document.get("parity")
        if not isinstance(parity, dict):
            raise ValueError("photometric readiness lacks a sealed parity artifact")
        for name in ("path", "sha256", "audit_seal", "audit_seal_sha256"):
            if not isinstance(parity.get(name), str):
                raise ValueError("photometric readiness parity binding is invalid")
        artifact, seal = Path(parity["path"]), Path(parity["audit_seal"])
        if (not artifact.is_file() or not seal.is_file() or sha256(artifact.read_bytes()).hexdigest() != parity["sha256"]
                or sha256(seal.read_bytes()).hexdigest() != parity["audit_seal_sha256"]):
            raise ValueError("photometric readiness parity artifact or audit seal is stale")
    for receipt in document["body_receipts"]:
        if not isinstance(receipt, dict) or receipt.get("native_exit") != 0 or not isinstance(receipt.get("path"), str):
            raise ValueError("photometric readiness receipt is invalid")
        path = Path(receipt["path"])
        if not path.is_dir() or not all((path / name).is_file() for name in ("argv.json", "stdout.txt", "stderr.txt", "exit.txt", "time-v.txt")):
            raise ValueError("photometric readiness receipt is incomplete")
        try:
            argv = json.loads((path / "argv.json").read_text(), object_pairs_hook=_unique_object)
            native_exit = int((path / "exit.txt").read_text().strip())
            timing = (path / "time-v.txt").read_text()
            timing_lines = [line.strip() for line in timing.splitlines()]
            elapsed_values = [line.split(": ", 1)[1] for line in timing_lines if "Elapsed (wall clock)" in line]
            if len(elapsed_values) != 1:
                raise ValueError("missing or duplicate elapsed time")
            elapsed = elapsed_values[0]
            elapsed_seconds = sum(float(value) * 60 ** index for index, value in enumerate(reversed(elapsed.split(":"))))
        except (OSError, ValueError, StopIteration, json.JSONDecodeError) as error:
            raise ValueError("photometric readiness receipt contents are invalid") from error
        if (not isinstance(argv, list) or not argv or any(not isinstance(value, str) or not value for value in argv) or not _receipt_command(argv)
                or native_exit != 0 or not np.isfinite(elapsed_seconds) or elapsed_seconds < 0
                or len(re.findall(r"^Exit status:\s*0\s*$", "\n".join(timing_lines), flags=re.MULTILINE)) != 1
                or len(re.findall(r"^Exit status:", "\n".join(timing_lines), flags=re.MULTILINE)) != 1):
            raise ValueError("photometric readiness receipt does not show a successful command")
    return document


def control_records(out: Path) -> list[dict[str, object]]:
    out.mkdir(parents=True, exist_ok=True)
    frame = np.full((736, 1088, 3), 20000, dtype=np.uint16)
    spatial = Fit("spatial-multiplier", np.array((1., .12, .03, -.04, .02)), False, False, 5, False, np.array((1., .12, .03, -.04, .02)), None, frame.shape[0] * frame.shape[1])
    tile = {str(width): whole_tile_equal(frame, spatial, width) for width in (512, 1024)}
    bad = np.empty_like(frame)
    for y0 in range(0, frame.shape[0], 512):
        for x0 in range(0, frame.shape[1], 512): bad[y0:min(y0+512,frame.shape[0]),x0:min(x0+512,frame.shape[1])] = apply(frame[y0:min(y0+512,frame.shape[0]),x0:min(x0+512,frame.shape[1])], spatial)
    sample = np.full((8, 50, 50, 3), 10000, dtype=np.uint16); sample[0] = 5000; sample[REFERENCE, :, :25] = 6000; sample[REFERENCE, :, 25:] = 4000
    mask = np.ones((8, 50, 50), bool); mask[0, :, 25:] = False
    covered, uncovered = estimate("global-gain", sample, mask, 0), estimate("global-gain", sample, None, 0)
    thin = estimate("global-gain", np.full((8, 1, 999, 3), 10000, dtype=np.uint16), None, 0)
    usable = estimate("global-gain", np.full((8, 1, 1000, 3), 10000, dtype=np.uint16), None, 0)
    clamped = estimate("global-gain", np.stack([np.full((50,50,3), 5000 if i != REFERENCE else 20000, dtype=np.uint16) for i in range(8)]), None, 0)
    even = np.full((8, 1, 1000, 3), 10000, dtype=np.uint16); even[REFERENCE, :, 500:] = 14000
    odd = np.full((8, 1, 1001, 3), 10000, dtype=np.uint16); odd[REFERENCE, :, 500:] = 14000
    even_fit, odd_fit = estimate("global-gain", even, None, 0), estimate("global-gain", odd, None, 0)
    black = sample.copy(); black[0, 0, 0] = 0
    nonzero_invalid = sample.copy(); nonzero_invalid[0, 0, 0] = 65000
    nonzero_mask = mask.copy(); nonzero_mask[0, 0, 0] = False
    black_fit, invalid_fit = estimate("global-gain", black, mask, 0), estimate("global-gain", nonzero_invalid, nonzero_mask, 0)
    records = [{"name":"physical-tile-origin","ok":bool(all(tile.values()) and not np.array_equal(apply(frame, spatial), bad)),"tiles":tile},
               {"name":"q16-positive-half","ok":int(apply(np.array([[[2,2,2]]],dtype=np.uint16), Fit("global-gain",np.array((1.25,)),False,False,1,False))[0,0,0]) == 3},
               {"name":"coverage-bounds","ok":bool(np.isclose(covered.values[0],1.2) and np.isclose(uncovered.values[0],1.) and clamped.clamped)},
               {"name":"thin-usable","ok":bool(thin.thin and not usable.thin)},
               {"name":"even-odd-median","ok":bool(np.isclose(even_fit.values[0],1.2) and np.isclose(odd_fit.estimated[0],1.4) and odd_fit.values[0] == HIGH and odd_fit.clamped)},
               {"name":"valid-black-invalid-nonzero","ok":bool(black_fit.sample_count == covered.sample_count - 1 and invalid_fit.sample_count == covered.sample_count - 1)},
               {"name":"degenerate-spatial","ok":estimate("spatial-multiplier",np.full((8,1,1000,3),10000,dtype=np.uint16),None,0).identity},
               {"name":"matrix","ok":len(stage_rows("parity"))==35 and len(stage_rows("calibration-off"))==210 and len(stage_rows("quality"))==315 and len(stage_rows("real"))==12}]
    (out / "controls.json").write_text(json.dumps(records, indent=2, sort_keys=True)+"\n")
    if not all(record["ok"] for record in records): raise AssertionError("photometric arithmetic control failed")
    return records
