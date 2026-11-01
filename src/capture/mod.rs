//! Screen capture across compositors. Backends, tried in order unless forced:
//!
//! | backend  | compositors                                          |
//! |----------|------------------------------------------------------|
//! | `ext`    | ext-image-copy-capture-v1: Sway 1.11+, niri, COSMIC, Hyprland 0.50+ |
//! | `wlr`    | wlr-screencopy: Sway, Hyprland, river, labwc, Wayfire |
//! | `kwin`   | KDE Plasma (org.kde.KWin.ScreenShot2)                |
//! | `portal` | GNOME and anything with xdg-desktop-portal           |
//! | `x11`    | X11 sessions (Xfce, Cinnamon, MATE, i3, ...)         |

pub mod kwin;
#[cfg(feature = "pipewire")]
pub mod kwin_cast;
#[cfg(feature = "pipewire")]
pub mod mutter_cast;
#[cfg(feature = "pipewire")]
pub mod pipewire;
pub mod portal;
pub mod wayland;

use crate::image::Image;
use anyhow::{Result, bail};

/// A monitor in the compositor's logical (scaled) coordinate space.
#[derive(Clone, Debug)]
pub struct Output {
    pub index: usize,
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Physical pixel size as the user sees it (current mode, rotated).
    /// `(0, 0)` when unknown.
    pub native: (u32, u32),
}

/// One output's pixels. `image` is at native resolution, so its size may be
/// larger than the output's logical size when scaling is in effect.
#[derive(Clone)]
pub struct Shot {
    pub output: Output,
    pub image: Image,
}

impl Shot {
    pub fn scale(&self) -> f64 {
        self.image.width as f64 / self.output.width as f64
    }
}

/// A rectangle in logical coordinates: x, y, width, height.
pub type LogicalRect = (f64, f64, f64, f64);

/// Byte order of a video frame's pixels.
/// The fourth byte is padding (alpha, when present, is ignored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    Bgrx,
    Rgbx,
}

/// One frame of a recording: packed rows (stride = width * 4), in whatever
/// byte order the compositor handed over, so the common case is a row copy.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub order: PixelOrder,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Ext,
    Wlr,
    Kwin,
    Portal,
    X11,
}

/// Which display system clipt is talking to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayKind {
    Wayland,
    X11,
}

enum Display {
    Wayland(Box<wayland::Wayland>),
    X11(Box<crate::x11::X11>),
}

impl Backend {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "ext" => Self::Ext,
            "wlr" => Self::Wlr,
            "kwin" => Self::Kwin,
            "portal" => Self::Portal,
            "x11" => Self::X11,
            _ => bail!("unknown backend '{s}' (ext, wlr, kwin, portal, x11)"),
        })
    }
}

pub struct Capturer {
    display: Display,
    bus: Option<crate::dbus::Connection>,
}

impl Capturer {
    /// Wayland when there is a compositor to talk to, X11 otherwise.
    /// `CLIPT_X11=1` forces X11 (useful for testing under XWayland, which
    /// only shows X clients).
    pub fn new() -> Result<Self> {
        let bus = crate::dbus::session().ok();
        if let Some(b) = &bus {
            portal::register(b);
        }
        let force_x11 = std::env::var_os("CLIPT_X11").is_some_and(|v| v != "0");
        let display = match (force_x11, wayland::Wayland::connect()) {
            (false, Ok(wl)) => Display::Wayland(Box::new(wl)),
            (_, wl_err) if crate::x11::available() => Display::X11(Box::new(crate::x11::X11::connect().map_err(|e| match wl_err {
                Err(w) => e.context(format!("and Wayland: {w:#}")),
                Ok(_) => e,
            })?)),
            (_, Err(e)) => return Err(e.context("no Wayland compositor and no X11 display")),
            (true, Ok(_)) => bail!("CLIPT_X11 is set but DISPLAY isn't"),
        };
        Ok(Self { display, bus })
    }

    pub fn kind(&self) -> DisplayKind {
        match self.display {
            Display::Wayland(_) => DisplayKind::Wayland,
            Display::X11(_) => DisplayKind::X11,
        }
    }

    pub fn outputs(&self) -> Vec<Output> {
        match &self.display {
            Display::Wayland(wl) => wl.outputs(),
            Display::X11(x) => x.outputs(),
        }
    }

    pub fn available(&self) -> Vec<Backend> {
        let mut v = Vec::new();
        let wl = match &self.display {
            Display::Wayland(wl) => wl,
            // X11 can always read the root window; nothing else needed.
            Display::X11(_) => return vec![Backend::X11],
        };
        for p in wl.protocols() {
            v.push(match p {
                wayland::Protocol::ExtImageCopy => Backend::Ext,
                wayland::Protocol::WlrScreencopy => Backend::Wlr,
            });
        }
        if let Some(bus) = &self.bus {
            if kwin::available(bus) {
                v.push(Backend::Kwin);
            }
            if portal::available(bus) {
                v.push(Backend::Portal);
            }
        }
        v
    }

    /// Capture `outputs` with the forced backend, or the first one that works.
    pub fn capture(&mut self, outputs: &[Output], cursor: bool, force: Option<Backend>) -> Result<(Backend, Vec<Shot>)> {
        let order = match force {
            Some(b) => vec![b],
            None => self.available(),
        };
        if order.is_empty() {
            bail!("no capture backend available (no screencopy protocol, KWin or portal)");
        }
        let mut errors = Vec::new();
        let all = self.outputs();
        for b in order {
            let r = match (b, &mut self.display) {
                (Backend::Ext, Display::Wayland(wl)) => wl.capture(outputs, cursor, wayland::Protocol::ExtImageCopy),
                (Backend::Wlr, Display::Wayland(wl)) => wl.capture(outputs, cursor, wayland::Protocol::WlrScreencopy),
                (Backend::X11, Display::X11(x)) => x.capture(outputs, cursor),
                (Backend::Ext | Backend::Wlr, _) => Err(anyhow::anyhow!("not a Wayland session")),
                (Backend::X11, _) => Err(anyhow::anyhow!("not an X11 session")),
                (Backend::Kwin, _) => match &self.bus {
                    Some(bus) => kwin::capture(bus, outputs, cursor),
                    None => Err(anyhow::anyhow!("no session bus")),
                },
                (Backend::Portal, _) => match &self.bus {
                    Some(bus) => portal::capture(bus, &all, outputs),
                    None => Err(anyhow::anyhow!("no session bus")),
                },
            };
            match r {
                Ok(shots) => {
                    if std::env::var_os("CLIPT_DEBUG").is_some() {
                        eprintln!("clipt: captured with {b:?}");
                    }
                    return Ok((b, shots));
                }
                Err(e) => errors.push(format!("{b:?}: {e:#}")),
            }
        }
        bail!("all capture backends failed:\n  {}", errors.join("\n  "))
    }
}
