//! Painting the editor onto one output, plus toolbar layout and hit-testing.
//! Everything is laid out in logical units and multiplied by the output scale,
//! so the chrome stays crisp on HiDPI screens.

use super::shapes::{Pt, RectExt, Shape, Tool, View};
use super::{Editor, PALETTE};
use crate::text;
use tiny_skia::{Color, FillRule, LineCap, Path, PathBuilder, PixmapMut, Rect, Stroke, Transform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Tool(Tool),
    Color(usize),
    Undo,
    Redo,
    Save,
    Copy,
    Close,
}

const B: f32 = 34.0; // button size
const GAP: f32 = 2.0;
const PAD: f32 = 6.0;
const SEP: f32 = 13.0;
const DOT: f32 = 22.0;

#[allow(non_snake_case)]
fn ACCENT() -> Color {
    Color::from_rgba8(0x3b, 0x82, 0xf6, 255)
}
#[allow(non_snake_case)]
fn PANEL() -> Color {
    Color::from_rgba8(0x18, 0x18, 0x1b, 240)
}
#[allow(non_snake_case)]
fn BORDER() -> Color {
    Color::from_rgba8(255, 255, 255, 28)
}
#[allow(non_snake_case)]
fn ICON() -> Color {
    Color::from_rgba8(0xe4, 0xe4, 0xe7, 255)
}
#[allow(non_snake_case)]
fn MUTED() -> Color {
    Color::from_rgba8(0xa1, 0xa1, 0xaa, 255)
}

enum Item {
    Btn(Button),
    Sep,
}

fn items() -> Vec<Item> {
    let mut v = vec![Item::Btn(Button::Tool(Tool::Select))];
    v.extend(Tool::DRAWING.iter().map(|&t| Item::Btn(Button::Tool(t))));
    v.push(Item::Sep);
    v.extend((0..PALETTE.len()).map(|i| Item::Btn(Button::Color(i))));
    v.push(Item::Sep);
    v.extend([Item::Btn(Button::Undo), Item::Btn(Button::Redo), Item::Sep]);
    v.extend([Item::Btn(Button::Save), Item::Btn(Button::Copy), Item::Btn(Button::Close)]);
    v
}

/// Toolbar panel rect and button rects, in logical coordinates.
pub fn toolbar_layout(ed: &Editor) -> Option<(Rect, Vec<(Rect, Button)>)> {
    let sel = ed.sel?;
    if ed.is_adjusting() {
        return None;
    }
    let its = items();
    let width: f32 = PAD * 2.0
        + its
            .iter()
            .map(|i| match i {
                Item::Sep => SEP,
                Item::Btn(Button::Color(_)) => DOT,
                Item::Btn(_) => B + GAP,
            })
            .sum::<f32>();
    let height = B + PAD * 2.0;
    let center = (sel.left() + sel.width() / 2.0, sel.top() + sel.height() / 2.0);
    let scr = ed.screens.iter().map(|s| s.bounds()).find(|b| b.contains(center.0, center.1)).unwrap_or_else(|| ed.desktop());
    let margin = 10.0;
    let y = if sel.bottom() + margin + height <= scr.bottom() - 6.0 {
        sel.bottom() + margin
    } else if sel.top() - margin - height >= scr.top() + 6.0 {
        sel.top() - margin - height
    } else {
        (sel.bottom() - margin - height).max(scr.top() + 6.0)
    };
    let x = (center.0 - width / 2.0).clamp(scr.left() + 6.0, (scr.right() - width - 6.0).max(scr.left() + 6.0));
    let panel = Rect::from_xywh(x, y, width, height)?;
    let mut out = Vec::new();
    let mut cx = x + PAD;
    for it in its {
        match it {
            Item::Sep => cx += SEP,
            Item::Btn(b @ Button::Color(_)) => {
                out.push((Rect::from_xywh(cx, y + PAD, DOT, B)?, b));
                cx += DOT;
            }
            Item::Btn(b) => {
                out.push((Rect::from_xywh(cx, y + PAD, B, B)?, b));
                cx += B + GAP;
            }
        }
    }
    Some((panel, out))
}

