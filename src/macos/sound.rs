//! Sound for recordings on macOS, as the 48 kHz 16-bit stereo chunks the
//! writer takes on Linux too:
//!
//! - `system`, what the computer plays: ScreenCaptureKit (macOS 13+), from
//!   the same stream as the picture (`record.rs`), as Apple intends: a
//!   second stream of the display halved the frame rate. Covered by the
//!   Screen Recording permission; clipt's own sound is left out. A window
//!   recorded on its own brings its app's sound only.
//! - `mic`, the default input: an Audio Queue (every macOS clipt runs on),
//!   which converts from the device's format. macOS asks for the Microphone
//!   permission the first time.
//!
//! Both say when each buffer was heard on the host clock, which `Instant`
//! also runs on, so no sample clock is needed as with PulseAudio. When
//! nothing arrives (ScreenCaptureKit may send nothing while all is silent)
//! silence fills in, so the writer always hears from every source.

use crate::record::audio::{BYTES_PER_FRAME, CHANNELS, CHUNK_FRAMES, Chunk, Device, RATE, Source};
use anyhow::{Result, bail};
use objc2::runtime::AnyClass;
use objc2::sel;
use objc2_core_media::CMSampleBuffer;
use objc2_screen_capture_kit::SCStreamConfiguration;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// After this long without sound from a source, silence stands in for it.
const QUIET_AFTER: Duration = Duration::from_millis(150);

enum Event {
    Sound(Chunk),
    Failed(String),
}

/// Whether this macOS can record the computer's sound (13 and later).
fn system_supported() -> bool {
    AnyClass::get(c"SCStreamConfiguration").is_some_and(|c| c.instance_method(sel!(setCapturesAudio:)).is_some())
}

/// Fails, saying why, if these sources can't be recorded here.
pub fn check(sources: &[Source]) -> Result<()> {
    for s in sources {
        match &s.device {
            Device::Named(n) => bail!("sound devices by name ('{n}') aren't available on macOS yet: system and mic are"),
            Device::System if !system_supported() => bail!("recording the computer's sound needs macOS 13 or later"),
            _ => {}
        }
    }
    Ok(())
}

/// One source, set up and ready to start.
pub struct Capture {
    kind: Kind,
}

enum Kind {
    /// Fed by the picture's stream, through `SYSTEM`.
    System,
    Mic(Mic),
}

/// Set up recording `src`, so that a problem stops the recording before it
/// starts.
pub fn open(src: &Source) -> Result<Capture> {
    check(std::slice::from_ref(src))?;
    let kind = match src.device {
        Device::System => Kind::System,
        Device::Mic => Kind::Mic(Mic::open()?),
        Device::Named(_) => unreachable!("check refuses named devices"),
    };
    Ok(Capture { kind })
}

impl Capture {
    /// Hand chunks to `sink` until it returns false.
    pub fn run(self, sink: impl FnMut(Chunk) -> bool) -> Result<()> {
        match self.kind {
            Kind::System => {
                let (tx, rx) = mpsc::channel();
                *SYSTEM.lock().unwrap() = Some(tx);
                let r = deliver(&rx, sink);
                *SYSTEM.lock().unwrap() = None;
                r
            }
            Kind::Mic(mic) => mic.run(sink),
        }
    }
}

/// Pass chunks on until the sink has what it needs, filling silence in for
/// stretches without any.
fn deliver(rx: &mpsc::Receiver<Event>, mut sink: impl FnMut(Chunk) -> bool) -> Result<()> {
    let chunk = Duration::from_secs_f64(CHUNK_FRAMES as f64 / RATE as f64);
    // When the sound so far ends.
    let mut next = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Sound(c)) => {
                next = c.at + Duration::from_secs_f64((c.data.len() as u32 / BYTES_PER_FRAME) as f64 / RATE as f64);
                if !sink(c) {
                    return Ok(());
                }
            }
            Ok(Event::Failed(e)) => bail!("{e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                while Instant::now().saturating_duration_since(next) > QUIET_AFTER {
                    let silence = Chunk { at: next, data: vec![0; (CHUNK_FRAMES * BYTES_PER_FRAME) as usize] };
                    next += chunk;
                    if !sink(silence) {
                        return Ok(());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the sound stopped"),
        }
    }
}

