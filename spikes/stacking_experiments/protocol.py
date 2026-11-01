# SPDX-License-Identifier: GPL-3.0-or-later
"""Fixed experiment controls shared by stack-fusion studies."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Literal

SeedSplit = Literal["training", "holdout"]


@dataclass(frozen=True)
class SeedTriple:
    """Independent layout, texture, and sensor-noise seeds."""

    layout: int
    texture: int
    noise: int

    @classmethod
    def parse(cls, value: str) -> "SeedTriple":
        try:
            layout, texture, noise = (int(part) for part in value.split(","))
        except ValueError as error:
            raise ValueError("seed must be layout,texture,noise") from error
        return cls(layout, texture, noise)

    def text(self) -> str:
        return f"{self.layout},{self.texture},{self.noise}"


TRAINING_SEEDS = tuple(SeedTriple(101 + i, 201 + i, 301 + i) for i in range(5))
HOLDOUT_SEEDS = tuple(SeedTriple(111 + i, 211 + i, 311 + i) for i in range(3))
ADVANCEMENT_MULTIPLIER = 2
IMAGE_SIZE = (512, 512)
FRAME_COUNT = 8
SIGMA_STEP = 0.45


@dataclass(frozen=True)
class Metric:
    """A reported quantity, its region, and its intended direction."""

    name: str
    definition: str
    region: str
    direction: Literal["higher", "lower", "diagnostic"]
    objective: bool


METRICS = (
    Metric("psnr", "10*log10(65535^2 / mean encoded-sample squared error)", "valid image samples", "higher", True),
    Metric("ssim", "mean 8x8 luma SSIM with C1=(.01*65535)^2 and C2=(.03*65535)^2", "valid image; invalid pixels filled from truth", "higher", True),
    Metric("edge_halo", "dark-side 4..24 px residual-luma error minus far-side residual", "truth 3x3-box luma edges >=2000 counts; registered distance bands", "lower", True),
    Metric("hair_retention", "mean measured line contrast / truth line contrast", "generated 1 px and 2 px hair segments", "higher", True),
    Metric("flat_noise", "standard deviation of luma residual", "generated near-flat regions", "lower", True),
    Metric("chroma_drift", "mean absolute normalized-channel residual", "valid, unsaturated pixels with positive luma", "lower", True),
    Metric("border_contamination", "mean luma shortfall below darkest valid source", "pixels with both valid and invalid sources", "lower", True),
    Metric("seam_energy", "mean absolute residual-luma step across each registered tile line", "registered interior tile lines", "lower", True),
    Metric("tile_difference", "maximum absolute encoded-sample difference", "tiled versus one-tile valid image", "lower", True),
    Metric("label_exact", "fraction of source labels exactly equal to truth", "valid source labels", "higher", True),
    Metric("label_within_one", "fraction of source labels within one of truth", "valid source labels", "higher", True),
    Metric("label_mad", "mean absolute source-label difference", "valid source labels", "lower", True),
    Metric("geometry_error", "per-frame RMS and maximum Euclidean corner error of recovered transform composed with inverse truth", "four 512 px image corners after truth composition; pixels", "lower", True),
    Metric("alignment_drift", "least-squares slope of truth-composed per-frame corner RMS against hop distance", "non-reference frames; pixels per hop", "lower", True),
    Metric("alignment_failures", "count of diverged or registered-error-budget frames with their status", "all frames; diverged|over-budget", "lower", True),
    Metric("gradient_energy", "mean squared luma-gradient magnitude", "valid image", "diagnostic", False),
    Metric("map_coherence", "source-map neighborhood coherence", "valid source labels", "diagnostic", False),
)


@dataclass(frozen=True)
class Family:
    """A deterministic scene family and its controls."""

    name: str
    parameters: dict[str, object]
    fusion_baseline: bool


FAMILIES = (
    Family("thin-hairs", {"widths_px": [1, 2], "sigma_step": SIGMA_STEP}, True),
    Family("flat-noise", {"members": ["noise-250", "noise-1000"], "noise_counts": [250, 1000], "sigma_step": SIGMA_STEP}, True),
    Family("colour-edges", {"chroma_step": "depth-crossing", "sigma_step": SIGMA_STEP}, True),
    Family("invalid-borders", {"members": ["zero-strip", "aligned-coverage"], "sigma_step": SIGMA_STEP}, True),
    Family("known-warp", {"model": "similarity", "transform_seed": "layout", "noise_seed": "independent"}, False),
    Family("controlled-lighting", {"members": ["global", "channel", "spatial", "unchanged"], "noise_seed": "independent"}, False),
)
GENERATOR_PARAMETERS = {
    "layout": {
        "plane_x": 0.33,
        "plane_y": 0.19,
        "step_size": [116, 154],
        "step_origin_y_inclusive": [48, 347],
        "step_origin_x_inclusive": [48, 309],
        "step_depth_offset": 0.42,
        "cap_center_fraction": [0.36, 0.64],
        "cap_radius_fraction": 0.25,
        "cap_depth_scale": 0.31,
    },
    "thin_hairs": {
        "horizontal_y_inclusive": [290, 369],
        "horizontal_x_start_inclusive": [48, 119],
        "horizontal_length": 180,
        "horizontal_width": 1,
        "horizontal_counts_offset": -9000,
        "horizontal_depth_offset": 1.1,
        "vertical_x_inclusive": [48, 119],
        "vertical_y_start": "horizontal_y - 58",
        "vertical_length": 146,
        "vertical_width": 2,
        "vertical_counts_offset": 7500,
        "vertical_depth_offset": -1.0,
    },
    "flat_noise": {"rect_size": [60, 70], "origin_y_inclusive": [48, 403], "origin_x_inclusive": [48, 393], "truth_counts": 32768, "noise_counts": [250, 1000]},
    "colour_edges": {"boundary": "horizontal through step midpoint, intersecting both vertical step sides", "red_gain": 1.07, "blue_gain": 0.91},
    "invalid_borders": {"zero_strip_width_inclusive": [6, 17], "alternating_sides": True, "known_warp_amplitude": 1.0, "reference_frame": 4},
    "known_warp": {"similarity_amplitude": 1.0, "reference_frame": 4, "transform_seed": "layout", "noise_seed": "independent"},
    "controlled_lighting": {"global_gain": "1 + 0.06*position", "global_offset_counts": "220*position", "channel_gain": [0.06, -0.04, 0.03], "channel_offset_counts": [180, -140, 110], "spatial_multiplier": "1 + 0.025*linear-x", "position": "(frame-4)/4", "noise_counts": 250, "clipping": "fraction before encoding, recorded per frame"},
}

BUDGETS = {
    "baseline_records": "360 core records (6 members x 5 seeds x 3 methods x 2 repeats x 2 tiles)",
    "context_records": "3 equalized context records and 6 real-crop outputs",
    "sessions": "3 sessions; each has a 2-hour measured-command wall cap",
    "memory": "record peak host RSS; CPU runs do not use VRAM",
    "temperature": "cold and warm labels appear only when measured",
}

REAL_CROPS = {
    "stack": {
        "rect_xywh": [1878, 1056, 1024, 1024],
        "montage_rect_y0y1x0x1": [0, 1024, 0, 1024],
        "frame_indices": [0, 8, 17, 26, 34, 43, 52, 60],
        "frame_count": 61,
        "header": "4781x3136",
        "reference_frame": 3,
    },
}


def split_for(seed: SeedTriple) -> SeedSplit:
    """Return the registered split for a complete seed triple."""
    if seed in TRAINING_SEEDS:
        return "training"
    if seed in HOLDOUT_SEEDS:
        return "holdout"
    raise ValueError(f"unknown seed triple: {seed.text()}")


def require_seed(seed: SeedTriple, allow_holdout: bool = False) -> SeedSplit:
    """Reject sealed seeds until a caller explicitly enables them."""
    split = split_for(seed)
    if split == "holdout" and not allow_holdout:
        raise ValueError("sealed hold-out split requires --allow-holdout")
    return split


def registration() -> dict[str, object]:
    """Return serializable experiment controls for a durable report."""
    return {
        "image_size": list(IMAGE_SIZE),
        "frames": FRAME_COUNT,
        "sigma_step": SIGMA_STEP,
        "training_seeds": [asdict(seed) for seed in TRAINING_SEEDS],
        "holdout_seeds": [asdict(seed) for seed in HOLDOUT_SEEDS],
        "families": [asdict(family) for family in FAMILIES],
        "generator_parameters": GENERATOR_PARAMETERS,
        "metrics": [asdict(metric) for metric in METRICS],
        "allowance_procedure": {
            "seed_spread": "max minus min across training seeds",
            "repeat_spread": "max minus min across identical repeats",
            "allowance": "max(seed_spread, repeat_spread)",
        },
        "advancement": {
            "multiplier": ADVANCEMENT_MULTIPLIER,
            "rule": "improvement must exceed multiplier times the registered allowance",
            "hard_regression": "a regression above one allowance on any training seed is no-go",
        },
        "budgets": BUDGETS,
        "real_crops": REAL_CROPS,
    }


def validate_registration() -> None:
    """Assert the fixed split and protocol invariants."""
    if len(TRAINING_SEEDS) != 5 or len(HOLDOUT_SEEDS) != 3:
        raise AssertionError("unexpected seed counts")
    for axis in ("layout", "texture", "noise"):
        train = {getattr(seed, axis) for seed in TRAINING_SEEDS}
        holdout = {getattr(seed, axis) for seed in HOLDOUT_SEEDS}
        if train & holdout:
            raise AssertionError(f"{axis} split overlaps")
    if len(FAMILIES) != 6:
        raise AssertionError("expected six scene families")