pub fn toolbar_hit(ed: &Editor, p: Pt) -> Option<Button> {
    let (panel, btns) = toolbar_layout(ed)?;
    if !panel.contains(p.0, p.1) {
        return None;
    }
    // Clicks on panel padding are swallowed rather than starting a drag.
    btns.iter().find(|(r, _)| r.contains(p.0, p.1)).map(|(_, b)| *b).or(Some(Button::Tool(ed.tool))).filter(|_| true)
}

// ---- painting ---------------------------------------------------------------

pub fn render(ed: &Editor, idx: usize, pm: &mut PixmapMut) {
    let scr = &ed.screens[idx];
    let view = scr.view(ed.swap_rb);
    let s = view.scale;
    let bounds = scr.bounds();
    let (pw, ph) = (pm.width() as usize, pm.height() as usize);

    // Background: dimmed everywhere, clean inside the selection.
    {
        let dst = pm.data_mut();
        dst.copy_from_slice(scr.dim.data());
        if let Some(sel) = ed.sel.and_then(|r| r.intersect(&bounds)) {
            let (x0, y0) = view.px((sel.left(), sel.top()));
            let (x1, y1) = view.px((sel.right(), sel.bottom()));
            let (x0, x1) = ((x0.round().max(0.0) as usize).min(pw), (x1.round().max(0.0) as usize).min(pw));
            let (y0, y1) = ((y0.round().max(0.0) as usize).min(ph), (y1.round().max(0.0) as usize).min(ph));
            let src = scr.bg.data();
            for y in y0..y1 {
                let a = (y * pw + x0) * 4;
                let b = (y * pw + x1) * 4;
                dst[a..b].copy_from_slice(&src[a..b]);
            }
        }
    }

    for shape in ed.shapes.iter().chain(ed.in_progress()) {
        shape.draw(pm, &view, Some(scr.bg.as_ref()), &ed.fonts);
    }
    if let Some(te) = &ed.text {
        let size = ed.text_size();
        let shape = Shape::Text { pos: te.pos, text: te.text.clone(), color: ed.current_color(), size };
        shape.draw(pm, &view, None, &ed.fonts);
        // caret after the last line
        let last = te.text.rsplit('\n').next().unwrap_or("");
        let lines = te.text.matches('\n').count() as f32;
        let lh = text::line_height(ed.fonts.bold(), size * s) / s;
        let cx = te.pos.0 + text::measure(ed.fonts.bold(), size * s, last) / s + 1.0;
        let cy = te.pos.1 + lines * lh;
        fill_rect(pm, &view, Rect::from_xywh(cx, cy, 2.0, lh), ed.current_color());
    }

    if let Some(sel) = ed.sel {
        draw_selection(ed, pm, &view, sel);
    }

    if ed.is_selecting() && ed.pointer_in && bounds.contains(ed.pointer.0, ed.pointer.1) {
        draw_crosshair(pm, &view, bounds, ed.pointer);
        draw_loupe(ed, idx, pm, &view, bounds);
    }

    if ed.sel.is_none() && !ed.is_adjusting() {
        let hint = "Drag to select   ·   Click for full screen   ·   Esc to cancel";
        let fs = 14.0;
        let w = text::measure(&ed.fonts.regular, fs * s, hint) / s + 28.0;
        let r = Rect::from_xywh(bounds.left() + (bounds.width() - w) / 2.0, bounds.top() + 28.0, w, 34.0).unwrap();
        pill(pm, &view, r, 17.0);
        label(ed, pm, &view, (r.left() + 14.0, r.top() + 8.0), fs, hint, ICON(), false);
    }

    if let Some((panel, btns)) = toolbar_layout(ed) {
        if panel.intersect(&bounds).is_some() {
            draw_toolbar(ed, pm, &view, panel, &btns);
        }
    }

    if ed.width_hint && bounds.contains(ed.pointer.0, ed.pointer.1) {
        let t = format!("{} px", ed.width as u32);
        let w = text::measure(&ed.fonts.regular, 13.0 * s, &t) / s + 20.0;
        let r = Rect::from_xywh(ed.pointer.0 + 16.0, ed.pointer.1 + 16.0, w, 26.0).unwrap();
        pill(pm, &view, r, 13.0);
        label(ed, pm, &view, (r.left() + 10.0, r.top() + 5.0), 13.0, &t, ICON(), false);
    }
}

