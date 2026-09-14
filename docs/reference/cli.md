# CLI reference

`focusforge-cli` is the headless interface to FocusForge. Every algorithm is
reachable here before any GUI control exists.

```
focusforge-cli <COMMAND>
```

## `info`

```
focusforge-cli info [--json] <PATH>
```

Prints TIFF metadata for a single file, or for every TIFF directly inside a
directory (or inside a `frames/` subdirectory of it, when that exists).
Entries are matched by `.tif`/`.tiff` extension, case-insensitively, and are
not recursed into further. `info` reads headers only — it never opens a tile
or a pixel, which is why it can report on the 16 GB, 62-file reference stack
in well under a second.

`PATH` may be a single file or a directory:

- A single file that fails to parse as a TIFF is a hard error: `info` prints
  `error: ...` to stderr and exits 1.
- Inside a directory, a file that fails to parse is *not* fatal to the whole
  command: it is reported as one entry with an `error` field and counted in
  `unreadable`, while every readable sibling still gets a full report. A
  directory with zero matching files is a hard error.
- A file that parses but falls outside the fast read path (see
  [`tiff-fast-path.md`](tiff-fast-path.md)) is reported normally, with
  `mmap fast path: no` and the reason named — this is not an error.

### Human output

One block per file:

```
path/to/frame.tif
  64x64 3ch 16-bit II compression=1 planar=1 photometric=2
  rows_per_strip=1 strips=64 file_size=25338
  description: FocusForge fixture mantis-64/00 from 20260829-IMG_9800.tif rect 4064,2700,64,64
  mmap fast path: yes
```

The `description:` line is only printed when the file carries an
`ImageDescription` tag; a file with none keeps its three-line block.

For a directory, a trailing summary follows every file block:

```
3 TIFFs, 76014 bytes total
uniform layout: yes
```

When the frames disagree on layout, the summary names the odd one out and
what differs, for example `uniform layout: no — reference.tif is MM/rps=8, others II/rps=1`.
A directory report also prints `unreadable: N` when any file failed to
parse.

### `--json`

`--json` emits the same information as one JSON document instead. The shape
is the same whether `PATH` is a file or a directory — a single-file
invocation reports `"is_directory": false`, `"frame_count": 1` and a
one-element `"files"` array. This schema is a compatibility promise: a field
is added, never renamed or removed, in later versions.

```json
{
  "path": "fixtures/mantis-64",
  "is_directory": true,
  "frame_count": 3,
  "total_bytes": 76014,
  "unreadable_count": 0,
  "uniform_layout": true,
  "uniform_detail": null,
  "files": [
    {
      "name": "00.tif",
      "path": "fixtures/mantis-64/frames/00.tif",
      "width": 64,
      "height": 64,
      "bits_per_sample": [16, 16, 16],
      "samples_per_pixel": 3,
      "byte_order": "II",
      "compression": 1,
      "photometric": 2,
      "planar_config": 1,
      "rows_per_strip": 1,
      "strip_count": 64,
      "file_size": 25338,
      "description": "FocusForge fixture mantis-64/00 from 20260829-IMG_9800.tif rect 4064,2700,64,64",
      "mmap_fast_path": true,
      "refusal": null,
      "error": null
    }
  ]
}
```

Top-level fields:

| field | meaning |
|---|---|
| `path` | the path given on the command line |
| `is_directory` | whether `path` was a directory |
| `frame_count` | number of TIFFs found (files attempted, whether or not they parsed) |
| `total_bytes` | sum of on-disk file sizes across all of them |
| `unreadable_count` | how many failed to parse as a TIFF at all |
| `uniform_layout` | `true` when every parsed frame agrees on dimensions, bit depth, samples per pixel, byte order, `RowsPerStrip`, compression, planar configuration and photometric interpretation |
| `uniform_detail` | `null` when uniform, otherwise a sentence naming the outlier frame and what differs |
| `files` | one entry per file, in the order found |

Per-file fields (all `null` when `error` is set, meaning the file did not
parse as a TIFF at all):

