//! Recording on macOS: frames from ScreenCaptureKit (macOS 12.3+), handed
//! to the same writer, ffmpeg pipe, pause and stop as on Linux and Windows.
//!
//! A display is captured with `sourceRect` cropping it to the area, so the
//! window server does the cropping and scaling; a window is captured on its
//! own (`desktopIndependentWindow`), even while something covers it, at the
//! size it had when recording started. Frames arrive as BGRA pixel buffers
//! on a dispatch queue of ours, are copied out and go to the writer's
//! newest-frame slot; a still screen sends nothing.

use crate::capture::{PixelOrder, VideoFrame};
use crate::record::{Feed, Options, Sink, Target, resolve_target};
use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::NSApplication;
use objc2_core_foundation::{CFDictionary, CGPoint, CGRect, CGSize};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

/// How long ScreenCaptureKit may take to list what's on screen, or to
/// start and stop a stream. It answers in milliseconds; this is for a hang.
const REPLY_LIMIT: Duration = Duration::from_secs(10);

/// Objective-C objects made on the main thread and used on the capture
/// thread. ScreenCaptureKit's objects are thread-safe; Rust can't know it.
pub(super) struct Sendable<T>(pub(super) T);
unsafe impl<T> Send for Sendable<T> {}

impl<T> Sendable<T> {
    /// A method, so a closure captures the whole wrapper, not its field.
    pub(super) fn into_inner(self) -> T {
        self.0
    }
}

/// What the stream's callbacks tell the capture thread.
enum Event {
    /// The writer has what it needs (the sink said stop).
    Done,
    /// ScreenCaptureKit stopped the stream: the display went away, the
    /// window closed, the permission was taken back.
    Failed(String),
}

struct Ivars {
    sink: Mutex<Option<Sink>>,
    events: Mutex<mpsc::Sender<Event>>,
}

define_class!(
    /// The stream's frame output and delegate.
    #[unsafe(super(NSObject))]
    #[name = "CliptRecordOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, buffer: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            if !complete(buffer) {
                return;
            }
            let Some(frame) = copy_frame(buffer) else { return };
            let mut sink = self.ivars().sink.lock().unwrap();
            let Some(s) = sink.as_mut() else { return };
            if !s(frame) {
                *sink = None;
                let _ = self.ivars().events.lock().unwrap().send(Event::Done);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let _ = self.ivars().events.lock().unwrap().send(Event::Failed(error.localizedDescription().to_string()));
        }
    }
);

impl Output {
    fn new(sink: Sink, events: mpsc::Sender<Event>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { sink: Mutex::new(Some(sink)), events: Mutex::new(events) });
        unsafe { msg_send![super(this), init] }
    }
}

/// Whether a sample buffer holds a new picture: `complete`, or the stream's
/// first frame (`started`), which may be the only one while the screen
/// stays still. Idle frames (nothing changed) and blank or suspended ones
/// (display asleep, screen locked) are left out.
fn complete(buffer: &CMSampleBuffer) -> bool {
    let Some(list) = (unsafe { buffer.sample_attachments_array(false) }) else { return false };
    if list.count() < 1 {
        return false;
    }
    let Some(info) = (unsafe { (list.value_at_index(0) as *const CFDictionary).as_ref() }) else { return false };
    let key: *const NSString = unsafe { SCStreamFrameInfoStatus };
    let status = unsafe { info.value(key.cast()) } as *const NSNumber;
    unsafe { status.as_ref() }.is_some_and(|n| matches!(SCFrameStatus(n.integerValue()), SCFrameStatus::Complete | SCFrameStatus::Started))
}

