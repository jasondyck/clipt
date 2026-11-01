//! Frames from a PipeWire screencast stream: what KDE (zkde_screencast),
//! GNOME (Mutter's ScreenCast) and the ScreenCast portal hand out instead of
//! a Wayland capture protocol.
//!
//! libpipewire is loaded at runtime (dlopen), only when a recording needs
//! it: linking it would stop clipt from starting at all, screenshots
//! included, on systems without it. That means a small hand-written FFI for
//! the dozen functions used here, and a minimal SPA pod encoder/parser for
//! the format negotiation (the pod layout is PipeWire's stable wire format).
//!
//! Only shared-memory buffers are asked for (no DMA-BUF modifiers are
//! offered), so frames can be read directly and copied out like the
//! wlroots ones; compositors fall back to memfd buffers for that.

use super::{PixelOrder, VideoFrame};
use anyhow::{Result, bail};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::OnceLock;

/// Where to find the stream: a PipeWire node, on the default daemon or on a
/// connection the portal handed out.
pub struct Remote {
    pub fd: Option<OwnedFd>,
    pub node: u32,
}

/// A crop in buffer pixels for a frame of the given size.
pub type CropFn = Box<dyn Fn(u32, u32) -> Option<(u32, u32, u32, u32)>>;

// ---- libpipewire, loaded at runtime ----------------------------------------

type P = *mut c_void;

struct Lib {
    init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    main_loop_new: unsafe extern "C" fn(P) -> P,
    main_loop_get_loop: unsafe extern "C" fn(P) -> P,
    main_loop_run: unsafe extern "C" fn(P) -> c_int,
    main_loop_quit: unsafe extern "C" fn(P) -> c_int,
    main_loop_destroy: unsafe extern "C" fn(P),
    context_new: unsafe extern "C" fn(P, P, usize) -> P,
    context_connect: unsafe extern "C" fn(P, P, usize) -> P,
    context_connect_fd: unsafe extern "C" fn(P, c_int, P, usize) -> P,
    context_destroy: unsafe extern "C" fn(P),
    core_disconnect: unsafe extern "C" fn(P) -> c_int,
    properties_new_string: unsafe extern "C" fn(*const c_char) -> P,
    stream_new: unsafe extern "C" fn(P, *const c_char, P) -> P,
    stream_add_listener: unsafe extern "C" fn(P, *mut SpaHook, *const StreamEvents, P),
    stream_connect: unsafe extern "C" fn(P, u32, u32, u32, *const *const u8, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(P) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(P, *mut PwBuffer) -> c_int,
    stream_destroy: unsafe extern "C" fn(P),
}

fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load).as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

/// A symbol address as the function pointer type `F`.
///
/// # Safety
/// `p` must be a function whose signature is `F`.
unsafe fn fn_ptr<F: Copy>(p: *mut c_void) -> F {
    assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
    unsafe { std::mem::transmute_copy(&p) }
}

