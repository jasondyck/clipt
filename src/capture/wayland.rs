//! Wayland-side capture: output discovery (wl_output + xdg-output) and the two
//! screencopy protocols used by wlroots-family compositors (Sway, Hyprland,
//! niri, river, labwc, Wayfire, COSMIC):
//!   * ext-image-copy-capture-v1 (the standardised one, preferred)
//!   * wlr-screencopy-unstable-v1 (older, still the most widely deployed)

use super::{Output, Shot};
use crate::image::Image;
use anyhow::{Context, Result, bail};
use std::os::fd::{AsFd, OwnedFd};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1, ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self as ext_frame, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self as ext_mgr, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self as ext_session, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self as wlr_frame, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

#[derive(Default, Debug)]
struct OutInfo {
    name: String,
    description: String,
    pos: (i32, i32),
    mode: (i32, i32),
    scale: i32,
    transform: Option<wl_output::Transform>,
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

struct State {
    outputs: Vec<(wl_output::WlOutput, OutInfo)>,
    frame: FrameState,
}

#[derive(Default)]
struct FrameState {
    formats: Vec<(wl_shm::Format, u32, u32, u32)>, // format, w, h, stride (stride 0 = derive)
    size: Option<(u32, u32)>,
    constraints_done: bool,
    y_invert: bool,
    transform: Option<wl_output::Transform>,
    ready: bool,
    failed: Option<String>,
}

pub struct Wayland {
    conn: Connection,
    globals: GlobalList,
    queue: EventQueue<State>,
    state: State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    ExtImageCopy,
    WlrScreencopy,
}

impl Wayland {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("no Wayland display (is WAYLAND_DISPLAY set?)")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let mut state = State { outputs: Vec::new(), frame: FrameState::default() };
        for g in globals.contents().clone_list() {
            if g.interface == "wl_output" {
                let o: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, ());
                state.outputs.push((o, OutInfo { scale: 1, ..Default::default() }));
            }
        }
        queue.roundtrip(&mut state)?;
        if let Ok(xm) = globals.bind::<ZxdgOutputManagerV1, _, _>(&qh, 2..=3, ()) {
            for (i, (o, _)) in state.outputs.iter().enumerate() {
                xm.get_xdg_output(o, &qh, i);
            }
            queue.roundtrip(&mut state)?;
        }
        Ok(Self { conn, globals, queue, state })
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.state
            .outputs
            .iter()
            .enumerate()
            .map(|(i, (_, o))| {
                let scale = o.scale.max(1);
                let rotated = matches!(
                    o.transform,
                    Some(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
                let mode = if rotated { (o.mode.1, o.mode.0) } else { o.mode };
                let (lw, lh) = o.logical_size.unwrap_or((mode.0 / scale, mode.1 / scale));
                let (lx, ly) = o.logical_pos.unwrap_or(o.pos);
                Output {
                    index: i,
                    name: if o.name.is_empty() { format!("output-{i}") } else { o.name.clone() },
                    description: o.description.clone(),
                    x: lx,
                    y: ly,
                    width: lw.max(1) as u32,
                    height: lh.max(1) as u32,
                    native: (mode.0.max(0) as u32, mode.1.max(0) as u32),
                }
            })
            .collect()
    }

    /// Screencopy protocols the compositor offers, preferred first.
    pub fn protocols(&self) -> Vec<Protocol> {
        let has = |n: &str| self.globals.contents().with_list(|l| l.iter().any(|g| g.interface == n));
        let mut v = Vec::new();
        if has("ext_image_copy_capture_manager_v1") && has("ext_output_image_capture_source_manager_v1") {
            v.push(Protocol::ExtImageCopy);
        }
        if has("zwlr_screencopy_manager_v1") {
            v.push(Protocol::WlrScreencopy);
        }
        v
    }

    pub fn capture(&mut self, outputs: &[Output], cursor: bool, proto: Protocol) -> Result<Vec<Shot>> {
        anyhow::ensure!(self.protocols().contains(&proto), "compositor doesn't offer {proto:?}");
        let qh = self.queue.handle();
        let shm: wl_shm::WlShm = self.globals.bind(&qh, 1..=1, ())?;
        let mut shots = Vec::new();
        for out in outputs {
            let wl_out = self.state.outputs[out.index].0.clone();
            let transform = self.state.outputs[out.index].1.transform;
            self.state.frame = FrameState { transform, ..Default::default() };
            let raw = match proto {
                Protocol::ExtImageCopy => self.capture_ext(&qh, &shm, &wl_out, cursor)?,
                Protocol::WlrScreencopy => self.capture_wlr(&qh, &shm, &wl_out, cursor)?,
            };
            let mut img = raw;
            if self.state.frame.y_invert {
                img = img.flip_vertical();
            }
            if let Some(t) = self.state.frame.transform {
                img = apply_transform(img, t);
            }
            shots.push(Shot { output: out.clone(), image: img });
        }
        Ok(shots)
    }

    fn dispatch_until(&mut self, mut done: impl FnMut(&FrameState) -> bool) -> Result<()> {
        while !done(&self.state.frame) {
            if let Some(e) = &self.state.frame.failed {
                bail!("capture failed: {e}");
            }
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        if let Some(e) = &self.state.frame.failed {
            bail!("capture failed: {e}");
        }
        Ok(())
    }

    fn capture_wlr(&mut self, qh: &QueueHandle<State>, shm: &wl_shm::WlShm, out: &wl_output::WlOutput, cursor: bool) -> Result<Image> {
        let mgr: ZwlrScreencopyManagerV1 = self.globals.bind(qh, 1..=3, ())?;
        let frame = mgr.capture_output(cursor as i32, out, qh, ());
        let v3 = mgr.version() >= 3;
        // v1/v2 have no buffer_done; the first buffer event is all we get.
        self.dispatch_until(|f| if v3 { f.constraints_done } else { !f.formats.is_empty() })?;
        let (fmt, w, h, stride) = pick_format(&self.state.frame.formats).context("no usable shm format offered")?;
        let buf = ShmBuffer::new(shm, qh, w, h, stride, fmt)?;
        frame.copy(&buf.buffer);
        self.dispatch_until(|f| f.ready)?;
        frame.destroy();
        mgr.destroy();
        buf.into_image()
    }

    fn capture_ext(&mut self, qh: &QueueHandle<State>, shm: &wl_shm::WlShm, out: &wl_output::WlOutput, cursor: bool) -> Result<Image> {
        let src_mgr: ExtOutputImageCaptureSourceManagerV1 = self.globals.bind(qh, 1..=1, ())?;
        let mgr: ExtImageCopyCaptureManagerV1 = self.globals.bind(qh, 1..=1, ())?;
        let source = src_mgr.create_source(out, qh, ());
        let opts = if cursor { ext_mgr::Options::PaintCursors } else { ext_mgr::Options::empty() };
        let session = mgr.create_session(&source, opts, qh, ());
        self.dispatch_until(|f| f.constraints_done)?;
        let (w, h) = self.state.frame.size.context("no buffer size from session")?;
        let formats: Vec<_> = self.state.frame.formats.iter().map(|&(f, ..)| (f, w, h, 0)).collect();
        let (fmt, w, h, stride) = pick_format(&formats).context("no usable shm format offered")?;
        let buf = ShmBuffer::new(shm, qh, w, h, stride, fmt)?;
        let frame = session.create_frame(qh, ());
        frame.attach_buffer(&buf.buffer);
        frame.damage_buffer(0, 0, w as i32, h as i32);
        frame.capture();
        self.dispatch_until(|f| f.ready)?;
        frame.destroy();
        session.destroy();
        source.destroy();
        mgr.destroy();
        src_mgr.destroy();
        buf.into_image()
    }

    #[allow(dead_code)]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Prefer plain 8-bit formats we can convert cheaply.
fn pick_format(fs: &[(wl_shm::Format, u32, u32, u32)]) -> Option<(wl_shm::Format, u32, u32, u32)> {
    use wl_shm::Format::*;
    for want in [Xrgb8888, Argb8888, Xbgr8888, Abgr8888] {
        if let Some(&(f, w, h, s)) = fs.iter().find(|x| x.0 == want) {
            return Some((f, w, h, if s == 0 { w * 4 } else { s }));
        }
    }
    None
}

struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    map: memmap2::MmapMut,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
}

impl ShmBuffer {
    fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, w: u32, h: u32, stride: u32, format: wl_shm::Format) -> Result<Self> {
        let size = stride as usize * h as usize;
        let fd: OwnedFd = rustix::fs::memfd_create("clipt-capture", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, size as u64)?;
        let map = unsafe { memmap2::MmapMut::map_mut(&fd)? };
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, ());
        pool.destroy();
        Ok(Self { buffer, map, width: w, height: h, stride, format })
    }

