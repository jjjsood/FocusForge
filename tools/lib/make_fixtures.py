#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Cut the checked-in fixture crops out of the reference stack.

Driven by tools/make-fixtures.sh; see that script for the flags. The whole job
is header reads and strip seeks -- a 512x512 crop of a 268 MB frame touches
512 rows of 3 KB, so a full regeneration reads single-digit megabytes and
finishes in seconds. Nothing here ever loads a frame.

Determinism is the point, not a nicety. `--check` regenerates into a temporary
directory and compares byte for byte, and that gate is only meaningful if the
generator has no hidden inputs: every output is written by rawtiff.write_tiff,
which emits no timestamp and no generator tag, and the one date in the tree
lives on a single normalizable line of fixtures/README.md.

To add a fixture set, append to SETS. To change an existing set's crop, add a
new set instead -- a fixture's identity is its rect, and silently moving one
invalidates every golden image recorded against it.
"""

from __future__ import annotations

import argparse
import datetime
import filecmp
import os
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rawtiff  # noqa: E402

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_STACK = os.environ.get("FOCUSFORGE_REFERENCE_STACK", "")
DEFAULT_OUT = os.path.join(REPO_ROOT, "fixtures")

FRAME_GLOB_PREFIX = "2026"
REFERENCE_NAME = "Mantis_Stacked.tif"
HEAD_BYTES = 65536
DATE_LINE_RE = re.compile(rb"(?m)^Generated: .*$")

# Every other output here is normalized to little-endian, one strip per row --
# which means no committed fixture has the layout of the one file the project
# will actually be compared against. Mantis_Stacked.tif is big-endian with
# RowsPerStrip=8, and a reader tested only against II/rps=1 has not been tested
# against the case that will break it. So one set carries the reference crop a
# second time in the source's own shape, byte for byte equivalent to
# reference.tif when read correctly and different from it on disk. Small set
# only: it is a reader test, not more data.
MM_VARIANT_NAME = "reference-MM-rps8.tif"
MM_VARIANT_ROWS_PER_STRIP = 8


@dataclass(frozen=True)
class FixtureSet:
    name: str
    indices: tuple  # indices into the sorted frame list, 0..60
    rect: tuple  # x, y, w, h
    purpose: str
    # Why this rectangle rather than some other one -- a human review, recorded
    # once so a reader never has to re-derive it: does the window actually span
    # focus variation across the stack, or does it sit on flat/uniformly-sharp
    # content, which would make every fusion golden test pass for the wrong
    # reason. Rendered into the set's README right after the rect line.
    rect_rationale: str
    # Also emit reference-MM-rps8.tif: the same crop in the SOURCE reference's
    # own layout rather than the normalized one. See MM_VARIANT_NAME below.
    mm_variant: bool = False


@dataclass(frozen=True)
class CompressedFixtureSet:
    name: str
    compression: int
    codec: str
    predictor: int
    layout: str
    byte_order: str


def _compressed_sets() -> tuple[CompressedFixtureSet, ...]:
    codecs = (("lzw", 5), ("deflate", 8), ("old-deflate", 32946))
    return tuple(
        CompressedFixtureSet(
            name=f"mantis-64-{codec}-p{predictor}-{layout}-{byte_order.lower()}",
            compression=compression,
            codec=codec,
            predictor=predictor,
            layout=layout,
            byte_order=byte_order,
        )
        for codec, compression in codecs
        for predictor in (1, 2)
        for layout in ("strip", "tile")
        for byte_order in ("II", "MM")
    )


COMPRESSED_SETS = _compressed_sets()
COMPRESSED_ROWS_PER_STRIP = 11
COMPRESSED_TILE = (48, 48)

# Folder-shaped inputs for the open paths, copied byte for byte from the
# committed mantis-64 frames. `open-rejected/frames/02.tif` is cut inside its
# strip data: the header still reads, the pixel data does not.
FOLDER_SET_NAMES = ("open-two-stacks", "open-root-and-subfolder", "open-rejected")
FOLDER_SET_FRAMES = 3


SETS = (
    FixtureSet(
        name="mantis-64",
        indices=(0, 30, 60),
        rect=(4064, 2700, 64, 64),
        purpose="unit-test scale: three frames spanning the focus sweep, small enough "
        "to assert on individual pixels in a test body",
        rect_rationale="Centred on the frame, at reader scale: the reference crop is "
        "3.59x sharper (16-bit Laplacian variance) than the best single frame, so the "
        "window spans real focus variation. But with only frames 0, 30 and 60 kept, "
        "frame 02 is sharpest in 12 of 16 tiles (spread 1.30x) -- this is a reader "
        "fixture, not a fusion one.",
        mm_variant=True,
    ),
    FixtureSet(
        name="mantis-512",
        indices=(0, 9, 17, 26, 34, 43, 51, 60),
        rect=(3840, 2476, 512, 512),
        purpose="algorithm scale: eight evenly spaced frames, wide enough for a "
        "pyramid deep enough to matter and for alignment to have something to find",
        rect_rationale="Centred on the frame, spanning the focus sweep: the reference "
        "crop is 3.24x sharper (16-bit Laplacian variance) than the best single frame, "
        "with an 8-way per-tile winner map and a 28x max best/worst ratio -- different "
        "source frames win different tiles, which is the phenomenon a fusion fixture "
        "has to exercise.",
    ),
)


class FixtureError(Exception):
    """A generation input is missing or inconsistent."""


def list_frames(stack_dir: str) -> list:
    if not os.path.isdir(stack_dir):
        raise FixtureError(f"no reference stack at {stack_dir}")
    names = sorted(
        n for n in os.listdir(stack_dir) if n.startswith(FRAME_GLOB_PREFIX) and n.endswith(".tif")
    )
    if not names:
        raise FixtureError(f"no {FRAME_GLOB_PREFIX}*.tif frames in {stack_dir}")
    return names


def _rect_str(rect) -> str:
    return "{},{},{},{}".format(*rect)


def _crop(
    src: str,
    dst: str,
    rect,
    description: str,
    byte_order: str = "II",
    rows_per_strip: int = 1,
    compression: int = 1,
    predictor: int = 1,
    tile: tuple[int, int] | None = None,
) -> None:
    x, y, w, h = rect
    info = rawtiff.read_info(src)
    pixels = rawtiff.read_rect(src, x, y, w, h, info=info)
    rawtiff.write_tiff(
        dst,
        w,
        h,
        pixels,
        bits=info.bits_per_sample[0],
        spp=info.samples_per_pixel,
        description=description,
        byte_order=byte_order,
        rows_per_strip=rows_per_strip,
        compression=compression,
        predictor=predictor,
        tile=tile,
    )


def _set_readme(fset: FixtureSet, stack_dir: str, frame_names: list) -> str:
    x, y, w, h = fset.rect
    rows = [
        "| NN | source frame | head64k_sha256 |",
        "|---|---|---|",
    ]
    for nn, idx in enumerate(fset.indices):
        name = frame_names[idx]
        digest = rawtiff.sha256_head(os.path.join(stack_dir, name), HEAD_BYTES)
        rows.append(f"| {nn:02d} | `{name}` | `{digest}` |")

    return "\n".join(
        [
            f"# Fixture set `{fset.name}`",
            "",
            f"{fset.purpose[0].upper()}{fset.purpose[1:]}.",
            "",
            f"Crop rect (x, y, w, h): **{_rect_str(fset.rect)}** "
            f"-- a {w}x{h} window taken from the same place in every frame.",
            "",
            f"Why this rect: {fset.rect_rationale}",
            "",
            "## Frames",
            "",
            *rows,
            "",
            "`head64k_sha256` is the sha256 of each source frame's first 64 KiB, "
            "truncated to 16 hex characters. It pins the header and the leading "
            "strips; it is not a whole-file digest, because each source frame is "
            "268 MB and hashing all 61 would read 16 GB to answer a provenance "
            "question.",
            "",
            "`reference.tif` is the identical crop of "
            f"`{REFERENCE_NAME}`, converted from big-endian to little-endian "
            "and to one strip per row like every other file here.",
            "",
            *(
                [
                    f"`{MM_VARIANT_NAME}` is that same crop again, left in "
                    f"`{REFERENCE_NAME}`'s own layout: **big-endian `MM`, "
                    f"RowsPerStrip={MM_VARIANT_ROWS_PER_STRIP}** "
                    f"({h} rows -> {(h + MM_VARIANT_ROWS_PER_STRIP - 1) // MM_VARIANT_ROWS_PER_STRIP} "
                    "strips). It exists because normalizing every other fixture "
                    "means nothing here would otherwise exercise the layout the "
                    "reader actually meets on the real reference file. Read "
                    "correctly it is sample-for-sample equal to `reference.tif`; "
                    "on disk the two differ. A reader that returns the same "
                    "pixels for both has handled byte order and multi-row strips.",
                    "",
                ]
                if fset.mm_variant
                else []
            ),
            "## Regenerating",
            "",
            "```",
            "tools/make-fixtures.sh          # rewrites all sets",
            "tools/make-fixtures.sh --check  # verify this tree still matches the generator",
            "```",
            "",
            "Generated by `tools/make-fixtures.sh` -- never hand-edit. A different "
            "crop is a new directory, not an edit to this one.",
            "",
        ]
    )


def _root_readme(stack_dir: str, frame_names: list, date: str) -> str:
    lines = [
        "# Fixtures",
        "",
        "Small crops of the reference focus stack, checked in so `cargo test` has "
        "real sensor data to work on without the 16 GB stack being anywhere near "
        "the repository.",
        "",
        f"Generated: {date}",
        "",
        "## Sets",
        "",
        "| set | frames | crop rect (x,y,w,h) | pixels |",
        "|---|---|---|---|",
    ]
    for fset in SETS:
        _x, _y, w, h = fset.rect
        lines.append(f"| `{fset.name}` | {len(fset.indices)} | `{_rect_str(fset.rect)}` | {w}x{h} |")
    for fset in COMPRESSED_SETS:
        lines.append(f"| `{fset.name}` | 3 | `4064,2700,64,64` | 64x64 |")
    lines.append("| `blackmagic-micro-dng` | 1 | single DNG | 1952x1104 |")
    lines.append("| `open-two-stacks` | 6 | `4064,2700,64,64` | 64x64 |")
    lines.append("| `open-root-and-subfolder` | 3 | `4064,2700,64,64` | 64x64 |")
    lines.append("| `open-rejected` | 3 | `4064,2700,64,64` | 64x64 |")
    lines.append("| `projects` | 3 | `4064,2700,64,64` | 64x64 |")
    lines += [
        "",
        "Each set holds `frames/NN.tif` in focus order, `reference.tif`, and a "
        "`README.md` naming the source frame behind every `NN`. Outputs are "
        "little-endian, uncompressed, chunky, one strip per row. The `mantis-64-` "
        "sets add LZW and deflate encodings in both byte orders, with strips or "
        "zero-padded tiles and Predictor 1 or 2.",
        "",
        "`blackmagic-micro-dng` is one lossless-JPEG DNG camera frame with its own "
        "README; it has no `frames/` directory or `reference.tif`.",
        "",
        "`open-two-stacks`, `open-root-and-subfolder` and `open-rejected` are folder "
        "shapes for the open paths, copied byte for byte from `mantis-64/frames` "
        "(`open-rejected/frames/02.tif` is then cut in half inside its strip data). "
        "They have their own READMEs and no `reference.tif`.",
        "",
        "## Project files",
        "",
        "`projects/` holds four project files over one shared `projects/frames/` "
        "(copies of `mantis-64/frames`). They are not regenerated: each records its "
        "frames' modified time and device/inode, so a rerun writes different bytes. "
        "They are registered in `MANIFEST.sha256` as they stand. Each opens to a "
        "different outcome:",
        "",
        "| file | opens as |",
        "|---|---|",
        "| `valid.ffp` | one project over the 3 frames |",
        "| `mismatch.ffp` | the fingerprint-mismatch question: its frames `01` and `02` were swapped when it was written |",
        "| `missing.ffp` | a project whose frames (`gone/NN.tif`) are not there |",
        "| `corrupt.ffp` | a refusal: the file is not a project |",
        "",
        "Produced in a scratch directory `S`, with the output directory set to the "
        "frames' parent so the recorded paths are relative:",
        "",
        "```",
        "mkdir -p S/projects/frames && cp mantis-64/frames/*.tif S/projects/frames/",
        "focusforge-cli stack S/projects/frames -o S/projects --algorithm laplacian",
        "    # S/projects/frames.ffp -> projects/valid.ffp",
        "# swap 01.tif and 02.tif in S/projects/frames, then the same stack command",
        "    # -> projects/mismatch.ffp",
        "mkdir -p S/projects/gone && cp mantis-64/frames/*.tif S/projects/gone/",
        "focusforge-cli stack S/projects/gone -o S/projects --algorithm laplacian",
        "    # S/projects/gone.ffp -> projects/missing.ffp; gone/ is not kept",
        "printf 'not a project\\n' > projects/corrupt.ffp",
        "```",
        "",
        "Then `tools/make-fixtures.sh --manifest-only` lists the files. `frames/` "
        "holds only the three `.tif` files: the stack command writes its report "
        "beside the frames it reads, so it is never run over `projects/frames/`.",
        "",
        f"With one deliberate exception: `mantis-64/{MM_VARIANT_NAME}` is the "
        f"reference crop left in `{REFERENCE_NAME}`'s own layout (big-endian "
        f"`MM`, RowsPerStrip={MM_VARIANT_ROWS_PER_STRIP}). Normalizing every "
        "fixture would leave nothing here exercising the layout the reader meets "
        "on the real file. It holds the same samples as that set's "
        "`reference.tif` and differs from it on disk.",
        "",
        "The source files are named in each set README. They live outside the "
        "repository and are not redistributed here.",
        "",
        "## `reference.tif` is not a golden image",
        "",
        "The two are different kinds of claim and must not be confused:",
        "",
        "- **`reference.tif`** is the same crop of a stack produced by a *different* "
        "stacker. It is a similarity floor: our output should be close to it, and a "
        "large divergence means something is wrong. It is never an exact target -- a "
        "different algorithm legitimately picks different pixels -- so it is only ever "
        "compared with a loose tolerance.",
        "- **`golden/*.tif`** is our own output, reviewed by a human and then frozen. "
        "It is a regression target: a change to it is a change in behaviour that "
        "someone has to look at and approve. Compared with a tight tolerance.",
        "",
        "A test that asserts an exact match against `reference.tif` is asserting that "
        "we reimplemented someone else's stacker, which is not a goal.",
        "",
        "## Reproducibility",
        "",
        "Regeneration is byte-for-byte deterministic. `MANIFEST.sha256` covers every "
        "file here; changed, missing, and unlisted files are errors.",
        "",
        "Never hand-edit anything in this tree.",
        "",
    ]
    return "\n".join(lines)


def _compressed_readme(fset: CompressedFixtureSet) -> str:
    if fset.layout == "strip":
        layout = (
            f"Strips use RowsPerStrip={COMPRESSED_ROWS_PER_STRIP}: six strips for 64 rows, "
            "with nine rows in the final strip."
        )
    else:
        layout = (
            f"Tiles are {COMPRESSED_TILE[0]}x{COMPRESSED_TILE[1]}; the 64x64 image has four "
            "tiles, and right/bottom edge cells are zero-padded outside the image border."
        )
    return "\n".join(
        [
            f"# Fixture set `{fset.name}`",
            "",
            (
                "Re-encoding of `mantis-64`: `frames/00.tif`, `frames/01.tif`, and "
                "`frames/02.tif` map directly to the same source files; `reference.tif` maps "
                "to `mantis-64/reference.tif`."
            ),
            "",
            (
                f"TIFF Compression tag: **{fset.compression}** ({fset.codec}). Predictor: "
                f"**{fset.predictor}**. Byte order: **{fset.byte_order}**. Layout: "
                f"**{fset.layout}**."
            ),
            "",
            layout,
            "",
        ]
    )


def _generate_compressed(source_fixtures: str, out_dir: str) -> list:
    source_set = os.path.join(source_fixtures, "mantis-64")
    source_files = [
        *(os.path.join(source_set, "frames", f"{index:02d}.tif") for index in range(3)),
        os.path.join(source_set, "reference.tif"),
    ]
    missing = [path for path in source_files if not os.path.isfile(path)]
    if missing:
        raise FixtureError("compressed fixtures need committed mantis-64 files: " + ", ".join(missing))

    produced = []
    for fset in COMPRESSED_SETS:
        frames_dir = os.path.join(out_dir, fset.name, "frames")
        os.makedirs(frames_dir, exist_ok=True)
        for index, source in enumerate(source_files[:3]):
            rel = f"{fset.name}/frames/{index:02d}.tif"
            _crop(
                source,
                os.path.join(out_dir, rel),
                (0, 0, 64, 64),
                f"FocusForge fixture {fset.name}/frames/{index:02d}",
                byte_order=fset.byte_order,
                rows_per_strip=COMPRESSED_ROWS_PER_STRIP,
                compression=fset.compression,
                predictor=fset.predictor,
                tile=COMPRESSED_TILE if fset.layout == "tile" else None,
            )
            produced.append(rel)
        rel = f"{fset.name}/reference.tif"
        _crop(
            source_files[3],
            os.path.join(out_dir, rel),
            (0, 0, 64, 64),
            f"FocusForge fixture {fset.name}/reference",
            byte_order=fset.byte_order,
            rows_per_strip=COMPRESSED_ROWS_PER_STRIP,
            compression=fset.compression,
            predictor=fset.predictor,
            tile=COMPRESSED_TILE if fset.layout == "tile" else None,
        )
        produced.append(rel)
        rel = f"{fset.name}/README.md"
        with open(os.path.join(out_dir, rel), "w", encoding="utf-8") as fh:
            fh.write(_compressed_readme(fset))
        produced.append(rel)
    return produced


def _folder_set_files() -> dict:
    """Relative path -> source frame index (None for a text file), per folder set."""
    files = {}
    for sub in ("a", "b"):
        for index in range(FOLDER_SET_FRAMES):
            files[f"open-two-stacks/{sub}/{index:02d}.tif"] = index
    for index in range(FOLDER_SET_FRAMES):
        files[f"open-root-and-subfolder/{index:02d}.tif"] = index
        files[f"open-rejected/frames/{index:02d}.tif"] = index
    return files


def _folder_set_readme(name: str) -> str:
    if name == "open-two-stacks":
        body = (
            "Two folders, `a/` and `b/`, each holding the three frames of `mantis-64` "
            "as `00.tif` to `02.tif`; the root holds no frames. Opening the root asks "
            "whether the six frames are one project or two."
        )
    elif name == "open-root-and-subfolder":
        body = (
            "The three frames of `mantis-64` as `00.tif` to `02.tif` in the root, plus "
            "`notes/readme.txt`, a subfolder with no frames. Opening the root gives one "
            "stack and skips `notes/`."
        )
    else:
        body = (
            "`frames/00.tif` and `frames/01.tif` are the first two frames of `mantis-64`. "
            "`frames/02.tif` is the third frame cut in half inside its strip data: the "
            "header and thumbnail path read, the pixel data does not. Opening the folder "
            "succeeds and fusing it fails."
        )
    return f"# Fixture set `{name}`\n\n{body}\n"


def _generate_folder_sets(source_fixtures: str, out_dir: str) -> list:
    source_frames = os.path.join(source_fixtures, "mantis-64", "frames")
    missing = [
        path
        for path in (os.path.join(source_frames, f"{i:02d}.tif") for i in range(FOLDER_SET_FRAMES))
        if not os.path.isfile(path)
    ]
    if missing:
        raise FixtureError("folder fixtures need committed mantis-64 files: " + ", ".join(missing))

    produced = []
    for rel, index in _folder_set_files().items():
        source = os.path.join(source_frames, f"{index:02d}.tif")
        dest = os.path.join(out_dir, rel)
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        shutil.copyfile(source, dest)
        produced.append(rel)

    rejected = os.path.join(out_dir, "open-rejected", "frames", f"{FOLDER_SET_FRAMES - 1:02d}.tif")
    info = rawtiff.read_info(rejected)
    with open(rejected, "rb") as fh:
        ifd_offset = int.from_bytes(fh.read(8)[4:8], "little" if info.byte_order == "II" else "big")
    first_strip = min(info.strip_offsets)
    cut = (first_strip + info.file_size) // 2
    if not ifd_offset < first_strip < cut:
        raise FixtureError(f"{rejected}: no strip data to cut inside")
    with open(rejected, "r+b") as fh:
        fh.truncate(cut)

    skipped_dir_file = os.path.join(out_dir, "open-root-and-subfolder", "notes", "readme.txt")
    os.makedirs(os.path.dirname(skipped_dir_file), exist_ok=True)
    with open(skipped_dir_file, "w", encoding="utf-8") as fh:
        fh.write("Not a frame: a folder with no TIFF images.\n")
    produced.append("open-root-and-subfolder/notes/readme.txt")

    for name in FOLDER_SET_NAMES:
        rel = f"{name}/README.md"
        with open(os.path.join(out_dir, rel), "w", encoding="utf-8") as fh:
            fh.write(_folder_set_readme(name))
        produced.append(rel)
    return produced


def generate(stack_dir: str, out_dir: str, date: str) -> list:
    """Write every fixture set plus the manifest. Returns generated relative paths."""
    frame_names = list_frames(stack_dir)
    reference = os.path.join(stack_dir, REFERENCE_NAME)
    if not os.path.isfile(reference):
        raise FixtureError(f"no {REFERENCE_NAME} in {stack_dir}")

    produced = []
    for fset in SETS:
        set_dir = os.path.join(out_dir, fset.name)
        frames_dir = os.path.join(set_dir, "frames")
        # Only the generated subtree is cleared: a hand-curated golden/ beside it
        # is ours to keep, and a shrunken index list must not leave orphan frames.
        shutil.rmtree(frames_dir, ignore_errors=True)
        os.makedirs(frames_dir, exist_ok=True)

        for nn, idx in enumerate(fset.indices):
            if idx >= len(frame_names):
                raise FixtureError(
                    f"set {fset.name} wants frame index {idx} but the stack has "
                    f"{len(frame_names)} frames"
                )
            name = frame_names[idx]
            rel = f"{fset.name}/frames/{nn:02d}.tif"
            _crop(
                os.path.join(stack_dir, name),
                os.path.join(out_dir, rel),
                fset.rect,
                f"FocusForge fixture {fset.name}/{nn:02d} from {name} "
                f"rect {_rect_str(fset.rect)}",
            )
            produced.append(rel)

        rel = f"{fset.name}/reference.tif"
        _crop(
            reference,
            os.path.join(out_dir, rel),
            fset.rect,
            f"FocusForge fixture {fset.name}/reference from {REFERENCE_NAME} "
            f"rect {_rect_str(fset.rect)}",
        )
        produced.append(rel)

        if fset.mm_variant:
            rel = f"{fset.name}/{MM_VARIANT_NAME}"
            _crop(
                reference,
                os.path.join(out_dir, rel),
                fset.rect,
                f"FocusForge fixture {fset.name}/{MM_VARIANT_NAME} from {REFERENCE_NAME} "
                f"rect {_rect_str(fset.rect)} -- same pixels as reference.tif, written in "
                f"{REFERENCE_NAME}'s own layout (big-endian MM, "
                f"RowsPerStrip={MM_VARIANT_ROWS_PER_STRIP})",
                byte_order="MM",
                rows_per_strip=MM_VARIANT_ROWS_PER_STRIP,
            )
            produced.append(rel)

        rel = f"{fset.name}/README.md"
        with open(os.path.join(out_dir, rel), "w", encoding="utf-8") as fh:
            fh.write(_set_readme(fset, stack_dir, frame_names))
        produced.append(rel)

    produced += _generate_compressed(out_dir, out_dir)
    produced += _generate_folder_sets(out_dir, out_dir)

    rel = "README.md"
    with open(os.path.join(out_dir, rel), "w", encoding="utf-8") as fh:
        fh.write(_root_readme(stack_dir, frame_names, date))
    produced.append(rel)

    write_manifest(out_dir)
    produced.append("MANIFEST.sha256")
    return sorted(produced)


def generate_compressed(source_fixtures: str, out_dir: str, date: str) -> list:
    """Write compressed mantis-64 variants from committed source fixtures."""
    os.makedirs(out_dir, exist_ok=True)
    produced = _generate_compressed(source_fixtures, out_dir)
    produced += _generate_folder_sets(source_fixtures, out_dir)
    frame_names = ["00.tif", "01.tif", "02.tif"]
    with open(os.path.join(out_dir, "README.md"), "w", encoding="utf-8") as fh:
        fh.write(_root_readme(source_fixtures, frame_names, date))
    produced.append("README.md")
    write_manifest(out_dir)
    produced.append("MANIFEST.sha256")
    return sorted(produced)


def write_manifest(out_dir: str) -> list:
    """Hash every file under out_dir except the manifest itself, sha256sum format.

    Everything, not just what this run produced: a golden image added later is
    covered without touching this code, and tools/check-fixtures.sh can then
    treat an unlisted file as a failure rather than as a maybe.
    """
    entries = []
    for root, dirs, files in os.walk(out_dir):
        dirs.sort()
        for name in sorted(files):
            rel = os.path.relpath(os.path.join(root, name), out_dir)
            if rel == "MANIFEST.sha256":
                continue
            entries.append((rel.replace(os.sep, "/"), os.path.join(root, name)))
    entries.sort(key=lambda e: e[0].encode("utf-8"))
    lines = [f"{rawtiff.sha256_file(path)}  {rel}\n" for rel, path in entries]
    with open(os.path.join(out_dir, "MANIFEST.sha256"), "w", encoding="utf-8") as fh:
        fh.writelines(lines)
    return [rel for rel, _ in entries]


def _normalized(path: str) -> bytes:
    """File bytes with the one generation date blanked, for --check comparison."""
    with open(path, "rb") as fh:
        return DATE_LINE_RE.sub(b"Generated: <date>", fh.read())


def _check_generated(produced: list, out_dir: str, tmp: str) -> list:
    differences = []
    for rel in produced:
        want = os.path.join(tmp, rel)
        got = os.path.join(out_dir, rel)
        if not os.path.exists(got):
            differences.append(f"missing: {rel}")
        elif rel == "MANIFEST.sha256" or rel.endswith("README.md"):
            if rel == "MANIFEST.sha256":
                def listed(path):
                    with open(path, encoding="utf-8") as fh:
                        return {
                            line
                            for line in fh.read().splitlines()
                            if line and not line.endswith("  README.md")
                        }

                stale = listed(want) - listed(got)
                if stale:
                    differences.append(
                        "MANIFEST.sha256: "
                        + ", ".join(sorted(line.split("  ", 1)[-1] for line in stale))
                        + " not listed with the regenerated hash"
                    )
            elif _normalized(want) != _normalized(got):
                differences.append(f"differs: {rel}")
        elif not filecmp.cmp(want, got, shallow=False):
            differences.append(f"differs: {rel}")
    return differences


def _find_orphans(out_dir: str, expected: set, fixture_sets: tuple, folder_sets: tuple = ()) -> list:
    differences = []
    for name in folder_sets:
        for root, _dirs, files in os.walk(os.path.join(out_dir, name)):
            for file in sorted(files):
                rel = os.path.relpath(os.path.join(root, file), out_dir).replace(os.sep, "/")
                if rel not in expected:
                    differences.append(f"orphan: {rel} is not produced by the generator")
    for fset in fixture_sets:
        frames_dir = os.path.join(out_dir, fset.name, "frames")
        if not os.path.isdir(frames_dir):
            continue
        for name in sorted(os.listdir(frames_dir)):
            rel = f"{fset.name}/frames/{name}"
            if rel not in expected:
                differences.append(f"orphan: {rel} is not produced by the generator")
    return differences


def check(stack_dir: str, out_dir: str) -> int:
    if not os.path.isdir(out_dir):
        print(f"FAIL: no fixture tree at {out_dir}")
        return 1

    with tempfile.TemporaryDirectory(prefix="focusforge-fixtures-") as tmp:
        produced = generate(stack_dir, tmp, date="<date>")
        differences = _check_generated(produced, out_dir, tmp)
        differences += _find_orphans(out_dir, set(produced), (*SETS, *COMPRESSED_SETS), FOLDER_SET_NAMES)

    if differences:
        print(f"FAIL: {out_dir} does not match the generator")
        for line in differences:
            print(f"  - {line}")
        return 1
    print("OK: fixtures match the generator")
    return 0


def check_compressed(source_fixtures: str, out_dir: str) -> int:
    if not os.path.isdir(out_dir):
        print(f"FAIL: no fixture tree at {out_dir}")
        return 1
    with tempfile.TemporaryDirectory(prefix="focusforge-compressed-fixtures-") as tmp:
        shutil.copytree(source_fixtures, tmp, dirs_exist_ok=True)
        produced = generate_compressed(source_fixtures, tmp, date="<date>")
        differences = _check_generated(produced, out_dir, tmp)
        differences += _find_orphans(out_dir, set(produced), COMPRESSED_SETS, FOLDER_SET_NAMES)
    if differences:
        print(f"FAIL: {out_dir} compressed fixtures do not match the generator")
        for line in differences:
            print(f"  - {line}")
        return 1
    print("OK: compressed fixtures match the generator")
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="make_fixtures.py",
        description="Cut the checked-in fixture crops out of the reference stack.",
    )
    parser.add_argument("--stack", default=DEFAULT_STACK, help="reference stack directory")
    parser.add_argument("--out", default=DEFAULT_OUT, help="fixtures directory to write")
    parser.add_argument(
        "--compressed-only",
        action="store_true",
        help="write or check compressed mantis-64 variants without the reference stack",
    )
    parser.add_argument(
        "--source-fixtures",
        help="committed fixture tree supplying the uncompressed mantis-64 source",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="regenerate into a temporary directory and compare; exit 1 on any difference",
    )
    parser.add_argument(
        "--manifest-only",
        action="store_true",
        help="re-hash --out in place (e.g. after adding a golden/ file by hand) without "
        "touching the reference stack or regenerating any fixture",
    )
    args = parser.parse_args(argv)

    try:
        if args.compressed_only and args.manifest_only:
            raise FixtureError("--compressed-only and --manifest-only cannot be combined")
        if args.compressed_only:
            source = args.source_fixtures or args.out
            if args.check:
                return check_compressed(source, args.out)
            produced = generate_compressed(source, args.out, date=datetime.date.today().isoformat())
            total = sum(os.path.getsize(os.path.join(args.out, rel)) for rel in produced)
            print(f"wrote {len(produced)} files ({total / 1024:.0f} KiB) to {args.out}")
            return 0
        if args.manifest_only:
            listed = write_manifest(args.out)
            print(f"wrote MANIFEST.sha256 covering {len(listed)} files under {args.out}")
            return 0
        if args.check:
            return check(args.stack, args.out)
        os.makedirs(args.out, exist_ok=True)
        produced = generate(
            args.stack, args.out, date=datetime.date.today().isoformat()
        )
        total = sum(os.path.getsize(os.path.join(args.out, r)) for r in produced)
        print(f"wrote {len(produced)} files ({total / 1024:.0f} KiB) to {args.out}")
        for rel in produced:
            print(f"  {rel}")
        return 0
    except FixtureError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    except rawtiff.UnsupportedTiff as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
