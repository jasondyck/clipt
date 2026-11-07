//! One window on its own, for the screenshot overlay: the overlay shows a
//! picture of the screen taken when it opened, so a window that something
//! covered would come out covered. Clicking it fetches the window by itself
//! and paints it over that picture, as if brought to the front.
//!
//! ScreenCaptureKit's `SCScreenshotManager` does this from macOS 14; before
//! that, `CGWindowListCreateImage`, which macOS 14 deprecated, looked up at
//! run time as `capture.rs` looks up `CGDisplayCreateImage`.

use super::record::{Sendable, error_text, shareable_content};
use crate::image::Image;
use crate::windows::Window;
use anyhow::{Context, Result, bail};
use block2::RcBlock;
use objc2::AnyThread;
use objc2::runtime::AnyClass;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCContentFilter, SCScreenshotManager, SCStreamConfiguration};
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

/// How long each ScreenCaptureKit step may take. It answers in
/// milliseconds; the overlay waits on it, frozen, so a hang gives up soon.
const SHOT_LIMIT: Duration = Duration::from_secs(2);

/// `w` alone, at `scale` pixels per point, with transparent rounded corners
/// (premultiplied RGBA).
pub fn capture(w: &Window, scale: f64) -> Result<Image> {
    let id: u32 = w.id.parse().with_context(|| format!("window id '{}'", w.id))?;
    let img = if AnyClass::get(c"SCScreenshotManager").is_some() { screenshot_manager(id, w, scale)? } else { window_list(id, w)? };
    super::capture::to_image_with_alpha(&img)
}

/// macOS 14 and later.
fn screenshot_manager(id: u32, w: &Window, scale: f64) -> Result<CFRetained<CGImage>> {
    let content = shareable_content(SHOT_LIMIT)?;
    let windows = unsafe { content.windows() };
    let win = windows.iter().find(|s| unsafe { s.windowID() } == id).context("that window has closed")?;
    let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &win) };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth((w.width as f64 * scale).round().max(1.0) as usize);
        config.setHeight((w.height as f64 * scale).round().max(1.0) as usize);
        config.setShowsCursor(false);
        config.setIgnoreShadowsSingleWindow(true);
    }
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |img: *mut CGImage, error: *mut NSError| {
        let r = match NonNull::new(img) {
            // Borrowed: retained to outlive the handler.
            Some(p) => Ok(Sendable(unsafe { CFRetained::retain(p) })),
            None => Err(error_text(error).unwrap_or_else(|| "no reason given".into())),
        };
        let _ = tx.send(r);
    });
    unsafe { SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&filter, &config, Some(&handler)) };
    match rx.recv_timeout(SHOT_LIMIT) {
        Ok(Ok(img)) => Ok(img.into_inner()),
        Ok(Err(e)) => bail!("ScreenCaptureKit couldn't capture the window: {e}"),
        Err(_) => bail!("ScreenCaptureKit didn't capture the window within {} s", SHOT_LIMIT.as_secs()),
    }
}

type WindowListCreateImage = unsafe extern "C" fn(CGRect, u32, u32, u32) -> Option<NonNull<CGImage>>;

/// macOS 12 and 13.
fn window_list(id: u32, w: &Window) -> Result<CFRetained<CGImage>> {
    // kCGWindowListOptionIncludingWindow; kCGWindowImageBoundsIgnoreFraming
    // (no shadow) | kCGWindowImageBestResolution.
    const INCLUDING_WINDOW: u32 = 1 << 3;
    const IMAGE_OPTIONS: u32 = (1 << 0) | (1 << 3);
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"CGWindowListCreateImage".as_ptr()) };
    if p.is_null() {
        bail!("this macOS has no way to capture one window");
    }
    let f = unsafe { std::mem::transmute::<*mut libc::c_void, WindowListCreateImage>(p) };
    let rect = CGRect::new(CGPoint::new(w.x as f64, w.y as f64), CGSize::new(w.width as f64, w.height as f64));
    let img = unsafe { f(rect, INCLUDING_WINDOW, id, IMAGE_OPTIONS) }.context("the window couldn't be captured")?;
    Ok(unsafe { CFRetained::from_raw(img) })
}
