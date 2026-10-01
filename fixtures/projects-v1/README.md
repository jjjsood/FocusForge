# Fixture set `projects-v1`

One format-1 project per stacking method, written by `focusforge-cli` at commit `90c3c5a`
before the fusion data contract changed any numerics. They pin what a project written then
holds, so a later reader or replay can be checked against it. Never regenerate them; after a
hand edit re-hash with `tools/make-fixtures.sh --manifest-only`.

| file | algorithm | non-default settings |
|---|---|---|
| `laplacian.ffp` | `laplacian` | `levels=3` |
| `weighted-average.ffp` | `weighted-average` | `radius=4`, `weight-exponent=3.0` |
| `depth-map.ffp` | `depth-map` | `score-radius=2`, `smoothing-radius=5` |

All three fuse the three frames in `frames/` (a copy of `fixtures/mantis-64/frames/`, so the
projects' recorded source paths are relative and resolve from this directory), with
`format_version` 1, `--no-align` (alignment refuses the 64x64 extent: "no further step would
improve the match") and `--no-gpu`. Each project holds a 60x60 composite and Z-Map.

Written with a groups file naming each group after its algorithm, so the project lands beside
the frames:

```
focusforge-cli stack fixtures/projects-v1 --groups <groups.toml> --algo <id> <--set ...> --no-align --no-gpu
```
