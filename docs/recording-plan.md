# Video recording: implementation plan

Status: proposed, 2026-10-31. Nothing here is built yet.

## Goal

`clipt record` records the screen, a region or a window to MP4 (or GIF),
staying as light as the screenshot side. Window recordings follow the window
and handle resizes smoothly instead of jumping. No audio in the first version.

## Decisions

| Question | Decision | Why |
|---|---|---|
| Frames on KDE/GNOME | libpipewire via the `pipewire` crate, behind a default-on `pipewire` cargo feature | The only stream either desktop offers; GStreamer and OBS use the same library underneath. The feature lets musl/armv7/riscv64/FreeBSD builds and minimal packagers leave it out |
| Frames on wlroots/niri | ext-image-copy-capture sessions, wlr-screencopy as fallback | Already spoken for screenshots; ext also captures a single window (toplevel source) |
| Frames on X11 | MIT-SHM GetImage on a timer | Already used for screenshots |
| Encoder | `ffmpeg` subprocess, optional at runtime | Hardware H.264 (VA-API) with a libx264 fallback and nothing added to the binary. No pure-Rust encoder is real-time at 4K |
| Resizing windows | Virtual camera: fixed canvas, eased scale-to-fit | Smooth instead of the OBS-style jump; never upscales |
| Audio | None for now | ffmpeg can add a PipeWire/Pulse input later |

## How it fits together

```
 capture source ──frames──▶ camera ──canvas frames──▶ pacer ──▶ ffmpeg stdin ──▶ file.mp4
 (ext / wlr / PipeWire /    (crop, fit,              (constant fps,
  X11, own thread)           ease, pad)               drop when behind)
```

- **Source** (`src/record/source/*.rs`): one trait, `next_frame() -> Frame`
  where `Frame { width, height, stride, format, data, rect_hint }`. Each
  backend runs on its own thread and keeps only the newest frame (a slot,
  not a queue), so a slow encoder drops frames instead of building up memory.
- **Camera** (`src/record/camera.rs`): pure function of (frame size, canvas
  size, time) → transform, so it's unit-testable. See below.
- **Pacer** (`src/record/pacer.rs`): ticks at the target fps (30 by default)
  and sends the newest canvas frame to ffmpeg, repeating the last one when
  nothing changed. That makes the output constant frame rate, which keeps the
  ffmpeg side a plain `-f rawvideo` pipe with no timestamps to manage.
- **Encoder** (`src/record/encode.rs`): builds the ffmpeg command line,
  probes it once at start and falls back step by step:
  1. `h264_vaapi`: `-vaapi_device /dev/dri/renderD128 -vf hwupload,scale_vaapi=format=nv12`
     (BGRA → NV12 on the GPU, not in swscale).
  2. `libx264 -preset veryfast -crf 23 -pix_fmt yuv420p`.
  3. No ffmpeg: a clear error that names the package to install.
  Output `-movflags +faststart`. Canvas sizes are rounded up to even numbers
  (H.264 needs them).
- **Control** (`src/record/mod.rs`): one recording at a time. A pidfile in
  `$XDG_RUNTIME_DIR/clipt-record.pid`; `clipt record --stop` and the tray
  send SIGINT, which ends the stream, closes ffmpeg's stdin and waits for it
  to finish the file. Ctrl-C in a terminal does the same. A `max_duration`
  safety limit (default 1 h) and a disk-space check every few seconds.

## Window recording and the smooth camera

**Following the window** is done by the capture side wherever it can be:

| Desktop | Window source | Follows moves/resizes | Occluded parts |
|---|---|---|---|
| Sway 1.11+, niri, COSMIC, Hyprland 0.50+ | ext toplevel image capture source (via `ext_foreign_toplevel_list_v1`) | yes, by the compositor | captured correctly |
| KDE | `zkde_screencast_unstable_v1.stream_window(uuid)` over PipeWire | yes | captured correctly |
| GNOME, other portals | ScreenCast portal, source type window (chosen in the portal's dialog) | yes | captured correctly |
| wlr-screencopy only (labwc, river, Wayfire), X11 | capture the monitor and crop to the window's rectangle, tracked by Sway/Hyprland IPC events or X11 `ConfigureNotify` | yes, by clipt | whatever covers the window is recorded too (documented limitation) |

`windows::Window` gains an `id` (an enum: ext toplevel handle, KWin uuid,
Sway con_id, Hyprland address, X11 XID) so the overlay's window pick can be
handed to the recorder.

**The camera** turns a window of changing size into a fixed-size video:

- The canvas is the window's size when recording starts (logical size times
  the output scale, rounded to even).
- Each frame, the target scale is `min(1, canvas_w / w, canvas_h / h)`:
  shrink a window that outgrew the canvas, never enlarge one that got
  smaller, so text stays sharp.
