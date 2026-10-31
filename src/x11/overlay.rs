//! The editor on X11: one override-redirect window per monitor (so the window
//! manager neither decorates nor moves it, and it covers panels), keyboard and
//! pointer grabs, and damage-tracked repaints through MIT-SHM where the server
//! offers it (local displays), plain PutImage otherwise (remote/ssh -X).

use super::X11;
use super::clip::ClipServer;
use crate::editor::{CursorKind, Damage, Editor, Handle, Key, Mods, Outcome};
use crate::overlay::{IRect, merge, screen_rects};
use crate::session::{Later, UiSession};
use anyhow::{Context, Result, bail};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, GrabMode, GrabStatus, ImageFormat, KeyButMask, PropMode, Window,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

struct Win {
    id: Window,
    gc: u32,
    screen: usize,
    buf: Buffer,
    full: bool,
}

/// Pixel storage shared with the server (MIT-SHM) or private (PutImage).
enum Buffer {
    Shm { seg: u32, map: memmap2::MmapMut },
    Plain(Vec<u8>),
}

impl Buffer {
    fn bytes(&mut self) -> &mut [u8] {
        match self {
            Buffer::Shm { map, .. } => map,
            Buffer::Plain(v) => v,
        }
    }
}

pub struct Session {
    x: X11,
    editor: Editor,
    wins: Vec<Win>,
    depth: u8,
    keymap: Keymap,
    cursors: CursorFont,
    current_cursor: Option<CursorKind>,
    last_time: u32,
    clip: Option<ClipServer>,
    dirty: bool,
}

impl Session {
    pub fn new(mut editor: Editor) -> Result<Self> {
        let x = X11::connect()?;
        let screen = x.screen().clone();
        // The editor draws BGRA when the visual's red channel is the high byte
        // (practically always); otherwise RGBA.
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.visual_id == screen.root_visual)
            .context("root visual not found")?;
        editor.set_swap_rb(visual.red_mask == 0xff0000);
        let shm = x.conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some() && is_local_display();

        let mut wins = Vec::new();
        for (i, scr) in editor.screens.iter().enumerate() {
            let o = &scr.output;
            let id = x.conn.generate_id()?;
            let events = EventMask::EXPOSURE
                | EventMask::KEY_PRESS
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::POINTER_MOTION
                | EventMask::LEAVE_WINDOW
                | EventMask::ENTER_WINDOW;
            x.conn.create_window(
                screen.root_depth,
                id,
                screen.root,
                o.x as i16,
                o.y as i16,
                o.width as u16,
                o.height as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &CreateWindowAux::new().override_redirect(1).event_mask(events).background_pixmap(x11rb::NONE),
            )?;
            // Lets compositors (picom etc.) match the overlay to skip
            // shadows/fades, and tests find it.
            x.conn.change_property8(PropMode::REPLACE, id, AtomEnum::WM_CLASS, AtomEnum::STRING, b"clipt\0clipt\0")?;
            x.conn.change_property8(PropMode::REPLACE, id, AtomEnum::WM_NAME, AtomEnum::STRING, b"clipt")?;
            let gc = x.conn.generate_id()?;
            x.conn.create_gc(gc, id, &CreateGCAux::new().graphics_exposures(0))?;
            let len = (scr.bg.width() * scr.bg.height() * 4) as usize;
            let buf = if shm { shm_buffer(&x, len).unwrap_or_else(|_| Buffer::Plain(vec![0; len])) } else { Buffer::Plain(vec![0; len]) };
            wins.push(Win { id, gc, screen: i, buf, full: true });
        }
        for w in &wins {
            x.conn.map_window(w.id)?;
        }
        x.conn.flush()?;