| field | meaning |
|---|---|
| `name`, `path` | file name and full path |
| `width`, `height` | pixel dimensions |
| `bits_per_sample` | one entry per sample (channel) |
| `samples_per_pixel` | channel count |
| `byte_order` | `"II"` (little-endian) or `"MM"` (big-endian) |
| `compression` | the TIFF `Compression` tag value; `1` is uncompressed |
| `photometric` | the TIFF `PhotometricInterpretation` tag value |
| `planar_config` | the TIFF `PlanarConfiguration` tag value; `1` is chunky |
| `rows_per_strip` | the TIFF `RowsPerStrip` tag value |
| `strip_count` | number of strips implied by height and `rows_per_strip` |
| `file_size` | on-disk size in bytes |
| `description` | the TIFF `ImageDescription` tag, `null` when the file has none — a Z-Map carries its frame count here |
| `mmap_fast_path` | whether the tile reader's fast path applies to this file |
| `refusal` | `null` when `mmap_fast_path` is `true`, otherwise the reason it is not (compression, tiling, planar layout, bit depth or sample format) |
| `error` | `null` on a successful parse, otherwise why the file could not be read as a TIFF at all |

## `stack`

```
focusforge-cli stack [OPTIONS] --output <OUTPUT> <DIR>
```

Fuses every TIFF frame in `<DIR>` (non-recursive, descending into
`<DIR>/frames` when that subdirectory exists) into one composite through a
registered stacking algorithm, and writes it to `--output`. The frames are
read tile by tile through an out-of-core scheduler bounded by a fixed memory
budget, so the command never needs to hold a whole stack — or a whole frame
— in RAM.

**Alignment runs first, by default.** Before fusion, `stack` computes each
frame's transform against a reference frame (the same solver `align` drives
directly — see below), warps every frame tile through it, and crops the
output to the region every frame actually covers. Both are on unless turned
off:

| flag | meaning |
|---|---|
| `-o, --output <OUTPUT>` | composite output path (required) |
| `--algo <ALGO>` | registered stacking algorithm id (required; see `algorithms` below for the list) |
| `--zmap <ZMAP>` | Z-Map output path; omitted entirely when this flag is absent |
| `--set <KEY=VALUE>` | set one algorithm parameter, repeatable; a repeated key keeps the last value |
| `--json` | emit the frozen JSON report instead of human-readable text |
| `--no-align` | skip alignment: frame tiles are fused unwarped and the output stays full-frame |
| `--no-crop` | skip the auto-crop: the output stays at the full frame extent even with alignment on |
| `--align-algo <ALGO>` | alignment algorithm id; optional, defaults to the sole registered alignment algorithm |
| `--align-set <KEY=VALUE>` | set one alignment parameter, repeatable; separate from `--set`, which is the stacking algorithm's own |
| `--no-gpu` | force the CPU path even when a usable GPU adapter is present; only exists when the binary was built with `--features gpu` |

Every algorithm also writes a **Z-Map** when `--zmap` is given: a
single-channel 16-bit TIFF with one value per output pixel naming the index
of the source frame that pixel came from. With alignment and the crop both
on (the default), the composite and the Z-Map are declared at the crop
rect's dimensions, not the source frame's; `--no-crop` keeps them at the
full frame.

Refusals happen before the input directory is read where possible: an
unknown `--algo` id or a `--set` value that fails the algorithm's own
parameter schema (unknown key, out-of-range value, an enum value that does
not name a declared variant) is reported and the command exits 1 without
opening a single frame. A frame that fails to register is a refusal too,
naming every failed frame, exit 1 — see `focusforge-cli align --help` for
how to soften that. An empty auto-crop (the frames' footprints do not
intersect) is refused before any output file is created, naming `--no-crop`
as the way through.

**GPU use is automatic, never an opt-in flag.** Built with `--features
gpu`, `stack` uses a usable GPU adapter automatically for the `laplacian`
algorithm — the only one with a GPU implementation; `weighted-average` and
`depth-map` always run on the CPU. `--no-gpu` forces the CPU path
regardless of algorithm. A missing or unusable adapter falls back to the
CPU path on its own, printing why on stderr, never silently and never as a
hard failure. Either path produces the same result — every GPU kernel is
proven to match its CPU twin to within 1 LSB at 16-bit — so switching
backends changes speed, not the output; `stack --json`'s `backend` field
names which one actually ran.

```
$ focusforge-cli stack fixtures/mantis-64 -o .scratch/out.tif --algo laplacian --zmap .scratch/zmap.tif --no-align
fixtures/mantis-64 (3 frames)
64x64 3ch
laplacian - Laplacian Pyramid
levels = 4 (Auto)
tile_size=512 levels=4 margin=48 tiles=1 in_flight_limit=32
backend: cpu
wrote: .scratch/out.tif
wrote: .scratch/zmap.tif
wall: 0.047s
```

