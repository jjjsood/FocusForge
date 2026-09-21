# The project container (`.ffp`)

`focusforge-cli stack` writes one `.ffp` file per detected stack instead
of an image. It is a zip archive holding exactly three kinds of entry:

```
$ unzip -l mantis-512.ffp
  Length      Date    Time    Name
---------  ---------- -----   ----
     3804  1980-01-01 00:00   manifest.json
  1426980  1980-01-01 00:00   blobs/d24eca28….zst
    40385  1980-01-01 00:00   blobs/7108de1d….zst
      359  1980-01-01 00:00   checkpoints/5b71adbf….json
```

Every entry is stored, not deflated — the blobs are already zstd-compressed
— and every timestamp is the DOS epoch, because nothing in the writer reads
the system clock. Two seals of the same inputs therefore produce byte-
identical archives.

- `manifest.json` — the algorithm, its resolved parameters, the source frame
  paths, dimensions, and a `format_version` for forward compatibility.
- `blobs/<hash>.zst` — the composite plane and the Z-Map plane, each named
  by a `blake3` hash taken over its uncompressed content, so a blob's name
  attests to what it holds.
- `checkpoints/<hash>.json` — the project's op log; a freshly written
  project carries one checkpoint with no parent and no ops.

Nothing outside those three paths is ever written. A project is read by
`focusforge-cli export` (turning it into a composite and/or Z-Map TIFF) and
`focusforge-cli review` (reporting the outcome and parameters the manifest
already recorded, streaming the Z-Map blob only to compute its coherence
metric). `focusforge-cli cache` never touches a project — its `clear`
subcommand only empties the stage cache under `.scratch/stage-cache/`.

A source frame path a project cannot resolve relative to the project's own
location is not a hard error: `export` and `review` name every unresolved
source and continue — the composite and Z-Map are unaffected, since they are
stored inside the container, not re-read from the sources.
