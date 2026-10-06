# Fixture set `retouch`

Three hand-chosen `brush-stroke` ops in `stroke-list.json`, a bare JSON array that
`focusforge-cli retouch <project> --ops` appends to a project stacked from
`fixtures/mantis-512` (512x512, 8 frames). `source_id` is the frame blended into the result
through the stroke's soft mask and stays in `0..8`; `radius` is in pixels and `softness` in
`0.0..=1.0`.

1. A horizontal stroke from `(120,256)` to `(380,256)`, radius 18, softness 0.3: frame 2 is
   blended along row 256, fully opaque on the path and fading to nothing at the radius.
2. A stroke from `(470,100)` to `(540,130)`, radius 24: `540 + 24` runs past the image's
   512-pixel width, so the stroke's footprint is clipped at the right edge rather than refused.
   Its vertical reach, `76..154`, stays inside the image.
3. A stroke over `(150,256)`-`(300,256)`, radius 12, with a different `source_id` than stroke 1:
   its path overlaps stroke 1's, and strokes apply in list order, so stroke 3's frame is blended
   over the part of stroke 1 it covers.

## Regenerating

Not generated: re-hash after a hand edit with `tools/make-fixtures.sh --manifest-only`.
