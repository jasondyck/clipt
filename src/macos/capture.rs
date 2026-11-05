//! Displays and their pixels through Core Graphics.
//!
//! `CGDisplayCreateImage` is deprecated in favour of ScreenCaptureKit but
//! still works, needs no run loop and returns at once. It is looked up when
//! called rather than linked, so that a macOS without it can't stop clipt
//! from starting; then, and for `--cursor` (which it can't include), the
//! system's `screencapture` command takes the shot instead.

use crate::capture::{Output, Shot};
use crate::image::Image;
use anyhow::{Context, Result, bail, ensure};
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGError,
    CGGetActiveDisplayList, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
    kCGColorSpaceSRGB,
};
use objc2_foundation::{NSNumber, NSString};
use std::ptr::NonNull;

/// Screen Recording permission. Without it macOS still hands over images,
/// but with every window blanked out, so ask first and stop.
pub fn ensure_permission() -> Result<()> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    // Shows the system's prompt the first time; after that, only Settings
    // can grant it, and it takes effect when clipt next starts.
    CGRequestScreenCaptureAccess();
    bail!(
        "clipt needs permission to record the screen.\n\
         Allow it in System Settings > Privacy & Security > Screen & System Audio Recording\n\
         for the app clipt runs from (your terminal, or the app that runs the shortcut), then try again"
    )
}

/// The active displays, the main one first, in the global coordinate space
/// Core Graphics uses: points, origin at the main display's top left, y down.
/// That's the space the editor works in, so no conversion is needed.
pub fn outputs(mtm: MainThreadMarker) -> Result<Vec<Output>> {
    let names = screen_names(mtm);
    Ok(display_ids()?
        .iter()
        .enumerate()
        .map(|(index, &id)| {
            let b = CGDisplayBounds(id);
            let mode = CGDisplayCopyDisplayMode(id);
            let native = (CGDisplayMode::pixel_width(mode.as_deref()) as u32, CGDisplayMode::pixel_height(mode.as_deref()) as u32);
            Output {
                index,
                // As `screencapture -D` numbers them.
                name: (index + 1).to_string(),
                description: names.iter().find(|(i, _)| *i == id).map(|(_, n)| n.clone()).unwrap_or_default(),
                x: b.origin.x.round() as i32,
                y: b.origin.y.round() as i32,
                width: b.size.width.round() as u32,
                height: b.size.height.round() as u32,
                native,
            }
        })
        .collect())
}

/// Display ids and the names System Settings shows ("Built-in Retina
/// Display", "DELL U2723QE").
fn screen_names(mtm: MainThreadMarker) -> Vec<(CGDirectDisplayID, String)> {
    let key = NSString::from_str("NSScreenNumber");
    NSScreen::screens(mtm)
        .iter()
        .filter_map(|s| {
            let id = s.deviceDescription().objectForKey(&key)?.downcast::<NSNumber>().ok()?.unsignedIntValue();
            Some((id, s.localizedName().to_string()))
        })
        .collect()
}

/// One shot per output, at the display's native resolution.
pub fn capture(outputs: &[Output], cursor: bool) -> Result<Vec<Shot>> {
    zone!("screenshot");
    let ids = display_ids()?;
    outputs
        .iter()
        .map(|o| {
            let id = *ids.get(o.index).context("display list changed")?;
            let image = match (cursor, create_image_fn()) {
                (false, Some(f)) => match unsafe { f(id) } {
                    Some(img) => to_image(&*unsafe { CFRetained::from_raw(img) })?,
                    None => screencapture(o, cursor)?,
                },
                _ => screencapture(o, cursor)?,
            };
            Ok(Shot { output: o.clone(), image })
        })
        .collect()
}

/// The active displays' ids, in the order `outputs` numbers them
/// (`Output::index`).
pub fn display_ids() -> Result<Vec<CGDirectDisplayID>> {
    let mut ids = [0 as CGDirectDisplayID; 32];
    let mut n = 0u32;
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) };
    ensure!(err == CGError::Success, "listing displays failed (CGError {})", err.0);
    Ok(ids[..n as usize].to_vec())
}

type CreateImage = unsafe extern "C" fn(CGDirectDisplayID) -> Option<NonNull<CGImage>>;

fn create_image_fn() -> Option<CreateImage> {
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"CGDisplayCreateImage".as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute::<*mut libc::c_void, CreateImage>(p) })
}

/// Draw the image into an sRGB bitmap: displays have their own colour
/// spaces (Display P3 on recent Macs), and a PNG without a profile is read
/// as sRGB.
fn to_image(img: &CGImage) -> Result<Image> {
    let (w, h) = (CGImage::width(Some(img)), CGImage::height(Some(img)));
    ensure!(w > 0 && h > 0, "empty display image");
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).context("no sRGB colour space")?;
    let mut out = Image::new(w as u32, h as u32);
    // Bytes R, G, B, then one unused, which is set opaque below.
    let info = CGImageAlphaInfo::NoneSkipLast.0 | CGImageByteOrderInfo::Order32Big.0;
    let ctx = unsafe { CGBitmapContextCreate(out.data.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), info) }
        .context("creating a bitmap context")?;
    let rect = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
    CGContext::draw_image(Some(&ctx), rect, Some(img));
    drop(ctx);
    for px in out.data.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    Ok(out)
}

/// The system's own screenshot command, as a PNG through a temporary file.
fn screencapture(o: &Output, cursor: bool) -> Result<Image> {
    let path = std::env::temp_dir().join(format!("clipt-{}-{}.png", std::process::id(), o.name));
    let mut cmd = std::process::Command::new("/usr/sbin/screencapture");
    // -x: no shutter sound. -D: display number, counted from 1.
    cmd.args(["-x", "-t", "png", "-D", &o.name]);
    if cursor {
        cmd.arg("-C");
    }
    let status = cmd.arg(&path).status().context("running screencapture")?;
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    ensure!(status.success(), "screencapture failed ({status})");
    Image::decode_png(&bytes.context("reading screencapture's PNG")?)
}
