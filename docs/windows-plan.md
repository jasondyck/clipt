# Windows: implementation plan

Status, 2026-11-04: phases 0, 1 and 2 are built (screenshots, the tray and
its hotkey), checked under Wine and in CI, and not yet tried on a real PC.
Phase 3 (recording) works from the command line and the overlay, checked
under Wine; the tray's recording controls and the red frame are next.
Of phase 5, releases carry `clipt-vX-x86_64-windows.zip`; the updater and
package manifests are still to do.
A tester at work can try each phase's build; see *Testing* below.

## Done

- **Phase 0.** The Wayland and X11 code is behind `cfg(wayland_x11)`, set by
  `build.rs` for every Unix but macOS; its crates are under
  `cfg(all(unix, not(target_os = "macos")))`. The `windows` CI job lints,
  runs the unit tests, builds `clipt.exe`, captures the runner's desktop and
  uploads it all as `clipt-windows`.
- **Phase 1.** `src/win32/`: per-monitor DPI awareness, monitors from
  `EnumDisplayMonitors`, capture with GDI `BitBlt` (`--cursor` draws the
  pointer in), one topmost overlay window per monitor painting the editor
  through a DIB section, the window picker from `EnumWindows` and DWM's
  frame bounds, and the clipboard as `CF_DIB` plus `PNG`. Config in
  `%APPDATA%\clipt`, Segoe UI for text, screenshots to
  `Pictures\Screenshots`.
- **Phase 2.** `src/win32/tray.rs`: `Shell_NotifyIcon` with the Linux
  tray's screenshot actions (left click captures a region), the ticks for
  *Copy when saving*, *Notifications* and *Start with Windows* (HKCU's Run
  key), and `RegisterHotKey` for `hotkey` (default `print`) and
  `hotkey_full`. If Print Screen is taken, a notification says where to
  free it. Screenshot processes hand their notification to the tray over
  `WM_COPYDATA` (Windows shows them only for a notification-area icon), and
  clicking one opens the file. Actions start `clipt` with no console, and a
  `clipt` double-clicked or started from a shortcut starts again without
  its console window. One tray per session (a named mutex).
- **Phase 3, so far.** `src/win32/record.rs` grabs frames with GDI at the
  recording's frame rate and follows a window by its DWM frame bounds;
  everything after the frame (pausing, the camera, the Matroska stream to
  ffmpeg, GIFs) is the Linux code, now shared. Recording control
  (`src/record/control.rs`) is a named mutex `Local\clipt-record`, a state
  file in %TEMP% saying whether it's paused, and a named pipe per session
  for stop, pause and resume; Ctrl+C reaches it through
  `SetConsoleCtrlHandler`. Encoders tried: NVENC, Quick Sync, AMF, Media
  Foundation (hardware only), libx264. Sound says it isn't there yet.

Choices made on the way, where they differ from the plan below:

- **GDI for recording too, for now.** At 1080p a `BitBlt` takes a few
  milliseconds, enough for 30 fps; Desktop Duplication would cost less at
  4K but needs COM interfaces by hand. Worth measuring on the tester's PC.
- **Vendor encoders before Media Foundation.** ffmpeg's `h264_mf` picks
  whatever encoder MFT the system offers and has coarser rate control, so
  it comes after NVENC, Quick Sync and AMF rather than first.
- **GDI, not Desktop Duplication, for screenshots.** `BitBlt` is a few calls,
  works in remote desktop sessions, and needs no COM (`windows-sys` has no
  COM interfaces). Desktop Duplication comes with recording (phase 3), and
  HDR with it.
- **One scale for the editor's coordinates.** They are physical pixels
  divided by the main monitor's scale. Each monitor's own factor would open
  gaps or overlaps between monitors of different scales; one factor keeps
  Windows' layout, and the toolbar has the main monitor's size everywhere.
