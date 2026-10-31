//! Fullscreen frozen-screen overlay hosting the editor. One surface per output:
//! wlr-layer-shell where available (KDE, wlroots, Hyprland, niri, COSMIC),
//! otherwise a fullscreen xdg_toplevel (GNOME).

use crate::editor::{CursorKind, Editor, Handle, Key, Mods};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    data_device_manager::{
        DataDeviceManagerState,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{CopyPasteSource, DataSourceHandler},
        WritePipe,
    },
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer, BTN_LEFT},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::io::Write;
use std::sync::Arc;
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_data_device_manager::DndAction, wl_data_source::WlDataSource, wl_data_device::WlDataDevice, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};

enum Role {
    Layer(LayerSurface),
    Window(Window),
}

impl Role {
    fn surface(&self) -> &wl_surface::WlSurface {
        match self {
            Role::Layer(l) => l.wl_surface(),
            Role::Window(w) => w.wl_surface(),
        }
    }
}

/// Screen-pixel rectangle.
#[derive(Clone, Copy, Debug)]
struct IRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl IRect {
    fn right(&self) -> i32 {
        self.x + self.w
    }
    fn bottom(&self) -> i32 {
        self.y + self.h
    }
    fn union(&self, o: &IRect) -> IRect {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        IRect { x, y, w: self.right().max(o.right()) - x, h: self.bottom().max(o.bottom()) - y }
    }
    fn touches(&self, o: &IRect) -> bool {
        self.x <= o.right() && o.x <= self.right() && self.y <= o.bottom() && o.y <= self.bottom()
    }
}

/// Merge overlapping rectangles until none touch.
fn merge(mut v: Vec<IRect>) -> Vec<IRect> {
    let mut i = 0;
    while i < v.len() {
        let mut j = i + 1;
        let mut merged = false;
        while j < v.len() {
            if v[i].touches(&v[j]) {
                let r = v.swap_remove(j);
                v[i] = v[i].union(&r);
                merged = true;
            } else {
                j += 1;
            }
        }
        if !merged {
            i += 1;
        }
    }
    v
}

/// Damage still owed to a buffer or the compositor. `None` = everything.
type Pending = Option<Vec<IRect>>;

fn add(p: &mut Pending, rects: &Pending) {
    match (p.as_mut(), rects) {
        (Some(v), Some(r)) => v.extend_from_slice(r),
        _ => *p = None,
    }
}

struct Buf {
    buffer: smithay_client_toolkit::shm::slot::Buffer,
    /// What changed since this buffer was last painted.
    stale: Pending,
}

struct Surf {
    screen: usize,
    role: Role,
    viewport: Option<WpViewport>,
    configured: bool,
    frame_pending: bool,
    bufs: Vec<Buf>,
    /// Damage to report on the next commit.
    damage: Pending,
}

impl Surf {
    fn dirty(&self) -> bool {
        self.damage.as_ref().is_none_or(|d| !d.is_empty())
    }
}

pub struct Overlay {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    format: wl_shm::Format,
    data_mgr: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    viewporter: Option<WpViewporter>,
    surfs: Vec<Surf>,
    pointer: Option<ThemedPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    mods: Mods,
    serial: u32,
    cursor: Option<CursorKind>,
    pub editor: Editor,
    conn: Connection,
    /// Clipboard payload while we're serving it after the overlay closed.
    clip: Option<(CopyPasteSource, Arc<Vec<u8>>)>,
    clip_done: bool,
}

pub struct Session {
    pub overlay: Overlay,
    queue: wayland_client::EventQueue<Overlay>,
}

