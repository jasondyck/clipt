# Contributing

Bug reports with the output of `clipt list` and `CLIPT_DEBUG=1 clipt …` help
the most: they show the monitor layout, scales and the capture backend in use.

## Checks

CI runs these on every push and pull request (only `typos` when the change
touches nothing but Markdown, `docs/` or the licence files); please run them
first:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check          # licences and RustSec advisories
typos
tests/e2e.sh              # needs Docker: headless Sway (4 layouts + sound), GNOME (2), X11 (2)
```

`tests/e2e.sh sway triple` (or any `IMAGE LAYOUT` pair) runs one suite. The
suites start a real compositor in a container, drive it with scripted mouse
and keyboard input (`examples/vinput.rs` on Sway, Mutter's RemoteDesktop API
on GNOME, xdotool on X11) and compare every capture against `grim`,
ImageMagick's `import` or the expected native size, including mixed-DPI,
fractional-scale and rotated monitors.

`tests/wine.sh` cross-builds `clipt.exe` and runs it under Wine on Xvfb:
monitors, captures, the clipboard and the editor, driven by xdotool (needs
Docker and `rustup target add x86_64-pc-windows-gnu`). Wine isn't Windows,
so CI's `windows` job and a real PC have the last word.

Rendering changes are guarded by `partial_repaints_match_full_render`, which
replays a session through the damage-tracked redraw path and compares every
frame with a full redraw.

`tests/screenshot/make.sh` regenerates the README's picture, `docs/screenshot.png`:
clipt's editor over real GNOME Settings in the GNOME test container, annotated
by the same scripted input (needs Docker).

## Benchmarks

```
cargo test --release bench_4k -- --ignored --nocapture
CLIPT_BENCH_PNGS=a.png,b.png cargo test --release bench_png -- --ignored --nocapture
cargo build --profile profiling   # release speed with symbols, for perf
```

## Licence

By contributing you agree that your work is dual-licensed under MIT or
Apache-2.0, like the rest of the project.

## Releasing

Run the **release** workflow from the Actions tab (or
`gh workflow run release -f version=0.2.0`). It runs clippy and the tests,
sets the version in `Cargo.toml`, turns the CHANGELOG's `[Unreleased]`
section into `[0.2.0] - <date>`, commits, tags `v0.2.0` and publishes a
GitHub release with Linux tarballs, `.deb`, `.rpm`, a signed and notarised
universal macOS tarball, a Windows zip, and `SHA256SUMS`. A version with
a suffix (`0.2.0-rc1`) is marked as a prerelease, which `clipt update`
ignores. Keep `[Unreleased]` up to date as you go; it becomes the release
notes.

Pushing a `v*` tag by hand also works, as long as `Cargo.toml` and the
CHANGELOG already say that version.
