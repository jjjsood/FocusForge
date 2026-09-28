# Fixture set `retouch`

Three hand-chosen `brush-stroke` ops in `stroke-list.json`, applied against the 512x512,
8-frame `fixtures/mantis-512/golden/laplacian-zmap.tif`. `source_id` stays in `0..8`.

1. A horizontal stroke from `(120,256)` to `(380,256)`, radius 18: the golden map's per-tile
   winner index changes dozens of times along row 256, so this stroke crosses several region
   boundaries rather than repainting one uniform patch.
2. A stroke from `(470,100)` to `(540,130)`, radius 24: `540 + 24` runs past the map's
   512-pixel width, so the stroke's footprint and its painted pixels are clipped at the right
   edge rather than refused. Its vertical reach, `76..154`, stays inside the map.
3. A stroke over `(150,256)`-`(300,256)`, radius 12, with a different `source_id` than stroke 1:
   its path overlaps stroke 1's, so applying the list in order repaints part of stroke 1's
   region with stroke 3's index.

## Regenerating

Not generated: re-hash after a hand edit with `tools/make-fixtures.sh --manifest-only`.
