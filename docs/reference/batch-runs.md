# Batch runs from the command line

The `focusforge-cli stack` options for a folder that holds several stacks, with
the file formats they read and write. Every option is listed in the
[command-line reference](cli.md#stack). In the window the same work is
[Projects, notifications and quitting](../guide/batch-runs.md).

## Splitting a folder into stacks

`stack` prints the split it proposes before it starts: every folder with
frames is a stack of its own, a gap of more than one minute between capture
times starts a new stack inside a folder, and camera RAW and TIFF files never
share a stack.

```bash
focusforge-cli stack sessions/ --algorithm laplacian --write-groups groups.toml --dry-run
```

`--write-groups` writes the proposal as a TOML file, one block per stack, with
frame paths relative to the folder you passed:

```toml
[[group]]
name = "session-a"
frames = ["IMG_9801.tif", "IMG_9802.tif"]
```

Edit it and run again with `--groups groups.toml`. Every frame must be in
exactly one group; a frame that is missing, listed twice or not listed is
refused by name before anything is read. `-o projects/` collects all projects
in one folder.

## Running one stack again

`--only <name>` runs just the named stack; the others and their projects are
left alone.

```bash
focusforge-cli stack sessions/ --algorithm laplacian --set levels=6 --only session-a -o projects/
```

## Presets

A preset is a TOML file with the methods and settings you chose, read with
`--preset`. A flag on the command line wins over the file.

```toml
algo = "laplacian"
align-algo = "pyramidal-ecc"

[stack]
levels = 5
equalize = true

[align]
max-iterations = 40
```

```bash
focusforge-cli stack frames/ --preset macro.toml
focusforge-cli stack frames/ --algorithm laplacian --write-preset macro.toml
```

`--write-preset` saves the settings of the run just made. `align` reads the
same file and uses only `align-algo` and `[align]`.

## Pausing and resuming

Ctrl-C stops a run cleanly and the run exits `130`; finished work stays.
`--resume` on the same command picks it up again:

```bash
focusforge-cli stack frames/ --algorithm laplacian --resume
```

## Saved alignment results

<a id="the-stage-cache"></a>

A `stack` run keeps its alignment result so that a re-run which changes only a
fusion setting does not align again. `--no-cache` skips this for one run, and
`focusforge-cli cache clear` empties it without touching a project or a
cancelled run.

## Reviewing the results

Every `stack` run except a `--dry-run` writes a run record: `manifest.json` in
the `-o` folder, or `<folder>-run.json` in the folder you passed when there is
no `-o`.

```bash
focusforge-cli review projects/
```

`review` lists one line per stack, a failed one included. See
[`review`](cli.md#review).

## Writing an image

`stack` writes projects, never images. Write each one you want with
`focusforge-cli export projects/session-a.ffp -o out.tif`; see
[`export`](cli.md#export).