fn load() -> Result<Lib, String> {
    let h = unsafe { libc::dlopen(c"libpipewire-0.3.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        return Err("recording on this desktop needs PipeWire (libpipewire-0.3.so.0 isn't installed)".into());
    }
    macro_rules! sym {
        ($name:literal) => {{
            let p = unsafe { libc::dlsym(h, concat!($name, "\0").as_ptr().cast()) };
            if p.is_null() {
                return Err(format!("libpipewire has no {}", $name));
            }
            // SAFETY: the symbol is the C function named, whose signature
            // matches the field it's stored in (from pipewire 0.3's headers).
            unsafe { fn_ptr(p) }
        }};
    }
    Ok(Lib {
        init: sym!("pw_init"),
        main_loop_new: sym!("pw_main_loop_new"),
        main_loop_get_loop: sym!("pw_main_loop_get_loop"),
        main_loop_run: sym!("pw_main_loop_run"),
        main_loop_quit: sym!("pw_main_loop_quit"),
        main_loop_destroy: sym!("pw_main_loop_destroy"),
        context_new: sym!("pw_context_new"),
        context_connect: sym!("pw_context_connect"),
        context_connect_fd: sym!("pw_context_connect_fd"),
        context_destroy: sym!("pw_context_destroy"),
        core_disconnect: sym!("pw_core_disconnect"),
        properties_new_string: sym!("pw_properties_new_string"),
        stream_new: sym!("pw_stream_new"),
        stream_add_listener: sym!("pw_stream_add_listener"),
        stream_connect: sym!("pw_stream_connect"),
        stream_dequeue_buffer: sym!("pw_stream_dequeue_buffer"),
        stream_queue_buffer: sym!("pw_stream_queue_buffer"),
        stream_destroy: sym!("pw_stream_destroy"),
    })
}

/// Fails, saying so, if PipeWire isn't installed (the library won't load).
pub fn check() -> Result<()> {
    lib().map(drop)
}

// Layouts from pipewire-0.3 / spa-0.2 (checked against the headers: hook 48
// bytes, events 96, spa_data 40 with data at 24 and chunk at 32).

#[repr(C)]
struct SpaHook([usize; 6]);

#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(P)>,
    state_changed: Option<unsafe extern "C" fn(P, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(P, u32, P)>,
    io_changed: Option<unsafe extern "C" fn(P, u32, P, u32)>,
    param_changed: Option<unsafe extern "C" fn(P, u32, *const u8)>,
    add_buffer: Option<unsafe extern "C" fn(P, P)>,
    remove_buffer: Option<unsafe extern "C" fn(P, P)>,
    process: Option<unsafe extern "C" fn(P)>,
    drained: Option<unsafe extern "C" fn(P)>,
    command: Option<unsafe extern "C" fn(P, P)>,
    trigger_done: Option<unsafe extern "C" fn(P)>,
}

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: P,
    size: u64,
    requested: u64,
    time: u64,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: P,
    datas: *mut SpaData,
}

#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut u8,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

const PW_STREAM_STATE_ERROR: c_int = -1;
const PW_STREAM_STATE_UNCONNECTED: c_int = 0;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
const SPA_DIRECTION_INPUT: u32 = 0;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;

// ---- SPA pods ----------------------------------------------------------------

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_CHOICE: u32 = 19;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
const SPA_CHOICE_RANGE: u32 = 1;
const SPA_CHOICE_ENUM: u32 = 3;
const VIDEO_RGBX: u32 = 7;
const VIDEO_BGRX: u32 = 8;
const VIDEO_RGBA: u32 = 11;
const VIDEO_BGRA: u32 = 12;

fn order_of(format: u32) -> Option<PixelOrder> {
    match format {
        VIDEO_BGRX | VIDEO_BGRA => Some(PixelOrder::Bgrx),
        VIDEO_RGBX | VIDEO_RGBA => Some(PixelOrder::Rgbx),
        _ => None,
    }
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

/// A pod: size and type, the body, then padding to 8 bytes.
fn pod(kind: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len() + 7);
    v.extend((body.len() as u32).to_ne_bytes());
    v.extend(kind.to_ne_bytes());
    v.extend_from_slice(body);
    pad8(&mut v);
    v
}

fn u32s(xs: &[u32]) -> Vec<u8> {
    xs.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

/// A Choice pod over values of one type, each `child_size` bytes.
fn choice(kind: u32, child_type: u32, child_size: u32, values: &[u32]) -> Vec<u8> {
    let mut body = u32s(&[kind, 0, child_size, child_type]);
    body.extend(u32s(values));
    pod(SPA_TYPE_CHOICE, &body)
}

/// The EnumFormat we offer: raw video, 32-bit RGB in either byte order,
/// any size and frame rate.
pub(crate) fn enum_format() -> Vec<u8> {
    let prop = |key: u32, value: Vec<u8>| {
        let mut v = u32s(&[key, 0]);
        v.extend(value);
        v
    };
    let mut body = u32s(&[SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT]);
    body.extend(prop(SPA_FORMAT_MEDIA_TYPE, pod(SPA_TYPE_ID, &u32s(&[SPA_MEDIA_TYPE_VIDEO]))));
    body.extend(prop(SPA_FORMAT_MEDIA_SUBTYPE, pod(SPA_TYPE_ID, &u32s(&[SPA_MEDIA_SUBTYPE_RAW]))));
    body.extend(prop(
        SPA_FORMAT_VIDEO_FORMAT,
        choice(SPA_CHOICE_ENUM, SPA_TYPE_ID, 4, &[VIDEO_BGRX, VIDEO_BGRX, VIDEO_BGRA, VIDEO_RGBX, VIDEO_RGBA]),
    ));
    body.extend(prop(SPA_FORMAT_VIDEO_SIZE, choice(SPA_CHOICE_RANGE, SPA_TYPE_RECTANGLE, 8, &[1920, 1080, 1, 1, 16384, 16384])));
    body.extend(prop(SPA_FORMAT_VIDEO_FRAMERATE, choice(SPA_CHOICE_RANGE, SPA_TYPE_FRACTION, 8, &[60, 1, 0, 1, 1000, 1])));
    pod(SPA_TYPE_OBJECT, &body)
}

/// The negotiated format: pixel layout and size. `bytes` is the whole pod.
pub(crate) fn parse_format(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    let rd = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_ne_bytes(b.try_into().unwrap()));
    let (size, kind) = (rd(0)? as usize, rd(4)?);
    if kind != SPA_TYPE_OBJECT || bytes.len() < 8 + size {
        return None;
    }
    let end = 8 + size;
    let (mut format, mut dims) = (None, None);
    let mut o = 16; // after the object's type and id
    while o + 16 <= end {
        let (key, vsize, vtype) = (rd(o)?, rd(o + 8)? as usize, rd(o + 12)?);
        let mut body = o + 16;
        let mut btype = vtype;
        // A fixed value may still come wrapped in a Choice of kind None.
        if vtype == SPA_TYPE_CHOICE {
            btype = rd(body + 12)?;
            body += 16;
        }
        match (key, btype) {
            (SPA_FORMAT_VIDEO_FORMAT, SPA_TYPE_ID) => format = rd(body),
            (SPA_FORMAT_VIDEO_SIZE, SPA_TYPE_RECTANGLE) => dims = Some((rd(body)?, rd(body + 4)?)),
            _ => {}
        }
        o += 16 + vsize.div_ceil(8) * 8;
    }
    let (w, h) = dims?;
    Some((format?, w, h))
}

// ---- the stream ------------------------------------------------------------------

struct Ctx {
    lib: &'static Lib,
    main_loop: P,
    stream: P,
    width: u32,
    height: u32,
    order: Option<PixelOrder>,
    crop: CropFn,
    sink: Box<dyn FnMut(VideoFrame) -> bool>,
    error: Option<String>,
}

unsafe extern "C" fn on_state(data: P, _old: c_int, state: c_int, error: *const c_char) {
    let ctx = unsafe { &mut *(data as *mut Ctx) };
    if state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED && ctx.error.is_none() {
        let msg = if error.is_null() { "stream ended".into() } else { unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned() };
        if state == PW_STREAM_STATE_ERROR {
            ctx.error = Some(msg);
            unsafe { (ctx.lib.main_loop_quit)(ctx.main_loop) };
        }
    }
}

unsafe extern "C" fn on_param(data: P, id: u32, param: *const u8) {
    let ctx = unsafe { &mut *(data as *mut Ctx) };
    if id != SPA_PARAM_FORMAT || param.is_null() {
        return;
    }
    let size = unsafe { std::ptr::read_unaligned(param as *const u32) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(param, 8 + size) };
    if let Some((format, w, h)) = parse_format(bytes) {
        ctx.width = w;
        ctx.height = h;
        ctx.order = order_of(format);
        debug(&format!("PipeWire format {format} {w}x{h}"));
    }
}

unsafe extern "C" fn on_process(data: P) {
    let ctx = unsafe { &mut *(data as *mut Ctx) };
    let b = unsafe { (ctx.lib.stream_dequeue_buffer)(ctx.stream) };
    if b.is_null() {
        return;
    }
    let keep = unsafe { read_frame(ctx, &*b) };
    unsafe { (ctx.lib.stream_queue_buffer)(ctx.stream, b) };
    if !keep {
        unsafe { (ctx.lib.main_loop_quit)(ctx.main_loop) };
    }
}

/// Copy a buffer's picture out and hand it to the sink; false to stop.
unsafe fn read_frame(ctx: &mut Ctx, b: &PwBuffer) -> bool {
    let Some(order) = ctx.order else { return true };
    let sb = unsafe { &*b.buffer };
    if sb.n_datas == 0 || sb.datas.is_null() {
        return true;
    }
    let d = unsafe { &*sb.datas };
    if d.data.is_null() || d.chunk.is_null() {
        if ctx.error.is_none() {
            ctx.error = Some("the compositor sent a buffer clipt can't read (DMA-BUF)".into());
        }
        return false;
    }
    let chunk = unsafe { &*d.chunk };
    // An empty chunk carries no new picture (a cursor-only update).
    if chunk.size == 0 {
        return true;
    }
    let (w, h) = (ctx.width, ctx.height);
    let stride = if chunk.stride > 0 { chunk.stride as usize } else { w as usize * 4 };
    let avail = (d.maxsize as usize).saturating_sub(chunk.offset as usize);
    let pixels = unsafe { std::slice::from_raw_parts(d.data.add(chunk.offset as usize), avail) };
    let (x, y, cw, ch) = (ctx.crop)(w, h).unwrap_or((0, 0, w, h));
    let row = cw as usize * 4;
    let mut out = Vec::with_capacity(row * ch as usize);
    for r in y..y + ch {
        let s = r as usize * stride + x as usize * 4;
        let Some(line) = pixels.get(s..s + row) else {
            ctx.error = Some(format!("short PipeWire buffer ({} bytes for {w}x{h})", pixels.len()));
            return false;
        };
        out.extend_from_slice(line);
    }
    (ctx.sink)(VideoFrame { width: cw, height: ch, order, data: out })
}

static EVENTS: StreamEvents = StreamEvents {
    version: 2,
    destroy: None,
    state_changed: Some(on_state),
    control_info: None,
    io_changed: None,
    param_changed: Some(on_param),
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
    trigger_done: None,
};

/// Read frames from `remote` until `sink` returns false (or the stream
/// fails). Runs PipeWire's loop on the calling thread.
pub fn stream(remote: Remote, crop: CropFn, sink: Box<dyn FnMut(VideoFrame) -> bool>) -> Result<()> {
    let lib = lib()?;
    unsafe {
        (lib.init)(std::ptr::null_mut(), std::ptr::null_mut());
        let main_loop = (lib.main_loop_new)(std::ptr::null_mut());
        anyhow::ensure!(!main_loop.is_null(), "PipeWire: no main loop");
        let context = (lib.context_new)((lib.main_loop_get_loop)(main_loop), std::ptr::null_mut(), 0);
        if context.is_null() {
            (lib.main_loop_destroy)(main_loop);
            bail!("PipeWire: no context");
        }
        let core = match remote.fd {
            Some(fd) => (lib.context_connect_fd)(context, fd.into_raw_fd(), std::ptr::null_mut(), 0),
            None => (lib.context_connect)(context, std::ptr::null_mut(), 0),
        };
        if core.is_null() {
            (lib.context_destroy)(context);
            (lib.main_loop_destroy)(main_loop);
            bail!("couldn't connect to PipeWire");
        }
        let props = CString::new("{ media.type = Video media.category = Capture media.role = Screen }").unwrap();
        let stream = (lib.stream_new)(core, c"clipt".as_ptr(), (lib.properties_new_string)(props.as_ptr()));
        let mut ctx = Box::new(Ctx { lib, main_loop, stream, width: 0, height: 0, order: None, crop, sink, error: None });
        let mut hook = Box::new(SpaHook([0; 6]));
        let result = if stream.is_null() {
            Err(anyhow::anyhow!("PipeWire: couldn't create the stream"))
        } else {
            (lib.stream_add_listener)(stream, &mut *hook, &EVENTS, &mut *ctx as *mut Ctx as P);
            let params = enum_format();
            let list = [params.as_ptr()];
            let flags = PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS;
            let r = (lib.stream_connect)(stream, SPA_DIRECTION_INPUT, remote.node, flags, list.as_ptr(), 1);
            if r < 0 {
                Err(anyhow::anyhow!("PipeWire: connecting the stream failed ({r})"))
            } else {
                (lib.main_loop_run)(main_loop);
                match ctx.error.take() {
                    Some(e) => Err(anyhow::anyhow!("PipeWire stream: {e}")),
                    None => Ok(()),
                }
            }
        };
        if !stream.is_null() {
            (lib.stream_destroy)(stream);
        }
        (lib.core_disconnect)(core);
        (lib.context_destroy)(context);
        (lib.main_loop_destroy)(main_loop);
        drop(hook);
        result
    }
}

fn debug(msg: &str) {
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_is_a_well_formed_object() {
        let p = enum_format();
        assert_eq!(p.len() % 8, 0);
        let size = u32::from_ne_bytes(p[0..4].try_into().unwrap()) as usize;
        assert_eq!(size + 8, p.len(), "object size covers its padded properties");
        assert_eq!(u32::from_ne_bytes(p[4..8].try_into().unwrap()), SPA_TYPE_OBJECT);
        assert_eq!(u32::from_ne_bytes(p[8..12].try_into().unwrap()), SPA_TYPE_OBJECT_FORMAT);
        assert_eq!(u32::from_ne_bytes(p[12..16].try_into().unwrap()), SPA_PARAM_ENUM_FORMAT);
    }

    #[test]
    fn parses_a_negotiated_format() {
        // What a producer sends back: fixed values, the size wrapped in a
        // Choice of kind None (as some do), plus a key we don't know.
        let prop = |key: u32, value: Vec<u8>| {
            let mut v = u32s(&[key, 0]);
            v.extend(value);
            v
        };
        let mut body = u32s(&[SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_FORMAT]);
        body.extend(prop(SPA_FORMAT_MEDIA_TYPE, pod(SPA_TYPE_ID, &u32s(&[SPA_MEDIA_TYPE_VIDEO]))));
        body.extend(prop(0x20002, pod(5, &[0; 8]))); // modifier (Long), ignored
        body.extend(prop(SPA_FORMAT_VIDEO_FORMAT, pod(SPA_TYPE_ID, &u32s(&[VIDEO_RGBX]))));
        body.extend(prop(SPA_FORMAT_VIDEO_SIZE, choice(0, SPA_TYPE_RECTANGLE, 8, &[3840, 2160])));
        let p = pod(SPA_TYPE_OBJECT, &body);
        assert_eq!(parse_format(&p), Some((VIDEO_RGBX, 3840, 2160)));
        assert_eq!(order_of(VIDEO_RGBX), Some(PixelOrder::Rgbx));
        // our own EnumFormat has no fixed size/format: not a negotiated format
        assert_eq!(parse_format(&enum_format()), Some((VIDEO_BGRX, 1920, 1080)), "first Choice value read as the default");
    }
}
