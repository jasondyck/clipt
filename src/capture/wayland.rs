//! Wayland-side capture: output discovery (wl_output + xdg-output) and the two
//! screencopy protocols used by wlroots-family compositors (Sway, Hyprland,
//! niri, river, labwc, Wayfire, COSMIC):
//!   * ext-image-copy-capture-v1 (the standardised one, preferred)
//!   * wlr-screencopy-unstable-v1 (older, still the most widely deployed)

use super::{LogicalRect, Output, PixelOrder, Shot, VideoFrame};
use crate::image::Image;
use anyhow::{Context, Result, bail};
use std::os::fd::{AsFd, OwnedFd};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self as ext_toplevel, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self as ext_toplevel_list, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
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

/// A window from ext-foreign-toplevel-list.
struct Toplevel {
    handle: ExtForeignToplevelHandleV1,
    app_id: String,
    title: String,
    closed: bool,
}

struct State {
    toplevels: Vec<Toplevel>,
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
    /// The frame failed because the buffer no longer fits (the window
    /// resized): wait for new constraints and try again.
    retry: bool,
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

/// A connection handed in through WAYLAND_SOCKET (see
/// packaging/flatpak/clipt-launch), kept for every later user: the fd can only
/// be taken once, and reconnecting through WAYLAND_DISPLAY would get a
/// Flatpak's filtered connection instead.
static INHERITED: std::sync::OnceLock<Connection> = std::sync::OnceLock::new();

/// Connect to the compositor. Capture, overlay and clipboard each call this.
pub fn connection() -> Result<Connection> {
    if let Some(c) = INHERITED.get() {
        return Ok(c.clone());
    }
    let inherited = std::env::var_os("WAYLAND_SOCKET").is_some();
    let conn = Connection::connect_to_env().context("no Wayland display (is WAYLAND_DISPLAY set?)")?;
    if inherited {
        let _ = INHERITED.set(conn.clone());
    }
    Ok(conn)
}

impl Wayland {
    pub fn connect() -> Result<Self> {
        let conn = connection()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        if std::env::var_os("CLIPT_DEBUG").is_some() {
            // Sandboxed clients (Flatpak) may be shown fewer globals.
            let mut names: Vec<String> = globals.contents().with_list(|l| l.iter().map(|g| g.interface.clone()).collect());
            names.sort();
            names.dedup();
            eprintln!("clipt: wayland globals: {}", names.join(" "));
        }
        let qh = queue.handle();
        let mut state = State { toplevels: Vec::new(), outputs: Vec::new(), frame: FrameState::default() };
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

    /// Capture `out` continuously for a recording, handing each new frame to
    /// `sink` until it returns false. `crop` is a rectangle in the output's
    /// logical coordinates (relative to its top-left corner).
    ///
    /// Both protocols wait for the screen to change before delivering, so a
    /// still screen produces no frames; the recorder repeats the last one.
    pub fn stream(
        &mut self,
        out: &Output,
        cursor: bool,
        proto: Protocol,
        crop: &dyn Fn() -> Option<LogicalRect>,
        sink: &mut dyn FnMut(VideoFrame) -> bool,
    ) -> Result<()> {
        anyhow::ensure!(self.protocols().contains(&proto), "compositor doesn't offer {proto:?}");
        let qh = self.queue.handle();
        let shm: wl_shm::WlShm = self.globals.bind(&qh, 1..=1, ())?;
        let wl_out = self.state.outputs[out.index].0.clone();
        let transform = self.state.outputs[out.index].1.transform;
        self.state.frame = FrameState { transform, ..Default::default() };
        match proto {
            Protocol::ExtImageCopy => {
                let src_mgr: ExtOutputImageCaptureSourceManagerV1 = self.globals.bind(&qh, 1..=1, ())?;
                let source = src_mgr.create_source(&wl_out, &qh, ());
                let r = self.stream_ext(&qh, &shm, &source, Some(out), cursor, crop, sink);
                source.destroy();
                src_mgr.destroy();
                r
            }
            Protocol::WlrScreencopy => self.stream_wlr(&qh, &shm, &wl_out, out, cursor, crop, sink),
        }
    }

    /// The compositor can capture single windows (ext-foreign-toplevel image
    /// capture source), which follows a window as it moves and resizes and
    /// sees it even when other windows cover it.
    pub fn can_capture_windows(&self) -> bool {
        let has = |n: &str| self.globals.contents().with_list(|l| l.iter().any(|g| g.interface == n));
        has("ext_foreign_toplevel_list_v1")
            && has("ext_foreign_toplevel_image_capture_source_manager_v1")
            && has("ext_image_copy_capture_manager_v1")
    }

    /// Capture one window continuously, found by app id and title. Frames
    /// are the window's own size, which changes when it resizes.
    pub fn stream_window(&mut self, app_id: &str, title: &str, cursor: bool, sink: &mut dyn FnMut(VideoFrame) -> bool) -> Result<()> {
        anyhow::ensure!(self.can_capture_windows(), "this compositor can't capture single windows");
        let qh = self.queue.handle();
        let list: ExtForeignToplevelListV1 = self.globals.bind(&qh, 1..=1, ())?;
        // The list arrives as events: one roundtrip for the handles, one for
        // their details.
        self.queue.roundtrip(&mut self.state)?;
        self.queue.roundtrip(&mut self.state)?;
        let open: Vec<&Toplevel> = self.state.toplevels.iter().filter(|t| !t.closed).collect();
        let exact: Vec<&&Toplevel> = open.iter().filter(|t| t.app_id == app_id && t.title == title).collect();
        let by_app: Vec<&&Toplevel> = open.iter().filter(|t| t.app_id == app_id).collect();
        let handle = match (exact.as_slice(), by_app.as_slice()) {
            ([t, ..], _) => t.handle.clone(),
            ([], [t]) => t.handle.clone(),
            _ => bail!("can't find the window '{app_id}' ('{title}') among {} windows", open.len()),
        };
        if exact.len() > 1 {
            eprintln!("clipt: {} windows are called '{title}'; recording the first", exact.len());
        }
        let src_mgr: ExtForeignToplevelImageCaptureSourceManagerV1 = self.globals.bind(&qh, 1..=1, ())?;
        let source = src_mgr.create_source(&handle, &qh, ());
        let shm: wl_shm::WlShm = self.globals.bind(&qh, 1..=1, ())?;
        self.state.frame = FrameState::default();
        let r = self.stream_ext(&qh, &shm, &source, None, cursor, &|| None, sink);
        source.destroy();
        src_mgr.destroy();
        list.stop();
        r
    }

    #[allow(clippy::too_many_arguments)]
    /// `out` is the output being captured (None for a window), for cropping.
    fn stream_ext(
        &mut self,
        qh: &QueueHandle<State>,
        shm: &wl_shm::WlShm,
        source: &ExtImageCaptureSourceV1,
        out: Option<&Output>,
        cursor: bool,
        crop: &dyn Fn() -> Option<LogicalRect>,
        sink: &mut dyn FnMut(VideoFrame) -> bool,
    ) -> Result<()> {
        let mgr: ExtImageCopyCaptureManagerV1 = self.globals.bind(qh, 1..=1, ())?;
        let opts = if cursor { ext_mgr::Options::PaintCursors } else { ext_mgr::Options::empty() };
        let session = mgr.create_session(source, opts, qh, ());
        let mut buf: Option<ShmBuffer> = None;
        let result = loop {
            if let Err(e) = self.dispatch_until(|f| f.constraints_done) {
                break Err(e);
            }
            let (w, h) = self.state.frame.size.context("no buffer size from session")?;
            // A new buffer starts out stale everywhere; after that the
            // compositor only rewrites what changed, since we never touch it.
            let fresh = buf.as_ref().is_none_or(|b| (b.width, b.height) != (w, h));
            if fresh {
                let formats: Vec<_> = self.state.frame.formats.iter().map(|&(f, ..)| (f, w, h, 0)).collect();
                let (fmt, w, h, stride) = pick_format(&formats).context("no usable shm format offered")?;
                if let Some(old) = buf.take() {
                    old.buffer.destroy();
                }
                buf = Some(ShmBuffer::new(shm, qh, w, h, stride, fmt)?);
            }
            let b = buf.as_ref().unwrap();
            let frame = session.create_frame(qh, ());
            frame.attach_buffer(&b.buffer);
            if fresh {
                frame.damage_buffer(0, 0, w as i32, h as i32);
            }
            frame.capture();
            self.state.frame.ready = false;
            let r = self.dispatch_until(|f| f.ready);
            frame.destroy();
            if r.is_err() && self.state.frame.retry {
                // Resized: new constraints are on their way; go round again.
                self.state.frame.failed = None;
                self.state.frame.retry = false;
                continue;
            }
            if let Err(e) = r {
                break Err(e);
            }
            let vf = b.video_frame(false, self.state.frame.transform, out, crop())?;
            if !sink(vf) {
                break Ok(());
            }
        };
        if let Some(b) = buf {
            b.buffer.destroy();
        }
        session.destroy();
        mgr.destroy();
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_wlr(
        &mut self,
        qh: &QueueHandle<State>,
        shm: &wl_shm::WlShm,
        wl_out: &wl_output::WlOutput,
        out: &Output,
        cursor: bool,
        crop: &dyn Fn() -> Option<LogicalRect>,
        sink: &mut dyn FnMut(VideoFrame) -> bool,
    ) -> Result<()> {
        let mgr: ZwlrScreencopyManagerV1 = self.globals.bind(qh, 1..=3, ())?;
        let v = mgr.version();
        let transform = self.state.frame.transform;
        let mut buf: Option<ShmBuffer> = None;
        let result = loop {
            // Each wlr frame is a new object that re-announces its buffer.
            self.state.frame = FrameState { transform, ..Default::default() };
            let frame = mgr.capture_output(cursor as i32, wl_out, qh, ());
            if let Err(e) = self.dispatch_until(|f| if v >= 3 { f.constraints_done } else { !f.formats.is_empty() }) {
                frame.destroy();
                break Err(e);
            }
            let (fmt, w, h, stride) = pick_format(&self.state.frame.formats).context("no usable shm format offered")?;
            let reuse = buf.as_ref().is_some_and(|b| (b.width, b.height, b.stride, b.format) == (w, h, stride, fmt));
            if !reuse {
                if let Some(old) = buf.take() {
                    old.buffer.destroy();
                }
                buf = Some(ShmBuffer::new(shm, qh, w, h, stride, fmt)?);
            }
            let b = buf.as_ref().unwrap();
            // copy_with_damage (v2+) waits for the screen to change, so a still
            // screen doesn't make us spin. The first copy must not wait.
            if v >= 2 && reuse {
                frame.copy_with_damage(&b.buffer);
            } else {
                frame.copy(&b.buffer);
            }
            let r = self.dispatch_until(|f| f.ready);
            frame.destroy();
            if let Err(e) = r {
                break Err(e);
            }
            let vf = b.video_frame(self.state.frame.y_invert, self.state.frame.transform, Some(out), crop())?;
            if !sink(vf) {
                break Ok(());
            }
        };
        if let Some(b) = buf {
            b.buffer.destroy();
        }
        mgr.destroy();
        result
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
        let img = self.image();
        self.buffer.destroy();
        img
    }

    fn image(&self) -> Result<Image> {
        use wl_shm::Format::*;
        let (w, h, s) = (self.width, self.height, self.stride as usize);
        match self.format {
            Xrgb8888 => Image::from_rows(w, h, s, 4, &self.map, |p| [p[2], p[1], p[0], 255]),
            Argb8888 => Image::from_rows(w, h, s, 4, &self.map, |p| [p[2], p[1], p[0], 255]),
            Xbgr8888 | Abgr8888 => Image::from_rows(w, h, s, 4, &self.map, |p| [p[0], p[1], p[2], 255]),
            f => bail!("unsupported shm format {f:?}"),
        }
    }

    /// Copy the buffer (or the `crop` part of it, in output-logical
    /// coordinates) out as a video frame. Upright outputs are a row copy in
    /// the buffer's own byte order; rotated ones go through `Image`.
    fn video_frame(
        &self,
        y_invert: bool,
        transform: Option<wl_output::Transform>,
        out: Option<&Output>,
        crop: Option<LogicalRect>,
    ) -> Result<VideoFrame> {
        use wl_shm::Format::*;
        let upright = matches!(transform, None | Some(wl_output::Transform::Normal));
        if !upright {
            let mut img = self.image()?;
            if y_invert {
                img = img.flip_vertical();
            }
            img = apply_transform(img, transform.unwrap());
            let (x, y, w, h) = crop_rect(img.width, img.height, out, crop);
            let img = if (x, y, w, h) == (0, 0, img.width, img.height) { img } else { img.crop(x, y, w, h) };
            return Ok(VideoFrame { width: img.width, height: img.height, order: PixelOrder::Rgbx, data: img.data });
        }
        let order = match self.format {
            Xrgb8888 | Argb8888 => PixelOrder::Bgrx,
            Xbgr8888 | Abgr8888 => PixelOrder::Rgbx,
            f => bail!("unsupported shm format {f:?}"),
        };
        let (x, y, w, h) = crop_rect(self.width, self.height, out, crop);
        let (stride, row) = (self.stride as usize, w as usize * 4);
        let mut data = Vec::with_capacity(row * h as usize);
        for r in 0..h {
            let sy = if y_invert { self.height - 1 - (y + r) } else { y + r } as usize;
            let start = sy * stride + x as usize * 4;
            data.extend_from_slice(&self.map[start..start + row]);
        }
        Ok(VideoFrame { width: w, height: h, order, data })
    }
}

/// `crop` (output-logical, relative to the output) in buffer pixels of a
/// buffer `bw` x `bh` that shows the whole output upright. Rounds like
/// screenshot crops do and never leaves the buffer.
fn crop_rect(bw: u32, bh: u32, out: Option<&Output>, crop: Option<LogicalRect>) -> (u32, u32, u32, u32) {
    let (Some(out), Some((x, y, w, h))) = (out, crop) else { return (0, 0, bw, bh) };
    let scale = bw as f64 / out.width.max(1) as f64;
    let r = |v: f64| (v * scale).round().max(0.0) as u32;
    let (x, y) = (r(x).min(bw - 1), r(y).min(bh - 1));
    (x, y, r(w).clamp(1, bw - x), r(h).clamp(1, bh - y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(w: u32, h: u32) -> Output {
        Output { index: 0, name: "o".into(), description: String::new(), x: 0, y: 0, width: w, height: h, native: (0, 0) }
    }

    #[test]
    fn crop_rect_scales_and_clamps() {
        let o = output(1280, 720);
        assert_eq!(crop_rect(2560, 1440, Some(&o), None), (0, 0, 2560, 1440));
        assert_eq!(crop_rect(2560, 1440, Some(&o), Some((100.0, 50.0, 300.0, 200.0))), (200, 100, 600, 400));
        // 1.5x fractional scale rounds like the screenshot crop
        assert_eq!(crop_rect(1920, 1080, Some(&o), Some((101.0, 0.0, 33.0, 33.0))), (152, 0, 50, 50));
        // never past the buffer's edge
        assert_eq!(crop_rect(1280, 720, Some(&o), Some((1200.0, 700.0, 500.0, 500.0))), (1200, 700, 80, 20));
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
            ext_session::Event::BufferSize { width, height } => {
                // A new batch of constraints (they change when a window resizes).
                f.size = Some((width, height));
                f.formats.clear();
                f.constraints_done = false;
            }
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
            ext_frame::Event::Failed { reason } => {
                f.retry = reason == WEnum::Value(ext_frame::FailureReason::BufferConstraints);
                f.failed = Some(format!("{reason:?}"));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(s: &mut Self, _: &ExtForeignToplevelListV1, ev: ext_toplevel_list::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_toplevel_list::Event::Toplevel { toplevel } = ev {
            s.toplevels.push(Toplevel { handle: toplevel, app_id: String::new(), title: String::new(), closed: false });
        }
    }

    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [
        ext_toplevel_list::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(s: &mut Self, h: &ExtForeignToplevelHandleV1, ev: ext_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(t) = s.toplevels.iter_mut().find(|t| t.handle == *h) else { return };
        match ev {
            ext_toplevel::Event::AppId { app_id } => t.app_id = app_id,
            ext_toplevel::Event::Title { title } => t.title = title,
            ext_toplevel::Event::Closed => t.closed = true,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ExtForeignToplevelImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(State: ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(State: ExtImageCaptureSourceV1);
