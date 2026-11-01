# SPDX-License-Identifier: GPL-3.0-or-later
"""Validation, records, and fixed inputs for alignment-reference measurements."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from hashlib import sha256
import json
import math
from pathlib import Path
import re
import subprocess
from typing import Any, Callable, Iterable, Mapping

from .protocol import TRAINING_SEEDS, SeedTriple, require_seed

REFERENCES = ("first", "middle")
FRAME_COUNTS = (8, 63)
SIGMAS = (.15, .45)
STRATEGIES = ("anchor", "chain", "rolling")
LIGHTING = ("unchanged", "spatial", "global", "channel")
ORDERS = ("none", "gain-before", "truth-oracle")
STAGES = ("source", "parity", "calibration", "controls", "quality", "repeat", "real")
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
FRAME_RE = re.compile(r"frame-(\d{2})\Z")
REQUIRED_SOURCE_PATHS = frozenset((
    "Cargo.toml", "Cargo.lock", "crates/core/src/align/mod.rs", "crates/core/src/align/driver.rs",
    "crates/core/Cargo.toml", "crates/core/src/lib.rs", "crates/cli/Cargo.toml",
    "crates/core/src/align/ecc.rs", "crates/core/src/align/ecc_algorithm.rs", "crates/core/src/align/transform.rs",
    "crates/core/src/align/warp.rs", "crates/core/src/align/warp_reader.rs", "crates/core/src/align/crop.rs",
    "crates/core/src/align/pyramid.rs", "crates/core/src/algorithm/mod.rs", "crates/core/src/equalize.rs",
    "crates/core/src/io/mod.rs", "crates/core/src/params/mod.rs", "crates/core/src/pipeline/mod.rs",
    "crates/cli/src/align.rs", "crates/cli/src/main.rs", "spikes/stacking_experiments/__init__.py",
    "spikes/stacking_experiments/__main__.py", "spikes/stacking_experiments/protocol.py",
    "spikes/stacking_experiments/scenes.py", "spikes/stacking_experiments/metrics.py",
    "spikes/stacking_experiments/runner.py", "spikes/stacking_experiments/alignment_reference.py",
    "spikes/guided-filter-fusion/gff_fusion.py", "spikes/synthetic-ground-truth/synth_stack.py", "tools/lib/rawtiff.py",
    "spikes/alignment-reference-strategies/Cargo.toml",
    "spikes/alignment-reference-strategies/Cargo.lock", "spikes/alignment-reference-strategies/src/main.rs",
))


@dataclass(frozen=True)
class Configuration:
    group: str
    strategy: str
    reference: str
    frames: int
    sigma: float
    seed: str
    lighting: str | None = None
    order: str | None = None

    def key(self) -> tuple[object, ...]:
        return (self.group, self.strategy, self.reference, self.frames, self.sigma, self.seed, self.lighting, self.order)


def _seed_text(seed: SeedTriple) -> str:
    require_seed(seed)
    return seed.text()


def configurations() -> tuple[Configuration, ...]:
    seeds = tuple(_seed_text(seed) for seed in TRAINING_SEEDS)
    geometry = tuple(Configuration("geometry", strategy, reference, frames, sigma, seed)
                     for strategy in STRATEGIES for reference in REFERENCES for frames in FRAME_COUNTS
                     for sigma in SIGMAS for seed in seeds)
    photometry = tuple(Configuration("photometry", strategy, "middle", 8, .45, seed, light, order)
                        for strategy in ("anchor", "rolling") for light in LIGHTING for order in ORDERS for seed in seeds)
    result = geometry + photometry
    if len(result) != 240 or len({row.key() for row in result}) != 240:
        raise ValueError("configuration matrix is not finite and unique")
    return result


def stage_counts() -> dict[str, int]:
    rows = configurations()
    calibration = [row for row in rows if row.group == "geometry" and row.strategy in {"anchor", "chain"}]
    calibration += [row for row in rows if row.group == "photometry" and row.strategy == "anchor" and row.order == "none"]
    if len(calibration) != 100:
        raise ValueError("calibration selection differs")
    return {"calibration": 200, "quality": 140, "repeat": 140, "parity": 4, "controls": 2, "real": 6, "total": 492}


def metric_key(row: Configuration, metric: str, frame_group: str, control_table: str) -> tuple[object, ...]:
    if not metric or not control_table:
        raise ValueError("metric key lacks metric or control table")
    if frame_group != "stack":
        match = FRAME_RE.fullmatch(frame_group)
        if match is None or int(match.group(1)) < 0 or int(match.group(1)) >= row.frames:
            raise ValueError("frame group does not belong to configuration")
    if row.group == "geometry" and (row.lighting is not None or row.order is not None):
        raise ValueError("geometry metric key has photometric fields")
    return (control_table, row.group, row.strategy, row.reference, row.frames, row.sigma, row.lighting, row.order, metric, frame_group, row.seed)


def _finite(value: object) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(float(value)):
        raise ValueError("value is not finite")
    return float(value)


def calibration_allowance(observations: Mapping[str, tuple[object, object]]) -> float:
    expected = {_seed_text(seed) for seed in TRAINING_SEEDS}
    if set(observations) != expected:
        raise ValueError("calibration requires exactly five registered seeds")
    means = []
    for seed in sorted(expected):
        first, second = observations[seed]
        means.append((_finite(first) + _finite(second)) / 2)
    return max(means) - min(means)


def paired_repeat_spread(pairs: Mapping[str, tuple[object, object]]) -> float:
    expected = {_seed_text(seed) for seed in TRAINING_SEEDS}
    if set(pairs) != expected:
        raise ValueError("repeat requires exactly five registered seeds")
    return max(abs(_finite(first) - _finite(second)) for first, second in pairs.values())


def allowance(calibration: Mapping[str, tuple[object, object]], repeats: Mapping[str, tuple[object, object]]) -> float:
    return max(calibration_allowance(calibration), paired_repeat_spread(repeats))


def error_budget(calibration: Iterable[object], instrument_allowance: object) -> float:
    values = tuple(_finite(value) for value in calibration)
    allowance_value = _finite(instrument_allowance)
    if len(values) != 10 or allowance_value < 0:
        raise ValueError("corner error budget requires ten finite calibration values")
    return max(values) + 2 * allowance_value


def known_warp_spec(frames: int, sigma: float, seed: SeedTriple, reference: str) -> dict[str, object]:
    if frames not in FRAME_COUNTS or sigma not in SIGMAS or reference not in REFERENCES:
        raise ValueError("scene request is outside the fixed matrix")
    require_seed(seed)
    return {"frames": frames, "sigma_step": sigma, "reference": reference, "depth": "layout/7*(frames-1)",
            "defocus": "sigma-bank-step-.2; intermediate-u16-before-warp", "noise": {"distribution": "normal", "sd": 250, "seed_xor": "0x5A5A5A5A"},
            "truth_rebase": "M_i_middle*inverse(M_reference_middle)", "seed": asdict(seed)}


def lighting_spec(member: str) -> dict[str, object]:
    if member not in LIGHTING:
        raise ValueError("unknown lighting member")
    formulas = {"unchanged": "truth", "spatial": "truth*(1+.025*linear_x)", "global": "truth*(1+.06*p)+220*p",
                "channel": "rgb gains(1+.06*p,1-.04*p,1+.03*p), offsets(180*p,-140*p,110*p)"}
    return {"member": member, "formula": formulas[member], "p": "(frame-4)/4", "order": "preclip,clip,defocus-u16,warp,noise-u16"}


def _sha(value: object) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _matrix(value: object) -> bool:
    return isinstance(value, list) and len(value) == 3 and all(isinstance(row, list) and len(row) == 3 and all(isinstance(number, (int, float)) and not isinstance(number, bool) and math.isfinite(float(number)) for number in row) for row in value)


def validate_manifest(manifest: Mapping[str, Any], expected_strategy: str, expected_reference: str) -> None:
    required = ("strategy", "reference", "raw_paths", "raw_hashes", "raw_indices", "dimensions", "roi_paths", "roi_hashes", "transforms", "statuses", "output_paths", "output_hashes", "reader_coverage", "prealigned", "crop_before_status", "roi", "solve_window")
    if any(name not in manifest for name in required):
        raise ValueError("real manifest is incomplete")
    if expected_strategy not in STRATEGIES or expected_reference not in REFERENCES or manifest["strategy"] != expected_strategy or manifest["reference"] != expected_reference:
        raise ValueError("real manifest strategy or reference differs")
    if manifest["prealigned"] is not False or manifest["crop_before_status"] is not False:
        raise ValueError("real manifest bypasses raw-before-crop alignment")
    values = [manifest[name] for name in ("raw_paths", "raw_hashes", "roi_paths", "roi_hashes", "transforms", "statuses", "output_paths", "output_hashes", "reader_coverage")]
    if any(not isinstance(value, list) or not value for value in values) or len({len(value) for value in values}) != 1 or len(manifest["raw_paths"]) != 8:
        raise ValueError("real manifest frame arrays differ")
    if tuple(manifest["raw_indices"]) != REAL_FRAME_INDICES or manifest["dimensions"] != list(REAL_DIMENSIONS):
        raise ValueError("real manifest raw identity differs")
    if len(set(manifest["raw_paths"])) != 8 or len(set(manifest["raw_hashes"])) != 8:
        raise ValueError("real manifest raw frame identity differs")
    if any(not isinstance(path, str) or not Path(path).is_file() for path in manifest["raw_paths"]) or any(_digest(Path(path)) != digest for path, digest in zip(manifest["raw_paths"], manifest["raw_hashes"])) or any(not _sha(value) for value in [*manifest["raw_hashes"], *manifest["output_hashes"]]):
        raise ValueError("real manifest hash binding is invalid")
    if any(not isinstance(path, str) or not Path(path).is_file() for path in manifest["roi_paths"]) or any(_digest(Path(path)) != digest for path, digest in zip(manifest["roi_paths"], manifest["roi_hashes"])) or any(not _sha(value) for value in manifest["roi_hashes"]):
        raise ValueError("real ROI hash binding is invalid")
    if any(not _matrix(value) for value in manifest["transforms"]) or any(status not in {"converged", "hit-cap", "stalled"} for status in manifest["statuses"]):
        raise ValueError("real manifest has invalid transform or status")
    if any(not Path(path).is_file() or _digest(Path(path)) != digest for path, digest in zip(manifest.get("output_paths", ()), manifest["output_hashes"])):
        raise ValueError("real output binding is invalid")
    if any(isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(float(value)) or float(value) != 1.0 for value in manifest["reader_coverage"]):
        raise ValueError("real manifest coverage is invalid")
    for name in ("roi", "solve_window"):
        rectangle = manifest[name]
        if not isinstance(rectangle, Mapping) or set(rectangle) != {"x", "y", "width", "height"}:
            raise ValueError("real manifest coordinate binding is invalid")
        if any(isinstance(rectangle[coordinate], bool) or not isinstance(rectangle[coordinate], int) for coordinate in rectangle) or rectangle["x"] < 0 or rectangle["y"] < 0 or rectangle["width"] <= 0 or rectangle["height"] <= 0:
            raise ValueError("real manifest rectangle is invalid")
    if manifest["roi"] != REAL_ROI:
        raise ValueError("real manifest ROI differs from frozen panel")
    if manifest["solve_window"] != real_solve_window(*REAL_DIMENSIONS):
        raise ValueError("real manifest solve window differs")


def _digest(path: Path) -> str:
    return sha256(path.read_bytes()).hexdigest()


@dataclass(frozen=True)
class SourceBinding:
    path: str
    sha256: str


def validate_source_closure(bindings: Iterable[Mapping[str, object]], executables: Mapping[str, object]) -> tuple[SourceBinding, ...]:
    rows = tuple(SourceBinding(str(row.get("path", "")), str(row.get("sha256", ""))) for row in bindings)
    if (not rows or len({row.path for row in rows}) != len(rows)
            or any(not row.path or Path(row.path).is_absolute() or ".." in Path(row.path).parts or not _sha(row.sha256) for row in rows)):
        raise ValueError("source closure is incomplete")
    if not REQUIRED_SOURCE_PATHS.issubset({row.path for row in rows}):
        raise ValueError("source closure omits a required dependency")
    if set(executables) != {"focusforge-cli", "alignment-reference-strategies"} or any(not _sha(value) for value in executables.values()):
        raise ValueError("executable binding is incomplete")
    return tuple(sorted(rows, key=lambda row: row.path))


def bind_source_closure(root: Path, bindings: Iterable[Mapping[str, object]], executables: Mapping[str, object]) -> tuple[SourceBinding, ...]:
    """Verify byte-exact regular source files and freshly built executable paths."""
    root = root.resolve()
    rows = validate_source_closure(bindings, executables)
    for row in rows:
        source = (root / row.path).resolve()
        if root not in source.parents or not source.is_file() or _digest(source) != row.sha256:
            raise ValueError("source closure binding differs")
    for name, digest in executables.items():
        path = root / "target" / "debug" / str(name)
        if not path.is_file() or _digest(path) != digest:
            raise ValueError("executable binding differs")
    return rows


def stage_plan(stage: str) -> dict[str, object]:
    calls = stage_calls(stage)
    return {"stage": stage, "alignment_invocations": len(calls), "immutable_output": True,
            "acceptance": "requires recorded executor, physical receipts, and independently authenticated bindings"}


@dataclass(frozen=True)
class StageCall:
    """One immutable alignment invocation selected from the fixed matrix."""

    stage: str
    ordinal: int
    configuration: Configuration | None
    label: str

    def key(self) -> Mapping[str, object] | None:
        """Return the complete comparison key stored beside a physical record."""
        if self.configuration is None:
            return None
        row = self.configuration
        return {
            "group": row.group,
            "strategy": row.strategy,
            "reference": row.reference,
            "frames": row.frames,
            "sigma": row.sigma,
            "seed": row.seed,
            "lighting": row.lighting,
            "order": row.order,
        }


def calibration_configurations() -> tuple[Configuration, ...]:
    """Return the 100 baseline configurations before their fixed second pass."""
    rows = configurations()
    selected = tuple(
        row
        for row in rows
        if (row.group == "geometry" and row.strategy in {"anchor", "chain"})
        or (row.group == "photometry" and row.strategy == "anchor" and row.order == "none")
    )
    if len(selected) != 100:
        raise ValueError("calibration matrix differs")
    return selected


def quality_configurations() -> tuple[Configuration, ...]:
    """Return every configuration not consumed by the baseline matrix."""
    baseline = set(calibration_configurations())
    selected = tuple(row for row in configurations() if row not in baseline)
    if len(selected) != 140:
        raise ValueError("quality matrix differs")
    return selected


@dataclass(frozen=True)
class SyntheticInput:
    """Fixed source construction for one matrix member before rendering."""

    configuration: Configuration
    depth_normalization: str
    defocus: str
    lighting: Mapping[str, object]
    warp: Mapping[str, object]
    raw_specification: str


def synthetic_input(row: Configuration) -> SyntheticInput:
    """Describe shared raw input identity independent of reference or strategy."""
    seed = next(seed for seed in TRAINING_SEEDS if seed.text() == row.seed)
    lighting = lighting_spec(row.lighting or "unchanged")
    warp = known_warp_spec(row.frames, row.sigma, seed, row.reference)
    # Reference rebasing and fit order are consumers of one measured raw
    # stack; neither may create a different advertised raw identity.
    raw_specification = sha256(
        f"{row.frames}|{row.sigma:.2f}|{row.seed}|{row.lighting or 'none'}".encode()
    ).hexdigest()
    return SyntheticInput(
        row,
        "layout_depth/7*(frames-1)",
        "render(noise_sd=0), round+clip u16, warp, normal(0,250), round+clip u16",
        lighting,
        warp,
        raw_specification,
    )


def require_shared_raw_specification(rows: Iterable[Configuration]) -> str:
    """Reject a comparison whose configurations do not name one raw specification."""
    identities = {synthetic_input(row).raw_specification for row in rows}
    if len(identities) != 1:
        raise ValueError("comparison does not share raw bytes")
    return identities.pop()


@dataclass(frozen=True)
class Rectangle:
    """Integer rectangle returned by the product crop operation."""

    x: int
    y: int
    width: int
    height: int

    def area(self) -> int:
        return self.width * self.height if self.width > 0 and self.height > 0 else 0


def crop_iou(fitted: Rectangle | None, truth: Rectangle | None) -> float | None:
    """Return stack crop IoU; missing product crops remain unavailable."""
    if fitted is None or truth is None:
        return None
    left, top = max(fitted.x, truth.x), max(fitted.y, truth.y)
    right = min(fitted.x + fitted.width, truth.x + truth.width)
    bottom = min(fitted.y + fitted.height, truth.y + truth.height)
    intersection = max(0, right - left) * max(0, bottom - top)
    union = fitted.area() + truth.area() - intersection
    return None if union == 0 else intersection / union


def stack_overlap_loss(fitted: Rectangle | None, truth: Rectangle | None) -> float | None:
    """Return the stack-only crop loss; no per-frame overlap is emitted."""
    iou = crop_iou(fitted, truth)
    return None if iou is None else 1.0 - iou


def matrix_product(left: list[list[object]], right: list[list[object]]) -> list[list[float]]:
    """Multiply finite affine records in the same order as product transforms."""
    if not _matrix(left) or not _matrix(right):
        raise ValueError("transform matrix is invalid")
    return [[sum(float(left[row][index]) * float(right[index][column]) for index in range(3)) for column in range(3)] for row in range(3)]


def ols_slope(points: Iterable[tuple[object, object]]) -> tuple[float, float]:
    """Return slope and intercept; duplicate hop distance leaves a null metric."""
    values = tuple((_finite(x), _finite(y)) for x, y in points)
    if len(values) < 2:
        raise ValueError("drift requires at least two nonreference frames")
    mean_x = sum(x for x, _ in values) / len(values)
    mean_y = sum(y for _, y in values) / len(values)
    denominator = sum((x - mean_x) ** 2 for x, _ in values)
    if denominator == 0.0:
        raise ValueError("drift hop distances are identical")
    slope = sum((x - mean_x) * (y - mean_y) for x, y in values) / denominator
    return slope, mean_y - slope * mean_x


@dataclass(frozen=True)
class RenderedSynthetic:
    """Generated frames and independent truth retained for later physical records."""

    frames: object
    oracle_frames: object
    truth: object
    transforms_middle: object
    transforms_reference: object
    warp_coverage: object
    clipped: object
    clip_fraction: tuple[float, ...]
    raw_specification: str


def _lighting_array(truth: object, member: str, frame: int) -> object:
    """Apply the registered pre-clip lighting law to one source image."""
    import numpy as np

    p = (frame - 4) / 4
    if member == "unchanged":
        return truth.copy()
    if member == "spatial":
        return truth * (1.0 + 0.025 * np.linspace(-1.0, 1.0, truth.shape[1])[None, :, None])
    if member == "global":
        return truth * (1.0 + 0.06 * p) + 220.0 * p
    if member == "channel":
        gains = np.array((1.0 + 0.06 * p, 1.0 - 0.04 * p, 1.0 + 0.03 * p))
        offsets = np.array((180.0 * p, -140.0 * p, 110.0 * p))
        return truth * gains[None, None, :] + offsets[None, None, :]
    raise ValueError("unknown lighting member")


def _oracle_array(observed: object, member: str, frame: int) -> object:
    """Invert pre-clip lighting only where the observed source was not censored."""
    import numpy as np

    p = (frame - 4) / 4
    visible = (observed > 0.0) & (observed < 65535.0)
    if member == "unchanged":
        corrected = observed
    elif member == "spatial":
        corrected = observed / (1.0 + 0.025 * np.linspace(-1.0, 1.0, observed.shape[1])[None, :, None])
    elif member == "global":
        corrected = (observed - 220.0 * p) / (1.0 + 0.06 * p)
    elif member == "channel":
        gains = np.array((1.0 + 0.06 * p, 1.0 - 0.04 * p, 1.0 + 0.03 * p))
        offsets = np.array((180.0 * p, -140.0 * p, 110.0 * p))
        corrected = (observed - offsets[None, None, :]) / gains[None, None, :]
    else:
        raise ValueError("unknown lighting member")
    return np.where(visible, corrected, observed)


def frames_for_order(rendered: RenderedSynthetic, order: str) -> object:
    """Select none or oracle input; gain-before remains the product reader path."""
    if order in {"none", "gain-before"}:
        return rendered.frames
    if order == "truth-oracle":
        return rendered.oracle_frames
    raise ValueError("unknown fit order")


def blur_metrics(fitted: object, truth: object, support: object) -> tuple[float | None, float | None]:
    """Measure encoded-count blur and Rec.709 finite-difference energy."""
    import numpy as np

    fitted_array = np.asarray(fitted)
    truth_array = np.asarray(truth)
    valid = np.asarray(support, dtype=bool)
    if fitted_array.shape != truth_array.shape or fitted_array.ndim != 3 or fitted_array.shape[2] != 3 or valid.shape != fitted_array.shape[:2] or not valid.any():
        return None, None
    mae = float(np.abs(fitted_array.astype(np.int64) - truth_array.astype(np.int64))[valid].sum()) / (3 * int(valid.sum()))
    luma = fitted_array.astype(np.float64) @ np.array((0.2126, 0.7152, 0.0722))
    horizontal = valid[:, :-1] & valid[:, 1:]
    vertical = valid[:-1, :] & valid[1:, :]
    terms = []
    if horizontal.any():
        terms.extend(((luma[:, 1:] - luma[:, :-1])[horizontal] ** 2).tolist())
    if vertical.any():
        terms.extend(((luma[1:, :] - luma[:-1, :])[vertical] ** 2).tolist())
    return mae, None if not terms else float(sum(terms) / len(terms))


def stack_blur_mae(frames: Iterable[tuple[object, object, object]]) -> float | None:
    """Pool encoded absolute differences over all nonreference valid samples."""
    import numpy as np

    numerator = 0
    denominator = 0
    for fitted, truth, support in frames:
        fitted_array, truth_array = np.asarray(fitted), np.asarray(truth)
        valid = np.asarray(support, dtype=bool)
        if fitted_array.shape != truth_array.shape or fitted_array.ndim != 3 or valid.shape != fitted_array.shape[:2]:
            return None
        numerator += int(np.abs(fitted_array.astype(np.int64) - truth_array.astype(np.int64))[valid].sum())
        denominator += 3 * int(valid.sum())
    return None if denominator == 0 else numerator / denominator


def instrument_allowance(clean_first: object, clean_second: object) -> float:
    """Freeze a measured detector allowance from two product-resampler readings."""
    return abs(_finite(clean_first) - _finite(clean_second))


def detector_detected(clean_first: object, clean_second: object, injected: object) -> bool:
    """Require a positive measured defect strictly above its frozen allowance."""
    baseline = _finite(clean_first)
    return _finite(injected) - baseline > instrument_allowance(clean_first, clean_second)


def detector_transform(transform: list[list[object]], offset_x: object) -> list[list[float]]:
    """Apply the registered absolute x-offset before corner measurement."""
    _finite(offset_x)
    return matrix_product([[1.0, 0.0, _finite(offset_x)], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], transform)


def hop_detector_transform(transform: list[list[object]], frame: int, reference: int, maximum_distance: int) -> list[list[float]]:
    """Apply the registered endpoint drift control, reaching 0.5 px."""
    if maximum_distance <= 0:
        raise ValueError("hop detector requires nonzero maximum distance")
    return detector_transform(transform, 0.5 * abs(frame - reference) / maximum_distance)


def require_shared_raw_bytes(manifests: Iterable[Mapping[str, object]]) -> tuple[str, ...]:
    """Require equal real raw TIFF digests for every matched strategy/reference row."""
    rows = tuple(manifests)
    if not rows:
        raise ValueError("comparison has no raw manifests")
    hashes = []
    for manifest in rows:
        frames = manifest.get("frames") if isinstance(manifest, Mapping) else None
        if not isinstance(frames, list) or not frames or any(not isinstance(frame, Mapping) or not _sha(frame.get("sha256")) for frame in frames):
            raise ValueError("raw manifest is incomplete")
        hashes.append(tuple(str(frame["sha256"]) for frame in frames))
    if len(set(hashes)) != 1:
        raise ValueError("comparison does not share raw bytes")
    return hashes[0]

def spike_argv(binary: Path, input_directory: Path, strategy: str, reference: str) -> list[str]:
    """Build the one-spike command for a raw input directory."""
    if strategy not in STRATEGIES or reference not in REFERENCES:
        raise ValueError("native strategy request is outside the fixed matrix")
    return [str(binary), "--input", str(input_directory), "--strategy", strategy, "--reference", reference]


def product_cli_argv(binary: Path, input_directory: Path, strategy: str, reference: str) -> list[str]:
    """Build one strict current-product parity command without executing it."""
    if strategy not in {"anchor", "chain"} or reference not in REFERENCES:
        raise ValueError("product parity request is outside the fixed matrix")
    return [
        str(binary), "align", str(input_directory), "--json", "--strict", "--algorithm", "pyramidal-ecc", "--cpu",
        "--set", "mode=rotation-scale", "--set", f"reference-policy={strategy}", "--set", f"reference-frame={reference}",
    ]


REAL_FRAME_INDICES = (0, 8, 17, 26, 34, 43, 52, 60)
REAL_DIMENSIONS = (4781, 3136)
REAL_ROI = {"x": 1878, "y": 1056, "width": 1024, "height": 1024}


def _file_binding(value: object, label: str) -> tuple[Path, str]:
    if not isinstance(value, Mapping):
        raise ValueError(f"{label} binding is absent")
    path, digest = Path(str(value.get("path", ""))), value.get("sha256")
    if not path.is_file() or not _sha(digest) or _digest(path) != digest:
        raise ValueError(f"{label} bytes differ")
    return path.resolve(), str(digest)


def _load_object(path: Path, label: str) -> Mapping[str, object]:
    try:
        value = json.loads(path.read_text())
    except json.JSONDecodeError as error:
        raise ValueError(f"{label} is not JSON") from error
    if not isinstance(value, Mapping):
        raise ValueError(f"{label} is not an object")
    return value


def _exact_input_directory(frames: list[Mapping[str, str]], label: str) -> Path:
    """Refuse native directory enumeration beyond the authenticated order."""
    if not frames:
        raise ValueError(f"{label} input directory is empty")
    paths = [Path(row["path"]).resolve() for row in frames]
    directory = paths[0].parent
    if any(path.parent != directory for path in paths):
        raise ValueError(f"{label} input directories differ")
    entries = list(directory.iterdir())
    if (directory / "frames").is_dir():
        raise ValueError(f"{label} input directory frames override differs")
    if any(path.is_symlink() and path.suffix.lower() in {".tif", ".tiff"} for path in entries):
        raise ValueError(f"{label} input directory membership or order differs")
    observed = sorted((path.resolve() for path in entries
                       if path.is_file() and path.suffix.lower() in {".tif", ".tiff"}),
                      key=lambda path: path.name)
    if observed != paths:
        raise ValueError(f"{label} input directory membership or order differs")
    return directory


def recursive_source_closure(root: Path) -> tuple[SourceBinding, ...]:
    """Read the canonical regular-file closure used by the registered run.

    The fixed required set is the lower bound from the registration.  Local
    Local Python imports, Rust module/include declarations, and Cargo roots
    add reachable files; symlinks, directories, and paths outside ``root``
    are rejected.
    """
    root = root.resolve()

    def canonical(candidate: str) -> str:
        requested = root / candidate
        cursor = Path(requested.anchor)
        for part in requested.parts[1:]:
            cursor = cursor / part
            if cursor.is_symlink():
                raise ValueError("source closure contains a symlink")
        resolved = requested.resolve()
        if root not in resolved.parents or not resolved.is_file() or resolved.is_symlink():
            raise ValueError("source closure contains an unavailable regular file")
        return resolved.relative_to(root).as_posix()

    pending = set(REQUIRED_SOURCE_PATHS)
    seen: set[str] = set()

    def add_candidate(candidate: Path) -> None:
        requested = root / candidate
        if requested.is_file() or requested.is_symlink():
            pending.add(canonical(candidate.as_posix()))

    def module_candidates(module: str, parent: Path) -> tuple[Path, ...]:
        if module.startswith("."):
            levels = len(module) - len(module.lstrip("."))
            base = parent
            for _ in range(levels - 1):
                base = base.parent
            base = base / module.lstrip(".").replace(".", "/")
        elif module == "spikes" or module.startswith("spikes.") or module == "tools" or module.startswith("tools."):
            base = Path(module.replace(".", "/"))
        else:
            return ()
        return (base.with_suffix(".py"), base / "__init__.py")

    while pending:
        relative = pending.pop()
        relative = canonical(relative)
        path = root / relative
        if relative in seen:
            continue
        seen.add(relative)
        text = path.read_text(errors="strict") if path.suffix in {".py", ".rs"} else ""
        parent = Path(relative).parent
        if path.suffix == ".py":
            for module in re.findall(r"^\s*import\s+([^\n#]+)", text, re.MULTILINE):
                for name in module.split(","):
                    target = name.strip().split(" as ", 1)[0].strip()
                    for candidate in module_candidates(target, parent):
                        add_candidate(candidate)
            for module, names in re.findall(r"^\s*from\s+([.\w]+)\s+import\s+([^\n#]+)", text, re.MULTILINE):
                candidates = module_candidates(module, parent)
                for candidate in candidates:
                    add_candidate(candidate)
                if module.startswith("."):
                    levels = len(module) - len(module.lstrip("."))
                    base = parent
                    for _ in range(levels - 1):
                        base = base.parent
                    base = base / module.lstrip(".").replace(".", "/")
                elif module == "spikes" or module.startswith("spikes.") or module == "tools" or module.startswith("tools."):
                    base = Path(module.replace(".", "/"))
                else:
                    base = None
                if base is None:
                    continue
                for name in names.split(","):
                    leaf = name.strip().split(" as ", 1)[0].strip()
                    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", leaf):
                        add_candidate(base / f"{leaf}.py")
        if path.suffix == ".rs":
            for module in re.findall(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*);", text, re.MULTILINE):
                add_candidate(parent / f"{module}.rs")
                add_candidate(parent / module / "mod.rs")
            for declared in re.findall(r"#\s*\[\s*path\s*=\s*\"([^\"]+)\"\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*;", text):
                add_candidate(parent / declared)
            for included in re.findall(r"\binclude(?:_str|_bytes)?!\s*\(\s*\"([^\"]+)\"\s*\)", text):
                add_candidate(parent / included)
            if re.search(r"\binclude(?:_str|_bytes)?!\s*\(\s*\$", text):
                directory = root / parent
                for asset in directory.iterdir():
                    if asset.is_file() and asset.suffix not in {".rs", ".toml"}:
                        add_candidate(asset.relative_to(root))
        if path.name == "Cargo.toml":
            cargo = path.read_text(errors="strict")
            for entry in ("src/lib.rs", "src/main.rs"):
                add_candidate(parent / entry)
            add_candidate(parent / "build.rs")
            for build in re.findall(r"(?m)^\s*build\s*=\s*\"([^\"]+)\"", cargo):
                add_candidate(parent / build)
            for declared in re.findall(r"\bpath\s*=\s*\"([^\"]+)\"", cargo):
                target = parent / declared
                add_candidate(target)
                add_candidate(target / "Cargo.toml")
            for block in re.findall(r"\[\[bin\]\]([\s\S]*?)(?=\n\s*\[|\Z)", cargo):
                name = re.search(r"(?m)^\s*name\s*=\s*\"([^\"]+)\"", block)
                if name is not None and not re.search(r"(?m)^\s*path\s*=", block):
                    add_candidate(parent / "src" / "bin" / f"{name.group(1)}.rs")
    return tuple(SourceBinding(path, _digest(root / path)) for path in sorted(seen))


def snapshot_source_graph(root: Path, destination: Path, executables: Mapping[str, Path]) -> Mapping[str, object]:
    """Copy the live closure and both freshly built executables byte-for-byte."""
    import shutil
    if destination.exists():
        raise ValueError("snapshot destination already exists")
    closure = recursive_source_closure(root)
    destination.mkdir(parents=True)
    source_rows = []
    for binding in closure:
        source, copied = root / binding.path, destination / "source" / binding.path
        copied.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, copied)
        if _digest(copied) != binding.sha256:
            raise ValueError("source snapshot copy differs")
        source_rows.append(asdict(binding))
    binaries: dict[str, Mapping[str, str]] = {}
    for name in ("focusforge-cli", "alignment-reference-strategies"):
        executable = executables.get(name)
        if executable is None or not executable.is_file() or executable.is_symlink():
            raise ValueError("fresh executable is absent")
        copied = destination / "executables" / name
        copied.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(executable, copied)
        digest = _digest(executable)
        if _digest(copied) != digest:
            raise ValueError("executable snapshot copy differs")
        binaries[name] = {"path": str(executable.resolve()), "sha256": digest,
                          "snapshot": str(copied.resolve()), "snapshot_sha256": _digest(copied)}
    record = {"closure": source_rows, "executables": binaries}
    manifest = destination / "source-graph.json"
    manifest.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    return {**record, "manifest": {"path": str(manifest.resolve()), "sha256": _digest(manifest)}}


def _valid_frame_record(row: Mapping[str, object], expected: Configuration) -> None:
    reference_index = 0 if expected.reference == "first" else (expected.frames - 1) // 2
    if row.get("strategy") != expected.strategy or row.get("reference") != reference_index:
        raise ValueError("native strategy or reference differs")
    frames = row.get("frames")
    if not isinstance(frames, list) or len(frames) != expected.frames:
        raise ValueError("native frame count differs")
    for index, frame in enumerate(frames):
        if not isinstance(frame, Mapping) or frame.get("frame") != index:
            raise ValueError("native frame identity differs")
        required = {"frame", "status", "reason", "iterations", "iterations_reason", "read_error", "best_rho", "solver_fraction", "validation_fraction", "solver_fraction_reason", "validation_fraction_reason", "interpolation_count", "fitting_template_coverage", "fitting_moving_coverage", "fitting_window", "fitting_template_frame", "fitting_prewarp", "fitting_reader", "transform"}
        if not required.issubset(frame):
            raise ValueError("native frame schema is incomplete")
        if frame.get("status") not in {"converged", "hit-cap", "stalled", "diverged", "read-error", "injected-failure"}:
            raise ValueError("native status differs")
        if frame.get("status") == "read-error":
            if not isinstance(frame.get("read_error"), str) or not frame["read_error"]:
                raise ValueError("native read error differs")
        elif frame.get("read_error") is not None:
            raise ValueError("native successful frame carries a read error")
        transform = frame.get("transform")
        if not _matrix(transform):
            raise ValueError("native transform is invalid")
        rho = frame.get("best_rho")
        if rho is not None:
            _finite(rho)
        if frame.get("iterations") is None:
            if frame.get("iterations_reason") != "not-exposed-by-current-cli":
                raise ValueError("native iteration absence differs")
        elif isinstance(frame.get("iterations"), bool) or not isinstance(frame.get("iterations"), int) or frame["iterations"] < 0 or frame.get("iterations_reason") is not None:
            raise ValueError("native iteration count differs")
        if isinstance(frame.get("interpolation_count"), bool) or not isinstance(frame.get("interpolation_count"), int) or frame["interpolation_count"] < 0:
            raise ValueError("native interpolation count differs")
        for name in ("solver_fraction", "validation_fraction"):
            if frame.get(name) is not None:
                _finite(frame[name])
        for name in ("fitting_template_coverage", "fitting_moving_coverage"):
            if frame.get(name) is not None and not 0.0 <= _finite(frame[name]) <= 1.0:
                raise ValueError("native fitting coverage differs")
        window = frame.get("fitting_window")
        template = frame.get("fitting_template_frame")
        prewarp, reader = frame.get("fitting_prewarp"), frame.get("fitting_reader")
        if window is not None and (not isinstance(window, list) or len(window) != 4 or any(isinstance(value, bool) or not isinstance(value, int) or value < 0 for value in window) or window[2] <= 0 or window[3] <= 0):
            raise ValueError("native fitting window differs")
        if template is not None and (isinstance(template, bool) or not isinstance(template, int) or not 0 <= template < expected.frames):
            raise ValueError("native fitting template differs")
        if prewarp is not None and not _matrix(prewarp):
            raise ValueError("native fitting prewarp differs")
        if reader is not None and reader not in {"raw-reader", "gain-reader", "unit-reader"}:
            raise ValueError("native fitting reader differs")
        if expected.strategy == "rolling" and index != reference_index and (window is None or template is None or prewarp is None or reader is None or (frame.get("status") != "read-error" and (frame.get("fitting_template_coverage") is None or frame.get("fitting_moving_coverage") is None))):
            raise ValueError("rolling fitting coverage is absent")
        for name in ("reason", "iterations_reason", "read_error", "solver_fraction_reason", "validation_fraction_reason"):
            if frame.get(name) is not None and not isinstance(frame.get(name), str):
                raise ValueError("native diagnostic reason differs")


def _record_process(binary: Path, argv: list[str], expected: Configuration, input_manifest: Mapping[str, object], output: Path,
                    *, current_cli: bool = False) -> Mapping[str, object]:
    """Run once after hashing every input and executable; reject malformed output."""
    if output.exists() or not binary.is_file() or binary.is_symlink():
        raise ValueError("native output exists or binary is unavailable")
    frames = input_manifest.get("frames")
    if not isinstance(frames, list) or len(frames) != expected.frames:
        raise ValueError("native input frame count differs")
    bound_frames = []
    for item in frames:
        if not isinstance(item, Mapping):
            raise ValueError("native input record differs")
        path, digest = _file_binding(item, "native input")
        bound_frames.append({"path": str(path), "sha256": digest})
    _exact_input_directory(bound_frames, "native")
    binary_digest = _digest(binary)
    output.mkdir(parents=True)
    argv_file = output / "argv.json"
    argv_file.write_text(json.dumps(argv) + "\n")
    completed = subprocess.run(argv, text=True, capture_output=True, check=False)
    stdout, stderr, exit_file = output / "stdout.txt", output / "stderr.txt", output / "native-exit.txt"
    stdout.write_text(completed.stdout); stderr.write_text(completed.stderr); exit_file.write_text(f"{completed.returncode}\n")
    try:
        result = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ValueError("native command did not emit JSON") from error
    if not isinstance(result, Mapping):
        raise ValueError("native command did not emit object")
    result = _normalize_cli_result(result, expected) if current_cli else result
    _valid_frame_record(result, expected)
    if completed.returncode != 0:
        frames = result.get("frames")
        if not current_cli or completed.returncode != 1 or not isinstance(frames, list):
            raise ValueError("native command failed")
        diverged = [frame for frame in frames if isinstance(frame, Mapping) and frame.get("status") == "diverged"]
        if not diverged or any(frame.get("read_error") is not None or not _matrix(frame.get("transform")) or frame.get("best_rho") is None or not math.isfinite(float(frame["best_rho"])) or "identityfallback" in str(frame.get("reason", "")).lower() for frame in diverged):
            raise ValueError("CLI strict failure lacks retained finite divergence diagnostics")
    outputs = result.get("outputs", [])
    if not current_cli:
        masks = result.get("masks")
        crop = result.get("crop")
        if (completed.returncode == 0 and (not isinstance(outputs, list) or not isinstance(masks, list)
                                           or len(outputs) != expected.frames or len(masks) != expected.frames)
                or not isinstance(crop, list) or len(crop) != 4
                or any(isinstance(value, bool) or not isinstance(value, int) or value < 0 for value in crop)
                or crop[2] <= 0 or crop[3] <= 0):
            raise ValueError("native warped output matrix differs")
        from .runner import gff
        output_bindings = []
        for image, mask in zip(outputs, masks):
            image_path = Path(str(image))
            mask_path = Path(str(mask))
            if not image_path.is_file() or not mask_path.is_file():
                raise ValueError("native warped output is absent")
            image_info, mask_info = gff.rawtiff.read_info(image_path), gff.rawtiff.read_info(mask_path)
            if (image_info.width, image_info.height, image_info.samples_per_pixel,
                    mask_info.width, mask_info.height, mask_info.samples_per_pixel) != (crop[2], crop[3], 3, crop[2], crop[3], 1):
                raise ValueError("native warped output schema differs")
            output_bindings.append({"image": {"path": str(image_path.resolve()), "sha256": _digest(image_path)},
                                    "mask": {"path": str(mask_path.resolve()), "sha256": _digest(mask_path)}})
    else:
        output_bindings = []
    if _digest(binary) != binary_digest:
        raise ValueError("binary changed during invocation")
    record = {"argv": {"path": str(argv_file.resolve()), "sha256": _digest(argv_file)},
              "stdout": {"path": str(stdout.resolve()), "sha256": _digest(stdout)},
              "stderr": {"path": str(stderr.resolve()), "sha256": _digest(stderr)},
              "native_exit": {"path": str(exit_file.resolve()), "sha256": _digest(exit_file), "value": completed.returncode},
              "binary": {"path": str(binary.resolve()), "sha256": binary_digest},
              "inputs": bound_frames, "outputs": output_bindings, "result": result}
    record_file = output / "record.json"
    record_file.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    return {**record, "record": {"path": str(record_file.resolve()), "sha256": _digest(record_file)}}


def _normalize_cli_result(result: Mapping[str, object], expected: Configuration) -> Mapping[str, object]:
    """Project the public ``align --json`` document into the spike schema."""
    if result.get("frame_count") != expected.frames or result.get("reference_policy") != expected.strategy:
        raise ValueError("current CLI parity identity differs")
    alignments = result.get("alignments")
    if not isinstance(alignments, list) or len(alignments) != expected.frames:
        raise ValueError("current CLI parity frame matrix differs")
    frames = []
    for index, item in enumerate(alignments):
        if not isinstance(item, Mapping) or item.get("frame") != index:
            raise ValueError("current CLI frame identity differs")
        transform = item.get("transform")
        if isinstance(transform, Mapping):
            transform = transform.get("m")
        if not _matrix(transform):
            raise ValueError("current CLI transform schema differs")
        iterations = item.get("iterations")
        if iterations is not None and (isinstance(iterations, bool) or not isinstance(iterations, int)):
            raise ValueError("current CLI iteration schema differs")
        frames.append({"frame": index, "status": item.get("status"), "reason": item.get("reason"),
                       "iterations": iterations, "iterations_reason": None if iterations is not None else "not-exposed-by-current-cli", "best_rho": item.get("residual"),
                       "transform": transform, "read_error": None, "solver_fraction": None,
                       "validation_fraction": None, "solver_fraction_reason": "not-exposed-by-current-cli",
                       "validation_fraction_reason": "not-exposed-by-current-cli", "interpolation_count": 0,
                       "fitting_template_coverage": None, "fitting_moving_coverage": None,
                       "fitting_window": None, "fitting_template_frame": None,
                       "fitting_prewarp": None, "fitting_reader": None})
    return {"strategy": expected.strategy, "reference": result.get("reference"), "crop": result.get("crop"),
            "reader_coverage": None, "frames": frames, "public_report": result}


def run_native_strategy(binary: Path, input_manifest: Mapping[str, object], strategy: str, reference: str,
                        output: Path, *, order: str = "none", inject_failure: tuple[int, str] | None = None) -> Mapping[str, object]:
    row = input_manifest.get("configuration")
    if not isinstance(row, Mapping):
        raise ValueError("native input lacks configuration")
    expected = Configuration(str(row["group"]), strategy, reference, int(row["frames"]), float(row["sigma"]), str(row["seed"]), row.get("lighting"), order if row.get("group") == "photometry" else None)
    input_rows = input_manifest.get("frames")
    if not isinstance(input_rows, list) or len(input_rows) != expected.frames:
        raise ValueError("native input frame count differs")
    bound_inputs = []
    for item in input_rows:
        path, digest = _file_binding(item, "native input")
        bound_inputs.append({"path": str(path), "sha256": digest})
    input_dir = _exact_input_directory(bound_inputs, "native")
    argv = spike_argv(binary, input_dir, strategy, reference)
    argv.extend(["--output", str((output / "warped").resolve())])
    if order not in ORDERS:
        raise ValueError("unknown input order")
    argv.extend(["--order", order])
    if inject_failure is not None:
        frame, leg = inject_failure
        if leg not in {"left", "right"} or not 0 <= frame < expected.frames:
            raise ValueError("injected failure differs")
        argv.extend(["--inject-failure", str(frame), "--leg", leg])
    if expected.group == "real":
        argv.append("--real")
    return _record_process(binary, argv, expected, input_manifest, output)


def _calibration_rows() -> tuple[Configuration, ...]:
    selected = calibration_configurations()
    priority = []
    for strategy in ("anchor", "chain"):
        priority.append(next(row for row in selected if row.group == "geometry" and row.strategy == strategy and row.reference == "middle" and row.frames == 8 and row.sigma == .45 and row.seed == _seed_text(TRAINING_SEEDS[0])))
    rest = tuple(row for row in selected if row not in priority)
    return tuple(priority) + rest


def stage_calls(stage: str) -> tuple[StageCall, ...]:
    if stage == "source":
        return ()
    if stage == "parity":
        row = next(item for item in configurations() if item.group == "geometry" and item.strategy == "anchor" and item.reference == "middle" and item.frames == 8 and item.sigma == .45 and item.seed == _seed_text(TRAINING_SEEDS[0]))
        chain = Configuration(row.group, "chain", row.reference, row.frames, row.sigma, row.seed)
        return tuple(StageCall(stage, i, choice, f"{choice.strategy}-cli-run-{run}") for i, (choice, run) in enumerate(((row, 1), (row, 2), (chain, 1), (chain, 2))))
    if stage == "calibration":
        selected = _calibration_rows()
        calls = tuple(StageCall(stage, i, row, f"CAL-{row.strategy}-{row.reference}-n{row.frames}-s{row.sigma:.2f}-seed{1 + [seed.text() for seed in TRAINING_SEEDS].index(row.seed)}-r{repeat}")
                      for i, (row, repeat) in enumerate((pair for row in selected for pair in ((row, 0), (row, 1)))))
    elif stage in {"quality", "repeat"}:
        selected = quality_configurations()
        calls = tuple(StageCall(stage, i, row, f"{stage.upper()}-{i:03d}") for i, row in enumerate(selected))
    elif stage == "controls":
        seed = _seed_text(TRAINING_SEEDS[0])
        calls = (StageCall(stage, 0, Configuration("geometry", "rolling", "first", 8, .45, seed), "CTRL-rolling-first-n8-s0.45-seed1-frame1-right"),
                 StageCall(stage, 1, Configuration("geometry", "rolling", "middle", 8, .45, seed), "CTRL-rolling-middle-n8-s0.45-seed1-frame2-left"))
    elif stage == "real":
        calls = tuple(StageCall(stage, i, Configuration("real", strategy, reference, 8, .45, "raw"), f"REAL-{strategy}-{reference}")
                      for i, (strategy, reference) in enumerate((strategy, reference) for strategy in STRATEGIES for reference in REFERENCES))
    else:
        raise ValueError("unknown stage")
    if len(calls) != stage_counts()[stage]:
        raise ValueError("registered stage count differs")
    return calls


def _metric_values(result: Mapping[str, object], truth: Mapping[str, object], row: Configuration) -> list[Mapping[str, object]]:
    """Compute immutable per-frame and stack geometry records from physical transforms."""
    import numpy as np
    transforms, truth_transforms = result.get("frames"), truth.get("reference")
    if not isinstance(transforms, list) or not isinstance(truth_transforms, list):
        raise ValueError("metric inputs are incomplete")
    rows: list[Mapping[str, object]] = []
    frame_errors: list[float | None] = []
    drift_points: list[tuple[float, float]] = []
    reference = 0 if row.reference == "first" else (row.frames - 1) // 2
    for frame, item in enumerate(transforms):
        status = item.get("status") if isinstance(item, Mapping) else None
        fitted = item.get("transform") if isinstance(item, Mapping) else None
        actual = truth_transforms[frame] if frame < len(truth_transforms) else None
        reason = None
        if fitted is None or actual is None or not _matrix(fitted) or not _matrix(actual):
            reason = "missing-or-nonfinite-transform"
            values = {"corner_rms": None, "corner_max": None, "drift_mean": None}
        else:
            values = corner_errors(fitted, actual, 512, 512)
        if frame == reference:
            values = {**values, "excluded": True}
        else:
            frame_errors.append(values.get("corner_rms"))
            if values.get("drift_mean") is not None:
                drift_points.append((float(abs(frame - reference)), _finite(values["drift_mean"])))
        for metric, value in values.items():
            if metric == "excluded":
                continue
            rows.append({"metric": metric, "frame_group": f"frame-{frame:02d}", "value": value, "reason": reason, "status": status})
    if any(value is None for value in frame_errors):
        stack = {"corner_rms": None, "corner_max": None, "drift_slope": None, "drift_intercept": None, "reason": "null-frame-hard-metric"}
    else:
        finite_errors = [float(value) for value in frame_errors]
        try:
            slope, intercept = ols_slope(drift_points)
        except ValueError:
            slope, intercept = None, None
        stack = {"corner_rms": math.sqrt(sum(value * value for value in finite_errors) / len(finite_errors)), "corner_max": max(
            _finite(next(item["value"] for item in rows if item["metric"] == "corner_max" and item["frame_group"] == f"frame-{frame:02d}"))
            for frame in range(row.frames) if frame != reference
        ), "drift_slope": slope, "drift_intercept": intercept, "reason": None if slope is not None else "insufficient-drift-hops"}
    rows.extend({"metric": metric, "frame_group": "stack", "value": value, "reason": stack["reason"]} for metric, value in stack.items() if metric not in {"reason", "drift_intercept"})
    rows.append({"metric": "drift_intercept", "frame_group": "stack", "value": stack["drift_intercept"], "reason": stack["reason"]})
    return rows


def _run_call(call: StageCall, root: Path, spike: Path, output: Path, source_cache: dict[tuple[object, ...], Mapping[str, object]],
              input_manifest: Mapping[str, object] | None = None) -> Mapping[str, object]:
    if call.configuration is None:
        raise ValueError("stage call lacks configuration")
    row = call.configuration
    source_key = (row.frames, row.sigma, row.seed, row.lighting, row.order)
    manifest = input_manifest or source_cache.get(source_key)
    if manifest is None:
        manifest = write_synthetic_input(row, output / "inputs" / f"input-{len(source_cache):03d}")
        source_cache[source_key] = manifest
    raw_key = manifest.get("configuration")
    if (not isinstance(raw_key, Mapping)
            or any(raw_key.get(name) != getattr(row, name) for name in ("group", "frames", "sigma", "seed", "lighting", "order"))):
        raise ValueError("registered call raw input differs")
    truth_path, _ = _file_binding(manifest.get("transforms"), "registered truth transforms")
    transforms = _load_object(truth_path, "truth transforms")
    middle_truth = transforms.get("middle")
    if not isinstance(middle_truth, list) or len(middle_truth) != row.frames:
        raise ValueError("middle truth transform matrix differs")
    import numpy as np
    reference_index = 0 if row.reference == "first" else (row.frames - 1) // 2
    try:
        reference_truth = np.linalg.inv(np.asarray(middle_truth[reference_index], dtype=np.float64))
    except np.linalg.LinAlgError as error:
        raise ValueError("requested truth reference is singular") from error
    requested_truth = [(np.asarray(matrix, dtype=np.float64) @ reference_truth).tolist() for matrix in middle_truth]
    transforms = {**transforms, "reference": requested_truth}
    inject = None
    if call.stage == "controls":
        inject = (1, "right") if call.ordinal == 0 else (2, "left")
    native = run_native_strategy(spike, manifest, row.strategy, row.reference, output / "calls" / f"{call.ordinal:03d}", order=row.order or "none", inject_failure=inject)
    metric_frames = manifest.get("metric_frames")
    if not isinstance(metric_frames, list) or len(metric_frames) != row.frames:
        raise ValueError("measured raw metric inputs differ")
    metric_input = {"configuration": manifest["configuration"], "frames": metric_frames}
    # F/M always resample the same measured raw u16 files.  The oracle and
    # gain-before choices affect only fitting, never the physical pixel score.
    fitted_resample = run_product_resample(spike, metric_input, [item.get("transform") for item in native["result"]["frames"]], output / "resample" / f"{call.ordinal:03d}" / "fitted")
    truth_resample = run_product_resample(spike, metric_input, transforms["reference"], output / "resample" / f"{call.ordinal:03d}" / "truth")
    metrics = [*_metric_values(native["result"], transforms, row), *physical_pixel_metrics(fitted_resample, truth_resample, row)]
    gain_after = _gain_after_control(native, output / "gain-after" / f"{call.ordinal:03d}") if row.group == "photometry" and row.strategy == "anchor" and row.order == "none" else None
    return {"call": {"ordinal": call.ordinal, "label": call.label, "key": call.key()}, "input": manifest["manifest"], "native": native["record"], "fitted_resample": fitted_resample["record"], "truth_resample": truth_resample["record"], "gain_after": gain_after, "metrics": metrics}


def _gain_after_control(native: Mapping[str, object], output: Path) -> Mapping[str, object]:
    """Record the no-fit gain-after control as an exact matrix byte reuse."""
    result = native.get("result")
    if not isinstance(result, Mapping) or not isinstance(result.get("frames"), list) or output.exists():
        raise ValueError("gain-after source result differs")
    matrices = [frame.get("transform") for frame in result["frames"] if isinstance(frame, Mapping)]
    if len(matrices) != len(result["frames"]):
        raise ValueError("gain-after source frames differ")
    gain_after_is_identical(matrices, matrices)
    encoded = json.dumps(matrices, separators=(",", ":"), allow_nan=False).encode()
    output.mkdir(parents=True)
    path = output / "reuse.json"
    path.write_text(json.dumps({"source_native": native.get("record"), "none_matrix_sha256": sha256(encoded).hexdigest(),
                                "gain_after_matrix_sha256": sha256(encoded).hexdigest(), "additional_fit": False}, indent=2, sort_keys=True) + "\n")
    return {"path": str(path.resolve()), "sha256": _digest(path)}


def _write_transform_csv(path: Path, matrices: Iterable[object]) -> None:
    rows = list(matrices)
    if not rows or any(not _matrix(matrix) for matrix in rows):
        raise ValueError("product resample transforms differ")
    path.parent.mkdir(parents=True, exist_ok=False)
    path.write_text("\n".join(
        ",".join([str(index), *(format(float(value), ".17g") for values in matrix for value in values)])
        for index, matrix in enumerate(rows)
    ) + "\n")


def run_product_resample(binary: Path, input_manifest: Mapping[str, object], matrices: Iterable[object], output: Path) -> Mapping[str, object]:
    """Run product crop/warp only; this path never invokes an aligner."""
    frames = input_manifest.get("frames")
    if not binary.is_file() or not isinstance(frames, list) or not frames or output.exists():
        raise ValueError("product resample inputs differ")
    bound_inputs = []
    for frame in frames:
        if not isinstance(frame, Mapping):
            raise ValueError("product resample input row differs")
        path, digest = _file_binding(frame, "product resample input")
        bound_inputs.append({"path": str(path), "sha256": digest})
    input_dir = _exact_input_directory(bound_inputs, "product resample")
    transforms = output / "transforms.csv"
    _write_transform_csv(transforms, matrices)
    argv = [str(binary), "--resample", "--input", str(input_dir), "--transforms", str(transforms), "--output", str(output / "pixels")]
    before = _digest(binary)
    completed = subprocess.run(argv, text=True, capture_output=True, check=False)
    stdout, stderr, exit_file = output / "stdout.txt", output / "stderr.txt", output / "native-exit.txt"
    stdout.write_text(completed.stdout); stderr.write_text(completed.stderr); exit_file.write_text(f"{completed.returncode}\n")
    if completed.returncode != 0 or _digest(binary) != before:
        raise ValueError("product resample failed or binary changed")
    try:
        result = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ValueError("product resample record differs") from error
    if not isinstance(result, Mapping) or not isinstance(result.get("outputs"), list) or not isinstance(result.get("masks"), list) or len(result["outputs"]) != len(frames) or len(result["masks"]) != len(frames):
        raise ValueError("product resample output matrix differs")
    output_rows = []
    for image, mask in zip(result["outputs"], result["masks"]):
        image_path, image_digest = _file_binding({"path": image, "sha256": _digest(Path(str(image)))}, "product resample image")
        mask_path, mask_digest = _file_binding({"path": mask, "sha256": _digest(Path(str(mask)))}, "product resample mask")
        output_rows.append({"image": {"path": str(image_path), "sha256": image_digest}, "mask": {"path": str(mask_path), "sha256": mask_digest}})
    argv_file = output / "argv.json"
    argv_file.write_text(json.dumps(argv) + "\n")
    record = {"argv": {"path": str(argv_file.resolve()), "sha256": _digest(argv_file)}, "binary": {"path": str(binary.resolve()), "sha256": before}, "inputs": bound_inputs, "transforms": {"path": str(transforms.resolve()), "sha256": _digest(transforms)},
              "stdout": {"path": str(stdout.resolve()), "sha256": _digest(stdout)}, "stderr": {"path": str(stderr.resolve()), "sha256": _digest(stderr)},
              "native_exit": {"path": str(exit_file.resolve()), "sha256": _digest(exit_file), "value": 0}, "outputs": output_rows, "result": result}
    record_file = output / "record.json"; record_file.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    return {**record, "record": {"path": str(record_file.resolve()), "sha256": _digest(record_file)}}


def physical_pixel_metrics(fitted: Mapping[str, object], truth: Mapping[str, object], row: Configuration,
                           *, propagated_first_support: Mapping[str, object] | None = None) -> list[Mapping[str, object]]:
    """Score RGB TIFFs and propagated kernel-valid masks emitted by product warp_tile."""
    import numpy as np
    from .runner import gff
    fitted_rows, truth_rows = fitted.get("outputs"), truth.get("outputs")
    if not isinstance(fitted_rows, list) or not isinstance(truth_rows, list) or len(fitted_rows) != row.frames or len(truth_rows) != row.frames:
        raise ValueError("product metric output matrix differs")
    per_frame, stack_rows = [], []
    gradient_numerator = 0.0
    gradient_denominator = 0
    truth_gradient_numerator = 0.0
    truth_gradient_denominator = 0
    stack_complete = True
    def rectangle(result: Mapping[str, object]) -> Rectangle | None:
        value = result.get("result", {}).get("crop") if isinstance(result.get("result"), Mapping) else None
        if not isinstance(value, list) or len(value) != 4 or any(isinstance(item, bool) or not isinstance(item, int) for item in value):
            return None
        return Rectangle(*value)
    overlap = stack_overlap_loss(rectangle(fitted), rectangle(truth))
    reference = 0 if row.reference == "first" else (row.frames - 1) // 2
    propagated_rows = None if propagated_first_support is None else propagated_first_support.get("outputs")
    if propagated_rows is not None and (not isinstance(propagated_rows, list) or len(propagated_rows) != row.frames):
        raise ValueError("propagated first-pass support differs")
    for index, (left, right) in enumerate(zip(fitted_rows, truth_rows)):
        images, masks = [], []
        for item in (left, right):
            image_path, _ = _file_binding(item.get("image"), "metric image")
            mask_path, _ = _file_binding(item.get("mask"), "metric mask")
            image_info, mask_info = gff.rawtiff.read_info(image_path), gff.rawtiff.read_info(mask_path)
            if (image_info.width, image_info.height, image_info.samples_per_pixel, mask_info.samples_per_pixel) != (mask_info.width, mask_info.height, 3, 1):
                raise ValueError("product metric image/mask geometry differs")
            images.append(np.frombuffer(gff.rawtiff.read_rect(image_path, 0, 0, image_info.width, image_info.height, image_info), dtype="<u2").reshape(image_info.height, image_info.width, 3))
            masks.append(np.frombuffer(gff.rawtiff.read_rect(mask_path, 0, 0, mask_info.width, mask_info.height, mask_info), dtype="<u2").reshape(mask_info.height, mask_info.width) != 0)
        fitted_crop, truth_crop = rectangle(fitted), rectangle(truth)
        if fitted_crop is None or truth_crop is None:
            values = (None, None, None, "different-product-crops")
        else:
            x0, y0 = max(fitted_crop.x, truth_crop.x), max(fitted_crop.y, truth_crop.y)
            x1, y1 = min(fitted_crop.x + fitted_crop.width, truth_crop.x + truth_crop.width), min(fitted_crop.y + fitted_crop.height, truth_crop.y + truth_crop.height)
            if x1 <= x0 or y1 <= y0:
                values = (None, None, None, "empty-common-product-coordinates")
                images = []
                masks = []
            else:
                def local(array: object, crop: Rectangle) -> object:
                    return array[y0 - crop.y:y1 - crop.y, x0 - crop.x:x1 - crop.x]
                images = [local(images[0], fitted_crop), local(images[1], truth_crop)]
                masks = [local(masks[0], fitted_crop), local(masks[1], truth_crop)]
                if propagated_rows is not None:
                    support_path, _ = _file_binding(propagated_rows[index].get("image"), "propagated first-pass support")
                    support_info = gff.rawtiff.read_info(support_path)
                    if (support_info.width, support_info.height, support_info.samples_per_pixel) != (fitted_crop.width, fitted_crop.height, 1):
                        raise ValueError("propagated first-pass support geometry differs")
                    propagated = np.frombuffer(gff.rawtiff.read_rect(support_path, 0, 0, support_info.width, support_info.height, support_info), dtype="<u2").reshape(support_info.height, support_info.width) != 0
                    masks[0] &= local(propagated, fitted_crop)
        if images and images[0].shape != images[1].shape:
            values = (None, None, None, "different-product-crops")
        elif images:
            mae, gradient = blur_metrics(images[0], images[1], masks[0] & masks[1])
            support = masks[0] & masks[1]
            if gradient is None:
                values = (mae, None, None, "empty-common-kernel-support")
            else:
                luma_left = images[0].astype(np.float64) @ np.array((0.2126, 0.7152, 0.0722))
                luma_right = images[1].astype(np.float64) @ np.array((0.2126, 0.7152, 0.0722))
                horizontal = support[:, :-1] & support[:, 1:]
                vertical = support[:-1, :] & support[1:, :]
                left_terms = []
                right_terms = []
                if horizontal.any():
                    left_terms.append((luma_left[:, 1:] - luma_left[:, :-1])[horizontal] ** 2)
                    right_terms.append((luma_right[:, 1:] - luma_right[:, :-1])[horizontal] ** 2)
                if vertical.any():
                    left_terms.append((luma_left[1:, :] - luma_left[:-1, :])[vertical] ** 2)
                    right_terms.append((luma_right[1:, :] - luma_right[:-1, :])[vertical] ** 2)
                if not left_terms:
                    values = (mae, None, None, "empty-common-kernel-support")
                else:
                    left_values = np.concatenate(left_terms)
                    right_values = np.concatenate(right_terms)
                    values = (mae, float(left_values.mean()), float(right_values.mean()), None)
        per_frame.extend(({"metric": "blur_mae", "frame_group": f"frame-{index:02d}", "value": values[0], "reason": values[3]},
                          {"metric": "gradient_energy", "frame_group": f"frame-{index:02d}", "value": values[1], "reason": values[3]},
                          {"metric": "truth_gradient_energy", "frame_group": f"frame-{index:02d}", "value": values[2], "reason": values[3]}))
        if index != reference:
            if values[0] is None or values[1] is None or values[2] is None:
                stack_complete = False
            else:
                stack_rows.append((images[0], images[1], masks[0] & masks[1]))
                horizontal = (masks[0] & masks[1])[:, :-1] & (masks[0] & masks[1])[:, 1:]
                vertical = (masks[0] & masks[1])[:-1, :] & (masks[0] & masks[1])[1:, :]
                left_luma = images[0].astype(np.float64) @ np.array((0.2126, 0.7152, 0.0722))
                right_luma = images[1].astype(np.float64) @ np.array((0.2126, 0.7152, 0.0722))
                if horizontal.any():
                    gradient_numerator += float(((left_luma[:, 1:] - left_luma[:, :-1])[horizontal] ** 2).sum())
                    truth_gradient_numerator += float(((right_luma[:, 1:] - right_luma[:, :-1])[horizontal] ** 2).sum())
                    gradient_denominator += int(horizontal.sum())
                    truth_gradient_denominator += int(horizontal.sum())
                if vertical.any():
                    gradient_numerator += float(((left_luma[1:, :] - left_luma[:-1, :])[vertical] ** 2).sum())
                    truth_gradient_numerator += float(((right_luma[1:, :] - right_luma[:-1, :])[vertical] ** 2).sum())
                    gradient_denominator += int(vertical.sum())
                    truth_gradient_denominator += int(vertical.sum())
    stack = stack_blur_mae(stack_rows) if stack_complete else None
    gradient_stack = gradient_numerator / gradient_denominator if stack_complete and gradient_denominator else None
    truth_gradient_stack = truth_gradient_numerator / truth_gradient_denominator if stack_complete and truth_gradient_denominator else None
    reason = None if stack is not None else "empty-or-null-frame-support"
    per_frame.append({"metric": "blur_mae", "frame_group": "stack", "value": stack, "reason": reason})
    per_frame.append({"metric": "gradient_energy", "frame_group": "stack", "value": gradient_stack, "reason": None if gradient_stack is not None else reason})
    per_frame.append({"metric": "truth_gradient_energy", "frame_group": "stack", "value": truth_gradient_stack, "reason": None if truth_gradient_stack is not None else reason})
    per_frame.append({"metric": "stack_overlap_loss", "frame_group": "stack", "value": overlap, "reason": None if overlap is not None else "missing-product-crop"})
    return per_frame


def _reference_canvas(first_pass: Mapping[str, object], source: Mapping[str, object], output: Path) -> Mapping[str, object]:
    """Place a product crop at its original reference coordinates without interpolation."""
    import numpy as np
    from .runner import gff
    if output.exists() or not isinstance(first_pass.get("outputs"), list) or not isinstance(source.get("frames"), list) or len(first_pass["outputs"]) != len(source["frames"]):
        raise ValueError("first-pass canvas inputs differ")
    crop = first_pass.get("result", {}).get("crop") if isinstance(first_pass.get("result"), Mapping) else None
    if not isinstance(crop, list) or len(crop) != 4 or any(isinstance(value, bool) or not isinstance(value, int) for value in crop):
        raise ValueError("first-pass canvas crop differs")
    x, y, width, height = crop
    rows = []
    (output / "images").mkdir(parents=True)
    (output / "masks").mkdir(parents=True)
    for index, (item, original) in enumerate(zip(first_pass["outputs"], source["frames"])):
        image_path, _ = _file_binding(item.get("image"), "first-pass image")
        mask_path, _ = _file_binding(item.get("mask"), "first-pass mask")
        raw_path, _ = _file_binding(original, "first-pass raw")
        image_info, mask_info, raw_info = (gff.rawtiff.read_info(path) for path in (image_path, mask_path, raw_path))
        if (image_info.width, image_info.height, image_info.samples_per_pixel) != (width, height, 3) or (mask_info.width, mask_info.height, mask_info.samples_per_pixel) != (width, height, 1):
            raise ValueError("first-pass crop pixels differ")
        if x < 0 or y < 0 or x + width > raw_info.width or y + height > raw_info.height or raw_info.samples_per_pixel != 3:
            raise ValueError("first-pass crop leaves reference canvas")
        image = np.frombuffer(gff.rawtiff.read_rect(image_path, 0, 0, width, height, image_info), dtype="<u2").reshape(height, width, 3)
        mask = np.frombuffer(gff.rawtiff.read_rect(mask_path, 0, 0, width, height, mask_info), dtype="<u2").reshape(height, width)
        canvas = np.zeros((raw_info.height, raw_info.width, 3), dtype="<u2")
        support = np.zeros((raw_info.height, raw_info.width), dtype="<u2")
        canvas[y:y + height, x:x + width] = image
        support[y:y + height, x:x + width] = mask
        image_target, mask_target = output / "images" / f"{index:02d}.tif", output / "masks" / f"{index:02d}.tif"
        gff.rawtiff.write_tiff(image_target, raw_info.width, raw_info.height, canvas.tobytes(), bits=16, spp=3)
        gff.rawtiff.write_tiff(mask_target, raw_info.width, raw_info.height, support.tobytes(), bits=16, spp=1)
        rows.append({"image": {"path": str(image_target.resolve()), "sha256": _digest(image_target)},
                     "mask": {"path": str(mask_target.resolve()), "sha256": _digest(mask_target)}})
    return {"crop": crop, "outputs": rows}


def _image_only_input(result: Mapping[str, object], configuration: Mapping[str, object], output: Path) -> Mapping[str, object]:
    """Copy selected RGB TIFFs into a directory containing no coverage masks."""
    import shutil
    rows = result.get("outputs")
    if output.exists() or not isinstance(rows, list) or not rows:
        raise ValueError("image-only input differs")
    output.mkdir(parents=True)
    frames = []
    for index, row in enumerate(rows):
        source, digest = _file_binding(row.get("image"), "selected product image")
        target = output / f"{index:02d}.tif"
        shutil.copyfile(source, target)
        if _digest(target) != digest:
            raise ValueError("selected product image copy differs")
        frames.append({"path": str(target.resolve()), "sha256": digest})
    return {"configuration": configuration, "frames": frames}


def _all_tap_first_support(canvas: Mapping[str, object], transforms: Iterable[object], resampled: Mapping[str, object], output: Path) -> Mapping[str, object]:
    """Derive all-nonzero-Catmull-Rom-tap validity beside product pixel output.

    The product's bitmap reports centre geometry only.  This separate support
    control follows its documented f64 map/f32 fraction/edge-clamped 4x4
    kernel and never substitutes for the product RGB resample.
    """
    import numpy as np
    from .runner import gff
    rows, matrices = canvas.get("outputs"), list(transforms)
    crop = resampled.get("result", {}).get("crop") if isinstance(resampled.get("result"), Mapping) else None
    if output.exists() or not isinstance(rows, list) or len(rows) != len(matrices) or not isinstance(crop, list) or len(crop) != 4:
        raise ValueError("all-tap support inputs differ")
    x0, y0, width, height = crop
    if any(isinstance(value, bool) or not isinstance(value, int) for value in crop) or width <= 0 or height <= 0:
        raise ValueError("all-tap support crop differs")
    output.mkdir(parents=True)
    emitted = []
    for index, (item, matrix) in enumerate(zip(rows, matrices)):
        if not _matrix(matrix):
            raise ValueError("all-tap support transform differs")
        mask_path, _ = _file_binding(item.get("mask"), "first-pass canvas mask")
        info = gff.rawtiff.read_info(mask_path)
        if info.samples_per_pixel != 1:
            raise ValueError("first-pass canvas mask samples differ")
        source = np.frombuffer(gff.rawtiff.read_rect(mask_path, 0, 0, info.width, info.height, info), dtype="<u2").reshape(info.height, info.width) != 0
        matrix64 = np.asarray(matrix, dtype=np.float64)
        dy, dx = np.indices((height, width), dtype=np.float64)
        x, y = dx + x0, dy + y0
        w = matrix64[2, 0] * x + matrix64[2, 1] * y + matrix64[2, 2]
        sx = (matrix64[0, 0] * x + matrix64[0, 1] * y + matrix64[0, 2]) / w
        sy = (matrix64[1, 0] * x + matrix64[1, 1] * y + matrix64[1, 2]) / w
        valid = np.isfinite(w) & (w > 1e-6) & np.isfinite(sx) & np.isfinite(sy) & (sx >= 0.0) & (sx <= info.width - 1) & (sy >= 0.0) & (sy <= info.height - 1)
        fx, fy = np.floor(sx).astype(np.int64), np.floor(sy).astype(np.int64)
        tx, ty = (sx - fx).astype(np.float32), (sy - fy).astype(np.float32)
        def weights(value: object) -> tuple[object, object, object, object]:
            return (value * (value * (-np.float32(.5) * value + np.float32(1.0)) - np.float32(.5)),
                    value * value * (np.float32(1.5) * value - np.float32(2.5)) + np.float32(1.0),
                    value * (value * (-np.float32(1.5) * value + np.float32(2.0)) + np.float32(.5)),
                    value * value * (np.float32(.5) * value - np.float32(.5)))
        wx, wy = weights(tx), weights(ty)
        for oy, weight_y in enumerate(wy):
            for ox, weight_x in enumerate(wx):
                active = (weight_x != np.float32(0.0)) & (weight_y != np.float32(0.0))
                px = np.clip(fx + ox - 1, 0, info.width - 1)
                py = np.clip(fy + oy - 1, 0, info.height - 1)
                valid &= (~active) | source[py, px]
        valid = valid.astype(np.uint16) * np.uint16(65535)
        target = output / f"{index:02d}.tif"
        gff.rawtiff.write_tiff(target, width, height, valid.tobytes(), bits=16, spp=1)
        emitted.append({"image": {"path": str(target.resolve()), "sha256": _digest(target)}})
    return {"crop": crop, "outputs": emitted, "kernel": "catmull-rom-a=-.5-f64-map-f32-fraction-all-nonzero-taps"}


def _detector_geometry_metrics(fitted: Iterable[object], truth: Iterable[object], reference: int) -> Mapping[str, float]:
    """Score fixed physical detector matrices at the registered four corners."""
    fitted_rows, truth_rows = list(fitted), list(truth)
    if len(fitted_rows) != len(truth_rows) or not (0 <= reference < len(fitted_rows)):
        raise ValueError("detector matrix identity differs")
    errors = [corner_errors(left, right) for left, right in zip(fitted_rows, truth_rows)]
    nonreference = [(index, value) for index, value in enumerate(errors) if index != reference]
    if not nonreference:
        raise ValueError("detector has no nonreference frame")
    rms = math.sqrt(sum(_finite(value["corner_rms"]) ** 2 for _, value in nonreference) / len(nonreference))
    maximum = max(_finite(value["corner_max"]) for _, value in nonreference)
    slope, intercept = ols_slope((abs(index - reference), _finite(value["drift_mean"])) for index, value in nonreference)
    return {"corner_rms": rms, "corner_max": maximum, "drift_slope": slope, "drift_intercept": intercept}


def _required_stack_metric(rows: Iterable[Mapping[str, object]], metric: str) -> float:
    value = _stack_metric(rows, metric)
    if value is None:
        raise ValueError(f"detector {metric} is unavailable")
    return _finite(value)


def run_detector_controls(spike: Path, output: Path) -> Mapping[str, object]:
    """Run fit-free product-resampler instruments over every frozen control key.

    The only native work here is ``--resample``.  It consumes no alignment
    call and retains clean, constant-offset, hop-drift, and round-trip pixels
    with the product kernel-valid masks for independent review.
    """
    import numpy as np
    if output.exists():
        raise ValueError("detector output already exists")
    output.mkdir(parents=True)
    seed = _seed_text(TRAINING_SEEDS[0])
    records = []
    ordinal = 0
    for reference in REFERENCES:
        for frames in FRAME_COUNTS:
            for sigma in SIGMAS:
                row = Configuration("geometry", "anchor", reference, frames, sigma, seed)
                manifest = write_synthetic_input(row, output / "inputs" / f"{ordinal:02d}")
                truth = _load_object(Path(str(manifest["transforms"]["path"])), "detector truth")
                matrices = truth["reference"]
                if not isinstance(matrices, list) or len(matrices) != frames:
                    raise ValueError("detector truth matrix differs")
                identity = [[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] for _ in range(frames)]
                call_root = output / "calls" / f"{ordinal:02d}"
                identity_result = run_product_resample(spike, manifest, identity, call_root / "identity-one")
                identity_second = run_product_resample(spike, manifest, identity, call_root / "identity-two")
                identity_one_canvas = _reference_canvas(identity_result, manifest, call_root / "identity-one-canvas")
                identity_two_canvas = _reference_canvas(identity_second, manifest, call_root / "identity-two-canvas")
                identity_one_input = _image_only_input(identity_one_canvas, manifest["configuration"], call_root / "identity-one-input")
                identity_two_input = _image_only_input(identity_two_canvas, manifest["configuration"], call_root / "identity-two-input")
                clean_identity_one = run_product_resample(spike, identity_one_input, identity, call_root / "identity-roundtrip-one")
                clean_identity_two = run_product_resample(spike, identity_two_input, identity, call_root / "identity-roundtrip-two")
                clean_one = run_product_resample(spike, manifest, matrices, output / "calls" / f"{ordinal:02d}" / "clean-one")
                clean_two = run_product_resample(spike, manifest, matrices, output / "calls" / f"{ordinal:02d}" / "clean-two")
                constant = [detector_transform(matrix, .5) for matrix in matrices]
                ref_index = 0 if reference == "first" else (frames - 1) // 2
                maximum = max(1, ref_index, frames - 1 - ref_index)
                hop = [hop_detector_transform(matrix, index, ref_index, maximum) for index, matrix in enumerate(matrices)]
                constant_result = run_product_resample(spike, manifest, constant, output / "calls" / f"{ordinal:02d}" / "constant")
                hop_result = run_product_resample(spike, manifest, hop, output / "calls" / f"{ordinal:02d}" / "hop")
                origin = clean_one["result"].get("crop")
                if not isinstance(origin, list) or len(origin) != 4:
                    raise ValueError("roundtrip first-pass crop is absent")
                # ``clean_canvas`` restores the first crop at absolute raw
                # reference coordinates.  Its second pass therefore uses
                # plain inverse(M); crop-origin conjugation applies only to
                # a local crop coordinate system and would shift this canvas.
                inverse = [np.linalg.inv(np.asarray(matrix, dtype=np.float64)).tolist() for matrix in matrices]
                clean_canvas = _reference_canvas(clean_one, manifest, call_root / "clean-one-canvas")
                roundtrip_input = _image_only_input(clean_canvas, manifest["configuration"], call_root / "roundtrip-input")
                roundtrip = run_product_resample(spike, roundtrip_input, inverse, output / "calls" / f"{ordinal:02d}" / "roundtrip")
                support_input = {"configuration": manifest["configuration"], "frames": [{"path": item["mask"]["path"], "sha256": item["mask"]["sha256"]} for item in clean_canvas["outputs"]]}
                propagated_support = run_product_resample(spike, support_input, inverse, output / "calls" / f"{ordinal:02d}" / "roundtrip-support-diagnostic")
                exact_first_support = _all_tap_first_support(clean_canvas, inverse, roundtrip, call_root / "roundtrip-all-tap-support")
                clean_metrics = physical_pixel_metrics(clean_one, clean_two, row)
                constant_metrics = physical_pixel_metrics(constant_result, clean_one, row)
                hop_metrics = physical_pixel_metrics(hop_result, clean_one, row)
                clean_roundtrip_one = physical_pixel_metrics(clean_identity_one, identity_result, row)
                clean_roundtrip_two = physical_pixel_metrics(clean_identity_two, identity_second, row)
                roundtrip_metrics = physical_pixel_metrics(roundtrip, identity_result, row, propagated_first_support=exact_first_support)
                clean_geometry = _detector_geometry_metrics(matrices, matrices, ref_index)
                constant_geometry = _detector_geometry_metrics(constant, matrices, ref_index)
                hop_geometry = _detector_geometry_metrics(hop, matrices, ref_index)
                constant_allowance = instrument_allowance(clean_geometry["corner_rms"], clean_geometry["corner_rms"])
                hop_allowance = instrument_allowance(clean_geometry["drift_slope"], clean_geometry["drift_slope"])
                roundtrip_allowance = instrument_allowance(_required_stack_metric(clean_roundtrip_one, "blur_mae"), _required_stack_metric(clean_roundtrip_two, "blur_mae"))
                gradient_allowance = instrument_allowance(_required_stack_metric(clean_roundtrip_one, "gradient_energy"), _required_stack_metric(clean_roundtrip_two, "gradient_energy"))
                constant_detected = detector_detected(clean_geometry["corner_rms"], clean_geometry["corner_rms"], constant_geometry["corner_rms"])
                hop_detected = detector_detected(clean_geometry["drift_slope"], clean_geometry["drift_slope"], hop_geometry["drift_slope"])
                roundtrip_mae = _required_stack_metric(roundtrip_metrics, "blur_mae")
                roundtrip_raw_gradient = _required_stack_metric(roundtrip_metrics, "truth_gradient_energy")
                roundtrip_gradient = _required_stack_metric(roundtrip_metrics, "gradient_energy")
                roundtrip_detected = (roundtrip_mae > roundtrip_allowance or roundtrip_raw_gradient - roundtrip_gradient > gradient_allowance)
                if not constant_detected or not hop_detected or not roundtrip_detected:
                    raise ValueError("physical detector did not observe its registered positive control")
                records.append({"key": {"reference": reference, "frames": frames, "sigma": sigma, "seed": seed},
                                "identity": identity_result["record"], "identity_canvas": identity_one_canvas, "identity_second": identity_second["record"], "identity_second_canvas": identity_two_canvas,
                                "clean_identity_one": clean_identity_one["record"], "clean_identity_two": clean_identity_two["record"],
                                "clean_one": clean_one["record"], "clean_one_canvas": clean_canvas, "clean_two": clean_two["record"], "constant": constant_result["record"], "hop": hop_result["record"], "roundtrip": roundtrip["record"], "roundtrip_support_diagnostic": propagated_support["record"], "roundtrip_all_tap_support": exact_first_support,
                                "clean_metrics": clean_metrics, "constant_metrics": constant_metrics, "hop_metrics": hop_metrics,
                                "clean_roundtrip_one": clean_roundtrip_one, "clean_roundtrip_two": clean_roundtrip_two, "roundtrip_metrics": roundtrip_metrics,
                                "geometry": {"clean": clean_geometry, "constant": constant_geometry, "hop": hop_geometry},
                                "instrument_allowance": {"constant_corner_rms": constant_allowance, "hop_drift_slope": hop_allowance,
                                                         "roundtrip_blur_mae": roundtrip_allowance, "roundtrip_gradient_energy": gradient_allowance},
                                "detected": {"constant": constant_detected, "hop": hop_detected, "roundtrip": roundtrip_detected}})
                ordinal += 1
    if len(records) != 8:
        raise ValueError("detector control matrix differs")
    destination = output / "detectors.json"; destination.write_text(json.dumps({"records": records}, indent=2, sort_keys=True) + "\n")
    return {"path": str(destination.resolve()), "sha256": _digest(destination), "records": len(records)}


def _stack_metric(rows: Iterable[Mapping[str, object]], name: str, default: float | None = None) -> float | None:
    for row in rows:
        if row.get("metric") == name and row.get("frame_group") == "stack" and row.get("value") is not None:
            return _finite(row["value"])
    return default


def execute_registered_stage(stage: str, registration: Path, source_proof: Path | None, allowance_proof: Path | None,
                             parity_proof: Path | None, root: Path, spike: Path, cli: Path, output: Path,
                             *, source_proof_sha256: str | None = None, parity_proof_sha256: str | None = None,
                             allowance_proof_sha256: str | None = None, quality_digest: str | None = None,
                             real_manifest: Path | None = None, calibration_records: Path | None = None,
                             calibration_records_sha256: str | None = None, quality_records: Path | None = None) -> Mapping[str, object]:
    """Execute exactly one admitted, finite stage and retain its complete graph.

    This function is intentionally the only stage runner.  It never accepts a
    caller-provided result list, and it writes no readiness envelope.
    """
    root = root.resolve()
    admission = validate_admission(stage, registration, source_proof, allowance_proof, parity_proof, root=root,
                                  source_expected=source_proof_sha256, parity_expected=parity_proof_sha256,
                                  allowance_expected=allowance_proof_sha256)
    if stage != "source":
        _bind_admitted_executable(admission["source"], "focusforge-cli", cli)
        _bind_admitted_executable(admission["source"], "alignment-reference-strategies", spike)
    if output.exists():
        raise ValueError("stage output already exists")
    if stage == "source":
        registration_digest = _digest(registration)
        graph = snapshot_source_graph(root, output / "snapshot", {"focusforge-cli": cli, "alignment-reference-strategies": spike})
        detectors = run_detector_controls(spike, output / "detectors")
        if _digest(registration) != registration_digest:
            raise ValueError("registration changed during source controls")
        if tuple({"path": item.path, "sha256": item.sha256} for item in recursive_source_closure(root)) != tuple({"path": item["path"], "sha256": item["sha256"]} for item in graph["closure"]):
            raise ValueError("source closure changed during controls")
        for name, binary in (("focusforge-cli", cli), ("alignment-reference-strategies", spike)):
            if _digest(binary) != graph["executables"][name]["sha256"]:
                raise ValueError("source executable changed during controls")
        artifacts = _write_source_artifacts(output, registration, graph, detectors)
        result = {"stage": stage, "admission": admission, "source_graph": graph, "artifacts": artifacts, "accepted": False,
                  "reason": "independent source proof required"}
        write_stage_output(output / "stage.json", result)
        return result
    if stage == "repeat":
        if not _sha(quality_digest) or quality_records is None or not quality_records.is_file() or _digest(quality_records) != quality_digest:
            raise ValueError("repeat requires the externally pinned immutable QUALITY record")
        quality = _load_object(quality_records, "immutable quality record")
        if quality.get("stage") != "quality" or not isinstance(quality.get("records"), list) or len(quality["records"]) != 140:
            raise ValueError("immutable quality record differs")
    if stage == "real":
        if real_manifest is None:
            raise ValueError("real requires original raw manifest")
        raw = _load_object(real_manifest, "real raw manifest")
        _validate_real_raw(raw)
        raw_manifest_binding = {"path": str(real_manifest.resolve()), "sha256": _digest(real_manifest)}
    output.mkdir(parents=True)
    calls = stage_calls(stage)
    if stage == "parity":
        # CLI4 is recorded before CAL200.  The later independent parity audit
        # calls ``verify_parity_records`` against these immutable CLI bytes.
        records = _execute_parity(calls, cli, output)
    elif stage == "real":
        records = _execute_real(calls, root, spike, output, raw, raw_manifest_binding)
    elif stage == "repeat":
        quality_rows = quality["records"]
        by_key = {json.dumps(item.get("call", {}).get("key"), sort_keys=True): item for item in quality_rows if isinstance(item, Mapping)}
        cache = {}
        records = []
        for call in calls:
            item = by_key.get(json.dumps(call.key(), sort_keys=True))
            if not isinstance(item, Mapping):
                raise ValueError("repeat QUALITY key is absent")
            input_path, _ = _file_binding(item.get("input"), "repeat QUALITY input")
            replay_manifest = _load_object(input_path, "repeat QUALITY input")
            records.append(_run_call(call, root, spike, output, cache, {**replay_manifest, "manifest": item["input"]}))
    else:
        cache: dict[tuple[object, ...], Mapping[str, object]] = {}
        records = [_run_call(call, root, spike, output, cache) for call in calls]
    if len(records) != stage_counts()[stage]:
        raise ValueError("executor call count differs")
    final_admission = validate_admission(stage, registration, source_proof, allowance_proof, parity_proof, root=root,
                                        source_expected=source_proof_sha256, parity_expected=parity_proof_sha256,
                                        allowance_expected=allowance_proof_sha256)
    _bind_admitted_executable(final_admission["source"], "focusforge-cli", cli)
    _bind_admitted_executable(final_admission["source"], "alignment-reference-strategies", spike)
    if stage == "repeat":
        verify_repeat({**quality, "sha256": quality_digest}, {"records": records}, str(quality_digest))
    allowances = _admitted_allowances(admission) if stage in {"quality", "repeat"} else None
    verdict = assess_stage(stage, records, quality_digest=quality_digest, allowances=allowances)
    result = {"stage": stage, "admission": admission, "plan": stage_plan(stage), "records": records,
              "verdict": verdict, "measurement_complete": verdict["state"] != "DEFER", "accepted": False}
    if stage == "calibration":
        result["calibration_tables"] = calibration_tables(records)
    write_stage_output(output / "stage.json", result)
    return result


def _admitted_allowances(admission: Mapping[str, object]) -> list[Mapping[str, object]]:
    proof = admission.get("allowance")
    if not isinstance(proof, Mapping) or not isinstance(proof.get("envelope"), Mapping):
        raise ValueError("allowance proof is absent")
    path, _ = _file_binding(proof["envelope"].get("allowances"), "admitted allowance table")
    document = _load_object(path, "admitted allowance table")
    rows = document.get("allowances")
    if not isinstance(rows, list):
        raise ValueError("admitted allowance rows are absent")
    _validate_allowance_document(document)
    return [row for row in rows if isinstance(row, Mapping)]


def _bind_admitted_executable(source: object, name: str, supplied: Path) -> None:
    """Refuse a child binary other than the source-ready ELF binding."""
    if not isinstance(source, Mapping) or not isinstance(source.get("envelope"), Mapping):
        raise ValueError("admitted source proof is absent")
    artifacts_path, _ = _file_binding(source["envelope"].get("artifacts"), "admitted source artifacts")
    artifacts = _load_object(artifacts_path, "admitted source artifacts")
    executables_path, _ = _file_binding(artifacts.get("executables"), "admitted executables")
    executables = _load_object(executables_path, "admitted executables").get("executables")
    if not isinstance(executables, Mapping) or not isinstance(executables.get(name), Mapping):
        raise ValueError("admitted executable binding is absent")
    bound, digest = _file_binding(executables[name], "admitted executable")
    if not supplied.is_file() or supplied.resolve() != bound or _digest(supplied) != digest:
        raise ValueError("stage executable differs from source-ready ELF")


def _write_source_artifacts(output: Path, registration: Path, graph: Mapping[str, object], detectors: Mapping[str, object]) -> Mapping[str, object]:
    """Write candidate SOURCE artifacts only; no ready envelope is created here."""
    snapshot = output / "snapshot"
    source_entries = []
    for item in graph["closure"]:
        assert isinstance(item, Mapping)
        copied = snapshot / "source" / str(item["path"])
        source_entries.append({"path": item["path"], "sha256": item["sha256"],
                               "snapshot": {"path": str(copied.resolve()), "sha256": _digest(copied)}})
    closure = output / "source-closure.json"
    closure.write_text(json.dumps({"entries": source_entries}, indent=2, sort_keys=True) + "\n")
    executable_rows = {}
    for name, item in graph["executables"].items():
        assert isinstance(item, Mapping)
        executable_rows[name] = {"path": item["path"], "sha256": item["sha256"],
                                 "snapshot": {"path": item["snapshot"], "sha256": item["snapshot_sha256"]},
                                 "bytes": Path(str(item["path"])).stat().st_size, "format": "ELF"}
    executables = output / "executables.json"
    executables.write_text(json.dumps({"executables": executable_rows}, indent=2, sort_keys=True) + "\n")
    controls = output / "controls.json"
    checks = self_checks()
    controls.write_text(json.dumps({"fit_free": True, "checks": checks, "detectors": detectors, "passed": True}, indent=2, sort_keys=True) + "\n")
    document = {"schema": "alignment-reference-source-artifacts-v1",
                "registration": {"path": str(registration.resolve()), "sha256": _digest(registration)},
                "source_closure": {"path": str(closure.resolve()), "sha256": _digest(closure)},
                "executables": {"path": str(executables.resolve()), "sha256": _digest(executables)},
                "controls": {"path": str(controls.resolve()), "sha256": _digest(controls)}}
    artifact = output / "source-artifacts.json"
    artifact.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    return {"path": str(artifact.resolve()), "sha256": _digest(artifact)}


def _execute_parity(calls: tuple[StageCall, ...], cli: Path, output: Path) -> list[Mapping[str, object]]:
    """Run exactly the four current-CLI diagnostics before CAL200."""
    rows: list[Mapping[str, object]] = []
    manifest: Mapping[str, object] | None = None
    for call in calls:
        if call.configuration is None:
            raise ValueError("parity call lacks configuration")
        if manifest is None:
            manifest = write_synthetic_input(call.configuration, output / "inputs" / f"{call.ordinal:03d}")
        assert manifest is not None
        product = _record_process(cli, product_cli_argv(cli, Path(str(manifest["frames"][0]["path"])).parent, call.configuration.strategy, call.configuration.reference), call.configuration, manifest, output / "cli" / f"{call.ordinal:03d}", current_cli=True)
        rows.append({"call": {"ordinal": call.ordinal, "label": call.label, "key": call.key()}, "input": manifest["manifest"], "cli": product["record"]})
    return rows


def verify_parity_records(parity_records: Path, parity_sha256: str, calibration_records: Path, calibration_sha256: str) -> Mapping[str, object]:
    """Fit-free, externally pinned CLI4/CAL200 comparison for the audit step."""
    if not all((_sha(parity_sha256), _sha(calibration_sha256), parity_records.is_file(), calibration_records.is_file())):
        raise ValueError("parity audit bindings are absent")
    if _digest(parity_records) != parity_sha256 or _digest(calibration_records) != calibration_sha256:
        raise ValueError("parity audit binding differs")
    parity, calibration = _load_object(parity_records, "parity records"), _load_object(calibration_records, "CAL200 records")
    records, calibration_rows = parity.get("records"), calibration.get("records")
    if parity.get("stage") != "parity" or calibration.get("stage") != "calibration" or not isinstance(records, list) or not isinstance(calibration_rows, list):
        raise ValueError("parity audit stage graph differs")
    comparisons = _assess_parity(records, calibration_rows)
    return {"parity": {"path": str(parity_records.resolve()), "sha256": parity_sha256},
            "calibration": {"path": str(calibration_records.resolve()), "sha256": calibration_sha256}, "comparisons": comparisons, "passed": True}


def _validate_real_raw(raw: Mapping[str, object], *, header_reader: object | None = None) -> None:
    if raw.get("dimensions") != list(REAL_DIMENSIONS) or raw.get("indices") != list(REAL_FRAME_INDICES):
        raise ValueError("real raw dimensions or indices differ")
    frames = raw.get("frames")
    if not isinstance(frames, list) or len(frames) != 8:
        raise ValueError("real raw frame list differs")
    references = raw.get("references")
    roi = raw.get("roi")
    if references != {"first": 0, "middle": 3} or not isinstance(roi, Mapping):
        raise ValueError("real reference groups or ROI differ")
    if roi != REAL_ROI:
        raise ValueError("real ROI differs from the frozen reference-local rectangle")
    try:
        roi_values = tuple(roi[name] for name in ("x", "y", "width", "height"))
    except (KeyError, TypeError) as error:
        raise ValueError("real ROI differs") from error
    if any(isinstance(value, bool) or not isinstance(value, int) for value in roi_values):
        raise ValueError("real ROI coordinate type differs")
    if roi_values[2] <= 0 or roi_values[3] <= 0 or roi_values[0] < 0 or roi_values[1] < 0 or roi_values[0] + roi_values[2] > REAL_DIMENSIONS[0] or roi_values[1] + roi_values[3] > REAL_DIMENSIONS[1]:
        raise ValueError("real ROI is outside the reference coordinates")
    if header_reader is None:
        from .runner import gff
        header_reader = gff.rawtiff.read_info
    if not callable(header_reader):
        raise ValueError("real raw header reader differs")
    paths_seen: set[str] = set()
    digests_seen: set[str] = set()
    for ordinal, frame in enumerate(frames):
        if not isinstance(frame, Mapping) or frame.get("index") != REAL_FRAME_INDICES[ordinal]:
            raise ValueError("real raw index order differs")
        path, digest = _file_binding(frame, "real raw")
        if str(path) in paths_seen or digest in digests_seen:
            raise ValueError("real raw source identity is duplicated")
        paths_seen.add(str(path))
        digests_seen.add(digest)
        if path.suffix.lower() not in {".tif", ".tiff"}:
            raise ValueError("real raw is not TIFF")
        info = header_reader(path)
        if (info.width, info.height, info.samples_per_pixel, info.bits_per_sample) != (4781, 3136, 3, (16, 16, 16)):
            raise ValueError("real raw header differs")


def _execute_real(calls: tuple[StageCall, ...], root: Path, spike: Path, output: Path, raw: Mapping[str, object], original_manifest: Mapping[str, str]) -> list[Mapping[str, object]]:
    # A native input directory is made only from the eight authenticated raw
    # selections; result acceptance separately rejects failed status/crop.
    raw_dir = output / "raw"
    raw_dir.mkdir(parents=True)
    import shutil
    frames = []
    for index, entry in enumerate(raw["frames"]):
        source, digest = _file_binding(entry, "real raw")
        target = raw_dir / f"{index:02d}{source.suffix.lower()}"
        shutil.copyfile(source, target)
        if _digest(target) != digest:
            raise ValueError("real raw copy differs")
        frames.append({"path": str(target.resolve()), "sha256": digest})
    manifest = {"configuration": {"group": "real", "frames": 8, "sigma": .45, "seed": "raw"}, "frames": frames}
    rows = []
    for call in calls:
        assert call.configuration is not None
        record = run_native_strategy(spike, manifest | {"configuration": asdict(call.configuration)}, call.configuration.strategy, call.configuration.reference, output / "calls" / f"{call.ordinal:03d}")
        native = record["result"]
        if any(frame.get("status") not in {"converged", "hit-cap", "stalled"} for frame in native["frames"]) or native.get("crop") is None:
            raise ValueError("failed real fit cannot prepare crop")
        coverage = native.get("reader_coverage")
        if not isinstance(coverage, list) or len(coverage) != 8 or any(isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or float(value) != 1.0 for value in coverage):
            raise ValueError("real crop coverage differs")
        outputs = native.get("outputs")
        masks = native.get("masks")
        crop = native.get("crop")
        if not isinstance(outputs, list) or not isinstance(masks, list) or len(outputs) != 8 or len(masks) != 8 or not isinstance(crop, list) or len(crop) != 4 or any(isinstance(value, bool) or not isinstance(value, int) for value in crop):
            raise ValueError("real resampled output matrix differs")
        from .runner import gff
        crop_x, crop_y, crop_width, crop_height = crop
        if not (crop_x <= REAL_ROI["x"] and crop_y <= REAL_ROI["y"] and REAL_ROI["x"] + REAL_ROI["width"] <= crop_x + crop_width and REAL_ROI["y"] + REAL_ROI["height"] <= crop_y + crop_height):
            raise ValueError("reference-local real ROI is outside aligned crop")
        local_x, local_y = REAL_ROI["x"] - crop_x, REAL_ROI["y"] - crop_y
        roi_frames = []
        aligned_outputs = []
        for index, (image, mask) in enumerate(zip(outputs, masks)):
            image_path, image_digest = _file_binding({"path": image, "sha256": _digest(Path(str(image)))}, "aligned real output")
            mask_path, _ = _file_binding({"path": mask, "sha256": _digest(Path(str(mask)))}, "aligned real mask")
            image_info, mask_info = gff.rawtiff.read_info(image_path), gff.rawtiff.read_info(mask_path)
            if (image_info.width, image_info.height, image_info.samples_per_pixel, mask_info.width, mask_info.height, mask_info.samples_per_pixel) != (crop_width, crop_height, 3, crop_width, crop_height, 1):
                raise ValueError("aligned real output geometry differs")
            support = gff.rawtiff.read_rect(mask_path, local_x, local_y, REAL_ROI["width"], REAL_ROI["height"], mask_info)
            if len(support) != REAL_ROI["width"] * REAL_ROI["height"] * 2 or any(byte != 255 for byte in support):
                raise ValueError("aligned real ROI lacks full kernel coverage")
            panel = gff.rawtiff.read_rect(image_path, local_x, local_y, REAL_ROI["width"], REAL_ROI["height"], image_info)
            roi_target = output / "roi" / f"{call.ordinal:02d}" / f"{index:02d}.tif"
            roi_target.parent.mkdir(parents=True, exist_ok=True)
            gff.rawtiff.write_tiff(roi_target, REAL_ROI["width"], REAL_ROI["height"], panel, bits=16, spp=3)
            roi_frames.append({"path": str(roi_target.resolve()), "sha256": _digest(roi_target), "aligned_output_sha256": image_digest})
            aligned_outputs.append({"path": str(image_path), "sha256": image_digest})
        strict = {"strategy": call.configuration.strategy, "reference": call.configuration.reference,
                  "raw_paths": [frame["path"] for frame in frames], "raw_hashes": [frame["sha256"] for frame in frames],
                  "raw_indices": raw["indices"], "dimensions": raw["dimensions"],
                  "roi_paths": [frame["path"] for frame in roi_frames], "roi_hashes": [frame["sha256"] for frame in roi_frames],
                  "transforms": [frame["transform"] for frame in native["frames"]], "statuses": [frame["status"] for frame in native["frames"]],
                  "output_paths": [entry["path"] for entry in aligned_outputs], "output_hashes": [entry["sha256"] for entry in aligned_outputs],
                  "reader_coverage": coverage, "prealigned": False, "crop_before_status": False,
                  "roi": raw["roi"], "solve_window": real_solve_window(*REAL_DIMENSIONS)}
        validate_manifest(strict, call.configuration.strategy, call.configuration.reference)
        manifest_path = output / "manifests" / f"{call.ordinal:02d}.json"; manifest_path.parent.mkdir(parents=True, exist_ok=True)
        manifest_path.write_text(json.dumps(strict, indent=2, sort_keys=True) + "\n")
        rows.append({"call": {"ordinal": call.ordinal, "label": call.label, "key": call.key()}, "original_manifest": dict(original_manifest), "original_raw": raw["frames"], "raw": frames, "roi": roi_frames, "native": record["record"], "manifest": {"path": str(manifest_path.resolve()), "sha256": _digest(manifest_path)}, "solve_window": real_solve_window(*REAL_DIMENSIONS)})
    require_shared_raw_bytes(({"frames": row["raw"]} for row in rows))
    return rows


def real_solve_window(width: int, height: int) -> Mapping[str, int]:
    if (width, height) != REAL_DIMENSIONS:
        raise ValueError("real solve dimensions differ")
    raw_w, raw_h = max(1, math.floor(.75 * width + .5)), max(1, math.floor(.75 * height + .5))
    if raw_w * raw_h > 2 ** 20:
        factor = math.sqrt(2 ** 20 / (raw_w * raw_h))
        raw_w, raw_h = max(1, math.floor(raw_w * factor)), max(1, math.floor(raw_h * factor))
    raw_w, raw_h = min(width, raw_w), min(height, raw_h)
    x, y = max(1, (width - raw_w) // 2), max(1, (height - raw_h) // 2)
    return {"x": x, "y": y, "width": min(raw_w, max(0, width - 1 - x)), "height": min(raw_h, max(0, height - 1 - y))}


def _assess_parity(records: list[Mapping[str, object]], calibration_rows: list[object]) -> list[Mapping[str, object]]:
    if len(records) != 4:
        raise ValueError("parity count differs")
    # Exact frame statuses/reference and finite scalar tolerance are checked
    # in pairs; a zero repeat difference remains zero.
    grouped: dict[str, list[Mapping[str, object]]] = {}
    for row in records:
        grouped.setdefault(str(row["call"]["key"]["strategy"]), []).append(row)
    if set(grouped) != {"anchor", "chain"} or any(len(rows) != 2 for rows in grouped.values()):
        raise ValueError("parity strategy pairs differ")
    comparisons: list[Mapping[str, object]] = []
    for strategy, rows in grouped.items():
        results = []
        for row in rows:
            _physical_record_identity(row.get("cli"), "CLI record")
            cli_path, _ = _file_binding(row.get("cli"), "CLI record")
            cli_record = _load_object(cli_path, "CLI record")
            cli = cli_record.get("result")
            if not isinstance(cli, Mapping):
                raise ValueError("CLI record result differs")
            _valid_frame_record(cli, Configuration("geometry", cli["strategy"], "middle", 8, .45, _seed_text(TRAINING_SEEDS[0])))
            results.append(cli)
        expected_key = rows[0]["call"]["key"]
        selected = [item for item in calibration_rows if isinstance(item, Mapping) and isinstance(item.get("call"), Mapping) and item["call"].get("key") == expected_key]
        if len(selected) != 2:
            raise ValueError("parity CAL key differs")
        selected.sort(key=lambda item: int(item["call"].get("ordinal", -1)))
        spikes = []
        for item in selected:
            binding = item.get("native")
            _physical_record_identity(binding, "parity CAL native record")
            path, _ = _file_binding(binding, "parity CAL native record")
            result = _load_object(path, "parity CAL native record").get("result")
            if not isinstance(result, Mapping):
                raise ValueError("parity CAL result differs")
            _valid_frame_record(result, Configuration("geometry", str(expected_key["strategy"]), "middle", 8, .45, _seed_text(TRAINING_SEEDS[0])))
            spikes.append(result)
        if any(cli.get("reference") != spike.get("reference") or [frame["status"] for frame in cli["frames"]] != [frame["status"] for frame in spike["frames"]] for cli, spike in zip(results, spikes)):
            raise ValueError("parity state differs")
        tau: dict[tuple[int, int, int], float] = {}
        for first, second in zip(results[0]["frames"], results[1]["frames"]):
            if not _matrix(first.get("transform")) or not _matrix(second.get("transform")):
                raise ValueError("parity CLI transform differs")
            for r in range(3):
                for c in range(3):
                    tau[(int(first["frame"]), r, c)] = abs(float(first["transform"][r][c]) - float(second["transform"][r][c]))
        for repeat, (cli, spike) in enumerate(zip(results, spikes)):
            for cli_frame, spike_frame in zip(cli["frames"], spike["frames"]):
                if not _matrix(cli_frame.get("transform")) or not _matrix(spike_frame.get("transform")):
                    raise ValueError("parity transform is absent")
                differences = [[abs(float(cli_frame["transform"][r][c]) - float(spike_frame["transform"][r][c])) for c in range(3)] for r in range(3)]
                if any(differences[r][c] > tau[(int(cli_frame["frame"]), r, c)] for r in range(3) for c in range(3)):
                    raise ValueError("parity matrix differs")
                comparisons.append({"strategy": strategy, "repeat": repeat, "frame": cli_frame["frame"], "tau": [[tau[(int(cli_frame["frame"]), r, c)] for c in range(3)] for r in range(3)], "difference": differences,
                                    "corner_difference": corner_errors(cli_frame["transform"], spike_frame["transform"])})
    return comparisons


HARD_METRICS = frozenset(("corner_rms", "corner_max", "drift_slope", "failure_count", "stack_overlap_loss", "blur_mae"))


def _allowance_index(rows: Iterable[Mapping[str, object]]) -> dict[tuple[object, ...], Mapping[str, object]]:
    result: dict[tuple[object, ...], Mapping[str, object]] = {}
    for row in rows:
        key = (row.get("control_table"), row.get("group"), row.get("reference"), row.get("frames"), row.get("sigma"),
               row.get("lighting_member"), row.get("metric"), row.get("frame_group"))
        if key in result:
            raise ValueError("allowance table has duplicate keys")
        result[key] = row
    return result


def _allowance_baseline(entry: Mapping[str, object], seed: str) -> tuple[float, float]:
    rows = entry.get("calibration")
    if not isinstance(rows, Mapping) or seed not in rows or not isinstance(rows[seed], list) or len(rows[seed]) != 2:
        raise ValueError("allowance seed pairing is incomplete")
    first, second = (_finite(value) for value in rows[seed])
    return ((first + second) / 2.0, _finite(entry.get("allowance")))


def _record_metric_values(record: Mapping[str, object], table: str, allowances: Mapping[tuple[object, ...], Mapping[str, object]]) -> dict[tuple[str, str], float]:
    call = record.get("call")
    if not isinstance(call, Mapping) or not isinstance(call.get("key"), Mapping) or not isinstance(record.get("metrics"), list):
        raise ValueError("assessment record is malformed")
    key, result = call["key"], {}
    frames = key.get("frames")
    if isinstance(frames, bool) or not isinstance(frames, int) or frames not in (8, 63):
        raise ValueError("assessment frame depth differs")
    expected = {
        *( (metric, f"frame-{frame:02d}") for metric in ("corner_rms", "corner_max", "blur_mae") for frame in range(frames) ),
        ("corner_rms", "stack"), ("corner_max", "stack"), ("drift_slope", "stack"),
        ("stack_overlap_loss", "stack"), ("blur_mae", "stack"),
    }
    for row in record["metrics"]:
        if not isinstance(row, Mapping) or row.get("metric") not in HARD_METRICS - {"failure_count"}:
            continue
        metric, frame_group, value = row.get("metric"), row.get("frame_group"), row.get("value")
        if not isinstance(metric, str) or not isinstance(frame_group, str):
            raise ValueError("assessment metric identity differs")
        if value is None:
            raise ValueError("assessment hard metric is null")
        if (metric, frame_group) not in expected or (metric, frame_group) in result:
            raise ValueError("assessment metric key is missing, extra, or duplicated")
        result[(metric, frame_group)] = _finite(value)
    if set(result) != expected:
        raise ValueError("assessment hard metric coverage differs")
    native_path, _ = _file_binding(record.get("native"), "assessment native record")
    native = _load_object(native_path, "assessment native record").get("result")
    if not isinstance(native, Mapping) or not isinstance(native.get("frames"), list):
        raise ValueError("assessment native result differs")
    failures = 0
    for index, frame in enumerate(native["frames"]):
        if not isinstance(frame, Mapping):
            raise ValueError("assessment native frame differs")
        allowance_key = (table, key.get("group"), key.get("reference"), key.get("frames"), key.get("sigma"), key.get("lighting") or "none", "corner_rms", f"frame-{index:02d}")
        budget = allowances.get(allowance_key)
        if budget is None or budget.get("corner_budget") is None:
            raise ValueError("assessment corner budget is absent")
        corner = result.get(("corner_rms", f"frame-{index:02d}"))
        if frame.get("status") in {"diverged", "read-error", "injected-failure"} or not _matrix(frame.get("transform")) or corner is None or corner > _finite(budget["corner_budget"]):
            failures += 1
    result[("failure_count", "stack")] = float(failures)
    return result


def _compare_candidate(record: Mapping[str, object], table_for_metric: callable, allowances: Mapping[tuple[object, ...], Mapping[str, object]]) -> list[Mapping[str, object]]:
    call = record.get("call")
    if not isinstance(call, Mapping) or not isinstance(call.get("key"), Mapping):
        raise ValueError("candidate call identity differs")
    key = call["key"]
    values_by_table: dict[str, dict[tuple[str, str], float]] = {}
    rows: list[Mapping[str, object]] = []
    for metric in HARD_METRICS:
        table = table_for_metric(metric)
        if table not in values_by_table:
            values_by_table[table] = _record_metric_values(record, table, allowances)
        for (name, frame_group), value in values_by_table[table].items():
            if name != metric:
                continue
            allowance_key = (table, key.get("group"), key.get("reference"), key.get("frames"), key.get("sigma"), key.get("lighting") or "none", name, frame_group)
            entry = allowances.get(allowance_key)
            if entry is None:
                raise ValueError("assessment allowance key is absent")
            baseline, allowance_value = _allowance_baseline(entry, str(key.get("seed")))
            rows.append({"call": {"ordinal": call.get("ordinal"), "label": call.get("label"), "key": key}, "control_table": table,
                         "metric": name, "frame_group": frame_group, "observed": value, "baseline": baseline,
                         "allowance": allowance_value, "increase": value - baseline, "veto": value - baseline > allowance_value,
                         "artifacts": {name: record.get(name) for name in ("input", "native", "fitted_resample", "truth_resample", "gain_after")}})
    if not rows:
        raise ValueError("candidate has no comparable hard metrics")
    return rows


def _quality_assessment(records: list[Mapping[str, object]], allowances_rows: list[Mapping[str, object]]) -> Mapping[str, object]:
    def deferred(reason: str, comparisons: list[Mapping[str, object]]) -> Mapping[str, object]:
        factor = {"state": "DEFER", "reason": reason}
        return {"state": "DEFER", "reason": reason, "rolling": factor, "photometry": factor, "comparisons": comparisons}
    if len(records) != 140:
        return deferred("incomplete-quality-matrix", [])
    allowances = _allowance_index(allowances_rows)
    geometry = [record for record in records if isinstance(record.get("call"), Mapping) and isinstance(record["call"].get("key"), Mapping) and record["call"]["key"].get("group") == "geometry"]
    photometry = [record for record in records if isinstance(record.get("call"), Mapping) and isinstance(record["call"].get("key"), Mapping) and record["call"]["key"].get("group") == "photometry"]
    if len(geometry) != 40 or len(photometry) != 100:
        return deferred("quality-group-coverage-differs", [])
    expected_geometry = {
        ("rolling", reference, frames, sigma, seed)
        for reference in REFERENCES for frames in (8, 63)
        for sigma in (.15, .45) for seed in (_seed_text(item) for item in TRAINING_SEEDS)
    }
    geometry_keys = [
        (key.get("strategy"), key.get("reference"), key.get("frames"), key.get("sigma"), key.get("seed"))
        for record in geometry for key in [record["call"]["key"]]
    ]
    if len(set(geometry_keys)) != len(geometry_keys) or set(geometry_keys) != expected_geometry:
        return deferred("geometry-coverage-differs", [])
    comparisons: list[Mapping[str, object]] = []
    try:
        for record in geometry:
            comparisons.extend(_compare_candidate(record, lambda metric: "geometry_chain" if metric == "drift_slope" else "geometry_anchor", allowances))
        # Strong-defocus failure benefit is C vs A for every matched seed/key.
        strong = [row for row in comparisons if row["metric"] == "failure_count" and row["frame_group"] == "stack" and row["control_table"] == "geometry_anchor" and row["call"]["key"].get("sigma") == .45]
        if len(strong) != 20:
            return deferred("strong-failure-coverage-differs", comparisons)
        geometry_benefit = all(-_finite(row["increase"]) > 2.0 * _finite(row["allowance"]) for row in strong)
        geometry_veto = any(bool(row["veto"]) for row in comparisons)
        by_photo: dict[tuple[object, ...], Mapping[str, object]] = {}
        for record in photometry:
            call = record["call"]; key = call["key"]
            by_photo[(key.get("strategy"), key.get("lighting"), key.get("order"), key.get("seed"))] = record
        expected_photo = {(strategy, member, order, _seed_text(seed)) for strategy in ("anchor", "rolling") for member in LIGHTING for order in ORDERS for seed in TRAINING_SEEDS if not (strategy == "anchor" and order == "none")}
        if set(by_photo) != expected_photo:
            return deferred("photometry-coverage-differs", comparisons)
        photo_veto = False
        photo_benefits: list[bool] = []
        unchanged_ok = True
        rolling_unchanged_ok = True
        rolling_corroboration: list[bool] = []
        for member in LIGHTING:
            for seed in (_seed_text(item) for item in TRAINING_SEEDS):
                anchor_before = _compare_candidate(by_photo[("anchor", member, "gain-before", seed)], lambda _: "photometry_anchor_none", allowances)
                anchor_oracle = _compare_candidate(by_photo[("anchor", member, "truth-oracle", seed)], lambda _: "photometry_anchor_none", allowances)
                rolling_values = {order: _record_metric_values(by_photo[("rolling", member, order, seed)], "photometry_anchor_none", allowances) for order in ORDERS}
                if set(rolling_values["none"]) != set(rolling_values["gain-before"]) or set(rolling_values["none"]) != set(rolling_values["truth-oracle"]):
                    return deferred("rolling-photometry-metric-pairing-differs", comparisons)
                rolling_rows = []
                rolling_key = by_photo[("rolling", member, "none", seed)]["call"]["key"]
                for order, values in rolling_values.items():
                    for (metric, frame_group), observed in values.items():
                        entry = allowances.get(("photometry_anchor_none", rolling_key.get("group"), rolling_key.get("reference"), rolling_key.get("frames"), rolling_key.get("sigma"), member, metric, frame_group))
                        if entry is None:
                            raise ValueError("rolling photometry allowance is absent")
                        _, instrument = _allowance_baseline(entry, seed)
                        baseline = rolling_values["none"][(metric, frame_group)]
                        increase = observed - baseline
                        rolling_rows.append({"call": by_photo[("rolling", member, order, seed)]["call"], "control_table": "photometry_anchor_none", "metric": metric,
                                             "frame_group": frame_group, "observed": observed, "baseline": baseline, "allowance": instrument,
                                             "increase": increase, "veto": order != "none" and increase > instrument,
                                             "artifacts": {name: by_photo[("rolling", member, order, seed)].get(name) for name in ("input", "native", "fitted_resample", "truth_resample")}})
                comparisons.extend(anchor_before); comparisons.extend(anchor_oracle); comparisons.extend(rolling_rows)
                photo_veto |= any(bool(row["veto"]) for row in [*anchor_before, *anchor_oracle, *rolling_rows])
                before = {(row["metric"], row["frame_group"]): row for row in anchor_before}
                oracle = {(row["metric"], row["frame_group"]): row for row in anchor_oracle}
                if set(before) != set(oracle) or not before:
                    return deferred("photometry-metric-pairing-differs", comparisons)
                conditions = []
                for pair, before_row in before.items():
                    oracle_row = oracle[pair]
                    none = _finite(before_row["baseline"])
                    instrument = _finite(before_row["allowance"])
                    improvement = none - _finite(before_row["observed"])
                    headroom = none - _finite(oracle_row["observed"])
                    conditions.append(improvement > 2.0 * instrument and headroom > 2.0 * instrument and improvement <= headroom + instrument)
                    if member == "unchanged" and (improvement > 2.0 * instrument or headroom > 2.0 * instrument):
                        unchanged_ok = False
                if member != "unchanged":
                    photo_benefits.extend(conditions)
                rolling_by_order = {order: {(row["metric"], row["frame_group"]): row for row in rolling_rows if row["call"]["key"].get("order") == order} for order in ORDERS}
                if any(set(rolling_by_order[order]) != set(rolling_by_order["none"]) for order in ORDERS):
                    return deferred("rolling-photometry-corroboration-pairing-differs", comparisons)
                for pair, none_row in rolling_by_order["none"].items():
                    before_row, oracle_row = rolling_by_order["gain-before"][pair], rolling_by_order["truth-oracle"][pair]
                    instrument = _finite(none_row["allowance"])
                    improvement = _finite(none_row["observed"]) - _finite(before_row["observed"])
                    headroom = _finite(none_row["observed"]) - _finite(oracle_row["observed"])
                    rolling_corroboration.append(improvement <= headroom + instrument)
                    if member == "unchanged" and (improvement > 2.0 * instrument or headroom > 2.0 * instrument):
                        rolling_unchanged_ok = False
        if not photo_benefits:
            return deferred("photometry-benefit-inputs-absent", comparisons)
        rolling = verdict(True, geometry_benefit, geometry_veto, True, True)
        photometry = verdict(True, all(photo_benefits), photo_veto, True, unchanged_ok and rolling_unchanged_ok)
        combined = verdict(True, rolling["state"] == "GO" and photometry["state"] == "GO",
                           rolling["state"] == "NO_GO" or photometry["state"] == "NO_GO", True, True)
        return {**combined, "rolling": rolling, "photometry": photometry,
                "geometry_benefit": geometry_benefit, "photometry_benefit": all(photo_benefits), "geometry_veto": geometry_veto,
                "photometry_veto": photo_veto, "unchanged_ok": unchanged_ok and rolling_unchanged_ok,
                "rolling_corroboration": all(rolling_corroboration), "comparisons": comparisons}
    except (KeyError, TypeError, ValueError) as error:
        result = deferred("missing-or-null-assessment-input", comparisons)
        return {**result, "detail": str(error)}


def assess_stage(stage: str, records: list[Mapping[str, object]], *, quality_digest: str | None = None,
                 allowances: list[Mapping[str, object]] | None = None) -> Mapping[str, object]:
    """Return only frozen GO, NO_GO, or DEFER states for measured stages."""
    if len(records) != stage_counts()[stage]:
        return {"state": "DEFER", "reason": "incomplete-matrix"}
    if stage in {"quality", "repeat"}:
        if allowances is None or (stage == "repeat" and not _sha(quality_digest)):
            return {"state": "DEFER", "reason": "admission-or-quality-binding-absent"}
        return _quality_assessment(records, allowances)
    if stage == "controls":
        expected = ((1, "right"), (2, "left"))
        try:
            for record, (frame_index, leg) in zip(records, expected):
                path, _ = _file_binding(record.get("native"), "control native record")
                result = _load_object(path, "control native record").get("result")
                frames = result.get("frames") if isinstance(result, Mapping) else None
                if not isinstance(frames, list) or not isinstance(frames[frame_index], Mapping) or frames[frame_index].get("status") != "injected-failure" or frames[frame_index].get("reason") != "registered-control":
                    return {"state": "NO_GO", "reason": "injected-control-not-observed"}
                direction = 1 if leg == "right" else -1
                later = frame_index + direction
                if 0 <= later < len(frames) and (not isinstance(frames[later], Mapping) or frames[later].get("status") in {"read-error", "injected-failure"}):
                    return {"state": "NO_GO", "reason": "rolling-control-did-not-continue"}
        except (OSError, ValueError, TypeError, KeyError):
            return {"state": "DEFER", "reason": "control-record-refused"}
        return {"state": "DEFER", "reason": "independent-control-audit-required"}
    return {"state": "DEFER", "reason": "independent-stage-audit-required"}


def calibration_tables(records: Iterable[Mapping[str, object]]) -> Mapping[str, Mapping[str, object]]:
    """Build all finite two-CAL/five-seed allowance rows from CAL200 only."""
    rows = tuple(records)
    if len(rows) != 200:
        raise ValueError("calibration record count differs")
    grouped: dict[tuple[object, ...], dict[str, list[float]]] = {}

    def table_for(key: Mapping[str, object]) -> str:
        if key.get("group") == "geometry":
            return "geometry_anchor" if key.get("strategy") == "anchor" else "geometry_chain" if key.get("strategy") == "chain" else ""
        if key.get("group") == "photometry" and key.get("strategy") == "anchor" and key.get("order") == "none":
            return "photometry_anchor_none"
        return ""

    def projection_for(key: Mapping[str, object], table: str, metric: str, frame_group: str) -> tuple[object, ...]:
        return (table, key.get("group"), key.get("reference"), key.get("frames"), key.get("sigma"), key.get("lighting") or "none", "none", metric, frame_group)

    hard = {"corner_rms", "corner_max", "drift_mean", "drift_slope", "stack_overlap_loss", "blur_mae"}
    for record in rows:
        call, metrics = record.get("call"), record.get("metrics")
        if not isinstance(call, Mapping) or not isinstance(call.get("key"), Mapping) or not isinstance(metrics, list):
            raise ValueError("calibration record is incomplete")
        key = call["key"]
        table = table_for(key)
        if not table:
            raise ValueError("candidate entered calibration")
        for metric in metrics:
            if not isinstance(metric, Mapping):
                raise ValueError("calibration metric is malformed")
            name, frame_group = metric.get("metric"), metric.get("frame_group")
            if name not in hard:
                continue
            if metric.get("value") is None:
                raise ValueError("calibration hard metric is null")
            if not isinstance(frame_group, str):
                raise ValueError("calibration frame group differs")
            projection = projection_for(key, table, str(name), frame_group)
            grouped.setdefault(projection, {}).setdefault(str(key.get("seed")), []).append(_finite(metric["value"]))
    output: dict[str, Mapping[str, object]] = {}
    required = {_seed_text(seed) for seed in TRAINING_SEEDS}
    for projection, seeds in grouped.items():
        if set(seeds) != required or any(len(values) != 2 for values in seeds.values()):
            raise ValueError("calibration key is incomplete")
        pairs = {seed: (values[0], values[1]) for seed, values in seeds.items()}
        value = allowance(pairs, pairs)
        output[json.dumps(projection)] = {"control_table": projection[0], "group": projection[1], "reference": projection[2], "frames": projection[3], "sigma": projection[4], "lighting_member": projection[5], "fit_order": projection[6], "metric": projection[7], "frame_group": projection[8], "calibration": {seed: list(pair) for seed, pair in pairs.items()}, "allowance": value,
                                           "corner_budget": error_budget([entry for pair in pairs.values() for entry in pair], value) if projection[7] == "corner_rms" and projection[8] != "stack" else None}
    # Freeze corner budgets first.  Only those CAL observations may classify
    # failures; later QUALITY/REPEAT values never influence either quantity.
    failures: dict[tuple[object, ...], dict[str, list[float]]] = {}
    for record in rows:
        call = record["call"]
        assert isinstance(call, Mapping) and isinstance(call["key"], Mapping)
        key = call["key"]
        table = table_for(key)
        native_binding = record.get("native")
        native_path, _ = _file_binding(native_binding, "calibration native record")
        native = _load_object(native_path, "calibration native record").get("result")
        if not isinstance(native, Mapping) or not isinstance(native.get("frames"), list):
            raise ValueError("calibration native frames differ")
        corners = {metric.get("frame_group"): metric.get("value") for metric in record["metrics"]
                   if isinstance(metric, Mapping) and metric.get("metric") == "corner_rms"}
        failed = 0
        for index, frame in enumerate(native["frames"]):
            if not isinstance(frame, Mapping):
                raise ValueError("calibration native frame differs")
            frame_group = f"frame-{index:02d}"
            budget_row = output.get(json.dumps(projection_for(key, table, "corner_rms", frame_group)))
            if not isinstance(budget_row, Mapping) or budget_row.get("corner_budget") is None:
                raise ValueError("calibration corner budget is incomplete")
            status, transform, corner = frame.get("status"), frame.get("transform"), corners.get(frame_group)
            if status in {"diverged", "read-error", "injected-failure"} or not _matrix(transform) or corner is None or _finite(corner) > _finite(budget_row["corner_budget"]):
                failed += 1
        projection = projection_for(key, table, "failure_count", "stack")
        failures.setdefault(projection, {}).setdefault(str(key.get("seed")), []).append(float(failed))
    for projection, seeds in failures.items():
        if set(seeds) != required or any(len(values) != 2 for values in seeds.values()):
            raise ValueError("calibration failure key is incomplete")
        pairs = {seed: (values[0], values[1]) for seed, values in seeds.items()}
        output[json.dumps(projection)] = {"control_table": projection[0], "group": projection[1], "reference": projection[2], "frames": projection[3], "sigma": projection[4], "lighting_member": projection[5], "fit_order": projection[6], "metric": projection[7], "frame_group": projection[8], "calibration": {seed: list(pair) for seed, pair in pairs.items()}, "allowance": allowance(pairs, pairs), "corner_budget": None}
    if not output:
        raise ValueError("calibration table is empty")
    return output


def classify_failure(record: Mapping[str, object], corner_budget: object) -> bool:
    """Apply the frozen order: corner budget first, then failure classification."""
    _finite(corner_budget)
    frames = record.get("native_result")
    if not isinstance(frames, list):
        raise ValueError("failure record lacks native frames")
    for frame in frames:
        if not isinstance(frame, Mapping) or frame.get("status") in {"diverged", "read-error", "injected-failure"}:
            return True
        transform, corner = frame.get("transform"), frame.get("corner_rms")
        if not _matrix(transform) or corner is None or _finite(corner) > float(corner_budget):
            return True
    return False


def verdict(complete: bool, benefit: bool | None, hard_veto: bool, detectors_ok: bool, unchanged_ok: bool) -> Mapping[str, str]:
    """Frozen GO/NO_GO/DEFER decision; incomplete data never becomes a miss."""
    if not complete or benefit is None:
        return {"state": "DEFER", "reason": "incomplete-or-null-required-matrix"}
    if hard_veto or not detectors_ok or not unchanged_ok:
        return {"state": "NO_GO", "reason": "hard-veto-or-control-failure"}
    if not benefit:
        return {"state": "NO_GO", "reason": "complete-required-benefit-miss"}
    return {"state": "GO", "reason": "all-frozen-rules-passed"}


def verify_repeat(quality: Mapping[str, object], repeated: Mapping[str, object], expected_digest: str) -> None:
    """Require the immutable quality document and every input/pixel binding to recur."""
    if quality.get("stage") != "quality":
        raise ValueError("repeat requires a quality stage")
    if not _sha(expected_digest) or quality.get("sha256") != expected_digest:
        raise ValueError("repeat quality digest differs")
    quality_records, repeat_records = quality.get("records"), repeated.get("records")
    if not isinstance(quality_records, list) or not isinstance(repeat_records, list) or len(quality_records) != 140 or len(repeat_records) != 140:
        raise ValueError("repeat matrix differs")
    for first, second in zip(quality_records, repeat_records):
        if not isinstance(first, Mapping) or not isinstance(second, Mapping):
            raise ValueError("repeat key differs")
        first_call, second_call = first.get("call"), second.get("call")
        if (not isinstance(first_call, Mapping) or not isinstance(second_call, Mapping)
                or first_call.get("ordinal") != second_call.get("ordinal") or first_call.get("key") != second_call.get("key")):
            raise ValueError("repeat key differs")
        first_input, _ = _file_binding(first.get("input"), "QUALITY repeat input")
        second_input, _ = _file_binding(second.get("input"), "REPEAT input")
        if _digest(first_input) != _digest(second_input):
            raise ValueError("repeat raw input manifest differs")
        for name in ("native", "fitted_resample", "truth_resample"):
            if _physical_record_identity(first.get(name), f"QUALITY {name}") != _physical_record_identity(second.get(name), f"REPEAT {name}"):
                raise ValueError("repeat physical result differs")


def _physical_record_identity(binding: object, label: str) -> Mapping[str, object]:
    """Compare immutable semantics and output bytes, never run-local paths."""
    path, _ = _file_binding(binding, label)
    record = _load_object(path, label)
    inputs = record.get("inputs")
    if inputs is not None and (not isinstance(inputs, list) or any(not isinstance(row, Mapping) for row in inputs)):
        raise ValueError(f"{label} inputs differ")
    if isinstance(inputs, list):
        for item in inputs:
            _file_binding(item, f"{label} input")
    for name in ("argv", "stdout", "stderr"):
        _file_binding(record.get(name), f"{label} {name}")
    exit_path, _ = _file_binding(record.get("native_exit"), f"{label} native_exit")
    native_exit = record.get("native_exit")
    if (not isinstance(native_exit, Mapping) or isinstance(native_exit.get("value"), bool)
            or not isinstance(native_exit.get("value"), int)):
        raise ValueError(f"{label} native exit differs")
    if exit_path.read_text().strip() != str(native_exit["value"]):
        raise ValueError(f"{label} native exit receipt differs")
    binary_path, binary_digest = _file_binding(record.get("binary"), f"{label} binary")
    if binary_path.is_symlink() or _digest(binary_path) != binary_digest:
        raise ValueError(f"{label} binary drift differs")
    transforms_digest = None
    if label.endswith(("fitted_resample", "truth_resample")):
        _, transforms_digest = _file_binding(record.get("transforms"), f"{label} transforms")
    elif record.get("transforms") is not None:
        _, transforms_digest = _file_binding(record.get("transforms"), f"{label} transforms")
    outputs = record.get("outputs")
    if not isinstance(outputs, list):
        raise ValueError(f"{label} outputs differ")
    output_hashes = []
    for item in outputs:
        if isinstance(item, Mapping) and isinstance(item.get("image"), Mapping) and isinstance(item.get("mask"), Mapping):
            _, image = _file_binding(item["image"], f"{label} output image")
            _, mask = _file_binding(item["mask"], f"{label} output mask")
            output_hashes.append((image, mask))
        elif isinstance(item, Mapping):
            _, digest = _file_binding(item, f"{label} output")
            output_hashes.append(digest)
        else:
            raise ValueError(f"{label} output differs")
    result = record.get("result")
    if not isinstance(result, Mapping):
        raise ValueError(f"{label} result differs")
    semantic_result = {name: value for name, value in result.items() if name not in {"outputs", "masks"}}
    return {"result": semantic_result, "inputs": [row.get("sha256") for row in inputs] if isinstance(inputs, list) else None,
            "outputs": output_hashes, "binary": binary_digest, "transforms": transforms_digest, "native_exit": native_exit["value"]}


def write_stage_output(path: Path, document: Mapping[str, object]) -> None:
    if path.exists():
        raise ValueError("immutable output already exists")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document, indent=2, sort_keys=True, allow_nan=False) + "\n")


def _audit_graph(binding: object, registration_digest: str) -> Mapping[str, object]:
    path, _ = _file_binding(binding, "audit summary")
    audit = _load_object(path, "audit summary")
    if (audit.get("schema") != "alignment-reference-audit-v1" or audit.get("result") != "PASS"
            or _file_binding(audit.get("registration"), "audit registration")[1] != registration_digest):
        raise ValueError("audit summary differs")
    index_path, _ = _file_binding(audit.get("artifact_index"), "audit index")
    assertions_path, _ = _file_binding(audit.get("assertions"), "audit assertions")
    receipt_path, _ = _file_binding(audit.get("receipt"), "audit receipt")
    index, assertions = _load_object(index_path, "audit index"), _load_object(assertions_path, "audit assertions")
    entries = index.get("entries")
    values = assertions.get("assertions")
    if not isinstance(entries, list) or not entries or not isinstance(values, list) or not values:
        raise ValueError("audit graph is empty")
    for entry in entries:
        _file_binding(entry, "audit artifact")
    if any(not isinstance(item, Mapping) or item.get("passed") is not True for item in values):
        raise ValueError("audit assertion failed")
    receipt = _load_object(receipt_path, "audit receipt")
    if receipt.get("native_exit") != 0:
        raise ValueError("audit receipt native exit differs")
    for name in ("argv", "stdout", "stderr"):
        _file_binding(receipt.get(name), f"audit receipt {name}")
    _file_binding(receipt.get("time", receipt.get("time_v")), "audit receipt time")
    return audit


def _ready_envelope(path: Path | None, expected_digest: str | None, state: str, registration_digest: str,
                    root: Path) -> Mapping[str, object]:
    """Verify an externally pinned envelope and all physical graph edges."""
    if path is None or not _sha(expected_digest) or not path.is_file() or _digest(path) != expected_digest:
        raise ValueError("externally pinned ready envelope differs")
    envelope = _load_object(path, "ready envelope")
    if (envelope.get("schema") != "alignment-reference-ready-v1" or envelope.get("state") != state
            or _file_binding(envelope.get("registration"), "ready registration")[1] != registration_digest):
        raise ValueError("ready envelope state differs")
    audit = _audit_graph(envelope.get("audit"), registration_digest)
    if state == "source-ready":
        artifacts_path, _ = _file_binding(envelope.get("artifacts"), "source artifacts")
        artifacts = _load_object(artifacts_path, "source artifacts")
        if (artifacts.get("schema") != "alignment-reference-source-artifacts-v1"
                or _file_binding(artifacts.get("registration"), "source artifact registration")[1] != registration_digest):
            raise ValueError("source artifact schema differs")
        closure_path, _ = _file_binding(artifacts.get("source_closure"), "source closure")
        executables_path, _ = _file_binding(artifacts.get("executables"), "executables")
        closure, executables_document = _load_object(closure_path, "source closure"), _load_object(executables_path, "executables")
        rows = closure.get("entries")
        executables = executables_document.get("executables")
        if not isinstance(rows, list) or not isinstance(executables, Mapping):
            raise ValueError("source closure graph differs")
        normalized = []
        for item in rows:
            if not isinstance(item, Mapping):
                raise ValueError("source closure entry differs")
            source = item.get("path")
            if not isinstance(source, str) or Path(source).is_absolute() or ".." in Path(source).parts:
                raise ValueError("source closure path differs")
            snapshot = item.get("snapshot")
            _, snapshot_digest = _file_binding(snapshot, "source snapshot")
            live = (root / source).resolve()
            if root not in live.parents or not live.is_file() or _digest(live) != item.get("sha256") or snapshot_digest != item.get("sha256"):
                raise ValueError("live source closure differs")
            normalized.append({"path": source, "sha256": item.get("sha256")})
        validate_source_closure(normalized, {name: value.get("sha256") if isinstance(value, Mapping) else None for name, value in executables.items()})
        recomputed = tuple({"path": item.path, "sha256": item.sha256} for item in recursive_source_closure(root))
        if tuple(sorted(normalized, key=lambda item: str(item["path"]))) != recomputed:
            raise ValueError("source closure differs from the live recursive closure")
        for name in ("focusforge-cli", "alignment-reference-strategies"):
            item = executables.get(name)
            if not isinstance(item, Mapping):
                raise ValueError("executable graph differs")
            live, digest = _file_binding(item, "live executable")
            snapshot, snapshot_digest = _file_binding(item.get("snapshot"), "executable snapshot")
            if digest != snapshot_digest or _digest(live) != _digest(snapshot):
                raise ValueError("executable snapshot differs")
            with live.open("rb") as stream:
                if stream.read(4) != b"\x7fELF":
                    raise ValueError("executable is not ELF")
        controls_path, _ = _file_binding(artifacts.get("controls"), "source controls")
        controls = _load_object(controls_path, "source controls")
        detector = controls.get("detectors")
        if controls.get("fit_free") is not True or controls.get("passed") is not True or not isinstance(detector, Mapping):
            raise ValueError("source controls differ")
        _file_binding(detector, "source detector controls")
    elif state == "parity-ready":
        _file_binding(envelope.get("source_ready"), "parity source-ready")
        _file_binding(envelope.get("comparison"), "parity comparison")
    elif state == "allowance-ready":
        for name in ("source_ready", "parity_ready", "calibration", "controls", "allowances"):
            _file_binding(envelope.get(name), f"allowance {name}")
        _validate_allowance_document(_load_object(_file_binding(envelope.get("allowances"), "allowance table")[0], "allowance table"))
    else:
        raise ValueError("unknown ready state")
    return {"envelope": envelope, "audit": audit, "sha256": str(expected_digest)}


def _required_allowance_projections() -> set[tuple[object, ...]]:
    """Return every CAL-only allowance identity fixed by the finite matrix."""
    projections: set[tuple[object, ...]] = set()
    frame_metrics = ("corner_rms", "corner_max", "drift_mean", "blur_mae")
    stack_metrics = ("corner_rms", "corner_max", "drift_slope", "stack_overlap_loss", "blur_mae", "failure_count")
    for row in calibration_configurations():
        if row.group == "geometry":
            table = "geometry_anchor" if row.strategy == "anchor" else "geometry_chain"
        elif row.group == "photometry" and row.strategy == "anchor" and row.order == "none":
            table = "photometry_anchor_none"
        else:
            raise ValueError("calibration projection differs")
        prefix = (table, row.group, row.reference, row.frames, row.sigma, row.lighting or "none", "none")
        projections.update((*prefix, metric, f"frame-{frame:02d}") for metric in frame_metrics for frame in range(row.frames))
        projections.update((*prefix, metric, "stack") for metric in stack_metrics)
    return projections


def _validate_allowance_document(document: Mapping[str, object]) -> None:
    rows = document.get("allowances")
    if not isinstance(rows, list) or not rows:
        raise ValueError("allowance table is absent")
    expected = _required_allowance_projections()
    actual: dict[tuple[object, ...], Mapping[str, object]] = {}
    seeds = {_seed_text(seed) for seed in TRAINING_SEEDS}
    for row in rows:
        if not isinstance(row, Mapping):
            raise ValueError("allowance row differs")
        projection = (row.get("control_table"), row.get("group"), row.get("reference"), row.get("frames"), row.get("sigma"),
                      row.get("lighting_member"), row.get("fit_order"), row.get("metric"), row.get("frame_group"))
        if projection in actual or projection not in expected:
            raise ValueError("allowance key projection differs")
        actual[projection] = row
        observations = row.get("calibration")
        if not isinstance(observations, Mapping) or set(observations) != seeds:
            raise ValueError("allowance calibration seed matrix differs")
        pairs = {seed: tuple(value) if isinstance(value, list) and len(value) == 2 else () for seed, value in observations.items()}
        value = allowance(pairs, pairs)
        if _finite(row.get("allowance")) != value:
            raise ValueError("allowance arithmetic differs")
        observed = [entry for pair in pairs.values() for entry in pair]
        budget = row.get("corner_budget")
        if projection[7] == "corner_rms" and projection[8] != "stack":
            if _finite(budget) != error_budget(observed, value):
                raise ValueError("corner budget differs")
        elif budget is not None:
            raise ValueError("unexpected corner budget differs")
    if set(actual) != expected:
        raise ValueError("allowance table is incomplete")


def validate_admission(stage: str, registration: Path, source_proof: Path | None, allowance_proof: Path | None,
                       parity_proof: Path | None = None, *, root: Path | None = None,
                       source_expected: str | None = None, parity_expected: str | None = None,
                       allowance_expected: str | None = None) -> dict[str, object]:
    if stage not in STAGES or root is None or not registration.is_file():
        raise ValueError("stage root or registration differs")
    registration_digest = _digest(registration)
    if _load_object(registration, "registration").get("schema") != "focusforge.alignment-reference.registration/v3":
        raise ValueError("registration schema differs")
    root = root.resolve()
    source = None if stage == "source" else _ready_envelope(source_proof, source_expected, "source-ready", registration_digest, root)
    parity = None if stage not in {"controls", "quality", "repeat", "real"} else _ready_envelope(parity_proof, parity_expected, "parity-ready", registration_digest, root)
    allowances = None if stage not in {"quality", "repeat", "real"} else _ready_envelope(allowance_proof, allowance_expected, "allowance-ready", registration_digest, root)
    if parity is not None:
        _, linked_source = _file_binding(parity["envelope"].get("source_ready"), "parity linked source-ready")
        if source is None or linked_source != source["sha256"]:
            raise ValueError("parity proof does not bind the admitted source proof")
    if allowances is not None:
        _, linked_source = _file_binding(allowances["envelope"].get("source_ready"), "allowance linked source-ready")
        _, linked_parity = _file_binding(allowances["envelope"].get("parity_ready"), "allowance linked parity-ready")
        if source is None or parity is None or linked_source != source["sha256"] or linked_parity != parity["sha256"]:
            raise ValueError("allowance proof does not bind admitted source/parity proofs")
    return {"stage": stage, "registration_sha256": registration_digest, "source": source,
            "parity": parity, "allowance": allowances, "counts": stage_counts()}


def corner_errors(fitted: object, truth: object, width: int = 512, height: int = 512) -> dict[str, float]:
    """Exact four-corner error for ``E = F * inverse(M)`` in reference space."""
    import numpy as np
    if not _matrix(fitted) or not _matrix(truth) or width < 2 or height < 2:
        raise ValueError("corner transform or dimensions differ")
    try:
        error = np.asarray(fitted, dtype=np.float64) @ np.linalg.inv(np.asarray(truth, dtype=np.float64))
    except np.linalg.LinAlgError as failure:
        raise ValueError("truth transform is not invertible") from failure
    distances = []
    for x, y in ((0.0, 0.0), (float(width - 1), 0.0), (0.0, float(height - 1)), (float(width - 1), float(height - 1))):
        mapped = error @ np.asarray((x, y, 1.0))
        if not math.isfinite(float(mapped[2])) or mapped[2] == 0:
            raise ValueError("corner maps to infinity")
        distances.append(math.hypot(float(mapped[0] / mapped[2] - x), float(mapped[1] / mapped[2] - y)))
    return {"corner_rms": math.sqrt(sum(value * value for value in distances) / 4),
            "corner_max": max(distances), "drift_mean": sum(distances) / 4}


def _render_selected_frames(renderer: Callable[[object, object, float, int, int, int], list[object]], sources: list[object],
                            depth: object, sigma: float, noise_seed: int, frames: int) -> list[object]:
    """Render equal raw sources once and retain only each caller-selected frame."""
    import numpy as np

    if frames <= 0 or not sources:
        raise ValueError("selected render inputs differ")
    groups: dict[tuple[tuple[int, ...], str, bytes], tuple[object, list[int]]] = {}
    for ordinal, source in enumerate(sources):
        array = np.asarray(source)
        key = (array.shape, array.dtype.str, array.tobytes())
        if key not in groups:
            groups[key] = (source, [])
        groups[key][1].append(ordinal)
    selected: list[object | None] = [None] * len(sources)
    for source, ordinals in groups.values():
        rendered = renderer(source, depth, sigma, 0, noise_seed, frames)
        if not isinstance(rendered, (list, tuple)) or len(rendered) != frames:
            raise ValueError("selected render frame matrix differs")
        for ordinal in ordinals:
            selected[ordinal] = rendered[ordinal % frames]
        del rendered
    if any(frame is None for frame in selected):
        raise ValueError("selected render cache is incomplete")
    return [frame for frame in selected if frame is not None]


def render_synthetic(row: Configuration) -> RenderedSynthetic:
    """Render float-lit sources, renderer-quantized defocus, warp coverage, then noise."""
    import numpy as np
    from . import scenes
    synth = scenes.ss
    seed = next(item for item in TRAINING_SEEDS if item.text() == row.seed)
    depth7, _ = scenes._layout(seed.layout)
    depth = depth7 / 7.0 * (row.frames - 1)
    truth = synth.scene_hard(seed.texture).astype(np.float64)
    member = row.lighting or "unchanged"
    original = [_lighting_array(truth, member, index) for index in range(row.frames)]
    clipped = [np.clip(source, 0.0, 65535.0) for source in original]
    censor = [(source <= 0.0) | (source >= 65535.0) for source in original]
    clip_fraction = tuple(float(((source < 0.0) | (source > 65535.0)).mean()) for source in original)
    oracle = [_oracle_array(source, member, index) for index, source in enumerate(clipped)]
    # synth.render owns the registered defocus interpolation and its u16
    # intermediate quantization.  Inputs here remain float until that call.
    selected = _render_selected_frames(synth.render, [*clipped, *oracle], depth, row.sigma, seed.noise, row.frames)
    clean, oracle_clean = selected[:row.frames], selected[row.frames:]
    middle = (row.frames - 1) // 2
    transforms_middle = synth.warp_transforms(1.0, seed.layout, middle, row.frames)
    reference = 0 if row.reference == "first" else middle
    transforms_reference = [matrix @ np.linalg.inv(transforms_middle[reference]) for matrix in transforms_middle]
    random = np.random.default_rng(seed.noise ^ 0x5A5A5A5A)
    frames, oracle_frames, coverages = [], [], []
    for image, inverse_image, transform in zip(clean, oracle_clean, transforms_middle):
        warped, inverse_warped = synth.resample(image.astype(np.float64), transform), synth.resample(inverse_image.astype(np.float64), transform)
        # The independent generator clamps sample values at edges.  Geometry
        # coverage therefore comes only from the known inverse warp, never
        # from finite or nonblack output values.
        coverage = scenes._coverage(transform)
        noise = random.normal(0.0, 250.0, warped.shape)
        frames.append(np.where(coverage[..., None], np.rint(np.clip(warped + noise, 0, 65535)), 0).astype(np.uint16))
        oracle_frames.append(np.where(coverage[..., None], np.rint(np.clip(inverse_warped + noise, 0, 65535)), 0).astype(np.uint16))
        coverages.append(coverage)
    return RenderedSynthetic(np.stack(frames), np.stack(oracle_frames), truth, transforms_middle, transforms_reference,
                             np.stack(coverages), tuple(clipped), clip_fraction, synthetic_input(row).raw_specification)


def gain_after_is_identical(none_matrix: object, gain_after_matrix: object) -> None:
    import numpy as np
    left, right = np.asarray(none_matrix), np.asarray(gain_after_matrix)
    if left.dtype != right.dtype or left.shape != right.shape or left.tobytes() != right.tobytes():
        raise ValueError("gain-after matrix differs from none control")


def write_synthetic_input(row: Configuration, output: Path, *, renderer: Callable[[Configuration], RenderedSynthetic] = render_synthetic) -> Mapping[str, object]:
    """Emit raw/order inputs plus preclip, censor, and warp-support evidence."""
    import numpy as np
    from .runner import gff, sha256_path
    if output.exists():
        raise ValueError("synthetic output already exists")
    rendered = renderer(row)
    output.mkdir(parents=True)
    selected = frames_for_order(rendered, row.order or "none")
    paths, metric_paths = [], []
    frame_dir = output / "frames"; frame_dir.mkdir()
    metric_dir = output / "metric-raw"; metric_dir.mkdir()
    for index, image in enumerate(selected):
        path = frame_dir / f"{index:02d}.tif"; gff.write_u16(path, image); paths.append(path)
    for index, image in enumerate(rendered.frames):
        path = metric_dir / f"{index:02d}.tif"; gff.write_u16(path, image); metric_paths.append(path)
    truth = output / "truth.tif"; gff.write_u16(truth, np.rint(rendered.truth).clip(0, 65535).astype(np.uint16))
    # Censoring is reconstructed against original lighting, before clipping;
    # support is retained from the known warp's zero-valued invalid output.
    member = row.lighting or "unchanged"
    originals = [_lighting_array(rendered.truth, member, index) for index in range(row.frames)]
    preclip = output / "preclip.npz"; np.savez_compressed(preclip, *originals)
    censor = output / "oracle-censor.npz"; np.savez_compressed(censor, *[((image <= 0) | (image >= 65535)) for image in originals])
    coverage = output / "warp-coverage.npz"; np.savez_compressed(coverage, *rendered.warp_coverage)
    transforms = output / "truth-transforms.json"
    transforms.write_text(json.dumps({"middle": [matrix.tolist() for matrix in rendered.transforms_middle], "reference": [matrix.tolist() for matrix in rendered.transforms_reference]}, indent=2, sort_keys=True) + "\n")
    artifacts = {name: {"path": str(path.resolve()), "sha256": _digest(path)} for name, path in
                 {"preclip": preclip, "oracle_censor": censor, "warp_coverage": coverage, "transforms": transforms}.items()}
    manifest = {"configuration": StageCall("synthetic", 0, row, "synthetic").key(), "raw_specification": rendered.raw_specification,
                "frames": [{"path": str(path.resolve()), "sha256": sha256_path(path)} for path in paths],
                "metric_frames": [{"path": str(path.resolve()), "sha256": sha256_path(path)} for path in metric_paths],
                "truth": {"path": str(truth.resolve()), "sha256": sha256_path(truth)}, "artifacts": artifacts, "transforms": artifacts["transforms"],
                "clip_fraction": list(rendered.clip_fraction), "intermediate": "renderer u16 after defocus before warp"}
    destination = output / "manifest.json"; destination.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return {**manifest, "manifest": {"path": str(destination.resolve()), "sha256": _digest(destination)}}


def _expect_refusal(action: object, expected: str) -> None:
    if not callable(action):
        raise AssertionError("self-check action is not callable")
    try:
        action()
    except ValueError as error:
        if str(error) == expected:
            return
        raise AssertionError(f"expected {expected!r}, got {str(error)!r}") from error
    raise AssertionError(f"missing refusal: {expected}")


def _ready_fixture(directory: Path) -> tuple[Path, str, str, Path, dict[str, Path]]:
    """Create unit-only bytes for the source-ready physical graph validator."""
    import shutil

    root = directory / "product"
    root.mkdir()
    source_key = "spikes/alignment-reference-strategies/src/main.rs"
    for relative in REQUIRED_SOURCE_PATHS:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("unit source\n")
    registration = directory / "registration.json"
    registration.write_text("{}\n")

    def binding(path: Path) -> dict[str, str]:
        return {"path": str(path.resolve()), "sha256": _digest(path)}

    snapshots = directory / "snapshots"
    entries = []
    for item in recursive_source_closure(root):
        source, snapshot = root / item.path, snapshots / "source" / item.path
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, snapshot)
        entries.append({"path": item.path, "sha256": item.sha256, "snapshot": binding(snapshot)})
    closure = directory / "closure.json"
    closure.write_text(json.dumps({"entries": entries}, sort_keys=True) + "\n")

    executable_source = Path("/usr/bin/true")
    if not executable_source.is_file() or executable_source.read_bytes()[:4] != b"\x7fELF":
        raise AssertionError("unit fixture ELF is unavailable")
    executables: dict[str, object] = {}
    files: dict[str, Path] = {"registration": registration, "source": root / source_key}
    for name in ("focusforge-cli", "alignment-reference-strategies"):
        live, snapshot = root / "target" / "debug" / name, snapshots / "executables" / name
        live.parent.mkdir(parents=True, exist_ok=True)
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(executable_source, live)
        shutil.copyfile(live, snapshot)
        executables[name] = {**binding(live), "snapshot": binding(snapshot)}
        files[name] = live
    executables_path = directory / "executables.json"
    executables_path.write_text(json.dumps({"executables": executables}, sort_keys=True) + "\n")
    detector = directory / "detector.json"
    detector.write_text("{}\n")
    controls = directory / "controls.json"
    controls.write_text(json.dumps({"fit_free": True, "passed": True, "detectors": binding(detector)}, sort_keys=True) + "\n")
    artifacts = directory / "artifacts.json"
    artifacts.write_text(json.dumps({"schema": "alignment-reference-source-artifacts-v1", "registration": binding(registration),
                                     "source_closure": binding(closure), "executables": binding(executables_path), "controls": binding(controls)}, sort_keys=True) + "\n")

    index = directory / "index.json"
    index.write_text(json.dumps({"entries": [binding(detector)]}, sort_keys=True) + "\n")
    assertions = directory / "assertions.json"
    assertions.write_text(json.dumps({"assertions": [{"passed": True}]}, sort_keys=True) + "\n")
    receipt_files = {}
    for name, text in (("argv", "[]\n"), ("stdout", "\n"), ("stderr", "\n"), ("time", "0\n")):
        path = directory / f"receipt-{name}.txt"
        path.write_text(text)
        receipt_files[name] = binding(path)
    native_exit = directory / "receipt-exit.txt"
    native_exit.write_text("0\n")
    receipt = directory / "receipt.json"
    receipt.write_text(json.dumps({**receipt_files, "native_exit": 0, "time": receipt_files["time"]}, sort_keys=True) + "\n")
    audit = directory / "audit.json"
    audit_document = {"schema": "alignment-reference-audit-v1", "result": "PASS", "registration": binding(registration),
                      "artifact_index": binding(index), "assertions": binding(assertions), "receipt": binding(receipt)}
    audit.write_text(json.dumps(audit_document, sort_keys=True) + "\n")
    ready = directory / "ready.json"
    ready_document = {"schema": "alignment-reference-ready-v1", "state": "source-ready", "registration": binding(registration),
                      "audit": binding(audit), "artifacts": binding(artifacts)}
    ready.write_text(json.dumps(ready_document, sort_keys=True) + "\n")
    files.update({"snapshot": snapshots / "source" / source_key, "index": index, "assertions": assertions,
                  "audit": audit, "ready": ready, "input": directory / "input.bin"})
    files["input"].write_bytes(b"unit input")
    return ready, _digest(ready), _digest(registration), root, files


def _self_check_physical_refusals() -> None:
    """Exercise physical ready-graph refusals with unit-only temporary bytes."""
    import tempfile

    with tempfile.TemporaryDirectory() as temporary:
        ready, expected, registration_digest, root, files = _ready_fixture(Path(temporary))
        if _ready_envelope(ready, expected, "source-ready", registration_digest, root).get("sha256") != expected:
            raise AssertionError("unit ready fixture did not reach the validator")
        _expect_refusal(lambda: _ready_envelope(ready, None, "source-ready", registration_digest, root), "externally pinned ready envelope differs")
        original = files["registration"].read_bytes()
        files["registration"].write_bytes(original + b"x")
        _expect_refusal(lambda: _ready_envelope(ready, expected, "source-ready", registration_digest, root), "ready registration bytes differ")
        files["registration"].write_bytes(original)
        original = files["source"].read_bytes()
        files["source"].write_bytes(original + b"x")
        _expect_refusal(lambda: _ready_envelope(ready, expected, "source-ready", registration_digest, root), "live source closure differs")
        files["source"].write_bytes(original)
        original = files["snapshot"].read_bytes()
        files["snapshot"].write_bytes(original + b"x")
        _expect_refusal(lambda: _ready_envelope(ready, expected, "source-ready", registration_digest, root), "source snapshot bytes differ")
        files["snapshot"].write_bytes(original)
        original = files["focusforge-cli"].read_bytes()
        files["focusforge-cli"].write_bytes(original + b"x")
        _expect_refusal(lambda: _ready_envelope(ready, expected, "source-ready", registration_digest, root), "live executable bytes differ")
        files["focusforge-cli"].write_bytes(original)
        original = files["index"].read_bytes()
        files["index"].write_bytes(original + b"x")
        _expect_refusal(lambda: _ready_envelope(ready, expected, "source-ready", registration_digest, root), "audit index bytes differ")
        files["index"].write_bytes(original)
        _expect_refusal(lambda: _ready_envelope(ready, "f" * 64, "source-ready", registration_digest, root), "externally pinned ready envelope differs")

        def binding(path: Path) -> dict[str, str]:
            return {"path": str(path.resolve()), "sha256": _digest(path)}

        original_audit, original_ready = files["audit"].read_bytes(), files["ready"].read_bytes()
        files["audit"].write_text("[]\n")
        ready_document = _load_object(files["ready"], "unit ready")
        files["ready"].write_text(json.dumps({**ready_document, "audit": binding(files["audit"])}, sort_keys=True) + "\n")
        expected = _digest(files["ready"])
        _expect_refusal(lambda: _ready_envelope(files["ready"], expected, "source-ready", registration_digest, root), "audit summary is not an object")
        files["audit"].write_bytes(original_audit)
        files["ready"].write_bytes(original_ready)
        expected = _digest(files["ready"])
        ready_document = _load_object(files["ready"], "unit ready")
        missing_artifacts = {name: value for name, value in ready_document.items() if name != "artifacts"}
        files["ready"].write_text(json.dumps(missing_artifacts, sort_keys=True) + "\n")
        expected = _digest(files["ready"])
        _expect_refusal(lambda: _ready_envelope(files["ready"], expected, "source-ready", registration_digest, root), "source artifacts binding is absent")
        files["ready"].write_bytes(original_ready)
        expected = _digest(files["ready"])
        _expect_refusal(lambda: _ready_envelope(files["ready"], expected, "parity-ready", registration_digest, root), "ready envelope state differs")

        files["assertions"].write_text(json.dumps({"assertions": [{"passed": False}]}, sort_keys=True) + "\n")
        audit = _load_object(files["audit"], "unit audit")
        audit = {**audit, "assertions": binding(files["assertions"])}
        files["audit"].write_text(json.dumps(audit, sort_keys=True) + "\n")
        ready_document = _load_object(files["ready"], "unit ready")
        files["ready"].write_text(json.dumps({**ready_document, "audit": binding(files["audit"])}, sort_keys=True) + "\n")
        expected = _digest(files["ready"])
        _expect_refusal(lambda: _ready_envelope(files["ready"], expected, "source-ready", registration_digest, root), "audit assertion failed")

        record_files = {}
        for name, contents in (("argv", b"[]\n"), ("stdout", b"\n"), ("stderr", b"\n"), ("native-exit", b"0\n")):
            path = Path(temporary) / f"record-{name}"
            path.write_bytes(contents)
            record_files[name] = binding(path)
        record = Path(temporary) / "record.json"
        record.write_text(json.dumps({"inputs": [binding(files["input"])], "argv": record_files["argv"], "stdout": record_files["stdout"],
                                      "stderr": record_files["stderr"], "native_exit": {**record_files["native-exit"], "value": 0},
                                      "binary": binding(files["alignment-reference-strategies"]), "outputs": [], "result": {}}, sort_keys=True) + "\n")
        record_binding = binding(record)
        _physical_record_identity(record_binding, "unit record")
        original_input = files["input"].read_bytes()
        files["input"].write_bytes(b"changed")
        _expect_refusal(lambda: _physical_record_identity(record_binding, "unit record"), "unit record input bytes differ")
        files["input"].write_bytes(original_input)
        exit_receipt = Path(temporary) / "record-native-exit"
        original_exit = exit_receipt.read_bytes()
        exit_receipt.write_bytes(b"1\n")
        _expect_refusal(lambda: _physical_record_identity(record_binding, "unit record"), "unit record native_exit bytes differ")
        exit_receipt.write_bytes(original_exit)


def _self_check_source_closure_refusals() -> None:
    """Exercise required-path, discovered-child, and root-escape boundaries."""
    import tempfile

    rows = [{"path": path, "sha256": "0" * 64} for path in REQUIRED_SOURCE_PATHS]
    executables = {name: "0" * 64 for name in ("focusforge-cli", "alignment-reference-strategies")}
    validate_source_closure(rows, executables)
    _expect_refusal(lambda: validate_source_closure(rows[:-1], executables), "source closure omits a required dependency")
    escaped = [{**row, "path": "../escape.py"} if row["path"] == "Cargo.toml" else row for row in rows]
    _expect_refusal(lambda: validate_source_closure(escaped, executables), "source closure is incomplete")
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary) / "product"
        root.mkdir()
        for relative in REQUIRED_SOURCE_PATHS:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("unit source\n")
        package = root / "spikes" / "stacking_experiments"
        package.joinpath("__main__.py").write_text("from spikes.stacking_experiments import discovered\n")
        package.joinpath("discovered.py").write_text("unit child\n")
        rust = root / "spikes" / "alignment-reference-strategies" / "src"
        rust.joinpath("main.rs").write_text('#[path = "child.rs"] mod child;\ninclude!("included.rs");\nconst DATA: &str = include_str!("asset.wgsl");\n')
        rust.joinpath("child.rs").write_text("unit child\n")
        rust.joinpath("included.rs").write_text("unit include\n")
        rust.joinpath("asset.wgsl").write_text("unit asset\n")
        root.joinpath("Cargo.toml").write_text('[package]\nbuild = "build-script.rs"\n[lib]\npath = "source-lib.rs"\n[[bin]]\nname = "unit-bin"\npath = "source-bin.rs"\n')
        for name in ("build-script.rs", "source-lib.rs", "source-bin.rs"):
            root.joinpath(name).write_text("unit cargo root\n")
        expected_children = {"spikes/stacking_experiments/discovered.py", "spikes/alignment-reference-strategies/src/child.rs",
                             "spikes/alignment-reference-strategies/src/included.rs", "spikes/alignment-reference-strategies/src/asset.wgsl", "build-script.rs", "source-lib.rs", "source-bin.rs"}
        if not expected_children.issubset({entry.path for entry in recursive_source_closure(root)}):
            raise AssertionError("recursive source closure omitted reachable child")
        outside = Path(temporary) / "escape"
        outside.mkdir()
        outside.joinpath("Cargo.toml").write_text("unit escape\n")
        root.joinpath("Cargo.toml").write_text('[workspace]\nmember = { path = "../escape" }\n')
        _expect_refusal(lambda: recursive_source_closure(root), "source closure contains an unavailable regular file")


def _self_check_input_directory_refusals() -> None:
    """Refuse native scanner alternatives before process dispatch."""
    import tempfile

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        first, second = root / "00.tif", root / "01.tiff"
        first.write_bytes(b"first")
        second.write_bytes(b"second")
        frames = [{"path": str(first.resolve()), "sha256": _digest(first)}, {"path": str(second.resolve()), "sha256": _digest(second)}]
        if _exact_input_directory(frames, "unit native") != root:
            raise AssertionError("native input directory differs")
        _expect_refusal(lambda: _exact_input_directory([frames[1], frames[0]], "unit native"), "unit native input directory membership or order differs")
        for name in ("rogue.TIF", "rogue.TIFF"):
            rogue = root / name
            rogue.write_bytes(b"rogue")
            _expect_refusal(lambda: _exact_input_directory(frames, "unit native"), "unit native input directory membership or order differs")
            rogue.unlink()
        link = root / "rogue-link.TIF"
        link.symlink_to(first)
        _expect_refusal(lambda: _exact_input_directory(frames, "unit native"), "unit native input directory membership or order differs")
        link.unlink()
        override = root / "frames"
        override.mkdir()
        _expect_refusal(lambda: _exact_input_directory(frames, "unit native"), "unit native input directory frames override differs")
        override.rmdir()
        if _exact_input_directory(frames, "unit native") != root:
            raise AssertionError("native input directory did not restore")


def _self_check_selected_render_cache() -> None:
    """Pin selected-frame caching without retaining one full stack per source."""
    import numpy as np

    def expected(source: object, index: int) -> object:
        return np.rint(np.asarray(source, dtype=np.float64) + index * 17.0).clip(0, 65535).astype(np.uint16)

    for count in (8, 63):
        base = np.arange(18, dtype=np.float64).reshape(2, 3, 3)
        calls: list[bytes] = []

        def fake_render(source: object, _: object, __: float, ___: int, ____: int, frames: int) -> list[object]:
            array = np.asarray(source, dtype=np.float64)
            calls.append(array.tobytes())
            return [expected(array, index) for index in range(frames)]

        repeated = [base.copy() for _ in range(count)]
        selected = _render_selected_frames(fake_render, repeated, np.zeros((2, 3), dtype=np.float64), .45, 17, count)
        if len(calls) != 1 or len(selected) != count:
            raise AssertionError("selected render cache did not share duplicate input")
        for index, image in enumerate(selected):
            expected_image = expected(base, index)
            if np.asarray(image).shape != expected_image.shape or np.asarray(image).dtype != np.uint16 or np.asarray(image).tobytes() != expected_image.tobytes():
                raise AssertionError("selected render cache bytes differ")

        calls.clear()
        combined = [base.copy() for _ in range(2 * count)]
        selected = _render_selected_frames(fake_render, combined, np.zeros((2, 3), dtype=np.float64), .45, 17, count)
        if len(calls) != 1 or len(selected) != 2 * count or any(np.asarray(image).tobytes() != expected(base, index % count).tobytes() for index, image in enumerate(selected)):
            raise AssertionError("selected render cache did not share raw and oracle inputs")

        lit = base + 1000.0
        calls.clear()
        isolated = [base.copy() for _ in range(count)]
        isolated[-1] = lit.copy()
        selected = _render_selected_frames(fake_render, isolated, np.zeros((2, 3), dtype=np.float64), .45, 17, count)
        if len(calls) != 2 or np.asarray(selected[-1]).tobytes() != expected(lit, count - 1).tobytes() or np.asarray(selected[0]).tobytes() != expected(base, 0).tobytes():
            raise AssertionError("selected render cache mixed distinct lighting")

        same_bytes = np.arange(12, dtype=np.uint8).reshape(2, 2, 3)
        different_dtype_shape = np.frombuffer(same_bytes.tobytes(), dtype=np.uint16).reshape(2, 3)
        calls.clear()
        separated = [same_bytes.copy() for _ in range(count)] + [different_dtype_shape.copy() for _ in range(count)]
        selected = _render_selected_frames(fake_render, separated, np.zeros((2, 3), dtype=np.float64), .45, 17, count)
        if (len(calls) != 2 or np.asarray(selected[0]).tobytes() != expected(same_bytes, 0).tobytes()
                or np.asarray(selected[count]).tobytes() != expected(different_dtype_shape, 0).tobytes()):
            raise AssertionError("selected render cache merged distinct dtype or shape")

    from . import scenes
    synth = scenes.ss
    row = Configuration("geometry", "anchor", "middle", 8, .45, _seed_text(TRAINING_SEEDS[0]))
    original_height, original_width, original_size = synth.H, synth.W, scenes.IMAGE_SIZE
    try:
        synth.H = synth.W = 256
        scenes.IMAGE_SIZE = (256, 256)
        seed = next(item for item in TRAINING_SEEDS if item.text() == row.seed)
        depth7, _ = scenes._layout(seed.layout)
        depth = depth7 / 7.0 * (row.frames - 1)
        truth = synth.scene_hard(seed.texture).astype(np.float64)
        sources = [_lighting_array(truth, "unchanged", index).clip(0.0, 65535.0) for index in range(row.frames)]
        legacy = [synth.render(source, depth, row.sigma, 0, seed.noise, row.frames)[index] for index, source in enumerate(sources)]
        render_calls = 0

        def counted_render(source: object, render_depth: object, sigma: float, noise: int, render_seed: int, frames: int) -> list[object]:
            nonlocal render_calls
            render_calls += 1
            return synth.render(source, render_depth, sigma, noise, render_seed, frames)

        selected = _render_selected_frames(counted_render, sources, depth, row.sigma, seed.noise, row.frames)
        if render_calls != 1 or any(np.asarray(actual).shape != np.asarray(expected_frame).shape or np.asarray(actual).dtype != np.asarray(expected_frame).dtype or np.asarray(actual).tobytes() != np.asarray(expected_frame).tobytes() for actual, expected_frame in zip(selected, legacy)):
            raise AssertionError("selected render cache changed unchanged sources")
        transforms = synth.warp_transforms(1.0, seed.layout, (row.frames - 1) // 2, row.frames)

        def postwarp(clean: list[object], warp_transforms: list[object]) -> object:
            random = np.random.default_rng(seed.noise ^ 0x5A5A5A5A)
            output = []
            for image, transform in zip(clean, warp_transforms):
                warped = synth.resample(np.asarray(image, dtype=np.float64), transform)
                coverage = scenes._coverage(transform)
                noise = random.normal(0.0, 250.0, warped.shape)
                output.append(np.where(coverage[..., None], np.rint(np.clip(warped + noise, 0, 65535)), 0).astype(np.uint16))
            return np.stack(output)

        selected_postwarp, legacy_postwarp = postwarp(selected, transforms), postwarp(legacy, transforms)
        if selected_postwarp.shape != legacy_postwarp.shape or selected_postwarp.dtype != np.uint16 or selected_postwarp.tobytes() != legacy_postwarp.tobytes():
            raise AssertionError("selected render cache changed seeded postwarp bytes")

        synth.H = synth.W = 4
        scenes.IMAGE_SIZE = (4, 4)
        long = Configuration("geometry", "anchor", "middle", 63, .45, _seed_text(TRAINING_SEEDS[0]))
        long_source = np.arange(48, dtype=np.float64).reshape(4, 4, 3) * 1000.0
        long_depth = np.linspace(0.0, float(long.frames - 1), 16, dtype=np.float64).reshape(4, 4)
        long_sources = [long_source.copy() for _ in range(long.frames)]
        long_legacy = [synth.render(source, long_depth, long.sigma, 0, seed.noise, long.frames)[index] for index, source in enumerate(long_sources)]
        render_calls = 0
        long_selected = _render_selected_frames(counted_render, long_sources, long_depth, long.sigma, seed.noise, long.frames)
        long_transforms = synth.warp_transforms(1.0, seed.layout, (long.frames - 1) // 2, long.frames)
        selected_postwarp, legacy_postwarp = postwarp(long_selected, long_transforms), postwarp(long_legacy, long_transforms)
        if (render_calls != 1 or any(np.asarray(actual).shape != np.asarray(expected_frame).shape or np.asarray(actual).dtype != np.asarray(expected_frame).dtype or np.asarray(actual).tobytes() != np.asarray(expected_frame).tobytes() for actual, expected_frame in zip(long_selected, long_legacy))
                or selected_postwarp.shape != legacy_postwarp.shape or selected_postwarp.dtype != np.uint16 or selected_postwarp.tobytes() != legacy_postwarp.tobytes()):
            raise AssertionError("selected render cache changed n63 actual generator bytes")
    finally:
        synth.H, synth.W, scenes.IMAGE_SIZE = original_height, original_width, original_size


def _self_check_real_raw_refusals() -> None:
    """Use tiny bound files plus an injected header reader; never touch real input."""
    import tempfile

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        paths = []
        for ordinal in range(8):
            path = root / f"{ordinal:02d}.tif"
            path.write_bytes(bytes((ordinal,)))
            paths.append(path)
        frames = [{"index": index, "path": str(path.resolve()), "sha256": _digest(path)} for index, path in zip(REAL_FRAME_INDICES, paths)]
        raw = {"dimensions": list(REAL_DIMENSIONS), "indices": list(REAL_FRAME_INDICES), "frames": frames,
               "references": {"first": 0, "middle": 3}, "roi": REAL_ROI}
        header = type("UnitHeader", (), {"width": 4781, "height": 3136, "samples_per_pixel": 3, "bits_per_sample": (16, 16, 16)})()
        _validate_real_raw(raw, header_reader=lambda _: header)
        _expect_refusal(lambda: _validate_real_raw({**raw, "frames": [*frames[:-1], frames[0]]}, header_reader=lambda _: header), "real raw index order differs")
        duplicate = [*frames]
        duplicate[1] = {**frames[0], "index": REAL_FRAME_INDICES[1]}
        _expect_refusal(lambda: _validate_real_raw({**raw, "frames": duplicate}, header_reader=lambda _: header), "real raw source identity is duplicated")
        copied = root / "copied.tif"
        copied.write_bytes(paths[0].read_bytes())
        duplicate_copy = [*frames]
        duplicate_copy[1] = {"index": REAL_FRAME_INDICES[1], "path": str(copied.resolve()), "sha256": _digest(copied)}
        _expect_refusal(lambda: _validate_real_raw({**raw, "frames": duplicate_copy}, header_reader=lambda _: header), "real raw source identity is duplicated")
        stale = paths[0].read_bytes()
        paths[0].write_bytes(b"stale")
        _expect_refusal(lambda: _validate_real_raw(raw, header_reader=lambda _: header), "real raw bytes differ")
        paths[0].write_bytes(stale)
        bindings = [{"path": str(path.resolve()), "sha256": _digest(path)} for path in paths]
        strict = {"strategy": "anchor", "reference": "middle", "raw_paths": [row["path"] for row in bindings], "raw_hashes": [row["sha256"] for row in bindings],
                  "raw_indices": list(REAL_FRAME_INDICES), "dimensions": list(REAL_DIMENSIONS), "roi_paths": [row["path"] for row in bindings], "roi_hashes": [row["sha256"] for row in bindings],
                  "transforms": [[[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]]] * 8, "statuses": ["converged"] * 8,
                  "output_paths": [row["path"] for row in bindings], "output_hashes": [row["sha256"] for row in bindings], "reader_coverage": [1.] * 8,
                  "prealigned": False, "crop_before_status": False, "roi": REAL_ROI, "solve_window": real_solve_window(*REAL_DIMENSIONS)}
        validate_manifest(strict, "anchor", "middle")
        _expect_refusal(lambda: validate_manifest({name: value for name, value in strict.items() if name != "reader_coverage"}, "anchor", "middle"), "real manifest is incomplete")
        _expect_refusal(lambda: validate_manifest({**strict, "prealigned": True}, "anchor", "middle"), "real manifest bypasses raw-before-crop alignment")
        _expect_refusal(lambda: validate_manifest({**strict, "statuses": ["diverged", *strict["statuses"][1:]]}, "anchor", "middle"), "real manifest has invalid transform or status")


def _self_check_persisted_manifest_replay() -> None:
    """Reload writer bytes into _run_call with unit spies; no renderer or native process."""
    import tempfile
    import numpy as np

    row = Configuration("geometry", "anchor", "middle", 8, .45, _seed_text(TRAINING_SEEDS[0]))
    identity = np.eye(3, dtype=np.float64)
    def renderer(_: Configuration) -> RenderedSynthetic:
        frames = np.full((8, 2, 2, 3), 17, dtype=np.uint16)
        return RenderedSynthetic(frames, frames.copy(), frames[0], [identity.copy() for _ in range(8)], [identity.copy() for _ in range(8)],
                                 np.ones((8, 2, 2), dtype=bool), tuple(frames), (0.,) * 8, "unit-renderer")
    calls = {"native": 0, "resample": 0, "writer": 0}
    original_native, original_resample, original_metrics, original_writer = run_native_strategy, run_product_resample, physical_pixel_metrics, write_synthetic_input
    def fake_native(_: Path, __: Mapping[str, object], strategy: str, reference: str, ___: Path, **____: object) -> Mapping[str, object]:
        calls["native"] += 1
        native = {"record": {"path": "unit-native", "sha256": "0" * 64}, "result": {"strategy": strategy, "reference": 3 if reference == "middle" else 0,
                  "frames": [{"frame": index, "status": "converged", "transform": identity.tolist()} for index in range(8)]}}
        return native
    def fake_resample(_: Path, __: Mapping[str, object], ___: Iterable[object], ____: Path) -> Mapping[str, object]:
        calls["resample"] += 1
        return {"record": {"path": "unit-resample", "sha256": "0" * 64}}
    try:
        globals()["run_native_strategy"], globals()["run_product_resample"], globals()["physical_pixel_metrics"] = fake_native, fake_resample, lambda *_: []
        with tempfile.TemporaryDirectory() as temporary:
            written = original_writer(row, Path(temporary) / "input", renderer=renderer)
            photo_row = Configuration("photometry", "anchor", "middle", 8, .45, _seed_text(TRAINING_SEEDS[0]), "global", "none")
            photo_written = original_writer(photo_row, Path(temporary) / "photo-input", renderer=renderer)
            saved = _load_object(Path(str(written["manifest"]["path"])), "unit persisted manifest")
            if "manifest" in saved or "transforms" not in saved:
                raise AssertionError("writer persistence shape differs")
            replay = {**saved, "manifest": written["manifest"]}
            result = _run_call(StageCall("repeat", 0, row, "unit-repeat"), Path(temporary), Path(temporary) / "spike", Path(temporary) / "out", {}, replay)
            if result["input"] != written["manifest"] or calls != {"native": 1, "resample": 2, "writer": 0}:
                raise AssertionError("persisted manifest replay did not reach unit consumers")
            def cached_writer(_: Configuration, __: Path) -> Mapping[str, object]:
                calls["writer"] += 1
                return photo_written if _.group == "photometry" else written
            globals()["write_synthetic_input"] = cached_writer
            cache: dict[tuple[object, ...], Mapping[str, object]] = {}
            for ordinal, configuration in enumerate((
                    row,
                    Configuration("geometry", "anchor", "first", 8, .45, _seed_text(TRAINING_SEEDS[0])),
                    Configuration("geometry", "rolling", "middle", 8, .45, _seed_text(TRAINING_SEEDS[0])),
                    Configuration("geometry", "rolling", "first", 8, .45, _seed_text(TRAINING_SEEDS[0])),
            )):
                _run_call(StageCall("quality", ordinal, configuration, f"unit-cache-{ordinal}"), Path(temporary), Path(temporary) / "spike",
                          Path(temporary) / "cached", cache)
            if calls != {"native": 5, "resample": 10, "writer": 1}:
                raise AssertionError("source cache did not reuse the physical input")
            photo_record = _run_call(StageCall("quality", 99, photo_row, "unit-gain-after"), Path(temporary), Path(temporary) / "spike",
                                     Path(temporary) / "cached", cache)
            if photo_record.get("gain_after") is None or calls != {"native": 6, "resample": 12, "writer": 2}:
                raise AssertionError("gain-after unit control added an unexpected fit")
    finally:
        globals()["run_native_strategy"], globals()["run_product_resample"], globals()["physical_pixel_metrics"], globals()["write_synthetic_input"] = original_native, original_resample, original_metrics, original_writer


def _self_check_repeat_physical_schema() -> None:
    """Prove all 140 QUALITY/REPEAT rows consume persisted physical records."""
    import shutil
    import tempfile

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = Path("/usr/bin/true")
        if not source.is_file() or source.read_bytes()[:4] != b"\x7fELF":
            raise AssertionError("unit repeat fixture ELF is unavailable")

        def binding(path: Path) -> dict[str, str]:
            return {"path": str(path.resolve()), "sha256": _digest(path)}

        input_manifest = root / "input.json"; input_manifest.write_text("{}\n")
        binary = root / "unit-native"; shutil.copyfile(source, binary)
        argv = root / "argv.json"; argv.write_text("[]\n")
        stdout = root / "stdout.txt"; stdout.write_text("\n")
        stderr = root / "stderr.txt"; stderr.write_text("\n")
        exit_file = root / "exit.txt"; exit_file.write_text("0\n")
        transform_files: dict[str, Path] = {}

        def record(name: str) -> dict[str, str]:
            path = root / f"{name}.json"
            document: dict[str, object] = {"inputs": [binding(input_manifest)], "argv": binding(argv), "stdout": binding(stdout),
                                            "stderr": binding(stderr), "native_exit": {**binding(exit_file), "value": 0},
                                            "binary": binding(binary), "outputs": [], "result": {"kind": name}}
            if name != "native":
                transforms = root / f"{name}-transforms.csv"
                transforms.write_text("0,1,0,0,0,1,0,0,0,1\n")
                transform_files[name] = transforms
                document["transforms"] = binding(transforms)
            path.write_text(json.dumps(document, sort_keys=True) + "\n")
            return binding(path)

        native, fitted, truth = record("native"), record("fitted"), record("truth")
        rows = [{"call": {"ordinal": call.ordinal, "key": call.key()}, "input": binding(input_manifest),
                 "native": native, "fitted_resample": fitted, "truth_resample": truth}
                for call in stage_calls("quality")]
        if len(rows) != 140:
            raise AssertionError("unit repeat fixture matrix differs")
        digest = "a" * 64
        quality = {"stage": "quality", "sha256": digest, "records": rows}
        verify_repeat(quality, {"records": list(rows)}, digest)
        _expect_refusal(lambda: verify_repeat(quality, {"records": list(rows)}, "b" * 64), "repeat quality digest differs")
        original_transforms = transform_files["fitted"].read_bytes()
        transform_files["fitted"].write_bytes(original_transforms + b"changed\n")
        _expect_refusal(lambda: verify_repeat(quality, {"records": list(rows)}, digest), "QUALITY fitted_resample transforms bytes differ")
        transform_files["fitted"].write_bytes(original_transforms)
        original_truth = transform_files["truth"].read_bytes()
        transform_files["truth"].unlink()
        _expect_refusal(lambda: verify_repeat(quality, {"records": list(rows)}, digest), "QUALITY truth_resample transforms bytes differ")
        transform_files["truth"].write_bytes(original_truth)
        verify_repeat(quality, {"records": list(rows)}, digest)
        original_input = input_manifest.read_bytes()
        input_manifest.write_text('{"changed":true}\n')
        _expect_refusal(lambda: verify_repeat(quality, {"records": list(rows)}, digest), "QUALITY repeat input bytes differ")
        input_manifest.write_bytes(original_input)


def _self_check_repeat_prefit_refusal() -> None:
    """Run the registered REPEAT gateway with spies and prove no fit starts before QUALITY pinning."""
    import tempfile

    original_admission, original_bind, original_run = validate_admission, _bind_admitted_executable, _run_call
    calls = {"bind": 0, "run": 0}
    def admitted(*_: object, **__: object) -> Mapping[str, object]:
        return {"source": {"envelope": {}}}
    def bound(*_: object, **__: object) -> None:
        calls["bind"] += 1
    def forbidden(*_: object, **__: object) -> Mapping[str, object]:
        calls["run"] += 1
        raise AssertionError("prefit refusal reached a call")
    try:
        globals()["validate_admission"], globals()["_bind_admitted_executable"], globals()["_run_call"] = admitted, bound, forbidden
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _expect_refusal(lambda: execute_registered_stage("repeat", root / "registration.json", None, None, None, root,
                                                              root / "spike", root / "cli", root / "output"),
                            "repeat requires the externally pinned immutable QUALITY record")
    finally:
        globals()["validate_admission"], globals()["_bind_admitted_executable"], globals()["_run_call"] = original_admission, original_bind, original_run
    if calls != {"bind": 2, "run": 0}:
        raise AssertionError("repeat QUALITY refusal did not precede fit dispatch")


def self_checks() -> list[str]:
    """Fit-free checks for fixed identities, inverses, bounds, and refusals."""
    import numpy as np
    if len(configurations()) != 240 or stage_counts() != {"calibration": 200, "quality": 140, "repeat": 140, "parity": 4, "controls": 2, "real": 6, "total": 492}:
        raise AssertionError("configuration cap differs")
    calls = stage_calls("calibration")
    if [call.label for call in calls[:4]] != ["CAL-anchor-middle-n8-s0.45-seed1-r0", "CAL-anchor-middle-n8-s0.45-seed1-r1", "CAL-chain-middle-n8-s0.45-seed1-r0", "CAL-chain-middle-n8-s0.45-seed1-r1"]:
        raise AssertionError("calibration order differs")
    truth = [[1., 0., 2.], [0., 1., -3.], [0., 0., 1.]]
    identical = corner_errors(truth, truth)
    inverse = corner_errors(np.linalg.inv(np.asarray(truth)).tolist(), np.linalg.inv(np.asarray(truth)).tolist())
    translated = [[1., 0., 2.5], [0., 1., -3.], [0., 0., 1.]]
    half_pixel = corner_errors(translated, truth)
    if identical["corner_rms"] != 0.0 or inverse["corner_max"] != 0.0 or half_pixel["corner_rms"] != .5 or half_pixel["corner_max"] != .5:
        raise AssertionError("corner inverse differs")
    _expect_refusal(lambda: corner_errors(truth, [[1., 0., 0.], [0., 0., 0.], [0., 0., 1.]]), "truth transform is not invertible")
    _expect_refusal(lambda: corner_errors([[True, 0., 0.], [0., 1., 0.], [0., 0., 1.]], truth), "corner transform or dimensions differ")
    _expect_refusal(lambda: corner_errors([[math.nan, 0., 0.], [0., 1., 0.], [0., 0., 1.]], truth), "corner transform or dimensions differ")
    gain_after_is_identical(np.array([[1, 2]], dtype=np.uint16), np.array([[1, 2]], dtype=np.uint16))
    try:
        gain_after_is_identical(np.array([[1]], dtype=np.uint16), np.array([[1]], dtype=np.uint32))
    except ValueError:
        pass
    else:
        raise AssertionError("gain-after byte difference accepted")
    pairs = {seed.text(): (1.0, 1.0) for seed in TRAINING_SEEDS}
    if allowance(pairs, pairs) != 0.0 or error_budget([1.0] * 10, 0.0) != 1.0:
        raise AssertionError("zero allowance differs")
    required = _required_allowance_projections()
    allowance_rows = []
    for projection in sorted(required, key=repr):
        table, group, reference, frames, sigma, member, order, metric, frame_group = projection
        allowance_rows.append({"control_table": table, "group": group, "reference": reference, "frames": frames,
                               "sigma": sigma, "lighting_member": member, "fit_order": order, "metric": metric,
                               "frame_group": frame_group, "calibration": {seed.text(): [0.0, 0.0] for seed in TRAINING_SEEDS},
                               "allowance": 0.0, "corner_budget": 0.0 if metric == "corner_rms" and frame_group != "stack" else None})
    _validate_allowance_document({"allowances": allowance_rows})
    _expect_refusal(lambda: _validate_allowance_document({"allowances": allowance_rows[:-1]}), "allowance table is incomplete")
    _expect_refusal(lambda: _validate_allowance_document({"allowances": allowance_rows + [allowance_rows[0]]}), "allowance key projection differs")
    wrong_budget = [{**row, "corner_budget": 1.0} if row["corner_budget"] is not None else row for row in allowance_rows]
    _expect_refusal(lambda: _validate_allowance_document({"allowances": wrong_budget}), "corner budget differs")
    wrong_axis = [{**row, "fit_order": "gain-before"} if row is allowance_rows[0] else row for row in allowance_rows]
    _expect_refusal(lambda: _validate_allowance_document({"allowances": wrong_axis}), "allowance key projection differs")
    nonfinite = [{**row, "allowance": math.nan} if row is allowance_rows[0] else row for row in allowance_rows]
    _expect_refusal(lambda: _validate_allowance_document({"allowances": nonfinite}), "value is not finite")
    if verdict(False, True, False, True, True)["state"] != "DEFER" or verdict(True, False, False, True, True)["state"] != "NO_GO" or verdict(True, True, False, True, True)["state"] != "GO":
        raise AssertionError("verdict state differs")
    sample = configurations()[0]
    for invalid in ("frame-08", "frame--1", "frame-x"):
        try:
            metric_key(sample, "corner_rms", invalid, "geometry_anchor")
        except ValueError:
            continue
        raise AssertionError("invalid metric frame accepted")
    try:
        _validate_real_raw({"dimensions": [1, 1], "indices": [], "frames": []})
    except ValueError:
        pass
    else:
        raise AssertionError("invalid real input accepted")
    native = {"strategy": "anchor", "reference": 3, "frames": [
        {"frame": index, "status": "converged", "reason": None, "iterations": 1, "iterations_reason": None,
         "read_error": None, "best_rho": 1.0, "solver_fraction": None, "validation_fraction": None,
         "solver_fraction_reason": "not-exposed", "validation_fraction_reason": "not-exposed", "interpolation_count": 0,
         "fitting_template_coverage": None, "fitting_moving_coverage": None, "fitting_window": None, "fitting_template_frame": None,
         "fitting_prewarp": None, "fitting_reader": None,
         "transform": [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]}
        for index in range(8)
    ]}
    expected_native = Configuration("geometry", "anchor", "middle", 8, .45, _seed_text(TRAINING_SEEDS[0]))
    _valid_frame_record(native, expected_native)
    for status, read_error in (("converged", None), ("hit-cap", None), ("stalled", None), ("diverged", None), ("read-error", "unit reader error")):
        accepted = {**native, "frames": [{**frame, "status": status, "read_error": read_error} if frame["frame"] == 0 else frame for frame in native["frames"]]}
        _valid_frame_record(accepted, expected_native)
    for altered, message in (
        ({**native, "frames": [{**frame, "status": "not-a-status"} if frame["frame"] == 0 else frame for frame in native["frames"]]}, "native status differs"),
        ({**native, "frames": [{name: value for name, value in frame.items() if name != "transform"} if frame["frame"] == 0 else frame for frame in native["frames"]]}, "native frame schema is incomplete"),
        ({**native, "frames": [{**frame, "transform": [[True, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]} if frame["frame"] == 0 else frame for frame in native["frames"]]}, "native transform is invalid"),
        ({**native, "frames": [{**frame, "frame": 1} if frame["frame"] == 0 else frame for frame in native["frames"]]}, "native frame identity differs"),
        ({**native, "frames": [{**frame, "read_error": "unexpected"} if frame["frame"] == 0 else frame for frame in native["frames"]]}, "native successful frame carries a read error"),
    ):
        _expect_refusal(lambda altered=altered: _valid_frame_record(altered, expected_native), message)
    all_calls = {stage: stage_calls(stage) for stage in STAGES}
    if sum(len(calls) for calls in all_calls.values()) != 492 or len(all_calls["source"]) != 0:
        raise AssertionError("registered invocation total differs")
    if any(len({(json.dumps(call.key(), sort_keys=True), call.label) for call in calls}) != len(calls) for calls in all_calls.values()):
        raise AssertionError("registered stage identities differ")
    if len({json.dumps(call.key(), sort_keys=True) for call in stage_calls("quality")}) != 140 or len({json.dumps(call.key(), sort_keys=True) for call in stage_calls("repeat")}) != 140:
        raise AssertionError("quality-repeat identities differ")
    if {json.dumps(call.key(), sort_keys=True) for call in stage_calls("quality")} != {json.dumps(call.key(), sort_keys=True) for call in stage_calls("repeat")}:
        raise AssertionError("repeat does not reuse the immutable quality matrix")
    if [(call.configuration.strategy, call.configuration.reference) for call in stage_calls("parity")] != [("anchor", "middle"), ("anchor", "middle"), ("chain", "middle"), ("chain", "middle")]:
        raise AssertionError("CLI parity identities differ")
    _expect_refusal(lambda: _ready_envelope(None, None, "source-ready", "0" * 64, Path("/nonexistent-root")), "externally pinned ready envelope differs")
    _expect_refusal(lambda: verify_repeat({"stage": "calibration", "sha256": "0" * 64, "records": []}, {"records": []}, "0" * 64), "repeat requires a quality stage")
    _self_check_source_closure_refusals()
    _self_check_input_directory_refusals()
    _self_check_selected_render_cache()
    _self_check_real_raw_refusals()
    _self_check_persisted_manifest_replay()
    _self_check_repeat_physical_schema()
    _self_check_repeat_prefit_refusal()
    _self_check_physical_refusals()
    return ["matrix", "calibration-order", "corner-known-answer", "gain-after-bytes", "allowance-completeness", "zero-allowance", "verdict", "metric-refusal", "real-refusal", "native-schema-refusal", "quality-before-repeat-refusal", "enumeration", "external-proof-refusal", "source-closure-refusal", "native-input-directory-refusal", "selected-render-cache", "strict-real-refusal", "persisted-manifest-replay", "repeat-physical-schema", "repeat-prefit-refusal", "physical-ready-graph-refusal"]
