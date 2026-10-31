//! clipt's icon, drawn in code so the tray shows the same mark on every panel
//! and theme: a flat mascot — a cream blob holding up a lavender picture
//! frame — on a lavender badge. Below 32 px the smile and hands drop out and
//! the eyes and frame grow, so it still reads at tray size.
//!
//! `data/clipt.svg` is the same drawing for the desktop entry; keep the two
//! in step.

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

fn rgb(hex: u32) -> Color {
    Color::from_rgba8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, 0xff)
}

const BADGE: u32 = 0xc4b1ef;
const BODY: u32 = 0xfff4de;
const INK: u32 = 0x2d2340;
const FRAME: u32 = 0x8a6cd6;
const PHOTO: u32 = 0xf1ebfd;

/// Renders the icon at `size`×`size` (premultiplied RGBA).
pub fn render(size: u32) -> Pixmap {
    let mut pm = Pixmap::new(size, size).expect("non-zero icon size");
    let s = size as f32;
    let small = s < 32.0;
    let ts = Transform::from_scale(s, s);
    let mut paint = Paint { anti_alias: true, ..Default::default() };
    let fill = |pm: &mut Pixmap, path: &tiny_skia::Path, c: u32| {
        let mut p = Paint { anti_alias: true, ..Default::default() };
        p.set_color(rgb(c));
        pm.fill_path(path, &p, FillRule::Winding, ts, None);
    };
    let oval = |cx: f32, cy: f32, rx: f32, ry: f32| {
        PathBuilder::from_oval(Rect::from_xywh(cx - rx, cy - ry, 2.0 * rx, 2.0 * ry).unwrap()).unwrap()
    };

    // Badge.
    fill(&mut pm, &rounded_rect(0.04, 0.04, 0.92, 0.92, 0.22), BADGE);

    // Body: a slightly bottom-heavy egg.
    let (bx, by, brx, bry) = if small { (0.5, 0.45, 0.3, 0.33) } else { (0.5, 0.52, 0.34, 0.34) };
    fill(&mut pm, &egg(bx, by, brx, bry), BODY);

    // Eyes.
    let (ey, edx, er) = if small { (0.37, 0.12, 0.065) } else { (0.43, 0.12, 0.05) };
    for dx in [-edx, edx] {
        if small {
            // Whole-pixel squares: a 1.5 px circle is just a grey smudge.
            let d = (0.11 * s).round().max(2.0);
            let x = ((bx + dx) * s - d / 2.0).round();
            let y = (ey * s - d / 2.0).round();
            let mut p = Paint::default();
            p.set_color(rgb(INK));
            pm.fill_rect(Rect::from_xywh(x, y, d, d + 1.0).unwrap(), &p, Transform::identity(), None);
            continue;
        }
        fill(&mut pm, &oval(bx + dx, ey, er, er * 1.1), INK);
        {
            fill(&mut pm, &oval(bx + dx - er * 0.35, ey - er * 0.4, er * 0.32, er * 0.32), BODY);
        }
    }

    // Smile.
    if !small {
        let mut pb = PathBuilder::new();
        pb.move_to(0.455, 0.5);
        pb.quad_to(0.5, 0.54, 0.545, 0.5);
        let smile = pb.finish().unwrap();
        paint.set_color(rgb(INK));
        let stroke = Stroke { width: 0.022, line_cap: LineCap::Round, ..Default::default() };
        pm.stroke_path(&smile, &paint, &stroke, ts, None);
    }

    // The frame it holds up, tilted a touch like a photo being shown off.
    let (fx, fy, fw, fh, border) = if small { (0.16, 0.54, 0.68, 0.38, 0.09) } else { (0.24, 0.58, 0.5, 0.28, 0.05) };
    let tilt = if small { Transform::identity() } else { Transform::from_rotate_at(-5.0, 0.5, fy + fh / 2.0) };
    let tts = tilt.post_concat(ts);
    paint.set_color(rgb(FRAME));
    pm.fill_path(&rounded_rect(fx, fy, fw, fh, 0.05), &paint, FillRule::Winding, tts, None);
    // Pale lavender inside so the frame doesn't melt into the body.
    paint.set_color(rgb(PHOTO));
    pm.fill_path(&rounded_rect(fx + border, fy + border, fw - 2.0 * border, fh - 2.0 * border, 0.02), &paint, FillRule::Winding, tts, None);

    // Hands gripping the frame's sides.
    if !small {
        let hy = fy + fh * 0.62;
        let hands = [oval(fx + 0.01, hy, 0.085, 0.075), oval(fx + fw - 0.01, hy + 0.03, 0.085, 0.075)];
        for h in &hands {
            fill(&mut pm, h, BODY);
        }
        // A hairline so the hands read against the cream body above them.
        paint.set_color(Color::from_rgba8(0x2d, 0x23, 0x40, 0x40));
        let stroke = Stroke { width: 0.012, ..Default::default() };
        for h in &hands {
            pm.stroke_path(h, &paint, &stroke, ts, None);
        }
    }
    pm
}

