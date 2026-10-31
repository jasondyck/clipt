//! Annotation shapes and their rendering. Shapes live in logical (global
//! desktop) coordinates; a `View` maps them into a pixmap's pixel space.

use crate::text::{self, Fonts};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, PixmapMut, PixmapRef, Rect, Stroke, Transform};

pub type Pt = (f32, f32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Rect,
    Ellipse,
    Arrow,
    Line,
    Pen,
    Marker,
    Text,
    Pixelate,
    Counter,
}

impl Tool {
    pub const DRAWING: [Tool; 9] =
        [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Line, Tool::Pen, Tool::Marker, Tool::Text, Tool::Pixelate, Tool::Counter];

    pub fn hotkey(self) -> char {
        match self {
            Tool::Select => 'v',
            Tool::Rect => 'r',
            Tool::Ellipse => 'e',
            Tool::Arrow => 'a',
            Tool::Line => 'l',
            Tool::Pen => 'p',
            Tool::Marker => 'm',
            Tool::Text => 't',
            Tool::Pixelate => 'x',
            Tool::Counter => 'n',
        }
    }
}

#[derive(Clone, Debug)]
pub enum Shape {
    Rect { a: Pt, b: Pt, color: Color, width: f32 },
    Ellipse { a: Pt, b: Pt, color: Color, width: f32 },
    Arrow { a: Pt, b: Pt, color: Color, width: f32 },
    Line { a: Pt, b: Pt, color: Color, width: f32 },
    Pen { pts: Vec<Pt>, color: Color, width: f32 },
    Marker { pts: Vec<Pt>, color: Color, width: f32 },
    Text { pos: Pt, text: String, color: Color, size: f32 },
    Pixelate { a: Pt, b: Pt, block: f32 },
    Counter { pos: Pt, n: u32, color: Color, radius: f32 },
}

/// Logical → pixel mapping for one target pixmap.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub ox: f32,
    pub oy: f32,
    pub scale: f32,
    /// Target stores BGRA instead of RGBA.
    pub swap_rb: bool,
    /// Pixel offset of the target within the full output (partial repaints).
    /// Applied after scaling so partial and full frames round identically.
    pub off: (f32, f32),
}

impl View {
    pub fn px(&self, p: Pt) -> Pt {
        ((p.0 - self.ox) * self.scale - self.off.0, (p.1 - self.oy) * self.scale - self.off.1)
    }
    pub fn color(&self, c: Color) -> Color {
        if self.swap_rb { Color::from_rgba(c.blue(), c.green(), c.red(), c.alpha()).unwrap() } else { c }
    }
    pub fn paint(&self, c: Color) -> Paint<'static> {
        let mut p = Paint::default();
        p.set_color(self.color(c));
        p.anti_alias = true;
        p
    }
}

pub fn norm_rect(a: Pt, b: Pt) -> Option<Rect> {
    Rect::from_ltrb(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
}

fn stroke(width: f32) -> Stroke {
    Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() }
}

fn polyline(view: &View, pts: &[Pt]) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let first = view.px(*pts.first()?);
    pb.move_to(first.0, first.1);
    if pts.len() == 1 {
        pb.line_to(first.0 + 0.01, first.1);
    }
    // Midpoint quadratic smoothing keeps freehand strokes from looking jagged.
    for w in pts.windows(2) {
        let p0 = view.px(w[0]);
        let p1 = view.px(w[1]);
        pb.quad_to(p0.0, p0.1, (p0.0 + p1.0) / 2.0, (p0.1 + p1.1) / 2.0);
    }
    let last = view.px(*pts.last()?);
    pb.line_to(last.0, last.1);
    pb.finish()
}