fn draw_selection(ed: &Editor, pm: &mut PixmapMut, view: &View, sel: Rect) {
    let s = view.scale;
    let id = Transform::identity();
    let (a, b) = (view.px((sel.left(), sel.top())), view.px((sel.right(), sel.bottom())));
    if let Some(r) = Rect::from_ltrb(a.0, a.1, b.0, b.1) {
        let st = Stroke { width: (1.5 * s).max(1.0), ..Default::default() };
        pm.stroke_path(&PathBuilder::from_rect(r), &view.paint(ACCENT()), &st, id, None);
    }
    if ed.tool == super::Tool::Select && !matches!(ed.text, Some(_)) {
        let hs = 8.0;
        let (cx, cy) = (sel.left() + sel.width() / 2.0, sel.top() + sel.height() / 2.0);
        for (x, y) in [
            (sel.left(), sel.top()),
            (cx, sel.top()),
            (sel.right(), sel.top()),
            (sel.left(), cy),
            (sel.right(), cy),
            (sel.left(), sel.bottom()),
            (cx, sel.bottom()),
            (sel.right(), sel.bottom()),
        ] {
            let r = Rect::from_xywh(x - hs / 2.0, y - hs / 2.0, hs, hs).unwrap();
            let path = rounded(view, r, 2.0);
            pm.fill_path(&path, &view.paint(Color::WHITE), FillRule::Winding, id, None);
            let st = Stroke { width: (1.5 * s).max(1.0), ..Default::default() };
            pm.stroke_path(&path, &view.paint(ACCENT()), &st, id, None);
        }
    }
    // Size label in physical pixels: that's what the saved file will be.
    let scale = ed.screens.iter().find(|x| x.bounds().contains(sel.left() + 1.0, sel.top() + 1.0)).map_or(s, |x| x.scale);
    let t = format!("{} × {}", (sel.width() * scale).round(), (sel.height() * scale).round());
    let fs = 12.5;
    let w = text::measure(&ed.fonts.regular, fs * s, &t) / s + 16.0;
    let h = 24.0;
    let y = if sel.top() - h - 6.0 >= ed.desktop().top() { sel.top() - h - 6.0 } else { sel.top() + 6.0 };
    let r = Rect::from_xywh(sel.left(), y, w, h).unwrap();
    pill(pm, view, r, 6.0);
    label(ed, pm, view, (r.left() + 8.0, r.top() + 4.5), fs, &t, ICON(), false);
}

fn draw_crosshair(pm: &mut PixmapMut, view: &View, b: Rect, p: Pt) {
    let c = Color::from_rgba8(255, 255, 255, 70);
    let w = 1.0 / view.scale;
    fill_rect(pm, view, Rect::from_xywh(b.left(), p.1, b.width(), w), c);
    fill_rect(pm, view, Rect::from_xywh(p.0, b.top(), w, b.height()), c);
}

