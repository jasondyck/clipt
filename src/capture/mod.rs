//! Screen capture across compositors. Backends, tried in order unless forced:
//!
//! | backend  | compositors                                          |
//! |----------|------------------------------------------------------|
//! | `ext`    | ext-image-copy-capture-v1: Sway 1.11+, niri, COSMIC, Hyprland 0.50+ |
//! | `wlr`    | wlr-screencopy: Sway, Hyprland, river, labwc, Wayfire |
//! | `kwin`   | KDE Plasma (org.kde.KWin.ScreenShot2)                |
//! | `portal` | GNOME and anything with xdg-desktop-portal           |

pub mod kwin;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Ext,
    Wlr,
    Kwin,
    Portal,
}

impl Backend {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "ext" => Self::Ext,
            "wlr" => Self::Wlr,
            "kwin" => Self::Kwin,
            "portal" => Self::Portal,
            _ => bail!("unknown backend '{s}' (ext, wlr, kwin, portal)"),
        })
    }
}

pub struct Capturer {
    wl: wayland::Wayland,
    bus: Option<zbus::blocking::Connection>,
}

impl Capturer {
    pub fn new() -> Result<Self> {
        Ok(Self { wl: wayland::Wayland::connect()?, bus: zbus::blocking::Connection::session().ok() })
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.wl.outputs()
    }

    pub fn available(&self) -> Vec<Backend> {
        let mut v = Vec::new();
        match self.wl.protocol() {
            Some(wayland::Protocol::ExtImageCopy) => v.push(Backend::Ext),
            Some(wayland::Protocol::WlrScreencopy) => v.push(Backend::Wlr),
            None => {}
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
        for b in order {
            let r = match b {
                Backend::Ext | Backend::Wlr => self.wl.capture(outputs, cursor),
                Backend::Kwin => match &self.bus {
                    Some(bus) => kwin::capture(bus, outputs, cursor),
                    None => Err(anyhow::anyhow!("no session bus")),
                },
                Backend::Portal => match &self.bus {
                    Some(bus) => portal::capture(bus, outputs),
                    None => Err(anyhow::anyhow!("no session bus")),
                },
            };
            match r {
                Ok(shots) => return Ok((b, shots)),
                Err(e) => errors.push(format!("{b:?}: {e:#}")),
            }
        }
        bail!("all capture backends failed:\n  {}", errors.join("\n  "))
    }
}
