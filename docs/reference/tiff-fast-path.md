# TIFF fast path

FocusForge's fast TIFF path is zero-decode: for a file with `Compression = 1`
(uncompressed), every row's bytes sit at a file offset that is computable
straight from the header, so a tile is sliced directly out of a memory
mapping with no frame ever fully decoded. This page describes that fast path
and the writer's output.

## What the reader accepts

A TIFF qualifies for the fast path when it is all of:

- **Classic TIFF** (magic `42`), not BigTIFF (magic `43`)
- **Uncompressed**: `Compression = 1`
- **Strip-based**, not tiled: no `TileWidth` tag present
- **Chunky**: `PlanarConfiguration = 1`
- **16-bit** on every sample: `BitsPerSample` is `16` for every channel
- **Unsigned integer** samples: `SampleFormat = 1` (or absent, which the TIFF
  spec treats as unsigned)

Byte order (`II` little-endian or `MM` big-endian) and `RowsPerStrip` are
*not* restrictions — the reader normalizes both per frame, at any strip
height, so a directory that mixes an `II`/`RowsPerStrip=1` frame with an
`MM`/`RowsPerStrip=8` frame reads both frames correctly.

A file that fails any of the six conditions above is not treated as broken:
`focusforge-cli info` shows `mmap fast path: no — <reason>` and the same
reason is available as the `refusal` field in `info --json`. A 16-bit
unsigned chunky Classic TIFF with LZW or Deflate compression, or a tiled
layout, is read through the compatible TIFF path instead. Other layouts are
refused by name when a stack is opened.

## What opening a stack checks across frames

Opening a directory of frames is lenient on a different axis than a single
file's fast-path check: it requires every frame to agree on dimensions,
bits per sample, samples per pixel, photometric interpretation and planar
configuration, naming the first frame that differs. It deliberately does
**not** require every frame to share one byte order or one `RowsPerStrip` —
each frame's own header settles that.

## What the writer produces

The writer emits exactly one shape: little-endian (`II`), `RowsPerStrip = 1`
(one strip per image row), chunky, 16-bit unsigned samples, uncompressed.
It computes every strip offset once, when the file is created, and accepts
tile writes in any order afterward — it never buffers a whole frame.
Finishing a frame that a caller left partly uncovered is refused: every row
must end up covered by exactly one contiguous span before the file is
considered complete.

## Why this matters

Loading a full 61-frame stack as decoded pixels does not fit in a
workstation's memory, so the reader's tile-only, no-decode design is a
correctness requirement, not a performance tweak: any operation that
assembles a whole frame is a way to run out of RAM, not a shortcut.