/// An egg: an ellipse whose lower half is a little wider than its top.
fn egg(cx: f32, cy: f32, rx: f32, ry: f32) -> tiny_skia::Path {
    const K: f32 = 0.552_284_8;
    let (top, bot) = (rx * 0.92, rx * 1.04);
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy - ry);
    pb.cubic_to(cx + top * K, cy - ry, cx + top, cy - ry * K, cx + (top + bot) / 2.0, cy);
    pb.cubic_to(cx + bot, cy + ry * K, cx + bot * K, cy + ry, cx, cy + ry);
    pb.cubic_to(cx - bot * K, cy + ry, cx - bot, cy + ry * K, cx - (top + bot) / 2.0, cy);
    pb.cubic_to(cx - top, cy - ry * K, cx - top * K, cy - ry, cx, cy - ry);
    pb.close();
    pb.finish().unwrap()
}

/// StatusNotifierItem pixmap: non-premultiplied ARGB32, network byte order.
pub fn argb32(size: u32) -> Vec<u8> {
    let pm = render(size);
    let mut out = Vec::with_capacity(pm.data().len());
    for p in pm.pixels() {
        let c = p.demultiply();
        out.extend_from_slice(&[c.alpha(), c.red(), c.green(), c.blue()]);
    }
    out
}

/// A contact sheet for eyeballing: every tray size on a light and a dark
/// panel colour, each also shown 8x enlarged so pixels are visible.
pub fn preview() -> crate::image::Image {
    const SIZES: [u32; 6] = [16, 22, 24, 32, 48, 128];
    const PAD: u32 = 24;
    let w = SIZES.iter().map(|s| s + PAD).sum::<u32>() + PAD + 2 * (22 * 8 + PAD);
    let row = 22 * 8 + 2 * PAD;
    let mut sheet = Pixmap::new(w, row * 2).expect("sheet");
    for (r, bg) in [Color::from_rgba8(0xef, 0xf0, 0xf1, 0xff), Color::from_rgba8(0x20, 0x23, 0x26, 0xff)].into_iter().enumerate() {
        let top = r as u32 * row;
        sheet.fill_rect(
            Rect::from_xywh(0.0, top as f32, w as f32, row as f32).unwrap(),
            &Paint { shader: tiny_skia::Shader::SolidColor(bg), ..Default::default() },
            Transform::identity(),
            None,
        );
        let mut x = PAD;
        for s in SIZES {
            sheet.draw_pixmap(
                x as i32,
                (top + PAD) as i32,
                render(s).as_ref(),
                &tiny_skia::PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            x += s + PAD;
        }
        for s in [16u32, 22] {
            let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Nearest, ..Default::default() };
            sheet.draw_pixmap(
                0,
                0,
                render(s).as_ref(),
                &paint,
                Transform::from_scale(8.0, 8.0).post_translate(x as f32, (top + PAD) as f32),
                None,
            );
            x += 22 * 8 + PAD;
        }
    }
    let mut img = crate::image::Image::new(w, row * 2);
    for (d, p) in img.data.chunks_exact_mut(4).zip(sheet.pixels()) {
        let c = p.demultiply();
        d.copy_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    img
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    // Quarter circles via cubic Béziers.
    const K: f32 = 0.552_284_8;
    let (x1, y1) = (x + w, y + h);
    let k = r * K;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x1 - r, y);
    pb.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x + r, y1);
    pb.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().unwrap_or_else(|| PathBuilder::from_rect(Rect::from_xywh(x, y, w, h).unwrap()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn argb_layout() {
        let px = super::argb32(16);
        assert_eq!(px.len(), 16 * 16 * 4);
        // Centre pixel is on the opaque body.
        let i = (8 * 16 + 8) * 4;
        assert_eq!(px[i], 255);
        // Corner pixel is outside the rounded badge.
        assert_eq!(px[0], 0);
    }
}