- **`src/windows/` keeps its name**; the host is `src/win32/`.
- **Testing without Windows.** `tests/wine.sh` cross-builds `clipt.exe` for
  `x86_64-pc-windows-gnu` and runs it under Wine with Xvfb: monitors,
  `full`, `screen`, `-g`, the clipboard, and the editor driven by xdotool
  (draw, save, copy, cancel, pick a window).

## Goal

The same clipt on Windows 10 and 11: press a key, select a region, a
window or a screen, annotate, copy or save; later, record with sound. One
`clipt.exe`, no installer needed, nothing to install beside it except
ffmpeg for recording (as on Linux).

## What carries over

About a third of the code is already independent of the display system:

- `src/editor/` (selection, annotation, toolbar, shapes, text), drawn with
  tiny-skia into a pixel buffer. It knows nothing about Wayland; a host
  feeds it pointer and key events and shows its buffer.
- `src/image.rs`, PNG output, `src/config.rs` (with a different path),
  `src/update.rs` (with a different asset).
- `src/record/`'s camera, writer and Matroska stream, and the ffmpeg
  encoder with its sound graph, including `duck`: ffmpeg runs on Windows.

What has to be written for Windows is the host around it:

| Part | Linux | Windows |
|---|---|---|
| Monitors | Wayland outputs, RandR | DXGI outputs (`IDXGIOutput1`), with per-monitor DPI |
| Screenshots | `capture/` (ext/wlr screencopy, KWin, portals, X11) | DXGI Desktop Duplication; GDI `BitBlt` as the fallback |
| Showing the editor | `overlay.rs`, `x11/overlay.rs`, behind `session::UiSession` | A third `UiSession`: borderless topmost windows over the monitors |
| Window picker | `windows.rs` (compositor lists) | `EnumWindows` and `DWMWA_EXTENDED_FRAME_BOUNDS` |
| Clipboard | Wayland and X11 selections, which clipt serves | Win32 clipboard: `CF_DIBV5` plus the registered `PNG` format; Windows keeps the data, so no serving |
| Fonts | `fc-match`, then known paths | `C:\Windows\Fonts\segoeui.ttf` and `segoeuib.ttf` |
| Config, save folder | `~/.config/clipt`, `~/Pictures` | `%APPDATA%\clipt\config.toml`, the Pictures known folder |
| Notifications | D-Bus | The tray icon's balloon (toasts need a registered app ID) |
| Tray | StatusNotifierItem over D-Bus | `Shell_NotifyIcon` |
| Hotkey | The desktop's own shortcut settings | `RegisterHotKey`, held by the tray process |
| Recording control | pid file, FIFO, signals | Named pipe `\\.\pipe\clipt-record`, named mutex as the lock |
| Frames for recording | screencopy, PipeWire, X11 SHM | DXGI Desktop Duplication (changed frames only, cursor separately) |
| Video encoder | VA-API, NVENC, libx264 | `h264_mf` (Media Foundation, any GPU), NVENC, QSV, AMF, libx264 |
| Sound | PulseAudio, the output's monitor | WASAPI: loopback on the default output, capture on the default microphone |

## Decisions