    fn into_image(self) -> Result<Image> {
        use wl_shm::Format::*;
        let (w, h, s) = (self.width, self.height, self.stride as usize);
        let img = match self.format {
            Xrgb8888 => Image::from_rows(w, h, s, 4, &self.map, |p| [p[2], p[1], p[0], 255]),
            Argb8888 => Image::from_rows(w, h, s, 4, &self.map, |p| [p[2], p[1], p[0], 255]),
            Xbgr8888 | Abgr8888 => Image::from_rows(w, h, s, 4, &self.map, |p| [p[0], p[1], p[2], 255]),
            f => bail!("unsupported shm format {f:?}"),
        };
        self.buffer.destroy();
        img
    }
}

fn apply_transform(img: Image, t: wl_output::Transform) -> Image {
    use wl_output::Transform as T;
    // The buffer is in the output's native orientation; rotate it into the
    // orientation the user sees. Verified against grim for all eight
    // transforms on both protocols (tests/sway, "triple" layout).
    match t {
        T::Normal => img,
        T::_90 => img.rotate(90),
        T::_180 => img.rotate(180),
        T::_270 => img.rotate(270),
        T::Flipped => img.flip_horizontal(),
        // Mirroring first reverses the sense of the rotation that follows.
        T::Flipped90 => img.flip_horizontal().rotate(270),
        T::Flipped180 => img.flip_horizontal().rotate(180),
        T::Flipped270 => img.flip_horizontal().rotate(90),
        _ => img,
    }
}

