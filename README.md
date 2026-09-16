<p align="center">
  <img src="assets/wordmark-horizontal.svg" alt="FocusForge" width="420">
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: GPL v3" src="https://img.shields.io/badge/license-GPLv3-blue.svg"></a>
  <img alt="Rust edition 2024" src="https://img.shields.io/badge/rust-edition%202024-orange.svg">
</p>

<p align="center">
A cross-platform focus stacking application for Linux, macOS and Windows, with
a full retouching editor rather than a batch processor. Built in Rust on Slint
for the interface and wgpu for compute and canvas. Free software under the
GNU General Public License v3.
</p>

<!--
  Hero screenshot goes here once the GUI has a window worth showing.
  Suggested: assets/screenshots/editor.png, ~1200px wide, PNG, with a
  <sub> caption underneath.
-->

## Features

- Fuses a directory of TIFF frames into a focus-stacked project
  (`focusforge-cli stack`), out-of-core so the stack never has to fit in RAM
- Detects and proposes how a mixed folder of frames splits into separate
  stacks by capture time and directory structure, printed before the run and
  overridable or saved to a file for editing and reuse
- Aligns handheld or breathing frames before fusing them, recovering shift,
  rotation and scale drift and cropping the result to the region every frame
  covers (`focusforge-cli align` reports the same transforms on their own)
- Three stacking algorithms — Laplacian-pyramid coefficient selection,
  sharpness-weighted blending, and depth-map selection — each producing a
  Z-Map naming the source frame behind every output pixel
- Turns a stacked project into an image file on request (`focusforge-cli
  export`), separately from the run that produced it, so exporting is never
  bundled into a batch you didn't ask to also render
- Reports outcome, sharpness and Z-Map coverage per stack after a run
  (`focusforge-cli review`), so a failed stack in a batch is a visible record
  rather than a missing file
- Reusable parameter bundles (presets) and a re-run flag that fuses only the
  stacks you name, so tuning one stack after a batch run costs one re-run,
  not the whole folder again
- Cancellable and resumable runs — Ctrl-C stops a run cleanly and `--resume`
  picks the same stack back up without redoing finished work
- Directory-wide stack inspection (`focusforge-cli info`) — dimensions, depth,
  byte order, compression, strip layout and a uniform-layout check across
  every frame, plain text or `--json`
- Flags an unsupported frame by name (compression, tiling, planar layout, a
  bit depth other than 16) instead of silently misreading it
- Fuses the `laplacian` algorithm on a GPU automatically when one is
  available, falling back to the CPU cleanly when it isn't; either path
  produces the same result, proven to match within 1 LSB at 16-bit
- A single parameter schema drives every algorithm's settings; `focusforge-cli
  algorithms` lists what's registered, prints a schema, and validates
  `--set key=value` overrides against it

## Quick start

### Prerequisites

Rust, toolchain pinned to `1.95.0` (`rust-toolchain.toml`, picked up
automatically by `rustup`). `crates/gui` links Slint with
`renderer-femtovg-wgpu`, which has a hard dependency on fontconfig; on
Debian/Ubuntu-family hosts install the dev package before building:

```bash
sudo apt install libfontconfig1-dev
```

There is no code-side workaround for a missing fontconfig on this renderer.

### Build

```bash
cargo build --release
```

Produces `target/release/focusforge-cli` and `target/release/focusforge-gui`,
with the GPU-accelerated fusion path compiled in by default. Pass
`--no-default-features` to exclude it; without it, `stack` always runs on the
CPU and `--no-gpu` does not exist as a flag.

### Inspect a stack

```bash
target/release/focusforge-cli info path/to/frames/
```

Prints one block per TIFF in the directory — size, depth, byte order,
compression, strip layout, whether the mmap fast path applies — followed by a
frame count, total size and a uniform-layout verdict. Add `--json` for the
same report as machine-readable JSON.

### Stack a folder

```bash
target/release/focusforge-cli stack path/to/frames/ --algo laplacian
target/release/focusforge-cli export path/to/frames/frames.ffproj -o out.tif --zmap zmap.tif
```

`stack` aligns the frames and fuses them into a project beside the frames
(one `.ffproj` file per detected stack); `export` then turns that project
into `out.tif` and its per-pixel source map `zmap.tif`. Alignment and the
auto-crop that precedes fusion are both on by default (`--no-align` /
`--no-crop` turn them off); `focusforge-cli align` reports the same
transforms on their own, before any fusion runs. See
[`docs/guide/stacking.md`](docs/guide/stacking.md) for the other algorithms
and their parameters, and [`docs/guide/batch-runs.md`](docs/guide/batch-runs.md)
for stacking a folder that holds more than one stack.

## Documentation

See [`docs/`](docs/README.md) for the full guide and reference, including
every `focusforge-cli` subcommand and the TIFF layouts it reads and writes.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the repository layout, tech
stack, tests and the gate to run before sending a change.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). FocusForge is GPLv3 because Slint
is used under its GPLv3 licensing option, which is what makes the whole
application GPLv3 rather than a more permissive license; dependency licenses
are held to a compatible allowlist and enforced by `cargo deny check
licenses`.
