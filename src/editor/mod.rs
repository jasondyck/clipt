//! The capture editor: selection, annotation and the toolbar. Knows nothing
//! about Wayland; the overlay feeds it input in logical desktop coordinates and
//! asks it to paint each output.

pub mod damage;
pub mod keys;
pub mod shapes;
pub mod ui;

use crate::capture::{Output, Shot};
use crate::image::Image;
use crate::text::Fonts;
use shapes::{Grip, Pt, RectExt, Shape, Tool, View, norm_rect};
use tiny_skia::{Color, FilterQuality, Pixmap, PixmapPaint, Rect, Transform};

/// The colours 1-8 start as; `palette` in the config overrides them, and
/// the colour picker changes them.
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

/// The colour picker, open on palette slot `slot`: hue in degrees,
/// saturation and value 0-1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Picker {
    pub slot: usize,
    pub h: f32,
    pub s: f32,
    pub v: f32,
    /// The colour before picking, for Esc.
    orig: (u8, u8, u8),
    drag: Option<ui::PickerPart>,
}

/// HSV (hue in degrees, saturation and value 0-1) to 8-bit RGB.
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to(r), to(g), to(b))
}

pub fn rgb_to_hsv((r, g, b): (u8, u8, u8)) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if max == 0.0 { 0.0 } else { d / max }, max)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Copy,
    Save,
    Cancel,
    /// Record mode: start recording the selection.
    Record,
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
    Tab,
    Delete,
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
    Pending {
        start: Pt,
    },
    Creating {
        start: Pt,
    },
    Moving {
        grab: Pt,
        orig: Rect,
    },
    Resizing {
        handle: Handle,
        orig: Rect,
    },
    Drawing(Shape),
    /// Moving or reshaping committed shape `idx`, which is out of `shapes`
    /// meanwhile and drawn like a shape being drawn. `grip` is the handle
    /// held, or `None` to move the whole shape.
    Editing {
        idx: usize,
        shape: Shape,
        orig: Shape,
        grab: Pt,
        grip: Option<Grip>,
        /// Past the click threshold: from here every motion moves it.
        moved: bool,
        /// Moved or scrolled, so undo has a step to go back to.
        changed: bool,
        /// A width scrolled to during the drag. `shape` is rebuilt from
        /// `orig` on every motion, and `orig` must stay as it was for Esc.
        width: Option<f32>,
    },
}

/// The first change to shape `idx` while it's out of `shapes` being edited:
/// save the shapes as they were, with it back in place, for undo.
fn edit_checkpoint(changed: &mut bool, shapes: &[Shape], idx: usize, orig: &Shape, undo: &mut Vec<Vec<Shape>>, redo: &mut Vec<Vec<Shape>>) {
    if !*changed {
        *changed = true;
        let mut before = shapes.to_vec();
        before.insert(idx, orig.clone());
        undo.push(before);
        redo.clear();
    }
}

/// What a click would select right now, shown before anything is selected.
#[derive(Clone, Debug, PartialEq)]
pub struct Hover {
    pub rect: Rect,
    /// "firefox" or "HDMI-A-1".
    pub name: String,
    pub is_window: bool,
}

pub struct TextEdit {
    pub pos: Pt,
    pub text: String,
    pub color: Color,
    pub size: f32,
    /// Editing committed text: where it was in `shapes`, and how it was, for
    /// Esc.
    orig: Option<(usize, Shape)>,
}

/// Record mode's sound toggles: the computer's sound and the microphone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SoundToggles {
    pub system: bool,
    pub mic: bool,
}

