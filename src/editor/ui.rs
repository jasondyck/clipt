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
    Record,
}

impl Button {
    fn ordinal(self) -> u8 {
        match self {
            Button::Tool(_) | Button::Color(_) => 0,
            Button::Undo => 1,
            Button::Redo => 2,
            Button::Save => 3,
            Button::Copy => 4,
            Button::Close => 5,
            Button::Record => 6,
        }
    }
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

fn items(ed: &Editor) -> Vec<Item> {
    if ed.record {
        return vec![Item::Btn(Button::Record), Item::Btn(Button::Close)];
    }
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
    let its = items(ed);
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

/// Margin around the selection covering its outline, handles and size label.
pub const SEL_MARGIN: f32 = 14.0;

pub fn inflate(r: Rect, m: f32) -> Option<Rect> {
    Rect::from_ltrb(r.left() - m, r.top() - m, r.right() + m, r.bottom() + m)
}

/// One piece of chrome for damage tracking: a key summarising everything that
/// affects how it looks, and the logical area it covers.
pub struct Layer {
    pub key: u64,
    pub rects: Vec<Rect>,
}

fn key(parts: &[f32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.to_bits().hash(&mut h);
    }
    h.finish()
}

fn rect_key(r: Option<Rect>) -> [f32; 4] {
    r.map_or([f32::NAN; 4], |r| [r.left(), r.top(), r.right(), r.bottom()])
}

/// Everything drawn on top of the background + committed shapes, as layers in
/// a fixed order. Must cover what `render` paints; the partial-repaint test
/// checks that.
pub fn dynamic_layers(ed: &Editor, frame: u64) -> Vec<Layer> {
    let m = 12.0;
    let grow = |v: Vec<Option<Rect>>| v.into_iter().flatten().filter_map(|r| inflate(r, m)).collect::<Vec<_>>();
    let mut layers = Vec::new();

    // selection outline, handles, size label
    let mut rects = Vec::new();
    if let Some(sel) = ed.sel {
        let w = SEL_MARGIN;
        rects.extend([
            Rect::from_ltrb(sel.left() - w, sel.top() - w, sel.right() + w, sel.top() + w),
            Rect::from_ltrb(sel.left() - w, sel.bottom() - w, sel.right() + w, sel.bottom() + w),
            Rect::from_ltrb(sel.left() - w, sel.top() - w, sel.left() + w, sel.bottom() + w),
            Rect::from_ltrb(sel.right() - w, sel.top() - w, sel.right() + w, sel.bottom() + w),
        ]);
        rects.push(size_label_rect(ed, sel));
    }
    let handles = ed.tool == super::Tool::Select && ed.text.is_none();
    let sk = rect_key(ed.sel);
    layers.push(Layer { key: key(&[sk[0], sk[1], sk[2], sk[3], handles as u8 as f32]), rects: grow(rects) });

    // hover target (window or monitor): outline, glow and card
    let (hk, rects) = match ed.hover_drawn() {
        Some(h) => {
            let r = h.rect;
            let w = GLOW + 2.0;
            let mut parts = rect_key(Some(r)).to_vec();
            parts.extend(h.name.chars().map(|c| c as u32 as f32));
            parts.push(h.is_window as u8 as f32);
            let rects = vec![
                Rect::from_ltrb(r.left() - w, r.top() - w, r.right() + w, r.top() + w),
                Rect::from_ltrb(r.left() - w, r.bottom() - w, r.right() + w, r.bottom() + w),
                Rect::from_ltrb(r.left() - w, r.top() - w, r.left() + w, r.bottom() + w),
                Rect::from_ltrb(r.right() - w, r.top() - w, r.right() + w, r.bottom() + w),
                hover_card(ed, &h).and_then(|c| inflate(c, CARD_SHADOW)),
            ];
            (key(&parts), rects)
        }
        None => (0, Vec::new()),
    };
    layers.push(Layer { key: hk, rects: grow(rects) });

    // crosshair + loupe
    let p = ed.pointer;
    let visible = ed.is_selecting() && ed.pointer_in;
    let mut rects = Vec::new();
    if visible && let Some(b) = ed.screens.iter().map(|s| s.bounds()).find(|b| b.contains(p.0, p.1)) {
        rects.push(Rect::from_xywh(b.left(), p.1 - 2.0, b.width(), 4.0));
        rects.push(Rect::from_xywh(p.0 - 2.0, b.top(), 4.0, b.height()));
        rects.push(loupe_rect(b, p));
    }
    layers.push(Layer { key: if visible { key(&[p.0, p.1]) } else { 0 }, rects: grow(rects) });

    // hint pill
    let hint = ed.sel.is_none() && !ed.is_adjusting();
    let rects = if hint {
        ed.screens.iter().map(|s| hint_rect(ed, s.bounds()).and_then(|r| inflate(r, CARD_SHADOW))).collect()
    } else {
        Vec::new()
    };
    layers.push(Layer { key: hint as u64 + 2 * !ed.windows.is_empty() as u64 + 4 * ed.record as u64, rects: grow(rects) });

    // toolbar + tooltip
    let tb = toolbar_layout(ed).map(|(panel, _)| panel);
    let hover = toolbar_hit(ed, p).filter(|_| ed.pointer_in);
    let hk = match hover {
        None => -1.0,
        Some(Button::Tool(t)) => t as u8 as f32,
        Some(Button::Color(i)) => 100.0 + i as f32,
        Some(b) => 200.0 + b.ordinal() as f32,
    };
    let tk = rect_key(tb);
    let rects = tb.and_then(|panel| Rect::from_ltrb(panel.left() - 80.0, panel.top() - 40.0, panel.right() + 80.0, panel.bottom() + 40.0));
    layers.push(Layer { key: key(&[tk[0], tk[1], tk[2], tk[3], hk, ed.tool as u8 as f32, ed.color as f32]), rects: grow(vec![rects]) });

    // stroke width hint
    let rects = if ed.width_hint { vec![Rect::from_xywh(p.0, p.1, 140.0, 60.0)] } else { Vec::new() };
    layers.push(Layer { key: if ed.width_hint { key(&[p.0, p.1, ed.width]) } else { 0 }, rects: grow(rects) });

    // shape being drawn: changes on every motion while it exists
    let ip = ed.in_progress().and_then(|s| s.bounds(&ed.fonts));
    layers.push(Layer { key: if ip.is_some() { frame } else { 0 }, rects: grow(vec![ip]) });

    // text being typed
    let (k, r) = match &ed.text {
        Some(te) => {
            let shape = Shape::Text { pos: te.pos, text: format!("{}MM", te.text), color: Color::BLACK, size: ed.text_size() };
            let mut parts = vec![te.pos.0, te.pos.1, ed.text_size(), ed.color as f32];
            parts.extend(te.text.chars().map(|c| c as u32 as f32));
            (key(&parts), shape.bounds(&ed.fonts))
        }
        None => (0, None),
    };
    layers.push(Layer { key: k, rects: grow(vec![r]) });
    layers
}

/// Paint screen `idx` into `pm`, whose (0, 0) is screen pixel `origin`. A full
/// frame passes the whole buffer and (0, 0); partial repaints pass a small
/// pixmap covering one damaged rectangle.
pub fn render(ed: &Editor, idx: usize, pm: &mut PixmapMut, origin: (i32, i32)) {
    let scr = &ed.screens[idx];
    let sv = scr.view(ed.swap_rb);
    let s = sv.scale;
    let view = View { off: (origin.0 as f32, origin.1 as f32), ..sv };
    let bounds = scr.bounds();

    // Background: dimmed everywhere, clean inside the selection.
    {
        let (pw, ph) = (pm.width() as usize, pm.height() as usize);
        let (bw, bh) = (scr.bg.width() as usize, scr.bg.height() as usize);
        let (ox, oy) = (origin.0.max(0) as usize, origin.1.max(0) as usize);
        let cols = pw.min(bw.saturating_sub(ox));
        let sel_px = ed.lit().and_then(|r| r.intersect(&bounds)).map(|sel| {
            let (x0, y0) = sv.px((sel.left(), sel.top()));
            let (x1, y1) = sv.px((sel.right(), sel.bottom()));
            (x0.round().max(0.0) as usize, y0.round().max(0.0) as usize, x1.round().max(0.0) as usize, y1.round().max(0.0) as usize)
        });
        let src = scr.bg.data();
        let dst = pm.data_mut();
        for y in 0..ph.min(bh.saturating_sub(oy)) {
            let sy = oy + y;
            let srow = &src[(sy * bw + ox) * 4..(sy * bw + ox + cols) * 4];
            let drow = &mut dst[y * pw * 4..(y * pw + cols) * 4];
            match sel_px {
                Some((x0, y0, x1, y1)) if sy >= y0 && sy < y1 => {
                    // [dim | clean | dim], in this pixmap's columns
                    let a = x0.saturating_sub(ox).min(cols);
                    let b = x1.saturating_sub(ox).min(cols);
                    dim_copy(&mut drow[..a * 4], &srow[..a * 4]);
                    drow[a * 4..b * 4].copy_from_slice(&srow[a * 4..b * 4]);
                    dim_copy(&mut drow[b * 4..], &srow[b * 4..]);
                }
                _ => dim_copy(drow, srow),
            }
        }
    }

    for shape in ed.shapes.iter().chain(ed.in_progress()) {
        shape.draw(pm, &view, Some((scr.bg.as_ref(), origin)), &ed.fonts);
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
    if let Some(h) = ed.hover_drawn() {
        draw_hover(ed, pm, &view, &h);
    }

    if ed.is_selecting() && ed.pointer_in && bounds.contains(ed.pointer.0, ed.pointer.1) {
        draw_crosshair(pm, &view, bounds, ed.pointer);
        draw_loupe(ed, idx, pm, &view, &sv, bounds);
    }

    if ed.sel.is_none() && !ed.is_adjusting() {
        draw_hint(ed, pm, &view, bounds);
    }

    if let Some((panel, btns)) = toolbar_layout(ed)
        && panel.intersect(&bounds).is_some()
    {
        draw_toolbar(ed, pm, &view, panel, &btns);
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
    if ed.tool == super::Tool::Select && ed.text.is_none() {
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
    let r = size_label_rect(ed, sel);
    let Some(r) = r else { return };
    pill(pm, view, r, 6.0);
    label(ed, pm, view, (r.left() + 8.0, r.top() + 4.5), 12.5, &size_label(ed, sel), ICON(), false);
}

// ---- hover target + hint: HUD-style cards ---------------------------------

/// How far the hover glow reaches outside the target.
const GLOW: f32 = 7.0;
/// How far a card's shadow reaches.
const CARD_SHADOW: f32 = 14.0;

#[allow(non_snake_case)]
fn MATERIAL() -> Color {
    Color::from_rgba8(0x1c, 0x1c, 0x1e, 222)
}
#[allow(non_snake_case)]
fn SECONDARY() -> Color {
    Color::from_rgba8(255, 255, 255, 150)
}

/// A dark translucent card with a hairline edge and a soft shadow: the
/// look of a system HUD.
fn card(pm: &mut PixmapMut, view: &View, r: Rect, rad: f32) {
    let id = Transform::identity();
    for i in 0..6 {
        let g = 2.0 + i as f32 * 2.2;
        if let Some(rr) = Rect::from_xywh(r.left() - g / 2.0, r.top() - g / 2.0 + 3.0, r.width() + g, r.height() + g) {
            pm.fill_path(&rounded(view, rr, rad + g / 2.0), &view.paint(Color::from_rgba8(0, 0, 0, 14)), FillRule::Winding, id, None);
        }
    }
    let path = rounded(view, r, rad);
    pm.fill_path(&path, &view.paint(MATERIAL()), FillRule::Winding, id, None);
    let st = Stroke { width: view.scale.max(1.0), ..Default::default() };
    pm.stroke_path(&path, &view.paint(Color::from_rgba8(255, 255, 255, 26)), &st, id, None);
}

/// "firefox" → "Firefox", "org.kde.dolphin" → "Dolphin", "claude-code" →
/// "Claude Code".
fn app_title(name: &str) -> String {
    let last = name.rsplit('.').next().unwrap_or(name);
    last.split(['-', '_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn hover_lines(ed: &Editor, h: &super::Hover) -> (String, String) {
    let size = size_label(ed, h.rect);
    if h.is_window { (app_title(&h.name), size) } else { ("Entire Screen".into(), format!("{}  ·  {}", h.name, size)) }
}

const CARD_TITLE: f32 = 16.0;
const CARD_SUB: f32 = 12.5;

/// The label card: centred in the target, or just below it (above near the
/// bottom of the screen) when the target is too small to hold it.
fn hover_card(ed: &Editor, h: &super::Hover) -> Option<Rect> {
    let (title, sub) = hover_lines(ed, h);
    let w = text::measure(ed.fonts.bold(), CARD_TITLE, &title).max(text::measure(&ed.fonts.regular, CARD_SUB, &sub)) + 40.0;
    let hgt = 62.0;
    let r = h.rect;
    let (cx, cy) = (r.left() + r.width() / 2.0, r.top() + r.height() / 2.0);
    let (x, y) = if r.width() >= w + 24.0 && r.height() >= hgt + 24.0 {
        (cx - w / 2.0, cy - hgt / 2.0)
    } else if r.bottom() + hgt + 16.0 <= ed.desktop().bottom() {
        (cx - w / 2.0, r.bottom() + 12.0)
    } else {
        (cx - w / 2.0, r.top() - hgt - 12.0)
    };
    Rect::from_xywh(x, y, w, hgt)
}

fn draw_hover(ed: &Editor, pm: &mut PixmapMut, view: &View, h: &super::Hover) {
    let s = view.scale;
    let id = Transform::identity();
    let rad = if h.is_window { 8.0 } else { 0.0 };
    // A whole screen's outline sits just inside its edges so all of it shows.
    let inset = if h.is_window { 0.0 } else { 1.5 };
    let Some(r) = Rect::from_ltrb(h.rect.left() + inset, h.rect.top() + inset, h.rect.right() - inset, h.rect.bottom() - inset) else {
        return;
    };
    let path = rounded(view, r, rad);
    let a = ACCENT();
    // Soft glow, then the crisp edge.
    for (w, alpha) in [(GLOW * 2.0, 0.10), (GLOW * 1.2, 0.18), (5.0, 0.30)] {
        let st = Stroke { width: w * s, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
        pm.stroke_path(&path, &view.paint(Color::from_rgba(a.red(), a.green(), a.blue(), alpha).unwrap()), &st, id, None);
    }
    let st = Stroke { width: (2.0 * s).max(1.5), line_join: tiny_skia::LineJoin::Round, ..Default::default() };
    pm.stroke_path(&path, &view.paint(a), &st, id, None);

    let Some(c) = hover_card(ed, h) else { return };
    card(pm, view, c, 12.0);
    let (title, sub) = hover_lines(ed, h);
    let tw = text::measure(ed.fonts.bold(), CARD_TITLE * s, &title) / s;
    let sw = text::measure(&ed.fonts.regular, CARD_SUB * s, &sub) / s;
    let mid = c.left() + c.width() / 2.0;
    label(ed, pm, view, (mid - tw / 2.0, c.top() + 11.0), CARD_TITLE, &title, Color::WHITE, true);
    label(ed, pm, view, (mid - sw / 2.0, c.top() + 36.0), CARD_SUB, &sub, SECONDARY(), false);
}

/// (key cap, what it does) for the hint bar.
fn hint_items(ed: &Editor) -> Vec<(&'static str, &'static str)> {
    if ed.record {
        return if ed.windows.is_empty() {
            vec![("Drag", "Area to record"), ("Click", "Screen"), ("Enter", "Record"), ("Esc", "Cancel")]
        } else {
            vec![("Drag", "Area to record"), ("Click", "Window"), ("Tab", "Screen"), ("Enter", "Record"), ("Esc", "Cancel")]
        };
    }
    if ed.windows.is_empty() {
        vec![("Drag", "Select area"), ("Click", "Screen"), ("Esc", "Cancel")]
    } else {
        vec![("Drag", "Select area"), ("Click", "Window"), ("Tab", "Screen"), ("Esc", "Cancel")]
    }
}

const HINT_FS: f32 = 13.0;
const CAP_FS: f32 = 11.5;
const HINT_H: f32 = 40.0;

fn hint_width(ed: &Editor) -> f32 {
    let items = hint_items(ed);
    let mut w = 16.0;
    for (i, (k, t)) in items.iter().enumerate() {
        w += text::measure(&ed.fonts.regular, CAP_FS, k) + 14.0 + 8.0 + text::measure(&ed.fonts.regular, HINT_FS, t);
        if i + 1 < items.len() {
            w += 22.0;
        }
    }
    w + 16.0
}

fn hint_rect(ed: &Editor, b: Rect) -> Option<Rect> {
    let w = hint_width(ed);
    Rect::from_xywh(b.left() + (b.width() - w) / 2.0, b.top() + 24.0, w, HINT_H)
}

fn draw_hint(ed: &Editor, pm: &mut PixmapMut, view: &View, b: Rect) {
    let Some(r) = hint_rect(ed, b) else { return };
    let s = view.scale;
    card(pm, view, r, HINT_H / 2.0);
    let id = Transform::identity();
    let mut x = r.left() + 16.0;
    let items = hint_items(ed);
    for (i, (k, t)) in items.iter().enumerate() {
        let kw = text::measure(&ed.fonts.regular, CAP_FS * s, k) / s + 14.0;
        if let Some(cap) = Rect::from_xywh(x, r.top() + 9.0, kw, 22.0) {
            let path = rounded(view, cap, 6.0);
            pm.fill_path(&path, &view.paint(Color::from_rgba8(255, 255, 255, 22)), FillRule::Winding, id, None);
            let st = Stroke { width: s.max(1.0), ..Default::default() };
            pm.stroke_path(&path, &view.paint(Color::from_rgba8(255, 255, 255, 46)), &st, id, None);
            label(ed, pm, view, (x + 7.0, cap.top() + 4.0), CAP_FS, k, Color::from_rgba8(255, 255, 255, 230), false);
        }
        x += kw + 8.0;
        label(ed, pm, view, (x, r.top() + 11.5), HINT_FS, t, SECONDARY(), false);
        x += text::measure(&ed.fonts.regular, HINT_FS * s, t) / s;
        if i + 1 < items.len() {
            circle(pm, view, (x + 11.0, r.top() + HINT_H / 2.0), 1.5, Some(Color::from_rgba8(255, 255, 255, 70)), None);
            x += 22.0;
        }
    }
}

/// Size in physical pixels: that's what the saved file will be.
fn size_label(ed: &Editor, sel: Rect) -> String {
    let scale = ed.screens.iter().find(|x| x.bounds().contains(sel.left() + 1.0, sel.top() + 1.0)).map_or(1.0, |x| x.scale);
    format!("{} × {}", (sel.width() * scale).round(), (sel.height() * scale).round())
}

fn size_label_rect(ed: &Editor, sel: Rect) -> Option<Rect> {
    // Measured at 1x: text width scales linearly, so this is close enough
    // for layout and damage.
    let w = text::measure(&ed.fonts.regular, 12.5, &size_label(ed, sel)) + 16.0;
    let h = 24.0;
    let y = if sel.top() - h - 6.0 >= ed.desktop().top() { sel.top() - h - 6.0 } else { sel.top() + 6.0 };
    Rect::from_xywh(sel.left(), y, w, h)
}

fn draw_crosshair(pm: &mut PixmapMut, view: &View, b: Rect, p: Pt) {
    let c = Color::from_rgba8(255, 255, 255, 70);
    let w = 1.0 / view.scale;
    fill_rect(pm, view, Rect::from_xywh(b.left(), p.1, b.width(), w), c);
    fill_rect(pm, view, Rect::from_xywh(p.0, b.top(), w, b.height()), c);
}

const LOUPE_N: i32 = 15; // sampled pixels per side
const LOUPE_CELL: f32 = 8.0; // logical size of one sampled pixel

/// Loupe square plus its coordinate label below.
fn loupe_rect(b: Rect, p: Pt) -> Option<Rect> {
    let size = LOUPE_N as f32 * LOUPE_CELL;
    let mut x = p.0 + 24.0;
    let mut y = p.1 + 24.0;
    if x + size > b.right() - 4.0 {
        x = p.0 - 24.0 - size;
    }
    if y + size + 30.0 > b.bottom() - 4.0 {
        y = p.1 - 24.0 - size - 30.0;
    }
    Rect::from_xywh(x, y, size, size + 30.0)
}

fn draw_loupe(ed: &Editor, idx: usize, pm: &mut PixmapMut, view: &View, sv: &View, b: Rect) {
    let scr = &ed.screens[idx];
    let n = LOUPE_N;
    let cell = LOUPE_CELL;
    let size = n as f32 * cell;
    let p = ed.pointer;
    let Some(lr) = loupe_rect(b, p) else { return };
    let (x, y) = (lr.left(), lr.top());
    let frame = Rect::from_xywh(x, y, size, size).unwrap();
    shadow(pm, view, frame, 10.0);
    let (cx, cy) = sv.px(p);
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
                    pm.fill_path(
                        &rounded(view, inset, 8.0),
                        &view.paint(Color::from_rgba8(255, 255, 255, 22)),
                        FillRule::Winding,
                        id,
                        None,
                    );
                }
                let fg = if active {
                    Color::WHITE
                } else if matches!(b, Button::Close) {
                    MUTED()
                } else {
                    ICON()
                };
                icon(ed, pm, view, r, b, fg);
            }
        }
    }

    // Tooltip for the hovered button.
    if let Some(b) = hover
        && let Some((r, _)) = btns.iter().find(|(_, x)| *x == b)
    {
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
        Button::Record => "Record  Enter".into(),
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
        Button::Record => {
            let c = g(12.0, 12.0);
            if let Some(p) = PathBuilder::from_circle(c.0, c.1, 7.0 * k * s) {
                pm.fill_path(&p, &view.paint(Color::from_rgba8(0xef, 0x44, 0x44, 255)), FillRule::Winding, id, None);
            }
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

/// Copy pixels darkened to ~55% (the dim outside the selection); alpha stays.
/// Works a whole pixel at a time: two channels per multiply, which the
/// compiler vectorises. Byte order doesn't matter as long as alpha is byte 3.
fn dim_copy(dst: &mut [u8], src: &[u8]) {
    let (d, s) = (as_u32_mut(dst), as_u32(src));
    for (d, &p) in d.iter_mut().zip(s) {
        let p = u32::from_le(p);
        let rb = (((p & 0x00ff_00ff) * 141) >> 8) & 0x00ff_00ff;
        let g = (((p & 0x0000_ff00) * 141) >> 8) & 0x0000_ff00;
        *d = (rb | g | (p & 0xff00_0000)).to_le();
    }
}

fn as_u32(b: &[u8]) -> &[u32] {
    // Pixmap and shm data are 4-byte aligned and whole pixels.
    let (pre, mid, post) = unsafe { b.align_to::<u32>() };
    assert!(pre.is_empty() && post.is_empty());
    mid
}

fn as_u32_mut(b: &mut [u8]) -> &mut [u32] {
    let (pre, mid, post) = unsafe { b.align_to_mut::<u32>() };
    assert!(pre.is_empty() && post.is_empty());
    mid
}

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

#[allow(clippy::too_many_arguments)] // plain drawing call; a params struct would only add noise
fn label(ed: &Editor, pm: &mut PixmapMut, view: &View, pos: Pt, size: f32, t: &str, c: Color, bold: bool) {
    let p = view.px(pos);
    let font = if bold { ed.fonts.bold() } else { &ed.fonts.regular };
    text::draw(pm, font, size * view.scale, p.0, p.1, t, c, view.swap_rb);
}
