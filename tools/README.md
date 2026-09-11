# tools/

Scripts that check and drive **this** repository: gates, fixture generation
from the reference stack, and the shared build/GPU lock.

| script | what it does |
|---|---|
| `check-all.sh [--static] [--verbose] [--tail N] [--log-dir DIR]` | Every gate, one command, bounded output, pass/fail summary. `--static` is the pre-commit set. Skips cleanly until `Cargo.toml` exists. |
| `check-licenses.sh [--advisories]` | `cargo deny` over the GPLv3 compatibility allowlist; `--advisories` checks RUSTSEC instead. Skips until `deny.toml` is committed. |
| `check-fixtures.sh` | Verifies `fixtures/MANIFEST.sha256` both ways: every listed file still hashes correctly, and no unlisted file is hiding under `fixtures/`. |
| `fixture-e2e.sh` | Runs the built `focusforge-cli` over the checked-in fixtures. Cargo-free on purpose, so `check-all.sh` can run it beside the cargo gates. |
| `run-acceptance.sh [--mem-limit SIZE]` | The manual 16 GB reference-stack run. **Ask before running.** Refuses until the CLI has a `stack` subcommand. |
| `with-mem-limit.sh <cmd>` | Runs a command under a hard RAM cap (`FF_MEM_MAX`, default 32G, swap off) so a runaway job is killed alone. |
| `lib/run-logged.sh` | The logging mechanism the gates share: each gate's full output to its own file, two bounded lines to stdout. |
| `run-logged.sh "<name>" -- <cmd>` | The same treatment for a one-off noisy command. |

Every script skips rather than fails when the thing it checks does not exist
yet — an empty repository is not a broken one. A skip prints `SKIP: <reason>`
and exits 0.