impl Shape {
    /// `src` is the clean background for pixelation, with the offset that maps
    /// target pixels to source pixels.
    pub fn draw(&self, pm: &mut PixmapMut, view: &View, src: Option<(PixmapRef, (i32, i32))>, fonts: &Fonts) {
        let s = view.scale;
        let id = Transform::identity();
        match self {
            Shape::Rect { a, b, color, width } => {
                let (a, b) = (view.px(*a), view.px(*b));
                if let Some(r) = norm_rect(a, b) {
                    let path = PathBuilder::from_rect(r);
                    let st = Stroke { line_join: LineJoin::Round, ..stroke(width * s) };
                    pm.stroke_path(&path, &view.paint(*color), &st, id, None);
                }
            }
            Shape::Ellipse { a, b, color, width } => {
                let (a, b) = (view.px(*a), view.px(*b));
                if let Some(path) = norm_rect(a, b).and_then(PathBuilder::from_oval) {
                    pm.stroke_path(&path, &view.paint(*color), &stroke(width * s), id, None);
                }
            }
            Shape::Line { a, b, color, width } => {
                let (a, b) = (view.px(*a), view.px(*b));
                let mut pb = PathBuilder::new();
                pb.move_to(a.0, a.1);
                pb.line_to(b.0, b.1);
                if let Some(path) = pb.finish() {
                    pm.stroke_path(&path, &view.paint(*color), &stroke(width * s), id, None);
                }
            }
            Shape::Arrow { a, b, color, width } => draw_arrow(pm, view, *a, *b, *color, *width),
            Shape::Pen { pts, color, width } => {
                if let Some(path) = polyline(view, pts) {
                    pm.stroke_path(&path, &view.paint(*color), &stroke(width * s), id, None);
                }
            }
            Shape::Marker { pts, color, width } => {
                if let Some(path) = polyline(view, pts) {
                    let mut c = *color;
                    c.set_alpha(0.38);
                    let st = Stroke { line_cap: LineCap::Square, ..stroke(width * s) };
                    pm.stroke_path(&path, &view.paint(c), &st, id, None);
                }
            }
            Shape::Text { pos, text: t, color, size } => {
                let p = view.px(*pos);
                text::draw(pm, fonts.bold(), size * s, p.0, p.1, t, *color, view.swap_rb);
            }
            Shape::Counter { pos, n, color, radius } => {
                let p = view.px(*pos);
                let r = radius * s;
                if let Some(c) = PathBuilder::from_circle(p.0, p.1, r) {
                    pm.fill_path(&c, &view.paint(*color), FillRule::Winding, id, None);
                    let ring = Stroke { width: (1.5 * s).max(1.0), ..Default::default() };
                    pm.stroke_path(&c, &view.paint(Color::from_rgba8(255, 255, 255, 230)), &ring, id, None);
                }
                let label = n.to_string();
                let fs = r * 1.15;
                let w = text::measure(fonts.bold(), fs, &label);
                let h = text::line_height(fonts.bold(), fs);
                let fg = if luminance(*color) > 0.6 { Color::BLACK } else { Color::WHITE };
                text::draw(pm, fonts.bold(), fs, p.0 - w / 2.0, p.1 - h / 2.0, &label, fg, view.swap_rb);
            }
            Shape::Pixelate { a, b, block } => {
                if let (Some((src, off)), Some(r)) = (src, norm_rect(view.px(*a), view.px(*b))) {
                    pixelate(pm, src, off, r, (block * s).max(2.0));
                }
            }
        }
    }
}

impl Shape {
    /// Generous logical bounding box (text is measured by the caller).
    pub fn bounds(&self, fonts: &Fonts) -> Option<Rect> {
        let pad = |r: Option<Rect>, m: f32| r.and_then(|r| Rect::from_ltrb(r.left() - m, r.top() - m, r.right() + m, r.bottom() + m));
        let of_pts = |pts: &[Pt]| {
            let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for p in pts {
                (l, t, r, b) = (l.min(p.0), t.min(p.1), r.max(p.0), b.max(p.1));
            }
            Rect::from_ltrb(l, t, r.max(l + 1.0), b.max(t + 1.0))
        };
        match self {
            Shape::Rect { a, b, width, .. } | Shape::Ellipse { a, b, width, .. } | Shape::Line { a, b, width, .. } => pad(of_pts(&[*a, *b]), width + 2.0),
            Shape::Arrow { a, b, width, .. } => pad(of_pts(&[*a, *b]), width * 3.0 + 12.0),
            Shape::Pixelate { a, b, block } => pad(of_pts(&[*a, *b]), block + 2.0),
            Shape::Pen { pts, width, .. } | Shape::Marker { pts, width, .. } => pad(of_pts(pts), width + 2.0),
            Shape::Counter { pos, radius, .. } => pad(of_pts(&[*pos]), radius + 3.0),
            Shape::Text { pos, text: t, size, .. } => {
                let w = t.split('\n').map(|l| text::measure(fonts.bold(), *size, l)).fold(0.0, f32::max);
                let h = text::line_height(fonts.bold(), *size) * (t.matches('\n').count() + 1) as f32;
                pad(Rect::from_xywh(pos.0, pos.1, w.max(1.0) + 4.0, h), 4.0)
            }
        }
    }
}

