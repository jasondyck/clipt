//! Sound from the sound server through libpulse-simple.
//!
//! libpulse-simple is loaded at runtime (dlopen) like libpipewire, so clipt
//! still starts without it. It works the same on Wayland, X11 and in the
//! Flatpak: PipeWire desktops serve it through pipewire-pulse, PulseAudio
//! natively.
//!
//! Listing the server's devices (`clipt list sound`) needs libpulse's full,
//! asynchronous API; only the first, stable fields of its records are read.

use super::{BYTES_PER_FRAME, CHANNELS, CHUNK_FRAMES, Chunk, Device, RATE, SampleClock, Source};
use anyhow::{Result, bail};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn server_name(src: &Source) -> CString {
    let n = match &src.device {
        Device::System => "@DEFAULT_MONITOR@",
        Device::Mic => "@DEFAULT_SOURCE@",
        Device::Named(n) => n,
    };
    CString::new(n).unwrap_or_default()
}

// ---- libpulse-simple, loaded at runtime ------------------------------------

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

const PA_SAMPLE_S16LE: c_int = 3;
const PA_STREAM_RECORD: c_int = 2;
/// "Server default" in a pa_buffer_attr field.
const DEFAULT: u32 = u32::MAX;

type P = *mut c_void;

struct Lib {
    new: unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        c_int,
        *const c_char,
        *const c_char,
        *const SampleSpec,
        P,
        *const BufferAttr,
        *mut c_int,
    ) -> P,
    read: unsafe extern "C" fn(P, *mut c_void, usize, *mut c_int) -> c_int,
    get_latency: unsafe extern "C" fn(P, *mut c_int) -> u64,
    free: unsafe extern "C" fn(P),
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    // The asynchronous API, for listing devices.
    mainloop_new: unsafe extern "C" fn() -> P,
    mainloop_get_api: unsafe extern "C" fn(P) -> P,
    mainloop_iterate: unsafe extern "C" fn(P, c_int, *mut c_int) -> c_int,
    mainloop_free: unsafe extern "C" fn(P),
    context_new: unsafe extern "C" fn(P, *const c_char) -> P,
    context_connect: unsafe extern "C" fn(P, *const c_char, c_int, P) -> c_int,
    context_get_state: unsafe extern "C" fn(P) -> c_int,
    context_disconnect: unsafe extern "C" fn(P),
    context_unref: unsafe extern "C" fn(P),
    context_get_server_info: unsafe extern "C" fn(P, ServerInfoCb, P) -> P,
    context_get_source_info_list: unsafe extern "C" fn(P, SourceInfoCb, P) -> P,
    operation_get_state: unsafe extern "C" fn(P) -> c_int,
    operation_unref: unsafe extern "C" fn(P),
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
    let h = unsafe { libc::dlopen(c"libpulse-simple.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        return Err("recording sound needs libpulse-simple.so.0 (install libpulse, or pipewire-pulse on PipeWire desktops)".into());
    }
    macro_rules! sym {
        ($name:literal) => {{
            // dlsym on the handle also searches its dependencies, where
            // pa_strerror and the asynchronous API (libpulse) live.
            let p = unsafe { libc::dlsym(h, concat!($name, "\0").as_ptr().cast()) };
            if p.is_null() {
                return Err(format!("libpulse-simple has no {}", $name));
            }
            // SAFETY: the symbol is the C function named, whose signature
            // matches the field it's stored in (from pulse/simple.h,
            // error.h, mainloop.h, context.h, introspect.h and operation.h).
            unsafe { fn_ptr(p) }
        }};
    }
    Ok(Lib {
        new: sym!("pa_simple_new"),
        read: sym!("pa_simple_read"),
        get_latency: sym!("pa_simple_get_latency"),
        free: sym!("pa_simple_free"),
        strerror: sym!("pa_strerror"),
        mainloop_new: sym!("pa_mainloop_new"),
        mainloop_get_api: sym!("pa_mainloop_get_api"),
        mainloop_iterate: sym!("pa_mainloop_iterate"),
        mainloop_free: sym!("pa_mainloop_free"),
        context_new: sym!("pa_context_new"),
        context_connect: sym!("pa_context_connect"),
        context_get_state: sym!("pa_context_get_state"),
        context_disconnect: sym!("pa_context_disconnect"),
        context_unref: sym!("pa_context_unref"),
        context_get_server_info: sym!("pa_context_get_server_info"),
        context_get_source_info_list: sym!("pa_context_get_source_info_list"),
        operation_get_state: sym!("pa_operation_get_state"),
        operation_unref: sym!("pa_operation_unref"),
    })
}

