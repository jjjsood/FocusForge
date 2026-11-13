# Free RAM per kind of frame

The free RAM a stack needs depends on the kind of frames and their size, not on
how many there are. It is the **free** RAM that counts, not the installed RAM.
Figures are in decimal GB, rounded up.

| Frames | Free RAM to run | Free RAM recommended | Temporary disk space per frame, only if the frames do not fit in RAM |
|---|---|---|---|
| Camera RAW, 24–26 MP | 3.1 GB | 7.5 GB | 160 MB |
| Camera RAW, 45–61 MP | 4.1–5.0 GB | 11.7–15.3 GB | 280–370 MB |
| 16-bit TIFF, uncompressed | 1.7 GB | 3.3 GB | none |
| 16-bit TIFF, compressed | 1.7 GB | 3.8 GB | none |
| JPEG, 24–61 MP | 3.0–5.2 GB | 3.8–5.2 GB | none |

- A compressed TIFF stored as one single strip needs more to run: 2.6 GB at
  26 MP up to 3.9 GB at 61 MP.
- Camera RAW needs the most, because every frame is developed in memory first.
  When the developed frames fit in RAM they stay there and use no disk;
  otherwise each one is written to a temporary file.
- For a camera that is not listed, use the row with the next larger megapixel
  count.
- Editing and retouching a project need the same free RAM as a fuse of the same
  frames.
- Fewer frames shorten the run and use less temporary disk space, but do not
  lower the free RAM that one frame needs.

If a run is short of memory, FocusForge says how much more it needs; see
[Not enough free RAM to read this
file](troubleshooting.md#not-enough-free-ram-to-read-this-file).