fn draw_loupe(ed: &Editor, idx: usize, pm: &mut PixmapMut, view: &View, b: Rect) {
    let scr = &ed.screens[idx];
    let n: i32 = 15; // sampled pixels per side
    let cell = 8.0; // logical size of one sampled pixel
    let size = n as f32 * cell;
    let p = ed.pointer;
    let mut x = p.0 + 24.0;
    let mut y = p.1 + 24.0;
    if x + size > b.right() - 4.0 {
        x = p.0 - 24.0 - size;
    }
    if y + size + 30.0 > b.bottom() - 4.0 {
        y = p.1 - 24.0 - size - 30.0;
    }
    let frame = Rect::from_xywh(x, y, size, size).unwrap();
    shadow(pm, view, frame, 10.0);
    let (cx, cy) = view.px(p);
    let (cx, cy) = (cx.floor() as i32, cy.floor() as i32);
    let src = scr.bg.data();
    let (bw, bh) = (scr.bg.width() as i32, scr.bg.height() as i32);
    for j in 0..n {
        for i in 0..n {
            let sx = (cx + i - n / 2).clamp(0, bw - 1);
            let sy = (cy + j - n / 2).clamp(0, bh - 1);
            let k = ((sy * bw + sx) * 4) as usize;
            // Already in surface order, so bypass the paint's swap.
            let c = Color::from_rgba8(src[k], src[k + 1], src[k + 2], 255);
            let r = Rect::from_xywh(x + i as f32 * cell, y + j as f32 * cell, cell, cell).unwrap();
            let (a, bb) = (view.px((r.left(), r.top())), view.px((r.right(), r.bottom())));
            let mut paint = tiny_skia::Paint::default();
            paint.set_color(c);
            if let Some(rr) = Rect::from_ltrb(a.0.floor(), a.1.floor(), bb.0.floor(), bb.1.floor()) {
                pm.fill_rect(rr, &paint, Transform::identity(), None);
            }
        }
    }
    let mid = Rect::from_xywh(x + (n / 2) as f32 * cell, y + (n / 2) as f32 * cell, cell, cell).unwrap();
    stroke_rect(pm, view, mid, ACCENT(), 1.5);
    stroke_rect(pm, view, frame, Color::from_rgba8(255, 255, 255, 200), 1.0);
    let t = format!("{}, {}", cx, cy);
    let fs = 12.0;
    let w = text::measure(&ed.fonts.regular, fs * view.scale, &t) / view.scale + 14.0;
    let r = Rect::from_xywh(x + (size - w) / 2.0, y + size + 6.0, w, 22.0).unwrap();
    pill(pm, view, r, 6.0);
    label(ed, pm, view, (r.left() + 7.0, r.top() + 4.0), fs, &t, ICON(), false);
}

fn draw_toolbar(ed: &Editor, pm: &mut PixmapMut, view: &View, panel: Rect, btns: &[(Rect, Button)]) {
    let id = Transform::identity();
    shadow(pm, view, panel, 12.0);
    let path = rounded(view, panel, 12.0);
    pm.fill_path(&path, &view.paint(PANEL()), FillRule::Winding, id, None);
    pm.stroke_path(&path, &view.paint(BORDER()), &Stroke { width: view.scale, ..Default::default() }, id, None);

    let hover = toolbar_hit(ed, ed.pointer).filter(|_| ed.pointer_in);
    for &(r, b) in btns {
        match b {
            Button::Color(i) => {
                let (cr, cg, cb) = PALETTE[i];
                let c = Color::from_rgba8(cr, cg, cb, 255);
                let (cx, cy) = (r.left() + r.width() / 2.0, r.top() + r.height() / 2.0);
                let rad = if ed.color == i { 8.0 } else { 6.5 };
                if ed.color == i {
                    circle(pm, view, (cx, cy), rad + 3.0, None, Some((Color::WHITE, 1.5)));
                }
                circle(pm, view, (cx, cy), rad, Some(c), Some((Color::from_rgba8(255, 255, 255, 60), 1.0)));
            }
            _ => {
                let active = matches!(b, Button::Tool(t) if t == ed.tool);
                let inset = Rect::from_xywh(r.left() + 1.0, r.top() + 1.0, r.width() - 2.0, r.height() - 2.0).unwrap();
                if active {
                    pm.fill_path(&rounded(view, inset, 8.0), &view.paint(ACCENT()), FillRule::Winding, id, None);
                } else if hover == Some(b) {
                    pm.fill_path(&rounded(view, inset, 8.0), &view.paint(Color::from_rgba8(255, 255, 255, 22)), FillRule::Winding, id, None);
                }
                let fg = if active { Color::WHITE } else if matches!(b, Button::Close) { MUTED() } else { ICON() };
                icon(ed, pm, view, r, b, fg);
            }
        }
    }

    // Tooltip for the hovered button.
    if let Some(b) = hover {
        if let Some((r, _)) = btns.iter().find(|(_, x)| *x == b) {
            let t = tooltip(b);
            let fs = 12.0;
            let w = text::measure(&ed.fonts.regular, fs * view.scale, &t) / view.scale + 16.0;
            let above = panel.top() - 30.0;
            let y = if above > ed.desktop().top() { above } else { panel.bottom() + 6.0 };
            let x = r.left() + r.width() / 2.0 - w / 2.0;
            let tr = Rect::from_xywh(x, y, w, 24.0).unwrap();
            pill(pm, view, tr, 6.0);
            label(ed, pm, view, (tr.left() + 8.0, tr.top() + 4.5), fs, &t, ICON(), false);
        }
    }
}

