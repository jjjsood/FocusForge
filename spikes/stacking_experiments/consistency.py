# SPDX-License-Identifier: GPL-3.0-or-later
"""Synchronous source-index consistency rules."""

from __future__ import annotations

from dataclasses import dataclass, replace
import numpy as np

from . import fusion
from .fusion import NO_SOURCE


@dataclass(frozen=True)
class SelectionResult:
    """One reconstructed Details result and its immutable and final maps."""

    composite: np.ndarray
    zmap: np.ndarray
    raw_winners: tuple[np.ndarray, ...]
    final_winners: tuple[np.ndarray, ...]


def _source_mask(source: object, frame: int, height: int, width: int) -> np.ndarray:
    masks = getattr(source, "masks", None)
    if masks is None and hasattr(source, "parent") and hasattr(source, "rect"):
        masks = getattr(source.parent, "masks", None)
        if masks is not None:
            y0, y1, x0, x1 = source.rect
            return np.asarray(masks[frame, y0:y1, x0:x1], dtype=bool)
    return np.ones((height, width), dtype=bool) if masks is None else np.asarray(masks[frame], dtype=bool)


def _has_file_masks(source: object) -> bool:
    """Identify direct or rectangle-parent mask storage for I/O accounting."""
    return getattr(source, "masks", None) is not None or getattr(getattr(source, "parent", None), "masks", None) is not None


def _eligible(coverage: np.ndarray, reach: int) -> np.ndarray:
    """Erode one coverage mask with an edge-clamped Chebyshev square."""
    invalid = np.pad(~coverage, reach, mode="edge")
    prefix = np.zeros((invalid.shape[0] + 1, invalid.shape[1] + 1), dtype=np.int64)
    prefix[1:, 1:] = np.cumsum(np.cumsum(invalid, axis=0, dtype=np.int64), axis=1, dtype=np.int64)
    extent = 2 * reach + 1
    return (prefix[extent:, extent:] - prefix[:-extent, extent:] - prefix[extent:, :-extent] + prefix[:-extent, :-extent]) == 0


def _one_frame_views(source: object, frame: int) -> tuple[object, np.ndarray]:
    values, mask = source.read(frame)
    encoded = np.rint(np.asarray(values, dtype=np.float64) * 65535.0).astype(np.uint16)
    mask = np.asarray(mask, dtype=bool)
    view = fusion.pyramid_views(encoded[None], 5)
    if mask.all():
        return view, mask
    current_coverage = mask[None]
    levels = []
    for level, entry in enumerate(view.levels):
        stride = 1 << level
        live = _eligible(mask, entry.reach)[::stride, ::stride][None]
        levels.append(replace(entry, raw_coverage=current_coverage, eligible=live, winners=fusion.select(entry.scores, live)))
        current_coverage = np.stack([fusion._reduce_coverage(value) for value in current_coverage])
    return fusion.PyramidView(tuple(levels), view.residual_inputs, view.residual_reach), mask


