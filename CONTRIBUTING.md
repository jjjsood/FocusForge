# Contributing

## Repository layout

```
crates/core/   TIFF and RAW I/O, tiles, stacking and alignment algorithms, the
               Z-Map, the retouch op log and its brush-stroke blend, the
               rainbow overlay palette, the job engine (pause/resume/cancel
               over tile-sized work units), the stage cache, grouping
               proposals, the `.ffp` project container and its
               checkpoint history (op log, render from the stored
               composite, image export in TIFF, PNG and JPEG)
crates/gpu/    the shared wgpu device, WGSL fusion/warp/blend kernels and the
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
               mode, a brush and pipette over live strokes blended
               on the GPU with a CPU fallback, a fuse
               worker that drives `crates/core`'s own job engine on a
               background thread, a load worker that walks a folder and
               decodes a thumbnail off the event-loop thread, and a
               keyboard/mouse binding table read from `settings.toml`
spikes/        standalone, workspace-excluded feasibility spikes
fixtures/      small checked-in TIFF crops, a RAW DNG fixture, folders that open
               or are refused, and `.ffp` projects (valid, corrupt, mismatched,
               missing frames) used by the test suite and the UI journeys
tests/ui/      end-to-end window journeys written as numbered steps
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
  (read back by the export tests), and the same `gpu` feature forwarded
  to enable `--cpu` and the automatic GPU path on `stack`/`align`.
- `focusforge-gui`: `slint = "~1.17"` (`default-features = false`, features
  `std`, `compat-1-2`, `backend-winit`, `renderer-femtovg-wgpu`,
  `unstable-wgpu-29` pinned to the same wgpu major `focusforge-gpu`
  declares, and `unstable-winit-030`), `slint-build` (build dependency),
  `i-slint-backend-testing` (dev dependency), `clap`, `rfd` (native
  folder/file picker), `toml`/`toml_edit`/`serde` (`settings.toml`'s
  `[recent]`, `[appearance]`, `[input]` and `[export]` tables), `notify-rust`
  (desktop banners while the window is in the background), `directories` (per-platform data/config
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
- `NOTICE` records the default CLI and GUI runtime dependency union for Linux,
  Windows, and both macOS release targets, plus bundled fonts and icons. Run
  `tools/check-notice.sh --write` after a locked dependency or asset-attribution
  change; ordinary checks only compare. It needs Cargo's offline metadata and
  package cache, and intentionally excludes dev, build, proc-macro, and
  inactive-feature dependencies.
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
container's read/write round trip, the retouch op log (brush strokes, the
rainbow overlay palette) and the render of a checkpoint's state, the
checkpoint history (append-only saves,
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

Under `cargo nextest` (`.config/nextest.toml`) each test is killed after 15
minutes and reported as a named TIMEOUT; the `hit_cap_behaviour` binary gets
40.

`tests/ui/` holds 18 journeys, each one whole path through the window as
numbered clicks, keys and hovers with an `Expect:` line per step, plus
`features/` files listing which journey covers which feature. They are run by
hand or by a script against `focusforge-gui`; `tests/ui/README.md` gives the
step format and how to launch one, including `--save-folder` so saves and
exports skip the file chooser. They are not part of `cargo test`.

`crates/gpu`'s WGSL fusion and warp kernels, and the pipeline in
`focusforge-cli` that dispatches them, sit behind the `gpu` Cargo feature —
on by default, so a plain `cargo build --workspace` or `cargo test
--workspace` compiles and, where a test drives the `laplacian` algorithm or
`align --write-warped`, runs them. `--no-default-features` excludes the GPU
path entirely and still compiles, test targets included; that build's one
behavioural check
(`cargo test -p focusforge-cli --no-default-features --test cli
no_gpu_flag_absent_without_the_feature`) is run by hand, not in the standing
gate. `cargo clippy --workspace --all-targets -- -D warnings` lints the
default feature set on every gate pass. The device tests that need
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
FOCUSFORGE_TRACE_FILE=trace.json target/debug/focusforge-cli stack fixtures/mantis-512
```