// ---- the host clock ---------------------------------------------------------

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
}

/// Host clock ticks as nanoseconds.
fn host_ns(ticks: u64) -> u128 {
    static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let &(num, den) = TIMEBASE.get_or_init(|| {
        let mut tb = [0u32; 2];
        unsafe { mach_timebase_info(&mut tb) };
        if tb[1] == 0 { (1, 1) } else { (tb[0], tb[1]) }
    });
    ticks as u128 * num as u128 / den as u128
}

/// The `Instant` of a moment `ns` nanoseconds into the host clock.
fn instant_at_host_ns(ns: u128) -> Instant {
    let now = host_ns(unsafe { mach_absolute_time() });
    let now_i = Instant::now();
    if ns <= now {
        now_i.checked_sub(Duration::from_nanos((now - ns) as u64)).unwrap_or(now_i)
    } else {
        now_i + Duration::from_nanos((ns - now) as u64)
    }
}

/// Floats in [-1, 1] as 16-bit samples.
fn to_i16(x: f32) -> [u8; 2] {
    ((x.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes()
}

// ---- the computer's sound: ScreenCaptureKit ---------------------------------

/// CoreAudio's AudioStreamBasicDescription.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FLAG_FLOAT: u32 = 1 << 0;
const FLAG_SIGNED_INTEGER: u32 = 1 << 2;
const FLAG_PACKED: u32 = 1 << 3;
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMAudioFormatDescriptionGetStreamBasicDescription(desc: *const c_void) -> *const Asbd;
}

/// Where the computer's sound goes while it's being recorded.
static SYSTEM: Mutex<Option<mpsc::Sender<Event>>> = Mutex::new(None);

/// Have the picture's stream bring the computer's sound too.
pub fn configure(config: &SCStreamConfiguration) {
    unsafe {
        config.setCapturesAudio(true);
        config.setSampleRate(RATE as isize);
        config.setChannelCount(CHANNELS as isize);
        config.setExcludesCurrentProcessAudio(true);
    }
}

/// A buffer of the computer's sound from the picture's stream, on its queue.
pub fn system_sound(buffer: &CMSampleBuffer) {
    let Some(tx) = SYSTEM.lock().unwrap().clone() else { return };
    let event = match convert(buffer) {
        Ok(Some(c)) => Event::Sound(c),
        Ok(None) => return,
        Err(e) => Event::Failed(format!("{e:#}")),
    };
    let _ = tx.send(event);
}

/// A sample buffer of sound as a chunk of 16-bit stereo, stamped with when
/// it was heard.
fn convert(buffer: &CMSampleBuffer) -> Result<Option<Chunk>> {
    let frames = unsafe { buffer.num_samples() }.max(0) as usize;
    let (Some(desc), Some(data)) = (unsafe { buffer.format_description() }, unsafe { buffer.data_buffer() }) else { return Ok(None) };
    if frames == 0 {
        return Ok(None);
    }
    let asbd = unsafe {
        CMAudioFormatDescriptionGetStreamBasicDescription((&*desc as *const objc2_core_media::CMFormatDescription).cast()).as_ref()
    }
    .copied()
    .unwrap_or_default();
    if asbd.format_id != FORMAT_LINEAR_PCM || asbd.sample_rate.round() as u32 != RATE {
        bail!("ScreenCaptureKit sent sound in a format clipt doesn't read ({} Hz)", asbd.sample_rate);
    }
    let len = unsafe { data.data_length() };
    let mut bytes = vec![0u8; len];
    if len > 0 {
        let p = std::ptr::NonNull::new(bytes.as_mut_ptr().cast()).unwrap();
        if unsafe { data.copy_data_bytes(0, len, p) } != 0 {
            bail!("reading ScreenCaptureKit's sound failed");
        }
    }
    let ch = asbd.channels_per_frame.max(1) as usize;
    let float = asbd.format_flags & FLAG_FLOAT != 0 && asbd.bits_per_channel == 32;
    let int16 = asbd.format_flags & FLAG_FLOAT == 0 && asbd.bits_per_channel == 16;
    let planar = asbd.format_flags & FLAG_NON_INTERLEAVED != 0;
    let width = if float { 4 } else { 2 };
    if !(float || int16) || bytes.len() < frames * ch * width {
        bail!(
            "ScreenCaptureKit sent sound in a format clipt doesn't read ({} bits, flags {:#x})",
            asbd.bits_per_channel,
            asbd.format_flags
        );
    }
    // Sample `i` of channel `c`, as 16-bit little-endian bytes.
    let sample = |i: usize, c: usize| -> [u8; 2] {
        let at = if planar { (c * frames + i) * width } else { (i * ch + c) * width };
        if float { to_i16(f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())) } else { [bytes[at], bytes[at + 1]] }
    };
    let mut out = Vec::with_capacity(frames * BYTES_PER_FRAME as usize);
    for i in 0..frames {
        out.extend(sample(i, 0));
        out.extend(sample(i, 1.min(ch - 1)));
    }
    let pts = unsafe { buffer.presentation_time_stamp() };
    let at = if pts.timescale > 0 {
        instant_at_host_ns(pts.value.max(0) as u128 * 1_000_000_000 / pts.timescale as u128)
    } else {
        Instant::now()
    };
    Ok(Some(Chunk { at, data: out }))
}

