# clipt

A lightweight screenshot tool for Wayland, in the spirit of Flameshot: freeze
the screen, drag a region, annotate it, copy or save. One small Rust binary, no
Qt/GTK, CPU-rendered with tiny-skia.

![clipt's editor: a selection with its size label, a rectangle, an arrow,
numbered counters, a highlighter, text and the toolbar](docs/screenshot.png)

## Compositor support

Capture backend is picked automatically (override with `-b` or `backend =`):

| Backend  | Used on                                                      |
|----------|--------------------------------------------------------------|
| `ext`    | ext-image-copy-capture-v1: Sway 1.11+, niri, COSMIC, Hyprland |
| `wlr`    | wlr-screencopy: Sway, Hyprland, river, labwc, Wayfire        |
| `kwin`   | KDE Plasma (org.kde.KWin.ScreenShot2, HDR is tonemapped by KWin) |
| `portal` | GNOME, or anything with xdg-desktop-portal                   |

The editor overlay uses wlr-layer-shell where available (KDE, wlroots,
Hyprland, niri, COSMIC) and falls back to a fullscreen window on GNOME.
### Multiple monitors

There is no "pick a monitor" step: every monitor gets its own overlay at the
same time, each drawn at that monitor's native resolution, and you can drag a
selection on any of them, or across two. Mixed resolutions, fractional scaling
(1.25×, 1.5×), offset layouts and rotated portrait monitors are all covered by
the end-to-end tests, and saved images keep the native pixel size of the
monitor they came from.

Tested on KDE Plasma 6.8 (by hand), and end-to-end in containers on Sway 1.12
and GNOME Shell 51 across single, fractional-scale, mixed-DPI and
three-monitor (offset + rotated) layouts.

## Usage

```
clipt               # interactive (same as `clipt gui`)
clipt full -c       # whole desktop to the clipboard
clipt screen DP-1   # one output, saved to the save dir
clipt full -g "$(slurp)" -o -   # slurp region to stdout
clipt list          # outputs and available backends
```

Editor: drag to select (click = whole screen), drag handles to resize, drag
inside to move. Tools: **V** select, **R** rectangle, **E** ellipse, **A**
arrow, **L** line, **P** pen, **M** highlighter, **T** text, **X** pixelate,
**N** numbered counter. **1–8** colour, scroll or **[ ]** stroke width, Shift
for squares and 45° lines. **Enter**/**Ctrl+C**/double-click copies, **Ctrl+S**
saves, **Ctrl+Z**/**Ctrl+Shift+Z** undo/redo, **Esc** cancels, right-click
backs out a step.

After copying, clipt forks into the background to serve the clipboard until
something else is copied (Wayland clipboards are owned by a live client).

## Config

`~/.config/clipt/config.toml`, every key optional:

```toml
save_dir = "~/Pictures/Screenshots"
filename = "Screenshot_%Y-%m-%d_%H-%M-%S"   # strftime
color = 1            # 1-8: red orange yellow green blue purple white black
width = 4
copy_on_save = true
notify = true
backend = "auto"     # ext, wlr, kwin, portal
cursor = false
# font = "/usr/share/fonts/TTF/Inter-Regular.ttf"
```

## Performance

Measured on a 4K KDE Plasma 6.8 desktop:

|                          | clipt  | Spectacle 6.8 | Flameshot 14 |
|--------------------------|--------|---------------|--------------|
| Full-screen capture to PNG | ~100 ms | 388 ms      | 593 ms       |
| Region overlay memory    | 71 MB  | 337 MB        | 126 MB       |
| CPU to open the overlay  | 50 ms  | 570 ms        | 140 ms       |

Redraws are damage-tracked: moving the pointer or drawing touches only the
changed pixels (0.5-5 ms per frame at 4K).

## Install

From a release tarball or a checkout:

```
./install.sh        # builds if needed; ~/.local/bin/clipt + desktop entry
```

On KDE the desktop entry carries `X-KDE-DBUS-Restricted-Interfaces` so KWin
lets clipt capture silently. Bind `clipt` to Print in System Settings →
Shortcuts.

**GNOME:** captures go through the screenshot portal, which needs the desktop
entry (clipt registers its app ID from it, and GLib ignores entries whose
`Exec` doesn't resolve) plus a one-time permission. If you aren't prompted,
grant it directly:

```
gdbus call --session --dest org.freedesktop.impl.portal.PermissionStore \
  --object-path /org/freedesktop/impl/portal/PermissionStore \
  --method org.freedesktop.impl.portal.PermissionStore.SetPermission \
  screenshot true screenshot clipt "['yes']"
```

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for the checks CI runs, the
containerised Sway/GNOME end-to-end suites and the benchmarks.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