fn tooltip(b: Button) -> String {
    let name = |t: Tool| match t {
        Tool::Select => "Select",
        Tool::Rect => "Rectangle",
        Tool::Ellipse => "Ellipse",
        Tool::Arrow => "Arrow",
        Tool::Line => "Line",
        Tool::Pen => "Pen",
        Tool::Marker => "Highlighter",
        Tool::Text => "Text",
        Tool::Pixelate => "Pixelate",
        Tool::Counter => "Counter",
    };
    match b {
        Button::Tool(t) => format!("{}  {}", name(t), t.hotkey().to_ascii_uppercase()),
        Button::Color(i) => format!("Colour  {}", i + 1),
        Button::Undo => "Undo  Ctrl+Z".into(),
        Button::Redo => "Redo  Ctrl+Shift+Z".into(),
        Button::Save => "Save  Ctrl+S".into(),
        Button::Copy => "Copy  Enter".into(),
        Button::Close => "Cancel  Esc".into(),
    }
}

// ---- icons ------------------------------------------------------------------

/// Icons are drawn on a 24-unit grid centred in the button.
fn icon(ed: &Editor, pm: &mut PixmapMut, view: &View, r: Rect, b: Button, fg: Color) {
    let s = view.scale;
    let ox = r.left() + (r.width() - 20.0) / 2.0;
    let oy = r.top() + (r.height() - 20.0) / 2.0;
    let k = 20.0 / 24.0;
    let g = |x: f32, y: f32| view.px((ox + x * k, oy + y * k));
    let st = Stroke { width: 1.8 * s, line_cap: LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
    let paint = view.paint(fg);
    let id = Transform::identity();
    let mut pb = PathBuilder::new();
    let line = |pb: &mut PathBuilder, pts: &[(f32, f32)]| {
        let p0 = g(pts[0].0, pts[0].1);
        pb.move_to(p0.0, p0.1);
        for &(x, y) in &pts[1..] {
            let p = g(x, y);
            pb.line_to(p.0, p.1);
        }
    };
    let mut fill: Option<Path> = None;
    match b {
        Button::Tool(Tool::Select) => {
            // cursor arrow
            line(&mut pb, &[(6.0, 3.5), (6.0, 19.0), (10.0, 15.0), (13.0, 21.0), (15.5, 20.0), (12.5, 14.0), (18.0, 14.0)]);
            pb.close();
        }
        Button::Tool(Tool::Rect) => line(&mut pb, &[(4.0, 6.0), (20.0, 6.0), (20.0, 18.0), (4.0, 18.0), (4.0, 6.0)]),
        Button::Tool(Tool::Ellipse) => {
            let (a, c) = (g(3.5, 5.5), g(20.5, 18.5));
            if let Some(p) = Rect::from_ltrb(a.0, a.1, c.0, c.1).and_then(PathBuilder::from_oval) {
                pm.stroke_path(&p, &paint, &st, id, None);
            }
        }
        Button::Tool(Tool::Arrow) => {
            line(&mut pb, &[(5.0, 19.0), (18.0, 6.0)]);
            line(&mut pb, &[(10.0, 6.0), (18.0, 6.0), (18.0, 14.0)]);
        }
        Button::Tool(Tool::Line) => line(&mut pb, &[(5.0, 19.0), (19.0, 5.0)]),
        Button::Tool(Tool::Pen) => {
            let p0 = g(3.0, 17.0);
            pb.move_to(p0.0, p0.1);
            let (c1, e1) = (g(7.0, 5.0), g(11.0, 12.0));
            pb.quad_to(c1.0, c1.1, e1.0, e1.1);
            let (c2, e2) = (g(15.0, 19.0), g(21.0, 7.0));
            pb.quad_to(c2.0, c2.1, e2.0, e2.1);
        }
        Button::Tool(Tool::Marker) => {
            let thick = Stroke { width: 5.0 * s, line_cap: LineCap::Square, ..st.clone() };
            let mut hb = PathBuilder::new();
            line(&mut hb, &[(5.0, 15.0), (19.0, 15.0)]);
            if let Some(p) = hb.finish() {
                let mut c = fg;
                c.set_alpha(0.45);
                pm.stroke_path(&p, &view.paint(c), &thick, id, None);
            }
            line(&mut pb, &[(8.0, 11.0), (14.0, 5.0), (17.0, 8.0), (11.0, 14.0), (8.0, 11.0)]);
        }
        Button::Tool(Tool::Text) => {
            line(&mut pb, &[(5.0, 7.0), (5.0, 5.0), (19.0, 5.0), (19.0, 7.0)]);
            line(&mut pb, &[(12.0, 5.0), (12.0, 19.0)]);
            line(&mut pb, &[(9.0, 19.0), (15.0, 19.0)]);
        }
        Button::Tool(Tool::Pixelate) => {
            let mut fb = PathBuilder::new();
            for (i, j) in [(0, 0), (2, 0), (1, 1), (3, 1), (0, 2), (2, 2), (1, 3), (3, 3)] {
                let (a, c) = (g(4.0 + i as f32 * 4.0, 4.0 + j as f32 * 4.0), g(8.0 + i as f32 * 4.0, 8.0 + j as f32 * 4.0));
                if let Some(rr) = Rect::from_ltrb(a.0, a.1, c.0, c.1) {
                    fb.push_rect(rr);
                }
            }
            fill = fb.finish();
            line(&mut pb, &[(4.0, 4.0), (20.0, 4.0), (20.0, 20.0), (4.0, 20.0), (4.0, 4.0)]);
        }
        Button::Tool(Tool::Counter) => {
            let c = g(12.0, 12.0);
            if let Some(p) = PathBuilder::from_circle(c.0, c.1, 8.5 * k * s) {
                pm.stroke_path(&p, &paint, &st, id, None);
            }
            let fs = 11.0;
            let w = text::measure(ed.fonts.bold(), fs * s, "1") / s;
            let h = text::line_height(ed.fonts.bold(), fs * s) / s;
            label(ed, pm, view, (ox + 12.0 * k - w / 2.0, oy + 12.0 * k - h / 2.0), fs, "1", fg, true);
        }
        Button::Undo | Button::Redo => {
            let flip = matches!(b, Button::Redo);
            let fx = |x: f32| if flip { 24.0 - x } else { x };
            let p0 = g(fx(8.0), 9.0);
            pb.move_to(p0.0, p0.1);
            let (c1, e1) = (g(fx(20.0), 7.0), g(fx(18.0), 16.0));
            pb.quad_to(c1.0, c1.1, e1.0, e1.1);
            let (c2, e2) = (g(fx(17.0), 19.0), g(fx(12.0), 19.0));
            pb.quad_to(c2.0, c2.1, e2.0, e2.1);
            line(&mut pb, &[(fx(12.0), 5.0), (fx(8.0), 9.0), (fx(12.0), 13.0)]);
        }
        Button::Save => {
            line(&mut pb, &[(12.0, 4.0), (12.0, 15.0)]);
            line(&mut pb, &[(7.5, 10.5), (12.0, 15.0), (16.5, 10.5)]);
            line(&mut pb, &[(4.0, 15.0), (4.0, 20.0), (20.0, 20.0), (20.0, 15.0)]);
        }
        Button::Copy => {
            line(&mut pb, &[(8.0, 8.0), (20.0, 8.0), (20.0, 20.0), (8.0, 20.0), (8.0, 8.0)]);
            line(&mut pb, &[(16.0, 8.0), (16.0, 4.0), (4.0, 4.0), (4.0, 16.0), (8.0, 16.0)]);
        }
        Button::Close => {
            line(&mut pb, &[(6.0, 6.0), (18.0, 18.0)]);
            line(&mut pb, &[(18.0, 6.0), (6.0, 18.0)]);
        }
        Button::Color(_) => {}
    }
    if let Some(p) = fill {
        pm.fill_path(&p, &paint, FillRule::Winding, id, None);
    }
    if let Some(p) = pb.finish() {
        pm.stroke_path(&p, &paint, &st, id, None);
    }
}

// ---- primitives ---------------------------------------------------------------

fn rounded(view: &View, r: Rect, rad: f32) -> Path {
    let (a, b) = (view.px((r.left(), r.top())), view.px((r.right(), r.bottom())));
    let rad = (rad * view.scale).min((b.0 - a.0) / 2.0).min((b.1 - a.1) / 2.0);
    let k = rad * 0.4477; // control offset for a circular-looking cubic corner
    let mut pb = PathBuilder::new();
    pb.move_to(a.0 + rad, a.1);
    pb.line_to(b.0 - rad, a.1);
    pb.cubic_to(b.0 - k, a.1, b.0, a.1 + k, b.0, a.1 + rad);
    pb.line_to(b.0, b.1 - rad);
    pb.cubic_to(b.0, b.1 - k, b.0 - k, b.1, b.0 - rad, b.1);
    pb.line_to(a.0 + rad, b.1);
    pb.cubic_to(a.0 + k, b.1, a.0, b.1 - k, a.0, b.1 - rad);
    pb.line_to(a.0, a.1 + rad);
    pb.cubic_to(a.0, a.1 + k, a.0 + k, a.1, a.0 + rad, a.1);
    pb.close();
    pb.finish().unwrap_or_else(|| PathBuilder::from_rect(Rect::from_xywh(a.0, a.1, 1.0, 1.0).unwrap()))
}

fn pill(pm: &mut PixmapMut, view: &View, r: Rect, rad: f32) {
    let path = rounded(view, r, rad);
    pm.fill_path(&path, &view.paint(PANEL()), FillRule::Winding, Transform::identity(), None);
    pm.stroke_path(&path, &view.paint(BORDER()), &Stroke { width: view.scale, ..Default::default() }, Transform::identity(), None);
}

fn shadow(pm: &mut PixmapMut, view: &View, r: Rect, rad: f32) {
    for i in 1..=4 {
        let g = i as f32 * 2.0;
        if let Some(rr) = Rect::from_xywh(r.left() - g / 2.0, r.top() - g / 2.0 + 2.0, r.width() + g, r.height() + g) {
            let path = rounded(view, rr, rad + g / 2.0);
            pm.fill_path(&path, &view.paint(Color::from_rgba8(0, 0, 0, 22)), FillRule::Winding, Transform::identity(), None);
        }
    }
}

fn fill_rect(pm: &mut PixmapMut, view: &View, r: Option<Rect>, c: Color) {
    let Some(r) = r else { return };
    let (a, b) = (view.px((r.left(), r.top())), view.px((r.right(), r.bottom())));
    if let Some(rr) = Rect::from_ltrb(a.0, a.1, b.0.max(a.0 + 1.0), b.1.max(a.1 + 1.0)) {
        pm.fill_rect(rr, &view.paint(c), Transform::identity(), None);
    }
}

fn stroke_rect(pm: &mut PixmapMut, view: &View, r: Rect, c: Color, w: f32) {
    let (a, b) = (view.px((r.left(), r.top())), view.px((r.right(), r.bottom())));
    if let Some(rr) = Rect::from_ltrb(a.0, a.1, b.0, b.1) {
        let st = Stroke { width: w * view.scale, ..Default::default() };
        pm.stroke_path(&PathBuilder::from_rect(rr), &view.paint(c), &st, Transform::identity(), None);
    }
}

fn circle(pm: &mut PixmapMut, view: &View, c: Pt, r: f32, fill: Option<Color>, stroke: Option<(Color, f32)>) {
    let p = view.px(c);
    let Some(path) = PathBuilder::from_circle(p.0, p.1, r * view.scale) else { return };
    if let Some(f) = fill {
        pm.fill_path(&path, &view.paint(f), FillRule::Winding, Transform::identity(), None);
    }
    if let Some((sc, w)) = stroke {
        pm.stroke_path(&path, &view.paint(sc), &Stroke { width: w * view.scale, ..Default::default() }, Transform::identity(), None);
    }
}

fn label(ed: &Editor, pm: &mut PixmapMut, view: &View, pos: Pt, size: f32, t: &str, c: Color, bold: bool) {
    let p = view.px(pos);
    let font = if bold { ed.fonts.bold() } else { &ed.fonts.regular };
    text::draw(pm, font, size * view.scale, p.0, p.1, t, c, view.swap_rb);
}
