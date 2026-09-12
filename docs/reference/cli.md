# CLI reference

`focusforge-cli` is the headless interface to FocusForge. Every algorithm is
reachable here before any GUI control exists.

```
focusforge-cli <COMMAND>
```

## `info`

```
focusforge-cli info [--json] <PATH>
```

Prints TIFF metadata for a single file, or for every TIFF directly inside a
directory (or inside a `frames/` subdirectory of it, when that exists).
Entries are matched by `.tif`/`.tiff` extension, case-insensitively, and are
not recursed into further. `info` reads headers only — it never opens a tile
or a pixel, which is why it can report on the 16 GB, 62-file reference stack
in well under a second.

`PATH` may be a single file or a directory:

- A single file that fails to parse as a TIFF is a hard error: `info` prints
  `error: ...` to stderr and exits 1.
- Inside a directory, a file that fails to parse is *not* fatal to the whole
  command: it is reported as one entry with an `error` field and counted in
  `unreadable`, while every readable sibling still gets a full report. A
  directory with zero matching files is a hard error.
- A file that parses but falls outside the fast read path (see
  [`tiff-fast-path.md`](tiff-fast-path.md)) is reported normally, with
  `mmap fast path: no` and the reason named — this is not an error.

### Human output

One block per file:

```
path/to/frame.tif
  64x64 3ch 16-bit II compression=1 planar=1 photometric=2
  rows_per_strip=1 strips=64 file_size=25338
  description: FocusForge fixture mantis-64/00 from 20260829-IMG_9800.tif rect 4064,2700,64,64
  mmap fast path: yes
```

The `description:` line is only printed when the file carries an
`ImageDescription` tag; a file with none keeps its three-line block.

For a directory, a trailing summary follows every file block:

```
3 TIFFs, 76014 bytes total
uniform layout: yes
```

When the frames disagree on layout, the summary names the odd one out and
what differs, for example `uniform layout: no — reference.tif is MM/rps=8, others II/rps=1`.
A directory report also prints `unreadable: N` when any file failed to
parse.

### `--json`

`--json` emits the same information as one JSON document instead. The shape
is the same whether `PATH` is a file or a directory — a single-file
invocation reports `"is_directory": false`, `"frame_count": 1` and a
one-element `"files"` array. This schema is a compatibility promise: a field
is added, never renamed or removed, in later versions.

```json
{
  "path": "fixtures/mantis-64",
  "is_directory": true,
  "frame_count": 3,
  "total_bytes": 76014,
  "unreadable_count": 0,
  "uniform_layout": true,
  "uniform_detail": null,
  "files": [
    {
      "name": "00.tif",
      "path": "fixtures/mantis-64/frames/00.tif",
      "width": 64,
      "height": 64,
      "bits_per_sample": [16, 16, 16],
      "samples_per_pixel": 3,
      "byte_order": "II",
      "compression": 1,
      "photometric": 2,
      "planar_config": 1,
      "rows_per_strip": 1,
      "strip_count": 64,
      "file_size": 25338,
      "description": "FocusForge fixture mantis-64/00 from 20260829-IMG_9800.tif rect 4064,2700,64,64",
      "mmap_fast_path": true,
      "refusal": null,
      "error": null
    }
  ]
}
```

Top-level fields:

| field | meaning |
|---|---|
| `path` | the path given on the command line |
| `is_directory` | whether `path` was a directory |
| `frame_count` | number of TIFFs found (files attempted, whether or not they parsed) |
| `total_bytes` | sum of on-disk file sizes across all of them |
| `unreadable_count` | how many failed to parse as a TIFF at all |
| `uniform_layout` | `true` when every parsed frame agrees on dimensions, bit depth, samples per pixel, byte order, `RowsPerStrip`, compression, planar configuration and photometric interpretation |
| `uniform_detail` | `null` when uniform, otherwise a sentence naming the outlier frame and what differs |
| `files` | one entry per file, in the order found |

Per-file fields (all `null` when `error` is set, meaning the file did not
parse as a TIFF at all):

| field | meaning |
|---|---|
| `name`, `path` | file name and full path |
| `width`, `height` | pixel dimensions |
| `bits_per_sample` | one entry per sample (channel) |
| `samples_per_pixel` | channel count |
| `byte_order` | `"II"` (little-endian) or `"MM"` (big-endian) |
| `compression` | the TIFF `Compression` tag value; `1` is uncompressed |
| `photometric` | the TIFF `PhotometricInterpretation` tag value |
| `planar_config` | the TIFF `PlanarConfiguration` tag value; `1` is chunky |
| `rows_per_strip` | the TIFF `RowsPerStrip` tag value |
| `strip_count` | number of strips implied by height and `rows_per_strip` |
| `file_size` | on-disk size in bytes |
| `description` | the TIFF `ImageDescription` tag, `null` when the file has none — a Z-Map carries its frame count here |
| `mmap_fast_path` | whether the tile reader's fast path applies to this file |
| `refusal` | `null` when `mmap_fast_path` is `true`, otherwise the reason it is not (compression, tiling, planar layout, bit depth or sample format) |
| `error` | `null` on a successful parse, otherwise why the file could not be read as a TIFF at all |

## `algorithms`

```
focusforge-cli algorithms [--json] [--set KEY=VALUE]... [ID]
```

Lists every algorithm registered with `focusforge-core`, or — given `ID` —
prints that one algorithm's full parameter schema. Every algorithm, stacking
or alignment, is registered under one shared id space and reachable through
this one subcommand; there is no separate listing per family.

Whether the listing has anything in it depends on how the binary was built:
the two algorithms in the source tree today are test doubles compiled in
only when `focusforge-core`'s `test-algorithms` feature is enabled (as it is
for `cargo test --workspace`, but not for a plain `cargo build`), so
`focusforge-cli algorithms` against a release build reports
`no algorithms registered`.

`ID` is required for `--set`: `--set` with no `ID` is rejected before
anything else runs, since there is no schema to validate against.

### Listing

With no `ID`, human output is one line per algorithm — id, display name,
family — or the literal line `no algorithms registered` when the registry is
empty. `--json` emits `{"algorithms": [...]}`, one element per registered
algorithm, each already carrying its full parameter list (see below); there
is no separate summary shape for the listing.

### One algorithm's schema

```
focusforge-cli algorithms <ID> [--json] [--set KEY=VALUE]...
```

Prints `<ID>`'s id, display name and family, then one block per declared
parameter: its kind (a float or int range and step, a bool, or an enum's
variant ids and labels), its default, the value that resolved for this
invocation, where that value came from, and whether the parameter has an
`auto` hook. Resolution here is explicit-over-default only — an `auto` hook
needs a loaded stack to run against, and this subcommand loads no image, so
a parameter with no `--set` reports its plain default and a hook is shown as
present but not evaluated, never run.

`--set key=value` sets one parameter, and is repeatable; a key set more than
once keeps the last value. Every `--set` pair is validated against `<ID>`'s
schema before anything is printed: an unknown key, a numeric value outside
its declared range, a value that does not name one of an enum's declared
variants, or text that does not parse as the parameter's kind, is reported
by name — every violation across every `--set` pair, not only the first —
and the command exits without printing a schema.

`--json` emits the same per-parameter facts as the frozen schema described
above, one element in `{"algorithms": [...]}`. This is the frozen
`algorithms --json` schema: a field is added, never renamed or removed, in
later versions, matching `info --json`'s own compatibility promise.
