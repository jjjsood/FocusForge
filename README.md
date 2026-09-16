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
  Hero screenshot goes here once the GUI ships a window worth showing
  (crates/gui has design tokens and vendored icons,
  but no surface yet). Suggested: assets/screenshots/editor.png, ~1200px
  wide, PNG, with a <sub> caption underneath.
-->

## Features

- Fuses a directory of TIFF frames into one composite (`focusforge-cli
  stack`), out-of-core so the stack never has to fit in RAM
- Aligns handheld or breathing frames before fusing them, recovering shift,
  rotation and scale drift and cropping the result to the region every frame
  covers (`focusforge-cli align` reports the same transforms on their own)
- Three stacking algorithms — Laplacian-pyramid coefficient selection,
  sharpness-weighted blending, and depth-map selection — each producing a
  Z-Map naming the source frame behind every output pixel
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

Rust, toolchain pinned to `1.94.1` (`rust-toolchain.toml`, picked up
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

Produces `target/release/focusforge-cli`, the only binary in the workspace
right now, with the GPU-accelerated fusion path compiled in by default. Pass
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
target/release/focusforge-cli stack path/to/frames/ -o out.tif --algo laplacian --zmap zmap.tif
```

Aligns the frames, fuses every one into `out.tif` through the `laplacian`
algorithm, and writes the per-pixel source map to `zmap.tif`. Alignment and
the auto-crop that follows it are both on by default (`--no-align` /
`--no-crop` turn them off); `focusforge-cli align` reports the same
transforms on their own, before any fusion runs. See
[`docs/guide/stacking.md`](docs/guide/stacking.md) for the other algorithms
and their parameters.

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
