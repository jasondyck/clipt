//! Minimal text rendering: one sans-serif face via ab_glyph, drawn straight into
//! tiny-skia pixmaps. Found through fontconfig's `fc-match`, with fallbacks.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use tiny_skia::{Color, PixmapMut};

pub struct Fonts {
    pub regular: FontVec,
    pub bold: Option<FontVec>,
}

impl Fonts {
    pub fn load(custom: Option<&str>) -> anyhow::Result<Self> {
        let find = |pattern: &str| -> Option<FontVec> {
            let path = std::process::Command::new("fc-match")
                .args(["-f", "%{file}", pattern])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .filter(|p| !p.is_empty());
            let candidates = path.into_iter().chain(FALLBACKS.iter().map(|s| s.to_string()));
            candidates.filter_map(|p| std::fs::read(p).ok()).find_map(|b| FontVec::try_from_vec(b).ok())
        };
        let regular = match custom {
            Some(p) => FontVec::try_from_vec(std::fs::read(p)?)?,
            None => find("sans-serif").ok_or_else(|| anyhow::anyhow!("no usable font found (install any TTF/OTF sans)"))?,
        };
        Ok(Self { regular, bold: find("sans-serif:weight=200") })
    }

    pub fn bold(&self) -> &FontVec {
        self.bold.as_ref().unwrap_or(&self.regular)
    }
}

const FALLBACKS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/cantarell/Cantarell-VF.otf",
];

/// Width of a single line in pixels.
pub fn measure(font: &FontVec, size: f32, text: &str) -> f32 {
    let f = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = f.glyph_id(c);
        if let Some(p) = prev {
            w += f.kern(p, id);
        }
        w += f.h_advance(id);
        prev = Some(id);
    }
    w
}

pub fn line_height(font: &FontVec, size: f32) -> f32 {
    let f = font.as_scaled(PxScale::from(size));
    f.ascent() - f.descent() + f.line_gap()
}

/// Draw `text` with its top-left at (x, y). Newlines start new lines.
/// `swap_rb` flips red/blue for BGRA targets.
pub fn draw(pm: &mut PixmapMut, font: &FontVec, size: f32, x: f32, y: f32, text: &str, color: Color, swap_rb: bool) {
    let f = font.as_scaled(PxScale::from(size));
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    let c = color.premultiply().to_color_u8();
    let (mut cr, cg, mut cb, ca) = (c.red() as f32, c.green() as f32, c.blue() as f32, c.alpha() as f32);
    if swap_rb {
        std::mem::swap(&mut cr, &mut cb);
    }
    let data = pm.data_mut();
    let lh = f.ascent() - f.descent() + f.line_gap();
    for (li, line) in text.split('\n').enumerate() {
        let mut caret = x;
        let base = y + f.ascent() + li as f32 * lh;
        let mut prev = None;
        for ch in line.chars() {
            let id = f.glyph_id(ch);
            if let Some(p) = prev {
                caret += f.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(caret, base));
            caret += f.h_advance(id);
            prev = Some(id);
            let Some(outlined) = font.outline_glyph(glyph) else { continue };
            let b = outlined.px_bounds();
            outlined.draw(|gx, gy, cov| {
                let px = b.min.x as i32 + gx as i32;
                let py = b.min.y as i32 + gy as i32;
                if px < 0 || py < 0 || px >= w || py >= h || cov <= 0.0 {
                    return;
                }
                let i = (py * w + px) as usize * 4;
                let a = cov.min(1.0);
                let inv = 1.0 - a * ca / 255.0;
                let blend = |d: u8, s: f32| (s * a + d as f32 * inv).round().clamp(0.0, 255.0) as u8;
                data[i] = blend(data[i], cr);
                data[i + 1] = blend(data[i + 1], cg);
                data[i + 2] = blend(data[i + 2], cb);
                data[i + 3] = blend(data[i + 3], ca);
            });
        }
    }
}
