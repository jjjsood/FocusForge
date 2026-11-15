# Fixture set `mantis-512-hot-pixels`

The eight frames of `mantis-512` with stuck pixels written in at fixed positions: the same sensor position in some or all of the frames, all three channels set to 0 or 65535. `hot-pixels.json` lists every injected pixel as `x`, `y`, `value` and the `frames` (indices of `frames/NN.tif`) that carry it. The defects sit in the interior, on every edge and on the corners, in 8, 7, 6 and 5 frames (more than half), in exactly 4 and in 3, 2 and 1.

Frames are deflate-compressed (Predictor 2, 16 rows per strip).

`defect-mask.tif` is a user-painted defect mask for these frames: 512x512, 8-bit grey, white with 86 black pixels (black marks a defect) -- 5 single pixels, among them the corners (0, 0) and (511, 511), and one disc of radius 5 centred on (380, 120).
