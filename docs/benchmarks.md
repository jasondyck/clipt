# Image benchmarks

Run `cargo bench --locked --bench image -- --noplot --save-baseline initial`
to save a local Criterion baseline. After a change, use `--baseline initial`
instead of `--save-baseline initial` to compare. Append a filter such as
`'image/rotate_90'` to run a subset. Results live under `target/criterion/`;
they are local build artefacts, so preserve that directory for comparisons.

The suite includes the binary's image module directly, without a public library
API or a copy of its algorithms. Criterion's plotting and parallel-analysis
defaults are disabled; no display, fonts, external images or ffmpeg are needed.
Criterion 0.7 supports the project's Rust 1.89 minimum and avoids the C helper
introduced in Criterion 0.8.

Coverage at 1920×1080 and 3840×2160:

- Padded BGRX capture rows converted to packed RGBA, including alpha fill.
- Centre crop to half width and height, 90° rotation, half-size box downsampling,
  and three-quarter-size bicubic resampling.
- Normal and quick PNG encoding, and decoding normal-encoder output, for opaque
  flat panels/stripes, opaque xorshift noise and translucent panels/stripes.
  These exercise RGB packing, the compressible-content re-encode and RGBA paths.

Fixture generation, padded row preparation, pre-encoding for decode, and PNG
round-trip assertions happen outside timing. Every iteration consumes its output
through Criterion and includes production allocation and output destruction.
Inputs are immutable and reused; no state accumulates between iterations.
Throughput counts source pixels, including for crops and resizes; it is not a
count of output pixels or encoded bytes.

These are CPU and memory microbenchmarks. They cover shared screenshot image
operations, not compositor capture, editor interaction/annotation rendering,
clipboard transfer, disk writes or video encoding. The existing ignored
`bench_4k` test remains the manual editor probe. Synthetic panels and noise are
repeatable bounds on content, not a representative corpus of real desktops.

## Initial baseline

Run on 2026-11-15 with this command:

```
cargo bench --locked --bench image -- --noplot --sample-size 20 --warm-up-time 1 --measurement-time 3 --save-baseline initial
```

Host: AMD Ryzen 7 3800X (8 cores / 16 threads), x86_64 Linux
7.3.1-1-cachyos, rustc 1.99.1 (LLVM 23.1.1), default features and Cargo's
optimised bench profile (release LTO, one codegen unit, no custom RUSTFLAGS).
The reported CPU governor was `powersave`, with boost disabled. No CPU affinity
or frequency was pinned. Bulk validation builds completed before sampling began;
a roughly 1 s final lint check overlapped part of sampling. Other host activity
was not controlled. These are warm-input measurements on one machine, with
20 samples, 1 s warm-up and a requested 3 s measurement per
case (Criterion may extend this). They are an initial reference, not evidence
of an improvement. Repeat with longer sampling on the same idle host before
interpreting small differences.

Times below are Criterion's point estimates in milliseconds, with its 95%
confidence intervals. Raw samples and the saved `initial` baseline remain in
`target/criterion/`; they are not committed.

| Operation / content | 1080p ms [95% CI] | 4K ms [95% CI] |
|---|---:|---:|
| `image/crop_centre_half` | 0.074 [0.073, 0.075] | 0.787 [0.771, 0.806] |
| `image/from_bgrx_padded` | 1.372 [1.357, 1.391] | 6.416 [6.339, 6.517] |
| `image/resize_half` | 3.002 [2.973, 3.036] | 12.174 [12.085, 12.289] |
| `image/resize_three_quarters` | 39.671 [39.515, 39.914] | 157.672 [157.295, 158.074] |
| `image/rotate_90` | 2.151 [2.132, 2.173] | 11.264 [11.187, 11.381] |
| `png/decode/noise_rgb` | 6.977 [6.943, 7.016] | 27.860 [27.384, 28.242] |
| `png/decode/ui_rgb` | 7.325 [7.276, 7.370] | 29.248 [29.022, 29.438] |
| `png/decode/ui_rgba` | 1.538 [1.530, 1.545] | 8.359 [8.282, 8.454] |
| `png/encode/noise_rgb` | 15.148 [15.069, 15.238] | 74.819 [74.402, 75.327] |
| `png/encode/ui_rgb` | 17.479 [17.314, 17.705] | 68.329 [68.013, 68.774] |
| `png/encode/ui_rgba` | 21.001 [20.865, 21.228] | 82.907 [82.831, 83.000] |
| `png/encode_quick/noise_rgb` | 15.038 [15.001, 15.100] | 74.304 [74.155, 74.461] |
| `png/encode_quick/ui_rgb` | 2.734 [2.688, 2.805] | 12.366 [12.311, 12.436] |
| `png/encode_quick/ui_rgba` | 2.038 [2.023, 2.059] | 9.481 [9.457, 9.512] |