def _eligible_levels(mask: np.ndarray) -> list[np.ndarray]:
    if mask.all():
        return [np.ones(((mask.shape[0] + (1 << level) - 1) // (1 << level),
                         (mask.shape[1] + (1 << level) - 1) // (1 << level)), dtype=bool) for level in range(5)]
    return [_eligible(mask, fusion.level_reach(level, 8))[::1 << level, ::1 << level] for level in range(5)]


def _shifted(plane: np.ndarray, dy: int, dx: int) -> np.ndarray:
    yy = np.clip(np.arange(plane.shape[0]) + dy, 0, plane.shape[0] - 1)
    xx = np.clip(np.arange(plane.shape[1]) + dx, 0, plane.shape[1] - 1)
    return plane[np.ix_(yy, xx)]


def _stream_spatial(raw: list[np.ndarray], source: object, rule: str, height: int, width: int) -> list[np.ndarray]:
    finals = [plane.copy() for plane in raw]
    if rule == "identity":
        return finals
    if rule in {"majority-all", "destructive"}:
        radius, strict = (1, True) if rule == "majority-all" else (4, False)
        valid_count = [np.zeros(plane.shape, dtype=np.uint16) for plane in raw]
        best_count = [np.zeros(plane.shape, dtype=np.uint16) for plane in raw]
        best = [plane.copy() for plane in raw]
        for frame in range(source.n):
            masks = _eligible_levels(_source_mask(source, frame, height, width))
            for level, plane in enumerate(raw):
                mask = masks[level]
                count = np.zeros(plane.shape, dtype=np.uint16)
                for dy in range(-radius, radius + 1):
                    for dx in range(-radius, radius + 1):
                        count += (_shifted(plane, dy, dx) == frame) & mask
                valid_count[level] += count
                better = count > best_count[level]
                best[level][better] = frame
                best_count[level][better] = count[better]
        for level in range(5):
            accepted = best_count[level] > 0 if not strict else best_count[level] * 2 > valid_count[level]
            finals[level][accepted] = best[level][accepted]
        return finals
    if rule in {"isolated-all", "isolated-level0"}:
        neighbours = [[_shifted(plane, dy, dx) for dy, dx in ((-1, 0), (1, 0), (0, -1), (0, 1))] for plane in raw]
        proposal = [np.floor(sum(item.astype(np.uint32) for item in values).astype(np.float64) / 4.0 + 0.5).astype(np.uint16)
                    for values in neighbours]
        valid_neighbours = [np.zeros(plane.shape, dtype=np.uint8) for plane in raw]
        original_ok = [np.zeros(plane.shape, dtype=bool) for plane in raw]
        proposal_ok = [np.zeros(plane.shape, dtype=bool) for plane in raw]
        for frame in range(source.n):
            masks = _eligible_levels(_source_mask(source, frame, height, width))
            for level, plane in enumerate(raw):
                if rule == "isolated-level0" and level:
                    continue
                mask = masks[level]
                original_ok[level] |= (plane == frame) & mask
                proposal_ok[level] |= (proposal[level] == frame) & mask
                for neighbour in neighbours[level]:
                    valid_neighbours[level] += ((neighbour == frame) & mask).astype(np.uint8)
        for level, plane in enumerate(raw):
            if rule == "isolated-level0" and level:
                continue
            outside = (plane < np.minimum.reduce(neighbours[level])) | (plane > np.maximum.reduce(neighbours[level]))
            change = original_ok[level] & proposal_ok[level] & (valid_neighbours[level] == 4) & outside
            finals[level][change] = proposal[level][change]
        return finals
    if rule != "cross-scale":
        raise ValueError("unknown consistency rule")
    parent_ok = [np.zeros(plane.shape, dtype=bool) for plane in raw[:4]]
    original_ok = [np.zeros(plane.shape, dtype=bool) for plane in raw[:4]]
    matching_neighbour = [np.zeros(plane.shape, dtype=bool) for plane in raw[:4]]
    parent_values = []
    neighbours = []
    for level in range(4):
        plane, parent = raw[level], raw[level + 1]
        yy = np.arange(plane.shape[0]) // 2
        xx = np.arange(plane.shape[1]) // 2
        parent_values.append(parent[np.ix_(yy, xx)])
        neighbours.append([_shifted(plane, dy, dx) for dy, dx in ((-1, 0), (1, 0), (0, -1), (0, 1))])
    for frame in range(source.n):
        masks = _eligible_levels(_source_mask(source, frame, height, width))
        for level, plane in enumerate(raw[:4]):
            yy = np.arange(plane.shape[0]) // 2
            xx = np.arange(plane.shape[1]) // 2
            mask, parent_mask = masks[level], masks[level + 1]
            original_ok[level] |= (plane == frame) & mask
            parent_ok[level] |= (parent_values[level] == frame) & parent_mask[np.ix_(yy, xx)] & mask
            for neighbour in neighbours[level]:
                matching_neighbour[level] |= (plane == frame) & (neighbour == frame) & mask
    for level, plane in enumerate(raw[:4]):
        change = original_ok[level] & parent_ok[level] & (plane != parent_values[level]) & ~matching_neighbour[level]
        finals[level][change] = parent_values[level][change]
    return finals


def fuse_streaming(source: object, rule: str, random_match: tuple[list[np.ndarray], str, str, str, str] | None = None) -> tuple[SelectionResult, dict[str, int]]:
    """Fuse one source with two sample passes and bounded per-frame working arrays."""
    if rule not in {"identity", "majority-all", "isolated-all", "isolated-level0", "cross-scale", "destructive", "matched-random"}:
        raise ValueError("unknown consistency rule")
    height, width = int(source.h), int(source.w)
    raw = [np.full((max(1, (height + (1 << level) - 1) // (1 << level)),
                    max(1, (width + (1 << level) - 1) // (1 << level))), NO_SOURCE, dtype=np.uint16) for level in range(5)]
    best = [np.zeros(plane.shape, dtype=np.float32) for plane in raw]
    seen = [np.zeros(plane.shape, dtype=bool) for plane in raw]
    residual_sum = residual_count = None
    for frame in range(source.n):
        view, mask = _one_frame_views(source, frame)
        for level, entry in enumerate(view.levels):
            eligible = entry.eligible[0]
            score = entry.scores[0]
            better = eligible & (~seen[level] | (score > best[level]))
            raw[level][better] = frame
            best[level][better] = score[better]
            seen[level] |= eligible
        residual_mask = (np.ones(view.residual_inputs.shape[1:3], dtype=bool) if mask.all()
                         else _eligible(mask, view.residual_reach)[::32, ::32])
        values = view.residual_inputs[0]
        if residual_sum is None:
            residual_sum = np.zeros_like(values, dtype=np.float32)
            residual_count = np.zeros(values.shape[:2], dtype=np.uint16)
        residual_sum = np.float32(residual_sum + np.where(residual_mask[..., None], values, 0.0).astype(np.float32))
        residual_count += residual_mask
    if residual_sum is None or residual_count is None:
        raise ValueError("source has no frames")
    if rule != "matched-random":
        final = _stream_spatial(raw, source, rule, height, width)
    else:
        if random_match is None:
            raise ValueError("matched random requires candidate maps and identity")
        candidate, family, member, seed, candidate_id = random_match
        final = [plane.copy() for plane in raw]
        alternatives = [np.full(plane.shape, NO_SOURCE, dtype=np.uint16) for plane in raw]
        counts = [np.zeros(plane.shape, dtype=np.uint16) for plane in raw]
        generators = []
        for level in range(5):
            text = f"consistency-random-v1|{family}|{member}|{seed}|{candidate_id}|{level}|sources".encode()
            generators.append(np.random.Generator(np.random.PCG64(int.from_bytes(__import__("hashlib").sha256(text).digest()[:8], "little"))))
        for frame in range(source.n):
            masks = _eligible_levels(_source_mask(source, frame, height, width))
            for level, mask in enumerate(masks):
                allowed = mask & (raw[level] != frame)
                counts[level] += allowed
                take = allowed & (generators[level].random(raw[level].shape) < 1.0 / counts[level].clip(1))
                alternatives[level][take] = frame
        for level in range(5):
            wanted = int(np.count_nonzero(raw[level] != candidate[level]))
            changeable = np.flatnonzero(alternatives[level].ravel() != NO_SOURCE)
            if wanted > len(changeable):
                raise ValueError("matched random has fewer eligible alternatives than candidate changes")
            text = f"consistency-random-v1|{family}|{member}|{seed}|{candidate_id}|{level}|positions".encode()
            rng = np.random.Generator(np.random.PCG64(int.from_bytes(__import__("hashlib").sha256(text).digest()[:8], "little")))
            picks = rng.choice(changeable, size=wanted, replace=False)
            final[level].ravel()[picks] = alternatives[level].ravel()[picks]
    selected = [np.zeros((*plane.shape, int(source.c)), dtype=np.float32) for plane in raw]
    for frame in range(source.n):
        view, _mask = _one_frame_views(source, frame)
        for level, entry in enumerate(view.levels):
            chosen = final[level] == frame
            selected[level][chosen] = entry.details[0][chosen]
    output = np.divide(residual_sum, residual_count[..., None], out=np.zeros_like(residual_sum), where=residual_count[..., None] != 0).astype(np.float32)
    for detail in reversed(selected):
        output = np.float32(fusion.expand(output, detail.shape[0], detail.shape[1]) + detail)
    result = SelectionResult(fusion.quantize_u16(output), final[0], tuple(raw), tuple(final))
    extra_masks = 0 if rule == "identity" else int(source.n)
    total_masks = int(source.n * 2 + extra_masks)
    mask_file_reads = total_masks if _has_file_masks(source) else 0
    return result, {"sample_reads": int(source.n * 2), "extra_mask_pass_reads": extra_masks,
                    "total_mask_reads": mask_file_reads, "mask_defaults_generated": total_masks if mask_file_reads == 0 else 0,
                    "frame_count": int(source.n)}


def fuse_configurations(source: object, configurations: list[dict[str, object]], controls: list[dict[str, object]], family: str, member: str, seed: str) -> dict[str, tuple[SelectionResult, dict[str, int]]]:
    """Run the local registered rules from immutable streaming selections."""
    names = {"identity": "identity", "majority-3x3-all": "majority-all", "isolated-all": "isolated-all", "cross-scale-raw-parent": "cross-scale", "isolated-level0": "isolated-level0", "mode-9x9-all": "destructive"}
    output = {}
    for item in [*controls, *configurations]:
        rule = names.get(str(item.get("rule")))
        if rule is not None:
            output[str(item["id"])] = fuse_streaming(source, rule)
    for item in controls:
        if item.get("rule") != "matched-random":
            continue
        candidate_id = str(item["matched_candidate"])
        candidate = output[candidate_id][0]
        output[str(item["id"])] = fuse_streaming(source, "matched-random", (list(candidate.final_winners), family, member, seed, candidate_id))
    return output


def fuse_tiled(source: object, rule: str, tile: int, margin: int) -> tuple[SelectionResult, dict[str, int], dict[str, object]]:
    """Assemble 32-phase aligned streamed tile interiors into full result maps."""
    interior = ((tile - 2 * margin) // 32) * 32
    if interior <= 0:
        raise ValueError("tile margin leaves no interior")
    vertical, horizontal = list(range(interior, source.w, interior)), list(range(interior, source.h, interior))
    xs, ys = [0, *vertical, source.w], [0, *horizontal, source.h]
    shapes = [((source.h + (1 << level) - 1) // (1 << level), (source.w + (1 << level) - 1) // (1 << level)) for level in range(5)]
    raw = [np.full(shape, NO_SOURCE, dtype=np.uint16) for shape in shapes]
    final = [np.full(shape, NO_SOURCE, dtype=np.uint16) for shape in shapes]
    image = np.zeros((source.h, source.w, source.c), dtype=np.uint16)
    counters: dict[str, int] = {"frame_count": int(source.n), "tile_count": 0}
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
            result, counts = fuse_streaming(Rect((oy0, oy1, ox0, ox1)), rule)
            counters["tile_count"] += 1
            windows.append({"interior_xywh": [x0, y0, x1 - x0, y1 - y0], "read_xywh": [ox0, oy0, ox1 - ox0, oy1 - oy0]})
            image[y0:y1, x0:x1] = result.composite[y0 - oy0:y1 - oy0, x0 - ox0:x1 - ox0]
            for level, stride in enumerate((1, 2, 4, 8, 16)):
                gy0, gy1, gx0, gx1 = y0 // stride, (y1 + stride - 1) // stride, x0 // stride, (x1 + stride - 1) // stride
                ly0, ly1, lx0, lx1 = (y0 - oy0) // stride, (y1 - oy0 + stride - 1) // stride, (x0 - ox0) // stride, (x1 - ox0 + stride - 1) // stride
                raw[level][gy0:gy1, gx0:gx1] = result.raw_winners[level][ly0:ly1, lx0:lx1]
                final[level][gy0:gy1, gx0:gx1] = result.final_winners[level][ly0:ly1, lx0:lx1]
            for key, value in counts.items():
                if key != "frame_count":
                    counters[key] = counters.get(key, 0) + value
    return SelectionResult(image, final[0], tuple(raw), tuple(final)), counters, {"margin": margin, "levels": 5, "interior": interior, "vertical": vertical, "horizontal": horizontal, "windows": windows}


def paired_random(raw: list[np.ndarray], candidate: list[np.ndarray], eligibility: list[np.ndarray], family: str, member: str, seed: str, candidate_id: str) -> list[np.ndarray]:
    """Choose exact matched global changes using registered PCG64 coordinate order."""
    output = [plane.copy() for plane in raw]
    for level, (base, changed, live) in enumerate(zip(raw, candidate, eligibility)):
        def rng(purpose: str) -> np.random.Generator:
            text = f"consistency-random-v1|{family}|{member}|{seed}|{candidate_id}|{level}|{purpose}".encode()
            return np.random.Generator(np.random.PCG64(int.from_bytes(__import__("hashlib").sha256(text).digest()[:8], "little")))
        alternatives = np.full(base.shape, NO_SOURCE, dtype=np.uint16)
        counts = np.zeros(base.shape, dtype=np.uint16)
        source_rng = rng("sources")
        for frame in range(live.shape[0]):
            allowed = live[frame] & (base != frame)
            counts += allowed
            take = allowed & (source_rng.random(base.shape) < (1.0 / counts.clip(1)))
            alternatives[take] = frame
        changeable = np.flatnonzero(alternatives.ravel() != NO_SOURCE)
        wanted = int(np.count_nonzero(base != changed))
        if wanted > len(changeable):
            raise ValueError("matched random has fewer eligible alternatives than candidate changes")
        picks = rng("positions").choice(changeable, size=wanted, replace=False)
        output[level].ravel()[picks] = alternatives.ravel()[picks]
    return output


def fuse_oracle(frames: np.ndarray, coverage: np.ndarray | None, rule: str) -> SelectionResult:
    """Reconstruct Details from a small all-frame oracle for identity checks."""
    view = fusion.pyramid_views(frames, 5, coverage=coverage)
    raw = [level.winners for level in view.levels]
    final = apply_rule(raw, [level.eligible for level in view.levels], rule)
    selected = [fusion.gather(level.details, winners).astype(np.float32) for level, winners in zip(view.levels, final)]
    if coverage is None:
        residual_masks = np.ones(view.residual_inputs.shape[:3], dtype=bool)
    else:
        residual_masks = np.stack([fusion.eligible(mask, view.residual_reach) for mask in coverage])[..., ::32, ::32]
    output = fusion._eligible_mean(view.residual_inputs, residual_masks)
    for selected_detail in reversed(selected):
        output = np.float32(fusion.expand(output, selected_detail.shape[0], selected_detail.shape[1]) + selected_detail)
    return SelectionResult(fusion.quantize_u16(output), final[0], tuple(raw), tuple(final))


def _valid(value: int, eligible: np.ndarray, y: int, x: int) -> bool:
    return value != int(NO_SOURCE) and 0 <= value < eligible.shape[0] and bool(eligible[value, y, x])


def _clamped(y: int, x: int, height: int, width: int) -> tuple[int, int]:
    return min(max(y, 0), height - 1), min(max(x, 0), width - 1)


def _mode(values: list[int], strict: bool) -> int | None:
    if not values:
        return None
    counts = np.bincount(np.asarray(values, dtype=np.intp))
    winner = int(np.flatnonzero(counts == counts.max())[0])
    return winner if not strict or int(counts[winner]) * 2 > len(values) else None


def _majority(raw: np.ndarray, eligible: np.ndarray, radius: int, strict: bool) -> np.ndarray:
    height, width = raw.shape
    result = raw.copy()
    for y in range(height):
        for x in range(width):
            values = []
            for dy in range(-radius, radius + 1):
                for dx in range(-radius, radius + 1):
                    yy, xx = _clamped(y + dy, x + dx, height, width)
                    value = int(raw[yy, xx])
                    if _valid(value, eligible, y, x):
                        values.append(value)
            proposal = _mode(values, strict)
            if proposal is not None:
                result[y, x] = proposal
    return result


def _isolated(raw: np.ndarray, eligible: np.ndarray) -> np.ndarray:
    height, width = raw.shape
    result = raw.copy()
    for y in range(height):
        for x in range(width):
            original = int(raw[y, x])
            if not _valid(original, eligible, y, x):
                continue
            neighbours = []
            for dy, dx in ((-1, 0), (1, 0), (0, -1), (0, 1)):
                yy, xx = _clamped(y + dy, x + dx, height, width)
                value = int(raw[yy, xx])
                if not _valid(value, eligible, y, x):
                    break
                neighbours.append(value)
            if len(neighbours) != 4 or not (original < min(neighbours) or original > max(neighbours)):
                continue
            proposal = int(np.floor(np.mean(neighbours, dtype=np.float64) + 0.5))
            if _valid(proposal, eligible, y, x):
                result[y, x] = proposal
    return result


def _cross_scale(raw: list[np.ndarray], eligible: list[np.ndarray]) -> list[np.ndarray]:
    result = [plane.copy() for plane in raw]
    for level in range(len(raw) - 1):
        plane, parent = raw[level], raw[level + 1]
        mask, parent_mask = eligible[level], eligible[level + 1]
        height, width = plane.shape
        for y in range(height):
            for x in range(width):
                original = int(plane[y, x])
                py, px = _clamped(y // 2, x // 2, *parent.shape)
                proposal = int(parent[py, px])
                if not _valid(original, mask, y, x) or not _valid(proposal, parent_mask, py, px):
                    continue
                neighbours = []
                for dy, dx in ((-1, 0), (1, 0), (0, -1), (0, 1)):
                    yy, xx = _clamped(y + dy, x + dx, height, width)
                    value = int(plane[yy, xx])
                    if _valid(value, mask, y, x):
                        neighbours.append(value)
                if _valid(proposal, mask, y, x) and original != proposal and all(original != value for value in neighbours):
                    result[level][y, x] = proposal
    return result


def apply_rule(raw_winners: list[np.ndarray], eligibility: list[np.ndarray], rule: str) -> list[np.ndarray]:
    """Apply one rule to immutable raw per-level winner maps."""
    if len(raw_winners) != len(eligibility) or not raw_winners:
        raise ValueError("winner and eligibility levels must match")
    if rule == "identity":
        return [plane.copy() for plane in raw_winners]
    if rule == "cross-scale":
        return _cross_scale(raw_winners, eligibility)
    if rule not in {"majority-all", "isolated-all", "isolated-level0", "destructive"}:
        raise ValueError("unknown consistency rule")
    result = []
    for level, (raw, eligible) in enumerate(zip(raw_winners, eligibility)):
        if rule == "isolated-level0" and level != 0:
            result.append(raw.copy())
        elif rule.startswith("isolated"):
            result.append(_isolated(raw, eligible))
        elif rule == "majority-all":
            result.append(_majority(raw, eligible, 1, True))
        else:
            result.append(_majority(raw, eligible, 4, False))
    return result


def changed_counts(raw_winners: list[np.ndarray], final_winners: list[np.ndarray]) -> list[int]:
    """Count changed source indices once per pyramid level."""
    if len(raw_winners) != len(final_winners):
        raise ValueError("winner levels must match")
    return [int(np.count_nonzero(raw != final)) for raw, final in zip(raw_winners, final_winners)]


def self_checks() -> list[str]:
    erosion_cases = (
        np.ones((1, 1), dtype=bool),
        np.array([[True, False, True], [False, True, True]], dtype=bool),
        np.ones((17, 19), dtype=bool),
        np.zeros((17, 19), dtype=bool),
    )
    sparse_coverage = np.ones((17, 19), dtype=bool)
    sparse_coverage[0, 0] = False
    sparse_coverage[8, 9] = False
    sparse_coverage[-1, -1] = False
    for coverage in (*erosion_cases, sparse_coverage):
        for reach in (0, 10, 14, 22, 38, 62, 78):
            assert np.array_equal(_eligible(coverage, reach), fusion.eligible(coverage, reach))
    raw = np.array([[1, 1, 0], [1, 2, 1], [0, 1, 0]], dtype=np.uint16)
    eligible = np.ones((3, 3, 3), dtype=bool)
    result = apply_rule([raw], [eligible], "majority-all")
    assert result[0][1, 1] == 1
    isolated = apply_rule([raw], [eligible], "isolated-all")
    assert isolated[0][1, 1] == 1
    half_up = np.array([[1, 1, 1], [1, 0, 2], [1, 2, 1]], dtype=np.uint16)
    assert apply_rule([half_up], [eligible], "isolated-all")[0][1, 1] == 2
    blocked = eligible.copy()
    blocked[1, 1, 1] = False
    assert apply_rule([raw], [blocked], "isolated-all")[0][1, 1] == 2
    no_source = np.full((3, 3), NO_SOURCE, dtype=np.uint16)
    assert np.array_equal(apply_rule([no_source], [eligible], "majority-all")[0], no_source)
    fine = np.full((3, 3), 1, dtype=np.uint16)
    fine[1, 1] = 0
    parent = np.full((2, 2), 1, dtype=np.uint16)
    crossed = apply_rule([fine, parent], [eligible, np.ones((3, 2, 2), dtype=bool)], "cross-scale")
    assert crossed[0][1, 1] == 1 and np.array_equal(crossed[1], parent)
    assert changed_counts([raw], result) == [1]
    frames = np.arange(2 * 33 * 35 * 3, dtype=np.uint16).reshape(2, 33, 35, 3)
    identity = fuse_oracle(frames, None, "identity")
    current = fusion.details(frames)
    assert np.array_equal(identity.composite, current.composite) and np.array_equal(identity.zmap, current.winners)
    class Source:
        n, h, w, c, masks = 2, 33, 35, 3, None

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return frames[frame].astype(np.float64) / 65535.0, np.ones((33, 35), dtype=bool)
    streamed, counts = fuse_streaming(Source(), "identity")
    assert np.array_equal(streamed.composite, identity.composite) and np.array_equal(streamed.zmap, identity.zmap)
    assert counts == {"sample_reads": 4, "extra_mask_pass_reads": 0, "total_mask_reads": 0,
                      "mask_defaults_generated": 4, "frame_count": 2}
    non_square_frames = np.arange(2 * 35 * 67 * 3, dtype=np.uint16).reshape(2, 35, 67, 3)

    class NonSquareSource:
        n, h, w, c, masks = 2, 35, 67, 3, None

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return non_square_frames[frame].astype(np.float64) / 65535.0, np.ones((35, 67), dtype=bool)

    non_square, _counts = fuse_streaming(NonSquareSource(), "identity")
    assert np.array_equal(non_square.composite, fuse_oracle(non_square_frames, None, "identity").composite)
    masked_frames = np.arange(2 * 17 * 19 * 3, dtype=np.uint16).reshape(2, 17, 19, 3)
    masked_coverage = np.ones((2, 17, 19), dtype=bool)
    masked_coverage[0, 0, 0] = False
    masked_coverage[0, 8, 9] = False
    masked_coverage[1, -1, -1] = False

    class MaskedSource:
        n, h, w, c, masks = 2, 17, 19, 3, masked_coverage

        @staticmethod
        def read(frame: int) -> tuple[np.ndarray, np.ndarray]:
            return masked_frames[frame].astype(np.float64) / 65535.0, masked_coverage[frame]

    for frame in range(MaskedSource.n):
        observed, observed_mask = _one_frame_views(MaskedSource(), frame)
        encoded = np.rint(masked_frames[frame].astype(np.float64) / 65535.0 * 65535.0).astype(np.uint16)
        expected = fusion.pyramid_views(encoded[None], 5, coverage=masked_coverage[frame:frame + 1])
        assert np.array_equal(observed_mask, masked_coverage[frame])
        assert observed.residual_reach == expected.residual_reach
        assert np.array_equal(observed.residual_inputs, expected.residual_inputs)
        for actual, wanted in zip(observed.levels, expected.levels):
            assert actual.score_radius == wanted.score_radius and actual.reach == wanted.reach
            assert np.array_equal(actual.gaussian, wanted.gaussian)
            assert np.array_equal(actual.details, wanted.details)
            assert np.array_equal(actual.raw_coverage, wanted.raw_coverage)
            assert np.array_equal(actual.scores, wanted.scores)
            assert np.array_equal(actual.eligible, wanted.eligible)
            assert np.array_equal(actual.winners, wanted.winners)
    sparse = np.full((3, 3), 2, dtype=np.uint16)
    sparse[1, 1] = 0
    child = np.zeros((3, 3, 3), dtype=bool)
    child[0, 1, 1] = True
    child[1, 1, 1] = True
    assert apply_rule([sparse, np.ones((2, 2), dtype=np.uint16)], [child, np.ones((3, 2, 2), dtype=bool)], "cross-scale")[0][1, 1] == 1
    cascade_child = np.full((5, 5), 1, dtype=np.uint16)
    cascade_child[2, 2] = 2
    changing_parent = np.full((3, 3), 1, dtype=np.uint16)
    changing_parent[1, 1] = 0
    coarse_parent = np.full((2, 2), 1, dtype=np.uint16)
    cascade = apply_rule(
        [cascade_child, changing_parent, coarse_parent],
        [np.ones((3, 5, 5), dtype=bool), np.ones((3, 3, 3), dtype=bool), np.ones((3, 2, 2), dtype=bool)],
        "cross-scale",
    )
    assert cascade[1][1, 1] == 1 and cascade[0][2, 2] == 0
    hair_labels = np.zeros((17, 17), dtype=np.uint16)
    hair_one = np.zeros(hair_labels.shape, dtype=bool)
    hair_two = np.zeros(hair_labels.shape, dtype=bool)
    hair_one[4:13, 4] = True
    hair_two[4:13, 10:12] = True
    hair_one_adjacent = np.zeros(hair_labels.shape, dtype=bool)
    hair_two_adjacent = np.zeros(hair_labels.shape, dtype=bool)
    hair_one_adjacent[4:13, (3, 5)] = True
    hair_two_adjacent[4:13, (9, 12)] = True
    hair_labels[hair_one] = 1
    hair_labels[hair_two] = 2
    hair_eligible = np.ones((3, *hair_labels.shape), dtype=bool)
    clean_identity = apply_rule([hair_labels], [hair_eligible], "identity")[0]
    destructive_hair = apply_rule([hair_labels], [hair_eligible], "destructive")[0]
    source_luma_tuples = np.array([0.0, 1.0, 1.0], dtype=np.float32)
    for hair, adjacent in ((hair_one, hair_one_adjacent), (hair_two, hair_two_adjacent)):
        clean_luma = source_luma_tuples[clean_identity]
        destructive_luma = source_luma_tuples[destructive_hair]
        clean_contrast = float(clean_luma[hair].mean())
        destructive_contrast = float(destructive_luma[hair].mean())
        retention_loss = 1.0 - destructive_contrast / clean_contrast
        label_loss = float(np.mean(destructive_hair[hair] != hair_labels[hair]))
        assert np.array_equal(clean_identity[hair], hair_labels[hair])
        assert np.all(clean_luma[adjacent] == 0.0)
        assert clean_contrast == 1.0 and destructive_contrast == 0.0
        assert retention_loss == 1.0 and label_loss == 1.0
    mask_mutant_raw = np.zeros((3, 3), dtype=np.uint16)
    mask_mutant_raw[1, 1] = 1
    mask_mutant_eligibility = np.ones((2, 3, 3), dtype=bool)
    mask_mutant_eligibility[0, 1, 1] = False
    masked_majority = apply_rule([mask_mutant_raw], [mask_mutant_eligibility], "majority-all")[0]
    ignore_mask_mutant = _majority(mask_mutant_raw, np.ones_like(mask_mutant_eligibility), 1, True)
    assert masked_majority[1, 1] == 1 and ignore_mask_mutant[1, 1] == 0
    for rule in ("majority-all", "isolated-all", "isolated-level0", "cross-scale", "destructive"):
        bounded, _counts = fuse_streaming(Source(), rule)
        reference = fuse_oracle(frames, None, rule)
        assert np.array_equal(bounded.composite, reference.composite) and np.array_equal(bounded.zmap, reference.zmap)
        masked_bounded, _counts = fuse_streaming(MaskedSource(), rule)
        masked_reference = fuse_oracle(masked_frames, masked_coverage, rule)
        assert np.array_equal(masked_bounded.composite, masked_reference.composite)
        assert np.array_equal(masked_bounded.zmap, masked_reference.zmap)
        assert all(np.array_equal(actual, wanted) for actual, wanted in zip(masked_bounded.raw_winners, masked_reference.raw_winners))
        assert all(np.array_equal(actual, wanted) for actual, wanted in zip(masked_bounded.final_winners, masked_reference.final_winners))
    return ["consistency majority", "consistency isolated", "consistency half-up", "consistency masks", "consistency raw parent", "consistency identity", "consistency streaming identity", "consistency streaming rules", "consistency non-square alltrue", "consistency raw-parent no cascade", "consistency constructed hair width1 retention-loss=1 label-loss=1", "consistency constructed hair width2 retention-loss=1 label-loss=1", "consistency ignore-mask mutant"]