/// The picture in a sample buffer, as packed BGRx rows.
fn copy_frame(buffer: &CMSampleBuffer) -> Option<VideoFrame> {
    let px = unsafe { buffer.image_buffer() }?;
    if unsafe { CVPixelBufferLockBaseAddress(&px, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return None;
    }
    let (w, h, stride) = (CVPixelBufferGetWidth(&px), CVPixelBufferGetHeight(&px), CVPixelBufferGetBytesPerRow(&px));
    let base = CVPixelBufferGetBaseAddress(&px) as *const u8;
    let frame = (!base.is_null() && w > 0 && h > 0).then(|| {
        let row = w * 4;
        let mut data = Vec::with_capacity(row * h);
        for y in 0..h {
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(base.add(y * stride), row) });
        }
        VideoFrame { width: w as u32, height: h as u32, order: PixelOrder::Bgrx, data }
    });
    unsafe { CVPixelBufferUnlockBaseAddress(&px, CVPixelBufferLockFlags::ReadOnly) };
    frame
}

/// An NSError from a completion handler, as text; `None` for no error.
pub(super) fn error_text(e: *mut NSError) -> Option<String> {
    unsafe { e.as_ref() }.map(|e| e.localizedDescription().to_string())
}

/// What's on screen now: displays and windows, if ScreenCaptureKit says
/// within `limit`.
pub(super) fn shareable_content(limit: Duration) -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let r = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(Sendable(c)),
            None => Err(error_text(error).unwrap_or_else(|| "no reason given".into())),
        };
        let _ = tx.send(r);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match rx.recv_timeout(limit) {
        Ok(Ok(c)) => Ok(c.into_inner()),
        Ok(Err(e)) => bail!("ScreenCaptureKit can't list the screen's contents: {e}"),
        Err(_) => bail!("ScreenCaptureKit didn't list the screen's contents within {} s", limit.as_secs_f32()),
    }
}

/// Frames for `opts.target`, checked and set up here; the stream itself
/// starts on the capture thread.
pub fn feed(opts: &Options, interval: Duration) -> Result<Feed> {
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    // Weakly linked (build.rs): before 12.3 its classes don't exist.
    anyhow::ensure!(AnyClass::get(c"SCStream").is_some(), "recording needs macOS 12.3 or later");
    // ScreenCaptureKit needs the window server connection AppKit makes.
    let _ = NSApplication::sharedApplication(mtm);
    super::capture::ensure_permission()?;
    let outputs = super::capture::outputs(mtm)?;
    anyhow::ensure!(!outputs.is_empty(), "no displays found");
    let content = shareable_content(REPLY_LIMIT)?;

    let config = unsafe { SCStreamConfiguration::new() };
    let (filter, size) = match &opts.target {
        Target::Window(w) => {
            let id: u32 = w.id.parse().with_context(|| format!("window id '{}'", w.id))?;
            let windows = unsafe { content.windows() };
            let win = windows.iter().find(|s| unsafe { s.windowID() } == id).context("that window has closed")?;
            let frame = unsafe { win.frame() };
            crate::record::debug(&format!("recording window {id} ({} — {}) on its own", w.app, w.title));
            let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &win) };
            let scale = window_scale(&filter, &outputs, frame);
            // macOS 14+; before that the shadow comes along.
            if config.respondsToSelector(sel!(setIgnoreShadowsSingleWindow:)) {
                unsafe { config.setIgnoreShadowsSingleWindow(true) };
            }
            (filter, (frame.size.width * scale, frame.size.height * scale))
        }
        t => {
            let (out, crop) = resolve_target(&outputs, t)?;
            let id = super::capture::display_ids()?.get(out.index).copied();
            let displays = unsafe { content.displays() };
            let display = displays
                .iter()
                .find(|d| Some(unsafe { d.displayID() }) == id)
                .with_context(|| format!("ScreenCaptureKit doesn't list display {}", out.name))?;
            let (x, y, w, h) = crop.unwrap_or((0.0, 0.0, out.width as f64, out.height as f64));
            crate::record::debug(&format!("recording {w}x{h} at {x},{y} of display {}", out.name));
            unsafe { config.setSourceRect(CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))) };
            let scale = scale(&out);
            let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new()) };
            (filter, (w * scale, h * scale))
        }
    };
    let (w, h) = (size.0.round().max(2.0) as usize, size.1.round().max(2.0) as usize);
    unsafe {
        config.setWidth(w);
        config.setHeight(h);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setShowsCursor(opts.cursor);
        config.setMinimumFrameInterval(CMTime::new(interval.as_micros() as i64, 1_000_000));
        // A few frames in hand, so a slow copy doesn't stall the stream.
        config.setQueueDepth(5);
    }
    crate::record::debug(&format!("ScreenCaptureKit stream: {w}x{h} pixels"));

    let parts = Sendable((filter, config));
    let stream = Box::new(move |sink: Sink| -> Result<()> {
        let (filter, config) = parts.into_inner();
        run_stream(&filter, &config, sink)
    });
    // The stream's size is fixed at start, so a window doesn't go through
    // the camera. A red frame needs the main thread, which is busy writing.
    Ok(Feed { stream, zoom: false, start: Box::new(|| {}) })
}