pub fn luminance(c: Color) -> f32 {
    0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue()
}

fn draw_arrow(pm: &mut PixmapMut, view: &View, a: Pt, b: Pt, color: Color, width: f32) {
    let s = view.scale;
    let (a, b) = (view.px(a), view.px(b));
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let head = ((width * 3.0 + 9.0) * s).min(len * 0.6);
    let half = head * 0.55;
    // Shaft stops inside the head so the round cap doesn't poke through the tip.
    let shaft_end = (b.0 - ux * head * 0.8, b.1 - uy * head * 0.8);
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(shaft_end.0, shaft_end.1);
    let paint = view.paint(color);
    if let Some(p) = pb.finish() {
        pm.stroke_path(&p, &paint, &stroke(width * s), Transform::identity(), None);
    }
    let base = (b.0 - ux * head, b.1 - uy * head);
    let mut pb = PathBuilder::new();
    pb.move_to(b.0, b.1);
    pb.line_to(base.0 - uy * half, base.1 + ux * half);
    pb.line_to(base.0 + uy * half, base.1 - ux * half);
    pb.close();
    if let Some(p) = pb.finish() {
        pm.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
        let st = Stroke { width: (width * s * 0.5).max(1.0), line_join: LineJoin::Round, ..Default::default() };
        pm.stroke_path(&p, &paint, &st, Transform::identity(), None);
    }
}

/// Replace `r` (target pixel space) with block averages of `src`; target pixel
/// (x, y) corresponds to source pixel (x + off.0, y + off.1). Blocks are aligned
/// in source space so partial repaints line up with each other.
fn pixelate(pm: &mut PixmapMut, src: PixmapRef, off: (i32, i32), r: Rect, block: f32) {
    let bs = block.round().max(2.0) as i32;
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    // Work in source coordinates, clipped to both images.
    let x0 = (r.left().floor() as i32 + off.0).clamp(0, sw);
    let y0 = (r.top().floor() as i32 + off.1).clamp(0, sh);
    let x1 = (r.right().ceil() as i32 + off.0).clamp(0, sw);
    let y1 = (r.bottom().ceil() as i32 + off.1).clamp(0, sh);
    let (tw, th) = (pm.width() as i32, pm.height() as i32);
    let sd = src.data();
    let stride = src.width() as usize * 4;
    let dstride = pm.width() as usize * 4;
    let dd = pm.data_mut();
    let mut by = y0;
    while by < y1 {
        let ey = (by + bs).min(y1);
        let mut bx = x0;
        while bx < x1 {
            let ex = (bx + bs).min(x1);
            if ex - off.0 <= 0 || bx - off.0 >= tw || ey - off.1 <= 0 || by - off.1 >= th {
                bx = ex; // block not visible in this (partial) target
                continue;
            }
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for y in by..ey {
                let row = y as usize * stride;
                for x in bx..ex {
                    let i = row + x as usize * 4;
                    for c in 0..4 {
                        acc[c] += sd[i + c] as u32;
                    }
                    n += 1;
                }
            }
            if n > 0 {
                let px = [(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8, (acc[3] / n) as u8];
                for y in (by - off.1).max(0)..(ey - off.1).min(th) {
                    let row = y as usize * dstride;
                    for x in (bx - off.0).max(0)..(ex - off.0).min(tw) {
                        let i = row + x as usize * 4;
                        dd[i..i + 4].copy_from_slice(&px);
                    }
                }
            }
            bx = ex;
        }
        by = ey;
    }
}

pub trait RectExt: Sized {
    fn contains(&self, x: f32, y: f32) -> bool;
    fn translate(&self, dx: f32, dy: f32) -> Option<Self>;
}

impl RectExt for Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left() && x < self.right() && y >= self.top() && y < self.bottom()
    }
    fn translate(&self, dx: f32, dy: f32) -> Option<Self> {
        Rect::from_xywh(self.left() + dx, self.top() + dy, self.width(), self.height())
    }
}
