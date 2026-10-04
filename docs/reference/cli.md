# CLI reference

`focusforge-cli` is the headless interface to FocusForge. Every algorithm,
and every batch-run control (grouping, presets, resume, review, the stage
cache), is reachable here.

```
focusforge-cli <COMMAND>
```

| command | does |
|---|---|
| [`info`](#info) | prints TIFF or RAW metadata for a file or a directory of frames |
| [`algorithms`](#algorithms) | lists registered algorithms, or prints one's full parameter schema |
| [`stack`](#stack) | fuses a directory's frames into one project per detected stack |
| [`export`](#export) | renders a checkpoint to a TIFF, PNG or JPEG image, and/or writes its Z-Map or one source frame |
| [`retouch`](#retouch) | applies an op list to a Z-Map and writes the edited map |
| [`history`](#history) | lists, materializes, extends and deletes a project's checkpoints |
| [`align`](#align) | computes and reports each frame's transform, without fusing |
| [`review`](#review) | reports one record per project found under a directory or a run's output |
| [`cache`](#cache) | manages the stage cache under `.scratch/stage-cache/` |
| [`diagnostics`](#diagnostics) | packages logs and crash evidence into one redacted ZIP |

## Global options

`-v` and `-q` apply to every command and control how much a run says on
stderr:

```
focusforge-cli [-v]... [-q] <COMMAND>
```

- Repeat `-v` for more detail: once for informational messages, twice for
  debug detail. Omit it for warnings and errors only.
- `-q` restricts stderr to errors only. `-q` wins when both are given.

Every run also writes a JSON Lines log (one JSON object per line) to a log
file in the platform's per-user data directory, independently of `-v`/`-q`.
The file rotates once it reaches 8 MiB, keeping up to 5 rotated files.

The log records every warning and refusal a run prints on stderr, whatever
`-v`/`-q` say, and never repeats a line stderr already carried. Each entry
also names the context it belongs to: the command, the stack, the stage
(`setup`, `align`, `fuse`, `warp_write`, `finalize`) and, for work that runs
per tile, the tile.

If an earlier run left crash evidence that hasn't been packaged yet, the
next run prints a one-line notice before doing anything else — see
[`diagnostics`](#diagnostics) and
[`guide/diagnostics.md`](../guide/diagnostics.md).

## `info`

```
focusforge-cli info [--json] <PATH>
```

Prints TIFF or RAW metadata for a single file, or for every matching file directly inside a
directory (or inside a `frames/` subdirectory of it, when that exists).
TIFF entries use `.tif`/`.tiff`; RAW entries use the extensions supported by
the bundled decoder. Extensions are matched case-insensitively and entries are
not recursed into further. `info` reads headers only — it never opens a tile
or develops a pixel, which is why it can report on the 16 GB, 62-file reference stack
in well under a second.

`PATH` may be a single file or a directory:

- A single file that fails to parse is a hard error: `info` prints
  `error: ...` to stderr and exits 1.
- Inside a directory, a file that fails to parse is *not* fatal to the whole
  command: it is reported as one entry with an `error` field and counted in
  `unreadable`, while every readable sibling still gets a full report. A
  directory with zero matching files is a hard error.
- A file that parses but falls outside the fast read path (see
  [`tiff-fast-path.md`](tiff-fast-path.md)) is reported normally, with
  `mmap fast path: no` and the reason named — this is not an error. Supported
  16-bit LZW-, Deflate-, or tiled frames remain available for stacking;
  unsupported values name a refusal.

### Human output

One block per file:

```
path/to/frame.tif
  64x64 3ch 16-bit II compression=1 planar=1 photometric=2
  rows_per_strip=1 strips=64 file_size=25338
  description: FocusForge fixture mantis-64/00 from 20260829-IMG_9800.tif rect 4064,2700,64,64
  mmap fast path: yes
  reader: TiffStack
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
      "reader": "TiffStack",
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
| `frame_count` | number of matching TIFF or RAW files found (files attempted, whether or not they parsed) |
| `total_bytes` | sum of on-disk file sizes across all matching files |
| `unreadable_count` | how many matching files failed to parse |
| `uniform_layout` | `true` when every parsed frame has the same format-native layout facts; RAW entries compare decoder output geometry |
| `uniform_detail` | `null` when uniform, otherwise a sentence naming the outlier frame and what differs |
| `files` | one entry per file, in the order found |

Per-file TIFF fields are `null` for a RAW entry. A nested `raw` object is
present only for a readable RAW entry; it contains make, model, raw and output
geometry, crop origin, nullable storage and decoder bit depths, black and white
level arrays, CFA pattern and dimensions, color matrix, nullable capture time
as Unix epoch seconds, and decoder name. Storage depth is file-header depth;
decoder depth is decoder-native and may differ. Both level arrays and the
matrix remain in decoder-native domains. CFA values use `0=R`, `1=G`, `2=B`,
in crop-relative row-major order. The matrix is row-major XYZ-to-camera. A
metadata result proves header access only; it does not claim pixel development
will succeed.

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
| `reader` | `"TiffStack"` for mmap-compatible frames, `"DecodedTiffStack"` for supported decoded TIFF frames, `"RawStack"` for RAW frames, otherwise `null` |
| `refusal` | `null` when `mmap_fast_path` is `true`, otherwise the reason it is not (compression, tiling, planar layout, bit depth or sample format) |
| `error` | `null` on a successful parse, otherwise why the file could not be read |
| `raw` | RAW metadata for a readable RAW file; absent for TIFF and unreadable files |

The `raw` object contains:

| field | meaning |
|---|---|
| `make`, `model` | camera maker and model reported by the decoder |
| `raw_width`, `raw_height` | sensor image dimensions before the default crop |
| `output_width`, `output_height` | developed-frame dimensions after the default crop |
| `crop_x`, `crop_y` | default crop origin in sensor coordinates |
| `storage_bits`, `decoder_bits` | file-header and decoder-native bit depths; either can be `null` |
| `black_levels`, `white_levels` | decoder-native channel levels, or `null` when absent |
| `cfa`, `cfa_width`, `cfa_height` | crop-relative CFA pattern and dimensions, or `null` when absent |
| `color_matrix` | row-major XYZ-to-camera matrix, or `null` when absent |
| `capture_time_epoch_seconds` | EXIF `DateTimeOriginal` as Unix epoch seconds, or `null` when absent or invalid |
| `decoder` | `"rawler"` |

## `stack`

```
focusforge-cli stack [OPTIONS] <DIR>
```

Fuses every TIFF or supported RAW frame in `<DIR>` (non-recursive, descending into
`<DIR>/frames` when that subdirectory exists) into one **project** per
detected stack, through a registered stacking algorithm. A run prepares
projects for review; it does not write an image file — run
[`export`](#export) for that. TIFF frames are read tile by tile through an
out-of-core scheduler bounded by a memory budget. RAW decoding develops
and caches whole frames within the host memory budget.

`stack` accepts mixed uncompressed, LZW-compressed, Deflate-compressed, and
tiled 16-bit unsigned chunky TIFF frames. RAW files use `rawler`; a stack
cannot mix TIFF and RAW frames. Unsupported layouts or unreadable RAW files
stop the run with the frame and reason named.

**Alignment runs first, by default.** Before fusion, `stack` computes each
frame's transform against a reference frame (the same solver `align` drives
directly — see below), warps every frame tile through it, and crops the
output to the region every frame actually covers. Both are on unless turned
off.

**Grouping runs before that.** `<DIR>` may hold more than one stack; `stack`
proposes a split by capture time or directory structure and prints it before
doing anything else — see [`guide/batch-runs.md`](../guide/batch-runs.md)
for the full workflow (`--groups`, `--write-groups`, `--only`).

| flag | meaning |
|---|---|
| `-o, --output <DIR>` | directory to collect this run's projects into; omit to write each project beside its own frames |
| `--algo <ALGO>` | registered stacking algorithm id (required; see `algorithms` below for the list) |
| `--set <KEY=VALUE>` | set one algorithm parameter, repeatable; a repeated key keeps the last value |
| `--json` | emit the frozen JSON report instead of human-readable text |
| `--groups <PATH>` | group the frames as this TOML file says instead of proposing a grouping |
| `--write-groups <PATH>` | write the proposed grouping to this path in the format `--groups` reads, then carry on with the run |
| `--only <ID>` | run only the named result(s), repeatable; every unselected group is skipped, its project left untouched |
| `--no-align` | skip alignment: frame tiles are fused unwarped and the output stays full-frame. Alignment is on by default, matching the project's `align` value |
| `--no-crop` | skip the auto-crop: the output stays at the full frame extent even with alignment on. The crop is on by default, matching the project's `auto-crop` value; with the crop on and no interior common to the aligned frames, `stack` stops and names this flag |
| `--align-algo <ALGO>` | alignment algorithm id; optional, defaults to the sole registered alignment algorithm |
| `--align-set <KEY=VALUE>` | set one alignment parameter, repeatable; separate from `--set`, which is the stacking algorithm's own |
| `--resume` | resume a previously cancelled run of this same stack, found by fingerprint under `.scratch/resume/` |
| `--quiet` | print no progress at all |
| `--tile-size <PX>` | override the scheduler's outer tile size, in pixels |
| `--max-host-memory <SIZE>` | a **ceiling only** on the adaptive host-memory budget: it can lower the budget the host-derived estimate resolved to, never raise it. Omit it entirely to get the machine's own concurrency. Accepts suffixed sizes: `B`; `KiB`/`MiB`/`GiB`/`TiB` (1024-based); `KB`/`MB`/`GB`/`TB` (1000-based); no suffix means bytes (e.g. `8GiB`, `512MiB`, `4096`) |
| `--max-vram <SIZE>` | a **ceiling only** on the adaptive device (VRAM) budget: it can lower the budget the startup probe settled on, never raise it. Omit it entirely to get the probed value. Same accepted sizes as `--max-host-memory`; absent when the binary was built `--no-default-features` |
| `--preset <PATH>` | read parameter values from a TOML preset — see [`guide/batch-runs.md`](../guide/batch-runs.md#presets) |
| `--write-preset <PATH>` | write the preset that reproduces this run |
| `--status <PATH>` | append one NDJSON status record for this stack to this path |
| `--dry-run` | resolve everything and print what would happen, without computing or writing anything |
| `--no-gpu` | force the CPU path even when a usable GPU adapter is present; absent when the binary was built `--no-default-features` |
| `--no-cache` | skip the stage cache: recompute the alignment even when a valid entry exists, and store no new entry |

Every project carries a **Z-Map**: a single-channel 16-bit TIFF, stored
inside the project alongside the composite, with one value per output pixel
naming the index of the source frame that pixel came from. With alignment
and the crop both on (the default), the composite and the Z-Map are declared
at the crop rect's dimensions, not the source frame's; `--no-crop` keeps
them at the full frame. Run [`export`](#export) to write either one out as a
TIFF file. See [`reference/project-format.md`](project-format.md) for the
container itself.

Refusals happen before the input directory is read where possible: an
unknown `--algo` id or a `--set` value that fails the algorithm's own
parameter schema (unknown key, out-of-range value, an enum value that does
not name a declared variant) is reported and the command exits 1 without
opening a single frame. A frame that fails to register is a refusal too,
naming every failed frame, exit 1. An empty auto-crop (the frames' footprints
do not intersect) is refused before any output file is created, naming
`--no-crop` as the way through.

**Ctrl-C cancels, SIGUSR1 pauses.** A first SIGINT stops the run cleanly at
the next segment boundary, prints the exact `--resume` recipe, and exits
130; work already committed to a project stays on disk. A second SIGINT
kills the process immediately. On Unix, SIGUSR1 toggles a pause in place —
the run prints `paused -- send SIGUSR1 to resume (Ctrl-C to cancel)` and
blocks until a second SIGUSR1 (resume) or a SIGINT (cancel); no `--resume`
flag is needed to come back from a pause, since the process itself is still
running. SIGUSR1 does not exist as a signal on Windows, so pausing is
Unix-only there.

**GPU use is automatic, never an opt-in flag.** The GPU path is compiled in
by default, and `stack` uses a usable GPU adapter automatically for the
`laplacian` algorithm — the only one with a GPU implementation; `weighted-average` and
`depth-map` always run on the CPU. `--no-gpu` forces the CPU path
regardless of algorithm. A missing or unusable adapter falls back to the
CPU path on its own, printing why on stderr, never silently and never as a
hard failure. Either path produces the same result — every GPU kernel is
proven to match its CPU twin to within 1 LSB at 16-bit — so switching
backends changes speed, not the output; `stack --json`'s `backend` field
names which one actually ran.

```
$ focusforge-cli stack fixtures/mantis-64 -o .scratch/out/ --algo laplacian --no-align --no-gpu --quiet
--no-gpu given: forcing the CPU path
grouping fixtures/mantis-64: 1 group (directory structure (no frame carries a readable capture time))
  mantis-64  3 frames  00.tif .. 02.tif  [directory]
fixtures/mantis-64 (3 frames)
64x64 3ch
laplacian - Laplacian Pyramid
levels = 4 (Auto)
tile_size=512 levels=4 margin=48 tiles=1 in_flight_limit=32
max_bytes_in_flight=20095898419 budget=Host backed_off=false
backend: cpu
wrote: .scratch/out/mantis-64.ffp
wall: 0.125s
stages: setup=0.001s align=not-entered fuse=0.109s finalize=0.015s workers=? peak=?
```

`.scratch/out/` must already exist — `-o` names an existing directory to
collect projects into, it never creates one. (`--no-align` keeps this
example to the fusion stage the `backend` field is about; `mantis-64`'s own
frames diverge under the default `pyramidal-ecc` alignment in strict mode,
so a real run without either `--no-align` or a softened failure policy
would refuse instead — see
[`guide/alignment.md`](../guide/alignment.md#failure-is-strict-by-default) for
what that policy is and why it defaults to strict. `backend` reads `"cpu"`
because this example passes `--no-gpu`; omit it on a default build with a
usable adapter present and it instead names the adapter, for example `NVIDIA
GeForce RTX 5070 Ti (backend=Vulkan, driver=NVIDIA, driver_info=595.84)`.)
`--quiet` suppresses only the ticking progress line — per-frame while
alignment runs, per-tile once fusion starts; the run summary above still
prints. `stages`' `workers=?`/`peak=?` here mean `--no-align` never
entered the alignment stage at all — a real run reports numbers instead
(see below).

### `--json`

`--json` emits a report with the input frames, the resolved algorithm and
its parameters (value and provenance — whether each came from its default,
an explicit `--set`, an `auto` hook, or a host-derived default, with
`HostFallback` when host facts were unavailable and the descriptor's own
default was used instead), the tile geometry the scheduler planned, and the
paths written. Like `info --json` and `algorithms --json`, this is a frozen
shape: a field is added, never renamed or removed, in later versions.

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

Additive fields from the host-memory budget: `max_bytes_in_flight` (the
in-flight byte budget actually in force), `budget_provenance` (`"Host"` —
derived from the host's currently reported available memory;
`"HostFallback"` — the host probe was unavailable and the stated default was
used instead; `"Explicit"` — `--max-host-memory` lowered the budget), and
`backed_off` (whether the **host** scheduler's runtime back-off shrunk the
in-flight limit during this run because of observed *host memory* pressure —
reported on both paths, since the host scheduler runs on the GPU path too;
never about VRAM, since the device budget has no runtime back-off of its
own). `peak_in_flight` (next to `in_flight_limit`, both above) is this
group's *configured* host in-flight-tile limit, not a measured high-water
mark — the engine this command drives does not expose the real observed
concurrency, so the configured bound is reported instead.

Additive fields from the device (VRAM) budget, GPU path only:
`max_device_bytes_in_flight` (the in-flight device-byte budget actually in
force), `device_budget_provenance` (`"Host"` — a startup probe settled on
this value; `"HostFallback"` — no probe ran and the stated default was used
instead; `"Explicit"` — `--max-vram` lowered it), and `device_peak_in_flight`
(the device budget's own high-water count of tiles admitted at once during
**this group** — unlike `peak_in_flight`, this one is a real measurement, not
a configured limit). `device_peak_in_flight` is scoped per group: in a
multi-group run, each group in the wrapped `groups` document reports its own
measurement, never a running maximum over the groups that preceded it in the
same process. It is `0` for a group that admitted no tile through the device
gate (fully served from the stage cache, say), and `null` only when there was
no GPU backend at all. All three fields are `null` on the CPU path.

Additive field `stages`: a per-stage wall-clock breakdown of `wall_seconds`
— `setup`, `align`, `fuse` and `finalize`, each seconds as a float or `null`
when that stage was never entered (`--no-align` leaves `align` null;
`warp_write` is always `null` here, `stack` has no such stage). The four
non-null numbers sum to `wall_seconds` exactly: the spans are contiguous and
disjoint, produced by one monotonic timer. `fuse` is the wall clock of a
parallel fan-out (the tile scheduler's own rayon pool), not CPU time — read
it beside `in_flight_limit`/`peak_in_flight`, not as a claim about how much
work happened. Additive field `align_source`: how the `align` stage's number
was produced — `"computed"` (a real solve), `"cached"` (a stage-cache hit),
`"resumed"` (a `--resume` ledger peek) or `"skipped"` (`--no-align`) — because
a cache hit or a resume enters the stage and reports a small real number that
is not "alignment took 10ms". Additive fields `align_workers`/`peak_align_workers`:
the anchor arm's worker bound and the peak it actually reached -- numbers only
when `align_source == "computed"`, `null` otherwise. A `"cached"`, `"resumed"`
or `"skipped"` run never runs the alignment driver itself, so it has no
worker count of its own; reporting one from a previous run's cache entry
would be a fabricated number, exactly the confusion `align_source` exists to
prevent.

```json
{
  "input": "fixtures/mantis-512",
  "frame_count": 8,
  "frames": ["00.tif", "01.tif", "..."],
  "width": 512,
  "height": 512,
  "samples_per_pixel": 3,
  "algorithm": "laplacian",
  "display_name": "Laplacian Pyramid",
  "params": [{ "id": "levels", "value": 5, "provenance": "Auto" }],
  "tile_size": 512,
  "levels": 5,
  "margin": 96,
  "tiles": 4,
  "frame_tiles_read": 32,
  "in_flight_limit": 32,
  "peak_in_flight": 32,
  "max_bytes_in_flight": 20381420748,
  "budget_provenance": "Host",
  "backed_off": false,
  "outputs": [".scratch/out/mantis-512.ffp"],
  "wall_seconds": 13.010864322,
  "output_width": 422,
  "output_height": 424,
  "crop": { "x": 31, "y": 40, "width": 422, "height": 424 },
  "description": "FocusForge 0.1.0 align=pyramidal-ecc anchor=3 mode=rotation-scale crop=31,40,422,424 cropped=true",
  "align": {
    "algorithm": "pyramidal-ecc",
    "display_name": "Pyramidal ECC",
    "params": [{ "id": "mode", "value": "rotation-scale", "provenance": "Default" }],
    "reference": 3,
    "reference_policy": "anchor",
    "failure_policy": "lax",
    "failure_count": 3,
    "alignments": [
      {
        "frame": 2,
        "name": "02.tif",
        "transform": { "m": [[1.0239, 0.0021, 45.03], [-0.0021, 1.0239, -37.77], [0.0, 0.0, 1.0]] },
        "status": "converged",
        "iterations": 81,
        "residual": 0.9690,
        "fallback": false,
        "scale": 1.0239,
        "rotation_deg": -0.119,
        "tx": 45.03,
        "ty": -37.77
      },
      "..."
    ]
  },
  "backend": "cpu",
  "max_device_bytes_in_flight": null,
  "device_budget_provenance": null,
  "device_peak_in_flight": null,
  "align_cached": false,
  "stages": { "setup": 0.001, "align": 9.918, "fuse": 2.712, "warp_write": null, "finalize": 0.379 },
  "align_source": "computed",
  "align_workers": 7,
  "peak_align_workers": 7
}
```

This run softened alignment failures to an identity fallback, which is why
`align.failure_count` is nonzero and one alignment's `status` is
`"diverged"` yet the run still completed — see
[`guide/alignment.md`](../guide/alignment.md#failure-is-strict-by-default).

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
| `--quiet` | print no progress at all (`align` has one stage line to suppress; it has no per-tile units to count) |
| `--preset <PATH>` | read `align-algo` and the `[align]` table from a TOML preset — see [`guide/batch-runs.md`](../guide/batch-runs.md#presets). A non-empty `[stack]` table or `algo` key in the same file is unused here and says so on stderr |
| `--write-preset <PATH>` | write the preset that reproduces this run: `align-algo` plus the `[align]` values chosen |
| `--status <PATH>` | append one NDJSON status record for this run to this path |
| `--max-host-memory <SIZE>` | ceiling on the host-memory budget the alignment workers are sized from; it only lowers the derived budget |
| `--max-vram <SIZE>` | ceiling on the device-memory budget for the alignment's GPU levels and the `--write-warped` fuse backend; it only lowers the probed budget. Absent when built `--no-default-features` |
| `--no-gpu` | force the CPU path: the alignment's large levels and the `--write-warped` fuse backend stay on the CPU. Absent when built `--no-default-features` |

A frame whose registration diverges refuses the run, exit 1, naming every
failed frame.

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
this reference.

Additive field `stages`: the same per-stage wall-clock breakdown `stack
--json` carries, but with `align`'s own stage set — `setup`, `align`,
`warp_write` and `finalize`, each seconds or `null` when never entered.
`fuse` is always `null` here (`align` never fuses). `warp_write` is `null`
unless `--write-warped` was given — a run without the flag never enters
that stage, which is not the same as it taking zero time. The non-null
stages sum to `wall_seconds` exactly, the same contiguous-and-disjoint
contract `stack --json` documents. Additive field `align_source`: always
`"computed"` for `align` (no `--resume`, no stage cache, no `--no-align`
here), carried so a consumer reading both reports' `align` stage does not
need to know which command produced it. Additive fields
`align_workers`/`peak_align_workers`: the anchor arm's worker bound and the
peak it actually reached (chain mode always reports `1`/`1` -- it is serial
by construction) -- always numbers here, since every `align` run computes.

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
  "wall_seconds": 8.497,
  "stages": { "setup": 0.021, "align": 8.401, "fuse": null, "warp_write": null, "finalize": 0.075 },
  "align_source": "computed",
  "align_workers": 2,
  "peak_align_workers": 2,
  "align_backend": "cpu"
}
```

`transform.m` is the raw 3x3 matrix (reference-to-frame); `scale`,
`rotation_deg`, `tx`, `ty` are its decomposition, exact for both shipped
modes. `status` is `"converged"`, `"hit-cap"`, `"stalled"` or `"diverged"`; only a
diverged frame carries `reason`: `singular-hessian`, `lemma-one-region`,
`non-invertible-increment`, `correlation-decreased`, `too-few-valid-samples`,
`non-finite-update` or `validation-failed` (the fit converged but its
transform leaves a large residual at patches away from the fit region).
`stalled` means the iteration cap was reached with flat or oscillating steps
and the best-correlation transform is kept; only `diverged` counts as a
failure, so a `stalled` frame neither fails a strict run nor adds to
`failure_count`. In the text report a diverged row
reads `diverged(<Reason>)`. `align_backend` is `"cpu"`, or the GPU adapter's
display name when at least one alignment level ran on it; a device lost
mid-run reruns the whole alignment on the CPU and reports `"cpu"`. The same
field is on `stack --json`, `null` unless the alignment was computed that run. `crop` is `null` when the frames' footprints
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

Three stacking algorithms and one alignment algorithm are registered in a
normal build:

```
$ focusforge-cli algorithms
depth-map - Depth Map (stacking)
laplacian - Laplacian Pyramid (stacking)
pyramidal-ecc - Pyramidal ECC (alignment)
weighted-average - Weighted Average (stacking)
```

- `laplacian` — Laplacian-pyramid coefficient selection
- `weighted-average` — per-pixel sharpness-weighted blend
- `depth-map` — per-pixel selection from a smoothed sharpness surface
- `pyramidal-ecc` — the alignment solver `align` and `stack` both drive; see
  [`guide/alignment.md`](../guide/alignment.md)

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
`auto` hook. Resolution here is explicit → `auto` (stack) → host-derived
default → plain default — but an `auto` hook needs a loaded stack to run
against, and this subcommand loads no image, so a *stack* `auto` hook is
always shown as present but not evaluated, never run. A parameter with a
host-derived default (`host_default`) is different: it needs no loaded
image, so it *is* evaluated here, reporting `Host` — or `HostFallback` with
the descriptor's own plain default, when host facts are unavailable.

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

## `export`

```
focusforge-cli export [OPTIONS] <PROJECT>
```

Turns a checkpoint of a project into image files: the image with `-o`, the
Z-Map with `--zmap`, one source frame warped and cropped to the composite's
own extent with `--frame`/`--frame-output` — any combination, at least one
required. The image is always rendered from the checkpoint's Z-Map, gathering
each pixel from the source frame its index names, so an edited checkpoint
exports as edited. Rendering re-reads the source frames and checks each
against the hash recorded in the project. See
[`reference/project-format.md`](project-format.md) for what `<PROJECT>` (a
`.ffp` file) holds.

| flag | meaning |
|---|---|
| `-o, --output <FILE>` | write the image to this path |
| `--zmap <FILE>` | write the Z-Map to this path (as TIFF); needs no source frames |
| `--checkpoint <ID>` | the checkpoint to export, an id or a unique id prefix; defaults to the head |
| `--format <FORMAT>` | `tiff`, `png` or `jpeg`. Inferred from `-o`'s extension when omitted: `.tif`/`.tiff`, `.png`, `.jpg`/`.jpeg`; no extension means TIFF. An unrecognised extension needs `--format`; a `--format` that contradicts a recognised extension is refused |
| `--tiff-bits <BITS>` | TIFF bit depth: `16` (default) or `8` |
| `--tiff-compression <NAME>` | TIFF compression: `none` (default), `lzw` or `deflate` |
| `--png-bits <BITS>` | PNG bit depth: `16` (default) or `8` |
| `--png-compression <NAME>` | PNG compression effort: `fast`, `default` (default) or `best` |
| `--jpeg-quality <N>` | JPEG quality, 1-100 (default `95`) |
| `--jpeg-subsampling <SPEC>` | JPEG chroma subsampling: `420` (default) or `444` |
| `--exif <on\|off>` | carry the alignment reference frame's EXIF (default `on`) |
| `--icc <on\|off>` | embed an ICC profile (default `on`) |
| `--xmp <on\|off>` | carry the source frame's XMP packet verbatim (default `off`) |
| `--iptc <on\|off>` | carry the source frame's IPTC block verbatim (default `off`; never written to PNG) |
| `--frame <INDEX>` | write this 0-based source frame "as fused" — warped by its recorded alignment transform and offset by the applied crop, at the checkpoint's composite width and height. Requires `--frame-output` |
| `--frame-output <FILE>` | where to write the frame `--frame` names |
| `--json` | emit the frozen JSON report instead of human-readable text |

A format option that belongs to another format (`--png-bits` with a JPEG
output) is refused by name, and a value outside its range is refused with the
range. PNG and JPEG hold grey or RGB images only; JPEG is limited to 65,535
pixels per side.

```
focusforge-cli export mantis-64.ffp -o out.jpg --jpeg-quality 90
focusforge-cli export mantis-64.ffp -o out.tif --tiff-compression lzw --zmap zmap.tif
focusforge-cli export mantis-64.ffp --checkpoint 5b71 --zmap zmap-5b71.tif
```

A source frame that cannot be found, or whose content changed since the
project was written, makes `-o` and `--frame-output` refuse, naming every
such frame; nothing is written. `--zmap` alone never reads a source frame.
The `.partial` file a run writes into is renamed only when the write
completes.

### `--json`

Frozen, additive-only, the same discipline as every other `--json` shape in
this reference: `project`, `wrote` (final paths only), `unresolved_sources`
(present and empty when every source resolved, not an omission), `width`,
`height`, `samples_per_pixel`, `frame_count`, `algorithm`, `checkpoint` (the
resolved checkpoint id), `format` (`null` without `-o`), `format_options`
(every option of that format with its resolved value, defaults included;
`null` without `-o`), `frame` (the `--frame` index written, `null` when the
flag was omitted), `metadata` (what EXIF, ICC, XMP and IPTC carry-over did
for the `-o` image: `exif`, `icc`, `xmp`, `iptc` each a status string, plus
`frame`, `frame_path` and `warnings`; `null` without `-o`) and
`wall_seconds`.

## `history`

```
focusforge-cli history [--accept-changed-sources] <COMMAND>
```

Reads and changes a project's checkpoint tree without a window. Every
checkpoint argument is an id or a unique prefix of one; an ambiguous prefix
names every match. See [`reference/project-format.md`](project-format.md)
for what a checkpoint is and how hidden checkpoints are handled.

A project is validated when it is opened. A damaged one is refused by name
with its own exit code (3 not a project, 4 truncated, 6 a missing entry, 7 a
blob hash mismatch, 8 a broken link between checkpoints, `1` anything
else). If a source frame's content changed since the project was written,
every subcommand refuses with exit 9 naming the frames;
`--accept-changed-sources` proceeds with a warning instead. Frames that
cannot be found only produce a warning: the history and Z-Maps are intact.

| subcommand | does |
|---|---|
| `list <PROJECT> [--json]` | prints every visible checkpoint, newest first: id, parent, algorithm and creation time. `--json` emits `{"checkpoints": [{"id", "parent", "algorithm", "created_at"}]}`, `created_at` in Unix seconds |
| `materialize <PROJECT> --checkpoint <ID> -o <FILE> [--json]` | writes that checkpoint's Z-Map as a TIFF; never overwrites the project |
| `apply <PROJECT> --ops <FILE> [--from <ID>] [--save] [-o <FILE>] [--json]` | applies an op list read from `--ops` (`{"ops": [...]}`, the shape described in [`guide/retouch.md`](../guide/retouch.md)) onto `--from` (default: the head). `--save` seals the result as a new child checkpoint, which branches the tree when `--from` is not the head; `-o` writes the edited Z-Map. At least one of the two is required; with both, the map written is the sealed checkpoint's |
| `delete <PROJECT> --checkpoint <ID> [--json]` | hides a checkpoint and removes it from the file once no visible checkpoint is built on it. Refuses the last visible checkpoint |

```
focusforge-cli history list mantis-64.ffp
focusforge-cli history apply mantis-64.ffp --ops ops.json --save
focusforge-cli history delete mantis-64.ffp --checkpoint 5b71
```

`apply --json` reports `project`, `from`, `checkpoint` (the sealed id, `null`
without `--save`), `wrote`, `ops` and `wall_seconds`; `materialize --json`
reports `project`, `checkpoint`, `wrote` and `wall_seconds`; `delete --json`
reports `project`, `hidden`, `removed` (every checkpoint taken out of the
file) and `head`. The list's JSON carries no version field, like every
project and op file.

## `retouch`

```
focusforge-cli retouch [OPTIONS] --zmap <FILE> --ops <FILE> --out <FILE>
```

Applies an op list to a Z-Map and writes the edited map. Every editing
capability the window offers is reachable through this one command,
headless — see [`guide/retouch.md`](../guide/retouch.md) for the op list's
JSON shape and a full walkthrough.

| flag | meaning |
|---|---|
| `--zmap <FILE>` | the Z-Map to edit |
| `--ops <FILE>` | the op list to apply, in the persisted `Op` JSON shape; positions in error messages count from 0 in list order |
| `--out <FILE>` | where to write the edited Z-Map; required, and never the same file as `--zmap` |
| `--undo <N>` | undo this many ops off the end of the list before writing (default `0`) |
| `--redo <N>` | redo this many undone ops, never more than `--undo` (default `0`) |
| `--frames <DIR>` | directory of source frames to recomposite `--render`/`--overlay` from; required by either |
| `--render <FILE>` | write the recomposited image at the edited map's undo/redo position — a per-pixel gather from `--frames`, CPU only |
| `--overlay <FILE>` | blend the rainbow overlay over the recomposited image and write it as an 8-bit RGBA PNG |
| `--opacity <FLOAT>` | the overlay's blend opacity, `0.0`–`1.0` (default `0.65`) |
| `--solo <INDEX>` | highlight this 0-based source index in the overlay — the numbering an op's `source_id` uses — and desaturate every other source |
| `--json` | emit the frozen JSON report instead of human-readable text |

```
$ focusforge-cli retouch --zmap zmap.tif --ops stroke-list.json --out edited.tif
zmap: zmap.tif
ops: stroke-list.json (3 in list, 3 applied)
pixels changed: 9608
wrote: edited.tif
wall: 0.035s
```

An op that fails to validate — an unsupported tag, or a value outside what
the op accepts — refuses the whole run before anything is written, naming
the op's 0-based position in the list. `--out` and any of `--render`/
`--overlay` are refused if they would overwrite `--zmap`, each other, or a
source frame.

### `--json`

Frozen, additive-only: `zmap`, `ops`, `ops_in_list`, `ops_applied`,
`pixels_changed`, `wrote`, `wall_seconds`, `rendered` (the `--render` path
written, `null` when omitted) and `overlay` (the `--overlay` path written,
`null` when omitted).

## `review`

```
focusforge-cli review [--json] <DIR>
```

Reports one record per stack found under `<DIR>` — a `stack` run's `-o`
output directory, or a plain directory of `.ffp` files — without
re-fusing, re-grouping or writing anything. When `<DIR>` holds a run
manifest (`manifest.json`, written by every `stack` run), `review` reads it
and lists every group the run attempted, a failed one included; otherwise it
lists the `.ffp` files it finds directly, non-recursively.

```
$ focusforge-cli review .scratch/out
review .scratch/out/manifest.json: 1 result
  fixtures/mantis-64       completed sharpness=0.9823   sources=3   coherence=0.7464   wall=0.120s
```

`sharpness` is the ratio of two numbers the run already recorded in the
project's manifest; `coherence` is computed by streaming the project's
Z-Map. Both are `--` when a project has no recorded metrics — a failed
stack, for instance — and a note (the metrics reason, or the run's error)
prints after the fields instead. See
[`guide/batch-runs.md`](../guide/batch-runs.md#reviewing-the-results) for a
walkthrough.

### `--json`

Emits `{"format": 1, "results": [...]}`. Each result carries the same
identity/outcome/error fields the `--status` NDJSON stream uses, plus
`metrics` (`{"sharpness_ratio": ..., "distinct_sources": ..., "coherence":
...}` or `null`), `metrics_reason` (why `metrics` is `null`, or `null`
itself), and `unresolved_sources` (present and empty when every source
resolved).

## `cache`

```
focusforge-cli cache [--json]
```

Removes every entry from the stage cache under `.scratch/stage-cache/`, the
store `stack` reads and writes to skip a repeated alignment solve — see
[`guide/batch-runs.md`](../guide/batch-runs.md#the-stage-cache). Never touches
`.scratch/resume/` or any project — a clear must never be able to destroy a
cancelled run's recoverable work.

```
$ focusforge-cli cache
removed 1 stage cache entry
```

`--json` emits `{"format": 1, "removed": <count>}`.

## `diagnostics`

```
focusforge-cli diagnostics [-o FILE] [--include-minidumps] [--force]
```

Packages this installation's log files, any crash reports, a system
summary, the graphics adapter record, the configuration a new run would
resolve (with `Provenance` for each value), and the version and commit into
one ZIP, printing the file list before writing it. Every text entry has the
home directory and the user name redacted out; no image data is ever
included, and nothing is sent anywhere. See
[`guide/diagnostics.md`](../guide/diagnostics.md) for the log format, what a
crash leaves behind, and the notice printed on the next start after one.

- `-o FILE` — where to write the ZIP. Omit for a dated name in the current
  directory.
- `--include-minidumps` — include the minidump binaries alongside their
  JSON metadata. They cannot be redacted, and each one is marked so in the
  printed file list.
- `--force` — overwrite `FILE` if it already exists.

Exits non-zero if the output path exists without `--force`, or if the
bundle could not be written.