impl Session {
    /// Build surfaces for every screen in `editor`. The editor's byte order is
    /// switched here to match the shm formats the compositor offers.
    pub fn new(editor: Editor) -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<Overlay>(&conn)?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
        let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;
        let layer_shell = LayerShell::bind(&globals, &qh).ok();
        let xdg = if layer_shell.is_none() { Some(XdgShell::bind(&globals, &qh).context("neither layer-shell nor xdg-shell available")?) } else { None };
        let mut ov = Overlay {
            registry: RegistryState::new(&globals),
            seats: SeatState::new(&globals, &qh),
            outputs: OutputState::new(&globals, &qh),
            // Room for two full frames per output, so the pool never regrows.
            pool: SlotPool::new(editor.screens.iter().map(|s| s.bg.data().len() * 2).sum::<usize>().max(4096), &shm)?,
            compositor,
            shm,
            format: wl_shm::Format::Argb8888,
            data_mgr: DataDeviceManagerState::bind(&globals, &qh).ok(),
            data_device: None,
            viewporter: globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok(),
            surfs: Vec::new(),
            pointer: None,
            keyboard: None,
            mods: Mods::default(),
            serial: 0,
            cursor: None,
            editor,
            conn: conn.clone(),
            clip: None,
            clip_done: false,
        };
        queue.roundtrip(&mut ov)?; // shm formats, outputs, seats
        // ABGR8888 is RGBA in memory (tiny-skia's layout); the mandatory
        // ARGB8888 is BGRA, which the editor handles by swapping red/blue.
        if ov.shm.formats().contains(&wl_shm::Format::Abgr8888) {
            ov.format = wl_shm::Format::Abgr8888;
            ov.editor.set_swap_rb(false);
        } else {
            ov.editor.set_swap_rb(true);
        }

        let wl_outputs: Vec<_> = ov.outputs.outputs().collect();
        for (i, scr) in ov.editor.screens.iter().enumerate() {
            let out = &scr.output;
            let wl_out = wl_outputs
                .iter()
                .find(|o| ov.outputs.info(o).and_then(|i| i.name).as_deref() == Some(out.name.as_str()))
                .or(wl_outputs.get(out.index))
                .cloned();
            let surface = ov.compositor.create_surface(&qh);
            let viewport = ov.viewporter.as_ref().map(|vp| vp.get_viewport(&surface, &qh, ()));
            let role = if let Some(ls) = &layer_shell {
                let l = ls.create_layer_surface(&qh, surface, Layer::Overlay, Some("glint"), wl_out.as_ref());
                l.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
                l.set_exclusive_zone(-1);
                l.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
                l.commit();
                Role::Layer(l)
            } else {
                let w = xdg.as_ref().unwrap().create_window(surface, WindowDecorations::None, &qh);
                w.set_title("glint");
                w.set_app_id("glint");
                w.set_fullscreen(wl_out.as_ref());
                w.commit();
                Role::Window(w)
            };
            ov.surfs.push(Surf { screen: i, role, viewport, configured: false, frame_pending: false, bufs: Vec::new(), damage: None });
        }
        Ok(Self { overlay: ov, queue })
    }

    /// Run until the editor produces an outcome.
    pub fn run(&mut self) -> Result<crate::editor::Outcome> {
        loop {
            self.queue.blocking_dispatch(&mut self.overlay)?;
            if let Some(o) = self.overlay.editor.outcome {
                return Ok(o);
            }
            let qh = self.queue.handle();
            self.overlay.draw_dirty(&qh);
        }
    }

    /// Put a PNG on the clipboard. Must be called while the overlay still has
    /// focus (the compositor checks the input serial).
    pub fn set_clipboard(&mut self, png: Vec<u8>) -> Result<()> {
        let qh = self.queue.handle();
        let ov = &mut self.overlay;
        let (Some(mgr), Some(dev)) = (&ov.data_mgr, &ov.data_device) else {
            anyhow::bail!("compositor has no data device; can't copy");
        };
        let src = mgr.create_copy_paste_source(&qh, ["image/png"]);
        src.set_selection(dev, ov.serial);
        ov.clip = Some((src, Arc::new(png)));
        self.queue.roundtrip(&mut self.overlay)?;
        Ok(())
    }

    pub fn hide(&mut self) -> Result<()> {
        for s in self.overlay.surfs.drain(..) {
            drop(s.bufs);
        }
        self.overlay.pointer = None;
        self.queue.roundtrip(&mut self.overlay)?;
        Ok(())
    }

    /// Free the captures, editor and buffer pool. Called before we linger in
    /// the background serving the clipboard, which only needs the PNG.
    pub fn release_memory(&mut self) {
        let ov = &mut self.overlay;
        ov.editor.screens = Vec::new();
        ov.editor.shapes = Vec::new();
        if let Ok(pool) = SlotPool::new(4096, &ov.shm) {
            ov.pool = pool;
        }
        unsafe { libc::malloc_trim(0) };
    }

    /// Keep serving the clipboard until another client takes the selection.
    pub fn serve_clipboard(&mut self) -> Result<()> {
        while self.overlay.clip.is_some() && !self.overlay.clip_done {
            self.queue.blocking_dispatch(&mut self.overlay)?;
        }
        Ok(())
    }
}

