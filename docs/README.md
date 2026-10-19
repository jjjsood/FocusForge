# Documentation

Product documentation for FocusForge: how to use it, and how its pieces fit
together. Design rationale — what was chosen, what was rejected and why — is
not here.

## Guide

Using FocusForge: shooting a stack, loading it, choosing an algorithm,
retouching the result.

- [`getting-started.md`](getting-started.md) — release artifacts when a
  matching release provides them, or the source-build fallback when they do not

- [`guide/gui.md`](guide/gui.md) — opening a folder or a project in the
  window, the dock's `Images` panel and the four-section `Settings` panel, the five-tab settings window, running and controlling
  a fuse, saving the held result, exporting an image, switching between open
  projects and the settings file
- [`guide/stacking.md`](guide/stacking.md) — stacking TIFF or supported RAW
  frames, choosing a fusion method, brightness equalization, getting a TIFF, PNG or JPEG image back with
  `export` and its format options, and getting the Z-Map
- [`guide/alignment.md`](guide/alignment.md) — how `focusforge-cli
  align`/`stack` register frames before fusing, choosing a mode, and
  checking a result before committing to a full run
- [`guide/retouch.md`](guide/retouch.md) — retouching a result with soft
  brush strokes, from the op list `focusforge-cli retouch` appends to the
  window's brush, pipette, overlay and undo/redo
- [`guide/history.md`](guide/history.md) — a project's checkpoint tree: the
  window's checkpoint list, saving, restoring, branching and deleting, the
  same from `focusforge-cli history`, and what a damaged or moved project
  does
- [`guide/batch-runs.md`](guide/batch-runs.md) — stacking a folder that holds
  more than one stack: the grouping proposal and overriding it, presets,
  re-running one stack, cancelling and resuming a run, the stage cache, and
  reviewing the results
- [`guide/diagnostics.md`](guide/diagnostics.md) — the log every run keeps,
  what a crash leaves behind, the notice on the next start, and packaging a
  diagnostics bundle, and tracing the window

## Reference

- [`reference/cli.md`](reference/cli.md) — every `focusforge-cli` subcommand
  and flag, and the `info --json`, `algorithms --json`, `stack --json`,
  `export --json`, `retouch --json`, `history` and `review --json` schemas
- [`reference/tiff-fast-path.md`](reference/tiff-fast-path.md) — the TIFF
  layout the reader accepts and the writer emits
- [`reference/project-format.md`](reference/project-format.md) — the
  `.ffp` project container: checkpoints, stored planes, source frame checks
  and the refusals a damaged project gets

Building from source, the repository layout and the test suite are in
[`../CONTRIBUTING.md`](../CONTRIBUTING.md), not here — this tree is for using
FocusForge, not developing it.
