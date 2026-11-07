# Alignment reference strategies

This standalone command records fixed-anchor, composed-chain, and rolling-reference alignment results for the experiment package. It uses the public complete `PyramidalEcc` API and strict failure semantics. The rolling record retains a failed best record for diagnostics, but only advances its reference transform after a non-diverged result.

Run the fit-free structural check:

```text
cargo test --manifest-path spikes/alignment-reference-strategies/Cargo.toml
```

Run one strict CPU strategy on a raw TIFF directory:

```text
alignment-reference-strategies --input <directory> --strategy <anchor|chain|rolling> --reference <first|middle> --order <none|gain-before|truth-oracle>
```

The command emits one standard-library JSON record per frame with the public
status, reason, iteration count, best-ρ, transform, crop, final reader coverage,
and rolling fitting-window template/moving coverage. `gain-before` estimates product global gains and decorates the reader
before fitting. Rolling stores failed diagnostics but advances only from a
non-diverged result. `--real` refuses failed or unreadable preparation; an
injected failure is accepted only for rolling controls. The package stage
executor retains input hashes, raw streams, native exit, and parsed records;
neither command creates an acceptance proof.

The fit-free product-resampler mode accepts one CSV row per frame (`index`
followed by the nine affine coefficients) and uses the same public crop and
warp APIs. It writes RGB16 TIFFs and one kernel-valid mask TIFF per frame:

```text
alignment-reference-strategies --resample --input <directory> --transforms <rows.csv> --output <directory>
```