(`--no-align` keeps this example to the fusion stage the backend field is
about; `mantis-64`'s own frames diverge under the default `pyramidal-ecc`
alignment in strict mode, so a real run without either `--no-align` or a
softened failure policy would refuse instead — see
[`guide/alignment.md`](../guide/alignment.md#failure-is-strict-by-default) for
what that policy is and why it defaults to strict. `backend` reads `"cpu"`
because this binary was built without `--features gpu`; built with it and a
usable adapter present, it instead names the adapter, for example `NVIDIA
GeForce RTX 5070 Ti (backend=Vulkan, driver=NVIDIA, driver_info=595.84)`.)

### `--json`

`--json` emits a report with the input frames, the resolved algorithm and
its parameters (value and provenance — whether each came from its default,
an explicit `--set`, or an `auto` hook), the tile geometry the scheduler
planned, and the paths written. Like `info --json` and `algorithms --json`,
this is a frozen shape: a field is added, never renamed or removed, in
later versions.

Additive fields from alignment: `output_width`/`output_height` (the actual
output extent — equal to `width`/`height` unless a crop was applied),
`crop` (`null` under `--no-crop`, otherwise `{x, y, width, height}`),
`description` (the `ImageDescription` string written to the composite, or
`null` under `--no-align`), and `align` — `null` under `--no-align`,
otherwise the alignment algorithm's id, display name, parameters,
reference frame, reference policy, failure policy, failure count and every
frame's transform, in the same shape `align --json` reports (see below).
Additive field from the GPU path: `backend` — `"cpu"`, or the selected
adapter's name and driver info.

```json
{
  "input": "fixtures/mantis-64",
  "frame_count": 3,
  "frames": ["00.tif", "01.tif", "02.tif"],
  "width": 64,
  "height": 64,
  "samples_per_pixel": 3,
  "algorithm": "laplacian",
  "display_name": "Laplacian Pyramid",
  "params": [{ "id": "levels", "value": 4, "provenance": "Auto" }],
  "tile_size": 512,
  "levels": 4,
  "margin": 48,
  "tiles": 1,
  "frame_tiles_read": 3,
  "in_flight_limit": 32,
  "peak_in_flight": 1,
  "outputs": [".scratch/out.tif", ".scratch/zmap.tif"],
  "wall_seconds": 0.030267391,
  "backend": "cpu"
}
```

## `align`

```
focusforge-cli align [OPTIONS] <DIR>
```

Computes and reports each frame's transform against a reference frame,
without fusing anything — the same alignment stage `stack` runs internally,
reachable on its own so a result is checkable before any fusion runs.

Options:

| flag | meaning |
|---|---|
| `--algo <ALGO>` | registered alignment algorithm id; optional, defaults to the sole registered one |
| `--json` | emit the frozen JSON report instead of human-readable text |
| `--write-warped <DIR>` | write one warped TIFF per frame into `<DIR>`, at source dimensions, uncropped |
| `--set <KEY=VALUE>` | set one algorithm parameter, repeatable; a repeated key keeps the last value |
| `--no-gpu` | force the CPU path even when a usable GPU adapter is present; only exists when the binary was built with `--features gpu`. Meaningful only together with `--write-warped` — the ECC solver `align` reports on is CPU-only regardless |

A frame whose registration diverges refuses the run, exit 1, naming every
failed frame — this is the default, and it is not a flag; run
`focusforge-cli align --help` for the full option list, including how to
soften it.

```
$ focusforge-cli align path/to/frames --algo pyramidal-ecc
path/to/frames (8 frames)
pyramidal-ecc - Pyramidal ECC (reference=3 policy=Anchor failure_policy=strict failures=0)
rho = ECC correlation in [-1, 1], not a pixel distance
00.tif    tx=+12.41 ty=-3.07 scale=1.000842 rot=+0.0131deg  converged iters=12 rho=0.9814
...
03.tif    tx=+0.00 ty=+0.00 scale=1.000000 rot=+0.0000deg  converged iters=0 rho=1.0000 (reference)
...
auto-crop: 31,40 422x424
```

`rho` is an ECC correlation in `[-1, 1]`, not a pixel distance — higher is
better. The reference frame's own row is marked `(reference)`; a row for a
frame that fell back to identity is marked `[fallback]`. The final line
reports the auto-crop rect `stack` would apply by default, or
`auto-crop: empty (frame footprints do not intersect)` when the frames'
footprints share no common region.

### `--json`

Frozen, additive-only, the same discipline as every other `--json` shape in
this reference:

```json
{
  "input": "fixtures/mantis-512",
  "frame_count": 8,
  "frames": ["00.tif", "01.tif", "..."],
  "width": 512,
  "height": 512,
  "samples_per_pixel": 3,
  "algorithm": "pyramidal-ecc",
  "display_name": "Pyramidal ECC",
  "params": [{ "id": "mode", "value": "rotation-scale", "provenance": "Default" }],
  "reference": 3,
  "reference_policy": "anchor",
  "failure_policy": "lax",
  "failure_count": 1,
  "alignments": [
    {
      "frame": 0,
      "name": "00.tif",
      "transform": { "m": [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] },
      "status": "diverged",
      "reason": "lemma-one-region",
      "iterations": 0,
      "residual": -0.1488,
      "fallback": true,
      "scale": 1.0,
      "rotation_deg": 0.0,
      "tx": 0.0,
      "ty": 0.0
    }
  ],
  "crop": { "x": 31, "y": 40, "width": 422, "height": 424 },
  "warped_dir": null,
  "wall_seconds": 8.497
}
```

`transform.m` is the raw 3x3 matrix (reference-to-frame); `scale`,
`rotation_deg`, `tx`, `ty` are its decomposition, exact for both shipped
modes. `status` is `"converged"`, `"hit-cap"` or `"diverged"`; only a
diverged frame carries `reason`. `crop` is `null` when the frames' footprints
do not intersect.

### `--write-warped`

Writes one TIFF per input frame into the given directory, at the frame's own
source dimensions (uncropped — the crop is reported, not applied, on this
path, so an uncovered rim stays visible for a manual check). Useful to
inspect an alignment result visually before running `stack`.

## `algorithms`

```
focusforge-cli algorithms [--json] [--set KEY=VALUE]... [ID]
```

Lists every algorithm registered with `focusforge-core`, or — given `ID` —
prints that one algorithm's full parameter schema. Every algorithm, stacking
or alignment, is registered under one shared id space and reachable through
this one subcommand; there is no separate listing per family.

Three stacking algorithms are registered in a normal build:

```
$ focusforge-cli algorithms
depth-map - Depth Map (stacking)
laplacian - Laplacian Pyramid (stacking)
weighted-average - Weighted Average (stacking)
```

- `laplacian` — Laplacian-pyramid coefficient selection
- `weighted-average` — per-pixel sharpness-weighted blend
- `depth-map` — per-pixel selection from a smoothed sharpness surface

`ID` is required for `--set`: `--set` with no `ID` is rejected before
anything else runs, since there is no schema to validate against.

### Listing

With no `ID`, human output is one line per algorithm — id, display name,
family — or the literal line `no algorithms registered` when the registry is
empty. `--json` emits `{"algorithms": [...]}`, one element per registered
algorithm, each already carrying its full parameter list (see below); there
is no separate summary shape for the listing.

### One algorithm's schema

```
focusforge-cli algorithms <ID> [--json] [--set KEY=VALUE]...
```

Prints `<ID>`'s id, display name and family, then one block per declared
parameter: its kind (a float or int range and step, a bool, or an enum's
variant ids and labels), its default, the value that resolved for this
invocation, where that value came from, and whether the parameter has an
`auto` hook. Resolution here is explicit-over-default only — an `auto` hook
needs a loaded stack to run against, and this subcommand loads no image, so
a parameter with no `--set` reports its plain default and a hook is shown as
present but not evaluated, never run.

`--set key=value` sets one parameter, and is repeatable; a key set more than
once keeps the last value. Every `--set` pair is validated against `<ID>`'s
schema before anything is printed: an unknown key, a numeric value outside
its declared range, a value that does not name one of an enum's declared
variants, or text that does not parse as the parameter's kind, is reported
by name — every violation across every `--set` pair, not only the first —
and the command exits without printing a schema.

`--json` emits the same per-parameter facts as the frozen schema described
above, one element in `{"algorithms": [...]}`. This is the frozen
`algorithms --json` schema: a field is added, never renamed or removed, in
later versions, matching `info --json`'s own compatibility promise.