impl Overlay {
    /// Turn the editor's logical damage into per-surface pixel damage.
    fn mark_all(&mut self) {
        let damage = self.editor.take_damage();
        for s in &mut self.surfs {
            let scr = &self.editor.screens[s.screen];
            let rects: Pending = match &damage {
                crate::editor::Damage::Full => None,
                crate::editor::Damage::Rects(rs) => {
                    let (bw, bh) = (scr.bg.width() as i32, scr.bg.height() as i32);
                    let o = &scr.output;
                    let k = scr.scale;
                    Some(
                        rs.iter()
                            .filter_map(|r| {
                                let x0 = (((r.left() - o.x as f32) * k).floor() as i32).clamp(0, bw);
                                let y0 = (((r.top() - o.y as f32) * k).floor() as i32).clamp(0, bh);
                                let x1 = (((r.right() - o.x as f32) * k).ceil() as i32).clamp(0, bw);
                                let y1 = (((r.bottom() - o.y as f32) * k).ceil() as i32).clamp(0, bh);
                                (x1 > x0 && y1 > y0).then_some(IRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
                            })
                            .collect(),
                    )
                }
            };
            add(&mut s.damage, &rects);
            for b in &mut s.bufs {
                add(&mut b.stale, &rects);
            }
        }
    }

    fn global(&self, surface: &wl_surface::WlSurface, pos: (f64, f64)) -> Option<(f32, f32)> {
        let s = self.surfs.iter().find(|s| s.role.surface() == surface)?;
        let o = &self.editor.screens[s.screen].output;
        Some((o.x as f32 + pos.0 as f32, o.y as f32 + pos.1 as f32))
    }

    fn draw_dirty(&mut self, qh: &QueueHandle<Self>) {
        for i in 0..self.surfs.len() {
            let s = &self.surfs[i];
            if s.configured && s.dirty() && !s.frame_pending {
                self.draw(qh, i);
            }
        }
        self.update_cursor();
    }

    fn draw(&mut self, qh: &QueueHandle<Self>, i: usize) {
        let scr_idx = self.surfs[i].screen;
        let (w, h) = (self.editor.screens[scr_idx].bg.width(), self.editor.screens[scr_idx].bg.height());
        let out = self.editor.screens[scr_idx].output.clone();
        let t = std::time::Instant::now();

        // Reuse a buffer the compositor has released; allocate up to three.
        let surf = &mut self.surfs[i];
        let idx = match surf.bufs.iter().position(|b| b.buffer.canvas(&mut self.pool).is_some()) {
            Some(idx) => idx,
            None if surf.bufs.len() < 3 => {
                let Ok((buffer, _)) = self.pool.create_buffer(w as i32, h as i32, w as i32 * 4, self.format) else { return };
                surf.bufs.push(Buf { buffer, stale: None });
                surf.bufs.len() - 1
            }
            None => return, // all busy; a release event will wake us
        };
        let stale = std::mem::replace(&mut surf.bufs[idx].stale, Some(Vec::new()));
        let damage = std::mem::replace(&mut surf.damage, Some(Vec::new()));
        let canvas = surf.bufs[idx].buffer.canvas(&mut self.pool).unwrap();

        // Repaint only what this buffer is missing; fall back to a full frame
        // when the damage covers most of the screen anyway.
        let full_area = (w * h) as i64;
        let rects = stale.map(merge).filter(|v| v.iter().map(|r| (r.w * r.h) as i64).sum::<i64>() < full_area / 2);
        let mut painted = 0i64;
        match &rects {
            None => {
                if let Some(mut pm) = tiny_skia::PixmapMut::from_bytes(canvas, w, h) {
                    crate::editor::ui::render(&self.editor, scr_idx, &mut pm, (0, 0));
                }
                painted = full_area;
            }
            Some(rs) => {
                for r in rs {
                    let Some(mut tmp) = tiny_skia::Pixmap::new(r.w as u32, r.h as u32) else { continue };
                    crate::editor::ui::render(&self.editor, scr_idx, &mut tmp.as_mut(), (r.x, r.y));
                    let row = r.w as usize * 4;
                    for (y, src) in tmp.data().chunks_exact(row).enumerate() {
                        let off = ((r.y as usize + y) * w as usize + r.x as usize) * 4;
                        canvas[off..off + row].copy_from_slice(src);
                    }
                    painted += (r.w * r.h) as i64;
                }
            }
        }
        let render_ms = t.elapsed().as_secs_f64() * 1e3;

        let s = &mut self.surfs[i];
        let surface = s.role.surface();
        if let Some(vp) = &s.viewport {
            vp.set_destination(out.width as i32, out.height as i32);
        } else {
            let k = (w / out.width.max(1)).max(1) as i32;
            surface.set_buffer_scale(k);
        }
        match &damage {
            Some(ds) if rects.is_some() => {
                for d in merge(ds.clone()) {
                    surface.damage_buffer(d.x, d.y, d.w, d.h);
                }
            }
            _ => surface.damage_buffer(0, 0, w as i32, h as i32),
        }
        surface.frame(qh, FrameCallbackData(surface.clone()));
        let _ = s.bufs[idx].buffer.attach_to(surface);
        surface.commit();
        s.frame_pending = true;
        if std::env::var_os("GLINT_DEBUG").is_some() {
            eprintln!("glint: {} buf{idx} painted {:.1}% in {render_ms:.2} ms", out.name, painted as f64 * 100.0 / full_area as f64);
        }
    }

    fn update_cursor(&mut self) {
        let want = self.editor.cursor();
        if self.cursor == Some(want) {
            return;
        }
        let Some(p) = &self.pointer else { return };
        let icon = match want {
            CursorKind::Crosshair => CursorIcon::Crosshair,
            CursorKind::Move => CursorIcon::Move,
            CursorKind::Pointer => CursorIcon::Pointer,
            CursorKind::Text => CursorIcon::Text,
            CursorKind::Resize(h) => match h {
                Handle::N | Handle::S => CursorIcon::NsResize,
                Handle::E | Handle::W => CursorIcon::EwResize,
                Handle::NE | Handle::SW => CursorIcon::NeswResize,
                Handle::NW | Handle::SE => CursorIcon::NwseResize,
            },
        };
        if p.set_cursor(&self.conn, icon).is_ok() {
            self.cursor = Some(want);
        }
    }

    fn configure_surface(&mut self, surface: &wl_surface::WlSurface, qh: &QueueHandle<Self>) {
        if let Some(i) = self.surfs.iter().position(|s| s.role.surface() == surface) {
            self.surfs[i].configured = true;
            self.surfs[i].damage = None;
            if !self.surfs[i].frame_pending {
                self.draw(qh, i);
            }
        }
    }
}

fn map_key(ev: &KeyEvent, ctrl: bool) -> Option<Key> {
    Some(match ev.keysym {
        Keysym::Escape => Key::Escape,
        Keysym::Return | Keysym::KP_Enter => Key::Enter,
        Keysym::BackSpace => Key::Backspace,
        Keysym::Left => Key::Left,
        Keysym::Right => Key::Right,
        Keysym::Up => Key::Up,
        Keysym::Down => Key::Down,
        k => {
            // With Ctrl held, utf8 is a control character; use the keysym.
            let c = if ctrl { k.key_char() } else { ev.utf8.as_deref().and_then(|s| s.chars().next()).or_else(|| k.key_char()) }?;
            if c.is_control() {
                return None;
            }
            Key::Char(c)
        }
    })
}

// ---- handlers -------------------------------------------------------------

impl CompositorHandler for Overlay {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(i) = self.surfs.iter().position(|s| s.role.surface() == surface) {
            self.surfs[i].frame_pending = false;
            if self.surfs[i].dirty() {
                self.draw(qh, i);
            }
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.editor.outcome.get_or_insert(crate::editor::Outcome::Cancel);
    }
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        self.configure_surface(&layer.wl_surface().clone(), qh);
    }
}

