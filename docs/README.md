# Documentation

Product documentation for FocusForge: how to use it, and how its pieces fit
together. Design rationale — what was chosen, what was rejected and why — is
not here.

Pages are added as the features they describe are built, so this index is
short on purpose rather than incomplete.

## Guide

Using FocusForge: shooting a stack, loading it, choosing an algorithm,
retouching the result.

- [`guide/stacking.md`](guide/stacking.md) — running `focusforge-cli stack`,
  choosing an algorithm, getting an image back with `export`, and getting the
  Z-Map
- [`guide/alignment.md`](guide/alignment.md) — how `focusforge-cli
  align`/`stack` register frames before fusing, choosing a mode, and
  checking a result before committing to a full run
- [`guide/batch-runs.md`](guide/batch-runs.md) — stacking a folder that holds
  more than one stack: the grouping proposal and overriding it, presets,
  re-running one stack, cancelling and resuming a run, the stage cache, and
  reviewing the results
- [`guide/diagnostics.md`](guide/diagnostics.md) — the log every run keeps,
  what a crash leaves behind, the notice on the next start, and packaging a
  diagnostics bundle

## Reference

- [`reference/cli.md`](reference/cli.md) — every `focusforge-cli` subcommand
  and flag, and the `info --json`, `algorithms --json`, `stack --json`,
  `export --json` and `review --json` schemas
- [`reference/tiff-fast-path.md`](reference/tiff-fast-path.md) — the TIFF
  layout the reader accepts and the writer emits
- [`reference/project-format.md`](reference/project-format.md) — the
  `.ffproj` project container `stack` writes and `export`/`review` read

Building from source, the repository layout and the test suite are in
[`../CONTRIBUTING.md`](../CONTRIBUTING.md), not here — this tree is for using
FocusForge, not developing it.
