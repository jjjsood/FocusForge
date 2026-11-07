# Batch runs from the command line

This page is for working in a terminal: stacking a whole folder of sessions in
one go with `focusforge-cli stack`, keeping parameter sets as presets,
re-running one stack, stopping and resuming a run, and reviewing what a batch
produced. Every flag named here is listed in full in the
[command-line reference](cli.md#stack).

If you work in the window, you do not need any of this: the window asks how to
split a folder that holds several stacks and opens one project per stack. See
[A folder with several stacks](../guide/first-stack.md#a-folder-with-several-stacks)
and [Projects, notifications and quitting](../guide/batch-runs.md).

## Stacking a folder of many stacks

A folder can hold more than one focus-bracketed stack — several sessions
dropped into one directory, for example. `focusforge-cli stack` detects the
split and prints it before doing anything else:

```bash
focusforge-cli stack path/to/many-sessions/ --algorithm laplacian -o projects/
```

Grouping reads folder names and file headers only, never pixels, and it never
decides silently:

- **Folders come first.** Every folder that holds frames is a stack of its own,
  so a tree you sorted by hand is never re-joined or re-cut across folders.
- **Capture time splits within one folder.** Inside a single folder, a gap of
  more than one minute between two frames' capture times starts a new stack —
  the case of a whole memory card dumped into one folder. The capture time is
  the EXIF `DateTimeOriginal` tag (36867) of a TIFF, or the capture time a RAW
  file records. The TIFF `DateTime` tag (306) is never used: an editor such as
  darktable rewrites it with the export time.
- **Camera RAW and TIFF files in one folder** always form separate stacks.

The reason for each split is named on the grouping line, and each group lists
the signal that made it. A folder with two subfolders `a/` and `b/` of three
frames each, carrying no capture time, prints:

```text
grouping many-sessions: 2 groups (directory structure (no frame carries a readable capture time))
  a  3 frames  00.tif .. 02.tif  [directory]
  b  3 frames  00.tif .. 02.tif  [directory]
```

`-o`/`--output` collects every project this run produces into one directory;
omit it to write each project beside its own frames.

## Overriding the proposal

```bash
focusforge-cli stack path/to/many-sessions/ --algorithm laplacian --write-groups groups.toml --dry-run
```

Writes the proposal to `groups.toml` in a plain TOML format — one
`[[group]]` block per stack, `name` plus a `frames` array of paths relative
to the directory the run was pointed at:

```toml
[[group]]
name = "session-a"
frames = ["IMG_9801.tif", "IMG_9802.tif"]
```

Edit it — rename a group, move a frame from one group to another, drop a
frame from every group — and feed it back with `--groups`:

```bash
focusforge-cli stack path/to/many-sessions/ --algorithm laplacian --groups groups.toml -o projects/
```

Every frame under the directory must appear in exactly one group; a listed
frame the walk didn't find, a frame in two groups, or a frame no group lists
is refused by name before anything is read.

> **Note:** `--dry-run` computes nothing and writes no project and no
> manifest.

## Status records and resource limits

A batch on a shared machine often needs to be watched and kept small. These
`stack` flags are described in [`reference/cli.md`](cli.md#stack):

- `--status <PATH>` appends one NDJSON status record for each stack to this
  file, which is what a script polls.
- `--tile-size <PX>` overrides the scheduler's outer tile size, in pixels.
- `--max-host-memory <SIZE>` and `--max-vram <SIZE>` are ceilings on the host
  and the device (VRAM) memory budgets. A ceiling only lowers a budget,
  never raises it. Sizes take a unit, for example `8GiB` or `512MiB`.

## Re-running one stack

`--only <name>` restricts a run to the named group(s) — matched by group
name or by the identity `review --json` and `--status` report — skipping
every other group without touching its project on disk:

```bash
focusforge-cli stack path/to/many-sessions/ --algorithm laplacian --set levels=6 --only session-a -o projects/
```

This is the cheap way to retune one stack after a batch run: change a
`--set`/`--align-set` value, add `--only <name>`, and only that stack
re-fuses. A stage cache (below) keeps the re-run from repeating alignment it
already computed.

## Presets

A preset is a TOML file bundling the algorithm choice and its parameter
values, read with `--preset` instead of retyped as `--set` flags:

```toml
algo = "laplacian"
align-algo = "pyramidal-ecc"

[stack]
levels = 5
align = true
equalize = true

[align]
max-iterations = 40
```

```bash
focusforge-cli stack path/to/frames/ --preset macro.toml
focusforge-cli stack path/to/frames/ --algorithm laplacian --write-preset macro.toml
```

`--write-preset` writes the preset that reproduces the run just made — the
resolved algorithm ids and explicit values actually chosen. Values still left
to an `auto` closure remain live rather than being frozen. A flag on the
command line always beats the file:
`--set`/`--align-set` override the file's tables, `--algorithm`/`--align-algorithm`
override its keys. The `[stack]` table also carries the project settings (`align`, `auto-crop`,
`equalize`, `equalize-max-gain`, `equalize-target`); `align = false` in a preset fuses the frames
as read. `focusforge-cli align` reads the same file, using only
its `align-algo` key and `[align]` table.

## Pausing, cancelling and resuming

Ctrl-C during a run stops it cleanly: work already committed to a project
stays, and the run exits 130. Passing `--resume` on a later invocation of
the same stack picks the cancelled run back up from where it left off,
instead of starting over:

```bash
focusforge-cli stack path/to/frames/ --algorithm laplacian --resume
```

`--resume` matches the earlier run by a fingerprint over the stack's inputs;
it refuses, rather than guessing, when no matching cancelled state exists or
it disagrees with this invocation.

On Unix, sending `SIGUSR1` to a running `stack` process pauses it in place —
no `--resume` needed, since the process itself is still alive: `kill -USR1
<pid>` toggles the pause, and the run's own progress output names the way
back (`paused -- send SIGUSR1 to resume (Ctrl-C to cancel)`). SIGUSR1 has no
Windows equivalent, so pausing is Unix-only there.

## The stage cache

A run's alignment result is kept in a stage cache under
`.scratch/stage-cache/`, keyed by the frames and alignment parameters that
produced it, so a re-run that changes only a fusion parameter (`--only`
plus `--set`) skips a costly re-solve. The frames are told apart by their
content: a stack whose files have the same content hits even when moved,
renamed, copied elsewhere or touched, while frames that were reshot or
rewritten (even with the same size and settings) are aligned afresh.
`--no-cache` skips the cache
entirely for one run, reading nothing from it and leaving nothing behind;
`focusforge-cli cache clear` empties it without touching any project or a
`--resume` run's own recoverable state.

## Reviewing the results

```bash
focusforge-cli review projects/
```

Every `stack` run except a `--dry-run` writes a manifest: `manifest.json` in the `-o` directory,
or, without `-o`, `<folder>-run.json` in the folder the run was pointed at.
`review` reads a run's manifest (or, given a plain directory of projects, no
manifest, the `.ffp` files it finds) and reports one record per stack:
outcome, the sharpness ratio and Z-Map coherence a successful project
recorded (`--` when absent), and the error a failed one carries — a failed
stack in a batch is a visible record here, not a missing file. `review`
never groups frames and writes nothing itself; see
[the `review` reference](cli.md#review) for `--json` and the full
field list.

## Turning a project into an image

`stack` prepares projects, it never writes an image file — run
`focusforge-cli export <project>.ffp -o out.tif --zmap zmap.tif` per
project once you know which ones you want, as described in
[Getting an image back](../guide/stacking.md#getting-an-image-back) and in
the [`export` reference](cli.md#export). In the window, the same step is
[Exporting an image](../guide/export.md). The Z-Map that `--zmap` writes is
the fusion's own: retouch strokes are not included in it.
