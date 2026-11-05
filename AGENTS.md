# Notes for coding agents

clipt is a screenshot and screen-recording tool for Wayland and X11, with
macOS and Windows ports under way: one Rust binary, no GUI toolkit. Read `CONTRIBUTING.md` for the full check list; this
file says which checks matter for which change, and the house style.

## Where things are

- `src/capture/`: getting pixels. `wayland.rs` (ext-image-copy-capture,
  wlr-screencopy), `kwin.rs` (KDE screenshots), `portal.rs` (GNOME
  screenshots); recording sources live beside them.
- `src/editor/`: selection, annotation and the toolbar; knows nothing about
  Wayland. `src/overlay.rs` and `src/x11/` host it on screen.
- `src/record/`: `clipt record` (writer, encoder, camera, frame indicator).
- `src/macos/`: the macOS host (capture, overlay, pasteboard, window list).
  Plan and status in `docs/macos-plan.md`.
- `src/win32/`: the Windows host (GDI capture, overlay, clipboard, window
  list). Plan and status in `docs/windows-plan.md`.
- Everything Wayland/X11-only is behind `cfg(wayland_x11)` (set by
  `build.rs` for every Unix but macOS; `Cargo.toml` spells it
  `cfg(all(unix, not(target_os = "macos")))`).
- `src/tray.rs`, `src/update.rs`, `src/config.rs`, `src/main.rs` (CLI).
- `src/profile.rs`: Tracy zones (`zone!`), empty unless built with
  `--features tracy`; see CONTRIBUTING's Profiling section.
- `tests/`: end-to-end suites per desktop, run in Docker by `tests/e2e.sh`.
  `examples/vinput.rs` and `examples/testwin.rs` are their input driver and
  test window; `examples/x11grab.rs` is a popup that holds the X11 grabs.
- `docs/recording-plan.md`: design and status of screen recording.

## Checks by kind of change

CI runs everything on every push, except that a change touching only
Markdown, `docs/` or the licence files runs just the spell check
(`typos`); a PR is only ready when all of what ran is green. Locally, run
what the change can break:

| Change | Run |
|---|---|
| Docs only (README, CHANGELOG, docs/) | `typos` |
| Any Rust | `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` |
| Capture, overlay, editor input, recording, clipboard | also the matching `tests/e2e.sh IMAGE LAYOUT` suite (needs Docker), e.g. `tests/e2e.sh sway single` |
| Dependencies (`Cargo.toml`, `Cargo.lock`) | also `cargo deny check` and `packaging/flatpak/cargo-sources.py` (commit the regenerated `cargo-sources.json`) |
| Code behind the `pipewire` feature, or `#[cfg]`s | also `cargo clippy --no-default-features --all-targets -- -D warnings` |
| `src/win32/`, or anything shared with it | also `cargo clippy --target x86_64-pc-windows-msvc --bins --tests -- -D warnings` (type-checks from Linux; `rustup target add x86_64-pc-windows-msvc`). CI's `windows` job runs the tests |
| `src/macos/`, or anything shared with it | also `cargo clippy --target aarch64-apple-darwin --bins --tests -- -D warnings` (type-checks from Linux; `rustup target add aarch64-apple-darwin`). Only CI's `macos` job, or a Mac, runs the tests: `cargo test --bins`, since the examples are Linux-only |

Notes:
- CI's clippy is the latest stable toolchain, which has lints older local
  toolchains miss. If yours is older, install the current stable beside it
  (`rustup toolchain install stable`) and run `cargo +stable clippy ...`.
- The minimum supported Rust is 1.89 (`rust-version` in `Cargo.toml`); don't
  use newer standard-library APIs without checking.
- Don't link new C libraries: clipt must start on machines without them.
  Load optional system libraries at runtime (`dlopen`) instead, as
  `src/capture/pipewire.rs` does for libpipewire.

## Style

- Prose (docs, comments, messages) uses Canadian/British spelling: colour,
  licence (noun). Identifiers keep their library's spelling (`Color`).
- README command examples go in fenced code blocks with aligned `#`
  comments, like the Usage section. Keep new README sections short and link
  to existing ones rather than repeating them.
- User-visible changes get a line in `CHANGELOG.md` under `[Unreleased]`
  (Keep a Changelog); it becomes the release notes.
- Comments explain why, not what. Match the surrounding code's density.

## Commits and pull requests

- Commit subjects describe the change in plain words, no `feat:`-style
  prefixes. The body says why, and what was checked when it isn't obvious.
- One logical change per commit. When addressing review feedback, push new
  commits on top; don't amend, squash or force-push a branch that's under
  review, so reviewers can see what changed.
- A PR description says what changed, why, and how it was tested. If an
  instruction was ambiguous (a length limit, a placement), say which reading
  you chose.
- Coding agents never merge, not even their own work. Merges happen only
  through the maintainer's merge automation, which merges the maintainer's
  own PRs once every CI check is green and a review of the current commit
  passed; everyone else's PRs are merged by a person.