// ---- the microphone: an Audio Queue ------------------------------------------

#[repr(C)]
struct AudioQueueBuffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

#[repr(C)]
struct SmpteTime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    kind: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SmpteTime,
    flags: u32,
    reserved: u32,
}

const HOST_TIME_VALID: u32 = 1 << 1;

type Queue = *mut c_void;
type InputCallback = unsafe extern "C" fn(*mut c_void, Queue, *mut AudioQueueBuffer, *const AudioTimeStamp, u32, *const c_void);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewInput(
        format: *const Asbd,
        callback: InputCallback,
        user_data: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        out: *mut Queue,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: Queue, size: u32, out: *mut *mut AudioQueueBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(queue: Queue, buffer: *mut AudioQueueBuffer, n: u32, descriptions: *const c_void) -> i32;
    fn AudioQueueStart(queue: Queue, at: *const AudioTimeStamp) -> i32;
    fn AudioQueueStop(queue: Queue, immediate: u8) -> i32;
    fn AudioQueueDispose(queue: Queue, immediate: u8) -> i32;
}

/// What the queue's callback, on the queue's own thread, needs.
struct MicState {
    events: Mutex<mpsc::Sender<Event>>,
    running: AtomicBool,
}

/// The default input, opened and not yet started.
struct Mic {
    queue: Queue,
    state: Box<MicState>,
    rx: mpsc::Receiver<Event>,
}

// SAFETY: the queue is started, stopped and disposed of from one thread at a
// time; it moves to the reading thread once.
unsafe impl Send for Mic {}

/// Buffers in flight: 80 ms of sound.
const MIC_BUFFERS: usize = 4;

unsafe extern "C" fn mic_callback(
    user: *mut c_void,
    queue: Queue,
    buffer: *mut AudioQueueBuffer,
    start: *const AudioTimeStamp,
    _n: u32,
    _descriptions: *const c_void,
) {
    let state = unsafe { &*(user as *const MicState) };
    let b = unsafe { &*buffer };
    if b.size > 0 && !b.data.is_null() {
        let data = unsafe { std::slice::from_raw_parts(b.data as *const u8, b.size as usize) }.to_vec();
        let at = match unsafe { start.as_ref() } {
            Some(t) if t.flags & HOST_TIME_VALID != 0 => instant_at_host_ns(host_ns(t.host_time)),
            _ => Instant::now(),
        };
        let _ = state.events.lock().unwrap().send(Event::Sound(Chunk { at, data }));
    }
    if state.running.load(Ordering::SeqCst) {
        unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null()) };
    }
}