        let cursors = CursorFont::open(&x)?;
        let mut s = Self {
            keymap: Keymap::load(&x)?,
            x,
            editor,
            wins,
            depth: screen.root_depth,
            cursors,
            current_cursor: None,
            last_time: x11rb::CURRENT_TIME,
            clip: None,
            dirty: true,
        };
        s.grab()?;
        if let Ok(p) = s.x.conn.query_pointer(screen.root)?.reply() {
            s.editor.pointer_motion((p.root_x as f32, p.root_y as f32), Mods::default());
        }
        Ok(s)
    }

    /// Grab keyboard and pointer. The hotkey that launched us may still be
    /// held by the window manager, so retry for up to a second.
    fn grab(&mut self) -> Result<()> {
        let w = self.wins[0].id;
        let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
        let cursor = self.cursors.get(&self.x, CursorKind::Crosshair);
        let (mut kb, mut ptr) = (false, false);
        for _ in 0..100 {
            if !ptr {
                ptr = self
                    .x
                    .conn
                    .grab_pointer(false, w, mask, GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, cursor, x11rb::CURRENT_TIME)?
                    .reply()?
                    .status
                    == GrabStatus::SUCCESS;
            }
            if !kb {
                kb = self.x.conn.grab_keyboard(false, w, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?.reply()?.status
                    == GrabStatus::SUCCESS;
            }
            if kb && ptr {
                self.current_cursor = Some(CursorKind::Crosshair);
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        bail!("couldn't grab the {} (another program holds it)", if kb { "pointer" } else { "keyboard" })
    }

    fn handle(&mut self, ev: Event) -> Result<()> {
        if let Some(c) = &mut self.clip
            && c.handle(&self.x, &ev)?
        {
            return Ok(());
        }
        let mods = |state: KeyButMask| Mods {
            shift: state.contains(KeyButMask::SHIFT),
            ctrl: state.contains(KeyButMask::CONTROL),
            alt: state.contains(KeyButMask::MOD1),
        };
        let changed = match ev {
            Event::Expose(e) => {
                if let Some(w) = self.wins.iter_mut().find(|w| w.id == e.window) {
                    w.full = true;
                }
                true
            }
            Event::MotionNotify(e) => {
                self.last_time = e.time;
                self.editor.pointer_motion((e.root_x as f32, e.root_y as f32), mods(e.state))
            }
            Event::ButtonPress(e) => {
                self.last_time = e.time;
                let p = (e.root_x as f32, e.root_y as f32);
                match e.detail {
                    1 => self.editor.button_press(p, true, mods(e.state)),
                    3 => self.editor.button_press(p, false, mods(e.state)),
                    4 => self.editor.scroll(-1.0),
                    5 => self.editor.scroll(1.0),
                    _ => false,
                }
            }
            Event::ButtonRelease(e) => {
                self.last_time = e.time;
                let p = (e.root_x as f32, e.root_y as f32);
                match e.detail {
                    1 => self.editor.button_release(p, true),
                    3 => self.editor.button_release(p, false),
                    _ => false,
                }
            }
            Event::KeyPress(e) => {
                self.last_time = e.time;
                let m = mods(e.state);
                match self.keymap.key(e.detail, m) {
                    Some(k) => self.editor.key(k, m),
                    None => false,
                }
            }
            _ => false,
        };
        self.dirty |= changed;
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let damage = self.editor.take_damage();
        for i in 0..self.wins.len() {
            let scr_idx = self.wins[i].screen;
            let (w, h) = (self.editor.screens[scr_idx].bg.width(), self.editor.screens[scr_idx].bg.height());
            let rects = match (&damage, self.wins[i].full) {
                (Damage::Rects(rs), false) => Some(merge(screen_rects(&self.editor.screens[scr_idx], rs))),
                _ => None,
            };
            let win = &mut self.wins[i];
            let buf = win.buf.bytes();
            let regions = match rects {
                None => {
                    if let Some(mut pm) = tiny_skia::PixmapMut::from_bytes(buf, w, h) {
                        crate::editor::ui::render(&self.editor, scr_idx, &mut pm, (0, 0));
                    }
                    vec![IRect { x: 0, y: 0, w: w as i32, h: h as i32 }]
                }
                Some(rs) => {
                    for r in &rs {
                        let Some(mut tmp) = tiny_skia::Pixmap::new(r.w as u32, r.h as u32) else { continue };
                        crate::editor::ui::render(&self.editor, scr_idx, &mut tmp.as_mut(), (r.x, r.y));
                        let row = r.w as usize * 4;
                        for (y, src) in tmp.data().chunks_exact(row).enumerate() {
                            let off = ((r.y as usize + y) * w as usize + r.x as usize) * 4;
                            buf[off..off + row].copy_from_slice(src);
                        }
                    }
                    rs
                }
            };
            win.full = false;
            for r in regions {
                put(&self.x, win, self.depth, w, h, r)?;
            }
        }
        // Round-trip so the server has read the shared buffers before we
        // draw into them again.
        self.x.conn.get_input_focus()?.reply()?;
        self.dirty = false;
        Ok(())
    }

    fn update_cursor(&mut self) -> Result<()> {
        let want = self.editor.cursor();
        if self.current_cursor == Some(want) {
            return Ok(());
        }
        let c = self.cursors.get(&self.x, want);
        let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
        self.x.conn.change_active_pointer_grab(c, x11rb::CURRENT_TIME, mask)?;
        self.current_cursor = Some(want);
        Ok(())
    }
}

impl UiSession for Session {
    fn run(&mut self) -> Result<Outcome> {
        loop {
            if self.dirty || self.wins.iter().any(|w| w.full) {
                self.draw()?;
            }
            self.update_cursor()?;
            self.x.conn.flush()?;
            let ev = self.x.conn.wait_for_event()?;
            self.handle(ev)?;
            while let Some(ev) = self.x.conn.poll_for_event()? {
                self.handle(ev)?;
            }
            if let Some(o) = self.editor.outcome {
                return Ok(o);
            }
        }
    }

    fn editor(&mut self) -> &mut Editor {
        &mut self.editor
    }

    fn claim_clipboard(&mut self) -> Result<Arc<Later<Vec<u8>>>> {
        let data = Arc::new(Later::default());
        self.clip = Some(ClipServer::claim(&self.x, data.clone(), self.last_time)?);
        Ok(data)
    }

    fn hide(&mut self) -> Result<()> {
        self.x.conn.ungrab_pointer(x11rb::CURRENT_TIME)?;
        self.x.conn.ungrab_keyboard(x11rb::CURRENT_TIME)?;
        for w in self.wins.drain(..) {
            self.x.conn.destroy_window(w.id)?;
            self.x.conn.free_gc(w.gc)?;
            if let Buffer::Shm { seg, .. } = w.buf {
                self.x.conn.shm_detach(seg)?;
            }
        }
        self.x.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn release_memory(&mut self) {
        self.editor.screens = Vec::new();
        self.editor.shapes = Vec::new();
        crate::trim_heap();
    }

    fn serve_clipboard(&mut self) -> Result<()> {
        while self.clip.as_ref().is_some_and(|c| !c.done) {
            let ev = self.x.conn.wait_for_event()?;
            if let Some(c) = &mut self.clip {
                c.handle(&self.x, &ev)?;
            }
        }
        Ok(())
    }
}

/// Send one rectangle of the window's buffer to the server.
fn put(x: &X11, win: &mut Win, depth: u8, w: u32, h: u32, r: IRect) -> Result<()> {
    match &mut win.buf {
        Buffer::Shm { seg, .. } => {
            x.conn.shm_put_image(
                win.id,
                win.gc,
                w as u16,
                h as u16,
                r.x as u16,
                r.y as u16,
                r.w as u16,
                r.h as u16,
                r.x as i16,
                r.y as i16,
                depth,
                ImageFormat::Z_PIXMAP.into(),
                false,
                *seg,
                0,
            )?;
        }
        Buffer::Plain(data) => {
            // Rows of the rectangle, in chunks that fit one request.
            let row = r.w as usize * 4;
            let rows_per = ((x.conn.maximum_request_bytes() - 64) / row).max(1);
            let mut y = 0;
            while y < r.h as usize {
                let n = rows_per.min(r.h as usize - y);
                let mut chunk = Vec::with_capacity(row * n);
                for yy in y..y + n {
                    let off = ((r.y as usize + yy) * w as usize + r.x as usize) * 4;
                    chunk.extend_from_slice(&data[off..off + row]);
                }
                x.conn.put_image(
                    ImageFormat::Z_PIXMAP,
                    win.id,
                    win.gc,
                    r.w as u16,
                    n as u16,
                    r.x as i16,
                    (r.y as usize + y) as i16,
                    0,
                    depth,
                    &chunk,
                )?;
                y += n;
            }
        }
    }
    let _ = h;
    Ok(())
}

fn shm_buffer(x: &X11, len: usize) -> Result<Buffer> {
    let fd: OwnedFd = rustix::fs::memfd_create("clipt-x11", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::fs::ftruncate(&fd, len as u64)?;
    let map = unsafe { memmap2::MmapMut::map_mut(&fd)? };
    let seg = x.conn.generate_id()?;
    x.conn.shm_attach_fd(seg, fd, false)?.check()?;
    Ok(Buffer::Shm { seg, map })
}

/// MIT-SHM only works when client and server share a machine.
fn is_local_display() -> bool {
    std::env::var("DISPLAY").is_ok_and(|d| d.starts_with(':') || d.starts_with("unix:"))
}

/// Keycode -> keysym from the core keyboard mapping, then to the editor's keys.
struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    fn load(x: &X11) -> Result<Self> {
        let setup = x.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let r = x.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Self { min, per: r.keysyms_per_keycode as usize, syms: r.keysyms })
    }

    fn key(&self, code: u8, m: Mods) -> Option<Key> {
        let base = (code.checked_sub(self.min)? as usize) * self.per;
        let sym = |i: usize| self.syms.get(base + i).copied().filter(|&s| s != 0);
        let lower = sym(0)?;
        let raw = if m.shift { sym(1).unwrap_or(lower) } else { lower };
        Some(match raw {
            0xff1b => Key::Escape,
            0xff0d | 0xff8d => Key::Enter,
            0xff08 => Key::Backspace,
            0xff51 => Key::Left,
            0xff52 => Key::Up,
            0xff53 => Key::Right,
            0xff54 => Key::Down,
            s => {
                let c = xkeysym::Keysym::new(s).key_char()?;
                if c.is_control() {
                    return None;
                }
                Key::Char(c)
            }
        })
    }
}

/// Cursors from the core "cursor" font (present on every X server).
struct CursorFont {
    font: u32,
    cache: Vec<(CursorKind, u32)>,
}

impl CursorFont {
    fn open(x: &X11) -> Result<Self> {
        let font = x.conn.generate_id()?;
        x.conn.open_font(font, b"cursor")?;
        Ok(Self { font, cache: Vec::new() })
    }

    fn get(&mut self, x: &X11, k: CursorKind) -> u32 {
        if let Some(&(_, c)) = self.cache.iter().find(|(kk, _)| *kk == k) {
            return c;
        }
        // Glyph indices from X11/cursorfont.h.
        let glyph = match k {
            CursorKind::Crosshair => 34,
            CursorKind::Move => 52,
            CursorKind::Pointer => 60,
            CursorKind::Text => 152,
            CursorKind::Resize(h) => match h {
                Handle::N | Handle::S => 116,
                Handle::E | Handle::W => 108,
                Handle::NW => 134,
                Handle::NE => 136,
                Handle::SW => 12,
                Handle::SE => 14,
            },
        };
        let Ok(c) = x.conn.generate_id() else { return x11rb::NONE };
        let _ = x.conn.create_glyph_cursor(c, self.font, self.font, glyph, glyph + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff);
        self.cache.push((k, c));
        c
    }
}
