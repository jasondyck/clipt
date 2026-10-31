# Contributing

Bug reports with the output of `clipt list` and `CLIPT_DEBUG=1 clipt …` help
the most: they show the monitor layout, scales and the capture backend in use.

## Checks

CI runs these on every push and pull request; please run them first:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check          # licences and RustSec advisories
typos
tests/e2e.sh              # needs Docker: headless Sway (4 layouts), GNOME (2), X11 (2)
```

`tests/e2e.sh sway triple` (or any `IMAGE LAYOUT` pair) runs one suite. The
suites start a real compositor in a container, drive it with scripted mouse
and keyboard input (`examples/vinput.rs` on Sway, Mutter's RemoteDesktop API
on GNOME, xdotool on X11) and compare every capture against `grim`,
ImageMagick's `import` or the expected native size, including mixed-DPI,
fractional-scale and rotated monitors.

Rendering changes are guarded by `partial_repaints_match_full_render`, which
replays a session through the damage-tracked redraw path and compares every
frame with a full redraw.

## Benchmarks

```
cargo test --release bench_4k -- --ignored --nocapture
CLIPT_BENCH_PNGS=a.png,b.png cargo test --release bench_png -- --ignored --nocapture
cargo build --profile profiling   # release speed with symbols, for perf
```

## Licence

By contributing you agree that your work is dual-licensed under MIT or
Apache-2.0, like the rest of the project.