/// Fails, saying what's wrong, if these sources can't be recorded: no
/// libpulse-simple, or a device the server doesn't have. Devices are checked
/// against the server's list because pipewire-pulse, unlike PulseAudio,
/// accepts a stream from a device that doesn't exist and never delivers it.
pub fn check(sources: &[Source]) -> Result<()> {
    lib()?;
    let named: Vec<&str> = sources
        .iter()
        .filter_map(|s| match &s.device {
            Device::Named(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    if !named.is_empty() {
        let have = devices()?;
        if let Some(n) = named.iter().find(|n| !have.iter().any(|d| d.name == **n)) {
            bail!("there's no sound device '{n}' (clipt list sound shows them)");
        }
    }
    Ok(())
}

fn error_text(lib: &Lib, err: c_int) -> String {
    let p = unsafe { (lib.strerror)(err) };
    if p.is_null() { format!("error {err}") } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

/// An open recording stream on the sound server.
pub struct Capture {
    lib: &'static Lib,
    s: P,
}

// SAFETY: a pa_simple is used from one thread at a time; it moves to the
// reading thread once and stays there.
unsafe impl Send for Capture {}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe { (self.lib.free)(self.s) };
    }
}

/// pa_strerror's code for a device that doesn't exist.
const PA_ERR_NOENTITY: c_int = 5;

/// Connect to the sound server and start recording `src`.
pub fn open(src: &Source) -> Result<Capture> {
    let lib = lib()?;
    let spec = SampleSpec { format: PA_SAMPLE_S16LE, rate: RATE, channels: CHANNELS as u8 };
    // fragsize: without it the server hands over much larger fragments,
    // seconds' worth on PulseAudio.
    let attr =
        BufferAttr { maxlength: DEFAULT, tlength: DEFAULT, prebuf: DEFAULT, minreq: DEFAULT, fragsize: CHUNK_FRAMES * BYTES_PER_FRAME };
    let mut err: c_int = 0;
    let s = unsafe {
        (lib.new)(
            std::ptr::null(),
            c"clipt".as_ptr(),
            PA_STREAM_RECORD,
            server_name(src).as_ptr(),
            c"Recording".as_ptr(),
            &spec,
            std::ptr::null_mut(),
            &attr,
            &mut err,
        )
    };
    if s.is_null() {
        if let (PA_ERR_NOENTITY, Device::Named(n)) = (err, &src.device) {
            bail!("there's no sound device '{n}' (clipt list sound shows them)");
        }
        bail!("can't record sound from {}: {} (is a PulseAudio or pipewire-pulse server running?)", src.label(), error_text(lib, err));
    }
    Ok(Capture { lib, s })
}

// ---- listing devices --------------------------------------------------------

/// The start of pa_server_info (introspect.h); later fields aren't read.
#[repr(C)]
struct ServerInfo {
    user_name: *const c_char,
    host_name: *const c_char,
    server_version: *const c_char,
    server_name: *const c_char,
    sample_spec: SampleSpec,
    default_sink_name: *const c_char,
    default_source_name: *const c_char,
}

/// The start of pa_source_info (introspect.h); later fields aren't read.
#[repr(C)]
struct SourceInfo {
    name: *const c_char,
    index: u32,
    description: *const c_char,
}

type ServerInfoCb = unsafe extern "C" fn(P, *const ServerInfo, P);
type SourceInfoCb = unsafe extern "C" fn(P, *const SourceInfo, c_int, P);

/// A recordable source on the server.
pub struct DeviceInfo {
    pub name: String,
    pub description: String,
    /// What `system` records (the default output's monitor).
    pub is_system: bool,
    /// What `mic` records (the default input).
    pub is_mic: bool,
}

#[derive(Default)]
struct Listing {
    default_sink: String,
    default_source: String,
    sources: Vec<(String, String)>,
}

fn text(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

unsafe extern "C" fn on_server_info(_: P, i: *const ServerInfo, ud: P) {
    // SAFETY: ud is the &mut Listing passed with the request, and i a valid
    // record for the duration of the call (or null).
    let (l, i) = unsafe { (&mut *(ud as *mut Listing), i.as_ref()) };
    if let Some(i) = i {
        l.default_sink = text(i.default_sink_name);
        l.default_source = text(i.default_source_name);
    }
}

unsafe extern "C" fn on_source_info(_: P, i: *const SourceInfo, eol: c_int, ud: P) {
    // SAFETY: as in on_server_info.
    let (l, i) = unsafe { (&mut *(ud as *mut Listing), i.as_ref()) };
    if let (0, Some(i)) = (eol, i) {
        l.sources.push((text(i.name), text(i.description)));
    }
}

/// The sources the server offers, with the ones `system` and `mic` mean marked.
pub fn devices() -> Result<Vec<DeviceInfo>> {
    const READY: c_int = 4;
    const FAILED: c_int = 5;
    const TERMINATED: c_int = 6;
    const OP_RUNNING: c_int = 0;
    let lib = lib()?;
    let mut l = Listing::default();
    unsafe {
        let ml = (lib.mainloop_new)();
        let ctx = (lib.context_new)((lib.mainloop_get_api)(ml), c"clipt".as_ptr());
        let result = (|| {
            if (lib.context_connect)(ctx, std::ptr::null(), 0, std::ptr::null_mut()) < 0 {
                bail!("can't reach the sound server (is a PulseAudio or pipewire-pulse server running?)");
            }
            loop {
                match (lib.context_get_state)(ctx) {
                    READY => break,
                    FAILED | TERMINATED => bail!("can't reach the sound server (is a PulseAudio or pipewire-pulse server running?)"),
                    _ => {
                        (lib.mainloop_iterate)(ml, 1, std::ptr::null_mut());
                    }
                }
            }
            let ud = &mut l as *mut Listing as P;
            for op in [(lib.context_get_server_info)(ctx, on_server_info, ud), (lib.context_get_source_info_list)(ctx, on_source_info, ud)]
            {
                if op.is_null() {
                    bail!("the sound server refused to list its devices");
                }
                while (lib.operation_get_state)(op) == OP_RUNNING {
                    if (lib.mainloop_iterate)(ml, 1, std::ptr::null_mut()) < 0 {
                        break;
                    }
                }
                (lib.operation_unref)(op);
            }
            Ok(())
        })();
        (lib.context_disconnect)(ctx);
        (lib.context_unref)(ctx);
        (lib.mainloop_free)(ml);
        result?;
    }
    let system = format!("{}.monitor", l.default_sink);
    Ok(l.sources
        .into_iter()
        .map(|(name, description)| DeviceInfo { is_system: name == system, is_mic: name == l.default_source, name, description })
        .collect())
}

impl Capture {
    /// Read and stamp chunks, handing each to `sink`, until it returns false.
    pub fn run(self, mut sink: impl FnMut(Chunk) -> bool) -> Result<()> {
        let mut clock = SampleClock::new(Instant::now());
        let bytes = (CHUNK_FRAMES * BYTES_PER_FRAME) as usize;
        loop {
            let mut data = vec![0u8; bytes];
            let mut err: c_int = 0;
            if unsafe { (self.lib.read)(self.s, data.as_mut_ptr().cast(), bytes, &mut err) } < 0 {
                bail!("reading sound: {}", error_text(self.lib, err));
            }
            let now = Instant::now();
            let latency = unsafe { (self.lib.get_latency)(self.s, &mut err) };
            let at = clock.stamp(now, Duration::from_micros(latency), CHUNK_FRAMES as u64);
            if !sink(Chunk { at, data }) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_records_match_libpulse() {
        // pa_server_info: four strings, a 12-byte sample spec, then the
        // default sink and source names (64-bit layout).
        if std::mem::size_of::<usize>() == 8 {
            assert_eq!(std::mem::offset_of!(ServerInfo, default_sink_name), 48);
            assert_eq!(std::mem::offset_of!(ServerInfo, default_source_name), 56);
            assert_eq!(std::mem::offset_of!(SourceInfo, description), 16);
        }
    }
}
