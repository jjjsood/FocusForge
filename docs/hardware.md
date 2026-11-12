# Hardware

The free RAM a run needs depends on the kind of frames and their size, not on how many there are. It is the **free** RAM that counts, not the installed RAM: FocusForge reads the available memory when a run starts (inside a container, the free memory under the container's limit), sets aside about 1.6 GB for what every run holds anyway, and plans its working memory with 65 % of the rest. With less than about 1.6 GB free nothing runs. If the free memory cannot be read, it plans with a fixed 1 GiB. `--max-host-memory` can only lower the working memory ([batch runs](reference/batch-runs.md)).

Free RAM per workload, in decimal GB, rounded up:

| Frames | Run at all | Recommended | Scratch disk per frame, if the frames do not fit in RAM |
|---|---|---|---|
| Camera RAW, 24–26 MP | 3.1 GB | 7.5 GB | 160 MB |
| Camera RAW, 45–61 MP | 4.1–5.0 GB | 11.7–15.3 GB | 280–370 MB |
| 16-bit TIFF, uncompressed | 1.7 GB | 3.3 GB | none |
| 16-bit TIFF, compressed | 1.7 GB; 2.6 GB (26 MP) to 3.9 GB (61 MP) if a frame is stored as one single strip | 3.8 GB (3.9 GB for a single-strip 61 MP frame) | none |
| JPEG, 24–61 MP | 3.0–5.2 GB | 3.8–5.2 GB | none |

- A retouch or edit session needs the same minimum as a stack run of the same frames. The window's preview buffers come on top. Exporting a retouched TIFF project held 24 MB beyond what the process holds anyway on a 4722 × 3100 result; a RAW project's export has not been measured.
- Camera RAW is the hungriest because every frame is developed in memory before it is stored: the minimum is the free RAM for one frame's develop pass, and the recommended figure lets four frames develop at once. A developed frame takes width × height × 6 bytes (the scratch figures above are for the developed size). When all the developed frames fit in the working memory beside what the run needs to develop and fuse, they stay in RAM, nothing is written to scratch and the stack runs faster. Otherwise every developed frame goes to a scratch file, and the scratch disk grows linearly with the frame count: 10 frames of 5088 × 3392 take 1.04 GB. The check of free disk space before a run counts the scratch files only in that case.
- Fewer frames cut scratch disk and run time, and make it likelier that the developed frames stay in RAM, but they do not lower the free RAM one frame needs.
- Rule of thumb for a camera not listed: free RAM ≈ 1.6 GB + (raw pixels × 30 B + file size) ÷ 0.65, about 1.6 GB + 53 MB per advertised megapixel + 1.5 × the file size. Sensors read out roughly 15 % more pixels than advertised; `focusforge-cli info` prints the exact `raw=` size.
- Uncompressed TIFF is read in tiles and needs only a tile set (about 37 MB for 3-channel 16-bit frames) beyond the fixed part. A compressed TIFF needs its largest compressed block decoded; JPEG needs one frame decoded and widened. The recommended figures for these formats give the tiles in flight their full 1 GiB.

## Measured on one machine

32 threads, 64 GB RAM, RTX 5070 Ti. The RAW runs were measured with the developed frames on scratch files; frames that stay in RAM count in the peak memory held. Peak memory held is the peak of memory the system cannot reclaim; memory-mapped frames and the file cache come on top and are given back under pressure. Frame sizes are in pixels.

| Run | Free RAM at start | Working memory | Peak memory held |
|---|---|---|---|
| 10 RAW frames, 5376 × 3574 raw, GPU | inside a 2.52 GB container | 586 MB | 1075 MB |
| the same | capped by `--max-host-memory` | 586 MB | 1035 MB |
| the same | capped | 2342 MB | 1718 MB |
| the same | capped | 4568 MB | 2967 MB |
| the same | 53.6 GB | 33.8 GB | 4009 MB |
| the same, CPU only | capped | 586 MB | 1587 MB |
| 10 TIFF frames, 4781 × 3136 16-bit, GPU | capped | 1 GiB | 919 MB |
| the same | capped | 4 GiB | 913 MB |
| the same, 32 frames | capped | 1 GiB | 912 MB |
| the same, CPU only | capped | 1 GiB | 1474 MB |
| Export of a retouched TIFF project, 4722 × 3100 | 53.8 GB | from free RAM | 24 MB |

The peak is mostly a fixed part, but with a GPU and RAW frames it grows with the working memory, from 1.0 GB at 586 MB to 4.0 GB at 33.8 GB. TIFF frames are read in tiles, so going from 10 to 32 frames changed the peak by −7 MB. More working memory also makes runs faster: the RAW stack took 25.7 s at 586 MB and 22.0 s at 2342 MB, the TIFF stack 11.3 s at 1 GiB and 9.4 s at 4 GiB. The 10-frame RAW set is accepted from about 2.46 GB of free RAM, or 2.52 GB inside a container, which holds about 58 MB itself; at 2.464 GB it is refused with the shortfall named.

## Graphics card

A GPU is optional and the result is the same without one. Under 256 MiB of VRAM FocusForge uses the CPU on its own. More VRAM keeps more tiles on the card: the GPU path asks for up to 8 GiB and backs off to what the card allows (`--max-vram` lowers it). It is also faster and holds less fixed memory: the RAW stack above took 69.5 s on the CPU against 25.7 s on the GPU, with a fixed part of 1563 MB against 807 MB.

## When a run is short of memory

FocusForge says how much more it needs. Close other programs, convert RAW or JPEG frames to TIFF, or, if you set a memory limit, raise it.
