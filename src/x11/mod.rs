//! X11 support: Xfce, Cinnamon, MATE, LXQt, i3 and other X window managers,
//! and the X11 sessions of KDE and GNOME.
//!
//! X11 has no compositor-side scaling, so logical and physical pixels are the
//! same and every monitor is captured at scale 1.

pub mod clip;
pub mod overlay;
pub mod record;

use crate::capture::{Output, Shot};
use crate::image::Image;
use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinerama::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Screen, VisualClass, Visualtype};
use x11rb::rust_connection::RustConnection;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        TIMESTAMP,
        INCR,
        ATOM,
        PNG: b"image/png",
        CLIPT_SELECTION,
    }
}

pub struct X11 {
    pub conn: RustConnection,
    pub screen_num: usize,
    pub atoms: Atoms,
}

impl X11 {
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("no X11 display (is DISPLAY set?)")?;
        let atoms = Atoms::new(&conn)?.reply()?;
        Ok(Self { conn, screen_num, atoms })
    }

    pub fn screen(&self) -> &Screen {
        &self.conn.setup().roots[self.screen_num]
    }

    /// Monitors from RandR 1.5, or from Xinerama when that knows more (older
    /// multi-head setups: Xinerama, NVIDIA TwinView); the whole screen when
    /// neither has anything to say.
    pub fn outputs(&self) -> Vec<Output> {
        let randr = self.randr_outputs();
        let xinerama = self.xinerama_outputs();
        let mut outs = if xinerama.len() > randr.len() { xinerama } else { randr };
        if outs.is_empty() {
            let s = self.screen();
            outs.push(Output {
                index: 0,
                name: "screen".into(),
                description: String::new(),
                x: 0,
                y: 0,
                width: s.width_in_pixels as u32,
                height: s.height_in_pixels as u32,
                native: (s.width_in_pixels as u32, s.height_in_pixels as u32),
            });
        }
        outs
    }

    fn xinerama_outputs(&self) -> Vec<Output> {
        let active = self.conn.xinerama_is_active().ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.state != 0);
        if !active {
            return Vec::new();
        }
        let screens = self.conn.xinerama_query_screens().ok().and_then(|c| c.reply().ok()).map(|r| r.screen_info).unwrap_or_default();
        screens
            .iter()
            .enumerate()
            .map(|(i, s)| Output {
                index: i,
                name: format!("xinerama-{i}"),
                description: String::new(),
                x: s.x_org as i32,
                y: s.y_org as i32,
                width: s.width as u32,
                height: s.height as u32,
                native: (s.width as u32, s.height as u32),
            })
            .collect()
    }

    fn randr_outputs(&self) -> Vec<Output> {
        let root = self.screen().root;
        let monitors = self.conn.randr_get_monitors(root, true).ok().and_then(|c| c.reply().ok()).map(|r| r.monitors).unwrap_or_default();
        let outs: Vec<Output> = monitors
            .iter()
            .enumerate()
            .filter(|(_, m)| m.width > 0 && m.height > 0)
            .map(|(i, m)| {
                let name = self
                    .conn
                    .get_atom_name(m.name)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                    .unwrap_or_else(|| format!("monitor-{i}"));
                Output {
                    index: i,
                    name,
                    description: if m.primary { "primary".into() } else { String::new() },
                    x: m.x as i32,
                    y: m.y as i32,
                    width: m.width as u32,
                    height: m.height as u32,
                    native: (m.width as u32, m.height as u32),
                }
            })
            .collect();
        outs
    }

    pub fn capture(&self, outputs: &[Output], cursor: bool) -> Result<Vec<Shot>> {
        let root = self.screen().root;
        let cur = if cursor { self.cursor_image() } else { None };
        let mut shots = Vec::new();
        for o in outputs {
            let r = self
                .conn
                .get_image(ImageFormat::Z_PIXMAP, root, o.x as i16, o.y as i16, o.width as u16, o.height as u16, !0)?
                .reply()
                .with_context(|| format!("GetImage for {}", o.name))?;
            let visual = self.visual(r.visual).context("unknown visual")?;
            let bpp = self.bits_per_pixel(r.depth).context("unknown pixmap format")?;
            let mut image = convert(o.width, o.height, bpp, &visual, &r.data)?;
            if let Some(c) = &cur {
                c.composite(&mut image, o.x, o.y);
            }
            shots.push(Shot { output: o.clone(), image });
        }
        Ok(shots)
    }

    fn visual(&self, id: u32) -> Option<Visualtype> {
        self.screen().allowed_depths.iter().flat_map(|d| d.visuals.iter()).find(|v| v.visual_id == id).copied()
    }

    fn bits_per_pixel(&self, depth: u8) -> Option<u8> {
        self.conn.setup().pixmap_formats.iter().find(|f| f.depth == depth).map(|f| f.bits_per_pixel)
    }

    fn cursor_image(&self) -> Option<CursorImage> {
        self.conn.xfixes_query_version(5, 0).ok()?.reply().ok()?;
        let r = self.conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        Some(CursorImage {
            x: r.x as i32 - r.xhot as i32,
            y: r.y as i32 - r.yhot as i32,
            w: r.width as u32,
            h: r.height as u32,
            argb: r.cursor_image,
        })
    }
}

