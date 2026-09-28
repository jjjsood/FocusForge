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

## Features

- Fuses a directory of TIFF or supported RAW frames into a focus-stacked project
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
- Cancellable, pausable and resumable runs — Ctrl-C stops a run cleanly,
  SIGUSR1 pauses one in place on Unix, and `--resume` picks a cancelled stack
  back up without redoing finished work
- Directory-wide stack inspection (`focusforge-cli info`) — dimensions, depth,
  byte order, compression, layout and a uniform-layout check across every
  frame, plain text or `--json`
- Reads 16-bit unsigned chunky TIFF frames stored uncompressed, with LZW, or
  with Deflate, in strip or tiled layouts; flags unsupported layouts by name
  instead of silently misreading them
- Fuses the `laplacian` algorithm on a GPU automatically when one is
  available, falling back to the CPU cleanly when it isn't; either path
  produces the same result, proven to match within 1 LSB at 16-bit
- A single parameter schema drives every algorithm's settings; `focusforge-cli
  algorithms` lists what's registered, prints a schema, and validates
  `--set key=value` overrides against it
- Keeps a structured log of every run and, on a crash, a report of what led
  to it, all local to your own machine; `focusforge-cli diagnostics`
  packages the evidence into one redacted ZIP, and the next start after a
  crash tells you it's there
- A window (`focusforge-gui`) alongside the CLI: open a folder or a saved
  project into a tab, view the selected source frame beside the composite or Z-Map,
  choose an algorithm and edit its parameters from the same schema
  `focusforge-cli algorithms` prints, and run, pause, cancel or retry a
  fuse without leaving it

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

Prints one block per frame in the directory. TIFF entries show size, depth,
byte order, compression, strip layout and whether the mmap fast path applies;
RAW entries show decoder metadata without developing pixels. The report ends
with a frame count, total size and a uniform-layout verdict. Add `--json` for
the same report as machine-readable JSON.

RAW frames use `rawler` as the only decoder. Files it cannot read return an
error; there is no decoder fallback.

### Stack a folder

```bash
target/release/focusforge-cli stack path/to/frames/ --algo laplacian
target/release/focusforge-cli export path/to/frames/frames.ffp -o out.tif --zmap zmap.tif
```

`stack` aligns the frames and fuses them into a project beside the frames
(one `.ffp` file per detected stack); `export` then turns that project
into `out.tif` and its per-pixel source map `zmap.tif`. Alignment and the
auto-crop that precedes fusion are both on by default (`--no-align` /
`--no-crop` turn them off); `focusforge-cli align` reports the same
transforms on their own, before any fusion runs. See
[`docs/guide/stacking.md`](docs/guide/stacking.md) for the other algorithms
and their parameters, and [`docs/guide/batch-runs.md`](docs/guide/batch-runs.md)
for stacking a folder that holds more than one stack.

### Open it in the window

```bash
target/release/focusforge-gui --open path/to/frames/
```

Opens the same folder into a window instead of the command line, one tab
per detected stack. See [`docs/guide/gui.md`](docs/guide/gui.md).

## Documentation

See [`docs/`](docs/README.md) for the full guide and reference, including
every `focusforge-cli` subcommand, supported RAW input, and the TIFF layouts
it reads and writes.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the repository layout, tech
stack, tests and the gate to run before sending a change.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). FocusForge is GPLv3 because Slint
is used under its GPLv3 licensing option, which is what makes the whole
application GPLv3 rather than a more permissive license; dependency licenses
are held to a compatible allowlist and enforced by `cargo deny check
licenses`.
