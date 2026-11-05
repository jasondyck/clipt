//! The red frame around a region being recorded, as on Linux: four thin
//! topmost strips just *outside* the region, so the frame never appears in
//! the video, which clicks pass through. Amber while paused. Not shown for
//! whole-screen and window recordings.
//!
//! The strips also ask to be left out of screen capture (Windows 10 2004
//! and later), in case a region touches another monitor's.

use crate::record::control::{PAUSED, STOP};
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, LWA_ALPHA, MSG, PostQuitMessage, RegisterClassExW, SW_SHOWNOACTIVATE,
    SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, ShowWindow, TranslateMessage, WM_PAINT, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Frame thickness in logical pixels, as on Linux.
const T: f64 = 3.0;
const RED: COLORREF = 0x0044_44ef; // COLORREF is 0x00bbggrr
const AMBER: COLORREF = 0x000b_9ef5;
/// SetWindowDisplayAffinity's WDA_EXCLUDEFROMCAPTURE.
const EXCLUDE_FROM_CAPTURE: u32 = 0x11;

/// The strips around `r`, clipped to monitor `m` (physical pixels); a side
/// that would fall off the monitor is left out.
pub fn strips(m: RECT, r: RECT, t: i32) -> Vec<RECT> {
    let clip = |s: RECT| {
        let c = RECT { left: s.left.max(m.left), top: s.top.max(m.top), right: s.right.min(m.right), bottom: s.bottom.min(m.bottom) };
        (c.right > c.left && c.bottom > c.top).then_some(c)
    };
    [
        RECT { left: r.left - t, top: r.top - t, right: r.right + t, bottom: r.top },
        RECT { left: r.left - t, top: r.bottom, right: r.right + t, bottom: r.bottom + t },
        RECT { left: r.left - t, top: r.top, right: r.left, bottom: r.bottom },
        RECT { left: r.right, top: r.top, right: r.right + t, bottom: r.bottom },
    ]
    .into_iter()
    .filter_map(clip)
    .collect()
}

/// Show the frame around `r` on monitor `m` until the recording stops (or
/// the process ends). Runs its own message loop: call it on its own thread.
pub fn run(m: RECT, r: RECT, scale: f64) {
    let rects = strips(m, r, (T * scale).round().max(1.0) as i32);
    if rects.is_empty() {
        return;
    }
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = super::wide("CliptFrame");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassExW(&wc) };
    let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT;
    let mut windows: Vec<HWND> = Vec::new();
    for s in &rects {
        let h = unsafe {
            CreateWindowExW(
                ex,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                s.left,
                s.top,
                s.right - s.left,
                s.bottom - s.top,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if h.is_null() {
            continue;
        }
        unsafe {
            // Layered and transparent: clicks go to what's below.
            SetLayeredWindowAttributes(h, 0, 255, LWA_ALPHA);
            SetWindowDisplayAffinity(h, EXCLUDE_FROM_CAPTURE);
            ShowWindow(h, SW_SHOWNOACTIVATE);
        }
        windows.push(h);
    }
    let Some(&first) = windows.first() else { return };
    // Pausing turns it amber; the timer also ends it with the recording.
    unsafe { SetTimer(first, 1, 100, None) };
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    let mut paused = false;
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        if msg.message == WM_TIMER {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            let now = PAUSED.load(Ordering::SeqCst);
            if now != paused {
                paused = now;
                for &h in &windows {
                    unsafe { InvalidateRect(h, std::ptr::null(), 0) };
                }
            }
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            let dc = unsafe { BeginPaint(h, &mut ps) };
            let brush = unsafe { CreateSolidBrush(if PAUSED.load(Ordering::SeqCst) { AMBER } else { RED }) };
            unsafe {
                FillRect(dc, &ps.rcPaint, brush);
                DeleteObject(brush);
                EndPaint(h, &ps);
            }
            0
        }
        windows_sys::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, w, l) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    #[test]
    fn frame_sits_outside_the_region() {
        let m = rect(0, 0, 1280, 720);
        let s = strips(m, rect(100, 50, 400, 250), 3);
        let got: Vec<_> = s.iter().map(|r| (r.left, r.top, r.right, r.bottom)).collect();
        assert_eq!(got, vec![(97, 47, 403, 50), (97, 250, 403, 253), (97, 50, 100, 250), (400, 50, 403, 250)]);
    }

    #[test]
    fn sides_off_the_monitor_are_dropped_or_clipped() {
        let m = rect(0, 0, 1280, 720);
        let s = strips(m, rect(0, 0, 100, 100), 3);
        let got: Vec<_> = s.iter().map(|r| (r.left, r.top, r.right, r.bottom)).collect();
        assert_eq!(got, vec![(0, 100, 103, 103), (100, 0, 103, 100)]);
        assert!(strips(m, m, 3).is_empty());
    }
}
