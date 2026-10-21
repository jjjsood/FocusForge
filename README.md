<p align="center">
  <img src="assets/wordmark-horizontal.svg" alt="FocusForge" width="420">
</p>

<h1 align="center">FocusForge</h1>

<p align="center">A cross-platform focus stacking application with a full retouching editor.</p>

<p align="center">
  <a href="LICENSE"><img alt="License: GPL v3" src="https://img.shields.io/badge/license-GPLv3-blue.svg"></a>
  <img alt="Rust edition 2024" src="https://img.shields.io/badge/rust-edition%202024-orange.svg">
  <a href="https://github.com/jjjsood/FocusForge/actions/workflows/check.yml"><img alt="CI" src="https://github.com/jjjsood/FocusForge/actions/workflows/check.yml/badge.svg"></a>
  <a href="https://github.com/jjjsood/FocusForge/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/jjjsood/FocusForge"></a>
</p>

<p align="center">
  <img src="docs/assets/readme-hero.png" alt="FocusForge showing a real Mantis source frame and its stacked composite" width="100%">
</p>

![Source frame 30 beside the FocusForge composite](docs/assets/stack-before-after.jpg)

## Features

**From shoot to sharp image**
- Drop in a focus bracket straight from the camera, RAW or 16-bit TIFF, and get one image sharp from front to back
- Shot several subjects in one session? FocusForge sorts the card into separate stacks for you; check the split before it runs
- Long brackets of high-resolution frames run on an ordinary computer, however much memory the stack would need
- Shot handheld, or your lens breathes while focusing? Frames are lined up first and the edges trimmed to what every shot covers
- Flickering light or exposure drift between shots is evened out
- Three methods to choose from: Details as the all-rounder, Soft transitions for gentler blends between frames, Focus map when you plan to retouch a lot
- Fast on a graphics card, the same result without one

**Fixing what the stack got wrong**
- Halos, ghosting or a misplaced bristle? Brush in the right frame by hand with a soft brush
- A colour overlay shows which shot every part of the image came from, so you know where to paint from
- Unlimited undo, and every version of the edit kept: go back, try another direction, export any of them

**Working through a whole session**
- Use the desktop app, or script the same steps from the command line
- Pause, cancel and pick up long runs later without starting over
- Save your favourite settings as presets; re-run only the one stack you want to tweak
- A summary after each batch shows which stacks worked, so nothing goes missing quietly
- Problems and notices collect in one place, and reach you as a system notification while you're in another app

**Delivering the result**
- Export TIFF, PNG or JPEG with your camera metadata, colour profile and keywords intact
- If something breaks, FocusForge packs a privacy-safe report you can send in

Details for each are in the [guides](docs/README.md).

## Install

Linux: install the matching `.deb` or run its AppImage from [Releases](https://github.com/jjjsood/FocusForge/releases), or build from source.
macOS: open the matching `.dmg` from [Releases](https://github.com/jjjsood/FocusForge/releases) and drag it to Applications, or build from source.
Windows: run the matching `.msi` from [Releases](https://github.com/jjjsood/FocusForge/releases), or build from source. See [getting started](docs/getting-started.md) for paths.

## Quick start

### Build

Rust, toolchain pinned to `1.95.0` (`rust-toolchain.toml`, picked up
automatically by `rustup`). `crates/gui` links Slint with
`renderer-femtovg-wgpu`, which has a hard dependency on fontconfig; on
Debian/Ubuntu-family hosts install the dev package before building:

```bash
sudo apt install libfontconfig1-dev
```

There is no code-side workaround for a missing fontconfig on this renderer.

```bash
cargo build --release
```

Produces `target/release/focusforge-cli` and `target/release/focusforge-gui`,
with the GPU-accelerated fusion path compiled in by default. Pass
`--no-default-features` to exclude it; without it, `stack` always runs on the
CPU and `--cpu` does not exist as a flag.

### CLI

### Inspect a stack

```bash
target/release/focusforge-cli info path/to/frames/
target/release/focusforge-cli info path/to/project.ffp
```

For a project file, prints stored project facts after structural and head
Z-Map validation without reading source frames. For a directory, prints one
block per frame. TIFF entries show size, depth, byte order, compression and
strip layout; RAW entries show decoder metadata without developing pixels. The
report ends with a frame count, total size and a uniform-layout verdict. Add
`--json` for the same report as machine-readable JSON.

RAW frames use `rawler` as the only decoder. Files it cannot read return an
error; there is no decoder fallback.

### Stack a folder

```bash
target/release/focusforge-cli stack path/to/frames/ --algorithm laplacian
target/release/focusforge-cli export path/to/frames/frames.ffp -o out.tif --zmap zmap.tif
```

`stack` aligns the frames and fuses them into a project beside the frames
(one `.ffp` file per detected stack); `export` then renders that project
into `out.tif` and writes its per-pixel source map `zmap.tif`. Name the
output `out.png` or `out.jpg` for those formats. Alignment and the
auto-crop that precedes fusion are both on by default (`--no-align` /
`--no-crop` turn them off); `focusforge-cli align` reports the same
transforms on their own, before any fusion runs. See
[`docs/guide/stacking.md`](docs/guide/stacking.md) for the other algorithms
and their parameters, and [`docs/guide/batch-runs.md`](docs/guide/batch-runs.md)
for stacking a folder that holds more than one stack.

The Mantis photographs and derived comparison image are used only within this
project; see [their image rights notice](docs/assets/README.md).

### GUI

```bash
target/release/focusforge-gui --open path/to/frames/
```

Opens the same folder into a window instead of the command line, one tab
per detected stack. See [`docs/guide/gui.md`](docs/guide/gui.md).

## Documentation

See [`docs/`](docs/README.md) for [getting started](docs/getting-started.md)
and the guides (the window, stacking, alignment, retouching, checkpoint
history, batch runs, diagnostics) and the
reference for every `focusforge-cli` subcommand, the project format and the
TIFF layouts it reads and writes.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for layout, tests and the contribution gate;
[Code of Conduct](CODE_OF_CONDUCT.md), [Security policy](SECURITY.md) and [changelog](CHANGELOG.md) set community standards.

## License

GPL-3.0-only. See [LICENSE](LICENSE). FocusForge is GPLv3 because Slint
is used under its GPLv3 licensing option, which is what makes the whole
application GPLv3 rather than a more permissive license; dependency licenses
are held to a compatible allowlist and enforced by `cargo deny check
licenses`.
This software is based in part on the work of the Independent JPEG Group,
carried by the `jpeg-encoder` crate's JPEG export path.
