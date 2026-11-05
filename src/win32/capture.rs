//! Monitors and their pixels through GDI.
//!
//! The editor's coordinates are the virtual desktop's physical pixels
//! divided by the main monitor's scale. One factor for all, rather than
//! each monitor's own, keeps the monitors' layout as Windows has it (each
//! its own factor would open gaps or overlaps between monitors of
//! different scales); the editor's toolbar then has the main monitor's
//! size everywhere.
//!
//! `BitBlt` from the screen works in every session, remote desktop
//! included, and returns at once. Desktop Duplication, which recording
//! will need, can come later.

use super::{from_wide, wide};
use crate::capture::{Output, Shot};
use crate::image::Image;
use anyhow::{Result, ensure};
use windows_sys::Win32::Foundation::{LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DISPLAY_DEVICEW,
    DeleteDC, DeleteObject, EnumDisplayDevicesW, EnumDisplayMonitors, GdiFlush, GetDC, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, ICONINFO, MONITORINFOF_PRIMARY,
};
use windows_sys::core::BOOL;

/// Per-monitor DPI awareness: Windows then reports and draws everything in
/// physical pixels instead of scaling clipt's windows and coordinates.
pub fn dpi_aware() {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub struct Desktop {
    /// The main monitor first, then left to right.
    pub outputs: Vec<Output>,
    /// Each output's rectangle on the virtual desktop, in physical pixels.
    pub phys: Vec<RECT>,
    /// Physical pixels per editor unit: the main monitor's scale.
    pub scale: f64,
}

impl Desktop {
    pub fn get() -> Result<Self> {
        let mut found: Vec<(RECT, bool, String, f64)> = Vec::new();
        unsafe extern "system" fn each(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
            let found = unsafe { &mut *(data as *mut Vec<(RECT, bool, String, f64)>) };
            let mut mi: MONITORINFOEXW = unsafe { std::mem::zeroed() };
            mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if unsafe { GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) } != 0 {
                let (mut dx, mut dy) = (96, 96);
                unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
                let primary = mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
                found.push((mi.monitorInfo.rcMonitor, primary, from_wide(&mi.szDevice), dx as f64 / 96.0));
            }
            1
        }
        unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut found as *mut _ as LPARAM) };
        ensure!(!found.is_empty(), "no monitors found");
        found.sort_by_key(|(r, primary, ..)| (!primary, r.left, r.top));
        let scale = found[0].3.max(0.5);
        let outputs = found
            .iter()
            .enumerate()
            .map(|(index, (r, _, device, _))| {
                let (pw, ph) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
                Output {
                    index,
                    // "\\.\DISPLAY2" is display 2 in Windows' Settings.
                    name: device.trim_start_matches(r"\\.\DISPLAY").to_string(),
                    description: monitor_name(device),
                    x: (r.left as f64 / scale).round() as i32,
                    y: (r.top as f64 / scale).round() as i32,
                    width: (pw as f64 / scale).round() as u32,
                    height: (ph as f64 / scale).round() as u32,
                    native: (pw, ph),
                }
            })
            .collect();
        Ok(Self { outputs, phys: found.iter().map(|f| f.0).collect(), scale })
    }

    /// One shot per output, at its physical resolution.
    pub fn capture(&self, which: &[usize], cursor: bool) -> Result<Vec<Shot>> {
        zone!("screenshot");
        which.iter().map(|&i| Ok(Shot { output: self.outputs[i].clone(), image: grab(self.phys[i], cursor)? })).collect()
    }
}

/// What Windows calls the monitor ("DELL U2723QE"; often just "Generic PnP
/// Monitor").
fn monitor_name(device: &str) -> String {
    let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
    dd.cb = size_of::<DISPLAY_DEVICEW>() as u32;
    let dev = wide(device);
    if unsafe { EnumDisplayDevicesW(dev.as_ptr(), 0, &mut dd, 0) } != 0 { from_wide(&dd.DeviceString) } else { String::new() }
}

/// A top-down 32-bit DIB section of `w`×`h`, selected into a memory DC.
pub struct Dib {
    pub dc: HDC,
    bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    old: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
    pub bits: *mut u8,
    pub w: i32,
    pub h: i32,
}

impl Dib {
    pub fn new(w: i32, h: i32) -> Result<Self> {
        let info = bitmap_info(w, h);
        let mut bits = std::ptr::null_mut();
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(std::ptr::null_mut(), screen);
            ensure!(!dc.is_null(), "CreateCompatibleDC failed");
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(dc);
                anyhow::bail!("CreateDIBSection failed for {w}x{h}");
            }
            let old = SelectObject(dc, bitmap);
            Ok(Self { dc, bitmap, old, bits: bits.cast(), w, h })
        }
    }

    pub fn bitmap(&self) -> windows_sys::Win32::Graphics::Gdi::HBITMAP {
        self.bitmap
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, (self.w * self.h * 4) as usize) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

/// BGRX rows top to bottom (a negative height).
pub fn bitmap_info(w: i32, h: i32) -> BITMAPINFO {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    info
}

fn grab(r: RECT, cursor: bool) -> Result<Image> {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    ensure!(w > 0 && h > 0, "empty monitor");
    let dib = Dib::new(w, h)?;
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        // CAPTUREBLT: layered (translucent) windows too.
        let ok = BitBlt(dib.dc, 0, 0, w, h, screen, r.left, r.top, SRCCOPY | CAPTUREBLT);
        ReleaseDC(std::ptr::null_mut(), screen);
        ensure!(ok != 0, "BitBlt from the screen failed");
        if cursor {
            draw_cursor(dib.dc, r);
        }
        GdiFlush();
    }
    let mut img = Image::new(w as u32, h as u32);
    for (d, s) in img.data.as_chunks_mut::<4>().0.iter_mut().zip(dib.bytes().as_chunks::<4>().0) {
        *d = [s[2], s[1], s[0], 255];
    }
    Ok(img)
}

/// The pointer as it is now, at its hotspot.
pub unsafe fn draw_cursor(dc: HDC, r: RECT) {
    let mut ci: CURSORINFO = unsafe { std::mem::zeroed() };
    ci.cbSize = size_of::<CURSORINFO>() as u32;
    if unsafe { GetCursorInfo(&mut ci) } == 0 || ci.flags & CURSOR_SHOWING == 0 || ci.hCursor.is_null() {
        return;
    }
    let mut ii: ICONINFO = unsafe { std::mem::zeroed() };
    if unsafe { GetIconInfo(ci.hCursor, &mut ii) } == 0 {
        return;
    }
    let POINT { x, y } = ci.ptScreenPos;
    unsafe {
        DrawIconEx(
            dc,
            x - ii.xHotspot as i32 - r.left,
            y - ii.yHotspot as i32 - r.top,
            ci.hCursor,
            0,
            0,
            0,
            std::ptr::null_mut(),
            DI_NORMAL,
        );
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask);
        }
        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor);
        }
    }
}
