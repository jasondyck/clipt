//! The capture editor: selection, annotation and the toolbar. Knows nothing
//! about Wayland; the overlay feeds it input in logical desktop coordinates and
//! asks it to paint each output.

pub mod shapes;
pub mod ui;

use crate::capture::{Output, Shot};
use crate::image::Image;
use crate::text::Fonts;
use shapes::{Pt, RectExt, Shape, Tool, View, norm_rect};
use tiny_skia::{Color, FilterQuality, Pixmap, PixmapPaint, Rect, Transform};

pub const PALETTE: [(u8, u8, u8); 8] = [
    (0xef, 0x44, 0x44), // red
    (0xf9, 0x73, 0x16), // orange
    (0xea, 0xb3, 0x08), // yellow
    (0x22, 0xc5, 0x5e), // green
    (0x3b, 0x82, 0xf6), // blue
    (0xa8, 0x55, 0xf7), // purple
    (0xff, 0xff, 0xff), // white
    (0x11, 0x11, 0x11), // black
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Copy,
    Save,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    Crosshair,
    Move,
    Resize(Handle),
    Pointer,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Escape,
    Enter,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    Char(char),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

pub struct Screen {
    pub output: Output,
    /// The capture in surface byte order. The only full-size copy we keep:
    /// the dimmed look is applied while copying it into the frame.
    pub bg: Pixmap,
    pub scale: f32,
}

impl Screen {
    pub fn bounds(&self) -> Rect {
        let o = &self.output;
        Rect::from_xywh(o.x as f32, o.y as f32, o.width as f32, o.height as f32).unwrap()
    }
    pub fn view(&self, swap_rb: bool) -> View {
        View { ox: self.output.x as f32, oy: self.output.y as f32, scale: self.scale, swap_rb, off: (0.0, 0.0) }
    }
}

/// What changed since the last frame, in logical coordinates.
pub enum Damage {
    Full,
    Rects(Vec<Rect>),
}

enum Drag {
    None,
    /// Pressed but not moved past the click threshold yet.
    Pending { start: Pt },
    Creating { start: Pt },
    Moving { grab: Pt, orig: Rect },
    Resizing { handle: Handle, orig: Rect },
    Drawing(Shape),
}

pub struct TextEdit {
    pub pos: Pt,
    pub text: String,
}

pub struct Editor {
    pub screens: Vec<Screen>,
    pub swap_rb: bool,
    pub fonts: Fonts,
    pub sel: Option<Rect>,
    pub shapes: Vec<Shape>,
    redo: Vec<Shape>,
    pub tool: Tool,
    pub color: usize,
    pub width: f32,
    pub pointer: Pt,
    pub pointer_in: bool,
    drag: Drag,
    pub text: Option<TextEdit>,
    pub outcome: Option<Outcome>,
    last_click: Option<(std::time::Instant, Pt)>,
    /// Show the size/width hint pill until the next pointer move.
    pub width_hint: bool,
    /// Bumped whenever committed shapes change; forces a full redraw.
    revision: u64,
    drawn_revision: Option<u64>,
    drawn_sel: Option<Rect>,
    drawn_layers: Vec<ui::Layer>,
    frame: u64,
}

const CLICK_SLOP: f32 = 4.0;
const MIN_SEL: f32 = 4.0;

impl Editor {
    pub fn new(shots: Vec<Shot>, fonts: Fonts, swap_rb: bool, color: usize, width: f32) -> Self {
        let screens = shots
            .into_iter()
            .map(|shot| {
                let scale = shot.scale() as f32;
                Screen { output: shot.output, bg: to_pixmap(shot.image, swap_rb), scale }
            })
            .collect();
        Self {
            screens,
            swap_rb,
            fonts,
            sel: None,
            shapes: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Select,
            color: color.min(PALETTE.len() - 1),
            width: width.clamp(1.0, 40.0),
            pointer: (0.0, 0.0),
            pointer_in: false,
            drag: Drag::None,
            text: None,
            outcome: None,
            last_click: None,
            width_hint: false,
            revision: 0,
            drawn_revision: None,
            drawn_sel: None,
            drawn_layers: Vec::new(),
            frame: 0,
        }
    }

    /// Regions to repaint since the previous call: everything the last frame's
    /// chrome covered plus everything this frame's will.
    pub fn take_damage(&mut self) -> Damage {
        self.frame += 1;
        let layers = ui::dynamic_layers(self, self.frame);
        let full = self.drawn_revision != Some(self.revision) || self.drawn_layers.len() != layers.len();
        let mut out = Vec::new();
        // A layer whose key changed needs both where it was and where it is.
        for (old, new) in self.drawn_layers.iter().zip(&layers) {
            if old.key != new.key {
                out.extend_from_slice(&old.rects);
                out.extend_from_slice(&new.rects);
            }
        }
        self.drawn_layers = layers;
        if self.drawn_sel != self.sel {
            // Only the area that switched between clean and dimmed changed:
            // the symmetric difference of the old and new selections.
            match (self.drawn_sel, self.sel) {
                (Some(a), Some(b)) => {
                    out.extend(rect_sub(a, b));
                    out.extend(rect_sub(b, a));
                }
                (a, b) => out.extend(a.or(b)),
            }
            self.drawn_sel = self.sel;
        }
        self.drawn_revision = Some(self.revision);
        if full { Damage::Full } else { Damage::Rects(out) }
    }

    pub fn set_swap_rb(&mut self, swap: bool) {
        if swap == self.swap_rb {
            return;
        }
        self.swap_rb = swap;
        self.revision += 1;
        for s in &mut self.screens {
            for p in s.bg.data_mut().chunks_exact_mut(4) {
                p.swap(0, 2);
            }
        }
    }

    pub fn current_color(&self) -> Color {
        let (r, g, b) = PALETTE[self.color];
        Color::from_rgba8(r, g, b, 255)
    }

    fn screen_at(&self, p: Pt) -> Option<usize> {
        self.screens.iter().position(|s| s.bounds().contains(p.0, p.1)).or_else(|| (!self.screens.is_empty()).then_some(0))
    }

    pub fn is_selecting(&self) -> bool {
        matches!(self.drag, Drag::Creating { .. } | Drag::Pending { .. }) || self.sel.is_none()
    }

    pub fn is_adjusting(&self) -> bool {
        matches!(self.drag, Drag::Creating { .. } | Drag::Moving { .. } | Drag::Resizing { .. })
    }

    pub fn in_progress(&self) -> Option<&Shape> {
        match &self.drag {
            Drag::Drawing(s) => Some(s),
            _ => None,
        }
    }

    // ---- geometry helpers ------------------------------------------------

    fn handle_at(&self, p: Pt) -> Option<Handle> {
        let r = self.sel?;
        let tol = 8.0;
        let near = |a: f32, b: f32| (a - b).abs() <= tol;
        let inside_x = p.0 >= r.left() - tol && p.0 <= r.right() + tol;
        let inside_y = p.1 >= r.top() - tol && p.1 <= r.bottom() + tol;
        let (l, rt, t, b) = (near(p.0, r.left()), near(p.0, r.right()), near(p.1, r.top()), near(p.1, r.bottom()));
        Some(match () {
            _ if l && t => Handle::NW,
            _ if rt && t => Handle::NE,
            _ if l && b => Handle::SW,
            _ if rt && b => Handle::SE,
            _ if t && inside_x => Handle::N,
            _ if b && inside_x => Handle::S,
            _ if l && inside_y => Handle::W,
            _ if rt && inside_y => Handle::E,
            _ => return None,
        })
    }

    fn clamp_to_desktop(&self, r: Rect) -> Rect {
        let b = self.desktop();
        let w = r.width().min(b.width());
        let h = r.height().min(b.height());
        let x = r.left().clamp(b.left(), b.right() - w);
        let y = r.top().clamp(b.top(), b.bottom() - h);
        Rect::from_xywh(x, y, w, h).unwrap_or(r)
    }

    pub fn desktop(&self) -> Rect {
        let mut it = self.screens.iter().map(|s| s.bounds());
        let first = it.next().unwrap_or(Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap());
        it.fold(first, |a, b| Rect::from_ltrb(a.left().min(b.left()), a.top().min(b.top()), a.right().max(b.right()), a.bottom().max(b.bottom())).unwrap())
    }

    pub fn cursor(&self) -> CursorKind {
        let p = self.pointer;
        if ui::toolbar_hit(self, p).is_some() {
            return CursorKind::Pointer;
        }
        match &self.drag {
            Drag::Moving { .. } => return CursorKind::Move,
            Drag::Resizing { handle, .. } => return CursorKind::Resize(*handle),
            Drag::Creating { .. } | Drag::Pending { .. } | Drag::Drawing(_) => return CursorKind::Crosshair,
            Drag::None => {}
        }
        match self.tool {
            Tool::Select => {
                if let Some(h) = self.handle_at(p) {
                    CursorKind::Resize(h)
                } else if self.sel.is_some_and(|r| r.contains(p.0, p.1)) {
                    CursorKind::Move
                } else {
                    CursorKind::Crosshair
                }
            }
            Tool::Text => CursorKind::Text,
            _ => CursorKind::Crosshair,
        }
    }

    // ---- input -----------------------------------------------------------

    /// Returns true if a redraw is needed.
    pub fn pointer_motion(&mut self, p: Pt, mods: Mods) -> bool {
        self.pointer = p;
        self.pointer_in = true;
        self.width_hint = false;
        match &mut self.drag {
            Drag::None => {}
            Drag::Pending { start } => {
                let s = *start;
                if (p.0 - s.0).abs() > CLICK_SLOP || (p.1 - s.1).abs() > CLICK_SLOP {
                    self.drag = Drag::Creating { start: s };
                    self.sel = norm_rect(s, p);
                }
            }
            Drag::Creating { start } => {
                let s = *start;
                let mut p = p;
                if mods.shift {
                    let d = (p.0 - s.0).abs().max((p.1 - s.1).abs());
                    p = (s.0 + d * (p.0 - s.0).signum(), s.1 + d * (p.1 - s.1).signum());
                }
                self.sel = norm_rect(s, p);
            }
            Drag::Moving { grab, orig } => {
                let r = orig.translate(p.0 - grab.0, p.1 - grab.1).unwrap_or(*orig);
                self.sel = Some(self.clamp_to_desktop(r));
            }
            Drag::Resizing { handle, orig } => {
                let (mut l, mut t, mut r, mut b) = (orig.left(), orig.top(), orig.right(), orig.bottom());
                match handle {
                    Handle::N => t = p.1,
                    Handle::S => b = p.1,
                    Handle::W => l = p.0,
                    Handle::E => r = p.0,
                    Handle::NW => (l, t) = p,
                    Handle::NE => (r, t) = p,
                    Handle::SW => (l, b) = p,
                    Handle::SE => (r, b) = p,
                }
                // Dragging past the opposite edge flips the rectangle.
                self.sel = Rect::from_ltrb(l.min(r), t.min(b), l.max(r).max(l.min(r) + 1.0), t.max(b).max(t.min(b) + 1.0));
            }
            Drag::Drawing(shape) => {
                let snap = |a: Pt, mut b: Pt| {
                    if mods.shift {
                        b = snap45(a, b);
                    }
                    b
                };
                match shape {
                    Shape::Rect { a, b, .. } | Shape::Ellipse { a, b, .. } | Shape::Pixelate { a, b, .. } => {
                        *b = if mods.shift {
                            let d = (p.0 - a.0).abs().max((p.1 - a.1).abs());
                            (a.0 + d * (p.0 - a.0).signum(), a.1 + d * (p.1 - a.1).signum())
                        } else {
                            p
                        }
                    }
                    Shape::Arrow { a, b, .. } | Shape::Line { a, b, .. } => *b = snap(*a, p),
                    Shape::Pen { pts, .. } | Shape::Marker { pts, .. } => {
                        let last = *pts.last().unwrap();
                        if (last.0 - p.0).hypot(last.1 - p.1) >= 1.5 {
                            pts.push(p);
                        }
                    }
                    Shape::Counter { pos, .. } => *pos = p,
                    Shape::Text { .. } => {}
                }
            }
        }
        true
    }

    pub fn pointer_leave(&mut self) -> bool {
        self.pointer_in = false;
        true
    }

    pub fn button_press(&mut self, p: Pt, left: bool, mods: Mods) -> bool {
        self.pointer = p;
        if !left {
            // Right click backs out: in-progress drag, then text, then selection.
            if !matches!(self.drag, Drag::None) {
                self.drag = Drag::None;
            } else if self.text.is_some() {
                self.text = None;
            } else if self.shapes.is_empty() {
                self.sel = None;
                self.tool = Tool::Select;
            }
            return true;
        }
        if let Some(btn) = ui::toolbar_hit(self, p) {
            self.commit_text();
            self.press_button(btn);
            return true;
        }

        let now = std::time::Instant::now();
        let double = self.last_click.is_some_and(|(t, q)| now.duration_since(t).as_millis() < 400 && (q.0 - p.0).hypot(q.1 - p.1) < CLICK_SLOP);
        self.last_click = Some((now, p));
        if double && self.tool == Tool::Select && self.sel.is_some_and(|r| r.contains(p.0, p.1)) {
            self.outcome = Some(Outcome::Copy);
            return true;
        }

        match self.tool {
            Tool::Select => {
                if let (Some(h), Some(r)) = (self.handle_at(p), self.sel) {
                    self.drag = Drag::Resizing { handle: h, orig: r };
                } else if let Some(r) = self.sel.filter(|r| r.contains(p.0, p.1)) {
                    self.drag = Drag::Moving { grab: p, orig: r };
                } else {
                    self.drag = Drag::Pending { start: p };
                }
            }
            Tool::Text => {
                self.commit_text();
                self.text = Some(TextEdit { pos: p, text: String::new() });
            }
            tool => {
                self.commit_text();
                if self.sel.is_none() {
                    // Drawing before selecting implies "this whole screen".
                    if let Some(i) = self.screen_at(p) {
                        self.sel = Some(self.screens[i].bounds());
                    }
                }
                let color = self.current_color();
                let width = self.width;
                let _ = mods;
                let shape = match tool {
                    Tool::Rect => Shape::Rect { a: p, b: p, color, width },
                    Tool::Ellipse => Shape::Ellipse { a: p, b: p, color, width },
                    Tool::Arrow => Shape::Arrow { a: p, b: p, color, width },
                    Tool::Line => Shape::Line { a: p, b: p, color, width },
                    Tool::Pen => Shape::Pen { pts: vec![p], color, width },
                    Tool::Marker => Shape::Marker { pts: vec![p], color, width: width * 4.0 + 8.0 },
                    Tool::Pixelate => Shape::Pixelate { a: p, b: p, block: 6.0 + width * 1.5 },
                    Tool::Counter => {
                        let n = self.shapes.iter().filter(|s| matches!(s, Shape::Counter { .. })).count() as u32 + 1;
                        Shape::Counter { pos: p, n, color, radius: 10.0 + width * 1.5 }
                    }
                    Tool::Select | Tool::Text => unreachable!(),
                };
                self.drag = Drag::Drawing(shape);
            }
        }
        true
    }

    pub fn button_release(&mut self, p: Pt, left: bool) -> bool {
        if !left {
            return false;
        }
        self.pointer = p;
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Pending { .. } => {
                // A click without a drag picks the whole screen under the pointer.
                if let Some(i) = self.screen_at(p) {
                    self.sel = Some(self.screens[i].bounds());
                }
            }
            Drag::Creating { .. } => {
                if self.sel.is_some_and(|r| r.width() < MIN_SEL || r.height() < MIN_SEL) {
                    self.sel = None;
                }
            }
            Drag::Drawing(shape) => {
                let keep = match &shape {
                    Shape::Rect { a, b, .. } | Shape::Ellipse { a, b, .. } | Shape::Pixelate { a, b, .. } | Shape::Arrow { a, b, .. } | Shape::Line { a, b, .. } => {
                        (a.0 - b.0).abs() + (a.1 - b.1).abs() > 2.0
                    }
                    _ => true,
                };
                if keep {
                    self.push_shape(shape);
                }
            }
            Drag::Moving { .. } | Drag::Resizing { .. } | Drag::None => {}
        }
        true
    }

    pub fn scroll(&mut self, dy: f32) -> bool {
        let step = if dy > 0.0 { -1.0 } else { 1.0 };
        self.width = (self.width + step).clamp(1.0, 40.0);
        self.width_hint = true;
        true
    }

    pub fn key(&mut self, key: Key, mods: Mods) -> bool {
        // Text entry swallows most keys.
        if let Some(te) = &mut self.text {
            match key {
                Key::Escape => self.text = None,
                Key::Enter if mods.shift => te.text.push('\n'),
                Key::Enter => self.commit_text(),
                Key::Backspace => {
                    te.text.pop();
                }
                Key::Char(c) if !mods.ctrl => te.text.push(c),
                _ => return false,
            }
            return true;
        }

        if mods.ctrl {
            match key {
                Key::Char('c') | Key::Char('C') => self.outcome = Some(Outcome::Copy),
                Key::Char('s') | Key::Char('S') => self.outcome = Some(Outcome::Save),
                Key::Char('z') if !mods.shift => self.undo(),
                Key::Char('Z') | Key::Char('z') | Key::Char('y') | Key::Char('Y') => self.redo(),
                Key::Char('a') | Key::Char('A') => {
                    let i = self.screen_at(self.pointer).unwrap_or(0);
                    self.sel = self.screens.get(i).map(|s| s.bounds());
                }
                _ => return false,
            }
            return true;
        }

        match key {
            Key::Escape => {
                if !matches!(self.drag, Drag::None) {
                    self.drag = Drag::None;
                } else {
                    self.outcome = Some(Outcome::Cancel);
                }
            }
            Key::Enter => self.outcome = Some(Outcome::Copy),
            Key::Left | Key::Right | Key::Up | Key::Down => {
                let Some(r) = self.sel else { return false };
                let d = if mods.alt { 10.0 } else { 1.0 };
                let (dx, dy) = match key {
                    Key::Left => (-d, 0.0),
                    Key::Right => (d, 0.0),
                    Key::Up => (0.0, -d),
                    _ => (0.0, d),
                };
                self.sel = if mods.shift {
                    Rect::from_xywh(r.left(), r.top(), (r.width() + dx).max(1.0), (r.height() + dy).max(1.0))
                } else {
                    Some(self.clamp_to_desktop(r.translate(dx, dy).unwrap_or(r)))
                };
            }
            Key::Char(c @ '1'..='8') => self.color = c as usize - '1' as usize,
            Key::Char('[') => return self.scroll(1.0),
            Key::Char(']') => return self.scroll(-1.0),
            Key::Char(c) => {
                let c = c.to_ascii_lowercase();
                let Some(t) = std::iter::once(Tool::Select).chain(Tool::DRAWING).find(|t| t.hotkey() == c) else { return false };
                self.tool = t;
            }
            Key::Backspace => self.undo(),
        }
        true
    }

    fn press_button(&mut self, b: ui::Button) {
        match b {
            ui::Button::Tool(t) => self.tool = t,
            ui::Button::Color(i) => self.color = i,
            ui::Button::Undo => self.undo(),
            ui::Button::Redo => self.redo(),
            ui::Button::Copy => self.outcome = Some(Outcome::Copy),
            ui::Button::Save => self.outcome = Some(Outcome::Save),
            ui::Button::Close => self.outcome = Some(Outcome::Cancel),
        }
    }

    fn push_shape(&mut self, s: Shape) {
        self.shapes.push(s);
        self.redo.clear();
        self.revision += 1;
    }

    fn commit_text(&mut self) {
        if let Some(te) = self.text.take() {
            if !te.text.trim().is_empty() {
                let size = self.text_size();
                self.push_shape(Shape::Text { pos: te.pos, text: te.text, color: self.current_color(), size });
            }
        }
    }

    pub fn text_size(&self) -> f32 {
        12.0 + self.width * 3.0
    }

    fn undo(&mut self) {
        if let Some(s) = self.shapes.pop() {
            self.redo.push(s);
            self.revision += 1;
        }
    }

    fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.shapes.push(s);
            self.revision += 1;
        }
    }

    /// Selection to export; falls back to the screen under the pointer.
    pub fn final_rect(&self) -> Option<Rect> {
        self.sel.or_else(|| self.screen_at(self.pointer).map(|i| self.screens[i].bounds()))
    }

    /// Render the selection with annotations at native resolution.
    pub fn export(&mut self) -> Option<Image> {
        self.commit_text();
        let rect = self.final_rect()?;
        let center = (rect.left() + rect.width() / 2.0, rect.top() + rect.height() / 2.0);
        let scale = self.screen_at(center).map_or(1.0, |i| self.screens[i].scale);
        let w = (rect.width() * scale).round().max(1.0) as u32;
        let h = (rect.height() * scale).round().max(1.0) as u32;
        let mut pm = Pixmap::new(w, h)?;
        for s in &self.screens {
            let b = s.bounds();
            if b.intersect(&rect).is_none() {
                continue;
            }
            let k = scale / s.scale;
            let quality = if (k - 1.0).abs() < 1e-3 { FilterQuality::Nearest } else { FilterQuality::Bicubic };
            let paint = PixmapPaint { quality, ..Default::default() };
            let tx = (b.left() - rect.left()) * scale;
            let ty = (b.top() - rect.top()) * scale;
            pm.draw_pixmap(0, 0, s.bg.as_ref(), &paint, Transform::from_row(k, 0.0, 0.0, k, tx.round(), ty.round()), None);
        }
        let clean = pm.clone();
        let view = View { ox: rect.left(), oy: rect.top(), scale, swap_rb: self.swap_rb, off: (0.0, 0.0) };
        for shape in &self.shapes {
            shape.draw(&mut pm.as_mut(), &view, Some((clean.as_ref(), (0, 0))), &self.fonts);
        }
        let mut data = pm.take();
        if self.swap_rb {
            for p in data.chunks_exact_mut(4) {
                p.swap(0, 2);
            }
        }
        // Captures are opaque, so premultiplied == straight here.
        Some(Image { width: w, height: h, data })
    }
}

