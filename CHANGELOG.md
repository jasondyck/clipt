# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/).

## [Unreleased]

## [0.2.0] - 2026-11-01

### Added
- `clipt record`: record the screen, a region or a window to MP4, MKV, MOV
  or GIF. With no arguments it opens the selection overlay in record mode
  (Enter records, click a window to record it); `clipt record screen
  [NAME]`, `-g GEOM` and `window APP` skip the overlay. Stop with
  `clipt record --stop`, Ctrl-C or the tray icon; `--pause`, `--resume`
  and `--toggle-pause` leave paused time out of the video.
- Recording works on wlroots compositors (Sway, Hyprland, niri, labwc,
  COSMIC, …), KDE Plasma, GNOME and X11, and through the ScreenCast portal
  elsewhere (its dialog appears once; the choice is remembered). On KDE and
  GNOME frames arrive over PipeWire, whose library is loaded only when
  recording, so clipt still starts without it.
- Windows are recorded on their own where the compositor can (KDE Plasma,
  Sway 1.11+, Hyprland 0.50+, COSMIC, or picked in the portal's dialog) and
  followed by position on older Sway and Hyprland, and on X11. When a
  recorded window resizes, the video keeps its size and the zoom eases to
  fit.
- Encoding runs ffmpeg (needed only for recording) on the GPU where
  possible (VA-API, NVENC), libx264 otherwise. Frames are only captured and
  encoded when the screen changes, so a still screen costs almost nothing.
  GIFs (`--gif`, or an output name ending in `.gif`) are made when the
  recording stops, at up to 15 fps.
- On wlroots compositors a red frame outside the area marks a region
  recording, amber while paused. The tray gains *Record region…*, *Record
  screen*, *Pause/Resume/Stop recording* and *Open recordings folder*, and
  its icon shows a red (paused: amber) dot while recording.
- New config keys: `video_dir` (default `~/Videos/Screencasts`),
  `video_filename`, `record_fps`, `record_codec`, `record_format` and
  `max_duration` (default 1 hour).
- A man page (`man clipt`) and bash, zsh and fish completions, installed by
  the packages, `install.sh` and the release tarballs.
- The Flatpak (still a prototype, built from `packaging/flatpak`) records
  too, with its own ffmpeg.

## [0.1.1] - 2026-10-31

### Fixed
- clipt hung at startup, before showing anything, when xdg-desktop-portal had
  stopped answering, even on KDE where the portal isn't used to capture. D-Bus
  calls now give up after 5 seconds and clipt carries on without that backend.
- A faint one-pixel seam could flash along the window highlight while it
  glided between windows; the highlight now moves in whole pixels.

## [0.1.0] - 2026-10-31

First public release.

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

### Fixed
- Redraws while moving the pointer repainted the whole screen: the crosshair's
  two lines were merged into one screen-sized damage box. Now ~4 ms median on
  4K instead of 7-12 ms.
- The tray kept launching the old binary path after an upgrade replaced it.
