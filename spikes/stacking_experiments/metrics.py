# SPDX-License-Identifier: GPL-3.0-or-later
"""Metric wrappers with explicit missing-region behavior."""

from __future__ import annotations

from pathlib import Path
import sys
from typing import Any

import numpy as np

ROOT = Path(__file__).resolve().parents[2]
GFF_DIR = ROOT / "spikes" / "guided-filter-fusion"
SYNTH_DIR = ROOT / "spikes" / "synthetic-ground-truth"
for directory in (GFF_DIR, SYNTH_DIR):
    if str(directory) not in sys.path:
        sys.path.insert(0, str(directory))
import gff_fusion as gff  # noqa: E402


def _mask(shape: tuple[int, int], rect: list[int]) -> np.ndarray:
    mask = np.zeros(shape, dtype=bool)
    y0, y1, x0, x1 = rect
    mask[y0:y1, x0:x1] = True
    return mask


def seam_energy(comp: np.ndarray, truth: np.ndarray, vertical: tuple[int, ...], horizontal: tuple[int, ...]) -> float | None:
    """Mean residual-luma step across registered interior tile boundaries."""
    if not vertical and not horizontal:
        return None
    residual = comp.astype(np.float64).mean(2) - truth.astype(np.float64).mean(2)
    samples = [np.abs(residual[:, line] - residual[:, line - 1]) for line in vertical if 0 < line < residual.shape[1]]
    samples.extend(np.abs(residual[line] - residual[line - 1]) for line in horizontal if 0 < line < residual.shape[0])
    return None if not samples else float(np.concatenate([sample.ravel() for sample in samples]).mean())


def tile_error(tiled: np.ndarray, one_tile: np.ndarray) -> float:
    """Largest encoded-sample difference between tile plans."""
    return float(np.abs(tiled.astype(np.int64) - one_tile.astype(np.int64)).max())


def label_metrics(zmap: np.ndarray, truth: np.ndarray, valid: np.ndarray) -> dict[str, float | None]:
    """Keep exact, within-one, and mean absolute label errors distinct."""
    if not valid.any():
        return {"label_exact": None, "label_within_one": None, "label_mad": None}
    delta = np.abs(zmap.astype(np.int64) - truth.astype(np.int64))[valid]
    return {"label_exact": float((delta == 0).mean()), "label_within_one": float((delta <= 1).mean()),
            "label_mad": float(delta.mean())}


def measure(comp: np.ndarray, truth: np.ndarray, metadata: dict[str, Any], source: object | None = None,
            vertical_lines: tuple[int, ...] = (), horizontal_lines: tuple[int, ...] = ()) -> dict[str, float | None]:
    """Measure registered image quantities; unavailable regions are None."""
    raw = gff.metrics(comp, truth, metadata, source)
    edge = gff.glow_measures(comp, truth)
    values: dict[str, float | None] = {
        "psnr": raw.get("psnr"),
        "ssim": raw.get("ssim"),
        "edge_halo": edge.get("glow"),
        "hair_retention": raw.get("hair_ret"),
        "flat_noise": raw.get("flat_std"),
        "chroma_drift": raw.get("chroma_err"),
        "border_contamination": raw.get("border_pull"),
        "seam_energy": seam_energy(comp, truth, vertical_lines, horizontal_lines),
        "gradient_energy": None,
        "map_coherence": None,
    }
    for name, value in tuple(values.items()):
        if isinstance(value, (float, np.floating)) and not np.isfinite(value):
            values[name] = None
    return values
