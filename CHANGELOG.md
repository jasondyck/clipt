# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/).

## [Unreleased]

### Added
- Windows: the red frame around a region being recorded, as on Linux,
  just outside it so it isn't in the video, and amber while paused.

## [0.8.0] - 2026-11-04

### Added
- Recording on Windows: `clipt record` (a region, a monitor or a window,
  which it follows), `--stop`, `--pause` and `--resume`, Ctrl+R in the
  screenshot overlay, and GIFs. It needs ffmpeg, and
  uses NVIDIA's, Intel's or AMD's encoder where there is one. Recordings go
  to `Videos\Captures`.
- The Windows tray records too: *Record region…*, *Record screen*, *Record
  as GIF*, then *Pause*, *Resume* and *Stop*, with a red dot on the icon
  while recording (amber while paused); clicking the icon stops it. It also
  has *Open recordings folder* and *Check for updates…*, and checks daily
  with `check_updates = true`; clicking the notification updates.
- Sound in Windows recordings, through WASAPI: `--audio` records the
  computer's sound (a loopback of the default output), `mic` the default
  microphone, or any device from `clipt list sound`, with volumes, ducking
  and separate tracks as on Linux. The tray has *Record sound*.
- `clipt update` on Windows and macOS. On Windows it updates an unpacked
  release zip, moving the running `clipt.exe` aside (the next start deletes
  it); on macOS, a copy of the command-line binary outside `clipt.app`.

## [0.7.1] - 2026-11-04

### Fixed
- X11: the editor still gave up with "couldn't grab the pointer" when the
  program in the way held only the pointer, as some menus do: clipt's
  Escape went to itself rather than to the menu.

## [0.7.0] - 2026-11-04

### Added
- `clipt config` lists the settings with their values;
  `clipt config KEY` shows one and `clipt config KEY VALUE` sets it (for
  example `clipt config record_format gif`), keeping the file's comments
  and other keys, and refusing values the setting can't take.
- A GIF toggle in the Video toolbar (G): it starts from `record_format`,
  and flipping it saves `record_format`, so the next recording, from Print
  Screen, the tray or `clipt record`, follows it.
- The tray menu ticks *Record as GIF*, *Copy when saving* and
  *Notifications*, saved in the config, and *Settings…* opens the config
  file. The macOS menu bar has the last three.
- An early Windows build: screenshots with the same editor (select,
  annotate, copy, save), `clipt full` and `clipt screen`, on Windows 10 and
  11, and `clipt tray`: an icon in the notification area with the Linux
  tray's screenshot actions, Print Screen to capture a region (`hotkey`),
  notifications, and *Start with Windows*. No recording yet. Releases
  include it as `clipt-vX-x86_64-windows.zip` (unsigned: Windows asks
  before the first run).
- **?** in the editor shows every shortcut, including ones that were only in
  the code (arrows nudge the selection, Alt+arrows by 10 pixels, Ctrl+A
  takes the whole screen). The README has the same list.
- After selecting, the hint bar at the top says how to copy, save and
  start again, until you draw something; it moves to the bottom when it
  would cover the selection.

### Changed
- Copying a screenshot is ready to paste sooner (at 4K, 51 ms rather than
  190 ms for `clipt full -c`): a PNG that only goes to the clipboard skips
  the slower, smaller second compression pass. Saved files still get it.
- The overlay opens about 13 ms sooner at 4K: its first frame is painted
  on several threads (21 ms → 8 ms).
- The Copy button's tooltip shows Ctrl+C rather than Enter (both still copy).

### Fixed
- X11: the editor opens while another program holds the keyboard and
  pointer, such as an open browser `<select>` or menu, instead of giving up
  with "couldn't grab the keyboard". clipt presses Escape to close the popup,
  which is already in the screenshot. Escape goes to whatever holds the grab,
  so a program that uses it for something else will see it too.
- Recordings of a busy screen came out at about 27 fps rather than 30: the
  time each frame took to hand to ffmpeg was added to every frame interval.
- `--gif` was ignored for a recording started from the screenshot overlay's
  Video mode.
- Turning the sound or microphone on or off with S or M didn't redraw their
  toolbar buttons until something else changed.
- Right-clicking in Video mode didn't clear the selection if anything had
  been drawn in Screenshot mode, though the drawing isn't shown there.
- macOS: the editor's tooltips said Ctrl+Z, Ctrl+S and so on, where the keys
  are Cmd+Z and Cmd+S.

## [0.6.1] - 2026-11-03

### Added
- macOS: a menu bar icon, which opening clipt.app starts, with the Linux
  tray's screenshot actions and Open at Login (macOS 13+), and global
  shortcuts that need no Accessibility permission: ⌘⇧2 captures a region
  (`hotkey` in the config, "" for none) and `hotkey_full` copies the whole
  desktop.
- A colour wheel for the editor's colours: right-click one of the eight in
  the toolbar to change it. The new colours are kept in the config as
  `palette`, which can also be set by hand.