/// Whether an X display is reachable (used when there is no Wayland).
pub fn available() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
}

pub(crate) struct CursorImage {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    /// Premultiplied ARGB, one u32 per pixel.
    argb: Vec<u32>,
}

impl CursorImage {
    fn composite(&self, img: &mut Image, ox: i32, oy: i32) {
        for cy in 0..self.h as i32 {
            for cx in 0..self.w as i32 {
                let (x, y) = (self.x - ox + cx, self.y - oy + cy);
                if x < 0 || y < 0 || x >= img.width as i32 || y >= img.height as i32 {
                    continue;
                }
                let p = self.argb[(cy as u32 * self.w + cx as u32) as usize];
                let a = p >> 24;
                if a == 0 {
                    continue;
                }
                let i = (y as usize * img.width as usize + x as usize) * 4;
                let d = &mut img.data[i..i + 4];
                let over = |src: u32, dst: u8| (src + (dst as u32 * (255 - a) + 127) / 255).min(255) as u8;
                d[0] = over((p >> 16) & 0xff, d[0]);
                d[1] = over((p >> 8) & 0xff, d[1]);
                d[2] = over(p & 0xff, d[2]);
                d[3] = 255;
            }
        }
    }
}

/// ZPixmap data -> RGBA, using the visual's channel masks. The overwhelmingly
/// common case (32 bpp, 0xff0000/0xff00/0xff, i.e. BGRX bytes) has a fast path.
fn convert(w: u32, h: u32, bpp: u8, v: &Visualtype, data: &[u8]) -> Result<Image> {
    if !matches!(v.class, VisualClass::TRUE_COLOR | VisualClass::DIRECT_COLOR) {
        bail!("only TrueColor visuals are supported");
    }
    // X11 scanlines are padded to 32 bits for every format we accept.
    let bytes = bpp as usize / 8;
    let stride = (w as usize * bpp as usize).div_ceil(32) * 4;
    if bpp == 32 && (v.red_mask, v.green_mask, v.blue_mask) == (0xff0000, 0xff00, 0xff) {
        return Image::from_rows(w, h, stride, 4, data, |p| [p[2], p[1], p[0], 255]);
    }
    if !matches!(bpp, 16 | 24 | 32) {
        bail!("unsupported {bpp} bits per pixel");
    }
    let chan = |mask: u32| {
        let shift = mask.trailing_zeros();
        let max = (mask >> shift).max(1);
        move |px: u32| (((px & mask) >> shift) * 255 / max) as u8
    };
    let (r, g, b) = (chan(v.red_mask), chan(v.green_mask), chan(v.blue_mask));
    Image::from_rows(w, h, stride, bytes, data, |p| {
        let mut px = 0u32;
        for (i, byte) in p.iter().enumerate() {
            px |= (*byte as u32) << (8 * i); // little-endian image byte order
        }
        [r(px), g(px), b(px), 255]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visual(r: u32, g: u32, b: u32) -> Visualtype {
        Visualtype {
            visual_id: 1,
            class: VisualClass::TRUE_COLOR,
            bits_per_rgb_value: 8,
            colormap_entries: 256,
            red_mask: r,
            green_mask: g,
            blue_mask: b,
        }
    }

    #[test]
    fn converts_bgrx_and_rgb565() {
        let img = convert(2, 1, 32, &visual(0xff0000, 0xff00, 0xff), &[1, 2, 3, 0, 4, 5, 6, 0]).unwrap();
        assert_eq!(img.data, [3, 2, 1, 255, 6, 5, 4, 255]);
        // 565: pure red, pure green; 2 px = 4 bytes, padded to 4
        let img = convert(2, 1, 16, &visual(0xf800, 0x07e0, 0x001f), &[0x00, 0xf8, 0xe0, 0x07]).unwrap();
        assert_eq!(img.data, [255, 0, 0, 255, 0, 255, 0, 255]);
    }
}
