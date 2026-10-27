# tools/

Scripts that check and drive **this** repository: gates, fixture generation
from the reference stack, and the shared build/GPU lock.

`tools/check-all.sh` is the one canonical build gate. Run it locally before
acceptance or merge; GitHub Actions runs the same command on Linux, macOS and
Windows for additional native-platform evidence. CI does not replace the local
gate. It needs Rust 1.95.0 with rustfmt and clippy, cargo-deny 0.20.2,
cargo-nextest 0.9.144, Python 3.12 with NumPy 2.2.6 (CI pins 3.12.10, the last 3.12 release with Windows and macOS builds), and Linux GUI link
prerequisites when run on Linux.

| script | what it does |
|---|---|
| `check-all.sh [--static] [--verbose] [--tail N] [--log-dir DIR]` | Every gate, one command, bounded output, pass/fail summary. `--static` is the pre-commit set. Skips cleanly until `Cargo.toml` exists. |
| `check-licenses.sh [--advisories]` | `cargo deny` over the GPLv3 compatibility allowlist; `--advisories` checks RUSTSEC instead. Skips until `deny.toml` is committed. |
| `check-headers.sh` | Requires the opening GPL-3.0-or-later SPDX comment on authored Rust, Slint, WGSL, shell, Python, and hook files. `check-headers-selftest.sh` proves its covered classes and exclusions. |
| `check-notice.sh [--write]` | Compares root `NOTICE` with the locked, offline default CLI+GUI runtime graphs for three release targets. `--write` explicitly regenerates it; the check never edits files. Cargo's cached metadata and package index must be available. `check-notice-selftest.sh` covers deterministic rendering and stale component records. |
| `check-fixtures.sh` | Verifies `fixtures/MANIFEST.sha256` both ways: every listed file still hashes correctly, and no unlisted file is hiding under `fixtures/`. |
| `check-fixtures-selftest.sh` | Proves `check-fixtures.sh`'s refusals and passes against synthetic throwaway trees, so the gate is never trusted on the strength of only ever having been seen green. |
| `check-workflow-selftest.py` | Guards the checked-in Actions workflow's native matrix, immutable action pins, one canonical gate call, and narrow hidden-log artifact upload. |
| `package-linux.sh` | Builds the GUI+CLI AppImage and `.deb` in the pinned Ubuntu 22.04 container. Run `tools/with-lock.sh build -- tools/package-linux.sh`; outputs, SHA-256 file and provenance inventory go under `.scratch/package-linux/`. |
| `check-package-linux.sh [.scratch/package-linux]` | Uses only generated artifacts and copied fixture/legal expectations in fresh Ubuntu 22.04 containers. It checks inventories, hashes, loader/GLIBC requirements, Debian dependencies, CLI metadata and GUI help. Run it under the build lock after `package-linux.sh`. |
| `package-macos.sh [--output DIR]` | On macOS, builds arm64 GUI and CLI binaries, assembles `FocusForge.app`, and creates a DMG below `target/distrib/native/`, retrying the DMG step and keeping diagnostics under `.scratch/package-macos-diagnostics/`. |
| `check-package-macos.sh DMG` | On macOS, mounts a DMG read-only and checks both arm64 binaries, bundle metadata, legal files, icon, and the packaged CLI. |
| `package-windows.ps1 [-OutputDirectory DIR]` | On Windows, verifies the pinned WiX package, builds both executables, and creates a combined MSI below `target/distrib/native/`. |
| `check-package-windows.ps1 -Msi PATH` | On Windows, silently installs and uninstalls an MSI, retaining installer logs while checking executables, legal files, and Start-menu shortcut. |
| `collect-native-artifacts.py` | Validates the macOS and Windows package receipts, hashes and symbol archives from `target/distrib` and gathers the DMG, MSI and symbol archives under `.scratch/native-artifacts/`. |
| `release-assets.py` | Stdlib release file list: `cli-archive` packs a built `focusforge-cli` with `LICENSE` and `NOTICE`; `assemble` copies the installers and archives into `files/`, writes `SHA256SUMS` and `notes.md`; `check` fails on any missing, extra, non-regular or mismatched file. Offline cases live in `check-release-assets-selftest.py`. |
| `package-symbols.py` | Downloads checksum-pinned `dump_syms`, creates complete native debug plus Breakpad stores, and refuses stale, missing or mismatched GUI/CLI module identities and source paths outside the product, its dependencies and the toolchain. On a GitHub-hosted runner (`GITHUB_ACTIONS=true`) the checkout, cargo home, Xcode/Visual Studio/Windows SDK and Rust toolchain locations count as those; elsewhere only the container layout does. Its pins and license declarations live in `packaging/symbol-tools.toml`. |
| `symbolicate.py MINIDUMP SYMBOL_ARCHIVE` | Extracts one verified local archive safely and runs the checksum-pinned offline stackwalker. It accepts only a crashing-thread application frame whose loaded module ID is in the archive. |
| `check-release-gate.py` | Stdlib fail-closed release gate. Its offline cases live in `check-packaging-selftest.py`; hosted workflow use queries the exact `check.yml` push run and its three platform jobs for the candidate SHA with the run's `GH_TOKEN` (`--anonymous` skips it) and waits out rate limits and transient server or network errors until its deadline. |
| `check-packaging-selftest.py` | Offline mutations for package pins/configuration, artifact checker boundaries and release-gate refusal rules. Run it separately before package builds. It needs Bash, Python 3.12 and PowerShell 7+ on `PATH`; its macOS/Windows command journeys use local mocks and do not build installers. |
| `fixture-e2e.sh` | Runs the built `focusforge-cli` over the checked-in fixtures. Cargo-free on purpose, so `check-all.sh` can run it beside the cargo gates. |
| `tiff-diff.py A.tif B.tif [--tol N] [--zmap] [--bbox]` | The independent oracle for image outputs: compares two TIFFs pixel by pixel without going through this project's own reader or writer. `--tol` sets the allowed per-channel difference, `--zmap` compares `u16` source-index maps instead of RGB, `--bbox` prints the bounding rect of pixels differing beyond `--tol`. |
| `image-metrics.py COMPOSITE.tif [--zmap ZMAP.tif] [--vs OTHER.tif] [--json]` | Reference-free numbers for a fused composite: clip fraction at 0 and at the maximum, row and column stripe energy, and grain, per channel; with `--zmap` the Z-Map coherence (the figure `focusforge-cli review` reports); with `--vs` the mean and maximum absolute difference and the differing-sample fraction against a second composite of the same size. Reads rows in bands through `lib/rawtiff.py`, so any image size fits in bounded memory. Input is an uncompressed strip TIFF, which is what `focusforge-cli export` writes by default (`--tiff-compression none`); LZW, deflate and tiled files are refused. The numbers carry no limits and rank nothing: stripe and grain follow the scene's own structure, so compare against a second method's or a known-good composite of the same scene (`--help` explains each number and what it does not show). `--json` prints one object with fixed keys. Exit 0 report printed, 1 unreadable or incomparable input, 2 usage error; `--selftest` checks every measure against synthetic images. |
| `make-fixtures.sh [--check] [--stack DIR]` | Cuts the committed `fixtures/` trees out of the reference stack named by `FOCUSFORGE_REFERENCE_STACK` (or `--stack`), regenerates their READMEs and `fixtures/MANIFEST.sha256`. `--check` verifies the committed trees match byte for byte without writing anything; SKIPs when the variable is unset. |
| `lib/rawtiff.py` | The stdlib-only TIFF reader/writer `tiff-diff.py` and `make-fixtures.sh` share: mmap-based, handles both byte orders and any strip height so it can open the reference stack's result file as well as the source frames. |
| `lib/make_fixtures.py` | The cropping and README/manifest generation `make-fixtures.sh` drives. |
| `synth-align.sh` | Renders a synthetic focus stack with a KNOWN per-frame misregistration and runs `focusforge-cli align --json` over it twice, writing `.scratch/synth-align/measurement.json` -- the artefact `crates/cli/tests/synthetic_ground_truth.rs` asserts against. Needs numpy. |
| `run-acceptance.sh [--limit SIZE] [--algo ID]` | The manual reference-stack run — minutes, so never in a gate. Reads the stack's location from `FOCUSFORGE_REFERENCE_STACK`, SKIPs when unset, and builds a 61-entry symlink farm from it (never reads the top level directly — see the glob trap in the script's header), runs `stack --algorithm laplacian --set levels=5` through `target/release/focusforge-cli` under `/usr/bin/time -v`, and with `--limit 8G` repeats it inside a `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0` cgroup, refusing unless the limit reads back enforced. `--algo ID` (default `laplacian`) swaps the runner's stacking algorithm; `--set levels=5` is emitted only for `laplacian` (`depth-map`'s schema has no `levels` parameter). Run each mode once and record the numbers. `ACCEPTANCE_TARGET_FRAMES` (default: the source count) repeats the 61 real frames up to that many farm entries (`r<replica>-<name>` symlinks), for a 300/600-frame CLI-scale run; `ACCEPTANCE_ALIGN=0` swaps the in-process/standalone `pyramidal-ecc` alignment for `--no-align`, for a leg that measures the scheduler, not alignment. |
| `cargo nextest run -p focusforge-gpu --features gpu-tests --profile gpu` | The GPU/CPU parity suite, under `with-lock.sh gpu` — over an hour, so never in a gate. The `gpu` profile in `.config/nextest.toml` runs one test at a time, prints `SLOW` every minute per running test, kills a test past its limit as a named `TIMEOUT`, reports a crash as its signal with the test name, and writes per-test durations to `target/nextest/gpu/junit.xml`. |
| `lib/run-logged.sh` | The logging mechanism the gates share: each gate's full output to its own file, two bounded lines to stdout. |
| `run-logged.sh "<name>" -- <cmd>` | The same treatment for a one-off noisy command. |
| `make-logo-assets.sh` | Rasterises the brand SVGs in `assets/` into `assets/build/`: the PNG ladder, the hicolor tree, `focusforge.ico`, `focusforge.icns` and the repository page images. Picks the small-size cut at 32 px and under. Skips until `assets/logo.svg` exists. |
| `lib/write-icns.py` | Packs PNGs into a macOS `.icns` suite and reads its own output back to verify it. Exists because the ImageMagick build here has no ICNS coder. |
| `lib/make-wordmark.py` | Authoring step, not part of the build: sets the wordmark in Inter, bakes the text to paths and rewrites the four lockup SVGs in `assets/`. Run it by hand when the type or the proportions change. |
| `with-lock.sh <build\|gpu> -- <command> [args...]` | Serializes a cargo invocation, a direct `target/{debug,release}/focusforge-cli` run, or any GPU-touching command against other builds and GPU runs on this machine — see `lib/lock.sh` for what each lock guards. `check-all.sh` and `run-acceptance.sh` already route every cargo/GPU call through it. Refuses instantly (exit 3) if wrapped around a script that already takes the same lock itself instead of hanging forever; any other held-but-wedged lock times out after `FF_LOCK_TIMEOUT` (default 1800s, exit 4) rather than blocking without end. |
| `lib/lock.sh` | The build/gpu flock pair `with-lock.sh` fronts — sourced directly by a script that wants `with_lock` itself rather than shelling out. Lock ownership is a real fd (`exec {fd}>lock; flock $fd`) with the holder's pid recorded alongside it, so a nested `with_lock` call whose own ancestor already holds the lock is recognised and refused rather than deadlocking silently. |
| `run-gpu-suite.sh [-- <extra cargo test args>]` | The full `cargo test -p focusforge-gpu --features gpu-tests` run, under the GPU lock, through `run-logged.sh` instead of a raw pipe — a `... \| tail -N` buffers every line until the process exits, so a run whose worst case (the forced-one-tile-budget parity tests) can take well over an hour gives no way to tell progress from a hang. Prints its log path immediately; `tail -f` on it follows a still-running suite. |

Every script skips rather than fails when the thing it checks does not exist
yet — an empty repository is not a broken one. A skip prints `SKIP: <reason>`
and exits 0.

Release producers retain release line tables only until symbol extraction. The
installer copies are stripped; Linux carries a GNU debuglink, macOS retains a
arm64 `.dSYM`, and Windows retains a matching `.pdb` only in the symbol
archive. Keep the generated `FocusForge-<version>-<platform>-symbols.tar.gz`
and its `symbols.sha256` or release `native-artifacts.sha256` checksum entry
beside the installer when diagnosing a user dump. `package-symbols.py` needs
Python 3.10+; it verifies dump_syms 2.3.9 (MIT OR Apache-2.0) and
minidump-stackwalk 0.27.0 (MIT) from the pinned release manifests before
execution. GitHub Release assets carry the Linux, both macOS and Windows target
archives next to their installers. Do not use network symbol retrieval:
`symbolicate.py MINIDUMP SYMBOL_ARCHIVE` uses only the archive `store/` path.
Line tables provide function/source-line diagnosis, not a promise of native
types, local variables or deeper inline-debugging detail; missing archived data
cannot be reconstructed from a stripped published binary.
