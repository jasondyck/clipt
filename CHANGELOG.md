# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/).

## [Unreleased]

### Added
- `clipt tray`: system tray icon (StatusNotifierItem + dbusmenu). Click for a
  region, middle-click for the full desktop, menu for delayed capture, the
  screenshots folder and Quit. `install.sh --tray` adds it to autostart.
- clipt icon (flat mascot holding a picture frame): drawn in code for the tray,
  simplified and pixel-snapped below 32 px; `data/clipt.svg` for the desktop entry.
- Window selection: before selecting, the window under the pointer (or the
  screen, over bare desktop) is highlighted with its name and size; a click
  takes it, Tab switches window/screen. Window positions come from KWin, Sway,
  Hyprland or EWMH on X11, fetched while the screen is captured.
- HUD-style hint bar with key caps; the hover highlight eases between targets.
- `clipt update [--check]` and *Check for updates…* in the tray: installs the
  new release in place for install.sh installs (SHA-256 verified), points
  packaged installs at their package manager. Opt-in daily check with
  `check_updates = true`.
- Release workflow: one click bumps, tags and publishes packages.

### Fixed
- Redraws while moving the pointer repainted the whole screen: the crosshair's
  two lines were merged into one screen-sized damage box. Now ~4 ms median on
  4K instead of 7-12 ms.
- The tray kept launching the old binary path after an upgrade replaced it.

## [0.1.0] - unreleased

First public release.

### Added
- Frozen-screen region selection with resize handles, magnifier and
  click-to-select-a-monitor; selections can span monitors.
- Annotation: rectangle, ellipse, arrow, line, pen, highlighter, text,
  pixelate and numbered counters; 8 colours, adjustable width, undo/redo.
- Copy (clipt stays in the background to serve the clipboard, ~5 MB), save,
  desktop notifications.
- Capture backends picked automatically: ext-image-copy-capture,
  wlr-screencopy, KWin ScreenShot2 and xdg-desktop-portal.
- Overlay on every monitor at once at native resolution: no monitor
  picker, correct with mixed resolutions, fractional scaling and rotated
  monitors.
- X11 support (Xfce, Cinnamon, MATE, LXQt, i3, Openbox, …): RandR or
  Xinerama monitors, MIT-SHM overlay, CLIPBOARD with INCR for large images.
- Builds on glibc and musl Linux (x86_64, aarch64, armv7, riscv64) and FreeBSD.
- `full`, `screen` and slurp-compatible `-g` modes; stdout output.
- End-to-end test suites on headless Sway, GNOME Shell and X11.
