//! Recording's frames on Windows: GDI `BitBlt` from the screen, at the
//! recording's frame rate, as screenshots take them. A window is followed
//! by its frame bounds and goes through the camera, as on X11.
//!
//! Desktop Duplication would hand over only changed frames and cost less at
//! 4K, but needs COM, which `windows-sys` doesn't cover; GDI also works in
//! remote desktop sessions, where duplication can fail.

use super::capture::{Desktop, Dib, draw_cursor};
use crate::capture::{PixelOrder, VideoFrame};
use crate::record::{Feed, Options, Sink, Target, resolve_target};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{BitBlt, CAPTUREBLT, GdiFlush, GetDC, ReleaseDC, SRCCOPY};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, IsIconic, IsWindow, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// What to capture each frame.
enum Area {
    /// A fixed rectangle of the desktop, in physical pixels.
    Fixed(RECT),
    /// Wherever this window is now.
    Window(usize),
}

pub fn feed(opts: &Options, interval: Duration) -> Result<Feed> {
    super::capture::dpi_aware();
    let desk = Desktop::get()?;
    let (area, zoom) = match &opts.target {
        Target::Window(w) if w.id.parse::<usize>().is_ok() => (Area::Window(w.id.parse()?), true),
        t => {
            let (o, crop) = resolve_target(&desk.outputs, t)?;
            let m = desk.phys[o.index];
            let r = match crop {
                Some((x, y, w, h)) => {
                    let s = desk.scale;
                    let (l, t) = (m.left + (x * s).round() as i32, m.top + (y * s).round() as i32);
                    RECT {
                        left: l,
                        top: t,
                        right: (l + (w * s).round() as i32).min(m.right),
                        bottom: (t + (h * s).round() as i32).min(m.bottom),
                    }
                }
                None => m,
            };
            ensure!(r.right > r.left && r.bottom > r.top, "the area to record is empty");
            (Area::Fixed(r), false)
        }
    };
    let cursor = opts.cursor;
    let stream = Box::new(move |sink: Sink| stream(area, cursor, interval, sink));
    Ok(Feed { stream, zoom, start: Box::new(|| {}) })
}

fn stream(area: Area, cursor: bool, interval: Duration, mut sink: Sink) -> Result<()> {
    let mut dib: Option<Dib> = None;
    let mut last = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut due = Instant::now();
    loop {
        let r = match &area {
            Area::Fixed(r) => *r,
            // A window that closes or is minimised keeps its last area, as
            // on Linux.
            &Area::Window(h) => window_rect(h as HWND).unwrap_or(last),
        };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w > 0 && h > 0 {
            if dib.as_ref().is_none_or(|d| (d.w, d.h) != (w, h)) {
                dib = Some(Dib::new(w, h)?);
            }
            let d = dib.as_ref().unwrap();
            unsafe {
                let screen = GetDC(std::ptr::null_mut());
                let ok = BitBlt(d.dc, 0, 0, w, h, screen, r.left, r.top, SRCCOPY | CAPTUREBLT);
                ReleaseDC(std::ptr::null_mut(), screen);
                ensure!(ok != 0, "BitBlt from the screen failed");
                if cursor {
                    draw_cursor(d.dc, r);
                }
                GdiFlush();
            }
            let frame = VideoFrame { width: w as u32, height: h as u32, order: PixelOrder::Bgrx, data: d.bytes().to_vec() };
            if !sink(frame) {
                return Ok(());
            }
            last = r;
        }
        // Paced from when each frame was due, so a slow BitBlt doesn't
        // lower the rate; after a stall, start afresh rather than catch up.
        due += interval;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else {
            due = now;
        }
    }
}

/// A window's visible frame on screen, clipped to the desktop; `None` once
/// it has closed or while it's minimised.
fn window_rect(h: HWND) -> Option<RECT> {
    unsafe {
        if IsWindow(h) == 0 || IsIconic(h) != 0 {
            return None;
        }
        let mut r: RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as u32, (&mut r as *mut RECT).cast(), size_of::<RECT>() as u32);
        if hr != 0 {
            return None;
        }
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let c = RECT { left: r.left.max(vx), top: r.top.max(vy), right: r.right.min(vx + vw), bottom: r.bottom.min(vy + vh) };
        (c.right > c.left && c.bottom > c.top).then_some(c)
    }
}
