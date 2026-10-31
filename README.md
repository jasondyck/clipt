# glint

A lightweight screenshot tool for Wayland, in the spirit of Flameshot: freeze
the screen, drag a region, annotate it, copy or save. One small Rust binary, no
Qt/GTK, CPU-rendered with tiny-skia.

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
Multiple monitors and fractional scaling are handled: the overlay shows each
output at native resolution, and exports are at native resolution.

Tested so far on KDE Plasma 6.8. The wlroots and GNOME paths compile and follow
the protocols but haven't been run on those compositors yet.

## Usage

```
glint               # interactive (same as `glint gui`)
glint full -c       # whole desktop to the clipboard
glint screen DP-1   # one output, saved to the save dir
glint full -g "$(slurp)" -o -   # slurp region to stdout
glint list          # outputs and available backends
```

Editor: drag to select (click = whole screen), drag handles to resize, drag
inside to move. Tools: **V** select, **R** rectangle, **E** ellipse, **A**
arrow, **L** line, **P** pen, **M** highlighter, **T** text, **X** pixelate,
**N** numbered counter. **1–8** colour, scroll or **[ ]** stroke width, Shift
for squares and 45° lines. **Enter**/**Ctrl+C**/double-click copies, **Ctrl+S**
saves, **Ctrl+Z**/**Ctrl+Shift+Z** undo/redo, **Esc** cancels, right-click
backs out a step.

After copying, glint forks into the background to serve the clipboard until
something else is copied (Wayland clipboards are owned by a live client).

## Config

`~/.config/glint/config.toml`, every key optional:

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

## Install

```
./install.sh        # ~/.local/bin/glint + desktop entry
```

On KDE the desktop entry carries `X-KDE-DBUS-Restricted-Interfaces` so KWin
lets glint capture silently. Bind `glint` to Print in System Settings →
Shortcuts.
