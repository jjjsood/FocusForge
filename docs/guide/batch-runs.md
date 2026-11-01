# Stacking a folder of many stacks

A folder can hold more than one focus-bracketed stack — several sessions
dropped into one directory, for example. `focusforge-cli stack` detects the
split and prints it before doing anything else:

```bash
focusforge-cli stack path/to/many-sessions/ --algorithm laplacian -o projects/
```

Grouping falls back through capture time (each frame's TIFF `DateTime` tag)
and directory structure — never silent, the reason it picked is always named
on the same line. `-o`/`--output` collects every project this run produces
into one directory; omit it to write each project beside its own frames.

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

`review` reads a run's manifest (or, given a plain directory of projects, no
manifest, the `.ffp` files it finds) and reports one record per stack:
outcome, the sharpness ratio and Z-Map coherence a successful project
recorded (`--` when absent), and the error a failed one carries — a failed
stack in a batch is a visible record here, not a missing file. `review`
never groups frames and writes nothing itself; see
[`reference/cli.md`](../reference/cli.md#review) for `--json` and the full
field list.

## Turning a project into an image

`stack` prepares projects, it never writes an image file — run
`focusforge-cli export <project>.ffp -o out.tif --zmap zmap.tif` per
project once you know which ones you want, as described in
[`guide/stacking.md`](stacking.md#getting-an-image-back).