/// Pixels per point for a window recorded on its own, wherever it is: the
/// filter knows (macOS 14+); before that, the sharpest display the window
/// is on, or of all displays when it's on none.
fn window_scale(filter: &SCContentFilter, outputs: &[crate::capture::Output], f: CGRect) -> f64 {
    if filter.respondsToSelector(sel!(pointPixelScale)) {
        return unsafe { filter.pointPixelScale() } as f64;
    }
    let on = |o: &&crate::capture::Output| {
        let (x, y, w, h) = (o.x as f64, o.y as f64, o.width as f64, o.height as f64);
        x < f.origin.x + f.size.width && f.origin.x < x + w && y < f.origin.y + f.size.height && f.origin.y < y + h
    };
    let sharpest = |o: &mut dyn Iterator<Item = &crate::capture::Output>| o.map(scale).reduce(f64::max);
    sharpest(&mut outputs.iter().filter(on)).or_else(|| sharpest(&mut outputs.iter())).unwrap_or(1.0)
}

/// Pixels per point on `out` (2 on Retina displays).
fn scale(out: &crate::capture::Output) -> f64 {
    if out.native.0 > 0 && out.width > 0 { out.native.0 as f64 / out.width as f64 } else { 1.0 }
}

/// Start the stream and keep it running until the writer stops or
/// ScreenCaptureKit does.
fn run_stream(filter: &SCContentFilter, config: &SCStreamConfiguration, sink: Sink) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let output = Output::new(sink, tx);
    let delegate = ProtocolObject::from_ref(&*output);
    let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), filter, config, Some(delegate)) };
    let queue = DispatchQueue::new("io.github.jasondyck.clipt.record", None);
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue))
    }
    .map_err(|e| anyhow!("ScreenCaptureKit: {}", e.localizedDescription()))?;

    if let Some(e) = call(|done| unsafe { stream.startCaptureWithCompletionHandler(Some(done)) })? {
        bail!("ScreenCaptureKit didn't start recording: {e}");
    }
    let r = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Done) => break Ok(()),
            Ok(Event::Failed(e)) => break Err(anyhow!("ScreenCaptureKit stopped: {e}")),
            Err(mpsc::RecvTimeoutError::Timeout) if crate::record::control::STOP.load(Ordering::Relaxed) => break Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    if let Err(e) = call(|done| unsafe { stream.stopCaptureWithCompletionHandler(Some(done)) }) {
        crate::record::debug(&format!("stopping the stream: {e:#}"));
    }
    r
}

/// Call a ScreenCaptureKit method that reports back through a completion
/// handler taking an error, and wait for it: the error's text, if any.
fn call(f: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<Option<String>> {
    let (tx, rx) = mpsc::channel();
    let done = RcBlock::new(move |e: *mut NSError| {
        let _ = tx.send(error_text(e));
    });
    f(&done);
    rx.recv_timeout(REPLY_LIMIT).map_err(|_| anyhow!("ScreenCaptureKit didn't answer within {} s", REPLY_LIMIT.as_secs()))
}
