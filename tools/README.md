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
| `synth-align.sh` | Renders a synthetic focus stack with a KNOWN per-frame misregistration and runs `focusforge-cli align --json` over it twice, writing `.scratch/synth-align/measurement.json` -- the artefact `crates/cli/tests/synthetic_ground_truth.rs` asserts against. Needs numpy. Foreground in `check-all.sh`, before `cargo test`. Measured ~8s. |
| `run-acceptance.sh [--limit SIZE]` | The manual reference-stack run — minutes, so never in a gate. Reads the stack's location from `FOCUSFORGE_REFERENCE_STACK`, SKIPs when unset, and builds a 61-entry symlink farm from it (never reads the top level directly — see the glob trap in the script's header), runs `stack --algo laplacian --set levels=5` through `target/release/focusforge-cli` under `/usr/bin/time -v`, and with `--limit 8G` repeats it inside a `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0` cgroup, refusing unless the limit reads back enforced. Run each mode once and record the numbers. |
| `lib/run-logged.sh` | The logging mechanism the gates share: each gate's full output to its own file, two bounded lines to stdout. |
| `run-logged.sh "<name>" -- <cmd>` | The same treatment for a one-off noisy command. |
| `make-logo-assets.sh` | Rasterises the brand SVGs in `assets/` into `assets/build/`: the PNG ladder, the hicolor tree, `focusforge.ico`, `focusforge.icns` and the repository page images. Picks the small-size cut at 32 px and under. Skips until `assets/logo.svg` exists. |
| `lib/write-icns.py` | Packs PNGs into a macOS `.icns` suite and reads its own output back to verify it. Exists because the ImageMagick build here has no ICNS coder. |
| `lib/make-wordmark.py` | Authoring step, not part of the build: sets the wordmark in Inter, bakes the text to paths and rewrites the four lockup SVGs in `assets/`. Run it by hand when the type or the proportions change. |

Every script skips rather than fails when the thing it checks does not exist
yet — an empty repository is not a broken one. A skip prints `SKIP: <reason>`
and exits 0.
