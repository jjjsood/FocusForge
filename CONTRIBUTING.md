# Contributing

## Repository layout

```
crates/core/   TIFF and RAW I/O, tiles, stacking and alignment algorithms, the
               Z-Map and its retouch op log (brush strokes, undo/redo, the
               rainbow overlay palette), the job engine (pause/resume/cancel
               over tile-sized work units), the stage cache, grouping
               proposals, the `.ffp` project container and its
               checkpoint history (op log replay, sparse delta blobs,
               image export in TIFF, PNG and JPEG)
crates/gpu/    the shared wgpu device, WGSL fusion/warp kernels and the
               tile-scheduler pipeline that runs them, behind the `gpu`
               feature (on by default). An allocate-and-back-off probe at
               startup discovers the device's usable VRAM budget, mirroring
               `crates/core`'s own host-memory budget one crate over
crates/cli/    focusforge-cli, the headless interface: every algorithm,
               every batch-run control (grouping, presets, resume, review,
               the stage cache), the retouch op list, `history` and `export`
               are reachable here
crates/diagnostics/ the shared `tracing` subscriber (stderr sink, rotating
               JSON Lines file), the panic hook and its in-memory flight
               recorder, the native-crash monitor process and the
               diagnostics bundle writer -- linked only by `crates/cli` and
               `crates/gui`, never by `crates/core` or `crates/gpu`
crates/gui/    focusforge-gui: a Slint window shell (toolbar, dock, tab
               strip, projects overlay, settings modal), a wgpu-backed
               canvas that renders a composite/Z-Map pair and, in editor
               mode, a brush and pipette over a live recomposite, a fuse
               worker that drives `crates/core`'s own job engine on a
               background thread, a load worker that walks a folder and
               decodes a thumbnail off the event-loop thread, and a
               keyboard/mouse binding table read from `settings.toml`
spikes/        standalone, workspace-excluded feasibility spikes
fixtures/      small checked-in TIFF crops and a RAW DNG fixture used by the test suite
```

## Tech stack

- Rust, edition 2024, toolchain pinned to `1.95.0` (`rust-toolchain.toml`).
- `focusforge-core`: `memmap2`, `thiserror`, `inventory` (algorithm
  registration), `rayon`, `serde`/`serde_json`, `blake3` (project-blob
  hashing), `zip`/`zstd` for the `.ffp` container (compression backends and
  `zip`'s clock feature both off, so two seals of the same inputs produce
  identical container bytes), `sysinfo` (host memory for the adaptive
  tile-scheduler budget), and `rawler` for RAW metadata and development, and `png` and `jpeg-encoder`
  (image export).
- `focusforge-gpu`: `wgpu = "29.0.4"`, gated behind its own `gpu` Cargo
  feature, on by default on both `focusforge-gpu` and `focusforge-cli`;
  `--no-default-features` excludes it, so `cargo build --workspace
  --no-default-features` never needs a GPU or a Vulkan/Metal/DX12-capable
  driver.
