# tools/

Scripts that check and drive **this** repository: gates, fixture generation
from the reference stack, and the shared build/GPU lock.

| script | what it does |
|---|---|
| `check-all.sh [--static] [--verbose] [--tail N] [--log-dir DIR]` | Every gate, one command, bounded output, pass/fail summary. `--static` is the pre-commit set. Skips cleanly until `Cargo.toml` exists. |
| `check-licenses.sh [--advisories]` | `cargo deny` over the GPLv3 compatibility allowlist; `--advisories` checks RUSTSEC instead. Skips until `deny.toml` is committed. |
| `check-fixtures.sh` | Verifies `fixtures/MANIFEST.sha256` both ways: every listed file still hashes correctly, and no unlisted file is hiding under `fixtures/`. |
| `check-fixtures-selftest.sh` | Proves `check-fixtures.sh`'s refusals and passes against synthetic throwaway trees, so the gate is never trusted on the strength of only ever having been seen green. |
| `fixture-e2e.sh` | Runs the built `focusforge-cli` over the checked-in fixtures. Cargo-free on purpose, so `check-all.sh` can run it beside the cargo gates. |
| `tiff-diff.py A.tif B.tif [--tol N] [--zmap]` | The independent oracle for image outputs: compares two TIFFs pixel by pixel without going through this project's own reader or writer. `--tol` sets the allowed per-channel difference, `--zmap` compares `u16` source-index maps instead of RGB. |
| `make-fixtures.sh [--check] [--stack DIR]` | Cuts the committed `fixtures/` trees out of the reference stack named by `FOCUSFORGE_REFERENCE_STACK` (or `--stack`), regenerates their READMEs and `fixtures/MANIFEST.sha256`. `--check` verifies the committed trees match byte for byte without writing anything; SKIPs when the variable is unset. |
| `lib/rawtiff.py` | The stdlib-only TIFF reader/writer `tiff-diff.py` and `make-fixtures.sh` share: mmap-based, handles both byte orders and any strip height so it can open the reference stack's result file as well as the source frames. |
| `lib/make_fixtures.py` | The cropping and README/manifest generation `make-fixtures.sh` drives. |
| `synth-align.sh` | Renders a synthetic focus stack with a KNOWN per-frame misregistration and runs `focusforge-cli align --json` over it twice, writing `.scratch/synth-align/measurement.json` -- the artefact `crates/cli/tests/synthetic_ground_truth.rs` asserts against. Needs numpy. |
| `run-acceptance.sh [--limit SIZE] [--algo ID]` | The manual reference-stack run — minutes, so never in a gate. Reads the stack's location from `FOCUSFORGE_REFERENCE_STACK`, SKIPs when unset, and builds a 61-entry symlink farm from it (never reads the top level directly — see the glob trap in the script's header), runs `stack --algo laplacian --set levels=5` through `target/release/focusforge-cli` under `/usr/bin/time -v`, and with `--limit 8G` repeats it inside a `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0` cgroup, refusing unless the limit reads back enforced. `--algo ID` (default `laplacian`) swaps the stacking algorithm; `--set levels=5` is emitted only for `laplacian` (`depth-map`'s schema has no `levels` parameter). Run each mode once and record the numbers. `ACCEPTANCE_TARGET_FRAMES` (default: the source count) repeats the 61 real frames up to that many farm entries (`r<replica>-<name>` symlinks), for a 300/600-frame CLI-scale run; `ACCEPTANCE_ALIGN=0` swaps the in-process/standalone `pyramidal-ecc` alignment for `--no-align`, for a leg that measures the scheduler, not alignment. |
| `lib/run-logged.sh` | The logging mechanism the gates share: each gate's full output to its own file, two bounded lines to stdout. |
| `run-logged.sh "<name>" -- <cmd>` | The same treatment for a one-off noisy command. |
| `make-logo-assets.sh` | Rasterises the brand SVGs in `assets/` into `assets/build/`: the PNG ladder, the hicolor tree, `focusforge.ico`, `focusforge.icns` and the repository page images. Picks the small-size cut at 32 px and under. Skips until `assets/logo.svg` exists. |
| `lib/write-icns.py` | Packs PNGs into a macOS `.icns` suite and reads its own output back to verify it. Exists because the ImageMagick build here has no ICNS coder. |
| `lib/make-wordmark.py` | Authoring step, not part of the build: sets the wordmark in Inter, bakes the text to paths and rewrites the four lockup SVGs in `assets/`. Run it by hand when the type or the proportions change. |
| `with-lock.sh <build\|gpu> -- <command> [args...]` | Serializes a cargo invocation, a direct `target/{debug,release}/focusforge-cli` run, or any GPU-touching command against other builds and GPU runs on this machine — see `lib/lock.sh` for what each lock guards. `check-all.sh` and `run-acceptance.sh` already route every cargo/GPU call through it. Refuses instantly (exit 3) if wrapped around a script that already takes the same lock itself instead of hanging forever; any other held-but-wedged lock times out after `FF_LOCK_TIMEOUT` (default 1800s, exit 4) rather than blocking without end. |
| `lib/lock.sh` | The build/gpu flock pair `with-lock.sh` fronts — sourced directly by a script that wants `with_lock` itself rather than shelling out. Lock ownership is a real fd (`exec {fd}>lock; flock $fd`) with the holder's pid recorded alongside it, so a nested `with_lock` call whose own ancestor already holds the lock is recognised and refused rather than deadlocking silently. |

Every script skips rather than fails when the thing it checks does not exist
yet — an empty repository is not a broken one. A skip prints `SKIP: <reason>`
and exits 0.
