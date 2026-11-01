# Fixture set `mantis8-64-lzw-p1-tile-mm`

Re-encoding of `mantis-64`: `frames/00.tif`, `frames/01.tif`, and `frames/02.tif` map directly to the same source files; `reference.tif` maps to `mantis-64/reference.tif`. Samples are the high byte of each 16-bit source sample.

TIFF Compression tag: **5** (lzw). Predictor: **1**. Byte order: **MM**. Layout: **tile**. Bit depth: **8**.

Tiles are 48x48; the 64x64 image has four tiles, and right/bottom edge cells are zero-padded outside the image border.
