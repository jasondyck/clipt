//! Sound on Windows through WASAPI: the computer's sound as a loopback of
//! the default output, the microphone from the default input, or any
//! device by its name in `clipt list sound`.
//!
//! The audio engine converts to the 48 kHz 16-bit stereo the recorder uses
//! (AUTOCONVERTPCM), so there's no resampling here. WASAPI is COM, which
//! `windows-sys` leaves out, so the few interfaces used are declared below
//! as their vtables, in order, with only the methods called given types.
//!
//! A loopback stream delivers nothing while nothing plays; the gaps are
//! filled with silence against the clock, or the sound would run ahead of
//! the picture after every quiet moment.

use super::{BYTES_PER_FRAME, CHANNELS, CHUNK_FRAMES, Chunk, Device, RATE, SampleClock, Source};
use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::PROPERTYKEY;
use windows_sys::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    DEVICE_STATE_ACTIVE, WAVE_FORMAT_PCM, WAVEFORMATEX, eCapture, eConsole, eRender,
};
use windows_sys::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::core::{GUID, HRESULT};

const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);
const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xa95664d2_9614_4f35_a746_de8db63617e6);
const IID_IAUDIO_CLIENT: GUID = GUID::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
const IID_IAUDIO_CAPTURE_CLIENT: GUID = GUID::from_u128(0xc8adbd64_e71e_48a0_a4de_185c395cd317);
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };
/// AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: a better resampler than the
/// conversion's default.
const SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
const VT_LPWSTR: u16 = 31;

type Unused = usize;