- Shapes can be changed after they're drawn: with the select tool (V),
  click one to pick it, then drag it or its handles, choose a colour or
  width to restyle it, or press Delete. Double-click text, or click it with
  the text tool, to edit it. Undo steps back through all of it.
- macOS: `clipt.app`, in each release as `clipt-vX.Y.Z-macos-app.zip`,
  signed, notarised and stapled. macOS keeps its Screen Recording
  permission under clipt rather than the terminal that started it, and it
  can go in Applications and Login Items. `clipt update` leaves it alone:
  a new version comes from the release page.

### Fixed
- Pixelating over a shape made the shape vanish: the blocks came from the
  screenshot alone. They now blur whatever is underneath, drawings included.
- Text landed half a line below the click. It's now centred on the click,
  where the text pointer points; a faint cursor at the text's size and
  colour shows where it will go before clicking, and while typing the
  cursor has a contrasting edge and the text a faint box, so both show on
  any background.

## [0.6.0] - 2026-11-03

### Added
- macOS, as a preview: the overlay, annotation, copy and save, `clipt full`,
  `clipt screen` and `clipt list`, with ⌘ for Ctrl in the editor. Not yet
  tried on a real Mac; recording and the menu bar icon come later (see
  docs/macos-plan.md). Releases include a universal binary, signed and
  notarised, which `clipt update` keeps current.

## [0.5.0] - 2026-11-02

### Added
- The red frame around a recording now shows on GNOME too, drawn by clipt's
  GNOME Shell extension (amber while paused). After updating, run
  `clipt gnome-extension` again and log out and back in.
- The screenshot overlay can record a video instead: click **Video** in the
  Screenshot | Video switch at the bottom of the screen, the camera button
  in the toolbar, or press Ctrl+R; then select, toggle the computer's sound
  (S) and the microphone (M), and press Enter. It starts from the sound
  `record_audio` sets.

## [0.4.2] - 2026-11-02

### Fixed
- Pasting a screenshot into Chromium-based apps (Edge, Teams, Chrome)
  could give only the top of the image, grey below: those apps hand over a
  non-blocking pipe, and clipt stopped writing once its 64 KB buffer filled.

## [0.4.1] - 2026-11-02

### Added
- Window highlighting on GNOME, through an optional GNOME Shell extension
  (`clipt gnome-extension` installs it; it answers only clipt). GNOME
  doesn't tell applications where windows are, so without it the overlay
  still highlights whole monitors there.

## [0.4.0] - 2026-11-02

### Added
- GNOME screenshots come from Mutter's ScreenCast (`-b mutter`) instead of
  the screenshot portal: no white flash, no PNG written to ~/Pictures and
  read back, and each monitor at its own resolution rather than the whole
  desktop at the highest monitor scale, so the overlay opens several times
  sooner on multi-monitor desktops with mixed scaling. A monitor Mutter
  sends nothing for within half a second still comes from the portal.
- `clipt record --audio-balance=duck` (or `record_audio_balance = "duck"`)
  ducks the other sound under the microphone: the microphone's sound is
  cleaned up in the mix, and the other sources dip by about 11 dB while the
  voice speaks. Needs the microphone and at least one other source.
- `--audio-balance=smart` ducks too, and at the stop says how clearly the
  voice came through ("speech clarity 0.84, good"). If it isn't good and
  turning the other sound down helps, it mixes the sound again with the
  other sources as much quieter as needed (up to 20 dB; the video is
  copied, not re-encoded). If the microphone
  hears the speakers, it says how far the voice is over the rest instead.

### Fixed
- A recording no longer hangs for ever if ffmpeg stalls at the end: after
  30 s without it finishing, clipt kills it and reports the partial file.
  Making a GIF gives ffmpeg 60 s plus twice the recording's length, then
  keeps the MP4 and says the GIF failed.

## [0.3.0] - 2026-11-02

### Added
- `clipt record --audio` records the sound the computer is playing along
  with the video, kept in sync and left out while paused. Off unless asked
  for (`--audio`, or `record_audio = "system"`); `--no-audio` overrides the
  setting. Needs a PulseAudio or pipewire-pulse server: libpulse-simple is
  loaded only when recording sound, so clipt still starts without it. On
  plain PulseAudio (not PipeWire) the first second or so of sound is
  silent: PulseAudio is slow to deliver the first chunk.
- Several sound sources at once, each with its own volume:
  `--audio=system:70,mic:120`, `--audio=mic`, `--audio=both`, or devices
  by name. They're mixed into one track; `--separate-audio` (or
  `record_separate_audio`) also keeps each source as its own track, at the
  volume it was recorded, to rebalance later. `clipt list sound` lists the
  devices and marks what `system` and `mic` stand for.
- Sound controls: in `clipt record`'s overlay, speaker and microphone
  buttons (or **S** and **M**) turn the computer's sound and the microphone
  on or off for that recording; the tray menu has a *Record sound*
  checkbox.
- The Flatpak records sound too.

### Changed
- Window recordings that zoom scale each frame 25–30 % faster.

### Fixed
- The man page and the shell completions were missing `--gif` and
  `record_format`, and the man page and README still said X11 and the
  Flatpak couldn't record.

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
