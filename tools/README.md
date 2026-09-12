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
| `run-acceptance.sh` | The manual 16 GB reference-stack run — minutes, so never in a gate. Run it once per item that needs it and record the numbers. Refuses until the CLI has a `stack` subcommand. |
| `lib/run-logged.sh` | The logging mechanism the gates share: each gate's full output to its own file, two bounded lines to stdout. |
| `run-logged.sh "<name>" -- <cmd>` | The same treatment for a one-off noisy command. |

Every script skips rather than fails when the thing it checks does not exist
yet — an empty repository is not a broken one. A skip prints `SKIP: <reason>`
and exits 0.
