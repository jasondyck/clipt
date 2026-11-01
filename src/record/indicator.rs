//! The red frame around a region being recorded: four thin layer-shell
//! strips just *outside* the region, so the frame never appears in the
//! video, with an empty input region so clicks go through to what's below.
//! Amber while paused. Not shown for whole-screen recordings, where it would
//! be in the video.

use super::{PAUSED, STOP};
use crate::capture::{LogicalRect, Output};
use anyhow::{Context, Result};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// Frame thickness in logical pixels.
const T: i32 = 3;
const RED: u32 = 0xffef_4444;
const AMBER: u32 = 0xfff5_9e0b;

struct Strip {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    w: i32,
    h: i32,
    configured: bool,
}

struct State {
    outputs: Vec<(wl_output::WlOutput, String)>,
    strips: Vec<Strip>,
}

/// The strips as (x, y, w, h) relative to the output, clipped to it. A side
/// that would fall off the output is left out.
fn strips(out: &Output, (x, y, w, h): LogicalRect) -> Vec<(i32, i32, i32, i32)> {
    let (ow, oh) = (out.width as i32, out.height as i32);
    let (l, t) = (x.floor() as i32, y.floor() as i32);
    let (r, b) = ((x + w).ceil() as i32, (y + h).ceil() as i32);
    let clip = |(sx, sy, sw, sh): (i32, i32, i32, i32)| {
        let (x0, y0) = (sx.max(0), sy.max(0));
        let (x1, y1) = ((sx + sw).min(ow), (sy + sh).min(oh));
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
    };
    [(l - T, t - T, r - l + 2 * T, T), (l - T, b, r - l + 2 * T, T), (l - T, t, T, b - t), (r, t, T, b - t)]
        .into_iter()
        .filter_map(clip)
        .collect()
}

/// Show the frame until the recording stops. Runs on its own thread with its
/// own connection; returns early (with the reason) if it can't be shown.
pub fn run(out: &Output, crop: LogicalRect) -> Result<()> {
    let rects = strips(out, crop);
    if rects.is_empty() {
        return Ok(());
    }
    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).context("no layer-shell for the recording frame")?;
    let mut state = State { outputs: Vec::new(), strips: Vec::new() };
    for g in globals.contents().clone_list() {
        if g.interface == "wl_output" && g.version >= 4 {
            let o: wl_output::WlOutput = globals.registry().bind(g.name, 4, &qh, ());
            state.outputs.push((o, String::new()));
        }
    }
    queue.roundtrip(&mut state)?;
    let wl_out = state.outputs.iter().find(|(_, n)| *n == out.name).map(|(o, _)| o.clone());

    let empty = compositor.create_region(&qh, ());
    for (x, y, w, h) in rects {
        let surface = compositor.create_surface(&qh, ());
        surface.set_input_region(Some(&empty));
        let layer = layer_shell.get_layer_surface(
            &surface,
            wl_out.as_ref(),
            zwlr_layer_shell_v1::Layer::Overlay,
            "clipt-recording".into(),
            &qh,
            (),
        );
        layer.set_anchor(Anchor::Top | Anchor::Left);
        layer.set_margin(y, 0, 0, x);
        layer.set_size(w as u32, h as u32);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        state.strips.push(Strip { surface, layer, w, h, configured: false });
    }
    empty.destroy();
    queue.roundtrip(&mut state)?;

    let mut shown: Option<u32> = None;
    let mut buffers: Vec<wl_buffer::WlBuffer> = Vec::new();
    while !STOP.load(Ordering::SeqCst) {
        let color = if PAUSED.load(Ordering::SeqCst) { AMBER } else { RED };
        if shown != Some(color) && state.strips.iter().all(|s| s.configured) {
            for b in buffers.drain(..) {
                b.destroy();
            }
            for s in &state.strips {
                let b = solid_buffer(&shm, &qh, s.w, s.h, color)?;
                s.surface.attach(Some(&b), 0, 0);
                s.surface.damage(0, 0, s.w, s.h);
                s.surface.commit();
                buffers.push(b);
            }
            shown = Some(color);
        }
        conn.flush()?;
        // Wait for events, but wake up to notice pause/resume and stop.
        if let Some(guard) = queue.prepare_read() {
            let mut fds = [libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            if unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) } > 0 {
                let _ = guard.read();
            }
        }
        queue.dispatch_pending(&mut state)?;
    }
    Ok(())
}

fn solid_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, w: i32, h: i32, argb: u32) -> Result<wl_buffer::WlBuffer> {
    let size = (w * h * 4) as usize;
    let fd: OwnedFd = rustix::fs::memfd_create("clipt-frame", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::fs::ftruncate(&fd, size as u64)?;
    let mut map = unsafe { memmap2::MmapMut::map_mut(&fd)? };
    for px in map.as_chunks_mut::<4>().0 {
        *px = argb.to_le_bytes();
    }
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let b = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Ok(b)
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(s: &mut Self, l: &ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                l.ack_configure(serial);
                if let Some(st) = s.strips.iter_mut().find(|st| st.layer == *l) {
                    st.configured = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => STOP.store(true, Ordering::SeqCst),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev
            && let Some(e) = s.outputs.iter_mut().find(|(x, _)| x == o)
        {
            e.1 = name;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

wayland_client::delegate_noop!(State: wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: wl_region::WlRegion);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn out() -> Output {
        Output { index: 0, name: "o".into(), description: String::new(), x: 0, y: 0, width: 1280, height: 720, native: (0, 0) }
    }

    #[test]
    fn frame_sits_outside_the_region() {
        let s = strips(&out(), (100.0, 50.0, 300.0, 200.0));
        assert_eq!(s, vec![(97, 47, 306, 3), (97, 250, 306, 3), (97, 50, 3, 200), (400, 50, 3, 200)]);
    }

    #[test]
    fn sides_off_the_output_are_dropped_or_clipped() {
        // region flush with the top-left corner: no top or left strip
        let s = strips(&out(), (0.0, 0.0, 100.0, 100.0));
        assert_eq!(s, vec![(0, 100, 103, 3), (100, 0, 3, 100)]);
        // the whole output: nothing to draw
        assert!(strips(&out(), (0.0, 0.0, 1280.0, 720.0)).is_empty());
    }
}
