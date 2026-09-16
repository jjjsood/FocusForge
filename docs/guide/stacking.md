# Stacking a folder of frames

`focusforge-cli stack` fuses a directory of TIFF frames — the same shape
`focusforge-cli info` inspects — into one composite through a chosen
algorithm.

```bash
focusforge-cli stack path/to/frames/ -o out.tif --algo laplacian
```

`path/to/frames/` is read non-recursively, or from a `frames/` subdirectory
of it when one exists. `--algo` picks the algorithm; see
[`reference/cli.md`](../reference/cli.md#algorithms) for the full list and
[`reference/cli.md`](../reference/cli.md#stack) for every flag.

Before fusing, `stack` aligns the frames and crops the output to the region
every one of them covers — both on by default. See
[`guide/alignment.md`](alignment.md) for how that stage works and how to
turn it off.

## Choosing an algorithm

Three are available:

- **`laplacian`** — decomposes each frame into a Laplacian pyramid and
  selects the sharpest coefficient at each level before recombining. Good
  general-purpose default; its `levels` parameter can be set explicitly or
  left on `auto`, which derives a depth from the stack's resolution.
- **`weighted-average`** — blends every frame at each pixel, weighted by
  local sharpness, rather than selecting a single winner. Smoother
  transitions between frames, at the cost of some softening versus a hard
  selection.
- **`depth-map`** — builds a smoothed sharpness surface across the stack and
  selects, per pixel, the frame that surface names. Produces the most
  coherent Z-Map of the three, useful when the map itself will be retouched.

Every algorithm accepts `--set key=value` to override a declared parameter;
run `focusforge-cli algorithms <id>` to see what each one exposes, its
range, and its default.

## Getting the Z-Map

Pass `--zmap` to also write a Z-Map — a single-channel 16-bit TIFF naming,
per pixel, which source frame contributed:

```bash
focusforge-cli stack path/to/frames/ -o out.tif --algo depth-map --zmap zmap.tif
```

## Using the GPU

The GPU-accelerated fusion path for the `laplacian` algorithm is compiled in
by default (`cargo build --release`) — `weighted-average` and `depth-map`
always run on the CPU. When the path is compiled in and a usable adapter is
present, `stack` uses it automatically; there is no opt-in flag. `--no-gpu`
forces the CPU path regardless, and `cargo build --release
--no-default-features` excludes the GPU path from the binary entirely. A
missing or unusable adapter falls back to the CPU path on its own, printing
why on stderr, never silently and never as a hard failure.
Every GPU kernel produces the same result as its CPU counterpart, to within
1 LSB at 16-bit, so switching backends changes speed, not the output — see
`--json`'s `backend` field in
[`reference/cli.md`](../reference/cli.md#stack) to confirm which one ran.

## Reading the result

```bash
focusforge-cli stack path/to/frames/ -o out.tif --algo laplacian --json
```

`--json` reports the frames that were read, the resolved algorithm and
parameters (with where each value came from), the tile geometry the run
planned, and the paths written — see
[`reference/cli.md`](../reference/cli.md#--json-1) for the full schema.