impl WindowHandler for Overlay {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.editor.outcome.get_or_insert(crate::editor::Outcome::Cancel);
    }
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, window: &Window, _: WindowConfigure, _: u32) {
        self.configure_surface(&window.wl_surface().clone(), qh);
    }
}

impl SeatHandler for Overlay {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.data_device.is_none() {
            self.data_device = self.data_mgr.as_ref().map(|m| m.get_data_device(qh, &seat));
        }
    }
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if self.data_device.is_none() {
            self.data_device = self.data_mgr.as_ref().map(|m| m.get_data_device(qh, &seat));
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seats.get_keyboard(qh, &seat, None).ok();
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            let surface = self.compositor.create_surface(qh);
            self.pointer = self.seats.get_pointer_with_theme::<Self, ()>(qh, &seat, self.shm.wl_shm(), surface, ThemeSpec::default()).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if cap == Capability::Pointer {
            self.pointer = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Overlay {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, serial: u32, _: &[u32], _: &[Keysym]) {
        self.serial = serial;
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, serial: u32, ev: KeyEvent) {
        self.serial = serial;
        if let Some(k) = map_key(&ev, self.mods.ctrl) {
            if self.editor.key(k, self.mods) {
                self.mark_all();
            }
        }
    }
    fn repeat_key(&mut self, c: &Connection, qh: &QueueHandle<Self>, k: &wl_keyboard::WlKeyboard, serial: u32, ev: KeyEvent) {
        self.press_key(c, qh, k, serial, ev);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, m: Modifiers, _: RawModifiers, _: u32) {
        self.mods = Mods { ctrl: m.ctrl, shift: m.shift, alt: m.alt };
    }
}

impl PointerHandler for Overlay {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        let mut dirty = false;
        for ev in events {
            let Some(p) = self.global(&ev.surface, ev.position) else { continue };
            dirty |= match ev.kind {
                PointerEventKind::Enter { serial } => {
                    self.serial = serial;
                    self.cursor = None;
                    self.editor.pointer_motion(p, self.mods)
                }
                PointerEventKind::Leave { .. } => self.editor.pointer_leave(),
                PointerEventKind::Motion { .. } => self.editor.pointer_motion(p, self.mods),
                PointerEventKind::Press { button, serial, .. } => {
                    self.serial = serial;
                    self.editor.button_press(p, button == BTN_LEFT, self.mods)
                }
                PointerEventKind::Release { button, serial, .. } => {
                    self.serial = serial;
                    self.editor.button_release(p, button == BTN_LEFT)
                }
                PointerEventKind::Axis { vertical, .. } => {
                    let d = if vertical.discrete != 0 { vertical.discrete as f32 } else { vertical.absolute as f32 / 10.0 };
                    d != 0.0 && self.editor.scroll(d)
                }
            };
        }
        if dirty {
            self.mark_all();
        }
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl DataDeviceHandler for Overlay {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64, _: &wl_surface::WlSurface) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for Overlay {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl DataSourceHandler for Overlay {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, mime: String, mut pipe: WritePipe) {
        let Some((_, data)) = &self.clip else { return };
        if mime != "image/png" {
            return;
        }
        let data = data.clone();
        // Large PNGs fill the pipe; write off-thread so we keep answering.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        self.clip_done = true;
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

wayland_client::delegate_noop!(Overlay: WpViewporter);
wayland_client::delegate_noop!(Overlay: WpViewport);

delegate_registry!(Overlay);
impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
smithay_client_toolkit::delegate_dispatch2!(Overlay);
