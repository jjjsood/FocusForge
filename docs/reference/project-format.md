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

The container carries no format version. A project written by an older
build that lacks a field the current reader needs is refused when opened
(see [Refusals](#refusals)); re-stack the frames to get a current one.

- `manifest.json` — the algorithm, its resolved parameters, the source frame
  paths with each file's content hash, dimensions, each frame's solved
  alignment transform and the applied crop, and the id of the head
  checkpoint.
- `blobs/<hash>.zst` — the composite plane and the Z-Map planes, each named
  by a `blake3` hash taken over its uncompressed content, so a blob's name
  attests to what it holds. Only a fused
  checkpoint has planes of its own; a retouched checkpoint stores its
  strokes alone, so it costs the size of its edit.
- `checkpoints/<hash>.json` — one file per checkpoint: its parent, its op
  log, the Z-Map blob of the fuse it descends from, the algorithm and parameters, a
  creation time and a hidden flag. A checkpoint a fuse produced also
  records that fuse's composite, alignment, crop, sources and metrics.

## Checkpoints

Checkpoints form a tree. A fresh stack writes one with no parent and no
ops; saving retouch ops (`focusforge-cli retouch`, or the window's
**Save checkpoint**) adds a child holding those ops. Appending ops to an
older checkpoint starts a new branch from it. A checkpoint's image is the
nearest fused ancestor's composite with the op logs on the way down blended
over it, in order.

Deleting a checkpoint hides it. A hidden checkpoint stays in the file for as
long as a visible checkpoint is built on it, and is removed together with
its blobs once none is. The last visible checkpoint cannot be deleted. When
the deleted checkpoint was the head, the newest visible checkpoint becomes the
head.

Nothing outside the three paths above is ever written.

## Who reads it

`focusforge-cli export` renders a checkpoint to an image or writes its
Z-Map; `focusforge-cli history` lists, extends and prunes
checkpoints; `focusforge-cli review` reports the outcome and parameters the
manifest recorded, streaming the Z-Map blob only to compute its coherence
metric. `focusforge-cli cache clear` never touches a project; it only empties
the stage cache under `.scratch/stage-cache/`.

## Source frames

A project stores no pixels from the sources, so rendering a retouched
checkpoint re-reads its source frames; an unretouched one needs none. Each is checked against
the content hash recorded when the project was written:

- A frame that cannot be found makes a retouched `export -o` refuse, naming
  every missing frame. An unretouched `export -o`, `export --zmap` alone and
  the `history` subcommands need no frames.
- A frame whose content changed makes a retouched `export -o` refuse, naming
  it. The strokes would blend frames the project was not stacked from.
  `history list` and `history delete` refuse the same way with exit code 9
  unless their `--accept-changed-sources` flag is passed, which prints a
  warning and proceeds.
- In the window, a project with changed frames asks before opening and
  offers **Locate frames…** to point at the originals, or **Open anyway**.
  A project whose frames are not where it recorded them opens with a notice
  and the same **Locate frames…** action. A folder chosen there is accepted
  only when its files hash to what the project recorded; otherwise it is
  refused as not the project's frames.

## Refusals

A project is checked as a whole when opened, and a damaged one is refused by
name rather than opened partially:

| refusal | exit code |
|---|---|
| not a FocusForge project | 3 |
| truncated or incomplete file | 4 |
| a referenced entry is missing | 6 |
| a blob fails its hash check | 7 |
| a dangling parent, a parent cycle, a stack fingerprint that disagrees with the recorded sources, or a checkpoint with ops but no parent | 8 |

A checkpoint with no ops must carry the record of its own fuse; one without
it is refused as a damaged container. Any other failure exits 1. The
exit codes above are those of `focusforge-cli history`.
