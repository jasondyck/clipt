# macOS: plan and status

Status, 2026-11-03: phase 1 (screenshots) shipped in 0.6.0 and has run on
one real Mac, where the overlay's lag was found and fixed. Treat it as a
preview until a tester has worked through the checklist below.

## Goal

The same clipt on macOS 12 and later, on Apple silicon and Intel: select a
region, a window or a display, annotate, copy or save; later, record. One
binary, nothing to install beside it except ffmpeg for recording.

## How it fits

The editor (`src/editor/`), images, text, the config and PNG output are
shared. macOS gets its own host in `src/macos/`, beside the Wayland and
X11 ones, and the Linux-only code and crates are behind
`cfg(not(target_os = "macos"))`.

| Part | Linux | macOS |
|---|---|---|
| Displays | Wayland outputs, RandR | `CGGetActiveDisplayList`, `CGDisplayBounds`, names from `NSScreen` |
| Screenshots | `capture/` | `CGDisplayCreateImage`, drawn into sRGB; `screencapture` as the fallback and for `--cursor` |
| Showing the editor | `overlay.rs`, `x11/overlay.rs` | `macos/overlay.rs`: a borderless window per display at screen-saver level, a layer showing the editor's bitmap |
| Window picker | `windows/linux.rs` | `CGWindowListCopyWindowInfo` (layer 0, frontmost first) |
| Clipboard | Wayland and X11 selections, which clipt serves | `NSPasteboard` (PNG and TIFF); the system keeps it, so clipt exits at once |
| Fonts | `fc-match` | Arial from `/System/Library/Fonts/Supplemental` |
| Notifications | D-Bus | `osascript` (`display notification`) |
| Shortcuts | ⌃C ⌃S ⌃Z | ⌘C ⌘S ⌘Z (⌃ works too) |

Decisions:

| Question | Decision | Why |
|---|---|---|
| Bindings | `objc2` and its framework crates | Safe, maintained bindings to frameworks every Mac has, so the no-new-C-libraries rule holds |
| Capture | Core Graphics, not ScreenCaptureKit | Synchronous, no run loop or async completion handlers, and on every macOS clipt targets. It's deprecated, so it's looked up at run time (`dlsym`), and `screencapture` takes over if it ever goes. Recording will need ScreenCaptureKit anyway |
| Event loop | clipt's own `nextEventMatchingMask` loop | `run` returns when the user decides, like the other hosts |
| Permission | Checked first (`CGPreflightScreenCaptureAccess`), with the system prompt on the first run | Without it macOS returns screenshots with every window blank, which looks like a bug |

## Phases

**1. Screenshots (this one).** `clipt`, `clipt full`, `clipt screen`,
`clipt list`, copy, save, notifications, window highlighting. CI builds a
universal binary (`clipt-macos` artifact on each run of the `macos` job).

**2. Menu bar icon and a hotkey.** `clipt tray` (`src/macos/tray.rs`), and
what opening clipt.app with no arguments does: an `NSStatusItem` with the
Linux tray's screenshot actions, Open at Login (`SMAppService`, macOS 13+,
from the app only), and global shortcuts through `RegisterEventHotKey`,
which needs no Accessibility permission: `hotkey` (a region, default
`cmd+shift+2`) and `hotkey_full` (the whole desktop, off) in the config.
Opening the app again while it runs captures a region. Each action starts a
fresh `clipt`, as on Linux. The recording items come with phase 3.

**2b. Whole windows in screenshots.** Clicking a window the frozen picture
hides behind another fetches that window on its own (ScreenCaptureKit's
`SCScreenshotManager` on macOS 14+, `CGWindowListCreateImage` before) and
paints it over the picture, so the saved image shows it whole. Selecting
anything else puts the screen back. Rounded corners still let a little of
the covering window through; capturing the window together with what's
really behind it is a follow-up.

**3. Recording.** Done; sound is step 4. `src/macos/record.rs`:
ScreenCaptureKit (`SCStream`, macOS 12.3+, weakly linked so clipt still
starts on 12.0) for frames, handed to the same writer as Linux through
`record::Feed`. A display is cropped by `sourceRect`, at the display's
pixel scale; a window is captured on its own (`desktopIndependentWindow`),
so it records while covered, at the size it had when recording started.
ffmpeg's `h264_videotoolbox` (constant-quality mode needs Apple silicon;
Intel Macs fall back to libx264), found on PATH or where Homebrew and
MacPorts put it. `--stop` and `--pause` use the same pid file and FIFO as
Linux, in the user's own temporary directory (`_CS_DARWIN_USER_TEMP_DIR`).
The menu bar icon has Record Region, Record Screen, and while recording,
Pause and Stop, and turns into a red record symbol. Frames arrive as NV12
(420v), the encoder's own format. Not yet: a frame around the area being
recorded (the main thread is busy writing, and AppKit draws only there).