- `focusforge-cli`: `clap`, `serde`/`serde_json`, `toml`, `signal-hook`
  (SIGINT/SIGUSR1 handling for `stack`'s pause/resume/cancel controls), `png`
  (`retouch --overlay`'s PNG output), and the same `gpu` feature forwarded
  to enable `--no-gpu` and the automatic GPU path on `stack`/`align`.
- `focusforge-gui`: `slint = "~1.17"` (`default-features = false`, features
  `std`, `compat-1-2`, `backend-winit`, `renderer-femtovg-wgpu`,
  `unstable-wgpu-29` pinned to the same wgpu major `focusforge-gpu`
  declares, and `unstable-winit-030`), `slint-build` (build dependency),
  `i-slint-backend-testing` (dev dependency), `clap`, `rfd` (native
  folder/file picker), `toml`/`toml_edit`/`serde` (`settings.toml`'s
  `[recent]`, `[input]` and `[export]` tables), `directories` (per-platform data/config
  paths), `png`
  (`--screenshot`), `tracing`. Inter ships bundled under `assets/fonts/`;
  the icon set is vendored from Lucide under `assets/icons/`.
- `focusforge-diagnostics`: `tracing`/`tracing-subscriber` (with the
  `tracing-log` bridge, so `log`-crate records from `wgpu`/`naga` land in
  the same sinks), `directories`, `crash-handler`/`minidumper` (the
  native-crash sidecar), `thiserror`, `zip` (diagnostics bundle). `tracing-tracy` and
  `tracing-chrome` are pulled in only behind its own `trace` Cargo feature,
  which `focusforge-cli` and `focusforge-gui` forward.
- Every dependency's license is checked against an allowlist in
  `deny.toml` with `cargo deny check licenses` (install with `cargo install
  cargo-deny --locked`).
- Prerequisites: **`libfontconfig1-dev`** (Debian/Ubuntu naming) — Slint's
  `renderer-femtovg-wgpu` renderer needs fontconfig. **numpy** (`python3 -m
  pip install numpy`) — needed for the synthetic-alignment gate stage
  (`spikes/synthetic-ground-truth/synth_stack.py --warp`).

## Tests

`cargo test --workspace` covers TIFF header parsing (byte order, tag
handling, truncation and out-of-bounds errors), tile reads against both
byte-order and strip-height variants, the writer's round trip back through
the reader, the tile scheduler's memory budget, the Z-Map reader and
writer, each stacking algorithm's fusion output, the pyramidal ECC solver
and its warp and auto-crop stages, the job engine's pause/resume/cancel
control loop, the stage cache, grouping proposals and the `.ffp` project
container's read/write round trip, the retouch op log (brush strokes,
undo/redo, the rainbow overlay palette) replayed against a whole-map
oracle, the checkpoint history (sparse delta blobs, snapshot cadence,
branching, delete and import validation, the export formats), and the
`focusforge-cli
stack`/`align`/`algorithms`/`export`/`retouch`/`history`/`review`/`cache`/`diagnostics`
commands end to end -- including a forced panic, a forced native crash and
the crash notice on the next start, each driven by spawning the built
binary as a child process so a deliberate crash never takes down the test
runner -- plus the fixture manifest. `crates/gui` has its own suite — the
theme tokens, the plain-Rust shell and canvas state, the viewport
arithmetic, the input-binding table and its `settings.toml` file format,
classifying a folder, a dropped set of frames or a saved project into tabs,
the `Images` and `Settings` dock panels, editor mode's brush, pipette,
undo/redo and overlay controls and its dual-pane cursor mirroring, the
checkpoint panel and export dialog, the settings file, the
projects overlay's gallery and failure cards, and the fuse worker's
pause/resume/cancel/retry states all run headlessly through
`i-slint-backend-testing`, with no real window; a separate windowed
suite launches the real binary and needs a compositor and a GPU adapter,
skipping itself with a printed line on a host with neither. Fixtures under
`fixtures/mantis-64` and `fixtures/mantis-512` are small, checksummed crops
of a 61-frame macro stack, generated rather than hand-edited. Regenerating
them, and the `#[ignore]`d tests that exercise the full stack, read its
location from `FOCUSFORGE_REFERENCE_STACK`; unset, they skip.
RAW metadata, directory grouping and decoding are covered with synthetic DNG
fixtures. `crates/diagnostics` has its own suite covering the subscriber's per-target
filters, the rotating log file, the `log`-crate bridge, the panic hook's
report and flight recording, and the crash-notice bookkeeping and
diagnostics bundle's redaction and manifest, each writing into its own
temporary directory rather than a shared one.

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

## Profiling

`--features trace` hangs two profiling layers on the spans the log file
already carries — no new instrumentation, and nothing of either profiler is
compiled or even resolved without the feature.

```
cargo build -p focusforge-cli --features trace
FOCUSFORGE_TRACE_FILE=trace.json target/debug/focusforge-cli stack fixtures/mantis-512 --lax
```

`FOCUSFORGE_TRACE_FILE` names the Chrome-trace file the run writes; unset,
no file is opened and the layer costs nothing. Load the result in a trace
viewer (`ui.perfetto.dev`, or Chrome's own `chrome://tracing`): the stage
spans nest inside the stack span, and each tile span sits on the pool
thread that ran it. A path that cannot be opened is one warning on stderr,
not a failed run.

The same build serves the Tracy profiler, which collects nothing until one
attaches: start the binary, then connect Tracy from the same machine — the
client listens on localhost and announces itself nowhere. Stage and tile
zones appear live, named by their fields.

`focusforge-gui` takes the same feature and reads the same variable; its
`gui_`-prefixed spans draw on their own admission budget, apart from the
processing spans, and the closing `timing_summary` span reports both. Add
`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` to see Slint's frame timing.

## Before you send a change

`tools/check-all.sh` runs every gate (formatting, licenses, fixtures, build,
clippy, tests, rustdoc, advisories, and the synthetic-ground-truth alignment
check) in one command; `--static` is the fast subset a pre-commit hook runs.
Read `tools/README.md` for what each script does before running cargo
commands by hand.

`crates/core` never links a UI crate, and no reader or writer API returns a
whole frame — the reference stack this project targets does not fit in
memory, so tiling is the default path everywhere, not a fallback.
