# Stacking a folder of frames

`focusforge-cli stack` fuses a directory of TIFF or supported RAW frames —
the same shape `focusforge-cli info` inspects — into a **project**, through a chosen
algorithm. A run prepares a project for review; it does not write an image
file. `focusforge-cli export` does that, as a separate step.

```bash
focusforge-cli stack path/to/frames/ --algorithm laplacian
```

`path/to/frames/` is read non-recursively, or from a `frames/` subdirectory
of it when one exists. `--algorithm` picks the algorithm; see
[`reference/cli.md`](../reference/cli.md#algorithms) for the full list and
[`reference/cli.md`](../reference/cli.md#stack) for every flag. The project
is written beside the frames as `<dir>/<dir-name>.ffp`, or collected into
a directory named with `-o`/`--output` — see
[`guide/batch-runs.md`](batch-runs.md) for a folder holding more than one
stack.

Before fusing, `stack` aligns the frames and crops the output to the region
every one of them covers — both on by default. See
[`guide/alignment.md`](alignment.md) for how that stage works and how to
turn it off.

## RAW frames

RAW input uses `rawler`. Put one compatible RAW format in a directory and run
the same command as for TIFF; `info` reports camera metadata before a stack
run. TIFF and RAW frames form separate stacks, and a group that mixes them is
refused before pixels are developed. A file the decoder cannot read reports an
error; FocusForge has no alternate RAW decoder.

## Getting an image back

```bash
focusforge-cli export path/to/frames/frames.ffp -o out.tif --zmap zmap.tif
```

`export` reads a project and writes the image (`-o`), the Z-Map
(`--zmap`), or both in one invocation — at least one of the two is required.
The image is rendered from the project's stored composite, so it always
reflects the checkpoint you ask for (the head by default; `--checkpoint <id>`
picks another). An unretouched checkpoint needs no source frames; a retouched
one reads them again and checks them against the hashes recorded when the
project was written, and a missing or changed frame is named with nothing
written. `export` refuses an output path that names the project itself.

The format follows `-o`'s extension: `.tif`, `.png` or `.jpg`. Each format
has its own options:

```bash
focusforge-cli export frames.ffp -o out.png --png-bits 8 --png-compression best
focusforge-cli export frames.ffp -o out.jpg --jpeg-quality 90 --jpeg-subsampling 444
focusforge-cli export frames.ffp -o out.tif --tiff-bits 16 --tiff-compression lzw
```

TIFF and PNG default to 16 bits and JPEG to quality 95 with 4:2:0 chroma
subsampling. EXIF from the alignment reference frame and an ICC profile are
carried into the image by default; `--xmp on` and `--iptc on` carry those
too (IPTC is never written to PNG), and `--exif off` / `--icc off` leave
them out. [`reference/cli.md`](../reference/cli.md#export) lists every flag.

## Choosing an algorithm

Three are available. The window shows the display name; the CLI takes the id.

- **Details** (`laplacian`) — decomposes each frame into a Laplacian pyramid
  and selects the sharpest coefficient at each level before recombining. The
  default in the window and a good general-purpose choice; its `levels`
  parameter can be set explicitly or left on `auto`, which derives a depth
  from the stack's resolution.
- **Soft transitions** (`weighted-average`) — blends every frame at each
  pixel, weighted by local sharpness, rather than selecting a single winner.
  Smoother transitions between frames, at the cost of some softening versus a
  hard selection. `radius` (0 to 16, default 16) sets how far a pixel looks
  around itself; `weight-exponent` (1 to 8, default 4) sets how strongly the
  sharpest frame is preferred.
- **Focus map** (`depth-map`) — builds a smoothed sharpness surface across the
  stack and selects, per pixel, the frame that surface names. Produces the
  most coherent Z-Map of the three, useful when the map itself will be
  retouched. `score-radius` (default 4) and `smoothing-radius` (default 8)
  take 0 to 16.

Every algorithm accepts `--set key=value` to override a declared parameter;
run `focusforge-cli algorithms <id>` to see what each one exposes, its
range, and its default.

## Brightness equalization

Frames of one stack often differ slightly in brightness. `stack` equalizes
them before fusing, on by default for every method: each frame gets one gain,
the same on every channel, bringing each frame's whole-image mean to one
target shared by all frames.

```bash
focusforge-cli stack path/to/frames/ --algorithm laplacian --set equalize=false
focusforge-cli stack path/to/frames/ --algorithm laplacian --set equalize-max-gain=1.5
focusforge-cli stack path/to/frames/ --algorithm laplacian --set equalize-target=0.2
```

`equalize-target` (-0.5 to 0.5, default 0.0) moves the shared target in stops
from the frames' average brightness: 0.0 keeps the average, +1 would double it.

`equalize-max-gain` (1.01 to 2.0, default 1.25) bounds the gain both ways: with
1.25 a frame's gain stays between 0.8 and 1.25. A frame that would need more
gets the bound, and the run prints a notice naming the frame, the gain it
needed, the smallest bound that would cover it and the mean it reached. A
black frame keeps gain 1, and a frame whose gain clips samples at white and
misses the target is reported as well. The gains are recorded in
the project, so `export` writes the equalized result. In the window these are
the **PREPARE** settings, where the target is a signed slider that steps by 0.1. See
[`reference/cli.md`](../reference/cli.md#stack) for the project settings.

## Getting the Z-Map

Every project already carries its Z-Map — a single-channel 16-bit TIFF
naming, per pixel, which source frame contributed. Pass `--zmap` to `export`
to write it out:

```bash
focusforge-cli stack path/to/frames/ --algorithm depth-map
focusforge-cli export path/to/frames/frames.ffp --zmap zmap.tif
```

## Using the GPU

The GPU-accelerated fusion path for the `laplacian` algorithm is compiled in
by default (`cargo build --release`) — `weighted-average` and `depth-map`
always run on the CPU. When the path is compiled in and a usable adapter is
present, `stack` uses it automatically; there is no opt-in flag. `--cpu`
forces the CPU path regardless, and `cargo build --release
--no-default-features` excludes the GPU path from the binary entirely. A
missing or unusable adapter falls back to the CPU path on its own, printing
why on stderr, never silently and never as a hard failure. If the graphics
device is lost partway through a run, the tiles still to do run on the host
instead, rather than failing the run; see
[`guide/diagnostics.md`](diagnostics.md) for where that and other GPU errors
are logged.
Every GPU kernel produces the same result as its CPU counterpart, to within
1 LSB at 16-bit, so switching backends changes speed, not the output — see
`--json`'s `backend` field in
[`reference/cli.md`](../reference/cli.md#stack) to confirm which one ran.

## Reading the result

```bash
focusforge-cli stack path/to/frames/ --algorithm laplacian --json
```

`--json` reports the frames that were read, the resolved algorithm and
parameters (with where each value came from), the tile geometry the run
planned, and the project path written — see
[`reference/cli.md`](../reference/cli.md#stack) for the full schema.
