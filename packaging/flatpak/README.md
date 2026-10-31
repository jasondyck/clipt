# Flatpak (prototype)

Not published. This exists to find out what clipt loses in a sandbox.

**Where it fits:** KDE Plasma and GNOME, where the Flatpak behaves like a
native install. On wlroots compositors (Sway, labwc, Hyprland, river, …) and
niri, install natively (install.sh, AUR, Nix) unless you're willing to use
`clipt-launch` below.

    packaging/flatpak/build.sh [--bundle]   # build, install for this user
    tests/flatpak/probe.sh                  # native vs Flatpak, per compositor
    packaging/flatpak/cargo-sources.py      # after any Cargo.lock change

## What the sandbox changes

Flatpak connects apps through `wp_security_context_v1`, and compositors hide
privileged globals from such clients. `probe.sh` on 2026-10-31:

| Compositor | Native | Flatpak | Flatpak via `clipt-launch` |
|---|---|---|---|
| Sway | ext + wlr capture, layer-shell, Sway IPC windows | no screencopy, no layer-shell, no IPC socket: **portal only** | same as native, minus window selection (IPC socket) |
| labwc | ext + wlr capture, layer-shell | only `xdg_wm_base` left: **portal only** | same as native |
| niri | wlr capture, layer-shell | only `xdg_wm_base` left: **portal only** | same as native |
| KWin (headless) | ScreenShot2, layer-shell, KWin script windows | **same as native** | not needed |
| GNOME | portal, xdg-shell overlay | same as native (portal is all GNOME offers anyone) | not needed |
| X11 | everything | expected the same: X11 can't sandbox (not probed) | not needed |

On wlroots/niri the portal (xdg-desktop-portal-wlr) is the only way to
capture, and the overlay falls back to fullscreen xdg-shell windows, one per
monitor. The portal backend wasn't probed (no portal backend in the headless
containers).

Checked on a real Plasma 6.8.2 session (2026-10-31): the Flatpak is the same
as native there too (backends kwin + portal, ScreenShot2 capture,
layer-shell). KWin also served ScreenShot2 to a renamed copy of the binary
with no desktop entry. That is not a `--virtual` quirk: Plasma 6.8 dropped
the `X-KDE-DBUS-Restricted-Interfaces` check (kwin commit 0ea31ae7, "the
best available option at the moment are sandbox environments like
flatpak"), so any client that can reach `org.kde.KWin` may capture.

Plasma 6.0–6.7 still check, by matching the caller's executable against the
first word of `Exec=` in installed desktop files. The Flatpak's executable is
`…/files/bin/clipt` while its exported desktop entry runs `flatpak`, so
there KWin refuses it and clipt falls back to the portal (inferred from the
kwin 6.7 source, not run). Native installs still need `data/clipt.desktop`
on those versions.

## clipt-launch: full parity on wlroots and niri

The filtering is deliberate compositor policy, so nothing inside the sandbox
can undo it. Flatpak's sanctioned way around it is
`--socket=inherit-wayland-socket` (in the manifest): if whoever launches the
Flatpak opened a Wayland connection outside the sandbox and passes it in as
`WAYLAND_SOCKET`, the app uses that unfiltered connection. The permission
grants nothing by itself.

`clipt-launch` is that launcher, a few lines of Python to copy somewhere on
the host and use in the compositor's hotkey:

    bindsym Print exec clipt-launch flatpak run io.github.jasondyck.clipt gui

clipt keeps the handed-in connection for capture, overlay and clipboard
(`capture::wayland::connection()`), since the fd can only be taken once.

This needs a file on the host, which is what Flatpak is meant to avoid, so it
is for people who want the Flatpak anyway; for everyone else on wlroots the
native install is simpler. Other options considered and not taken: the
ScreenCast portal with a restore token (silent after one approval, but a
PipeWire dependency clipt doesn't otherwise need), and per-app allow-lists in
compositors (their configuration, not ours).

## Flatpak-specific code

- `update::flatpak()` detects the sandbox (`/.flatpak-info`).
- `clipt update` refuses and points at `flatpak update`; the tray hides
  *Check for updates…* and ignores `check_updates` (no network permission).
- Tray: the lock name becomes `<app-id>.Tray` and the item registers under
  its unique bus name, since a sandbox can only own names under its app ID.
  On start it asks the Background portal to autostart `clipt tray`.
- `--socket=inherit-wayland-socket` plus the shared connection described above.
- KWin's window script is written to `$XDG_RUNTIME_DIR/app/<app-id>/`, the
  only part of the runtime dir KWin can see from outside.

## Open questions

- Sway window selection under `clipt-launch`: the launcher could also pass
  an fd for Sway's IPC socket.
- The portal path on wlroots (xdg-desktop-portal-wlr) hasn't been run.
- Flathub review: `--talk-name=org.kde.KWin` (and `.ScreenShot2`) and
  `--filesystem=xdg-run/hypr` need justifying; Sway's IPC socket has a
  per-session name and can't be granted statically at all. Since Plasma 6.8,
  `--talk-name=org.kde.KWin` amounts to silent screen capture, so expect
  reviewers to treat it that way.
- Autostart is requested on every tray start; quitting from the menu doesn't
  withdraw it (the desktop's app settings do).
- Not yet checked by hand: the tray icon, notifications, `xdg-open` of the
  screenshots folder, the GNOME portal flow.
