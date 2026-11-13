# Command-line reference

`focusforge-cli` does from a terminal everything the window does with a
stack. This page lists every command, its options, the settings you can pass
with `--set`, the exit codes and example invocations. Installing the command
line and its shell completion is covered in
[For the command line](../getting-started.md#for-the-command-line); photographers
working in the window can start with [Your first stack](../guide/first-stack.md).

```
focusforge-cli [OPTIONS] <COMMAND>
```

| command | does |
|---|---|
| [`stack`](#stack) | fuses the frames of a folder into one project per stack |
| [`align`](#align) | reports how each frame has to move to line up, without fusing |
| [`export`](#export) | writes a project's image, its source map or a single frame to a file |
| [`retouch`](#retouch) | adds brush strokes from a list file to a project as new checkpoints |
| [`history`](#history) | lists, deletes and compacts a project's checkpoints |
| [`info`](#info) | prints what a project, a frame or a folder of frames contains |
| [`review`](#review) | reports the outcome of every stack in a folder of projects |
| [`algorithms`](#algorithms) | lists the stacking and alignment methods and their settings |
| [`step-plan`](#step-plan) | plans a focus bracket: step, frame count, duration, disk need |
| [`sharp-areas`](#sharp-areas) | counts the sharpest areas per frame |
| [`cache`](#cache) | clears the saved alignment results |
| [`diagnostics`](#diagnostics) | packs logs and crash reports into one ZIP |
| [`check-update`](#check-update) | tells you whether a newer release exists |

`focusforge-cli <command> --help` prints the same options on the terminal.

## Options every command takes

| option | meaning |
|---|---|
| `-v` | more detail on stderr; repeat it for still more |
| `-q, --quiet` | errors only; wins over `-v` |
| `-h, --help` | print help |
| `-V, --version` | print the version |

Most commands also take `--json`, which prints the result as one JSON
document on stdout instead of readable text.

Every run also writes a log file in your user data folder, whatever `-v` and
`-q` say. If an earlier run crashed, the next run prints a one-line notice
pointing at [`diagnostics`](#diagnostics).

## Exit codes

| code | meaning |
|---|---|
| `0` | success |
| `1` | the command failed or refused to run; the reason is on stderr |
| `2` | wrong usage: unknown option or missing argument |
| `130` | a `stack` run was cancelled with Ctrl-C |

Commands that open a project (`export`, `history`, `info`, `retouch`) refuse a
damaged project by name, with these codes:

| code | meaning |
|---|---|
| `3` | not a FocusForge project |
| `4` | the file is truncated or incomplete |
| `6` | a part the project refers to is missing |
| `7` | a part of the project no longer matches its recorded check value |
| `8` | the checkpoints no longer link up, or the project does not match its recorded source frames |
| `9` | `history list` and `history delete`: a source frame has changed since the project was written; `--accept-changed-sources` goes ahead with a warning |

A project saved by a newer FocusForge, and a RAW project saved by an earlier
version (stack its frames again to open it), exit `1`. A source frame that
cannot be found only prints a warning.

Two more codes are specific: `info` exits `5` after printing a folder report
when some files could not be read, and `check-update` exits `3` when a newer
release exists.

## `stack`

```
focusforge-cli stack [OPTIONS] <DIR>
```

Fuses the TIFF, JPEG or camera RAW frames in `<DIR>` (or in `<DIR>/frames`
when that folder exists; subfolders are not searched) into one project per
stack. It writes projects, not images: use [`export`](#export) for the image.
A stack holds one kind of frame; TIFF, JPEG and RAW cannot be mixed.

Frames are aligned first and the result is trimmed to the area every frame
covers. When `<DIR>` holds several stacks, `stack` prints the proposed split
before it starts.

| option | meaning |
|---|---|
| `-o, --output <DIR>` | folder to collect the projects in; it must exist. Without it each project is written beside its frames |
| `--algorithm <ALGO>` | stacking method; required unless a preset names one. `focusforge-cli algorithms` lists them |
| `--set <KEY=VALUE>` | set a method setting or a [project setting](#project-settings); repeatable, the last value of a key wins |
| `--no-align` | do not align; frames are fused as they are |
| `--no-crop` | keep the full frame instead of trimming to the area all frames cover |
| `--strict` | refuse the run when a frame cannot be aligned. Without it such a frame is left where it is and reported |
| `--align-algorithm <ALGO>` | alignment method; the only one available is the default |
| `--align-set <KEY=VALUE>` | set an [alignment setting](#algorithms); repeatable |
| `--groups <PATH>` | split the frames into stacks as this TOML file says |
| `--write-groups <PATH>` | write the proposed split to this file, in the format `--groups` reads, and carry on |
| `--only <NAME>` | run only the named stack; repeatable. The other stacks and their projects are left alone |
| `--preset <PATH>` | read the method and settings from a preset file |
| `--write-preset <PATH>` | write a preset that reproduces this run |
| `--resume` | continue a run of the same stack that was cancelled |
| `--dry-run` | show what would happen and exit without computing or writing anything |
| `--json` | print the report as JSON |
| `--status <PATH>` | append one JSON line per stack to this file: its name, outcome, outputs and error |
| `--no-progress` | print no progress line |
| `--no-cache` | align afresh and do not store the result for later runs |
| `--cpu` | use the processor only, not the graphics card (only in builds with graphics-card support) |
| `--max-host-memory <SIZE>` | upper limit on the RAM the run may use, for example `8GiB` or `512MiB`; it can only lower what FocusForge would use |
| `--max-vram <SIZE>` | the same limit for graphics-card memory (only in builds with graphics-card support) |
| `--tile-size <PX>` | work on the image in blocks of this many pixels; the default suits almost every stack |
| `--trace-accounting` | add extra counters to the `--json` report; costs a little time |

An unknown method, a setting outside its range and a missing `-o` folder are
refused before any frame is read. With alignment and trimming on and no area
common to all frames, `stack` stops and names `--no-crop`.

### Project settings

`--set`, and a preset's `[stack]` table, also take these settings of the whole
run:

| id | values | default | meaning |
|---|---|---|---|
| `align` | `true` / `false` | `true` | align the frames before fusing; `--no-align` is the same as `align=false` |
| `auto-crop` | `true` / `false` | `true` | trim the result to the area every frame covers; `--no-crop` is the same as `auto-crop=false` |
| `equalize` | `true` / `false` | `false` | give each frame one brightness gain so that all frames reach the same average |
| `equalize-max-gain` | 1.01 to 2.0 | `1.25` | the largest gain a frame may get |
| `equalize-target` | -0.5 to 0.5 | `0.0` | the shared brightness target in stops from the frames' average |

### Cancel, pause, resume

Ctrl-C stops the run cleanly and prints the command that resumes it; the
process then exits `130`. A second Ctrl-C ends it at once. On Linux and macOS,
`kill -USR1 <pid>` pauses a running `stack` and a second one continues it.
`--resume` continues the cancelled run of the same stack, and refuses when
there is nothing to resume or the inputs differ.

### Dry run

`--dry-run` prints the resolved method and settings, the alignment settings,
the projects it would write and a predicted duration, then exits `0`:

```
dry run -- resolved only, nothing computed and nothing written
...
would write: .scratch/out/mantis-64.ffp
predicted duration: 12.3 s (estimate for the reference machine, cold run)
crop: not computed -- --dry-run stops before alignment
output size: not computed -- it follows from the crop
```

The prediction is for a reference machine and a first run. For an input
FocusForge cannot predict well it reads `predicted duration: ~N s (not
calibrated for this input)`, and `predicted duration: unknown` when there is
nothing to estimate. `--dry-run` cannot be combined with `--resume`.

### Examples

```bash
focusforge-cli stack frames/ --algorithm laplacian
focusforge-cli stack frames/ --algorithm weighted-average --set radius=8 -o projects/
focusforge-cli stack sessions/ --algorithm laplacian --write-groups groups.toml --dry-run
focusforge-cli stack frames/ --preset macro.toml --set equalize=true
```

Grouping, presets, re-running one stack and reviewing a batch are on
[Batch runs from the command line](batch-runs.md). When a frame cannot be
aligned, see [A frame that does not line up](../guide/alignment.md#a-frame-that-does-not-line-up).

## `align`

```
focusforge-cli align [OPTIONS] <DIR>
```

Reports, for each frame of `<DIR>`, how far it has to move, scale and turn to
line up with the reference frame, and the area that would be kept. Nothing is
fused.

| option | meaning |
|---|---|
| `--algorithm <ALGO>` | alignment method; the only one available is the default |
| `--set <KEY=VALUE>` | set an [alignment setting](#algorithms); repeatable |
| `--strict` | refuse the run when a frame cannot be aligned |
| `--write-warped <DIR>` | write each frame as moved into this folder, uncropped, for a visual check |
| `--preset <PATH>` | read the alignment method and settings from a preset file |
| `--write-preset <PATH>` | write a preset that reproduces this run |
| `--status <PATH>` | append one JSON line for this run to this file |
| `--json` | print the report as JSON |
| `--no-progress` | print no progress line |
| `--cpu` | as for [`stack`](#stack) |
| `--max-host-memory <SIZE>` | as for [`stack`](#stack) |
| `--max-vram <SIZE>` | as for [`stack`](#stack) |

```bash
focusforge-cli align frames/
focusforge-cli align frames/ --write-warped .scratch/warped/
```

Each frame's row shows its shift (`tx`, `ty`), `scale`, rotation (`rot`), a
status and a match value `rho` between -1 and 1 (higher is better). The
reference frame is marked `(reference)` and a frame left in place is marked
`[fallback]`. The last line is the area that `stack` would keep.

## `export`

```
focusforge-cli export [OPTIONS] <PROJECT>
```

Writes an image from a project. It is the only command that writes an image.
Give at least one of `-o`, `--zmap` and `--frame`. The output must not be the
project file itself.

| option | meaning |
|---|---|
| `-o, --output <FILE>` | write the image to this file |
| `--zmap <FILE>` | write the source map (which frame each pixel came from) as a TIFF; retouching is not included |
| `--checkpoint <ID>` | the checkpoint to write: an id or a unique start of one; the latest by default |
| `--format <FORMAT>` | `tiff`, `png`, `jpeg` or `dng` (a linear DNG, for RAW projects). Taken from the extension of `-o` when omitted; no extension means TIFF |
| `--tiff-bits <BITS>` | `16` (default) or `8` |
| `--tiff-compression <NAME>` | `none` (default), `lzw` or `deflate` |
| `--png-bits <BITS>` | `16` (default) or `8` |
| `--png-compression <NAME>` | `fast`, `default` (default) or `best` |
| `--jpeg-quality <N>` | 1 to 100, default `95` |
| `--jpeg-subsampling <SPEC>` | `420` (default) or `444` |
| `--exif <on\|off>` | carry over the camera metadata; default `on` |
| `--icc <on\|off>` | embed the colour profile; default `on` |
| `--xmp <on\|off>` | carry over the XMP data; default `off` |
| `--iptc <on\|off>` | carry over the IPTC data; default `off`; never written to PNG |
| `--frame <INDEX>` | write source frame number `INDEX` (counting from 0) as it was fused: aligned and trimmed like the result. Needs `--frame-output` |
| `--frame-output <FILE>` | where to write the frame `--frame` names |
| `--json` | print the report as JSON |

An option for another format than the one written (`--png-bits` with a JPEG)
is refused by name, and so is a value out of range. PNG and JPEG hold grey or
RGB images only.

A retouched checkpoint needs its source frames; when one is missing or has
changed since the project was written, the export refuses and names it, and
nothing is written. `--zmap` alone reads no source frame.

```bash
focusforge-cli export mantis-64.ffp -o out.jpg --jpeg-quality 90
focusforge-cli export mantis-64.ffp -o out.tif --tiff-compression lzw --zmap zmap.tif
focusforge-cli export mantis-64.ffp --checkpoint 5b71 -o out-5b71.png
```

## `retouch`

```
focusforge-cli retouch [OPTIONS] --ops <FILE> <PROJECT>
```

Adds a list of brush strokes to a project. Each stroke becomes a new
checkpoint, each the child of the previous one, under the latest checkpoint or
under `--from`. See [Retouching](../guide/retouch.md) for the stroke list.
Write the result with [`export --checkpoint`](#export).

| option | meaning |
|---|---|
| `--ops <FILE>` | the list of strokes, a JSON file; required. Every stroke is checked before anything is written, and an invalid one is named by its position counting from 0 |
| `--from <ID>` | the checkpoint to add under: an id or a unique start of one; the latest by default |
| `--name <NAME>` | name the last checkpoint added |
| `--json` | print the report as JSON |

```bash
focusforge-cli retouch frames.ffp --ops stroke-list.json --name "halo fixed"
```

## `history`

```
focusforge-cli history [OPTIONS] <COMMAND>
```

Reads and changes a project's checkpoints without opening the window. A
checkpoint is given by its id or a unique start of it; an ambiguous start is
refused and names every match.

| option | meaning |
|---|---|
| `-h, --help` | print help |

| subcommand | does |
|---|---|
| [`list`](#history-list) | prints every visible checkpoint, newest first |
| [`delete`](#history-delete) | hides a checkpoint, and removes it from the file once no visible checkpoint builds on it. The last visible checkpoint cannot be deleted |
| [`compact`](#history-compact) | rewrites the project to what is still visible and reports the space freed |

### `history list`

```
focusforge-cli history list [OPTIONS] <PROJECT>
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |
| `--accept-changed-sources` | go ahead with a warning when a source frame has changed since the project was written (see exit code `9`) |

### `history delete`

```
focusforge-cli history delete [OPTIONS] --checkpoint <CHECKPOINT> <PROJECT>
```

| option | meaning |
|---|---|
| `--checkpoint <CHECKPOINT>` | the checkpoint to delete: an id or a unique start of one |
| `--json` | print the result as JSON |
| `--accept-changed-sources` | go ahead with a warning when a source frame has changed since the project was written (see exit code `9`) |

### `history compact`

```
focusforge-cli history compact [OPTIONS] <PROJECT>
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |

### Examples

```bash
focusforge-cli history list mantis-64.ffp
focusforge-cli history delete mantis-64.ffp --checkpoint 5b71
focusforge-cli history compact mantis-64.ffp
```

## `info`

```
focusforge-cli info [OPTIONS] <PATH>
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |

Prints what is stored in a project file, a TIFF, JPEG or RAW frame, or every
frame directly inside a folder (or its `frames` subfolder). Only file headers
are read, so it answers at once even for large stacks. `--json` prints the
report as JSON.

For a folder, the report ends with the number of frames, their total size and
whether all frames share one layout; if not, it names the odd one out. A file
that cannot be read is reported with its reason; the rest of the folder is
still listed and the command exits `5`. A single file that cannot be read
exits `1`; a folder without frames also exits `1`.

```bash
focusforge-cli info frames/
focusforge-cli info mantis-64.ffp
```

## `review`

```
focusforge-cli review [OPTIONS] <DIR>
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |

Prints one line per stack found in `<DIR>`: the output folder of a `stack` run
or a plain folder of `.ffp` projects. Each line has the stack's name, its
outcome, the sharpness ratio, the number of source frames and a coherence
value of the source map; `--` stands for a value that is not available, as for
a failed stack, whose error is printed instead. A failed stack is a visible
line, not a missing file. Nothing is written and no frame is read. `--json`
prints the report as JSON.

```bash
focusforge-cli review projects/
```

## `algorithms`

```
focusforge-cli algorithms [OPTIONS] [ID]
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |
| `--set <KEY=VALUE>` | check a setting's value against its range; repeatable, the last value of a key wins |

Without `ID` it lists the methods and the project settings. With an `ID` it
prints that method's settings with range and default. `--json` prints JSON.
`--set <KEY=VALUE>` checks a value against the method's range and names every
value that is out of range.

The window's names for the methods:

| id | window name |
|---|---|
| `laplacian` | Details |
| `weighted-average` | Soft transitions |
| `depth-map` | Focus map |

`pyramidal-ecc` is the alignment method.

`algorithms` also lists test-only methods whose ids start with `dummy-`; they
are not for photographs.

Settings, for `stack --set` and `stack --align-set` (also in a preset's
`[stack]` and `[align]` tables):

| method | id | values | default |
|---|---|---|---|
| `laplacian` | `levels` | 1 to 16 | chosen from the image size |
| `weighted-average` | `radius` | 0 to 16 | `16` |
| `weighted-average` | `weight-exponent` | 1 to 8 | `4.0` |
| `depth-map` | `score-radius` | 0 to 16 | `4` |
| `depth-map` | `smoothing-radius` | 0 to 16 | `8` |
| `pyramidal-ecc` | `mode` | `translation`, `rotation-scale` | `rotation-scale` |
| `pyramidal-ecc` | `reference-frame` | `first`, `middle`, `last` | `middle` |
| `pyramidal-ecc` | `reference-policy` | `anchor`, `chain` | `chain` |
| `pyramidal-ecc` | `max-iterations` | 1 to 1000 | `400` |
| `pyramidal-ecc` | `epsilon-px` | 0.000000001 to 1 | `0.0001` |
| `pyramidal-ecc` | `pyramid-depth` | 0 to 16 | chosen from the image size |

```bash
focusforge-cli algorithms
focusforge-cli algorithms laplacian
focusforge-cli algorithms weighted-average --set weight-exponent=9
```

The last command reports the value as out of range.

## `step-plan`

```
focusforge-cli step-plan [OPTIONS]
```

Plans a focus bracket from the optics and the depth range you want to cover.

| option | meaning |
|---|---|
| `--set <KEY=VALUE>` | set one of the values below; repeatable, the last value of a key wins |
| `--json` | print the plan as one JSON object |

| id | meaning | default | allowed |
|---|---|---|---|
| `optics` | `lens` or `objective` | `lens` | `lens`, `objective` |
| `magnification` | magnification (x) | `1` | 0.01 to 100 |
| `aperture` | marked f-stop, for a lens | `8` | 0.7 to 64 |
| `na` | numerical aperture, for an objective | `0.25` | 0.01 to 0.95 |
| `overlap` | overlap, as a fraction of the depth of field | `0.25` | 0 to 0.9 |
| `range_mm` | depth range (mm); `0` plans one frame | `5` | 0 to 1000 |
| `frame_seconds` | time per frame (s) | `1` | 0 to 3600 |
| `frame_mb` | size per frame (MB) | `50` | 0 to 2000 |

A value the chosen optics does not use is ignored. A value out of range or an
unknown id is reported on stderr, one line each, and the command exits `1`.

```
$ focusforge-cli step-plan --set magnification=1 --set aperture=8 --set frame_seconds=3
optics:    lens, f/8
NA:        0.031
DOF:       563.2 µm
step:      422.4 µm (422400 nm)
frames:    13
duration:  39 s
disk:      650 MB
```

## `sharp-areas`

```
focusforge-cli sharp-areas [OPTIONS] <DIR>
```

Divides the image into square cells and counts, per frame, in how many cells
that frame is the sharpest. Frames are counted as they are, without
alignment. The text output has one row per frame: index, file name, count.

| option | meaning |
|---|---|
| `--set <KEY=VALUE>` | set one of the values below; repeatable |
| `--json` | print the counts as one JSON object |
| `--priority <low\|normal>` | `low` reads one frame at a time and leaves the other cores free; the counts are the same. Default `normal` |

| id | meaning | default | allowed |
|---|---|---|---|
| `cell-size` | side of a cell (px) | `64` | 16 to 512 |
| `contrast` | how much sharper the peak must be than the cell's weakest frame | `2` | 1 to 10 |

A value out of range, a folder without frames, a frame that cannot be read and
frames of different sizes all exit `1`.

```bash
focusforge-cli sharp-areas frames/ --set cell-size=128
```

## `cache`

```
focusforge-cli cache [OPTIONS] <COMMAND>
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |

| subcommand | does |
|---|---|
| [`clear`](#cache-clear) | removes the saved alignment results |

### `cache clear`

```
focusforge-cli cache clear [OPTIONS]
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |

Removes the saved alignment results under `.scratch/stage-cache/`. It never
touches a cancelled run that `--resume` can continue, or any project. With
`--json` it prints `{"removed": <count>}`.

```
$ focusforge-cli cache clear
removed 1 stage cache entry
```

## `diagnostics`

```
focusforge-cli diagnostics [OPTIONS]
```

Packs this installation's logs, crash reports, a system summary, the graphics
adapter, the effective configuration and the version into one ZIP. The file
list is printed before the ZIP is written. The home folder and the user name
are removed from every text entry, no image data is included and nothing is
sent anywhere. See [Diagnostics and crash reports](../guide/diagnostics.md).

| option | meaning |
|---|---|
| `-o, --output <FILE>` | where to write the ZIP; without it a dated name in the current folder |
| `--include-minidumps` | include the crash dump files; they cannot be redacted and are marked in the file list |
| `--force` | overwrite the output file if it exists |

The command exits non-zero when the output file exists and `--force` is not
given, or when the ZIP cannot be written.

```bash
focusforge-cli diagnostics -o report.zip
```

## `check-update`

```
focusforge-cli check-update [OPTIONS]
```

| option | meaning |
|---|---|
| `--json` | print the result as JSON |

Asks the release server once whether a newer final release exists. The request
carries only the program version; nothing is downloaded or installed.

| code | meaning |
|---|---|
| `0` | this version is up to date |
| `3` | a newer release exists; its address is printed |
| `1` | the check failed (no network, a timeout, or an unusable answer); the reason is printed on stderr |

```
$ focusforge-cli check-update
FocusForge 0.1.0 is up to date
```

`--json` prints one document with an `outcome` of `up-to-date`, `newer` (with
`version` and `url`) or `failed` (with `reason`).