/// `a` minus `b` as up to four rectangles.
fn rect_sub(a: Rect, b: Rect) -> Vec<Rect> {
    let Some(i) = a.intersect(&b) else { return vec![a] };
    [
        Rect::from_ltrb(a.left(), a.top(), a.right(), i.top()),       // above
        Rect::from_ltrb(a.left(), i.bottom(), a.right(), a.bottom()), // below
        Rect::from_ltrb(a.left(), i.top(), i.left(), i.bottom()),     // left
        Rect::from_ltrb(i.right(), i.top(), a.right(), i.bottom()),   // right
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn snap45(a: Pt, b: Pt) -> Pt {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy);
    let ang = (dy.atan2(dx) / std::f32::consts::FRAC_PI_4).round() * std::f32::consts::FRAC_PI_4;
    (a.0 + len * ang.cos(), a.1 + len * ang.sin())
}

fn to_pixmap(img: Image, swap_rb: bool) -> Pixmap {
    let mut data = img.data;
    for p in data.chunks_exact_mut(4) {
        if swap_rb {
            p.swap(0, 2);
        }
        p[3] = 255;
    }
    Pixmap::from_vec(data, tiny_skia::IntSize::from_wh(img.width, img.height).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Output;

    fn shot(name: &str, x: i32, w: u32, h: u32, scale: u32, rgb: [u8; 3]) -> Shot {
        let mut image = Image::new(w * scale, h * scale);
        for p in image.data.chunks_exact_mut(4) {
            p.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        Shot { output: Output { index: 0, name: name.into(), description: String::new(), x, y: 0, width: w, height: h }, image }
    }

    fn editor(shots: Vec<Shot>, swap: bool) -> Editor {
        Editor::new(shots, Fonts::load(None).expect("a system font"), swap, 0, 4.0)
    }

    fn drag(ed: &mut Editor, a: Pt, b: Pt) {
        let m = Mods::default();
        ed.button_press(a, true, m);
        ed.pointer_motion(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0), m);
        ed.pointer_motion(b, m);
        ed.button_release(b, true);
    }

    #[test]
    fn drag_selects_and_click_selects_screen() {
        let mut ed = editor(vec![shot("A", 0, 200, 100, 1, [10, 20, 30])], false);
        drag(&mut ed, (150.0, 80.0), (20.0, 10.0));
        let r = ed.sel.unwrap();
        assert_eq!((r.left(), r.top(), r.width(), r.height()), (20.0, 10.0, 130.0, 70.0));

        let mut ed = editor(vec![shot("A", 0, 200, 100, 1, [0, 0, 0])], false);
        ed.button_press((50.0, 50.0), true, Mods::default());
        ed.button_release((51.0, 50.0), true);
        assert_eq!(ed.sel.unwrap().width(), 200.0);
    }

    #[test]
    fn move_and_resize_selection() {
        let mut ed = editor(vec![shot("A", 0, 400, 300, 1, [0, 0, 0])], false);
        drag(&mut ed, (100.0, 100.0), (200.0, 200.0));
        drag(&mut ed, (150.0, 150.0), (170.0, 160.0)); // move
        let r = ed.sel.unwrap();
        assert_eq!((r.left(), r.top()), (120.0, 110.0));
        drag(&mut ed, (220.0, 210.0), (300.0, 250.0)); // SE handle
        let r = ed.sel.unwrap();
        assert_eq!((r.right(), r.bottom()), (300.0, 250.0));
    }

    #[test]
    fn export_is_native_resolution_with_annotations() {
        // 2x HiDPI: a 100x50 logical selection exports as 200x100 pixels.
        for swap in [false, true] {
            let mut ed = editor(vec![shot("A", 0, 300, 200, 2, [200, 10, 10])], swap);
            drag(&mut ed, (10.0, 10.0), (110.0, 60.0));
            ed.tool = Tool::Rect;
            ed.color = 4; // blue
            drag(&mut ed, (20.0, 20.0), (100.0, 50.0));
            let img = ed.export().unwrap();
            assert_eq!((img.width, img.height), (200, 100));
            // background keeps its colour in RGBA order regardless of surface order
            assert_eq!(&img.data[0..3], &[200, 10, 10]);
            // the blue rectangle's left edge, at logical x=20 → pixel 20
            let i = ((40 * 200) + 20) * 4;
            let p = &img.data[i..i + 3];
            assert!(p[2] > 150 && p[0] < 150, "expected blue stroke, got {p:?}");
        }
    }

    #[test]
    fn export_spans_two_outputs() {
        let mut ed = editor(vec![shot("L", 0, 100, 100, 1, [255, 0, 0]), shot("R", 100, 100, 100, 1, [0, 255, 0])], false);
        drag(&mut ed, (50.0, 10.0), (150.0, 60.0));
        let img = ed.export().unwrap();
        assert_eq!((img.width, img.height), (100, 50));
        assert_eq!(&img.data[0..3], &[255, 0, 0]);
        let last = (99) * 4;
        assert_eq!(&img.data[last..last + 3], &[0, 255, 0]);
    }

    #[test]
    fn undo_redo_and_text() {
        let mut ed = editor(vec![shot("A", 0, 400, 300, 1, [0, 0, 0])], false);
        drag(&mut ed, (0.0, 0.0), (300.0, 200.0));
        ed.tool = Tool::Arrow;
        drag(&mut ed, (10.0, 10.0), (90.0, 90.0));
        ed.tool = Tool::Text;
        ed.button_press((50.0, 50.0), true, Mods::default());
        for c in "hi".chars() {
            ed.key(Key::Char(c), Mods::default());
        }
        ed.key(Key::Enter, Mods::default());
        assert_eq!(ed.shapes.len(), 2);
        assert!(ed.outcome.is_none(), "Enter in a text box commits, not copies");
        let ctrl = Mods { ctrl: true, ..Default::default() };
        ed.key(Key::Char('z'), ctrl);
        assert_eq!(ed.shapes.len(), 1);
        ed.key(Key::Char('Z'), Mods { shift: true, ..ctrl });
        assert_eq!(ed.shapes.len(), 2);
        ed.key(Key::Enter, Mods::default());
        assert_eq!(ed.outcome, Some(Outcome::Copy));
    }

    /// Replays a session the way the overlay does (one persistent buffer,
    /// repainting only damaged rects) and checks the result matches a full
    /// redraw: any region the damage tracking misses shows up as a diff.
    #[test]
    fn partial_repaints_match_full_render() {
        let mut img = Image::new(640, 400);
        for (i, p) in img.data.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % 640, i / 640);
            p.copy_from_slice(&[(x * 7 % 256) as u8, (y * 5 % 256) as u8, ((x ^ y) % 256) as u8, 255]);
        }
        let out = Output { index: 0, name: "A".into(), description: String::new(), x: 0, y: 0, width: 640, height: 400 };
        let mut ed = editor(vec![Shot { output: out, image: img }], true);
        let mut buf = Pixmap::new(640, 400).unwrap();
        let m = Mods::default();

        let mut n = 0;
        let mut step = |ed: &mut Editor, f: &dyn Fn(&mut Editor)| {
            n += 1;
            f(ed);
            match ed.take_damage() {
                Damage::Full => ui::render(ed, 0, &mut buf.as_mut(), (0, 0)),
                Damage::Rects(rs) => {
                    for r in rs {
                        let x0 = (r.left().floor() as i32).clamp(0, 640);
                        let y0 = (r.top().floor() as i32).clamp(0, 400);
                        let x1 = (r.right().ceil() as i32).clamp(0, 640);
                        let y1 = (r.bottom().ceil() as i32).clamp(0, 400);
                        if x1 <= x0 || y1 <= y0 {
                            continue;
                        }
                        let mut tmp = Pixmap::new((x1 - x0) as u32, (y1 - y0) as u32).unwrap();
                        ui::render(ed, 0, &mut tmp.as_mut(), (x0, y0));
                        let row = (x1 - x0) as usize * 4;
                        for (y, src) in tmp.data().chunks_exact(row).enumerate() {
                            let off = ((y0 as usize + y) * 640 + x0 as usize) * 4;
                            buf.data_mut()[off..off + row].copy_from_slice(src);
                        }
                    }
                }
            }
            let mut full = Pixmap::new(640, 400).unwrap();
            ui::render(ed, 0, &mut full.as_mut(), (0, 0));
            // tiny-skia's edge antialiasing varies a little with the target size,
            // so allow small AA wobble; a missed region shows stale dim/clean
            // content or missing chrome, which is far larger.
            let diffs: Vec<i16> = buf.data().iter().zip(full.data()).map(|(a, b)| (*a as i16 - *b as i16).abs()).filter(|d| *d > 2).collect();
            let max = diffs.iter().copied().max().unwrap_or(0);
            assert!(max <= 48 && diffs.len() < 400, "step {n}: {} differing values, max {max}: damage tracking missed something", diffs.len());
        };

        step(&mut ed, &|e| { e.pointer_motion((100.0, 100.0), m); });
        for i in 0..6 {
            step(&mut ed, &|e| { e.pointer_motion((120.0 + i as f32 * 30.0, 90.0 + i as f32 * 20.0), m); });
        }
        step(&mut ed, &|e| { e.button_press((80.0, 60.0), true, m); });
        for i in 1..=8 {
            step(&mut ed, &|e| { e.pointer_motion((80.0 + i as f32 * 50.0, 60.0 + i as f32 * 30.0), m); });
        }
        step(&mut ed, &|e| { e.button_release((480.0, 300.0), true); });
        step(&mut ed, &|e| { e.pointer_motion((300.0, 200.0), m); });
        step(&mut ed, &|e| { e.button_press((300.0, 200.0), true, m); });
        step(&mut ed, &|e| { e.pointer_motion((250.0, 150.0), m); }); // move selection
        step(&mut ed, &|e| { e.button_release((250.0, 150.0), true); });
        // big selection jumps: select-all, clear, select-all again
        let ctrl = Mods { ctrl: true, ..Default::default() };
        step(&mut ed, &|e| { e.key(Key::Char('a'), ctrl); });
        step(&mut ed, &|e| { e.button_press((300.0, 200.0), false, m); });
        step(&mut ed, &|e| { e.pointer_motion((310.0, 210.0), m); });
        step(&mut ed, &|e| { e.button_press((20.0, 20.0), true, m); });
        step(&mut ed, &|e| { e.pointer_motion((200.0, 140.0), m); });
        step(&mut ed, &|e| { e.button_release((200.0, 140.0), true); });
        step(&mut ed, &|e| { e.key(Key::Char('a'), ctrl); });
        step(&mut ed, &|e| { e.button_press((300.0, 200.0), false, m); });
        step(&mut ed, &|e| { e.button_press((40.0, 40.0), true, m); });
        step(&mut ed, &|e| { e.pointer_motion((500.0, 330.0), m); });
        step(&mut ed, &|e| { e.button_release((500.0, 330.0), true); });
        step(&mut ed, &|e| { e.tool = Tool::Pen; });
        step(&mut ed, &|e| { e.button_press((100.0, 100.0), true, m); });
        for i in 1..10 {
            step(&mut ed, &|e| { e.pointer_motion((100.0 + i as f32 * 20.0, 100.0 + (i % 3) as f32 * 25.0), m); });
        }
        step(&mut ed, &|e| { e.button_release((280.0, 125.0), true); });
        step(&mut ed, &|e| { e.tool = Tool::Pixelate; });
        step(&mut ed, &|e| { e.button_press((150.0, 150.0), true, m); });
        step(&mut ed, &|e| { e.pointer_motion((260.0, 230.0), m); });
        step(&mut ed, &|e| { e.button_release((260.0, 230.0), true); });
        step(&mut ed, &|e| { e.tool = Tool::Text; });
        step(&mut ed, &|e| { e.button_press((120.0, 240.0), true, m); });
        for c in "abc".chars() {
            step(&mut ed, &|e| { e.key(Key::Char(c), m); });
        }
        step(&mut ed, &|e| { e.key(Key::Backspace, m); });
        step(&mut ed, &|e| { e.scroll(-1.0); });
        // hover the toolbar to get a tooltip, then leave
        let (_, btns) = ui::toolbar_layout(&ed).unwrap();
        let (r, _) = btns[3];
        step(&mut ed, &|e| { e.pointer_motion((r.left() + 5.0, r.top() + 5.0), m); });
        step(&mut ed, &|e| { e.pointer_motion((50.0, 50.0), m); });

    }

    /// `cargo test --release bench_4k -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_4k() {
        let img = Image::new(3840, 2160);
        let out = Output { index: 0, name: "A".into(), description: String::new(), x: 0, y: 0, width: 3840, height: 2160 };
        let t = std::time::Instant::now();
        let mut ed = editor(vec![Shot { output: out, image: img }], true);
        eprintln!("editor new: {:?}", t.elapsed());
        let mut buf = Pixmap::new(3840, 2160).unwrap();
        let m = Mods::default();
        let t = std::time::Instant::now();
        ui::render(&ed, 0, &mut buf.as_mut(), (0, 0));
        eprintln!("full frame: {:?}", t.elapsed());
        let _ = ed.take_damage();
        let run = |name: &str, ed: &mut Editor, f: &dyn Fn(&mut Editor, usize)| {
            let t = std::time::Instant::now();
            let mut px = 0;
            for i in 0..50 {
                f(ed, i);
                if let Damage::Rects(rs) = ed.take_damage() {
                    for r in rs {
                        let (x0, y0) = (r.left().max(0.0) as i32, r.top().max(0.0) as i32);
                        let (x1, y1) = (r.right().min(3840.0) as i32, r.bottom().min(2160.0) as i32);
                        if x1 <= x0 || y1 <= y0 { continue; }
                        let mut tmp = Pixmap::new((x1 - x0) as u32, (y1 - y0) as u32).unwrap();
                        ui::render(ed, 0, &mut tmp.as_mut(), (x0, y0));
                        px += (x1 - x0) * (y1 - y0);
                    }
                }
            }
            eprintln!("{name}: {:.2} ms/frame, {:.1}% of screen", t.elapsed().as_secs_f64() * 1e3 / 50.0, px as f64 / 50.0 / (3840.0 * 2160.0) * 100.0);
        };
        run("hover (crosshair+loupe)", &mut ed, &|e, i| { e.pointer_motion((1000.0 + i as f32 * 7.0, 800.0 + i as f32 * 3.0), m); });
        ed.button_press((500.0, 400.0), true, m);
        run("drag selection", &mut ed, &|e, i| { e.pointer_motion((900.0 + i as f32 * 20.0, 700.0 + i as f32 * 12.0), m); });
        ed.button_release((1900.0, 1300.0), true);
        ed.tool = Tool::Pen;
        ed.button_press((800.0, 800.0), true, m);
        run("pen stroke", &mut ed, &|e, i| { e.pointer_motion((800.0 + i as f32 * 15.0, 800.0 + (i % 5) as f32 * 10.0), m); });
        ed.button_release((1500.0, 800.0), true);
        run("hover toolbar area", &mut ed, &|e, i| { e.pointer_motion((1100.0 + i as f32 * 4.0, 1340.0), m); });
    }
}
