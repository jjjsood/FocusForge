# Contributing

## Repository layout

```
crates/core/   TIFF I/O, tiles, stacking algorithms and the Z-Map
crates/gpu/    wgpu device setup; no kernels yet
crates/cli/    focusforge-cli, the headless interface
crates/gui/    the planned Slint application; no code yet
spikes/        standalone, workspace-excluded feasibility spikes
fixtures/      small checked-in TIFF crops used by the test suite
```

## Tech stack

Rust, edition 2024, toolchain pinned to `1.94.1` (`rust-toolchain.toml`).
`focusforge-core` depends on `memmap2`, `thiserror`, `inventory` (algorithm
registration), `rayon` and `serde`; `focusforge-gpu` declares
`wgpu = "29.0.4"`; `focusforge-cli` adds `clap`, `serde` and `serde_json`.
Every dependency's license is checked against an allowlist in
`deny.toml` with `cargo deny check licenses` (install with
`cargo install cargo-deny --locked`). Building the Slint spikes under
`spikes/` additionally needs `libfontconfig1-dev` (or an equivalent
pkg-config shim) for Slint's font stack; the main workspace does not need it.

## Tests

`cargo test --workspace` covers TIFF header parsing (byte order, tag
handling, truncation and out-of-bounds errors), tile reads against both
byte-order and strip-height variants, the writer's round trip back through
the reader, the tile scheduler's memory budget, the Z-Map reader and
writer, each stacking algorithm's fusion output, and the `focusforge-cli
stack`/`algorithms` commands end to end, plus the fixture manifest.
Fixtures under `fixtures/mantis-64` and `fixtures/mantis-512` are small
crops of a 61-frame macro stack, cut and checksummed by harness tooling —
never hand-edited.

## Before you send a change

`tools/check-all.sh` runs every gate (formatting, licenses, fixtures, build,
clippy, tests, rustdoc, advisories) in one command; `--static` is the fast
subset a pre-commit hook runs. Read `tools/README.md` for what each script
does before running cargo commands by hand.

`crates/core` never links a UI crate, and no reader or writer API returns a
whole frame — the reference stack this project targets does not fit in
memory, so tiling is the default path everywhere, not a fallback.