impl Mic {
    fn open() -> Result<Self> {
        let format = Asbd {
            sample_rate: RATE as f64,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FLAG_SIGNED_INTEGER | FLAG_PACKED,
            bytes_per_packet: BYTES_PER_FRAME,
            frames_per_packet: 1,
            bytes_per_frame: BYTES_PER_FRAME,
            channels_per_frame: CHANNELS,
            bits_per_channel: 16,
            reserved: 0,
        };
        let (tx, rx) = mpsc::channel();
        let state = Box::new(MicState { events: Mutex::new(tx), running: AtomicBool::new(true) });
        let mut queue: Queue = std::ptr::null_mut();
        let user = &*state as *const MicState as *mut c_void;
        // No run loop: the callbacks come on the queue's own thread.
        let err = unsafe { AudioQueueNewInput(&format, mic_callback, user, std::ptr::null(), std::ptr::null(), 0, &mut queue) };
        if err != 0 || queue.is_null() {
            bail!("can't open the microphone (OSStatus {err}); is there an input device?");
        }
        let mic = Mic { queue, state, rx };
        for _ in 0..MIC_BUFFERS {
            let mut b: *mut AudioQueueBuffer = std::ptr::null_mut();
            let err = unsafe { AudioQueueAllocateBuffer(mic.queue, CHUNK_FRAMES * BYTES_PER_FRAME, &mut b) };
            if err != 0 || b.is_null() {
                bail!("can't set up the microphone (OSStatus {err})");
            }
            unsafe { AudioQueueEnqueueBuffer(mic.queue, b, 0, std::ptr::null()) };
        }
        Ok(mic)
    }

    fn run(self, sink: impl FnMut(Chunk) -> bool) -> Result<()> {
        let err = unsafe { AudioQueueStart(self.queue, std::ptr::null()) };
        if err != 0 {
            bail!("can't start the microphone (OSStatus {err}); is clipt allowed to use it in System Settings > Privacy & Security?");
        }
        deliver(&self.rx, sink)
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.state.running.store(false, Ordering::SeqCst);
        unsafe {
            AudioQueueStop(self.queue, 1);
            AudioQueueDispose(self.queue, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_become_16_bit() {
        assert_eq!(to_i16(0.0), [0, 0]);
        assert_eq!(i16::from_le_bytes(to_i16(1.0)), 32767);
        assert_eq!(i16::from_le_bytes(to_i16(-2.0)), -32767, "clamped");
        assert_eq!(i16::from_le_bytes(to_i16(0.5)), 16384);
    }

    #[test]
    fn audio_records_match_core_audio() {
        assert_eq!(std::mem::size_of::<Asbd>(), 40);
        assert_eq!(std::mem::size_of::<AudioTimeStamp>(), 64);
        assert_eq!(std::mem::offset_of!(AudioTimeStamp, flags), 56);
        assert_eq!(std::mem::offset_of!(AudioQueueBuffer, size), 16);
    }

    #[test]
    fn silence_fills_in_and_stops_with_the_sink() {
        let (tx, rx) = mpsc::channel();
        drop(tx.send(Event::Sound(Chunk { at: Instant::now() - Duration::from_secs(1), data: vec![1; 3840] })));
        let mut got = Vec::new();
        deliver(&rx, |c| {
            got.push(c);
            got.len() < 5
        })
        .unwrap();
        assert_eq!(got[0].data[0], 1);
        assert!(got[1..].iter().all(|c| c.data.iter().all(|&b| b == 0)), "then silence");
        assert_eq!(got[2].at - got[1].at, Duration::from_millis(20), "20 ms apart");
        drop(tx);
    }
}
