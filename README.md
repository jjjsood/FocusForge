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
  (crates/gui has no code yet). Suggested: assets/screenshots/editor.png,
  ~1200px wide, PNG, with a <sub> caption underneath.
-->

## Features

- Directory-wide stack inspection (`focusforge-cli info`) — dimensions, depth,
  byte order, compression, strip layout and a uniform-layout check across
  every frame, plain text or `--json`
- Flags an unsupported frame by name (compression, tiling, planar layout, a
  bit depth other than 16) instead of silently misreading it
- A single parameter schema drives every algorithm's settings; `focusforge-cli
  algorithms` lists what's registered, prints a schema, and validates
  `--set key=value` overrides against it

## Quick start

### Build

```bash
cargo build --release
```

Produces `target/release/focusforge-cli`, the only binary in the workspace
right now.

### Inspect a stack

```bash
target/release/focusforge-cli info path/to/frames/
```

Prints one block per TIFF in the directory — size, depth, byte order,
compression, strip layout, whether the mmap fast path applies — followed by a
frame count, total size and a uniform-layout verdict. Add `--json` for the
same report as machine-readable JSON.

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