- The current scale eases toward the target with a critically damped spring
  (~200 ms settle), the same easing the hover highlight uses. A drag-resize,
  which changes the size every frame, reads as a smooth zoom instead of
  flicker.
- The window is centred; spare canvas is a neutral background
  (`record_background`, default dark grey).
- Scaling uses tiny-skia's bilinear pattern draw (already a dependency). It
  only happens while the window doesn't fit at 1:1; the common case is a
  plain copy.

Later (not in this plan): a "studio" mode that records at native size plus
the window rectangle per frame, then renders the final framing on stop, so
nothing is ever cut off.

## User interface

- CLI: `clipt record [screen [NAME] | region | window] [-g GEOM] [-o FILE]
  [--fps N] [--gif] [--cursor] [-d DELAY]`, plus `clipt record --stop`.
  `clipt record` with no mode opens the overlay.
- Overlay: the existing selection UI with a record flag. Pick a region, a
  monitor or (with the hover highlight) a window. Enter starts recording.
  No annotation tools in this mode.
- While recording: a thin red border just *outside* the region (layer-shell
  surface with an empty input region so clicks go through; X11: four
  override-redirect windows). Not drawn for whole-screen recordings, where
  it would end up in the video. The tray icon turns red; clicking it stops.
- Tray menu: *Record region…*, *Record window…*, *Record screen*; *Stop
  recording* while one runs.
- When done: a notification with the file, like screenshots. Saved to
  `video_dir` (default `~/Videos/clipt`), named like screenshots.
- Config: `video_dir`, `record_fps`, `record_format` (`mp4`/`gif`),
  `record_codec` (`auto`/`vaapi`/`x264`), `record_cursor`,
  `record_background`, `max_duration`.

## Phases

Each phase ends with something usable, tests in CI, and a PR.

**0. Spike (1–2 days).** Measure before building: ext-image-copy-capture
session loop at 4K on headless Sway (fps, CPU), throughput of BGRA frames
through a pipe into `h264_vaapi` and `libx264`, and the cost of one
tiny-skia 4K scaled draw. If a 4K pipe can't keep 30 fps, pass frames through
a memfd ring plus `-f rawvideo -i /proc/self/fd/N` instead.

**1. Pipeline + screen/region on wlroots (about 1 week).** `src/record/`
with source trait, ext and wlr sources, pacer, encoder with fallback,
pidfile control, `clipt record screen` and `-g`. Notification on finish.
CI: Sway e2e records 3 s, `ffprobe` checks duration, fps and size, and a
decoded frame shows the test pattern.

**2. Selection UI, indicator, tray (about 1 week).** Record mode in the
overlay, the border indicator, tray items and red icon, `--delay`
countdown. CI: overlay record mode driven by the existing e2e input.

**3. Window recording + camera (about 1 week).** `Window::id`, ext toplevel
source, crop-follow for Sway/Hyprland/X11, `camera.rs`. Unit tests for the
spring and the fit maths. CI: Sway e2e resizes the test window mid-recording
(`swaymsg resize`); `ffprobe` shows a constant size and decoded frames show
the window shrinking smoothly over several frames, not in one.

**4. PipeWire: KDE and GNOME (about 1–2 weeks).** `pipewire` feature, a
shared PipeWire stream reader (SHM/memfd buffers first, DMA-BUF later),
KDE `zkde_screencast` for screen/region/window, the ScreenCast portal for
GNOME and Flatpak with a restore token so the dialog appears once. CI:
GNOME e2e (add PipeWire to its container); KDE checked with
`kwin_wayland --virtual` and on a real Plasma session.

**5. X11, GIF, polish (about 1 week).** X11 source, GIF via ffmpeg's
`palettegen`/`paletteuse`, config keys, README and CHANGELOG, Flatpak
manifest (ffmpeg from the `org.freedesktop.Platform.ffmpeg-full`
extension), and cross-target CI with `--no-default-features` where PipeWire
can't build.

## Risks

- **4K CPU cost**: BGRA through a pipe is ~1 GB/s at 30 fps. Mitigated by the
  phase 0 measurement, the memfd fallback, and handing DMA-BUFs to VA-API in
  a later step.
- **VA-API varies by driver**: probe with a one-frame test encode at start
  and fall back to libx264 silently (logged with `CLIPT_DEBUG`).
- **Fractional scaling**: capture buffers are native pixels, window
  geometry is logical; the camera works in buffer pixels throughout.
- **PipeWire cross-compiling**: feature off for musl/armv7/riscv64/FreeBSD
  release targets; those get wlroots and X11 recording only.
- **GNOME's dialog**: unavoidable the first time; restore tokens keep it to
  once.
- **Flatpak**: needs the ffmpeg extension and the portal path; zkde_screencast
  is hidden from the sandbox like the other privileged globals.

## Not in this plan

Audio, webcam overlay, pause/resume, the studio mode, editing or trimming,
hardware encoders other than VA-API (NVENC), and recording that keeps
running after clipt's process exits.
