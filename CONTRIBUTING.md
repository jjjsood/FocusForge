# Contributing

## Repository layout

```
crates/core/   TIFF I/O, tiles, stacking algorithms and the Z-Map
crates/gpu/    the shared wgpu device, WGSL fusion/warp kernels and the
               tile-scheduler pipeline that runs them, behind the `gpu` feature.
               A stated 1 GiB VRAM working-set budget bounds how many tiles
               that pipeline keeps in flight on the device at once, mirroring
               `crates/core`'s own host-memory budget one crate over
crates/cli/    focusforge-cli, the headless interface
crates/gui/    the planned Slint application; no code yet
spikes/        standalone, workspace-excluded feasibility spikes
fixtures/      small checked-in TIFF crops used by the test suite
```

## Tech stack

Rust, edition 2024, toolchain pinned to `1.95.0` (`rust-toolchain.toml`).
`focusforge-core` depends on `memmap2`, `thiserror`, `inventory` (algorithm
registration), `rayon` and `serde`; `focusforge-gpu` declares
`wgpu = "29.0.4"`, gated behind its own `gpu` Cargo feature (off by
default, so `cargo build --workspace` alone never needs a GPU or a
Vulkan/Metal/DX12-capable driver); `focusforge-cli` adds `clap`, `serde`
and `serde_json`, and forwards the same `gpu` feature to enable `--no-gpu`
and the automatic GPU path on `stack`/`align`. Every dependency's license
is checked against an allowlist in
`deny.toml` with `cargo deny check licenses` (install with
`cargo install cargo-deny --locked`). Building the Slint spikes under
`spikes/` additionally needs `libfontconfig1-dev` (or an equivalent
pkg-config shim) for Slint's font stack; the main workspace does not need it.
`tools/check-all.sh`'s "synthetic alignment truth" gate stage needs numpy
(`python3 -m pip install numpy`): it drives
`spikes/synthetic-ground-truth/synth_stack.py --warp`, an independent numpy
oracle for alignment accuracy that is deliberately never ported to Rust (see
that script's and `tools/synth-align.sh`'s own headers for why).

## Tests

`cargo test --workspace` covers TIFF header parsing (byte order, tag
handling, truncation and out-of-bounds errors), tile reads against both
byte-order and strip-height variants, the writer's round trip back through
the reader, the tile scheduler's memory budget, the Z-Map reader and
writer, each stacking algorithm's fusion output, the pyramidal ECC solver
and its warp and auto-crop stages, and the `focusforge-cli
stack`/`align`/`algorithms` commands end to end, plus the fixture manifest.
Fixtures under `fixtures/mantis-64` and `fixtures/mantis-512` are small
crops of a 61-frame macro stack, cut and checksummed by harness tooling —
never hand-edited.

`crates/gpu`'s WGSL fusion and warp kernels, and the pipeline in
`focusforge-cli` that dispatches them, sit behind the `gpu` Cargo feature —
on by default, so a plain `cargo build --workspace` or `cargo test
--workspace` compiles and, where a test drives the `laplacian` algorithm or
`align --write-warped`, runs them. `--no-default-features` excludes the GPU
path entirely and still compiles, test targets included; that build's one
behavioural check
(`cargo test -p focusforge-cli --no-default-features --test cli
no_gpu_flag_absent_without_the_feature`) is run by hand, not in the standing
gate. `cargo clippy --workspace --all-targets --all-features` compiles that
code on every gate pass regardless of the default. The device tests that need
a real GPU adapter sit behind a separate, non-default `gpu-tests` feature and
run as `cargo test --workspace --features gpu-tests -- --test-threads=1` —
run by hand, deliberately, once per change to that code, never inside the
default gate. `--test-threads=1` is required: `parity_pipeline`'s
`warp_to_fusion_never_reads_back` asserts on a process-global readback
counter that only holds under serial execution. Those tests assert each
kernel matches its CPU twin to within 1 LSB at 16-bit, and that a whole GPU
`stack` run matches the equivalent CPU run on `fixtures/mantis-512`.

## Before you send a change

`tools/check-all.sh` runs every gate (formatting, licenses, fixtures, build,
clippy, tests, rustdoc, advisories, and the synthetic-ground-truth alignment
check) in one command; `--static` is the fast subset a pre-commit hook runs.
Read `tools/README.md` for what each script does before running cargo
commands by hand.

`crates/core` never links a UI crate, and no reader or writer API returns a
whole frame — the reference stack this project targets does not fit in
memory, so tiling is the default path everywhere, not a fallback.