| Question | Decision | Why |
|---|---|---|
| Bindings | `windows-sys` (Microsoft's raw Win32 bindings); `windows` only if WinRT is needed | Calls DLLs every Windows has, so the no-new-C-libraries rule holds; `windows-sys` compiles quickly and adds little |
| Capture | DXGI Desktop Duplication, GDI fallback | Windows 8+, native pixels per monitor, and the same API records. Windows.Graphics.Capture draws a yellow border before Windows 11 and needs WinRT. GDI covers remote desktop sessions, where duplication can fail |
| Overlay | One window per monitor, not one across the virtual desktop | Monitors can have different scales; per-monitor-v2 DPI awareness gives each window its own, as the Wayland overlay does per output |
| Linux code | Behind `cfg(unix)` (and its crates under `[target.'cfg(unix)'.dependencies]`) | wayland-client, x11rb, zbus, rustix and libc don't build for Windows |
| Toolchain | `x86_64-pc-windows-msvc`, built on CI's `windows-latest` | The standard target; Windows on ARM (`aarch64-pc-windows-msvc`) later if someone needs it |
| Windows versions | 10 (22H2) and 11 | 10 is out of support but still common at work; nothing in the plan needs 11 |
| ffmpeg | Found on `PATH` or beside `clipt.exe`; not bundled | As on Linux: optional, and only recording needs it. `winget install Gyan.FFmpeg` installs it |

The current `src/windows.rs` (the window list) would be renamed
`src/window_list.rs` so the Windows host can live in `src/win32/`.

## Phases

Each phase ends with a build the tester can run, tests in CI, and a PR.

**0. It builds.** The Linux-only code and crates behind `cfg(unix)`; on
Windows, `clipt` says it isn't supported yet. A CI job on `windows-latest`
runs `cargo clippy` and `cargo test` (the editor's and encoder's tests),
and uploads `clipt.exe`. Nothing for the tester yet, except checking that
the exe starts on their machine (see *At work*).

**1. Screenshots.** Per-monitor DPI awareness; monitors and capture through
DXGI with the GDI fallback; the overlay windows feeding the editor (mouse,
keys, the cursor shapes it asks for); the window picker; copy, save, the
config file and fonts. `clipt` runs from a shortcut or the command line.
The biggest phase: probably 1 500–2 500 new lines.

**2. Tray and hotkey.** `clipt tray` with the Linux tray's menu, holding a
global hotkey (default Print Screen, configurable). Windows 11 gives Print
Screen to the Snipping Tool by default; the tray says so if it can't
register the key, and the README says where to turn that off.

**3. Recording.** DXGI duplication as a recording source; the writer and
camera as they are; encoder candidates `h264_mf`, NVENC, QSV, AMF, then
libx264; `--stop`, `--pause` and the overlay controls over the named pipe;
the frame indicator as a click-through topmost window.

**4. Sound.** WASAPI loopback and microphone capture, fed to the writer as
the PulseAudio sources are now (48 kHz, 16-bit stereo, stamped by sample
count). `--audio`, volumes, separate tracks and `duck` then work unchanged.

**5. Release.** A zip on GitHub releases (`clipt-vX-x86_64-windows.zip`,
done: `.github/scripts/pack-windows.sh`), the updater replacing a running
exe (rename, then copy), and winget or Scoop manifests if people want them.

## Testing

The e2e suites run desktops in Docker, which can't host Windows. Instead:

- **CI:** `windows-latest` builds, lints and runs the unit tests on every
  push. Capture can be smoke-tested there too (the runner has a desktop
  session), saving a PNG and checking its size.
- **The tester:** for each phase, a CI build and a short checklist in the
  PR: one and two monitors, a scaled monitor (125 % or 150 %), mixed
  scales, a window behind another, copy into a chat app and an email,
  save, and for recording, a minute with system sound and the microphone.
  Results go in the PR, as the e2e results do now.

## At work

Things that can stop a work machine before any code runs. Worth asking the
tester to check with a phase 0 build:

- **SmartScreen and unsigned code.** An unsigned exe from the internet gets
  a "Windows protected your PC" warning; some company policies (AppLocker,
  WDAC, Defender settings) block it outright. Most open-source tools ship
  unsigned. If signing becomes worth it, the SignPath Foundation signs open
  source projects for free, and Azure Artifact Signing costs about US$10 a
  month.
- **Screen capture policy.** Some security software blocks or blanks
  screen capture; DXGI and GDI would both show it.
- **No admin rights.** The plan needs none: no installer, no driver, no
  service.

## Open questions

- HDR monitors: Desktop Duplication returns 16-bit float frames when HDR is
  on, which need tone-mapping to look right (GDI hands over SDR, already
  mapped). Phase 3 should measure what the tester's hardware gives.
- Whether the tester's machines need Windows on ARM.
