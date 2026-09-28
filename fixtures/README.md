# Fixtures

Small crops of the reference focus stack, checked in so `cargo test` has real sensor data to work on without the 16 GB stack being anywhere near the repository.

Generated: 2026-09-28

## Sets

| set | frames | crop rect (x,y,w,h) | pixels |
|---|---|---|---|
| `mantis-64` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-512` | 8 | `3840,2476,512,512` | 512x512 |
| `mantis-64-lzw-p1-strip-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-lzw-p1-strip-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-lzw-p1-tile-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-lzw-p1-tile-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-lzw-p2-strip-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-lzw-p2-strip-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-lzw-p2-tile-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-lzw-p2-tile-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p1-strip-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p1-strip-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p1-tile-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p1-tile-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p2-strip-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p2-strip-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p2-tile-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-deflate-p2-tile-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p1-strip-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p1-strip-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p1-tile-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p1-tile-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p2-strip-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p2-strip-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p2-tile-ii` | 3 | `4064,2700,64,64` | 64x64 |
| `mantis-64-old-deflate-p2-tile-mm` | 3 | `4064,2700,64,64` | 64x64 |
| `blackmagic-micro-dng` | 1 | single DNG | 1952x1104 |

Each set holds `frames/NN.tif` in focus order, `reference.tif`, and a `README.md` naming the source frame behind every `NN`. Outputs are little-endian, uncompressed, chunky, one strip per row. The `mantis-64-` sets add LZW and deflate encodings in both byte orders, with strips or zero-padded tiles and Predictor 1 or 2.

`blackmagic-micro-dng` is one lossless-JPEG DNG camera frame with its own README; it has no `frames/` directory or `reference.tif`.

With one deliberate exception: `mantis-64/reference-MM-rps8.tif` is the reference crop left in `Mantis_Stacked.tif`'s own layout (big-endian `MM`, RowsPerStrip=8). Normalizing every fixture would leave nothing here exercising the layout the reader meets on the real file. It holds the same samples as that set's `reference.tif` and differs from it on disk.

The source files are named in each set README. They live outside the repository and are not redistributed here.

## `reference.tif` is not a golden image

The two are different kinds of claim and must not be confused:

- **`reference.tif`** is the same crop of a stack produced by a *different* stacker. It is a similarity floor: our output should be close to it, and a large divergence means something is wrong. It is never an exact target -- a different algorithm legitimately picks different pixels -- so it is only ever compared with a loose tolerance.
- **`golden/*.tif`** is our own output, reviewed by a human and then frozen. It is a regression target: a change to it is a change in behaviour that someone has to look at and approve. Compared with a tight tolerance.

A test that asserts an exact match against `reference.tif` is asserting that we reimplemented someone else's stacker, which is not a goal.

## Reproducibility

Regeneration is byte-for-byte deterministic. `MANIFEST.sha256` covers every file here; changed, missing, and unlisted files are errors.

Never hand-edit anything in this tree.