pub struct Editor {
    pub screens: Vec<Screen>,
    pub swap_rb: bool,
    pub fonts: Fonts,
    pub sel: Option<Rect>,
    pub shapes: Vec<Shape>,
    /// `shapes` as it was before each change, and after each undone one.
    undo: Vec<Vec<Shape>>,
    redo: Vec<Vec<Shape>>,
    /// The shape the select tool picked, which moves, restyles and deletes.
    pub selected: Option<usize>,
    pub tool: Tool,
    pub color: usize,
    /// Colours 1-8.
    pub palette: [(u8, u8, u8); 8],
    pub picker: Option<Picker>,
    /// The picker changed a colour: the host saves the palette.
    pub palette_changed: bool,
    pub width: f32,
    pub pointer: Pt,
    pub pointer_in: bool,
    drag: Drag,
    pub text: Option<TextEdit>,
    /// Visible windows, topmost first (see `crate::windows`).
    pub windows: Vec<crate::windows::Window>,
    /// Tab flips hover from the window under the pointer to its monitor.
    pub hover_screen: bool,
    /// The highlight eases between targets: where it started, where it's
    /// going, when, and where it is on the frame being drawn.
    hover_from: Option<Rect>,
    hover_to: Option<Rect>,
    hover_t0: std::time::Instant,
    hover_shown: Option<Rect>,
    pub outcome: Option<Outcome>,
    /// Picking a region to record: no annotation tools, and Enter records.
    pub record: bool,
    /// Record mode: which sound to record with it, toggled in the toolbar.
    pub sound: SoundToggles,
    /// Record mode: save a GIF rather than an MP4 (G, or its button).
    pub gif: bool,
    /// The screenshot overlay may switch to record mode and back (Ctrl+R or
    /// the toolbar). `clipt record`'s picker can't: it only records.
    pub can_switch: bool,
    /// Record mode has sound toggles. Off where recording has no sound yet.
    pub can_sound: bool,
    last_click: Option<(std::time::Instant, Pt)>,
    /// Show the size/width hint pill until the next pointer move.
    pub width_hint: bool,
    /// The `?` sheet of shortcuts, open on this screen.
    pub help: Option<usize>,
    /// The last `?`, to tell a held key's repeats from a second press.
    help_key_at: Option<std::time::Instant>,
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
            undo: Vec::new(),
            redo: Vec::new(),
            selected: None,
            tool: Tool::Select,
            color: color.min(PALETTE.len() - 1),
            palette: PALETTE,
            picker: None,
            palette_changed: false,
            width: width.clamp(1.0, 40.0),
            pointer: (0.0, 0.0),
            pointer_in: false,
            drag: Drag::None,
            text: None,
            windows: Vec::new(),
            hover_screen: false,
            hover_from: None,
            hover_to: None,
            hover_t0: std::time::Instant::now(),
            hover_shown: None,
            outcome: None,
            record: false,
            sound: SoundToggles::default(),
            gif: false,
            can_switch: false,
            can_sound: true,
            last_click: None,
            width_hint: false,
            help: None,
            help_key_at: None,
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
        self.advance_hover();
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
        let lit = self.lit();
        if self.drawn_sel != lit {
            // Only the area that switched between clean and dimmed changed:
            // the symmetric difference of the old and new selections.
            match (self.drawn_sel, lit) {
                (Some(a), Some(b)) => {
                    out.extend(rect_sub(a, b));
                    out.extend(rect_sub(b, a));
                }
                (a, b) => out.extend(a.or(b)),
            }
            self.drawn_sel = lit;
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
            for p in s.bg.data_mut().as_chunks_mut::<4>().0 {
                p.swap(0, 2);
            }
        }
    }

    pub fn current_color(&self) -> Color {
        let (r, g, b) = self.palette[self.color];
        Color::from_rgba8(r, g, b, 255)
    }

    fn screen_at(&self, p: Pt) -> Option<usize> {
        self.screens.iter().position(|s| s.bounds().contains(p.0, p.1)).or_else(|| (!self.screens.is_empty()).then_some(0))
    }

    pub fn is_selecting(&self) -> bool {
        matches!(self.drag, Drag::Creating { .. } | Drag::Pending { .. }) || self.sel.is_none()
    }

    /// The window or monitor a click would select, while nothing is selected
    /// yet and the select tool is idle.
    pub fn hover(&self) -> Option<Hover> {
        if self.sel.is_some() || !self.pointer_in || self.text.is_some() || self.tool != Tool::Select {
            return None;
        }
        if !matches!(self.drag, Drag::None | Drag::Pending { .. }) {
            return None;
        }
        let p = self.pointer;
        let scr = self.screens.iter().find(|s| s.bounds().contains(p.0, p.1))?;
        if !self.hover_screen {
            let desk = self.desktop();
            let hit = self.windows.iter().find(|w| Rect::from_xywh(w.x, w.y, w.width, w.height).is_some_and(|r| r.contains(p.0, p.1)));
            if let Some(w) = hit
                && let Some(r) = Rect::from_xywh(w.x, w.y, w.width, w.height).and_then(|r| r.intersect(&desk))
            {
                return Some(Hover { rect: r, name: w.name().to_string(), is_window: true });
            }
        }
        Some(Hover { rect: scr.bounds(), name: scr.output.name.clone(), is_window: false })
    }

    /// The area drawn undimmed: the selection, or else the hover highlight.
    pub fn lit(&self) -> Option<Rect> {
        self.sel.or(self.hover_shown)
    }

    /// The hover target as it should be drawn this frame (mid-animation rect).
    pub fn hover_drawn(&self) -> Option<Hover> {
        let mut h = self.hover()?;
        h.rect = self.hover_shown.unwrap_or(h.rect);
        Some(h)
    }

    /// True while the highlight is still moving; the overlay keeps drawing.
    pub fn animating(&self) -> bool {
        self.hover_shown != self.hover_to
    }

    const HOVER_ANIM: std::time::Duration = std::time::Duration::from_millis(140);

    fn advance_hover(&mut self) {
        let target = self.hover().map(|h| h.rect);
        let now = std::time::Instant::now();
        if target != self.hover_to {
            // Appearing or disappearing snaps; moving between targets glides.
            self.hover_from = if target.is_some() { self.hover_shown } else { None };
            self.hover_to = target;
            self.hover_t0 = now;
        }
        self.hover_shown = match (self.hover_from, self.hover_to) {
            (Some(a), Some(b)) => {
                let t = (now.duration_since(self.hover_t0).as_secs_f32() / Self::HOVER_ANIM.as_secs_f32()).min(1.0);
                if t >= 1.0 {
                    Some(b)
                } else {
                    let e = 1.0 - (1.0 - t).powi(3);
                    // Whole pixels: a fractional edge antialiases differently
                    // in a partial repaint than in a full one, leaving seams.
                    let l = |x: f32, y: f32| (x + (y - x) * e).round();
                    Rect::from_ltrb(l(a.left(), b.left()), l(a.top(), b.top()), l(a.right(), b.right()), l(a.bottom(), b.bottom()))
                        .or(Some(b))
                }
            }
            (_, b) => b,
        };
    }

    #[cfg(test)]
    fn drag_is_none(&self) -> bool {
        matches!(self.drag, Drag::None)
    }

    pub fn is_adjusting(&self) -> bool {
        matches!(self.drag, Drag::Creating { .. } | Drag::Moving { .. } | Drag::Resizing { .. })
    }

    /// Where a click would start text, shown as a faint cursor at the text's
    /// real size before clicking.
    pub fn text_ghost(&self) -> Option<Pt> {
        let p = self.pointer;
        (self.tool == Tool::Text
            && self.pointer_in
            && !self.record
            && matches!(self.drag, Drag::None)
            && self.picker.is_none()
            && self.shape_at(p).is_none_or(|i| !matches!(self.shapes[i], Shape::Text { .. }))
            && ui::toolbar_hit(self, p).is_none()
            && ui::mode_switch_hit(self, p).is_none())
        .then_some(p)
    }

    pub fn in_progress(&self) -> Option<&Shape> {
        match &self.drag {
            Drag::Drawing(s) | Drag::Editing { shape: s, .. } => Some(s),
            _ => None,
        }
    }

    /// The shape the select tool has picked, where it is now.
    pub fn selected_shape(&self) -> Option<&Shape> {
        if self.tool != Tool::Select || self.record {
            return None;
        }
        match &self.drag {
            Drag::Editing { shape, .. } => Some(shape),
            _ => self.shapes.get(self.selected?),
        }
    }

    /// The topmost shape under `p`.
    fn shape_at(&self, p: Pt) -> Option<usize> {
        if self.record {
            return None;
        }
        self.shapes.iter().rposition(|s| s.hit(p, &self.fonts))
    }

    /// A handle of the selected shape under `p`.
    fn grip_at(&self, p: Pt) -> Option<Grip> {
        let s = self.shapes.get(self.selected?)?;
        s.grips().into_iter().find(|(_, q)| (q.0 - p.0).abs() <= 7.0 && (q.1 - p.1).abs() <= 7.0).map(|(g, _)| g)
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
        it.fold(first, |a, b| {
            Rect::from_ltrb(a.left().min(b.left()), a.top().min(b.top()), a.right().max(b.right()), a.bottom().max(b.bottom())).unwrap()
        })
    }

    pub fn cursor(&self) -> CursorKind {
        let p = self.pointer;
        if ui::toolbar_hit(self, p).is_some() || ui::mode_switch_hit(self, p).is_some() || ui::picker_hit(self, p).is_some() {
            return CursorKind::Pointer;
        }
        match &self.drag {
            Drag::Moving { .. } => return CursorKind::Move,
            Drag::Resizing { handle, .. } => return CursorKind::Resize(*handle),
            Drag::Creating { .. } | Drag::Pending { .. } | Drag::Drawing(_) => return CursorKind::Crosshair,
            Drag::Editing { grip: None, .. } => return CursorKind::Move,
            Drag::Editing { grip: Some(_), .. } => return CursorKind::Crosshair,
            Drag::None => {}
        }
        match self.tool {
            Tool::Select => {
                if self.grip_at(p).is_some() {
                    CursorKind::Crosshair
                } else if self.shape_at(p).is_some() {
                    CursorKind::Move
                } else if let Some(h) = self.handle_at(p) {
                    CursorKind::Resize(h)
                } else if self.sel.is_some_and(|r| r.contains(p.0, p.1)) {
                    CursorKind::Move
                } else {
                    CursorKind::Crosshair
                }
            }
            Tool::Text if self.shape_at(p).is_some_and(|i| matches!(self.shapes[i], Shape::Text { .. })) => CursorKind::Pointer,
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
        if let Some(part) = self.picker.and_then(|pk| pk.drag) {
            self.pick(part, p);
            return true;
        }
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
            Drag::Editing { idx, shape, orig, grab, grip, moved, changed, width } => {
                if !*moved && (p.0 - grab.0).abs() <= CLICK_SLOP && (p.1 - grab.1).abs() <= CLICK_SLOP {
                    return false;
                }
                *moved = true;
                edit_checkpoint(changed, &self.shapes, *idx, orig, &mut self.undo, &mut self.redo);
                *shape = orig.clone();
                if let Some(w) = width {
                    shape.set_width(*w);
                }
                match grip {
                    Some(g) => shape.drag_grip(*g, p),
                    None => shape.translate(p.0 - grab.0, p.1 - grab.1),
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
        // A click closes the shortcut sheet, and does nothing else.
        if self.help.take().is_some() {
            return true;
        }
        if self.picker.is_some() {
            if let Some(part) = ui::picker_hit(self, p).filter(|_| left) {
                self.pick(part, p);
                return true;
            }
            // A click anywhere else closes it, and does what it would do
            // only on the toolbar.
            self.close_picker(true);
            if !left || ui::toolbar_hit(self, p).is_none() {
                return true;
            }
        }
        if !left {
            // Right-clicking a colour changes it.
            if let Some(ui::Button::Color(i)) = ui::toolbar_hit(self, p) {
                self.open_picker(i);
                return true;
            }
            // Right click backs out: in-progress drag, then text, then the
            // picked shape, then the selection (in record mode, whose
            // shapes aren't shown, even when there are some).
            if !matches!(self.drag, Drag::None) {
                self.cancel_drag();
            } else if self.text.is_some() {
                self.cancel_text();
            } else if self.selected.take().is_some() {
                self.revision += 1;
            } else if self.shapes.is_empty() || self.record {
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
        if let Some(record) = ui::mode_switch_hit(self, p) {
            if record != self.record {
                self.commit_text();
                self.switch_mode();
            }
            return true;
        }

        let now = std::time::Instant::now();
        let double =
            self.last_click.is_some_and(|(t, q)| now.duration_since(t).as_millis() < 400 && (q.0 - p.0).hypot(q.1 - p.1) < CLICK_SLOP);
        self.last_click = Some((now, p));
        let on_shape = self.shape_at(p);
        if double && self.tool == Tool::Select {
            match on_shape {
                Some(i) if matches!(self.shapes[i], Shape::Text { .. }) => {
                    self.edit_text(i);
                    return true;
                }
                Some(_) => {}
                None if self.sel.is_some_and(|r| r.contains(p.0, p.1)) => {
                    self.outcome = Some(self.accept());
                    return true;
                }
                None => {}
            }
        }

        match self.tool {
            Tool::Select => {
                if let Some(g) = self.grip_at(p) {
                    self.start_edit(self.selected.unwrap(), p, Some(g));
                } else if let Some(i) = on_shape {
                    self.start_edit(i, p, None);
                } else if let (Some(h), Some(r)) = (self.handle_at(p), self.sel) {
                    self.drag = Drag::Resizing { handle: h, orig: r };
                } else if let Some(r) = self.sel.filter(|r| r.contains(p.0, p.1)) {
                    self.drag = Drag::Moving { grab: p, orig: r };
                } else {
                    self.drag = Drag::Pending { start: p };
                }
                if self.selected.is_some() && !matches!(self.drag, Drag::Editing { .. }) {
                    self.selected = None;
                    self.revision += 1;
                }
            }
            Tool::Text => {
                self.commit_text();
                // Clicking text edits it.
                if let Some(i) = self.shape_at(p).filter(|&i| matches!(self.shapes[i], Shape::Text { .. })) {
                    self.edit_text(i);
                    return true;
                }
                // The I-beam's middle is where people aim: centre the first
                // line on the click, not hang the text below it.
                let size = self.text_size();
                let lh = crate::text::line_height(self.fonts.bold(), size);
                let (pos, color) = ((p.0, p.1 - lh / 2.0), self.current_color());
                self.text = Some(TextEdit { pos, text: String::new(), color, size, orig: None });
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
                    Tool::Marker => Shape::Marker { pts: vec![p], color, width: shapes::marker_width(width) },
                    Tool::Pixelate => Shape::Pixelate { a: p, b: p, block: shapes::pixel_block(width) },
                    Tool::Counter => {
                        let n = self.shapes.iter().filter(|s| matches!(s, Shape::Counter { .. })).count() as u32 + 1;
                        Shape::Counter { pos: p, n, color, radius: shapes::counter_radius(width) }
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
        if let Some(pk) = &mut self.picker
            && pk.drag.take().is_some()
        {
            return true;
        }
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Pending { .. } => {
                // A click without a drag picks what's highlighted: the window
                // under the pointer, or its whole screen.
                self.pointer = p;
                if let Some(h) = self.hover() {
                    self.sel = Some(h.rect);
                } else if let Some(i) = self.screen_at(p) {
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
                    Shape::Rect { a, b, .. }
                    | Shape::Ellipse { a, b, .. }
                    | Shape::Pixelate { a, b, .. }
                    | Shape::Arrow { a, b, .. }
                    | Shape::Line { a, b, .. } => (a.0 - b.0).abs() + (a.1 - b.1).abs() > 2.0,
                    _ => true,
                };
                if keep {
                    self.push_shape(shape);
                }
            }
            Drag::Editing { idx, shape, .. } => {
                self.shapes.insert(idx, shape);
                self.revision += 1;
            }
            Drag::Moving { .. } | Drag::Resizing { .. } | Drag::None => {}
        }
        true
    }

    pub fn scroll(&mut self, dy: f32) -> bool {
        if self.help.take().is_some() {
            return true;
        }
        if self.record {
            return false;
        }
        let step = if dy > 0.0 { -1.0 } else { 1.0 };
        self.width = (self.width + step).clamp(1.0, 40.0);
        self.width_hint = true;
        let w = self.width;
        // Mid-drag, the shape under the pointer takes it too.
        match &mut self.drag {
            Drag::Drawing(shape) => shape.set_width(w),
            Drag::Editing { idx, shape, orig, changed, width, .. } => {
                edit_checkpoint(changed, &self.shapes, *idx, orig, &mut self.undo, &mut self.redo);
                shape.set_width(w);
                *width = Some(w);
            }
            _ => self.restyle(|s| s.set_width(w)),
        }
        true
    }

    pub fn key(&mut self, key: Key, mods: Mods) -> bool {
        if key == Key::Char('?') && !mods.ctrl && self.picker.is_none() && self.text.is_none() {
            self.help_key();
            return true;
        }
        // Any other key closes the shortcut sheet; Esc there mustn't also
        // cancel.
        if self.help.take().is_some() {
            return true;
        }
        if self.picker.is_some() {
            match key {
                Key::Escape => self.close_picker(false),
                Key::Enter => self.close_picker(true),
                _ => return false,
            }
            return true;
        }
        // Text entry swallows most keys.
        if let Some(te) = &mut self.text {
            match key {
                Key::Escape => self.cancel_text(),
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

        if mods.ctrl && matches!(key, Key::Char('r' | 'R')) && self.can_switch {
            self.switch_mode();
            return true;
        }
        if self.record {
            return self.record_key(key, mods);
        }
        if mods.ctrl {
            // Undo or redo mid-drag takes back the drag: undoing under it
            // would restore the shape being edited while the drag still
            // holds a copy to put back on release, leaving two.
            if self.in_progress().is_some() && matches!(key, Key::Char('z' | 'Z' | 'y' | 'Y')) {
                self.cancel_drag();
                return true;
            }
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
                    self.cancel_drag();
                } else if self.selected_shape().is_some() {
                    self.selected = None;
                    self.revision += 1;
                } else {
                    self.outcome = Some(Outcome::Cancel);
                }
            }
            Key::Enter => self.outcome = Some(Outcome::Copy),
            Key::Tab if self.sel.is_none() && !self.windows.is_empty() => self.hover_screen = !self.hover_screen,
            Key::Left | Key::Right | Key::Up | Key::Down => {
                let d = if mods.alt { 10.0 } else { 1.0 };
                let (dx, dy) = match key {
                    Key::Left => (-d, 0.0),
                    Key::Right => (d, 0.0),
                    Key::Up => (0.0, -d),
                    _ => (0.0, d),
                };
                // A picked shape moves; otherwise the selection does.
                if self.selected_shape().is_some() {
                    self.restyle(|s| s.translate(dx, dy));
                    return true;
                }
                let Some(r) = self.sel else { return false };
                self.sel = if mods.shift {
                    Rect::from_xywh(r.left(), r.top(), (r.width() + dx).max(1.0), (r.height() + dy).max(1.0))
                } else {
                    Some(self.clamp_to_desktop(r.translate(dx, dy).unwrap_or(r)))
                };
            }
            Key::Char(c @ '1'..='8') => self.set_color(c as usize - '1' as usize),
            Key::Char('[') => return self.scroll(1.0),
            Key::Char(']') => return self.scroll(-1.0),
            Key::Char(c) => {
                let c = c.to_ascii_lowercase();
                let Some(t) = std::iter::once(Tool::Select).chain(Tool::DRAWING).find(|t| t.hotkey() == c) else { return false };
                self.set_tool(t);
            }
            Key::Backspace | Key::Delete if self.selected_shape().is_some() => self.delete_selected(),
            Key::Backspace => self.undo(),
            Key::Delete | Key::Tab => return false,
        }
        true
    }

    /// `?` opens the sheet of shortcuts, or closes it. Hosts repeat a held
    /// key, which would flicker it open and shut: a `?` soon after another
    /// is taken as a repeat. Not mid-drag, where it would hide the drag.
    fn help_key(&mut self) {
        let now = std::time::Instant::now();
        let repeat = self.help_key_at.is_some_and(|t| now.duration_since(t).as_millis() < 700);
        self.help_key_at = Some(now);
        if repeat {
            return;
        }
        if self.help.take().is_none() && matches!(self.drag, Drag::None) {
            self.help = Some(self.screen_at(self.pointer).unwrap_or(0));
        }
    }

    /// Between screenshot and record mode. Annotations stay, but aren't shown
    /// in record mode: a recording is of the live screen, without them.
    fn switch_mode(&mut self) {
        self.record = !self.record;
        self.cancel_text();
        self.cancel_drag();
        self.selected = None;
        // The toolbar, hint and shapes all change.
        self.revision += 1;
    }

    /// What Enter, double-click and the main button do.
    fn accept(&self) -> Outcome {
        if self.record { Outcome::Record } else { Outcome::Copy }
    }

    /// Keys in record mode: selecting and adjusting, and the sound toggles.
    fn record_key(&mut self, key: Key, mods: Mods) -> bool {
        match key {
            Key::Char('s' | 'S') if !mods.ctrl && self.can_sound => {
                self.sound.system = !self.sound.system;
                return true;
            }
            Key::Char('m' | 'M') if !mods.ctrl && self.can_sound => {
                self.sound.mic = !self.sound.mic;
                return true;
            }
            Key::Char('g' | 'G') if !mods.ctrl => {
                self.gif = !self.gif;
                return true;
            }
            Key::Escape | Key::Enter | Key::Tab | Key::Left | Key::Right | Key::Up | Key::Down => {}
            Key::Char('a' | 'A') if mods.ctrl => {}
            _ => return false,
        }
        // Reuse the normal handling for these; Enter is the only one that differs.
        if key == Key::Enter {
            self.outcome = Some(Outcome::Record);
            return true;
        }
        self.record = false;
        let r = self.key(key, mods);
        self.record = true;
        r
    }

    fn press_button(&mut self, b: ui::Button) {
        match b {
            ui::Button::Record => self.outcome = Some(Outcome::Record),
            ui::Button::SystemSound => self.sound.system = !self.sound.system,
            ui::Button::Mic => self.sound.mic = !self.sound.mic,
            ui::Button::Gif => self.gif = !self.gif,
            ui::Button::RecordMode | ui::Button::ShotMode => self.switch_mode(),
            ui::Button::Tool(t) => self.set_tool(t),
            ui::Button::Color(i) => self.set_color(i),
            ui::Button::Undo => self.undo(),
            ui::Button::Redo => self.redo(),
            ui::Button::Copy => self.outcome = Some(Outcome::Copy),
            ui::Button::Save => self.outcome = Some(Outcome::Save),
            ui::Button::Close => self.outcome = Some(Outcome::Cancel),
        }
    }

    fn open_picker(&mut self, slot: usize) {
        self.commit_text();
        self.color = slot;
        let orig = self.palette[slot];
        let (h, s, v) = rgb_to_hsv(orig);
        self.picker = Some(Picker { slot, h, s, v, orig, drag: None });
    }

    /// Close the picker, keeping the colour or putting the old one back.
    fn close_picker(&mut self, keep: bool) {
        let Some(pk) = self.picker.take() else { return };
        if !keep {
            self.palette[pk.slot] = pk.orig;
        } else if self.palette[pk.slot] != pk.orig {
            self.palette_changed = true;
        }
    }

    /// Set the picker's colour from a point on its wheel or slider, and
    /// carry on with that part while the button is held.
    fn pick(&mut self, part: ui::PickerPart, p: Pt) {
        let Some(l) = ui::picker_layout(self) else { return };
        let Some(pk) = &mut self.picker else { return };
        match part {
            ui::PickerPart::Wheel => {
                // Black has no hue to show: picking one means a colour.
                if pk.drag.is_none() && pk.v < 0.05 {
                    pk.v = 1.0;
                }
                let (dx, dy) = (p.0 - l.center.0, p.1 - l.center.1);
                pk.h = dy.atan2(dx).to_degrees().rem_euclid(360.0);
                pk.s = (dx.hypot(dy) / l.radius).min(1.0);
            }
            ui::PickerPart::Value => pk.v = ((p.0 - l.slider.left()) / l.slider.width()).clamp(0.0, 1.0),
            ui::PickerPart::Panel => {}
        }
        pk.drag = Some(part);
        self.palette[pk.slot] = hsv_to_rgb(pk.h, pk.s, pk.v);
    }

    fn set_tool(&mut self, t: Tool) {
        self.tool = t;
        if self.selected.take().is_some() {
            self.revision += 1;
        }
    }

    /// Pick colour `i`, and give it to the picked shape.
    fn set_color(&mut self, i: usize) {
        self.color = i;
        let c = self.current_color();
        self.restyle(|s| s.set_color(c));
    }

    /// Change the picked shape, if there is one, as one undoable step.
    fn restyle(&mut self, f: impl FnOnce(&mut Shape)) {
        if self.selected_shape().is_none() || !matches!(self.drag, Drag::None) {
            return;
        }
        let Some(i) = self.selected else { return };
        self.checkpoint();
        f(&mut self.shapes[i]);
        self.revision += 1;
    }

    fn delete_selected(&mut self) {
        let Some(i) = self.selected.take() else { return };
        self.checkpoint();
        self.shapes.remove(i);
        self.revision += 1;
    }

    /// Save `shapes` for undo, before changing them.
    fn checkpoint(&mut self) {
        self.undo.push(self.shapes.clone());
        self.redo.clear();
    }

    fn push_shape(&mut self, s: Shape) {
        self.checkpoint();
        self.shapes.push(s);
        self.selected = None;
        self.revision += 1;
    }

    /// Start moving shape `i` (or reshaping it by `grip`), and pick it.
    fn start_edit(&mut self, idx: usize, p: Pt, grip: Option<Grip>) {
        let shape = self.shapes.remove(idx);
        self.selected = Some(idx);
        self.drag = Drag::Editing { idx, orig: shape.clone(), shape, grab: p, grip, moved: false, changed: false, width: None };
        self.revision += 1;
    }

    /// Drop whatever drag is going on; a shape being edited goes back as it was.
    fn cancel_drag(&mut self) {
        if let Drag::Editing { idx, orig, changed, .. } = std::mem::replace(&mut self.drag, Drag::None) {
            self.shapes.insert(idx, orig);
            if changed {
                self.undo.pop();
            }
            self.revision += 1;
        }
    }

    /// Reopen text shape `i` for typing, in its own colour and size.
    fn edit_text(&mut self, i: usize) {
        let Shape::Text { pos, text, color, size } = self.shapes[i].clone() else { return };
        self.checkpoint();
        let orig = self.shapes.remove(i);
        self.selected = None;
        self.text = Some(TextEdit { pos, text, color, size, orig: Some((i, orig)) });
        self.revision += 1;
    }

    /// Esc while typing: new text goes, edited text goes back as it was.
    fn cancel_text(&mut self) {
        if let Some(TextEdit { orig: Some((i, shape)), .. }) = self.text.take() {
            self.shapes.insert(i, shape);
            self.undo.pop();
            self.revision += 1;
        }
    }

    fn commit_text(&mut self) {
        let Some(te) = self.text.take() else { return };
        let keep = !te.text.trim().is_empty();
        let shape = Shape::Text { pos: te.pos, text: te.text, color: te.color, size: te.size };
        match te.orig {
            // Edited: back in its place, or gone if emptied (undo restores it).
            Some((i, orig)) => {
                if shape == orig {
                    self.undo.pop();
                }
                if keep {
                    self.shapes.insert(i, shape);
                }
                self.revision += 1;
            }
            None if keep => self.push_shape(shape),
            None => {}
        }
    }

    pub fn text_size(&self) -> f32 {
        shapes::text_size(self.width)
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.shapes, prev));
            self.selected = None;
            self.revision += 1;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.shapes, next));
            self.selected = None;
            self.revision += 1;
        }
    }

    /// The window the selection is, if it was picked by clicking a window
    /// (exactly its highlight) rather than dragged or adjusted.
    pub fn selected_window(&self) -> Option<&crate::windows::Window> {
        let sel = self.sel?;
        let desk = self.desktop();
        self.windows.iter().find(|w| Rect::from_xywh(w.x, w.y, w.width, w.height).and_then(|r| r.intersect(&desk)) == Some(sel))
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
        // The screens under the selection, for the output pixels from `at` on.
        let fill = |pm: &mut tiny_skia::PixmapMut, at: (i32, i32)| {
            for s in &self.screens {
                let b = s.bounds();
                if b.intersect(&rect).is_none() {
                    continue;
                }
                let k = scale / s.scale;
                let quality = if (k - 1.0).abs() < 1e-3 { FilterQuality::Nearest } else { FilterQuality::Bicubic };
                let paint = PixmapPaint { quality, ..Default::default() };
                let tx = ((b.left() - rect.left()) * scale).round() - at.0 as f32;
                let ty = ((b.top() - rect.top()) * scale).round() - at.1 as f32;
                pm.draw_pixmap(0, 0, s.bg.as_ref(), &paint, Transform::from_row(k, 0.0, 0.0, k, tx, ty), None);
            }
        };
        fill(&mut pm.as_mut(), (0, 0));
        let view = View { ox: rect.left(), oy: rect.top(), scale, swap_rb: self.swap_rb, off: (0.0, 0.0) };
        let shapes: Vec<&Shape> = self.shapes.iter().collect();
        shapes::draw_all(&mut pm.as_mut(), &view, (w as i32, h as i32), &shapes, &self.fonts, &fill);
        let mut data = pm.take();
        if self.swap_rb {
            for p in data.as_chunks_mut::<4>().0 {
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
    for p in data.as_chunks_mut::<4>().0 {
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
        for p in image.data.as_chunks_mut::<4>().0 {
            p.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        Shot {
            output: Output {
                index: 0,
                name: name.into(),
                description: String::new(),
                x,
                y: 0,
                width: w,
                height: h,
                native: (w * scale, h * scale),
            },
            image,
        }
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
    fn click_selects_topmost_window_and_tab_picks_screen() {
        let win = |x: f32, y: f32, w: f32, h: f32| crate::windows::Window {
            x,
            y,
            width: w,
            height: h,
            app: "a".into(),
            title: String::new(),
            id: String::new(),
        };
        let mut ed = editor(vec![shot("A", 0, 400, 300, 1, [0, 0, 0])], false);
        // Topmost first; the second window partly hangs off the screen.
        ed.windows = vec![win(100.0, 100.0, 50.0, 50.0), win(50.0, 50.0, 400.0, 100.0)];
        let click = |ed: &mut Editor, p: Pt| {
            ed.pointer_motion(p, Mods::default());
            ed.button_press(p, true, Mods::default());
            ed.button_release(p, true);
        };
        click(&mut ed, (120.0, 120.0));
        let r = ed.sel.unwrap();
        assert_eq!((r.left(), r.top(), r.width(), r.height()), (100.0, 100.0, 50.0, 50.0));

        ed.sel = None;
        click(&mut ed, (60.0, 60.0));
        let r = ed.sel.unwrap();
        assert_eq!((r.left(), r.top(), r.right(), r.bottom()), (50.0, 50.0, 400.0, 150.0), "clipped to the desktop");

        ed.sel = None;
        ed.key(Key::Tab, Mods::default());
        click(&mut ed, (120.0, 120.0));
        assert_eq!(ed.sel.unwrap().width(), 400.0, "Tab switches to the whole screen");

        ed.sel = None;
        ed.key(Key::Tab, Mods::default());
        click(&mut ed, (10.0, 290.0));
        assert_eq!(ed.sel.unwrap().width(), 400.0, "bare desktop selects the screen");
    }

    /// The screenshot overlay switches to record mode and back with Ctrl+R
    /// or its buttons; `clipt record`'s picker (no `can_switch`) doesn't.
    #[test]
    fn the_screenshot_overlay_switches_to_recording() {
        let mut ed = editor(vec![shot("A", 0, 400, 300, 1, [0, 0, 0])], false);
        let ctrl = Mods { ctrl: true, ..Default::default() };
        drag(&mut ed, (100.0, 100.0), (200.0, 200.0));
        assert!(!ed.key(Key::Char('r'), ctrl), "not without can_switch");
        assert!(!ed.record);
        ed.can_switch = true;
        let buttons = |ed: &Editor| ui::toolbar_layout(ed).unwrap().1.into_iter().map(|b| b.1).collect::<Vec<_>>();
        assert!(buttons(&ed).contains(&ui::Button::RecordMode));
        assert!(ed.key(Key::Char('r'), ctrl));
        assert!(ed.record);
        assert_eq!(
            buttons(&ed),
            vec![ui::Button::Record, ui::Button::SystemSound, ui::Button::Mic, ui::Button::Gif, ui::Button::ShotMode, ui::Button::Close]
        );
        ed.key(Key::Char('s'), Mods::default());
        assert!(ed.sound.system, "the sound toggles work once switched");
        ed.key(Key::Enter, Mods::default());
        assert_eq!(ed.outcome, Some(Outcome::Record));
        ed.outcome = None;
        ed.press_button(ui::Button::ShotMode);
        assert!(!ed.record);
        assert!(ed.key(Key::Char('r'), Mods::default()) && ed.tool == Tool::Rect, "plain R is still the rectangle");
        ed.key(Key::Enter, Mods::default());
        assert_eq!(ed.outcome, Some(Outcome::Copy));

        // The Screenshot | Video switch at the bottom of the screen, there
        // before anything is selected.
        let mut ed = editor(vec![shot("A", 0, 1280, 720, 1, [0, 0, 0])], false);
        assert!(ui::mode_switch_layout(&ed, ed.screens[0].bounds()).is_none(), "not without can_switch");
        ed.can_switch = true;
        let (panel, halves) = ui::mode_switch_layout(&ed, ed.screens[0].bounds()).unwrap();
        assert!(panel.bottom() < 720.0 && panel.top() > 600.0, "{panel:?}");
        let video = halves.iter().find(|h| h.1).unwrap().0;
        let shot_half = halves.iter().find(|h| !h.1).unwrap().0;
        let mid = |r: Rect| (r.left() + r.width() / 2.0, r.top() + r.height() / 2.0);
        assert_eq!(ed.cursor(), CursorKind::Crosshair);
        ed.pointer_motion(mid(video), Mods::default());
        assert_eq!(ed.cursor(), CursorKind::Pointer);
        assert!(ed.button_press(mid(video), true, Mods::default()));
        ed.button_release(mid(video), true);
        assert!(ed.record && ed.sel.is_none(), "a click on it switches, and selects nothing");
        ed.button_press(mid(video), true, Mods::default());
        assert!(ed.record, "clicking the current mode keeps it");
        ed.button_press(mid(shot_half), true, Mods::default());
        assert!(!ed.record);
    }

    #[test]
    fn record_mode_selects_but_never_annotates() {
        let mut ed = editor(vec![shot("A", 0, 400, 300, 1, [0, 0, 0])], false);
        ed.record = true;
        drag(&mut ed, (100.0, 100.0), (200.0, 200.0));
        for k in ['r', 'p', 't', '3'] {
            assert!(!ed.key(Key::Char(k), Mods::default()), "'{k}' is ignored");
        }
        assert_eq!(ed.tool, Tool::Select);
        let ctrl = Mods { ctrl: true, ..Default::default() };
        ed.key(Key::Char('c'), ctrl);
        ed.key(Key::Char('s'), ctrl);
        assert_eq!(ed.outcome, None, "no copy or save while recording");
        assert!(!ed.scroll(1.0));
        ed.key(Key::Right, Mods::default());
        assert_eq!(ed.sel.unwrap().left(), 101.0, "arrows still nudge");
        let (_, buttons) = ui::toolbar_layout(&ed).unwrap();
        assert_eq!(
            buttons.iter().map(|b| b.1).collect::<Vec<_>>(),
            vec![ui::Button::Record, ui::Button::SystemSound, ui::Button::Mic, ui::Button::Gif, ui::Button::Close]
        );
        // The sound toggles: S and M, or their buttons.
        assert_eq!(ed.sound, SoundToggles::default());
        assert!(ed.key(Key::Char('s'), Mods::default()) && ed.key(Key::Char('M'), Mods::default()));
        assert_eq!(ed.sound, SoundToggles { system: true, mic: true });
        ed.press_button(ui::Button::Mic);
        assert_eq!(ed.sound, SoundToggles { system: true, mic: false });
        // G, or its button, makes it a GIF.
        assert!(!ed.gif && ed.key(Key::Char('g'), Mods::default()) && ed.gif);
        ed.press_button(ui::Button::Gif);
        assert!(!ed.gif);
        assert_eq!(ed.outcome, None, "toggling doesn't start or cancel anything");
        ed.key(Key::Enter, Mods::default());
        assert_eq!(ed.outcome, Some(Outcome::Record));
        ed.record = true;
        ed.key(Key::Escape, Mods::default());
        assert_eq!(ed.outcome, Some(Outcome::Cancel));
        assert!(ed.record, "record mode survives reused key handling");

        // Where there's no sound to record: the GIF toggle stays, S does nothing.
        ed.outcome = None;
        ed.can_sound = false;
        let (_, buttons) = ui::toolbar_layout(&ed).unwrap();
        assert_eq!(buttons.iter().map(|b| b.1).collect::<Vec<_>>(), vec![ui::Button::Record, ui::Button::Gif, ui::Button::Close]);
        let before = ed.sound;
        ed.key(Key::Char('s'), Mods::default());
        assert_eq!(ed.sound, before, "nor the S key");
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

    /// Pixelating over a shape blurs the shape too, rather than replacing it
    /// with the blurred screenshot.
    #[test]
    fn pixelate_covers_shapes_under_it() {
        let mut ed = editor(vec![shot("A", 0, 300, 200, 1, [200, 10, 10])], false);
        drag(&mut ed, (0.0, 0.0), (300.0, 200.0));
        ed.tool = Tool::Line;
        ed.color = 4; // blue
        ed.width = 20.0;
        drag(&mut ed, (0.0, 100.0), (300.0, 100.0));
        ed.tool = Tool::Pixelate;
        drag(&mut ed, (50.0, 50.0), (250.0, 150.0));
        let img = ed.export().unwrap();
        let at = |x: usize, y: usize| &img.data[(y * 300 + x) * 4..][..3];
        assert!(at(150, 100)[2] > 100, "the line shows through, blurred: {:?}", at(150, 100));
        assert_eq!(at(150, 60), &[200, 10, 10], "away from the line, the screenshot");
        // Blocks are uniform: the line's edge is blurred, not crisp.
        let row: Vec<_> = (50..150).map(|y| at(150, y).to_vec()).collect();
        let distinct = row.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(distinct < 20, "{distinct} changes down a pixelated column");
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

    /// Text is centred on the click, the way the I-beam pointer suggests,
    /// and a faint cursor shows where it would go before clicking.
    #[test]
    fn text_lands_where_the_pointer_points() {
        let mut ed = editor(vec![shot("A", 0, 400, 300, 1, [0, 0, 0])], false);
        drag(&mut ed, (0.0, 0.0), (300.0, 200.0));
        ed.tool = Tool::Text;
        ed.pointer_motion((100.0, 100.0), Mods::default());
        assert_eq!(ed.text_ghost(), Some((100.0, 100.0)));
        ed.button_press((100.0, 100.0), true, Mods::default());
        let lh = crate::text::line_height(ed.fonts.bold(), ed.text_size());
        let pos = ed.text.as_ref().unwrap().pos;
        assert_eq!(pos, (100.0, 100.0 - lh / 2.0), "the first line's middle is the click");
        let (_, btns) = ui::toolbar_layout(&ed).unwrap();
        let r = btns[0].0;
        ed.pointer_motion((r.left() + 5.0, r.top() + 5.0), Mods::default());
        assert_eq!(ed.text_ghost(), None, "not over the toolbar");
    }

    /// Right-clicking a colour opens the picker on it: the wheel sets hue
    /// and saturation, the slider brightness; Esc puts the colour back,
    /// Enter or a click elsewhere keeps it.
    #[test]
    fn colour_picker_changes_a_palette_slot() {
        let mut ed = editor(vec![shot("A", 0, 1000, 800, 1, [0, 0, 0])], false);
        drag(&mut ed, (200.0, 100.0), (800.0, 400.0));
        let m = Mods::default();
        let dot = |ed: &Editor, i: usize| {
            let (r, _) = ui::toolbar_layout(ed).unwrap().1.into_iter().find(|b| b.1 == ui::Button::Color(i)).unwrap();
            (r.left() + r.width() / 2.0, r.top() + r.height() / 2.0)
        };
        ed.button_press(dot(&ed, 2), false, m);
        assert_eq!(ed.picker.map(|p| p.slot), Some(2));
        assert_eq!(ed.color, 2, "and picks that colour");
        let l = ui::picker_layout(&ed).unwrap();
        assert!(l.panel.bottom() <= ui::toolbar_layout(&ed).unwrap().0.top(), "above the toolbar");

        // Due right of the middle, at the edge: pure red at the current brightness.
        ed.button_press((l.center.0 + l.radius, l.center.1), true, m);
        ed.button_release((l.center.0 + l.radius, l.center.1), true);
        assert_eq!(ed.palette[2], (0xea, 0, 0), "{:?}", ed.palette[2]);
        // Drag the slider to full brightness, past its end.
        ed.button_press((l.slider.left() + 2.0, l.slider.top() + 7.0), true, m);
        ed.pointer_motion((l.slider.right() + 50.0, l.slider.top() + 7.0), m);
        ed.button_release((l.slider.right() + 50.0, l.slider.top() + 7.0), true);
        assert_eq!(ed.palette[2], (255, 0, 0));
        assert_eq!(ed.current_color(), Color::from_rgba8(255, 0, 0, 255));
        ed.key(Key::Escape, m);
        assert_eq!((ed.picker, ed.palette[2], ed.palette_changed), (None, PALETTE[2], false), "Esc puts it back");

        ed.button_press(dot(&ed, 0), false, m);
        let l = ui::picker_layout(&ed).unwrap();
        ed.button_press((l.center.0, l.center.1 - l.radius / 2.0), true, m);
        ed.button_release((l.center.0, l.center.1 - l.radius / 2.0), true);
        ed.button_press((l.panel.left() + 3.0, l.panel.top() + 3.0), true, m);
        assert!(ed.picker.is_some(), "a click on the panel's edge keeps it open");
        ed.button_press((300.0, 200.0), true, m);
        assert!(ed.picker.is_none() && ed.palette_changed && ed.palette[0] != PALETTE[0], "a click elsewhere keeps the colour");
        assert!(ed.shapes.is_empty() && ed.drag_is_none(), "and does nothing else");
    }

    /// The select tool picks shapes: drag to move, handles to reshape,
    /// colour and width keys restyle, Delete removes, and undo steps back
    /// through all of it.
    #[test]
    fn shapes_move_reshape_restyle_and_delete() {
        let mut ed = editor(vec![shot("A", 0, 800, 600, 1, [0, 0, 0])], false);
        let m = Mods::default();
        drag(&mut ed, (0.0, 0.0), (700.0, 500.0));
        ed.tool = Tool::Rect;
        drag(&mut ed, (100.0, 100.0), (200.0, 150.0));
        ed.tool = Tool::Arrow;
        drag(&mut ed, (300.0, 300.0), (400.0, 300.0));
        ed.key(Key::Char('v'), m);
        let sel = ed.sel;

        // A click on the rectangle's edge picks it; a drag moves it.
        drag(&mut ed, (100.0, 125.0), (130.0, 145.0));
        assert_eq!(ed.selected, Some(0));
        assert!(matches!(ed.shapes[0], Shape::Rect { a: (130.0, 120.0), b: (230.0, 170.0), .. }), "{:?}", ed.shapes[0]);
        assert_eq!(ed.sel, sel, "the capture area stays put");
        assert_eq!(ed.shapes.len(), 2);
        // Its bottom-right handle reshapes it.
        drag(&mut ed, (230.0, 170.0), (260.0, 200.0));
        assert!(matches!(ed.shapes[0], Shape::Rect { a: (130.0, 120.0), b: (260.0, 200.0), .. }), "{:?}", ed.shapes[0]);
        // Colour and width go to the picked shape.
        ed.key(Key::Char('5'), m);
        ed.key(Key::Char(']'), m);
        let Shape::Rect { color, width, .. } = ed.shapes[0] else { panic!() };
        assert_eq!((color, width), (ed.current_color(), ed.width));
        assert_eq!(ed.width, 5.0);
        // The arrow's end moves on its own.
        drag(&mut ed, (350.0, 300.0), (350.0, 300.0));
        assert_eq!(ed.selected, Some(1));
        drag(&mut ed, (400.0, 300.0), (420.0, 340.0));
        assert!(matches!(ed.shapes[1], Shape::Arrow { a: (300.0, 300.0), b: (420.0, 340.0), .. }), "{:?}", ed.shapes[1]);
        ed.key(Key::Delete, m);
        assert_eq!((ed.shapes.len(), ed.selected), (1, None));

        // Undo walks back: the delete, the arrow's end, the width, the
        // colour, the reshape, the move.
        let ctrl = Mods { ctrl: true, ..m };
        ed.key(Key::Char('z'), ctrl);
        assert_eq!(ed.shapes.len(), 2);
        for _ in 0..5 {
            ed.key(Key::Char('z'), ctrl);
        }
        assert!(matches!(ed.shapes[0], Shape::Rect { a: (100.0, 100.0), b: (200.0, 150.0), .. }), "{:?}", ed.shapes[0]);
        ed.key(Key::Char('Z'), Mods { shift: true, ..ctrl });
        assert!(matches!(ed.shapes[0], Shape::Rect { a: (130.0, 120.0), .. }), "redo");

        // Esc lets go of the shape before it cancels; a right-click mid-drag
        // puts the shape back.
        drag(&mut ed, (130.0, 145.0), (130.0, 145.0));
        assert_eq!(ed.selected, Some(0));
        ed.key(Key::Escape, m);
        assert_eq!((ed.selected, ed.outcome), (None, None));
        ed.button_press((130.0, 145.0), true, m);
        ed.pointer_motion((300.0, 400.0), m);
        ed.button_press((300.0, 400.0), false, m);
        assert!(matches!(ed.shapes[0], Shape::Rect { a: (130.0, 120.0), .. }), "{:?}", ed.shapes[0]);
        assert!(ed.drag_is_none());
        // A click on nothing lets go and works the capture area as before.
        drag(&mut ed, (130.0, 145.0), (130.0, 145.0));
        drag(&mut ed, (500.0, 450.0), (510.0, 450.0));
        assert_eq!(ed.selected, None);
        assert_eq!(ed.sel.unwrap().left(), sel.unwrap().left() + 10.0);
    }

    /// Scrolling mid-drag sets the width of the shape being drawn, moved or
    /// reshaped, not only the next one's; undo and Esc treat it as part of
    /// the drag.
    #[test]
    fn scrolling_mid_drag_sets_the_shapes_width() {
        let mut ed = editor(vec![shot("A", 0, 800, 600, 1, [0, 0, 0])], false);
        let m = Mods::default();
        let ctrl = Mods { ctrl: true, ..m };
        let arrow = |ed: &Editor| match ed.shapes[0] {
            Shape::Arrow { a, b, width, .. } => (a, b, width),
            ref s => panic!("{s:?}"),
        };
        drag(&mut ed, (0.0, 0.0), (700.0, 500.0));
        let w = ed.width;

        // Drawing: the arrow takes the width as it's drawn.
        ed.tool = Tool::Arrow;
        ed.button_press((100.0, 100.0), true, m);
        ed.pointer_motion((200.0, 100.0), m);
        ed.scroll(-1.0);
        ed.scroll(-1.0);
        assert!(matches!(ed.in_progress(), Some(Shape::Arrow { width, .. }) if *width == w + 2.0));
        ed.pointer_motion((300.0, 100.0), m);
        ed.button_release((300.0, 100.0), true);
        assert_eq!(arrow(&ed), ((100.0, 100.0), (300.0, 100.0), w + 2.0));

        // Moving: a scroll before the pointer has moved changes the width
        // only; a wobble within the click threshold still doesn't move it.
        ed.key(Key::Char('v'), m);
        ed.button_press((200.0, 100.0), true, m);
        ed.scroll(1.0);
        assert!(matches!(ed.selected_shape(), Some(Shape::Arrow { width, .. }) if *width == w + 1.0));
        ed.pointer_motion((201.0, 100.0), m);
        assert!(matches!(ed.selected_shape(), Some(Shape::Arrow { a: (100.0, 100.0), .. })));
        // Each motion rebuilds the shape from where the drag began; the
        // scrolled width survives that.
        ed.pointer_motion((220.0, 130.0), m);
        ed.button_release((220.0, 130.0), true);
        assert_eq!(arrow(&ed), ((120.0, 130.0), (320.0, 130.0), w + 1.0));
        // One undo step takes back both the move and the width.
        ed.key(Key::Char('z'), ctrl);
        assert_eq!(arrow(&ed), ((100.0, 100.0), (300.0, 100.0), w + 2.0));
        ed.key(Key::Char('Z'), Mods { shift: true, ..ctrl });
        assert_eq!(arrow(&ed), ((120.0, 130.0), (320.0, 130.0), w + 1.0));

        // Reshaping by a handle, then cancelling with a right-click: the
        // arrow goes back as it was, width included, and leaves no undo step.
        let undo = ed.undo.len();
        ed.button_press((320.0, 130.0), true, m);
        ed.pointer_motion((350.0, 160.0), m);
        ed.scroll(-1.0);
        assert!(matches!(ed.selected_shape(), Some(Shape::Arrow { b: (350.0, 160.0), width, .. }) if *width == w + 2.0));
        ed.button_press((350.0, 160.0), false, m);
        assert!(ed.drag_is_none());
        assert_eq!(arrow(&ed), ((120.0, 130.0), (320.0, 130.0), w + 1.0));
        assert_eq!(ed.undo.len(), undo);

        // A scroll alone, then a cancel, leaves no undo step either.
        ed.button_press((200.0, 130.0), true, m);
        ed.scroll(-1.0);
        ed.button_press((200.0, 130.0), false, m);
        assert_eq!((arrow(&ed).2, ed.undo.len()), (w + 1.0, undo));
    }

    /// Ctrl+Z mid-drag takes back the drag, rather than undoing under it
    /// and leaving a second copy of the shape when the drag lets go.
    #[test]
    fn undo_mid_drag_takes_back_the_drag() {
        let mut ed = editor(vec![shot("A", 0, 800, 600, 1, [0, 0, 0])], false);
        let m = Mods::default();
        let ctrl = Mods { ctrl: true, ..m };
        drag(&mut ed, (0.0, 0.0), (700.0, 500.0));
        ed.tool = Tool::Arrow;
        drag(&mut ed, (100.0, 100.0), (300.0, 100.0));
        let undo = ed.undo.len();

        // Moving it: the arrow goes back where it was, once.
        ed.key(Key::Char('v'), m);
        ed.button_press((200.0, 100.0), true, m);
        ed.pointer_motion((220.0, 130.0), m);
        ed.key(Key::Char('z'), ctrl);
        assert!(ed.drag_is_none());
        ed.button_release((220.0, 130.0), true);
        assert_eq!(ed.shapes.len(), 1);
        assert!(matches!(ed.shapes[0], Shape::Arrow { a: (100.0, 100.0), b: (300.0, 100.0), .. }), "{:?}", ed.shapes[0]);
        assert_eq!(ed.undo.len(), undo, "the drag's undo step went with it");

        // Drawing one: the new arrow is dropped, and the first one stays.
        ed.tool = Tool::Arrow;
        ed.button_press((100.0, 300.0), true, m);
        ed.pointer_motion((300.0, 300.0), m);
        ed.key(Key::Char('y'), ctrl);
        ed.button_release((300.0, 300.0), true);
        assert_eq!((ed.shapes.len(), ed.undo.len()), (1, undo));

        // Outside a drag, undo still undoes.
        ed.key(Key::Char('z'), ctrl);
        assert!(ed.shapes.is_empty());
    }

    /// Text reopens for typing: double-click it with the select tool, or
    /// click it with the text tool. It keeps its colour, size and place;
    /// Esc puts it back as it was.
    #[test]
    fn text_can_be_edited_again() {
        let mut ed = editor(vec![shot("A", 0, 800, 600, 1, [0, 0, 0])], false);
        let m = Mods::default();
        drag(&mut ed, (0.0, 0.0), (700.0, 500.0));
        ed.tool = Tool::Rect;
        drag(&mut ed, (400.0, 400.0), (450.0, 450.0));
        ed.tool = Tool::Text;
        ed.color = 2;
        ed.button_press((100.0, 100.0), true, m);
        for c in "hi".chars() {
            ed.key(Key::Char(c), m);
        }
        ed.key(Key::Enter, m);
        ed.tool = Tool::Rect;
        drag(&mut ed, (500.0, 400.0), (550.0, 450.0));
        let Shape::Text { color, size, pos, .. } = ed.shapes[1].clone() else { panic!("{:?}", ed.shapes[1]) };
        ed.color = 5;
        ed.width = 10.0;

        ed.key(Key::Char('v'), m);
        ed.button_press((105.0, 100.0), true, m);
        ed.button_release((105.0, 100.0), true);
        ed.button_press((105.0, 100.0), true, m);
        ed.button_release((105.0, 100.0), true);
        let te = ed.text.as_ref().expect("double-click opens it");
        assert_eq!((te.text.as_str(), te.color, te.size, te.pos), ("hi", color, size, pos), "in its own style");
        assert_eq!(ed.shapes.len(), 2);
        ed.key(Key::Char('!'), m);
        ed.key(Key::Escape, m);
        assert!(matches!(&ed.shapes[1], Shape::Text { text, .. } if text == "hi"), "Esc keeps the old text, in place");

        ed.tool = Tool::Text;
        ed.button_press((105.0, 100.0), true, m);
        assert!(ed.text.is_some(), "the text tool reopens it too");
        ed.key(Key::Char('!'), m);
        ed.key(Key::Enter, m);
        assert_eq!(ed.shapes.len(), 3);
        assert!(matches!(&ed.shapes[1], Shape::Text { text, color: c, .. } if text == "hi!" && *c == color), "{:?}", ed.shapes[1]);
        ed.key(Key::Char('z'), Mods { ctrl: true, ..m });
        assert!(matches!(&ed.shapes[1], Shape::Text { text, .. } if text == "hi"), "undo");
    }

    #[test]
    fn hsv_round_trips() {
        for c in PALETTE {
            let (h, s, v) = rgb_to_hsv(c);
            assert_eq!(hsv_to_rgb(h, s, v), c);
        }
    }

    /// Replays a session the way the overlay does (one persistent buffer,
    /// repainting only damaged rects) and checks the result matches a full
    /// redraw: any region the damage tracking misses shows up as a diff.
    #[test]
    fn partial_repaints_match_full_render() {
        let mut img = Image::new(640, 400);
        for (i, p) in img.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % 640, i / 640);
            p.copy_from_slice(&[(x * 7 % 256) as u8, (y * 5 % 256) as u8, ((x ^ y) % 256) as u8, 255]);
        }
        let out =
            Output { index: 0, name: "A".into(), description: String::new(), x: 0, y: 0, width: 640, height: 400, native: (640, 400) };
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
                        let r = damage::IRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
                        damage::repaint(ed, 0, buf.data_mut(), 640, 400, r);
                    }
                }
            }
            let mut full = Pixmap::new(640, 400).unwrap();
            ui::render(ed, 0, &mut full.as_mut(), (0, 0));
            // tiny-skia's edge antialiasing varies a little with the target size,
            // so allow small AA wobble; a missed region shows stale dim/clean
            // content or missing chrome, which is far larger.
            let diffs: Vec<(usize, i16)> = buf
                .data()
                .iter()
                .zip(full.data())
                .map(|(a, b)| (*a as i16 - *b as i16).abs())
                .enumerate()
                .filter(|(_, d)| *d > 2)
                .collect();
            let max = diffs.iter().map(|d| d.1).max().unwrap_or(0);
            // Where they are, for working out which layer's damage fell short.
            let (xs, ys): (Vec<usize>, Vec<usize>) = diffs.iter().map(|(i, _)| (i / 4 % 640, i / 4 / 640)).unzip();
            let span = |v: &[usize]| (v.iter().min().copied().unwrap_or(0), v.iter().max().copied().unwrap_or(0));
            assert!(
                max <= 48 && diffs.len() < 400,
                "step {n}: {} differing values, max {max}, in x {:?}, y {:?}: damage tracking missed something",
                diffs.len(),
                span(&xs),
                span(&ys)
            );
        };

        // Hover highlight: window, overlapping window, mid-glide frames,
        // bare desktop (the screen), Tab to the screen and back.
        let win = |x: f32, y: f32, w: f32, h: f32, app: &str| crate::windows::Window {
            x,
            y,
            width: w,
            height: h,
            app: app.into(),
            title: String::new(),
            id: String::new(),
        };
        ed.windows = vec![win(300.0, 120.0, 220.0, 160.0, "front"), win(60.0, 40.0, 320.0, 240.0, "back")];
        step(&mut ed, &|e| {
            e.pointer_motion((100.0, 100.0), m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((400.0, 200.0), m);
        });
        for _ in 0..3 {
            step(&mut ed, &|_| std::thread::sleep(std::time::Duration::from_millis(30)));
        }
        step(&mut ed, &|e| {
            e.pointer_motion((600.0, 380.0), m);
        });
        step(&mut ed, &|_| std::thread::sleep(std::time::Duration::from_millis(200)));
        step(&mut ed, &|e| {
            e.pointer_motion((100.0, 100.0), m);
            e.key(Key::Tab, m);
        });
        step(&mut ed, &|e| {
            e.key(Key::Tab, m);
        });
        step(&mut ed, &|_| std::thread::sleep(std::time::Duration::from_millis(200)));
        ed.windows.clear();
        step(&mut ed, &|e| {
            e.pointer_motion((100.0, 100.0), m);
        });
        for i in 0..6 {
            step(&mut ed, &|e| {
                e.pointer_motion((120.0 + i as f32 * 30.0, 90.0 + i as f32 * 20.0), m);
            });
        }
        step(&mut ed, &|e| {
            e.button_press((80.0, 60.0), true, m);
        });
        for i in 1..=8 {
            step(&mut ed, &|e| {
                e.pointer_motion((80.0 + i as f32 * 50.0, 60.0 + i as f32 * 30.0), m);
            });
        }
        step(&mut ed, &|e| {
            e.button_release((480.0, 300.0), true);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((300.0, 200.0), m);
        });
        step(&mut ed, &|e| {
            e.button_press((300.0, 200.0), true, m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((250.0, 150.0), m);
        }); // move selection
        step(&mut ed, &|e| {
            e.button_release((250.0, 150.0), true);
        });
        // big selection jumps: select-all, clear, select-all again
        let ctrl = Mods { ctrl: true, ..Default::default() };
        step(&mut ed, &|e| {
            e.key(Key::Char('a'), ctrl);
        });
        step(&mut ed, &|e| {
            e.button_press((300.0, 200.0), false, m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((310.0, 210.0), m);
        });
        step(&mut ed, &|e| {
            e.button_press((20.0, 20.0), true, m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((200.0, 140.0), m);
        });
        step(&mut ed, &|e| {
            e.button_release((200.0, 140.0), true);
        });
        step(&mut ed, &|e| {
            e.key(Key::Char('a'), ctrl);
        });
        step(&mut ed, &|e| {
            e.button_press((300.0, 200.0), false, m);
        });
        step(&mut ed, &|e| {
            e.button_press((40.0, 40.0), true, m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((500.0, 330.0), m);
        });
        step(&mut ed, &|e| {
            e.button_release((500.0, 330.0), true);
        });
        step(&mut ed, &|e| {
            e.tool = Tool::Pen;
        });
        step(&mut ed, &|e| {
            e.button_press((100.0, 100.0), true, m);
        });
        for i in 1..10 {
            step(&mut ed, &|e| {
                e.pointer_motion((100.0 + i as f32 * 20.0, 100.0 + (i % 3) as f32 * 25.0), m);
            });
        }
        step(&mut ed, &|e| {
            e.button_release((280.0, 125.0), true);
        });
        step(&mut ed, &|e| {
            e.tool = Tool::Pixelate;
        });
        step(&mut ed, &|e| {
            e.button_press((150.0, 150.0), true, m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((260.0, 230.0), m);
        });
        step(&mut ed, &|e| {
            e.button_release((260.0, 230.0), true);
        });
        // A small repaint inside the pixelation cuts through its blocks.
        step(&mut ed, &|e| {
            e.pointer_motion((200.0, 160.0), m);
        });
        step(&mut ed, &|e| {
            e.scroll(1.0);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((205.0, 165.0), m);
        });
        step(&mut ed, &|e| {
            e.tool = Tool::Text;
        });
        for i in 0..3 {
            step(&mut ed, &|e| {
                e.pointer_motion((100.0 + i as f32 * 10.0, 230.0 + i as f32 * 5.0), m);
            });
        }
        step(&mut ed, &|e| {
            e.button_press((120.0, 240.0), true, m);
        });
        for c in "abc".chars() {
            step(&mut ed, &|e| {
                e.key(Key::Char(c), m);
            });
        }
        step(&mut ed, &|e| {
            e.key(Key::Backspace, m);
        });
        step(&mut ed, &|e| {
            e.scroll(-1.0);
        });
        // Pick the pen stroke and move it, then let go.
        step(&mut ed, &|e| {
            e.key(Key::Char('v'), m);
        });
        step(&mut ed, &|e| {
            e.button_press((100.0, 100.0), true, m);
        });
        for i in 1..5 {
            step(&mut ed, &|e| {
                e.pointer_motion((100.0 + i as f32 * 15.0, 100.0 + i as f32 * 8.0), m);
            });
        }
        step(&mut ed, &|e| {
            e.button_release((160.0, 132.0), true);
        });
        step(&mut ed, &|e| {
            e.key(Key::Char('2'), m);
        });
        step(&mut ed, &|e| {
            e.key(Key::Escape, m);
        });
        // The colour picker: open, drag on the wheel and slider, close.
        let (_, btns) = ui::toolbar_layout(&ed).unwrap();
        let (dot, _) = *btns.iter().find(|b| b.1 == ui::Button::Color(3)).unwrap();
        step(&mut ed, &|e| {
            e.button_press((dot.left() + 5.0, dot.top() + 10.0), false, m);
        });
        let l = ui::picker_layout(&ed).unwrap();
        step(&mut ed, &|e| {
            e.button_press((l.center.0 + 20.0, l.center.1), true, m);
        });
        for i in 0..4 {
            step(&mut ed, &|e| {
                e.pointer_motion((l.center.0 + 20.0 - i as f32 * 12.0, l.center.1 + i as f32 * 9.0), m);
            });
        }
        step(&mut ed, &|e| {
            e.button_release((l.center.0, l.center.1), true);
        });
        step(&mut ed, &|e| {
            e.button_press((l.slider.left() + 30.0, l.slider.top() + 7.0), true, m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((l.slider.left() + 80.0, l.slider.top() + 7.0), m);
        });
        step(&mut ed, &|e| {
            e.button_release((l.slider.left() + 80.0, l.slider.top() + 7.0), true);
        });
        step(&mut ed, &|e| {
            e.key(Key::Enter, m);
        });
        // Record mode's toggles redraw the toolbar.
        let ctrl = Mods { ctrl: true, ..m };
        step(&mut ed, &|e| {
            e.can_switch = true;
            e.key(Key::Char('r'), ctrl);
        });
        for k in ['s', 'm', 'g', 'g'] {
            step(&mut ed, &|e| {
                e.key(Key::Char(k), m);
            });
        }
        step(&mut ed, &|e| {
            e.key(Key::Char('r'), ctrl);
        });
        // hover the toolbar to get a tooltip, then leave
        let (_, btns) = ui::toolbar_layout(&ed).unwrap();
        let (r, _) = btns[3];
        step(&mut ed, &|e| {
            e.pointer_motion((r.left() + 5.0, r.top() + 5.0), m);
        });
        step(&mut ed, &|e| {
            e.pointer_motion((50.0, 50.0), m);
        });
    }

    /// `cargo test --release bench_4k -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_4k() {
        let img = Image::new(3840, 2160);
        let out =
            Output { index: 0, name: "A".into(), description: String::new(), x: 0, y: 0, width: 3840, height: 2160, native: (3840, 2160) };
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
                        if x1 <= x0 || y1 <= y0 {
                            continue;
                        }
                        let mut tmp = Pixmap::new((x1 - x0) as u32, (y1 - y0) as u32).unwrap();
                        ui::render(ed, 0, &mut tmp.as_mut(), (x0, y0));
                        px += (x1 - x0) * (y1 - y0);
                    }
                }
            }
            eprintln!(
                "{name}: {:.2} ms/frame, {:.1}% of screen",
                t.elapsed().as_secs_f64() * 1e3 / 50.0,
                px as f64 / 50.0 / (3840.0 * 2160.0) * 100.0
            );
        };
        run("hover (crosshair+loupe)", &mut ed, &|e, i| {
            e.pointer_motion((1000.0 + i as f32 * 7.0, 800.0 + i as f32 * 3.0), m);
        });
        ed.button_press((500.0, 400.0), true, m);
        run("drag selection", &mut ed, &|e, i| {
            e.pointer_motion((900.0 + i as f32 * 20.0, 700.0 + i as f32 * 12.0), m);
        });
        ed.button_release((1900.0, 1300.0), true);
        ed.tool = Tool::Pen;
        ed.button_press((800.0, 800.0), true, m);
        run("pen stroke", &mut ed, &|e, i| {
            e.pointer_motion((800.0 + i as f32 * 15.0, 800.0 + (i % 5) as f32 * 10.0), m);
        });
        ed.button_release((1500.0, 800.0), true);
        run("hover toolbar area", &mut ed, &|e, i| {
            e.pointer_motion((1100.0 + i as f32 * 4.0, 1340.0), m);
        });
    }

    /// `?` opens the sheet of shortcuts on the pointer's screen; any key or
    /// click closes it without doing anything else, so Esc doesn't cancel.
    #[test]
    fn question_mark_shows_the_shortcuts() {
        let mut ed = editor(vec![shot("A", 0, 1366, 768, 1, [0, 0, 0]), shot("B", 1366, 1366, 768, 1, [0, 0, 0])], false);
        let m = Mods::default();
        // As if the last ? was a while ago: a quick second one is a repeat.
        let earlier = || std::time::Instant::now().checked_sub(std::time::Duration::from_secs(1));
        drag(&mut ed, (100.0, 100.0), (300.0, 300.0));
        ed.pointer_motion((2000.0, 400.0), m);
        assert!(ed.key(Key::Char('?'), Mods { shift: true, ..m }));
        assert_eq!(ed.help, Some(1));
        let r = ui::help_rect(&ed).unwrap();
        let b = ed.screens[1].bounds();
        assert!(r.left() >= b.left() && r.right() <= b.right() && r.bottom() <= b.bottom(), "fits a 1366×768 screen: {r:?}");
        let mut small = editor(vec![shot("A", 0, 960, 540, 2, [0, 0, 0])], false);
        small.key(Key::Char('?'), m);
        let r = ui::help_rect(&small).unwrap();
        assert!(r.left() >= 0.0 && r.top() >= 0.0 && r.right() <= 960.0 && r.bottom() <= 540.0, "shrinks to fit 1920×1080 at 200 %: {r:?}");
        ed.key(Key::Escape, m);
        assert_eq!((ed.help, ed.outcome), (None, None));

        ed.help_key_at = earlier();
        ed.key(Key::Char('?'), m);
        let sel = ed.sel;
        ed.button_press((150.0, 150.0), false, m);
        assert_eq!((ed.help, ed.sel), (None, sel), "the right-click only closed it");

        ed.can_switch = true;
        ed.key(Key::Char('r'), Mods { ctrl: true, ..m });
        ed.help_key_at = earlier();
        assert!(ed.key(Key::Char('?'), m) && ed.help.is_some(), "record mode has one too");
        ed.key(Key::Char('?'), m);
        ed.key(Key::Char('?'), m);
        assert!(ed.help.is_some(), "a held ? repeats; that doesn't close it");
        ed.help_key_at = earlier();
        ed.key(Key::Char('?'), m);
        assert!(ed.help.is_none(), "? again closes it");

        ed.help_key_at = earlier();
        ed.key(Key::Char('?'), m);
        let w = ed.width;
        assert!(ed.scroll(1.0) && ed.help.is_none() && ed.width == w, "scrolling only closed it");
        ed.button_press((150.0, 150.0), true, m);
        ed.help_key_at = earlier();
        ed.key(Key::Char('?'), m);
        assert!(ed.help.is_none(), "not mid-drag");
        ed.button_release((150.0, 150.0), true);

        ed.key(Key::Char('r'), Mods { ctrl: true, ..m });
        ed.set_tool(Tool::Text);
        ed.button_press((150.0, 150.0), true, m);
        ed.button_release((150.0, 150.0), true);
        ed.key(Key::Char('?'), m);
        assert_eq!((ed.help, ed.text.as_ref().map(|t| t.text.as_str())), (None, Some("?")), "typed into text");
    }

    /// The hint bar says how to select, then how to finish until something
    /// is drawn, moving out of the selection's way.
    #[test]
    fn the_hint_bar_follows_along() {
        let mut ed = editor(vec![shot("A", 0, 1366, 768, 1, [0, 0, 0])], false);
        let b = ed.screens[0].bounds();
        let top = ui::hint_rect(&ed, b).expect("before selecting").top();
        drag(&mut ed, (400.0, 300.0), (900.0, 500.0));
        assert_eq!(ui::hint_rect(&ed, b).map(|r| r.top()), Some(top), "after selecting");
        ed.sel = Rect::from_xywh(400.0, 10.0, 500.0, 300.0);
        assert!(ui::hint_rect(&ed, b).unwrap().top() > 500.0, "below, out of the selection's way");
        ed.sel = Some(b);
        assert!(ui::hint_rect(&ed, b).is_none(), "nowhere to put it");
        ed.sel = Rect::from_xywh(400.0, 300.0, 500.0, 200.0);
        ed.set_tool(Tool::Rect);
        drag(&mut ed, (450.0, 350.0), (500.0, 400.0));
        assert!(!ed.shapes.is_empty() && ui::hint_rect(&ed, b).is_none(), "gone once something is drawn");
    }

    /// Shapes drawn before switching to record mode aren't shown there, so
    /// they don't stop a right-click from selecting again.
    #[test]
    fn right_click_selects_again_in_record_mode() {
        let mut ed = editor(vec![shot("A", 0, 400, 300, 1, [0, 0, 0])], false);
        let m = Mods::default();
        drag(&mut ed, (100.0, 100.0), (300.0, 250.0));
        ed.shapes.push(Shape::Counter { pos: (150.0, 150.0), n: 1, color: Color::BLACK, radius: 20.0 });
        ed.button_press((150.0, 150.0), false, m);
        assert!(ed.sel.is_some(), "screenshot mode keeps the selection under the shapes");
        ed.record = true;
        ed.button_press((150.0, 150.0), false, m);
        assert!(ed.sel.is_none());
    }
}
