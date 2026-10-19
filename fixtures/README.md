# Fixtures

Small crops of the reference focus stack, checked in so `cargo test` has real sensor data to work on without the 16 GB stack being anywhere near the repository.

Generated: 2026-10-18

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
| `open-two-stacks` | 6 | `4064,2700,64,64` | 64x64 |
| `open-root-and-subfolder` | 3 | `4064,2700,64,64` | 64x64 |
| `open-rejected` | 3 | `4064,2700,64,64` | 64x64 |
| `projects` | 3 | `4064,2700,64,64` | 64x64 |

Each set holds `frames/NN.tif` in focus order, `reference.tif`, and a `README.md` naming the source frame behind every `NN`. Outputs are little-endian, uncompressed, chunky, one strip per row. The `mantis-64-` sets add LZW and deflate encodings in both byte orders, with strips or zero-padded tiles and Predictor 1 or 2.

`blackmagic-micro-dng` is one lossless-JPEG DNG camera frame with its own README; it has no `frames/` directory or `reference.tif`.

`open-two-stacks`, `open-root-and-subfolder` and `open-rejected` are folder shapes for the open paths, copied byte for byte from `mantis-64/frames` (`open-rejected/frames/02.tif` is then cut in half inside its strip data). They have their own READMEs and no `reference.tif`.

## Project files

`projects/` holds four project files over one shared `projects/frames/` (copies of `mantis-64/frames`). They are not regenerated: each records its frames' modified time and device/inode, so a rerun writes different bytes. They are registered in `MANIFEST.sha256` as they stand. Each opens to a different outcome:

| file | opens as |
|---|---|
| `valid.ffp` | one project over the 3 frames |
| `mismatch.ffp` | the fingerprint-mismatch question: its frames `01` and `02` were swapped when it was written |
| `missing.ffp` | a project whose frames (`gone/NN.tif`) are not there |
| `corrupt.ffp` | a refusal: the file is not a project |

Produced in a scratch directory `S`, with the output directory set to the frames' parent so the recorded paths are relative:

```
mkdir -p S/projects/frames && cp mantis-64/frames/*.tif S/projects/frames/
focusforge-cli stack S/projects/frames -o S/projects --algorithm laplacian
    # S/projects/frames.ffp -> projects/valid.ffp
# swap 01.tif and 02.tif in S/projects/frames, then the same stack command
    # -> projects/mismatch.ffp
mkdir -p S/projects/gone && cp mantis-64/frames/*.tif S/projects/gone/
focusforge-cli stack S/projects/gone -o S/projects --algorithm laplacian
    # S/projects/gone.ffp -> projects/missing.ffp; gone/ is not kept
printf 'not a project\n' > projects/corrupt.ffp
```

Then `tools/make-fixtures.sh --manifest-only` lists the files. `frames/` holds only the three `.tif` files: the stack command writes its report beside the frames it reads, so it is never run over `projects/frames/`.

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