// ---- dispatch -------------------------------------------------------------

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

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some((_, info)) = s.outputs.iter_mut().find(|(x, _)| x == o) else { return };
        match ev {
            wl_output::Event::Geometry { x, y, transform, .. } => {
                info.pos = (x, y);
                if let WEnum::Value(t) = transform {
                    info.transform = Some(t);
                }
            }
            wl_output::Event::Mode { flags, width, height, .. } => {
                if let WEnum::Value(f) = flags
                    && f.contains(wl_output::Mode::Current)
                {
                    info.mode = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => info.scale = factor,
            wl_output::Event::Name { name } => info.name = name,
            wl_output::Event::Description { description } => info.description = description,
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(s: &mut Self, _: &ZxdgOutputV1, ev: zxdg_output_v1::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let info = &mut s.outputs[*idx].1;
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => info.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => info.logical_size = Some((width, height)),
            zxdg_output_v1::Event::Name { name } if info.name.is_empty() => info.name = name,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(s: &mut Self, _: &ZwlrScreencopyFrameV1, ev: wlr_frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let f = &mut s.frame;
        match ev {
            wlr_frame::Event::Buffer { format: WEnum::Value(fmt), width, height, stride } => f.formats.push((fmt, width, height, stride)),
            wlr_frame::Event::BufferDone => f.constraints_done = true,
            wlr_frame::Event::Flags { flags: WEnum::Value(fl) } => f.y_invert = fl.contains(wlr_frame::Flags::YInvert),
            wlr_frame::Event::Ready { .. } => f.ready = true,
            wlr_frame::Event::Failed => f.failed = Some("compositor refused the copy".into()),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(s: &mut Self, _: &ExtImageCopyCaptureSessionV1, ev: ext_session::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let f = &mut s.frame;
        match ev {
            ext_session::Event::BufferSize { width, height } => f.size = Some((width, height)),
            ext_session::Event::ShmFormat { format: WEnum::Value(fmt) } => f.formats.push((fmt, 0, 0, 0)),
            ext_session::Event::Done => f.constraints_done = true,
            ext_session::Event::Stopped => f.failed = Some("session stopped".into()),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(s: &mut Self, _: &ExtImageCopyCaptureFrameV1, ev: ext_frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let f = &mut s.frame;
        match ev {
            ext_frame::Event::Transform { transform: WEnum::Value(t) } => f.transform = Some(t),
            ext_frame::Event::Ready => f.ready = true,
            ext_frame::Event::Failed { reason } => f.failed = Some(format!("{reason:?}")),
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(State: ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(State: ExtImageCaptureSourceV1);
