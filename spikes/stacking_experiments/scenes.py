# SPDX-License-Identifier: GPL-3.0-or-later
"""Synthetic scene construction with independent random axes."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import sys

import numpy as np

from .protocol import FRAME_COUNT, IMAGE_SIZE, SIGMA_STEP, Family, SeedTriple

ROOT = Path(__file__).resolve().parents[2]
SYNTH_DIR = ROOT / "spikes" / "synthetic-ground-truth"
if str(SYNTH_DIR) not in sys.path:
    sys.path.insert(0, str(SYNTH_DIR))
import synth_stack as ss  # noqa: E402


@dataclass(frozen=True)
class SceneData:
    """Rendered frames, unblurred truth, depth, and generated regions."""

    frames: np.ndarray
    truth: np.ndarray
    depth: np.ndarray
    metadata: dict[str, object]


def family_members(family: Family) -> tuple[str, ...]:
    """Return the concrete registered members of one scene family."""
    if family.name == "flat-noise":
        return ("noise-250", "noise-1000")
    if family.name == "invalid-borders":
        return ("zero-strip", "aligned-coverage")
    if family.name == "controlled-lighting":
        return ("global", "channel", "spatial", "unchanged")
    return ("default",)


def _rect(seed: int, h: int, w: int) -> tuple[int, int, int, int]:
    rng = np.random.default_rng(seed)
    y = int(rng.integers(48, IMAGE_SIZE[0] - h - 48))
    x = int(rng.integers(48, IMAGE_SIZE[1] - w - 48))
    return y, y + h, x, x + w


def _layout(seed: int, frame_count: int = FRAME_COUNT) -> tuple[np.ndarray, dict[str, object]]:
    height, width = IMAGE_SIZE
    yy, xx = np.mgrid[:height, :width]
    plane = 0.33 * xx / (width - 1) + 0.19 * yy / (height - 1)
    step = _rect(seed + 1, 116, 154)
    plane[step[0]:step[1], step[2]:step[3]] += 0.42
    cy, cx, radius = height * 0.64, width * 0.36, 0.25 * width
    cap = np.maximum(0.0, 1.0 - ((xx - cx) ** 2 + (yy - cy) ** 2) / radius**2)
    plane += 0.31 * np.sqrt(cap)
    depth = (plane - plane.min()) / (plane.max() - plane.min()) * (frame_count - 1)
    hair_y = int(np.random.default_rng(seed + 3).integers(290, 370))
    hair_x = int(np.random.default_rng(seed + 4).integers(48, 120))
    hairs = [("h", (hair_y, hair_y + 1), (hair_y - 3, hair_y - 1), (hair_y + 2, hair_y + 4), (hair_x, hair_x + 180)),
             ("v", (hair_x, hair_x + 2), (hair_x - 4, hair_x - 1), (hair_x + 3, hair_x + 6), (hair_y - 58, hair_y + 88))]
    return depth, {"step": list(step), "halo": list(step), "flat": list(_rect(seed + 2, 60, 70)), "hairs": hairs}


def _apply_hairs(truth: np.ndarray, depth: np.ndarray, hairs: list[tuple[object, ...]], frame_count: int = FRAME_COUNT) -> None:
    for kind, line, _, _, extent in hairs:
        line_slice = slice(*line)
        if kind == "h":
            truth[line_slice, extent[0]:extent[1]] = np.clip(truth[line_slice, extent[0]:extent[1]].astype(np.int32) - 9000, 0, 65535)
            depth[line_slice, extent[0]:extent[1]] = np.clip(depth[line_slice, extent[0]:extent[1]] + 1.1, 0, frame_count - 1)
        else:
            truth[extent[0]:extent[1], line_slice] = np.clip(truth[extent[0]:extent[1], line_slice].astype(np.int32) + 7500, 0, 65535)
            depth[extent[0]:extent[1], line_slice] = np.clip(depth[extent[0]:extent[1], line_slice] - 1.0, 0, frame_count - 1)


def _apply_colour_boundary(truth: np.ndarray, step: list[int]) -> int:
    y0, y1, _, _ = step
    boundary = (y0 + y1) // 2
    truth[boundary:, :, 0] = np.clip(truth[boundary:, :, 0].astype(np.float64) * 1.07, 0, 65535)
    truth[boundary:, :, 2] = np.clip(truth[boundary:, :, 2].astype(np.float64) * 0.91, 0, 65535)
    return boundary


def _coverage(transform: np.ndarray) -> np.ndarray:
    height, width = IMAGE_SIZE
    inverse = np.linalg.inv(transform)
    yy, xx = np.mgrid[:height, :width]
    xy1 = np.stack((xx, yy, np.ones_like(xx)), axis=-1).astype(np.float64)
    source = xy1 @ inverse.T
    return ((source[..., 0] >= 0) & (source[..., 0] <= width - 1) & (source[..., 1] >= 0) & (source[..., 1] <= height - 1))


def _render_independent_sources(sources: np.ndarray, depth: np.ndarray, noise_seed: int) -> np.ndarray:
    rng = np.random.default_rng(noise_seed)
    output = []
    for index, source in enumerate(sources):
        clean = ss.render(source, depth, SIGMA_STEP, 0, noise_seed, FRAME_COUNT)[index]
        noisy = clean.astype(np.float64) + rng.normal(0.0, 250.0, clean.shape)
        output.append(np.rint(np.clip(noisy, 0, 65535)).astype(np.uint16))
    return np.stack(output)


def render_family(family: Family, seed: SeedTriple, member: str | None = None, frame_count: int = FRAME_COUNT) -> SceneData:
    """Render one family member without mixing layout, texture, or noise seeds.

    Only `thin-hairs` and `flat-noise` render at other frame counts; the depth range scales to `frame_count`.
    """
    if frame_count != FRAME_COUNT and family.name not in ("thin-hairs", "flat-noise"):
        raise ValueError(f"{family.name} renders at {FRAME_COUNT} frames only")
    if IMAGE_SIZE != (ss.H, ss.W):
        raise ValueError("synthetic renderer geometry differs from registered geometry")
    member = member or family_members(family)[0]
    if member not in family_members(family):
        raise ValueError(f"unknown {family.name} member: {member}")
    depth, metadata = _layout(seed.layout, frame_count)
    truth = ss.scene_hard(seed.texture).astype(np.float64)
    if family.name == "thin-hairs":
        _apply_hairs(truth, depth, metadata["hairs"], frame_count)
    if family.name == "flat-noise":
        y0, y1, x0, x1 = metadata["flat"]
        truth[y0:y1, x0:x1] = 32768
    if family.name == "colour-edges":
        metadata["colour_boundary_y"] = _apply_colour_boundary(truth, metadata["step"])
    noise = 1000 if member == "noise-1000" else 250
    frames = np.stack(ss.render(truth, depth, SIGMA_STEP, noise, seed.noise, frame_count))
    metadata.update({"family": family.name, "member": member, "layout_seed": seed.layout, "texture_seed": seed.texture,
                     "noise_seed": seed.noise, "noise_counts": noise, "frames": frame_count, "sigma_step": SIGMA_STEP})
    if family.name == "invalid-borders":
        if member == "zero-strip":
            masks = np.ones((FRAME_COUNT, *IMAGE_SIZE), dtype=bool)
            for frame in range(FRAME_COUNT):
                strip = int(np.random.default_rng(seed.layout + frame).integers(6, 18))
                if frame % 2:
                    masks[frame, :, :strip] = False
                else:
                    masks[frame, :, -strip:] = False
            frames[~masks] = 0
        else:
            transforms = ss.warp_transforms(1.0, seed.layout, FRAME_COUNT // 2, FRAME_COUNT)
            masks = np.stack([_coverage(transform) for transform in transforms])
            frames = np.stack([np.rint(ss.resample(frame, transform)).clip(0, 65535).astype(np.uint16)
                               for frame, transform in zip(frames, transforms)])
            frames[~masks] = 0
            metadata["transforms"] = [transform.tolist() for transform in transforms]
        metadata["coverage"] = masks
    if family.name == "known-warp":
        transforms = ss.warp_transforms(1.0, seed.layout, FRAME_COUNT // 2, FRAME_COUNT)
        masks = np.stack([_coverage(transform) for transform in transforms])
        frames = np.stack([np.rint(ss.resample(frame, transform)).clip(0, 65535).astype(np.uint16)
                           for frame, transform in zip(frames, transforms)])
        frames[~masks] = 0
        metadata.update({"coverage": masks, "transforms": [transform.tolist() for transform in transforms], "reference_frame": FRAME_COUNT // 2})
    if family.name == "controlled-lighting":
        position = (np.arange(FRAME_COUNT) - FRAME_COUNT // 2) / max(1, FRAME_COUNT // 2)
        sources = np.repeat(truth[None].astype(np.float64), FRAME_COUNT, axis=0)
        controls: dict[str, object] = {"member": member}
        if member == "global":
            gain = 1.0 + 0.06 * position
            offset = 220.0 * position
            sources = sources * gain[:, None, None, None] + offset[:, None, None, None]
            controls.update({"global_gain": gain.tolist(), "global_offset_counts": offset.tolist()})
        elif member == "channel":
            gain = np.stack((1.0 + 0.06 * position, 1.0 - 0.04 * position, 1.0 + 0.03 * position), axis=1)
            offset = np.stack((180.0 * position, -140.0 * position, 110.0 * position), axis=1)
            sources = sources * gain[:, None, None, :] + offset[:, None, None, :]
            controls.update({"channel_gain": gain.tolist(), "channel_offset_counts": offset.tolist()})
        elif member == "spatial":
            multiplier = 1.0 + 0.025 * np.linspace(-1.0, 1.0, IMAGE_SIZE[1])[None, :, None]
            sources *= multiplier
            controls["spatial_multiplier"] = "linear-x"
        else:
            controls["unchanged_light"] = True
        controls["clip_fraction"] = ((sources < 0) | (sources > 65535)).mean(axis=(1, 2, 3)).tolist()
        frames = _render_independent_sources(np.clip(sources, 0, 65535), depth, seed.noise)
        metadata["lighting"] = controls
    return SceneData(frames, np.rint(truth).clip(0, 65535).astype(np.uint16), depth, metadata)
