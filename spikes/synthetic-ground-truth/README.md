# Spike: synthetic ground truth

Synthetic focus stack generator with a known Z-Map and, with `--warp`, a
known misregistration. The `--warp` path is used by `tools/synth-align.sh`
and `crates/cli/tests/synthetic_ground_truth.rs`.

It renders a focus stack from a known depth map, so the true Z-Map is exact rather
than inferred, runs `focusforge-cli stack` over it at several `levels`, and scores
every result against that truth.

## Why there is no `Cargo.toml`

The other spike packages carry one so that `[workspace] exclude = ["spikes"]` keeps
their dependency pins out of `cargo test --workspace`. This spike is Python: the
exclusion already covers the directory, and adding an empty manifest would only
invite cargo to look at it.

The language choice matches `tools/tiff-diff.py`: an oracle that checks the Rust
code should not share the Rust code's arithmetic. Here it also never ships, so
numpy never reaches `cargo deny check licenses`.

## Running it

From the repository root, with the workspace built:

```
python3 spikes/synthetic-ground-truth/synth_stack.py --scene hard
python3 spikes/synthetic-ground-truth/synth_stack.py --scene all --sigma-step 1.1 --noise 0
```

Needs numpy and Pillow.

## Ground-truth alignment (`--warp`)

`--warp AMP` (`AMP > 0`) selects a second, independent instrument instead of
the fusion-scoring run above: it renders the same kind of stack with a known,
seeded per-frame similarity misregistration, verifies the resampler's warp
direction with two checks (an impulse test and a recovered-sign check), and
drives `focusforge-cli align --json` twice (`mode=rotation-scale` strict,
`mode=translation` lax), writing `<out>/measurement.json` -- the artefact
`crates/cli/tests/synthetic_ground_truth.rs` asserts against.

```
python3 spikes/synthetic-ground-truth/synth_stack.py --self-check
python3 spikes/synthetic-ground-truth/synth_stack.py --warp 2.0 --scene hard --seed 11 \
    --out .scratch/synth-align --algo pyramidal-ecc
python3 spikes/synthetic-ground-truth/synth_stack.py --warp 2.0 --frames-only \
    --out .scratch/sgt-frames
```

`--self-check` runs only the impulse direction check and exits non-zero on
disagreement -- needs only numpy. `--frames-only` renders the frames and
`truth.json` and invokes no CLI at all. Never combine `--warp > 0` with
`--scene easy`: its checkerboard aliases under sub-pixel warp plus defocus
(see below).

Everything lands in `.scratch/spike-synth/`: eight frames per scene, the true
all-in-focus image, one composite and Z-Map per `levels`, and an error-map PNG.
About 120 MB for all three scenes.

## Reading the output

Three control rows come before the candidates and are the spike's own correctness
check. The true Z-Map must score 100% exact, a random one must score near 1/NFRAMES
(12.5% at eight frames), and the best single frame must land between them. If those
three do not hold, the scoring is broken and no candidate row means anything.

`--scene mantis` reuses `fixtures/mantis-512/reference.tif` as the sharp source.
That file is another stacker's output, so its residual artefacts become part of this
scene's "truth" — which is why the two fully synthetic scenes exist beside it.

## Knobs that change the answer

`--sigma-step` dominates everything. At 1.1 adjacent frames differ obviously and the
fusion scores above 99% everywhere, which makes every candidate look alike; at 0.45
adjacent frames look almost identical, as they do in a real fine-stepped stack, and
the candidates spread across 30 points. A generator that does not record this number
alongside its output produces figures that cannot be compared with anything.

`--warp` and `--seed` are the same kind of knob for the ground-truth alignment path:
`--warp` sets the misregistration amplitude (the per-frame error FLOOR barely moves
with it -- doubling `--warp` roughly doubles the identity baseline instead), and
`--seed` reseeds both the per-frame warp model and the
post-warp sensor noise. `--warp 1.0` makes `mode=translation`'s most distant frame
diverge outright, which blunts the discrimination case `tools/synth-align.sh` exists
to demonstrate -- `--warp 2.0` is what keeps every frame converged while still
landing far from the truth.
