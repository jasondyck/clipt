# Video recording: implementation plan

Status: phases 0–4 done, phase 5 done except recording in the Flatpak (X11, GIF, ScreenCast portal), 2026-11-01.

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
 (ext / wlr / PipeWire /    (crop, fit,              (on change,
  X11, own thread)           ease, pad)               ≤ fps, drops)
```

- **Source** (`src/record/source/*.rs`): one trait, `next_frame() -> Frame`
  where `Frame { width, height, stride, format, data, rect_hint }`. Each
  backend runs on its own thread and keeps only the newest frame (a slot,
  not a queue), so a slow encoder drops frames instead of building up memory.
- **Camera** (`src/record/camera.rs`): pure function of (frame size, canvas
  size, time) → transform, so it's unit-testable. See below.
- **Writer** (`src/record/mod.rs`): hands the newest frame to ffmpeg at most
  `fps` times a second, only when the screen changed, stamped with its
  capture time minus any paused time. Frames travel in a minimal Matroska
  stream (`record/mkv.rs`: `V_UNCOMPRESSED`, one cluster per frame) so the
  timestamps are clipt's own, and the output keeps them (`-fps_mode
  passthrough`, `-enc_time_base 1/1000`): variable frame rate, so a still
  screen costs nothing and a slow encoder drops frames instead of slowing
  the video. At stop the last frame is sent again so a still ending isn't
  cut short; the track's DefaultDuration gives it one frame interval.
  What didn't work, in order: constant frame rate (ffmpeg's cfr mode padded
  each recording's end by over a second); raw frames stamped on arrival
  (`-use_wallclock_as_timestamps`: frames read late during input probing or
  under load were stamped late, up to a second around sparse frames); and
  ffmpeg's own time base (picked from its frame-rate guess, it snapped
  sparse timestamps to a 0.48 s grid).
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
  `$XDG_RUNTIME_DIR/clipt-record.pid`, which the recorder holds under a
  flock; `clipt record --stop` and the tray write a command to the
  `clipt-record.ctl` FIFO beside it, which ends the stream, closes ffmpeg's
  stdin and waits for it to finish the file. (They first sent SIGINT to the
  pid, which can't work across Flatpak instances; see phase 5.) Ctrl-C in
  a terminal does the same. A `max_duration` safety limit (default 1 h) and
  a disk-space check every few seconds.

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

**2. Selection UI, indicator, tray, pause (done 2026-11-01).** Record mode
in the editor (`Editor::record`), `record/indicator.rs` (raw layer-shell
strips), tray state from the pidfile. Pause/resume added on request:
SIGUSR1/SIGUSR2; the signal handler notes the time, and paused time is
subtracted from later timestamps, so one ffmpeg runs throughout. (The first
version used one ffmpeg segment per stretch, joined with the concat
demuxer, until timestamps became clipt's own.) The `--delay` countdown was left out: the tray already
waits for its menu to close, and a countdown needs text on the frame.
Original scope: Record mode in the
overlay, the border indicator, tray items and red icon, `--delay`
countdown. CI: overlay record mode driven by the existing e2e input.

**3. Window recording + camera (done 2026-11-01).** `Wayland::stream_window`
(ext-foreign-toplevel list + image capture source; frames that fail with
buffer_constraints on a resize are retried at the new size),
`record/follow.rs` (Sway/Hyprland IPC polled at 10 Hz, output crop updated
live), `record/camera.rs` (spring, fit, threaded separable scaler). The
overlay reports a clicked window via `Editor::selected_window`. Differences
from the plan: window ids are matched by app id and title (ext toplevel
handles carry no geometry); window capture gives the window's own pixels,
so a 1x client on a 1.5x output records at 1x; the spring caps each step at
50 ms so a resize after a still stretch glides instead of jumping; no frame
indicator around a recorded window yet. Tests: `examples/testwin.rs` (a
resizable test window); the Sway suite records it with `-b ext` and
`-b wlr`, widens it, moves it, and checks size, zoom and that the zoom
passes through several sizes. Original scope: `Window::id`, ext toplevel
source, crop-follow for Sway/Hyprland/X11, `camera.rs`. Unit tests for the
spring and the fit maths. CI: Sway e2e resizes the test window mid-recording
(`swaymsg resize`); `ffprobe` shows a constant size and decoded frames show
the window shrinking smoothly over several frames, not in one.

**4. PipeWire: KDE and GNOME (done 2026-11-01).** `capture/pipewire.rs`
reads a PipeWire node into `VideoFrame`s; `capture/kwin_cast.rs`
(zkde_screencast: output, region via `stream_region`, window by KWin's
internal id) and `capture/mutter_cast.rs` (org.gnome.Mutter.ScreenCast:
RecordMonitor, RecordArea) get the nodes. Changes from the plan:
- **libpipewire is dlopen'ed, not linked.** The first version used the
  `pipewire` crate; the e2e suites showed that a linked libpipewire stops
  clipt from starting at all (screenshots included) where it's missing. So
  `pipewire.rs` has its own small FFI (18 functions) and a minimal SPA pod
  encoder/parser for format negotiation, with struct layouts checked
  against the 0.3 headers. Side effect: no PipeWire headers, clang or
  bindgen at build time.
- **GNOME uses Mutter's ScreenCast directly**, not the portal: no dialog,
  and RecordArea crops for us. The portal (with a restore token) moves to
  phase 5, where the Flatpak needs it.
- GPU encoders run without B-frames: with VFR, h264_vaapi's reordered
  timestamps gave MP4s a wrong duration (0.13 s for a 1.8 s recording).
- `clipt record window APP` picks a window by app id without the overlay
  (scripts, hotkeys, and testing KDE).
Tested live on Plasma 6.8 (screen, region, window, through PipeWire with
BGRx memfd buffers) and in the GNOME suite (PipeWire and WirePlumber added
to its image: screen and area recordings, first frame against a
screenshot, a window opened mid-recording). Not yet measured: KDE frame
rates (the test session's display was asleep).
Original scope: `pipewire` feature, a
shared PipeWire stream reader (SHM/memfd buffers first, DMA-BUF later),
KDE `zkde_screencast` for screen/region/window, the ScreenCast portal for
GNOME and Flatpak with a restore token so the dialog appears once. CI:
GNOME e2e (add PipeWire to its container); KDE checked with
`kwin_wayland --virtual` and on a real Plasma session.

**5. X11, GIF, portal (done 2026-11-01; Flatpak recording open).**
- X11 (`x11/record.rs`): MIT-SHM `GetImage` into shared memory, taken only
  on DAMAGE events (with the pointer included, every frame interval, since
  the hardware cursor moving doesn't damage the screen); windows followed by
  position through the EWMH list (`record/follow.rs`). No red frame on X11:
  its override-redirect windows would be captured whenever the region
  touches them.
- GIF: the recording is made as a hidden MP4 and converted at stop with
  `palettegen`/`paletteuse` (at most 15 fps), because a single-pass palette
  makes ffmpeg hold every frame in memory.
- ScreenCast portal (`capture/portal_cast.rs`): CreateSession,
  SelectSources (persist_mode 2, restore token in
  `~/.local/state/clipt`), Start, OpenPipeWireRemote; regions are cropped
  inside the monitor the user shares. Tested in the GNOME suite by clicking
  Share, then recording again with the token and no dialog.
- Found on the way: libx264's B-frames gave sparse VFR recordings a short
  MP4 duration too (as h264_vaapi's had), so every encoder now runs with
  `-bf 0`.
- Found on the way: control by pid can't work in the Flatpak. Each `flatpak
  run` has its own PID namespace whose first process is pid 2, so the
  recorder's pid meant nothing to `clipt record --stop`, which would have
  signalled itself, and a leftover pidfile always looked live. The
  recorder now holds a flock on the pidfile (locks cross namespaces) and
  reads one-byte commands from a FIFO beside it; SIGINT, SIGTERM and
  SIGUSR1/2 still work.
- Open: **recording in the Flatpak.** The plan said ffmpeg would come from
  the `org.freedesktop.Platform.ffmpeg-full` extension, but that extension
  provides ffmpeg's libraries, not the `ffmpeg` program clipt runs. Options:
  build ffmpeg as a module of the Flatpak (simple, slow builds, larger
  download), or encode through libavcodec directly (no subprocess, more
  code).

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

Audio, webcam overlay, the studio mode, editing or trimming,
QSV/AMF-specific encoders (VA-API covers Intel and AMD), and recording that keeps
running after clipt's process exits.
