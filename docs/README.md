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
  choosing an algorithm, and getting the Z-Map

## Reference

- [`reference/cli.md`](reference/cli.md) — every `focusforge-cli` subcommand
  and flag, and the `info --json`, `algorithms --json` and `stack --json`
  schemas
- [`reference/tiff-fast-path.md`](reference/tiff-fast-path.md) — the TIFF
  layout the reader accepts and the writer emits

Building from source, the repository layout and the test suite are in
[`../CONTRIBUTING.md`](../CONTRIBUTING.md), not here — this tree is for using
FocusForge, not developing it.