Known limitation: while a whole Retina display is recorded, a video
playing in an app that isn't frontmost on that display stutters, on
screen and in the recording; QuickTime's recording of the same screen
doesn't. It happens with the stream alone, every frame dropped on arrival,
so it isn't the copying or the encoder. clipt is aimed at recording UI
interactions, which it records smoothly.

**4. Sound.** Done. `src/macos/sound.rs`, behind the same `check`, `open`
and `Capture::run` as PulseAudio's. `system` is ScreenCaptureKit's audio
(macOS 13+) from the picture's own stream (`capturesAudio`), leaving
clipt's own sound out; `mic` is the default input through an Audio Queue,
which converts to 48 kHz 16-bit stereo itself (`NSMicrophoneUsageDescription`
in Info.plist; macOS asks once). The Developer ID builds' hardened runtime
also needs the `com.apple.security.device.audio-input` entitlement
(`packaging/macos/clipt.entitlements`, given by `macos-sign.sh`), or the
microphone is refused without asking. The computer's sound needs an
`Audio` output added to the stream beside the `Screen` one; `capturesAudio`
alone delivers nothing. Not yet: devices by name, and `clipt list sound`.

**App bundle.** `packaging/macos/build-app.sh` wraps the binary in
`clipt.app` (bundle ID `io.github.jasondyck.clipt`, no Dock icon, the icon
drawn by `clipt debug-icon -o clipt.iconset` and packed by `iconutil`). macOS then files the Screen
Recording permission under clipt itself, not the terminal or launcher that
ran it, and the app can live in Applications and Login Items. CI uploads it
as `clipt-app.zip` inside the `clipt-macos` artifact; releases carry
`clipt-vX.Y.Z-macos-app.zip`, notarised with the bare binary in one
submission and stapled. Opened from Finder it starts the editor; once the
menu bar icon exists it will start that instead. Keep the bundle ID: a
new one makes macOS ask every user for the permission again.

**5. Release.** Done ahead of the others: the release workflow adds a
signed and notarised universal tarball (`clipt-vX.Y.Z-universal-macos.tar.gz`)
to each GitHub release, and `clipt update` installs it.

## Signing

The `macos` CI job signs the binary with the project's Developer ID
(hardened runtime, secure timestamp) and has Apple notarise it, so
Gatekeeper lets a downloaded copy run. It needs five repository secrets:
`MACOS_CERT_P12` (the Developer ID Application certificate and its key, as
base64), `MACOS_CERT_PASSWORD`, and the App Store Connect API key used for
notarisation, `MACOS_NOTARY_KEY` (the `.p8` file's text),
`MACOS_NOTARY_KEY_ID` and `MACOS_NOTARY_ISSUER`. Without them (pull
requests from forks), the build is ad-hoc signed and a downloaded copy
needs `xattr -d com.apple.quarantine clipt` before it runs. A bare binary
can't carry its notarisation ticket, so the first run checks with Apple
online.

The job waits at most ten minutes for Apple, then uploads the signed build
with a warning naming the submission. To see where a submission stands,
run the `notary` workflow (Actions → notary → Run workflow), with or
without a submission id.

## Testing

CI can't show a screen to anyone, so phase 1 needs a person with a Mac:

1. Download `clipt-macos` from the latest run of the `macos` CI job, unzip
   it, and `chmod +x clipt`. (An ad-hoc signed build, from a run without
   the signing secrets, also needs `xattr -d com.apple.quarantine clipt`.)
2. `./clipt --version` and `./clipt list`: each display, with its size.
3. `./clipt full -o test.png`: the first time, macOS asks to allow Screen
   Recording for the terminal; allow it, quit and reopen the terminal, run
   it again. The PNG should show every window, at full Retina resolution.
4. `./clipt`: the screen freezes on every display. Check that the pointer
   highlights windows, a drag selects, the toolbar's tools draw, ⌘Z undoes,
   Return copies (paste into Notes, Mail, Messages, Slack), ⌘S saves to
   `~/Pictures/Screenshots`, Esc cancels.
5. Keys work without clicking first. If they only work after a click,
   that's macOS refusing clipt focus, and worth reporting.
6. With two displays, especially a Retina one next to a non-Retina one:
   selections on each and across both.
7. A full-screen app on its own Space: the overlay should cover it.

Report what you see in the PR or an issue, with `CLIPT_DEBUG=1` output for
anything odd.
