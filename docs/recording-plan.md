# Video recording: implementation plan

Status: phase 0 (spike) and phase 1 (pipeline, screen/region on wlroots) done, 2026-11-01.

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
 capture source ──frames──▶ camera ──canvas frames──▶ writer ──▶ ffmpeg stdin ──▶ file.mp4
 (ext / wlr / PipeWire /    (crop, fit,              (constant fps,
  X11, own thread)           ease, pad)               change; drops)  
```

- **Source** (`src/record/source/*.rs`): one trait, `next_frame() -> Frame`
  where `Frame { width, height, stride, format, data, rect_hint }`. Each
  backend runs on its own thread and keeps only the newest frame (a slot,
  not a queue), so a slow encoder drops frames instead of building up memory.
- **Camera** (`src/record/camera.rs`): pure function of (frame size, canvas
  size, time) → transform, so it's unit-testable. See below.
- **Writer** (`src/record/mod.rs`): hands the newest frame to ffmpeg at most
  `fps` times a second, only when the screen changed. ffmpeg stamps frames
  with their arrival time (`-use_wallclock_as_timestamps`) and keeps those
  times (`-fps_mode passthrough`), so the output is variable frame rate: a
  still screen costs nothing, and a slow encoder drops frames instead of
  slowing the video. (Constant frame rate was the first plan; ffmpeg's cfr
  mode padded the end of each recording by over a second.) At stop the last
  frame is sent twice, a frame apart, so a still ending isn't cut short.
- **Encoder** (`src/record/encode.rs`): builds the ffmpeg command line,
  probes it once at start and falls back step by step:
  1. `h264_vaapi` (AMD, Intel): `-vaapi_device /dev/dri/renderD128 -vf
     hwupload,scale_vaapi=format=nv12` (BGRA → NV12 on the GPU; doing it in
     swscale costs 3.5× the CPU, see spike results).
  2. `h264_nvenc` (NVIDIA proprietary driver, which has no VA-API encode):
     takes BGRA directly and converts on the GPU.
  3. `libx264 -preset veryfast -crf 23 -pix_fmt yuv420p`; at 4K without a GPU
     encoder, drop to `ultrafast` or 24 fps rather than fall behind.
  4. No ffmpeg: a clear error that names the package to install.
  Each step is probed with a one-frame test encode, so the choice is
  automatic; `record_codec` overrides it.
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
- Scaling is a purpose-built separable bilinear downscale (fixed point,
  rows split across threads), not tiny-skia: tiny-skia's general pipeline
  took 64 ms per 4K frame, the dedicated one 9 ms on 4 threads (spike
  results). It only runs while the window doesn't fit at 1:1; the common
  case is a plain row copy (3.6 ms at 4K).

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
  `video_dir` (default `~/Videos/Screencasts`), named like screenshots.
- Config (phase 1 has the first five): `video_dir`, `video_filename`,
  `record_fps`, `record_codec` (`auto`/`vaapi`/`nvenc`/`x264`),
  `max_duration`; later `record_format` (`mp4`/`gif`) and
  `record_background`.

## Phases

Each phase ends with something usable, tests in CI, and a PR.

**0. Spike (done 2026-10-31).** Results below. The pipe is fast enough, so
no memfd ring; the camera needs its own scaler. Capture-loop rates were not
measured: headless Sway renders in software, so its numbers wouldn't say
anything about real desktops. Phase 1 measures them on real compositors.

**1. Pipeline + screen/region on wlroots (done 2026-11-01).** Capture loops
in `capture/wayland.rs` (`Wayland::stream`), writer and encoder in
`src/record/`. Regions must lie within one output for now. Still to measure:
capture rates on real (GPU) wlroots compositors. Original scope: source
trait, ext and wlr sources, pacer, encoder with fallback,
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

## Spike results (2026-10-31)

Ryzen 7 3800X (8 cores), Radeon RX 9070 XT (radeonsi VA-API), ffmpeg 9.0.2.
Raw BGRA frames written to ffmpeg's stdin as fast as it takes them, 10 s of
30 fps input. "Real" rows use `docs/screenshot.png` scaled to 4K and
scrolling; synthetic rows compress unrealistically well.

| Test | Throughput | CPU |
|---|---|---|
| 4K pipe only (`-f null`) | 70 fps | ffmpeg 78 %, writer 51 % of a core |
| 4K `h264_vaapi`, NV12 conversion on GPU | 69 fps (pipe-bound); 76 fps real | ffmpeg 73 % |
| 4K `h264_vaapi`, NV12 conversion in swscale | 62 fps | ffmpeg 266 % |
| 4K `av1_vaapi`, GPU conversion | 68 fps | ffmpeg 72 % |
| 4K `libx264 veryfast`, real | 42 fps | ~5 cores |
| 4K `libx264 ultrafast`, real | 60 fps | ~4 cores, 4× the file size |
| 1080p `h264_vaapi` / `libx264 veryfast` | 286 / 216 fps | 72 % / ~6 cores |

So at 4K 30 fps with VA-API, recording costs about half of one core for the
pipe plus encoder. Without a GPU encoder, 4K costs several cores and 1080p
is comfortable.

Camera scaling, per frame, single process:

| Operation | Time |
|---|---|
| 4K plain row copy | 3.6 ms |
| tiny-skia bilinear draw, 4K → 4K canvas at 0.8 | 64 ms |
| tiny-skia, 2560×1440 → 1920×1080 | 24.5 ms |
| Dedicated separable bilinear, 4K → 3072×1728, 1 / 4 threads | 34 / 9.3 ms |
| Dedicated, 2560×1440 → 1920×1080, 1 / 4 threads | 13.3 / 3.8 ms |

The dedicated scaler still recomputes each source row twice; caching the
previous row should roughly halve it.

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
QSV/AMF-specific encoders (VA-API covers Intel and AMD), and recording that keeps
running after clipt's process exits.