#[repr(C)]
struct IUnknownVtbl {
    query_interface: Unused,
    add_ref: Unused,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct EnumeratorVtbl {
    base: IUnknownVtbl,
    enum_audio_endpoints: unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> HRESULT,
    get_default_audio_endpoint: unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct CollectionVtbl {
    base: IUnknownVtbl,
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct DeviceVtbl {
    base: IUnknownVtbl,
    activate: unsafe extern "system" fn(*mut c_void, *const GUID, u32, *const c_void, *mut *mut c_void) -> HRESULT,
    open_property_store: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
}

#[repr(C)]
struct PropertyStoreVtbl {
    base: IUnknownVtbl,
    get_count: Unused,
    get_at: Unused,
    get_value: unsafe extern "system" fn(*mut c_void, *const PROPERTYKEY, *mut PropVariant) -> HRESULT,
}

#[repr(C)]
struct AudioClientVtbl {
    base: IUnknownVtbl,
    initialize: unsafe extern "system" fn(*mut c_void, i32, u32, i64, i64, *const WAVEFORMATEX, *const GUID) -> HRESULT,
    get_buffer_size: Unused,
    get_stream_latency: Unused,
    get_current_padding: Unused,
    is_format_supported: Unused,
    get_mix_format: Unused,
    get_device_period: Unused,
    start: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    stop: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    reset: Unused,
    set_event_handle: Unused,
    get_service: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct CaptureClientVtbl {
    base: IUnknownVtbl,
    get_buffer: unsafe extern "system" fn(*mut c_void, *mut *mut u8, *mut u32, *mut u32, *mut u64, *mut u64) -> HRESULT,
    release_buffer: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_next_packet_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
}

/// The start of a PROPVARIANT: for VT_LPWSTR the string follows the header.
#[repr(C)]
struct PropVariant {
    vt: u16,
    reserved: [u16; 3],
    pwsz: *mut u16,
    rest: usize,
}

/// A COM interface pointer, released when dropped.
struct Com<V>(*mut *const V);

// SAFETY: every object here lives in the multithreaded apartment, where
// interface pointers may be used from any thread that joined it.
unsafe impl<V> Send for Com<V> {}

impl<V> Com<V> {
    fn vt(&self) -> &V {
        unsafe { &**self.0 }
    }

    fn raw(&self) -> *mut c_void {
        self.0.cast()
    }
}

impl<V> Drop for Com<V> {
    fn drop(&mut self) {
        // Every vtable starts with IUnknown's.
        unsafe { ((*(*self.0 as *const IUnknownVtbl)).release)(self.raw()) };
    }
}

fn check_hr(hr: HRESULT, what: &str) -> Result<()> {
    if hr < 0 {
        bail!("{what} failed (0x{:08x})", hr as u32);
    }
    Ok(())
}

/// Join the multithreaded apartment; harmless if this thread already has.
fn com_init() {
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) };
}

fn enumerator() -> Result<Com<EnumeratorVtbl>> {
    com_init();
    let mut p = std::ptr::null_mut();
    let hr = unsafe { CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, std::ptr::null_mut(), CLSCTX_ALL, &IID_IMM_DEVICE_ENUMERATOR, &mut p) };
    check_hr(hr, "opening Windows' sound devices")?;
    Ok(Com(p.cast()))
}

/// Output devices are recorded by loopback, input devices directly.
struct Endpoint {
    device: Com<DeviceVtbl>,
    name: String,
    id: String,
    output: bool,
}

fn wide_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

fn endpoint(device: Com<DeviceVtbl>, output: bool) -> Result<Endpoint> {
    let mut id = std::ptr::null_mut();
    check_hr(unsafe { (device.vt().get_id)(device.raw(), &mut id) }, "reading a sound device's id")?;
    let id_s = wide_string(id);
    unsafe { CoTaskMemFree(id.cast()) };
    let mut store = std::ptr::null_mut();
    let mut name = String::new();
    if unsafe { (device.vt().open_property_store)(device.raw(), STGM_READ, &mut store) } >= 0 {
        let store: Com<PropertyStoreVtbl> = Com(store.cast());
        let mut v = PropVariant { vt: 0, reserved: [0; 3], pwsz: std::ptr::null_mut(), rest: 0 };
        if unsafe { (store.vt().get_value)(store.raw(), &PKEY_DEVICE_FRIENDLY_NAME, &mut v) } >= 0 && v.vt == VT_LPWSTR {
            name = wide_string(v.pwsz);
            // What PropVariantClear does for a VT_LPWSTR.
            unsafe { CoTaskMemFree(v.pwsz.cast()) };
        }
    }
    Ok(Endpoint { device, name, id: id_s, output })
}

fn default_endpoint(e: &Com<EnumeratorVtbl>, output: bool) -> Result<Endpoint> {
    let mut d = std::ptr::null_mut();
    let hr = unsafe { (e.vt().get_default_audio_endpoint)(e.raw(), if output { eRender } else { eCapture }, eConsole, &mut d) };
    if hr < 0 {
        bail!("Windows has no default sound {}", if output { "output" } else { "input (no microphone?)" });
    }
    endpoint(Com(d.cast()), output)
}

fn all_endpoints(e: &Com<EnumeratorVtbl>) -> Result<Vec<Endpoint>> {
    let mut v = Vec::new();
    for output in [true, false] {
        let mut c = std::ptr::null_mut();
        let hr = unsafe { (e.vt().enum_audio_endpoints)(e.raw(), if output { eRender } else { eCapture }, DEVICE_STATE_ACTIVE, &mut c) };
        check_hr(hr, "listing sound devices")?;
        let c: Com<CollectionVtbl> = Com(c.cast());
        let mut n = 0;
        unsafe { (c.vt().get_count)(c.raw(), &mut n) };
        for i in 0..n {
            let mut d = std::ptr::null_mut();
            if unsafe { (c.vt().item)(c.raw(), i, &mut d) } >= 0 {
                v.push(endpoint(Com(d.cast()), output)?);
            }
        }
    }
    Ok(v)
}

/// The endpoint a source means: the defaults, or a device by its name (or
/// id) from the list.
fn find(src: &Source) -> Result<Endpoint> {
    let e = enumerator()?;
    match &src.device {
        Device::System => default_endpoint(&e, true),
        Device::Mic => default_endpoint(&e, false),
        Device::Named(n) => all_endpoints(&e)?
            .into_iter()
            .find(|d| d.name.eq_ignore_ascii_case(n) || d.id == *n)
            .with_context(|| format!("there's no sound device '{n}' (clipt list sound shows them)")),
    }
}

/// A recordable device, as `clipt list sound` shows it.
pub struct DeviceInfo {
    pub name: String,
    pub description: String,
    pub is_system: bool,
    pub is_mic: bool,
}

pub fn devices() -> Result<Vec<DeviceInfo>> {
    let e = enumerator()?;
    let (sys, mic) = (default_endpoint(&e, true).ok().map(|d| d.id), default_endpoint(&e, false).ok().map(|d| d.id));
    Ok(all_endpoints(&e)?
        .into_iter()
        .map(|d| DeviceInfo {
            is_system: Some(&d.id) == sys.as_ref(),
            is_mic: Some(&d.id) == mic.as_ref(),
            description: if d.output { "output: its sound, by loopback" } else { "input" }.into(),
            name: d.name,
        })
        .collect())
}

/// Fails, saying which, if a source has no device.
pub fn check(sources: &[Source]) -> Result<()> {
    for s in sources {
        find(s)?;
    }
    Ok(())
}

/// A started capture stream.
pub struct Capture {
    client: Com<AudioClientVtbl>,
    capture: Com<CaptureClientVtbl>,
    loopback: bool,
}

pub fn open(src: &Source) -> Result<Capture> {
    let ep = find(src)?;
    let mut client = std::ptr::null_mut();
    let hr = unsafe { (ep.device.vt().activate)(ep.device.raw(), &IID_IAUDIO_CLIENT, CLSCTX_ALL, std::ptr::null(), &mut client) };
    check_hr(hr, &format!("opening {}", src.label()))?;
    let client: Com<AudioClientVtbl> = Com(client.cast());
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * BYTES_PER_FRAME,
        nBlockAlign: BYTES_PER_FRAME as u16,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let loopback = ep.output;
    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | SRC_DEFAULT_QUALITY | if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
    // A 200 ms buffer, read every 10 ms.
    let hr = unsafe { (client.vt().initialize)(client.raw(), AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, std::ptr::null()) };
    check_hr(hr, &format!("starting to record {} ({})", src.label(), ep.name))?;
    let mut cap = std::ptr::null_mut();
    check_hr(unsafe { (client.vt().get_service)(client.raw(), &IID_IAUDIO_CAPTURE_CLIENT, &mut cap) }, "opening the capture")?;
    let capture = Com(cap.cast());
    check_hr(unsafe { (client.vt().start)(client.raw()) }, &format!("starting {}", src.label()))?;
    Ok(Capture { client, capture, loopback })
}

/// QueryPerformanceCounter in 100 ns units, WASAPI's clock for packets.
fn qpc_100ns() -> u64 {
    let (mut c, mut f) = (0i64, 1i64);
    unsafe {
        QueryPerformanceCounter(&mut c);
        QueryPerformanceFrequency(&mut f);
    }
    (c as i128 * 10_000_000 / f.max(1) as i128) as u64
}

impl Capture {
    /// Read and stamp 20 ms chunks, handing each to `sink`, until it
    /// returns false.
    pub fn run(self, mut sink: impl FnMut(Chunk) -> bool) -> Result<()> {
        com_init();
        let chunk_bytes = (CHUNK_FRAMES * BYTES_PER_FRAME) as usize;
        let started = Instant::now();
        let mut clock = SampleClock::new(started);
        let mut pending: Vec<u8> = Vec::with_capacity(chunk_bytes * 4);
        // When the newest sample in `pending` was heard, in qpc_100ns units.
        let mut pending_end = qpc_100ns();
        // Frames handed on or pending, against the wall clock for loopback.
        let mut frames: u64 = 0;
        let frame_100ns = 10_000_000 / RATE as u64;
        let result = (|| -> Result<()> {
            loop {
                let mut got = false;
                loop {
                    let mut next = 0u32;
                    check_hr(unsafe { (self.capture.vt().get_next_packet_size)(self.capture.raw(), &mut next) }, "reading sound")?;
                    if next == 0 {
                        break;
                    }
                    let (mut data, mut n, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                    let hr = unsafe {
                        (self.capture.vt().get_buffer)(self.capture.raw(), &mut data, &mut n, &mut flags, std::ptr::null_mut(), &mut qpc)
                    };
                    check_hr(hr, "reading sound")?;
                    let bytes = n as usize * BYTES_PER_FRAME as usize;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT as u32 != 0 || data.is_null() {
                        pending.resize(pending.len() + bytes, 0);
                    } else {
                        pending.extend_from_slice(unsafe { std::slice::from_raw_parts(data, bytes) });
                    }
                    unsafe { (self.capture.vt().release_buffer)(self.capture.raw(), n) };
                    pending_end = if qpc > 0 { qpc + n as u64 * frame_100ns } else { qpc_100ns() };
                    frames += n as u64;
                    got = true;
                }
                // Loopback says nothing during silence: make up what the
                // clock says is missing, keeping 20 ms in hand for a packet
                // on its way.
                if self.loopback && !got {
                    let due = (started.elapsed().as_secs_f64() * RATE as f64) as u64;
                    if due > frames + 2 * CHUNK_FRAMES as u64 {
                        let fill = due - frames - CHUNK_FRAMES as u64;
                        pending.resize(pending.len() + fill as usize * BYTES_PER_FRAME as usize, 0);
                        frames += fill;
                        pending_end = qpc_100ns().saturating_sub(CHUNK_FRAMES as u64 * frame_100ns);
                    }
                }
                while pending.len() >= chunk_bytes {
                    let data: Vec<u8> = pending.drain(..chunk_bytes).collect();
                    // This chunk ended before whatever is still pending.
                    let after = (pending.len() / BYTES_PER_FRAME as usize) as u64 * frame_100ns;
                    let latency = Duration::from_nanos(qpc_100ns().saturating_sub(pending_end.saturating_sub(after)) * 100);
                    let at = clock.stamp(Instant::now(), latency, CHUNK_FRAMES as u64);
                    if !sink(Chunk { at, data }) {
                        return Ok(());
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        unsafe { (self.client.vt().stop)(self.client.raw()) };
        result
    }
}
