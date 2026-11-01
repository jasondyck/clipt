//! Test window for end-to-end tests: an xdg_toplevel filled with one colour
//! (and a black 4 px border) that redraws at whatever size the compositor
//! configures, so tests can resize it while recording.
//!
//!   testwin [RRGGBB] [W H]      (default c020c0 400 300, app_id clipt-testwin)

use std::os::fd::AsFd;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct S {
    color: u32,
    size: (i32, i32),
    pending: (i32, i32),
    configured: Option<u32>,
    closed: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let color = args.get(1).and_then(|c| u32::from_str_radix(c, 16).ok()).unwrap_or(0xc020c0);
    let w = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(400);
    let h = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(300);
    let conn = Connection::connect_to_env().expect("wayland");
    let (globals, mut q) = registry_queue_init::<S>(&conn).unwrap();
    let qh = q.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=2, ()).unwrap();
    let surface = compositor.create_surface(&qh, ());
    let xs = wm.get_xdg_surface(&surface, &qh, ());
    let top = xs.get_toplevel(&qh, ());
    top.set_app_id("clipt-testwin".into());
    top.set_title("testwin".into());
    surface.commit();
    let mut s = S { color: 0xff00_0000 | color, size: (w, h), pending: (0, 0), configured: None, closed: false };
    let mut buffer: Option<wl_buffer::WlBuffer> = None;
    while !s.closed {
        q.blocking_dispatch(&mut s).unwrap();
        if let Some(serial) = s.configured.take() {
            xs.ack_configure(serial);
            if s.pending.0 > 0 && s.pending.1 > 0 {
                s.size = s.pending;
            }
            if let Some(b) = buffer.take() {
                b.destroy();
            }
            let (w, h) = s.size;
            let b = draw(&shm, &qh, w, h, s.color);
            surface.attach(Some(&b), 0, 0);
            surface.damage_buffer(0, 0, w, h);
            surface.commit();
            buffer = Some(b);
        }
    }
}

fn draw(shm: &wl_shm::WlShm, qh: &QueueHandle<S>, w: i32, h: i32, color: u32) -> wl_buffer::WlBuffer {
    let size = (w * h * 4) as usize;
    let fd = rustix::fs::memfd_create("testwin", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    rustix::fs::ftruncate(&fd, size as u64).unwrap();
    let mut map = unsafe { memmap2::MmapMut::map_mut(&fd).unwrap() };
    for (i, px) in map.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i as i32 % w, i as i32 / w);
        let border = x < 4 || y < 4 || x >= w - 4 || y >= h - 4;
        *px = if border { 0xff00_0000u32 } else { color }.to_le_bytes();
    }
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let b = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    b
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for S {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for S {
    fn event(s: &mut Self, _: &xdg_surface::XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            s.configured = Some(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for S {
    fn event(s: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } => s.pending = (width, height),
            xdg_toplevel::Event::Close => s.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for S {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(S: wl_compositor::WlCompositor);
wayland_client::delegate_noop!(S: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(S: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(S: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(S: ignore wl_buffer::WlBuffer);