`FOCUSFORGE_TRACE_FILE` names the Chrome-trace file the run writes; unset,
no file is opened and the layer costs nothing. Load the result in a trace
viewer (`ui.perfetto.dev`, or Chrome's own `chrome://tracing`): the stage
spans nest inside the stack span, and each tile span sits on the pool
thread that ran it. A path that cannot be opened is one warning on stderr,
not a failed run.

The trace ends in a `timing_summary` span whose fields are run totals:
bytes uploaded, read back and copied, buffer and pipeline creations, and
nanosecond counters. `prepare_equalize_ns` is the time spent measuring the
sources and fixing the per-frame brightness gains before alignment. On the GPU
tile path, nine `gpu_tile_*_ns` counters (`read`, `warp`, `upload`, `pyramid`, `select`,
`reconstruct`, `readback`, `wait`, `other`) split each tile's time so the
stages sum to the tile's time; they are thread-seconds summed over
concurrent tiles and stay 0 on a CPU run. `upload_bytes` counts every
host-to-device buffer write the tile path makes, parameters included.

The same build serves the Tracy profiler, which collects nothing until one
attaches: start the binary, then connect Tracy from the same machine — the
client listens on localhost and announces itself nowhere. Stage and tile
zones appear live, named by their fields.

`focusforge-gui` takes the same feature and reads the same variable; its
`gui_`-prefixed spans draw on their own admission budget, apart from the
processing spans, and the closing `timing_summary` span reports both. Add
`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` to see Slint's frame timing.

## Before you send a change

`tools/check-all.sh` runs every gate (formatting, licenses, SPDX headers, NOTICE, fixtures, build,
clippy, tests, rustdoc, advisories, and the synthetic-ground-truth alignment
check) in one command; `--static` is the fast subset a pre-commit hook runs.
Run this local gate before acceptance and merge. GitHub Actions calls the same
gate on Linux, macOS and Windows as additional evidence; a green hosted run
does not replace a required local run.
Read `tools/README.md` for what each script does before running cargo
commands by hand.

For user-visible changes, add an entry under `Unreleased` in
[CHANGELOG.md](CHANGELOG.md). New Rust, Slint, WGSL, shell, and Python source
files need the SPDX header checked by `tools/check-headers.sh`.

Read [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md), [SECURITY.md](SECURITY.md), and
[CHANGELOG.md](CHANGELOG.md) before contributing or reporting a problem.
The maintainer website is [somestrangedays.dev](https://www.somestrangedays.dev/).

`crates/core` never links a UI crate, and no reader or writer API returns a
whole frame — the reference stack this project targets does not fit in
memory, so tiling is the default path everywhere, not a fallback.

## Releases

A human starts a release by pushing a version tag such as `v0.1.0`;
`.github/workflows/release.yml` builds the Linux, macOS and Windows packages and
creates the release only after its same-commit checks pass. The release holds
the installers (AppImage, `.deb`, DMG, MSI), one command-line archive per
platform and a single `SHA256SUMS`; the release text puts the changelog entries
above a download table. `tools/release-assets.py` decides that file list and fails on
any extra or missing file. Before creating or pushing a tag, run the documented
Linux package recipe and inspect its two artifacts locally.

Pull requests run the same package jobs and assemble the release files without
creating a release. macOS produces an Apple silicon (arm64) DMG and Windows
produces one combined MSI; their target runners validate the installed package
before uploading it for the assemble job.

The macOS and Windows targets are type-checked natively: the macOS and Windows
legs of the check workflow each run `tools/check-all.sh`, which builds the
workspace on its own OS. A Linux cross check (`rustup target add
aarch64-apple-darwin x86_64-pc-windows-msvc`, then `cargo check -p
focusforge-gui --target …` or `cargo xwin check`) is unsupported: the `blake3`
and `zstd-sys` build scripts need the target's C toolchain and SDK. Windows
toasts are attributed through the AppUserModelID `org.focusforge.FocusForge`,
which the installed Start-menu shortcut registers; a build run outside the MSI
install may show no toast, or one under another application
(https://github.com/hoodie/notify-rust/issues/219).

The macOS package script sizes the disk image from the built app, retries
`hdiutil create` up to three times with more headroom each time, and writes
disk-space and `hdiutil` output to `.scratch/package-macos-diagnostics/`, which
the package workflow uploads as an artifact. The hosted check job is bounded
at 240 minutes.

### Release smoke checklist

Run `tools/check-package-linux.sh .scratch/package-linux` after producing the
Linux artifacts. It runs `tools/smoke-release.sh` once against the extracted
AppImage CLI and once against the installed Debian CLI at
`/usr/bin/focusforge-cli`; each receipt records the installed CLI hash,
version, commands, output and exit status. On macOS the package check runs the
same helper while the DMG is mounted, using
`FocusForge.app/Contents/MacOS/focusforge-cli`. On Windows it runs through Git
Bash before MSI uninstall, using
`$env:ProgramFiles\FocusForge\focusforge-cli.exe`. Keep each native job log and
the helper receipt with the checked artifact; record its source OS, candidate
artifact SHA-256 and native-run URL.

The helper uses only copied `mantis-64` and `mantis-512` frames, forces CPU
fusion and disables alignment. It checks the grouping proposal, two completed
projects, one readable TIFF export, and a mixed corrupt run whose failed record
survives in the manifest, status stream and `review --json` output.

For each artifact, also launch the installed GUI with `--exit-after 3000
--screenshot PATH` on its own operating system and retain its exit result and
nonempty PNG. A working display and graphics driver are required. That launch
does not prove the interactive journey: on Linux X11 and Wayland, macOS and
Windows, import one stack, choose Fuse, save and reopen, select a source, paint
a visible stroke, export TIFF, and record the artifact identity, session type,
observations and screenshot.

Release builds use line tables to create a same-build native debug archive and
Breakpad store for both GUI and CLI. Preserve the platform symbol archive and
its SHA-256 file with each installer; diagnose a retained minidump locally with
`python3 tools/symbolicate.py MINIDUMP SYMBOL_ARCHIVE`. The command downloads
only its pinned stackwalker and never contacts a symbol server.

The symbol archives (`FocusForge-<version>-x86_64-unknown-linux-gnu-symbols.tar.gz`,
`FocusForge-<version>-aarch64-apple-darwin-symbols.tar.gz` and
`FocusForge-<version>-x86_64-pc-windows-msvc-symbols.tar.gz`) are not release
assets. Each release run keeps them for 90 days as the workflow artifacts
`symbols-linux` and `symbols-native`; download them from the run's page and file
each with the installer it belongs to. `package-symbols.py`
requires Python 3.10+ and verifies dump_syms 2.3.9 (MIT OR Apache-2.0) and
minidump-stackwalk 0.27.0 (MIT) before execution. It only supports local
archives and stores; it does not fetch symbols for a dump.

`line-tables-only` supports function and source-line diagnosis; it does not
promise native type, local-variable, or deeper inline-debugging detail, and a
stripped published binary cannot restore debug data that was not archived.

Before tagging, move the `Unreleased` entries into a heading such as
`## [0.1.0] - YYYY-MM-DD`, add a fresh `Unreleased` heading, and commit the
result. The release notes take that version heading (a candidate takes
`Unreleased`); tag only after that commit is ready.

When the CLI parser or workspace version changes, regenerate its packaged
manuals and shell completions before running the package checks:

```sh
FOCUSFORGE_REGENERATE_CLI_ASSETS=1 cargo test -p focusforge-cli --bin focusforge-cli tests::regenerate_cli_assets -- --exact
```
